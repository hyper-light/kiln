//! Prometheus metrics with no allocation on the hot path: atomics for counters and gauges, fixed
//! bucket histograms, and labelled families keyed by a small string set. Host statistics are read
//! straight from `/proc` on demand (only when scraped or sampled), so idle cost is ~zero.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

#[derive(Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[derive(Default)]
pub struct Gauge(AtomicI64);

impl Gauge {
    pub fn set(&self, v: i64) {
        self.0.store(v, Ordering::Relaxed);
    }
    pub fn add(&self, v: i64) {
        self.0.fetch_add(v, Ordering::Relaxed);
    }
    pub fn get(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// A histogram of seconds, its buckets fixed (build and step durations, sync's latencies).
pub struct Histogram {
    bounds: &'static [f64],
    buckets: Vec<AtomicU64>,
    count: AtomicU64,
    /// Microseconds, so that a millisecond's latencies add up.
    sum_us: AtomicU64,
}

pub const DURATION_BUCKETS: &[f64] =
    &[1.0, 5.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1200.0, 1800.0, 3600.0, 7200.0];
pub const QUEUE_BUCKETS: &[f64] = &[0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 300.0, 900.0];
/// A millisecond to ten minutes, about 2.5 times apart: sync's commits, transfers and how long a
/// change takes to reach another machine, and a move's waits.
pub const LATENCY_BUCKETS: &[f64] =
    &[0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 60.0, 150.0, 300.0, 600.0];

impl Histogram {
    pub fn new(bounds: &'static [f64]) -> Self {
        Self {
            bounds,
            buckets: bounds.iter().map(|_| AtomicU64::new(0)).collect(),
            count: AtomicU64::new(0),
            sum_us: AtomicU64::new(0),
        }
    }

    pub fn observe(&self, secs: f64) {
        for (b, c) in self.bounds.iter().zip(&self.buckets) {
            if secs <= *b {
                c.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        self.sum_us.fetch_add((secs.max(0.0) * 1_000_000.0) as u64, Ordering::Relaxed);
    }

    fn render(&self, out: &mut String, name: &str, labels: &str) {
        let sep = if labels.is_empty() { "" } else { "," };
        for (b, c) in self.bounds.iter().zip(&self.buckets) {
            let _ = writeln!(
                out,
                "{name}_bucket{{{labels}{sep}le=\"{b}\"}} {}",
                c.load(Ordering::Relaxed)
            );
        }
        let n = self.count.load(Ordering::Relaxed);
        let _ = writeln!(out, "{name}_bucket{{{labels}{sep}le=\"+Inf\"}} {n}");
        #[allow(clippy::cast_precision_loss)]
        let sum = self.sum_us.load(Ordering::Relaxed) as f64 / 1_000_000.0;
        let wrap = if labels.is_empty() { String::new() } else { format!("{{{labels}}}") };
        let _ = writeln!(out, "{name}_sum{wrap} {sum}");
        let _ = writeln!(out, "{name}_count{wrap} {n}");
    }
}

/// A label-keyed family, e.g. job results by `(repo, status)`.
pub struct CounterVec {
    keys: &'static [&'static str],
    series: RwLock<BTreeMap<Vec<String>, u64>>,
}

impl CounterVec {
    pub fn new(keys: &'static [&'static str]) -> Self {
        Self { keys, series: RwLock::new(BTreeMap::new()) }
    }

    pub fn inc(&self, values: &[&str]) {
        self.add(values, 1);
    }

    pub fn add(&self, values: &[&str], n: u64) {
        let key: Vec<String> = values.iter().map(|s| (*s).to_owned()).collect();
        *self.series.write().entry(key).or_insert(0) += n;
    }

    fn render(&self, out: &mut String, name: &str) {
        for (vals, n) in self.series.read().iter() {
            let _ = writeln!(out, "{name}{{{}}} {n}", labels(self.keys, vals));
        }
    }
}

/// A label-keyed gauge family, measured whole each time: what isn't measured any more (a runner
/// that went) goes from it.
pub struct GaugeVec {
    keys: &'static [&'static str],
    series: RwLock<BTreeMap<Vec<String>, i64>>,
}

impl GaugeVec {
    pub fn new(keys: &'static [&'static str]) -> Self {
        Self { keys, series: RwLock::new(BTreeMap::new()) }
    }

    /// Every series, as just measured: label values (in `keys` order) and value.
    pub fn replace(&self, series: impl IntoIterator<Item = (Vec<String>, i64)>) {
        *self.series.write() = series.into_iter().collect();
    }

    /// The series of one publisher, as just measured: those whose first label is one of `owned`
    /// (what it published before, and now) replaced, another publisher's kept (several sync
    /// servers in one process, as tests have them).
    pub fn replace_owned(&self, owned: &BTreeSet<String>, series: impl IntoIterator<Item = (Vec<String>, i64)>) {
        let mut g = self.series.write();
        g.retain(|k, _| k.first().is_none_or(|first| !owned.contains(first)));
        g.extend(series);
    }

