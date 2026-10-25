//! What each conversation store is doing (`docs/conversations.md`, "Telemetry"): the counters its
//! writer and its readers keep as they go ([`Meter`]), what the writer last saw of its file
//! ([`FileView`]), and where each feed stands ([`ShipStats`], [`PeerView`]). Cheap to keep, read
//! at any time without asking the writer: what the console's System page shows, and what its
//! dashboards chart.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How long reads waited for the writer, counted by power of two of milliseconds: under 1 ms,
/// under 2, under 4, ... and the last bucket for the rest (past 16 s).
pub const WAIT_BUCKETS: usize = 16;

/// The bucket a wait of `d` counts in.
pub fn wait_bucket(d: Duration) -> usize {
    let ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    (u64::BITS - ms.leading_zeros()).min(WAIT_BUCKETS as u32 - 1) as usize
}

/// About the `q`th quantile (0 to 1) of the waits counted in `buckets`, in ms: the top of the
/// bucket it falls in (None: none counted).
pub fn wait_quantile(buckets: &[u64], q: f64) -> Option<u64> {
    let total: u64 = buckets.iter().sum();
    if total == 0 {
        return None;
    }
    let want = ((total as f64) * q).ceil().max(1.0) as u64;
    let mut seen = 0;
    for (i, n) in buckets.iter().enumerate() {
        seen += n;
        if seen >= want {
            return Some(1u64 << i);
        }
    }
    Some(1u64 << (buckets.len() - 1))
}

/// A store's counters, bumped by its writer and its readers as they go.
#[derive(Default)]
pub struct Meter {
    /// Commits of the rows sessions send, and those rows.
    commits: AtomicU64,
    rows: AtomicU64,
    /// Jobs run on the writer (reads, feed batches taken), and how long it was held, by the
    /// commits, the jobs and looking after the file, in µs.
    jobs: AtomicU64,
    busy_us: AtomicU64,
    /// Updates dropped while the writer couldn't keep up.
    dropped: AtomicU64,
    /// Reads that didn't get an answer in time (`READ_WAIT`).
    timeouts: AtomicU64,
    /// How long reads waited for their answer ([`WAIT_BUCKETS`]).
    waits: [AtomicU64; WAIT_BUCKETS],
    file: Mutex<FileView>,
}

impl Meter {
    pub fn committed(&self, rows: usize) {
        self.commits.fetch_add(1, Ordering::Relaxed);
        self.rows.fetch_add(rows as u64, Ordering::Relaxed);
    }

    pub fn ran(&self) {
        self.jobs.fetch_add(1, Ordering::Relaxed);
    }

    pub fn held(&self, d: Duration) {
        self.busy_us.fetch_add(u64::try_from(d.as_micros()).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    /// An update dropped: how many have been, this one too.
    pub fn dropped(&self) -> u64 {
        self.dropped.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn waited(&self, d: Duration) {
        if let Some(w) = self.waits.get(wait_bucket(d)) {
            w.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn timed_out(&self) {
        self.timeouts.fetch_add(1, Ordering::Relaxed);
    }

    /// What the writer saw of its file just now.
    pub fn saw(&self, f: impl FnOnce(&mut FileView)) {
        f(&mut self.file.lock().unwrap_or_else(PoisonError::into_inner));
    }

    /// Everything counted so far, and the file as last seen.
    pub fn report(&self, id: &str) -> StoreReport {
        StoreReport {
            id: id.to_owned(),
            commits: self.commits.load(Ordering::Relaxed),
            rows: self.rows.load(Ordering::Relaxed),
            jobs: self.jobs.load(Ordering::Relaxed),
            busy_ms: self.busy_us.load(Ordering::Relaxed) / 1000,
            dropped: self.dropped.load(Ordering::Relaxed),
            timeouts: self.timeouts.load(Ordering::Relaxed),
            waits: self.waits.iter().map(|w| w.load(Ordering::Relaxed)).collect(),
            file: self.file.lock().unwrap_or_else(PoisonError::into_inner).clone(),
        }
    }
}

/// What the writer last saw of its file: it looks every few seconds, between whatever else it does.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileView {
    /// Unix ms it looked (0: not yet).
    pub at: i64,
    /// The file's size, the part of it free for what comes next, and its log's size.
    pub bytes: u64,
    pub free: u64,
    pub wal: u64,
    /// The last number the store gave a write: how many it has had; and when it last saw that
    /// change (unix ms, 0: not since kiln started).
    pub top: u64,
    #[serde(default)]
    pub wrote_at: i64,
    /// Sessions it holds (None: not counted yet; it counts now and then).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sessions: Option<u64>,
    /// What it does to its file while it has nothing else to do.
    #[serde(default)]
    pub upkeep: Upkeeping,
    /// The last time its file was made compact, since kiln started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted: Option<Compacted>,
    /// The last time the records of sessions that ended long ago were let go, since kiln started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pruned: Option<Pruned>,
}

/// What a store does to its file while it has nothing else to do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Upkeeping {
    #[default]
    Idle,
    /// Compressing record lines kept before lines were compressed.
    Squeezing,
    /// Letting go of the records of sessions that ended long ago: lines so far.
    Pruning { lines: u64 },
    /// Making its file compact ([`CompactView`]).
    Compacting(CompactView),
}

/// Where making a file compact stands.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactView {
    /// `copy` (a table at a time: `table`), `catch_up` (what was written meanwhile), `count` (the
    /// copy counted against the store), `swap` (the copy about to take the file's place).
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
    /// The copy's size so far, and what the file held when it began (its size less what was free).
    pub copied: u64,
    pub of: u64,
    /// Unix ms it began.
    pub began: i64,
}

/// A file made compact.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compacted {
    /// Unix ms it was done, its size before and after, and how long it took.
    pub at: i64,
    pub before: u64,
    pub after: u64,
    pub ms: u64,
}

/// The records of sessions that ended long ago, let go.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pruned {
    pub at: i64,
    pub lines: u64,
}

