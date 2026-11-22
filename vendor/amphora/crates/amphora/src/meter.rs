//! What a store is doing, counted as it goes ([`Meter`]): cheap to keep, read at any time without
//! asking its writer ([`Counters`]).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// How long reads waited for their answer, counted by power of two of milliseconds: under 1 ms,
/// under 2, under 4, ... and the last bucket for the rest (past 16 s).
pub const WAIT_BUCKETS: usize = 16;
const LAST_BUCKET: u32 = 15;

/// The bucket a wait of `d` counts in.
#[must_use]
pub fn wait_bucket(d: Duration) -> usize {
    let ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    let bucket = u64::BITS
        .saturating_sub(ms.leading_zeros())
        .min(LAST_BUCKET);
    usize::try_from(bucket).unwrap_or(WAIT_BUCKETS.saturating_sub(1))
}

/// About the `q`th quantile (0 to 1) of the waits counted in `buckets`, in ms: the top of the
/// bucket it falls in (None: none counted).
#[must_use]
pub fn wait_quantile(buckets: &[u64], q: f64) -> Option<u64> {
    let total = buckets.iter().fold(0u64, |a, n| a.saturating_add(*n));
    if total == 0 {
        return None;
    }
    let want = (total as f64 * q).max(1.0);
    let top = |i: usize| {
        1u64.checked_shl(u32::try_from(i).unwrap_or(LAST_BUCKET))
            .unwrap_or(u64::MAX)
    };
    let mut seen = 0u64;
    for (i, n) in buckets.iter().enumerate() {
        seen = seen.saturating_add(*n);
        if seen as f64 >= want {
            return Some(top(i));
        }
    }
    Some(top(buckets.len().saturating_sub(1)))
}

/// What the store keeps the longest of ([`Counters::longest_us`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Longest {
    Commit,
    Flush,
    Checkpoint,
    RestartWait,
    Restart,
}

const LONGEST: usize = 5;

impl Longest {
    /// Their order in [`Counters::longest_us`].
    pub const ALL: [Self; LONGEST] = [
        Self::Commit,
        Self::Flush,
        Self::Checkpoint,
        Self::RestartWait,
        Self::Restart,
    ];

    fn index(self) -> usize {
        match self {
            Self::Commit => 0,
            Self::Flush => 1,
            Self::Checkpoint => 2,
            Self::RestartWait => 3,
            Self::Restart => 4,
        }
    }
}

/// A store's counters, bumped by its writer, its flusher and its readers as they go.
#[derive(Default)]
pub struct Meter {
    commits: AtomicU64,
    rows: AtomicU64,
    /// Jobs run on the writer.
    jobs: AtomicU64,
    /// How long the writer was held (its commits, its jobs, its upkeep), and of that its commits,
    /// in µs.
    busy_us: AtomicU64,
    apply_us: AtomicU64,
    /// Flushes of what was committed, and how long they took, in µs.
    flushes: AtomicU64,
    flush_us: AtomicU64,
    /// Batches refused because the writer's queue was full.
    dropped: AtomicU64,
    /// Batches whose commit failed, each refused to its sender.
    failed: AtomicU64,
    /// Passive checkpoints, and how long they took, in µs.
    checkpoints: AtomicU64,
    checkpoint_us: AtomicU64,
    /// The log started again by the writer, how long it waited for the readers and the
    /// checkpointer to let go, and how long the restart took once they had, in µs.
    restarts: AtomicU64,
    restart_wait_us: AtomicU64,
    restart_us: AtomicU64,
    /// The longest of each, in µs: a stall shows here first.
    longest: [AtomicU64; LONGEST],
    /// Reads that got no answer in time.
    timeouts: AtomicU64,
    waits: [AtomicU64; WAIT_BUCKETS],
}