    fn render(&self, out: &mut String, name: &str) {
        for (vals, v) in self.series.read().iter() {
            let _ = writeln!(out, "{name}{{{}}} {v}", labels(self.keys, vals));
        }
    }
}

/// A label-keyed histogram family, e.g. how long commits take by repo.
pub struct HistogramVec {
    keys: &'static [&'static str],
    bounds: &'static [f64],
    series: RwLock<BTreeMap<Vec<String>, Histogram>>,
}

impl HistogramVec {
    pub fn new(keys: &'static [&'static str], bounds: &'static [f64]) -> Self {
        Self { keys, bounds, series: RwLock::new(BTreeMap::new()) }
    }

    pub fn observe(&self, values: &[&str], secs: f64) {
        let key: Vec<String> = values.iter().map(|s| (*s).to_owned()).collect();
        if let Some(h) = self.series.read().get(&key) {
            h.observe(secs);
            return;
        }
        self.series.write().entry(key).or_insert_with(|| Histogram::new(self.bounds)).observe(secs);
    }

    fn render(&self, out: &mut String, name: &str) {
        for (vals, h) in self.series.read().iter() {
            h.render(out, name, &labels(self.keys, vals));
        }
    }
}

fn escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn labels(keys: &[&str], vals: &[String]) -> String {
    keys.iter()
        .zip(vals)
        .map(|(k, v)| format!("{k}=\"{}\"", escape(v)))
        .collect::<Vec<_>>()
        .join(",")
}

pub struct Metrics {
    pub runs_started: Counter,
    pub jobs_finished: CounterVec,
    pub steps_finished: CounterVec,
    pub jobs_queued: Gauge,
    pub jobs_running: Gauge,
    pub agents_connected: Gauge,
    pub agent_slots_total: Gauge,
    pub agent_slots_busy: Gauge,
    pub job_duration: Histogram,
    pub queue_wait: Histogram,
    pub log_bytes: Counter,
    pub gitops_polls: CounterVec,
    pub gitops_rate_remaining: Gauge,
    pub gitops_rate_limit: Gauge,
    pub terminal_sessions: Gauge,
    pub ws_clients: Gauge,
    pub supervisor_restarts: Counter,
    pub panics_caught: Counter,
    pub quic_streams: Counter,
    pub k8s_pods_launched: CounterVec,
    /// Each runner, connected (1) or not (0).
    pub runner_up: GaugeVec,
    /// Each runner, behind the newest kiln (1) or not (0).
    pub runner_behind: GaugeVec,
    /// Each runner's update policy (1 for the one it has).
    pub runner_update_policy: GaugeVec,
    /// The kiln each runner runs (1).
    pub runner_version_info: GaugeVec,
    /// The CI engine's (docs/ci.md W19-R02): cache lookups by result and saves (W08-R13),
    /// attempts by conclusion and failure class, queue seconds by wait reason, and how far
    /// behind the rollups are.
    pub ci_cache: CounterVec,
    pub ci_attempts: CounterVec,
    pub ci_wait_seconds: CounterVec,
    pub ci_rollup_lag: Gauge,
}

pub fn global() -> &'static Metrics {
    static M: OnceLock<Metrics> = OnceLock::new();
    M.get_or_init(|| Metrics {
        runs_started: Counter::default(),
        jobs_finished: CounterVec::new(&["repo", "status", "executor"]),
        steps_finished: CounterVec::new(&["status"]),
        jobs_queued: Gauge::default(),
        jobs_running: Gauge::default(),
        agents_connected: Gauge::default(),
        agent_slots_total: Gauge::default(),
        agent_slots_busy: Gauge::default(),
        job_duration: Histogram::new(DURATION_BUCKETS),
        queue_wait: Histogram::new(QUEUE_BUCKETS),
        log_bytes: Counter::default(),
        gitops_polls: CounterVec::new(&["result"]),
        gitops_rate_remaining: Gauge::default(),
        gitops_rate_limit: Gauge::default(),
        terminal_sessions: Gauge::default(),
        ws_clients: Gauge::default(),
        supervisor_restarts: Counter::default(),
        panics_caught: Counter::default(),
        quic_streams: Counter::default(),
        k8s_pods_launched: CounterVec::new(&["result"]),
        runner_up: GaugeVec::new(&["runner"]),
        runner_behind: GaugeVec::new(&["runner"]),
        runner_update_policy: GaugeVec::new(&["runner", "policy"]),
        runner_version_info: GaugeVec::new(&["runner", "version"]),
        ci_cache: CounterVec::new(&["result"]),
        ci_attempts: CounterVec::new(&["conclusion", "class"]),
        ci_wait_seconds: CounterVec::new(&["reason"]),
        ci_rollup_lag: Gauge::default(),
    })
}

