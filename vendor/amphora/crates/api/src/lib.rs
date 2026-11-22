//! Product-owned commands and records (CP-T1.5; kiln unification U05/U06).
//! Methods block until their deadline. Async hosts call them on their blocking pool. A timeout
//! after submission leaves the outcome unknown: the transaction may subsequently commit.
//! The embedded implementation currently promises local disk durability, not a replicated quorum.
//!
//! Records: every record by family and key, each at the revision of the commit that last wrote
//! it ([`Records`]). Two implementations pass one conformance suite ([`conformance`], kiln-seams'
//! Records cases ported, plus Amphora's own): Amphora's `Namespace`, and [`sim::SimRecords`], which
//! keeps the same layout in memory. Index families ([`index`]) and counters ([`counter`]) are rows
//! of ordinary families, written in the commit that needs them.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

use serde::{Deserialize, Serialize};
use std::time::Instant;

pub mod blobs;
pub mod conformance;
pub mod counter;
pub mod import;
pub mod index;
pub mod replication;
pub mod restore;
pub mod sim;
pub mod sql;

pub type Rev = u64;
pub const FAMILY_MAX: usize = 64;
pub const KEY_MAX: usize = 1024;
pub const VALUE_MAX: usize = 1 << 20;
pub const TX_MAX: usize = 1000;
pub const SCAN_MAX: usize = 1000;
pub const SQL_MAX: usize = 1 << 20;
pub const RESULT_MAX: usize = 32 << 20;
/// Aggregate request bound in addition to per-value bounds: a batch cannot hold TX_MAX MiB values.
pub const REQUEST_MAX: usize = 32 << 20;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Statement {
    pub sql: String,
    pub params: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SqlResult {
    pub rows: Vec<Vec<Value>>,
    pub affected: u64,
    pub last_insert_rowid: i64,
}

/// Applied once by name. Reusing a name with different SQL is refused, never silently skipped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Migration {
    pub name: String,
    pub sql: String,
}