/// What a store's counters say now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counters {
    pub commits: u64,
    pub rows: u64,
    pub jobs: u64,
    pub busy_ms: u64,
    pub apply_ms: u64,
    /// How long its commits held the writer, in µs: with `commits`, a commit's mean cost, what a
    /// batch of a split's, merge's or move's copy path holds a writer no longer than (CP-T1.9).
    pub apply_us: u64,
    pub flushes: u64,
    pub flush_ms: u64,
    pub dropped: u64,
    pub failed: u64,
    pub checkpoints: u64,
    pub checkpoint_ms: u64,
    pub restarts: u64,
    pub restart_wait_ms: u64,
    pub restart_ms: u64,
    /// The longest commit, flush, checkpoint, wait to restart and restart, in µs, in the order of
    /// [`Longest::ALL`].
    pub longest_us: Vec<u64>,
    pub timeouts: u64,
    /// Reads' waits ([`WAIT_BUCKETS`]).
    pub waits: Vec<u64>,
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

impl Meter {
    pub fn committed(&self, rows: usize, took: Duration) {
        self.commits.fetch_add(1, Ordering::Relaxed);
        self.rows
            .fetch_add(u64::try_from(rows).unwrap_or(u64::MAX), Ordering::Relaxed);
        self.apply_us.fetch_add(micros(took), Ordering::Relaxed);
        self.longest(Longest::Commit, took);
    }

    fn longest(&self, of: Longest, took: Duration) {
        if let Some(l) = self.longest.get(of.index()) {
            l.fetch_max(micros(took), Ordering::Relaxed);
        }
    }

    pub fn checkpointed(&self, took: Duration) {
        self.checkpoints.fetch_add(1, Ordering::Relaxed);
        self.checkpoint_us
            .fetch_add(micros(took), Ordering::Relaxed);
        self.longest(Longest::Checkpoint, took);
    }

    /// The log started again: `waited` for the readers and the checkpointer, then `took`.
    pub fn restarted(&self, waited: Duration, took: Duration) {
        self.restarts.fetch_add(1, Ordering::Relaxed);
        self.restart_wait_us
            .fetch_add(micros(waited), Ordering::Relaxed);
        self.restart_us.fetch_add(micros(took), Ordering::Relaxed);
        self.longest(Longest::RestartWait, waited);
        self.longest(Longest::Restart, took);
    }

    pub fn ran(&self) {
        self.jobs.fetch_add(1, Ordering::Relaxed);
    }

    pub fn held(&self, d: Duration) {
        self.busy_us.fetch_add(micros(d), Ordering::Relaxed);
    }

    pub fn flushed(&self, d: Duration) {
        self.flushes.fetch_add(1, Ordering::Relaxed);
        self.flush_us.fetch_add(micros(d), Ordering::Relaxed);
        self.longest(Longest::Flush, d);
    }

