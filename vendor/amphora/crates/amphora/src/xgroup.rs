//! Transactions across groups (CP-T1.10: AC-T1.10.2, AC-T1.10.5, T-T1.10.1; docs/design/xgroup.md).
//!
//! A transaction reads and writes keys in several groups and commits in all of them or none. Two
//! protocols run it, behind [`Protocol`], as G13 step 3 asks:
//!
//! - **Two-phase commit over groups** (Spanner, OSDI 2012, section 4.2.1): every participant group
//!   locks its keys and holds the writes (a prepare); then the anchor group records the decision;
//!   the client hears after both, two group commits one after the other.
//! - **Parallel Commits** (CockroachDB, SIGMOD 2020, section 3.2): the anchor group writes a
//!   *staging* record naming the participants in the same group commit as its own locks, beside
//!   the other groups' locks; the transaction is committed once every lock is in place, so the
//!   client hears after one group commit; the decision is made explicit afterwards.
//!
//! Each step is one atomic, durable commit in one group: [`apply`] is a group's whole part, pure
//! over a [`Tablet`] (the group's state), the same on [`Memory`] (the simulator's) and on a group's
//! own database ([`sql`]). [`Coordinator`] is the client's part, sans-IO: it says which steps to
//! send and takes their replies. Nothing here keeps time: a step whose outcome the shell can't
//! learn comes back [`Lost`], and the coordinator acts as if it may still land.
//!
//! What makes a crash safe: every decision lives in a group. A coordinator's incarnation is an
//! epoch the registry group hands out ([`Step::Begin`]); a new incarnation fences the old one in
//! every group ([`Step::Scan`]) and finishes what it left ([`Step::Recover`], [`Step::Prevent`]).
//! A group refuses a late step of a transaction it has finished (its done row) and of one below
//! the coordinator's floor (the oldest transaction it still runs), which bounds the done rows.

use std::collections::{BTreeMap, BTreeSet};

use amphora_api as api;

pub mod coordinator;
pub mod sql;

pub use coordinator::{Answer, Config, Coordinator, Counts, Event, Lost, Protocol, Send, Txn, Why};
pub use sql::{Cluster, Route};

/// A group's number among the fixed groups a cluster has.
pub type GroupId = u32;

/// One page of [`Tablet::open`]: each transaction with its anchor (`None`: this group is the anchor),
/// and whether there are more.
pub type OpenPage = (Vec<(TxnId, Option<GroupId>)>, bool);

/// A place in one coordinator's order of transactions: its incarnation, then its count there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Mark {
    pub epoch: u64,
    pub seq: u64,
}

/// A transaction: its coordinator, the incarnation that began it, and its count there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TxnId {
    pub coordinator: u64,
    pub epoch: u64,
    pub seq: u64,
}

impl TxnId {
    /// Its place in its coordinator's order.
    #[must_use]
    pub fn mark(self) -> Mark {
        Mark {
            epoch: self.epoch,
            seq: self.seq,
        }
    }
}

/// What became of a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Committed,
    Aborted,
}

/// A transaction's record in its anchor group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    /// Parallel Commits' staging status: committed once every participant named holds its locks.
    Staging(Vec<GroupId>),
    Decided(Outcome),
}

/// What a lock holds: a read (shared with other reads) or a write waiting for the decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    Read,
    Put(Vec<u8>),
    Delete,
}

/// A lock on a key, held by a transaction until its group resolves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lock {
    pub txn: TxnId,
    /// The group whose record decides the transaction.
    pub anchor: GroupId,
    pub intent: Intent,
}

/// What a group knows of a coordinator: the newest incarnation it has heard from (older ones are
/// fenced) and the floor below which every transaction of it is finished everywhere.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Guard {
    pub fence: u64,
    pub floor: Option<Mark>,
}

