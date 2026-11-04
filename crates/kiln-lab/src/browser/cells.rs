//! A job's samples made into its numbers (docs/browser-lab.md, "The three measures"; plan
//! 01a1a832e9cc steps 4 and 6), and rounds made into cells. Pure: everything comes in as data.
//!
//! - **Time**: each operation's, from its server stamp (streams, bursts) or its scheduled time
//!   (requests) to the page having it parsed.
//! - **Bytes**: the client link's frames both ways over fixed windows, each window's bytes over the
//!   operations that completed in it (windows with none are left out), percentiles across windows;
//!   and the mean, all bytes over all operations. Also the bytes above the transport (what the page
//!   was given), as a mean.
//! - **Server CPU**: the server's scope (with its origin's, for the reference servers) over fixed
//!   windows, less the background's rate measured just before with nobody attached, over the
//!   window's operations (every client's), percentiles across windows; and the mean.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// What the page noted for one operation (`probe.js`'s samples).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Sample {
    pub op: String,
    #[serde(default)]
    pub case: String,
    #[serde(default)]
    pub stamp_us: Option<u64>,
    #[serde(default)]
    pub sched_us: Option<u64>,
    pub at_us: u64,
    #[serde(default)]
    pub lat_us: Option<i64>,
    #[serde(default)]
    pub app_bytes: u64,
    /// A frame without a stamp: bytes, no operation.
    #[serde(default)]
    pub frame: bool,
    #[serde(default)]
    pub ok: Option<bool>,
}

impl Sample {
    pub fn is_op(&self) -> bool {
        !self.frame && self.lat_us.is_some()
    }
}

/// A link's counters at a time (`counters.jsonl`).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Tick {
    pub t_us: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// The server's CPU at a time (its scope's, its origin's), µs.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct CpuTick {
    pub t_us: u64,
    pub server_us: u64,
    pub origin_us: u64,
}

/// Nearest-rank quantile of sorted values.
pub fn quantile(sorted: &[f64], q: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted.get(rank - 1).copied()
}

/// The quantiles the bar asks for.
pub const QS: [(&str, f64); 3] = [("p50", 0.5), ("p99", 0.99), ("p999", 0.999)];

fn put_quantiles(out: &mut BTreeMap<String, f64>, name: &str, mut v: Vec<f64>) {
    v.retain(|x| x.is_finite());
    v.sort_by(f64::total_cmp);
    for (q, p) in QS {
        if let Some(x) = quantile(&v, p) {
            out.insert(format!("{name}.{q}"), x);
        }
    }
    #[allow(clippy::cast_precision_loss)]
    out.insert(format!("{name}.n"), v.len() as f64);
}

/// Per-window values: each window's `amount` over the operations whose times fall in it.
fn per_window(edges: &[(u64, f64)], op_times: &[u64]) -> Vec<f64> {
    let mut out = Vec::new();
    for w in edges.windows(2) {
        let (Some((t0, a0)), Some((t1, a1))) = (w.first(), w.get(1)) else { continue };
        let ops = op_times.iter().filter(|t| **t > *t0 && **t <= *t1).count();
        if ops > 0 {
            #[allow(clippy::cast_precision_loss)]
            out.push((a1 - a0) / ops as f64);
        }
    }
    out
}

/// What a job's measures came to: `time.p50_ms`, `bytes.p99`, `cpu.mean_us`, each with its `.n`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct JobNumbers {
    pub numbers: BTreeMap<String, f64>,
    /// Time samples (ms), kept for pooling and for the run's raw files.
    pub times_ms: Vec<f64>,
}