/// Owned input to the database and its future replicated state machine. External IDs and
/// timestamps are bound parameters supplied by the caller.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Mutation {
    Sql(Vec<Statement>),
    Migrate(Migration),
    Records { namespace: String, tx: Tx },
    Compact { namespace: String, before: Rev },
    CompactStep { namespace: String, before: Rev },
    // A group's holding (CP-T1.9, docs/design/reshard.md, "A group's holding"), encoded by the
    // group's shell, kept with its rows in place of the one before: one entry of its log, so a
    // replicated group keeps it through its log like any write. Appended after every other
    // variant (postcard by variant index): the wire only grows.
    Holding(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Consistency {
    Linearizable,
    Snapshot(Rev),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub rev: Rev,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cond {
    Absent {
        family: String,
        key: Vec<u8>,
    },
    At {
        family: String,
        key: Vec<u8>,
        rev: Rev,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Write {
    Put {
        family: String,
        key: Vec<u8>,
        value: Vec<u8>,
    },
    Delete {
        family: String,
        key: Vec<u8>,
    },
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tx {
    pub conds: Vec<Cond>,
    pub writes: Vec<Write>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scan {
    pub prefix: Vec<u8>,
    pub after: Option<Vec<u8>>,
    pub limit: usize,
    pub at: Consistency,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub rows: Vec<Row>,
    pub more: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub rev: Rev,
    pub key: Vec<u8>,
    pub value: Option<Vec<u8>>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changes {
    pub changes: Vec<Change>,
    pub head: Rev,
}

/// Durable progress of one bounded physical compaction transaction. Logical horizons can
/// advance before cleanup completes; a later timeout does not roll those horizons back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionProgress {
    pub head: Rev,
    pub horizon: Rev,
    pub feed: Rev,
    pub completed: Rev,
    pub target: Option<Rev>,
    pub visited: usize,
    pub deleted: usize,
    pub more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum Refusal {
    #[error("condition failed for {family}: row is at {at:?}")]
    Conflict {
        family: String,
        key: Vec<u8>,
        at: Option<Rev>,
    },
    #[error("{what} has size {size}, over {limit}")]
    TooLarge {
        what: String,
        size: usize,
        limit: usize,
    },
    #[error("invalid {what}: {why}")]
    Invalid { what: String, why: String },
    #[error("the store holds its most rows ({limit})")]
    Full { limit: usize },
    #[error("revision {asked} is older than horizon {horizon}")]
    Compacted { asked: Rev, horizon: Rev },
    #[error("revision {asked} is ahead of head {head}")]
    Ahead { asked: Rev, head: Rev },
    #[error("the deadline elapsed; a submitted transaction may still commit")]
    Timeout,
    #[error("the store is unavailable: {0}")]
    Unavailable(String),
    #[error("the commit outcome is unknown; reconcile it before retrying the operation")]
    UnknownOutcome,
    #[error("the replica is not leader; known leader: {leader:?}")]
    NotLeader { leader: Option<u64> },
    #[error("the replica is fenced: {0}")]
    Fenced(String),
}

/// One namespace's records.
///
/// The change feed (`changes`, `wait`, `changed`) is read from the version rows the commits
/// themselves wrote, as PostgreSQL's logical decoding reads the changes from the write-ahead log
/// (docs/design/records.md): it can't disagree with the rows, and nothing beside a commit writes
/// it. Its guarantees are etcd's for watches:
/// - ordered: a family's changes come by revision, a commit's in the order of its writes (a key
///   written twice in one commit once, at its last write's place);
/// - reliable: from any revision at or after the feed's horizon, no change is skipped;
/// - atomic: a page of changes never ends inside a commit, so the last revision of a page is a
///   cursor that loses nothing;
/// - bounded: from before the horizon (a compaction's revision less one), `changes`, `wait` and
///   `changed` are refused `Compacted`, naming the horizon, never answered from a partial history.
pub trait Records: Send + Sync {
    fn get(
        &self,
        family: &str,
        key: &[u8],
        at: Consistency,
        until: Instant,
    ) -> Result<Option<Row>, Refusal>;
    fn scan(&self, family: &str, scan: Scan, until: Instant) -> Result<Page, Refusal>;
    fn commit(&self, tx: Tx, until: Instant) -> Result<Rev, Refusal>;
    /// Changes to `family` committed after `after`, oldest first, at most `limit` (capped at
    /// `SCAN_MAX`), never ending inside a commit: a page holds whole commits only, and when the
    /// first commit after `after` alone has more changes than `limit`, it is refused `Invalid`.
    /// `head` is the newest commit of any family when they were read.
    fn changes(
        &self,
        family: &str,
        after: Rev,
        limit: usize,
        until: Instant,
    ) -> Result<Changes, Refusal>;
    /// The newest revision of a change to `family` once one is committed after `after` (at once
    /// when one already is), or `Timeout` at `until`.
    fn wait(&self, family: &str, after: Rev, until: Instant) -> Result<Rev, Refusal>;
    /// `wait`'s answer if it has one now, without waiting: the newest revision of a change to
    /// `family` after `after`, or `None` while there is none. A caller that registers for the next
    /// change itself (an async watch, a driver's event loop) checks this first.
    fn changed(&self, family: &str, after: Rev, until: Instant) -> Result<Option<Rev>, Refusal>;
    fn head(&self, until: Instant) -> Result<Rev, Refusal>;
    fn compact(&self, before: Rev, until: Instant) -> Result<(), Refusal>;
    /// One recoverable step through the existing writer. Unsupported owners must refuse before
    /// submitting a mutation; callers must not fall back to an unbounded compaction.
    fn compact_step(&self, _before: Rev, _until: Instant) -> Result<CompactionProgress, Refusal> {
        Err(invalid(
            "bounded compaction",
            "this implementation does not support it",
        ))
    }
}

/// Completes the first step's accepted cutoff under one absolute deadline. Concurrent requests
/// may raise the logical horizon, but do not make this call chase a moving target indefinitely.
pub fn compact(
    records: &(impl Records + ?Sized),
    before: Rev,
    until: Instant,
) -> Result<(), Refusal> {
    let mut progress = records.compact_step(before, until)?;
    let cutoff = progress.horizon;
    while progress.completed < cutoff {
        if Instant::now() >= until {
            return Err(Refusal::Timeout);
        }
        progress = records.compact_step(cutoff, until)?;
    }
    Ok(())
}

pub fn invalid(what: &str, why: &str) -> Refusal {
    Refusal::Invalid {
        what: what.into(),
        why: why.into(),
    }
}
pub fn bound(what: &str, size: usize, limit: usize) -> Result<(), Refusal> {
    if size > limit {
        Err(Refusal::TooLarge {
            what: what.into(),
            size,
            limit,
        })
    } else {
        Ok(())
    }
}
pub fn family(name: &str) -> Result<(), Refusal> {
    if name.is_empty()
        || name.len() > FAMILY_MAX
        || !name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.')
        })
    {
        Err(invalid(
            "family or namespace",
            "use 1 to 64 bytes of a-z, 0-9, _, -, .",
        ))
    } else {
        Ok(())
    }
}
pub fn key(value: &[u8]) -> Result<(), Refusal> {
    if value.is_empty() {
        return Err(invalid("key", "empty"));
    }
    bound("key", value.len(), KEY_MAX)
}
pub fn limit(n: usize) -> Result<usize, Refusal> {
    if n == 0 {
        return Err(invalid("limit", "zero"));
    }
    Ok(n.min(SCAN_MAX))
}
pub fn transaction(tx: &Tx) -> Result<(), Refusal> {
    bound("conditions", tx.conds.len(), TX_MAX)?;
    bound("writes", tx.writes.len(), TX_MAX)?;
    let mut bytes = 0usize;
    for c in &tx.conds {
        let (Cond::Absent { family: f, key: k }
        | Cond::At {
            family: f, key: k, ..
        }) = c;
        family(f)?;
        key(k)?;
        bytes = bytes
            .checked_add(f.len())
            .and_then(|n| n.checked_add(k.len()))
            .ok_or_else(|| invalid("transaction", "size overflow"))?;
    }
    for w in &tx.writes {
        let (Write::Put {
            family: f, key: k, ..
        }
        | Write::Delete { family: f, key: k }) = w;
        family(f)?;
        key(k)?;
        bytes = bytes
            .checked_add(f.len())
            .and_then(|n| n.checked_add(k.len()))
            .ok_or_else(|| invalid("transaction", "size overflow"))?;
        if let Write::Put { value, .. } = w {
            bound("value", value.len(), VALUE_MAX)?;
            bytes = bytes
                .checked_add(value.len())
                .ok_or_else(|| invalid("transaction", "size overflow"))?;
        }
    }
    bound("transaction bytes", bytes, REQUEST_MAX)?;
    Ok(())
}
