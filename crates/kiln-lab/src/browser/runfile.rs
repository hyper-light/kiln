//! A browser run's files (docs/browser-lab.md, "The run files"): its settings, each job's record
//! and the cases' paces, read by the report, the compare and the doc's own test on every platform,
//! written by the rounds on Linux (`run`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::link::{self, Profile};
use super::targets::Target;

/// What one case asks of one profile's link: how long, and the operations' paces.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Workload {
    pub case: String,
    pub duration_s: u64,
    pub tail_s: u64,
    /// Streams: an agent session's events a second, and a terminal's and a log's lines a second.
    pub session_per_s: f64,
    pub lines_per_s: f64,
    /// Bursts: an agent's turn of `burst_events`, `burst_every_ms` apart, every `burst_gap_s`.
    pub burst_events: u64,
    pub burst_every_ms: u64,
    pub burst_gap_s: u64,
    /// Requests: one every `request_ms`, open-loop, the catalogue's reads and the write in turn.
    pub request_ms: u64,
}

/// The cases' paces on a profile: about half the link at most, so the bar's numbers measure the
/// protocols rather than a link run full (the plan's run budget sizes `duration_s`).
pub fn workload(case: &str, p: &Profile) -> Workload {
    let slow = p.down.rate_kbit < 1000;
    let fifty = p.down.rate_kbit <= 50;
    let pick = |good: f64, hundred: f64, fifty_k: f64| if !slow { good } else if fifty { fifty_k } else { hundred };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Workload {
        case: case.to_owned(),
        duration_s: pick(60.0, 180.0, 180.0) as u64,
        tail_s: pick(3.0, 20.0, 30.0) as u64,
        session_per_s: pick(5.0, 0.5, 0.25),
        lines_per_s: pick(20.0, 2.0, 1.0),
        burst_events: 40,
        burst_every_ms: 25,
        burst_gap_s: pick(3.0, 20.0, 40.0) as u64,
        request_ms: pick(100.0, 2000.0, 4000.0) as u64,
    }
}

/// The run's settings.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    pub targets: Vec<Target>,
    pub profiles: Vec<Profile>,
    pub losses: Vec<link::Loss>,
    pub browsers: Vec<String>,
    pub cases: Vec<String>,
    pub rounds: usize,
    pub k: usize,
    pub seed: u64,
    /// Seconds each job runs, if not the workload's own.
    pub duration_s: Option<u64>,
    pub window_ms: u64,
    pub background_s: u64,
    pub kiln: Option<PathBuf>,
    pub h2o: Option<PathBuf>,
    pub caddy: Option<PathBuf>,
    pub payloads: Option<PathBuf>,
    pub web: PathBuf,
    /// A packet capture on each client link during each job (`tcpdump`, headers only), with the
    /// browser's TLS keys (`SSLKEYLOGFILE`) beside it: step 4's check of the counters, and goal 7's.
    #[serde(default)]
    pub capture: bool,
    /// The server stopped (SIGSTOP) for 2 s this many seconds into each request job: step 4's
    /// check that requests behind a stall count their whole wait.
    #[serde(default)]
    pub stall_s: Option<u64>,
    /// A job log's stream in the stream case. Off unless asked: kiln today's log needs its CI to
    /// take the lab job, which kiln's runner declines while the lab machine is loaded past its
    /// limit, and the stream case's mix must be the same for every target in a cell.
    #[serde(default)]
    pub logs: bool,
}

/// One job's record: a cell's draw for one round.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobRecord {
    pub condition: String,
    pub profile: String,
    pub loss: String,
    pub seed: u64,
    pub round: usize,
    pub target: String,
    pub browser: String,
    pub case: String,
    pub k: usize,
    pub numbers: std::collections::BTreeMap<String, f64>,
    /// The protocol each path came over, as the page saw it.
    pub protocols: std::collections::BTreeMap<String, String>,
    /// The calibration: its offset and the p99 of its residuals (µs), the worst client's.
    pub calibration_p99_us: Option<f64>,
    /// The page's timer grain (µs): the smallest step `performance.now()` took, the coarsest
    /// client's (5 µs in Chromium cross-origin isolated, 1,000 µs in WebKit). The calibration can
    /// resolve no finer, nor can the job's times.
    #[serde(default)]
    pub timer_grain_us: Option<f64>,
    /// The background's CPU rate (µs a second), the mean of a window before the job (once the
    /// server is at rest) and one after, and its noise: the larger of two windows' difference
    /// before and the before's and after's.
    pub background_us_per_s: f64,
    pub noise_us_per_s: f64,
    pub errors: Vec<String>,
    /// With a capture: the bytes it saw over the job's window, and the counters' over the same.
    #[serde(default)]
    pub capture: Option<(u64, u64)>,
    pub machine: Value,
    pub started_us: u64,
}

/// A run: its settings and its jobs, written as each ends.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrowserRun {
    pub command: String,
    pub started_us: u64,
    pub settings: Settings,
    pub certs_spki: String,
    pub jobs: Vec<JobRecord>,
    pub failures: Vec<String>,
    pub network: Vec<String>,
}

/// A seed of the round's own, the same for every target in it.
pub fn round_seed(base: u64, condition: &str, round: usize) -> u64 {
    let h = crate::stats::seed_of(&format!("{base}/{condition}/{round}"));
    // netem takes any 64-bit seed; keep clear of 0.
    h | 1
}