/// The numbers of one job: every client's samples and counters, and the server's CPU.
/// `bg_us_per_s` is the server's background CPU rate; operations before `from_us` (opening, a
/// snapshot's backlog) are left out.
pub fn job_numbers(clients: &[(Vec<Sample>, Vec<Tick>)], cpu: &[CpuTick], bg_us_per_s: f64, from_us: u64) -> JobNumbers {
    let mut n = BTreeMap::new();
    let mut times = Vec::new();
    let mut bytes_w = Vec::new();
    let (mut bytes_all, mut ops_all, mut app_all, mut failed) = (0u64, 0usize, 0u64, 0usize);
    let mut all_op_times = Vec::new();
    for (samples, ticks) in clients {
        let ops: Vec<&Sample> = samples.iter().filter(|s| s.is_op() && s.at_us > from_us).collect();
        failed += ops.iter().filter(|s| s.ok == Some(false)).count();
        #[allow(clippy::cast_precision_loss)]
        times.extend(ops.iter().filter(|s| s.ok != Some(false)).filter_map(|s| s.lat_us).map(|l| l as f64 / 1000.0));
        app_all += samples.iter().filter(|s| s.at_us > from_us).map(|s| s.app_bytes).sum::<u64>();
        let op_times: Vec<u64> = ops.iter().map(|s| s.at_us).collect();
        let ticks: Vec<&Tick> = ticks.iter().filter(|t| t.t_us >= from_us).collect();
        #[allow(clippy::cast_precision_loss)]
        let edges: Vec<(u64, f64)> = ticks.iter().map(|t| (t.t_us, (t.rx_bytes + t.tx_bytes) as f64)).collect();
        bytes_w.extend(per_window(&edges, &op_times));
        if let (Some(f), Some(l)) = (ticks.first(), ticks.last()) {
            bytes_all += (l.rx_bytes + l.tx_bytes).saturating_sub(f.rx_bytes + f.tx_bytes);
        }
        ops_all += op_times.len();
        all_op_times.extend(op_times);
    }
    put_quantiles(&mut n, "time", times.clone());
    let mut sorted = times.clone();
    sorted.sort_by(f64::total_cmp);
    if let Some((lo, hi)) = quantile_band(sorted.len(), 0.999) {
        n.extend(sorted.get(lo - 1).map(|v| ("time.p999_band_lo_ms".to_owned(), *v)));
        n.extend(sorted.get(hi - 1).map(|v| ("time.p999_band_hi_ms".to_owned(), *v)));
    }
    for k in ["time.p50", "time.p99", "time.p999"] {
        if let Some(v) = n.remove(k) {
            n.insert(format!("{k}_ms"), v);
        }
    }
    put_quantiles(&mut n, "bytes", bytes_w);
    #[allow(clippy::cast_precision_loss)]
    let ops_f = ops_all.max(1) as f64;
    #[allow(clippy::cast_precision_loss)]
    {
        n.insert("bytes.mean".into(), bytes_all as f64 / ops_f);
        n.insert("app_bytes.mean".into(), app_all as f64 / ops_f);
        n.insert("ops".into(), ops_all as f64);
        n.insert("failed".into(), failed as f64);
    }
    // CPU: the server's and its origin's, each window less the background's share.
    let cpu: Vec<&CpuTick> = cpu.iter().filter(|c| c.t_us >= from_us).collect();
    #[allow(clippy::cast_precision_loss)]
    let edges: Vec<(u64, f64)> = cpu.iter().map(|c| (c.t_us, (c.server_us + c.origin_us) as f64 - bg_us_per_s * c.t_us as f64 / 1e6)).collect();
    put_quantiles(&mut n, "cpu_us", per_window(&edges, &all_op_times));
    if let (Some(f), Some(l)) = (cpu.first(), cpu.last()) {
        #[allow(clippy::cast_precision_loss)]
        let secs = l.t_us.saturating_sub(f.t_us) as f64 / 1e6;
        #[allow(clippy::cast_precision_loss)]
        let total = (l.server_us + l.origin_us).saturating_sub(f.server_us + f.origin_us) as f64 - bg_us_per_s * secs;
        #[allow(clippy::cast_precision_loss)]
        let origin = l.origin_us.saturating_sub(f.origin_us) as f64;
        n.insert("cpu_us.mean".into(), total / ops_f);
        n.insert("cpu_origin_us.mean".into(), origin / ops_f);
        n.insert("window_s".into(), secs);
    }
    JobNumbers { numbers: n, times_ms: times }
}