    /// A batch dropped: how many have been, this one too.
    pub fn dropped(&self) -> u64 {
        self.dropped
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1)
    }

    /// A batch whose commit failed: how many have, this one too.
    pub fn failed(&self) -> u64 {
        self.failed
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1)
    }

    pub fn waited(&self, d: Duration) {
        if let Some(w) = self.waits.get(wait_bucket(d)) {
            w.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn timed_out(&self) {
        self.timeouts.fetch_add(1, Ordering::Relaxed);
    }

    #[must_use]
    pub fn counters(&self) -> Counters {
        Counters {
            commits: self.commits.load(Ordering::Relaxed),
            rows: self.rows.load(Ordering::Relaxed),
            jobs: self.jobs.load(Ordering::Relaxed),
            busy_ms: self.busy_us.load(Ordering::Relaxed) / 1000,
            apply_ms: self.apply_us.load(Ordering::Relaxed) / 1000,
            apply_us: self.apply_us.load(Ordering::Relaxed),
            flushes: self.flushes.load(Ordering::Relaxed),
            flush_ms: self.flush_us.load(Ordering::Relaxed) / 1000,
            dropped: self.dropped.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            checkpoints: self.checkpoints.load(Ordering::Relaxed),
            checkpoint_ms: self.checkpoint_us.load(Ordering::Relaxed) / 1000,
            restarts: self.restarts.load(Ordering::Relaxed),
            restart_wait_ms: self.restart_wait_us.load(Ordering::Relaxed) / 1000,
            restart_ms: self.restart_us.load(Ordering::Relaxed) / 1000,
            longest_us: self
                .longest
                .iter()
                .map(|l| l.load(Ordering::Relaxed))
                .collect(),
            timeouts: self.timeouts.load(Ordering::Relaxed),
            waits: self
                .waits
                .iter()
                .map(|w| w.load(Ordering::Relaxed))
                .collect(),
        }
    }
}

/// How long the replicated class's flushes took, by power of two: bucket `i` holds those of at
/// most 62.5 µs · 2^i, from 62.5 µs to 16.384 s, and the last the rest. The measured flushes run
/// from 3.9 ms (p50, a calm Mac) to 5.6 s (max, loaded btrfs; replication.md, "Fast
/// acknowledgement"); the bounds below a millisecond are for a quiet fsync on NVMe, kiln's
/// production disk, as the Kiln Metrics agent asked (replication.md, "Metrics").
pub const FLUSH_BUCKETS: usize = 20;
const LAST_FLUSH: u32 = 19;
const FIRST_FLUSH_BOUND_NS: u64 = 62_500;

/// The bucket a flush of `d` counts in.
#[must_use]
pub fn flush_bucket(d: Duration) -> usize {
    let ns = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
    let bucket = (0..LAST_FLUSH)
        .find(|i| {
            FIRST_FLUSH_BOUND_NS
                .checked_shl(*i)
                .is_some_and(|bound| ns <= bound)
        })
        .unwrap_or(LAST_FLUSH);
    usize::try_from(bucket).unwrap_or(FLUSH_BUCKETS.saturating_sub(1))
}

/// The replicated class's metrics, as their host exports them: name, type and help, in the
/// order of [`ReplicaCounters`]' fields (docs/design/replication.md, "Metrics").
pub const REPLICA_METRICS: [(&str, &str, &str); 16] = [
    (
        "amphora_replica_flush_backlog_entries",
        "gauge",
        "Entries this member applied that its database's log hasn't flushed yet.",
    ),
    (
        "amphora_replica_flush_seconds",
        "histogram",
        "Each flush of the database's log the owner's flushing thread completed; a failed flush fences the owner.",
    ),
    (
        "amphora_replica_commits_total",
        "counter",
        "Entries this member committed as leader, by the quorum that committed each first (quorum: held, classic); leader only, so a follower's stays at 0.",
    ),
    // The leader's quorum patience (replication.md, "The leader's quorum patience"), its help lines
    // as Kiln Metrics agreed them (amphora-class-metrics/names/v1-disk-stalls).
    (
        "amphora_replica_quorum_patience_seconds",
        "gauge",
        "The ticks this replica, while it leads, waits beyond its election timeout before its quorum check, in seconds; zero while it follows or has measured no answer gap.",
    ),
    (
        "amphora_replica_quorum_patience_refused_total",
        "counter",
        "Answer gaps a leading replica refused to learn for its quorum patience, because the host's detector suspected the voter meanwhile.",
    ),
    // As Kiln Metrics agreed it (amphora-constants/names/v1): the falsifier of the owner's own
    // sessions' lease (replication.md, "The owner's own sessions last their role's raft timing").
    (
        "amphora_replica_own_sessions_expired_total",
        "counter",
        "The owner's own sessions (a floor session, the seed's attempt) that reached their lease at their barrier while this member led their term; many in one term are a sign the quorum patience is too short (one can wait behind a slow flush).",
    ),
    // Local reads (replication.md, "A local read on a replica"), as Kiln Metrics agreed them
    // (amphora-local-read/names/v1).
    (
        "amphora_replica_local_reads_total",
        "counter",
        "Local reads this replica's own readers ran (Database::query_local), asking no leader.",
    ),
    (
        "amphora_replica_local_reads_refused_total",
        "counter",
        "Local reads this replica turned away, by reason (timeout: no reader before its deadline; fenced: the owner ended or is fenced; bootstrapping: no first image yet).",
    ),
    (
        "amphora_replica_local_readers_in_use",
        "gauge",
        "This replica's local readers reading now, 0 to 4.",
    ),
    // The database's log (replication.md, "A replica's database log"), as Kiln Metrics agreed
    // them (amphora-replica-log/names/v1): the histogram's count stands for the passes.
    (
        "amphora_replica_checkpoint_seconds",
        "histogram",
        "Each checkpoint pass's time on the flushing thread; a failed pass fences the owner, so none is counted.",
    ),
    (
        "amphora_replica_database_log_restarts_total",
        "counter",
        "Times the owner started the database's log again from its beginning.",
    ),
    (
        "amphora_replica_database_log_bytes",
        "gauge",
        "The database log's length as the owner last counted it, in bytes (frames x 4,120).",
    ),
    // Kiln Metrics agreed it under amphora-log-bound/names/v1 (2026-11-12), named as amphora-replica-log/names/v1 reserved.
    (
        "amphora_replica_database_log_limit_bytes",
        "gauge",
        "The most the database's log may hold as its owner last learned it, in bytes (frames x 4,120): an equal share of its filesystem's free bytes among the replica logs there, past one snapshot's image.",
    ),
    // The leader's write slot (replication.md, "A follower's write under steady leader writes"),
    // as Kiln Metrics agreed them (amphora-g-slot/names/v2).
    (
        "amphora_replica_write_slot_holds_total",
        "counter",
        "Write slots this replica held while it led, a member's refused write given the next turn, by how each came out (outcome: forwarded, expired, released).",
    ),
    (
        "amphora_replica_write_slot_held_seconds_total",
        "counter",
        "Seconds this replica, while it led, held its own writes for members' write slots; its rate is the leader's share of time held.",
    ),
    (
        "amphora_replica_write_slot_length_seconds",
        "gauge",
        "The length of the write slot this replica last began while it led, learned from its members' retries, in seconds; zero before its first.",
    ),
];

/// How a write slot ended: `amphora_replica_write_slot_holds_total`'s `outcome`, a closed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotEnd {
    /// The member's next forward arrived, and the leader stepped it first.
    Forwarded,
    /// The slot's learned length passed first.
    Expired,
    /// The lead or its term ended, the member left or was suspected, a change of members or a
    /// hand-off began, or a held member forwarded again.
    Released,
}