/// Repo sync's (docs/sync.md, "Watching it"): the server's, as the leader of each synced repo's
/// log; each runner's, as the copies it holds; and agent sessions' moves between machines. `repo`
/// is a synced repo's name; `machine`, a copy's machine (Observe adds `host`, the process's own).
pub struct SyncMetrics {
    /// Steps each repo's log committed and its tree took; the changes they made by `kind`
    /// (`added`, `changed`, `deleted`); and those that met another machine's change (the log kept
    /// its own; the step's version went beside it).
    pub steps_committed: CounterVec,
    pub changes: CounterVec,
    pub conflicts: CounterVec,
    /// What the server let go: `what` a step or a raft `message`, and why.
    pub dropped: CounterVec,
    /// Something done again: by the server (`step_again`, a step sent again; `contents_asked`,
    /// contents asked for again) or a runner (`fetch`, `upload`, `resend`).
    pub retries: CounterVec,
    /// What went wrong, by `kind`: a repo's `group` (server or runner), a copy's `folder` taking
    /// the log, or `look`ing at it (runner).
    pub errors: CounterVec,
    /// From a step's arrival at the server to its commit.
    pub commit_seconds: HistogramVec,
    /// Each copy, as the server last heard of it: log entries its folder hasn't taken, its own
    /// steps not in the log yet, contents it waits for, its state (every state, 1 for the one it's
    /// in), and how long since it said how it is.
    pub copy_lag_steps: GaugeVec,
    pub copy_unsent_steps: GaugeVec,
    pub copy_waiting_blobs: GaugeVec,
    pub copy_state: GaugeVec,
    pub copy_heard_age: GaugeVec,
    /// Conflict copies each repo holds, not resolved yet.
    pub conflict_files: GaugeVec,
    /// Contents machines sent (`up`) and fetched (`down`) at the server, by result, and their bytes.
    pub blob_requests: CounterVec,
    pub blob_bytes: CounterVec,
    /// A runner's: steps its copies proposed and took into their folders; what taking them did to
    /// each file (`applied`, `conflict`, `kept`: changed here and on its way, `refused`).
    pub proposed: CounterVec,
    pub applied: CounterVec,
    pub folder_writes: CounterVec,
    /// Contents moved with the server: `up` or `down`, `ok`, `present` (there already), `gone`
    /// (replaced since) or `failed` (tried again); their bytes, and how long each took.
    pub transfers: CounterVec,
    pub transfer_bytes: CounterVec,
    pub transfer_seconds: HistogramVec,
    /// From a change's look on the machine that made it to its file here (the two clocks apart
    /// add to it).
    pub apply_lag: HistogramVec,
    /// How long a look at a folder for what changed takes.
    pub look_seconds: HistogramVec,
    /// Files each copy's last look left out (too big, or unreadable).
    pub skipped_files: GaugeVec,
    /// Agent sessions moving off this machine, by result; how long a session leaving waited for
    /// its folder's last changes to be in the log, and one arriving for them to be in its folder;
    /// and what its repository carried besides synced files (`git`, `ignored`, `worktree`).
    pub moves: CounterVec,
    pub move_barrier: Histogram,
    pub move_files_wait: Histogram,
    pub move_carried_bytes: CounterVec,
}

pub fn sync() -> &'static SyncMetrics {
    static M: OnceLock<SyncMetrics> = OnceLock::new();
    M.get_or_init(|| SyncMetrics {
        steps_committed: CounterVec::new(&["repo"]),
        changes: CounterVec::new(&["repo", "kind"]),
        conflicts: CounterVec::new(&["repo"]),
        dropped: CounterVec::new(&["repo", "what", "reason"]),
        retries: CounterVec::new(&["repo", "what"]),
        errors: CounterVec::new(&["repo", "kind"]),
        commit_seconds: HistogramVec::new(&["repo"], LATENCY_BUCKETS),
        copy_lag_steps: GaugeVec::new(&["repo", "machine"]),
        copy_unsent_steps: GaugeVec::new(&["repo", "machine"]),
        copy_waiting_blobs: GaugeVec::new(&["repo", "machine"]),
        copy_state: GaugeVec::new(&["repo", "machine", "state"]),
        copy_heard_age: GaugeVec::new(&["repo", "machine"]),
        conflict_files: GaugeVec::new(&["repo"]),
        blob_requests: CounterVec::new(&["machine", "direction", "result"]),
        blob_bytes: CounterVec::new(&["machine", "direction"]),
        proposed: CounterVec::new(&["repo"]),
        applied: CounterVec::new(&["repo"]),
        folder_writes: CounterVec::new(&["repo", "outcome"]),
        transfers: CounterVec::new(&["repo", "direction", "result"]),
        transfer_bytes: CounterVec::new(&["repo", "direction"]),
        transfer_seconds: HistogramVec::new(&["direction"], LATENCY_BUCKETS),
        apply_lag: HistogramVec::new(&["repo"], LATENCY_BUCKETS),
        look_seconds: HistogramVec::new(&["repo"], LATENCY_BUCKETS),
        skipped_files: GaugeVec::new(&["repo"]),
        moves: CounterVec::new(&["result"]),
        move_barrier: Histogram::new(LATENCY_BUCKETS),
        move_files_wait: Histogram::new(LATENCY_BUCKETS),
        move_carried_bytes: CounterVec::new(&["what"]),
    })
}