/// The ranks (1-based, from the bottom) of a 95% interval for the `q` quantile of `n` samples:
/// Binomial(n, q) by the normal approximation, the band a p99.9 carries within a round (f36).
pub fn quantile_band(n: usize, q: f64) -> Option<(usize, usize)> {
    if n == 0 {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let nf = n as f64;
    let sd = (nf * q * (1.0 - q)).sqrt();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lo = (nf * q - 1.96 * sd).floor().max(1.0) as usize;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let hi = (nf * q + 1.96 * sd).ceil() as usize + 1;
    // Past the top sample nothing bounds it from above.
    (hi <= n).then_some((lo, hi))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(at: u64, lat_ms: i64) -> Sample {
        Sample { op: "x".into(), case: "stream".into(), at_us: at, lat_us: Some(lat_ms * 1000), app_bytes: 100, ..Sample::default() }
    }

    /// plan 01a1a832e9cc step 4: a window's bytes and CPU go to the operations that completed in it,
    /// the background's CPU taken away first; time is each operation's own.
    #[test]
    fn g1_s4_bytes_and_cpu_go_to_their_windows() {
        let samples = vec![op(1_500_000, 10), op(1_600_000, 20), op(2_500_000, 30)];
        let ticks = vec![Tick { t_us: 1_000_000, rx_bytes: 0, tx_bytes: 0 }, Tick { t_us: 2_000_000, rx_bytes: 1500, tx_bytes: 500 }, Tick { t_us: 3_000_000, rx_bytes: 2500, tx_bytes: 600 }];
        let cpu = vec![CpuTick { t_us: 1_000_000, server_us: 1000, origin_us: 0 }, CpuTick { t_us: 2_000_000, server_us: 3000, origin_us: 0 }, CpuTick { t_us: 3_000_000, server_us: 4500, origin_us: 0 }];
        let j = job_numbers(&[(samples, ticks)], &cpu, 1000.0, 0);
        let n = &j.numbers;
        assert_eq!(n.get("time.p50_ms"), Some(&20.0));
        assert_eq!(n.get("time.n"), Some(&3.0));
        // Windows: 2,000 bytes over 2 ops, then 1,100 over 1.
        assert_eq!(n.get("bytes.p50"), Some(&1000.0));
        assert_eq!(n.get("bytes.p99"), Some(&1100.0));
        assert!((n.get("bytes.mean").copied().unwrap_or(0.0) - 3100.0 / 3.0).abs() < 1e-9);
        // CPU: (2,000 - 1,000) over 2 = 500, then (1,500 - 1,000) over 1 = 500.
        assert_eq!(n.get("cpu_us.p50"), Some(&500.0));
        assert!((n.get("cpu_us.mean").copied().unwrap_or(0.0) - 500.0).abs() < 1e-9);
    }

    /// A request that waited behind a stall counts its whole wait from when it was due (no
    /// coordinated omission): the page notes `at - sched`, and nothing here shortens it.
    #[test]
    fn g1_s4_a_stalled_requests_wait_counts_from_its_schedule() {
        let mut samples: Vec<Sample> = (0..10).map(|i| Sample { sched_us: Some(i * 100_000), at_us: i * 100_000 + 5_000, lat_us: Some(5_000), ..op(0, 0) }).collect();
        // Requests 3 to 5 were held behind one answered 2 s late.
        for (i, s) in samples.iter_mut().enumerate().skip(3).take(3) {
            let due = i as u64 * 100_000;
            s.at_us = 300_000 + 2_000_000;
            s.lat_us = Some(i64::try_from(s.at_us - due).unwrap_or(0));
        }
        let j = job_numbers(&[(samples, Vec::new())], &[], 0.0, 0);
        assert!(j.numbers.get("time.p99_ms").copied().unwrap_or(0.0) >= 2000.0, "{:?}", j.numbers);
    }

    #[test]
    fn g1_s6_a_p999_band_needs_enough_samples() {
        assert_eq!(quantile_band(1000, 0.999), None);
        let (lo, hi) = quantile_band(10_000, 0.999).unwrap_or((0, 0));
        assert!(lo >= 9_980 && hi <= 10_000 && lo < hi, "{lo} {hi}");
    }
}