impl SlotEnd {
    pub const ALL: [Self; 3] = [Self::Forwarded, Self::Expired, Self::Released];
    #[must_use]
    pub fn outcome(self) -> &'static str {
        match self {
            Self::Forwarded => "forwarded",
            Self::Expired => "expired",
            Self::Released => "released",
        }
    }
    fn index(self) -> usize {
        match self {
            Self::Forwarded => 0,
            Self::Expired => 1,
            Self::Released => 2,
        }
    }
}

/// Why a local read was turned away: `amphora_replica_local_reads_refused_total`'s `reason`, a
/// closed set (amphora-local-read/names/v1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalRefusal {
    /// No reader came free before the read's deadline, or the deadline had passed.
    Timeout,
    /// The owner ended or is fenced.
    Fenced,
    /// The replica hasn't taken its group's first image.
    Bootstrapping,
}

impl LocalRefusal {
    pub const ALL: [Self; 3] = [Self::Timeout, Self::Fenced, Self::Bootstrapping];
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Fenced => "fenced",
            Self::Bootstrapping => "bootstrapping",
        }
    }
    fn index(self) -> usize {
        match self {
            Self::Timeout => 0,
            Self::Fenced => 1,
            Self::Bootstrapping => 2,
        }
    }
}

/// A replica owner's counters for the replicated class, bumped by its owner and its flushing
/// thread, read by its host at any time without asking the owner.
#[derive(Default)]
pub struct ReplicaMeter {
    backlog: AtomicU64,
    flushes: [AtomicU64; FLUSH_BUCKETS],
    flush_us: AtomicU64,
    held: AtomicU64,
    classic: AtomicU64,
    patience_us: AtomicU64,
    refused: AtomicU64,
    local_reads: AtomicU64,
    local_refused: [AtomicU64; 3],
    readers_in_use: AtomicU64,
    checkpoints: [AtomicU64; FLUSH_BUCKETS],
    checkpoint_us: AtomicU64,
    restarts: AtomicU64,
    log_bytes: AtomicU64,
    log_limit_bytes: AtomicU64,
    slot_holds: [AtomicU64; 3],
    slot_held_us: AtomicU64,
    slot_us: AtomicU64,
    own_expired: AtomicU64,
}