/// kiln's bytes behind kiln-seams' `Blobs` (docs/blobs.md), by `store` (`local`, `s3`): requests by
/// `op` and `outcome` (`ok`, `refused`: a typed refusal, `unavailable`: the store couldn't answer);
/// how long each took; the bytes `written` and `read`; objects let go by what keeps them (`kind`:
/// `cas`, `sync`, `recordings`, `transcripts`); and closed conversations whose record couldn't be
/// archived before the conversation store let it go (`crate::harness::archive`).
pub struct BlobsMetrics {
    pub requests: CounterVec,
    pub seconds: HistogramVec,
    pub bytes: CounterVec,
    pub collected: CounterVec,
    pub archives_given_up: Counter,
}

pub fn blobs() -> &'static BlobsMetrics {
    static M: OnceLock<BlobsMetrics> = OnceLock::new();
    M.get_or_init(|| BlobsMetrics {
        requests: CounterVec::new(&["store", "op", "outcome"]),
        seconds: HistogramVec::new(&["store", "op"], LATENCY_BUCKETS),
        bytes: CounterVec::new(&["store", "direction"]),
        collected: CounterVec::new(&["kind"]),
        archives_given_up: Counter::default(),
    })
}

/// The bus kiln's events ride (docs/bus.md): what its live feeds published and were refused, and
/// what its viewers were told they missed.
pub struct BusMetrics {
    /// Frames the live feeds published.
    pub published: Counter,
    /// What the bus refused a live feed, by operation (create, publish, delete).
    pub refused: CounterVec,
    /// Viewers told they fell behind what a feed keeps, and sent back to a snapshot.
    pub lag_notices: Counter,
    /// How long a frame took from its publish to a viewer's read.
    pub deliver_seconds: Histogram,
}

pub fn bus() -> &'static BusMetrics {
    static M: OnceLock<BusMetrics> = OnceLock::new();
    M.get_or_init(|| BusMetrics { published: Counter::default(), refused: CounterVec::new(&["op"]), lag_notices: Counter::default(), deliver_seconds: Histogram::new(LATENCY_BUCKETS) })
}

/// The bus's figures as the server's node states them when scraped: set by whoever holds the bus.
pub struct BusGauges {
    pub topics: Gauge,
    pub viewers: Gauge,
    pub memory_bytes: Gauge,
    pub memory_limit_bytes: Gauge,
}

pub fn bus_gauges() -> &'static BusGauges {
    static M: OnceLock<BusGauges> = OnceLock::new();
    M.get_or_init(|| BusGauges { topics: Gauge::default(), viewers: Gauge::default(), memory_bytes: Gauge::default(), memory_limit_bytes: Gauge::default() })
}

/// The `kiln_bus_*` families, in the exposition.
fn render_bus(o: &mut String) {
    let m = bus();
    let g = bus_gauges();
    let help = |o: &mut String, name: &str, kind: &str, h: &str| {
        let _ = writeln!(o, "# HELP {name} {h}\n# TYPE {name} {kind}");
    };
    help(o, "kiln_bus_published_total", "counter", "Frames sessions' live feeds published on the bus.");
    let _ = writeln!(o, "kiln_bus_published_total {}", m.published.get());
    help(o, "kiln_bus_refused_total", "counter", "Operations the bus refused a live feed, by operation (create, publish, delete): each sends the feed's viewers back to a snapshot.");
    m.refused.render(o, "kiln_bus_refused_total");
    help(o, "kiln_bus_lag_notices_total", "counter", "Viewers told they fell behind what a live feed keeps, and sent back to a snapshot.");
    let _ = writeln!(o, "kiln_bus_lag_notices_total {}", m.lag_notices.get());
    help(o, "kiln_bus_deliver_seconds", "histogram", "How long a live feed's frame took from its publish to a viewer's read.");
    m.deliver_seconds.render(o, "kiln_bus_deliver_seconds", "");
    help(o, "kiln_bus_topics", "gauge", "Topics the bus holds.");
    let _ = writeln!(o, "kiln_bus_topics {}", g.topics.get());
    help(o, "kiln_bus_viewers", "gauge", "Viewers reading sessions' live feeds.");
    let _ = writeln!(o, "kiln_bus_viewers {}", g.viewers.get());
    help(o, "kiln_bus_memory_bytes", "gauge", "Bytes the bus's messages take, framing included, until their readers let them go.");
    let _ = writeln!(o, "kiln_bus_memory_bytes {}", g.memory_bytes.get());
    help(o, "kiln_bus_memory_limit_bytes", "gauge", "Bytes the bus's messages may take ([bus] memory).");
    let _ = writeln!(o, "kiln_bus_memory_limit_bytes {}", g.memory_limit_bytes.get());
}