/// A store's counters and file, as [`Meter::report`] reads them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreReport {
    pub id: String,
    pub commits: u64,
    pub rows: u64,
    pub jobs: u64,
    pub busy_ms: u64,
    pub dropped: u64,
    pub timeouts: u64,
    pub waits: Vec<u64>,
    pub file: FileView,
}

/// What a runner says of one of its stores, every few seconds ([`super::feed::Msg::Stats`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// `own`: the runner's store; `mirror`: its copy of the server's.
    pub role: String,
    /// The kiln it runs.
    pub version: String,
    pub store: StoreReport,
    /// Its feed to the server (its own store's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed: Option<ShipView>,
}

/// How often a runner reports its stores.
pub const REPORT_EVERY: Duration = Duration::from_secs(5);

/// The kiln this is, as reports say: its commit, short, else its version.
pub fn version() -> String {
    crate::api::COMMIT.get(..7).map_or_else(|| env!("CARGO_PKG_VERSION").to_owned(), str::to_owned)
}

/// Where a feed's sending side stands ([`super::feed::Shipper`]): bumped as it goes.
#[derive(Default)]
pub struct ShipStats {
    up: AtomicBool,
    /// What the taker has, plus one (0: it hasn't said, on this link).
    acked: AtomicU64,
    sent: AtomicU64,
    flight: AtomicU64,
    batches: AtomicU64,
    rows: AtomicU64,
    bytes: AtomicU64,
    /// Unix ms (0: never).
    last_sent: AtomicI64,
    last_ack: AtomicI64,
    /// Hellos said: when the link came up, and when the taker said nothing for too long.
    hellos: AtomicU64,
    stalls: AtomicU64,
}

impl ShipStats {
    pub fn link(&self, up: bool) {
        self.up.store(up, Ordering::Relaxed);
        if !up {
            self.acked.store(0, Ordering::Relaxed);
            self.flight.store(0, Ordering::Relaxed);
        }
    }

    pub fn hello(&self, stalled: bool) {
        self.hellos.fetch_add(1, Ordering::Relaxed);
        if stalled {
            self.stalls.fetch_add(1, Ordering::Relaxed);
            self.acked.store(0, Ordering::Relaxed);
        }
    }

    /// The taker has it through `rev`; `flight` batches are sent and not yet acknowledged.
    pub fn acked(&self, rev: u64, flight: usize) {
        self.acked.store(rev.saturating_add(1), Ordering::Relaxed);
        self.flight.store(flight as u64, Ordering::Relaxed);
        self.last_ack.store(crate::util::now_ms(), Ordering::Relaxed);
    }

    /// A batch of `rows` rows sent, `bytes` on the wire, reading through `upto`.
    pub fn sent(&self, upto: u64, rows: usize, bytes: usize, flight: usize) {
        self.sent.store(upto, Ordering::Relaxed);
        self.flight.store(flight as u64, Ordering::Relaxed);
        self.batches.fetch_add(1, Ordering::Relaxed);
        self.rows.fetch_add(rows as u64, Ordering::Relaxed);
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        self.last_sent.store(crate::util::now_ms(), Ordering::Relaxed);
    }