/// What a replica owner's counters say now ([`REPLICA_METRICS`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplicaCounters {
    /// Entries applied and not yet flushed by the owner's flushing thread.
    pub flush_backlog: u64,
    /// The flushing thread's flushes, by [`flush_bucket`], and how long they took in all.
    pub flushes: [u64; FLUSH_BUCKETS],
    pub flush_time: Duration,
    /// Entries committed as leader by a held quorum before a classic one held them, and by a
    /// classic quorum.
    pub commits_held: u64,
    pub commits_classic: u64,
    /// The quorum patience in force while this member leads; zero while it follows.
    pub quorum_patience: Duration,
    /// Answer gaps refused for the patience because the host's detector suspected the voter.
    pub quorum_patience_refused: u64,
    /// Local reads run on this replica's readers, and those turned away by
    /// [`LocalRefusal::ALL`]'s order.
    pub local_reads: u64,
    pub local_reads_refused: [u64; 3],
    /// Local readers reading now.
    pub local_readers_in_use: u64,
    /// The flushing thread's checkpoint passes, by [`flush_bucket`], and how long they took in all.
    pub checkpoints: [u64; FLUSH_BUCKETS],
    pub checkpoint_time: Duration,
    /// Times the owner started the database's log again.
    pub database_log_restarts: u64,
    /// The database log's length as the owner last counted it, in bytes.
    pub database_log_bytes: u64,
    /// The most the database's log may hold as its owner last learned it, in bytes.
    pub database_log_limit_bytes: u64,
    /// Write slots held while leading, by [`SlotEnd::ALL`]'s order; the time the leader's own
    /// writes were held for them in all; and the length of the slot it last began.
    pub write_slot_holds: [u64; 3],
    pub write_slot_held: Duration,
    pub write_slot_length: Duration,
    /// The owner's own sessions that reached their lease at their barrier while it led their term.
    pub own_sessions_expired: u64,
}

impl ReplicaCounters {
    /// The local reads turned away for `reason`.
    #[must_use]
    pub fn local_refused(&self, reason: LocalRefusal) -> u64 {
        self.local_reads_refused
            .get(reason.index())
            .copied()
            .unwrap_or(0)
    }

    /// The write slots that ended `how`.
    #[must_use]
    pub fn write_slot_ended(&self, how: SlotEnd) -> u64 {
        self.write_slot_holds.get(how.index()).copied().unwrap_or(0)
    }
}

impl ReplicaMeter {
    pub fn flushed(&self, took: Duration) {
        if let Some(b) = self.flushes.get(flush_bucket(took)) {
            b.fetch_add(1, Ordering::Relaxed);
        }
        self.flush_us.fetch_add(micros(took), Ordering::Relaxed);
    }

    /// The owner's state after a step: its backlog, and its core's commits by quorum.
    pub fn stepped(&self, backlog: u64, held: u64, classic: u64) {
        self.backlog.store(backlog, Ordering::Relaxed);
        self.held.store(held, Ordering::Relaxed);
        self.classic.store(classic, Ordering::Relaxed);
    }

    /// The quorum patience the owner set for this tick's check: zero while it follows.
    pub fn patience(&self, patience: Duration) {
        self.patience_us.store(micros(patience), Ordering::Relaxed);
    }

    /// An answer gap the owner refused to learn, past the patience's ceiling.
    pub fn refused(&self) {
        self.refused.fetch_add(1, Ordering::Relaxed);
    }

    /// One of the owner's own sessions reached its lease at its barrier while it led its term.
    pub fn own_session_expired(&self) {
        self.own_expired.fetch_add(1, Ordering::Relaxed);
    }

    /// A local read ran on one of this replica's readers.
    pub fn local_read(&self) {
        self.local_reads.fetch_add(1, Ordering::Relaxed);
    }