/// The `kiln_blobs_*` families, in the exposition.
fn render_blobs(o: &mut String) {
    let m = blobs();
    let help = |o: &mut String, name: &str, kind: &str, h: &str| {
        let _ = writeln!(o, "# HELP {name} {h}\n# TYPE {name} {kind}");
    };
    help(o, "kiln_blobs_requests_total", "counter", "Requests to the store kiln keeps its bytes in, by store, operation and outcome (ok, refused, unavailable).");
    m.requests.render(o, "kiln_blobs_requests_total");
    help(o, "kiln_blobs_bytes_total", "counter", "Bytes written to and read from the store kiln keeps its bytes in.");
    m.bytes.render(o, "kiln_blobs_bytes_total");
    help(o, "kiln_blobs_collected_total", "counter", "Objects kiln let go that nothing refers to, by what kept them.");
    m.collected.render(o, "kiln_blobs_collected_total");
    help(o, "kiln_blobs_archives_given_up_total", "counter", "Closed conversations whose record couldn't be archived in the days held past the conversation store's keeping: given up, and let go by the store.");
    let _ = writeln!(o, "kiln_blobs_archives_given_up_total {}", m.archives_given_up.get());
    help(o, "kiln_blobs_request_seconds", "histogram", "How long requests to the store kiln keeps its bytes in took, by store and operation.");
    m.seconds.render(o, "kiln_blobs_request_seconds");
}

/// Repo sync's families, in the exposition.
fn render_sync(o: &mut String) {
    let m = sync();
    let help = |o: &mut String, name: &str, kind: &str, h: &str| {
        let _ = writeln!(o, "# HELP {name} {h}\n# TYPE {name} {kind}");
    };
    for (name, h, v) in [
        ("kiln_sync_steps_committed_total", "Steps a synced repo's log committed.", &m.steps_committed),
        ("kiln_sync_changes_total", "Changes committed steps made, by kind (added, changed, deleted).", &m.changes),
        ("kiln_sync_conflicts_total", "Committed changes that met another machine's: kept beside.", &m.conflicts),
        ("kiln_sync_dropped_total", "Steps and raft messages the server let go, by reason.", &m.dropped),
        ("kiln_sync_retries_total", "Steps, contents and transfers tried again.", &m.retries),
        ("kiln_sync_errors_total", "Sync errors, by kind (group, folder, look).", &m.errors),
        ("kiln_sync_blob_requests_total", "Contents sent (up) and fetched (down) at the server, by machine and result.", &m.blob_requests),
        ("kiln_sync_blob_bytes_total", "Bytes of contents sent (up) and fetched (down) at the server, by machine.", &m.blob_bytes),
        ("kiln_sync_proposed_total", "Steps this machine's copies proposed.", &m.proposed),
        ("kiln_sync_applied_total", "Committed steps this machine's copies took into their folders.", &m.applied),
        ("kiln_sync_folder_writes_total", "What taking committed steps did to each file here, by outcome.", &m.folder_writes),
        ("kiln_sync_transfers_total", "Contents moved with the server, by direction and result.", &m.transfers),
        ("kiln_sync_transfer_bytes_total", "Bytes of contents moved with the server, by direction.", &m.transfer_bytes),
        ("kiln_moves_total", "Agent sessions moving off this machine, by result.", &m.moves),
        ("kiln_move_carried_bytes_total", "What moving sessions' repositories carried besides synced files (git, ignored, worktree).", &m.move_carried_bytes),
    ] {
        help(o, name, "counter", h);
        v.render(o, name);
    }
    for (name, h, v) in [
        ("kiln_sync_copy_lag_steps", "Log entries a copy's folder hasn't taken yet.", &m.copy_lag_steps),
        ("kiln_sync_copy_unsent_steps", "A copy's own steps not in the log yet.", &m.copy_unsent_steps),
        ("kiln_sync_copy_waiting_blobs", "Contents a copy waits for.", &m.copy_waiting_blobs),
        ("kiln_sync_copy_state", "Each copy's state: 1 for the one it is in, 0 for the others.", &m.copy_state),
        ("kiln_sync_copy_heard_age_seconds", "Seconds since a copy last said how it is.", &m.copy_heard_age),
        ("kiln_sync_conflict_files", "Conflict copies a synced repo holds, not resolved yet.", &m.conflict_files),
        ("kiln_sync_skipped_files", "Files a copy's last look left out.", &m.skipped_files),
    ] {
        help(o, name, "gauge", h);
        v.render(o, name);
    }
    for (name, h, v) in [
        ("kiln_sync_commit_seconds", "From a step's arrival at the server to its commit.", &m.commit_seconds),
        ("kiln_sync_transfer_seconds", "How long moving contents with the server took, by direction.", &m.transfer_seconds),
        ("kiln_sync_apply_lag_seconds", "From a change's look on the machine that made it to its file here.", &m.apply_lag),
        ("kiln_sync_look_seconds", "How long a look at a copy's folder took.", &m.look_seconds),
    ] {
        help(o, name, "histogram", h);
        v.render(o, name);
    }
    help(o, "kiln_move_barrier_seconds", "histogram", "How long a session leaving waited for its folder's last changes to be in the log.");
    m.move_barrier.render(o, "kiln_move_barrier_seconds", "");
    help(o, "kiln_move_files_wait_seconds", "histogram", "How long a session arriving waited for its folder's last changes.");
    m.move_files_wait.render(o, "kiln_move_files_wait_seconds", "");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_labelled_histogram_renders_each_series_whole() {
        let h = HistogramVec::new(&["repo"], LATENCY_BUCKETS);
        h.observe(&["a"], 0.0004);
        h.observe(&["a"], 0.003);
        h.observe(&["b\"q"], 90.0);
        let mut out = String::new();
        h.render(&mut out, "x_seconds");
        assert!(out.contains("x_seconds_bucket{repo=\"a\",le=\"0.001\"} 1"), "{out}");
        assert!(out.contains("x_seconds_bucket{repo=\"a\",le=\"0.005\"} 2"), "{out}");
        assert!(out.contains("x_seconds_bucket{repo=\"a\",le=\"+Inf\"} 2"), "{out}");
        // Microseconds add up where milliseconds would round to nothing.
        assert!(out.contains("x_seconds_sum{repo=\"a\"} 0.0034"), "{out}");
        assert!(out.contains("x_seconds_count{repo=\"a\"} 2"), "{out}");
        assert!(out.contains("x_seconds_bucket{repo=\"b\\\"q\",le=\"60\"} 0"), "escaped, and over the top bucket but one: {out}");
        assert!(out.contains("x_seconds_bucket{repo=\"b\\\"q\",le=\"150\"} 1"), "{out}");
    }

    #[test]
    fn sync_families_render_even_before_anything_happened() {
        let text = render();
        for name in ["kiln_sync_steps_committed_total", "kiln_sync_copy_state", "kiln_sync_transfer_seconds", "kiln_move_barrier_seconds", "kiln_moves_total"] {
            assert!(text.contains(&format!("# TYPE {name} ")), "{name}");
        }
        assert!(text.contains("kiln_move_barrier_seconds_bucket{le=\"600\"} "), "an unlabelled histogram's buckets");
    }
}