/// One step of a transaction, applied atomically in one group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// In the registry group: the coordinator's next incarnation.
    Begin,
    /// A new incarnation's look at a group: this coordinator's transactions of incarnations below
    /// `below` that hold locks or a staging record here, after `after`, a page at a time. The
    /// envelope's epoch fences the older incarnations here first.
    Scan { below: u64, after: Option<TxnId> },
    /// Lock `reads` and `writes`, holding the writes; with `stage`, also write the staging record
    /// naming the participants (Parallel Commits' anchor).
    Lock {
        txn: TxnId,
        anchor: GroupId,
        reads: Vec<Vec<u8>>,
        writes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
        stage: Option<Vec<GroupId>>,
    },
    /// At the anchor: decide `outcome` unless it is decided already; answers what stands.
    Decide { txn: TxnId, outcome: Outcome },
    /// At the anchor, in recovery: what the record says; a transaction with none is aborted.
    Recover { txn: TxnId },
    /// At a participant, in recovery: does it hold the transaction's locks? If not, it never will.
    Prevent { txn: TxnId },
    /// At a participant: apply the held writes (committed) or drop them (aborted), and unlock.
    Resolve { txn: TxnId, outcome: Outcome },
}

impl Step {
    /// The transaction it is about, if one.
    #[must_use]
    pub fn txn(&self) -> Option<TxnId> {
        match self {
            Step::Begin | Step::Scan { .. } => None,
            Step::Lock { txn, .. }
            | Step::Decide { txn, .. }
            | Step::Recover { txn }
            | Step::Prevent { txn }
            | Step::Resolve { txn, .. } => Some(*txn),
        }
    }
}

/// A step as a coordinator's incarnation sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub coordinator: u64,
    /// The sending incarnation (0 before [`Step::Begin`] answers).
    pub epoch: u64,
    /// The sender's floor, once its recovery is done: every transaction of it below is finished.
    pub floor: Option<Mark>,
    pub step: Step,
}

/// A group's answer to a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Epoch(u64),
    Open {
        txns: Vec<(TxnId, GroupId)>,
        more: bool,
    },
    /// The locks are held; the reads' committed values.
    Locked {
        reads: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    },
    /// Another transaction holds a lock on `key`: nothing was locked.
    Conflict {
        key: Vec<u8>,
    },
    /// The transaction is finished here, or below the coordinator's floor: nothing was done.
    Late,
    /// A newer incarnation of the coordinator has been here: nothing was done.
    Fenced,
    Decided(Outcome),
    Staging(Vec<GroupId>),
    Present(bool),
    Resolved,
}

/// A group's state as the protocol sees it. Every method acts inside the step's one atomic commit.
pub trait Tablet {
    /// A key's committed value.
    fn value(&mut self, key: &[u8]) -> anyhow::Result<Option<Vec<u8>>>;
    fn set_value(&mut self, key: &[u8], value: Option<&[u8]>) -> anyhow::Result<()>;
    /// The locks on a key: at most one per transaction a coordinator runs at once.
    fn locks(&mut self, key: &[u8]) -> anyhow::Result<Vec<Lock>>;
    /// A transaction's locks here, with their keys: at most its own keys.
    fn locks_of(&mut self, txn: TxnId) -> anyhow::Result<Vec<(Vec<u8>, Lock)>>;
    fn put_lock(&mut self, key: &[u8], lock: &Lock) -> anyhow::Result<()>;
    fn drop_locks_of(&mut self, txn: TxnId) -> anyhow::Result<()>;
    fn record(&mut self, txn: TxnId) -> anyhow::Result<Option<Record>>;
    fn set_record(&mut self, txn: TxnId, record: &Record) -> anyhow::Result<()>;
    /// How a transaction finished here, until the coordinator's floor passes it.
    fn done(&mut self, txn: TxnId) -> anyhow::Result<Option<Outcome>>;
    fn set_done(&mut self, txn: TxnId, outcome: Outcome) -> anyhow::Result<()>;
    fn guard(&mut self, coordinator: u64) -> anyhow::Result<Guard>;
    fn set_guard(&mut self, coordinator: u64, guard: Guard) -> anyhow::Result<()>;
    /// Forgets the coordinator's done rows and decided records below `floor`.
    fn forget_below(&mut self, coordinator: u64, floor: Mark) -> anyhow::Result<()>;
    /// The registry's count of the coordinator's incarnations, one up.
    fn next_epoch(&mut self, coordinator: u64) -> anyhow::Result<u64>;
    /// The coordinator's transactions of incarnations below `below`, after `after`, in order, that
    /// hold locks here (with their anchor) or a staging record here (`None`: this group is the
    /// anchor): at most `limit`, and whether there are more.
    fn open(
        &mut self,
        coordinator: u64,
        below: u64,
        after: Option<TxnId>,
        limit: usize,
    ) -> anyhow::Result<OpenPage>;
}