    /// A local read turned away, for `reason`.
    pub fn local_refused(&self, reason: LocalRefusal) {
        if let Some(count) = self.local_refused.get(reason.index()) {
            count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The local readers reading now, set under the pool's lock as each is taken or given back.
    pub fn readers_in_use(&self, out: usize) {
        self.readers_in_use
            .store(u64::try_from(out).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    /// A checkpoint pass took `took`.
    pub fn checkpointed(&self, took: Duration) {
        if let Some(b) = self.checkpoints.get(flush_bucket(took)) {
            b.fetch_add(1, Ordering::Relaxed);
        }
        self.checkpoint_us
            .fetch_add(micros(took), Ordering::Relaxed);
    }

    /// The database's log is `bytes` long, as the owner counted it at its step.
    pub fn log_bytes(&self, bytes: u64) {
        self.log_bytes.store(bytes, Ordering::Relaxed);
    }

    /// The database's log may hold `bytes`, as the owner last learned it.
    pub fn log_limit_bytes(&self, bytes: u64) {
        self.log_limit_bytes.store(bytes, Ordering::Relaxed);
    }

    /// A write slot of this leader's ended `how`, its own writes held `held` for it.
    pub fn slot_ended(&self, how: SlotEnd, held: Duration) {
        if let Some(count) = self.slot_holds.get(how.index()) {
            count.fetch_add(1, Ordering::Relaxed);
        }
        self.slot_held_us.fetch_add(micros(held), Ordering::Relaxed);
    }

    /// This leader began a write slot `length` long.
    pub fn slot_began(&self, length: Duration) {
        self.slot_us.store(micros(length), Ordering::Relaxed);
    }

    /// The owner started the database's log again: it is empty.
    pub fn restarted(&self) {
        self.restarts.fetch_add(1, Ordering::Relaxed);
        self.log_bytes.store(0, Ordering::Relaxed);
    }

    #[must_use]
    pub fn counters(&self) -> ReplicaCounters {
        ReplicaCounters {
            flush_backlog: self.backlog.load(Ordering::Relaxed),
            flushes: std::array::from_fn(|i| {
                self.flushes.get(i).map_or(0, |b| b.load(Ordering::Relaxed))
            }),
            flush_time: Duration::from_micros(self.flush_us.load(Ordering::Relaxed)),
            commits_held: self.held.load(Ordering::Relaxed),
            commits_classic: self.classic.load(Ordering::Relaxed),
            quorum_patience: Duration::from_micros(self.patience_us.load(Ordering::Relaxed)),
            quorum_patience_refused: self.refused.load(Ordering::Relaxed),
            local_reads: self.local_reads.load(Ordering::Relaxed),
            local_reads_refused: std::array::from_fn(|i| {
                self.local_refused
                    .get(i)
                    .map_or(0, |count| count.load(Ordering::Relaxed))
            }),
            local_readers_in_use: self.readers_in_use.load(Ordering::Relaxed),
            checkpoints: std::array::from_fn(|i| {
                self.checkpoints
                    .get(i)
                    .map_or(0, |b| b.load(Ordering::Relaxed))
            }),
            checkpoint_time: Duration::from_micros(self.checkpoint_us.load(Ordering::Relaxed)),
            database_log_restarts: self.restarts.load(Ordering::Relaxed),
            database_log_bytes: self.log_bytes.load(Ordering::Relaxed),
            database_log_limit_bytes: self.log_limit_bytes.load(Ordering::Relaxed),
            write_slot_holds: std::array::from_fn(|i| {
                self.slot_holds
                    .get(i)
                    .map_or(0, |count| count.load(Ordering::Relaxed))
            }),
            write_slot_held: Duration::from_micros(self.slot_held_us.load(Ordering::Relaxed)),
            write_slot_length: Duration::from_micros(self.slot_us.load(Ordering::Relaxed)),
            own_sessions_expired: self.own_expired.load(Ordering::Relaxed),
        }
    }
}

/// The replicated application floor's metrics, as their host exports them: name, type and help,
/// in the order of [`FloorCounters`]' fields (docs/design/replication.md, "The application floor,
/// replicated", "Metrics"; agreed with Kiln Metrics under amphora-membership-floor/names/v1 and
/// v1-keeper).
pub const FLOOR_METRICS: [(&str, &str, &str); 6] = [
    (
        "amphora_floor_version",
        "gauge",
        "The application floor committed at this member's applied index, 0 for none.",
    ),
    (
        "amphora_floor_raises_total",
        "counter",
        "Raises of the application floor this member applied.",
    ),
    (
        "amphora_floor_refusals_total",
        "counter",
        "Requests the application floor refused at this member, by reason (unproven, stale_base, below_floor, no_claim, other_application, busy).",
    ),
    (
        "amphora_floor_unattested_members",
        "gauge",
        "Members of the configuration in force with no attestation at the floor under their current join; every member reports it, so take max by db.",
    ),
    (
        "amphora_floor_keeper_failures_total",
        "counter",
        "Passes of this member's floor keeper that failed for a reason other than fencing; it looks again when the applied index moves.",
    ),
    (
        "amphora_floor_keeper_failing",
        "gauge",
        "1 while this member's floor keeper's last pass failed, 0 once a pass succeeds; a stuck keeper on an idle database shows here.",
    ),
];

/// Why the floor refused a request: `amphora_floor_refusals_total`'s `reason`, a closed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloorRefusal {
    /// A raise or a promotion some member's attestation doesn't prove.
    Unproven,
    /// A raise the stale-base rule refused: the group changed while it was proven.
    StaleBase,
    /// A claim, a bind or a learner below the floor.
    BelowFloor,
    /// A handle with no claim on a database with a floor.
    NoClaim,
    /// A claim for another application than the floor's.
    OtherApplication,
    /// A raise or a promotion the group can't take now: joint, or a tail still to apply.
    Busy,
}

impl FloorRefusal {
    pub const ALL: [Self; 6] = [
        Self::Unproven,
        Self::StaleBase,
        Self::BelowFloor,
        Self::NoClaim,
        Self::OtherApplication,
        Self::Busy,
    ];
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::Unproven => "unproven",
            Self::StaleBase => "stale_base",
            Self::BelowFloor => "below_floor",
            Self::NoClaim => "no_claim",
            Self::OtherApplication => "other_application",
            Self::Busy => "busy",
        }
    }
    fn index(self) -> usize {
        match self {
            Self::Unproven => 0,
            Self::StaleBase => 1,
            Self::BelowFloor => 2,
            Self::NoClaim => 3,
            Self::OtherApplication => 4,
            Self::Busy => 5,
        }
    }
}

/// A replica owner's floor counters, bumped by its owner and its handles, read by its host at any
/// time without asking the owner.
#[derive(Default)]
pub struct FloorMeter {
    version: AtomicU64,
    raises: AtomicU64,
    refusals: [AtomicU64; 6],
    unattested: AtomicU64,
    keeper_failures: AtomicU64,
    keeper_failing: AtomicU64,
}

/// What a replica owner's floor counters say now ([`FLOOR_METRICS`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FloorCounters {
    pub version: u64,
    pub raises: u64,
    /// By [`FloorRefusal::ALL`]'s order.
    pub refusals: [u64; 6],
    pub unattested_members: u64,
    pub keeper_failures: u64,
    pub keeper_failing: u64,
}

