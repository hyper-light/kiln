//! The single owner of a replicated Database. Hosts carry authenticated wire records only.
//!
//! One command at a time crosses ReadIndex before evaluation. A captured answer stays here until
//! hyper-durable releases its matching durable application; no caller sees speculative state.

use super::*;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::time::Duration;

use crate::meter::{
    FloorCounters, FloorMeter, FloorRefusal, LocalRefusal, ReplicaCounters, ReplicaMeter,
};
use crate::store::Checkpointed;
use api::replication::{
    CatchUp, Durability, MemberChange, Outbound, ReplicaConfig, ReplicaStatus, Verdict,
};
use hyper_block::block::BlockFile;
use hyper_block::file::{CachingRequest, DeviceFile};
use hyper_durable::{Bytes, GroupStore, Output, Point, Replica, StateMachine};
use hyper_raft::proto::{
    ConfChangeSingle, ConfChangeTransition, ConfChangeType, ConfChangeV2, ConfState, Message, Plan,
};
use hyper_raft::wire::Record;

// Admission limits, not throughput tuning: overflow refuses before taking ownership. The queue
// charges a full API request for an opaque FnOnce, whose captures the API cannot inspect.
const COMMANDS: usize = 64;
const QUEUED_BYTES: usize = 512 << 20;
const OUTGOING_BYTES: usize = 512 << 20;
const OUTGOING_MESSAGES: usize = 4096;
pub(crate) const MEMBERS: usize = 64;
/// The most image bytes one wire record carries into this owner's command queue: the queue's room
/// held back from application calls for one record (`enqueue`). Picked with `QUEUED_BYTES`, whose
/// split it is, and replaced with it (lane constants, piece 3: the queue sized from the memory
/// budget and the measured drain rate). An image's own bound is the memory budget's room for one
/// (SB.1), no more than this while it travels through this queue (`owner_image_room`).
const WIRE_IMAGE_BYTES: usize = 256 << 20;
const WIRE_BYTES: usize = WIRE_IMAGE_BYTES.saturating_add(2 << 20);
const MESSAGE_BYTES: usize = crate::replication::COMMAND_BYTES.saturating_add(2 << 20);
// Ticks are the pinned core's documented tick election mode (raft.rs, Elections::Ticks). Its
// randomized election and heartbeat, in the core's fixed tick counts, use this owner clock, never
// host votes. The period between ticks is learned where the owner runs (`pace`): this is only
// hyper-timing's start state, the period of a pace with nothing measured (`TickPace::floor`,
// vendor/hyper-raft/crates/hyper-timing/src/lib.rs), the owner's period before it has measured a
// quorum's round trips, its timer and its leader's link. Once measured, it bounds nothing.
const TICK: Duration = Duration::from_millis(10);
// The admission charge of a change of members, a hand-off of the lead, a catch-up or an
// attestation: the bytes the queued input itself takes, its place in the queue's byte budget (an
// attestation's name is charged beside it). The response channel's own allocation is charged for
// no input, as before.
const CHANGE_BYTES: usize = std::mem::size_of::<Input>();

/// One of the floor's own sessions: the work it does on the database at its base.
type FloorSession = Box<dyn FnOnce(&mut Db) -> anyhow::Result<()> + Send>;
type Durable<F = DeviceFile> = Replica<GroupStore<F>, crate::replication::Machine, Bytes>;
type Response<T> = mpsc::SyncSender<Result<T, Refusal>>;

/// A bounded ingress and egress handle. Clones share one product-owned Raft actor.
#[derive(Clone)]
pub struct ReplicaHandle {
    sender: mpsc::SyncSender<Envelope>,
    // Declared after `sender`, so it rings once the sender is gone: the last handle's drop wakes
    // the owner to find its channel closed.
    _ring: Ring,
    shared: Arc<Control>,
}

/// What wakes the owner thread (replication.md, "Decision (lane constants): the owner's wakes"): an input queued, a handle
/// dropped, the log's write or compaction done (its [`std::task::Waker`]), or a flush done. The
/// owner waits on it only until its next deadline, so a wake is never needed for a deadline.
struct Bell {
    rung: Mutex<bool>,
    ringing: std::sync::Condvar,
}
impl Bell {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            rung: Mutex::new(false),
            ringing: std::sync::Condvar::new(),
        })
    }
    /// Wakes the owner, or its next wait if it isn't waiting. A poisoned bell only rings for an
    /// owner whose own wait fails on that poison and fences, so nothing is left to wake.
    fn ring(&self) {
        if let Ok(mut rung) = self.rung.lock() {
            *rung = true;
            self.ringing.notify_one();
        }
    }
    /// Waits until the bell rings or `wait` passes, whichever is first, and takes the ring:
    /// whether it rang.
    fn wait(&self, wait: Duration) -> Result<bool, Refusal> {
        let rung = locked(&self.rung)?;
        let (mut rung, _) = self
            .ringing
            .wait_timeout_while(rung, wait, |rung| !*rung)
            .map_err(|_| fenced("the owner's bell was poisoned"))?;
        Ok(std::mem::take(&mut *rung))
    }
}
impl std::task::Wake for Bell {
    fn wake(self: Arc<Self>) {
        self.ring();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.ring();
    }
}
/// A handle's ring as it drops.
#[derive(Clone)]
struct Ring(Arc<Bell>);
impl Drop for Ring {
    fn drop(&mut self) {
        self.0.ring();
    }
}

/// Completion of a bounded queued wire record. Admission transfers byte ownership only;
/// processing, application and durability are not acknowledged until their own protocol steps.
#[must_use = "poll the receipt to observe later processing errors"]
pub struct ReceiveReceipt {
    // Mutex makes receipts Sync for host carrier shells; exclusive polling uses get_mut and
    // never locks or waits for the actor. The actor holds only the channel's sending end.
    receiver: Mutex<mpsc::Receiver<Result<(), Refusal>>>,
    until: Instant,
    completed: bool,
}
impl ReceiveReceipt {
    /// None is pending; Some(()) means the actor accepted the record through step. Typed errors
    /// report rejection, deadline expiry or owner loss. A completed receipt cannot be polled again.
    /// Expiry stops observation; it does not undo processing that already began.
    pub fn try_result(&mut self) -> Result<Option<()>, Refusal> {
        if self.completed {
            return Err(api::invalid("replica receive receipt", "already completed"));
        }
        let result = match self.receiver.get_mut() {
            Ok(receiver) => match receiver.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) if Instant::now() < self.until => None,
                Err(mpsc::TryRecvError::Empty) => Some(Err(Refusal::Timeout)),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(fenced("replica owner ended"))),
            },
            Err(_) => Some(Err(unavailable(
                "replica receive receipt lock was poisoned",
            ))),
        };
        match result {
            None => Ok(None),
            Some(result) => {
                self.completed = true;
                result.map(|()| Some(()))
            }
        }
    }
}
#[derive(Clone)]
pub(super) struct Client {
    handle: ReplicaHandle,
}
impl Client {
    pub(super) fn submit(
        &self,
        namespace: String,
        operation: Operation,
        application: Option<crate::database::Application>,
        until: Instant,
    ) -> Result<Answer, Refusal> {
        self.handle.submit(namespace, operation, application, until)
    }
    pub(super) fn handle(&self) -> ReplicaHandle {
        self.handle.clone()
    }
    /// `f` on this owner's own applied state, on the caller's thread, asking the group's leader
    /// nothing (replication.md, "A local read on a replica"): it answers with the leader gone or
    /// out of reach, and sees only entries the group committed and this owner applied, never a
    /// preparation, which rolls back without a commit. Refused while this owner hasn't taken its
    /// group's first image (its state is no group's yet), once it ended, and while it is fenced.
    pub(super) fn read_local<R>(
        &self,
        until: Instant,
        f: impl FnOnce(&mut Db) -> anyhow::Result<R>,
    ) -> Result<R, Refusal> {
        let meter = &self.handle.shared.meter;
        if Instant::now() >= until {
            meter.local_refused(LocalRefusal::Timeout);
            return Err(Refusal::Timeout);
        }
        let status = match locked(&self.handle.shared.status)?.clone() {
            Ok(status) => status,
            Err(ended) => {
                meter.local_refused(LocalRefusal::Fenced);
                return Err(ended);
            }
        };
        if let Some(reason) = status.fenced {
            meter.local_refused(LocalRefusal::Fenced);
            return Err(fenced(reason));
        }
        if status.bootstrapping {
            meter.local_refused(LocalRefusal::Bootstrapping);
            return Err(unavailable(
                "this replica hasn't taken its group's first image yet; it reads once it has",
            ));
        }
        self.handle.shared.local.with(until, f)
    }
    /// Waits until this replica's published closed time is at least `at_least` and past `seen`
    /// (the time a read found too old, if one did), on the caller's thread, until `until`
    /// (replication.md, "Follower reads at a closed time"). Refused Timeout at the deadline, and
    /// Fenced once the owner ended or is fenced.
    fn wait_closed(&self, at_least: u64, seen: Option<u64>, until: Instant) -> Result<(), Refusal> {
        let shared = &self.handle.shared;
        let mut closed = locked(&shared.closed)?;
        loop {
            if locked(&shared.local.open)?.is_none() {
                shared.meter.local_refused(LocalRefusal::Fenced);
                return Err(fenced("replica owner ended"));
            }
            match &*locked(&shared.status)? {
                Err(ended) => {
                    shared.meter.local_refused(LocalRefusal::Fenced);
                    return Err(ended.clone());
                }
                Ok(status) => {
                    if let Some(reason) = &status.fenced {
                        shared.meter.local_refused(LocalRefusal::Fenced);
                        return Err(fenced(reason));
                    }
                }
            }
            if closed
                .time
                .is_some_and(|time| time >= at_least && seen.is_none_or(|seen| time > seen))
            {
                return Ok(());
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                shared.meter.local_refused(LocalRefusal::Timeout);
                return Err(Refusal::Timeout);
            }
            closed = shared
                .moved
                .wait_timeout(closed, left)
                .map_err(|_| unavailable("the closed time's lock was poisoned"))?
                .0;
        }
    }
    /// `read` on a reader once this replica's closed time is at least `at_least` (replication.md,
    /// "Follower reads at a closed time"): `read` takes its transaction's time first and answers
    /// `Err` with it (none before the group's first stamped entry) if that is earlier, and the
    /// next try waits for a later one. Each turn waits for the owner's next time, so the turns are
    /// bounded by the deadline.
    pub(super) fn read_closed<R>(
        &self,
        at_least: u64,
        until: Instant,
        mut read: impl FnMut(&mut Db) -> anyhow::Result<Result<R, Option<u64>>>,
    ) -> Result<R, Refusal> {
        let mut seen = None;
        loop {
            self.wait_closed(at_least, seen, until)?;
            match self.read_local(until, &mut read)? {
                Ok(answer) => return Ok(answer),
                Err(found) => seen = found.or(Some(0)),
            }
        }
    }
    /// The lag this replica measured between its closed time and its clock, if it has measured
    /// any: a read asked that far behind now finds its time here at least 99 times in 100.
    pub(super) fn closed_lag(&self) -> Result<Option<u64>, Refusal> {
        Ok(locked(&self.handle.shared.closed)?.lag)
    }
}
/// The wall clock now, in microseconds since the Unix epoch: where an owner's stamps start
/// (replication.md, "Follower reads at a closed time").
pub(super) fn wall_micros() -> Result<u64, Refusal> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|since| u64::try_from(since.as_micros()).ok())
        .ok_or_else(|| unavailable("the wall clock reads before the Unix epoch or past u64"))
}
struct Control {
    admitted: AtomicUsize,
    // The application this member's handles last bound, so that binding it again asks nothing.
    attesting: Mutex<Option<crate::database::Application>>,
    // The floor in force at the applied index last published, so a bind below it is refused
    // without asking the owner.
    floor: Mutex<Option<crate::database::Application>>,
    // What floor_status answers: the floor and each member against it, at that same index.
    floor_status: Mutex<amphora_api::replication::FloorStatus>,
    floor_meter: FloorMeter,
    outgoing: Mutex<Outgoing>,
    status: Mutex<Result<ReplicaStatus, Refusal>>,
    meter: Arc<ReplicaMeter>,
    local: Local,
    bell: Arc<Bell>,
    // This replica's closed time and measured lag, as its owner published them after its last
    // step; told on `moved` whenever the time moves or the owner ends (replication.md, "Follower
    // reads at a closed time").
    closed: Mutex<Closed>,
    moved: std::sync::Condvar,
}
/// What a replica's owner published of its closed time.
#[derive(Debug, Clone, Copy, Default)]
struct Closed {
    time: Option<u64>,
    lag: Option<u64>,
}
/// The readers of a replica's local reads (replication.md, "A local read on a replica"):
/// connections of their own on the machine's database, each on what the machine committed when
/// its read began. At most the descriptor budget's bound out at once, each charged to the process's page
/// budget; one given back waits in the process's account for the next read (replication.md, "A
/// replica's local readers bounded by derivation"). While the owner waits to start the database's
/// log again, no read of this replica begins (replication.md, "A replica's database log").
struct Local {
    /// Opens a reader; none once the owner ended, so that no reader holds its lease past the reads
    /// under way then.
    open: Mutex<Option<crate::store::Opener<Db>>>,
    /// This replica's number in the account.
    id: u64,
    /// Counts the reads and refusals, and the readers out (`amphora_replica_local_*`).
    meter: Arc<ReplicaMeter>,
    /// The process's readers and the page budget they are charged to.
    account: Arc<Account>,
    /// Bounds the readers out at once on this replica: at most the clients that can be attached
    /// at once, each a connection running one session at a time, so more readers than that can't
    /// be used by them; a caller in the process past it waits. A connection holds a descriptor, so
    /// that is the process's descriptor budget's bound as it stands at each read
    /// (local-owner-transport.md, "One descriptor budget"); the readers themselves hold none.
    descriptors: Arc<crate::descriptors::Descriptors>,
}
/// Every replica's local readers in the process, and the page budget they are charged to
/// (replication.md, "A replica's local readers bounded by derivation").
struct Account {
    pool: Arc<crate::db::SharedPages>,
    ledger: Mutex<Ledger>,
    /// Told whenever a reader is given back or a charge returned.
    freed: std::sync::Condvar,
    /// The number the next replica takes.
    next: AtomicU64,
    /// The replica logs growing on each filesystem, by its number (`Machine::filesystem`): each
    /// counted while its owner's flushing thread runs ([`LogShare`]). At most one entry a live log.
    logs: Mutex<std::collections::BTreeMap<u64, u64>>,
}
/// One replica log among those that grow on its filesystem (replication.md, "A replica's database
/// log bounded by what it measures"): counted from its owner's flushing thread's start to its end.
struct LogShare {
    account: Arc<Account>,
    filesystem: u64,
}
impl LogShare {
    fn join(account: Arc<Account>, filesystem: u64) -> Result<Self, Refusal> {
        let mut logs = locked(&account.logs)?;
        let count = logs.entry(filesystem).or_insert(0);
        *count = count.checked_add(1).ok_or_else(|| {
            unavailable("the replica logs on a filesystem overflowed their count")
        })?;
        drop(logs);
        Ok(Self {
            account,
            filesystem,
        })
    }
    /// The replica logs on this one's filesystem, itself among them.
    fn logs(&self) -> Result<u64, Refusal> {
        Ok(locked(&self.account.logs)?
            .get(&self.filesystem)
            .copied()
            .unwrap_or(1))
    }
}
impl Drop for LogShare {
    fn drop(&mut self) {
        // A poisoned count keeps this log in it: an overcount, so the others take less, not more.
        if let Ok(mut logs) = self.account.logs.lock()
            && let Some(count) = logs.get_mut(&self.filesystem)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                logs.remove(&self.filesystem);
            }
        }
    }
}
/// The account's readers: those given back, oldest first, and how many each replica has out.
/// Its idle readers are at most what the budget charges (each holds `db::READER_PAGES`), and it
/// names a replica only while that replica's readers are out, or while its owner holds its reads
/// back to start its database's log again (`restarting`, at most one entry a replica).
#[derive(Default)]
struct Ledger {
    idle: VecDeque<Idle>,
    out: std::collections::BTreeMap<u64, usize>,
    restarting: std::collections::BTreeSet<u64>,
}
struct Idle {
    replica: u64,
    reader: Db,
    charge: crate::db::Charge,
}
impl Account {
    fn new(pool: Arc<crate::db::SharedPages>) -> Arc<Self> {
        Arc::new(Self {
            pool,
            ledger: Mutex::new(Ledger::default()),
            freed: std::sync::Condvar::new(),
            next: AtomicU64::new(0),
            logs: Mutex::default(),
        })
    }
    /// Tells the waiting reads that a reader or a charge is free: under the ledger's lock, so a
    /// read that found none and is about to wait can't miss it.
    fn free(&self) {
        let ledger = self.ledger.lock();
        self.freed.notify_all();
        drop(ledger);
    }
}
/// The process's account, over its page pool (`db::process_pages`); a test's own, where it set
/// one for the owners it opens on its thread.
fn account() -> Arc<Account> {
    #[cfg(test)]
    if let Some(account) = tests::ACCOUNT.with(|account| account.borrow().clone()) {
        return account;
    }
    static ACCOUNT: std::sync::OnceLock<Arc<Account>> = std::sync::OnceLock::new();
    ACCOUNT
        .get_or_init(|| Account::new(crate::db::process_pages()))
        .clone()
}
/// The pages a reader is charged: what it keeps of its own (`db::READER_PAGES`).
fn reader_pages() -> Result<usize, Refusal> {
    usize::try_from(crate::db::READER_PAGES).map_err(unavailable)
}
impl Local {
    fn new(
        open: crate::store::Opener<Db>,
        meter: Arc<ReplicaMeter>,
        account: Arc<Account>,
        descriptors: Arc<crate::descriptors::Descriptors>,
    ) -> Self {
        Self {
            open: Mutex::new(Some(open)),
            id: account.next.fetch_add(1, Ordering::Relaxed),
            meter,
            account,
            descriptors,
        }
    }
    /// `f` on a reader: this replica's idle one, one opened now on a charge, one opened after
    /// another replica's idle reader closed for its charge, or the first that comes free before
    /// `until`.
    fn with<R>(
        &self,
        until: Instant,
        f: impl FnOnce(&mut Db) -> anyhow::Result<R>,
    ) -> Result<R, Refusal> {
        let pages = reader_pages()?;
        let taken = loop {
            let Some(open) = locked(&self.open)?.clone() else {
                self.meter.local_refused(LocalRefusal::Fenced);
                return Err(fenced("replica owner ended"));
            };
            let mut ledger = locked(&self.account.ledger)?;
            let held = ledger.restarting.contains(&self.id);
            let own = ledger
                .idle
                .iter()
                .position(|idle| idle.replica == self.id)
                .filter(|_| !held);
            if let Some(idle) = own.and_then(|at| ledger.idle.remove(at)) {
                self.took(&mut ledger)?;
                break (Some(idle.reader), idle.charge, open);
            }
            if !held && ledger.out.get(&self.id).copied().unwrap_or(0) < self.descriptors.bound() {
                if let Some(charge) = self.account.pool.charge(pages) {
                    self.took(&mut ledger)?;
                    break (None, charge, open);
                }
                let other = ledger.idle.iter().position(|idle| idle.replica != self.id);
                if let Some(closed) = other.and_then(|at| ledger.idle.remove(at)) {
                    // Closed outside the lock; its charge, given back, is this read's next try.
                    drop(ledger);
                    drop(closed);
                    continue;
                }
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                self.meter.local_refused(LocalRefusal::Timeout);
                return Err(Refusal::Timeout);
            }
            drop(
                self.account
                    .freed
                    .wait_timeout(ledger, left)
                    .map_err(|_| unavailable("the local readers' account lock was poisoned"))?,
            );
        };
        // A reader given back, or one opened now on the charge taken for it.
        let (reader, charge, open) = taken;
        let mut reader = match reader.map_or_else(|| open(), Ok) {
            Ok(reader) => reader,
            Err(error) => {
                drop(charge);
                self.give_back(None);
                return Err(refused(&error));
            }
        };
        self.meter.local_read();
        let answer = f(&mut reader).map_err(|error| refused(&error));
        self.give_back(Some((reader, charge)));
        answer
    }
    /// One more reader out on this replica, under the ledger's lock.
    fn took(&self, ledger: &mut Ledger) -> Result<(), Refusal> {
        let out = ledger.out.entry(self.id).or_default();
        *out = out
            .checked_add(1)
            .ok_or_else(|| fenced("a replica's readers out overflowed"))?;
        self.meter.readers_in_use(*out);
        Ok(())
    }
    /// A reader given back: kept in the account for the next read while the owner runs, closed
    /// (outside the lock) once it ended. `out` never falls below none: each give-back follows
    /// one taken.
    fn give_back(&self, reader: Option<(Db, crate::db::Charge)>) {
        let live = self.open.lock().is_ok_and(|open| open.is_some());
        let closed = match self.account.ledger.lock() {
            Ok(mut ledger) => {
                let out = ledger
                    .out
                    .get(&self.id)
                    .copied()
                    .unwrap_or(0)
                    .saturating_sub(1);
                if out == 0 {
                    ledger.out.remove(&self.id);
                } else {
                    ledger.out.insert(self.id, out);
                }
                self.meter.readers_in_use(out);
                match reader {
                    Some((reader, charge)) if live => {
                        ledger.idle.push_back(Idle {
                            replica: self.id,
                            reader,
                            charge,
                        });
                        None
                    }
                    reader => reader,
                }
            }
            Err(_) => reader,
        };
        drop(closed);
        self.account.free();
    }
    /// The owner is to start the database's log again: no read of this replica begins until it
    /// has.
    fn hold_reads(&self) -> Result<(), Refusal> {
        locked(&self.account.ledger)?.restarting.insert(self.id);
        Ok(())
    }
    /// `restart`, once no read of this replica is out, its reads held back while it runs: none
    /// while one is out. Reads begin again once it started the log (`Ok(true)`).
    fn restart_unread(
        &self,
        restart: impl FnOnce() -> anyhow::Result<bool>,
    ) -> Result<Option<bool>, Refusal> {
        if locked(&self.account.ledger)?.out.contains_key(&self.id) {
            return Ok(None);
        }
        // No read of this replica begins while it is restarting, so none is out meanwhile.
        let restarted = restart().map_err(|error| refused(&error))?;
        if restarted {
            locked(&self.account.ledger)?.restarting.remove(&self.id);
            self.account.free();
        }
        Ok(Some(restarted))
    }
    /// The owner ended: no reader opens again, its idle ones close, and reads waiting are refused.
    fn release(&self) {
        if let Ok(mut open) = self.open.lock() {
            *open = None;
        }
        let closed: VecDeque<Idle> = match self.account.ledger.lock() {
            Ok(mut ledger) => {
                let (mine, others): (VecDeque<Idle>, VecDeque<Idle>) =
                    std::mem::take(&mut ledger.idle)
                        .into_iter()
                        .partition(|idle| idle.replica == self.id);
                ledger.idle = others;
                ledger.restarting.remove(&self.id);
                mine
            }
            Err(_) => VecDeque::new(),
        };
        drop(closed);
        self.account.free();
    }
}
#[derive(Default)]
struct Outgoing {
    bytes: usize,
    messages: VecDeque<Outbound>,
}
struct Reservation {
    control: Arc<Control>,
    bytes: usize,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.control
            .admitted
            .fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
struct Envelope {
    command: Input,
    _reservation: Reservation,
}
enum Input {
    Submit {
        namespace: String,
        operation: Operation,
        // The handle's application claim, checked against the floor at the session's base.
        application: Option<crate::database::Application>,
        until: Instant,
        response: Response<Answer>,
    },
    Receive {
        from: u64,
        bytes: Vec<u8>,
        until: Instant,
        response: Response<()>,
    },
    Change {
        change: MemberChange,
        until: Instant,
        response: Response<ReplicaStatus>,
    },
    Transfer {
        to: u64,
        until: Instant,
        response: Response<ReplicaStatus>,
    },
    Verdict {
        verdict: Verdict,
        until: Instant,
        response: Response<()>,
    },
    /// Where catching up each learner stands, judged at once (`ReplicaHandle::catch_up`), unless
    /// `until` has passed: a stale request stages no learner's round.
    CatchUp {
        until: Instant,
        response: Response<Vec<(u64, CatchUp)>>,
    },
    /// The application bound on this member, which its owner attests (replication.md,
    /// "Attestation"). It has no answer: an attestation not yet made only holds a raise back.
    Attest {
        application: crate::database::Application,
    },
}
/// The hand-off of the lead under way (replication.md, "Membership"): asked of the leader in
/// `term`, answered once this member follows `to`.
struct Transferring {
    to: u64,
    term: u64,
    until: Instant,
    response: Response<ReplicaStatus>,
    _reservation: Reservation,
}
/// The change of members under way (replication.md, "Membership"): one at a time, beside the
/// write under way, which neither waits for.
struct Changing {
    context: Vec<u8>,
    term: u64,
    leader: u64,
    until: Instant,
    change: MemberChange,
    stage: Stage,
    response: Response<ReplicaStatus>,
    _reservation: Reservation,
}
enum Stage {
    /// Waiting for the ReadIndex that shows this leader current and its configuration applied.
    Barrier,
    /// Proposed at `index`, in this leader's term.
    Proposed { index: u64 },
    /// Applied: the group is in the joint configuration, or has left it.
    Applied,
}
/// Whose session a pending one is, for the application floor's check at its preparation
/// (replication.md, "Writers").
enum Author {
    /// A caller's, with its handle's application claim.
    Application(Option<crate::database::Application>),
    /// The owner's own: the seed, and the floor's attestations, marker J and requests.
    Owner,
}
struct Pending {
    context: Vec<u8>,
    term: u64,
    leader: u64,
    until: Instant,
    namespace: String,
    author: Author,
    response: Option<Response<Answer>>,
    phase: Phase,
    // Keep admission charged while its command is in flight, not just while queued.
    _reservation: Reservation,
}
enum Phase {
    Barrier(Option<Operation>),
    Proposed(Answer),
    Bootstrap(Option<Operation>),
    /// A follower's write in the replicated class, answered on its group's commit with `result`,
    /// kept until its entry, at `index`, applies (`Actor::answer_committed`).
    Answered {
        result: Result<(), Refusal>,
        index: u64,
    },
}
struct Actor<F: BlockFile + 'static = DeviceFile> {
    replica: Durable<F>,
    // Keep the log owner and its exclusive lease until the replica has drained/dropped.
    _log: hyper_log::Log<F>,
    _log_owner: File,
    // Drop after the log writer has drained; reopen cannot race its final disk completion.
    _lease: Owned,
    changed: Arc<Changed>,
    control: Arc<Control>,
    pending: Option<Pending>,
    changing: Option<Changing>,
    transferring: Option<Transferring>,
    // The application this member's handles are bound to, which it keeps attested.
    attesting: Option<crate::database::Application>,
    // The applied index the floor's keeper last looked at, and the configuration_at at which it
    // found nothing left to do (joins tracked, and this member's attestation current).
    floor_checked: u64,
    floor_settled: Option<u64>,
    // What the keeper proposed, counted: this member's attestations, and as leader its requests
    // that every member attest again. Read by tests as non-vacuity counters.
    floor_attestations: u64,
    floor_requests: u64,
    // Whether one of the keeper's sessions is in flight; how many ended, applied or not; and how
    // many passes failed short of fencing. Counters read by tests.
    floor_inflight: bool,
    floor_ended: u64,
    // The owner's own sessions that reached their lease at their barrier while this member stayed
    // leader in their term (`Actor::own_session_lease`'s falsifier: a patience too short), the
    // running count the `replica.own_session_expired` event carries; the meter's
    // `amphora_replica_own_sessions_expired_total` counts the same.
    own_sessions_expired: u64,
    floor_failures: u64,
    // Whether the floor session in hand is a request to attest again, and how this owner's
    // requests ended: applied, or refused by the stale-base rule and proposed again on a later pass.
    floor_requesting: bool,
    // A floor session, a follower's write, or the leader's in its turn after a slot, past its
    // barrier, waiting for this member to apply its own log up to the last index it held then.
    tail: Option<u64>,
    floor_requests_applied: u64,
    floor_requests_stale: u64,
    // The floor version this owner last published, none before its first: a raise is it rising.
    floor_published: Option<u32>,
    // At most COMMANDS admitted operations wait behind the sole prepared transaction.
    waiting: VecDeque<Envelope>,
    // The leader's write slot (replication.md, "A follower's write under steady leader writes"):
    // the members whose writes it refused, queued, and the forwards and barrier it holds, each
    // still charged to the inbound queue by its reservation; and whether this leader's own
    // preparation waits on a slot.
    slots: super::slot::Slots<(Message, Reservation)>,
    slot_deferred: bool,
    serial: u64,
    // Every time this owner reads comes from this clock (replication.md, "The owner's time").
    clock: Clock,
    started: Instant,
    // The wall clock when this owner started, in microseconds since the Unix epoch: its stamps
    // are this plus its own clock's time since (replication.md, "Follower reads at a closed time").
    wall: u64,
    // The delays this owner measured between its closed time and its own clock.
    lag: crate::replication::closed::Lag,
    ticked: Instant,
    // Whether this step's drive left more to drive without waiting for the log
    // (`Driven::more`): the next step comes at once, without waiting for an input.
    driving: bool,
    // The log's writes and compactions wake the owner through this, its bell's waker.
    waker: std::task::Waker,
    // The instant this owner last read from its clock: at its step's start, and again after a
    // preparation, whose work takes time of its own.
    at: Instant,
    published: u64,
    transferred: Option<(u64, Instant)>,
    flushing: Flushing,
    // What this owner's restarts of its database's log held, against what it applied
    // (replication.md, "A replica's database log bounded by what it measures").
    spacing: log_bound::Spacing,
    // What this owner's images and its log's replay cost a byte, which its compactions follow
    // (replication.md, "Decision (lane constants): the compaction's expansion learned where it
    // runs"; CT.1).
    compacting: super::compacting::Costs,
    // While the database's log is past its room: why, for the submits this member refuses.
    log_full: Option<String>,
    // The input queued as this step began, its own envelope among it: what a restart in the
    // step found, on the basis each later step reads its queue.
    step_queued: u64,
    // What this owner measured of its voters' answers while it led: its quorum patience
    // (replication.md, "The leader's quorum patience").
    answers: patience::Answers,
    // The tick pace this owner measured where it runs (replication.md, "Decision (lane
    // constants): the owner's tick pace").
    pace: super::pace::Pace,
    // The ticks since the pace was last derived: it is derived once an election timeout, the
    // span the core draws each timeout over.
    paced: usize,
    // A measurement's record of this owner's stalls (kiln goal 6b112f14, step 1): test-only.
    #[cfg(test)]
    stalls: Option<Arc<Mutex<tests::Stalls>>>,
    // The replicated class's gate on held answers (replication.md, "The class stated once per
    // group"): the members the host's detector suspects, and those this owner holds suspected
    // only until its group's recorded class is its own. A member is counted held when neither
    // suspects it. Each is at most the members a configuration names (`MEMBERS`).
    detected: std::collections::BTreeSet<u64>,
    gated: std::collections::BTreeSet<u64>,
    // The term this owner proposed its class entry in, once taken: one a term.
    class_proposed: Option<u64>,
    // While set, this owner proposes no class entry (replication.md, "The class stated once per
    // group"): a test's group from before the class was stated.
    #[cfg(test)]
    class_held: bool,
    // While set, this leader holds no write slot: today's rule alone, a test's control.
    #[cfg(test)]
    slot_off: bool,
    // While set, this leader moves no stale base by its read set: the exact base alone, a test's
    // control; and the mutant the isolation checks must catch, which moves every one unchecked.
    #[cfg(test)]
    read_sets_off: bool,
    #[cfg(test)]
    read_sets_unchecked: bool,
    // Stale bases this leader moved by their read sets. Read by tests: non-vacuity.
    #[cfg(test)]
    read_sets_moved: u64,
    // The leader's writes made in its turn after a slot, and those refused as stale; and the one
    // in flight. Read by tests: a turn is a turn in fact.
    #[cfg(test)]
    turns: (u64, u64),
    #[cfg(test)]
    turn_context: Option<Vec<u8>>,
    // The steps whose drive left more to drive (`driving`): read by tests.
    #[cfg(test)]
    drove_more: u64,
}

/// The flushes of the machine's database log, in every class (docs/design/replication.md, "Fast
/// acknowledgement" and "A replica's database log"): one out at a time, off the owner's step. The
/// owner hands each the point it had applied, and takes the point back once the flush has made it
/// durable, with what the checkpoint pass after it left in the log.
enum Flushing {
    /// The owner's flushing thread; whether a flush is out; what the last checkpoint pass said.
    Thread {
        ask: mpsc::SyncSender<Ask>,
        done: mpsc::Receiver<Result<Flushed, String>>,
        out: bool,
        pass: Pass,
    },
    /// A test's: the log is flushed when the test says so (`Driven::flush`).
    #[cfg(test)]
    Held,
}

/// What the owner asks of its flushing thread: a flush of what was applied through `at`, and
/// whether a checkpoint pass follows it at once (`copy`: a restart is due, and waits for a pass
/// that copies every durable frame).
struct Ask {
    at: Point,
    copy: bool,
}
/// A flush done, and what its checkpoint pass said, if one ran.
struct Flushed {
    at: Point,
    pass: Option<Pass>,
}
/// What a checkpoint pass said of the database's log.
#[derive(Debug, Clone)]
struct Pass {
    /// It copied every frame a flush had made durable (store.md, decision 15): a restart now
    /// copies only what was applied since that flush was asked.
    caught_up: bool,
    /// It found the log busy and copied nothing: a reader on read-mark 0 holds passes off (wal.c,
    /// `walCheckpoint` takes `WAL_READ_LOCK(0)`), until reads are held.
    busy: bool,
    /// The frames the log may hold, from the filesystem's free bytes as read after the pass
    /// (`log_bound::log_room`).
    room: u64,
    /// Why the free bytes couldn't be read, if they couldn't: the log is then held to its file's
    /// room, and a submit refused past it says why.
    unread: Option<String>,
}

/// How often a checkpoint pass follows a flush while no restart is due: the standalone store's
/// checkpoint period, measured on fedora 2026-10-20 (docs/design/store.md, decision 4).
const CHECKPOINT_EVERY: Duration = Duration::from_millis(25);

/// Whether the owner holds local reads back for a restart of the database's log (replication.md,
/// "A replica's database log bounded by what it measures"): past its room, always; with a restart
/// due, once a pass copied every durable frame (the restart waits only for the reads out), or
/// once a pass found the log busy: a read that began on a log wholly copied reads the file alone
/// and holds every pass off, so only holding reads back lets the passes copy again.
fn hold_due(pass: &Pass, due: bool, past: bool) -> bool {
    past || (due && (pass.caught_up || pass.busy))
}

impl Flushing {
    /// A thread that flushes `machine`'s log for each point it's handed, then copies what the
    /// flush made durable into the database's file on a connection of its own (a passive
    /// checkpoint, which never copies past a reader's mark), and counts each in `meter`.
    fn start(
        machine: &crate::replication::Machine,
        meter: Arc<ReplicaMeter>,
        account: Arc<Account>,
        bell: Arc<Bell>,
    ) -> Result<Self, Refusal> {
        let flush = machine.log_flusher();
        let mut copier = machine.reader()().map_err(|error| refused(&error))?;
        let mut free = machine.free_space();
        let share = LogShare::join(account, machine.filesystem().map_err(unavailable)?)?;
        // The room before any pass, read as each pass reads it.
        let first = room(&copier, &mut free, &share).map_err(fenced)?;
        let (ask, asked) = mpsc::sync_channel::<Ask>(1);
        let (finished, done) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("amphora.replica.flush".into())
            .spawn(move || {
                let mut copied_at: Option<Instant> = None;
                for Ask { at, copy } in asked {
                    // The disk's time, not the owner's: a flush is real I/O however its owner's
                    // clock runs.
                    let began = Instant::now();
                    let mut result = flush()
                        .map(|()| Flushed { at, pass: None })
                        .map_err(|e| e.to_string());
                    if result.is_ok() {
                        meter.flushed(began.elapsed());
                    }
                    let due =
                        copy || copied_at.is_none_or(|then| then.elapsed() >= CHECKPOINT_EVERY);
                    if let Ok(flushed) = result.as_mut()
                        && due
                    {
                        let began = Instant::now();
                        copied_at = Some(began);
                        let said = match copier.checkpoint() {
                            // A busy pass copied nothing and says nothing of the log: not counted.
                            Ok(copied) if copied == Checkpointed::default() => Ok(None),
                            Ok(copied) => {
                                meter.checkpointed(began.elapsed());
                                Ok(Some(copied))
                            }
                            // The file's state is unknown after a failed copy: the owner fences.
                            Err(e) => Err(format!("a checkpoint failed: {e:#}")),
                        };
                        match said.and_then(|copied| {
                            let (room, unread) = room(&copier, &mut free, &share)?;
                            let through = copier.durable_frame();
                            let caught_up = copied.is_some_and(|c| {
                                c.left == 0
                                    || through.is_some_and(|d| c.log.saturating_sub(c.left) >= d)
                            });
                            Ok(Pass {
                                caught_up,
                                busy: copied.is_none(),
                                room,
                                unread,
                            })
                        }) {
                            Ok(pass) => flushed.pass = Some(pass),
                            Err(e) => result = Err(e),
                        }
                    }
                    if finished.send(result).is_err() {
                        return;
                    }
                    bell.ring();
                }
            })
            .map_err(unavailable)?;
        let (room, unread) = first;
        Ok(Self::Thread {
            ask,
            done,
            out: false,
            pass: Pass {
                caught_up: false,
                busy: false,
                room,
                unread,
            },
        })
    }
}

/// The most an image of this owner's machine may take now: the memory budget's room for one (SB.1),
/// no more than one wire record carries ([`WIRE_IMAGE_BYTES`]).
fn owner_image_room() -> Result<usize, Refusal> {
    Ok(crate::replication::image_room()?.min(WIRE_IMAGE_BYTES))
}

/// The frames the database's log may hold now, read on the flushing thread: its file's room and
/// an equal share of what its filesystem has free (`log_bound::log_room`). Free bytes that can't
/// be read give no share, and say why; a file whose length can't be read fails the pass.
fn room(
    copier: &Db,
    free: &mut crate::replication::FreeSpace,
    share: &LogShare,
) -> Result<(u64, Option<String>), String> {
    let kept = copier
        .log_frames()
        .map_err(|e| format!("the database log's length couldn't be read: {e:#}"))?;
    let logs = share.logs().map_err(|e| e.to_string())?;
    Ok(match free() {
        Ok(available) => {
            let image = owner_image_room().map_err(|e| e.to_string())?;
            let image = u64::try_from(image).unwrap_or(u64::MAX);
            (log_bound::log_room(available, image, logs, kept), None)
        }
        Err(e) => (
            kept,
            Some(format!("its filesystem's free bytes couldn't be read: {e}")),
        ),
    })
}

/// An owner's time, from its shell: the real clock for the owner thread, one its test moves for a
/// test shell.
type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// The nanoseconds from `from` to `to` on the owner's clock, none if it went back.
fn nanos_between(from: Instant, to: Instant) -> u64 {
    u64::try_from(to.saturating_duration_since(from).as_nanos()).unwrap_or(u64::MAX)
}

fn fenced(reason: impl std::fmt::Display) -> Refusal {
    Refusal::Fenced(reason.to_string())
}
fn unknown() -> Refusal {
    Refusal::UnknownOutcome
}
/// Counts a refusal the floor made and says so on its target (replication.md, "Metrics").
fn floor_refused(control: &Control, reason: FloorRefusal, refusal: &Refusal) {
    control.floor_meter.refused(reason);
    tracing::info!(
        target: "amphora.floor",
        event = "floor.refused",
        reason = reason.reason(),
        error = %refusal,
        "the application floor refused a request"
    );
}
fn raft_error(error: hyper_durable::ReplicaError) -> Refusal {
    match error {
        hyper_durable::ReplicaError::Fenced(cause) => fenced(cause),
        other => unavailable(other),
    }
}
fn not_leader(leader: u64) -> Refusal {
    Refusal::NotLeader {
        leader: if leader == 0 { None } else { Some(leader) },
    }
}
fn answer<T>(response: Response<T>, result: Result<T, Refusal>) {
    // Failure means the deadline-expired caller relinquished its reply. Committed work remains
    // applied and published; dropping this private response never drops a database update.
    let _ = response.send(result);
}

impl ReplicaHandle {
    fn enqueue(&self, command: Input, bytes: usize) -> Result<(), Refusal> {
        // Untouched application calls can wait for leadership. Keep room for one maximum wire
        // record against application pressure so that waiting calls cannot block that discovery.
        let application_limit = QUEUED_BYTES
            .checked_sub(WIRE_BYTES)
            .ok_or_else(|| fenced("replica wire reservation exceeds command queue budget"))?;
        let mut charged = self.shared.admitted.load(Ordering::Acquire);
        let mut reserved = false;
        for _ in 0..COMMANDS {
            let next = charged.checked_add(bytes).ok_or(Refusal::Full {
                limit: QUEUED_BYTES,
            })?;
            if matches!(
                &command,
                Input::Submit { .. }
                    | Input::Change { .. }
                    | Input::Transfer { .. }
                    | Input::CatchUp { .. }
            ) && next > application_limit
            {
                return Err(Refusal::Full {
                    limit: application_limit,
                });
            }
            api::bound("replica command queue bytes", next, QUEUED_BYTES)?;
            match self.shared.admitted.compare_exchange_weak(
                charged,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    reserved = true;
                    break;
                }
                Err(now) => charged = now,
            }
        }
        if !reserved {
            return Err(Refusal::Full { limit: COMMANDS });
        }
        let envelope = Envelope {
            command,
            _reservation: Reservation {
                control: Arc::clone(&self.shared),
                bytes,
            },
        };
        self.sender
            .try_send(envelope)
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => Refusal::Full { limit: COMMANDS },
                mpsc::TrySendError::Disconnected(_) => {
                    fenced("replica owner ended; reopen from durable state")
                }
            })?;
        self.shared.bell.ring();
        Ok(())
    }

    pub(super) fn submit(
        &self,
        namespace: String,
        operation: Operation,
        application: Option<crate::database::Application>,
        until: Instant,
    ) -> Result<Answer, Refusal> {
        if Instant::now() >= until {
            return Err(Refusal::Timeout);
        }
        let (response, receiver) = mpsc::sync_channel(1);
        self.enqueue(
            Input::Submit {
                namespace,
                operation,
                application,
                until,
                response,
            },
            api::REQUEST_MAX,
        )?;
        receiver
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => unknown(),
                mpsc::RecvTimeoutError::Disconnected => unknown(),
            })?
    }

    fn enqueue_receive(
        &self,
        from: u64,
        bytes: Vec<u8>,
        until: Instant,
    ) -> Result<mpsc::Receiver<Result<(), Refusal>>, Refusal> {
        if Instant::now() >= until {
            return Err(Refusal::Timeout);
        }
        api::bound("replica wire record", bytes.len(), WIRE_BYTES)?;
        let charge = bytes.len();
        let (response, receiver) = mpsc::sync_channel(1);
        self.enqueue(
            Input::Receive {
                from,
                bytes,
                until,
                response,
            },
            charge,
        )?;
        Ok(receiver)
    }

    /// `from` must be the identity authenticated by the host's transport, not a wire claim.
    /// Success waits until the actor processes the record; it does not acknowledge a commit.
    pub fn receive(&self, from: u64, bytes: Vec<u8>, until: Instant) -> Result<(), Refusal> {
        let receiver = self.enqueue_receive(from, bytes, until)?;
        receiver
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => Refusal::Timeout,
                mpsc::RecvTimeoutError::Disconnected => fenced("replica owner ended"),
            })?
    }

    /// Transfers a wire record to the existing bounded actor queue without waiting for step.
    /// `from` is the host-authenticated identity. The host must reap the returned receipt to
    /// observe later validation, expiry and fencing; successful admission is not a durability ACK.
    /// Dropping the receipt abandons observation; it does not cancel or replay the queued record.
    /// Hosts must bound their retained receipts as well as handling the queue's admission limit.
    pub fn admit_receive(
        &self,
        from: u64,
        bytes: Vec<u8>,
        until: Instant,
    ) -> Result<ReceiveReceipt, Refusal> {
        Ok(ReceiveReceipt {
            receiver: Mutex::new(self.enqueue_receive(from, bytes, until)?),
            until,
            completed: false,
        })
    }

    /// Takes at most `limit` messages. An undrained host eventually fences explicitly at the
    /// egress bound; the actor never acknowledges a send the host has not taken.
    pub fn drain(&self, limit: usize) -> Result<Vec<Outbound>, Refusal> {
        api::bound("replica drain count", limit, OUTGOING_MESSAGES)?;
        let mut outgoing = locked(&self.shared.outgoing)?;
        if outgoing.messages.is_empty()
            && let Err(error) = &*locked(&self.shared.status)?
        {
            return Err(error.clone());
        }
        let mut messages = Vec::with_capacity(limit.min(outgoing.messages.len()));
        for _ in 0..limit {
            let Some(message) = outgoing.messages.pop_front() else {
                break;
            };
            outgoing.bytes = outgoing
                .bytes
                .checked_sub(message.bytes.len())
                .ok_or_else(|| fenced("outgoing accounting underflow"))?;
            messages.push(message);
        }
        Ok(messages)
    }

    pub fn status(&self) -> Result<ReplicaStatus, Refusal> {
        locked(&self.shared.status)?.clone()
    }

    /// Makes `change` to the group's members (replication.md, "Membership"). Only the leader takes
    /// one, one at a time. It proposes it through a joint configuration, which it leaves by itself,
    /// and answers its status once the group has left it. A change the configuration already
    /// shows proposes nothing and answers at once. A change whose deadline passes once its joint
    /// configuration is applied answers its status too: it is committed, the status shows the
    /// group still joint (`voters_outgoing`), and the leader leaves it by itself. Refused: off the
    /// leader, naming the leader known (`NotLeader`); while another change or a hand-off is under
    /// way, while the group is leaving a joint configuration or has a change still to apply, or
    /// while it takes its seed (`Unavailable`); when the configuration can't take it (`Invalid`),
    /// or past the members a group may have, or a learner past the snapshot ceiling (`TooLarge`).
    /// UnknownOutcome: it was proposed, and the lead moved or the deadline passed before it was
    /// seen applied. Read the configuration before trying again.
    pub fn change(&self, change: MemberChange, until: Instant) -> Result<ReplicaStatus, Refusal> {
        if Instant::now() >= until {
            return Err(Refusal::Timeout);
        }
        let (response, receiver) = mpsc::sync_channel(1);
        self.enqueue(
            Input::Change {
                change,
                until,
                response,
            },
            CHANGE_BYTES,
        )?;
        receiver
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => unknown(),
                mpsc::RecvTimeoutError::Disconnected => unknown(),
            })?
    }

    /// Keeps `application` attested on this member (replication.md, "Attestation"): its owner
    /// proposes the member's attestation once it has applied what the group had committed, and
    /// again whenever a change or a request makes it stale. Nothing is answered.
    pub(crate) fn attest(&self, application: crate::database::Application) -> Result<(), Refusal> {
        application.validate()?;
        // A binary can't raise its highest by waiting: a bind below the floor fails closed now.
        if let Some(floor) = locked(&self.shared.floor)?.as_ref()
            && floor.name == application.name
            && application.version < floor.version
        {
            let refusal = api::invalid(
                "application floor",
                &format!(
                    "this binary runs up to {}; the group's floor is {}",
                    application.version, floor.version
                ),
            );
            floor_refused(&self.shared, FloorRefusal::BelowFloor, &refusal);
            return Err(refusal);
        }
        let mut attesting = locked(&self.shared.attesting)?;
        if attesting.as_ref() == Some(&application) {
            return Ok(());
        }
        let charge = CHANGE_BYTES.saturating_add(application.name.len());
        self.enqueue(
            Input::Attest {
                application: application.clone(),
            },
            charge,
        )?;
        *attesting = Some(application);
        Ok(())
    }

    /// Hands the lead to the voter `to` (replication.md, "Membership"), so that the leader can then
    /// be demoted or removed by its successor. Only the leader takes one, and not while a change of
    /// members, a joint configuration or the group's seed is under way. It answers its status once
    /// it follows `to`, at once when it is `to`. Refused: off the leader, or once the lead went to
    /// another, naming the leader known (`NotLeader`); while a change or another hand-off is under
    /// way, or when `to` did not take the lead within an election timeout (`Unavailable`); to a
    /// member that does not vote (`Invalid`). UnknownOutcome: the deadline passed first; read the
    /// status.
    pub fn transfer(&self, to: u64, until: Instant) -> Result<ReplicaStatus, Refusal> {
        if Instant::now() >= until {
            return Err(Refusal::Timeout);
        }
        let (response, receiver) = mpsc::sync_channel(1);
        self.enqueue(
            Input::Transfer {
                to,
                until,
                response,
            },
            CHANGE_BYTES,
        )?;
        receiver
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => unknown(),
                mpsc::RecvTimeoutError::Disconnected => unknown(),
            })?
    }

    /// The replicated class's counters (`crate::meter::REPLICA_METRICS`), for the host to export:
    /// read without asking the owner, and no wire type.
    pub fn counters(&self) -> ReplicaCounters {
        self.shared.meter.counters()
    }

    /// The application floor's counters (`crate::meter::FLOOR_METRICS`), for the host to export:
    /// read without asking the owner.
    pub fn floor_counters(&self) -> FloorCounters {
        self.shared.floor_meter.counters()
    }

    /// The floor and each member against it, as the owner published them at its last applied
    /// index (replication.md, "The seam"), without asking it.
    pub fn floor_status(&self) -> Result<amphora_api::replication::FloorStatus, Refusal> {
        Ok(locked(&self.shared.floor_status)?.clone())
    }

    /// Counts a refusal the floor made outside the owner (a raise the stale-base rule caught).
    pub(crate) fn floor_refused(&self, reason: FloorRefusal, refusal: &Refusal) {
        floor_refused(&self.shared, reason, refusal);
    }

    /// Tells the owner what the host's node-level failure detector concluded of a member's node
    /// (docs/design/replication.md, "The suspicion feed"). Success waits until the owner has taken
    /// it. A verdict of this member or of a node that isn't a member is refused, typed; a reopened
    /// owner trusts every member until the host tells it again.
    pub fn verdict(&self, verdict: Verdict, until: Instant) -> Result<(), Refusal> {
        if Instant::now() >= until {
            return Err(Refusal::Timeout);
        }
        let (response, receiver) = mpsc::sync_channel(1);
        self.enqueue(
            Input::Verdict {
                verdict,
                until,
                response,
            },
            std::mem::size_of::<Verdict>(),
        )?;
        receiver
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => Refusal::Timeout,
                mpsc::RecvTimeoutError::Disconnected => fenced("replica owner ended"),
            })?
    }

    /// Where catching up each of the group's learners stands, as this leader judges it now
    /// (replication.md, "Membership"; hyper-raft's `Raft::catch_up`). Asked first, a learner is
    /// staged, its first round sending what the leader holds then; asked again, it is judged as an
    /// election timeout passes, and its rounds end as its answers arrive. A new leader stages its
    /// learners afresh. Promoting a learner is the host's change to make. Refused off the leader,
    /// naming the leader known (`NotLeader`); past `until` when the owner takes it (`Timeout`),
    /// staging nothing.
    pub fn catch_up(&self, until: Instant) -> Result<Vec<(u64, CatchUp)>, Refusal> {
        if Instant::now() >= until {
            return Err(Refusal::Timeout);
        }
        let (response, receiver) = mpsc::sync_channel(1);
        self.enqueue(Input::CatchUp { until, response }, CHANGE_BYTES)?;
        receiver
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => Refusal::Timeout,
                mpsc::RecvTimeoutError::Disconnected => fenced("replica owner ended"),
            })?
    }
}

pub(super) fn start(
    path: &Path,
    log_path: &Path,
    config: ReplicaConfig,
    durability: Durability,
    changed: Arc<Changed>,
    lease: Owned,
    until: Instant,
) -> Result<Client, Refusal> {
    let (client, mut actor, receiver) = open(
        path,
        log_path,
        config,
        durability,
        changed,
        lease,
        until,
        Arc::new(Instant::now),
    )?;
    std::thread::Builder::new()
        .name("amphora.replica".into())
        .spawn(move || actor.run(receiver))
        .map_err(unavailable)?;
    Ok(client)
}

/// An owner and its handle, not yet running: `start` runs it on the owner thread, and a test
/// shell steps it itself. `clock` is its shell's; the owner starts at its first reading.
#[allow(clippy::too_many_arguments)]
fn open(
    path: &Path,
    log_path: &Path,
    config: ReplicaConfig,
    durability: Durability,
    changed: Arc<Changed>,
    lease: Owned,
    until: Instant,
    clock: Clock,
) -> Result<(Client, Actor, mpsc::Receiver<Envelope>), Refusal> {
    if Instant::now() >= until {
        return Err(Refusal::Timeout);
    }
    validate(&config)?;
    validate_durability(&config, &durability)?;
    let log_owner = exclusive_file_of(log_path, &lease)?;
    let mut machine = crate::replication::Machine::open_with_owner(
        path,
        ConfState {
            voters: config.voters.clone(),
            learners: config.learners.clone(),
            ..ConfState::default()
        },
        lease.clone(),
        config.node_id,
        config.bootstrap.clone(),
        matches!(durability, Durability::Replicated(_)),
    )?;
    machine.set_snapshot_bytes(WIRE_IMAGE_BYTES);
    let file = DeviceFile::open(
        log_path,
        true,
        CachingRequest::Buffered,
        hyper_block::buf::Alignment::new(4096).map_err(unavailable)?,
    )
    .map_err(unavailable)?;
    open_on(
        machine, log_owner, file, config, durability, changed, lease, until, clock,
    )
}

/// The write slot's bounds, stated from the core's, so the slot adds no number of its own: a
/// member's first slot is the core's election timeout in this owner's ticks of `period`, its pace
/// in force; a learned gap is believed for one to two of its member's own recurrences (`slot`). A
/// retry is not learned when the host's detector suspected its member meanwhile
/// (`slot::Slots::keep`).
fn slot_bounds(election_tick: usize, period: Duration) -> Result<super::slot::Bounds, Refusal> {
    Ok(super::slot::Bounds {
        election: u32::try_from(election_tick)
            .ok()
            .and_then(|count| period.checked_mul(count))
            .ok_or_else(|| fenced("the write slot's bounds overflowed"))?,
    })
}

/// [`open`] past its machine and its log's device: `file` is the system's device file, or a
/// test's simulated one (hyper-block's `SimHandle`), whose power the test can cut.
#[allow(clippy::too_many_arguments)]
fn open_on<F: BlockFile + 'static>(
    machine: crate::replication::Machine,
    log_owner: File,
    file: F,
    config: ReplicaConfig,
    durability: Durability,
    changed: Arc<Changed>,
    lease: Owned,
    until: Instant,
    clock: Clock,
) -> Result<(Client, Actor<F>, mpsc::Receiver<Envelope>), Refusal> {
    let mut machine = machine;
    machine.state_class(&durability)?;
    let settings = settings(config.node_id, &durability)?;
    let meter = Arc::new(ReplicaMeter::default());
    let bell = Bell::new();
    let flushing = Flushing::start(&machine, Arc::clone(&meter), account(), Arc::clone(&bell))?;
    let reader = machine.reader();
    let log_config = hyper_log::Config {
        segment_bytes: 64 << 20,
        max_segments: 16,
        max_groups: 1,
        group_entries: 65_536,
        // A 256 MiB seed needs its chunk envelopes and election/configuration entries too.
        // 512 MiB payload admission stays below the 16 x 64 MiB segment ring (plus fixed
        // persist metadata). Even one 32 MiB chunk per segment uses at most nine segments;
        // seven remain, including hyper-log's final segment reserved for making room.
        group_bytes: 512 << 20,
        group_cache: 64 << 20,
        queue_submissions: 16,
        waits: hyper_log::Waits::Never,
    };
    let log = if file.len().map_err(unavailable)? == 0 {
        if machine.durable().index != 0 {
            return Err(fenced("state exists but its replication log is missing"));
        }
        hyper_log::Log::create(file, log_config, u128::from(config.node_id)).map_err(unavailable)?
    } else {
        let (log, recovery) = hyper_log::Log::open(file, log_config, u128::from(config.node_id))
            .map_err(unavailable)?;
        if !recovery.damaged.is_empty() {
            return Err(fenced("replication log recovery found acknowledged damage"));
        }
        log
    };
    let group = GroupStore::claim(&log, 1).map_err(unavailable)?;
    let replica =
        Replica::open(&settings, group, machine, Bytes::new(512 << 20)).map_err(unavailable)?;
    let local = Local::new(
        reader,
        Arc::clone(&meter),
        account(),
        crate::descriptors::process()?,
    );
    let control = Arc::new(Control {
        admitted: AtomicUsize::new(0),
        attesting: Mutex::new(None),
        floor: Mutex::new(None),
        floor_status: Mutex::default(),
        floor_meter: FloorMeter::default(),
        outgoing: Mutex::default(),
        status: Mutex::new(Ok(status(&replica))),
        meter,
        local,
        bell: Arc::clone(&bell),
        closed: Mutex::new(Closed {
            time: replica.machine().closed(),
            lag: None,
        }),
        moved: std::sync::Condvar::new(),
    });
    let election = replica.core().raft.config().election_tick;
    let slots = super::slot::Slots::new(slot_bounds(election, TICK)?);
    let (sender, receiver) = mpsc::sync_channel(COMMANDS);
    let at = clock();
    let mut actor = Actor {
        published: replica.applied().index,
        flushing,
        replica,
        _log: log,
        _log_owner: log_owner,
        _lease: lease,
        changed,
        control: Arc::clone(&control),
        pending: None,
        changing: None,
        transferring: None,
        attesting: None,
        floor_checked: 0,
        floor_settled: None,
        floor_attestations: 0,
        floor_requests: 0,
        floor_inflight: false,
        floor_ended: 0,
        own_sessions_expired: 0,
        floor_failures: 0,
        floor_requesting: false,
        tail: None,
        floor_requests_applied: 0,
        floor_requests_stale: 0,
        floor_published: None,
        waiting: VecDeque::new(),
        slots,
        slot_deferred: false,
        serial: 0,
        transferred: None,
        spacing: log_bound::Spacing::default(),
        compacting: super::compacting::Costs::default(),
        log_full: None,
        step_queued: 0,
        answers: patience::Answers::new(election),
        pace: super::pace::Pace::new(TICK, 0),
        paced: 0,
        #[cfg(test)]
        stalls: None,
        #[cfg(test)]
        class_held: false,
        #[cfg(test)]
        slot_off: false,
        #[cfg(test)]
        read_sets_off: false,
        #[cfg(test)]
        read_sets_unchecked: false,
        #[cfg(test)]
        read_sets_moved: 0,
        #[cfg(test)]
        turns: (0, 0),
        #[cfg(test)]
        turn_context: None,
        #[cfg(test)]
        drove_more: 0,
        detected: std::collections::BTreeSet::new(),
        gated: std::collections::BTreeSet::new(),
        class_proposed: None,
        clock,
        started: at,
        wall: wall_micros()?,
        lag: crate::replication::closed::Lag::default(),
        ticked: at,
        driving: false,
        waker: std::task::Waker::from(Arc::clone(&bell)),
        at,
    };
    actor.refresh()?;
    actor.keep_class_gate()?;
    if Instant::now() >= until {
        return Err(Refusal::Timeout);
    }
    let client = Client {
        handle: ReplicaHandle {
            sender,
            _ring: Ring(bell),
            shared: control,
        },
    };
    Ok((client, actor, receiver))
}

pub(super) fn validate(config: &ReplicaConfig) -> Result<(), Refusal> {
    let count = config
        .voters
        .len()
        .checked_add(config.learners.len())
        .ok_or_else(|| api::invalid("replica configuration", "member count overflow"))?;
    api::bound("replica members", count, MEMBERS)?;
    if config.node_id == 0 || config.voters.is_empty() {
        return Err(api::invalid(
            "replica configuration",
            "nonzero identity and voters required",
        ));
    }
    let mut unique = std::collections::BTreeSet::new();
    for id in config.voters.iter().chain(&config.learners) {
        if *id == 0 || !unique.insert(*id) {
            return Err(api::invalid(
                "replica configuration",
                "members must be nonzero, unique and disjoint",
            ));
        }
    }
    if let Some(bootstrap) = &config.bootstrap
        && !config.voters.contains(&bootstrap.source_node)
    {
        return Err(api::invalid(
            "bootstrap source",
            "source must be an initial voter",
        ));
    }
    if !unique.contains(&config.node_id) {
        return Err(api::invalid(
            "replica configuration",
            "local identity is not a member",
        ));
    }
    Ok(())
}
/// The replicated class's configuration (docs/design/replication.md, "Fast acknowledgement"): every
/// member states one fate, and the voters span at least the two or more fates a write's holders
/// must. A configuration that can't is refused, typed, naming what it spans.
pub(super) fn validate_durability(
    config: &ReplicaConfig,
    durability: &Durability,
) -> Result<(), Refusal> {
    let Durability::Replicated(fates) = durability else {
        return Ok(());
    };
    if fates.spanning < 2 {
        return Err(api::invalid(
            "replicated class",
            "a write's holders must span at least two fates",
        ));
    }
    let members: std::collections::BTreeSet<u64> = config
        .voters
        .iter()
        .chain(&config.learners)
        .copied()
        .collect();
    let mut stated = std::collections::BTreeMap::new();
    for (member, fate) in &fates.of {
        if !members.contains(member) {
            return Err(api::invalid(
                "replicated class",
                "a fate is stated for a node that isn't a member",
            ));
        }
        if stated.insert(*member, *fate).is_some() {
            return Err(api::invalid(
                "replicated class",
                "a member's fate is stated twice",
            ));
        }
    }
    if stated.len() != members.len() {
        return Err(api::invalid(
            "replicated class",
            "every member states its fate",
        ));
    }
    let spanned: std::collections::BTreeSet<u64> = config
        .voters
        .iter()
        .filter_map(|voter| stated.get(voter).copied())
        .collect();
    let asked = usize::try_from(fates.spanning).map_err(unavailable)?;
    if spanned.len() < asked {
        return Err(api::invalid(
            "replicated class",
            &format!(
                "the voters span {} fates, fewer than the {asked} a write's holders must span",
                spanned.len()
            ),
        ));
    }
    Ok(())
}
fn settings(id: u64, durability: &Durability) -> Result<hyper_durable::Settings, Refusal> {
    let mut limits = hyper_raft::Limits::derive(hyper_raft::Stated {
        message: MESSAGE_BYTES,
        members: MEMBERS,
        memory: 64 << 20,
        depth: 1,
    })
    .map_err(unavailable)?;
    limits.pending_messages = OUTGOING_MESSAGES;
    limits.pending_reads = COMMANDS;
    limits.unstable_entries = 1024;
    limits.entries_per_message = 1024;
    limits.proposals = 1;
    // The replicated class: a commit on entries held in memory across fates (hyper-raft R-8), and a
    // leader applying what its own term committed before its own write is durable (R-6).
    let fates = match durability {
        Durability::Persisted => None,
        Durability::Replicated(fates) => {
            let mut of = fates.of.clone();
            of.sort_unstable();
            Some(hyper_raft::Fates {
                of,
                spanning: usize::try_from(fates.spanning).map_err(unavailable)?,
            })
        }
    };
    let apply_unpersisted = fates.is_some();
    Ok(hyper_durable::Settings {
        core: hyper_raft::Config {
            fates,
            apply_unpersisted,
            check_quorum: true,
            pre_vote: true,
            max_size_per_msg: u64::try_from(MESSAGE_BYTES).map_err(unavailable)?,
            max_uncommitted_size: 64 << 20,
            max_committed_size_per_ready: 64 << 20,
            max_inflight_msgs: 4,
            max_inflight_bytes: 64 << 20,
            seed: id,
            ..hyper_raft::Config::new(id, limits)
        },
        // CP-T2.2: preserve flush-proved group durability during API reconciliation.
        disk: hyper_durable::Disk::Flushed,
        elections: hyper_raft::Elections::Ticks,
        quiet: Duration::from_millis(50),
    })
}
fn status<F: BlockFile + 'static>(replica: &Durable<F>) -> ReplicaStatus {
    ReplicaStatus {
        node_id: replica.id(),
        leader: replica.leader(),
        term: replica.term(),
        applied: replica.applied().index,
        durable_commit: replica.durable_commit(),
        bootstrapping: replica.machine().bootstrap_pending(),
        voters: replica.configuration().voters.clone(),
        learners: replica.configuration().learners.clone(),
        fenced: replica.fenced().map(ToString::to_string),
        voters_outgoing: replica.configuration().voters_outgoing.clone(),
    }
}

/// An owner dropped without its loop's end, as a crash or a test's cut drops it, ends its local
/// reads all the same: no reader holds its lease past it (kiln 289db3175f3's fix to its 0003).
impl<F: BlockFile + 'static> Drop for Actor<F> {
    fn drop(&mut self) {
        self.control.local.release();
        if let Ok(closed) = self.control.closed.lock() {
            self.control.moved.notify_all();
            drop(closed);
        }
    }
}
impl<F: BlockFile + 'static> Actor<F> {
    fn now(&self) -> Result<u64, Refusal> {
        u64::try_from(self.at.saturating_duration_since(self.started).as_nanos())
            .map_err(unavailable)
    }
    fn refresh(&mut self) -> Result<(), Refusal> {
        if self.replica.machine().bootstrap_pending() {
            self.published = self.replica.applied().index;
            return Ok(());
        }
        let configuration_at = self.replica.machine().configuration_at();
        let configuration = self.replica.configuration().clone();
        let (floor, members) = self.replica.machine_mut().read_db(|db| {
            reload_changed(db, &self.changed)?;
            let floor = super::application::policy(db)?;
            let mut members = Vec::new();
            for member in crate::replication::floor::members(&configuration) {
                let attested = match &floor {
                    Some(floor) => crate::replication::floor::current(
                        db,
                        member,
                        &floor.name,
                        configuration_at,
                    )?
                    .map(|attested| attested.highest),
                    None => None,
                };
                members.push(amphora_api::replication::MemberFloor {
                    member,
                    voter: configuration.voters.contains(&member)
                        || configuration.voters_outgoing.contains(&member),
                    attested,
                });
            }
            Ok((floor, members))
        })?;
        let version = floor.as_ref().map_or(0, |floor| floor.version);
        let unattested = if floor.is_some() {
            members
                .iter()
                .filter(|member| !matches!(member.attested, Some(highest) if highest >= version))
                .count()
        } else {
            0
        };
        // A raise is the floor rising past the one this owner already published; the first
        // publication after it opens is where it starts, not a raise.
        if let Some(before) = self.floor_published
            && version > before
        {
            self.control.floor_meter.raised();
            tracing::info!(
                target: "amphora.floor",
                event = "floor.raised",
                version,
                from = before,
                "the application floor was raised"
            );
        }
        self.floor_published = Some(version);
        self.control.floor_meter.applied(version, unattested);
        *locked(&self.control.floor_status)? = amphora_api::replication::FloorStatus {
            application: floor.as_ref().map(|floor| floor.name.clone()),
            version,
            members,
        };
        *locked(&self.control.floor)? = floor;
        self.published = self.replica.applied().index;
        Ok(())
    }
    fn run(&mut self, receiver: mpsc::Receiver<Envelope>) {
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.run_loop(&receiver)))
                .unwrap_or_else(|_| {
                    Err(fenced("replica owner unwound; reopen from durable state"))
                });
        let reason = match result {
            Ok(()) => fenced("replica owner stopped"),
            Err(error) => fenced(error),
        };
        if let Some(mut pending) = self.pending.take()
            && let Some(response) = pending.response.take()
        {
            answer(response, Err(unknown()));
        }
        if let Some(changing) = self.changing.take() {
            let result = match changing.stage {
                Stage::Barrier => Err(reason.clone()),
                Stage::Proposed { .. } | Stage::Applied => Err(unknown()),
            };
            answer(changing.response, result);
        }
        if let Some(transferring) = self.transferring.take() {
            answer(transferring.response, Err(unknown()));
        }
        if let Ok(mut status) = self.control.status.lock() {
            *status = Err(reason.clone());
        }
        // Local reads end with the owner: none opens again, and the idle readers let its lease go.
        self.control.local.release();
        // Reads waiting for a closed time hear it ended (under the lock, so none misses it).
        if let Ok(closed) = self.control.closed.lock() {
            self.control.moved.notify_all();
            drop(closed);
        }
        for _ in 0..COMMANDS {
            let Some(envelope) = self.waiting.pop_front() else {
                break;
            };
            if let Input::Submit { response, .. } = envelope.command {
                answer(response, Err(reason.clone()));
            }
        }
        // The ingress channel has at most COMMANDS items. Notify callers before closing it.
        for _ in 0..COMMANDS {
            let Ok(envelope) = receiver.try_recv() else {
                break;
            };
            match envelope.command {
                Input::Submit { response, .. } => answer(response, Err(reason.clone())),
                Input::Receive { response, .. } | Input::Verdict { response, .. } => {
                    answer(response, Err(reason.clone()))
                }
                Input::Change { response, .. } | Input::Transfer { response, .. } => {
                    answer(response, Err(reason.clone()))
                }
                Input::CatchUp { response, .. } => answer(response, Err(reason.clone())),
                Input::Attest { .. } => (),
            }
        }
    }
    /// The owner thread: a step for each input queued, at once while the last step's drive left
    /// more to drive, and otherwise once its bell rings or its next deadline comes, whichever is
    /// first (replication.md, "Decision (lane constants): the owner's wakes"). It ends once every handle is gone.
    fn run_loop(&mut self, receiver: &mpsc::Receiver<Envelope>) -> Result<(), Refusal> {
        loop {
            let input = match receiver.try_recv() {
                Ok(envelope) => Some(envelope),
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                Err(mpsc::TryRecvError::Empty) if self.driving => None,
                Err(mpsc::TryRecvError::Empty) => {
                    let deadline = self.deadline();
                    let wait = deadline.saturating_duration_since((self.clock)());
                    self.control.bell.wait(wait)?;
                    // A wait begun before its deadline and ended at or past it, whatever ended
                    // it, is one the timer's lateness is measured on (`hyper_timing::Lateness`).
                    let woke = (self.clock)();
                    if !wait.is_zero() && woke >= deadline {
                        let since = |at: Instant| {
                            u64::try_from(at.saturating_duration_since(self.started).as_nanos())
                                .map_err(unavailable)
                        };
                        self.pace.waited(since(deadline)?, since(woke)?);
                    }
                    match receiver.try_recv() {
                        Ok(envelope) => Some(envelope),
                        Err(mpsc::TryRecvError::Empty) => None,
                        Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                    }
                }
            };
            self.run_step(input)?;
        }
    }
    /// The tick pace from what this owner measured (`pace`), derived once an election timeout:
    /// the core draws each timeout over that many ticks, so a pace derived oftener changes no
    /// timeout drawn meanwhile. The write slot's bounds follow it.
    fn repace(&mut self) -> Result<(), Refusal> {
        let raft = &self.replica.core().raft;
        let (election, heartbeat) = (raft.config().election_tick, raft.config().heartbeat_tick);
        self.paced = self.paced.saturating_add(1);
        if self.paced < election {
            return Ok(());
        }
        self.paced = 0;
        let voters = raft.configuration().voters().to_vec();
        let votes = self.replica.flushes().mean();
        let now = self.now()?;
        let id = self.replica.id();
        let pace = self
            .pace
            .derive(&voters, id, election, heartbeat, votes, now);
        self.slots.bound(slot_bounds(election, pace.period)?);
        Ok(())
    }
    /// The owner's next deadline: its next tick, or sooner a deadline of what it holds (a queued
    /// or pending submit's, a change's, a hand-off's, the write slot's). Every other timed check
    /// counts in ticks (elections, heartbeats, the quorum check, a bootstrap's hand-off), so it
    /// is due at a tick.
    fn deadline(&self) -> Instant {
        let tick = self
            .ticked
            .checked_add(self.pace.period())
            .unwrap_or(self.at);
        let waiting = self
            .waiting
            .iter()
            .filter_map(|envelope| match &envelope.command {
                Input::Submit { until, .. } => Some(*until),
                _ => None,
            });
        let held = [
            self.pending.as_ref().map(|pending| pending.until),
            self.changing.as_ref().map(|changing| changing.until),
            self.transferring
                .as_ref()
                .map(|transferring| transferring.until),
            self.slots.until(),
        ];
        waiting
            .chain(held.into_iter().flatten())
            .fold(tick, Instant::min)
    }
    /// One step at its clock's reading: the input first, then a tick if one is due, the core's
    /// work, deadlines, and what waits.
    fn run_step(&mut self, input: Option<Envelope>) -> Result<(), Refusal> {
        let at = (self.clock)();
        self.at = at;
        // A step that begins with no more input queued than a restart found ends its hold
        // (`log_bound::Spacing`).
        let queued = self.queued();
        self.step_queued = queued;
        let applied = self.replica.applied().index;
        if let Some(envelope) = input {
            self.receive(envelope)?;
        }
        #[cfg(test)]
        if let Some(stalls) = &self.stalls {
            locked(stalls)?.stepped(at);
        }
        if at.saturating_duration_since(self.ticked) >= self.pace.period() {
            // A delayed owner does not invent missed heartbeats or run an unbounded catchup.
            self.ticked = at;
            self.repace()?;
            // The quorum patience in force for this tick's check, from the answers measured.
            let leading = self.replica.is_leader().then(|| self.replica.term());
            // The voters the core's quorum check counts: the newest configuration its log states,
            // both halves of a joint one (replication.md, "Membership").
            let configuration = self.replica.core().raft.configuration();
            let (voters, outgoing) = (configuration.voters(), configuration.outgoing());
            self.answers.tick(leading, voters, outgoing)?;
            // A voter the host's detector suspects is gone, not slow: its gap isn't learned.
            for voter in &self.detected {
                self.answers.suspected(*voter);
            }
            let election = self.replica.core().raft.config().election_tick;
            let patience = self
                .answers
                .patience(voters, outgoing, self.replica.id(), election);
            self.replica
                .set_quorum_patience(patience)
                .map_err(raft_error)?;
            let shown = if leading.is_some() { patience } else { 0 };
            let shown = u32::try_from(shown)
                .ok()
                .and_then(|ticks| self.pace.period().checked_mul(ticks))
                .ok_or_else(|| fenced("the quorum patience overflowed its gauge"))?;
            self.control.meter.patience(shown);
            self.replica.tick().map_err(raft_error)?;
            #[cfg(test)]
            if let Some(stalls) = &self.stalls {
                locked(stalls)?.ticked(at);
            }
        }
        // One drive a step, which takes at most one `Ready` (hyper-durable's own quantum: its
        // owner.rs, docs/durable.md section 7), and one input a step: the two take turns, the
        // deficit round robin of Shreedhar and Varghese ("Efficient Fair Queuing Using Deficit
        // Round Robin", SIGCOMM 1995, Theorem 4.5: fair within one unit when every quantum is at
        // least the largest unit, here one `Ready`), so no count of drives is picked. A drive that
        // leaves more steps again at once (`run_loop`).
        let mut output = Output::default();
        let nanos = self.now()?;
        let (began, applying) = ((self.clock)(), self.replica.machine().applied_bytes());
        let driven = self
            .replica
            .drive(nanos, &self.waker, &mut output)
            .map_err(raft_error)?;
        // What the drive applied, and the time it took: a byte of log's replay (CT.1).
        let replayed = self
            .replica
            .machine()
            .applied_bytes()
            .saturating_sub(applying);
        self.compacting
            .replayed(replayed, nanos_between(began, (self.clock)()));
        if let Some((submitted, taken)) = driven.flushed {
            self.pace.flushed(submitted, taken);
        }
        #[cfg(test)]
        if let (Some(stalls), Some((submitted, taken))) = (&self.stalls, driven.flushed) {
            locked(stalls)?.wrote(Duration::from_nanos(taken.saturating_sub(submitted)));
        }
        self.release(output)?;
        self.driving = driven.more;
        #[cfg(test)]
        if driven.more {
            self.drove_more = self.drove_more.saturating_add(1);
        }
        self.answer_committed();
        self.flush()?;
        if self.replica.applied().index != self.published {
            self.refresh()?;
        }
        self.expire();
        self.expire_change();
        self.settle_change();
        self.settle_transfer();
        self.bootstrap()?;
        self.propose_class()?;
        self.keep_class_gate()?;
        self.keep_slots()?;
        if self.tail.is_some() || self.slot_deferred {
            self.prepare()?;
        }
        self.keep_floor()?;
        if !self.replica.machine().bootstrap_pending()
            && self.pending.is_none()
            && self.replica.leader() != 0
            && !self.class_in_flight()
            && let Some(envelope) = self.waiting.pop_front()
        {
            self.receive(envelope)?;
        }
        if self.replica.machine().bootstrap_pending() && self.replica.is_stalled() {
            return Err(fenced(
                "bootstrap exhausted replication log admission before publication",
            ));
        }
        // The log is due by Ongaro's rule at the expansion this owner learned from its own images
        // and replays (`compacting`; replication.md, "Decision (lane constants): the compaction's
        // expansion learned where it runs"; CT.1), or at once while it stalls admission: a
        // compaction is what frees its room. An image past its room is refused (`owner_image_room`),
        // so a due compaction waits for one within it; a stall still compacts, and fences on it.
        let ready = !self.replica.machine().bootstrap_pending();
        let due = ready
            && self.replica.compaction_due(self.compacting.rule())
            && self.replica.machine_mut().image_fits()?;
        if due || (ready && self.replica.is_stalled()) {
            let nanos = self.now()?;
            let began = (self.clock)();
            let imaged = self
                .replica
                .compact(0, nanos, &self.waker)
                .map_err(raft_error)?;
            if imaged {
                let bytes = self.replica.machine().imaged_bytes();
                self.compacting
                    .imaged(bytes, nanos_between(began, (self.clock)()));
            }
        }
        #[cfg(test)]
        if let Some(stalls) = &self.stalls {
            let (term, leader, id) = (
                self.replica.term(),
                self.replica.leader(),
                self.replica.id(),
            );
            locked(stalls)?.led(term, leader, id);
        }
        // An install moves the applied point forward; none applied is the meaning of a point not
        // past where the step began.
        let entries = self.replica.applied().index.saturating_sub(applied);
        let took = (self.clock)().saturating_duration_since(at);
        self.spacing.stepped(entries, took, queued)?;
        *locked(&self.control.status)? = Ok(status(&self.replica));
        self.publish_closed()?;
        let commits = self.replica.core().raft.commit_stats();
        self.control.meter.stepped(
            self.replica.machine().flush_backlog(),
            commits.held,
            commits.classic,
        );
        Ok(())
    }
    /// The flushing: takes what a finished flush made durable and what its checkpoint pass said,
    /// starts the database's log again when it is due and no flush or read is out, and hands the
    /// flushing thread the point applied when none is out (replication.md, "A replica's database
    /// log bounded by what it measures"). A restart is due by its share of what it holds
    /// (`log_bound::Spacing`), or past the log's room whatever the share says: disk before
    /// latency. A failed flush or checkpoint leaves the log's state unknown, so the owner fences.
    fn flush(&mut self) -> Result<(), Refusal> {
        match &mut self.flushing {
            Flushing::Thread {
                ask,
                done,
                out,
                pass,
            } => {
                match done.try_recv() {
                    Ok(Ok(flushed)) => {
                        self.replica.machine_mut().flushed_through(flushed.at);
                        *out = false;
                        if let Some(said) = flushed.pass {
                            *pass = said;
                        }
                    }
                    Ok(Err(error)) => {
                        return Err(fenced(format!(
                            "the database's log failed a flush: {error}"
                        )));
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => {
                        return Err(fenced("the replica's flushing thread ended"));
                    }
                }
                let frames = self
                    .replica
                    .machine()
                    .log_frames()
                    .map_err(|error| refused(&error))?;
                // Gauges: past u64's range they say the most they can.
                self.control
                    .meter
                    .log_bytes(frames.saturating_mul(log_bound::FRAME_BYTES));
                self.control
                    .meter
                    .log_limit_bytes(pass.room.saturating_mul(log_bound::FRAME_BYTES));
                let past = frames > pass.room;
                let due = past || (frames > 0 && self.spacing.due()?);
                if hold_due(pass, due, past) {
                    self.control.local.hold_reads()?;
                }
                if due && pass.caught_up && !*out {
                    // The queue as this step began, on the basis each later step reads it.
                    let queued = self.step_queued;
                    let machine = self.replica.machine_mut();
                    // The file keeps its length up to what the disk allows, and is cut (to nothing)
                    // only past it: a cut costs a restart and the commit after it more than reusing
                    // the file (store.md decision 31: files.rs what_cutting_the_log_costs_its_writer,
                    // adacachy 2026-10-29, a check rather than a recorded run).
                    machine.keep_log(pass.room);
                    if self
                        .control
                        .local
                        .restart_unread(|| machine.restart_log())?
                        == Some(true)
                    {
                        pass.caught_up = false;
                        pass.busy = false;
                        self.spacing.restarted(queued)?;
                        self.control.meter.restarted();
                    }
                }
                let frames = self
                    .replica
                    .machine()
                    .log_frames()
                    .map_err(|error| refused(&error))?;
                let past = frames > pass.room;
                self.log_full = past.then(|| {
                    let why = pass.unread.as_deref().unwrap_or(
                        "an equal share of its filesystem's free bytes among the replica logs there, past one snapshot's image",
                    );
                    format!(
                        "the database's log holds {frames} frames, past the {} it may hold: {why}",
                        pass.room
                    )
                });
                // While a restart is due, a pass follows every flush, and one is asked even with
                // nothing to flush: the restart waits for one that copies every durable frame.
                let due = past || (frames > 0 && self.spacing.due()?);
                let next = match self.replica.machine().unflushed() {
                    Some(at) => Some(at),
                    None if due => Some(self.replica.machine().applied()),
                    None => None,
                };
                if !*out && let Some(at) = next {
                    ask.try_send(Ask { at, copy: due })
                        .map_err(|_| fenced("the replica's flushing thread took no flush"))?;
                    *out = true;
                }
                Ok(())
            }
            #[cfg(test)]
            Flushing::Held => Ok(()),
        }
    }
    /// The bytes of input admitted to this owner and not yet taken: its channel's and those
    /// waiting behind the prepared transaction, each envelope's reservation. Past u64's range it
    /// says the most it can.
    fn queued(&self) -> u64 {
        u64::try_from(self.control.admitted.load(Ordering::Acquire)).unwrap_or(u64::MAX)
    }
    fn expire(&mut self) {
        // Expired callers relinquish queued operations even while a seed or proposal is held.
        // Rotate the bounded queue once; surviving requests retain their FIFO order.
        let queued = self.waiting.len().min(COMMANDS);
        for _ in 0..queued {
            let Some(envelope) = self.waiting.pop_front() else {
                break;
            };
            let expired =
                matches!(&envelope.command, Input::Submit { until, .. } if self.at >= *until);
            if expired {
                if let Input::Submit { response, .. } = envelope.command {
                    answer(response, Err(Refusal::Timeout));
                }
            } else {
                self.waiting.push_back(envelope);
            }
        }
        let Some(pending) = self.pending.as_ref() else {
            return;
        };
        if let Phase::Answered { index, .. } = pending.phase {
            // Committed and answered: its entry applies in whatever term, and `release` checks
            // the apply against the answer, so neither a new term nor the deadline ends it. Applied
            // through it with no apply answered, an image covered it: nothing is left to check.
            if self.replica.applied().index >= index {
                self.pending = None;
            }
            return;
        }
        let expired = self.at >= pending.until;
        let changed_term = pending.term != self.replica.term()
            || pending.leader != self.replica.leader()
            || self.replica.leader() == 0;
        if !expired && !changed_term {
            return;
        }
        let Some(mut pending) = self.pending.take() else {
            return;
        };
        match &pending.phase {
            Phase::Barrier(_) => {
                if expired {
                    if let Some(response) = pending.response.take() {
                        answer(response, Err(Refusal::Timeout));
                    }
                    if matches!(pending.author, Author::Owner)
                        && !changed_term
                        && self.replica.is_leader()
                    {
                        self.own_sessions_expired = self.own_sessions_expired.saturating_add(1);
                        self.control.meter.own_session_expired();
                        tracing::info!(target: "amphora.replica", event = "replica.own_session_expired", term = self.replica.term(), expired = self.own_sessions_expired, "an owner's session reached its lease at its barrier while this member led its term");
                    }
                } else {
                    self.requeue(pending);
                }
            }
            Phase::Proposed(_) | Phase::Bootstrap(_) | Phase::Answered { .. } => {
                if let Some(response) = pending.response.take() {
                    answer(response, Err(unknown()));
                }
                // The captured base cannot be used by another write until this one applies. A
                // caller timing out relinquishes its response, never an unresolved proposal.
                if !changed_term {
                    self.pending = Some(pending);
                }
            }
        }
    }
    fn requeue(&mut self, mut pending: Pending) {
        // Only an untouched application operation may move back. A fresh dispatch assigns a new
        // context and ReadIndex; its old read completion cannot release the renewed barrier.
        let Phase::Barrier(Some(operation)) = pending.phase else {
            return;
        };
        // The owner's own sessions answer no caller: the floor's keeper and the seed make theirs
        // again on a later pass. Only a caller's moves back, with its claim.
        let Author::Application(application) = pending.author else {
            return;
        };
        let Some(response) = pending.response.take() else {
            return;
        };
        if self.at >= pending.until {
            answer(response, Err(Refusal::Timeout));
        } else if self.waiting.len() >= COMMANDS {
            answer(response, Err(Refusal::Full { limit: COMMANDS }));
        } else {
            self.waiting.push_front(Envelope {
                command: Input::Submit {
                    namespace: pending.namespace,
                    operation,
                    application,
                    until: pending.until,
                    response,
                },
                _reservation: pending._reservation,
            });
        }
    }
    /// The change under way, once its barrier shows this leader current: proposed, or answered
    /// at once when the group is so already or can't take it.
    fn propose_change(&mut self) {
        let Some(mut changing) = self.changing.take() else {
            return;
        };
        if !matches!(changing.stage, Stage::Barrier) {
            self.changing = Some(changing);
            return;
        }
        let result = if self.at >= changing.until {
            Err(Refusal::Timeout)
        } else if changing.term != self.replica.term() || !self.replica.is_leader() {
            Err(not_leader(self.replica.leader()))
        } else {
            match self.member_change(changing.change) {
                Ok(None) => Ok(status(&self.replica)),
                Ok(Some(proposal)) => {
                    let before = self.replica.core().raft.pending_conf_index();
                    match self.replica.change(changing.context.clone(), &proposal) {
                        // The core marks the entry it took as its pending change. One it can't
                        // take it keeps in place stating nothing, and leaves that mark.
                        Ok(()) if self.replica.core().raft.pending_conf_index() > before => {
                            let index = self.replica.core().raft.pending_conf_index();
                            changing.stage = Stage::Proposed { index };
                            self.changing = Some(changing);
                            return;
                        }
                        Ok(()) => Err(unavailable(
                            "the leader did not take the change of members; try again",
                        )),
                        Err(error) => Err(raft_error(error)),
                    }
                }
                Err(refusal) => Err(refusal),
            }
        };
        answer(changing.response, result);
    }
    /// The change under way, once applied, and once the group has left the joint configuration it
    /// made: answered with the status. It runs after `expire_change`, so a change still proposed
    /// here has its lead, whose own entry at `index` no other replaces in its term.
    fn settle_change(&mut self) {
        let Some(changing) = self.changing.as_mut() else {
            return;
        };
        if let Stage::Proposed { index } = changing.stage
            && self.replica.applied().index >= index
        {
            changing.stage = Stage::Applied;
        }
        // The configuration applied, not the core's: the core counts by the newest its log
        // states, so its own leaves the joint one as soon as the leave is appended.
        if matches!(changing.stage, Stage::Applied)
            && self.replica.configuration().voters_outgoing.is_empty()
            && let Some(changing) = self.changing.take()
        {
            answer(changing.response, Ok(status(&self.replica)));
        }
    }
    /// The change under way past its deadline, or past the lead it was made under.
    fn expire_change(&mut self) {
        let Some(changing) = self.changing.as_ref() else {
            return;
        };
        let expired = self.at >= changing.until;
        let moved = changing.term != self.replica.term()
            || changing.leader != self.replica.leader()
            || self.replica.leader() == 0;
        let result = match changing.stage {
            Stage::Barrier if expired => Err(Refusal::Timeout),
            Stage::Barrier if moved => Err(not_leader(self.replica.leader())),
            Stage::Proposed { .. } if expired || moved => Err(unknown()),
            // Committed. The leader leaves the joint configuration by itself, this one or the
            // next, so a new lead waits for that; a deadline answers what is applied.
            Stage::Applied if expired => Ok(status(&self.replica)),
            _ => return,
        };
        if let Some(changing) = self.changing.take() {
            answer(changing.response, result);
        }
    }
    fn receive(&mut self, envelope: Envelope) -> Result<(), Refusal> {
        let Envelope {
            command,
            _reservation: reservation,
        } = envelope;
        match command {
            Input::Receive {
                from,
                bytes,
                until,
                response,
            } => {
                let result = if self.at >= until {
                    Err(Refusal::Timeout)
                } else {
                    self.step(from, &bytes, reservation)
                };
                let fatal = matches!(&result, Err(Refusal::Fenced(_)));
                let error = result.as_ref().err().cloned();
                answer(response, result);
                if fatal && let Some(error) = error {
                    return Err(error);
                }
            }
            Input::Verdict {
                verdict,
                until,
                response,
            } => {
                let result = if self.at >= until {
                    Err(Refusal::Timeout)
                } else {
                    self.heard(verdict)
                };
                let fatal = matches!(&result, Err(Refusal::Fenced(_)));
                let error = result.as_ref().err().cloned();
                answer(response, result);
                if fatal && let Some(error) = error {
                    return Err(error);
                }
            }
            Input::Submit {
                namespace,
                operation,
                application,
                until,
                response,
            } => {
                if self.at >= until {
                    answer(response, Err(Refusal::Timeout));
                } else if let Some(full) = &self.log_full {
                    answer(response, Err(Refusal::Unavailable(full.clone())));
                } else if self.replica.leader() == 0
                    || self.pending.is_some()
                    || self.replica.machine().bootstrap_pending()
                    || self.class_in_flight()
                {
                    if self.waiting.len() >= COMMANDS {
                        answer(response, Err(Refusal::Full { limit: COMMANDS }));
                    } else {
                        self.waiting.push_back(Envelope {
                            command: Input::Submit {
                                namespace,
                                operation,
                                application,
                                until,
                                response,
                            },
                            _reservation: reservation,
                        });
                    }
                } else {
                    let context = match self.context() {
                        Ok(context) => context,
                        Err(refusal) => {
                            answer(response, Err(refusal.clone()));
                            return Err(refusal);
                        }
                    };
                    match self.replica.read(context.clone()) {
                        Ok(()) => {
                            self.pending = Some(Pending {
                                context,
                                term: self.replica.term(),
                                leader: self.replica.leader(),
                                until,
                                namespace,
                                author: Author::Application(application),
                                response: Some(response),
                                phase: Phase::Barrier(Some(operation)),
                                _reservation: reservation,
                            })
                        }
                        Err(error) => answer(response, Err(raft_error(error))),
                    }
                }
            }
            Input::Change {
                change,
                until,
                response,
            } => {
                if self.at >= until {
                    answer(response, Err(Refusal::Timeout));
                } else if let Err(refusal) = self.may_change() {
                    answer(response, Err(refusal));
                } else {
                    // The barrier: this leader's term has committed, and its configuration is
                    // applied through what the group had committed when the change came.
                    let context = match self.context() {
                        Ok(context) => context,
                        Err(refusal) => {
                            answer(response, Err(refusal.clone()));
                            return Err(refusal);
                        }
                    };
                    match self.replica.read(context.clone()) {
                        Ok(()) => {
                            self.changing = Some(Changing {
                                context,
                                term: self.replica.term(),
                                leader: self.replica.leader(),
                                until,
                                change,
                                stage: Stage::Barrier,
                                response,
                                _reservation: reservation,
                            })
                        }
                        Err(error) => answer(response, Err(raft_error(error))),
                    }
                }
            }
            Input::Attest { application } => {
                // A bind of the application already attested changes nothing.
                if self.attesting.as_ref() != Some(&application) {
                    self.attesting = Some(application);
                    self.floor_settled = None;
                    self.floor_checked = 0;
                }
            }
            Input::Transfer {
                to,
                until,
                response,
            } => {
                if self.at >= until {
                    answer(response, Err(Refusal::Timeout));
                } else if let Err(refusal) = self.may_change() {
                    answer(response, Err(refusal));
                } else if to == self.replica.id() {
                    answer(response, Ok(status(&self.replica)));
                } else if !self.replica.core().raft.configuration().votes(to) {
                    // The core gives up, saying nothing, a hand-off to a member that can't lead.
                    answer(
                        response,
                        Err(api::invalid("lead transfer", "only a voter takes the lead")),
                    );
                } else {
                    match self.replica.transfer(to) {
                        Ok(()) => {
                            self.transferring = Some(Transferring {
                                to,
                                term: self.replica.term(),
                                until,
                                response,
                                _reservation: reservation,
                            })
                        }
                        Err(error) => answer(response, Err(raft_error(error))),
                    }
                }
            }
            Input::CatchUp { until, response } => {
                if self.at >= until {
                    answer(response, Err(Refusal::Timeout));
                    return Ok(());
                }
                let judged = self.catching_up();
                let fatal = match &judged {
                    Err(error @ Refusal::Fenced(_)) => Some(error.clone()),
                    _ => None,
                };
                answer(response, judged);
                if let Some(error) = fatal {
                    return Err(error);
                }
            }
        }
        Ok(())
    }
    /// Where catching up each learner of the configuration applied stands, judged now
    /// (`ReplicaHandle::catch_up`). A learner the core no longer counts, its removal appended but
    /// not yet applied, is left out.
    fn catching_up(&mut self) -> Result<Vec<(u64, CatchUp)>, Refusal> {
        if !self.replica.is_leader() {
            return Err(not_leader(self.replica.leader()));
        }
        let learners = self.replica.configuration().learners.clone();
        let mut judged = Vec::with_capacity(learners.len());
        for member in learners {
            let stands = match self.replica.catch_up(member).map_err(raft_error)? {
                hyper_raft::CatchUp::Ready => CatchUp::Ready,
                hyper_raft::CatchUp::Pending => CatchUp::Pending,
                hyper_raft::CatchUp::Aborted => CatchUp::Aborted,
                hyper_raft::CatchUp::NotLeader => return Err(not_leader(self.replica.leader())),
                hyper_raft::CatchUp::NotMember => continue,
            };
            judged.push((member, stands));
        }
        Ok(judged)
    }
    /// The hand-off under way: answered once this member follows `to`, once the lead went to
    /// another, once the core gave it up (it does within an election timeout), or past its
    /// deadline.
    fn settle_transfer(&mut self) {
        let Some(transferring) = self.transferring.as_ref() else {
            return;
        };
        let leader = self.replica.leader();
        let result = if leader == transferring.to {
            Ok(status(&self.replica))
        } else if self.at >= transferring.until {
            Err(unknown())
        } else if leader != 0 && leader != self.replica.id() {
            Err(not_leader(leader))
        } else if self.replica.is_leader()
            && (self.replica.term() != transferring.term
                || self.replica.core().raft.lead_transferee().is_none())
        {
            Err(unavailable(
                "the lead was not taken within an election timeout; try again",
            ))
        } else {
            return;
        };
        if let Some(transferring) = self.transferring.take() {
            answer(transferring.response, result);
        }
    }
    /// Whether this owner takes a change of members now.
    fn may_change(&self) -> Result<(), Refusal> {
        if !self.replica.is_leader() {
            return Err(not_leader(self.replica.leader()));
        }
        if self.changing.is_some() || self.transferring.is_some() {
            return Err(unavailable(
                "a change of members is under way; try again once it is answered",
            ));
        }
        if self.replica.machine().bootstrap_pending() {
            return Err(unavailable(
                "the group is taking its seed; change its members once it has",
            ));
        }
        // A change only once the last is applied, and a hand-off only once its voter can have the
        // way out of a joint configuration: one admitted while joint loses its election for nothing.
        let raft = &self.replica.core().raft;
        if raft.configuration().is_joint() {
            return Err(unavailable(
                "the group is leaving a joint configuration; try again once it has",
            ));
        }
        if raft.has_pending_conf() {
            return Err(unavailable(
                "the group has a change of members still to apply; try again once it has",
            ));
        }
        Ok(())
    }
    /// What `change` asks of the group as this leader leads it now: the joint change that makes
    /// it, none when the group is so already, or why the group can't take it.
    fn member_change(&mut self, change: MemberChange) -> Result<Option<ConfChangeV2>, Refusal> {
        let raft = &self.replica.core().raft;
        let configuration = raft.configuration();
        if configuration.is_joint() || raft.has_pending_conf() {
            return Err(unavailable(
                "a change of members is under way; try again once it is applied",
            ));
        }
        let invalid = |why: &str| -> Result<Option<ConfChangeV2>, Refusal> {
            Err(api::invalid("member change", why))
        };
        let (MemberChange::AddLearner(member)
        | MemberChange::Promote(member)
        | MemberChange::Demote(member)
        | MemberChange::Remove(member)) = change;
        if member == 0 {
            return invalid("a member's identity is nonzero");
        }
        let (learns, votes) = (configuration.learns(member), configuration.votes(member));
        let change_type = match change {
            MemberChange::AddLearner(_) if learns => return Ok(None),
            MemberChange::AddLearner(_) if votes => {
                return invalid("a voter is demoted, not added as a learner");
            }
            MemberChange::AddLearner(_) => ConfChangeType::AddLearnerNode,
            MemberChange::Promote(_) if votes => return Ok(None),
            MemberChange::Promote(_) if !learns => {
                return invalid("only a learner is promoted; add it as a learner first");
            }
            MemberChange::Promote(_) => ConfChangeType::AddNode,
            MemberChange::Demote(_) if learns => return Ok(None),
            MemberChange::Demote(_) if !votes => return invalid("only a voter is demoted"),
            MemberChange::Remove(_) if !learns && !votes => return Ok(None),
            MemberChange::Demote(_) | MemberChange::Remove(_) if member == self.replica.id() => {
                return invalid("the leader hands on its lead first (transfer), then is changed");
            }
            MemberChange::Demote(_) => ConfChangeType::AddLearnerNode,
            MemberChange::Remove(_) => ConfChangeType::RemoveNode,
        };
        // Two phases (Ongaro and Ousterhout 2014, section 6): into the joint configuration, then
        // out of it, which the leader proposes by itself once the first is committed.
        let proposal = ConfChangeV2 {
            transition: ConfChangeTransition::Implicit,
            changes: vec![ConfChangeSingle {
                change_type,
                node_id: member,
            }],
            context: Vec::new(),
        };
        // What the core would make of it, so that a change it can't take (one leaving no voter)
        // is refused here, not committed as a change that changes nothing.
        let changed = Plan::of(&proposal)
            .and_then(|plan| plan.apply(configuration))
            .map_err(|error| api::invalid("member change", &error.to_string()))?;
        api::bound(
            "replica members",
            changed.configuration.members().count(),
            raft.config().limits.members.min(MEMBERS),
        )?;
        // A joiner catches up by the leader's image once the log no longer reaches back: past
        // the image's ceiling it never could, so it is refused here, not added to fall behind.
        match change {
            MemberChange::AddLearner(_) => self.replica.machine_mut().admits_joiner()?,
            MemberChange::Promote(member) => self.admits_voter(member)?,
            MemberChange::Demote(_) | MemberChange::Remove(_) => (),
        }
        Ok(Some(proposal))
    }
    /// A learner becomes a voter only on its attestation of the floor in force, under its current
    /// join, at or above it (replication.md, "How it composes with membership changes"). It is read
    /// on a quiescent tail, so the state read is the state at the change's index; a tail still to
    /// apply is answered at once, never waited on. A group with no floor promotes as before.
    fn admits_voter(&mut self, member: u64) -> Result<(), Refusal> {
        let (floor, attested) = self.replica.machine_mut().read_db(|db| {
            let Some(floor) = super::application::policy(db)? else {
                return Ok((None, None));
            };
            let attested = crate::replication::floor::current(db, member, &floor.name, 0)?;
            Ok((Some(floor), attested))
        })?;
        let Some(floor) = floor else {
            return Ok(());
        };
        let applied = self.replica.applied().index;
        let last = self
            .replica
            .core()
            .raft
            .log()
            .last_index()
            .map_err(unavailable)?;
        let refused = if applied != last {
            (
                FloorRefusal::Busy,
                unavailable(format!(
                    "the log runs to {last} and {applied} is applied; promote once the tail is applied"
                )),
            )
        } else {
            match attested {
                None => (
                    FloorRefusal::Unproven,
                    unavailable(format!(
                        "learner {member} has not attested {} under its current join; the group's floor is {}",
                        floor.name, floor.version
                    )),
                ),
                Some(attested) if attested.highest < floor.version => (
                    FloorRefusal::BelowFloor,
                    api::invalid(
                        "application floor",
                        &format!(
                            "learner {member} runs {} up to {}; the group's floor is {}",
                            floor.name, attested.highest, floor.version
                        ),
                    ),
                ),
                Some(_) => return Ok(()),
            }
        };
        let (reason, refusal) = refused;
        floor_refused(&self.control, reason, &refusal);
        Err(refusal)
    }
    /// Steps a wire record from `from`. One the write slot holds is processed as held: its receipt
    /// is answered Ok, as admission's is ("not a durability ACK"), so no host waits on a slot
    /// (amphora-owner-t1-g-slot-fc28e2d-20261112).
    fn step(&mut self, from: u64, bytes: &[u8], reservation: Reservation) -> Result<(), Refusal> {
        #[cfg(test)]
        if let Some(stalls) = &self.stalls
            && self.replica.leader() == self.replica.id()
        {
            locked(stalls)?.answered(from, self.at);
        }
        let message = Message::decode(bytes)
            .map_err(|error| api::invalid("replica wire record", &error.to_string()))?;
        if message.from != from || message.to != self.replica.id() || from == self.replica.id() {
            return Err(api::invalid(
                "replica sender",
                "authenticated sender or destination differs from record",
            ));
        }
        // The members the core counts by: the newest configuration its log states, applied or
        // not. A leader sends to a member it adds from the moment it appends the change, and stops
        // at the moment it appends a removal, so that member's answers count from and to then.
        if from == 0 || !self.replica.core().raft.configuration().contains(from) {
            return Err(api::invalid(
                "replica sender",
                "sender is not a current member",
            ));
        }
        // What the message measures of its path and its link, at the owner's clock (`pace`).
        let heartbeat = self.replica.core().raft.config().heartbeat_tick;
        let now = self.now()?;
        self.pace.took(&message, now, heartbeat);
        // A forwarded proposal's entries name their origin in their contexts (`context`). Nothing
        // re-forwards a proposal, so its origin is its authenticated sender: one naming another
        // member is refused here and applies nowhere (replication.md, "Attestation").
        if message.msg_type == hyper_raft::proto::MessageType::MsgPropose
            && message
                .entries
                .iter()
                .any(|entry| entry.context.get(..8) != Some(from.to_le_bytes().as_slice()))
        {
            return Err(api::invalid(
                "replica sender",
                "a forwarded proposal names another member as its origin",
            ));
        }
        if self.replica.is_leader()
            && message.term == self.replica.term()
            && self.replica.core().raft.configuration().votes(from)
            && let Err(unmeasured) = self.answers.heard(from, message.term)
        {
            if matches!(unmeasured, patience::Unmeasured::Suspected { .. }) {
                self.control.meter.refused();
            }
            tracing::warn!(target: "amphora.replica", event = "quorum_patience_refused", error = %unmeasured, "an answer gap was not learned for the leader's quorum patience");
        }
        // The write slot holds what would refuse its member's retry, past the sender's checks.
        if self.replica.is_leader() && !self.slot_off() {
            match message.msg_type {
                hyper_raft::proto::MessageType::MsgPropose => {
                    let forward = self.slots.forward(from, self.at, (message, reservation));
                    if let Some(unmeasured) = forward.unmeasured {
                        tracing::warn!(target: "amphora.replication", event = "write_slot_unmeasured", error = %unmeasured, "a retry's gap was not learned for the write slot");
                    }
                    self.slot_ended(forward.ended);
                    let stepped = match forward.step {
                        Some((message, _reservation)) => self.step_forward(from, message),
                        None => Ok(()),
                    };
                    self.step_ready()?;
                    return stepped;
                }
                hyper_raft::proto::MessageType::MsgReadIndex => {
                    let read = self.slots.read(from, self.at, (message, reservation));
                    self.slot_ended(read.ended);
                    let stepped = match read.step {
                        Some((message, _reservation)) => {
                            self.replica.step(message).map_err(raft_error)
                        }
                        None => Ok(()),
                    };
                    self.start_slot()?;
                    self.step_ready()?;
                    return stepped;
                }
                _ => {}
            }
        }
        let mut message = message;
        if self.replica.is_leader()
            && message.msg_type == hyper_raft::proto::MessageType::MsgPropose
        {
            let log = self.replica.core().raft.log();
            let last = log.last_index().map_err(unavailable)?;
            let before = Point {
                index: last,
                term: log.term(last).map_err(unavailable)?,
            };
            self.rebase_forward(&mut message, before)?;
        }
        self.replica.step(message).map_err(raft_error)
    }
    /// Steps a member's forwarded proposal into the leading core, and tells the slot what it
    /// appended: each entry's verdict is decided from the entry before it, as its apply will
    /// decide (`replication::verdict`), and a write refused as stale queues its member for a
    /// slot. A leader never displaces an entry of its own term (Raft's Leader Append-Only
    /// property, Ongaro and Ousterhout, USENIX ATC 2014, figure 3) and the slots die with the
    /// term, so the refusal seen at the append is the one the commit decides; seen here, it
    /// comes before any member can learn of it (amphora-seam-owner-2's ruling, Q2).
    fn step_forward(&mut self, from: u64, mut message: Message) -> Result<(), Refusal> {
        let log = self.replica.core().raft.log();
        let last = log.last_index().map_err(unavailable)?;
        let mut before = Point {
            index: last,
            term: log.term(last).map_err(unavailable)?,
        };
        self.rebase_forward(&mut message, before)?;
        let term = self.replica.term();
        let mut refused = false;
        for entry in &message.entries {
            if let Ok(Err(Refusal::Invalid { what, .. })) =
                crate::replication::verdict(&entry.data, before)
                && what == "prepared transaction"
            {
                refused = true;
            }
            let Some(index) = before.index.checked_add(1) else {
                break;
            };
            before = Point { index, term };
        }
        let entries = u64::try_from(message.entries.len()).ok();
        self.replica.step(message).map_err(raft_error)?;
        let appended = self
            .replica
            .core()
            .raft
            .log()
            .last_index()
            .map_err(unavailable)?;
        if refused && appended.checked_sub(last) == entries {
            self.slots.refused(from);
        }
        Ok(())
    }
    /// The write `data`, prepared on a base older than `before`, with its base moved to `before`
    /// if no entry the leader's log holds after its base wrote a page it read or wrote; `None`
    /// leaves it to the exact base, which refuses it at apply. The walk is over the log between
    /// the base and `before`, which the log's own bound bounds, one entry at a time, ending at the
    /// first conflict (replication.md, "A follower's write that read nothing written since its
    /// base: read sets"; goal ceea4e0a).
    fn rebase(&mut self, data: &[u8], before: Point) -> Result<Option<Vec<u8>>, Refusal> {
        use crate::replication::readsets;
        let Some(write) = readsets::Write::decode(data) else {
            return Ok(None);
        };
        if write.base.index >= before.index {
            return Ok(None);
        }
        #[cfg(test)]
        if self.read_sets_off {
            return Ok(None);
        }
        #[cfg(test)]
        if self.read_sets_unchecked {
            self.read_sets_moved += 1;
            return Ok(write.rebased(data, before));
        }
        // A leader without the closed time's table holds no write that carries a read set
        // (`Machine::prepare_db`): its refusal leaves the write to the exact base.
        let Ok(applied) = self.replica.machine().applied_pages() else {
            Self::read_set_event("unknown_base", write.base, before);
            return Ok(None);
        };
        let log = self.replica.core().raft.log();
        let held = log.first_index().map_err(unavailable)?.saturating_sub(1);
        if write.base.index < held
            || log.term(write.base.index).map_err(unavailable)? != write.base.term
        {
            Self::read_set_event("unknown_base", write.base, before);
            return Ok(None);
        }
        let bound = u64::try_from(crate::replication::COMMAND_BYTES).unwrap_or(u64::MAX);
        let high = before.index.saturating_add(1);
        let mut low = write.base.index.saturating_add(1);
        while low < high {
            let entries = log.slice(low, high, bound).map_err(unavailable)?;
            let Some(last) = entries.last().map(|entry| entry.index) else {
                return Ok(None);
            };
            for entry in &entries {
                let wrote = if entry.entry_type == hyper_raft::proto::EntryType::EntryNormal {
                    readsets::wrote(&entry.data)
                } else {
                    readsets::Wrote::Opaque
                };
                if !write.untouched_by(&wrote, &applied) {
                    Self::read_set_event("conflict", write.base, before);
                    return Ok(None);
                }
            }
            low = last.saturating_add(1);
        }
        Self::read_set_event("rebased", write.base, before);
        #[cfg(test)]
        {
            self.read_sets_moved += 1;
        }
        Ok(write.rebased(data, before))
    }
    /// A member's forwarded proposal, at the leader: one write forwarded alone appends right after
    /// `before`, the leader's last entry, so a stale base that read nothing written since moves
    /// to it (`readsets`).
    fn rebase_forward(&mut self, message: &mut Message, before: Point) -> Result<(), Refusal> {
        if let [entry] = message.entries.as_mut_slice()
            && entry.entry_type == hyper_raft::proto::EntryType::EntryNormal
            && let Ok(Err(_)) = crate::replication::verdict(&entry.data, before)
            && let Some(data) = self.rebase(&entry.data, before)?
        {
            entry.data = data;
        }
        Ok(())
    }
    fn read_set_event(outcome: &'static str, base: Point, before: Point) {
        tracing::debug!(target: "amphora.replication", event = "replication.read_set", outcome, base = base.index, before = before.index, "a write prepared on an older base was checked against its read set");
    }
    /// Steps what the write slot released, in the order it arrived. At a member that no longer
    /// leads, none is stepped: its core would forward it again under its sender's name, which
    /// nothing does (replication.md, "A forward that meets an election"). What was held is
    /// discarded there, counted in an event, as the core at a non-leader drops a proposal
    /// (`Dropped`): its receipt was answered as it was held, and its origin answers its own caller
    /// on the term or leader change (amphora-owner-t1-g-slot-fc28e2d-20261112).
    fn step_ready(&mut self) -> Result<(), Refusal> {
        let (mut forwards, mut barriers) = (0u64, 0u64);
        while let Some((from, (message, _reservation))) = self.slots.ready() {
            let proposal = message.msg_type == hyper_raft::proto::MessageType::MsgPropose;
            let stepped = if !self.replica.is_leader() {
                if proposal {
                    forwards = forwards.saturating_add(1);
                } else {
                    barriers = barriers.saturating_add(1);
                }
                Ok(())
            } else if proposal {
                self.step_forward(from, message)
            } else {
                self.replica.step(message).map_err(raft_error)
            };
            match stepped {
                Err(fenced @ Refusal::Fenced(_)) => return Err(fenced),
                Err(refusal) => {
                    tracing::warn!(target: "amphora.replication", event = "replication.write_slot_step_refused", from, ?refusal, "a record the write slot held was refused as it was stepped");
                }
                Ok(()) => {}
            }
        }
        if forwards > 0 || barriers > 0 {
            tracing::warn!(target: "amphora.replication", event = "replication.write_slot_discarded", forwards, barriers, "records the write slot held were discarded at the end of this member's lead; their origins answer their callers on the term change");
        }
        Ok(())
    }
    /// A write slot that ended, counted and told.
    fn slot_ended(&self, ended: Option<super::slot::Ended>) {
        if let Some(ended) = ended {
            self.control.meter.slot_ended(ended.how, ended.held);
            let held_seconds = ended.held.as_secs_f64();
            tracing::debug!(target: "amphora.replication", event = "replication.write_slot", member = ended.member, outcome = ended.how.outcome(), held_seconds, "a write slot ended");
        }
    }
    /// The write slot at this step: the term it belongs to, the members it keeps, its expiry, the
    /// commit a held barrier waits on, a change of members or a hand-off (each releases it), and
    /// the next slot's start; then what any of them released steps (replication.md, "A
    /// follower's write under steady leader writes: the write slot").
    fn keep_slots(&mut self) -> Result<(), Refusal> {
        let at = self.at;
        let leading = (self.replica.is_leader() && !self.slot_off()).then(|| self.replica.term());
        let ended = self.slots.lead(leading, at);
        self.slot_ended(ended);
        if leading.is_some() {
            let configuration = self.replica.core().raft.configuration();
            let detected = &self.detected;
            let ended = self.slots.keep(
                at,
                |member| configuration.contains(member),
                |member| detected.contains(&member),
            );
            self.slot_ended(ended);
            let ended = self.slots.expire(at);
            self.slot_ended(ended);
            self.slots
                .committed(self.replica.core().raft.log().committed());
            if self.changing.is_some() || self.transferring.is_some() {
                let ended = self.slots.release(at);
                self.slot_ended(ended);
            }
            self.start_slot()?;
        }
        self.step_ready()
    }
    /// Begins the next write slot if a barrier waits for one and none is held; none while this
    /// member doesn't lead, or a change of members, a hand-off or a bootstrap is under way.
    fn start_slot(&mut self) -> Result<(), Refusal> {
        if !self.replica.is_leader()
            || self.slot_off()
            || self.changing.is_some()
            || self.transferring.is_some()
            || self.replica.machine().bootstrap_pending()
        {
            return Ok(());
        }
        let log = self.replica.core().raft.log();
        let (last, commit) = (log.last_index().map_err(unavailable)?, log.committed());
        let own = self
            .pending
            .as_ref()
            .is_some_and(|pending| matches!(pending.phase, Phase::Barrier(_)));
        if let Some((_, length)) = self.slots.start(self.at, last, commit, own) {
            self.control.meter.slot_began(length);
        }
        Ok(())
    }
    /// Whether a test's control has turned the write slot off.
    fn slot_off(&self) -> bool {
        #[cfg(test)]
        let off = self.slot_off;
        #[cfg(not(test))]
        let off = false;
        off
    }
    /// The host's detector's verdict, for the held count alone (hyper-raft R-8): Amphora elects on
    /// ticks, and its elections take no detector's word.
    fn heard(&mut self, verdict: Verdict) -> Result<(), Refusal> {
        let member = match verdict {
            Verdict::Suspected(member) | Verdict::Trusted(member) => member,
        };
        let conf = self.replica.configuration();
        if member == self.replica.id()
            || !conf
                .voters
                .iter()
                .chain(&conf.learners)
                .chain(&conf.voters_outgoing)
                .chain(&conf.learners_next)
                .any(|id| *id == member)
        {
            return Err(api::invalid(
                "replica verdict",
                "the member is this node or not a current member",
            ));
        }
        match verdict {
            Verdict::Suspected(_) => {
                self.detected.insert(member);
                self.replica.suspect_held(member)
            }
            Verdict::Trusted(_) => {
                self.detected.remove(&member);
                if self.gated.contains(&member) {
                    return Ok(());
                }
                self.replica.trust_held(member)
            }
        }
        .map_err(raft_error)
    }
    /// The class's gate on held answers: until this member's recorded class is its own, every
    /// other member it names is held suspected; once it is, each is trusted again unless the
    /// detector suspects it (replication.md, "The class stated once per group").
    fn keep_class_gate(&mut self) -> Result<(), Refusal> {
        if !self.replica.machine().is_replicated() {
            return Ok(());
        }
        let conf = self.replica.configuration();
        let named: std::collections::BTreeSet<u64> = conf
            .voters
            .iter()
            .chain(&conf.learners)
            .chain(&conf.voters_outgoing)
            .chain(&conf.learners_next)
            .copied()
            .filter(|member| *member != self.replica.id())
            .collect();
        if self.replica.machine().class_agreed() {
            for member in std::mem::take(&mut self.gated) {
                if named.contains(&member) && !self.detected.contains(&member) {
                    self.replica.trust_held(member).map_err(raft_error)?;
                }
            }
            return Ok(());
        }
        self.gated.retain(|member| named.contains(member));
        for member in named {
            if !self.gated.contains(&member) {
                self.replica.suspect_held(member).map_err(raft_error)?;
                self.gated.insert(member);
            }
        }
        Ok(())
    }
    /// Whether this leader's class entry is proposed and its group's record not yet its own:
    /// no write is prepared meanwhile, as its entry would move the point under it.
    fn class_in_flight(&self) -> bool {
        self.class_proposed == Some(self.replica.term())
            && self.replica.is_leader()
            && self.replica.machine().class_proposal().is_some()
    }
    /// A leader whose group records no class, or one its own supersedes, proposes its own, once a
    /// term and never beside a write or a change under way.
    fn propose_class(&mut self) -> Result<(), Refusal> {
        #[cfg(test)]
        if self.class_held {
            return Ok(());
        }
        if !self.replica.is_leader()
            || self.replica.machine().bootstrap_pending()
            || self.pending.is_some()
            || self.changing.is_some()
            || self.slots.holds()
            || self.class_proposed == Some(self.replica.term())
        {
            return Ok(());
        }
        let Some(class) = self.replica.machine().class_proposal() else {
            return Ok(());
        };
        let bytes = crate::replication::class::entry(&class)?;
        let context = self.context()?;
        match self.propose(&context, bytes) {
            Ok(()) => {
                self.class_proposed = Some(self.replica.term());
                Ok(())
            }
            Err(fenced @ Refusal::Fenced(_)) => Err(fenced),
            // Not taken in this term (a lead lost, a proposal refused): the next lead proposes it.
            Err(refusal) => {
                tracing::debug!(target: "amphora.replication", event = "class_not_proposed", ?refusal);
                Ok(())
            }
        }
    }
    fn release(&mut self, output: Output<crate::replication::Applied>) -> Result<(), Refusal> {
        // Publish durable follower and leader state before releasing any result or read.
        if self.replica.applied().index != self.published {
            self.refresh()?;
        }
        for applied in output.answers {
            let matches = self
                .pending
                .as_ref()
                .is_some_and(|p| p.context == applied.context);
            if matches && let Some(mut pending) = self.pending.take() {
                let result = match pending.phase {
                    Phase::Proposed(value) => applied.result.map(|()| value),
                    // Answered on the commit: the apply must decide as the log did.
                    Phase::Answered { result: given, .. } => {
                        if given.is_ok() != applied.result.is_ok() {
                            return Err(fenced(
                                "a follower's answer on the commit differs from its write's apply",
                            ));
                        }
                        continue;
                    }
                    Phase::Barrier(_) => {
                        Err(fenced("a read barrier matched an unapplied proposal"))
                    }
                    Phase::Bootstrap(operation) => match applied.result {
                        Ok(()) => {
                            pending.phase = Phase::Barrier(operation);
                            match self.replica.read(pending.context.clone()) {
                                Ok(()) => {
                                    self.pending = Some(pending);
                                    continue;
                                }
                                Err(error) => Err(raft_error(error)),
                            }
                        }
                        Err(error) => Err(error),
                    },
                };
                #[cfg(test)]
                if self.turn_context.as_ref() == Some(&pending.context) {
                    self.turn_context = None;
                    if matches!(&result, Err(Refusal::Invalid { what, .. }) if what == "prepared transaction")
                    {
                        self.turns.1 = self.turns.1.saturating_add(1);
                    }
                }
                // A request to attest again, applied or refused by the stale-base rule (another
                // floor session at the same base committed first): the keeper's counted retry.
                if matches!(pending.author, Author::Owner) && self.floor_requesting {
                    self.floor_requesting = false;
                    match &result {
                        Ok(_) => {
                            self.floor_requests_applied =
                                self.floor_requests_applied.saturating_add(1);
                        }
                        Err(Refusal::Invalid { what, .. }) if what == "prepared transaction" => {
                            self.floor_requests_stale = self.floor_requests_stale.saturating_add(1);
                        }
                        Err(_) => {}
                    }
                }
                if let Some(response) = pending.response.take() {
                    answer(response, result);
                }
            }
        }
        for (context, _) in output.reads {
            if self.pending.as_ref().is_some_and(|p| p.context == context) {
                self.prepare()?;
            } else if self.changing.as_ref().is_some_and(|c| c.context == context) {
                self.propose_change();
            }
        }
        for entry in output.displaced {
            if self
                .pending
                .as_ref()
                .is_some_and(|p| p.context == entry.context)
                && let Some(mut pending) = self.pending.take()
                && let Some(response) = pending.response.take()
            {
                answer(response, Err(unknown()));
            }
        }
        // The heartbeats unanswered at which the core takes a peer for lost: an election timeout's.
        let config = self.replica.core().raft.config();
        let lost = config
            .election_tick
            .checked_div(config.heartbeat_tick)
            .and_then(|beats| u32::try_from(beats).ok())
            .unwrap_or(u32::MAX);
        let now = self.now()?;
        let mut outgoing = locked(&self.control.outgoing)?;
        for message in output.messages {
            self.pace.sent(&message, now, lost);
            let mut bytes = Vec::new();
            message.encode(&mut bytes);
            api::bound("replica outgoing record", bytes.len(), WIRE_BYTES)?;
            let total = outgoing
                .bytes
                .checked_add(bytes.len())
                .ok_or_else(|| fenced("outgoing size overflow"))?;
            if total > OUTGOING_BYTES || outgoing.messages.len() >= OUTGOING_MESSAGES {
                return Err(fenced(
                    "replica outgoing queue is full; drain and reopen from durable state",
                ));
            }
            outgoing.bytes = total;
            outgoing.messages.push_back(Outbound {
                to: message.to,
                bytes,
            });
        }
        Ok(())
    }
    /// The replicated class at a follower (replication.md, "A write made at a follower is answered
    /// on the leader's commit"): a write made here is answered once the commit this member has
    /// heard covers its entry, with what its apply will answer, decided from the log; its own log
    /// write and apply come after. Only entries past this member's log write are looked at, the
    /// core's unstable entries (`Limits::unstable_entries`); one this member's log holds already
    /// is answered at its apply, which follows.
    fn answer_committed(&mut self) {
        let raft = &self.replica.core().raft;
        if raft.config().fates.is_none() || self.replica.is_leader() {
            return;
        }
        let Some(pending) = self.pending.as_ref() else {
            return;
        };
        if !matches!(pending.phase, Phase::Proposed(_)) {
            return;
        }
        let log = raft.log();
        let committed = log.committed();
        let Some(entry) = log
            .unstable()
            .entries()
            .iter()
            .take_while(|entry| entry.index <= committed)
            .find(|entry| entry.context == pending.context)
        else {
            return;
        };
        // An entry whose predecessor's term this member no longer has is answered at its apply.
        let Some(before) = entry.index.checked_sub(1).and_then(|index| {
            Some(Point {
                index,
                term: log.term(index).ok()?,
            })
        }) else {
            return;
        };
        let index = entry.index;
        let result = match crate::replication::verdict(&entry.data, before) {
            Ok(result) => result,
            // A malformed entry fences at its apply, which answers.
            Err(_) => return,
        };
        let Some(mut pending) = self.pending.take() else {
            return;
        };
        if let Phase::Proposed(value) = std::mem::replace(
            &mut pending.phase,
            Phase::Answered {
                result: result.clone(),
                index,
            },
        ) && let Some(response) = pending.response.take()
        {
            answer(response, result.map(|()| value));
        }
        self.pending = Some(pending);
    }
    fn context(&mut self) -> Result<Vec<u8>, Refusal> {
        self.serial = self
            .serial
            .checked_add(1)
            .ok_or_else(|| fenced("request identity exhausted"))?;
        // Followers can reopen within a term; the durable shell's fresh incarnation disambiguates
        // an old log entry from a new request at the same origin.
        let mut context = self.replica.id().to_le_bytes().to_vec();
        context.extend_from_slice(&self.replica.term().to_le_bytes());
        context.extend_from_slice(&self.replica.core().raft.config().incarnation.to_le_bytes());
        context.extend_from_slice(&self.serial.to_le_bytes());
        Ok(context)
    }
    fn bootstrap(&mut self) -> Result<(), Refusal> {
        if !self.replica.machine().bootstrap_pending() || self.pending.is_some() {
            return Ok(());
        }
        let source = self
            .replica
            .machine()
            .bootstrap_source()
            .ok_or_else(|| fenced("pending bootstrap has no designated source"))?;
        if source != self.replica.id() || !self.replica.is_leader() {
            // A failed transfer can be requested again after the core's election timeout. At most
            // one such request is made per period, never a host election.
            let period = u32::try_from(self.replica.core().raft.config().election_tick)
                .ok()
                .and_then(|ticks| self.pace.period().checked_mul(ticks))
                .ok_or_else(|| fenced("tick period overflow"))?;
            let due = self.transferred.is_none_or(|(term, at)| {
                term != self.replica.term() || self.at.saturating_duration_since(at) >= period
            });
            if due
                && self.replica.leader() != 0
                && (self.replica.is_leader() || source == self.replica.id())
            {
                self.replica.transfer(source).map_err(raft_error)?;
                self.transferred = Some((self.replica.term(), self.at));
            }
            return Ok(());
        }
        let context = self.context()?;
        self.replica.read(context.clone()).map_err(raft_error)?;
        // One internal attempt has a finite lease, its role's (`own_session_lease`); committed
        // chunk progress survives its expiry. User requests retain their own deadlines in the
        // bounded waiting queue throughout.
        let until = self
            .at
            .checked_add(self.own_session_lease()?)
            .ok_or_else(|| fenced("bootstrap deadline overflow"))?;
        self.pending = Some(Pending {
            context,
            term: self.replica.term(),
            leader: self.replica.leader(),
            until,
            namespace: String::new(),
            author: Author::Owner,
            response: None,
            phase: Phase::Barrier(None),
            _reservation: Reservation {
                control: Arc::clone(&self.control),
                bytes: 0,
            },
        });
        Ok(())
    }
    /// The floor's keeper (replication.md, "Attestation" and "The upgrade that introduces the join
    /// rows"), when this owner has nothing else in hand and something was applied since it last
    /// looked, or one of its sessions ended: this member attests when its attestation of the
    /// application it is bound to is missing or stale; the leader writes marker J once every
    /// member's attestation of its own application counts, or asks them all to attest again when
    /// its own knowledge of the last change is above theirs. Only a fenced machine ends the owner:
    /// any other failure is reported and the keeper looks again on its next pass.
    fn keep_floor(&mut self) -> Result<(), Refusal> {
        // A floor session that ended, applied or refused or dropped, is looked at again at once,
        // so a quiet group never leaves one lost.
        if self.floor_inflight && self.pending.is_none() {
            self.floor_inflight = false;
            self.floor_checked = 0;
            self.floor_ended = self.floor_ended.saturating_add(1);
        }
        let applied = self.replica.applied().index;
        let configuration_at = self.replica.machine().configuration_at();
        if self.pending.is_some()
            || !self.waiting.is_empty()
            || self.changing.is_some()
            || self.transferring.is_some()
            || self.replica.leader() == 0
            || self.replica.machine().bootstrap_pending()
            || applied == self.floor_checked
            || self.floor_settled == Some(configuration_at)
            || (self.attesting.is_none() && !self.replica.is_leader())
        {
            return Ok(());
        }
        self.floor_checked = applied;
        match self.floor_pass(configuration_at) {
            Ok(()) => {
                self.control.floor_meter.kept(false);
                Ok(())
            }
            Err(Refusal::Fenced(reason)) => Err(Refusal::Fenced(reason)),
            Err(refusal) => {
                tracing::warn!(
                    target: "amphora.floor",
                    event = "floor.keeper_failed",
                    error = %refusal,
                    "the floor's keeper looks again on its next pass"
                );
                self.floor_failures = self.floor_failures.saturating_add(1);
                self.control.floor_meter.kept(true);
                Ok(())
            }
        }
    }
    fn floor_pass(&mut self, configuration_at: u64) -> Result<(), Refusal> {
        use crate::replication::floor;
        let me = self.replica.id();
        let members = floor::members(self.replica.configuration());
        let leads = self.replica.is_leader() && !self.replica.core().raft.has_pending_conf();
        let attesting = self.attesting.clone();
        let (mine, tracked, all, requested, any) = self.replica.machine_mut().read_db(|db| {
            let (mine, mut all) = match &attesting {
                Some(application) => {
                    let mine = floor::current(db, me, &application.name, configuration_at)?
                        .is_some_and(|attested| attested.highest == application.version);
                    (mine, true)
                }
                None => (true, false),
            };
            if let Some(application) = &attesting {
                for member in &members {
                    all &=
                        floor::current(db, *member, &application.name, configuration_at)?.is_some();
                }
            }
            let tracked = floor::tracked(db)?.is_some();
            let requested = floor::requested(db)?;
            let any = floor::any_attested(db)?;
            Ok((mine, tracked, all, requested, any))
        })?;
        if !mine && let Some(application) = attesting.clone() {
            self.floor_attestations = self.floor_attestations.saturating_add(1);
            return self.submit_floor(Box::new(move |db| {
                floor::attest(db, me, &application.name, application.version)
            }));
        }
        if tracked {
            self.floor_settled = Some(configuration_at);
            return Ok(());
        }
        // A group where nobody has bound an application has nothing to prove, and a leader bound
        // to none has none to prove: the leader writes nothing for the floor until then.
        let Some(application) = attesting.filter(|_| leads && any) else {
            return Ok(());
        };
        let configuration = self.replica.configuration().clone();
        if all {
            return self.submit_floor(Box::new(move |db| {
                floor::track_joins(db, &configuration, &application.name, configuration_at)
                    .map(|_| ())
            }));
        }
        // While any member lacks an attestation that counts, at most one request for each
        // configuration_at this leader holds: one already at or past it is outstanding, and every
        // member's keeper answers it once.
        if requested >= configuration_at {
            return Ok(());
        }
        self.floor_requests = self.floor_requests.saturating_add(1);
        self.submit_floor(Box::new(move |db| floor::request(db, configuration_at)))?;
        self.floor_requesting = true;
        Ok(())
    }
    /// The lease of one of the owner's own sessions (a floor session, the seed's attempt), read
    /// from its role's raft timing when it is made (replication.md, "Attestation";
    /// amphora-seam-owner-2, amphora-owner-t1-constants-floor-session-20261111). A leader's is two
    /// of its quorum checks, 2 × (election_tick + quorum_patience) ticks: the check runs on its own
    /// counter and clears the answers' marks only when it runs, so a stall that begins just after
    /// an answer passes the next check and steps the leader down at the one after (raft.rs,
    /// `quorum_due`, `tick_heartbeat`). A follower's is its own election bound and stall
    /// patience, (2 × election_tick + patience) ticks: it campaigns within a randomized timeout
    /// below twice election_tick, plus its patience (`tick_election`). Past it a session is in a
    /// role that has moved on, where it can't apply. A live-quorum leader's session can still
    /// reach it, behind a slow flush or a ReadIndex backlog, and then ends unapplied: safe, since
    /// the stale-base rule refuses a late apply and the keeper makes it again. Those are counted
    /// (`own_sessions_expired`), since many would mean the patience is too short.
    fn own_session_lease(&self) -> Result<Duration, Refusal> {
        let raft = &self.replica.core().raft;
        let election = raft.config().election_tick;
        let ticks = if self.replica.is_leader() {
            election
                .checked_add(raft.quorum_patience())
                .and_then(|due| due.checked_mul(2))
        } else {
            election
                .checked_mul(2)
                .and_then(|bound| bound.checked_add(raft.patience()))
        };
        ticks
            .and_then(|ticks| u32::try_from(ticks).ok())
            .and_then(|ticks| self.pace.period().checked_mul(ticks))
            .ok_or_else(|| fenced("own session lease overflow"))
    }
    /// One of the floor's own sessions, through the same barrier and proposal as a call; it has no
    /// caller, and one that ends without applying is made again on the keeper's next pass.
    fn submit_floor(&mut self, session: FloorSession) -> Result<(), Refusal> {
        // Each floor session is the next one in hand; a request marks itself after this, and its
        // wait for the log takes its own target.
        self.floor_requesting = false;
        self.tail = None;
        let context = self.context()?;
        self.replica.read(context.clone()).map_err(raft_error)?;
        let until = self
            .at
            .checked_add(self.own_session_lease()?)
            .ok_or_else(|| fenced("floor session deadline overflow"))?;
        let call: Call = Box::new(move |db| {
            session(db)?;
            Ok(Box::new(()) as Box<dyn Any + Send>)
        });
        self.pending = Some(Pending {
            context,
            term: self.replica.term(),
            leader: self.replica.leader(),
            until,
            namespace: String::new(),
            author: Author::Owner,
            response: None,
            phase: Phase::Barrier(Some(Operation::Call(Mutex::new(Some(call))))),
            _reservation: Reservation {
                control: Arc::clone(&self.control),
                bytes: 0,
            },
        });
        self.floor_inflight = true;
        Ok(())
    }
    fn entry_room(&self, context: usize) -> Result<usize, Refusal> {
        self._log
            .entry_room()
            .map_err(unavailable)?
            .checked_sub(hyper_durable::ENTRY_OVERHEAD)
            .and_then(|room| room.checked_sub(context))
            .and_then(|room| room.checked_sub(crate::replication::closed::STAMP_BYTES))
            .ok_or_else(|| fenced("replication log cannot hold the proposal envelope"))
    }
    /// Proposes `bytes`, stamped with this owner's clock (replication.md, "Follower reads at a
    /// closed time"): every entry an owner proposes carries one, its room taken out of
    /// [`Actor::entry_room`].
    fn propose(&mut self, context: &[u8], bytes: Vec<u8>) -> Result<(), Refusal> {
        api::bound(
            "replication log entry",
            bytes.len(),
            self.entry_room(context.len())?,
        )?;
        let stamped = crate::replication::closed::stamp(self.wall_now()?, &bytes);
        self.replica
            .propose(context.to_vec(), stamped)
            .map_err(raft_error)
    }
    /// This owner's wall clock: the wall clock when it started, moved on by its own clock, in
    /// microseconds since the Unix epoch.
    fn wall_now(&self) -> Result<u64, Refusal> {
        u64::try_from(self.at.saturating_duration_since(self.started).as_micros())
            .ok()
            .and_then(|since| self.wall.checked_add(since))
            .ok_or_else(|| fenced("the owner's wall clock overflowed"))
    }
    /// Publishes this replica's closed time if it moved since the last step, with the lag it
    /// measured, and tells the reads waiting for it.
    fn publish_closed(&mut self) -> Result<(), Refusal> {
        let time = self.replica.machine().closed();
        let mut closed = locked(&self.control.closed)?;
        if closed.time == time {
            return Ok(());
        }
        if let Some(time) = time {
            self.lag.moved(self.wall_now()?, time);
        }
        *closed = Closed {
            time,
            lag: self.lag.p99(),
        };
        drop(closed);
        self.control.moved.notify_all();
        Ok(())
    }
    fn prepare(&mut self) -> Result<(), Refusal> {
        let Some(mut pending) = self.pending.take() else {
            return Ok(());
        };
        // A delayed read completion must not refuse or replay an already proposed operation.
        if !matches!(pending.phase, Phase::Barrier(_)) {
            self.pending = Some(pending);
            return Ok(());
        }
        if self.at >= pending.until {
            if let Some(response) = pending.response.take() {
                answer(response, Err(Refusal::Timeout));
            }
            return Ok(());
        }
        if pending.term != self.replica.term()
            || pending.leader != self.replica.leader()
            || self.replica.leader() == 0
        {
            self.requeue(pending);
            return Ok(());
        }
        // A leader holding a write slot for a member prepares nothing of its own until the slot
        // ends; its run loop asks again each step.
        self.slot_deferred = false;
        if self.replica.is_leader() && self.slots.holds() {
            self.slot_deferred = true;
            self.pending = Some(pending);
            return Ok(());
        }
        // A floor session, and since the write slot every write a follower makes and the
        // leader's own write in its turn after a slot, prepares only once this member has
        // applied its own whole log: its barrier covers what was committed, and an entry
        // appended ahead of it, not yet committed, would make its base stale, and members out of
        // phase would keep missing each other (amphora-seam-owner's ruling on the keeper's
        // livelock, b'; a slot's holder's retry prepares on the leader's tail, and the leader's
        // turn on the holder's entry, amphora-owner-t1-g-slot-878285a-20261111).
        // Local, bounded by the write's own deadline; the run loop asks again each step.
        // The target is this member's last log index as its barrier passed, fixed then: entries
        // appended after it never move it, so the write neither waits out its deadline nor
        // holds the write slot through others' writes (amphora-seam-owner's review of 392902d).
        if (matches!(pending.author, Author::Owner)
            || !self.replica.is_leader()
            || self.slots.turn())
            && matches!(pending.phase, Phase::Barrier(Some(_)))
            && !self.replica.machine().bootstrap_pending()
        {
            let target = match self.tail {
                Some(target) => target,
                None => self
                    .replica
                    .core()
                    .raft
                    .log()
                    .last_index()
                    .map_err(unavailable)?,
            };
            if self.replica.applied().index < target {
                self.tail = Some(target);
                self.pending = Some(pending);
                return Ok(());
            }
        }
        self.tail = None;
        let Phase::Barrier(operation) = pending.phase else {
            self.pending = Some(pending);
            return Ok(());
        };
        if self.replica.machine().bootstrap_pending() {
            let source = self
                .replica
                .machine()
                .bootstrap_source()
                .ok_or_else(|| fenced("pending bootstrap has no designated source"))?;
            if source != self.replica.id() || !self.replica.is_leader() {
                // The core's transfer request names its target in `from`. Only the source can
                // forward that request authenticated as itself; a leader executes it locally.
                if self.replica.is_leader() || source == self.replica.id() {
                    self.replica.transfer(source).map_err(raft_error)?;
                }
                if let Some(response) = pending.response.take() {
                    answer(
                        response,
                        Err(Refusal::NotLeader {
                            leader: Some(source),
                        }),
                    );
                }
                return Ok(());
            }
            let room = self
                .entry_room(pending.context.len())?
                .min(crate::replication::COMMAND_BYTES);
            let seed = self.replica.machine_mut().prepare_bootstrap(room);
            let result = match seed {
                Ok(Some(bytes)) => self.propose(&pending.context, bytes),
                Ok(None) => Err(fenced("pending bootstrap produced no seed chunk")),
                Err(error) => Err(error),
            };
            match result {
                Ok(()) => {
                    pending.phase = Phase::Bootstrap(operation);
                    self.pending = Some(pending);
                }
                Err(error) => {
                    if let Some(response) = pending.response.take() {
                        answer(response, Err(error));
                    }
                }
            }
            return Ok(());
        }
        let Some(operation) = operation else {
            return Ok(());
        };
        let bounded = matches!(
            &operation,
            Operation::Mutation(Mutation::CompactStep { .. })
        );
        // A caller's session runs only if its handle's claim holds against the floor at this
        // session's base; the stale-base rule refuses it at apply if a raise commits first.
        let claim = match &pending.author {
            Author::Application(application) => Some(application.clone()),
            Author::Owner => None,
        };
        // The claim is checked at the base the session prepares at (nothing applies between),
        // and its refusal is its caller's answer, counted under its reason: never the poison rule
        // below, which is for a compaction's own failures.
        if let Some(application) = &claim {
            let checked = self.replica.machine_mut().read_db(|db| {
                super::application::claim_refused(db, application.as_ref(), &operation)
            });
            let refusal = match checked {
                Ok(None) => None,
                Ok(Some(refused)) => {
                    floor_refused(&self.control, refused.reason, &refused.refusal);
                    Some(refused.refusal)
                }
                Err(refusal) => Some(refusal),
            };
            if let Some(refusal) = refusal {
                if let Some(response) = pending.response.take() {
                    answer(response, Err(refusal));
                }
                return Ok(());
            }
        }
        let prepared = if let Operation::Mutation(Mutation::CompactStep { before, .. }) = &operation
        {
            let room = self.entry_room(pending.context.len())?;
            self.replica
                .machine_mut()
                .prepare_compaction(&pending.namespace, *before, pending.until, room)
                .map(|(prepared, progress)| (prepared, Answer::Compacted(progress)))
        } else {
            let clock = Arc::clone(&self.clock);
            self.replica.machine_mut().prepare_db(|db| {
                // The guard ends with application evaluation, before prepare_pages rolls back its
                // speculative transaction. Cleanup must run even after the SQL deadline elapsed.
                let _deadline = db.deadline_on(pending.until, move || clock());
                if restore::incomplete(db)? {
                    return Err(api::invalid("database", "restore is incomplete").into());
                }
                match &operation {
                    Operation::Call(call) => {
                        let call = locked(call)?
                            .take()
                            .ok_or_else(|| unavailable("SQL session already executed"))?;
                        call(db).map(Answer::Called)
                    }
                    other => apply(db, &pending.namespace, other)?.map_err(Into::into),
                }
            })
        };
        // The preparation ran the caller's work, which takes time of its own: its deadline is
        // judged at the clock's reading after it, never at the step's start.
        self.at = (self.clock)();
        // A failed whole rollback poisons this machine. Stop its existing owner instead of
        // accepting another request against an unverified speculative base.
        if bounded && let Err(Refusal::Fenced(reason)) = &prepared {
            return Err(fenced(reason));
        }
        let result = match prepared {
            Ok((prepared, value)) => {
                if self.at >= pending.until {
                    Err(Refusal::Timeout)
                } else if prepared.is_empty() {
                    Ok(value)
                } else {
                    match prepared
                        .encode()
                        .and_then(|bytes| self.propose(&pending.context, bytes))
                    {
                        Ok(()) => {
                            if self.replica.is_leader() {
                                #[cfg(test)]
                                if self.slots.turn() {
                                    self.turn_context = Some(pending.context.clone());
                                    self.turns.0 = self.turns.0.saturating_add(1);
                                }
                                self.slots.took_turn();
                            }
                            pending.phase = Phase::Proposed(value);
                            self.pending = Some(pending);
                            return Ok(());
                        }
                        Err(error) => Err(error),
                    }
                }
            }
            Err(error) => Err(if self.at >= pending.until {
                Refusal::Timeout
            } else {
                error
            }),
        };
        if let Some(response) = pending.response.take() {
            answer(response, result);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    /// A test relay's yield to the disk when it had nothing to move: a hang guard on the test's
    /// own pacing, which no verdict depends on (replication.md, "The owner's time"). The owner
    /// itself waits on its bell, never on this.
    const POLL: Duration = Duration::from_millis(1);
    // Follower reads at a closed time, under leader changes (replication.md, "Follower reads at a
    // closed time"): in a file of its own, on this module's driven shell.
    mod closed_reads;

    thread_local! {
        /// The account the owners a test opens on its thread charge their readers to, in place of
        /// the process's (`account`), so that other tests' readers don't share its budget.
        pub(super) static ACCOUNT: std::cell::RefCell<Option<Arc<Account>>> =
            const { std::cell::RefCell::new(None) };
    }

    fn value(db: &mut Db) -> anyhow::Result<Vec<u8>> {
        let mut value = Vec::new();
        db.query("SELECT value FROM actor_items", [], |row| {
            value = row
                .get_value(0)
                .to_blob()
                .ok_or_else(|| anyhow::anyhow!("expected blob"))?
                .to_vec();
            Ok(false)
        })?;
        Ok(value)
    }

    /// U15/CP-T1.8 on driven time: the product owner must not run a closure before leadership and
    /// ReadIndex, and authentication binds a wire record to its sender. Do: call a lone member of
    /// three, which has no leader, and step it with its clock a nanosecond before the call's
    /// deadline, then at it. Expect: no answer before the deadline, Timeout at it, the closure never
    /// run; then records whose claimed sender isn't the authenticated one, or isn't a member,
    /// refused, and a drain past the outgoing bound refused too.
    #[test]
    fn unknown_leader_waits_to_deadline_without_evaluating_and_authentication_binds_wire_sender() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let client = driven.open(dir.path(), 1, vec![1, 2, 3], None);
        let evaluated = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&evaluated);
        let until = driven.until();
        let answer = submit(&client, until, move |_| {
            count.fetch_add(1, Ordering::Relaxed);
            Ok(Vec::new())
        });
        let clock = driven.clock(0);
        set(&clock, driven.base, until - Duration::from_nanos(1));
        driven.step();
        assert!(matches!(answer.try_recv(), Err(mpsc::TryRecvError::Empty)));
        set(&clock, driven.base, until);
        driven.step();
        assert!(matches!(answer.try_recv(), Ok(Err(Refusal::Timeout))));
        assert_eq!(evaluated.load(Ordering::Relaxed), 0);
        let mut forged = Vec::new();
        Message {
            from: 2,
            to: 1,
            ..Message::default()
        }
        .encode(&mut forged);
        let mut foreign = Vec::new();
        Message {
            from: 4,
            to: 1,
            ..Message::default()
        }
        .encode(&mut foreign);
        let mut receipts = [
            client
                .handle()
                .admit_receive(3, forged, driven.until())
                .unwrap(),
            client
                .handle()
                .admit_receive(4, foreign, driven.until())
                .unwrap(),
        ];
        driven.step();
        for receipt in &mut receipts {
            assert!(matches!(receipt.try_result(), Err(Refusal::Invalid { .. })));
        }
        assert!(matches!(
            client.handle().drain(OUTGOING_MESSAGES + 1),
            Err(Refusal::TooLarge { .. })
        ));
    }

    /// CP-T1.8 on driven time: admitted application calls cannot consume replication's reserved
    /// byte capacity. Do: fill a leaderless member's queue with calls until it refuses one, then
    /// admit a member's heartbeat and step the member. Expect: the refusal is the application
    /// bound, the heartbeat processed, and no call run.
    #[test]
    fn unknown_leader_application_pressure_keeps_wire_admission_available() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let client = driven.open(dir.path(), 1, vec![1, 2, 3], None);
        let handle = client.handle();
        let until = driven.until();
        let evaluated = Arc::new(AtomicUsize::new(0));
        let mut replies = Vec::new();
        let mut refusal = None;
        for _ in 0..COMMANDS {
            let counted = Arc::clone(&evaluated);
            let operation = Operation::Call(Mutex::new(Some(Box::new(move |_| {
                counted.fetch_add(1, Ordering::Relaxed);
                Ok(Box::new(Vec::<u8>::new()) as Box<dyn Any + Send>)
            }))));
            let (response, receiver) = mpsc::sync_channel(1);
            match handle.enqueue(
                Input::Submit {
                    namespace: String::new(),
                    operation,
                    application: None,
                    until,
                    response,
                },
                api::REQUEST_MAX,
            ) {
                Ok(()) => replies.push(receiver),
                Err(error) => {
                    refusal = Some(error);
                    break;
                }
            }
        }
        assert_eq!(evaluated.load(Ordering::Relaxed), 0);
        let mut bytes = Vec::new();
        Message {
            msg_type: hyper_raft::proto::MessageType::MsgHeartbeat,
            from: 2,
            to: 1,
            term: 1,
            ..Message::default()
        }
        .encode(&mut bytes);
        let mut heartbeat = handle.admit_receive(2, bytes, until).unwrap();
        driven.step();
        assert!(matches!(heartbeat.try_result(), Ok(Some(()))));
        assert!(matches!(
            refusal,
            Some(Refusal::Full { limit }) if limit == QUEUED_BYTES - WIRE_BYTES
        ));
        assert_eq!(
            replies.len(),
            (QUEUED_BYTES - WIRE_BYTES) / api::REQUEST_MAX
        );
        assert!(!replies.is_empty());
        assert_eq!(evaluated.load(Ordering::Relaxed), 0);
    }

    /// U15/CP-T1.8 on driven time: a real DeviceFile log, a product-driven election, a once-only
    /// capture and bounded GC by the same owner, all kept through a durable restart. Do: a single
    /// voter captures a random value, puts and deletes a record, compacts, then restarts. Expect:
    /// the closure run once, the value and the deletion's change back after the restart, and a
    /// second compaction with nothing left to visit.
    #[test]
    fn single_voter_actor_captures_once_and_reopens_durable_random_value() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = vec![driven.open(dir.path(), 1, vec![1], None)];
        let mut pump = Pump::new(&clients);
        driven.elect(&mut pump, &clients, 0, 1);
        let client = clients.first().unwrap();
        let evaluated = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&evaluated);
        let chosen = driven.call(&mut pump, client, move |db| {
            count.fetch_add(1, Ordering::Relaxed);
            db.exec("CREATE TABLE actor_items(value BLOB DEFAULT(randomblob(32))); INSERT INTO actor_items DEFAULT VALUES")?;
            value(db)
        }).unwrap();
        assert_eq!(evaluated.load(Ordering::Relaxed), 1);
        assert_eq!(driven.call(&mut pump, client, value).unwrap(), chosen);
        // Bounded GC is captured by this same owner and survives its durable restart.
        for writes in [
            vec![Write::Put {
                family: "items".into(),
                key: b"retired".to_vec(),
                value: b"old".to_vec(),
            }],
            vec![Write::Delete {
                family: "items".into(),
                key: b"retired".to_vec(),
            }],
        ] {
            let records = Operation::Mutation(Mutation::Records {
                namespace: "gc".into(),
                tx: Tx {
                    conds: vec![],
                    writes,
                },
            });
            let answer = enqueue(client, driven.until(), "gc", records);
            driven.answer(&mut pump, &answer).unwrap();
        }
        let step = Operation::Mutation(Mutation::CompactStep {
            namespace: "gc".into(),
            before: 2,
        });
        let answer = enqueue(client, driven.until(), "gc", step);
        let Answer::Compacted(progress) = driven.answer(&mut pump, &answer).unwrap() else {
            panic!("wrong compaction answer")
        };
        assert_eq!(progress.completed, 2);
        assert_eq!(progress.feed, 1);
        let applied = client.handle().status().unwrap().applied;
        // Past hyper-durable's quiet period, so the owner states its durable commit before it stops.
        driven.advance(0, Duration::from_millis(60));
        for _ in 0..8 {
            driven.round(&mut pump);
        }
        driven.drain(&mut pump);
        pump.finish();
        drop(clients);
        drop(driven);

        let mut driven = Driven::new();
        let reopened = vec![driven.open(dir.path(), 1, vec![1], None)];
        let mut pump = Pump::new(&reopened);
        driven.elect(&mut pump, &reopened, 0, applied);
        let client = reopened.first().unwrap();
        assert!(client.handle().status().unwrap().applied >= applied);
        assert_eq!(driven.call(&mut pump, client, value).unwrap(), chosen);
        let changes = enqueue(
            client,
            driven.until(),
            "gc",
            Operation::Changes("items".into(), 1, 10),
        );
        let Answer::Changes(changes) = driven.answer(&mut pump, &changes).unwrap() else {
            panic!("wrong changes answer")
        };
        assert_eq!(changes.changes.len(), 1);
        assert_eq!(changes.changes[0].value, None);
        let step = Operation::Mutation(Mutation::CompactStep {
            namespace: "gc".into(),
            before: 2,
        });
        let answer = enqueue(client, driven.until(), "gc", step);
        let Answer::Compacted(progress) = driven.answer(&mut pump, &answer).unwrap() else {
            panic!("wrong recovered compaction answer")
        };
        assert_eq!(progress.completed, 2);
        assert_eq!(progress.visited, 0);
        assert_eq!(evaluated.load(Ordering::Relaxed), 1);
        driven.drain(&mut pump);
        pump.finish();
    }
    struct PumpPacket {
        from: u64,
        message: Outbound,
        until: Instant,
    }
    struct PumpReceipt {
        receipt: ReceiveReceipt,
        bytes: usize,
        proposal: bool,
        from: u64,
        to: u64,
        // The record, as its type and its wire sender and recipient, for a failure to name.
        record: String,
    }
    enum PumpPhase {
        Relay,
        Captures {
            leader: usize,
            held: [Option<PumpPacket>; 2],
            // How many of `held` to fill before they may be released: one or two.
            want: usize,
        },
    }
    /// Says whether the pump loses a record, given the member that sent it.
    type Losing = Box<dyn Fn(u64, &Message) -> bool>;
    /// CP-T1.8: mirror the carrier's bounded admission and observe every processed receipt.
    /// Admission never waits for another owner. All records, including retained proposals,
    /// share COMMANDS/QUEUED_BYTES bounds; each keeps its first absolute receive deadline.
    struct Pump<'a> {
        clients: &'a [Client],
        receipts: [Option<PumpReceipt>; COMMANDS],
        phase: PumpPhase,
        next_source: usize,
        records: usize,
        bytes: usize,
        // A nemesis's elections: a leader handing off its lead, and an old leader that has taken
        // the new term before it hears the new leader, refuse the proposals forwarded to them
        // (replication.md, "A forward that meets an election"). Those refusals are counted here
        // instead of failing the test.
        refusals: Option<usize>,
        // Members removed from the group: what they sent before the removal reaches a leader that
        // no longer counts them, which refuses it as a non-member's.
        departed: Vec<u64>,
        // Records admitted to each member, by its identity.
        delivered: BTreeMap<u64, usize>,
        // Members cut off: every record to one is lost, as on a link that is down.
        lost: Vec<u64>,
        // While some, the durable answers to appends, kept back in order: held answers pass, so
        // only a held quorum can commit (the suspicion feed's test). Counted in `records`.
        // This holds answers in transit, not the member's disk: a heartbeat's answer stating
        // the member's log position passes it, and the leader counts that position toward
        // `matched` as an append's answer (hyper-raft's handle_heartbeat_response). That is
        // sound, since a member that doesn't lead sends it only once its log is durable
        // (hyper-durable's persisted messages), but it means a held test that moves the
        // leader's clock lets durable progress through. To model a disk that hasn't finished, a
        // test holds every persisted message, heartbeat answers included, or moves no clock
        // (as a_promoted_joiner_counts_in_a_held_quorum does).
        durable: Option<Vec<PumpPacket>>,
        // Owners whose records stay in their own queues, untaken: a follower whose answers are
        // held, as its own slow write holds them (kiln goal 6b112f14).
        held: Vec<usize>,
        // Every MsgAppend lost while heartbeats go on: a configuration's way out appended stays
        // uncommitted while a ReadIndex still completes.
        appends_lost: bool,
        // The records the test loses, by the member that sent them and what they are.
        losing: Option<Losing>,
        // While some, every record waits a seeded number of relay rounds before it is admitted,
        // in order on each link: a network's round trips, in driven time (goal 9c63f7d4).
        delay: Option<Delay>,
    }
    /// Records held back by [`Pump::delay`]: each with the relay round it is admitted at.
    struct Delay {
        mix: u64,
        most: u64,
        round: u64,
        held: VecDeque<(u64, PumpPacket)>,
        last: BTreeMap<(u64, u64), u64>,
    }
    impl Delay {
        fn new(seed: u64, most: u64) -> Self {
            Self {
                mix: seed,
                most,
                round: 0,
                held: VecDeque::new(),
                last: BTreeMap::new(),
            }
        }
        fn wait(&mut self) -> u64 {
            mix(&mut self.mix) % (self.most + 1)
        }
    }
    /// SplitMix64's step (Steele, Lea and Flood, OOPSLA 2014): a seeded test's draws.
    fn mix(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    impl<'a> Pump<'a> {
        fn new(clients: &'a [Client]) -> Self {
            assert!(!clients.is_empty() && clients.len() <= MEMBERS);
            Self {
                clients,
                receipts: std::array::from_fn(|_| None),
                phase: PumpPhase::Relay,
                next_source: 0,
                records: 0,
                bytes: 0,
                refusals: None,
                departed: Vec::new(),
                delivered: BTreeMap::new(),
                lost: Vec::new(),
                durable: None,
                held: Vec::new(),
                appends_lost: false,
                losing: None,
                delay: None,
            }
        }
        /// Admits every record the delay holds, and holds no more.
        fn undelay(&mut self) {
            if let Some(delay) = self.delay.take() {
                for (_, packet) in delay.held {
                    self.admit(packet);
                }
            }
        }
        fn reap(&mut self) {
            for slot in &mut self.receipts {
                let Some(pending) = slot.as_mut() else {
                    continue;
                };
                let result = pending.receipt.try_result();
                if matches!(result, Ok(None)) {
                    continue;
                }
                let pending = slot.take().unwrap();
                self.records -= 1;
                self.bytes -= pending.bytes;
                if let (Some(refusals), true, Err(Refusal::Unavailable(why))) =
                    (self.refusals.as_mut(), pending.proposal, &result)
                    && [
                        hyper_raft::Dropped::Transferring,
                        hyper_raft::Dropped::NoLeader,
                    ]
                    .iter()
                    .any(|cause| why.ends_with(&cause.to_string()))
                {
                    *refusals += 1;
                    continue;
                }
                if let Err(Refusal::Invalid { why, .. }) = &result
                    && why == "sender is not a current member"
                    && self.departed.contains(&pending.from)
                {
                    continue;
                }
                // Admission was not an ACK: expiry, validation and fencing must fail the test.
                match result {
                    Ok(answered) => assert_eq!(answered, Some(())),
                    Err(refusal) => panic!("{} refused: {refusal:?}", pending.record),
                }
            }
        }
        fn admit(&mut self, packet: PumpPacket) {
            let peer = self
                .clients
                .get(usize::try_from(packet.message.to - 1).unwrap())
                .unwrap();
            let bytes = packet.message.bytes.len();
            let decoded = Message::decode(&packet.message.bytes).unwrap();
            let proposal = decoded.msg_type == hyper_raft::proto::MessageType::MsgPropose;
            let record = format!(
                "{:?} from {} (wire {}) to {} (wire {}), term {}",
                decoded.msg_type,
                packet.from,
                decoded.from,
                packet.message.to,
                decoded.to,
                decoded.term
            );
            *self.delivered.entry(packet.message.to).or_default() += 1;
            let receipt = peer
                .handle()
                .admit_receive(packet.from, packet.message.bytes, packet.until)
                .unwrap();
            let slot = self
                .receipts
                .iter_mut()
                .find(|slot| slot.is_none())
                .unwrap();
            *slot = Some(PumpReceipt {
                receipt,
                bytes,
                proposal,
                from: packet.from,
                to: packet.message.to,
                record,
            });
        }
        /// One relay round, each record admitted with `until` as its receive deadline.
        fn drive_until(&mut self, until: Instant) -> usize {
            self.reap();
            let mut released = 0;
            if let Some(delay) = self.delay.as_mut() {
                delay.round += 1;
                let round = delay.round;
                let mut due = Vec::new();
                while delay.held.front().is_some_and(|(at, _)| *at <= round) {
                    due.push(delay.held.pop_front().unwrap().1);
                }
                for packet in due {
                    self.admit(packet);
                    released += 1;
                }
            }
            let first = self.next_source;
            self.next_source = (first + 1) % self.clients.len();
            let mut admitted = 0;
            // U15/CP-T1.8: match the host carrier's bounded 16-record quantum. One
            // record per origin each round preserves fairness and the held-origin barrier;
            // control traffic must not consume the entire relay service opportunity.
            for _ in 0..16 {
                let mut drained = false;
                for offset in 0..self.clients.len() {
                    // Reserve room for any legal wire record before draining ownership from a peer.
                    // The conservative byte check needs no unbounded overflow/backpressure buffer.
                    if self.records == COMMANDS || self.bytes + WIRE_BYTES > QUEUED_BYTES {
                        break;
                    }
                    let index = (first + offset) % self.clients.len();
                    if self.held.contains(&index) {
                        continue;
                    }
                    let from = u64::try_from(index + 1).unwrap();
                    // No later record from this origin may overtake its retained proposal.
                    if let PumpPhase::Captures { held, .. } = &self.phase
                        && held.iter().flatten().any(|packet| packet.from == from)
                    {
                        continue;
                    }
                    for message in self.clients.get(index).unwrap().handle().drain(1).unwrap() {
                        drained = true;
                        if self.lost.contains(&message.to)
                            || (self.appends_lost
                                && Message::decode(&message.bytes).unwrap().msg_type
                                    == hyper_raft::proto::MessageType::MsgAppend)
                            || self.losing.as_ref().is_some_and(|losing| {
                                losing(from, &Message::decode(&message.bytes).unwrap())
                            })
                        {
                            continue;
                        }
                        api::bound("fixture wire record", message.bytes.len(), WIRE_BYTES).unwrap();
                        let packet = PumpPacket {
                            from,
                            message,
                            until,
                        };
                        self.records += 1;
                        self.bytes += packet.message.bytes.len();
                        if let PumpPhase::Captures { leader, held, want } = &mut self.phase {
                            let record = Message::decode(&packet.message.bytes).unwrap();
                            if record.msg_type == hyper_raft::proto::MessageType::MsgPropose {
                                assert_ne!(index, *leader);
                                assert_eq!(packet.message.to, u64::try_from(*leader + 1).unwrap());
                                assert!(held.iter().flatten().all(|other| other.from != from));
                                assert!(held.iter().flatten().count() < *want);
                                *held.iter_mut().find(|slot| slot.is_none()).unwrap() =
                                    Some(packet);
                                continue;
                            }
                        }
                        if let Some(delay) = self.delay.as_mut() {
                            let link = (packet.from, packet.message.to);
                            let at = (delay.round + 1 + delay.wait())
                                .max(delay.last.get(&link).copied().unwrap_or(0));
                            delay.last.insert(link, at);
                            // Held in admission order; one due sooner waits behind it, as a later
                            // record on its link does.
                            delay.held.push_back((at, packet));
                            admitted += 1;
                            continue;
                        }
                        if let Some(durable) = self.durable.as_mut() {
                            let record = Message::decode(&packet.message.bytes).unwrap();
                            if record.msg_type == hyper_raft::proto::MessageType::MsgAppendResponse
                                && !record.held
                                && !record.reject
                            {
                                durable.push(packet);
                                continue;
                            }
                        }
                        self.admit(packet);
                        admitted += 1;
                    }
                }
                if !drained {
                    break;
                }
            }
            self.reap();
            admitted + released
        }
        fn begin_captures(&mut self, leader: usize, want: usize) {
            assert!(matches!(self.phase, PumpPhase::Relay));
            assert!((1..=2).contains(&want));
            self.phase = PumpPhase::Captures {
                leader,
                held: [None, None],
                want,
            };
        }
        fn captures_ready(&self) -> bool {
            matches!(&self.phase, PumpPhase::Captures { held, want, .. } if held.iter().flatten().count() == *want)
        }
        fn release_captures(&mut self) {
            assert!(self.captures_ready());
            let PumpPhase::Captures { held, .. } =
                std::mem::replace(&mut self.phase, PumpPhase::Relay)
            else {
                panic!("capture phase disappeared")
            };
            for packet in held.into_iter().flatten() {
                self.admit(packet);
            }
            self.reap();
        }
        /// Keeps every durable answer to an append back from now on, letting held answers pass.
        /// Member `to`'s owner ended: every record to it is lost from now on, and those admitted
        /// to it are given up, as their answers never come.
        fn ended(&mut self, to: u64) {
            self.lost.push(to);
            for slot in &mut self.receipts {
                if slot.as_ref().is_some_and(|pending| pending.to == to) {
                    let pending = slot.take().unwrap();
                    self.records -= 1;
                    self.bytes -= pending.bytes;
                }
            }
        }
        fn hold_durable(&mut self) {
            assert!(self.durable.is_none());
            self.durable = Some(Vec::new());
        }
        /// Lets the durable answers kept back through, in order, and keeps none from now on.
        fn let_durable(&mut self) {
            for packet in self.durable.take().unwrap() {
                self.admit(packet);
            }
            self.reap();
        }
        fn finish(mut self) {
            assert!(
                matches!(self.phase, PumpPhase::Relay),
                "retained proposals were not released"
            );
            assert!(self.durable.is_none(), "durable answers were kept back");
            // No new records or grace deadline: try_result enforces each admitted until.
            while self.records != 0 {
                self.reap();
                if self.records != 0 {
                    std::thread::park_timeout(POLL);
                }
            }
            assert_eq!(self.bytes, 0);
            assert!(self.receipts.iter().all(Option::is_none));
        }
    }
    impl Drop for Pump<'_> {
        fn drop(&mut self) {
            // Never wait in Drop, including failure unwinding. Success requires explicit finish.
            if !std::thread::panicking() {
                assert_eq!(
                    self.records, 0,
                    "fixture pump dropped with unsettled records"
                );
                assert_eq!(self.bytes, 0);
                assert!(self.receipts.iter().all(Option::is_none));
                assert!(matches!(self.phase, PumpPhase::Relay));
            }
        }
    }

    /// Rounds a driven group may take for one call, election or settling before the test calls
    /// it hung. A round with nothing to relay yields `POLL` to the disk, so this is a counted hang
    /// guard of about a minute of disk waits (CLAUDE.md section 2), never a verdict on time.
    const ROUNDS: usize = 60_000;

    /// A test's own clocks and the owners it steps (replication.md, "The owner's time"). Each
    /// owner reads its own clock, as a machine reads its own, and only the test moves it: between
    /// steps, or from a call's work while the owner runs it. Its instants start an hour past the
    /// real clock, so a real-clock check of a test deadline (a receipt's poll) holds for an hour:
    /// a hang guard, never a verdict.
    struct Driven<F: BlockFile + 'static = DeviceFile> {
        // Each member's owner, by member order; none while a test has cut its power.
        owners: Vec<Option<(Actor<F>, mpsc::Receiver<Envelope>)>>,
        base: Instant,
        // Each owner's clock: the nanoseconds it reads past `base`.
        clocks: Vec<Arc<AtomicU64>>,
        // Clock moves made for a write slot's end (`Driven::lapse_slots`), each asserted to end it.
        lapses: u64,
    }
    /// The time a driven clock reads past its shell's base.
    fn offset(clock: &AtomicU64) -> Duration {
        Duration::from_nanos(clock.load(Ordering::Acquire))
    }
    /// Moves a driven clock to `to`, its shell's `base` plus some offset.
    fn set(clock: &AtomicU64, base: Instant, to: Instant) {
        let nanos = u64::try_from(to.duration_since(base).as_nanos()).unwrap();
        clock.store(nanos, Ordering::Release);
    }
    /// The lease on a member's state file: the previous owner of the path lets go of it as its
    /// owner drops, which a test waits for, its rounds counted.
    fn lease(path: &Path) -> Arc<File> {
        for _ in 0..ROUNDS {
            match exclusive_file(path) {
                Ok(file) => return Arc::new(file),
                Err(_) => std::thread::park_timeout(POLL),
            }
        }
        panic!("the previous owner kept its lease");
    }
    impl Driven {
        /// A shell whose members run on the system's files.
        fn new() -> Self {
            Self::empty()
        }
        /// A member on the system's files: its state and its log under `dir`.
        fn open(
            &mut self,
            dir: &Path,
            node_id: u64,
            voters: Vec<u64>,
            bootstrap: Option<api::replication::Bootstrap>,
        ) -> Client {
            self.open_member(dir, node_id, voters, Vec::new(), bootstrap)
        }
        /// [`Driven::open`], with `learners` beside the voters.
        fn open_member(
            &mut self,
            dir: &Path,
            node_id: u64,
            voters: Vec<u64>,
            learners: Vec<u64>,
            bootstrap: Option<api::replication::Bootstrap>,
        ) -> Client {
            self.member(
                dir,
                node_id,
                voters,
                learners,
                bootstrap,
                Durability::Persisted,
            )
        }
        /// [`Driven::open`], its writes acknowledged as `durability` says.
        fn open_with(
            &mut self,
            dir: &Path,
            node_id: u64,
            voters: Vec<u64>,
            bootstrap: Option<api::replication::Bootstrap>,
            durability: Durability,
        ) -> Client {
            self.member(dir, node_id, voters, Vec::new(), bootstrap, durability)
        }
        fn member(
            &mut self,
            dir: &Path,
            node_id: u64,
            voters: Vec<u64>,
            learners: Vec<u64>,
            bootstrap: Option<api::replication::Bootstrap>,
            durability: Durability,
        ) -> Client {
            let path = dir.join(format!("state-{node_id}.db"));
            let lease = Owned::unbudgeted(lease(&path), None);
            let (reading, clock) = self.reading();
            let (client, actor, receiver) = super::open(
                &path,
                &dir.join(format!("raft-{node_id}.log")),
                ReplicaConfig {
                    node_id,
                    voters,
                    learners,
                    bootstrap,
                },
                durability,
                Arc::new(Changed::default()),
                lease,
                self.until(),
                clock,
            )
            .unwrap();
            self.owners.push(Some((actor, receiver)));
            self.clocks.push(reading);
            client
        }
        /// Member `owner`'s crash: its owner dropped mid-run, and opened again on its own files,
        /// its clock running on, as a restarted process is. `voters` and `learners` are only what
        /// a fresh member would start with; a member with state keeps its own. Returns the
        /// reopened member's client.
        fn restart(
            &mut self,
            dir: &Path,
            owner: usize,
            voters: Vec<u64>,
            learners: Vec<u64>,
        ) -> Client {
            drop(self.owners.get_mut(owner).unwrap().take());
            let node_id = u64::try_from(owner + 1).unwrap();
            let path = dir.join(format!("state-{node_id}.db"));
            let lease = Owned::unbudgeted(lease(&path), None);
            let (base, read) = (self.base, Arc::clone(self.clocks.get(owner).unwrap()));
            let (client, actor, receiver) = super::open(
                &path,
                &dir.join(format!("raft-{node_id}.log")),
                ReplicaConfig {
                    node_id,
                    voters,
                    learners,
                    bootstrap: None,
                },
                Durability::Persisted,
                Arc::new(Changed::default()),
                lease,
                self.until(),
                Arc::new(move || base + offset(&read)),
            )
            .unwrap();
            *self.owners.get_mut(owner).unwrap() = Some((actor, receiver));
            client
        }
    }
    impl<F: BlockFile + 'static> Driven<F> {
        fn empty() -> Self {
            Self {
                owners: Vec::new(),
                base: Instant::now() + Duration::from_secs(3600),
                clocks: Vec::new(),
                lapses: 0,
            }
        }
        /// A call's or a record's budget on these clocks: it runs out only if the test moves a
        /// clock past it.
        fn until(&self) -> Instant {
            self.base
                + self
                    .clocks
                    .iter()
                    .map(|clock| offset(clock))
                    .max()
                    .unwrap_or_default()
                + Duration::from_secs(5)
        }
        fn advance(&mut self, owner: usize, by: Duration) {
            let clock = self.clocks.get(owner).unwrap();
            set(clock, self.base, self.base + offset(clock) + by);
        }
        /// Owner `owner`'s tick period: the pace it derived from what it measured (`pace`), read,
        /// never assumed; a stopped owner's is the start pace it reopens with.
        fn period(&self, owner: usize) -> Duration {
            self.owners
                .get(owner)
                .and_then(Option::as_ref)
                .map_or(TICK, |(actor, _)| actor.pace.period())
        }
        /// Owner `owner`'s clock moves one tick of its own pace.
        fn advance_tick(&mut self, owner: usize) {
            let period = self.period(owner);
            self.advance(owner, period);
        }
        /// Owner `owner`'s clock, for a call's work to move while the owner runs it.
        fn clock(&self, owner: usize) -> Arc<AtomicU64> {
            Arc::clone(self.clocks.get(owner).unwrap())
        }
        /// Owner `owner`, which must be running.
        fn owner(&self, owner: usize) -> &Actor<F> {
            &self.owners.get(owner).unwrap().as_ref().unwrap().0
        }
        fn owner_mut(&mut self, owner: usize) -> &mut Actor<F> {
            &mut self.owners.get_mut(owner).unwrap().as_mut().unwrap().0
        }
        /// Owner `owner`'s flushes of its database's log wait for the test (`Driven::flush`).
        fn hold_flushes(&mut self, owner: usize) {
            self.owner_mut(owner).flushing = Flushing::Held;
        }
        /// Flushes owner `owner`'s database log now, as its flushing thread would: what it has
        /// applied is durable after.
        fn flush(&mut self, owner: usize) {
            let actor = self.owner_mut(owner);
            assert!(
                matches!(actor.flushing, Flushing::Held),
                "owner {owner}'s flushes aren't held"
            );
            if let Some(at) = actor.replica.machine().unflushed() {
                actor.replica.machine().log_flusher()().unwrap();
                actor.replica.machine_mut().flushed_through(at);
            }
        }
        /// Rounds until owner `owner`'s flushing thread has made all it applied durable: a
        /// compaction moves the raft log's start no further than that (I8).
        fn flushed(&mut self, pump: &mut Pump<'_>, owner: usize) {
            for _ in 0..ROUNDS {
                if self.owner(owner).replica.machine().unflushed().is_none() {
                    return;
                }
                self.round(pump);
            }
            panic!("owner {owner} did not flush in {ROUNDS} driven rounds");
        }
        /// A new member's clock: its reading, which only the test moves, and the clock its owner
        /// reads it through.
        fn reading(&self) -> (Arc<AtomicU64>, Clock) {
            let reading = Arc::new(AtomicU64::new(0));
            let (base, read) = (self.base, Arc::clone(&reading));
            (reading, Arc::new(move || base + offset(&read)))
        }
        /// One round: every owner takes each input waiting for it, then steps once more, each at
        /// its own clock's reading.
        fn step(&mut self) {
            for (owner, inputs) in self.owners.iter_mut().flatten() {
                while let Ok(input) = inputs.try_recv() {
                    owner.run_step(Some(input)).unwrap();
                }
                owner.run_step(None).unwrap();
            }
        }
        /// One relay round and one step; a round with nothing to relay yields `POLL` to the disk.
        fn round(&mut self, pump: &mut Pump<'_>) {
            if pump.drive_until(self.until()) == 0 {
                std::thread::park_timeout(POLL);
            }
            self.step();
        }
        /// Owner `first` campaigns now, every clock standing still, and rounds go on until every
        /// owner follows one leader with at least `applied` applied. Returns the leader's index.
        fn elect(
            &mut self,
            pump: &mut Pump<'_>,
            clients: &[Client],
            first: usize,
            applied: u64,
        ) -> usize {
            self.owner_mut(first).replica.campaign().unwrap();
            self.follow(pump, clients, applied)
        }
        /// Rounds, time standing still, until every owner follows one leader with at least
        /// `applied` applied, and each of them past its seed records a class its leader has
        /// nothing to add to (a class entry moves the point, so a write prepared beside it would
        /// be refused).
        /// Returns the leader's index.
        fn follow(&mut self, pump: &mut Pump<'_>, clients: &[Client], applied: u64) -> usize {
            let mut last = Vec::new();
            for _ in 0..ROUNDS {
                self.round(pump);
                let states: Vec<_> = clients
                    .iter()
                    .map(|c| c.handle().status().unwrap())
                    .collect();
                let leader = states.first().unwrap().leader;
                let named: Vec<u64> = states.iter().map(|s| s.node_id).collect();
                let settled = self.owners.iter().flatten().all(|(owner, _)| {
                    let machine = owner.replica.machine();
                    !named.contains(&owner.replica.id())
                        || owner.class_held
                        || machine.bootstrap_pending()
                        || (machine.class().is_some()
                            && (!owner.replica.is_leader() || machine.class_proposal().is_none()))
                });
                if leader != 0
                    && settled
                    && states
                        .iter()
                        .all(|s| s.leader == leader && s.applied >= applied)
                {
                    return usize::try_from(leader - 1).unwrap();
                }
                last = states;
            }
            panic!("no leader followed in {ROUNDS} driven rounds: {last:?}");
        }
        /// The lead moves from owner `from` to owner `to` now (`Replica::transfer`, which no lease
        /// refuses), every clock standing still; rounds go on until every owner follows `to`.
        fn transfer(&mut self, pump: &mut Pump<'_>, clients: &[Client], from: usize, to: usize) {
            let id = u64::try_from(to + 1).unwrap();
            self.owner_mut(from).replica.transfer(id).unwrap();
            for _ in 0..ROUNDS {
                self.round(pump);
                if clients
                    .iter()
                    .all(|c| c.handle().status().unwrap().leader == id)
                {
                    return;
                }
            }
            panic!("the lead did not move to {id} in {ROUNDS} driven rounds");
        }
        /// Owner `from` hands its lead to owner `to`, every clock standing still; rounds go on until
        /// every one of `clients` follows one leader, whose index it returns. The handing-off can
        /// lose its election: a change's way out, written after `to` was told to campaign, is an
        /// entry `to` lacks and the voters holding it refuse it their votes. An election timeout
        /// settles that on a running owner; here, with nobody leading and nothing moving, each
        /// voter in turn campaigns.
        fn hand_off(
            &mut self,
            pump: &mut Pump<'_>,
            clients: &[Client],
            from: usize,
            to: usize,
        ) -> usize {
            let id = u64::try_from(to + 1).unwrap();
            let term = self.owner(from).replica.term();
            self.owner_mut(from).replica.transfer(id).unwrap();
            let (mut still, mut voter) = (0, 0);
            for _ in 0..ROUNDS {
                let relayed = pump.drive_until(self.until());
                if relayed == 0 {
                    std::thread::park_timeout(POLL);
                }
                self.step();
                let states: Vec<_> = clients
                    .iter()
                    .map(|c| c.handle().status().unwrap())
                    .collect();
                let leader = states[0].leader;
                if leader != 0 && states.iter().all(|s| s.leader == leader && s.term > term) {
                    return usize::try_from(leader - 1).unwrap();
                }
                still = if relayed == 0 { still + 1 } else { 0 };
                if still >= 64 && states.iter().all(|s| s.leader == 0) {
                    still = 0;
                    // One that can't lead refuses, and the next is asked.
                    let _ = self.owner_mut(voter % clients.len()).replica.campaign();
                    voter += 1;
                }
            }
            panic!("no lead after the hand-off in {ROUNDS} driven rounds");
        }
        /// Rounds, time standing still, until `answer` comes.
        fn answer<T>(
            &mut self,
            pump: &mut Pump<'_>,
            answer: &mpsc::Receiver<Result<T, Refusal>>,
        ) -> Result<T, Refusal> {
            for _ in 0..ROUNDS {
                match answer.try_recv() {
                    Ok(result) => return result,
                    Err(mpsc::TryRecvError::Empty) => {
                        self.round(pump);
                        self.lapse_slots(pump);
                    }
                    Err(error) => panic!("owner ended: {error}"),
                }
            }
            panic!("a driven operation was unanswered after {ROUNDS} rounds");
        }
        /// Time moves for a write slot alone. Once the group is quiet (nothing relayed or held in
        /// transit, no write out on any disk, and no member but a slot's leader with work in
        /// hand), the only thing that happens next in time is that slot's end by its length: its
        /// holder's work forwarded nothing (a read, or a write refused before it proposed). So the
        /// leader's clock moves just past the slot's end, the one wait the design states for it
        /// (replication.md, "the write slot"); no other clock moves.
        fn lapse_slots(&mut self, pump: &Pump<'_>) {
            if pump.records != 0
                || pump
                    .delay
                    .as_ref()
                    .is_some_and(|delay| !delay.held.is_empty())
            {
                return;
            }
            let quiet = self.owners.iter().flatten().all(|(owner, _)| {
                owner.replica.in_flight() == 0 && (owner.slots.holds() || owner.pending.is_none())
            });
            if !quiet {
                return;
            }
            let held: Vec<(usize, Instant)> = self
                .owners
                .iter()
                .enumerate()
                .filter_map(|(index, owner)| {
                    owner
                        .as_ref()
                        .and_then(|(owner, _)| owner.slots.until())
                        .map(|until| (index, until))
                })
                .collect();
            for (index, until) in held {
                let clock = self.clocks.get(index).unwrap();
                if self.base + offset(clock) <= until {
                    set(clock, self.base, until + Duration::from_nanos(1));
                }
                self.step();
                assert_ne!(
                    self.owner(index).slots.until(),
                    Some(until),
                    "owner {index}'s clock moved past its slot's end and the slot held on"
                );
                self.lapses += 1;
            }
        }
        /// Rounds, time standing still, until the call's `answer` comes.
        fn settle(
            &mut self,
            pump: &mut Pump<'_>,
            answer: &mpsc::Receiver<Result<Answer, Refusal>>,
        ) -> Result<Vec<u8>, Refusal> {
            self.answer(pump, answer).map(called)
        }
        fn call(
            &mut self,
            pump: &mut Pump<'_>,
            client: &Client,
            f: impl FnOnce(&mut Db) -> anyhow::Result<Vec<u8>> + Send + 'static,
        ) -> Result<Vec<u8>, Refusal> {
            let answer = submit(client, self.until(), f);
            self.settle(pump, &answer)
        }
        /// Rounds, time standing still, until the group is quiet: no write out on any owner's
        /// disk, every admitted record taken, and a round with nothing to relay. A held owner's
        /// records wait in its own queue.
        fn quiet(&mut self, pump: &mut Pump<'_>) {
            for _ in 0..ROUNDS {
                let admitted = pump.drive_until(self.until());
                if admitted == 0
                    && pump.records == 0
                    && self
                        .owners
                        .iter()
                        .flatten()
                        .all(|(owner, _)| owner.replica.in_flight() == 0)
                {
                    return;
                }
                if admitted == 0 {
                    std::thread::park_timeout(POLL);
                }
                self.step();
            }
            panic!("the group was not quiet after {ROUNDS} driven rounds");
        }
        /// Owner `owner`'s clock moves one tick, every owner steps, and rounds go on until the
        /// group is quiet: what the tick sent is answered at the same count of its ticks, unless
        /// the answer is held.
        fn tick(&mut self, pump: &mut Pump<'_>, owner: usize) {
            self.advance_tick(owner);
            self.step();
            self.quiet(pump);
        }
        /// Rounds until every record the relay admitted has been taken, so the pump can finish.
        fn drain(&mut self, pump: &mut Pump<'_>) {
            for _ in 0..ROUNDS {
                pump.reap();
                if pump.records == 0 {
                    return;
                }
                self.step();
            }
            panic!("admitted records still untaken after {ROUNDS} rounds");
        }
    }
    /// A memory disk whose power a test cuts (the replicated class's power-cut tests): what a
    /// file's last sync made durable survives a cut, nothing written to it since. The disk restarted
    /// on what survived is another disk, so a dying owner's writes never reach it.
    struct PowerDisk {
        id: u64,
        files: Mutex<std::collections::HashMap<String, Arc<PowerFile>>>,
        // Every open asked of it, of a file it has or a new one.
        opened: AtomicUsize,
    }
    /// A file of a [`PowerDisk`]: what its syncs made durable, and what a reader sees.
    #[derive(Default)]
    struct PowerFile {
        held: Mutex<(Vec<u8>, Vec<u8>)>,
    }
    /// Gives each disk its own file identities: the engine keeps one database open per file.
    static POWER_DISKS: AtomicU64 = AtomicU64::new(0);
    impl PowerDisk {
        fn new(image: std::collections::HashMap<String, Vec<u8>>) -> Arc<Self> {
            let files = image
                .into_iter()
                .map(|(path, bytes)| {
                    let held = Mutex::new((bytes.clone(), bytes));
                    (path, Arc::new(PowerFile { held }))
                })
                .collect();
            Arc::new(Self {
                id: POWER_DISKS.fetch_add(1, Ordering::Relaxed),
                files: Mutex::new(files),
                opened: AtomicUsize::new(0),
            })
        }
        /// The power cut: what each file's syncs made durable.
        fn cut(&self) -> std::collections::HashMap<String, Vec<u8>> {
            let files = self.files.lock().unwrap();
            files
                .iter()
                .map(|(path, file)| (path.clone(), file.held.lock().unwrap().0.clone()))
                .collect()
        }
    }
    impl PowerFile {
        fn write(
            &self,
            pos: u64,
            data: &[u8],
            c: amphora_engine::Completion,
        ) -> amphora_engine::Result<amphora_engine::Completion> {
            let mut held = self.held.lock().unwrap();
            let pos = usize::try_from(pos).unwrap();
            let end = pos + data.len();
            if held.1.len() < end {
                held.1.resize(end, 0);
            }
            held.1[pos..end].copy_from_slice(data);
            drop(held);
            c.complete(i32::try_from(data.len()).unwrap());
            Ok(c)
        }
    }
    impl amphora_engine::File for PowerFile {
        fn lock_file(&self, _exclusive: bool) -> amphora_engine::Result<()> {
            Ok(())
        }
        fn unlock_file(&self) -> amphora_engine::Result<()> {
            Ok(())
        }
        fn pread(
            &self,
            pos: u64,
            c: amphora_engine::Completion,
        ) -> amphora_engine::Result<amphora_engine::Completion> {
            let n = {
                let held = self.held.lock().unwrap();
                let dst = c.as_read().buf().as_mut_slice();
                let src = held
                    .1
                    .get(usize::try_from(pos).unwrap()..)
                    .unwrap_or_default();
                let n = src.len().min(dst.len());
                dst[..n].copy_from_slice(&src[..n]);
                n
            };
            c.complete(i32::try_from(n).unwrap());
            Ok(c)
        }
        fn pwrite(
            &self,
            pos: u64,
            buffer: Arc<amphora_engine::Buffer>,
            c: amphora_engine::Completion,
        ) -> amphora_engine::Result<amphora_engine::Completion> {
            self.write(pos, buffer.as_slice(), c)
        }
        fn pwritev(
            &self,
            pos: u64,
            buffers: Vec<Arc<amphora_engine::Buffer>>,
            c: amphora_engine::Completion,
        ) -> amphora_engine::Result<amphora_engine::Completion> {
            let data: Vec<u8> = buffers
                .iter()
                .flat_map(|b| b.as_slice().iter().copied())
                .collect();
            self.write(pos, &data, c)
        }
        fn sync(
            &self,
            c: amphora_engine::Completion,
            _sync_type: amphora_engine::io::FileSyncType,
        ) -> amphora_engine::Result<amphora_engine::Completion> {
            let mut held = self.held.lock().unwrap();
            held.0 = held.1.clone();
            drop(held);
            c.complete(0);
            Ok(c)
        }
        fn truncate(
            &self,
            len: u64,
            c: amphora_engine::Completion,
        ) -> amphora_engine::Result<amphora_engine::Completion> {
            self.held
                .lock()
                .unwrap()
                .1
                .resize(usize::try_from(len).unwrap(), 0);
            c.complete(0);
            Ok(c)
        }
        fn size(&self) -> amphora_engine::Result<u64> {
            Ok(self.held.lock().unwrap().1.len() as u64)
        }
    }
    impl amphora_engine::Clock for PowerDisk {
        fn current_time_monotonic(&self) -> amphora_engine::MonotonicInstant {
            amphora_engine::MonotonicInstant::now()
        }
        fn current_time_wall_clock(&self) -> amphora_engine::WallClockInstant {
            amphora_engine::WallClockInstant::now()
        }
    }
    impl amphora_engine::IO for PowerDisk {
        fn open_file(
            &self,
            path: &str,
            flags: amphora_engine::OpenFlags,
            _direct: bool,
        ) -> amphora_engine::Result<Arc<dyn amphora_engine::File>> {
            self.opened.fetch_add(1, Ordering::Relaxed);
            let mut files = self.files.lock().unwrap();
            if let Some(file) = files.get(path) {
                return Ok(file.clone());
            }
            if !flags.contains(amphora_engine::OpenFlags::Create) {
                return Err(amphora_engine::LimboError::CompletionError(
                    amphora_engine::CompletionError::IOError(std::io::ErrorKind::NotFound, "open"),
                ));
            }
            let file = Arc::new(PowerFile::default());
            files.insert(path.to_owned(), file.clone());
            Ok(file)
        }
        fn remove_file(&self, path: &str) -> amphora_engine::Result<()> {
            self.files.lock().unwrap().remove(path);
            Ok(())
        }
        fn file_id(&self, path: &str) -> amphora_engine::Result<amphora_engine::io::FileId> {
            Ok(amphora_engine::io::FileId::from_path_hash(&format!(
                "power-{}:{path}",
                self.id
            )))
        }
    }

    /// A member's storage whose power a test cuts: its log on hyper-block's simulated device
    /// (power-loss semantics, `Crash`), its database on a [`PowerDisk`].
    struct Power {
        log: hyper_block::sim::SimDevice,
        disk: Arc<PowerDisk>,
    }
    impl Power {
        fn new(seed: u64) -> Self {
            let align = hyper_block::buf::Alignment::new(4096).unwrap();
            let sector = hyper_block::buf::Alignment::new(512).unwrap();
            Self {
                log: hyper_block::sim::SimDevice::new(align, sector, seed, 2).unwrap(),
                disk: PowerDisk::new(std::collections::HashMap::new()),
            }
        }
        /// The power cut: the log's device refuses every write and flush from now on, so the dying
        /// owner's drain reaches nothing, and loses what it held unflushed; the database keeps what
        /// its files' syncs made durable, which this returns.
        fn cut(&mut self) -> std::collections::HashMap<String, Vec<u8>> {
            self.log
                .inject(hyper_block::sim::Fault::PowerCut { ops: 0 })
                .unwrap();
            self.log.crash(hyper_block::sim::Crash::LoseAll).unwrap();
            self.disk.cut()
        }
        /// The power back, once the dying owner is gone: the device takes writes again, and the
        /// database is a disk of what `survived`.
        fn restore(&mut self, survived: std::collections::HashMap<String, Vec<u8>>) {
            self.log.crash(hyper_block::sim::Crash::LoseAll).unwrap();
            self.log.clear_faults().unwrap();
            self.disk = PowerDisk::new(survived);
        }
    }

    impl Driven<hyper_block::sim::SimHandle> {
        /// A shell whose members run on storage the test cuts the power of.
        fn powered() -> Self {
            Self::empty()
        }
        /// Member `node_id` on `power`: its database on the power disk, its log on the device; its
        /// leases on files under `dir`. Its clock is `reading`, a new one when none is given.
        fn open_power(
            &mut self,
            dir: &Path,
            node_id: u64,
            voters: Vec<u64>,
            durability: Durability,
            power: &Power,
        ) -> Client {
            let reading = Arc::new(AtomicU64::new(0));
            let (client, owner) = self.power_owner(
                dir,
                node_id,
                voters,
                durability,
                power,
                Arc::clone(&reading),
            );
            self.owners.push(Some(owner));
            self.clocks.push(reading);
            client
        }
        fn power_owner(
            &self,
            dir: &Path,
            node_id: u64,
            voters: Vec<u64>,
            durability: Durability,
            power: &Power,
            reading: Arc<AtomicU64>,
        ) -> (
            Client,
            (Actor<hyper_block::sim::SimHandle>, mpsc::Receiver<Envelope>),
        ) {
            let log = power.log.handle().unwrap();
            self.owner_on(dir, node_id, voters, durability, power, log, reading)
        }
    }

    impl<F: BlockFile + 'static> Driven<F> {
        /// Member `node_id`: its database on `power`'s disk, its log on `log`, its leases on files
        /// under `dir`, its clock `reading`.
        #[allow(clippy::too_many_arguments)]
        fn owner_on(
            &self,
            dir: &Path,
            node_id: u64,
            voters: Vec<u64>,
            durability: Durability,
            power: &Power,
            log: F,
            reading: Arc<AtomicU64>,
        ) -> (Client, (Actor<F>, mpsc::Receiver<Envelope>)) {
            let machine_lease =
                Owned::unbudgeted(lease(&dir.join(format!("state-{node_id}.lease"))), None);
            let log_owner =
                Arc::try_unwrap(lease(&dir.join(format!("raft-{node_id}.lease")))).unwrap();
            let replicated = matches!(durability, Durability::Replicated(_));
            let mut machine = crate::replication::Machine::open_on(
                Path::new(&format!("/power/state-{node_id}.db")),
                Arc::clone(&power.disk) as Arc<dyn amphora_engine::IO>,
                ConfState {
                    voters: voters.clone(),
                    ..ConfState::default()
                },
                machine_lease.clone(),
                node_id,
                replicated,
            )
            .unwrap();
            // The simulated disk is no filesystem: one of the test's, all of it free.
            machine.stand_in_filesystem(0, u64::MAX);
            let base = self.base;
            let read = Arc::clone(&reading);
            let (client, actor, receiver) = super::open_on(
                machine,
                log_owner,
                log,
                ReplicaConfig {
                    node_id,
                    voters,
                    learners: Vec::new(),
                    bootstrap: None,
                },
                durability,
                Arc::new(Changed::default()),
                machine_lease,
                self.until(),
                Arc::new(move || base + offset(&read)),
            )
            .unwrap();
            (client, (actor, receiver))
        }
    }

    impl Driven<hyper_block::sim::SimHandle> {
        /// Member `owner`'s power cut, and the member restarted on what survived: its log loses
        /// what it hadn't flushed, its database what it hadn't synced. The other members run on.
        /// Its clock runs on too. Returns the restarted member's client.
        fn power_cut(
            &mut self,
            dir: &Path,
            owner: usize,
            voters: Vec<u64>,
            durability: Durability,
            power: &mut Power,
        ) -> Client {
            let survived = power.cut();
            drop(self.owners.get_mut(owner).unwrap().take());
            power.restore(survived);
            let node_id = u64::try_from(owner + 1).unwrap();
            let reading = Arc::clone(self.clocks.get(owner).unwrap());
            let (client, restarted) =
                self.power_owner(dir, node_id, voters, durability, power, reading);
            *self.owners.get_mut(owner).unwrap() = Some(restarted);
            client
        }
    }

    /// Holds the flushes of a member's log device while the test says so: its writes go on, and
    /// none becomes durable until the test lets them go. The log's own threads wait here, never
    /// the test's.
    #[derive(Default)]
    struct Gate {
        held: Mutex<bool>,
        moved: std::sync::Condvar,
    }
    impl Gate {
        fn hold(&self) {
            *self.held.lock().unwrap() = true;
        }
        fn release(&self) {
            *self.held.lock().unwrap() = false;
            self.moved.notify_all();
        }
        /// Waits while the flushes are held, up to the transport tests' hang guard: a hang,
        /// never a verdict.
        fn pass(&self) -> Result<(), hyper_block::DiskError> {
            let until = Instant::now() + crate::transport::HANG;
            let mut held = self.held.lock().unwrap();
            while *held {
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(hyper_block::DiskError::Io {
                        op: "a held flush",
                        path: std::path::PathBuf::new(),
                        source: std::io::Error::from(std::io::ErrorKind::TimedOut),
                    });
                }
                held = self.moved.wait_timeout(held, left).unwrap().0;
            }
            Ok(())
        }
    }
    /// A member's log on a simulated device whose flushes, and writes durable on their own, wait
    /// at its gate.
    struct Gated {
        log: hyper_block::sim::SimHandle,
        gate: Arc<Gate>,
    }
    impl BlockFile for Gated {
        fn alignment(&self) -> hyper_block::buf::Alignment {
            self.log.alignment()
        }
        fn layout_block(&self) -> hyper_block::buf::Alignment {
            self.log.layout_block()
        }
        fn len(&self) -> Result<u64, hyper_block::DiskError> {
            self.log.len()
        }
        fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<(), hyper_block::DiskError> {
            self.log.read_exact_at(buf, offset)
        }
        fn read_resident_at(
            &mut self,
            buf: &mut [u8],
            offset: u64,
        ) -> Result<bool, hyper_block::DiskError> {
            self.log.read_resident_at(buf, offset)
        }
        fn reads_resident(&self) -> bool {
            self.log.reads_resident()
        }
        fn residency(&self) -> hyper_block::Residency {
            self.log.residency()
        }
        fn set_residency_shadow(&mut self, on: bool) {
            self.log.set_residency_shadow(on);
        }
        fn write_all_at(&self, buf: &[u8], offset: u64) -> Result<(), hyper_block::DiskError> {
            self.log.write_all_at(buf, offset)
        }
        fn sync_data(&self) -> Result<(), hyper_block::DiskError> {
            self.gate.pass()?;
            self.log.sync_data()
        }
        fn fills_new_space(&self) -> bool {
            self.log.fills_new_space()
        }
        fn grows_by_segments(&self) -> bool {
            self.log.grows_by_segments()
        }
        fn grow(&self, len: u64) -> Result<Option<u64>, hyper_block::DiskError> {
            self.log.grow(len)
        }
        fn write_whole(
            &self,
            offset: u64,
            copies: u64,
            fill: &[u8],
        ) -> Result<(), hyper_block::DiskError> {
            self.log.write_whole(offset, copies, fill)
        }
        fn segment_end(&self, offset: u64) -> Result<Option<u64>, hyper_block::DiskError> {
            self.log.segment_end(offset)
        }
        fn write_durable_at(
            &self,
            buf: &[u8],
            offset: u64,
        ) -> Result<hyper_block::block::Durable, hyper_block::DiskError> {
            self.gate.pass()?;
            self.log.write_durable_at(buf, offset)
        }
        fn try_clone(&self) -> Result<Self, hyper_block::DiskError> {
            Ok(Self {
                log: self.log.try_clone()?,
                gate: Arc::clone(&self.gate),
            })
        }
    }
    impl Driven<Gated> {
        /// Member `node_id`: its database on `power`'s disk, its log on `power`'s device behind
        /// `gate`.
        fn open_gated(
            &mut self,
            dir: &Path,
            node_id: u64,
            voters: Vec<u64>,
            durability: Durability,
            power: &Power,
            gate: &Arc<Gate>,
        ) -> Client {
            let reading = Arc::new(AtomicU64::new(0));
            let log = Gated {
                log: power.log.handle().unwrap(),
                gate: Arc::clone(gate),
            };
            let (client, owner) = self.owner_on(
                dir,
                node_id,
                voters,
                durability,
                power,
                log,
                Arc::clone(&reading),
            );
            self.owners.push(Some(owner));
            self.clocks.push(reading);
            client
        }
    }

    impl Driven<Gated> {
        /// Member `owner`'s power cut while its log's flushes are held, and the member restarted on
        /// what survived with its gate open: the held flushes meet the cut device and make nothing
        /// durable. Returns the restarted member's client.
        fn gated_cut(
            &mut self,
            dir: &Path,
            owner: usize,
            power: &mut Power,
            gate: &Arc<Gate>,
        ) -> Client {
            let survived = power.cut();
            gate.release();
            drop(self.owners.get_mut(owner).unwrap().take());
            power.restore(survived);
            let node_id = u64::try_from(owner + 1).unwrap();
            let reading = Arc::clone(self.clocks.get(owner).unwrap());
            let log = Gated {
                log: power.log.handle().unwrap(),
                gate: Arc::clone(gate),
            };
            let (client, restarted) =
                self.owner_on(dir, node_id, vec![1, 2, 3], fated(), power, log, reading);
            *self.owners.get_mut(owner).unwrap() = Some(restarted);
            client
        }
    }

    /// Three members of three fates, two to span.
    fn fated() -> Durability {
        Durability::Replicated(api::replication::Fates {
            of: vec![(1, 10), (2, 20), (3, 30)],
            spanning: 2,
        })
    }

    /// Three voters of three fates, two to span, each on its own power and its log behind its own
    /// gate.
    fn gated(dir: &Path) -> (Driven<Gated>, Vec<Power>, Vec<Arc<Gate>>, Vec<Client>) {
        let mut driven = Driven::<Gated>::empty();
        let powers: Vec<Power> = (1..=3).map(Power::new).collect();
        let gates: Vec<Arc<Gate>> = (0..3).map(|_| Arc::new(Gate::default())).collect();
        let clients = (0..3)
            .map(|i| {
                driven.open_gated(
                    dir,
                    u64::try_from(i + 1).unwrap(),
                    vec![1, 2, 3],
                    fated(),
                    powers.get(i).unwrap(),
                    gates.get(i).unwrap(),
                )
            })
            .collect();
        (driven, powers, gates, clients)
    }

    /// The values of warm0, read through `client`.
    fn warm0(driven: &mut Driven<Gated>, pump: &mut Pump<'_>, client: &Client) -> String {
        let rows = driven
            .call(pump, client, |db| {
                let mut rows = Vec::new();
                db.query("SELECT value FROM warm0", [], |row| {
                    rows.push(row.get_value(0).to_string());
                    Ok(true)
                })?;
                Ok(rows.join(",").into_bytes())
            })
            .unwrap();
        String::from_utf8(rows).unwrap()
    }

    /// A leader elected and written at until each follower states a durable promise; then the
    /// value 1 written into warm0 at a follower, whose log's flushes are held once the write is in
    /// the leader's log, and rounds until the follower answers it on the group's commit, before it
    /// applies it. Returns the leader, the follower and the write's index.
    fn answered_at_a_follower(
        driven: &mut Driven<Gated>,
        pump: &mut Pump<'_>,
        clients: &[Client],
        gates: &[Arc<Gate>],
    ) -> (usize, usize, u64) {
        let leader = driven.elect(pump, clients, 0, 1);
        for n in 0..4 {
            let answer = driven.call(pump, clients.get(leader).unwrap(), move |db| {
                db.exec(&format!("CREATE TABLE warm{n}(value)"))?;
                Ok(Vec::new())
            });
            assert_eq!(answer, Ok(Vec::new()));
        }
        let applied = clients
            .get(leader)
            .unwrap()
            .handle()
            .status()
            .unwrap()
            .applied;
        driven.follow(pump, clients, applied);
        let follower = (0..3).find(|i| *i != leader).unwrap();
        let id = u64::try_from(follower + 1).unwrap();
        let write = submit(clients.get(follower).unwrap(), driven.until(), |db| {
            db.exec("INSERT INTO warm0 VALUES (1)")?;
            Ok(vec![1])
        });
        let index = (0..ROUNDS)
            .find_map(|_| {
                driven.round(pump);
                let log = driven.owner(leader).replica.core().raft.log();
                log.entries(applied + 1, u64::MAX, 16)
                    .unwrap()
                    .iter()
                    .find(|entry| entry.context.starts_with(&id.to_le_bytes()))
                    .map(|entry| entry.index)
            })
            .expect("the follower's write never reached its leader");
        gates.get(follower).unwrap().hold();
        assert_eq!(driven.settle(pump, &write), Ok(vec![1]));
        let at = driven.owner(follower).replica.applied().index;
        assert!(
            at < index,
            "the follower answered at its apply ({at}), not on the commit of {index}"
        );
        (leader, follower, index)
    }

    /// CP-T1.8 (AC-T1.8.5), a follower's answer on its group's commit (replication.md, "A write
    /// made at a follower is answered on the leader's commit"). Do: a write answered at a follower
    /// whose log's flushes are held; the lead handed to the third member. Expect: the follower in
    /// the new term, its write not applied and still pending, so its apply is still checked against
    /// its answer; once its flushes go, it applies with nothing pending and reads the write.
    #[test]
    fn a_follower_keeps_its_answered_write_across_a_term_change_until_it_applies() {
        let dir = tempfile::tempdir().unwrap();
        let (mut driven, _powers, gates, clients) = gated(dir.path());
        let mut pump = Pump::new(&clients);
        let (leader, follower, index) =
            answered_at_a_follower(&mut driven, &mut pump, &clients, &gates);
        let third = (0..3).find(|i| *i != leader && *i != follower).unwrap();
        let term = driven.owner(follower).replica.term();
        driven.transfer(&mut pump, &clients, leader, third);
        let owner = driven.owner(follower);
        assert!(owner.replica.term() > term, "no new term at the follower");
        assert!(owner.replica.applied().index < index, "applied while held");
        assert!(
            owner.pending.is_some(),
            "the answered write was dropped at the term change, before its apply"
        );
        gates.get(follower).unwrap().release();
        driven.follow(&mut pump, &clients, index);
        assert!(driven.owner(follower).pending.is_none());
        assert_eq!(
            warm0(&mut driven, &mut pump, clients.get(follower).unwrap()),
            "1"
        );
        driven.drain(&mut pump);
        pump.finish();
    }

    /// CP-T1.8 (AC-T1.8.5): a follower cut after it answered a write on its group's commit, and
    /// before its own log flushed it, loses nothing acknowledged. Do: a write answered at a follower
    /// whose log's flushes are held; its power cut, its log losing what it hadn't flushed, and the
    /// member restarted. Expect: every member, the restarted one among them, reads the write.
    #[test]
    fn a_follower_cut_after_answering_on_the_commit_loses_nothing_acknowledged() {
        let dir = tempfile::tempdir().unwrap();
        let (mut driven, mut powers, gates, mut clients) = gated(dir.path());
        let mut pump = Pump::new(&clients);
        let (leader, follower, index) =
            answered_at_a_follower(&mut driven, &mut pump, &clients, &gates);
        driven.drain(&mut pump);
        pump.finish();
        let restarted = driven.gated_cut(
            dir.path(),
            follower,
            powers.get_mut(follower).unwrap(),
            gates.get(follower).unwrap(),
        );
        *clients.get_mut(follower).unwrap() = restarted;
        let mut pump = Pump::new(&clients);
        // Driven clocks stand still, so the leader's heartbeats go only as its own clock moves:
        // one tick a round, until the restarted member follows it through the write.
        let id = u64::try_from(leader + 1).unwrap();
        let caught_up = (0..ROUNDS).any(|_| {
            driven.advance_tick(leader);
            driven.round(&mut pump);
            clients.iter().all(|client| {
                let status = client.handle().status().unwrap();
                status.leader == id && status.applied >= index
            })
        });
        assert!(
            caught_up,
            "the restarted member never followed through {index}"
        );
        for client in &clients {
            assert_eq!(warm0(&mut driven, &mut pump, client), "1");
        }
        driven.drain(&mut pump);
        pump.finish();
    }

    /// CP-T1.8 (AC-T1.8.5), the identity of a follower's answer with its apply (replication.md, "A
    /// write made at a follower is answered on the leader's commit"). Do: a write answered at a
    /// follower; an apply of it that refuses, given to the owner. Expect: the owner fences, naming
    /// the difference, rather than answer the write otherwise than it did.
    #[test]
    fn an_apply_that_differs_from_the_answer_on_the_commit_fences() {
        let dir = tempfile::tempdir().unwrap();
        let (mut driven, _powers, gates, clients) = gated(dir.path());
        let mut pump = Pump::new(&clients);
        let (_, follower, index) = answered_at_a_follower(&mut driven, &mut pump, &clients, &gates);
        let owner = driven.owner_mut(follower);
        let context = owner.pending.as_ref().unwrap().context.clone();
        let refused = owner.release(Output {
            answers: vec![crate::replication::Applied {
                index,
                context,
                result: Err(api::invalid("test", "the apply refuses")),
            }],
            ..Output::default()
        });
        assert!(
            matches!(&refused, Err(Refusal::Fenced(why)) if why.contains("differs")),
            "{refused:?}"
        );
        gates.get(follower).unwrap().release();
        driven.drain(&mut pump);
        pump.finish();
    }

    /// CP-T1.8 (AC-T1.8.5), the replicated class's answer for a write made at a follower
    /// (replication.md, "A write made at a follower is answered on the leader's commit"). Do: three
    /// voters of three fates, two to span; writes at the leader that every member applies; then a
    /// write at a follower and one at the leader, both prepared on one base (the follower's
    /// forward held until the leader's own is appended); the follower's log
    /// flushes held once its write is in the leader's log. Expect: the follower's answer while it
    /// hasn't applied its write; of the two writes one taken and the other refused as its apply
    /// refuses it; once the flushes go, the follower applies as it answered and reads the write
    /// that took.
    #[test]
    fn a_write_at_a_follower_is_answered_on_its_groups_commit() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::<Gated>::empty();
        let durability = Durability::Replicated(api::replication::Fates {
            of: vec![(1, 10), (2, 20), (3, 30)],
            spanning: 2,
        });
        let powers: Vec<Power> = (1..=3).map(Power::new).collect();
        let gates: Vec<Arc<Gate>> = (0..3).map(|_| Arc::new(Gate::default())).collect();
        let clients: Vec<Client> = (0..3)
            .map(|i| {
                driven.open_gated(
                    dir.path(),
                    u64::try_from(i + 1).unwrap(),
                    vec![1, 2, 3],
                    durability.clone(),
                    powers.get(i).unwrap(),
                    gates.get(i).unwrap(),
                )
            })
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        // Writes before the test's, so that each follower states a durable promise and answers
        // held at once (hyper-raft R-8).
        for n in 0..4 {
            let answer = driven.call(&mut pump, clients.get(leader).unwrap(), move |db| {
                db.exec(&format!("CREATE TABLE warm{n}(value)"))?;
                Ok(Vec::new())
            });
            assert_eq!(answer, Ok(Vec::new()));
        }
        let applied = clients
            .get(leader)
            .unwrap()
            .handle()
            .status()
            .unwrap()
            .applied;
        driven.follow(&mut pump, &clients, applied);

        let follower = (0..3).find(|i| *i != leader).unwrap();
        let id = u64::try_from(follower + 1).unwrap();
        let write = |made: u8| {
            move |db: &mut Db| {
                db.exec(&format!("INSERT INTO warm0 VALUES ({made})"))?;
                Ok(vec![made])
            }
        };
        // Both prepared on one base: the follower's forward is held until the leader has appended
        // its own, as each prepares on its log's tail.
        pump.begin_captures(leader, 1);
        let at_follower = submit(clients.get(follower).unwrap(), driven.until(), write(1));
        for _ in 0..ROUNDS {
            if pump.captures_ready() {
                break;
            }
            driven.round(&mut pump);
        }
        let last = |driven: &Driven<Gated>| {
            driven
                .owner(leader)
                .replica
                .core()
                .raft
                .log()
                .last_index()
                .unwrap()
        };
        let before = last(&driven);
        let at_leader = submit(clients.get(leader).unwrap(), driven.until(), write(2));
        for _ in 0..ROUNDS {
            if last(&driven) > before {
                break;
            }
            driven.round(&mut pump);
        }
        pump.release_captures();
        // Rounds until the follower's write is in the leader's log: its proposal has left it, and
        // the leader's append of it is still to be relayed.
        let index = (0..ROUNDS)
            .find_map(|_| {
                driven.round(&mut pump);
                let log = driven.owner(leader).replica.core().raft.log();
                log.entries(applied + 1, u64::MAX, 16)
                    .unwrap()
                    .iter()
                    .find(|entry| entry.context.starts_with(&id.to_le_bytes()))
                    .map(|entry| entry.index)
            })
            .expect("the follower's write never reached its leader");
        gates.get(follower).unwrap().hold();
        let answered = driven.settle(&mut pump, &at_follower);
        let at = driven.owner(follower).replica.applied().index;
        assert!(
            at < index,
            "the follower answered at its apply ({at}), not on the commit of {index}"
        );
        let other = driven.settle(&mut pump, &at_leader);
        let results = [answered, other];
        let taken: Vec<&Vec<u8>> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
        assert_eq!(taken.len(), 1, "{results:?}");
        assert!(
            results.iter().any(|result| matches!(
                result,
                Err(Refusal::Invalid { why, .. }) if why.contains("advanced since preparation")
            )),
            "{results:?}"
        );

        gates.get(follower).unwrap().release();
        driven.follow(&mut pump, &clients, index);
        let rows = driven.call(&mut pump, clients.get(follower).unwrap(), |db| {
            let mut rows = Vec::new();
            db.query("SELECT value FROM warm0", [], |row| {
                rows.push(row.get_value(0).to_string());
                Ok(true)
            })?;
            Ok(rows.join(",").into_bytes())
        });
        let made = taken.first().unwrap().first().unwrap().to_string();
        assert_eq!(rows, Ok(made.into_bytes()));
        driven.drain(&mut pump);
        pump.finish();
    }

    /// A seeded generator (xorshift64*): a seed's schedule is its own.
    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) % n.max(1)
        }
    }

    /// What one seed of the power-cut nemesis did.
    #[derive(Default)]
    struct Cuts {
        acknowledged: usize,
        cuts: usize,
        /// Cuts of a leader whose database held an acknowledged write unflushed.
        unflushed_leader: usize,
    }

    /// One seed: three voters of `durability`, their flushes held and let go at random; rounds of
    /// writes at the leader, each round ending in one member's power cut and restart, and a leader
    /// elected among the members that ran on when the cut one led. Every acknowledged write must
    /// be read back at the end.
    fn power_seed(seed: u64, durability: &Durability) -> Cuts {
        const ROUNDS_PER_SEED: u64 = 3;
        let dir = tempfile::tempdir().unwrap();
        let mut rng = Rng(seed | 1);
        let durability = durability.clone();
        let voters = vec![1, 2, 3];
        let mut power: Vec<Power> = (0..3u64).map(|n| Power::new(seed * 4 + n)).collect();
        let mut driven = Driven::powered();
        let mut clients: Vec<Client> = (1..=3u64)
            .map(|n| {
                let at = usize::try_from(n - 1).unwrap();
                driven.open_power(
                    dir.path(),
                    n,
                    voters.clone(),
                    durability.clone(),
                    power.get(at).unwrap(),
                )
            })
            .collect();
        for owner in 0..3 {
            driven.hold_flushes(owner);
        }
        let mut done = Cuts::default();
        let mut acknowledged = Vec::new();
        let mut value = 0u64;
        let mut pump = Pump::new(&clients);
        let mut leader = driven.elect(&mut pump, &clients, 0, 1);
        driven
            .call(&mut pump, clients.get(leader).unwrap(), |db| {
                db.exec("CREATE TABLE kept(value INTEGER PRIMARY KEY)")?;
                Ok(Vec::new())
            })
            .unwrap_or_else(|e| panic!("seed {seed}: the table: {e:?}"));
        for _ in 0..ROUNDS_PER_SEED {
            for _ in 0..=rng.below(4) {
                value += 1;
                let v = value;
                match driven.call(&mut pump, clients.get(leader).unwrap(), move |db| {
                    db.exec(&format!("INSERT INTO kept VALUES ({v})"))?;
                    Ok(Vec::new())
                }) {
                    Ok(_) => acknowledged.push(v),
                    Err(Refusal::UnknownOutcome | Refusal::Timeout) => {}
                    Err(error) => panic!("seed {seed}: write {v}: {error:?}"),
                }
                if rng.below(3) == 0 {
                    driven.flush(usize::try_from(rng.below(3)).unwrap());
                }
            }
            let victim = usize::try_from(rng.below(3)).unwrap();
            if victim == leader && driven.owner(leader).replica.machine().unflushed().is_some() {
                done.unflushed_leader += 1;
            }
            driven.drain(&mut pump);
            pump.finish();
            let restarted = driven.power_cut(
                dir.path(),
                victim,
                voters.clone(),
                durability.clone(),
                power.get_mut(victim).unwrap(),
            );
            *clients.get_mut(victim).unwrap() = restarted;
            driven.hold_flushes(victim);
            done.cuts += 1;
            pump = Pump::new(&clients);
            if victim == leader {
                leader = driven.elect(&mut pump, &clients, (victim + 1) % 3, 1);
            } else {
                // The leader ran on. A member restarted with all it had applied durable (the
                // persisted class's) is sent no entry, so only a heartbeat tells it who leads:
                // the leader's clock alone moves, so no follower's election timer does.
                let id = u64::try_from(leader + 1).unwrap();
                for _ in 0..ROUNDS {
                    if clients
                        .iter()
                        .all(|c| c.handle().status().unwrap().leader == id)
                    {
                        break;
                    }
                    driven.tick(&mut pump, leader);
                }
                driven.follow(&mut pump, &clients, 1);
            }
        }
        let read = driven
            .call(&mut pump, clients.get(leader).unwrap(), |db| {
                let mut values = Vec::new();
                db.query_owned("SELECT value FROM kept ORDER BY value", [], |row| {
                    values.push(row.get::<i64>(0)?.to_string());
                    Ok(true)
                })?;
                Ok(values.join(",").into_bytes())
            })
            .unwrap_or_else(|e| panic!("seed {seed}: the read: {e:?}"));
        driven.drain(&mut pump);
        pump.finish();
        let read = String::from_utf8(read).unwrap();
        let kept: std::collections::BTreeSet<&str> =
            read.split(',').filter(|v| !v.is_empty()).collect();
        for v in &acknowledged {
            assert!(
                kept.contains(v.to_string().as_str()),
                "seed {seed}: acknowledged write {v} lost; kept {read}; acknowledged {acknowledged:?}"
            );
        }
        done.acknowledged = acknowledged.len();
        done
    }

    /// Runs seeds `from..from + n` of the power-cut nemesis on `workers` threads, its members of
    /// `durability`. Every seed runs, and the run fails naming each failing seed with its account,
    /// whatever panicked in it.
    fn power_seeds(n: u64, from: u64, workers: usize, durability: &Durability) {
        let next = AtomicU64::new(from);
        let total = Mutex::new(Cuts::default());
        let failed = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let seed = next.fetch_add(1, Ordering::Relaxed);
                        if seed >= from + n {
                            return;
                        }
                        match std::panic::catch_unwind(|| power_seed(seed, durability)) {
                            Ok(done) => {
                                let mut total = total.lock().unwrap();
                                total.acknowledged += done.acknowledged;
                                total.cuts += done.cuts;
                                total.unflushed_leader += done.unflushed_leader;
                            }
                            Err(panic) => {
                                let account = panic
                                    .downcast_ref::<String>()
                                    .cloned()
                                    .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                                    .unwrap_or_default();
                                failed.lock().unwrap().push((seed, account));
                            }
                        }
                    }
                });
            }
        });
        let mut failed = failed.into_inner().unwrap();
        failed.sort();
        assert!(
            failed.is_empty(),
            "{} of the seeds failed: {failed:#?}",
            failed.len()
        );
        let total = total.into_inner().unwrap();
        println!(
            "power-cut nemesis: seeds {from}..{}, {} writes acknowledged, {} cuts, {} of them of a leader holding a write unflushed, none lost",
            from + n,
            total.acknowledged,
            total.cuts,
            total.unflushed_leader
        );
        // Non-vacuity: writes were acknowledged, members were cut, and some cut hit a leader that
        // held an acknowledged write unflushed.
        assert!(
            total.acknowledged > 0 && total.cuts > 0,
            "nothing was acknowledged or cut"
        );
        assert!(
            total.unflushed_leader > 0,
            "no cut hit a leader holding a write unflushed"
        );
    }

    /// The nemesis's power cut is a machine's (AC-T1.8.5): nothing a powered-off machine does
    /// reaches its disk. A dying owner's log drains what it took before its threads end, writes
    /// and flushes among it, through its own handle onto the same device. Do: through a member's
    /// log device, write and flush one block and write a second unflushed; cut the power; write
    /// and flush a third through the same handle, as that drain does; restore the power. Expect:
    /// the late write or its flush refused, and only the first block durable.
    #[test]
    fn nothing_a_member_writes_after_its_power_cut_survives_it() {
        let align = hyper_block::buf::Alignment::new(4096).unwrap();
        let block = |byte: u8| {
            let mut buf = hyper_block::buf::AlignedBuf::zeroed(4096, align).unwrap();
            buf.extend_from_slice(&[byte; 4096]).unwrap();
            buf
        };
        let mut power = Power::new(7);
        let dying = power.log.handle().unwrap();
        dying.write_all_at(block(b'a').as_slice(), 0).unwrap();
        dying.sync_data().unwrap();
        dying.write_all_at(block(b'b').as_slice(), 4096).unwrap();
        let survived = power.cut();
        let late = dying
            .write_all_at(block(b'c').as_slice(), 8192)
            .and_then(|()| dying.sync_data());
        drop(dying);
        power.restore(survived);
        let image = power.log.durable_image().unwrap();
        assert_eq!(image.get(..4096), Some(&[b'a'; 4096][..]));
        for (at, byte) in [(4096, b'b'), (8192, b'c')] {
            assert!(
                image.get(at..at + 4096) != Some(&[byte; 4096][..]),
                "the block written at {at} survived the cut"
            );
        }
        assert!(late.is_err(), "a write and flush after the cut succeeded");
    }

    /// CP-T1.8 (AC-T1.8.5), the replicated class under a nemesis (replication.md, "Fast
    /// acknowledgement": one member's crash or power cut loses nothing acknowledged while the
    /// other fates hold it). Do: 12 seeds of rounds of writes at the leader, the flushes held and
    /// let go at random, each round ending in one member's power cut, its log losing what it hadn't
    /// flushed and its database what it hadn't synced. Expect: every acknowledged write read back,
    /// with cuts of leaders holding writes unflushed among them.
    #[test]
    fn a_member_power_cut_loses_no_acknowledged_replicated_write() {
        power_seeds(12, 0, 4, &three_fates());
    }

    /// AC-T1.8.D2 (AC-T1.8.5), the persisted class under the same nemesis (replication.md, "A
    /// replica's database log": its database's log flushed off the owner, a restart applying again
    /// from the raft log what the database lost). Do: 12 seeds, three persisted voters, the
    /// database flushes held and let go at random, each round ending in one member's power cut.
    /// Expect: every acknowledged write read back, with cuts of leaders holding writes unflushed
    /// among them.
    #[test]
    fn a_member_power_cut_loses_no_acknowledged_persisted_write() {
        power_seeds(12, 0, 4, &Durability::Persisted);
    }

    /// The same nemesis over 10,000 seeds, the plan's measure, run by hand or nightly. A machine can
    /// take a share: `AMPHORA_POWER_SEEDS=from:count` (default 0:10000).
    #[test]
    #[ignore = "10,000 seeds: run with --ignored"]
    fn a_member_power_cut_loses_no_acknowledged_replicated_write_over_10000_seeds() {
        let share = std::env::var("AMPHORA_POWER_SEEDS").unwrap_or_else(|_| "0:10000".into());
        let (from, count) = share.split_once(':').unwrap();
        power_seeds(
            count.parse().unwrap(),
            from.parse().unwrap(),
            8,
            &three_fates(),
        );
    }

    /// Enqueues `operation` at `client` without waiting: its answer comes on the returned receiver.
    fn enqueue(
        client: &Client,
        until: Instant,
        namespace: &str,
        operation: Operation,
    ) -> mpsc::Receiver<Result<Answer, Refusal>> {
        let (response, receiver) = mpsc::sync_channel(1);
        client
            .handle
            .enqueue(
                Input::Submit {
                    namespace: namespace.into(),
                    operation,
                    application: None,
                    until,
                    response,
                },
                api::REQUEST_MAX,
            )
            .unwrap();
        receiver
    }
    /// Enqueues the call `f` at `client` without waiting.
    fn submit(
        client: &Client,
        until: Instant,
        f: impl FnOnce(&mut Db) -> anyhow::Result<Vec<u8>> + Send + 'static,
    ) -> mpsc::Receiver<Result<Answer, Refusal>> {
        let operation = Operation::Call(Mutex::new(Some(Box::new(move |db| {
            Ok(Box::new(f(db)?) as Box<dyn Any + Send>)
        }))));
        enqueue(client, until, "", operation)
    }
    /// Tells owner `owner` the host detector's `verdict` and steps every owner once: what the
    /// owner answered, which it has by then.
    fn tell(
        driven: &mut Driven,
        clients: &[Client],
        owner: usize,
        verdict: Verdict,
    ) -> Result<(), Refusal> {
        let (response, receiver) = mpsc::sync_channel(1);
        clients
            .get(owner)
            .unwrap()
            .handle
            .enqueue(
                Input::Verdict {
                    verdict,
                    until: driven.until(),
                    response,
                },
                std::mem::size_of::<Verdict>(),
            )
            .unwrap();
        driven.step();
        receiver.try_recv().unwrap()
    }
    fn called(answer: Answer) -> Vec<u8> {
        match answer {
            Answer::Called(value) => *value.downcast::<Vec<u8>>().unwrap(),
            _ => panic!("wrong actor reply"),
        }
    }
    fn terms(clients: &[Client]) -> Vec<u64> {
        clients
            .iter()
            .map(|c| c.handle().status().unwrap().term)
            .collect()
    }

    /// U15/CP-T1.8 on driven time (replication.md, "The owner's time"): a call whose own work
    /// outlasts its deadline is refused and never proposed. Do: the call's work inserts a row,
    /// then moves its owner's clock to the call's deadline, as slow work moves the real clock.
    /// Expect: Timeout, no row on any owner, and no election.
    #[test]
    fn a_call_whose_work_outlasts_its_deadline_is_refused_and_never_proposed() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        let elected = terms(&clients);
        let origin = clients.get(leader).unwrap();
        driven
            .call(&mut pump, origin, |db| {
                db.exec("CREATE TABLE slow_items(value)")?;
                Ok(Vec::new())
            })
            .unwrap();
        // The call's deadline is a second past its owner's clock, inside the relay's budget for
        // the records it carries, so the clock reaching it expires the call alone.
        let (clock, base) = (driven.clock(leader), driven.base);
        let until = base + offset(&clock) + Duration::from_secs(1);
        let answer = submit(origin, until, move |db| {
            db.exec("INSERT INTO slow_items VALUES(1)")?;
            set(&clock, base, until);
            Ok(Vec::new())
        });
        assert_eq!(driven.settle(&mut pump, &answer), Err(Refusal::Timeout));
        for client in &clients {
            let rows = driven
                .call(&mut pump, client, |db| {
                    let mut count = 0;
                    db.query("SELECT count(*) FROM slow_items", [], |row| {
                        count = row.get::<i64>(0).map_err(unavailable)?;
                        Ok(false)
                    })?;
                    Ok(vec![u8::try_from(count)?])
                })
                .unwrap();
            assert_eq!(rows, vec![0]);
        }
        assert_eq!(
            terms(&clients),
            elected,
            "an election happened during the scenario"
        );
        driven.drain(&mut pump);
        pump.finish();
    }

    /// How many tables named cancelled_actor_write the database has.
    fn cancelled_tables(db: &mut Db) -> anyhow::Result<u8> {
        let mut count = 0;
        db.query(
            "SELECT count(*) FROM sqlite_schema WHERE name='cancelled_actor_write'",
            [],
            |row| {
                count = row.get::<i64>(0).map_err(unavailable)?;
                Ok(false)
            },
        )?;
        Ok(u8::try_from(count)?)
    }

    /// U15/CP-T1.8 on driven time: a SQL deadline interrupts execution, rolls back its capture
    /// and frees the owner. Do: the call's work creates a table, moves its owner's clock to the
    /// call's deadline, as long work moves the real clock, then runs a query of ten million rows.
    /// Expect: the query interrupted at its first deadline check, Timeout, the table rolled back,
    /// and the owner's next call answered.
    #[test]
    fn expired_sql_rolls_back_before_actor_continues() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = vec![driven.open(dir.path(), 1, vec![1], None)];
        let mut pump = Pump::new(&clients);
        driven.elect(&mut pump, &clients, 0, 1);
        let ran = Arc::new(AtomicUsize::new(0));
        let created = Arc::new(AtomicUsize::new(0));
        let interrupted = Arc::new(AtomicUsize::new(0));
        let (counted, made, stopped) = (
            Arc::clone(&ran),
            Arc::clone(&created),
            Arc::clone(&interrupted),
        );
        let (clock, base, until) = (driven.clock(0), driven.base, driven.until());
        let answer = submit(clients.first().unwrap(), until, move |db| {
            counted.fetch_add(1, Ordering::Relaxed);
            db.exec("CREATE TABLE cancelled_actor_write(value)")?;
            made.store(usize::from(cancelled_tables(db)?), Ordering::Relaxed);
            set(&clock, base, until);
            // Each runner reports an interrupt as one (bug 68a308b7).
            let result = db.query_owned("WITH RECURSIVE n(x) AS(VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10000000) SELECT sum(x) FROM n", [], |_| Ok(true));
            if matches!(&result, Err(error) if format!("{error:#}").contains("interrupt")) {
                stopped.fetch_add(1, Ordering::Relaxed);
            }
            result?;
            Ok(Vec::new())
        });
        assert_eq!(driven.settle(&mut pump, &answer), Err(Refusal::Timeout));
        assert_eq!(ran.load(Ordering::Relaxed), 1);
        assert_eq!(created.load(Ordering::Relaxed), 1);
        assert_eq!(interrupted.load(Ordering::Relaxed), 1);
        let retained = driven
            .call(&mut pump, clients.first().unwrap(), |db| {
                Ok(vec![cancelled_tables(db)?])
            })
            .unwrap();
        assert_eq!(retained, vec![0]);
        driven.drain(&mut pump);
        pump.finish();
    }

    /// CP-T1.8 (AC-T1.8.3), the replicated class (replication.md, "Fast acknowledgement"): a
    /// write is answered once its commit is applied, without its leader flushing the database's
    /// log for it. Do: three voters, each its own fate, two to span, their flushes held by the test;
    /// a write at the leader. Expect: its answer while the leader holds it applied and unflushed;
    /// then the held flush makes it durable.
    #[test]
    fn a_replicated_write_is_answered_before_its_leader_flushes_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let durability = Durability::Replicated(api::replication::Fates {
            of: vec![(1, 10), (2, 20), (3, 30)],
            spanning: 2,
        });
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open_with(dir.path(), n, vec![1, 2, 3], None, durability.clone()))
            .collect();
        for owner in 0..3 {
            driven.hold_flushes(owner);
        }
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        let answer = driven.call(&mut pump, clients.get(leader).unwrap(), |db| {
            db.exec("CREATE TABLE answered_unflushed(value)")?;
            Ok(b"answered".to_vec())
        });
        assert_eq!(answer, Ok(b"answered".to_vec()));
        let held = driven.owner(leader).replica.machine().unflushed();
        assert!(
            held.is_some(),
            "the leader answered with its log flushed for the write, not held: {held:?}"
        );
        driven.flush(leader);
        assert_eq!(driven.owner(leader).replica.machine().unflushed(), None);
        driven.drain(&mut pump);
        pump.finish();
    }

    /// CP-T1.8 (AC-T1.8.3), the replicated class's metrics (replication.md, "Metrics"): what its
    /// host reads of a member through its handle. Do: three voters, each its own fate, two to
    /// span, the leader's flushes held; a write at the leader; its flush; then a write with the
    /// flushing thread's. Expect: the leader's backlog counts the write until its flush and is 0
    /// after, each entry it committed is counted once, by its quorum, and the thread's flush is
    /// counted in a bucket and in the time; a persisted member, whose flushing thread flushes too
    /// (AC-T1.8.D2), counts its backlog to 0 and its flushes, and no held commit.
    #[test]
    fn the_replicated_class_counts_its_backlog_flushes_and_commits() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let durability = Durability::Replicated(api::replication::Fates {
            of: vec![(1, 10), (2, 20), (3, 30)],
            spanning: 2,
        });
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open_with(dir.path(), n, vec![1, 2, 3], None, durability.clone()))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        let handle = clients.get(leader).unwrap().handle();
        driven.hold_flushes(leader);
        let answer = driven.call(&mut pump, clients.get(leader).unwrap(), |db| {
            db.exec("CREATE TABLE counted(value)")?;
            Ok(Vec::new())
        });
        assert_eq!(answer, Ok(Vec::new()));
        let held = handle.counters();
        assert!(
            held.flush_backlog >= 1,
            "the write applied and unflushed: {held:?}"
        );
        let committed =
            |driven: &Driven| driven.owner(leader).replica.core().raft.log().committed();
        assert_eq!(
            held.commits_held + held.commits_classic,
            committed(&driven),
            "each entry committed counted once, by its quorum: {held:?}"
        );
        driven.flush(leader);
        driven.step();
        assert_eq!(handle.counters().flush_backlog, 0);

        let owner = driven.owner(leader);
        let thread = Flushing::start(
            owner.replica.machine(),
            Arc::clone(&owner.control.meter),
            account(),
            Arc::clone(&owner.control.bell),
        )
        .unwrap();
        driven.owner_mut(leader).flushing = thread;
        let answer = driven.call(&mut pump, clients.get(leader).unwrap(), |db| {
            db.exec("INSERT INTO counted VALUES (1)")?;
            Ok(Vec::new())
        });
        assert_eq!(answer, Ok(Vec::new()));
        let mut flushed = handle.counters();
        for _ in 0..ROUNDS {
            if flushed.flush_backlog == 0 && flushed.flushes.iter().sum::<u64>() >= 1 {
                break;
            }
            driven.round(&mut pump);
            flushed = handle.counters();
        }
        assert_eq!(flushed.flush_backlog, 0, "{flushed:?}");
        assert!(flushed.flushes.iter().sum::<u64>() >= 1, "{flushed:?}");
        assert!(flushed.flush_time > Duration::ZERO, "{flushed:?}");
        assert_eq!(
            flushed.commits_held + flushed.commits_classic,
            committed(&driven),
            "{flushed:?}"
        );
        driven.drain(&mut pump);
        pump.finish();

        let persisted_dir = tempfile::tempdir().unwrap();
        let mut persisted = Driven::new();
        let single = vec![persisted.open(persisted_dir.path(), 1, vec![1], None)];
        let mut pump = Pump::new(&single);
        persisted.elect(&mut pump, &single, 0, 1);
        let answer = persisted.call(&mut pump, single.first().unwrap(), |db| {
            db.exec("CREATE TABLE counted(value)")?;
            Ok(Vec::new())
        });
        assert_eq!(answer, Ok(Vec::new()));
        let handle = single.first().unwrap().handle();
        let mut counted = handle.counters();
        for _ in 0..ROUNDS {
            if counted.flush_backlog == 0 && counted.flushes.iter().sum::<u64>() >= 1 {
                break;
            }
            persisted.round(&mut pump);
            counted = handle.counters();
        }
        assert_eq!(counted.flush_backlog, 0, "{counted:?}");
        assert!(counted.flushes.iter().sum::<u64>() >= 1, "{counted:?}");
        assert_eq!(counted.commits_held, 0);
        assert!(counted.commits_classic >= 2, "{counted:?}");
        persisted.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.D2, T-T1.8.D2 (replication.md, "A replica's database log"): the persisted class's
    /// owner never flushes its database's log on its own thread, so a flush that stands still
    /// stands none of the owner's steps still. Do: three persisted voters; the leader's flushes
    /// held from the test's thread, which steps every owner; a write at the leader, answered;
    /// then four election timeouts of ticks, every clock a tick and the group quiet after each;
    /// then the hold opened. Expect: no flush reached the hold on the owners' thread; the write
    /// answered Ok while its flush was held, the leader's backlog counting it; the term and the
    /// leader unchanged; the backlog 0 once the hold opens.
    #[test]
    fn a_held_database_flush_stalls_no_persisted_owner_step() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        let term = driven.owner(leader).replica.term();
        let gate = driven.owner(leader).replica.machine().flush_gate();
        gate.hold();
        let answered = driven.call(&mut pump, clients.get(leader).unwrap(), |db| {
            db.exec("CREATE TABLE held_flush(value)")?;
            Ok(b"answered".to_vec())
        });
        let election = driven
            .owner(leader)
            .replica
            .core()
            .raft
            .config()
            .election_tick;
        // Four election timeouts on ticks, each tick on every clock and the group quiet after it,
        // so that only the owners' steps, not the disk's pace, decide who campaigns.
        for _ in 0..election * 4 {
            for owner in 0..clients.len() {
                driven.advance_tick(owner);
            }
            driven.step();
            driven.quiet(&mut pump);
        }
        let held = clients.get(leader).unwrap().handle().counters();
        assert_eq!(
            gate.reached_from_holder(),
            0,
            "the leader's owner flushed its database's log on its own thread"
        );
        assert_eq!(answered, Ok(b"answered".to_vec()));
        assert!(held.flush_backlog >= 1, "{held:?}");
        assert_eq!(driven.owner(leader).replica.term(), term);
        let id = u64::try_from(leader + 1).unwrap();
        for client in &clients {
            assert_eq!(client.handle().status().unwrap().leader, id);
        }
        gate.open();
        let handle = clients.get(leader).unwrap().handle();
        for _ in 0..ROUNDS {
            if handle.counters().flush_backlog == 0 {
                break;
            }
            driven.round(&mut pump);
        }
        assert_eq!(handle.counters().flush_backlog, 0);
        driven.drain(&mut pump);
        pump.finish();
    }

    /// CP-T1.8, the replicated class's configuration (replication.md, "Fast acknowledgement"): a
    /// group that can't span the fates asked is refused, typed. Do: a voter set in one fate, one to
    /// span, a member with no fate, a non-member's fate, a fate stated twice, then three fates with
    /// two to span. Expect: each refused naming why, and the last taken.
    #[test]
    fn the_replicated_class_is_refused_where_its_voters_span_too_few_fates() {
        let config = ReplicaConfig {
            node_id: 1,
            voters: vec![1, 2, 3],
            learners: Vec::new(),
            bootstrap: None,
        };
        let class = |of: Vec<(u64, u64)>, spanning: u32| {
            Durability::Replicated(api::replication::Fates { of, spanning })
        };
        let refused = |durability: Durability| match validate_durability(&config, &durability) {
            Err(Refusal::Invalid { why, .. }) => why,
            other => panic!("expected a typed refusal, got {other:?}"),
        };
        assert!(refused(class(vec![(1, 7), (2, 7), (3, 7)], 2)).contains("span 1 fates"));
        assert!(refused(class(vec![(1, 1), (2, 2), (3, 3)], 1)).contains("at least two"));
        assert!(refused(class(vec![(1, 1), (2, 2)], 2)).contains("every member"));
        assert!(refused(class(vec![(1, 1), (2, 2), (3, 3), (4, 4)], 2)).contains("isn't a member"));
        assert!(refused(class(vec![(1, 1), (1, 2), (3, 3)], 2)).contains("twice"));
        assert_eq!(
            validate_durability(&config, &class(vec![(3, 30), (1, 10), (2, 20)], 2)),
            Ok(())
        );
        assert_eq!(validate_durability(&config, &Durability::Persisted), Ok(()));
    }

    /// CP-T1.8, the replicated class reaches the core (hyper-raft R-8 and R-6): its fates, ordered
    /// by member, and a leader applying before its own write is durable; the persisted class keeps
    /// neither. Do: open one member of each. Expect: the replicated one's core holds the fates and
    /// applies unpersisted, the persisted one's neither.
    #[test]
    fn the_replicated_class_reaches_the_core() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let durability = Durability::Replicated(api::replication::Fates {
            of: vec![(2, 20), (1, 10), (3, 30)],
            spanning: 2,
        });
        let _replicated = driven.open_with(dir.path(), 1, vec![1, 2, 3], None, durability);
        let persisted_dir = tempfile::tempdir().unwrap();
        let _persisted = driven.open(persisted_dir.path(), 1, vec![1, 2, 3], None);
        let config = driven.owner(0).replica.core().raft.config();
        let fates = config
            .fates
            .as_ref()
            .expect("the replicated class states its fates");
        assert_eq!(fates.of, vec![(1, 10), (2, 20), (3, 30)]);
        assert_eq!(fates.spanning, 2);
        assert!(config.apply_unpersisted);
        let config = driven.owner(1).replica.core().raft.config();
        assert!(config.fates.is_none() && !config.apply_unpersisted);
    }

    /// CP-T1.8 (AC-T1.8.3), the suspicion feed (replication.md, "The suspicion feed"): a verdict
    /// told to a member while it follows stays in force once it leads. Do: three voters of three
    /// fates, two to span; member B suspected at follower A; the lead handed to A, and writes there;
    /// the old leader cut off; B's durable answers kept back by the relay, its held ones let
    /// through; a write at A. Expect: no answer though B said it holds the write, since A's disk
    /// and a suspected member's memory are no held quorum; once B's durable answers pass, the
    /// answer, the classic quorum's. Then, the control: B trusted at A, its durable answers kept
    /// back again, and a write answered on the held quorum.
    #[test]
    fn a_verdict_told_while_following_stays_in_force_once_leading() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open_with(dir.path(), n, vec![1, 2, 3], None, fated()))
            .collect();
        let mut pump = Pump::new(&clients);
        let old = driven.elect(&mut pump, &clients, 0, 1);
        let (a, b) = ((old + 1) % 3, (old + 2) % 3);
        let b_id = u64::try_from(b + 1).unwrap();
        assert_eq!(
            tell(&mut driven, &clients, a, Verdict::Suspected(b_id)),
            Ok(())
        );
        driven.transfer(&mut pump, &clients, old, a);
        // Writes at the new leader, so that B states a durable promise to it and answers held at
        // once (hyper-raft R-8).
        for n in 0..4 {
            let answer = driven.call(&mut pump, clients.get(a).unwrap(), move |db| {
                db.exec(&format!("CREATE TABLE warm{n}(value)"))?;
                Ok(Vec::new())
            });
            assert_eq!(answer, Ok(Vec::new()));
        }
        let applied = clients.get(a).unwrap().handle().status().unwrap().applied;
        driven.follow(&mut pump, &clients, applied);
        pump.lost.push(u64::try_from(old + 1).unwrap());

        // A write at A, and rounds until B said it holds it.
        let held_by_b = |driven: &mut Driven, pump: &mut Pump<'_>, before: u64| -> u64 {
            for _ in 0..ROUNDS {
                driven.round(pump);
                let raft = &driven.owner(a).replica.core().raft;
                let last = raft.log().last_index().unwrap();
                if last > before && raft.tracker().get(b_id).unwrap().held >= last {
                    return last;
                }
            }
            panic!("B never said it holds the write in {ROUNDS} driven rounds");
        };
        pump.hold_durable();
        let before = driven
            .owner(a)
            .replica
            .core()
            .raft
            .log()
            .last_index()
            .unwrap();
        let write = submit(clients.get(a).unwrap(), driven.until(), |db| {
            db.exec("INSERT INTO warm0 VALUES (1)")?;
            Ok(b"suspected".to_vec())
        });
        let index = held_by_b(&mut driven, &mut pump, before);
        for _ in 0..100 {
            driven.round(&mut pump);
        }
        let committed = driven.owner(a).replica.core().raft.log().committed();
        assert!(
            committed < index && matches!(write.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "a held quorum counted a member suspected while its leader followed: committed \
             {committed} of {index}"
        );
        pump.let_durable();
        assert_eq!(driven.settle(&mut pump, &write), Ok(b"suspected".to_vec()));

        assert_eq!(
            tell(&mut driven, &clients, a, Verdict::Trusted(b_id)),
            Ok(())
        );
        pump.hold_durable();
        let control = submit(clients.get(a).unwrap(), driven.until(), |db| {
            db.exec("INSERT INTO warm0 VALUES (2)")?;
            Ok(b"trusted".to_vec())
        });
        assert_eq!(driven.settle(&mut pump, &control), Ok(b"trusted".to_vec()));
        pump.let_durable();
        pump.lost.clear();
        driven.drain(&mut pump);
        pump.finish();
    }

    /// CP-T1.8 (AC-T1.8.3), the suspicion feed (replication.md, "The suspicion feed"): a member the
    /// host's detector suspects isn't counted in a held quorum until it is trusted, and meanwhile
    /// a write waits for the classic quorum. Do: three voters of three fates, two to span; the
    /// followers' durable answers kept back by the relay, their held answers let through; both
    /// followers suspected at the leader; a write there. Expect: no answer though both followers
    /// said they hold it; trusting one, the answer (the leader's disk and that member's memory).
    /// Then both suspected again and a second write: answered only once the durable answers pass.
    /// A verdict of the leader itself or of a non-member is refused, typed.
    #[test]
    fn a_member_the_detector_suspects_isnt_counted_in_a_held_quorum_until_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let durability = Durability::Replicated(api::replication::Fates {
            of: vec![(1, 10), (2, 20), (3, 30)],
            spanning: 2,
        });
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open_with(dir.path(), n, vec![1, 2, 3], None, durability.clone()))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        let at = clients.get(leader).unwrap();
        let id = u64::try_from(leader + 1).unwrap();
        let followers: Vec<u64> = (1..=3).filter(|member| *member != id).collect();
        // Writes before the test's, so that each follower states a durable promise and answers
        // held at once (hyper-raft R-8).
        for n in 0..4 {
            let answer = driven.call(&mut pump, at, move |db| {
                db.exec(&format!("CREATE TABLE warm{n}(value)"))?;
                Ok(Vec::new())
            });
            assert_eq!(answer, Ok(Vec::new()));
        }
        // Rounds until every follower said it holds the leader's last entry, which isn't
        // committed: the write is proposed and only a held quorum could have answered it.
        let held_by_all = |driven: &mut Driven, pump: &mut Pump<'_>, before: u64| -> u64 {
            for _ in 0..ROUNDS {
                driven.round(pump);
                let raft = &driven.owner(leader).replica.core().raft;
                let last = raft.log().last_index().unwrap();
                if last > before
                    && followers
                        .iter()
                        .all(|f| raft.tracker().get(*f).unwrap().held >= last)
                {
                    return last;
                }
            }
            panic!("the followers never said they hold the write in {ROUNDS} driven rounds");
        };

        pump.hold_durable();
        for follower in &followers {
            assert_eq!(
                tell(&mut driven, &clients, leader, Verdict::Suspected(*follower)),
                Ok(())
            );
        }
        let before = driven
            .owner(leader)
            .replica
            .core()
            .raft
            .log()
            .last_index()
            .unwrap();
        let first = submit(at, driven.until(), |db| {
            db.exec("INSERT INTO warm0 VALUES (1)")?;
            Ok(b"first".to_vec())
        });
        let index = held_by_all(&mut driven, &mut pump, before);
        for _ in 0..100 {
            driven.round(&mut pump);
        }
        let committed = driven.owner(leader).replica.core().raft.log().committed();
        assert!(
            committed < index && matches!(first.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "a held quorum of suspected members committed {index} (committed {committed})"
        );
        let trusted = *followers.first().unwrap();
        assert_eq!(
            tell(&mut driven, &clients, leader, Verdict::Trusted(trusted)),
            Ok(())
        );
        assert_eq!(driven.settle(&mut pump, &first), Ok(b"first".to_vec()));

        assert_eq!(
            tell(&mut driven, &clients, leader, Verdict::Suspected(trusted)),
            Ok(())
        );
        let before = driven
            .owner(leader)
            .replica
            .core()
            .raft
            .log()
            .last_index()
            .unwrap();
        let second = submit(at, driven.until(), |db| {
            db.exec("INSERT INTO warm0 VALUES (2)")?;
            Ok(b"second".to_vec())
        });
        let index = held_by_all(&mut driven, &mut pump, before);
        for _ in 0..100 {
            driven.round(&mut pump);
        }
        let committed = driven.owner(leader).replica.core().raft.log().committed();
        assert!(
            committed < index && matches!(second.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "a held quorum of suspected members committed {index} (committed {committed})"
        );
        pump.let_durable();
        assert_eq!(driven.settle(&mut pump, &second), Ok(b"second".to_vec()));

        for refused in [Verdict::Suspected(id), Verdict::Trusted(9)] {
            assert!(
                matches!(
                    tell(&mut driven, &clients, leader, refused),
                    Err(Refusal::Invalid { .. })
                ),
                "{refused:?} was taken"
            );
        }
        for follower in &followers {
            assert_eq!(
                tell(&mut driven, &clients, leader, Verdict::Trusted(*follower)),
                Ok(())
            );
        }
        driven.drain(&mut pump);
        pump.finish();
    }

    /// The fates a group of three voters states in its tests: one each, two to span.
    fn three_fates() -> Durability {
        Durability::Replicated(api::replication::Fates {
            of: vec![(1, 10), (2, 20), (3, 30)],
            spanning: 2,
        })
    }

    /// AC-T1.8.5, T-T1.8.C2 (replication.md, "The class stated once per group"), on driven time: a
    /// member stating another class than its group's log is refused, typed, at apply and at open.
    /// Do: three voters, the first two stating one fate each and the third stating the first two
    /// share a fate; the first campaigns, and rounds go on, each owner stepped, until an owner
    /// ends; a write at the leader; the third opened again stating its own class, then the
    /// group's. Expect: the third alone ends, Fenced, naming the class; the write answered by the
    /// two left; the reopening under its own class refused Invalid, naming the class; under the
    /// group's it opens and catches up to the write.
    #[test]
    fn two_members_stating_different_classes_the_second_is_refused_typed() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let group = three_fates();
        let other = Durability::Replicated(api::replication::Fates {
            of: vec![(1, 10), (2, 10), (3, 30)],
            spanning: 2,
        });
        let clients: Vec<Client> = (1..=3)
            .map(|n| {
                let stated = if n == 3 { &other } else { &group };
                driven.open_with(dir.path(), n, vec![1, 2, 3], None, stated.clone())
            })
            .collect();
        let mut pump = Pump::new(&clients);
        driven.owner_mut(0).replica.campaign().unwrap();
        let mut ended = None;
        for _ in 0..ROUNDS {
            if pump.drive_until(driven.until()) == 0 {
                std::thread::park_timeout(POLL);
            }
            for (at, slot) in driven.owners.iter_mut().enumerate() {
                let Some((owner, inputs)) = slot.as_mut() else {
                    continue;
                };
                let mut stepped = Ok(());
                while let Ok(input) = inputs.try_recv() {
                    stepped = stepped.and_then(|()| owner.run_step(Some(input)));
                }
                if let Err(refusal) = stepped.and_then(|()| owner.run_step(None)) {
                    assert_eq!(at, 2, "member {} ended: {refusal:?}", at + 1);
                    ended = Some(refusal);
                    *slot = None;
                }
            }
            if ended.is_some() {
                break;
            }
        }
        let ended = ended.expect("the member stating another class was never refused");
        assert!(
            matches!(&ended, Refusal::Fenced(why) if why.contains("class")),
            "{ended:?}"
        );
        pump.ended(3);
        let leader = clients.first().unwrap();
        assert_eq!(
            driven.call(&mut pump, leader, |db| {
                db.exec("CREATE TABLE member_items(value); INSERT INTO member_items VALUES (1)")?;
                Ok(Vec::new())
            }),
            Ok(Vec::new())
        );
        pump.finish();

        let path = dir.path().join("state-3.db");
        let reopen = |driven: &Driven, durability: Durability| {
            let (_, clock) = driven.reading();
            super::open(
                &path,
                &dir.path().join("raft-3.log"),
                ReplicaConfig {
                    node_id: 3,
                    voters: vec![1, 2, 3],
                    learners: Vec::new(),
                    bootstrap: None,
                },
                durability,
                Arc::new(Changed::default()),
                Owned::unbudgeted(lease(&path), None),
                driven.until(),
                clock,
            )
        };
        match reopen(&driven, other) {
            Err(Refusal::Invalid { what, why }) => {
                assert!(
                    what.contains("class") || why.contains("class"),
                    "{what}: {why}"
                );
            }
            Err(refusal) => panic!("refused, but not Invalid: {refusal:?}"),
            Ok(_) => panic!("the member opened stating another class than its group's"),
        }
        let (client, actor, receiver) = reopen(&driven, group).unwrap();
        *driven.owners.get_mut(2).unwrap() = Some((actor, receiver));
        let clients = vec![
            clients.first().unwrap().clone(),
            clients.get(1).unwrap().clone(),
            client,
        ];
        let mut pump = Pump::new(&clients);
        let reopened = clients.get(2).unwrap();
        let mut caught_up = Ok(Vec::new());
        for _ in 0..ROUNDS {
            driven.round(&mut pump);
            caught_up = reopened.read_local(driven.until(), values);
            if caught_up == Ok(vec![1]) {
                break;
            }
        }
        assert_eq!(
            caught_up,
            Ok(vec![1]),
            "the reopened member never caught up"
        );
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.5, T-T1.8.C3 (replication.md, "The class stated once per group"), on driven time:
    /// a member counts no held quorum before it has applied its group's class. Do: three voters
    /// of three fates, two to span, none proposing the class; four writes, each durable; every
    /// follower's durable answers held, and a write; the class let be proposed, the durable
    /// answers let through and rounds; the durable answers held again, and a write. Expect: the
    /// first held write not answered once both followers said they hold it; answered once the
    /// durable answers pass; the second answered on held answers alone.
    #[test]
    fn no_held_quorum_is_counted_before_the_class_entry_applies() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open_with(dir.path(), n, vec![1, 2, 3], None, three_fates()))
            .collect();
        for owner in 0..3 {
            driven.owner_mut(owner).class_held = true;
        }
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        let at = clients.get(leader).unwrap();
        let id = u64::try_from(leader + 1).unwrap();
        let followers: Vec<u64> = (1..=3).filter(|member| *member != id).collect();
        for n in 0..4 {
            let answer = driven.call(&mut pump, at, move |db| {
                db.exec(&format!("CREATE TABLE warm{n}(value)"))?;
                Ok(Vec::new())
            });
            assert_eq!(answer, Ok(Vec::new()));
        }
        let held_by_all = |driven: &mut Driven, pump: &mut Pump<'_>, before: u64| -> u64 {
            for _ in 0..ROUNDS {
                driven.round(pump);
                let raft = &driven.owner(leader).replica.core().raft;
                let last = raft.log().last_index().unwrap();
                if last > before
                    && followers
                        .iter()
                        .all(|f| raft.tracker().get(*f).unwrap().held >= last)
                {
                    return last;
                }
            }
            panic!("the followers never said they hold the write in {ROUNDS} driven rounds");
        };
        let last = |driven: &Driven| {
            driven
                .owner(leader)
                .replica
                .core()
                .raft
                .log()
                .last_index()
                .unwrap()
        };

        pump.hold_durable();
        let before = last(&driven);
        let first = submit(at, driven.until(), |db| {
            db.exec("INSERT INTO warm0 VALUES (1)")?;
            Ok(b"first".to_vec())
        });
        let index = held_by_all(&mut driven, &mut pump, before);
        for _ in 0..100 {
            driven.round(&mut pump);
        }
        let committed = driven.owner(leader).replica.core().raft.log().committed();
        assert!(
            committed < index && matches!(first.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "a held quorum committed {index} before the class applied (committed {committed})"
        );
        for owner in 0..3 {
            driven.owner_mut(owner).class_held = false;
        }
        pump.let_durable();
        assert_eq!(driven.settle(&mut pump, &first), Ok(b"first".to_vec()));
        for _ in 0..100 {
            driven.round(&mut pump);
        }

        pump.hold_durable();
        let second = submit(at, driven.until(), |db| {
            db.exec("INSERT INTO warm0 VALUES (2)")?;
            Ok(b"second".to_vec())
        });
        assert_eq!(driven.settle(&mut pump, &second), Ok(b"second".to_vec()));
        pump.let_durable();
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.5, T-T1.8.C5 (replication.md, "The class stated once per group", the amendment), on
    /// driven time: a joiner's fate is recorded by a compatible class entry, the procedure
    /// "A member added or removed" states, member by member. Do: three voters of three fates
    /// record their class; a fourth member added as a learner (not running); a follower reopened
    /// stating the joiner's fate too, and handed the lead; rounds until each records a class;
    /// then the lead handed between the narrower-stating member and the wider three times, a
    /// write in each lead. Expect: the addition answered; the follower opens under the wider
    /// class beside the group's record; every voter records the wider class, the others'
    /// narrower one compatible with it, and none gates held answers after; no leader after
    /// proposes a class, and the record changed twice in all (the group's class, the joiner's
    /// fate), never back: it only grows between changes of members.
    #[test]
    fn a_joiners_fate_is_recorded_by_a_compatible_class_entry() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open_with(dir.path(), n, vec![1, 2, 3], None, three_fates()))
            .collect();
        let mut pump = Pump::new(&clients);
        pump.lost.push(4);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        write(&mut driven, &mut pump, clients.get(leader).unwrap(), 1);
        let added = change(
            clients.get(leader).unwrap(),
            driven.until(),
            MemberChange::AddLearner(4),
        );
        assert!(driven.answer(&mut pump, &added).is_ok());
        driven.drain(&mut pump);
        pump.finish();

        // The procedure, member by member: a follower reopened stating the joiner's fate too,
        // then handed the lead.
        let wider = Durability::Replicated(api::replication::Fates {
            of: vec![(1, 10), (2, 20), (3, 30), (4, 40)],
            spanning: 2,
        });
        let follower = (leader + 1) % 3;
        let node = u64::try_from(follower + 1).unwrap();
        drop(driven.owners.get_mut(follower).unwrap().take());
        let path = dir.path().join(format!("state-{node}.db"));
        let (_, clock) = driven.reading();
        let (client, actor, receiver) = super::open(
            &path,
            &dir.path().join(format!("raft-{node}.log")),
            ReplicaConfig {
                node_id: node,
                voters: vec![1, 2, 3],
                learners: vec![4],
                bootstrap: None,
            },
            wider.clone(),
            Arc::new(Changed::default()),
            Owned::unbudgeted(lease(&path), None),
            driven.until(),
            clock,
        )
        .unwrap();
        *driven.owners.get_mut(follower).unwrap() = Some((actor, receiver));
        let mut clients = clients;
        *clients.get_mut(follower).unwrap() = client;
        let mut pump = Pump::new(&clients);
        pump.lost.push(4);
        // A write at the leader, whose append tells the reopened follower who leads.
        write(&mut driven, &mut pump, clients.get(leader).unwrap(), 2);
        driven.follow(&mut pump, &clients, 1);
        driven.transfer(&mut pump, &clients, leader, follower);
        let recorded = |driven: &Driven| {
            (0..3).all(|owner| driven.owner(owner).replica.machine().class() == Some(&wider))
        };
        for _ in 0..ROUNDS {
            if recorded(&driven) {
                break;
            }
            driven.round(&mut pump);
        }
        let seen: Vec<_> = (0..3)
            .map(|owner| {
                let actor = driven.owner(owner);
                (
                    actor.replica.is_leader(),
                    actor.replica.term(),
                    actor.replica.applied().index,
                    actor.class_proposed,
                    actor.replica.machine().class().cloned(),
                )
            })
            .collect();
        assert!(
            recorded(&driven),
            "the joiner's fate was never recorded: {seen:?}"
        );
        for owner in 0..3 {
            let actor = driven.owner(owner);
            assert!(actor.replica.machine().class_agreed());
            assert!(
                actor.gated.is_empty(),
                "owner {owner} still gates held answers"
            );
        }

        // Leaders stating the narrower class and the wider in turn: the record changes no more.
        let changes = driven.owner(leader).replica.machine().class_changes();
        assert_eq!(changes, 2, "the group's class, then the joiner's fate");
        for (from, to) in [(follower, leader), (leader, follower), (follower, leader)] {
            driven.transfer(&mut pump, &clients, from, to);
            write(&mut driven, &mut pump, clients.get(to).unwrap(), 3);
            let term = driven.owner(to).replica.term();
            assert_ne!(
                driven.owner(to).class_proposed,
                Some(term),
                "owner {to} proposed a class leading"
            );
            for owner in 0..3 {
                assert_eq!(driven.owner(owner).replica.machine().class(), Some(&wider));
            }
            assert_eq!(
                driven.owner(leader).replica.machine().class_changes(),
                changes
            );
        }
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.5, T-T1.8.C6 (replication.md, "The class stated once per group", the amendment;
    /// amphora-seam-owner, amphora-owner-t1-class-and-readers-8d4c05b-20261109), on driven time: a
    /// joiner whose fate the group recorded, once promoted to voter, counts in a held quorum. Do:
    /// three voters of fates 10, 20 and 30 and a learner of fate 40, two to span, all four
    /// stating the class of all four; the class recorded; a write, the learner caught up and
    /// promoted, and four writes more; then, every clock still, every durable answer held, one
    /// follower's answers to appends lost (its heartbeats' pass), the joiner's records kept in its
    /// queue, and a write at the leader; rounds; then the joiner's records let through. Expect: the promotion answered
    /// with 4 a voter; the write uncommitted while only the leader and the other follower hold
    /// it (two of four voters); once the joiner holds it, answered on held answers alone, the
    /// joiner's held mark at its index, and the leader's held commits counted up.
    #[test]
    fn a_promoted_joiner_counts_in_a_held_quorum() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let four = Durability::Replicated(api::replication::Fates {
            of: vec![(1, 10), (2, 20), (3, 30), (4, 40)],
            spanning: 2,
        });
        let clients: Vec<Client> = (1..=4)
            .map(|n| driven.member(dir.path(), n, vec![1, 2, 3], vec![4], None, four.clone()))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients[..3], 0, 1);
        let at = clients.get(leader).unwrap();
        write(&mut driven, &mut pump, at, 1);
        for _ in 0..ROUNDS {
            if (0..4).all(|owner| driven.owner(owner).replica.machine().class() == Some(&four)) {
                break;
            }
            driven.round(&mut pump);
        }
        for owner in 0..4 {
            assert_eq!(driven.owner(owner).replica.machine().class(), Some(&four));
        }
        catch_up(&mut driven, &mut pump, &clients, leader, 3);
        let promoted = promote(&mut driven, &mut pump, at).unwrap();
        assert_eq!(
            (promoted.voters, promoted.learners),
            (vec![1, 2, 3, 4], vec![])
        );
        // Writes before the test's, so that each voter, the joiner among them, states a durable
        // promise and answers held at once (hyper-raft R-8).
        for value in 2..6 {
            write(&mut driven, &mut pump, at, value);
        }
        catch_up(&mut driven, &mut pump, &clients, leader, 3);

        let id = u64::try_from(leader + 1).unwrap();
        let cut = (1..=3).find(|member| *member != id).unwrap();
        let term = driven.owner(leader).replica.term();
        let held_before = clients[leader].handle().counters().commits_held;
        // One follower's answers to appends are lost, its heartbeats' pass, so the write's read
        // fence has its quorum. The joiner's records stay in its own queue until let through. No
        // clock moves: a heartbeat's answer stating a member's position counts as an append's
        // (hyper-raft's handle_heartbeat_response), which would pass the durable hold.
        pump.losing = Some(Box::new(move |from, message: &Message| {
            from == cut && message.msg_type == hyper_raft::proto::MessageType::MsgAppendResponse
        }));
        pump.held.push(3);
        pump.hold_durable();
        let last = |driven: &Driven| {
            driven
                .owner(leader)
                .replica
                .core()
                .raft
                .log()
                .last_index()
                .unwrap()
        };
        let before = last(&driven);
        let answer = submit(at, driven.until(), |db| {
            db.exec("INSERT INTO member_items VALUES (6)")?;
            Ok(b"held".to_vec())
        });
        for _ in 0..ROUNDS {
            if last(&driven) > before {
                break;
            }
            driven.round(&mut pump);
        }
        let index = last(&driven);
        assert!(index > before, "the leader never appended the write");
        for _ in 0..100 {
            driven.round(&mut pump);
        }
        let committed = driven.owner(leader).replica.core().raft.log().committed();
        assert!(
            committed < index && matches!(answer.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "two of four voters committed {index} (committed {committed})"
        );

        // The joiner's records let through: its held answer passes, its durable one is kept back.
        pump.held.retain(|owner| *owner != 3);
        let mut answered = None;
        for _ in 0..ROUNDS {
            match answer.try_recv() {
                Ok(result) => {
                    answered = Some(result.map(called));
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(error) => panic!("owner ended: {error}"),
            }
            driven.round(&mut pump);
        }
        assert_eq!(answered, Some(Ok(b"held".to_vec())));
        let raft = &driven.owner(leader).replica.core().raft;
        let seen: Vec<_> = (1..=4)
            .map(|member| {
                let progress = raft.tracker().get(member).unwrap();
                (member, progress.matched, progress.held)
            })
            .collect();
        assert!(
            raft.tracker().get(4).unwrap().held >= index,
            "term {} (was {term}), index {index}, committed {}, members (matched, held) {seen:?}",
            driven.owner(leader).replica.term(),
            raft.log().committed()
        );
        assert!(
            clients[leader].handle().counters().commits_held > held_before,
            "{:?}",
            clients[leader].handle().counters()
        );
        pump.losing = None;
        pump.let_durable();
        driven.drain(&mut pump);
        pump.finish();
    }

    /// U15/CP-T1.8 on driven time (replication.md, "The owner's time"): follower calls use the
    /// real leader's ReadIndex and proposal forwarding, and no election happens during the scenario.
    #[test]
    fn follower_origin_captures_once_and_reads_after_local_durable_apply() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        let elected = terms(&clients);
        let origin = (leader + 1) % 3;
        let evaluated = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&evaluated);
        let chosen = driven
            .call(&mut pump, clients.get(origin).unwrap(), move |db| {
                count.fetch_add(1, Ordering::Relaxed);
                db.exec("CREATE TABLE actor_items(value BLOB DEFAULT(randomblob(32))); INSERT INTO actor_items DEFAULT VALUES")?;
                value(db)
            })
            .unwrap();
        assert_eq!(evaluated.load(Ordering::Relaxed), 1);
        assert_eq!(
            driven
                .call(&mut pump, clients.get((origin + 1) % 3).unwrap(), value)
                .unwrap(),
            chosen
        );
        assert_eq!(evaluated.load(Ordering::Relaxed), 1);

        // Both forwarded proposals are held, so both captures use the same committed base; exactly
        // one page set may apply and the stale one's answer is refused.
        let captures = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
        pump.begin_captures(leader, 2);
        let answers: Vec<_> = (1..=2usize)
            .map(|offset| {
                let captures = Arc::clone(&captures);
                submit(
                    clients.get((leader + offset) % 3).unwrap(),
                    driven.until(),
                    move |db| {
                        captures[offset - 1].fetch_add(1, Ordering::Relaxed);
                        db.exec("INSERT INTO actor_items(value) VALUES(x'01')")?;
                        Ok(vec![1])
                    },
                )
            })
            .collect();
        let mut outcomes: Vec<Option<Result<Vec<u8>, Refusal>>> = vec![None, None];
        for _ in 0..ROUNDS {
            for (answer, outcome) in answers.iter().zip(outcomes.iter_mut()) {
                if outcome.is_none()
                    && let Ok(result) = answer.try_recv()
                {
                    *outcome = Some(result.map(called));
                }
            }
            if outcomes.iter().all(Option::is_some) {
                break;
            }
            driven.round(&mut pump);
            if pump.captures_ready() {
                assert!(
                    captures
                        .iter()
                        .all(|count| count.load(Ordering::Relaxed) == 1)
                );
                pump.release_captures();
            }
        }
        let outcomes: Vec<_> = outcomes.into_iter().map(Option::unwrap).collect();
        assert!(matches!(pump.phase, PumpPhase::Relay));
        assert!(
            captures
                .iter()
                .all(|count| count.load(Ordering::Relaxed) == 1)
        );
        assert_eq!(
            outcomes.iter().filter(|r| r.is_ok()).count(),
            1,
            "{outcomes:?}"
        );
        assert_eq!(outcomes.iter().filter(|r| matches!(r, Err(Refusal::Invalid { what, .. }) if what == "prepared transaction")).count(), 1, "{outcomes:?}");
        let count = driven
            .call(&mut pump, clients.get(origin).unwrap(), |db| {
                db.exec("INSERT INTO actor_items(value) VALUES(x'03')")?;
                let mut count = 0;
                db.query("SELECT count(*) FROM actor_items", [], |row| {
                    count = row.get::<i64>(0).map_err(unavailable)?;
                    Ok(false)
                })?;
                Ok(vec![u8::try_from(count)?])
            })
            .unwrap();
        assert_eq!(count, vec![3]);
        assert_eq!(
            terms(&clients),
            elected,
            "an election happened during the scenario"
        );
        driven.drain(&mut pump);
        pump.finish();
    }

    /// U15/CP-T1.8 under a nemesis (replication.md, "The owner's time"): the follower-origin
    /// scenario with the lead moved to another owner at each of its steps: while its first call
    /// and the read after it are in flight, as the held captures go out, after the first capture
    /// answers, and before the last call. Whatever the election: never two Oks for the two
    /// captures and never two of their rows, every answer Ok, the stale capture's refusal or
    /// UnknownOutcome, and a capture answered Ok has its row.
    #[test]
    fn follower_origin_captures_hold_through_an_election_at_each_step() {
        for position in 0..5 {
            let dir = tempfile::tempdir().unwrap();
            let mut driven = Driven::new();
            let clients: Vec<Client> = (1..=3)
                .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
                .collect();
            let mut pump = Pump::new(&clients);
            pump.refusals = Some(0);
            let leader = driven.elect(&mut pump, &clients, 0, 1);
            let origin = (leader + 1) % 3;
            // Positions 3 and 4: the lead moves while the first call, then the read after it, is
            // in flight. Each answers Ok or UnknownOutcome, and the table holds what an Ok says.
            let create = submit(clients.get(origin).unwrap(), driven.until(), |db| {
                db.exec("CREATE TABLE actor_items(value BLOB DEFAULT(randomblob(32))); INSERT INTO actor_items DEFAULT VALUES")?;
                value(db)
            });
            if position == 3 {
                driven.transfer(&mut pump, &clients, leader, origin);
            }
            let created = driven.settle(&mut pump, &create);
            assert!(
                matches!(created, Ok(ref v) if v.len() == 32)
                    || matches!(created, Err(Refusal::UnknownOutcome)),
                "position {position}: {created:?}"
            );
            if position >= 3 {
                let read = submit(
                    clients.get((origin + 1) % 3).unwrap(),
                    driven.until(),
                    |db| {
                        let mut tables = 0;
                        db.query(
                            "SELECT count(*) FROM sqlite_schema WHERE name = 'actor_items'",
                            [],
                            |row| {
                                tables = row.get::<i64>(0).map_err(unavailable)?;
                                Ok(false)
                            },
                        )?;
                        Ok(if tables == 1 { value(db)? } else { Vec::new() })
                    },
                );
                if position == 4 {
                    let now = usize::try_from(
                        clients.first().unwrap().handle().status().unwrap().leader - 1,
                    )
                    .unwrap();
                    driven.transfer(&mut pump, &clients, now, (now + 1) % 3);
                }
                let after = driven.settle(&mut pump, &read);
                match (&created, &after) {
                    (Ok(chosen), Ok(seen)) => assert_eq!(seen, chosen, "position {position}"),
                    (Err(_), Ok(seen)) => assert!(
                        seen.is_empty() || seen.len() == 32,
                        "position {position}: {seen:?}"
                    ),
                    (_, Err(refusal)) => assert!(
                        matches!(refusal, Refusal::UnknownOutcome),
                        "position {position}: {refusal:?}"
                    ),
                }
                driven.drain(&mut pump);
                pump.finish();
                continue;
            }
            assert!(created.is_ok(), "position {position}: {created:?}");
            pump.begin_captures(leader, 2);
            let answers: Vec<_> = (1..=2usize)
                .map(|offset| {
                    submit(
                        clients.get((leader + offset) % 3).unwrap(),
                        driven.until(),
                        |db| {
                            db.exec("INSERT INTO actor_items(value) VALUES(x'01')")?;
                            Ok(vec![1])
                        },
                    )
                })
                .collect();
            let mut outcomes: Vec<Option<Result<Vec<u8>, Refusal>>> = vec![None, None];
            let mut moved = false;
            for _ in 0..ROUNDS {
                for (answer, outcome) in answers.iter().zip(outcomes.iter_mut()) {
                    if outcome.is_none()
                        && let Ok(result) = answer.try_recv()
                    {
                        *outcome = Some(result.map(called));
                    }
                }
                if outcomes.iter().all(Option::is_some) {
                    break;
                }
                if position == 1 && !moved && outcomes.iter().any(Option::is_some) {
                    driven.transfer(&mut pump, &clients, leader, origin);
                    moved = true;
                }
                driven.round(&mut pump);
                if pump.captures_ready() {
                    pump.release_captures();
                    if position == 0 {
                        driven.transfer(&mut pump, &clients, leader, origin);
                        moved = true;
                    }
                }
            }
            let outcomes: Vec<_> = outcomes.into_iter().map(Option::unwrap).collect();
            for outcome in &outcomes {
                assert!(
                    matches!(outcome, Ok(v) if v == &vec![1])
                        || matches!(outcome, Err(Refusal::Invalid { what, .. }) if what == "prepared transaction")
                        || matches!(outcome, Err(Refusal::UnknownOutcome)),
                    "position {position}: {outcomes:?}"
                );
            }
            let oks = outcomes.iter().filter(|r| r.is_ok()).count();
            assert!(oks <= 1, "position {position}: {outcomes:?}");
            if position == 2 {
                let now = clients
                    .iter()
                    .map(|c| c.handle().status().unwrap().leader)
                    .next()
                    .unwrap();
                let current = usize::try_from(now - 1).unwrap();
                driven.transfer(&mut pump, &clients, current, (current + 1) % 3);
            }
            let rows = driven
                .call(&mut pump, clients.get(origin).unwrap(), |db| {
                    let mut rows = 0;
                    db.query(
                        "SELECT count(*) FROM actor_items WHERE value = x'01'",
                        [],
                        |row| {
                            rows = row.get::<i64>(0).map_err(unavailable)?;
                            Ok(false)
                        },
                    )?;
                    Ok(vec![u8::try_from(rows)?])
                })
                .unwrap();
            let rows = *rows.first().unwrap();
            assert!(
                rows <= 1,
                "position {position}: {rows} rows for {outcomes:?}"
            );
            if oks == 1 {
                assert_eq!(
                    rows, 1,
                    "position {position}: an Ok capture without its row, {outcomes:?}"
                );
            }
            driven.drain(&mut pump);
            pump.finish();
        }
    }

    /// The ticks of the leader between two answers of each follower whose answers are held: past
    /// the election timeout of 20 ticks, and short of two, so that a leader at fixed ticks
    /// survives it or not by where it falls (replication.md, "The leader's quorum patience").
    const HELD: usize = 30;

    /// Kiln goal 6b112f14, step 3: owner 0 is elected, then leads while both followers' answers
    /// are held for `HELD` of its ticks, from its count 22 to 52. Its checks at fixed ticks, at 20,
    /// 40 and 60, each find an answer (at 20 or 22, then at 52), so it keeps its lead and has
    /// measured that gap. The lead then moves away and back, and owner 0 leads a fresh term, its
    /// checks counted from 0 again. Returns its index and its followers'.
    fn measured_lead(driven: &mut Driven, pump: &mut Pump<'_>, clients: &[Client]) -> usize {
        let leader = driven.elect(pump, clients, 0, 1);
        let followers: Vec<usize> = (0..3).filter(|i| *i != leader).collect();
        for _ in 0..22 {
            driven.tick(pump, leader);
        }
        pump.held.clone_from(&followers);
        for _ in 0..HELD {
            driven.tick(pump, leader);
        }
        pump.held.clear();
        driven.quiet(pump);
        let id = u64::try_from(leader + 1).unwrap();
        assert!(
            clients
                .iter()
                .all(|c| c.handle().status().unwrap().leader == id),
            "the leader lost its lead while it measured its followers' answers"
        );
        let other = *followers.first().unwrap();
        driven.transfer(pump, clients, leader, other);
        driven.transfer(pump, clients, other, leader);
        driven.quiet(pump);
        leader
    }

    /// CP-T1.8, kiln goal 6b112f14 step 3 (replication.md, "The leader's quorum patience"): a
    /// follower's write held past the quorum window no longer deposes a sound leader. Do: the
    /// leader measures a gap of `HELD` ticks in its followers' answers (`measured_lead`), then, in
    /// a fresh term, both followers' answers are held for `HELD` of its ticks again, from its count
    /// 18 to 48, while a call it took is in flight. At fixed ticks the hold covers its whole
    /// window from 20 to 40 and its check at 40 steps it down. Expect: every owner still follows
    /// it, no term changed, and the call commits once the followers answer, its row on every
    /// replica.
    #[test]
    fn a_leader_whose_followers_answer_as_late_as_it_measured_keeps_its_lead() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = measured_lead(&mut driven, &mut pump, &clients);
        let id = u64::try_from(leader + 1).unwrap();
        let before = terms(&clients);
        for _ in 0..18 {
            driven.tick(&mut pump, leader);
        }
        pump.held = (0..3).filter(|i| *i != leader).collect();
        let call = submit(clients.get(leader).unwrap(), driven.until(), |db| {
            db.exec("CREATE TABLE held(value); INSERT INTO held VALUES(1)")?;
            Ok(vec![1])
        });
        for _ in 0..HELD {
            driven.tick(&mut pump, leader);
        }
        pump.held.clear();
        driven.quiet(&mut pump);
        let leaders: Vec<u64> = clients
            .iter()
            .map(|c| c.handle().status().unwrap().leader)
            .collect();
        assert!(
            leaders.iter().all(|l| *l == id),
            "the leader stepped down at its quorum check while its followers' answers were held \
             for {HELD} ticks: leaders {leaders:?}, terms {before:?} -> {:?}",
            terms(&clients)
        );
        assert_eq!(
            terms(&clients),
            before,
            "the term changed while the followers' answers were held"
        );
        assert_eq!(driven.settle(&mut pump, &call), Ok(vec![1]));
        // The acknowledged write is on every replica: read through each, after ReadIndex.
        for client in &clients {
            let rows = driven
                .call(&mut pump, client, |db| {
                    let mut rows = 0;
                    db.query("SELECT count(*) FROM held", [], |row| {
                        rows = row.get::<i64>(0).map_err(unavailable)?;
                        Ok(false)
                    })?;
                    Ok(vec![u8::try_from(rows)?])
                })
                .unwrap();
            assert_eq!(rows, vec![1], "an acknowledged write is missing");
        }
        assert_eq!(terms(&clients), before, "the term changed after the hold");
        driven.drain(&mut pump);
        pump.finish();
    }

    /// CP-T1.8, kiln goal 6b112f14 step 3, and AC-T1.8.M1 (replication.md, "The leader's quorum
    /// patience" and "Membership"): through a joint configuration the leader's patience covers the
    /// voters its quorum check counts. Do: the leader measures a gap of `HELD` ticks in its
    /// followers' answers (`measured_lead`); at its count 18 in a fresh term, it demotes one
    /// follower; from the moment the way out is appended, the other follower's answers are held
    /// for `HELD` ticks, to its count 48. The group's applied configuration stays joint, since only
    /// that follower can commit the way out, and the core's check counts the way out's voters, the
    /// leader and that follower, so no quorum answers through the window 20 to 40 and at fixed
    /// ticks its check at 40 steps it down. Expect: every owner still follows it, no term changed,
    /// the applied configuration joint all through the hold; then the change answers with the
    /// demoted member a learner.
    #[test]
    fn a_leader_in_a_joint_configuration_keeps_its_lead_through_the_late_halfs_tail() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = measured_lead(&mut driven, &mut pump, &clients);
        let id = u64::try_from(leader + 1).unwrap();
        let (late, demoted) = ((leader + 1) % 3, (leader + 2) % 3);
        let demoted_id = u64::try_from(demoted + 1).unwrap();
        let before = terms(&clients);
        for _ in 0..18 {
            driven.tick(&mut pump, leader);
        }
        let proposed = pending_change(&driven, leader);
        let until = driven.until() + Duration::from_secs(3600);
        let asked = change(
            clients.get(leader).unwrap(),
            until,
            MemberChange::Demote(demoted_id),
        );
        reach(&mut driven, &mut pump, "the joint entry", |driven| {
            pending_change(driven, leader) > proposed
        });
        let joint = pending_change(&driven, leader);
        reach(&mut driven, &mut pump, "the way out appended", |driven| {
            pending_change(driven, leader) > joint
        });
        // Only the late follower's answers can commit the way out, so the group stays joint.
        pump.held = vec![late];
        for _ in 0..HELD {
            driven.tick(&mut pump, leader);
            assert!(
                !applied_configuration(&driven, leader)
                    .voters_outgoing
                    .is_empty(),
                "the group left its joint configuration during the hold"
            );
        }
        pump.held.clear();
        driven.quiet(&mut pump);
        let leaders: Vec<u64> = clients
            .iter()
            .map(|c| c.handle().status().unwrap().leader)
            .collect();
        assert!(
            leaders.iter().all(|l| *l == id),
            "the leader stepped down in its joint configuration while the late half's answers \
             were held for {HELD} ticks: leaders {leaders:?}, terms {before:?} -> {:?}",
            terms(&clients)
        );
        assert_eq!(terms(&clients), before, "the term changed during the hold");
        let changed = driven.answer(&mut pump, &asked).unwrap();
        assert_eq!(changed.learners, vec![demoted_id]);
        assert!(changed.voters_outgoing.is_empty(), "{changed:?}");
        driven.drain(&mut pump);
        pump.finish();
    }

    /// CP-T1.8, kiln goals 6b112f14 and 867fa589 (replication.md, "The leader's quorum patience",
    /// its metrics; CLAUDE.md section 10): the patience and its refusals as the host reads them.
    /// Do: the leader measures a gap of `HELD` ticks in its followers' answers (`measured_lead`)
    /// and ticks on; then the host's detector suspects one follower, whose answers are held while
    /// the other's flow, then trusts it again, and its answers are let go (T-GN.1.1). Expect: the leader's `amphora_replica_quorum_patience_seconds` is its core's
    /// patience in ticks of its pace, its followers' zero, and nothing refused; then the leader still
    /// leads and has refused that one gap.
    #[test]
    fn the_quorum_patience_and_its_refusals_are_counted_for_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = measured_lead(&mut driven, &mut pump, &clients);
        let id = u64::try_from(leader + 1).unwrap();
        driven.tick(&mut pump, leader);
        let ticks = driven.owner(leader).replica.core().raft.quorum_patience();
        assert!(ticks > 0, "no patience after a measured gap");
        let counters = |owner: usize| clients.get(owner).unwrap().handle().counters();
        assert_eq!(
            counters(leader).quorum_patience,
            driven.period(leader) * u32::try_from(ticks).unwrap()
        );
        assert_eq!(counters(leader).quorum_patience_refused, 0);
        for follower in (0..3).filter(|i| *i != leader) {
            assert_eq!(counters(follower).quorum_patience, Duration::ZERO);
        }

        let late = (leader + 1) % 3;
        let late_id = u64::try_from(late + 1).unwrap();
        tell(&mut driven, &clients, leader, Verdict::Suspected(late_id)).unwrap();
        pump.held = vec![late];
        for _ in 0..HELD {
            driven.tick(&mut pump, leader);
        }
        tell(&mut driven, &clients, leader, Verdict::Trusted(late_id)).unwrap();
        pump.held.clear();
        driven.tick(&mut pump, leader);
        assert!(
            clients
                .iter()
                .all(|c| c.handle().status().unwrap().leader == id),
            "the leader lost its lead with one follower answering"
        );
        assert_eq!(counters(leader).quorum_patience_refused, 1);
        driven.drain(&mut pump);
        pump.finish();
    }

    /// CP-T1.8, kiln goal 6b112f14 step 3 (replication.md, "The leader's quorum patience"): the
    /// cost of the patience is bounded. Do: the leader measures a gap of `HELD` ticks
    /// (`measured_lead`), then, in a fresh term, is cut off from both followers, neither way
    /// relayed, just before its first check, and a read is asked of it. Expect: that check passes
    /// on the answers before the cut, the leader steps down within the election timeout and its
    /// patience after it, a patience within the ceiling, and the read is not answered while it
    /// is cut off: no lease answers a read.
    #[test]
    fn a_leader_cut_off_from_its_followers_steps_down_within_its_patience() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = measured_lead(&mut driven, &mut pump, &clients);
        let id = u64::try_from(leader + 1).unwrap();
        driven.tick(&mut pump, leader);
        let raft = &driven.owner(leader).replica.core().raft;
        let (election, patience) = (raft.config().election_tick, raft.quorum_patience());
        assert!(patience <= HELD + 1 - election, "{patience}");
        let due = election + patience;
        for _ in 1..due - 1 {
            driven.tick(&mut pump, leader);
        }
        pump.held = vec![0, 1, 2];
        // A read asked of the patient leader while it is cut off: no quorum confirms it.
        let read = submit(clients.get(leader).unwrap(), driven.until(), |_| {
            Ok(vec![1])
        });
        let mut after = None;
        for count in due - 1..=due + election + patience {
            if clients
                .get(leader)
                .unwrap()
                .handle()
                .status()
                .unwrap()
                .leader
                != id
            {
                after = Some(count.checked_sub(due));
                break;
            }
            driven.tick(&mut pump, leader);
        }
        assert!(
            matches!(read.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "a leader cut off from its followers answered a read"
        );
        let after = after
            .expect("the cut-off leader never stepped down")
            .expect("the cut-off leader stepped down before its first check");
        assert!(
            after <= election + patience,
            "stepped down {after} ticks after its first check, past {election} + {patience}"
        );
        pump.held.clear();
        driven.drain(&mut pump);
        pump.finish();
    }

    /// The numbers a text states: each maximal run of characters between spaces and punctuation
    /// that holds a digit and no letter, its ends' commas and periods stripped; a section's sign
    /// (`§3.6`) is a citation, not a number.
    fn numbers(text: &str) -> Vec<&str> {
        text.split(|c: char| c.is_whitespace() || "/|()[];:*`\"'?".contains(c))
            .map(|word| word.trim_matches(|c| c == '.' || c == ','))
            .filter(|word| {
                !word.starts_with('§')
                    && word.chars().any(|c| c.is_ascii_digit())
                    && !word.chars().any(char::is_alphabetic)
            })
            .collect()
    }

    /// Kiln goal 6b112f14 step 2, doc-truth: do read the decision "The leader's quorum patience"
    /// and step 1's record. Expect every number the decision states to be in the record, or to be
    /// one of the design's own, each as the code has it: the tick, the ceiling, the patience at
    /// the ceiling, a cut-off leader's cost, the window; else AC-T1.8.4's bound, a source's year
    /// or the decision's date.
    #[test]
    fn the_decisions_numbers_are_step_ones_or_its_own() {
        let design = include_str!("../../../../docs/design/replication.md");
        // The decision's own section, up to the next one.
        let decision = design
            .split_once("## The leader's quorum patience")
            .unwrap()
            .1
            .split("\n## ")
            .next()
            .unwrap();
        let record = numbers(include_str!("../../../../docs/research/owner-stalls.md"));
        let requirements = include_str!("../../../../docs/requirements.toml");
        let election = settings(1, &Durability::Persisted)
            .unwrap()
            .core
            .election_tick;
        let tick = TICK.as_millis();
        let own = [
            tick.to_string(),
            (2 * election).to_string(),
            (2 * u128::try_from(election).unwrap() * tick).to_string(),
        ];
        assert!(requirements.contains("failover p99 is at most 1000 ms"));
        let cited = ["1000", "2014", "2017", "2018", "2026-11-06", "2026-11-21"];
        let stray: Vec<&str> = numbers(decision)
            .into_iter()
            .filter(|n| !record.contains(n) && !own.iter().any(|o| o == n) && !cited.contains(n))
            .collect();
        assert!(
            stray.is_empty(),
            "numbers neither in step 1's record nor the design's own: {stray:?}"
        );
        assert!(
            numbers(decision).len() > 40,
            "the decision states its numbers"
        );
    }

    /// U15/CP-T1.8: malformed membership and a source that cannot lead are refused at admission.
    #[test]
    fn malformed_initial_membership_is_refused() {
        let valid = ReplicaConfig {
            node_id: 1,
            voters: vec![1, 2, 3],
            learners: Vec::new(),
            bootstrap: None,
        };
        assert!(validate(&valid).is_ok());
        for config in [
            ReplicaConfig {
                node_id: 0,
                ..valid.clone()
            },
            ReplicaConfig {
                voters: Vec::new(),
                ..valid.clone()
            },
            ReplicaConfig {
                voters: vec![1, 1],
                ..valid.clone()
            },
            ReplicaConfig {
                learners: vec![2],
                ..valid.clone()
            },
            ReplicaConfig {
                node_id: 9,
                ..valid.clone()
            },
            ReplicaConfig {
                learners: vec![4],
                bootstrap: Some(api::replication::Bootstrap {
                    source_node: 4,
                    source_id: [0; 32],
                }),
                ..valid.clone()
            },
        ] {
            assert!(matches!(validate(&config), Err(Refusal::Invalid { .. })));
        }
    }

    /// Enqueues a change of members at `client` without waiting: its answer comes on the returned
    /// receiver.
    fn change(
        client: &Client,
        until: Instant,
        change: MemberChange,
    ) -> mpsc::Receiver<Result<ReplicaStatus, Refusal>> {
        let (response, receiver) = mpsc::sync_channel(1);
        client
            .handle
            .enqueue(
                Input::Change {
                    change,
                    until,
                    response,
                },
                CHANGE_BYTES,
            )
            .unwrap();
        receiver
    }
    /// Enqueues a hand-off of the lead to `to` at `client` without waiting: its answer comes on
    /// the returned receiver.
    fn transfer(
        client: &Client,
        until: Instant,
        to: u64,
    ) -> mpsc::Receiver<Result<ReplicaStatus, Refusal>> {
        let (response, receiver) = mpsc::sync_channel(1);
        client
            .handle
            .enqueue(
                Input::Transfer {
                    to,
                    until,
                    response,
                },
                CHANGE_BYTES,
            )
            .unwrap();
        receiver
    }
    /// Writes `value` into member_items through `client`.
    fn write(driven: &mut Driven, pump: &mut Pump<'_>, client: &Client, value: u8) {
        driven
            .call(pump, client, move |db| {
                db.exec("CREATE TABLE IF NOT EXISTS member_items(value)")?;
                db.exec(&format!("INSERT INTO member_items VALUES({value})"))?;
                Ok(Vec::new())
            })
            .unwrap();
    }
    /// The values of member_items that owner `owner` has applied, read on its own state.
    fn held(driven: &mut Driven, owner: usize) -> Vec<u8> {
        let actor = driven.owner_mut(owner);
        actor
            .replica
            .machine_mut()
            .read_db(|db| {
                let mut tables = 0;
                db.query(
                    "SELECT count(*) FROM sqlite_schema WHERE name='member_items'",
                    [],
                    |row| {
                        tables = row.get::<i64>(0).map_err(unavailable)?;
                        Ok(false)
                    },
                )?;
                let mut values = Vec::new();
                if tables == 1 {
                    db.query("SELECT value FROM member_items ORDER BY value", [], |row| {
                        values.push(u8::try_from(row.get::<i64>(0).map_err(unavailable)?)?);
                        Ok(true)
                    })?;
                }
                Ok(values)
            })
            .unwrap()
    }
    /// Rounds, time standing still, until owner `owner` has applied all that owner `leader` has.
    fn catch_up(
        driven: &mut Driven,
        pump: &mut Pump<'_>,
        clients: &[Client],
        leader: usize,
        owner: usize,
    ) {
        let applied = |n: usize| clients[n].handle().status().unwrap().applied;
        for _ in 0..ROUNDS {
            if applied(owner) >= applied(leader) {
                return;
            }
            driven.round(pump);
        }
        panic!("owner {owner} did not catch up in {ROUNDS} driven rounds");
    }
    /// Rounds, time standing still, until a round relays nothing and the step after it gives
    /// nothing to relay: what the owners had to send is sent.
    fn quiet(driven: &mut Driven, pump: &mut Pump<'_>) {
        for _ in 0..ROUNDS {
            let relayed = pump.drive_until(driven.until());
            driven.step();
            if relayed == 0 && pump.drive_until(driven.until()) == 0 {
                return;
            }
        }
        panic!("the group did not go quiet in {ROUNDS} driven rounds");
    }
    /// Asks `client` where catching up each learner stands, without waiting: its answer comes on
    /// the returned receiver.
    fn judged(
        client: &Client,
        until: Instant,
    ) -> mpsc::Receiver<Result<Vec<(u64, CatchUp)>, Refusal>> {
        let (response, receiver) = mpsc::sync_channel(1);
        client
            .handle
            .enqueue(Input::CatchUp { until, response }, CHANGE_BYTES)
            .unwrap();
        receiver
    }
    /// The ticks owner 0's core waits, at least, before it campaigns: an election timeout.
    fn leader_core_election_tick(driven: &Driven) -> usize {
        driven.owner(0).replica.core().raft.config().election_tick
    }

    /// AC-T1.8.M1 (CP-T1.8) on driven time (replication.md, "Membership"; kiln plan 01a1a283e14c,
    /// its docs/p2p.md P3): the leader says how far each learner has caught up, judged in rounds
    /// (Ongaro's thesis section 4.2.1), for its host to promote only a learner that won't stall
    /// commits. Do: ask the leader and a member to be added before the add; add the member as a
    /// learner and ask the leader as rounds go; ask the learner; promote it and ask the leader
    /// again. Expect: no learner to judge, and NotLeader naming no leader from the member; the
    /// learner Pending, then Ready within the rounds, no time passing; NotLeader naming the leader
    /// from the learner; once it votes, no learner to judge.
    #[test]
    fn the_leader_judges_a_learners_catch_up_in_rounds() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = vec![
            driven.open(dir.path(), 1, vec![1], None),
            driven.open_member(dir.path(), 2, vec![1], vec![2], None),
        ];
        let mut pump = Pump::new(&clients);
        let (voter, learner) = (&clients[0], &clients[1]);
        driven.elect(&mut pump, &clients[..1], 0, 1);
        write(&mut driven, &mut pump, voter, 1);
        assert_eq!(
            driven.answer(&mut pump, &judged(voter, driven.until())),
            Ok(Vec::new())
        );
        assert_eq!(
            driven.answer(&mut pump, &judged(learner, driven.until())),
            Err(Refusal::NotLeader { leader: None })
        );

        let until = driven.until();
        driven
            .answer(
                &mut pump,
                &change(voter, until, MemberChange::AddLearner(2)),
            )
            .unwrap();
        write(&mut driven, &mut pump, voter, 2);
        let mut stands = Vec::new();
        for _ in 0..ROUNDS {
            stands = driven
                .answer(&mut pump, &judged(voter, driven.until()))
                .unwrap();
            if stands == vec![(2, CatchUp::Ready)] {
                break;
            }
            assert_eq!(stands, vec![(2, CatchUp::Pending)]);
            driven.round(&mut pump);
        }
        assert_eq!(stands, vec![(2, CatchUp::Ready)]);
        catch_up(&mut driven, &mut pump, &clients, 0, 1);
        assert_eq!(held(&mut driven, 1), vec![1, 2]);
        assert_eq!(
            driven.answer(&mut pump, &judged(learner, driven.until())),
            Err(Refusal::NotLeader { leader: Some(1) })
        );

        let until = driven.until();
        let promoted = driven
            .answer(&mut pump, &change(voter, until, MemberChange::Promote(2)))
            .unwrap();
        assert_eq!((promoted.voters, promoted.learners), (vec![1, 2], vec![]));
        assert_eq!(
            driven.answer(&mut pump, &judged(voter, driven.until())),
            Ok(Vec::new())
        );
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.M1 (CP-T1.8) on driven time (replication.md, "Membership"; kiln plan 01a1a283e14c,
    /// its docs/p2p.md P3): a learner that never answers is given up once its lag hasn't shrunk
    /// over an election timeout, and staged afresh when asked again (Ongaro's thesis section
    /// 4.2.1: the leader aborts for a server that "will never catch up", and "the caller may always
    /// try again"). Do, in a group of one voter: add member 9, which has no owner, so what the
    /// leader sends it is lost; ask where it stands with a deadline already past; let an election
    /// timeout pass on the leader's clock and ask; let another pass and ask, then once more.
    /// Expect: Timeout, staging nothing; Pending; Aborted; Pending.
    #[test]
    fn a_learner_that_never_answers_is_given_up_after_an_election_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = vec![driven.open(dir.path(), 1, vec![1], None)];
        let mut pump = Pump::new(&clients);
        driven.elect(&mut pump, &clients, 0, 1);
        write(&mut driven, &mut pump, &clients[0], 1);
        driven.drain(&mut pump);
        pump.finish();
        // Rounds without the pump, what the leader sends lost, until `answer` comes. Each round
        // yields `POLL` to the disk, as a round with nothing to relay does.
        fn lost<T>(
            driven: &mut Driven,
            client: &Client,
            answer: &mpsc::Receiver<Result<T, Refusal>>,
        ) -> Result<T, Refusal> {
            for _ in 0..ROUNDS {
                match answer.try_recv() {
                    Ok(result) => return result,
                    Err(mpsc::TryRecvError::Empty) => {
                        drop(client.handle().drain(OUTGOING_MESSAGES).unwrap());
                        std::thread::park_timeout(POLL);
                        driven.step();
                    }
                    Err(error) => panic!("owner ended: {error}"),
                }
            }
            panic!("a driven operation was unanswered after {ROUNDS} rounds");
        }
        let leader = &clients[0];
        let until = driven.until();
        let added = lost(
            &mut driven,
            leader,
            &change(leader, until, MemberChange::AddLearner(9)),
        )
        .unwrap();
        assert_eq!((added.voters, added.learners), (vec![1], vec![9]));
        // An election timeout on ticks: the core's election_tick, each tick a step of its own pace.
        let election = |driven: &mut Driven| {
            for _ in 0..=leader_core_election_tick(driven) {
                driven.advance_tick(0);
                drop(leader.handle().drain(OUTGOING_MESSAGES).unwrap());
                driven.step();
            }
        };
        // A request whose deadline passed before the owner took it stages nothing: an election
        // timeout later, the learner is staged afresh, not given up.
        let stale = Instant::now();
        assert_eq!(
            lost(&mut driven, leader, &judged(leader, stale)),
            Err(Refusal::Timeout)
        );
        election(&mut driven);
        assert_eq!(
            {
                let until = driven.until();
                lost(&mut driven, leader, &judged(leader, until))
            },
            Ok(vec![(9, CatchUp::Pending)])
        );
        election(&mut driven);
        assert_eq!(
            {
                let until = driven.until();
                lost(&mut driven, leader, &judged(leader, until))
            },
            Ok(vec![(9, CatchUp::Aborted)])
        );
        assert_eq!(
            {
                let until = driven.until();
                lost(&mut driven, leader, &judged(leader, until))
            },
            Ok(vec![(9, CatchUp::Pending)])
        );
    }

    /// AC-T1.8.M1, T-T1.8.M1 (CP-T1.8) on driven time (replication.md, "Membership"; kiln plan
    /// 01a1a283e14c, step 1): a group of one voter adds a learner, writes before and after, then
    /// removes it. Do: write 1 on the voter; add member 2 as a learner; write 2 on the voter and 3
    /// on the learner; remove the learner; write 4 on the voter; then write 5 on the removed
    /// member. Expect: each change answered with the configuration it made, two entries on (into
    /// the joint configuration and out of it); 1, 2 and 3 on both, within 5 s of every clock; from
    /// the removal on, no record sent to the removed member and 4 on the voter alone; its own write
    /// 5 never applied there by itself, Timeout.
    #[test]
    fn a_learner_added_has_every_write_and_once_removed_receives_none() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = vec![
            driven.open(dir.path(), 1, vec![1], None),
            // A member to be added opens as the learner it is to be.
            driven.open_member(dir.path(), 2, vec![1], vec![2], None),
        ];
        let mut pump = Pump::new(&clients);
        let (voter, learner) = (&clients[0], &clients[1]);
        driven.elect(&mut pump, &clients[..1], 0, 1);
        let elected = terms(&clients[..1]);
        write(&mut driven, &mut pump, voter, 1);
        let before = voter.handle().status().unwrap();
        assert_eq!((before.voters.clone(), before.learners), (vec![1], vec![]));
        assert_eq!(held(&mut driven, 1), Vec::<u8>::new());

        let until = driven.until();
        let added = driven
            .answer(
                &mut pump,
                &change(voter, until, MemberChange::AddLearner(2)),
            )
            .unwrap();
        assert_eq!((added.voters, added.learners), (vec![1], vec![2]));
        assert_eq!(added.applied, before.applied + 2, "not through two phases");
        write(&mut driven, &mut pump, voter, 2);
        // The learner's own write reaches the leader.
        write(&mut driven, &mut pump, learner, 3);
        catch_up(&mut driven, &mut pump, &clients, 0, 1);
        assert_eq!(held(&mut driven, 0), vec![1, 2, 3]);
        assert_eq!(held(&mut driven, 1), vec![1, 2, 3]);
        // The writes reach the learner within 5 s: here, before any clock moves at all.
        assert!(
            driven
                .clocks
                .iter()
                .all(|clock| offset(clock) < Duration::from_secs(5))
        );

        pump.departed.push(2);
        let until = driven.until();
        let removed = driven
            .answer(&mut pump, &change(voter, until, MemberChange::Remove(2)))
            .unwrap();
        assert_eq!((removed.voters, removed.learners), (vec![1], vec![]));
        // What the leader sent before the removal is delivered; nothing after it is.
        quiet(&mut driven, &mut pump);
        let sent = pump.delivered.get(&2).copied().unwrap_or_default();
        assert!(sent > 0);
        write(&mut driven, &mut pump, voter, 4);
        quiet(&mut driven, &mut pump);
        assert_eq!(pump.delivered.get(&2).copied().unwrap_or_default(), sent);
        assert_eq!(held(&mut driven, 0), vec![1, 2, 3, 4]);
        assert_eq!(held(&mut driven, 1), vec![1, 2, 3]);

        // The removed member's own write: the leader refuses what it sends, and it never applies
        // the write by itself.
        let until = driven.until();
        let answer = submit(learner, until, |db| {
            db.exec("INSERT INTO member_items VALUES(5)")?;
            Ok(Vec::new())
        });
        quiet(&mut driven, &mut pump);
        assert!(matches!(answer.try_recv(), Err(mpsc::TryRecvError::Empty)));
        set(&driven.clock(1), driven.base, until);
        assert_eq!(driven.settle(&mut pump, &answer), Err(Refusal::Timeout));
        assert_eq!(held(&mut driven, 1), vec![1, 2, 3]);
        assert_eq!(held(&mut driven, 0), vec![1, 2, 3, 4]);
        assert_eq!(pump.delivered.get(&2).copied().unwrap_or_default(), sent);
        assert_eq!(terms(&clients[..1]), elected, "an election happened");
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.M1, T-T1.8.M3 (CP-T1.8) on driven time (replication.md, "Membership"): a learner is
    /// promoted to voter and demoted again, each through the joint configuration. Do: add member 2
    /// as a learner, promote it, write, demote it, write. Expect: each change answered with the
    /// configuration it made, two entries on; while 2 votes, the write commits through it; once
    /// demoted it still has every write, and no election happens.
    #[test]
    fn a_learner_is_promoted_and_demoted_through_the_joint_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = vec![
            driven.open(dir.path(), 1, vec![1], None),
            driven.open_member(dir.path(), 2, vec![1], vec![2], None),
        ];
        let mut pump = Pump::new(&clients);
        let voter = &clients[0];
        driven.elect(&mut pump, &clients[..1], 0, 1);
        let elected = terms(&clients[..1]);
        write(&mut driven, &mut pump, voter, 1);
        let mut configurations = Vec::new();
        let mut applied = voter.handle().status().unwrap().applied;
        for (made, then) in [
            (MemberChange::AddLearner(2), 2),
            (MemberChange::Promote(2), 3),
            (MemberChange::Demote(2), 4),
        ] {
            let until = driven.until();
            let status = driven
                .answer(&mut pump, &change(voter, until, made))
                .unwrap();
            assert_eq!(
                status.applied,
                applied + 2,
                "{made:?} not through two phases"
            );
            configurations.push((status.voters, status.learners));
            write(&mut driven, &mut pump, voter, then);
            applied = voter.handle().status().unwrap().applied;
        }
        assert_eq!(
            configurations,
            vec![(vec![1], vec![2]), (vec![1, 2], vec![]), (vec![1], vec![2])]
        );
        catch_up(&mut driven, &mut pump, &clients, 0, 1);
        assert_eq!(held(&mut driven, 0), vec![1, 2, 3, 4]);
        assert_eq!(held(&mut driven, 1), vec![1, 2, 3, 4]);
        assert_eq!(terms(&clients), vec![elected[0]; 2], "an election happened");
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.M1, T-T1.8.M2 (CP-T1.8) on driven time (replication.md, "Membership"): only the
    /// leader changes members, one change at a time, and only as the configuration can take it. Do,
    /// in a group of three voters: ask a follower; ask the leader for two changes at once; promote
    /// a non-member, add a voter as a learner, demote and remove the leader; add a learner to a
    /// group of the most members. Expect: NotLeader naming the leader; the first of the two
    /// answered and the second Unavailable; Invalid for each the configuration can't take and
    /// TooLarge past the members, each proposing nothing; a change the configuration already shows
    /// answered with it.
    #[test]
    fn member_changes_are_refused_off_the_leader_at_once_or_when_impossible() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        let id = u64::try_from(leader + 1).unwrap();
        let led = &clients[leader];
        let before = led.handle().status().unwrap();
        let until = driven.until();
        let asked = change(
            &clients[(leader + 1) % 3],
            until,
            MemberChange::AddLearner(4),
        );
        assert_eq!(
            driven.answer(&mut pump, &asked),
            Err(Refusal::NotLeader { leader: Some(id) })
        );
        // A removal of no member: shown already, it proposes nothing.
        let first = change(led, until, MemberChange::Remove(9));
        let second = change(led, until, MemberChange::Remove(8));
        assert!(matches!(
            driven.answer(&mut pump, &second),
            Err(Refusal::Unavailable(why)) if why.contains("under way")
        ));
        let shown = driven.answer(&mut pump, &first).unwrap();
        assert_eq!((shown.voters, shown.learners), (vec![1, 2, 3], vec![]));
        for impossible in [
            MemberChange::Promote(9),
            MemberChange::AddLearner((id % 3) + 1),
            MemberChange::Demote(id),
            MemberChange::Remove(id),
        ] {
            assert!(
                matches!(
                    driven.answer(&mut pump, &change(led, until, impossible)),
                    Err(Refusal::Invalid { what, .. }) if what == "member change"
                ),
                "{impossible:?}"
            );
        }
        assert_eq!(led.handle().status().unwrap().applied, before.applied);
        driven.drain(&mut pump);
        pump.finish();

        // A group of the most members: one voter and 63 learners, none of them running, so no
        // relay; time stands still.
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let full = driven.open_member(dir.path(), 1, vec![1], (2..=64).collect(), None);
        driven.owner_mut(0).replica.campaign().unwrap();
        // Its lead and the change's barrier each wait on its disk: each step yields to it, and
        // the change is asked only once the voter leads.
        let led = (0..ROUNDS).any(|_| {
            driven.step();
            std::thread::park_timeout(POLL);
            full.handle().status().unwrap().leader == 1
        });
        assert!(led, "the voter did not lead in {ROUNDS} driven steps");
        let asked = change(&full, driven.until(), MemberChange::AddLearner(65));
        let refused = (0..ROUNDS)
            .find_map(|_| {
                driven.step();
                std::thread::park_timeout(POLL);
                asked.try_recv().ok()
            })
            .unwrap();
        assert_eq!(
            refused,
            Err(Refusal::TooLarge {
                what: "replica members".into(),
                size: 65,
                limit: MEMBERS,
            })
        );
        let status = full.handle().status().unwrap();
        assert_eq!((status.voters.len(), status.learners.len()), (1, 63));
    }
    /// AC-T1.8.M1, T-T1.8.M3 (CP-T1.8) on driven time (replication.md, "Membership"): the members a
    /// group changed to are what it reopens with, whatever its config first named. Do: add member 2
    /// as a learner, write, restart both from the configs they first opened with, write again.
    /// Expect: the configuration reopened names the learner, and it has both writes.
    #[test]
    fn a_changed_membership_holds_through_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = vec![
            driven.open(dir.path(), 1, vec![1], None),
            driven.open_member(dir.path(), 2, vec![1], vec![2], None),
        ];
        let mut pump = Pump::new(&clients);
        driven.elect(&mut pump, &clients[..1], 0, 1);
        let until = driven.until();
        driven
            .answer(
                &mut pump,
                &change(&clients[0], until, MemberChange::AddLearner(2)),
            )
            .unwrap();
        write(&mut driven, &mut pump, &clients[0], 1);
        catch_up(&mut driven, &mut pump, &clients, 0, 1);
        for owner in 0..2 {
            driven.advance(owner, Duration::from_millis(60));
        }
        for _ in 0..8 {
            driven.round(&mut pump);
        }
        driven.drain(&mut pump);
        pump.finish();
        drop(clients);
        drop(driven);

        let mut driven = Driven::new();
        let reopened = vec![
            driven.open(dir.path(), 1, vec![1], None),
            driven.open_member(dir.path(), 2, vec![1], vec![2], None),
        ];
        let mut pump = Pump::new(&reopened);
        driven.elect(&mut pump, &reopened[..1], 0, 1);
        let status = reopened[0].handle().status().unwrap();
        assert_eq!((status.voters, status.learners), (vec![1], vec![2]));
        write(&mut driven, &mut pump, &reopened[0], 2);
        catch_up(&mut driven, &mut pump, &reopened, 0, 1);
        assert_eq!(held(&mut driven, 1), vec![1, 2]);
        driven.drain(&mut pump);
        pump.finish();
    }

    /// Rounds, time standing still, until `reached` holds of the owners.
    fn reach(
        driven: &mut Driven,
        pump: &mut Pump<'_>,
        what: &str,
        reached: impl Fn(&Driven) -> bool,
    ) {
        for _ in 0..ROUNDS {
            if reached(driven) {
                return;
            }
            driven.round(pump);
        }
        panic!("{what} not reached in {ROUNDS} driven rounds");
    }
    /// The configuration owner `owner` has applied.
    fn applied_configuration(driven: &Driven, owner: usize) -> ConfState {
        driven.owner(owner).replica.configuration().clone()
    }
    /// The change the core of owner `owner` last took or wrote.
    fn pending_change(driven: &Driven, owner: usize) -> u64 {
        driven.owner(owner).replica.core().raft.pending_conf_index()
    }
    /// Rounds until every one of `voters` has applied the group out of any joint configuration,
    /// with `learners` its learners and nothing of a change still to apply.
    fn settled(driven: &mut Driven, pump: &mut Pump<'_>, voters: usize, learners: &[u64]) {
        reach(driven, pump, "the group settled", |driven| {
            (0..voters).all(|owner| {
                let applied = applied_configuration(driven, owner);
                applied.voters_outgoing.is_empty()
                    && applied.learners_next.is_empty()
                    && applied.learners == learners
                    && !driven.owner(owner).replica.core().raft.has_pending_conf()
            })
        });
    }

    /// AC-T1.8.M1, T-T1.8.M3 (CP-T1.8) on driven time (replication.md, "Membership"): a change past
    /// its deadline answers what its stage knows. In a group of three voters, the relay held so
    /// that nothing moves on: Do: a change's deadline passes at its barrier; then, relayed to its
    /// proposal, another's; then, relayed until the joint configuration it made is applied with its
    /// way out held from committing, a third's. Expect: Timeout, with nothing proposed;
    /// UnknownOutcome, and with the relay back it applies all the same, as reading the
    /// configuration shows; while the joint configuration is applied, the status names its outgoing
    /// voters (`voters_outgoing`), and the deadline answers the status applied; with the relay back
    /// and the leader's clock moving, the group leaves it.
    #[test]
    fn a_change_past_its_deadline_answers_what_its_stage_knows() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let mut clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        clients.push(driven.open_member(dir.path(), 4, vec![1, 2, 3], vec![4], None));
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients[..3], 0, 1);
        let led = &clients[leader];
        let clock = driven.clock(leader);
        // A deadline a second past the leader's clock, inside the relay's budget for the records it
        // carries, so the clock reaching it expires the change alone.
        let deadline =
            |clock: &AtomicU64, base: Instant| base + offset(clock) + Duration::from_secs(1);

        let proposed = pending_change(&driven, leader);
        let until = deadline(&clock, driven.base);
        let asked = change(led, until, MemberChange::AddLearner(4));
        driven.step();
        assert!(matches!(asked.try_recv(), Err(mpsc::TryRecvError::Empty)));
        set(&clock, driven.base, until);
        driven.step();
        assert_eq!(asked.try_recv().unwrap(), Err(Refusal::Timeout));
        for _ in 0..8 {
            driven.round(&mut pump);
        }
        assert_eq!(pending_change(&driven, leader), proposed);
        assert_eq!(
            applied_configuration(&driven, leader).learners,
            Vec::<u64>::new()
        );

        let until = deadline(&clock, driven.base);
        let asked = change(led, until, MemberChange::AddLearner(4));
        reach(&mut driven, &mut pump, "the proposal", |driven| {
            pending_change(driven, leader) > proposed
        });
        assert!(matches!(asked.try_recv(), Err(mpsc::TryRecvError::Empty)));
        set(&clock, driven.base, until);
        driven.step();
        assert_eq!(asked.try_recv().unwrap(), Err(Refusal::UnknownOutcome));
        settled(&mut driven, &mut pump, 3, &[4]);
        assert_eq!(led.handle().status().unwrap().learners, vec![4]);

        pump.departed.push(4);
        let configured = pending_change(&driven, leader);
        let until = deadline(&clock, driven.base);
        let asked = change(led, until, MemberChange::Remove(4));
        reach(&mut driven, &mut pump, "the joint proposal", |driven| {
            pending_change(driven, leader) > configured
        });
        // The way out is proposed once the joint configuration applies, and can commit in the same
        // round, so the joint one would never be seen applied: no answer that a member holds past
        // the joint entry is heard until the deadline has answered (a heartbeat's answer saying
        // where a log ends counts as an append's).
        let joint_at = pending_change(&driven, leader);
        pump.losing = Some(Box::new(move |_, record| {
            matches!(
                record.msg_type,
                hyper_raft::proto::MessageType::MsgAppendResponse
                    | hyper_raft::proto::MessageType::MsgHeartbeatResponse
            ) && record.index > joint_at
        }));
        reach(
            &mut driven,
            &mut pump,
            "the joint configuration",
            |driven| {
                !applied_configuration(driven, leader)
                    .voters_outgoing
                    .is_empty()
            },
        );
        let joint = led.handle().status().unwrap();
        assert_eq!(
            (joint.voters_outgoing, joint.learners),
            (vec![1, 2, 3], vec![])
        );
        assert!(matches!(asked.try_recv(), Err(mpsc::TryRecvError::Empty)));
        set(&clock, driven.base, until);
        driven.step();
        // The status applied, still joint: its way out cannot have committed.
        let answered = asked.try_recv().unwrap().unwrap();
        assert_eq!(
            (answered.voters_outgoing, answered.learners),
            (vec![1, 2, 3], vec![])
        );
        // What was lost is made good by the leader's next heartbeat, which only its ticks send.
        pump.losing = None;
        for _ in 0..ROUNDS {
            if applied_configuration(&driven, leader)
                .voters_outgoing
                .is_empty()
            {
                break;
            }
            driven.tick(&mut pump, leader);
        }
        settled(&mut driven, &mut pump, 3, &[]);
        write(&mut driven, &mut pump, led, 1);
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.M1, T-T1.8.M3 (CP-T1.8) on driven time (replication.md, "Membership"): a change at
    /// its barrier when its lead is lost is refused, with nothing proposed. Do: in a group of three
    /// voters, the relay held, ask the leader for a change, then move its clock until its quorum
    /// check finds no quorum. Expect: NotLeader, naming no leader, and no member changed.
    #[test]
    fn a_change_at_its_barrier_is_refused_once_the_lead_is_lost() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        driven.drain(&mut pump);
        let proposed = pending_change(&driven, leader);
        let asked = change(
            &clients[leader],
            driven.until(),
            MemberChange::AddLearner(4),
        );
        driven.step();
        assert!(matches!(asked.try_recv(), Err(mpsc::TryRecvError::Empty)));
        let refused = (0..ROUNDS)
            .find_map(|_| {
                driven.advance_tick(leader);
                driven.step();
                asked.try_recv().ok()
            })
            .unwrap();
        assert_eq!(refused, Err(Refusal::NotLeader { leader: None }));
        assert_eq!(pending_change(&driven, leader), proposed);
        assert_eq!(
            applied_configuration(&driven, leader).learners,
            Vec::<u64>::new()
        );
        pump.finish();
    }

    /// AC-T1.8.M1, T-T1.8.M3 (CP-T1.8) on driven time (replication.md, "Membership"): a change
    /// proposed and then lost is never answered as done, though the index it held is applied. Do:
    /// in a group of three voters, the leader proposes a change whose records are lost; the other
    /// two, their clocks past an election timeout, elect one of them, whose entry takes that index.
    /// Expect: UnknownOutcome, no member changed anywhere, and the new leader takes the change when
    /// asked again.
    #[test]
    fn a_change_another_lead_wrote_over_is_never_answered_done() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let mut clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        clients.push(driven.open_member(dir.path(), 4, vec![1, 2, 3], vec![4], None));
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients[..3], 0, 1);
        driven.drain(&mut pump);
        let term = clients[leader].handle().status().unwrap().term;
        let proposed = pending_change(&driven, leader);
        let until = driven.until();
        let asked = change(&clients[leader], until, MemberChange::AddLearner(4));
        reach(&mut driven, &mut pump, "the proposal", |driven| {
            pending_change(driven, leader) > proposed
        });
        // What goes on is what the proposal's round hadn't sent: its entry is lost with this.
        clients[leader].handle().drain(OUTGOING_MESSAGES).unwrap();
        let others: Vec<usize> = (0..3).filter(|owner| *owner != leader).collect();
        let answered = (0..ROUNDS)
            .find_map(|_| {
                for owner in &others {
                    driven.advance_tick(*owner);
                }
                driven.round(&mut pump);
                asked.try_recv().ok()
            })
            .unwrap();
        assert_eq!(answered, Err(Refusal::UnknownOutcome));
        settled(&mut driven, &mut pump, 3, &[]);
        let status = clients[others[0]].handle().status().unwrap();
        assert!(status.term > term && status.leader != 0, "{status:?}");
        let led = usize::try_from(status.leader - 1).unwrap();
        // The index the change held is applied, as another entry.
        reach(&mut driven, &mut pump, "the index applied", |driven| {
            (0..3).all(|owner| driven.owner(owner).replica.applied().index > proposed)
        });
        settled(&mut driven, &mut pump, 3, &[]);
        let until = driven.until();
        let asked = change(&clients[led], until, MemberChange::AddLearner(4));
        assert_eq!(driven.answer(&mut pump, &asked).unwrap().learners, vec![4]);
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.M1, T-T1.8.M3 (CP-T1.8) under a nemesis (replication.md, "Membership"): a change
    /// holds through an election at each of its stages. Do, in a group of three voters: the lead
    /// moves as the change passes its barrier, once it is proposed, and once its joint
    /// configuration is applied. Expect: at the barrier, a refusal saying to ask again, nothing
    /// changed; once proposed, Ok or UnknownOutcome, and the new leader carries it through; once
    /// applied, Ok. Each time the group leaves the joint configuration by itself, the new leader
    /// takes the change (asked again, shown already), and a write reaches the learner.
    #[test]
    fn a_change_holds_through_an_election_at_each_stage() {
        for stage in 0..3 {
            let dir = tempfile::tempdir().unwrap();
            let mut driven = Driven::new();
            let mut clients: Vec<Client> = (1..=3)
                .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
                .collect();
            clients.push(driven.open_member(dir.path(), 4, vec![1, 2, 3], vec![4], None));
            let mut pump = Pump::new(&clients);
            pump.refusals = Some(0);
            let leader = driven.elect(&mut pump, &clients[..3], 0, 1);
            let proposed = pending_change(&driven, leader);
            let until = driven.until();
            let asked = change(&clients[leader], until, MemberChange::AddLearner(4));
            match stage {
                0 => driven.step(),
                1 => reach(&mut driven, &mut pump, "the proposal", |driven| {
                    pending_change(driven, leader) > proposed
                }),
                _ => reach(&mut driven, &mut pump, "the change applied", |driven| {
                    applied_configuration(driven, leader).learners == [4]
                }),
            }
            let led = driven.hand_off(&mut pump, &clients[..3], leader, (leader + 1) % 3);
            let outcome = driven.answer(&mut pump, &asked);
            match stage {
                0 => {
                    assert!(
                        matches!(
                            &outcome,
                            Err(Refusal::NotLeader { .. } | Refusal::Unavailable(_))
                        ),
                        "stage {stage}: {outcome:?}"
                    );
                    settled(&mut driven, &mut pump, 3, &[]);
                }
                1 => {
                    assert!(
                        matches!(&outcome, Ok(_) | Err(Refusal::UnknownOutcome)),
                        "stage {stage}: {outcome:?}"
                    );
                    settled(&mut driven, &mut pump, 3, &[4]);
                }
                _ => {
                    assert_eq!(outcome.unwrap().learners, vec![4], "stage {stage}");
                    settled(&mut driven, &mut pump, 3, &[4]);
                }
            }
            let until = driven.until();
            let status = driven
                .answer(
                    &mut pump,
                    &change(&clients[led], until, MemberChange::AddLearner(4)),
                )
                .unwrap();
            assert_eq!(
                (status.leader, status.voters, status.learners),
                (u64::try_from(led + 1).unwrap(), vec![1, 2, 3], vec![4]),
                "stage {stage}"
            );
            write(&mut driven, &mut pump, &clients[led], 1);
            catch_up(&mut driven, &mut pump, &clients, led, 3);
            assert_eq!(held(&mut driven, 3), vec![1], "stage {stage}");
            driven.drain(&mut pump);
            pump.finish();
        }
    }

    /// AC-T1.8.M2, T-T1.8.M4 (CP-T1.8) on driven time (replication.md, "Membership"): the leader is
    /// removed once it has handed on its lead. Do, in a group of three voters: ask a follower to
    /// hand on the lead; ask the leader to hand it to a non-member and to itself, and to remove
    /// itself; then the leader hands it to a follower, the new leader removes the old one, and
    /// writes. Expect: NotLeader naming the leader; Invalid; the status at once, in the same term;
    /// Invalid, saying to hand the lead on first; then the follower leading, in a later term; the
    /// removal answered with the other two voters; the write on both of them, none on the removed
    /// member, and no record delivered to it from the removal's answer on.
    #[test]
    fn the_leader_is_removed_once_it_has_handed_on_its_lead() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        let (heir, other) = ((leader + 1) % 3, (leader + 2) % 3);
        let id = |owner: usize| u64::try_from(owner + 1).unwrap();
        let before = clients[leader].handle().status().unwrap();
        let until = driven.until();
        assert_eq!(
            driven.answer(&mut pump, &transfer(&clients[heir], until, id(heir))),
            Err(Refusal::NotLeader {
                leader: Some(id(leader))
            })
        );
        assert!(matches!(
            driven.answer(&mut pump, &transfer(&clients[leader], until, 9)),
            Err(Refusal::Invalid { what, .. }) if what == "lead transfer"
        ));
        let stayed = driven
            .answer(&mut pump, &transfer(&clients[leader], until, id(leader)))
            .unwrap();
        assert_eq!((stayed.leader, stayed.term), (id(leader), before.term));
        assert!(matches!(
            driven.answer(
                &mut pump,
                &change(&clients[leader], until, MemberChange::Remove(id(leader)))
            ),
            Err(Refusal::Invalid { why, .. }) if why.contains("transfer")
        ));

        let handed = driven
            .answer(&mut pump, &transfer(&clients[leader], until, id(heir)))
            .unwrap();
        assert_eq!(handed.leader, id(heir));
        assert!(handed.term > before.term, "{handed:?}");
        assert_eq!(driven.follow(&mut pump, &clients, 1), heir);

        pump.departed.push(id(leader));
        let until = driven.until();
        let removed = driven
            .answer(
                &mut pump,
                &change(&clients[heir], until, MemberChange::Remove(id(leader))),
            )
            .unwrap();
        let mut rest = vec![id(heir), id(other)];
        rest.sort_unstable();
        assert_eq!((removed.voters, removed.learners), (rest, vec![]));
        quiet(&mut driven, &mut pump);
        let sent = pump.delivered.get(&id(leader)).copied().unwrap_or_default();
        write(&mut driven, &mut pump, &clients[heir], 1);
        catch_up(&mut driven, &mut pump, &clients, heir, other);
        quiet(&mut driven, &mut pump);
        assert_eq!(
            pump.delivered.get(&id(leader)).copied().unwrap_or_default(),
            sent
        );
        assert_eq!(held(&mut driven, heir), vec![1]);
        assert_eq!(held(&mut driven, other), vec![1]);
        assert_eq!(held(&mut driven, leader), Vec::<u8>::new());
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.M3, T-T1.8.M5 (CP-T1.8) on driven time (replication.md, "Membership"): a member that
    /// joins catches up by the leader's image when its log no longer reaches back, and that image
    /// is bounded, so a database past the snapshot ceiling refuses a joiner before the change is
    /// proposed. Do: in a group of one voter, its ceiling lowered to 64 KiB, write a 128 KiB row;
    /// add member 2 as a learner; raise the ceiling back; add it again. Expect: TooLarge naming the
    /// database's bytes and the ceiling, nothing proposed and no learner; then the learner added,
    /// with the row.
    #[test]
    fn a_joiner_to_a_database_past_the_snapshot_ceiling_is_refused_before_it_is_proposed() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = vec![
            driven.open(dir.path(), 1, vec![1], None),
            driven.open_member(dir.path(), 2, vec![1], vec![2], None),
        ];
        let mut pump = Pump::new(&clients);
        driven.elect(&mut pump, &clients[..1], 0, 1);
        driven
            .owner_mut(0)
            .replica
            .machine_mut()
            .set_snapshot_bytes(64 << 10);
        driven
            .call(&mut pump, &clients[0], |db| {
                db.exec("CREATE TABLE joined(b)")?;
                db.exec("INSERT INTO joined VALUES(randomblob(131072))")?;
                Ok(Vec::new())
            })
            .unwrap();
        let proposed = pending_change(&driven, 0);
        let until = driven.until();
        let refused = driven.answer(
            &mut pump,
            &change(&clients[0], until, MemberChange::AddLearner(2)),
        );
        assert!(
            matches!(
                &refused,
                Err(Refusal::TooLarge { what, size, limit })
                    if what.contains("joiner") && *size > 128 << 10 && *limit == 64 << 10
            ),
            "{refused:?}"
        );
        assert_eq!(pending_change(&driven, 0), proposed);
        assert_eq!(
            clients[0].handle().status().unwrap().learners,
            Vec::<u64>::new()
        );

        driven
            .owner_mut(0)
            .replica
            .machine_mut()
            .set_snapshot_bytes(WIRE_IMAGE_BYTES);
        let added = driven
            .answer(
                &mut pump,
                &change(&clients[0], until, MemberChange::AddLearner(2)),
            )
            .unwrap();
        assert_eq!(added.learners, vec![2]);
        catch_up(&mut driven, &mut pump, &clients, 0, 1);
        let rows = driven
            .owner_mut(1)
            .replica
            .machine_mut()
            .read_db(|db| {
                let mut rows = 0;
                db.query("SELECT length(b) FROM joined", [], |row| {
                    rows += row.get::<i64>(0).map_err(unavailable)?;
                    Ok(true)
                })?;
                Ok(rows)
            })
            .unwrap();
        assert_eq!(rows, 128 << 10);
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.M1, T-T1.8.M3 (CP-T1.8) on driven time (replication.md, "Membership"; Ongaro's
    /// dissertation, section 4.1): a member's records count by the newest configuration its log
    /// states, applied or not, so a removal another lead writes over gives the member back. Do: in
    /// a group of three voters and learner 4, the leader appends 4's removal, whose records are
    /// lost; the other two, their clocks past an election timeout, elect one of them, whose entry
    /// takes that index; a write. Expect: the old leader refuses a record from 4 while its log
    /// holds the removal; once the new leader's entry replaces it, its configuration names 4 again
    /// and it takes 4's record; the change answered UnknownOutcome, and the write reaches 4.
    #[test]
    fn a_removal_another_lead_wrote_over_gives_the_member_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let mut clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        clients.push(driven.open_member(dir.path(), 4, vec![1, 2, 3], vec![4], None));
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients[..3], 0, 1);
        let until = driven.until();
        driven
            .answer(
                &mut pump,
                &change(&clients[leader], until, MemberChange::AddLearner(4)),
            )
            .unwrap();
        settled(&mut driven, &mut pump, 3, &[4]);
        driven.drain(&mut pump);
        // From the removal on, what 4 sent before it reaches a leader that no longer counts it.
        pump.departed.push(4);
        let proposed = pending_change(&driven, leader);
        let asked = change(&clients[leader], until, MemberChange::Remove(4));
        reach(&mut driven, &mut pump, "the removal appended", |driven| {
            pending_change(driven, leader) > proposed
        });
        clients[leader].handle().drain(OUTGOING_MESSAGES).unwrap();
        let counts = |driven: &Driven| {
            driven
                .owner(leader)
                .replica
                .core()
                .raft
                .configuration()
                .contains(4)
        };
        assert!(!counts(&driven));
        let from_learner = |driven: &mut Driven| {
            let mut record = hyper_raft::proto::message(
                u64::try_from(leader + 1).unwrap(),
                hyper_raft::proto::MessageType::MsgHeartbeatResponse,
            );
            record.from = 4;
            record.term = driven.owner(leader).replica.term();
            let mut bytes = Vec::new();
            record.encode(&mut bytes);
            let mut receipt = clients[leader]
                .handle()
                .admit_receive(4, bytes, driven.until())
                .unwrap();
            driven.step();
            receipt.try_result()
        };
        assert!(matches!(
            from_learner(&mut driven),
            Err(Refusal::Invalid { why, .. }) if why == "sender is not a current member"
        ));
        // Lost too: whatever that step had the old leader send.
        clients[leader].handle().drain(OUTGOING_MESSAGES).unwrap();

        let others: Vec<usize> = (0..3).filter(|owner| *owner != leader).collect();
        let answered = (0..ROUNDS)
            .find_map(|_| {
                for owner in &others {
                    driven.advance_tick(*owner);
                }
                driven.round(&mut pump);
                asked.try_recv().ok()
            })
            .unwrap();
        assert_eq!(answered, Err(Refusal::UnknownOutcome));
        reach(
            &mut driven,
            &mut pump,
            "the removal written over",
            |driven| driven.owner(leader).replica.applied().index > proposed && counts(driven),
        );
        settled(&mut driven, &mut pump, 3, &[4]);
        assert_eq!(from_learner(&mut driven), Ok(Some(())));
        let led =
            usize::try_from(clients[others[0]].handle().status().unwrap().leader - 1).unwrap();
        write(&mut driven, &mut pump, &clients[led], 1);
        catch_up(&mut driven, &mut pump, &clients, led, 3);
        assert_eq!(held(&mut driven, 3), vec![1]);
        driven.drain(&mut pump);
        pump.finish();
    }
    /// A group of three voters and member 4, opened as the learner it is to be.
    fn three_and_a_learner(driven: &mut Driven, dir: &Path) -> Vec<Client> {
        let mut clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir, n, vec![1, 2, 3], None))
            .collect();
        clients.push(driven.open_member(dir, 4, vec![1, 2, 3], vec![4], None));
        clients
    }

    /// AC-T1.8.M1, T-T1.8.M2 (CP-T1.8) on driven time (replication.md, "Membership"): a change past
    /// its deadline once its joint configuration is applied answers its status, joint, and neither
    /// a change nor a hand-off is taken until the group has left it. Do, in a group of three
    /// voters: add learner 4, the relay held from the moment the leader appends its way out, so the
    /// group stays joint; its deadline passes; then ask the leader for another change and for a
    /// hand-off. Expect: the status, with voters_outgoing [1, 2, 3] and learner 4; both refused
    /// Unavailable, saying the group is leaving a joint configuration or has a change to apply;
    /// with the relay back, the group leaves it and takes the next change.
    #[test]
    fn a_change_or_hand_off_is_refused_while_the_group_leaves_a_joint_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = three_and_a_learner(&mut driven, dir.path());
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients[..3], 0, 1);
        let led = &clients[leader];
        let clock = driven.clock(leader);
        let until = driven.base + offset(&clock) + Duration::from_secs(1);
        let proposed = pending_change(&driven, leader);
        let asked = change(led, until, MemberChange::AddLearner(4));
        reach(&mut driven, &mut pump, "the joint entry", |driven| {
            pending_change(driven, leader) > proposed
        });
        let joint = pending_change(&driven, leader);
        // The way out is appended once the joint entry commits. From then on the relay is held:
        // what the leader sends of it waits in its queue, so it can't commit.
        reach(&mut driven, &mut pump, "the way out appended", |driven| {
            pending_change(driven, leader) > joint
        });
        for _ in 0..ROUNDS {
            if !applied_configuration(&driven, leader)
                .voters_outgoing
                .is_empty()
            {
                break;
            }
            driven.step();
            std::thread::park_timeout(POLL);
        }
        assert_eq!(
            applied_configuration(&driven, leader).voters_outgoing,
            vec![1, 2, 3]
        );
        set(&clock, driven.base, until);
        driven.step();
        let answered = asked.try_recv().unwrap().unwrap();
        assert_eq!(
            (answered.voters_outgoing, answered.learners),
            (vec![1, 2, 3], vec![4])
        );

        let later = driven.base + offset(&clock) + Duration::from_secs(5);
        let other = u64::try_from((leader + 1) % 3 + 1).unwrap();
        for refused in [
            change(led, later, MemberChange::Promote(4)),
            transfer(led, later, other),
        ] {
            driven.step();
            assert!(matches!(
                refused.try_recv().unwrap(),
                Err(Refusal::Unavailable(why))
                    if why.contains("leaving a joint configuration")
                        || why.contains("still to apply")
            ),);
        }
        // The relay back: the way out reaches the followers and commits.
        settled(&mut driven, &mut pump, 3, &[4]);
        let until = driven.until();
        let promoted = driven
            .answer(&mut pump, &change(led, until, MemberChange::Promote(4)))
            .unwrap();
        assert_eq!(promoted.voters, vec![1, 2, 3, 4]);
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.M2, T-T1.8.M4 (CP-T1.8) on driven time (replication.md, "Membership"): a hand-off
    /// the core gives up is refused, and the lead stays. Do, in a group of three voters, every
    /// record to one follower lost: the leader hands its lead to that follower; the leader's and
    /// the other follower's clocks run on. Expect: Unavailable, saying the lead was not taken, and
    /// the leader still leading, in its term.
    #[test]
    fn a_hand_off_the_core_gives_up_is_refused_and_the_lead_stays() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        let (heir, other) = ((leader + 1) % 3, (leader + 2) % 3);
        let term = clients[leader].handle().status().unwrap().term;
        pump.lost.push(u64::try_from(heir + 1).unwrap());
        let asked = transfer(
            &clients[leader],
            driven.until(),
            u64::try_from(heir + 1).unwrap(),
        );
        let answered = (0..ROUNDS)
            .find_map(|_| {
                driven.advance_tick(leader);
                driven.advance_tick(other);
                driven.round(&mut pump);
                asked.try_recv().ok()
            })
            .unwrap();
        assert!(
            matches!(&answered, Err(Refusal::Unavailable(why)) if why.contains("not taken")),
            "{answered:?}"
        );
        let status = clients[leader].handle().status().unwrap();
        assert_eq!(
            (status.leader, status.term),
            (u64::try_from(leader + 1).unwrap(), term)
        );
        pump.lost.clear();
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.M2, T-T1.8.M4 (CP-T1.8) on driven time (replication.md, "Membership"): a hand-off
    /// whose lead goes to a third member is refused naming it. Do, in a group of three voters,
    /// every record to one follower lost: the leader is asked to hand its lead to that follower,
    /// and its core then hands it to the other (as a disk-stall hand-off does). Expect: NotLeader
    /// naming the other follower.
    #[test]
    fn a_hand_off_whose_lead_goes_to_a_third_member_is_refused_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        let (heir, other) = ((leader + 1) % 3, (leader + 2) % 3);
        let other_id = u64::try_from(other + 1).unwrap();
        pump.lost.push(u64::try_from(heir + 1).unwrap());
        let asked = transfer(
            &clients[leader],
            driven.until(),
            u64::try_from(heir + 1).unwrap(),
        );
        driven.step();
        assert!(matches!(asked.try_recv(), Err(mpsc::TryRecvError::Empty)));
        driven.owner_mut(leader).replica.transfer(other_id).unwrap();
        assert_eq!(
            driven.answer(&mut pump, &asked),
            Err(Refusal::NotLeader {
                leader: Some(other_id)
            })
        );
        pump.lost.clear();
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.M2, T-T1.8.M4 (CP-T1.8) on driven time (replication.md, "Membership"): a hand-off
    /// past its deadline before the lead moves answers UnknownOutcome. Do, in a group of three
    /// voters, every record to one follower lost: the leader hands its lead to that follower, and
    /// its clock reaches the hand-off's deadline. Expect: UnknownOutcome, and the status read shows
    /// who leads.
    #[test]
    fn a_hand_off_past_its_deadline_answers_unknown_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        let heir = (leader + 1) % 3;
        pump.lost.push(u64::try_from(heir + 1).unwrap());
        let clock = driven.clock(leader);
        let until = driven.base + offset(&clock) + Duration::from_secs(1);
        let asked = transfer(&clients[leader], until, u64::try_from(heir + 1).unwrap());
        driven.step();
        assert!(matches!(asked.try_recv(), Err(mpsc::TryRecvError::Empty)));
        set(&clock, driven.base, until);
        driven.step();
        assert_eq!(asked.try_recv().unwrap(), Err(Refusal::UnknownOutcome));
        assert_eq!(
            clients[leader].handle().status().unwrap().leader,
            u64::try_from(leader + 1).unwrap()
        );
        pump.lost.clear();
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.M2, T-T1.8.M4 (CP-T1.8) on driven time (replication.md, "Membership"): a hand-off is
    /// refused beside a change, a change beside a hand-off, and a hand-off to a learner. Do, in a
    /// group of three voters and learner 4: ask a change and then a hand-off; then, every record to
    /// a follower lost, a hand-off to it and then a change; then a hand-off to learner 4. Expect:
    /// the second of each pair Unavailable, saying a change is under way; Invalid for the learner.
    #[test]
    fn a_hand_off_is_refused_beside_a_change_and_to_a_learner() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = three_and_a_learner(&mut driven, dir.path());
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients[..3], 0, 1);
        let led = &clients[leader];
        let heir = u64::try_from((leader + 1) % 3 + 1).unwrap();
        let until = driven.until();
        let first = change(led, until, MemberChange::AddLearner(4));
        let beside = transfer(led, until, heir);
        assert!(matches!(
            driven.answer(&mut pump, &beside),
            Err(Refusal::Unavailable(why)) if why.contains("under way")
        ));
        assert_eq!(driven.answer(&mut pump, &first).unwrap().learners, vec![4]);
        settled(&mut driven, &mut pump, 3, &[4]);

        pump.lost.push(heir);
        let handing = transfer(led, until, heir);
        driven.step();
        let beside = change(led, until, MemberChange::Remove(4));
        driven.step();
        assert!(matches!(
            beside.try_recv().unwrap(),
            Err(Refusal::Unavailable(why)) if why.contains("under way")
        ));
        let clock = driven.clock(leader);
        set(&clock, driven.base, until);
        driven.step();
        assert_eq!(handing.try_recv().unwrap(), Err(Refusal::UnknownOutcome));
        pump.lost.clear();

        let until = driven.until();
        assert!(matches!(
            driven.answer(&mut pump, &transfer(led, until, 4)),
            Err(Refusal::Invalid { what, .. }) if what == "lead transfer"
        ));
        driven.drain(&mut pump);
        pump.finish();
    }

    /// The application the floor's tests bind members to.
    fn kiln(version: u32) -> crate::database::Application {
        crate::database::Application {
            name: "kiln.host-store".into(),
            version,
        }
    }
    /// The attestation that counts for `member` as owner `owner`'s applied state proves it.
    fn attested(
        driven: &mut Driven,
        owner: usize,
        member: u64,
    ) -> Option<crate::replication::floor::Attested> {
        let machine = driven.owner_mut(owner).replica.machine_mut();
        let last = machine.configuration_at();
        machine
            .read_db(|db| crate::replication::floor::current(db, member, "kiln.host-store", last))
            .unwrap()
    }
    /// Marker J as owner `owner` has applied it.
    fn tracked(driven: &mut Driven, owner: usize) -> Option<u64> {
        driven
            .owner_mut(owner)
            .replica
            .machine_mut()
            .read_db(|db| crate::replication::floor::tracked(db))
            .unwrap()
    }
    /// Member `member`'s join as owner `owner` has applied it.
    fn joined(driven: &mut Driven, owner: usize, member: u64) -> u64 {
        driven
            .owner_mut(owner)
            .replica
            .machine_mut()
            .read_db(|db| crate::replication::floor::joined(db, member))
            .unwrap()
    }
    /// The floor's rows owner `owner` holds: joins tracked, join rows, and attestations.
    fn floor_rows(driven: &mut Driven, owner: usize) -> Vec<String> {
        driven.owner_mut(owner)
            .replica
            .machine_mut()
            .read_db(|db| {
                let mut rows = Vec::new();
                for (table, query) in [
                    (
                        "__amphora_joins",
                        "SELECT 'J ' || tracked FROM __amphora_joins",
                    ),
                    (
                        "__amphora_members",
                        "SELECT 'M ' || member || ' ' || joined FROM __amphora_members ORDER BY member",
                    ),
                    (
                        "__amphora_attestations",
                        "SELECT 'A ' || member || ' ' || joined || ' ' || application || ' ' || highest || ' ' || seen FROM __amphora_attestations ORDER BY member, joined",
                    ),
                ] {
                    let mut present = false;
                    db.query(
                        "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1",
                        [amphora_engine::Value::from_text(table.to_string())],
                        |_| {
                            present = true;
                            Ok(false)
                        },
                    )?;
                    if present {
                        db.query(query, [], |row| {
                            rows.push(row.get::<String>(0).map_err(unavailable)?);
                            Ok(true)
                        })?;
                    }
                }
                Ok(rows)
            })
            .unwrap()
    }
    /// No two of `owners` recorded different image digests at one applied index, each from the
    /// index given with it: a member to be added opens with the configuration it is to have, so
    /// its applied marker differs from the group's until it applies the change that adds it
    /// (replication.md, "Membership"). Answers how many digests it compared.
    fn digests_agree(driven: &Driven, owners: &[(usize, u64)]) -> usize {
        let mut compared = 0;
        let mut by_index: BTreeMap<u64, u64> = BTreeMap::new();
        for (owner, from) in owners {
            for (index, digest) in driven
                .owner(*owner)
                .replica
                .machine()
                .digests()
                .iter()
                .filter(|(index, _)| index >= from)
            {
                match by_index.get(index) {
                    Some(seen) => {
                        assert_eq!(
                            seen, digest,
                            "owner {owner} differs at applied index {index}"
                        );
                        compared += 1;
                    }
                    None => {
                        by_index.insert(*index, *digest);
                    }
                }
            }
        }
        compared
    }

    /// AC-T1.8.A1, T-T1.8.A4 (CP-T1.8) on driven time (replication.md, "Attestation"): a forwarded
    /// proposal whose context names a member other than its authenticated sender is refused and
    /// applies nowhere. Do, in a group of three voters: a follower prepares a write creating a table,
    /// and forwards it to the leader with contexts naming the third member, empty, four bytes long,
    /// and two entries of which only the second names the third; then once naming the follower
    /// itself, as an owner of either binary makes it. Expect: each hostile forward refused Invalid at
    /// the leader's step, and the table on no member after the group goes quiet; the last taken.
    #[test]
    fn a_forward_naming_another_member_as_its_origin_is_refused_and_applies_nowhere() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        let (follower, third) = ((leader + 1) % 3, (leader + 2) % 3);
        let id = |owner: usize| u64::try_from(owner + 1).unwrap();
        driven.drain(&mut pump);
        let (prepared, ()) = driven
            .owner_mut(follower)
            .replica
            .machine_mut()
            .prepare_db(|db| {
                db.exec("CREATE TABLE hostile(x)")?;
                Ok(())
            })
            .unwrap();
        let data = prepared.encode().unwrap();
        // A context as the owner makes one (`context()`): the origin, then term, incarnation and
        // serial. A binary from before the check makes it the same way.
        let context = |origin: u64| {
            let mut context = origin.to_le_bytes().to_vec();
            context.extend_from_slice(&[7; 24]);
            context
        };
        let forward_contexts = |driven: &mut Driven, contexts: Vec<Vec<u8>>| {
            let mut record =
                hyper_raft::proto::message(id(leader), hyper_raft::proto::MessageType::MsgPropose);
            record.from = id(follower);
            record.entries = contexts
                .into_iter()
                .map(|context| hyper_raft::proto::Entry {
                    data: data.clone(),
                    context,
                    ..hyper_raft::proto::Entry::default()
                })
                .collect();
            let mut bytes = Vec::new();
            record.encode(&mut bytes);
            let mut receipt = clients[leader]
                .handle()
                .admit_receive(id(follower), bytes, driven.until())
                .unwrap();
            driven.step();
            receipt.try_result()
        };
        let named_another = Err(api::invalid(
            "replica sender",
            "a forwarded proposal names another member as its origin",
        ));
        for hostile in [
            vec![context(id(third))],
            vec![Vec::new()],
            vec![id(follower).to_le_bytes()[..4].to_vec()],
            vec![context(id(follower)), context(id(third))],
        ] {
            assert_eq!(
                forward_contexts(&mut driven, hostile.clone()),
                named_another,
                "{hostile:?}"
            );
        }
        quiet(&mut driven, &mut pump);
        for owner in 0..3 {
            let present = driven
                .owner_mut(owner)
                .replica
                .machine_mut()
                .read_db(|db| {
                    let mut present = false;
                    db.query(
                        "SELECT 1 FROM sqlite_schema WHERE name = 'hostile'",
                        [],
                        |_| {
                            present = true;
                            Ok(false)
                        },
                    )?;
                    Ok(present)
                })
                .unwrap();
            assert!(!present, "owner {owner} applied the hostile forward");
        }
        assert_eq!(
            forward_contexts(&mut driven, vec![context(id(follower))]),
            Ok(Some(()))
        );
        driven.drain(&mut pump);
        pump.finish();
    }

    /// T-T1.8.A5 (CP-T1.8) on driven time (replication.md, "Attestation"; amphora-seam-owner's review
    /// of 7096916, D1): an attestation counts only for its own application. Do, in a group of three
    /// voters bound to kiln.host-store 7 with joins tracked: a follower binds other.app 1. Expect:
    /// its other.app attestation applied; it alone holds one; every member's kiln.host-store one
    /// still counts; and its keeper's proposals stop once it has attested, none in 200 rounds after.
    #[test]
    fn an_attestation_counts_only_for_its_own_application() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        for client in &clients {
            client.handle().attest(kiln(7)).unwrap();
        }
        joins_tracked(&mut driven, &mut pump, &[0, 1, 2]);
        let other = (leader + 1) % 3;
        let id = |owner: usize| u64::try_from(owner + 1).unwrap();
        let before = driven.owner_mut(other).floor_attestations;
        clients[other]
            .handle()
            .attest(crate::database::Application {
                name: "other.app".into(),
                version: 1,
            })
            .unwrap();
        let current = |driven: &mut Driven, member: u64, application: &'static str| {
            driven
                .owner_mut(leader)
                .replica
                .machine_mut()
                .read_db(move |db| crate::replication::floor::current(db, member, application, 0))
                .unwrap()
        };
        for _ in 0..ROUNDS {
            if current(&mut driven, id(other), "other.app").is_some() {
                break;
            }
            driven.round(&mut pump);
        }
        assert_eq!(
            current(&mut driven, id(other), "other.app")
                .unwrap()
                .highest,
            1
        );
        for owner in 0..3 {
            assert_eq!(
                current(&mut driven, id(owner), "kiln.host-store")
                    .unwrap()
                    .highest,
                7
            );
            if owner != other {
                assert_eq!(current(&mut driven, id(owner), "other.app"), None);
            }
        }
        let proposed = driven.owner_mut(other).floor_attestations - before;
        for _ in 0..200 {
            driven.round(&mut pump);
        }
        assert_eq!(
            driven.owner_mut(other).floor_attestations - before,
            proposed,
            "the keeper kept proposing"
        );
        assert!((1..=3).contains(&proposed), "{proposed} proposals");
        driven.drain(&mut pump);
        pump.finish();
    }

    /// CP-T1.8 on driven time (replication.md, "Attestation"; amphora-seam-owner's review of
    /// 7096916, D2): a keeper that can't read its rows reports it and leaves the owner serving. Do,
    /// in a group of three voters: write an attestation row for the leader whose highest is past a
    /// u32; then the leader binds kiln.host-store 7, and the group writes. Expect: the leader's
    /// keeper fails its passes, counted; no owner fenced; the write applied.
    #[test]
    fn a_keeper_that_cannot_read_its_rows_leaves_the_owner_serving() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        let me = u64::try_from(leader + 1).unwrap();
        driven
            .call(&mut pump, &clients[leader], move |db| {
                crate::replication::floor::attest(db, me, "kiln.host-store", 7)?;
                db.exec("UPDATE __amphora_attestations SET highest = 1099511627776")?;
                Ok(Vec::new())
            })
            .unwrap();
        clients[leader].handle().attest(kiln(7)).unwrap();
        for _ in 0..64 {
            driven.round(&mut pump);
        }
        assert!(
            driven.owner_mut(leader).floor_failures > 0,
            "the keeper never failed"
        );
        let counters = clients[leader].handle().floor_counters();
        assert!(counters.keeper_failures > 0, "{counters:?}");
        assert_eq!(counters.keeper_failing, 1, "{counters:?}");
        write(&mut driven, &mut pump, &clients[leader], 1);
        for client in &clients {
            assert_eq!(client.handle().status().unwrap().fenced, None);
        }
        driven.drain(&mut pump);
        pump.finish();
    }

    /// CP-T1.8 on driven time (replication.md, "Attestation"; amphora-seam-owner's review of
    /// 7096916, D3): a floor session that ends without applying is made again, in a group where
    /// nothing else applies. Do, in a group of three voters: a follower binds kiln.host-store 7;
    /// once its keeper has a session in hand, drop it, as one lost at its barrier is; then rounds.
    /// Expect: the session counted as ended, and the follower's attestation applied all the same.
    #[test]
    fn a_floor_session_lost_before_it_applies_is_made_again_in_a_quiet_group() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        let follower = (leader + 1) % 3;
        quiet(&mut driven, &mut pump);
        clients[follower].handle().attest(kiln(7)).unwrap();
        for _ in 0..ROUNDS {
            if driven.owner_mut(follower).floor_inflight {
                break;
            }
            driven.step();
        }
        assert!(driven.owner_mut(follower).floor_inflight);
        assert!(
            matches!(
                driven
                    .owner_mut(follower)
                    .pending
                    .as_ref()
                    .map(|p| &p.phase),
                Some(Phase::Barrier(_))
            ),
            "the session left its barrier"
        );
        driven.owner_mut(follower).pending = None;
        let ended = driven.owner_mut(follower).floor_ended;
        let member = u64::try_from(follower + 1).unwrap();
        for _ in 0..ROUNDS {
            if attested(&mut driven, leader, member).is_some() {
                break;
            }
            driven.round(&mut pump);
        }
        assert!(driven.owner_mut(follower).floor_ended > ended);
        assert_eq!(attested(&mut driven, leader, member).unwrap().highest, 7);
        driven.drain(&mut pump);
        pump.finish();
    }

    /// The floor session `owner`'s keeper has in hand at its barrier, after rounds: its deadline
    /// less the owner's time when it was made.
    fn session_lease(driven: &mut Driven, owner: usize) -> Duration {
        for _ in 0..ROUNDS {
            if driven.owner_mut(owner).floor_inflight {
                break;
            }
            driven.step();
        }
        let actor = driven.owner_mut(owner);
        let pending = actor.pending.as_ref().expect("a floor session in hand");
        assert!(
            matches!(pending.phase, Phase::Barrier(_)),
            "the session left its barrier"
        );
        pending.until.saturating_duration_since(actor.at)
    }

    /// CP-T1.8 (replication.md, "Attestation"; amphora-seam-owner-2,
    /// amphora-owner-t1-constants-floor-session-20261111): an owner's own session lasts what its
    /// role's raft timing says, read when it is made, not a picked 60 s. Do, in a group of three
    /// voters: a follower binds kiln.host-store 7 and its keeper makes a session; then the
    /// leader binds it and its keeper makes one; then the leader's voters stall (nothing relayed)
    /// while its clock moves a tick at a time. Expect: the follower's lease is its election bound
    /// and stall patience, (2 x election_tick + patience) ticks; the leader's two quorum checks,
    /// 2 x (election_tick + quorum_patience) ticks; and under the stall the leader's session ends
    /// within its lease.
    #[test]
    fn an_owners_session_lasts_its_roles_raft_timing() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        let follower = (leader + 1) % 3;
        quiet(&mut driven, &mut pump);
        let ticks = |period: Duration, n: usize| period * u32::try_from(n).unwrap();

        clients[follower].handle().attest(kiln(7)).unwrap();
        let lease = session_lease(&mut driven, follower);
        let raft = &driven.owner(follower).replica.core().raft;
        let want = ticks(
            driven.period(follower),
            2 * raft.config().election_tick + raft.patience(),
        );
        assert_eq!(lease, want, "the follower's session lease");
        for _ in 0..ROUNDS {
            if attested(&mut driven, leader, u64::try_from(follower + 1).unwrap()).is_some() {
                break;
            }
            driven.round(&mut pump);
        }

        clients[leader].handle().attest(kiln(7)).unwrap();
        let lease = session_lease(&mut driven, leader);
        let raft = &driven.owner(leader).replica.core().raft;
        let want = ticks(
            driven.period(leader),
            2 * (raft.config().election_tick + raft.quorum_patience()),
        );
        assert_eq!(lease, want, "the leader's session lease");
        let mut waited = Duration::ZERO;
        while driven.owner_mut(leader).pending.is_some() {
            assert!(waited <= lease, "a stalled leader's session past its lease");
            let period = driven.period(leader);
            driven.advance(leader, period);
            waited += period;
            driven.step();
        }
        driven.drain(&mut pump);
        pump.finish();
    }

    /// CP-T1.8, AC-DQ.1.1 (replication.md, "Decision (lane constants): one drive a step"): an
    /// owner drives once a step and, while that drive leaves more, steps again at once, so no
    /// count of drives a step is picked. Do: three members, an election, 20 writes at the leader,
    /// each step on driven time. Expect: every write answered; some step left more to drive for
    /// the next (a step that drives until nothing is left, as the 32 drives a step did short of
    /// their count, leaves none).
    #[test]
    fn an_owner_drives_once_a_step_and_steps_again_while_more_waits() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        for value in 1..=20 {
            write(&mut driven, &mut pump, &clients[leader], value);
        }
        let more: u64 = (0..3).map(|owner| driven.owner(owner).drove_more).sum();
        assert!(more > 0, "no step left more to drive");
        driven.drain(&mut pump);
        pump.finish();
    }

    /// CP-T1.8, AC-WK.1.1 (replication.md, "Decision (lane constants): the owner's wakes"): the owner's bell keeps a ring
    /// given before its wait, and its waker rings it from another thread. Do: ring, then wait not
    /// at all, twice; then wait an hour while another thread wakes the bell's waker. Expect: the
    /// first wait finds the ring and the second none; the hour's wait ends rung.
    #[test]
    fn the_owners_bell_keeps_a_ring_and_its_waker_rings_it() {
        let bell = Bell::new();
        bell.ring();
        assert!(bell.wait(Duration::ZERO).unwrap(), "a ring before the wait");
        assert!(!bell.wait(Duration::ZERO).unwrap(), "a ring taken twice");
        let waker = std::task::Waker::from(Arc::clone(&bell));
        let ringer = std::thread::spawn(move || waker.wake());
        assert!(
            bell.wait(Duration::from_secs(3600)).unwrap(),
            "the waker's ring"
        );
        ringer.join().unwrap();
    }

    /// CP-T1.8, AC-WK.1.2: an owner with nothing to do waits until its next deadline: its next
    /// tick, or sooner the deadline of a submit it holds. Do: a lone member of three (no leader),
    /// stepped with its clock standing still; then a submit due a nanosecond past its clock, which
    /// it holds for want of a leader. Expect: its deadline is its next tick, then the submit's.
    #[test]
    fn an_owners_next_deadline_is_its_tick_or_a_sooner_held_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let client = driven.open(dir.path(), 1, vec![1, 2, 3], None);
        driven.step();
        let owner = driven.owner(0);
        assert_eq!(owner.deadline(), owner.ticked + owner.pace.period());
        let due = owner.at + Duration::from_nanos(1);
        let _answer = submit(&client, due, |_| Ok(Vec::new()));
        driven.step();
        assert_eq!(
            driven.owner(0).deadline(),
            due,
            "the held submit's deadline"
        );
    }

    /// CP-T1.8, AC-WK.1.3: the owner thread wakes on events, not on a poll. Do: a lone voter on
    /// the owner thread and the real clock; a write submitted until it is answered (it waits on
    /// the log's write, which wakes the owner); then every handle dropped. Expect: the write
    /// answered, and the owner thread ended once the last handle went.
    #[test]
    fn an_owner_thread_answers_on_its_wakes_and_ends_with_its_last_handle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let (client, mut actor, receiver) = super::open(
            &path,
            &dir.path().join("raft.log"),
            ReplicaConfig {
                node_id: 1,
                voters: vec![1],
                learners: Vec::new(),
                bootstrap: None,
            },
            Durability::Persisted,
            Arc::new(Changed::default()),
            Owned::unbudgeted(lease(&path), None),
            Instant::now() + Duration::from_secs(60),
            Arc::new(Instant::now),
        )
        .unwrap();
        let owner = std::thread::spawn(move || actor.run(receiver));
        let mut answered = None;
        for _ in 0..ROUNDS {
            let until = Instant::now() + Duration::from_secs(30);
            let receiver = submit(&client, until, |db| {
                db.exec(
                    "CREATE TABLE IF NOT EXISTS woken(n INTEGER); INSERT INTO woken VALUES (1)",
                )?;
                Ok(Vec::new())
            });
            match receiver.recv_timeout(Duration::from_secs(30)).unwrap() {
                Err(Refusal::NotLeader { .. } | Refusal::Unavailable(_)) => {
                    std::thread::park_timeout(POLL)
                }
                other => {
                    answered = Some(other);
                    break;
                }
            }
        }
        assert!(
            matches!(answered, Some(Ok(_))),
            "the write's answer: {:?}",
            answered.as_ref().map(|answer| answer.as_ref().err())
        );
        drop(client);
        owner.join().unwrap();
    }

    /// CP-T1.8 (amphora-seam-owner-2, amphora-owner-t1-constants-721acf2-20261112): an owner's own
    /// session that reaches its lease at its barrier while its member still leads its term is
    /// counted. Do, in a group of three voters: the leader binds kiln.host-store 7 and its keeper
    /// makes a session; bring its deadline to now, as a ReadIndex backlog behind a live quorum
    /// would leave it, and step. Expect: that session ended (any in hand after is the keeper's next,
    /// with a lease of its own), the leader still leading the same term, and
    /// `amphora_replica_own_sessions_expired_total` at 1.
    #[test]
    fn a_live_leaders_session_past_its_lease_is_counted() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        quiet(&mut driven, &mut pump);
        clients[leader].handle().attest(kiln(7)).unwrap();
        session_lease(&mut driven, leader);
        let term = driven.owner(leader).replica.term();
        assert_eq!(clients[leader].handle().counters().own_sessions_expired, 0);
        let actor = driven.owner_mut(leader);
        let now = actor.at;
        actor.pending.as_mut().unwrap().until = now;
        driven.step();
        let actor = driven.owner(leader);
        assert!(actor.replica.is_leader() && actor.replica.term() == term);
        // Ended; the keeper may have made the next at once, with a lease of its own.
        assert!(
            actor.pending.as_ref().is_none_or(|p| p.until > now),
            "the session past its lease ended"
        );
        assert_eq!(clients[leader].handle().counters().own_sessions_expired, 1);
        driven.drain(&mut pump);
        pump.finish();
    }

    /// Rounds, time standing still, until every one of `owners` has marker J applied.
    fn joins_tracked(driven: &mut Driven, pump: &mut Pump<'_>, owners: &[usize]) {
        for _ in 0..ROUNDS {
            if owners.iter().all(|owner| tracked(driven, *owner).is_some()) {
                return;
            }
            driven.round(pump);
        }
        panic!("joins were not tracked in {ROUNDS} driven rounds");
    }

    /// AC-T1.8.A1, T-T1.8.A5 (CP-T1.8) on driven time (replication.md, "Joins", "Attestation"): a
    /// member removed and added again after marker J attests under its new join, and its old
    /// attestation never vouches nor blocks. Do, in a group of three voters and learner 4: every
    /// member binds kiln.host-store 7 and the group tracks joins; remove 4 and, its owner no longer
    /// attesting, add it again; then let it attest. Expect: J on every voter with 4 joined at 0 and
    /// attested; the re-add answered Ok, 4 joined at the change's index on every voter, its old
    /// row still there but no attestation counting; once it attests, its new one counts.
    #[test]
    fn a_member_added_again_after_joins_are_tracked_attests_under_its_new_join() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = three_and_a_learner(&mut driven, dir.path());
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients[..3], 0, 1);
        let until = driven.until();
        driven
            .answer(
                &mut pump,
                &change(&clients[leader], until, MemberChange::AddLearner(4)),
            )
            .unwrap();
        settled(&mut driven, &mut pump, 3, &[4]);
        for client in &clients {
            client.handle().attest(kiln(7)).unwrap();
        }
        joins_tracked(&mut driven, &mut pump, &[0, 1, 2, 3]);
        assert_eq!(joined(&mut driven, leader, 4), 0);
        assert_eq!(attested(&mut driven, leader, 4).unwrap().highest, 7);

        pump.departed.push(4);
        let until = driven.until();
        driven
            .answer(
                &mut pump,
                &change(&clients[leader], until, MemberChange::Remove(4)),
            )
            .unwrap();
        settled(&mut driven, &mut pump, 3, &[]);
        driven.owner_mut(3).attesting = None;
        clients[3].handle.shared.attesting.lock().unwrap().take();
        pump.departed.clear();
        let until = driven.until();
        let added = driven
            .answer(
                &mut pump,
                &change(&clients[leader], until, MemberChange::AddLearner(4)),
            )
            .unwrap();
        assert_eq!(added.learners, vec![4]);
        settled(&mut driven, &mut pump, 3, &[4]);
        let rejoined = joined(&mut driven, leader, 4);
        assert!(rejoined > 0, "4 has no new join");
        for owner in 0..3 {
            assert_eq!(joined(&mut driven, owner, 4), rejoined, "owner {owner}");
            assert_eq!(attested(&mut driven, owner, 4), None, "owner {owner}");
            assert!(
                floor_rows(&mut driven, owner)
                    .iter()
                    .any(|row| row.starts_with("A 4 0 ")),
                "owner {owner} lost the old row"
            );
        }

        clients[3].handle().attest(kiln(7)).unwrap();
        for _ in 0..ROUNDS {
            if attested(&mut driven, leader, 4).is_some() {
                break;
            }
            driven.round(&mut pump);
        }
        let again = attested(&mut driven, leader, 4).unwrap();
        assert_eq!(again.highest, 7);
        assert!(
            floor_rows(&mut driven, leader)
                .iter()
                .any(|row| row.starts_with(&format!("A 4 {rejoined} ")))
        );
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.A1, T-T1.8.A5 (CP-T1.8) on driven time (replication.md, "Joins"): a member added
    /// after marker J holds the join rows every voter holds, though it opens with the
    /// configuration it is to have, and its attestation counts. Do, in a group of three voters and
    /// member 4 opened as a learner: the voters bind kiln.host-store 7 and the group tracks joins;
    /// add learner 4; then 4 binds kiln.host-store 7. Expect: on 4 the same floor rows as on the
    /// leader, 4 joined at the change's index on both, 4's attestation counting at the leader, and
    /// every member's image digest the same at every applied index from 4's join (T-T1.8.A7).
    #[test]
    fn a_member_added_after_joins_are_tracked_holds_the_join_rows_every_voter_holds() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = three_and_a_learner(&mut driven, dir.path());
        for owner in 0..4 {
            driven
                .owner_mut(owner)
                .replica
                .machine_mut()
                .record_digests();
        }
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients[..3], 0, 1);
        for client in &clients[..3] {
            client.handle().attest(kiln(7)).unwrap();
        }
        joins_tracked(&mut driven, &mut pump, &[0, 1, 2]);
        let until = driven.until();
        driven
            .answer(
                &mut pump,
                &change(&clients[leader], until, MemberChange::AddLearner(4)),
            )
            .unwrap();
        settled(&mut driven, &mut pump, 3, &[4]);
        let added = joined(&mut driven, leader, 4);
        assert!(added > 0, "4 has no join at the leader");
        // 4 applies the change in its own time: wait on that, then read its row.
        let at = driven.owner(leader).replica.applied().index;
        reach(
            &mut driven,
            &mut pump,
            "4 applying its addition",
            |driven| driven.owner(3).replica.applied().index >= at,
        );
        assert_eq!(joined(&mut driven, 3, 4), added);

        clients[3].handle().attest(kiln(7)).unwrap();
        for _ in 0..ROUNDS {
            if attested(&mut driven, leader, 4).is_some() && attested(&mut driven, 3, 4).is_some() {
                break;
            }
            driven.round(&mut pump);
        }
        assert_eq!(attested(&mut driven, leader, 4).unwrap().highest, 7);
        assert_eq!(floor_rows(&mut driven, 3), floor_rows(&mut driven, leader));
        // A7's guard against divergence covers this member too: every image digest the same from
        // its join on.
        let from = [(0, added), (1, added), (2, added), (3, added)];
        assert!(digests_agree(&driven, &from) > 0, "no digest compared");
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.A1, T-T1.8.A7 (CP-T1.8) on driven time (replication.md, "The upgrade that
    /// introduces the join rows"): a group with a member on a binary from before the floor
    /// never tracks joins, keeps every member's pages identical, and counts no stale
    /// attestation; once that member runs the floor's binary, joins are tracked. Do: voters 1,
    /// 2 and 3, 3 standing in for the old binary (no join rows, no attestation); learner 4
    /// added; 1, 2 and 4 bind kiln.host-store 7; remove 4 and, its owner no longer attesting,
    /// add it again; then 4 attests, then 3 runs the floor's binary and attests; finally 4 is
    /// removed after J. Expect: no J while 3 is old; after the re-add, 4's old attestation
    /// counts on no voter until it attests again; J once 3 attests; join rows after it; and at
    /// every applied index every member's image digest the same.
    #[test]
    fn a_group_upgrading_to_the_floor_tracks_joins_only_once_every_member_attests() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = three_and_a_learner(&mut driven, dir.path());
        for owner in 0..4 {
            driven
                .owner_mut(owner)
                .replica
                .machine_mut()
                .record_digests();
        }
        driven
            .owner_mut(2)
            .replica
            .machine_mut()
            .set_floor_aware(false);
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients[..3], 0, 1);
        assert_ne!(leader, 2, "the old member leads");
        let until = driven.until();
        let added = driven
            .answer(
                &mut pump,
                &change(&clients[leader], until, MemberChange::AddLearner(4)),
            )
            .unwrap()
            .applied;
        settled(&mut driven, &mut pump, 3, &[4]);
        for owner in [0, 1, 3] {
            clients[owner].handle().attest(kiln(7)).unwrap();
        }
        for _ in 0..ROUNDS {
            if [1, 2, 4]
                .iter()
                .all(|member| attested(&mut driven, leader, *member).is_some())
            {
                break;
            }
            driven.round(&mut pump);
        }
        for _ in 0..64 {
            driven.round(&mut pump);
        }
        assert_eq!(tracked(&mut driven, leader), None, "J with an old member");

        pump.departed.push(4);
        let until = driven.until();
        driven
            .answer(
                &mut pump,
                &change(&clients[leader], until, MemberChange::Remove(4)),
            )
            .unwrap();
        settled(&mut driven, &mut pump, 3, &[]);
        driven.owner_mut(3).attesting = None;
        clients[3].handle.shared.attesting.lock().unwrap().take();
        pump.departed.clear();
        let until = driven.until();
        driven
            .answer(
                &mut pump,
                &change(&clients[leader], until, MemberChange::AddLearner(4)),
            )
            .unwrap();
        settled(&mut driven, &mut pump, 3, &[4]);
        for owner in [0, 1] {
            assert_eq!(attested(&mut driven, owner, 4), None, "owner {owner}");
        }
        clients[3].handle().attest(kiln(7)).unwrap();
        for _ in 0..ROUNDS {
            if attested(&mut driven, leader, 4).is_some() {
                break;
            }
            driven.round(&mut pump);
        }
        assert!(attested(&mut driven, leader, 4).is_some());
        assert_eq!(tracked(&mut driven, leader), None, "J with an old member");

        driven
            .owner_mut(2)
            .replica
            .machine_mut()
            .set_floor_aware(true);
        clients[2].handle().attest(kiln(7)).unwrap();
        joins_tracked(&mut driven, &mut pump, &[0, 1, 2, 3]);
        pump.departed.push(4);
        let until = driven.until();
        driven
            .answer(
                &mut pump,
                &change(&clients[leader], until, MemberChange::Remove(4)),
            )
            .unwrap();
        settled(&mut driven, &mut pump, 3, &[]);
        let rows = floor_rows(&mut driven, leader);
        for owner in 0..3 {
            assert_eq!(floor_rows(&mut driven, owner), rows, "owner {owner}");
        }
        let compared = digests_agree(&driven, &[(0, 0), (1, 0), (2, 0), (3, added)]);
        assert!(compared > 0, "no digests compared");
        pump.departed.clear();
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.A1, T-T1.8.A3 (CP-T1.8) on driven time (replication.md, "Start"): a member installed
    /// from a snapshot holds the floor's rows and proves as the leader does. Do, in a group of
    /// three voters that has tracked joins with every member attested: compact the leader's log,
    /// then add learner 4, which can only catch up by the leader's image. Expect: 4's first applied
    /// index is the image's, beyond the change that added it; its floor rows and every member's
    /// attestation as it proves them equal the leader's; its digest at that index the leader's.
    #[test]
    fn a_member_installed_from_a_snapshot_holds_the_floors_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = three_and_a_learner(&mut driven, dir.path());
        for owner in 0..4 {
            driven
                .owner_mut(owner)
                .replica
                .machine_mut()
                .record_digests();
        }
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients[..3], 0, 1);
        for client in &clients[..3] {
            client.handle().attest(kiln(7)).unwrap();
        }
        joins_tracked(&mut driven, &mut pump, &[0, 1, 2]);
        write(&mut driven, &mut pump, &clients[leader], 1);
        quiet(&mut driven, &mut pump);
        driven.flushed(&mut pump, leader);
        let compacted = {
            let owner = &mut driven.owner_mut(leader);
            let now = owner.now().unwrap();
            owner
                .replica
                .compact(0, now, std::task::Waker::noop())
                .unwrap();
            owner.replica.applied().index
        };
        for _ in 0..64 {
            driven.round(&mut pump);
        }
        let until = driven.until();
        driven
            .answer(
                &mut pump,
                &change(&clients[leader], until, MemberChange::AddLearner(4)),
            )
            .unwrap();
        catch_up(&mut driven, &mut pump, &clients, leader, 3);
        let first = driven
            .owner_mut(3)
            .replica
            .machine()
            .digests()
            .first()
            .copied();
        let (installed, _) = first.expect("4 recorded nothing");
        assert!(
            installed >= compacted,
            "4 replayed the log from {installed}, not the image at {compacted}"
        );
        quiet(&mut driven, &mut pump);
        assert_eq!(floor_rows(&mut driven, 3), floor_rows(&mut driven, leader));
        for member in [1, 2, 3] {
            assert_eq!(
                attested(&mut driven, 3, member),
                attested(&mut driven, leader, member)
            );
        }
        assert!(digests_agree(&driven, &[(0, 0), (1, 0), (2, 0), (3, 0)]) > 0);
        driven.drain(&mut pump);
        pump.finish();
    }

    /// The `seen` of each attestation row of member `member` owner `owner` holds.
    fn seen_rows(driven: &mut Driven, owner: usize, member: u64) -> Vec<u64> {
        floor_rows(driven, owner)
            .iter()
            .filter_map(|row| {
                let fields: Vec<&str> = row.split(' ').collect();
                (fields.first() == Some(&"A")
                    && fields.get(1) == Some(&member.to_string().as_str()))
                .then(|| fields.last().unwrap().parse().unwrap())
            })
            .collect()
    }

    /// AC-T1.8.A1, T-T1.8.A7 (CP-T1.8) on driven time (replication.md, "The upgrade that introduces
    /// the join rows", point 2): a leader whose knowledge of the last change is reset by a restart
    /// asks every member to attest again once, each attests again once, and joins are tracked. Do,
    /// in a group of three voters: two bind kiln.host-store 7 and attest; a write; the leader opens
    /// again (its configuration_at becomes its applied point); the third binds. Expect: J on every
    /// voter; the request row applied once, its session proposed at most once more than the
    /// stale-base rule refused it (members' attestation sessions at the same base committing
    /// first; the keeper proposes it again on its next pass), those refusals no more than the
    /// attestations applied; each attesting member's row changed once more, the third's once; and
    /// the keeper's proposals bounded, never a loop.
    #[test]
    fn a_leader_restarted_before_joins_asks_every_member_to_attest_again_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        let (other, late) = ((leader + 1) % 3, (leader + 2) % 3);
        let id = |owner: usize| u64::try_from(owner + 1).unwrap();
        for owner in [leader, other] {
            clients[owner].handle().attest(kiln(7)).unwrap();
        }
        for _ in 0..ROUNDS {
            if [leader, other]
                .iter()
                .all(|owner| attested(&mut driven, leader, id(*owner)).is_some())
            {
                break;
            }
            driven.round(&mut pump);
        }
        write(&mut driven, &mut pump, &clients[leader], 1);
        quiet(&mut driven, &mut pump);
        assert_eq!(tracked(&mut driven, leader), None);
        let before: Vec<(Vec<u64>, u64)> = (0..3)
            .map(|owner| {
                (
                    seen_rows(&mut driven, leader, id(owner)),
                    driven.owner_mut(owner).floor_attestations,
                )
            })
            .collect();
        let requests = driven.owner_mut(leader).floor_requests;
        let (applied_before, stale_before) = {
            let owner = driven.owner(leader);
            (owner.floor_requests_applied, owner.floor_requests_stale)
        };

        driven
            .owner_mut(leader)
            .replica
            .machine_mut()
            .reopen_configuration_at();
        driven.owner_mut(leader).floor_settled = None;
        driven.owner_mut(leader).floor_checked = 0;
        clients[late].handle().attest(kiln(7)).unwrap();
        let mut seen: Vec<std::collections::BTreeSet<u64>> = before
            .iter()
            .map(|(rows, _)| rows.iter().copied().collect())
            .collect();
        let mut requested = std::collections::BTreeSet::new();
        for _ in 0..ROUNDS {
            for (owner, rows) in seen.iter_mut().enumerate() {
                rows.extend(seen_rows(&mut driven, leader, id(owner)));
            }
            requested.insert(
                driven
                    .owner_mut(leader)
                    .replica
                    .machine_mut()
                    .read_db(|db| crate::replication::floor::requested(db))
                    .unwrap(),
            );
            if (0..3).all(|owner| tracked(&mut driven, owner).is_some()) {
                break;
            }
            driven.round(&mut pump);
        }
        joins_tracked(&mut driven, &mut pump, &[0, 1, 2]);
        assert_eq!(
            requested.iter().filter(|at| **at > 0).count(),
            1,
            "requests: {requested:?}"
        );
        // The request row is written once. Its session may be proposed again: members'
        // attestation sessions prepared at the same base commit first and the stale-base rule
        // refuses it, and the keeper proposes it again on its next pass. Each refusal needs a
        // session committed in between, so the retries are bounded by the attestations applied.
        let (proposed, applied, stale) = {
            let owner = driven.owner(leader);
            (
                owner.floor_requests - requests,
                owner.floor_requests_applied - applied_before,
                owner.floor_requests_stale - stale_before,
            )
        };
        assert_eq!(applied, 1, "requests applied");
        assert!(
            proposed <= 1 + stale,
            "{proposed} proposed, {stale} refused stale"
        );
        let attestations_applied: usize = seen
            .iter()
            .zip(&before)
            .map(|(rows, (was, _))| rows.len() - was.len())
            .sum();
        assert!(
            stale <= u64::try_from(attestations_applied).unwrap(),
            "{stale} stale requests, {attestations_applied} attestations applied"
        );
        for owner in [leader, other] {
            assert_eq!(
                seen[owner].len(),
                2,
                "owner {owner} attested {:?}",
                seen[owner]
            );
        }
        assert_eq!(seen[late].len(), 1, "the third attested {:?}", seen[late]);
        // A session another member's makes stale is proposed again on a later pass; prepared on
        // its member's whole log, each refusal needs another member's session committed first,
        // so every member's proposals are bounded by the members; once settled, none at all.
        let proposed: Vec<u64> = before
            .iter()
            .enumerate()
            .map(|(owner, (_, attestations))| {
                driven.owner_mut(owner).floor_attestations - attestations
            })
            .collect();
        for (owner, proposed) in proposed.iter().enumerate() {
            let keeper = driven.owner(owner);
            // Bounded by the members (amphora-seam-owner's ruling on the keeper's livelock, b'):
            // a floor session prepares on its member's whole log, so each refusal needs another
            // member's session committed first, at most one each.
            assert!(
                (1..=3).contains(proposed),
                "owner {owner} proposed {proposed} attestations (sessions ended {}, keeper failures {}, applied {}, seen {:?})",
                keeper.floor_ended,
                keeper.floor_failures,
                keeper.replica.applied().index,
                seen[owner]
            );
        }
        let settled_requests = driven.owner_mut(leader).floor_requests;
        for _ in 0..200 {
            driven.round(&mut pump);
        }
        for (owner, proposed) in proposed.iter().enumerate() {
            assert_eq!(
                driven.owner_mut(owner).floor_attestations - before[owner].1,
                *proposed,
                "owner {owner} kept attesting once settled"
            );
        }
        assert_eq!(driven.owner_mut(leader).floor_requests, settled_requests);
        driven.drain(&mut pump);
        pump.finish();
    }

    /// Runs `f` at `client` as a session of a handle bound to `application`, rounds until it is
    /// answered.
    fn call_as<R: Send + 'static>(
        driven: &mut Driven,
        pump: &mut Pump<'_>,
        client: &Client,
        application: crate::database::Application,
        f: impl FnOnce(&mut Db) -> anyhow::Result<R> + Send + 'static,
    ) -> Result<R, Refusal> {
        let operation = Operation::Call(Mutex::new(Some(Box::new(move |db| {
            Ok(Box::new(f(db)?) as Box<dyn Any + Send>)
        }))));
        let (response, receiver) = mpsc::sync_channel(1);
        client
            .handle
            .enqueue(
                Input::Submit {
                    namespace: String::new(),
                    operation,
                    application: Some(application),
                    until: driven.until(),
                    response,
                },
                api::REQUEST_MAX,
            )
            .unwrap();
        driven.answer(pump, &receiver).map(|answer| match answer {
            Answer::Called(value) => *value.downcast::<R>().unwrap(),
            _ => panic!("wrong actor reply"),
        })
    }
    /// A raise of kiln.host-store's floor to `version` at `client`, by a handle bound to it.
    fn raise(
        driven: &mut Driven,
        pump: &mut Pump<'_>,
        client: &Client,
        version: u32,
    ) -> Result<u32, Refusal> {
        // As writer_floor answers it: a refusal counted under its reason, then answered.
        let outcome = call_as(driven, pump, client, kiln(version), move |db| {
            Ok(
                crate::database::application::raise(db, kiln(version), true)?
                    .map(|floor| floor.version),
            )
        })?;
        outcome.map_err(|refused| {
            client
                .handle()
                .floor_refused(refused.reason, &refused.refusal);
            refused.refusal
        })
    }
    /// A write into floor_items by a handle bound to kiln.host-store at `version`.
    fn write_as(
        driven: &mut Driven,
        pump: &mut Pump<'_>,
        client: &Client,
        version: u32,
    ) -> Result<(), Refusal> {
        call_as(driven, pump, client, kiln(version), move |db| {
            db.exec("CREATE TABLE IF NOT EXISTS floor_items(version)")?;
            db.exec(&format!("INSERT INTO floor_items VALUES({version})"))?;
            Ok(())
        })
    }
    /// The versions written into floor_items that owner `owner` holds.
    fn floor_items(driven: &mut Driven, owner: usize) -> Vec<u32> {
        driven
            .owner_mut(owner)
            .replica
            .machine_mut()
            .read_db(|db| {
                let mut present = false;
                db.query(
                    "SELECT 1 FROM sqlite_schema WHERE name = 'floor_items'",
                    [],
                    |_| {
                        present = true;
                        Ok(false)
                    },
                )?;
                let mut versions = Vec::new();
                if present {
                    db.query(
                        "SELECT version FROM floor_items ORDER BY version",
                        [],
                        |row| {
                            versions.push(u32::try_from(row.get::<i64>(0).map_err(unavailable)?)?);
                            Ok(true)
                        },
                    )?;
                }
                Ok(versions)
            })
            .unwrap()
    }
    /// Three voters bound to kiln.host-store 7, joins tracked; the leader's index.
    fn tracked_group(driven: &mut Driven, pump: &mut Pump<'_>, clients: &[Client]) -> usize {
        let leader = driven.elect(pump, &clients[..3], 0, 1);
        for client in &clients[..3] {
            client.handle().attest(kiln(7)).unwrap();
        }
        joins_tracked(driven, pump, &[0, 1, 2]);
        leader
    }

    /// AC-T1.8.A1, T-T1.8.A1 (CP-T1.8) on driven time (replication.md, "Raising the floor"): the
    /// race CockroachDB's automatic finalization leaves open, a member on an old binary added just
    /// before the floor is raised. Do, in a group of three voters bound to kiln.host-store 7 with
    /// joins tracked: add learner 4, bound to version 6; raise the floor to 7; remove 4 and raise
    /// again; then write as 6 and as 7 at the leader and at a follower. Expect: the first raise
    /// refused Unavailable naming member 4 alone, no floor installed; the second raised to 7; the
    /// writes as 6 refused Fenced and the writes as 7 applied, so on every voter no write below
    /// the floor applied.
    #[test]
    fn a_raise_is_refused_while_a_member_on_an_older_binary_belongs_to_the_group() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = three_and_a_learner(&mut driven, dir.path());
        let mut pump = Pump::new(&clients);
        let leader = tracked_group(&mut driven, &mut pump, &clients);
        let follower = (leader + 1) % 3;
        let until = driven.until();
        driven
            .answer(
                &mut pump,
                &change(&clients[leader], until, MemberChange::AddLearner(4)),
            )
            .unwrap();
        settled(&mut driven, &mut pump, 3, &[4]);
        clients[3].handle().attest(kiln(6)).unwrap();
        for _ in 0..ROUNDS {
            if attested(&mut driven, leader, 4).is_some() {
                break;
            }
            driven.round(&mut pump);
        }
        assert_eq!(attested(&mut driven, leader, 4).unwrap().highest, 6);

        let refused = raise(&mut driven, &mut pump, &clients[leader], 7);
        assert!(
            matches!(&refused, Err(Refusal::Unavailable(why)) if why.contains("members [4]")),
            "{refused:?}"
        );

        pump.departed.push(4);
        let until = driven.until();
        driven
            .answer(
                &mut pump,
                &change(&clients[leader], until, MemberChange::Remove(4)),
            )
            .unwrap();
        settled(&mut driven, &mut pump, 3, &[]);
        assert_eq!(raise(&mut driven, &mut pump, &clients[leader], 7), Ok(7));
        for client in [leader, follower] {
            assert!(matches!(
                write_as(&mut driven, &mut pump, &clients[client], 6),
                Err(Refusal::Fenced(_))
            ));
            write_as(&mut driven, &mut pump, &clients[client], 7).unwrap();
        }
        // Each voter applies both writes in its own time: wait on that, then read what it holds.
        for _ in 0..ROUNDS {
            if (0..3).all(|owner| floor_items(&mut driven, owner).len() >= 2) {
                break;
            }
            driven.round(&mut pump);
        }
        for owner in 0..3 {
            assert_eq!(floor_items(&mut driven, owner), vec![7, 7], "owner {owner}");
        }
        pump.departed.clear();
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.A1, T-T1.8.A2 (CP-T1.8) under a nemesis (replication.md, "Raising the floor"): the
    /// lead is lost between a change of members and a raise. Do, in a group of three voters bound
    /// to kiln.host-store 7 with joins tracked: ask the leader to add learner 5 (never running,
    /// never attesting) and to raise the floor to 7; once both are in its log, hand the lead to a
    /// follower; then raise again at the new leader. Expect: the first raise applied on every voter
    /// exactly when its entry precedes the change's in the log and on none otherwise, never
    /// answered as raised past the change; the second the floor in force if the first applied,
    /// else refused naming member 5.
    #[test]
    fn a_raise_beside_a_change_holds_through_a_lost_lead() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = three_and_a_learner(&mut driven, dir.path());
        let mut pump = Pump::new(&clients);
        pump.refusals = Some(0);
        // Learner 5 never runs: every record to it is lost.
        pump.lost.push(5);
        let leader = tracked_group(&mut driven, &mut pump, &clients);
        let until = driven.until();
        let added = change(&clients[leader], until, MemberChange::AddLearner(5));
        let operation = Operation::Call(Mutex::new(Some(Box::new(move |db: &mut Db| {
            let floor = crate::database::application::raise(db, kiln(7), true)?
                .map_err(|refused| anyhow::Error::from(refused.refusal))?;
            // The configuration at the raise's base, which the stale-base rule makes its own at apply.
            let learners = crate::replication::floor::configuration(db)?.learners;
            Ok(Box::new((floor.version, learners)) as Box<dyn Any + Send>)
        }))));
        let (response, raised) = mpsc::sync_channel(1);
        clients[leader]
            .handle
            .enqueue(
                Input::Submit {
                    namespace: String::new(),
                    operation,
                    application: Some(kiln(7)),
                    until,
                    response,
                },
                api::REQUEST_MAX,
            )
            .unwrap();
        // The lead is lost once both are in the leader's log, the change's entry and the raise's.
        for _ in 0..ROUNDS {
            let owner = &driven.owner_mut(leader);
            if owner.replica.core().raft.has_pending_conf()
                && matches!(
                    owner.pending.as_ref().map(|pending| &pending.phase),
                    Some(Phase::Proposed(_))
                )
            {
                break;
            }
            driven.round(&mut pump);
        }
        let owner = &driven.owner_mut(leader);
        assert!(
            owner.replica.core().raft.has_pending_conf(),
            "the change was never proposed"
        );
        assert!(
            matches!(
                owner.pending.as_ref().map(|pending| &pending.phase),
                Some(Phase::Proposed(_))
            ),
            "the raise was never proposed"
        );
        // Where each sits in the leader's log: the stale-base rule lets the raise apply only if
        // it comes before the change, its base having not seen it.
        let change_index = owner.replica.core().raft.pending_conf_index();
        let context = owner.pending.as_ref().unwrap().context.clone();
        let raise_index = owner
            .replica
            .core()
            .raft
            .log()
            .entries(owner.replica.applied().index + 1, u64::MAX, 64)
            .unwrap()
            .iter()
            .find(|entry| entry.context == context)
            .unwrap()
            .index;
        let raised_first = raise_index < change_index;
        let led = driven.hand_off(&mut pump, &clients[..3], leader, (leader + 1) % 3);
        let first = driven.answer(&mut pump, &raised);
        let _ = driven.answer(&mut pump, &added);
        settled(&mut driven, &mut pump, 3, &[5]);
        match &first {
            Ok(Answer::Called(value)) => {
                let (version, learners) = value.downcast_ref::<(u32, Vec<u64>)>().unwrap();
                assert_eq!(*version, 7);
                assert!(!learners.contains(&5), "raised while 5 was a member");
                assert!(
                    raised_first,
                    "raised at {raise_index}, past the change at {change_index}"
                );
            }
            Ok(_) => panic!("wrong actor reply"),
            Err(_) => {}
        }
        // Applied on every voter exactly when it came first; never past the change.
        for owner in 0..3 {
            assert_eq!(
                floor_version(&mut driven, owner),
                if raised_first { Some(7) } else { None },
                "owner {owner}: raise at {raise_index}, change at {change_index}"
            );
        }
        // Raised again at the new leader: the floor in force at once if the first applied, else
        // a proof that now covers 5, which never attests.
        let again = raise(&mut driven, &mut pump, &clients[led], 7);
        if raised_first {
            assert_eq!(again, Ok(7));
        } else {
            assert!(
                matches!(&again, Err(Refusal::Unavailable(why)) if why.contains("members [5]")),
                "{again:?}"
            );
        }
        driven.drain(&mut pump);
        pump.finish();
    }

    /// Three voters bound to kiln.host-store 7, joins tracked and the floor raised to 7, then
    /// learner 4 added (not bound); the leader's index.
    fn floor_and_a_learner(driven: &mut Driven, pump: &mut Pump<'_>, clients: &[Client]) -> usize {
        let leader = tracked_group(driven, pump, clients);
        assert_eq!(raise(driven, pump, &clients[leader], 7), Ok(7));
        let until = driven.until();
        driven
            .answer(
                pump,
                &change(&clients[leader], until, MemberChange::AddLearner(4)),
            )
            .unwrap();
        settled(driven, pump, 3, &[4]);
        leader
    }
    /// A promotion of learner 4 at `client`, answered.
    fn promote(
        driven: &mut Driven,
        pump: &mut Pump<'_>,
        client: &Client,
    ) -> Result<ReplicaStatus, Refusal> {
        let until = driven.until();
        driven.answer(pump, &change(client, until, MemberChange::Promote(4)))
    }

    /// AC-T1.8.A1, T-T1.8.A1 (CP-T1.8) on driven time (replication.md, "How it composes with
    /// membership changes"): a learner that has not attested is not promoted. Do, in a group with
    /// the floor at kiln.host-store 7: add learner 4, never bound; promote it. Expect: Unavailable,
    /// naming the learner, the application and the floor; 4 still a learner.
    #[test]
    fn an_unattested_learner_is_not_promoted() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = three_and_a_learner(&mut driven, dir.path());
        let mut pump = Pump::new(&clients);
        let leader = floor_and_a_learner(&mut driven, &mut pump, &clients);
        let refused = promote(&mut driven, &mut pump, &clients[leader]);
        assert_eq!(
            refused,
            Err(Refusal::Unavailable(
                "learner 4 has not attested kiln.host-store under its current join; the group's floor is 7"
                    .into()
            ))
        );
        assert_eq!(applied_configuration(&driven, leader).learners, vec![4]);
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.A1 (CP-T1.8) on driven time (replication.md, "How it composes with membership
    /// changes"): a learner attested below the floor is not promoted. Do, in a group with the floor
    /// at kiln.host-store 7: add learner 4, and write its attestation at 6 under its join (as a
    /// bind racing the raise could); promote it. Expect: Invalid, "application floor", naming 6
    /// and 7; 4 still a learner.
    #[test]
    fn a_learner_attested_below_the_floor_is_not_promoted() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = three_and_a_learner(&mut driven, dir.path());
        let mut pump = Pump::new(&clients);
        let leader = floor_and_a_learner(&mut driven, &mut pump, &clients);
        call_as(&mut driven, &mut pump, &clients[leader], kiln(7), |db| {
            crate::replication::floor::attest(db, 4, "kiln.host-store", 6)
        })
        .unwrap();
        let refused = promote(&mut driven, &mut pump, &clients[leader]);
        assert_eq!(
            refused,
            Err(Refusal::Invalid {
                what: "application floor".into(),
                why: "learner 4 runs kiln.host-store up to 6; the group's floor is 7".into(),
            })
        );
        assert_eq!(applied_configuration(&driven, leader).learners, vec![4]);
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.A1 (CP-T1.8) on driven time (replication.md, "How it composes with membership
    /// changes"): a learner attested at the floor is promoted. Do, in a group with the floor at
    /// kiln.host-store 7: add learner 4; it binds kiln.host-store 7; promote it. Expect: Ok, with 4
    /// among the voters.
    #[test]
    fn a_learner_attested_at_the_floor_is_promoted() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = three_and_a_learner(&mut driven, dir.path());
        let mut pump = Pump::new(&clients);
        let leader = floor_and_a_learner(&mut driven, &mut pump, &clients);
        clients[3].handle().attest(kiln(7)).unwrap();
        for _ in 0..ROUNDS {
            if attested(&mut driven, leader, 4).is_some() {
                break;
            }
            driven.round(&mut pump);
        }
        assert_eq!(attested(&mut driven, leader, 4).unwrap().highest, 7);
        let promoted = promote(&mut driven, &mut pump, &clients[leader]).unwrap();
        assert_eq!(promoted.voters, vec![1, 2, 3, 4]);
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.A1 (CP-T1.8) on driven time (replication.md, "How it composes with membership
    /// changes"): a promotion is admitted only on a quiescent tail, so the attestation it reads is
    /// the one in force at its change's index, and is answered at once otherwise. Do, in a group
    /// with the floor at kiln.host-store 7 and learner 4 attested at 7: every append lost while
    /// heartbeats go on, a write appended at the leader; promote 4. Expect: Unavailable naming the
    /// last index and the applied index, and nothing proposed: 4 still a learner.
    #[test]
    fn a_promotion_beside_a_write_in_flight_is_refused_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = three_and_a_learner(&mut driven, dir.path());
        let mut pump = Pump::new(&clients);
        let leader = floor_and_a_learner(&mut driven, &mut pump, &clients);
        clients[3].handle().attest(kiln(7)).unwrap();
        for _ in 0..ROUNDS {
            if attested(&mut driven, leader, 4).is_some() && driven.owner_mut(3).pending.is_none() {
                break;
            }
            driven.round(&mut pump);
        }
        pump.appends_lost = true;
        let operation = Operation::Call(Mutex::new(Some(Box::new(|db: &mut Db| {
            db.exec("CREATE TABLE IF NOT EXISTS floor_items(version)")?;
            db.exec("INSERT INTO floor_items VALUES(7)")?;
            Ok(Box::new(()) as Box<dyn Any + Send>)
        }))));
        let (response, written) = mpsc::sync_channel(1);
        clients[leader]
            .handle
            .enqueue(
                Input::Submit {
                    namespace: String::new(),
                    operation,
                    application: Some(kiln(7)),
                    until: driven.until(),
                    response,
                },
                api::REQUEST_MAX,
            )
            .unwrap();
        let tail = |driven: &Driven| {
            let replica = &driven.owner(leader).replica;
            (
                replica.applied().index,
                replica.core().raft.log().last_index().unwrap(),
            )
        };
        for _ in 0..ROUNDS {
            let (applied, last) = tail(&driven);
            if last > applied {
                break;
            }
            driven.round(&mut pump);
        }
        let (applied, last) = tail(&driven);
        assert!(last > applied, "the write was never appended");
        let proposed = pending_change(&driven, leader);
        let refused = promote(&mut driven, &mut pump, &clients[leader]);
        assert_eq!(
            refused,
            Err(Refusal::Unavailable(format!(
                "the log runs to {last} and {applied} is applied; promote once the tail is applied"
            )))
        );
        assert_eq!(
            clients[leader]
                .handle()
                .floor_counters()
                .refused(FloorRefusal::Busy),
            1,
            "a tail's refusal counted as busy"
        );
        // Nothing proposed: the change slot's mark unmoved, and 4 still a learner.
        assert_eq!(pending_change(&driven, leader), proposed);
        let configuration = applied_configuration(&driven, leader);
        assert_eq!(
            (configuration.voters, configuration.learners),
            (vec![1, 2, 3], vec![4])
        );
        drop(written);
        pump.finish();
    }

    /// AC-T1.8.A1 (CP-T1.8) on driven time (replication.md, "Raising the floor", d): binding below
    /// the committed floor fails closed at once. Do, in a group of three voters bound to
    /// kiln.host-store 7 with joins tracked: raise the floor to 7; then a follower binds
    /// kiln.host-store 6. Expect: Invalid at once, naming the binary's highest and the floor; no
    /// proposal made, and the follower's attestation still 7 on every voter.
    #[test]
    fn a_bind_below_the_committed_floor_is_refused_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = three_and_a_learner(&mut driven, dir.path());
        let mut pump = Pump::new(&clients);
        let leader = tracked_group(&mut driven, &mut pump, &clients);
        assert_eq!(raise(&mut driven, &mut pump, &clients[leader], 7), Ok(7));
        let follower = (leader + 1) % 3;
        for _ in 0..ROUNDS {
            if (0..3).all(|owner| floor_version(&mut driven, owner) == Some(7)) {
                break;
            }
            driven.round(&mut pump);
        }
        let member = u64::try_from(follower + 1).unwrap();
        let proposed = driven.owner_mut(follower).floor_attestations;

        let refused = clients[follower].handle().attest(kiln(6));
        assert_eq!(
            refused,
            Err(Refusal::Invalid {
                what: "application floor".into(),
                why: "this binary runs up to 6; the group's floor is 7".into(),
            })
        );
        for _ in 0..64 {
            driven.round(&mut pump);
        }
        assert_eq!(driven.owner_mut(follower).floor_attestations, proposed);
        for owner in 0..3 {
            assert_eq!(attested(&mut driven, owner, member).unwrap().highest, 7);
        }
        driven.drain(&mut pump);
        pump.finish();
    }

    /// The floor's version owner `owner` holds, if one is installed.
    fn floor_version(driven: &mut Driven, owner: usize) -> Option<u32> {
        driven
            .owner_mut(owner)
            .replica
            .machine_mut()
            .read_db(|db| crate::database::application::policy(db))
            .unwrap()
            .map(|floor| floor.version)
    }

    /// Enqueues a raise of kiln.host-store's floor to 7 at `client`, by a handle bound to it,
    /// without waiting.
    fn raise_at(client: &Client, until: Instant) -> mpsc::Receiver<Result<Answer, Refusal>> {
        let operation = Operation::Call(Mutex::new(Some(Box::new(|db: &mut Db| {
            let floor = crate::database::application::raise(db, kiln(7), true)?
                .map_err(|refused| anyhow::Error::from(refused.refusal))?;
            Ok(Box::new(floor.version) as Box<dyn Any + Send>)
        }))));
        let (response, raised) = mpsc::sync_channel(1);
        client
            .handle
            .enqueue(
                Input::Submit {
                    namespace: String::new(),
                    operation,
                    application: Some(kiln(7)),
                    until,
                    response,
                },
                api::REQUEST_MAX,
            )
            .unwrap();
        raised
    }

    /// AC-T1.8.A1 (CP-T1.8) on driven time (replication.md, "Raising the floor", b;
    /// amphora-seam-owner's ruling on step 4, a): a raise another lead's entry writes over is
    /// answered UnknownOutcome, which writer_floor passes on as it is, and applies nowhere. Do, in
    /// a group of three voters bound to kiln.host-store 7 with joins tracked: the leader proposes
    /// a raise to 7 whose records are lost; the other two, their clocks past an election timeout,
    /// elect one of them; then the new leader raises to 7. Expect: UnknownOutcome; no floor on any
    /// voter; the new leader's raise Ok(7).
    #[test]
    fn a_raise_another_lead_wrote_over_is_answered_unknown_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = three_and_a_learner(&mut driven, dir.path());
        let mut pump = Pump::new(&clients);
        let leader = tracked_group(&mut driven, &mut pump, &clients);
        driven.drain(&mut pump);
        let raised = raise_at(&clients[leader], driven.until());
        reach(&mut driven, &mut pump, "the raise proposed", |driven| {
            matches!(
                driven
                    .owner(leader)
                    .pending
                    .as_ref()
                    .map(|pending| &pending.phase),
                Some(Phase::Proposed(_))
            )
        });
        // What goes on is what the proposal's round hadn't sent: its entry is lost with this.
        clients[leader].handle().drain(OUTGOING_MESSAGES).unwrap();
        let others: Vec<usize> = (0..3).filter(|owner| *owner != leader).collect();
        let answered = (0..ROUNDS)
            .find_map(|_| {
                for owner in &others {
                    driven.advance_tick(*owner);
                }
                driven.round(&mut pump);
                raised.try_recv().ok()
            })
            .unwrap();
        let refusal = answered.err().unwrap();
        assert_eq!(refusal, Refusal::UnknownOutcome);
        assert_eq!(
            crate::database::application::overtaken(refusal),
            Refusal::UnknownOutcome
        );
        settled(&mut driven, &mut pump, 3, &[]);
        for owner in 0..3 {
            assert_eq!(floor_version(&mut driven, owner), None, "owner {owner}");
        }
        let status = clients[others[0]].handle().status().unwrap();
        let led = usize::try_from(status.leader - 1).unwrap();
        assert_eq!(raise(&mut driven, &mut pump, &clients[led], 7), Ok(7));
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.A1 (CP-T1.8) on driven time (amphora-seam-owner's review of step 4, D1): a
    /// compaction the floor refuses is answered to its caller and the owner keeps serving. Do, in
    /// a group of three voters with the floor at kiln.host-store 7: a compaction step from a
    /// handle with no claim; then a write as 7. Expect: Fenced, "application writer contract
    /// required"; the write applied; no member fenced.
    #[test]
    fn a_compaction_the_floor_refuses_leaves_the_owner_serving() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = three_and_a_learner(&mut driven, dir.path());
        let mut pump = Pump::new(&clients);
        let leader = tracked_group(&mut driven, &mut pump, &clients);
        assert_eq!(raise(&mut driven, &mut pump, &clients[leader], 7), Ok(7));
        let step = Operation::Mutation(Mutation::CompactStep {
            namespace: "gc".into(),
            before: 2,
        });
        let answer = enqueue(&clients[leader], driven.until(), "gc", step);
        let refused = driven.answer(&mut pump, &answer).err().unwrap();
        assert_eq!(
            refused,
            Refusal::Fenced("application writer contract required".into())
        );
        write_as(&mut driven, &mut pump, &clients[leader], 7).unwrap();
        for client in &clients[..3] {
            assert_eq!(client.handle().status().unwrap().fenced, None);
        }
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.A1 (CP-T1.8) on driven time (replication.md, promise 3; amphora-seam-owner's
    /// review of step 4, D5): a raise to the floor in force or below answers it at once, with
    /// nothing to prove and nothing proposed. Do, in a group with the floor at kiln.host-store 7
    /// and learner 4 added, never attesting: raise to 7, then to 6. Expect: Ok(7) both times, and
    /// the leader's log no longer than before.
    #[test]
    fn a_raise_to_the_floor_or_below_answers_the_floor_in_force_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = three_and_a_learner(&mut driven, dir.path());
        let mut pump = Pump::new(&clients);
        let leader = floor_and_a_learner(&mut driven, &mut pump, &clients);
        quiet(&mut driven, &mut pump);
        let last = |driven: &Driven| {
            driven
                .owner(leader)
                .replica
                .core()
                .raft
                .log()
                .last_index()
                .unwrap()
        };
        let before = last(&driven);
        assert_eq!(raise(&mut driven, &mut pump, &clients[leader], 7), Ok(7));
        let lower = call_as(&mut driven, &mut pump, &clients[leader], kiln(7), |db| {
            crate::database::application::raise(db, kiln(6), true)?
                .map(|floor| floor.version)
                .map_err(|refused| anyhow::Error::from(refused.refusal))
        });
        assert_eq!(lower, Ok(7));
        assert_eq!(last(&driven), before);
        driven.drain(&mut pump);
        pump.finish();
    }

    /// CP-T1.8 on driven time (replication.md, "Metrics", "The seam"; CLAUDE.md section 10): the
    /// floor's metrics and floor_status say what happened. Do, in a group of three voters bound to
    /// kiln.host-store 7 with joins tracked: read every member's counters; raise to 7; add learner
    /// 4, never bound; then a raise to 8, a write as 6, a write with no claim and 4's promotion,
    /// each refused. Expect: before, every count 0; after the raise, version 7, one raise and no
    /// member unattested on every voter; with 4 added, one unattested and floor_status naming 4 a
    /// learner with no attestation, the voters at 7; each refusal counted under its reason at the
    /// member that made it; the keeper not failing.
    #[test]
    fn the_floors_metrics_and_status_say_what_happened() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = three_and_a_learner(&mut driven, dir.path());
        let mut pump = Pump::new(&clients);
        let leader = tracked_group(&mut driven, &mut pump, &clients);
        for client in &clients[..3] {
            assert_eq!(
                client.handle().floor_counters(),
                crate::meter::FloorCounters::default()
            );
        }
        assert_eq!(raise(&mut driven, &mut pump, &clients[leader], 7), Ok(7));
        for _ in 0..ROUNDS {
            if clients[..3]
                .iter()
                .all(|client| client.handle().floor_counters().version == 7)
            {
                break;
            }
            driven.round(&mut pump);
        }
        for client in &clients[..3] {
            let counters = client.handle().floor_counters();
            assert_eq!(
                (
                    counters.version,
                    counters.raises,
                    counters.unattested_members
                ),
                (7, 1, 0)
            );
        }
        let until = driven.until();
        driven
            .answer(
                &mut pump,
                &change(&clients[leader], until, MemberChange::AddLearner(4)),
            )
            .unwrap();
        settled(&mut driven, &mut pump, 3, &[4]);
        let led = clients[leader].handle();
        assert_eq!(led.floor_counters().unattested_members, 1);
        let status = led.floor_status().unwrap();
        assert_eq!(
            (status.application.as_deref(), status.version),
            (Some("kiln.host-store"), 7)
        );
        for member in &status.members {
            let expected = if member.member == 4 {
                (false, None)
            } else {
                (true, Some(7))
            };
            assert_eq!((member.voter, member.attested), expected, "{member:?}");
        }
        assert_eq!(status.members.len(), 4);

        // Each refusal site counts its own reason, once, where it refuses (amphora-seam-owner's
        // review of efc1248): what each moved, and nothing else.
        let moved = |before: &FloorCounters, after: &FloorCounters| -> Vec<(FloorRefusal, u64)> {
            FloorRefusal::ALL
                .into_iter()
                .filter(|reason| after.refused(*reason) != before.refused(*reason))
                .map(|reason| (reason, after.refused(reason) - before.refused(reason)))
                .collect()
        };
        let follower = (leader + 1) % 3;
        let mut sites: Vec<(&str, usize, FloorRefusal, bool)> = Vec::new();
        let before = led.floor_counters();
        let refused = raise(&mut driven, &mut pump, &clients[leader], 8).is_err();
        sites.push(("the raise's proof", leader, FloorRefusal::Unproven, refused));
        assert_eq!(
            moved(&before, &led.floor_counters()),
            vec![(FloorRefusal::Unproven, 1)]
        );
        let before = led.floor_counters();
        let refused = write_as(&mut driven, &mut pump, &clients[leader], 6).is_err();
        assert_eq!(
            moved(&before, &led.floor_counters()),
            vec![(FloorRefusal::BelowFloor, 1)]
        );
        sites.push((
            "a write below the floor",
            leader,
            FloorRefusal::BelowFloor,
            refused,
        ));
        let before = led.floor_counters();
        let unclaimed = submit(&clients[leader], driven.until(), |db| {
            db.exec("CREATE TABLE IF NOT EXISTS floor_items(version)")?;
            Ok(Vec::new())
        });
        let refused = driven.answer(&mut pump, &unclaimed).is_err();
        assert_eq!(
            moved(&before, &led.floor_counters()),
            vec![(FloorRefusal::NoClaim, 1)]
        );
        sites.push(("an unclaimed write", leader, FloorRefusal::NoClaim, refused));
        let before = led.floor_counters();
        let other = crate::database::Application {
            name: "another.app".into(),
            version: 9,
        };
        let refused = call_as(&mut driven, &mut pump, &clients[leader], other, |db| {
            db.exec("CREATE TABLE IF NOT EXISTS floor_items(version)")?;
            Ok(())
        })
        .is_err();
        assert_eq!(
            moved(&before, &led.floor_counters()),
            vec![(FloorRefusal::OtherApplication, 1)]
        );
        sites.push((
            "another application's write",
            leader,
            FloorRefusal::OtherApplication,
            refused,
        ));
        let before = led.floor_counters();
        let compaction = enqueue(
            &clients[leader],
            driven.until(),
            "gc",
            Operation::Mutation(Mutation::CompactStep {
                namespace: "gc".into(),
                before: 2,
            }),
        );
        let refused = driven.answer(&mut pump, &compaction).is_err();
        assert_eq!(
            moved(&before, &led.floor_counters()),
            vec![(FloorRefusal::NoClaim, 1)]
        );
        sites.push((
            "an unclaimed compaction",
            leader,
            FloorRefusal::NoClaim,
            refused,
        ));
        let before = led.floor_counters();
        let refused = promote(&mut driven, &mut pump, &clients[leader]).is_err();
        assert_eq!(
            moved(&before, &led.floor_counters()),
            vec![(FloorRefusal::Unproven, 1)]
        );
        sites.push((
            "the promotion guard",
            leader,
            FloorRefusal::Unproven,
            refused,
        ));
        let bound = clients[follower].handle();
        let before = bound.floor_counters();
        let refused = bound.attest(kiln(6)).is_err();
        assert_eq!(
            moved(&before, &bound.floor_counters()),
            vec![(FloorRefusal::BelowFloor, 1)]
        );
        sites.push((
            "a bind below the floor",
            follower,
            FloorRefusal::BelowFloor,
            refused,
        ));
        for (site, owner, reason, refused) in &sites {
            assert!(
                refused,
                "{site} at owner {owner} was not refused ({reason:?})"
            );
        }
        let counters = led.floor_counters();
        assert_eq!(counters.keeper_failing, 0);
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.A1, T-T1.8.A7 (CP-T1.8) on driven time (replication.md, "The upgrade that
    /// introduces the join rows", point 2; amphora-seam-owner's ruling on the keeper's livelock,
    /// b'): a floor session prepares only once its member has applied its own whole log, so an
    /// entry appended ahead of it, not yet committed when its barrier passed, never makes it stale.
    /// Do, in a group of three voters bound to kiln.host-store 7 with joins tracked: every
    /// follower's durable answer to an append kept back, so a write at the leader is appended on
    /// every member and committed nowhere; a follower binds kiln.host-store 8, its keeper's barrier
    /// passing on heartbeats; then the answers let through. Expect: the follower's attestation of
    /// 8 applied, proposed once, never refused as stale.
    #[test]
    fn a_floor_session_waits_for_its_members_own_log_before_it_prepares() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = three_and_a_learner(&mut driven, dir.path());
        let mut pump = Pump::new(&clients);
        let leader = tracked_group(&mut driven, &mut pump, &clients);
        quiet(&mut driven, &mut pump);
        let follower = (leader + 1) % 3;
        let member = u64::try_from(follower + 1).unwrap();
        let proposed = driven.owner(follower).floor_attestations;
        pump.hold_durable();
        let operation = Operation::Call(Mutex::new(Some(Box::new(|db: &mut Db| {
            db.exec("CREATE TABLE IF NOT EXISTS ahead(x)")?;
            Ok(Box::new(()) as Box<dyn Any + Send>)
        }))));
        let (response, written) = mpsc::sync_channel(1);
        clients[leader]
            .handle
            .enqueue(
                Input::Submit {
                    namespace: String::new(),
                    operation,
                    application: Some(kiln(7)),
                    until: driven.until(),
                    response,
                },
                api::REQUEST_MAX,
            )
            .unwrap();
        reach(
            &mut driven,
            &mut pump,
            "the write appended on the follower",
            |driven| {
                let replica = &driven.owner(follower).replica;
                replica.core().raft.log().last_index().unwrap() > replica.applied().index
            },
        );
        clients[follower].handle().attest(kiln(8)).unwrap();
        reach(
            &mut driven,
            &mut pump,
            "the follower's keeper with a session in hand",
            |driven| driven.owner(follower).floor_inflight,
        );
        for _ in 0..64 {
            driven.round(&mut pump);
        }
        pump.let_durable();
        for _ in 0..ROUNDS {
            if attested(&mut driven, leader, member).is_some_and(|attested| attested.highest == 8) {
                break;
            }
            driven.round(&mut pump);
        }
        assert_eq!(attested(&mut driven, leader, member).unwrap().highest, 8);
        assert!(driven.answer(&mut pump, &written).is_ok());
        assert_eq!(
            driven.owner(follower).floor_attestations - proposed,
            1,
            "the follower's attestation was proposed again: its base missed the write ahead of it"
        );
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.A1 (CP-T1.8) on driven time (amphora-seam-owner's review of 392902d): a floor
    /// session's wait for its member's log has a target fixed when its barrier passes, so an entry
    /// appended after that never moves it, and the session never waits out its deadline holding the
    /// member's write slot through others' writes. Do, in a group of three voters bound to
    /// kiln.host-store 7 with joins tracked: a write at the leader appended on it and a follower
    /// and committed nowhere, the other voter's appends and every answer that would count the
    /// follower as holding it lost; the follower binds kiln.host-store 8, its session waiting for
    /// its log with that write ahead, its target the write's index; then its target set to what it
    /// has applied, as a barrier that passed with nothing ahead and an entry appended since would
    /// leave it, its log running past what it has applied; one step. Expect: the target the write's
    /// index while it waits; then the session prepared and proposed, its wait cleared; after an
    /// outage's time, the attestation applied.
    #[test]
    fn a_floor_session_waits_for_its_log_as_it_was_not_for_writes_behind_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = three_and_a_learner(&mut driven, dir.path());
        let mut pump = Pump::new(&clients);
        // The lost records may cost the lead; proposals dropped in that election are counted.
        pump.refusals = Some(0);
        let leader = tracked_group(&mut driven, &mut pump, &clients);
        quiet(&mut driven, &mut pump);
        let follower = (leader + 1) % 3;
        let member = u64::try_from(follower + 1).unwrap();
        let other = u64::try_from((leader + 2) % 3 + 1).unwrap();
        let tail = |driven: &Driven| {
            let replica = &driven.owner(follower).replica;
            (
                replica.applied().index,
                replica.core().raft.log().last_index().unwrap(),
            )
        };
        // The write ahead commits nowhere: the other voter never holds it, and no answer says the
        // follower does (a heartbeat's answer saying where its log ends counts as an append's).
        // The other voter's answers still confirm the follower's barrier.
        let (_, before) = tail(&driven);
        pump.losing = Some(Box::new(move |from, record| match record.msg_type {
            hyper_raft::proto::MessageType::MsgAppend => record.to == other,
            hyper_raft::proto::MessageType::MsgAppendResponse => true,
            hyper_raft::proto::MessageType::MsgHeartbeatResponse => {
                from == member && record.index > before
            }
            _ => false,
        }));
        let operation = Operation::Call(Mutex::new(Some(Box::new(|db: &mut Db| {
            db.exec("CREATE TABLE IF NOT EXISTS ahead(x)")?;
            Ok(Box::new(()) as Box<dyn Any + Send>)
        }))));
        let (response, written) = mpsc::sync_channel(1);
        clients[leader]
            .handle
            .enqueue(
                Input::Submit {
                    namespace: String::new(),
                    operation,
                    application: Some(kiln(7)),
                    until: driven.until(),
                    response,
                },
                api::REQUEST_MAX,
            )
            .unwrap();
        reach(
            &mut driven,
            &mut pump,
            "the write appended ahead",
            |driven| {
                let (applied, last) = tail(driven);
                last > applied
            },
        );
        let (_, ahead) = tail(&driven);
        let proposed = driven.owner(follower).floor_attestations;
        clients[follower].handle().attest(kiln(8)).unwrap();
        reach(
            &mut driven,
            &mut pump,
            "the follower's session waiting for its log",
            |driven| driven.owner(follower).tail.is_some(),
        );
        assert_eq!(driven.owner(follower).tail, Some(ahead));
        let (applied, last) = tail(&driven);
        assert!(last > applied, "the write ahead already applied");
        driven.owner_mut(follower).tail = Some(applied);
        driven.step();
        let owner = driven.owner(follower);
        assert_eq!(
            owner.tail, None,
            "the session still waits: its target moved"
        );
        let (applied, last) = tail(&driven);
        assert!(last > applied, "the write ahead applied");
        assert!(
            owner.floor_attestations > proposed
                || matches!(
                    owner.pending.as_ref().map(|pending| &pending.phase),
                    Some(Phase::Proposed(_))
                ),
            "the session did not prepare at its target"
        );
        // Time passes, as after an outage: what was lost is sent again or the group elects, and
        // the follower's attestation applies.
        pump.losing = None;
        let led = settle_lead(&mut driven, &mut pump, &clients);
        for _ in 0..ROUNDS {
            if attested(&mut driven, led, member).is_some_and(|attested| attested.highest == 8) {
                break;
            }
            for owner in 0..3 {
                driven.advance_tick(owner);
            }
            driven.round(&mut pump);
        }
        // Its attestation, prepared at a base the write behind it moves past, may be refused by
        // the stale-base rule and made again; it applies all the same.
        assert_eq!(attested(&mut driven, led, member).unwrap().highest, 8);
        // The write ahead is answered as the group decided: applied, or unknown after a new lead.
        drop(written);
        driven.drain(&mut pump);
        pump.finish();
    }

    /// How often T-T1.8.A6 reached each falsifier's situation, over all its runs (non-vacuity):
    /// each must be above 0, else that falsifier was asserted over nothing.
    #[derive(Debug, Default)]
    struct Reached {
        below_floor_write_refused: u64,
        write_at_floor_applied: u64,
        raise_applied: u64,
        raise_refused_unproven: u64,
        floor_checked_on_every_member: u64,
        promotion_applied: u64,
        promotion_refused: u64,
        bind_below_floor_refused: u64,
        hostile_forward_refused: u64,
        earlier_join_held_a_raise: u64,
        join_rows_compared: u64,
        tail_refused_at_once: u64,
        raise_before_joins_refused: u64,
        digests_compared: u64,
        stale_attestation_held_joins: u64,
        leader_crashed: u64,
        lead_handed_off: u64,
        records_lost: u64,
    }
    impl Reached {
        fn unreached(&self) -> Vec<&'static str> {
            [
                ("below_floor_write_refused", self.below_floor_write_refused),
                ("write_at_floor_applied", self.write_at_floor_applied),
                ("raise_applied", self.raise_applied),
                ("raise_refused_unproven", self.raise_refused_unproven),
                (
                    "floor_checked_on_every_member",
                    self.floor_checked_on_every_member,
                ),
                ("promotion_applied", self.promotion_applied),
                ("promotion_refused", self.promotion_refused),
                ("bind_below_floor_refused", self.bind_below_floor_refused),
                ("hostile_forward_refused", self.hostile_forward_refused),
                ("earlier_join_held_a_raise", self.earlier_join_held_a_raise),
                ("join_rows_compared", self.join_rows_compared),
                ("tail_refused_at_once", self.tail_refused_at_once),
                (
                    "raise_before_joins_refused",
                    self.raise_before_joins_refused,
                ),
                ("digests_compared", self.digests_compared),
                (
                    "stale_attestation_held_joins",
                    self.stale_attestation_held_joins,
                ),
                ("leader_crashed", self.leader_crashed),
                ("lead_handed_off", self.lead_handed_off),
                ("records_lost", self.records_lost),
            ]
            .into_iter()
            .filter(|(_, count)| *count == 0)
            .map(|(name, _)| name)
            .collect()
        }
    }

    /// Rounds until every voter of the group's applied configuration is out of any joint
    /// configuration with no change of members still to apply.
    fn calm(driven: &mut Driven, pump: &mut Pump<'_>, owners: usize) {
        reach(driven, pump, "the group calm", |driven| {
            (0..owners).all(|owner| {
                let applied = applied_configuration(driven, owner);
                applied.voters_outgoing.is_empty()
                    && applied.learners_next.is_empty()
                    && !driven.owner(owner).replica.core().raft.has_pending_conf()
            })
        });
    }
    /// The member the group follows, as owner `asked` sees it, by its index; None while it knows
    /// none.
    fn leading(clients: &[Client], asked: usize) -> Option<usize> {
        let leader = clients[asked].handle().status().ok()?.leader;
        if leader == 0 {
            None
        } else {
            Some(usize::try_from(leader - 1).unwrap())
        }
    }
    /// Time passes, every clock alike, until the first three members name one leader and it
    /// leads; its index. What follows an election or an outage, as real time would bring it.
    fn settle_lead(driven: &mut Driven, pump: &mut Pump<'_>, clients: &[Client]) -> usize {
        for _ in 0..ROUNDS {
            let named: Vec<Option<usize>> = (0..3).map(|owner| leading(clients, owner)).collect();
            if let Some(Some(leader)) = named.first()
                && named.iter().all(|seen| *seen == Some(*leader))
                && leading(clients, *leader) == Some(*leader)
            {
                return *leader;
            }
            for owner in 0..4 {
                driven.advance_tick(owner);
            }
            driven.round(pump);
        }
        panic!("no lead settled in {ROUNDS} driven rounds");
    }
    /// A write into a6_items at `client` by a handle bound to kiln.host-store at `version`,
    /// recording the floor at the session's base beside it.
    fn write_recording_floor(
        driven: &mut Driven,
        pump: &mut Pump<'_>,
        client: &Client,
        version: u32,
    ) -> Result<(), Refusal> {
        call_as(driven, pump, client, kiln(version), move |db| {
            let floor = crate::database::installed_floor(db)?.map_or(0, |floor| floor.version);
            db.exec("CREATE TABLE IF NOT EXISTS a6_items(version, floor)")?;
            db.exec(&format!("INSERT INTO a6_items VALUES({version}, {floor})"))?;
            Ok(())
        })
    }
    /// Every (version, floor at its base) row owner `owner` holds in a6_items.
    fn recorded_writes(driven: &mut Driven, owner: usize) -> Vec<(u32, u32)> {
        driven
            .owner_mut(owner)
            .replica
            .machine_mut()
            .read_db(|db| {
                let mut present = false;
                db.query(
                    "SELECT 1 FROM sqlite_schema WHERE name = 'a6_items'",
                    [],
                    |_| {
                        present = true;
                        Ok(false)
                    },
                )?;
                let mut rows = Vec::new();
                if present {
                    db.query("SELECT version, floor FROM a6_items", [], |row| {
                        rows.push((
                            u32::try_from(row.get::<i64>(0).map_err(unavailable)?)?,
                            u32::try_from(row.get::<i64>(1).map_err(unavailable)?)?,
                        ));
                        Ok(true)
                    })?;
                }
                Ok(rows)
            })
            .unwrap()
    }
    /// Forwards to `leader` a proposal `from` prepared, its context naming `named`; answers the
    /// leader's admission.
    fn forward_naming(
        driven: &mut Driven,
        clients: &[Client],
        leader: usize,
        from: usize,
        named: usize,
    ) -> Result<(), Refusal> {
        let id = |owner: usize| u64::try_from(owner + 1).unwrap();
        // The leader refuses it on its context, before its data: an opaque payload, which no
        // member could apply, would fail the run loudly if one were taken.
        let mut context = id(named).to_le_bytes().to_vec();
        context.extend_from_slice(&[7; 24]);
        let mut record =
            hyper_raft::proto::message(id(leader), hyper_raft::proto::MessageType::MsgPropose);
        record.from = id(from);
        record.entries = vec![hyper_raft::proto::Entry {
            data: b"hostile".to_vec(),
            context,
            ..hyper_raft::proto::Entry::default()
        }];
        let mut bytes = Vec::new();
        record.encode(&mut bytes);
        let mut receipt = clients[leader]
            .handle()
            .admit_receive(id(from), bytes, driven.until())
            .unwrap();
        for _ in 0..ROUNDS {
            driven.step();
            match receipt.try_result() {
                Ok(None) => {}
                Ok(Some(())) => return Ok(()),
                Err(refusal) => return Err(refusal),
            }
        }
        panic!("the forward was never answered");
    }

    /// AC-T1.8.A1, T-T1.8.A6 (CP-T1.8) under a nemesis (replication.md, "The application floor,
    /// replicated", its falsifiers): every falsifier of the floor, as an assertion over every run,
    /// over many seeds. Do, per seed, in a group of three voters and member 4: before joins are
    /// tracked, a raise, and member 4 attesting, removed and added again while unbound, the voters
    /// binding; then 4 binding, which lets J through. Then seeded steps: writes as 6 to 9 at a
    /// voter, recording the floor at their base; raises; binds at 6 to 9; 4 added, promoted,
    /// demoted, removed and added again; forwards naming another member; under elections (the
    /// lead handed to another voter), records to a follower lost, and once the leader's crash and
    /// restart. Last, a promotion beside a write in flight. Expect, against each falsifier: no
    /// write below the floor at its base applied anywhere; no raise applied while a member of the
    /// configuration lacks an attestation at its version; no member's floor falling, and members'
    /// image digests equal at every applied index they share (so their floors and join rows too);
    /// no promotion without an attestation at the floor; no bind below the floor; no forward naming
    /// another member taken; a member added again holding raises back until it attests under its new
    /// join; members' join rows equal once calm; the tail refused at once naming both indexes; before
    /// J, no join rows, a raise refused, and a member's attestation from before its re-add not
    /// letting J through. And every situation reached at least once over the runs.
    #[test]
    fn every_falsifier_of_the_floor_holds_under_a_nemesis() {
        const SEEDS: u64 = 12;
        const STEPS: u64 = 28;
        let mut reached = Reached::default();
        for seed in 1..=SEEDS {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let dir = tempfile::tempdir().unwrap();
            let mut driven = Driven::new();
            let mut clients = three_and_a_learner(&mut driven, dir.path());
            for owner in 0..4 {
                driven
                    .owner_mut(owner)
                    .replica
                    .machine_mut()
                    .record_digests();
            }
            let mut pump = Pump::new(&clients);
            // Elections drop proposals in flight to a leader handing its lead over; counted, not
            // failed (as in T-T1.8.A2).
            pump.refusals = Some(0);
            let mut leader = driven.elect(&mut pump, &clients[..3], 0, 1);
            let id = |owner: usize| u64::try_from(owner + 1).unwrap();

            // Before J. A raise is refused, and no member holds join rows.
            let early = raise(&mut driven, &mut pump, &clients[leader], 7);
            assert!(
                matches!(&early, Err(Refusal::Unavailable(why)) if why.starts_with("joins aren't tracked")),
                "seed {seed}: {early:?}"
            );
            reached.raise_before_joins_refused += 1;
            // 4 joins and attests, then is removed and added again while no longer bound: its
            // attestation predates the last change, and must not let J through.
            let until = driven.until();
            driven
                .answer(
                    &mut pump,
                    &change(&clients[leader], until, MemberChange::AddLearner(4)),
                )
                .unwrap();
            calm(&mut driven, &mut pump, 3);
            let joined_first = driven.owner(leader).replica.applied().index;
            clients[3].handle().attest(kiln(7)).unwrap();
            for _ in 0..ROUNDS {
                if attested(&mut driven, leader, 4).is_some() {
                    break;
                }
                driven.round(&mut pump);
            }
            pump.departed.push(4);
            let until = driven.until();
            driven
                .answer(
                    &mut pump,
                    &change(&clients[leader], until, MemberChange::Remove(4)),
                )
                .unwrap();
            calm(&mut driven, &mut pump, 3);
            driven.owner_mut(3).attesting = None;
            clients[3].handle.shared.attesting.lock().unwrap().take();
            pump.departed.clear();
            let until = driven.until();
            driven
                .answer(
                    &mut pump,
                    &change(&clients[leader], until, MemberChange::AddLearner(4)),
                )
                .unwrap();
            calm(&mut driven, &mut pump, 3);
            let mut voters_bound = [0usize, 1, 2];
            for i in 0..3 {
                let j = usize::try_from(rng.below(3)).unwrap();
                voters_bound.swap(i, j);
            }
            for owner in voters_bound {
                clients[owner].handle().attest(kiln(7)).unwrap();
                for _ in 0..=rng.below(40) {
                    driven.round(&mut pump);
                }
            }
            for _ in 0..200 {
                driven.round(&mut pump);
            }
            for owner in 0..4 {
                assert_eq!(
                    tracked(&mut driven, owner),
                    None,
                    "seed {seed}: J with 4 stale"
                );
                assert!(
                    !floor_rows(&mut driven, owner)
                        .iter()
                        .any(|row| row.starts_with("M ")),
                    "seed {seed}: a join row before J on owner {owner}"
                );
            }
            reached.stale_attestation_held_joins += 1;
            clients[3].handle().attest(kiln(7)).unwrap();
            joins_tracked(&mut driven, &mut pump, &[0, 1, 2, 3]);
            reached.digests_compared += u64::try_from(digests_agree(
                &driven,
                &[(0, 0), (1, 0), (2, 0), (3, joined_first)],
            ))
            .unwrap();

            // After J: the nemesis.
            let mut floors = [0u32; 4];
            let mut crashed = false;
            let mut four = Some(false); // None: not a member; Some(false): learner; Some(true): voter
            let mut four_attested = true;
            for step in 0..STEPS {
                leader = settle_lead(&mut driven, &mut pump, &clients);
                let voter = usize::try_from(rng.below(3)).unwrap();
                let floor_at =
                    |driven: &mut Driven, owner: usize| floor_version(driven, owner).unwrap_or(0);
                let action = rng.below(8);
                match action {
                    0 | 1 => {
                        let version = u32::try_from(6 + rng.below(4)).unwrap();
                        let before = floor_at(&mut driven, voter);
                        match write_recording_floor(
                            &mut driven,
                            &mut pump,
                            &clients[voter],
                            version,
                        ) {
                            Ok(()) => {
                                assert!(
                                    version >= before,
                                    "seed {seed} step {step}: {version} below {before} applied"
                                );
                                reached.write_at_floor_applied += 1;
                            }
                            Err(Refusal::Fenced(why))
                                if why.contains("older than the installed floor") =>
                            {
                                reached.below_floor_write_refused += 1;
                            }
                            Err(_) => {}
                        }
                    }
                    2 => {
                        let version = u32::try_from(7 + rng.below(3)).unwrap();
                        let before = floor_at(&mut driven, voter);
                        match raise(&mut driven, &mut pump, &clients[voter], version) {
                            // At or below the floor in force it answers that floor, writing
                            // nothing: no floor entry applies, so there is nothing to prove.
                            Ok(raised) if raised <= before => {}
                            Ok(raised) => {
                                quiet(&mut driven, &mut pump);
                                let status = clients[voter].handle().floor_status().unwrap();
                                for member in &status.members {
                                    assert!(
                                        member.attested.is_some_and(|highest| highest >= raised),
                                        "seed {seed} step {step}: raised to {raised} past {member:?}"
                                    );
                                }
                                reached.raise_applied += 1;
                            }
                            Err(Refusal::Unavailable(why)) if why.starts_with("members [") => {
                                reached.raise_refused_unproven += 1;
                                if four.is_some() && !four_attested && why.contains('4') {
                                    reached.earlier_join_held_a_raise += 1;
                                }
                            }
                            Err(_) => {}
                        }
                    }
                    3 => {
                        let owner = usize::try_from(rng.below(4)).unwrap();
                        let version = u32::try_from(6 + rng.below(4)).unwrap();
                        let published = clients[owner].handle().floor_counters().version;
                        let bound = clients[owner].handle().attest(kiln(version));
                        if u64::from(version) < published {
                            assert!(
                                matches!(&bound, Err(Refusal::Invalid { what, .. }) if what == "application floor"),
                                "seed {seed} step {step}: bound {version} below {published}: {bound:?}"
                            );
                            reached.bind_below_floor_refused += 1;
                        } else if owner == 3 && bound.is_ok() {
                            four_attested = true;
                        }
                    }
                    4 => {
                        if leader == 3 {
                            continue;
                        }
                        let until = driven.until();
                        let asked = match four {
                            None => MemberChange::AddLearner(4),
                            Some(false) if rng.below(2) == 0 => MemberChange::Promote(4),
                            Some(false) => MemberChange::Remove(4),
                            Some(true) => MemberChange::Demote(4),
                        };
                        if matches!(asked, MemberChange::Remove(4)) {
                            pump.departed.push(4);
                        }
                        let answered =
                            driven.answer(&mut pump, &change(&clients[leader], until, asked));
                        calm(&mut driven, &mut pump, 3);
                        let configuration = applied_configuration(&driven, leader);
                        four = if configuration.voters.contains(&4) {
                            Some(true)
                        } else if configuration.learners.contains(&4) {
                            Some(false)
                        } else {
                            None
                        };
                        match (asked, &answered) {
                            (MemberChange::Promote(_), Ok(_)) if four == Some(true) => {
                                let status = clients[leader].handle().floor_status().unwrap();
                                let fourth = status.members.iter().find(|m| m.member == 4).unwrap();
                                assert!(
                                    status.version == 0
                                        || fourth
                                            .attested
                                            .is_some_and(|highest| highest >= status.version),
                                    "seed {seed} step {step}: promoted {fourth:?} under {}",
                                    status.version
                                );
                                reached.promotion_applied += 1;
                            }
                            (MemberChange::Promote(_), Err(_)) => reached.promotion_refused += 1,
                            (MemberChange::Remove(_), _) if four.is_none() => {
                                driven.owner_mut(3).attesting = None;
                                clients[3].handle.shared.attesting.lock().unwrap().take();
                                four_attested = false;
                            }
                            _ => {}
                        }
                        // What 4 sends counts only while it is a member.
                        pump.departed.clear();
                        if four.is_none() {
                            pump.departed.push(4);
                        }
                    }
                    5 => {
                        let follower = (leader + 1 + usize::try_from(rng.below(2)).unwrap()) % 3;
                        let other = (0..3).find(|o| *o != leader && *o != follower).unwrap();
                        if leader < 3 {
                            let forwarded =
                                forward_naming(&mut driven, &clients, leader, follower, other);
                            assert!(
                                forwarded.is_err(),
                                "seed {seed} step {step}: a forward naming {other} taken"
                            );
                            reached.hostile_forward_refused += 1;
                        }
                    }
                    6 => {
                        let to = (leader + 1 + usize::try_from(rng.below(2)).unwrap()) % 3;
                        if leader < 3 && to != leader {
                            leader = driven.hand_off(&mut pump, &clients[..3], leader, to);
                            reached.lead_handed_off += 1;
                        }
                    }
                    _ => {
                        if !crashed && leader < 3 && rng.below(2) == 0 {
                            driven.drain(&mut pump);
                            pump.finish();
                            let configuration = applied_configuration(&driven, leader);
                            let restarted = driven.restart(
                                dir.path(),
                                leader,
                                configuration.voters.clone(),
                                configuration.learners.clone(),
                            );
                            *clients.get_mut(leader).unwrap() = restarted;
                            driven
                                .owner_mut(leader)
                                .replica
                                .machine_mut()
                                .record_digests();
                            pump = Pump::new(&clients);
                            pump.refusals = Some(0);
                            if four.is_none() {
                                pump.departed.push(4);
                            }
                            // Time passes, as it does after a crash: the others' leases on the old
                            // lead run out, and the group elects among its voters, four or three.
                            leader = settle_lead(&mut driven, &mut pump, &clients);
                            crashed = true;
                            reached.leader_crashed += 1;
                        } else {
                            let follower = (leader + 1) % 3;
                            pump.lost.push(id(follower));
                            for _ in 0..=rng.below(20) {
                                driven.round(&mut pump);
                            }
                            pump.lost.clear();
                            // Time passes, as it does after an outage: a session whose records
                            // were lost ends at its deadline, and its owner tries again.
                            for _ in 0..ROUNDS {
                                if (0..4).all(|owner| driven.owner(owner).pending.is_none()) {
                                    break;
                                }
                                for owner in 0..4 {
                                    driven.advance_tick(owner);
                                }
                                driven.round(&mut pump);
                            }
                            reached.records_lost += 1;
                        }
                    }
                }
                for (owner, seen) in floors.iter_mut().enumerate().take(3) {
                    let floor = floor_version(&mut driven, owner).unwrap_or(0);
                    assert!(
                        floor >= *seen,
                        "seed {seed} step {step}: owner {owner}'s floor fell from {seen} to {floor}"
                    );
                    *seen = floor;
                }
                reached.floor_checked_on_every_member += 1;
            }

            // Calm: join rows equal, every write recorded at or above the floor at its base, and
            // members' digests equal at every applied index they share.
            quiet(&mut driven, &mut pump);
            calm(&mut driven, &mut pump, 3);
            let rows = floor_rows(&mut driven, 0);
            for owner in 1..3 {
                assert_eq!(
                    floor_rows(&mut driven, owner),
                    rows,
                    "seed {seed}: owner {owner}'s rows"
                );
            }
            reached.join_rows_compared += 1;
            for owner in 0..3 {
                for (version, floor) in recorded_writes(&mut driven, owner) {
                    assert!(
                        version >= floor,
                        "seed {seed}: owner {owner} applied {version} under {floor}"
                    );
                }
            }
            reached.digests_compared +=
                u64::try_from(digests_agree(&driven, &[(0, 0), (1, 0), (2, 0)])).unwrap();

            // Last, a promotion beside a write in flight is refused at once, naming both indexes.
            if four.is_none() && leader < 3 {
                let until = driven.until();
                let _ = driven.answer(
                    &mut pump,
                    &change(&clients[leader], until, MemberChange::AddLearner(4)),
                );
                calm(&mut driven, &mut pump, 3);
                four = Some(false);
            }
            let mut in_flight = false;
            if four == Some(false) && leader < 3 && floor_version(&mut driven, leader).is_some() {
                quiet(&mut driven, &mut pump);
                pump.appends_lost = true;
                // Claimed at 9, the highest floor a run raises, so only the tail holds it back.
                let operation = Operation::Call(Mutex::new(Some(Box::new(|db: &mut Db| {
                    db.exec("CREATE TABLE IF NOT EXISTS in_flight(x)")?;
                    Ok(Box::new(()) as Box<dyn Any + Send>)
                }))));
                let (response, written) = mpsc::sync_channel(1);
                clients[leader]
                    .handle
                    .enqueue(
                        Input::Submit {
                            namespace: String::new(),
                            operation,
                            application: Some(kiln(9)),
                            until: driven.until(),
                            response,
                        },
                        api::REQUEST_MAX,
                    )
                    .unwrap();
                let tail = |driven: &Driven| {
                    let replica = &driven.owner(leader).replica;
                    (
                        replica.applied().index,
                        replica.core().raft.log().last_index().unwrap(),
                    )
                };
                reach(&mut driven, &mut pump, "the write appended", |driven| {
                    let (applied, last) = tail(driven);
                    last > applied
                });
                let (applied, last) = tail(&driven);
                // At once, Unavailable, naming both indexes as the guard read them: the log past
                // what is applied, and no shorter than when the write was seen appended.
                let refused = promote(&mut driven, &mut pump, &clients[leader]);
                let named = match &refused {
                    Err(Refusal::Unavailable(why)) => why
                        .strip_prefix("the log runs to ")
                        .and_then(|rest| {
                            rest.strip_suffix(" is applied; promote once the tail is applied")
                        })
                        .and_then(|rest| rest.split_once(" and "))
                        .and_then(|(l, a)| Some((l.parse::<u64>().ok()?, a.parse::<u64>().ok()?))),
                    _ => None,
                };
                assert!(
                    named.is_some_and(|(l, a)| l > a && l >= last && a >= applied),
                    "seed {seed}: {refused:?} (seen appended at {last}, applied {applied})"
                );
                reached.tail_refused_at_once += 1;
                // The write stays in flight: its appends went nowhere, and nothing ticks to send
                // them again. The run ends here, as the step-5 test does.
                drop(written);
                in_flight = true;
            }
            if !in_flight {
                driven.drain(&mut pump);
            }
            pump.finish();
        }
        // The run's record: how often each falsifier's situation was reached (non-vacuity).
        eprintln!("T-T1.8.A6 over {SEEDS} seeds of {STEPS} steps reached: {reached:#?}");
        let unreached = reached.unreached();
        assert!(
            unreached.is_empty(),
            "situations never reached: {unreached:?} in {reached:?}"
        );
    }

    /// AC-T1.8.A1, T-T1.8.A2 (CP-T1.8) on driven time (replication.md, "Raising the floor", b;
    /// amphora-seam-owner's ruling on step 4, a): a raise a change overtakes is refused, typed, by
    /// the stale-base rule, never answered UnknownOutcome. Do, in a group of three voters bound to
    /// kiln.host-store 7 with joins tracked: the leader appends the addition of learner 5 (never
    /// running), every append lost while heartbeats go on, so the change can't commit; a follower
    /// raises the floor to 7, prepared at a base without the change and forwarded; then the appends
    /// go again. Expect: the raise Invalid, "prepared transaction", "the database advanced since
    /// preparation", which writer_floor answers Unavailable, to be asked again; the change
    /// applied; and no floor on any voter.
    #[test]
    fn a_raise_a_change_overtakes_is_refused_by_the_stale_base_rule() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = three_and_a_learner(&mut driven, dir.path());
        let mut pump = Pump::new(&clients);
        pump.lost.push(5);
        let leader = tracked_group(&mut driven, &mut pump, &clients);
        let follower = (leader + 1) % 3;
        let proposed = pending_change(&driven, leader);
        let until = driven.until();
        let added = change(&clients[leader], until, MemberChange::AddLearner(5));
        reach(&mut driven, &mut pump, "the change appended", |driven| {
            pending_change(driven, leader) > proposed
        });
        pump.appends_lost = true;
        let raised = raise_at(&clients[follower], driven.until());
        for _ in 0..ROUNDS {
            if matches!(
                driven
                    .owner_mut(follower)
                    .pending
                    .as_ref()
                    .map(|pending| &pending.phase),
                Some(Phase::Proposed(_))
            ) {
                break;
            }
            driven.round(&mut pump);
        }
        assert!(
            matches!(
                driven
                    .owner_mut(follower)
                    .pending
                    .as_ref()
                    .map(|pending| &pending.phase),
                Some(Phase::Proposed(_))
            ),
            "the raise was never forwarded"
        );
        assert!(applied_configuration(&driven, follower).learners.is_empty());

        pump.appends_lost = false;
        let refused = driven.answer(&mut pump, &raised).err().unwrap();
        assert_eq!(
            refused,
            Refusal::Invalid {
                what: "prepared transaction".into(),
                why: "the database advanced since preparation".into(),
            }
        );
        // What writer_floor answers its caller for it: ask again.
        assert_eq!(
            crate::database::application::overtaken(refused),
            Refusal::Unavailable(
                "the database advanced since preparation: the group changed while the raise was proven; raise again".into()
            )
        );

        driven.answer(&mut pump, &added).unwrap();
        settled(&mut driven, &mut pump, 3, &[5]);
        for owner in 0..3 {
            assert_eq!(floor_version(&mut driven, owner), None, "owner {owner}");
        }
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.A1 (CP-T1.8) on driven time (replication.md, "Raising the floor", b): a raise whose
    /// base holds a joint configuration is refused before it proposes. Do, in a group of three
    /// voters bound to kiln.host-store 7 with joins tracked: add learner 4, and once the leader has
    /// applied the joint entry, lose every append so its way out can't commit while heartbeats
    /// go on; raise the floor to 7 at the leader; then let the appends through and raise again.
    /// Expect: the first refused Unavailable, saying the group is leaving a joint configuration;
    /// the second refused naming member 4, which has not attested.
    #[test]
    fn a_raise_whose_base_is_joint_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = three_and_a_learner(&mut driven, dir.path());
        let mut pump = Pump::new(&clients);
        let leader = tracked_group(&mut driven, &mut pump, &clients);
        let led = &clients[leader];
        let until = driven.until();
        let proposed = pending_change(&driven, leader);
        let _asked = change(led, until, MemberChange::AddLearner(4));
        reach(&mut driven, &mut pump, "the joint entry", |driven| {
            pending_change(driven, leader) > proposed
        });
        let joint = pending_change(&driven, leader);
        reach(&mut driven, &mut pump, "the way out appended", |driven| {
            pending_change(driven, leader) > joint
        });
        pump.appends_lost = true;
        for _ in 0..ROUNDS {
            if !applied_configuration(&driven, leader)
                .voters_outgoing
                .is_empty()
            {
                break;
            }
            driven.round(&mut pump);
        }
        assert_eq!(
            applied_configuration(&driven, leader).voters_outgoing,
            vec![1, 2, 3]
        );

        let refused = raise(&mut driven, &mut pump, led, 7);
        assert!(
            matches!(&refused, Err(Refusal::Unavailable(why)) if why.contains("leaving a joint configuration")),
            "{refused:?}"
        );
        assert_eq!(
            led.handle().floor_counters().refused(FloorRefusal::Busy),
            1,
            "a joint base's refusal counted as busy"
        );
        assert!(
            !applied_configuration(&driven, leader)
                .voters_outgoing
                .is_empty()
        );
        pump.appends_lost = false;
        settled(&mut driven, &mut pump, 3, &[4]);
        let again = raise(&mut driven, &mut pump, led, 7);
        assert!(
            matches!(&again, Err(Refusal::Unavailable(why)) if why.contains("members [4]")),
            "{again:?}"
        );
        driven.drain(&mut pump);
        pump.finish();
    }

    fn unchanged(db: &mut Db) -> anyhow::Result<Vec<u8>> {
        let mut value = Vec::new();
        db.query("SELECT value FROM baseline_items WHERE id = 1", [], |row| {
            value = row
                .get_value(0)
                .to_blob()
                .ok_or_else(|| anyhow::anyhow!("expected blob"))?
                .to_vec();
            Ok(false)
        })?;
        Ok(value)
    }

    /// U15/CP-T1.8 on driven time (replication.md, "The owner's time"): a complete original image
    /// precedes every delta, including untouched pages, and the seed's transfer to its source is
    /// the scenario's only election.
    #[test]
    fn autonomous_quorum_seed_preserves_unchanged_pages_then_delta_and_restart() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("state-3.db");
        let original = {
            let mut db = Db::open(&source).unwrap();
            db.exec("CREATE TABLE baseline_items(id INTEGER PRIMARY KEY,value BLOB); INSERT INTO baseline_items VALUES(1,randomblob(9000)); INSERT INTO baseline_items VALUES(2,x'02')").unwrap();
            db.flusher()().unwrap();
            unchanged(&mut db).unwrap()
        };
        let bootstrap = crate::replication::bootstrap::inspect_source(&source, 3).unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], Some(bootstrap.clone())))
            .collect();
        let mut pump = Pump::new(&clients);
        driven.elect(&mut pump, &clients, 0, 0);
        // Admitted before the seed lands, the call waits in the product queue while the transfer
        // to the source and the seed's application go on.
        assert_eq!(
            driven
                .call(&mut pump, clients.first().unwrap(), unchanged)
                .unwrap(),
            original
        );
        let seeded = terms(&clients);
        assert!(
            clients
                .iter()
                .all(|c| c.handle().status().unwrap().leader == 3)
        );
        // The class recorded once the seed is in, before the write beside it.
        driven.follow(&mut pump, &clients, 0);
        driven
            .call(&mut pump, clients.get(1).unwrap(), |db| {
                db.exec("UPDATE baseline_items SET value=x'03' WHERE id=2")?;
                Ok(Vec::new())
            })
            .unwrap();
        for client in &clients {
            assert_eq!(driven.call(&mut pump, client, unchanged).unwrap(), original);
        }
        assert_eq!(
            terms(&clients),
            seeded,
            "an election happened after the seed's transfer"
        );
        // Past hyper-durable's quiet period, so each owner states its durable commit before it stops.
        for owner in 0..3 {
            driven.advance(owner, Duration::from_millis(60));
        }
        for _ in 0..8 {
            driven.round(&mut pump);
        }
        driven.drain(&mut pump);
        pump.finish();
        drop(clients);
        drop(driven);

        let mut driven = Driven::new();
        let reopened: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], Some(bootstrap.clone())))
            .collect();
        let mut pump = Pump::new(&reopened);
        driven.elect(&mut pump, &reopened, 0, 0);
        for client in &reopened {
            let status = client.handle().status().unwrap();
            assert_eq!(status.voters, vec![1, 2, 3]);
            assert!(status.learners.is_empty());
            assert!(status.fenced.is_none());
            assert!(!status.bootstrapping);
        }
        let elected = terms(&reopened);
        for client in &reopened {
            assert_eq!(
                driven
                    .call(&mut pump, client, |db| {
                        let mut value = Vec::new();
                        db.query("SELECT value FROM baseline_items WHERE id=2", [], |row| {
                            value = row.get_value(0).to_blob().unwrap().to_vec();
                            Ok(false)
                        })?;
                        Ok(value)
                    })
                    .unwrap(),
                vec![3]
            );
            assert_eq!(driven.call(&mut pump, client, unchanged).unwrap(), original);
        }
        assert_eq!(
            terms(&reopened),
            elected,
            "an election happened during the reopened scenario"
        );
        driven.drain(&mut pump);
        pump.finish();
    }

    /// U15/CP-T1.8 under a nemesis (replication.md, "The owner's time"): the seeded group with
    /// the lead moved while the delta's call is in flight, while a replica reads the seed's image,
    /// and while the reopened group's reads are. The seed's own transfer to its source is its
    /// first election. Whatever the election: the delta's answer is Ok or UnknownOutcome, every
    /// replica reads one value for it (the new one if it answered Ok), and every replica keeps
    /// the seed's original image.
    #[test]
    fn seed_and_delta_hold_through_an_election_at_each_step() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("state-3.db");
        let original = {
            let mut db = Db::open(&source).unwrap();
            db.exec("CREATE TABLE baseline_items(id INTEGER PRIMARY KEY,value BLOB); INSERT INTO baseline_items VALUES(1,randomblob(9000)); INSERT INTO baseline_items VALUES(2,x'02')").unwrap();
            db.flusher()().unwrap();
            unchanged(&mut db).unwrap()
        };
        let bootstrap = crate::replication::bootstrap::inspect_source(&source, 3).unwrap();
        let second = |db: &mut Db| -> anyhow::Result<Vec<u8>> {
            let mut value = Vec::new();
            db.query("SELECT value FROM baseline_items WHERE id=2", [], |row| {
                value = row.get_value(0).to_blob().unwrap().to_vec();
                Ok(false)
            })?;
            Ok(value)
        };
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], Some(bootstrap.clone())))
            .collect();
        let mut pump = Pump::new(&clients);
        pump.refusals = Some(0);
        driven.elect(&mut pump, &clients, 0, 0);
        assert_eq!(
            driven
                .call(&mut pump, clients.first().unwrap(), unchanged)
                .unwrap(),
            original
        );
        // The class recorded once the seed is in, before the delta beside it.
        driven.follow(&mut pump, &clients, 0);
        // The delta goes in from n2 while the lead moves from the seed's source to n1.
        let delta = submit(clients.get(1).unwrap(), driven.until(), |db| {
            db.exec("UPDATE baseline_items SET value=x'03' WHERE id=2")?;
            Ok(Vec::new())
        });
        driven.transfer(&mut pump, &clients, 2, 0);
        let answered = driven.settle(&mut pump, &delta);
        assert!(
            matches!(answered, Ok(ref v) if v.is_empty())
                || matches!(answered, Err(Refusal::UnknownOutcome)),
            "{answered:?}"
        );
        let values: Vec<_> = clients
            .iter()
            .map(|client| driven.call(&mut pump, client, second).unwrap())
            .collect();
        assert!(
            values.windows(2).all(|pair| pair.first() == pair.last()),
            "{values:?}"
        );
        if answered.is_ok() {
            assert_eq!(values.first().unwrap(), &vec![3]);
        }
        // The lead moves again while a replica reads the seed's original image.
        let image = submit(clients.get(1).unwrap(), driven.until(), unchanged);
        driven.transfer(&mut pump, &clients, 0, 1);
        let read = driven.settle(&mut pump, &image);
        assert!(
            matches!(read, Ok(ref v) if v == &original)
                || matches!(read, Err(Refusal::UnknownOutcome)),
            "{read:?}"
        );
        for client in &clients {
            assert_eq!(driven.call(&mut pump, client, unchanged).unwrap(), original);
        }
        for owner in 0..3 {
            driven.advance(owner, Duration::from_millis(60));
        }
        for _ in 0..8 {
            driven.round(&mut pump);
        }
        driven.drain(&mut pump);
        pump.finish();
        drop(clients);
        drop(driven);

        // Reopened, the group elects again and the lead moves while the replicas are read.
        let mut driven = Driven::new();
        let reopened: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], Some(bootstrap.clone())))
            .collect();
        let mut pump = Pump::new(&reopened);
        pump.refusals = Some(0);
        let leader = driven.elect(&mut pump, &reopened, 0, 0);
        let read = submit(reopened.get(2).unwrap(), driven.until(), second);
        driven.transfer(&mut pump, &reopened, leader, (leader + 1) % 3);
        let after = driven.settle(&mut pump, &read);
        assert!(
            matches!(after, Ok(ref v) if v == values.first().unwrap())
                || matches!(after, Err(Refusal::UnknownOutcome)),
            "{after:?} after {values:?}"
        );
        for client in &reopened {
            assert_eq!(
                &driven.call(&mut pump, client, second).unwrap(),
                values.first().unwrap()
            );
            assert_eq!(driven.call(&mut pump, client, unchanged).unwrap(), original);
        }
        driven.drain(&mut pump);
        pump.finish();
    }

    /// U15/AC-T1.8.1 under a nemesis (replication.md, "A forward that meets an election"): a
    /// follower's proposal is held on its way to the leader while the lead moves, and arrives
    /// once the old leader has stepped down to the new term and knows no leader yet. The old
    /// leader refuses it typed, NoLeader, a cause hyper-raft retries at the next leader; the
    /// relay counts that refusal, the call answers UnknownOutcome, and no replica has its row.
    #[test]
    fn a_forward_that_arrives_mid_election_is_refused_no_leader_and_answers_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        let mut pump = Pump::new(&clients);
        pump.refusals = Some(0);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        let (origin, next) = ((leader + 1) % 3, (leader + 2) % 3);
        let count = |db: &mut Db| -> anyhow::Result<Vec<u8>> {
            let mut count = 0;
            db.query("SELECT count(*) FROM forward_items", [], |row| {
                count = row.get::<i64>(0).map_err(unavailable)?;
                Ok(false)
            })?;
            Ok(vec![u8::try_from(count)?])
        };
        driven
            .call(&mut pump, clients.get(leader).unwrap(), |db| {
                db.exec("CREATE TABLE forward_items(value BLOB)")?;
                Ok(Vec::new())
            })
            .unwrap();

        // The origin's forward is held before it reaches the leader.
        pump.begin_captures(leader, 1);
        let insert = submit(clients.get(origin).unwrap(), driven.until(), |db| {
            db.exec("INSERT INTO forward_items VALUES(x'01')")?;
            Ok(vec![1])
        });
        for _ in 0..ROUNDS {
            if pump.captures_ready() {
                break;
            }
            driven.round(&mut pump);
        }
        assert!(pump.captures_ready(), "the origin forwarded no proposal");

        // The lead moves to the third owner; the forward is released once the old leader has
        // taken the new term and knows no leader in it.
        let elected = driven.owner(leader).replica.term();
        driven
            .owner_mut(leader)
            .replica
            .transfer(u64::try_from(next + 1).unwrap())
            .unwrap();
        for _ in 0..ROUNDS {
            let old = &driven.owner(leader).replica;
            if old.term() > elected && old.leader() == 0 {
                break;
            }
            driven.round(&mut pump);
        }
        let old = &driven.owner(leader).replica;
        assert!(
            old.term() > elected && old.leader() == 0,
            "the old leader never stood leaderless in a new term"
        );
        pump.release_captures();

        let answered = driven.settle(&mut pump, &insert);
        assert!(
            matches!(answered, Err(Refusal::UnknownOutcome)),
            "{answered:?}"
        );
        // The origin answers on the new term alone; the old leader's receipt is reaped here.
        driven.drain(&mut pump);
        assert_eq!(pump.refusals, Some(1), "the forward was not refused");
        driven.follow(&mut pump, &clients, 0);
        for client in &clients {
            assert_eq!(driven.call(&mut pump, client, count).unwrap(), vec![0]);
        }
        driven.drain(&mut pump);
        pump.finish();
    }

    /// The values of member_items a connection sees: none while there is no such table.
    fn values(db: &mut Db) -> anyhow::Result<Vec<u8>> {
        let mut tables = 0;
        db.query(
            "SELECT count(*) FROM sqlite_schema WHERE name='member_items'",
            [],
            |row| {
                tables = row.get::<i64>(0).map_err(unavailable)?;
                Ok(false)
            },
        )?;
        let mut values = Vec::new();
        if tables == 1 {
            db.query("SELECT value FROM member_items ORDER BY value", [], |row| {
                values.push(u8::try_from(row.get::<i64>(0).map_err(unavailable)?)?);
                Ok(true)
            })?;
        }
        Ok(values)
    }
    /// Steps owner `owner` alone, `rounds` times, taking each input waiting for it: nothing relayed.
    fn step_alone(driven: &mut Driven, owner: usize, rounds: usize) {
        let (actor, inputs) = driven.owners.get_mut(owner).unwrap().as_mut().unwrap();
        for _ in 0..rounds {
            while let Ok(input) = inputs.try_recv() {
                actor.run_step(Some(input)).unwrap();
            }
            actor.run_step(None).unwrap();
        }
    }
    /// AC-T1.10.L1, T-T1.10.L1, Amphora goal 13f90c05 (replication.md, "A local read on a
    /// replica"), on driven time: a local read answers with the leader gone. Do: a voter and its
    /// learner, 1 and 2 written and caught up; then the voter is gone (nothing relayed, its owner
    /// never stepped again); the learner asks a call, which needs the leader's ReadIndex, and
    /// reads locally. Expect: the call waits and is refused once its deadline passes, never
    /// evaluated; the local read answers 1 and 2 meanwhile and after.
    #[test]
    fn a_local_read_answers_with_the_leader_gone() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = vec![
            driven.open(dir.path(), 1, vec![1], None),
            driven.open_member(dir.path(), 2, vec![1], vec![2], None),
        ];
        let mut pump = Pump::new(&clients);
        let (voter, learner) = (clients.first().unwrap(), clients.get(1).unwrap());
        driven.elect(&mut pump, clients.get(..1).unwrap(), 0, 1);
        let until = driven.until();
        driven
            .answer(
                &mut pump,
                &change(voter, until, MemberChange::AddLearner(2)),
            )
            .unwrap();
        write(&mut driven, &mut pump, voter, 1);
        write(&mut driven, &mut pump, voter, 2);
        catch_up(&mut driven, &mut pump, &clients, 0, 1);
        driven.drain(&mut pump);
        pump.finish();

        let until = driven.until();
        let evaluated = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&evaluated);
        let asked = submit(learner, until, move |db| {
            counted.fetch_add(1, Ordering::Relaxed);
            values(db)
        });
        step_alone(&mut driven, 1, 64);
        assert!(
            matches!(asked.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "a call was answered without its leader"
        );
        assert_eq!(learner.read_local(until, values), Ok(vec![1, 2]));
        set(&driven.clock(1), driven.base, until);
        step_alone(&mut driven, 1, 1);
        assert!(matches!(asked.try_recv(), Ok(Err(Refusal::Timeout))));
        assert_eq!(evaluated.load(Ordering::Relaxed), 0);
        assert_eq!(
            learner.read_local(Instant::now() + HANG_READ, values),
            Ok(vec![1, 2])
        );
    }

    /// A local read's own budget in these tests, on the real clock: a hang guard, never a verdict.
    const HANG_READ: Duration = Duration::from_secs(60);

    /// AC-T1.10.L1, T-T1.10.L2 (replication.md, "A local read on a replica"; kiln plan
    /// 01a1a283e14c, step 4), on driven time: a local read sees only what the group committed and
    /// its owner applied. Do: three voters, 1 written; the leader prepares 2 beside thousands of
    /// pages (past its writer's cache, which spills them), reading locally within that
    /// preparation; once the write is proposed, nothing is relayed, and every owner reads locally;
    /// then relaying resumes. Expect: the read within the preparation sees 1 alone; while the
    /// write is proposed and not committed, every owner's read sees 1 alone; once it is answered,
    /// the leader's sees 1 and 2, and each follower's once it has caught up.
    #[test]
    fn a_local_read_never_sees_what_the_group_has_not_committed() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        let origin = clients.get(leader).unwrap();
        write(&mut driven, &mut pump, origin, 1);
        for owner in 0..3 {
            catch_up(&mut driven, &mut pump, &clients, leader, owner);
        }

        let within = Arc::new(Mutex::new(None));
        let (probe, seen) = (origin.clone(), Arc::clone(&within));
        let until = driven.until();
        let answer = submit(origin, until, move |db| {
            db.exec("CREATE TABLE spilled(data)")?;
            db.exec("INSERT INTO spilled SELECT zeroblob(4000) FROM (WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<2000) SELECT x FROM n)")?;
            db.exec("INSERT INTO member_items VALUES(2)")?;
            *seen.lock().unwrap() = Some(probe.read_local(Instant::now() + HANG_READ, values)?);
            Ok(Vec::new())
        });
        let proposed = |driven: &Driven| {
            driven
                .owner(leader)
                .pending
                .as_ref()
                .is_some_and(|pending| matches!(pending.phase, Phase::Proposed(_)))
        };
        for _ in 0..ROUNDS {
            if proposed(&driven) {
                break;
            }
            driven.round(&mut pump);
        }
        assert!(proposed(&driven), "the write was never proposed");
        assert_eq!(within.lock().unwrap().take(), Some(vec![1]));
        for client in &clients {
            assert_eq!(
                client
                    .read_local(Instant::now() + HANG_READ, values)
                    .unwrap(),
                vec![1]
            );
        }
        assert!(matches!(answer.try_recv(), Err(mpsc::TryRecvError::Empty)));

        assert_eq!(driven.settle(&mut pump, &answer).unwrap(), Vec::<u8>::new());
        assert_eq!(
            origin
                .read_local(Instant::now() + HANG_READ, values)
                .unwrap(),
            vec![1, 2]
        );
        for owner in 0..3 {
            catch_up(&mut driven, &mut pump, &clients, leader, owner);
            let client = clients.get(owner).unwrap();
            assert_eq!(
                client
                    .read_local(Instant::now() + HANG_READ, values)
                    .unwrap(),
                vec![1, 2]
            );
        }
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.10.L1, T-T1.10.L3 (replication.md, "A local read on a replica"; kiln plan
    /// 01a1a283e14c, step 4): a member that hasn't taken its group's first image has no state of
    /// the group's to read. Do: open a learner declared to take its first image from member 1,
    /// which never sends it; read locally. Expect: refused (Unavailable), saying why, and counted
    /// as refused bootstrapping.
    #[test]
    fn a_local_read_waits_for_the_groups_first_image() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let declared = api::replication::Bootstrap {
            source_node: 1,
            source_id: [7; 32],
        };
        let learner = driven.open_member(dir.path(), 2, vec![1], vec![2], Some(declared));
        assert!(learner.handle().status().unwrap().bootstrapping);
        let refused = learner.read_local(Instant::now() + HANG_READ, values);
        assert!(
            matches!(&refused, Err(Refusal::Unavailable(why)) if why.contains("first image")),
            "{refused:?}"
        );
        assert_eq!(
            learner
                .handle()
                .counters()
                .local_refused(LocalRefusal::Bootstrapping),
            1
        );
    }

    /// How long the fifth local read waits in the test below: its own deadline, the thing under
    /// test, so that it is refused Timeout when it passes.
    const FIFTH_WAIT: Duration = Duration::from_millis(100);

    /// A local read of `values` on its own thread that, once it has its reader, says so on
    /// `entered` and holds the reader until its release is sent or dropped.
    fn held_read(
        client: &Client,
        entered: &mpsc::Sender<()>,
    ) -> (
        mpsc::Sender<()>,
        std::thread::JoinHandle<Result<Vec<u8>, Refusal>>,
    ) {
        let (release, released) = mpsc::channel::<()>();
        let (client, entered) = (client.clone(), entered.clone());
        let read = std::thread::spawn(move || {
            client.read_local(Instant::now() + HANG_READ, move |db| {
                entered.send(()).unwrap();
                let _ = released.recv();
                values(db)
            })
        });
        (release, read)
    }

    /// AC-T1.10.L2, T-T1.10.L8 (replication.md, "A replica's local readers bounded by
    /// derivation"), on driven time: at most as many local readers at once as the process's
    /// descriptor budget lets clients attach, and one more waits on its caller's thread up to its
    /// own deadline. Do: a descriptor budget of 6, one voter, 1 written; 6 local reads held open on threads
    /// of their own; one more asked on a thread of its own, its deadline FIFTH_WAIT away, while the
    /// owner answers a call; one of the held let go and a read asked; one held again and a read
    /// waiting with a long deadline; the owner dropped without its loop's end. Expect: every held
    /// read has its reader; the call answered while they are out; the one more refused Timeout no
    /// sooner than its deadline; the read after the let-go answers 1; the waiting read refused
    /// Fenced once the owner is gone; the meter counting 6 readers in use while
    /// they are held, every read that had a reader, one refused timeout and one fenced, and none
    /// in use at the end.
    #[test]
    fn local_readers_are_bounded_by_the_transport_not_four() {
        let dir = tempfile::tempdir().unwrap();
        // The bound moves with the budget: a budget of its own, small enough that the readers
        // reach it, for the owner this thread opens.
        let most = crate::descriptors::tests::budget(6).bound();
        // An account of the test's own, with room for one reader more than the bound, so the
        // count is what holds the one more back, never the budget (or another test's readers).
        let reader = usize::try_from(crate::db::READER_PAGES).unwrap();
        let pages = ((most + 1) * reader + 64).div_ceil(64) * 64;
        ACCOUNT.with(|account| {
            *account.borrow_mut() = Some(Account::new(Arc::new(crate::db::SharedPages::new(pages))))
        });
        let mut driven = Driven::new();
        let clients = vec![driven.open(dir.path(), 1, vec![1], None)];
        let mut pump = Pump::new(&clients);
        let client = clients.first().unwrap();
        driven.elect(&mut pump, &clients, 0, 1);
        write(&mut driven, &mut pump, client, 1);

        let (entered, entering) = mpsc::channel();
        let mut held: Vec<_> = (0..most).map(|_| held_read(client, &entered)).collect();
        for _ in 0..most {
            assert!(
                entering.recv_timeout(HANG_READ).is_ok(),
                "a held read never had its reader"
            );
        }
        assert_eq!(
            client.handle().counters().local_readers_in_use,
            u64::try_from(most).unwrap()
        );
        let asked = Instant::now();
        let fifth = {
            let client = client.clone();
            std::thread::spawn(move || client.read_local(asked + FIFTH_WAIT, values))
        };
        // The owner answers while every reader is out and one more waits: it waits on its own
        // thread, never the owner's.
        assert_eq!(driven.call(&mut pump, client, values).unwrap(), vec![1]);
        let refused = fifth.join().unwrap();
        assert!(matches!(refused, Err(Refusal::Timeout)), "{refused:?}");
        assert!(asked.elapsed() >= FIFTH_WAIT, "the fifth was refused early");
        assert_eq!(
            client
                .handle()
                .counters()
                .local_refused(LocalRefusal::Timeout),
            1
        );

        let (release, read) = held.pop().unwrap();
        release.send(()).unwrap();
        assert_eq!(read.join().unwrap(), Ok(vec![1]));
        assert_eq!(
            client.read_local(Instant::now() + HANG_READ, values),
            Ok(vec![1])
        );

        held.push(held_read(client, &entered));
        assert!(
            entering.recv_timeout(HANG_READ).is_ok(),
            "a held read never had its reader"
        );
        let waiting = {
            let client = client.clone();
            std::thread::spawn(move || client.read_local(Instant::now() + HANG_READ, values))
        };
        drop(driven.owners.get_mut(0).unwrap().take());
        let ended = waiting.join().unwrap();
        assert!(matches!(ended, Err(Refusal::Fenced(_))), "{ended:?}");
        for (release, read) in held {
            release.send(()).unwrap();
            read.join().unwrap().unwrap();
        }
        // Every read that had a reader counted, the two turned away by their reasons, and none
        // out now (amphora_replica_local_*).
        let counted = client.handle().counters();
        assert_eq!(counted.local_reads, u64::try_from(most + 2).unwrap());
        assert_eq!(counted.local_refused(LocalRefusal::Timeout), 1);
        assert_eq!(counted.local_refused(LocalRefusal::Fenced), 1);
        assert_eq!(counted.local_readers_in_use, 0);
    }

    /// AC-T1.10.L2, T-T1.10.L9 (replication.md, "A replica's local readers bounded by
    /// derivation"): every replica's readers are charged to one page budget, and a read takes
    /// another replica's idle reader before it waits. Do: an account over a pool with room for two
    /// readers' pages, and two replicas' readers on it, A and B; A reads once; B holds a read open,
    /// then holds a second; A reads with its deadline FIFTH_WAIT away; one of B's let go, and A
    /// reads again. Expect: A's idle reader charged once it is given back; B's second read runs,
    /// A's idle reader closed for it; A's read refused Timeout no sooner than its deadline; A's
    /// read after the let-go answers, B's idle reader closed for it; the charge never past two
    /// readers' pages, and the idle readers' alone at the end.
    #[test]
    fn local_readers_past_the_process_page_budget_wait_and_reclaim_idle_ones() {
        let dir = tempfile::tempdir().unwrap();
        let reader = usize::try_from(crate::db::READER_PAGES).unwrap();
        // 64 pages a shard, of which a charge leaves one: room for two readers' 2,000 pages, not
        // three.
        let account = Account::new(Arc::new(crate::db::SharedPages::new(64 * 64)));
        let pool = Arc::clone(&account.pool);
        let replica = |name: &str| {
            let db = Db::open(&dir.path().join(name)).unwrap();
            db.exec("CREATE TABLE member_items(value); INSERT INTO member_items VALUES (1)")
                .unwrap();
            let local = Arc::new(Local::new(
                db.opener(),
                Arc::new(ReplicaMeter::default()),
                Arc::clone(&account),
                crate::descriptors::process().unwrap(),
            ));
            (db, local)
        };
        let ((_a_db, a), (_b_db, b)) = (replica("a.db"), replica("b.db"));
        let far = || Instant::now() + HANG_READ;
        let held = |local: &Arc<Local>, entered: &mpsc::Sender<()>| {
            let (release, released) = mpsc::channel::<()>();
            let (local, entered) = (Arc::clone(local), entered.clone());
            let read = std::thread::spawn(move || {
                local.with(Instant::now() + HANG_READ, move |db| {
                    entered.send(()).unwrap();
                    let _ = released.recv();
                    values(db)
                })
            });
            (release, read)
        };

        assert_eq!(a.with(far(), values), Ok(vec![1]));
        assert_eq!(pool.charged(), reader, "A's idle reader is charged");
        let (entered, entering) = mpsc::channel();
        let first = held(&b, &entered);
        entering.recv_timeout(HANG_READ).unwrap();
        assert_eq!(pool.charged(), 2 * reader);
        let second = held(&b, &entered);
        entering.recv_timeout(HANG_READ).unwrap();
        assert_eq!(
            pool.charged(),
            2 * reader,
            "A's idle reader was closed for B's"
        );

        let asked = Instant::now();
        let refused = a.with(asked + FIFTH_WAIT, values);
        assert!(matches!(refused, Err(Refusal::Timeout)), "{refused:?}");
        assert!(asked.elapsed() >= FIFTH_WAIT, "A's read was refused early");

        let (release, read) = first;
        release.send(()).unwrap();
        assert_eq!(read.join().unwrap(), Ok(vec![1]));
        assert_eq!(a.with(far(), values), Ok(vec![1]));
        assert_eq!(
            pool.charged(),
            2 * reader,
            "B's idle reader was closed for A's"
        );
        let (release, read) = second;
        release.send(()).unwrap();
        assert_eq!(read.join().unwrap(), Ok(vec![1]));
        assert_eq!(pool.charged(), 2 * reader, "the idle readers alone");
        a.release();
        b.release();
        assert_eq!(
            pool.charged(),
            0,
            "an ended replica's readers give their charge back"
        );
    }

    /// AC-T1.10.L2 (replication.md, "A replica's local readers bounded by derivation", the
    /// amendment): a reader takes no file of its own, so the process's descriptor limit doesn't
    /// bound readers. Do: a database on a disk that counts the opens asked of it, written once;
    /// eight readers opened on it and each read. Expect: every reader reads the row, and no open
    /// asked of the disk past those the database's own open asked.
    #[test]
    fn a_local_reader_opens_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let disk = PowerDisk::new(std::collections::HashMap::new());
        let io: Arc<dyn amphora_engine::IO> = disk.clone();
        let db = Db::open_with(&dir.path().join("state.db"), io).unwrap();
        db.exec("CREATE TABLE member_items(value); INSERT INTO member_items VALUES (1)")
            .unwrap();
        let opened = disk.opened.load(Ordering::Relaxed);
        let open = db.opener();
        let mut readers: Vec<Db> = (0..8).map(|_| open().unwrap()).collect();
        for reader in &mut readers {
            assert_eq!(values(reader).unwrap(), vec![1]);
        }
        assert_eq!(disk.opened.load(Ordering::Relaxed), opened);
    }

    /// AC-T1.10.L1, T-T1.10.L6 (replication.md, "A local read on a replica", amendment 4): across
    /// a power cut a local read can go back, never past the replica's last finished flush. Do:
    /// three voters of three fates, two to span, every database flush held; 1 written and flushed
    /// on a follower, then 2 written and applied there unflushed, and read locally; the
    /// follower's power cut, its database losing what it hadn't synced, and the member restarted;
    /// a local read before it steps; then rounds until it has caught up. Expect: 1 and 2 before
    /// the cut; 1 alone after it, the state its last flush made durable and never less; 1 and 2
    /// once caught up.
    #[test]
    fn a_local_read_after_a_power_cut_is_no_older_than_its_last_flush() {
        local_read_across_a_power_cut(&three_fates());
    }

    /// AC-T1.10.L1, T-T1.10.L6, AC-T1.8.D2 (replication.md, "A local read on a replica",
    /// amendment 4, and "A replica's database log"): the persisted class, whose database flushes
    /// are its flushing thread's too, goes back across a power cut no further than its last
    /// finished flush, and applies again from its own raft log what it lost. Do and expect: as in
    /// `a_local_read_after_a_power_cut_is_no_older_than_its_last_flush`, three persisted voters.
    #[test]
    fn a_persisted_local_read_after_a_power_cut_is_no_older_than_its_last_flush() {
        local_read_across_a_power_cut(&Durability::Persisted);
    }

    fn local_read_across_a_power_cut(durability: &Durability) {
        let dir = tempfile::tempdir().unwrap();
        let durability = durability.clone();
        let voters = vec![1, 2, 3];
        let mut power: Vec<Power> = (1..=3u64).map(Power::new).collect();
        let mut driven = Driven::powered();
        let mut clients: Vec<Client> = (0..3)
            .map(|at| {
                driven.open_power(
                    dir.path(),
                    u64::try_from(at + 1).unwrap(),
                    voters.clone(),
                    durability.clone(),
                    power.get(at).unwrap(),
                )
            })
            .collect();
        for owner in 0..3 {
            driven.hold_flushes(owner);
        }
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        let follower = (leader + 1) % 3;
        let insert = |value: u8| {
            move |db: &mut Db| {
                db.exec("CREATE TABLE IF NOT EXISTS member_items(value)")?;
                db.exec(&format!("INSERT INTO member_items VALUES({value})"))?;
                Ok(Vec::new())
            }
        };
        // Rounds, the leader's clock moving a tick each, until the follower has applied all the
        // leader has.
        let caught_up = |driven: &mut Driven<hyper_block::sim::SimHandle>,
                         pump: &mut Pump<'_>,
                         clients: &[Client]| {
            let applied = |n: usize| clients[n].handle().status().unwrap().applied;
            (0..ROUNDS).any(|_| {
                driven.advance_tick(leader);
                driven.round(pump);
                applied(follower) >= applied(leader)
            })
        };
        driven.call(&mut pump, &clients[leader], insert(1)).unwrap();
        assert!(caught_up(&mut driven, &mut pump, &clients));
        driven.flush(follower);
        driven.call(&mut pump, &clients[leader], insert(2)).unwrap();
        assert!(caught_up(&mut driven, &mut pump, &clients));
        assert!(
            driven
                .owner(follower)
                .replica
                .machine()
                .unflushed()
                .is_some(),
            "the follower's last write was flushed"
        );
        assert_eq!(
            clients[follower].read_local(Instant::now() + HANG_READ, values),
            Ok(vec![1, 2])
        );
        driven.drain(&mut pump);
        pump.finish();

        clients[follower] = driven.power_cut(
            dir.path(),
            follower,
            voters.clone(),
            durability.clone(),
            &mut power[follower],
        );
        driven.hold_flushes(follower);
        assert_eq!(
            clients[follower].read_local(Instant::now() + HANG_READ, values),
            Ok(vec![1])
        );
        let mut pump = Pump::new(&clients);
        assert!(
            caught_up(&mut driven, &mut pump, &clients),
            "the restarted follower never caught up"
        );
        assert_eq!(
            clients[follower].read_local(Instant::now() + HANG_READ, values),
            Ok(vec![1, 2])
        );
        driven.drain(&mut pump);
        pump.finish();
    }
    /// AC-T1.10.L1, T-T1.10.L4 (replication.md, "A local read on a replica", amendment 1), on
    /// driven time: a local read open across an image's install sees one side of it, whole. Do: a
    /// voter, 1 and 2 written beside a table of many pages, its log compacted; a local read on its
    /// learner-to-be that, within one read transaction, reads, has the learner added (it can catch
    /// up only by the voter's image) and caught up, runs a checkpoint and a log restart on the
    /// learner's database from a second reader, and reads again; then a read after it.
    /// Expect: the learner installed the image; both of the open read's views are the state
    /// before it (no table), while the second reader, at the same time, sees the image whole (1
    /// and 2, and all 200 rows of spread); the restart answered Ok(false), not gone through; the
    /// read after sees the image whole.
    #[test]
    fn a_local_read_open_across_an_image_install_sees_one_side_whole() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = vec![
            driven.open(dir.path(), 1, vec![1], None),
            driven.open_member(dir.path(), 2, vec![1], vec![2], None),
        ];
        driven.owner_mut(1).replica.machine_mut().record_digests();
        let mut pump = Pump::new(&clients);
        let (voter, learner) = (&clients[0], &clients[1]);
        driven.elect(&mut pump, &clients[..1], 0, 1);
        write(&mut driven, &mut pump, voter, 1);
        write(&mut driven, &mut pump, voter, 2);
        driven
            .call(&mut pump, voter, |db| {
                db.exec("CREATE TABLE spread(data)")?;
                db.exec("INSERT INTO spread SELECT zeroblob(3000) FROM (WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<200) SELECT x FROM n)")?;
                Ok(Vec::new())
            })
            .unwrap();
        quiet(&mut driven, &mut pump);
        driven.flushed(&mut pump, 0);
        let compacted = {
            let owner = driven.owner_mut(0);
            let now = owner.now().unwrap();
            owner
                .replica
                .compact(0, now, std::task::Waker::noop())
                .unwrap();
            owner.replica.applied().index
        };
        for _ in 0..64 {
            driven.round(&mut pump);
        }

        // What one read transaction sees: member_items' values and the pages of spread.
        let seen = |db: &mut Db| -> anyhow::Result<(Vec<u8>, i64)> {
            let mut tables = 0;
            db.query(
                "SELECT count(*) FROM sqlite_schema WHERE name='spread'",
                [],
                |row| {
                    tables = row.get::<i64>(0).map_err(unavailable)?;
                    Ok(false)
                },
            )?;
            let mut spread = 0;
            if tables == 1 {
                db.query("SELECT count(*) FROM spread", [], |row| {
                    spread = row.get::<i64>(0).map_err(unavailable)?;
                    Ok(false)
                })?;
            }
            Ok((values(db)?, spread))
        };
        let (before, across, (restarted, beside)) = learner
            .read_local(Instant::now() + HANG_READ, |db| {
                db.query_transaction(|db| {
                    let before = seen(db)?;
                    // The learner is added only now, so its image installs under this read.
                    let until = driven.until();
                    driven
                        .answer(
                            &mut pump,
                            &change(voter, until, MemberChange::AddLearner(2)),
                        )
                        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
                    catch_up(&mut driven, &mut pump, &clients, 0, 1);
                    // A second reader of the learner's database copies what it may of the log
                    // into the file, and tries to start the log again, while this read is open.
                    // It reads the image whole while this read is open, so the install committed
                    // under this read and this read didn't hold it off.
                    let beside = learner
                        .read_local(Instant::now() + HANG_READ, |other| {
                            other.checkpoint()?;
                            let restarted = other.restart_log();
                            Ok((restarted, other.query_transaction(|db| seen(db))?))
                        })
                        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
                    Ok((before, seen(db)?, beside))
                })
            })
            .unwrap();
        let installed = driven.owner(1).replica.machine().digests().first().copied();
        let (installed, _) = installed.expect("the learner recorded nothing");
        assert!(
            installed >= compacted,
            "the learner replayed the log from {installed}, not the image at {compacted}"
        );
        assert_eq!(before, (Vec::new(), 0));
        assert_eq!(across, before, "the open read saw part of the image");
        assert_eq!(
            beside,
            (vec![1, 2], 200),
            "the image wasn't committed beside the open read"
        );
        assert!(
            matches!(restarted, Ok(false)),
            "the log restart under an open read: {restarted:?}"
        );
        assert_eq!(
            learner.read_local(Instant::now() + HANG_READ, |db| db.query_transaction(seen)),
            Ok((vec![1, 2], 200))
        );
        driven.drain(&mut pump);
        pump.finish();
    }

    /// Rounds until owner 1 has applied all owner 0 has and flushed it, and so acted on the
    /// checkpoint pass that follows the flush.
    fn settle_log(driven: &mut Driven, pump: &mut Pump<'_>, clients: &[Client]) {
        catch_up(driven, pump, clients, 0, 1);
        let handle = clients[1].handle();
        for _ in 0..ROUNDS {
            driven.round(pump);
            if handle.counters().flush_backlog == 0 {
                return;
            }
        }
        panic!("owner 1 did not flush in {ROUNDS} driven rounds");
    }
    /// A voter, and a learner of it, which applies what the voter commits and prepares nothing:
    /// only a log restart of its owner's starts its database's log again (a leader's preparation
    /// is a write transaction, which the engine starts on a log it finds wholly copied).
    fn voter_and_learner(driven: &mut Driven, dir: &Path) -> Vec<Client> {
        let clients = vec![
            driven.open(dir, 1, vec![1], None),
            driven.open_member(dir, 2, vec![1], vec![2], None),
        ];
        let mut pump = Pump::new(&clients);
        driven.elect(&mut pump, &clients[..1], 0, 1);
        let until = driven.until();
        driven
            .answer(
                &mut pump,
                &change(&clients[0], until, MemberChange::AddLearner(2)),
            )
            .unwrap();
        catch_up(driven, &mut pump, &clients, 0, 1);
        driven.drain(&mut pump);
        pump.finish();
        clients
    }

    /// An account of the test's own, so that the replica logs counted on its filesystem are its
    /// owners' alone, not other tests'.
    fn own_account() {
        ACCOUNT
            .with(|account| *account.borrow_mut() = Some(Account::new(crate::db::process_pages())));
    }
    /// Free bytes that give each of `logs` replica logs on a filesystem `frames` frames past what
    /// its own file holds (`log_bound::log_room`).
    fn free_for(frames: u64, logs: u64) -> u64 {
        u64::try_from(owner_image_room().unwrap()).unwrap()
            + frames * logs * (log_bound::FRAME_BYTES + crate::db::PAGE_BYTES)
    }
    /// The frames owner `owner`'s database log may hold, as it last learned them.
    fn room(clients: &[Client], owner: usize) -> u64 {
        clients[owner].handle().counters().database_log_limit_bytes / log_bound::FRAME_BYTES
    }

    /// AC-T1.8.D1, T-T1.8.D1 (replication.md, "A replica's database log bounded by what it
    /// measures"): a replica's database log is copied into its file and started again, so it
    /// stays within the room its filesystem's free bytes give it however many entries it applies,
    /// on a learner and on its leader. Do: a voter and its learner on one filesystem, each told it
    /// has room for 64 frames past its own file; 300 writes of a 3,000-byte row each at the voter,
    /// each followed by rounds until the learner applied and flushed it and the voter flushed it;
    /// then the learner restarted on its files. Expect: after each write each log within its room
    /// as last learned, plus what two writes grow it (the flush out as it passed the room, and the
    /// one its pass follows); the learner's log started again and passes counted; every row read
    /// back locally on the learner, and from its own state once restarted.
    #[test]
    fn a_replicas_database_log_stays_within_its_room() {
        const WRITES: i64 = 300;
        own_account();
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = voter_and_learner(&mut driven, dir.path());
        for owner in 0..2 {
            driven
                .owner(owner)
                .replica
                .machine()
                .set_free_bytes(free_for(64, 2));
        }
        let mut pump = Pump::new(&clients);
        let voter = clients[0].handle();
        let mut seen = Vec::new();
        for n in 0..WRITES {
            driven
                .call(&mut pump, &clients[0], move |db| {
                    db.exec("CREATE TABLE IF NOT EXISTS bounded(n, body)")?;
                    db.exec(&format!(
                        "INSERT INTO bounded VALUES ({n}, randomblob(3000))"
                    ))?;
                    Ok(Vec::new())
                })
                .unwrap();
            settle_log(&mut driven, &mut pump, &clients);
            for _ in 0..ROUNDS {
                if voter.counters().flush_backlog == 0 {
                    break;
                }
                driven.round(&mut pump);
            }
            seen.push([0, 1].map(|owner| {
                (
                    driven.owner(owner).replica.machine().log_frames().unwrap(),
                    room(&clients, owner),
                )
            }));
        }
        for owner in 0..2 {
            let grew = seen
                .windows(2)
                .map(|pair| pair[1][owner].0.saturating_sub(pair[0][owner].0))
                .max()
                .unwrap();
            for (write, frames) in seen.iter().enumerate() {
                let (frames, room) = frames[owner];
                assert!(
                    frames <= room + 2 * grew,
                    "owner {owner} after write {write}: {frames} frames, room {room}, a write {grew}"
                );
            }
        }
        let counted = clients[1].handle().counters();
        assert!(counted.database_log_restarts >= 1, "{counted:?}");
        assert!(counted.checkpoints.iter().sum::<u64>() >= 1, "{counted:?}");
        fn count(db: &mut Db) -> anyhow::Result<i64> {
            let mut rows = 0;
            db.query("SELECT count(*) FROM bounded", [], |row| {
                rows = row.get::<i64>(0).map_err(unavailable)?;
                Ok(false)
            })?;
            Ok(rows)
        }
        assert_eq!(
            clients[1].read_local(Instant::now() + HANG_READ, count),
            Ok(WRITES)
        );
        driven.drain(&mut pump);
        pump.finish();

        driven.restart(dir.path(), 1, vec![1], vec![2]);
        assert_eq!(
            driven.owner_mut(1).replica.machine_mut().read_db(count),
            Ok(WRITES)
        );
    }

    /// AC-T1.8.D1, T-T1.8.D1 (replication.md, "A replica's database log bounded by what it
    /// measures"): a local read open holds the log's restart off and sees its own view whole while
    /// the log grows under it, and a read that comes while the restart waits waits for it. Do: a
    /// voter and its learner on one filesystem, the learner told it has room for 16 frames past
    /// its own file, 1 written; a local read opened on the learner, reading member_items; within
    /// it, 2 to 40 written at the voter, each applied and flushed by the learner, and a second
    /// local read on the learner with 50 ms to run; the open read reads again and ends; then
    /// rounds. Expect: the learner's log past its room and no restart while the read is open; the
    /// second read refused Timeout; the open read's two views both [1]; a restart once it ends,
    /// and a read after sees 1 to 40.
    #[test]
    fn a_local_read_holds_off_the_logs_restart_and_one_after_waits_for_it() {
        own_account();
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = voter_and_learner(&mut driven, dir.path());
        driven
            .owner(1)
            .replica
            .machine()
            .set_free_bytes(free_for(16, 2));
        let mut pump = Pump::new(&clients);
        let (voter, learner) = (&clients[0], &clients[1]);
        let handle = learner.handle();
        write(&mut driven, &mut pump, voter, 1);
        settle_log(&mut driven, &mut pump, &clients);
        let restarts = handle.counters().database_log_restarts;
        let (first, second, frames, room, held_off, waited) = learner
            .read_local(Instant::now() + HANG_READ, |db| {
                db.query_transaction(|db| {
                    let first = values(db)?;
                    for value in 2..=40 {
                        write(&mut driven, &mut pump, voter, value);
                        settle_log(&mut driven, &mut pump, &clients);
                    }
                    let frames = driven.owner(1).replica.machine().log_frames()?;
                    let room = room(&clients, 1);
                    let held_off = handle.counters().database_log_restarts == restarts;
                    let waited =
                        learner.read_local(Instant::now() + Duration::from_millis(50), values);
                    Ok((first, values(db)?, frames, room, held_off, waited))
                })
            })
            .unwrap();
        assert!(
            frames > room,
            "the log held {frames} frames, within its room of {room}"
        );
        assert!(held_off, "the log restarted under an open read");
        assert_eq!(waited, Err(Refusal::Timeout));
        assert_eq!((first, second), (vec![1], vec![1]));
        for _ in 0..ROUNDS {
            if handle.counters().database_log_restarts > restarts {
                break;
            }
            driven.round(&mut pump);
        }
        assert!(
            handle.counters().database_log_restarts > restarts,
            "{:?}",
            handle.counters()
        );
        assert_eq!(
            learner.read_local(Instant::now() + HANG_READ, values),
            Ok((1..=40).collect::<Vec<u8>>())
        );
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.D1, T-T1.8.D1 (replication.md, "A replica's database log"; store.md, decision 15):
    /// a checkpoint pass copies only what a finished flush made durable. Do: a voter and its
    /// learner, the learner's flushes held; 1 written and the learner's log flushed; 2 to 5
    /// written and applied there unflushed; a checkpoint pass on a connection of the learner's
    /// own. Expect: the pass copied the log through the frame the flush covered and no further.
    #[test]
    fn a_checkpoint_pass_copies_no_frame_past_the_last_finished_flush() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = voter_and_learner(&mut driven, dir.path());
        driven.hold_flushes(1);
        let mut pump = Pump::new(&clients);
        write(&mut driven, &mut pump, &clients[0], 1);
        catch_up(&mut driven, &mut pump, &clients, 0, 1);
        driven.flush(1);
        let flushed = driven.owner(1).replica.machine().log_frames().unwrap();
        for value in 2..=5 {
            write(&mut driven, &mut pump, &clients[0], value);
        }
        catch_up(&mut driven, &mut pump, &clients, 0, 1);
        let machine = driven.owner(1).replica.machine();
        assert!(machine.unflushed().is_some());
        let log = machine.log_frames().unwrap();
        assert!(log > flushed, "nothing was written past the flush");
        let copied = machine.reader()().unwrap().checkpoint().unwrap();
        assert_eq!(copied.log, log);
        assert_eq!(
            copied.log - copied.left,
            flushed,
            "the pass copied through another frame than the flush covered: {copied:?}"
        );
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.L1, T-T1.8.L1 (CP-T1.8, G12 step 2; replication.md, "The raft log as the WAL"):
    /// compaction waits for the engine's flush. Do: a voter and its learner, the voter's flushes
    /// held; 1 written and the voter's log flushed; 2 to 5 written, applied unflushed; the voter's
    /// raft log compacted, keeping nothing; then its log flushed and compacted again. Expect: the
    /// writes answered with their flushes held; the first compaction moved the raft log's start
    /// to the flushed point and no further, and flushed nothing itself (2 to 5 still unflushed,
    /// still in the raft log); after the flush, the second moved it to the last entry applied.
    #[test]
    fn compaction_waits_for_the_engines_flush() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = voter_and_learner(&mut driven, dir.path());
        driven.hold_flushes(0);
        let mut pump = Pump::new(&clients);
        write(&mut driven, &mut pump, &clients[0], 1);
        driven.flush(0);
        let flushed = StateMachine::durable(driven.owner(0).replica.machine());
        for value in 2..=5 {
            write(&mut driven, &mut pump, &clients[0], value);
        }
        quiet(&mut driven, &mut pump);
        let start = |driven: &Driven| {
            driven
                .owner(0)
                .replica
                .core()
                .raft
                .log()
                .first_index()
                .unwrap()
        };
        let compact = |driven: &mut Driven, pump: &mut Pump<'_>| {
            driven.quiet(pump);
            let owner = driven.owner_mut(0);
            let now = owner.now().unwrap();
            let out = owner
                .replica
                .compact(0, now, std::task::Waker::noop())
                .unwrap();
            assert!(out, "no compaction went out");
            driven.quiet(pump);
        };
        let applied = driven.owner(0).replica.applied();
        assert!(
            applied.index > flushed.index,
            "2 to 5 weren't applied past the flush"
        );
        assert!(
            start(&driven) <= flushed.index,
            "the raft log already started past the flush"
        );
        compact(&mut driven, &mut pump);
        assert_eq!(
            start(&driven),
            flushed.index + 1,
            "the raft log's start isn't the flushed point"
        );
        let machine = driven.owner(0).replica.machine();
        assert_eq!(machine.unflushed(), Some(applied), "the compaction flushed");
        assert_eq!(StateMachine::durable(machine), flushed);
        driven.flush(0);
        compact(&mut driven, &mut pump);
        assert_eq!(start(&driven), applied.index + 1);
        assert_eq!(held(&mut driven, 0), vec![1, 2, 3, 4, 5]);
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.D1, T-T1.8.D1 (replication.md, "A replica's database log bounded by what it
    /// measures", the bound stated): a restart that falls due while a local read is out waits for
    /// that read, and so no longer than the read's deadline. Do: a voter and its learner on one
    /// filesystem, the learner told it has room for 16 frames past its own file; a local read on
    /// the learner, on a thread of its own, running a query with no end under a deadline on a
    /// clock the test moves (as `Database::query_local` puts one on every read), standing at the
    /// read's start until the test passes it, so no wall-clock window decides it; once it reads, 2
    /// to 40 written at the voter, each applied and flushed by the learner; then rounds; then the
    /// read's deadline passed, and rounds until a restart is counted. Expect: the read still out
    /// after the writes; the learner's log past its room and no restart while the read runs; the
    /// read refused at its deadline; a restart after it.
    #[test]
    fn a_restart_due_waits_for_a_reads_deadline_and_no_longer() {
        own_account();
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = voter_and_learner(&mut driven, dir.path());
        driven
            .owner(1)
            .replica
            .machine()
            .set_free_bytes(free_for(16, 2));
        let mut pump = Pump::new(&clients);
        let (voter, learner) = (&clients[0], &clients[1]);
        let handle = learner.handle();
        write(&mut driven, &mut pump, voter, 1);
        settle_log(&mut driven, &mut pump, &clients);
        let restarts = handle.counters().database_log_restarts;
        let passed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        std::thread::scope(|scope| {
            let read = {
                let passed = Arc::clone(&passed);
                scope.spawn(move || {
                    learner.read_local(Instant::now() + HANG_READ, move |db| {
                        // The read's clock stands at its start until the test passes its
                        // deadline: on the real clock a slow host's writes outlast any window,
                        // and the read ends before the test means it to.
                        let began = Instant::now();
                        let until = began + HANG_READ;
                        let _deadline = db.deadline_on(until, move || {
                            if passed.load(Ordering::Acquire) {
                                until
                            } else {
                                began
                            }
                        });
                        db.query(
                            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n) SELECT count(*) FROM n",
                            [],
                            |_| Ok(false),
                        )?;
                        Ok(())
                    })
                })
            };
            for _ in 0..ROUNDS {
                if handle.counters().local_readers_in_use == 1 {
                    break;
                }
                std::thread::park_timeout(POLL);
            }
            assert_eq!(handle.counters().local_readers_in_use, 1);
            for value in 2..=40 {
                write(&mut driven, &mut pump, voter, value);
                settle_log(&mut driven, &mut pump, &clients);
            }
            assert_eq!(
                handle.counters().local_readers_in_use,
                1,
                "the read ended before its deadline was passed"
            );
            let frames = driven.owner(1).replica.machine().log_frames().unwrap();
            let room = room(&clients, 1);
            assert!(frames > room, "the log held {frames} frames, room {room}");
            for _ in 0..64 {
                driven.round(&mut pump);
            }
            assert_eq!(
                handle.counters().database_log_restarts,
                restarts,
                "a restart under a read"
            );
            passed.store(true, Ordering::Release);
            let answer = read.join().unwrap();
            assert!(
                answer.is_err(),
                "the read ran past its deadline: {answer:?}"
            );
        });
        for _ in 0..ROUNDS {
            if handle.counters().database_log_restarts > restarts {
                break;
            }
            driven.round(&mut pump);
        }
        assert!(
            handle.counters().database_log_restarts > restarts,
            "{:?}",
            handle.counters()
        );
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.D1, T-T1.8.D1 (replication.md, "A replica's database log bounded by what it
    /// measures", B): a local read begun on a log wholly copied reads the file alone and holds
    /// every checkpoint pass off, so a restart due by its share holds reads once a pass finds the
    /// log busy. Do: a voter and its learner, the room the filesystem's own; 1 written, flushed and
    /// wholly copied by a pass; a local read opened on the learner; within it, 2 to 40 written,
    /// each applied and flushed, a pass on a connection of the learner's own; the learner's count
    /// of its restarts forgotten (a restart due by its share), then rounds until a local read
    /// with 1 ms to run is refused. Expect: the first pass left nothing to copy; the pass under
    /// the read busy (it copied nothing and said nothing: the default); the log within its room;
    /// a read refused Timeout, the reads held for the restart.
    #[test]
    fn a_read_on_a_wholly_copied_log_holds_passes_off_and_a_busy_pass_holds_reads() {
        own_account();
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = voter_and_learner(&mut driven, dir.path());
        let mut pump = Pump::new(&clients);
        let (voter, learner) = (&clients[0], &clients[1]);
        write(&mut driven, &mut pump, voter, 1);
        settle_log(&mut driven, &mut pump, &clients);
        let mut copier = driven.owner(1).replica.machine().reader()().unwrap();
        let whole = copier.checkpoint().unwrap();
        assert_eq!(whole.left, 0, "{whole:?}");
        let (busy, frames, room, waited) = learner
            .read_local(Instant::now() + HANG_READ, |db| {
                db.query_transaction(|db| {
                    values(db)?;
                    for value in 2..=40 {
                        write(&mut driven, &mut pump, voter, value);
                        settle_log(&mut driven, &mut pump, &clients);
                    }
                    let busy = copier.checkpoint()?;
                    let frames = driven.owner(1).replica.machine().log_frames()?;
                    driven.owner_mut(1).spacing = log_bound::Spacing::default();
                    driven.owner_mut(1).spacing.stepped(1, Duration::ZERO, 0)?;
                    let mut waited = Ok(Vec::new());
                    for _ in 0..ROUNDS {
                        driven.round(&mut pump);
                        waited =
                            learner.read_local(Instant::now() + Duration::from_millis(1), values);
                        if waited.is_err() {
                            break;
                        }
                    }
                    Ok((busy, frames, room(&clients, 1), waited))
                })
            })
            .unwrap();
        assert_eq!(
            busy,
            Checkpointed::default(),
            "a pass copied beside the read"
        );
        assert!(
            frames <= room,
            "the log held {frames} frames, past its room {room}"
        );
        assert_eq!(waited, Err(Refusal::Timeout));
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.D1, T-T1.8.D1 (replication.md, "A replica's database log bounded by what it
    /// measures", B): the owner holds reads past the log's room whatever a pass said, and with a
    /// restart due once a pass copied every durable frame or found the log busy; otherwise not.
    /// Do: the rule at the states it meets. Expect: as said.
    #[test]
    fn the_owner_holds_reads_past_the_room_or_once_a_due_restart_can_go() {
        let pass = |caught_up, busy| Pass {
            caught_up,
            busy,
            room: 64,
            unread: None,
        };
        assert!(hold_due(&pass(false, false), true, true));
        assert!(hold_due(&pass(false, false), false, true));
        assert!(hold_due(&pass(true, false), true, false));
        assert!(hold_due(&pass(false, true), true, false));
        assert!(!hold_due(&pass(false, false), true, false));
        assert!(!hold_due(&pass(true, true), false, false));
    }

    /// AC-T1.8.D1, T-T1.8.D3 (replication.md, "A replica's database log bounded by what it
    /// measures", A): the first restart is free, a restart that held entries spaces the next by a
    /// thousand times them and the guess. Do: a voter and its learner, the room the filesystem's
    /// own; 1 written and flushed, rounds until the learner restarts; its count then told of a
    /// restart that held one entry; 2 to 21 written and flushed; then its count told of a million
    /// entries applied (its generation covering its spend), 22 written, rounds. Expect: a restart
    /// after 1 (on b151af0 none: its bound was 131,072 frames); none while 2 to 21 applied; one
    /// once its generation covered its spend.
    #[test]
    fn the_first_restart_is_free_and_the_next_waits_on_its_share() {
        own_account();
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = voter_and_learner(&mut driven, dir.path());
        let mut pump = Pump::new(&clients);
        let handle = clients[1].handle();
        let restarted = |driven: &mut Driven, pump: &mut Pump<'_>, after: u64| {
            for _ in 0..ROUNDS {
                if handle.counters().database_log_restarts > after {
                    return true;
                }
                driven.round(pump);
            }
            false
        };
        write(&mut driven, &mut pump, &clients[0], 1);
        settle_log(&mut driven, &mut pump, &clients);
        assert!(
            restarted(&mut driven, &mut pump, 0),
            "no restart: {:?}",
            handle.counters()
        );
        let spacing = &mut driven.owner_mut(1).spacing;
        spacing.restarted(0).unwrap();
        spacing.stepped(1, Duration::ZERO, 1).unwrap();
        spacing.stepped(0, Duration::ZERO, 0).unwrap();
        let restarts = handle.counters().database_log_restarts;
        for value in 2..=21 {
            write(&mut driven, &mut pump, &clients[0], value);
            settle_log(&mut driven, &mut pump, &clients);
        }
        for _ in 0..64 {
            driven.round(&mut pump);
        }
        assert_eq!(handle.counters().database_log_restarts, restarts);
        driven
            .owner_mut(1)
            .spacing
            .stepped(1_000_000, Duration::ZERO, 0)
            .unwrap();
        write(&mut driven, &mut pump, &clients[0], 22);
        settle_log(&mut driven, &mut pump, &clients);
        assert!(
            restarted(&mut driven, &mut pump, restarts),
            "no restart once its generation covered its spend: {:?}",
            handle.counters()
        );
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.D1, T-T1.8.D3 (replication.md, "A replica's database log bounded by what it
    /// measures", A; amphora-owner-t1-log-bound-004f682-20261111): an owner kept busy at a steady
    /// queue depth after a restart ends the restart's hold, its queue read on one basis at the
    /// restart and at each step. Do: a voter and its learner, 1 written and flushed, the learner's
    /// share spent, 2 written and flushed, then its count of restarts forgotten (a restart due by
    /// its share, its log holding 2's frames); the learner stepped on
    /// its own, each step taking one input with one more always waiting behind it, until it
    /// restarts, then 8 steps more. Expect: a restart, and its hold ended.
    #[test]
    fn a_restart_hold_ends_on_an_owner_busy_at_a_steady_queue() {
        own_account();
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = voter_and_learner(&mut driven, dir.path());
        let mut pump = Pump::new(&clients);
        write(&mut driven, &mut pump, &clients[0], 1);
        settle_log(&mut driven, &mut pump, &clients);
        // Its share spent, so the log holds 2's frames when the test makes a restart due.
        let spacing = &mut driven.owner_mut(1).spacing;
        spacing.restarted(0).unwrap();
        spacing.stepped(1_000_000, Duration::ZERO, 1).unwrap();
        write(&mut driven, &mut pump, &clients[0], 2);
        settle_log(&mut driven, &mut pump, &clients);
        driven.drain(&mut pump);
        pump.finish();
        let handle = clients[1].handle();
        assert!(driven.owner(1).replica.machine().log_frames().unwrap() > 0);
        let restarts = handle.counters().database_log_restarts;
        let (mut learner, inputs) = driven.owners.get_mut(1).unwrap().take().unwrap();
        learner.spacing = log_bound::Spacing::default();
        learner.spacing.stepped(1, Duration::ZERO, 0).unwrap();
        let mut answers = Vec::new();
        let mut ask = || {
            let (response, answer) = mpsc::sync_channel(1);
            handle
                .enqueue(
                    Input::CatchUp {
                        until: Instant::now() + HANG_READ,
                        response,
                    },
                    CHANGE_BYTES,
                )
                .unwrap();
            answers.push(answer);
        };
        ask();
        let mut after = None;
        for _ in 0..ROUNDS {
            ask();
            learner.run_step(Some(inputs.try_recv().unwrap())).unwrap();
            if after.is_none() && handle.counters().database_log_restarts > restarts {
                after = Some(0);
            }
            if let Some(steps) = after.as_mut() {
                *steps += 1;
                if *steps > 8 {
                    break;
                }
            }
            std::thread::park_timeout(POLL);
        }
        assert!(after.is_some(), "no restart: {:?}", handle.counters());
        assert!(!learner.spacing.holding(), "the hold never ended");
        drop((learner, inputs, answers));
    }

    /// AC-T1.8.D1, T-T1.8.D3 (replication.md, "A replica's database log bounded by what it
    /// measures", C): past its room a replica refuses submits, typed, and a restart is due
    /// whatever its share says: disk before latency. Do: a voter and its learner; the voter's
    /// share spent (a restart that held a million entries) and its filesystem told it has nothing
    /// free past one image; a local read held open on the voter while it is written to until a
    /// write is refused; the read ended, rounds until the voter restarts, then a write. Expect: a
    /// write refused Unavailable naming the frames past the room; a restart once the read ended;
    /// the write after it taken.
    #[test]
    fn past_its_room_a_replica_refuses_submits_and_restarts_whatever_its_share() {
        own_account();
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = voter_and_learner(&mut driven, dir.path());
        let mut pump = Pump::new(&clients);
        let (voter, handle) = (&clients[0], clients[0].handle());
        write(&mut driven, &mut pump, voter, 1);
        settle_log(&mut driven, &mut pump, &clients);
        let spacing = &mut driven.owner_mut(0).spacing;
        spacing.restarted(0).unwrap();
        spacing.stepped(1_000_000, Duration::ZERO, 1).unwrap();
        driven
            .owner(0)
            .replica
            .machine()
            .set_free_bytes(free_for(0, 2));
        let restarts = handle.counters().database_log_restarts;
        let refused = voter
            .read_local(Instant::now() + HANG_READ, |db| {
                db.query_transaction(|db| {
                    values(db)?;
                    for value in 2..=40 {
                        let answer = driven.call(&mut pump, voter, move |db| {
                            db.exec(&format!("INSERT INTO member_items VALUES({value})"))?;
                            Ok(Vec::new())
                        });
                        if let Err(refusal) = answer {
                            return Ok(Some(refusal));
                        }
                        settle_log(&mut driven, &mut pump, &clients);
                    }
                    Ok(None)
                })
            })
            .unwrap();
        assert!(
            matches!(&refused, Some(Refusal::Unavailable(why)) if why.contains("frames, past the")),
            "{refused:?}"
        );
        assert_eq!(handle.counters().database_log_restarts, restarts);
        for _ in 0..ROUNDS {
            if handle.counters().database_log_restarts > restarts {
                break;
            }
            driven.round(&mut pump);
        }
        assert!(
            handle.counters().database_log_restarts > restarts,
            "{:?}",
            handle.counters()
        );
        write(&mut driven, &mut pump, voter, 41);
        driven.drain(&mut pump);
        pump.finish();
    }

    /// AC-T1.8.D1, T-T1.8.D3 (replication.md, "A replica's database log bounded by what it
    /// measures", C, the follower's limit): a member past its room still applies what its group
    /// committed, and a disk that then fills fences it, typed, its database whole on reopen. Do: a
    /// voter and its learner, 1 to 5 written and flushed; the learner told it has nothing free
    /// past one image; a local read held open on the learner (so no restart brings it back within
    /// its room), within it 6 on written and flushed until the learner is past its room, then the
    /// learner's flushing thread's flushes held and 4 more written at the voter and applied by the
    /// learner; the read ended and the relay finished; the learner's disk made full (its flushes
    /// fail as a full disk fails them) and its flushes let go, the learner stepped on its own until
    /// it ends; the learner opened again on its files. Expect: the learner past its room, refusing
    /// submits, yet applying the 4; then fenced, saying its flush failed; reopened, it holds every
    /// value written.
    #[test]
    fn a_follower_past_its_room_applies_on_and_fences_when_its_disk_fills() {
        own_account();
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = voter_and_learner(&mut driven, dir.path());
        let mut pump = Pump::new(&clients);
        for value in 1..=5 {
            write(&mut driven, &mut pump, &clients[0], value);
            settle_log(&mut driven, &mut pump, &clients);
        }
        let gate = driven.owner(1).replica.machine().flush_gate();
        driven
            .owner(1)
            .replica
            .machine()
            .set_free_bytes(free_for(0, 2));
        // Its room is about half what its log's file holds: written to under the read, which
        // holds every restart off, the log outgrows it.
        let (full, last) = clients[1]
            .read_local(Instant::now() + HANG_READ, |db| {
                db.query_transaction(|db| {
                    values(db)?;
                    let mut value = 5;
                    while driven.owner(1).log_full.is_none() && value < u8::MAX - 4 {
                        value += 1;
                        write(&mut driven, &mut pump, &clients[0], value);
                        settle_log(&mut driven, &mut pump, &clients);
                    }
                    let full = driven.owner(1).log_full.clone();
                    gate.hold();
                    for more in 1..=4 {
                        write(&mut driven, &mut pump, &clients[0], value + more);
                    }
                    catch_up(&mut driven, &mut pump, &clients, 0, 1);
                    Ok((full, value + 4))
                })
            })
            .unwrap();
        assert!(
            full.as_deref()
                .is_some_and(|why| why.contains("frames, past the")),
            "{full:?}"
        );
        driven.drain(&mut pump);
        pump.finish();

        let (mut learner, inputs) = driven.owners.get_mut(1).unwrap().take().unwrap();
        gate.fill();
        gate.open();
        let mut ended = None;
        for _ in 0..ROUNDS {
            if let Err(refusal) = learner.run_step(None) {
                ended = Some(refusal);
                break;
            }
            std::thread::park_timeout(POLL);
        }
        assert!(
            matches!(&ended, Some(Refusal::Fenced(why)) if why.contains("failed a flush")),
            "{ended:?}"
        );
        drop((learner, inputs));

        driven.restart(dir.path(), 1, vec![1], vec![2]);
        assert_eq!(held(&mut driven, 1), (1..=last).collect::<Vec<u8>>());
    }

    /// AC-T1.8.D1, T-T1.8.D3 (replication.md, "A replica's database log bounded by what it
    /// measures", C.1): the replica logs counted on a filesystem are those whose flushing thread
    /// runs. Do: two owners opened on one directory, then one of them dropped. Expect: two logs
    /// counted on its filesystem, then one once the dropped owner's flushing thread ended, and so
    /// the other's room larger.
    #[test]
    fn a_dropped_replica_leaves_its_filesystems_room_to_the_others() {
        own_account();
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = voter_and_learner(&mut driven, dir.path());
        let account = account();
        let filesystem = driven.owner(0).replica.machine().filesystem().unwrap();
        let logs = || *account.logs.lock().unwrap().get(&filesystem).unwrap();
        assert_eq!(logs(), 2);
        drop(driven.owners.get_mut(1).unwrap().take());
        for _ in 0..ROUNDS {
            if logs() == 1 {
                break;
            }
            std::thread::park_timeout(POLL);
        }
        assert_eq!(logs(), 1);
        let free = free_for(64, 2);
        let image = u64::try_from(owner_image_room().unwrap()).unwrap();
        assert!(log_bound::log_room(free, image, 1, 0) > log_bound::log_room(free, image, 2, 0));
        drop(clients);
    }

    /// AC-T1.10.L1 (replication.md, "A local read on a replica"; kiln 289db3175f3's fix to its
    /// 0003): an owner dropped without its loop's end, as a crash drops it, gives back its lease,
    /// though its local readers held its database. Do: one voter, 1 written, a local read, which
    /// leaves its reader idle in the pool; the owner dropped mid-run and the member opened again
    /// on its own files. Expect: it opens, and reads 1 locally.
    #[test]
    fn a_dropped_owner_gives_back_its_lease_through_its_local_readers() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients = vec![driven.open(dir.path(), 1, vec![1], None)];
        let mut pump = Pump::new(&clients);
        driven.elect(&mut pump, &clients, 0, 1);
        write(&mut driven, &mut pump, &clients[0], 1);
        assert_eq!(
            clients[0].read_local(Instant::now() + HANG_READ, values),
            Ok(vec![1])
        );
        driven.drain(&mut pump);
        pump.finish();

        let restarted = driven.restart(dir.path(), 0, vec![1], Vec::new());
        assert_eq!(
            restarted.read_local(Instant::now() + HANG_READ, values),
            Ok(vec![1])
        );
    }

    mod receive_receipts {
        use super::*;

        /// Steps `driven` until `receipt` completes.
        fn finish(driven: &mut Driven, receipt: &mut ReceiveReceipt) -> Result<(), Refusal> {
            for _ in 0..ROUNDS {
                match receipt.try_result() {
                    Ok(None) => driven.step(),
                    Ok(Some(())) => return Ok(()),
                    Err(error) => return Err(error),
                }
            }
            panic!("a receive receipt was unanswered after {ROUNDS} rounds");
        }

        /// CP-T1.8 on driven time: admission must not wait behind a SQL capture or acknowledge
        /// validation. Do: from inside a call's work, while a single voter runs it, admit records
        /// up to the queue's bound, one of them through the waiting `receive`. Expect: each
        /// admission answered at once, the bound's refusal, the waiting `receive` still waiting;
        /// then, once the owner runs again, each record's own refusal.
        #[test]
        fn admission_is_bounded_while_processed_receive_waits_and_errors_remain_observable() {
            let dir = tempfile::tempdir().unwrap();
            let mut driven = Driven::new();
            let clients = vec![driven.open(dir.path(), 1, vec![1], None)];
            let mut pump = Pump::new(&clients);
            driven.elect(&mut pump, &clients, 0, 1);
            let handle = clients.first().unwrap().handle().clone();
            let until = driven.until();
            let (kept, taken) = mpsc::sync_channel(1);
            let capture = submit(clients.first().unwrap(), until, move |_| {
                // The owner is inside this capture: nothing admitted now can be processed yet.
                let mut receipts = Vec::new();
                for _ in 0..COMMANDS - 1 {
                    receipts.push(handle.admit_receive(2, vec![0xff], until).unwrap());
                }
                assert!(
                    receipts
                        .iter_mut()
                        .all(|r| matches!(r.try_result(), Ok(None)))
                );
                let legacy = handle.clone();
                let (finished, finishing) = mpsc::sync_channel(1);
                let processed = std::thread::spawn(move || {
                    finished.send(legacy.receive(2, vec![0xff], until)).unwrap();
                });
                // Wait for the legacy record's actual queue reservation, not thread scheduling.
                for _ in 0..ROUNDS {
                    if handle.shared.admitted.load(Ordering::Acquire) == api::REQUEST_MAX + COMMANDS
                    {
                        break;
                    }
                    std::thread::park_timeout(POLL);
                }
                assert_eq!(
                    handle.shared.admitted.load(Ordering::Acquire),
                    api::REQUEST_MAX + COMMANDS
                );
                assert!(matches!(
                    finishing.try_recv(),
                    Err(mpsc::TryRecvError::Empty)
                ));
                assert!(matches!(
                    handle.admit_receive(2, vec![0xff], until),
                    Err(Refusal::Full { limit: COMMANDS })
                ));
                kept.send((receipts, finishing, processed)).unwrap();
                Ok(Vec::new())
            });
            assert_eq!(driven.settle(&mut pump, &capture), Ok(Vec::new()));
            let (mut receipts, finishing, processed) = taken.try_recv().unwrap();
            for receipt in &mut receipts {
                assert!(matches!(
                    finish(&mut driven, receipt),
                    Err(Refusal::Invalid { .. })
                ));
                assert!(matches!(receipt.try_result(), Err(Refusal::Invalid { .. })));
            }
            // The owner answered the legacy record with the others; its thread passes it on.
            processed.join().unwrap();
            assert!(matches!(
                finishing.try_recv().unwrap(),
                Err(Refusal::Invalid { .. })
            ));
            driven.drain(&mut pump);
            pump.finish();
        }

        /// CP-T1.8 on driven time: only processing a valid authenticated record completes a
        /// success receipt. Do: admit a member's heartbeat to a lone member of three, and poll its
        /// receipt before and after stepping the member. Expect: pending before, done after, a
        /// second poll of the completed receipt refused, and a record whose deadline has already
        /// passed refused at admission.
        #[test]
        fn authenticated_heartbeat_completes_through_the_existing_actor_step() {
            fn send_sync<T: Send + Sync>() {}
            send_sync::<ReceiveReceipt>();
            let dir = tempfile::tempdir().unwrap();
            let mut driven = Driven::new();
            let client = driven.open(dir.path(), 1, vec![1, 2, 3], None);
            let mut bytes = Vec::new();
            Message {
                msg_type: hyper_raft::proto::MessageType::MsgHeartbeat,
                from: 2,
                to: 1,
                term: 1,
                ..Message::default()
            }
            .encode(&mut bytes);
            let mut receipt = client
                .handle()
                .admit_receive(2, bytes, driven.until())
                .unwrap();
            assert!(matches!(receipt.try_result(), Ok(None)));
            assert!(finish(&mut driven, &mut receipt).is_ok());
            assert!(matches!(receipt.try_result(), Err(Refusal::Invalid { .. })));
            assert!(matches!(
                client.handle().admit_receive(2, Vec::new(), Instant::now()),
                Err(Refusal::Timeout)
            ));
        }

        /// CP-T1.8: queued completion wins over expiry; missing replies expire or fence explicitly.
        #[test]
        fn receipt_poll_preserves_ready_result_and_typed_terminal_errors() {
            let (sender, receiver) = mpsc::sync_channel(1);
            sender.send(Ok(())).unwrap();
            let mut receipt = ReceiveReceipt {
                receiver: Mutex::new(receiver),
                until: Instant::now(),
                completed: false,
            };
            assert!(matches!(receipt.try_result(), Ok(Some(()))));

            let (sender, receiver) = mpsc::sync_channel(1);
            let mut receipt = ReceiveReceipt {
                receiver: Mutex::new(receiver),
                until: Instant::now(),
                completed: false,
            };
            assert!(matches!(receipt.try_result(), Err(Refusal::Timeout)));
            drop(sender);

            // An owner gone before the deadline fences the receipt.
            let (sender, receiver) = mpsc::sync_channel(1);
            let mut receipt = ReceiveReceipt {
                receiver: Mutex::new(receiver),
                until: Instant::now() + Duration::from_secs(3600),
                completed: false,
            };
            drop(sender);
            assert!(matches!(receipt.try_result(), Err(Refusal::Fenced(_))));
        }
    }

    /// One series of a measurement: at most `CAP` samples kept, the rest counted (CLAUDE.md
    /// section 2).
    #[derive(Default)]
    struct Series {
        samples: Vec<Duration>,
        dropped: u64,
    }
    impl Series {
        const CAP: usize = 4_000_000;
        fn add(&mut self, d: Duration) {
            if self.samples.len() < Self::CAP {
                self.samples.push(d);
            } else {
                self.dropped += 1;
            }
        }
        fn over(&self, limit: Duration) -> usize {
            self.samples.iter().filter(|d| **d > limit).count()
        }
        /// p50, p99, p99.9 and max, in ms, and the count.
        fn summary(&self) -> String {
            let mut sorted = self.samples.clone();
            sorted.sort();
            // The sample at rank ceil(n · per_mille / 1000), as before in integers.
            let q = |per_mille: usize| {
                let i = (sorted.len() * per_mille).div_ceil(1000).saturating_sub(1);
                sorted.get(i).copied().unwrap_or_default().as_secs_f64() * 1e3
            };
            format!(
                "p50 {:.3} ms p99 {:.3} ms p99.9 {:.3} ms max {:.3} ms ({} samples{})",
                q(500),
                q(990),
                q(999),
                sorted.last().copied().unwrap_or_default().as_secs_f64() * 1e3,
                sorted.len(),
                if self.dropped > 0 {
                    format!(", {} past the cap", self.dropped)
                } else {
                    String::new()
                }
            )
        }
    }

    /// What one owner's measurement saw (kiln goal 6b112f14, step 1): its step and tick gaps, its
    /// log writes' times from submit to durable, each follower's answer gaps while it led, and
    /// each change of leader with its cause.
    #[derive(Default)]
    pub(super) struct Stalls {
        last_step: Option<Instant>,
        last_tick: Option<Instant>,
        steps: Series,
        ticks: Series,
        writes: Series,
        last_answer: std::collections::BTreeMap<u64, Instant>,
        answers: std::collections::BTreeMap<u64, Series>,
        term: u64,
        leader: u64,
        /// Times this member, leading, was stepped down by its quorum check in its own term.
        step_downs: u64,
        /// Terms it moved to: elections.
        elections: u64,
        changes: Vec<String>,
    }
    impl Stalls {
        pub(super) fn stepped(&mut self, at: Instant) {
            if let Some(last) = self.last_step.replace(at) {
                self.steps.add(at.saturating_duration_since(last));
            }
        }
        pub(super) fn ticked(&mut self, at: Instant) {
            if let Some(last) = self.last_tick.replace(at) {
                self.ticks.add(at.saturating_duration_since(last));
            }
        }
        pub(super) fn wrote(&mut self, took: Duration) {
            self.writes.add(took);
        }
        pub(super) fn answered(&mut self, from: u64, at: Instant) {
            if let Some(last) = self.last_answer.insert(from, at) {
                self.answers
                    .entry(from)
                    .or_default()
                    .add(at.saturating_duration_since(last));
            }
        }
        /// A change of leader: a step-down by the quorum check when this owner led and stops
        /// leading in the same term, an election when the term moves.
        pub(super) fn led(&mut self, term: u64, leader: u64, id: u64) {
            if (term, leader) == (self.term, self.leader) {
                return;
            }
            let cause = if term != self.term {
                self.elections += 1;
                "an election"
            } else if self.leader == id && leader != id {
                self.step_downs += 1;
                "its quorum check stepped it down"
            } else if leader == 0 {
                "its leader stepped down"
            } else {
                "the election's leader learned"
            };
            if self.term != 0 {
                self.changes.push(format!(
                    "term {} leader {} -> term {term} leader {leader}: {cause}",
                    self.term, self.leader
                ));
            }
            self.last_answer.clear();
            (self.term, self.leader) = (term, leader);
        }
    }

    /// Kiln goal 6b112f14, step 1 (replication.md, "The owner's time"): how long a production
    /// owner's writes and periods stall on this host. Do: three voters on this host's disk under
    /// ~/.cache (Disk::Flushed), on the real clock, relayed in-process, a write every 20 ms at the
    /// first member for AMPHORA_STALL_SECONDS (default 1,800). Expect: for each member, the
    /// p50, p99, p99.9 and max of its log writes, steps and ticks, each follower's answer gaps as
    /// its leader saw them, the gaps over 200 ms and the leader changes with their causes; and
    /// none of the series empty.
    #[test]
    #[ignore = "a measurement of this host: AMPHORA_STALL_SECONDS of three owners on its disk"]
    fn production_owner_stalls_measured_on_this_host() {
        let seconds: u64 = std::env::var("AMPHORA_STALL_SECONDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1_800);
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .unwrap();
        let root = Path::new(&home).join(".cache").join("amphora-stalls");
        std::fs::create_dir_all(&root).unwrap();
        let dir = tempfile::tempdir_in(&root).unwrap();
        let until = Instant::now() + Duration::from_secs(seconds + 600);
        let mut clients = Vec::new();
        let mut records = Vec::new();
        let mut owners = Vec::new();
        for node_id in 1..=3u64 {
            let path = dir.path().join(format!("state-{node_id}.db"));
            let (client, mut actor, receiver) = super::open(
                &path,
                &dir.path().join(format!("raft-{node_id}.log")),
                ReplicaConfig {
                    node_id,
                    voters: vec![1, 2, 3],
                    learners: Vec::new(),
                    bootstrap: None,
                },
                Durability::Persisted,
                Arc::new(Changed::default()),
                Owned::unbudgeted(lease(&path), None),
                until,
                Arc::new(Instant::now),
            )
            .unwrap();
            let stalls = Arc::new(Mutex::new(Stalls::default()));
            actor.stalls = Some(Arc::clone(&stalls));
            records.push(stalls);
            owners.push(std::thread::spawn(move || actor.run(receiver)));
            clients.push(client);
        }
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handles: Vec<ReplicaHandle> = clients.iter().map(Client::handle).collect();
        let relay = {
            let (stop, handles) = (Arc::clone(&stop), handles.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let mut moved = 0;
                    for (from, handle) in (1u64..).zip(&handles) {
                        for record in handle.drain(OUTGOING_MESSAGES).unwrap_or_default() {
                            let to = usize::try_from(record.to - 1).unwrap();
                            let deadline = Instant::now() + Duration::from_secs(5);
                            let _ = handles
                                .get(to)
                                .unwrap()
                                .receive(from, record.bytes, deadline);
                            moved += 1;
                        }
                    }
                    if moved == 0 {
                        std::thread::park_timeout(POLL);
                    }
                }
            })
        };
        let writer = clients.first().unwrap();
        let first = Instant::now();
        loop {
            match submit(writer, Instant::now() + Duration::from_secs(30), |db| {
                db.exec("CREATE TABLE IF NOT EXISTS kiln_like(id INTEGER PRIMARY KEY, body BLOB)")?;
                Ok(Vec::new())
            })
            .recv()
            .unwrap()
            {
                Ok(_) => break,
                Err(_) if first.elapsed() < Duration::from_secs(120) => {
                    std::thread::park_timeout(Duration::from_millis(100))
                }
                Err(error) => panic!("no leader took the first write: {error:?}"),
            }
        }
        let (mut acknowledged, mut refused) = (0u64, 0u64);
        let start = Instant::now();
        let mut next = start;
        while start.elapsed() < Duration::from_secs(seconds) {
            let answer = submit(writer, Instant::now() + Duration::from_secs(30), |db| {
                db.exec("INSERT INTO kiln_like(body) VALUES (randomblob(512))")?;
                Ok(Vec::new())
            });
            match answer.recv() {
                Ok(Ok(_)) => acknowledged += 1,
                _ => refused += 1,
            }
            next += Duration::from_millis(20);
            // A pacing wait; an early wake waits again for what is left.
            while let Some(wait) = next.checked_duration_since(Instant::now()) {
                std::thread::park_timeout(wait);
            }
        }
        stop.store(true, Ordering::Relaxed);
        relay.join().unwrap();
        drop(handles);
        drop(clients);
        for owner in owners {
            let _ = owner.join();
        }
        println!(
            "stalls: {seconds} s on {}, load {}, {acknowledged} writes acknowledged, {refused} refused",
            dir.path().display(),
            std::fs::read_to_string("/proc/loadavg")
                .unwrap_or_else(|_| "?".into())
                .trim()
        );
        let limit = Duration::from_millis(200);
        for (node_id, record) in (1..).zip(&records) {
            let stalls = record.lock().unwrap();
            println!(
                "member {node_id}: log write submit to durable: {}",
                stalls.writes.summary()
            );
            println!(
                "member {node_id}: owner step gap: {}",
                stalls.steps.summary()
            );
            println!(
                "member {node_id}: owner tick gap: {}",
                stalls.ticks.summary()
            );
            for (from, gaps) in &stalls.answers {
                println!(
                    "member {node_id}: answer gaps from {from} while leading: {}; over 200 ms: {}",
                    gaps.summary(),
                    gaps.over(limit)
                );
            }
            println!(
                "member {node_id}: step-downs by its own quorum check: {}; elections: {}; changes seen: {}",
                stalls.step_downs,
                stalls.elections,
                stalls.changes.len()
            );
            for change in &stalls.changes {
                println!("member {node_id}:   {change}");
            }
            assert!(
                !stalls.writes.samples.is_empty()
                    && !stalls.steps.samples.is_empty()
                    && !stalls.ticks.samples.is_empty(),
                "member {node_id} counted no writes, steps or ticks"
            );
        }
        assert!(
            records
                .iter()
                .any(|r| !r.lock().unwrap().answers.is_empty()),
            "no leader counted an answer"
        );
        assert!(acknowledged > 0, "no write was acknowledged");
    }

    use std::collections::BTreeSet;

    /// One list-append transaction of the write slot's isolation checks (Elle's list-append,
    /// Kingsbury and Alvaro, PVLDB 14(3), 2021): it read `key`'s and `other`'s lists and appended
    /// `element` to `key`'s.
    struct Append {
        element: String,
        key: u64,
        other: u64,
        read: Option<(Vec<String>, Vec<String>)>,
        outcome: Outcome,
    }
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Outcome {
        Pending,
        Committed,
        Refused,
        Unknown,
    }
    /// What one list-append run left: its history's violations and, by writer, its counts.
    struct ListRun {
        violations: Vec<String>,
        committed: Vec<u64>,
        // Attempts refused as stale, and the most refused in a row before one committed.
        refused: Vec<u64>,
        streak: Vec<u64>,
        unknown: u64,
        // The leader's writes in its turn after a slot, and those refused.
        turns: (u64, u64),
        // Stale bases the leader moved by their read sets.
        moved: u64,
    }
    type Answering = mpsc::Receiver<Result<Answer, Refusal>>;
    fn list(db: &mut Db, key: u64) -> anyhow::Result<Vec<String>> {
        let mut out = Vec::new();
        db.query_owned(&format!("SELECT v FROM lists WHERE k = {key}"), [], |row| {
            out = row
                .get::<String>(0)?
                .split_whitespace()
                .map(str::to_string)
                .collect();
            Ok(false)
        })?;
        Ok(out)
    }
    /// Each owner in `writers` (the leader first) makes `ops` list-append writes over `keys`
    /// lists, one at a time, a refused one made again as a new write, as kiln's `store::write`
    /// evaluates its closure again; every record is delayed up to three relay rounds. Each list's
    /// row is padded to half a page, so lists far apart sit on different pages. With `slot_off`,
    /// the leader holds no write slot: today's rule alone.
    fn list_append_run(
        writers: &[usize],
        slot_off: bool,
        seed: u64,
        keys: u64,
        ops: u64,
    ) -> ListRun {
        list_append_run_with(writers, slot_off, ReadSets::On, seed, keys, ops)
    }
    /// How a list-append run's leader treats read sets: as built, off (the exact base alone), or
    /// the mutant that moves every stale base unchecked.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum ReadSets {
        On,
        Off,
        Unchecked,
    }
    fn list_append_run_with(
        writers: &[usize],
        slot_off: bool,
        read_sets: ReadSets,
        seed: u64,
        keys: u64,
        ops: u64,
    ) -> ListRun {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        assert_eq!(leader, 0, "the run's writers name the leader as owner 0");
        driven
            .call(&mut pump, &clients[leader], move |db| {
                db.exec("CREATE TABLE lists(k INTEGER PRIMARY KEY, v TEXT NOT NULL, pad BLOB)")?;
                for k in 1..=keys {
                    db.exec(&format!(
                        "INSERT INTO lists VALUES ({k}, '', zeroblob(1800))"
                    ))?;
                }
                Ok(Vec::new())
            })
            .unwrap();
        driven.tick(&mut pump, leader);
        driven.owner_mut(leader).slot_off = slot_off;
        driven.owner_mut(leader).read_sets_off = read_sets == ReadSets::Off;
        driven.owner_mut(leader).read_sets_unchecked = read_sets == ReadSets::Unchecked;
        pump.delay = Some(Delay::new(seed, 3));
        let mut state = seed;
        let mut history: Vec<Append> = Vec::new();
        let mut inflight: Vec<Option<(usize, Answering)>> = writers.iter().map(|_| None).collect();
        let mut streaks = vec![0u64; writers.len()];
        let mut run = ListRun {
            violations: Vec::new(),
            committed: vec![0; writers.len()],
            refused: vec![0; writers.len()],
            streak: vec![0; writers.len()],
            unknown: 0,
            turns: (0, 0),
            moved: 0,
        };
        let mut rounds = 0;
        while run.committed.iter().any(|done| *done < ops) {
            assert!(rounds < ROUNDS, "seed {seed}: hung");
            for (w, &owner) in writers.iter().enumerate() {
                if inflight[w].is_none() && run.committed[w] < ops {
                    let key = 1 + mix(&mut state) % keys;
                    let other = 1 + mix(&mut state) % keys;
                    let element = format!("w{w}n{}", history.len());
                    let appended = element.clone();
                    let answer = submit(&clients[owner], driven.until(), move |db| {
                        let (a, b) = (list(db, key)?, list(db, other)?);
                        db.exec(&format!(
                            "UPDATE lists SET v = v || ' {appended}' WHERE k = {key}"
                        ))?;
                        Ok(format!("{}|{}", a.join(" "), b.join(" ")).into_bytes())
                    });
                    history.push(Append {
                        element,
                        key,
                        other,
                        read: None,
                        outcome: Outcome::Pending,
                    });
                    inflight[w] = Some((history.len() - 1, answer));
                }
                let Some((index, answer)) = inflight[w].take() else {
                    continue;
                };
                match answer.try_recv() {
                    Err(mpsc::TryRecvError::Empty) => inflight[w] = Some((index, answer)),
                    Err(error) => panic!("owner ended: {error}"),
                    Ok(Ok(answer)) => {
                        let text = String::from_utf8(called(answer)).unwrap();
                        let (a, b) = text.split_once('|').unwrap();
                        let words = |s: &str| s.split_whitespace().map(str::to_string).collect();
                        history[index].read = Some((words(a), words(b)));
                        history[index].outcome = Outcome::Committed;
                        run.committed[w] += 1;
                        streaks[w] = 0;
                    }
                    Ok(Err(Refusal::Invalid { what, .. })) if what == "prepared transaction" => {
                        history[index].outcome = Outcome::Refused;
                        run.refused[w] += 1;
                        streaks[w] += 1;
                        run.streak[w] = run.streak[w].max(streaks[w]);
                    }
                    Ok(Err(_)) => {
                        history[index].outcome = Outcome::Unknown;
                        run.unknown += 1;
                    }
                }
            }
            driven.round(&mut pump);
            rounds += 1;
        }
        pump.undelay();
        driven.quiet(&mut pump);
        driven.tick(&mut pump, leader);
        driven.tick(&mut pump, leader);
        let finals: Vec<Vec<Vec<String>>> = (0..3)
            .map(|owner| {
                driven
                    .owner_mut(owner)
                    .replica
                    .machine_mut()
                    .read_db(|db| (1..=keys).map(|k| list(db, k)).collect())
                    .unwrap()
            })
            .collect();
        run.violations = list_violations(&history, &finals);
        run.turns = driven.owner(leader).turns;
        run.moved = driven.owner(leader).read_sets_moved;
        driven.drain(&mut pump);
        pump.finish();
        run
    }
    /// The checks: every replica holds the same lists; each committed write's element appears
    /// once, in its key's list, and a refused one's never; each committed write read its key's
    /// list exactly as it stood before its element (the serial replay per key), and its other
    /// key's as some prefix; and the dependency graph Elle infers (ww from each list's order, wr
    /// from what was read, rw to the next element a read missed) has no cycle (Adya's G0, G1c and
    /// G2).
    fn list_violations(history: &[Append], finals: &[Vec<Vec<String>>]) -> Vec<String> {
        let mut out = Vec::new();
        for (owner, lists) in finals.iter().enumerate().skip(1) {
            if *lists != finals[0] {
                out.push(format!("replica {owner} differs from replica 0"));
            }
        }
        let lists = &finals[0];
        let by_element: BTreeMap<&str, usize> = history
            .iter()
            .enumerate()
            .map(|(i, op)| (op.element.as_str(), i))
            .collect();
        let at = |key: u64| &lists[usize::try_from(key - 1).unwrap()];
        for (k, list) in lists.iter().enumerate() {
            let mut seen = BTreeSet::new();
            for element in list {
                if !seen.insert(element) {
                    out.push(format!("{element} appears twice in list {}", k + 1));
                }
                match by_element.get(element.as_str()).map(|i| &history[*i]) {
                    None => out.push(format!("{element} in list {} was never written", k + 1)),
                    Some(op) if op.key != u64::try_from(k + 1).unwrap() => {
                        out.push(format!("{element} is in list {}, not its own", k + 1))
                    }
                    Some(op) if op.outcome == Outcome::Refused => {
                        out.push(format!("{element}, refused, is in list {}", k + 1))
                    }
                    Some(_) => {}
                }
            }
        }
        for op in history.iter().filter(|op| op.outcome == Outcome::Committed) {
            let list = at(op.key);
            let Some(position) = list.iter().position(|e| *e == op.element) else {
                out.push(format!(
                    "{} committed but lost from list {}",
                    op.element, op.key
                ));
                continue;
            };
            let (a, b) = op.read.as_ref().unwrap();
            if a.as_slice() != &list[..position] {
                out.push(format!(
                    "{} read list {} as {a:?}, not as it stood before it, {:?}",
                    op.element,
                    op.key,
                    &list[..position]
                ));
            }
            if !at(op.other).starts_with(b) {
                out.push(format!(
                    "{} read list {} as {b:?}, no prefix of it",
                    op.element, op.other
                ));
            }
        }
        // Elle's dependency graph over committed writes; Kahn's algorithm finds a cycle.
        let committed: Vec<usize> = (0..history.len())
            .filter(|i| history[*i].outcome == Outcome::Committed)
            .collect();
        let node: BTreeMap<usize, usize> =
            committed.iter().enumerate().map(|(n, i)| (*i, n)).collect();
        let writer = |element: &String| {
            by_element
                .get(element.as_str())
                .and_then(|i| node.get(i).copied())
        };
        let mut edges: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); committed.len()];
        for list in lists {
            for pair in list.windows(2) {
                if let (Some(from), Some(to)) = (writer(&pair[0]), writer(&pair[1])) {
                    edges[from].insert(to);
                }
            }
        }
        for (n, i) in committed.iter().enumerate() {
            let op = &history[*i];
            let (a, b) = op.read.as_ref().unwrap();
            for (key, read) in [(op.key, a), (op.other, b)] {
                let list = at(key);
                if let Some(from) = read.last().and_then(writer)
                    && from != n
                {
                    edges[from].insert(n);
                }
                if let Some(to) = list.get(read.len()).and_then(writer)
                    && to != n
                {
                    edges[n].insert(to);
                }
            }
        }
        let mut incoming = vec![0usize; committed.len()];
        for targets in &edges {
            for to in targets {
                incoming[*to] += 1;
            }
        }
        let mut ready: Vec<usize> = (0..committed.len()).filter(|n| incoming[*n] == 0).collect();
        let mut sorted = 0;
        while let Some(n) = ready.pop() {
            sorted += 1;
            for to in &edges[n] {
                incoming[*to] -= 1;
                if incoming[*to] == 0 {
                    ready.push(*to);
                }
            }
        }
        if sorted != committed.len() {
            out.push(format!(
                "a dependency cycle among {} committed writes",
                committed.len() - sorted
            ));
        }
        out
    }

    /// T-T1.8.W1 (AC-T1.8.W1; replication.md, "A follower's write under steady leader writes"):
    /// a refused write lands on its next try under the slot. Do: the leader and one follower
    /// each append 15 times to two hot lists, then to 32 spread over pages, every record delayed
    /// up to three relay rounds, three seeds each. Expect: no follower write refused twice in a
    /// row, every write committed, none unknown, and the history serializable (replicas equal,
    /// each list its serial replay, no G0, G1c or G2 cycle). Its control is
    /// `todays_rule_refuses_a_follower_twice_in_a_row_where_the_slot_lands_its_retry`: this
    /// run's interleaving follows the log's own I/O threads, so whether today's rule starves
    /// the follower here is the host's timing, never the seed's (the Linux gate at load 25 to 50
    /// found it didn't).
    #[test]
    fn a_refused_follower_write_lands_on_its_next_try_under_the_slot() {
        for keys in [2, 32] {
            for seed in 1..=3 {
                let run = list_append_run(&[0, 1], false, seed, keys, 15);
                eprintln!(
                    "slot keys {keys} seed {seed}: committed {:?} refused {:?} in a row {:?}",
                    run.committed, run.refused, run.streak
                );
                assert!(
                    run.violations.is_empty(),
                    "keys {keys} seed {seed}: {:?}",
                    run.violations
                );
                assert_eq!(run.unknown, 0, "keys {keys} seed {seed}");
                assert!(
                    run.streak[1] <= 1,
                    "keys {keys} seed {seed}: {:?}",
                    run.streak
                );
            }
        }
    }

    /// T-T1.8.W1's control (non-vacuity), an interleaving the test makes itself. Do, with the slot
    /// off and then on: the follower's write forwarded and held in transit while the leader
    /// appends a write of its own on the same base, then let through; the follower's write made
    /// again, its forward held the same way while the leader is asked for a second write, then let
    /// through. Expect, with the slot off: the follower refused both times, stale. With the slot
    /// on: refused once, then applied on its next try, the leader's second write held until it
    /// lands and then applied too.
    #[test]
    fn todays_rule_refuses_a_follower_twice_in_a_row_where_the_slot_lands_its_retry() {
        for slot_off in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let mut driven = Driven::new();
            let clients: Vec<Client> = (1..=3)
                .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
                .collect();
            let mut pump = Pump::new(&clients);
            let leader = driven.elect(&mut pump, &clients, 0, 1);
            assert_eq!(leader, 0);
            driven
                .call(&mut pump, &clients[0], |db| {
                    db.exec("CREATE TABLE t(k INTEGER PRIMARY KEY, v TEXT)")?;
                    Ok(Vec::new())
                })
                .unwrap();
            driven.tick(&mut pump, leader);
            driven.owner_mut(leader).slot_off = slot_off;
            let last = |driven: &Driven| {
                driven
                    .owner(0)
                    .replica
                    .core()
                    .raft
                    .log()
                    .last_index()
                    .unwrap()
            };
            let mut outcomes = Vec::new();
            for attempt in 0..2u64 {
                pump.begin_captures(leader, 1);
                let follower = submit(&clients[1], driven.until(), move |db| {
                    db.exec(&format!(
                        "INSERT INTO t VALUES ({}, 'follower')",
                        10 + attempt
                    ))?;
                    Ok(Vec::new())
                });
                for _ in 0..ROUNDS {
                    if pump.captures_ready() {
                        break;
                    }
                    driven.round(&mut pump);
                }
                assert!(
                    pump.captures_ready(),
                    "slot off {slot_off} attempt {attempt}"
                );
                let before = last(&driven);
                let own = submit(&clients[0], driven.until(), move |db| {
                    db.exec(&format!(
                        "INSERT INTO t VALUES ({}, 'leader')",
                        20 + attempt
                    ))?;
                    Ok(Vec::new())
                });
                for _ in 0..ROUNDS {
                    if last(&driven) > before || driven.owner(0).slots.holds() {
                        break;
                    }
                    driven.round(&mut pump);
                }
                pump.release_captures();
                outcomes.push(driven.settle(&mut pump, &follower));
                assert_eq!(
                    driven.settle(&mut pump, &own),
                    Ok(Vec::new()),
                    "slot off {slot_off} attempt {attempt}: the leader's write"
                );
            }
            let stale = |outcome: &Result<Vec<u8>, Refusal>| matches!(outcome, Err(Refusal::Invalid { what, .. }) if what == "prepared transaction");
            if slot_off {
                assert!(outcomes.iter().all(stale), "today's rule: {outcomes:?}");
            } else {
                assert!(stale(&outcomes[0]), "the slot: {outcomes:?}");
                assert_eq!(outcomes[1], Ok(Vec::new()), "the slot: {outcomes:?}");
            }
            driven.quiet(&mut pump);
            driven.drain(&mut pump);
            pump.finish();
        }
    }

    /// T-T1.8.R4 (AC-T1.8.R1; goal ceea4e0a): a follower's write that read nothing written since
    /// its base lands on its first try. Do, with the write slot off so read sets act alone: the
    /// follower's write to table `u` forwarded and held in transit while the leader appends a
    /// write of its own to table `t` on the same base, then let through; then the same with both
    /// writing `t`. Expect, with read sets on: the first applied (its base moved, counted), the
    /// second refused, stale, as its page changed. With read sets off: both refused.
    #[test]
    fn r4_a_follower_write_on_pages_nothing_since_wrote_lands_on_its_first_try() {
        for off in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let mut driven = Driven::new();
            let clients: Vec<Client> = (1..=3)
                .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
                .collect();
            let mut pump = Pump::new(&clients);
            let leader = driven.elect(&mut pump, &clients, 0, 1);
            assert_eq!(leader, 0);
            driven
                .call(&mut pump, &clients[0], |db| {
                    db.exec("CREATE TABLE t(k INTEGER PRIMARY KEY, v TEXT)")?;
                    db.exec("CREATE TABLE u(k INTEGER PRIMARY KEY, v TEXT)")?;
                    Ok(Vec::new())
                })
                .unwrap();
            driven.tick(&mut pump, leader);
            driven.owner_mut(leader).slot_off = true;
            driven.owner_mut(leader).read_sets_off = off;
            let last = |driven: &Driven| {
                driven
                    .owner(0)
                    .replica
                    .core()
                    .raft
                    .log()
                    .last_index()
                    .unwrap()
            };
            let mut outcomes = Vec::new();
            for (attempt, table) in [(0u64, "u"), (1, "t")] {
                pump.begin_captures(leader, 1);
                let follower = submit(&clients[1], driven.until(), move |db| {
                    db.exec(&format!(
                        "INSERT INTO {table} VALUES ({}, 'follower')",
                        10 + attempt
                    ))?;
                    Ok(Vec::new())
                });
                for _ in 0..ROUNDS {
                    if pump.captures_ready() {
                        break;
                    }
                    driven.round(&mut pump);
                }
                assert!(pump.captures_ready(), "off {off} attempt {attempt}");
                let before = last(&driven);
                let own = submit(&clients[0], driven.until(), move |db| {
                    db.exec(&format!(
                        "INSERT INTO t VALUES ({}, 'leader')",
                        20 + attempt
                    ))?;
                    Ok(Vec::new())
                });
                for _ in 0..ROUNDS {
                    if last(&driven) > before {
                        break;
                    }
                    driven.round(&mut pump);
                }
                assert!(last(&driven) > before, "off {off} attempt {attempt}");
                pump.release_captures();
                outcomes.push(driven.settle(&mut pump, &follower));
                assert_eq!(
                    driven.settle(&mut pump, &own),
                    Ok(Vec::new()),
                    "off {off} attempt {attempt}: the leader's write"
                );
            }
            let stale = |outcome: &Result<Vec<u8>, Refusal>| matches!(outcome, Err(Refusal::Invalid { what, .. }) if what == "prepared transaction");
            let moved = driven.owner(leader).read_sets_moved;
            if off {
                assert!(outcomes.iter().all(stale), "read sets off: {outcomes:?}");
                assert_eq!(moved, 0);
            } else {
                assert_eq!(outcomes[0], Ok(Vec::new()), "read sets on: {outcomes:?}");
                assert!(stale(&outcomes[1]), "read sets on: {outcomes:?}");
                assert_eq!(moved, 1, "the first write's base moved, the second's not");
                let rows = |driven: &mut Driven, owner: usize| {
                    driven
                        .owner_mut(owner)
                        .replica
                        .machine_mut()
                        .read_db(|db| {
                            let mut out = Vec::new();
                            db.query_owned(
                                "SELECT k FROM u UNION ALL SELECT k FROM t ORDER BY 1",
                                [],
                                |row| {
                                    out.push(row.get::<i64>(0)?);
                                    Ok(true)
                                },
                            )?;
                            Ok(out)
                        })
                        .unwrap()
                };
                driven.quiet(&mut pump);
                for owner in 0..3 {
                    assert_eq!(rows(&mut driven, owner), vec![10, 20, 21], "member {owner}");
                }
            }
            driven.quiet(&mut pump);
            driven.drain(&mut pump);
            pump.finish();
        }
    }

    /// T-T1.8.R5 (AC-T1.8.R2; Elle's list-append): read sets keep every history serializable,
    /// alone and under the write slot. Do: the leader and two followers each append 15 times to
    /// 32 lists spread over pages, then to two hot lists, every record delayed up to three relay
    /// rounds, three seeds each, with the slot off and on. Expect: replicas equal, each list its
    /// serial replay, no G0, G1c or G2 cycle, none unknown; some base moved (non-vacuity). The
    /// mutant that moves every stale base unchecked is caught: some run of it breaks the history.
    #[test]
    fn r5_read_sets_keep_every_list_append_history_serializable() {
        let mut moved = 0;
        for slot_off in [true, false] {
            for keys in [32, 2] {
                for seed in 1..=3 {
                    let run =
                        list_append_run_with(&[0, 1, 2], slot_off, ReadSets::On, seed, keys, 15);
                    eprintln!(
                        "read sets, slot off {slot_off} keys {keys} seed {seed}: committed {:?} refused {:?} in a row {:?} moved {}",
                        run.committed, run.refused, run.streak, run.moved
                    );
                    assert!(
                        run.violations.is_empty(),
                        "slot off {slot_off} keys {keys} seed {seed}: {:?}",
                        run.violations
                    );
                    assert_eq!(
                        run.unknown, 0,
                        "slot off {slot_off} keys {keys} seed {seed}"
                    );
                    moved += run.moved;
                }
            }
        }
        assert!(
            moved > 0,
            "no base moved: the runs never exercised read sets"
        );
        let caught = (1..=3).any(|seed| {
            let run = list_append_run_with(&[0, 1, 2], true, ReadSets::Unchecked, seed, 2, 15);
            eprintln!(
                "unchecked seed {seed}: moved {} violations {:?}",
                run.moved, run.violations
            );
            !run.violations.is_empty()
        });
        assert!(caught, "the isolation checks missed the unchecked mutant");
    }

    /// T-T1.8.W2 (AC-T1.8.W1): two followers and a leader all writing steadily, none missing.
    /// Do: the leader and both followers each append 15 times to two hot lists, then to 32,
    /// every record delayed, three seeds each. Expect: every write committed within its
    /// deadline (none unknown), no follower write refused more than the members ahead of it in
    /// the slot's queue in a row (two), no write the leader made in its turn after a slot refused
    /// (its turn between two slots is a turn in fact; some such turn taken, non-vacuity), and
    /// the history serializable.
    #[test]
    fn two_followers_and_a_leader_writing_steadily_all_commit_within_their_deadlines() {
        let mut turns = 0;
        for keys in [2, 32] {
            for seed in 1..=3 {
                let run = list_append_run(&[0, 1, 2], false, seed, keys, 15);
                eprintln!(
                    "keys {keys} seed {seed}: committed {:?} refused {:?} in a row {:?} turns {:?}",
                    run.committed, run.refused, run.streak, run.turns
                );
                assert!(
                    run.violations.is_empty(),
                    "keys {keys} seed {seed}: {:?}",
                    run.violations
                );
                assert_eq!(run.unknown, 0, "keys {keys} seed {seed}");
                assert!(
                    run.streak[1..].iter().all(|s| *s <= 2),
                    "keys {keys} seed {seed}: {:?}",
                    run.streak
                );
                assert_eq!(
                    run.turns.1, 0,
                    "keys {keys} seed {seed}: the leader's turns {:?}",
                    run.turns
                );
                turns += run.turns.0;
            }
        }
        assert!(turns > 0, "the leader never took a turn after a slot");
    }

    /// T-T1.8.W3 (AC-T1.8.W1; amphora-seam-owner-2's ruling, Q2 b): a follower's retry racing its
    /// slot's start lands on its next try. Do: the leader appends a write of its own, W, with one
    /// follower's answers held and its appends to the other, the slot's holder, lost, so W is
    /// neither committed nor sent to the holder; then the holder, queued by a refusal, writes,
    /// its barrier beginning its slot with W the leader's last entry. Expect: its barrier held
    /// while W is uncommitted, and its write, prepared on W once answers and appends flow again,
    /// applied; W too. With the tail rule alone (the barrier answered at the older commit) the
    /// holder's write was refused: the commit's red run.
    #[test]
    fn a_follower_retry_racing_the_slots_start_lands_on_its_next_try() {
        let dir = tempfile::tempdir().unwrap();
        let mut driven = Driven::new();
        let clients: Vec<Client> = (1..=3)
            .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
            .collect();
        let mut pump = Pump::new(&clients);
        let leader = driven.elect(&mut pump, &clients, 0, 1);
        assert_eq!(leader, 0);
        driven
            .call(&mut pump, &clients[0], |db| {
                db.exec("CREATE TABLE t(k INTEGER PRIMARY KEY, v TEXT)")?;
                Ok(Vec::new())
            })
            .unwrap();
        driven.tick(&mut pump, leader);
        let holder = 3;
        pump.held = vec![1];
        pump.losing = Some(Box::new(move |from, message| {
            from == 1
                && message.to == holder
                && message.msg_type == hyper_raft::proto::MessageType::MsgAppend
        }));
        let last = |driven: &Driven| {
            driven
                .owner(0)
                .replica
                .core()
                .raft
                .log()
                .last_index()
                .unwrap()
        };
        let before = last(&driven);
        let own = submit(&clients[0], driven.until(), |db| {
            db.exec("INSERT INTO t VALUES (1, 'leader')")?;
            Ok(Vec::new())
        });
        for _ in 0..ROUNDS {
            if last(&driven) > before {
                break;
            }
            driven.round(&mut pump);
        }
        let w = last(&driven);
        assert!(w > before, "the leader's write was never appended");
        assert!(driven.owner(0).replica.core().raft.log().committed() < w);
        // The holder's earlier write refused: it is queued, and its next barrier begins its slot.
        driven.owner_mut(0).slots.refused(holder);
        let retry = submit(&clients[2], driven.until(), |db| {
            db.exec("INSERT INTO t VALUES (2, 'holder')")?;
            Ok(Vec::new())
        });
        let mut held = false;
        for _ in 0..ROUNDS {
            held = driven.owner(0).slots.holds_read();
            if held || last(&driven) > w {
                break;
            }
            driven.round(&mut pump);
        }
        pump.held.clear();
        pump.losing = None;
        assert_eq!(
            driven.settle(&mut pump, &retry),
            Ok(Vec::new()),
            "the holder's write"
        );
        assert_eq!(
            driven.settle(&mut pump, &own),
            Ok(Vec::new()),
            "the leader's write"
        );
        assert!(held, "the holder's barrier was never held");
        driven.quiet(&mut pump);
        driven.drain(&mut pump);
        pump.finish();
    }
}
