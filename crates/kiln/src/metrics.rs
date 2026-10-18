//! Prometheus metrics with no allocation on the hot path: atomics for counters and gauges, fixed
//! bucket histograms, and labelled families keyed by a small string set. Host statistics are read
//! straight from `/proc` on demand (only when scraped or sampled), so idle cost is ~zero.

use std::collections::BTreeMap;
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

/// Seconds-scale histogram suited to build and step durations.
pub struct Histogram {
    bounds: &'static [f64],
    buckets: Vec<AtomicU64>,
    count: AtomicU64,
    sum_ms: AtomicU64,
}

pub const DURATION_BUCKETS: &[f64] =
    &[1.0, 5.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1200.0, 1800.0, 3600.0, 7200.0];
pub const QUEUE_BUCKETS: &[f64] = &[0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 300.0, 900.0];

impl Histogram {
    pub fn new(bounds: &'static [f64]) -> Self {
        Self {
            bounds,
            buckets: bounds.iter().map(|_| AtomicU64::new(0)).collect(),
            count: AtomicU64::new(0),
            sum_ms: AtomicU64::new(0),
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
        self.sum_ms.fetch_add((secs.max(0.0) * 1000.0) as u64, Ordering::Relaxed);
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
        let sum = self.sum_ms.load(Ordering::Relaxed) as f64 / 1000.0;
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
        ci_cache: CounterVec::new(&["result"]),
        ci_attempts: CounterVec::new(&["conclusion", "class"]),
        ci_wait_seconds: CounterVec::new(&["reason"]),
        ci_rollup_lag: Gauge::default(),
    })
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
    let p = crate::host::process_self();
    scalar!("process_resident_memory_bytes", "gauge", "Server RSS.", p.rss_bytes);
    scalar!("process_cpu_seconds_total", "counter", "Server CPU time.", p.cpu_seconds);
    scalar!("process_open_fds", "gauge", "Open file descriptors.", p.open_fds);
    o
}