    /// Where it starts again from: the taker said it has `rev` (a first answer, or after a gap).
    pub fn from(&self, rev: u64) {
        self.sent.store(rev, Ordering::Relaxed);
        self.acked(rev, 0);
    }

    pub fn view(&self) -> ShipView {
        let at = |a: &AtomicI64| Some(a.load(Ordering::Relaxed)).filter(|t| *t > 0);
        ShipView {
            up: self.up.load(Ordering::Relaxed),
            acked: self.acked.load(Ordering::Relaxed).checked_sub(1),
            sent: self.sent.load(Ordering::Relaxed),
            flight: self.flight.load(Ordering::Relaxed),
            batches: self.batches.load(Ordering::Relaxed),
            rows: self.rows.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            last_sent: at(&self.last_sent),
            last_ack: at(&self.last_ack),
            hellos: self.hellos.load(Ordering::Relaxed),
            stalls: self.stalls.load(Ordering::Relaxed),
        }
    }
}

/// Where a feed's sending side stands, as [`ShipStats::view`] reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShipView {
    /// Linked to the taker.
    pub up: bool,
    /// Through where the taker has it (None: it hasn't said, on this link).
    pub acked: Option<u64>,
    /// Read and sent through here.
    pub sent: u64,
    /// Batches sent and not yet acknowledged.
    pub flight: u64,
    /// Since kiln started: batches sent, their rows, their bytes on the wire.
    pub batches: u64,
    pub rows: u64,
    pub bytes: u64,
    pub last_sent: Option<i64>,
    pub last_ack: Option<i64>,
    pub hellos: u64,
    pub stalls: u64,
}

/// Where a feed's taking side stands, for one store that feeds it ([`super::feed::Intake`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerView {
    pub machine: String,
    pub store: String,
    /// Through where it has the store's feed.
    pub rev: u64,
    /// Unix ms the store last said hello (0: not since kiln started).
    pub hello_at: i64,
    /// Since kiln started: batches taken, rows written from them, rows left out (of a session's
    /// former writer, or a copy's of what this store has), bytes on the wire.
    pub batches: u64,
    pub rows: u64,
    pub fenced: u64,
    pub left: u64,
    pub bytes: u64,
    /// Since kiln started: rows it had already as they came (sent again, from another store of
    /// the same machine), so nothing was written for them.
    #[serde(default)]
    pub same: u64,
    /// Batches that didn't follow what it had, and batches it couldn't take.
    pub gaps: u64,
    pub errors: u64,
    /// Unix ms of the last batch taken (0: none), and how long taking it held the store.
    pub last_batch: i64,
    pub take_ms: u64,
    /// The store is a mirror's copy, sent back to restore this one from.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub copy: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_count_by_power_of_two_and_quantiles_read_the_top_of_their_bucket() {
        assert_eq!(wait_bucket(Duration::from_micros(300)), 0);
        assert_eq!(wait_bucket(Duration::from_millis(1)), 1);
        assert_eq!(wait_bucket(Duration::from_millis(3)), 2);
        assert_eq!(wait_bucket(Duration::from_millis(1000)), 10);
        assert_eq!(wait_bucket(Duration::from_secs(600)), WAIT_BUCKETS - 1);
        assert_eq!(wait_quantile(&[0; WAIT_BUCKETS], 0.5), None);
        let b: Vec<u64> = (0..WAIT_BUCKETS).map(|i| match i {
            0 => 98,
            10 => 2,
            _ => 0,
        }).collect();
        assert_eq!(wait_quantile(&b, 0.5), Some(1));
        assert_eq!(wait_quantile(&b, 0.99), Some(1024));
    }

    #[test]
    fn a_feed_that_went_down_has_no_acknowledgement_until_the_taker_says_again() {
        let s = ShipStats::default();
        s.link(true);
        s.from(10);
        assert_eq!(s.view().acked, Some(10));
        s.sent(20, 5, 300, 1);
        s.acked(20, 0);
        let v = s.view();
        assert_eq!((v.acked, v.sent, v.batches, v.rows, v.bytes), (Some(20), 20, 1, 5, 300));
        s.link(false);
        assert_eq!(s.view().acked, None);
        assert!(!s.view().up);
    }
}