/// Group `here`'s part of a step: what it does to `tablet` and answers. An error leaves the step
/// undone (its commit rolls back) and is a fault to report, never a vote.
pub fn apply<T: Tablet + ?Sized>(
    here: GroupId,
    tablet: &mut T,
    envelope: &Envelope,
) -> anyhow::Result<Reply> {
    if envelope.step != Step::Begin
        && let Some(refused) = admit(tablet, envelope)?
    {
        return Ok(refused);
    }
    let coordinator = envelope.coordinator;
    match &envelope.step {
        Step::Begin => Ok(Reply::Epoch(tablet.next_epoch(coordinator)?)),
        Step::Scan { below, after } => {
            // The API's own page bound: one reply holds no more transactions than a scan's rows.
            let (txns, more) = tablet.open(coordinator, *below, *after, api::SCAN_MAX)?;
            let txns = txns
                .into_iter()
                .map(|(txn, anchor)| (txn, anchor.unwrap_or(here)))
                .collect();
            Ok(Reply::Open { txns, more })
        }
        Step::Lock {
            txn,
            anchor,
            reads,
            writes,
            stage,
        } => lock(tablet, *txn, *anchor, reads, writes, stage.as_deref()),
        Step::Decide { txn, outcome } => match tablet.record(*txn)? {
            Some(Record::Decided(stands)) => Ok(Reply::Decided(stands)),
            Some(Record::Staging(_)) | None => {
                tablet.set_record(*txn, &Record::Decided(*outcome))?;
                Ok(Reply::Decided(*outcome))
            }
        },
        Step::Recover { txn } => match tablet.record(*txn)? {
            Some(Record::Decided(stands)) => Ok(Reply::Decided(stands)),
            Some(Record::Staging(participants)) => Ok(Reply::Staging(participants)),
            None => {
                // No record: no staging can land now, and two-phase commit never decided it.
                tablet.set_record(*txn, &Record::Decided(Outcome::Aborted))?;
                Ok(Reply::Decided(Outcome::Aborted))
            }
        },
        Step::Prevent { txn } => match tablet.done(*txn)? {
            Some(outcome) => Ok(Reply::Present(outcome == Outcome::Committed)),
            None if !tablet.locks_of(*txn)?.is_empty() => Ok(Reply::Present(true)),
            None => {
                tablet.set_done(*txn, Outcome::Aborted)?;
                Ok(Reply::Present(false))
            }
        },
        Step::Resolve { txn, outcome } => resolve(tablet, *txn, *outcome),
    }
}

/// A step's admission in a group: refused if its incarnation is fenced or its transaction is
/// below the coordinator's floor; else the group's guard raised to what the envelope says.
fn admit<T: Tablet + ?Sized>(tablet: &mut T, envelope: &Envelope) -> anyhow::Result<Option<Reply>> {
    let coordinator = envelope.coordinator;
    let guard = tablet.guard(coordinator)?;
    if envelope.epoch < guard.fence {
        return Ok(Some(Reply::Fenced));
    }
    if let Some(txn) = envelope.step.txn() {
        if txn.coordinator != coordinator {
            anyhow::bail!("a step about transaction {txn:?} came from coordinator {coordinator}");
        }
        if guard.floor.is_some_and(|floor| txn.mark() < floor) {
            return Ok(Some(Reply::Late));
        }
        if envelope.floor.is_some_and(|floor| floor > txn.mark()) {
            anyhow::bail!(
                "a step's floor {:?} passes its own transaction {txn:?}",
                envelope.floor
            );
        }
    }
    let raised = Guard {
        fence: guard.fence.max(envelope.epoch),
        floor: guard.floor.max(envelope.floor),
    };
    if raised != guard {
        tablet.set_guard(coordinator, raised)?;
        if let Some(floor) = raised.floor
            && raised.floor != guard.floor
        {
            tablet.forget_below(coordinator, floor)?;
        }
    }
    Ok(None)
}