impl FloorCounters {
    /// The refusals counted for `reason`.
    #[must_use]
    pub fn refused(&self, reason: FloorRefusal) -> u64 {
        self.refusals.get(reason.index()).copied().unwrap_or(0)
    }
}

impl FloorMeter {
    /// The floor and the members short of it at the applied index the owner just published.
    pub fn applied(&self, version: u32, unattested: usize) {
        self.version.store(u64::from(version), Ordering::Relaxed);
        self.unattested.store(
            u64::try_from(unattested).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }
    pub fn raised(&self) {
        self.raises.fetch_add(1, Ordering::Relaxed);
    }
    pub fn refused(&self, reason: FloorRefusal) {
        if let Some(count) = self.refusals.get(reason.index()) {
            count.fetch_add(1, Ordering::Relaxed);
        }
    }
    /// The keeper's pass: a failure counts and holds the gauge at 1; a success clears it.
    pub fn kept(&self, failed: bool) {
        if failed {
            self.keeper_failures.fetch_add(1, Ordering::Relaxed);
        }
        self.keeper_failing
            .store(u64::from(failed), Ordering::Relaxed);
    }
    #[must_use]
    pub fn counters(&self) -> FloorCounters {
        FloorCounters {
            version: self.version.load(Ordering::Relaxed),
            raises: self.raises.load(Ordering::Relaxed),
            refusals: std::array::from_fn(|i| {
                self.refusals
                    .get(i)
                    .map_or(0, |c| c.load(Ordering::Relaxed))
            }),
            unattested_members: self.unattested.load(Ordering::Relaxed),
            keeper_failures: self.keeper_failures.load(Ordering::Relaxed),
            keeper_failing: self.keeper_failing.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Waits count by power of two of milliseconds, and their quantiles read back as the top of the
    /// bucket they fall in (CP-T1.1: the store's waits, which its dashboard charts).
    #[test]
    fn waits_count_by_power_of_two_and_quantiles_read_back() {
        assert_eq!(wait_bucket(Duration::from_micros(300)), 0);
        assert_eq!(wait_bucket(Duration::from_millis(3)), 2);
        assert_eq!(wait_bucket(Duration::from_secs(600)), WAIT_BUCKETS - 1);
        assert_eq!(wait_quantile(&[0; WAIT_BUCKETS], 0.5), None);
        let mut b = [0u64; WAIT_BUCKETS];
        b[0] = 98;
        b[4] = 2;
        assert_eq!(wait_quantile(&b, 0.5), Some(1));
        assert_eq!(wait_quantile(&b, 0.99), Some(16));
    }

    /// Flushes count by power of two from 62.5 µs, each in the first bucket whose bound holds it
    /// (CP-T1.8: the replicated class's flush seconds, which its dashboard charts).
    #[test]
    fn flushes_count_in_the_first_bucket_whose_bound_holds_them() {
        assert_eq!(flush_bucket(Duration::from_micros(50)), 0);
        assert_eq!(flush_bucket(Duration::from_nanos(62_500)), 0);
        assert_eq!(flush_bucket(Duration::from_nanos(62_501)), 1);
        assert_eq!(flush_bucket(Duration::from_micros(400)), 3);
        assert_eq!(flush_bucket(Duration::from_millis(1)), 4);
        assert_eq!(flush_bucket(Duration::from_micros(1_001)), 5);
        assert_eq!(flush_bucket(Duration::from_micros(3_928)), 6);
        assert_eq!(flush_bucket(Duration::from_millis(16_384)), 18);
        assert_eq!(
            flush_bucket(Duration::from_millis(16_384) + Duration::from_nanos(1)),
            FLUSH_BUCKETS - 1
        );
        assert_eq!(flush_bucket(Duration::from_secs(600)), FLUSH_BUCKETS - 1);
    }

    /// CP-T1.8, doc truth (CLAUDE.md sections 6 and 10): the replicated class's metrics are the
    /// ones its design names. Do: read docs/design/replication.md. Expect: every
    /// `amphora_replica_` name there (a histogram's `_bucket` and `_count` series as the
    /// histogram) is in [`REPLICA_METRICS`], and every name there is in the design; every local
    /// read refusal reason in [`LocalRefusal::ALL`] is named there (AC-T1.10.L2).
    #[test]
    fn the_replica_metrics_are_the_ones_the_design_names() {
        let design = include_str!("../../../docs/design/replication.md");
        let named: std::collections::BTreeSet<&str> = design
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|word| word.starts_with("amphora_replica_"))
            .map(|word| {
                word.strip_suffix("_bucket")
                    .or_else(|| word.strip_suffix("_count"))
                    .unwrap_or(word)
            })
            .collect();
        let code = REPLICA_METRICS.iter().map(|(name, _, _)| *name).collect();
        assert_eq!(named, code);
        for reason in LocalRefusal::ALL {
            assert!(
                design.contains(&format!("`{}`", reason.reason())),
                "{reason:?}"
            );
        }
        for how in SlotEnd::ALL {
            assert!(design.contains(&format!("`{}`", how.outcome())), "{how:?}");
        }
    }

    /// CP-T1.8, doc truth (CLAUDE.md sections 6 and 10): the floor's metrics and their refusal
    /// reasons are the ones its design names. Do: read docs/design/replication.md. Expect: every
    /// `amphora_floor_` name there is in [`FLOOR_METRICS`] and every one of those is there; every
    /// reason in [`FloorRefusal::ALL`] is named there.
    #[test]
    fn the_floor_metrics_are_the_ones_the_design_names() {
        let design = include_str!("../../../docs/design/replication.md");
        let named: std::collections::BTreeSet<&str> = design
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|word| word.starts_with("amphora_floor_"))
            .collect();
        let code = FLOOR_METRICS.iter().map(|(name, _, _)| *name).collect();
        assert_eq!(named, code);
        for reason in FloorRefusal::ALL {
            assert!(
                design.contains(&format!("`{}`", reason.reason())),
                "{reason:?}"
            );
        }
    }
}