/// Live per-agent host samples, reported by agents and rendered with an `agent` label.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HostSample {
    pub ts_ms: i64,
    pub cpu_pct: f32,
    pub cpus: u32,
    pub load1: f32,
    pub mem_total: u64,
    pub mem_used: u64,
    pub disk_total: u64,
    pub disk_used: u64,
    pub net_rx: u64,
    pub net_tx: u64,
    pub procs: u32,
}

pub static AGENT_SAMPLES: OnceLock<RwLock<BTreeMap<String, HostSample>>> = OnceLock::new();

pub fn agent_samples() -> &'static RwLock<BTreeMap<String, HostSample>> {
    AGENT_SAMPLES.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// The conversation stores' metrics (`crate::convo::board`): name, type and help of each, in the
/// order [`set_convo`] measures them.
const CONVO: [(&str, &str, &str); 13] = [
    ("kiln_convo_store_up", "gauge", "Conversation stores linked to the server (1) or not (0)."),
    ("kiln_convo_store_bytes", "gauge", "Each conversation store's file size."),
    ("kiln_convo_store_free_bytes", "gauge", "The part of each store's file free for what comes next."),
    ("kiln_convo_store_wal_bytes", "gauge", "Each store's log (WAL) size."),
    ("kiln_convo_store_rev", "gauge", "Each store's last write number: how many writes it has had."),
    ("kiln_convo_store_sessions", "gauge", "Sessions each store holds."),
    ("kiln_convo_store_behind_rows", "gauge", "Rows a store's feed hasn't delivered yet (a runner's to the server, the server's to a mirror)."),
    ("kiln_convo_store_commits_total", "counter", "Commits of each store's writer, since its kiln started."),
    ("kiln_convo_store_busy_milliseconds_total", "counter", "Time each store's writer was busy, since its kiln started."),
    ("kiln_convo_store_dropped_total", "counter", "Updates each store dropped while its writer couldn't keep up."),
    ("kiln_convo_store_read_timeouts_total", "counter", "Reads of each store that got no answer in time."),
    ("kiln_convo_feed_batches_total", "counter", "Feed batches: taken by the server, from a runner's store; sent to a mirror."),
    ("kiln_convo_feed_bytes_total", "counter", "Feed bytes on the wire, as for kiln_convo_feed_batches_total."),
];

