//! The work device's strain and its rate under its load now (W05-R26, W13-R01; docs/needs.md
//! "Pace"), from what every OS counts of it over an interval: its requests' mean latency R and
//! their mean number in flight N (`sense::disk::Activity`), which Little's law ties to its
//! throughput X: N = X·R (Little, "A proof for the queuing formula L = λW", Operations Research
//! 9(3), 1961).
//!
//! - **Its uncontended service time S₀, from its own history.** M/M/1 has R = S / (1 − ρ) and
//!   N = ρ / (1 − ρ) (Kleinrock, *Queueing Systems* vol. 1, Wiley 1975, §3.2), so R = S (1 + N):
//!   S₀ is that line's least-squares slope through the origin over every interval measured, each
//!   weighted by its requests (its R is the mean of that many latencies),
//!   S₀ = Σ w R (1 + N) / Σ w (1 + N)²: two sums, folded in as each interval is ([`Calibration`]).
//! - **Its utilization now:** ρ = 1 − S₀ / R, M/M/1's R = S / (1 − ρ) read the other way: a
//!   device's latency rises with its queue as its load does, so one that thrashes is one whose
//!   latency is a large multiple of its own uncontended. Where the kernel keeps no I/O stall times
//!   (macOS, Windows) this is the I/O signal of `super::slow`, and strain where Linux's stall
//!   time would be.
//! - **Its rate for a new run:** the capacity it has left, the most it was seen to move times
//!   1 − ρ (the utilization law, U = X·S: Denning & Buzen, ACM Computing Surveys 10(3), 1978: busy
//!   ρ of its time, it can serve 1 − ρ of its capacity more).
//! - **Swap's share of time:** swap-ins a second times S₀ (the same law): the share of time the
//!   device served a swap-in that some task waited for, as a stall on memory.

use serde::{Deserialize, Serialize};

/// The sums [`Calibration::service_us`] fits S₀ from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Calibration {
    /// Σ w R (1 + N), µs.
    pub rn: f64,
    /// Σ w (1 + N)².
    pub nn: f64,
}

impl Calibration {
    /// An interval of `ops` requests, their mean latency `latency_us` and their mean number in
    /// flight `in_flight`.
    pub fn add(&mut self, latency_us: f64, in_flight: f64, ops: f64) {
        if !(latency_us.is_finite() && latency_us > 0.0 && in_flight.is_finite() && in_flight >= 0.0 && ops.is_finite() && ops > 0.0) {
            return;
        }
        let q = 1.0 + in_flight;
        self.rn += ops * latency_us * q;
        self.nn += ops * q * q;
    }

    /// S₀, µs; none before an interval with requests.
    pub fn service_us(&self) -> Option<f64> {
        let s = self.rn / self.nn;
        (self.nn > 0.0 && s.is_finite() && s > 0.0).then_some(s)
    }
}

/// ρ now, in [0, 1): 1 − S₀ / R.
pub fn utilization(service_us: f64, latency_us: f64) -> Option<f64> {
    (service_us.is_finite() && service_us > 0.0 && latency_us.is_finite() && latency_us > 0.0).then(|| (1.0 - service_us / latency_us).clamp(0.0, 1.0 - f64::EPSILON))
}

/// The rate a new run's I/O gets now, bytes a second: `peak_bps` × (1 − ρ); 0 unmeasured.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn rate(peak_bps: f64, rho: f64) -> u64 {
    let r = (peak_bps * (1.0 - rho.clamp(0.0, 1.0))).round();
    if r.is_finite() && r >= 1.0 { if r >= u64::MAX as f64 { u64::MAX } else { r as u64 } } else { 0 }
}

/// Swap-ins' share of time: `per_s` swap-ins a second times the device's service time.
pub fn swap_share(per_s: f64, service_us: f64) -> Option<f64> {
    (per_s.is_finite() && per_s >= 0.0 && service_us.is_finite() && service_us > 0.0).then(|| (per_s * service_us / 1e6).clamp(0.0, 1.0 - f64::EPSILON))
}

/// The mean latency of an interval's reads and writes together, weighted by their counts, µs.
pub fn mean_latency(read_us: Option<f64>, read_ops: f64, write_us: Option<f64>, write_ops: f64) -> Option<f64> {
    let parts = [(read_us, read_ops), (write_us, write_ops)];
    let (sum, ops) = parts.iter().filter_map(|(l, n)| l.filter(|l| l.is_finite() && *l > 0.0).map(|l| (l * n, *n))).filter(|(_, n)| n.is_finite() && *n > 0.0).fold((0.0, 0.0), |(s, o), (a, b)| (s + a, o + b));
    (ops > 0.0).then(|| sum / ops)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Draws(u64);

    impl Draws {
        fn uniform(&mut self) -> f64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 11) as f64 + 0.5) / (1u64 << 53) as f64
        }
    }

    /// W05-R26: a device that is M/M/1 with service time 80 µs, seen at loads from idle to 90%:
    /// the calibration recovers 80 µs from its history alone, and its utilization now from its
    /// latency now. Planted: reading ρ as N / R or utilization off a latency picked as "slow" would
    /// miss both.
    #[test]
    fn w05_r26_calibration_recovers_mm1_service_time() -> Result<(), String> {
        let s = 80.0;
        let mut d = Draws(17);
        let mut c = Calibration::default();
        for _ in 0..200 {
            let rho: f64 = 0.9 * d.uniform();
            let (r, n) = (s / (1.0 - rho), rho / (1.0 - rho));
            c.add(r * (1.0 + 0.02 * (d.uniform() - 0.5)), n, 100.0 + 1000.0 * d.uniform());
        }
        let s0 = c.service_us().ok_or("calibrated")?;
        assert!((s0 / s - 1.0).abs() < 0.01, "{s0}");
        for rho in [0.0, 0.3, 0.75] {
            let u = utilization(s0, s / (1.0 - rho)).ok_or("ρ")?;
            assert!((u - rho).abs() < 0.01, "{rho} {u}");
        }
        // A latency under the service time (a cache's) is no load.
        assert_eq!(utilization(s0, 10.0), Some(0.0));
        Ok(())
    }

    /// W05-R26: the rate left for a new run is the capacity times 1 − ρ; swap's share is the
    /// utilization law's.
    #[test]
    fn w05_r26_rate_and_swap_share() {
        assert_eq!(rate(1e9, 0.25), 750_000_000);
        assert_eq!(rate(0.0, 0.0), 0);
        assert_eq!(rate(f64::NAN, 0.0), 0);
        assert!(matches!(swap_share(500.0, 100.0), Some(p) if (p - 0.05).abs() < 1e-12));
        assert_eq!(swap_share(1.0, 0.0), None);
        assert!(matches!(mean_latency(Some(100.0), 3.0, Some(200.0), 1.0), Some(l) if (l - 125.0).abs() < 1e-12));
        assert_eq!(mean_latency(None, 3.0, None, 1.0), None);
    }
}