fn lock<T: Tablet + ?Sized>(
    tablet: &mut T,
    txn: TxnId,
    anchor: GroupId,
    reads: &[Vec<u8>],
    writes: &[(Vec<u8>, Option<Vec<u8>>)],
    stage: Option<&[GroupId]>,
) -> anyhow::Result<Reply> {
    if tablet.done(txn)?.is_some() || (stage.is_some() && tablet.record(txn)?.is_some()) {
        return Ok(Reply::Late);
    }
    let written: BTreeSet<&[u8]> = writes.iter().map(|(key, _)| key.as_slice()).collect();
    let only_read = reads.iter().filter(|key| !written.contains(key.as_slice()));
    let wanted = written
        .iter()
        .map(|key| (*key, true))
        .chain(only_read.clone().map(|key| (key.as_slice(), false)));
    for (key, writing) in wanted {
        let conflict = tablet
            .locks(key)?
            .iter()
            .any(|held| held.txn != txn && (writing || held.intent != Intent::Read));
        if conflict {
            return Ok(Reply::Conflict { key: key.to_vec() });
        }
    }
    let mut values = Vec::with_capacity(reads.len());
    for key in reads {
        values.push((key.clone(), tablet.value(key)?));
    }
    for (key, value) in writes {
        let intent = value.clone().map_or(Intent::Delete, Intent::Put);
        tablet.put_lock(
            key,
            &Lock {
                txn,
                anchor,
                intent,
            },
        )?;
    }
    for key in only_read {
        tablet.put_lock(
            key,
            &Lock {
                txn,
                anchor,
                intent: Intent::Read,
            },
        )?;
    }
    if let Some(participants) = stage {
        tablet.set_record(txn, &Record::Staging(participants.to_vec()))?;
    }
    Ok(Reply::Locked { reads: values })
}

fn resolve<T: Tablet + ?Sized>(
    tablet: &mut T,
    txn: TxnId,
    outcome: Outcome,
) -> anyhow::Result<Reply> {
    if let Some(done) = tablet.done(txn)? {
        if done != outcome {
            anyhow::bail!("transaction {txn:?} finished {done:?} here, not {outcome:?}");
        }
        return Ok(Reply::Resolved);
    }
    if outcome == Outcome::Committed {
        for (key, lock) in tablet.locks_of(txn)? {
            match lock.intent {
                Intent::Read => {}
                Intent::Put(value) => tablet.set_value(&key, Some(&value))?,
                Intent::Delete => tablet.set_value(&key, None)?,
            }
        }
    }
    tablet.drop_locks_of(txn)?;
    tablet.set_done(txn, outcome)?;
    Ok(Reply::Resolved)
}

/// The simulator's group: a [`Tablet`] in memory (CLAUDE.md §9's simulator implementation; the
/// conformance tests run the same steps on it and on a group's database).
#[derive(Debug, Clone, Default)]
pub struct Memory {
    data: BTreeMap<Vec<u8>, Vec<u8>>,
    locks: BTreeMap<(Vec<u8>, TxnId), Lock>,
    records: BTreeMap<TxnId, Record>,
    done: BTreeMap<TxnId, Outcome>,
    guards: BTreeMap<u64, Guard>,
    epochs: BTreeMap<u64, u64>,
}

impl Memory {
    /// The committed values.
    #[must_use]
    pub fn data(&self) -> &BTreeMap<Vec<u8>, Vec<u8>> {
        &self.data
    }

    /// Every lock held, by key and transaction.
    #[must_use]
    pub fn held(&self) -> &BTreeMap<(Vec<u8>, TxnId), Lock> {
        &self.locks
    }

    /// Every record kept.
    #[must_use]
    pub fn records(&self) -> &BTreeMap<TxnId, Record> {
        &self.records
    }

    /// How many done rows are kept.
    #[must_use]
    pub fn done_rows(&self) -> usize {
        self.done.len()
    }
}