/// Each store's labels (`store`, `role`, `machine`) and its [`CONVO`] values (None: not known).
type ConvoRow = ([String; 3], [Option<i64>; 13]);
static CONVO_ROWS: OnceLock<RwLock<Vec<ConvoRow>>> = OnceLock::new();

fn convo_rows() -> &'static RwLock<Vec<ConvoRow>> {
    CONVO_ROWS.get_or_init(|| RwLock::new(Vec::new()))
}

/// The conversation stores as the board just sampled them.
pub fn set_convo(status: &crate::convo::board::Status) {
    let n = |v: u64| Some(i64::try_from(v).unwrap_or(i64::MAX));
    let rows = status
        .stores
        .iter()
        .filter(|s| s.id.is_some())
        .map(|s| {
            let r = s.report.as_ref();
            let behind = s.feed.as_ref().and_then(|f| f.behind).or(s.mirror.as_ref().and_then(|m| m.behind));
            let (batches, bytes) = match (&s.feed, &s.mirror) {
                (Some(f), _) => (f.intake.as_ref().map(|i| i.batches), f.intake.as_ref().map(|i| i.bytes)),
                (None, Some(m)) => (Some(m.ship.batches), Some(m.ship.bytes)),
                // The server's: what it took from every feed.
                (None, None) => {
                    let feeds = status.stores.iter().filter_map(|x| x.feed.as_ref()?.intake.as_ref());
                    let (b, w) = feeds.fold((0, 0), |(b, w), i| (b + i.batches, w + i.bytes));
                    (Some(b), Some(w))
                }
            };
            (
                [s.key.clone(), s.role.to_owned(), s.machine.clone()],
                [
                    Some(i64::from(s.connected)),
                    r.and_then(|r| n(r.file.bytes)),
                    r.and_then(|r| n(r.file.free)),
                    r.and_then(|r| n(r.file.wal)),
                    r.and_then(|r| n(r.file.top)),
                    r.and_then(|r| r.file.sessions).and_then(n),
                    behind.and_then(n),
                    r.and_then(|r| n(r.commits)),
                    r.and_then(|r| n(r.busy_ms)),
                    r.and_then(|r| n(r.dropped)),
                    r.and_then(|r| n(r.timeouts)),
                    batches.and_then(n),
                    bytes.and_then(n),
                ],
            )
        })
        .collect();
    *convo_rows().write() = rows;
}

pub fn render() -> String {
    let m = global();
    let mut o = String::with_capacity(8192);
    let help = |o: &mut String, name: &str, kind: &str, h: &str| {
        let _ = writeln!(o, "# HELP {name} {h}\n# TYPE {name} {kind}");
    };
    macro_rules! scalar {
        ($name:literal, $kind:literal, $help:literal, $v:expr) => {{
            help(&mut o, $name, $kind, $help);
            let _ = writeln!(o, "{} {}", $name, $v);
        }};
    }
    scalar!("kiln_runs_started_total", "counter", "Pipeline runs created.", m.runs_started.get());
    scalar!("kiln_jobs_queued", "gauge", "Jobs waiting for an agent.", m.jobs_queued.get());
    scalar!("kiln_jobs_running", "gauge", "Jobs currently executing.", m.jobs_running.get());
    scalar!("kiln_agents_connected", "gauge", "Agents with a live session.", m.agents_connected.get());
    scalar!("kiln_agent_slots_total", "gauge", "Job slots offered by agents.", m.agent_slots_total.get());
    scalar!("kiln_agent_slots_busy", "gauge", "Job slots in use.", m.agent_slots_busy.get());
    scalar!("kiln_log_bytes_total", "counter", "Bytes of job log ingested.", m.log_bytes.get());
    scalar!("kiln_github_rate_remaining", "gauge", "GitHub API requests remaining.", m.gitops_rate_remaining.get());
    scalar!("kiln_github_rate_limit", "gauge", "GitHub API request quota per window.", m.gitops_rate_limit.get());
    scalar!("kiln_terminal_sessions", "gauge", "Open PTY sessions.", m.terminal_sessions.get());
    scalar!("kiln_ws_clients", "gauge", "Connected UI websocket clients.", m.ws_clients.get());
    scalar!("kiln_supervisor_restarts_total", "counter", "Supervised subsystem restarts.", m.supervisor_restarts.get());
    scalar!("kiln_panics_caught_total", "counter", "Panics contained at task boundaries.", m.panics_caught.get());
    scalar!("kiln_quic_streams_total", "counter", "QUIC streams accepted.", m.quic_streams.get());

    help(&mut o, "kiln_jobs_finished_total", "counter", "Finished jobs.");
    m.jobs_finished.render(&mut o, "kiln_jobs_finished_total");
    help(&mut o, "kiln_steps_finished_total", "counter", "Finished steps.");
    m.steps_finished.render(&mut o, "kiln_steps_finished_total");
    help(&mut o, "kiln_gitops_polls_total", "counter", "Repository polls by result.");
    m.gitops_polls.render(&mut o, "kiln_gitops_polls_total");
    help(&mut o, "kiln_k8s_pods_total", "counter", "Agent pods launched.");
    m.k8s_pods_launched.render(&mut o, "kiln_k8s_pods_total");
    help(&mut o, "kiln_runner_up", "gauge", "Runners connected (1) or not (0).");
    m.runner_up.render(&mut o, "kiln_runner_up");
    help(&mut o, "kiln_runner_behind", "gauge", "Runners behind the newest kiln (1) or not (0).");
    m.runner_behind.render(&mut o, "kiln_runner_behind");
    help(&mut o, "kiln_runner_update_policy", "gauge", "Each runner's update policy: auto, notify or off (1).");
    m.runner_update_policy.render(&mut o, "kiln_runner_update_policy");
    help(&mut o, "kiln_runner_version_info", "gauge", "The kiln each runner runs: its commit, else its build (1).");
    m.runner_version_info.render(&mut o, "kiln_runner_version_info");
    help(&mut o, "kiln_ci_cache_total", "counter", "CI cache lookups (hit, miss) and saves.");
    m.ci_cache.render(&mut o, "kiln_ci_cache_total");
    help(&mut o, "kiln_ci_attempts_total", "counter", "CI leg attempts by conclusion and failure class.");
    m.ci_attempts.render(&mut o, "kiln_ci_attempts_total");
    help(&mut o, "kiln_ci_wait_seconds_total", "counter", "Seconds CI legs waited, by wait reason.");
    m.ci_wait_seconds.render(&mut o, "kiln_ci_wait_seconds_total");
    scalar!("kiln_ci_rollup_lag_seconds", "gauge", "How far behind the CI insights rollups are.", m.ci_rollup_lag.get());
    help(&mut o, "kiln_job_duration_seconds", "histogram", "Job wall time.");
    m.job_duration.render(&mut o, "kiln_job_duration_seconds", "");
    help(&mut o, "kiln_queue_wait_seconds", "histogram", "Time from queued to started.");
    m.queue_wait.render(&mut o, "kiln_queue_wait_seconds", "");
    render_sync(&mut o);
    render_blobs(&mut o);
    render_bus(&mut o);

    let samples = agent_samples().read();
    if !samples.is_empty() {
        for (name, kind, h) in [
            ("kiln_agent_cpu_percent", "gauge", "Agent host CPU utilisation."),
            ("kiln_agent_load1", "gauge", "Agent host 1m load."),
            ("kiln_agent_memory_used_bytes", "gauge", "Agent host memory in use."),
            ("kiln_agent_memory_total_bytes", "gauge", "Agent host memory."),
            ("kiln_agent_disk_used_bytes", "gauge", "Agent work volume used."),
            ("kiln_agent_disk_total_bytes", "gauge", "Agent work volume size."),
            ("kiln_agent_net_rx_bytes_total", "counter", "Agent host bytes received."),
            ("kiln_agent_net_tx_bytes_total", "counter", "Agent host bytes sent."),
        ] {
            help(&mut o, name, kind, h);
            for (agent, s) in samples.iter() {
                let v: f64 = match name {
                    "kiln_agent_cpu_percent" => f64::from(s.cpu_pct),
                    "kiln_agent_load1" => f64::from(s.load1),
                    #[allow(clippy::cast_precision_loss)]
                    "kiln_agent_memory_used_bytes" => s.mem_used as f64,
                    #[allow(clippy::cast_precision_loss)]
                    "kiln_agent_memory_total_bytes" => s.mem_total as f64,
                    #[allow(clippy::cast_precision_loss)]
                    "kiln_agent_disk_used_bytes" => s.disk_used as f64,
                    #[allow(clippy::cast_precision_loss)]
                    "kiln_agent_disk_total_bytes" => s.disk_total as f64,
                    #[allow(clippy::cast_precision_loss)]
                    "kiln_agent_net_rx_bytes_total" => s.net_rx as f64,
                    #[allow(clippy::cast_precision_loss)]
                    _ => s.net_tx as f64,
                };
                let _ = writeln!(o, "{name}{{agent=\"{}\"}} {v}", escape(agent));
            }
        }
    }
    let convo = convo_rows().read();
    if !convo.is_empty() {
        for (i, (name, kind, h)) in CONVO.iter().enumerate() {
            help(&mut o, name, kind, h);
            for (l, values) in convo.iter() {
                if let Some(v) = values.get(i).copied().flatten() {
                    let _ = writeln!(o, "{name}{{{}}} {v}", labels(&["store", "role", "machine"], l));
                }
            }
        }
    }
    drop(convo);
    let p = crate::host::process_self();
    scalar!("process_resident_memory_bytes", "gauge", "Server RSS.", p.rss_bytes);
    scalar!("process_cpu_seconds_total", "counter", "Server CPU time.", p.cpu_seconds);
    scalar!("process_open_fds", "gauge", "Open file descriptors.", p.open_fds);
    o
}