fn below(txn: &TxnId, coordinator: u64, floor: Mark) -> bool {
    txn.coordinator == coordinator && txn.mark() < floor
}

impl Tablet for Memory {
    fn value(&mut self, key: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(self.data.get(key).cloned())
    }

    fn set_value(&mut self, key: &[u8], value: Option<&[u8]>) -> anyhow::Result<()> {
        match value {
            Some(value) => self.data.insert(key.to_vec(), value.to_vec()),
            None => self.data.remove(key),
        };
        Ok(())
    }

    fn locks(&mut self, key: &[u8]) -> anyhow::Result<Vec<Lock>> {
        Ok(self
            .locks
            .iter()
            .filter(|((held, _), _)| held.as_slice() == key)
            .map(|(_, lock)| lock.clone())
            .collect())
    }

    fn locks_of(&mut self, txn: TxnId) -> anyhow::Result<Vec<(Vec<u8>, Lock)>> {
        Ok(self
            .locks
            .iter()
            .filter(|((_, of), _)| *of == txn)
            .map(|((key, _), lock)| (key.clone(), lock.clone()))
            .collect())
    }

    fn put_lock(&mut self, key: &[u8], lock: &Lock) -> anyhow::Result<()> {
        if self
            .locks
            .insert((key.to_vec(), lock.txn), lock.clone())
            .is_some()
        {
            anyhow::bail!("transaction {:?} locked a key twice", lock.txn);
        }
        Ok(())
    }

    fn drop_locks_of(&mut self, txn: TxnId) -> anyhow::Result<()> {
        self.locks.retain(|(_, of), _| *of != txn);
        Ok(())
    }

    fn record(&mut self, txn: TxnId) -> anyhow::Result<Option<Record>> {
        Ok(self.records.get(&txn).cloned())
    }

    fn set_record(&mut self, txn: TxnId, record: &Record) -> anyhow::Result<()> {
        self.records.insert(txn, record.clone());
        Ok(())
    }

    fn done(&mut self, txn: TxnId) -> anyhow::Result<Option<Outcome>> {
        Ok(self.done.get(&txn).copied())
    }

    fn set_done(&mut self, txn: TxnId, outcome: Outcome) -> anyhow::Result<()> {
        self.done.insert(txn, outcome);
        Ok(())
    }

    fn guard(&mut self, coordinator: u64) -> anyhow::Result<Guard> {
        Ok(self.guards.get(&coordinator).copied().unwrap_or_default())
    }

    fn set_guard(&mut self, coordinator: u64, guard: Guard) -> anyhow::Result<()> {
        self.guards.insert(coordinator, guard);
        Ok(())
    }

    fn forget_below(&mut self, coordinator: u64, floor: Mark) -> anyhow::Result<()> {
        self.done.retain(|txn, _| !below(txn, coordinator, floor));
        self.records.retain(|txn, record| {
            !(below(txn, coordinator, floor) && matches!(record, Record::Decided(_)))
        });
        Ok(())
    }

    fn next_epoch(&mut self, coordinator: u64) -> anyhow::Result<u64> {
        let epoch = self.epochs.entry(coordinator).or_default();
        *epoch = epoch
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("coordinator {coordinator} ran out of incarnations"))?;
        Ok(*epoch)
    }

    fn open(
        &mut self,
        coordinator: u64,
        below: u64,
        after: Option<TxnId>,
        limit: usize,
    ) -> anyhow::Result<OpenPage> {
        let wanted = |txn: &TxnId| {
            txn.coordinator == coordinator && txn.epoch < below && after.is_none_or(|a| *txn > a)
        };
        let mut found: BTreeMap<TxnId, Option<GroupId>> = BTreeMap::new();
        for lock in self.locks.values().filter(|lock| wanted(&lock.txn)) {
            found.insert(lock.txn, Some(lock.anchor));
        }
        for (txn, record) in &self.records {
            if wanted(txn) && matches!(record, Record::Staging(_)) {
                found.entry(*txn).or_insert(None);
            }
        }
        let more = found.len() > limit;
        Ok((found.into_iter().take(limit).collect(), more))
    }
}

#[cfg(test)]
mod tests;
