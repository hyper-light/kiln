//! RFC 6298's estimator ("Computing TCP's Retransmission Timer", 2011), for any deadline on an
//! answer that may come late (LB05, docs/protect.md "Deadlines"): how long past when an answer is
//! due to wait before taking its absence as failure. Its smoothed mean and mean deviation of what
//! was measured, and the bound on them, are the RFC's, which follow Jacobson's algorithm ("Congestion
//! Avoidance and Control", SIGCOMM 1988) and the gains Jacobson and Karels give; nothing is fitted
//! here. Every time is µs.

use serde::{Deserialize, Serialize};

/// §2.3: "The above SHOULD be computed using alpha=1/8 and beta=1/4 (as suggested in [JK88])".
const ALPHA: f64 = 1.0 / 8.0;
const BETA: f64 = 1.0 / 4.0;
/// §2.2: "RTO <- SRTT + max (G, K*RTTVAR) where K = 4".
const K: f64 = 4.0;
/// §2.1: before any measurement, "the sender SHOULD set RTO <- 1 second"; §2.4: "if it is less than
/// 1 second, then the RTO SHOULD be rounded up to 1 second", the RFC's conservative floor against a
/// spurious timeout (here: work stopped for a lateness the estimator hadn't yet seen).
const FLOOR_US: f64 = 1_000_000.0;

/// What was measured so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Rto {
    /// SRTT, once anything was measured.
    srtt_us: Option<f64>,
    /// RTTVAR.
    rttvar_us: f64,
}

impl Rto {
    /// A measurement `r_us` (§2.2 for the first, §2.3 after: RTTVAR from SRTT before SRTT moves).
    pub fn sample(&mut self, r_us: f64) {
        if !r_us.is_finite() || r_us < 0.0 {
            return;
        }
        match self.srtt_us {
            None => {
                self.srtt_us = Some(r_us);
                self.rttvar_us = r_us / 2.0;
            }
            Some(srtt) => {
                self.rttvar_us = (1.0 - BETA) * self.rttvar_us + BETA * (srtt - r_us).abs();
                self.srtt_us = Some((1.0 - ALPHA) * srtt + ALPHA * r_us);
            }
        }
    }

    /// The timeout (µs) on a clock of granularity `g_us` (§2: G): SRTT + max(G, K × RTTVAR), at
    /// least 1 s (§2.4); 1 s before anything was measured (§2.1).
    pub fn timeout_us(&self, g_us: f64) -> f64 {
        match self.srtt_us {
            None => FLOOR_US,
            Some(srtt) => (srtt + g_us.max(0.0).max(K * self.rttvar_us)).max(FLOOR_US),
        }
    }

    /// Whether anything was measured.
    pub fn measured(&self) -> bool {
        self.srtt_us.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-6
    }

    /// The RFC's arithmetic, step by step: each value would come out otherwise with K = 2, RTTVAR
    /// started at R instead of R/2, alpha and beta swapped, or SRTT updated before RTTVAR.
    #[test]
    fn lb05_rfc_6298_first_and_later_measurements() {
        let mut r = Rto::default();
        assert!(close(r.timeout_us(1.0), 1e6), "before any measurement: 1 s (§2.1)");
        r.sample(400_000.0);
        // SRTT 0.4 s, RTTVAR 0.2 s: 0.4 + 4 x 0.2 = 1.2 s.
        assert!(close(r.timeout_us(1.0), 1_200_000.0), "{}", r.timeout_us(1.0));
        r.sample(600_000.0);
        // RTTVAR 0.75 x 0.2 + 0.25 x |0.4 - 0.6| = 0.2; SRTT 0.875 x 0.4 + 0.125 x 0.6 = 0.425: 1.225 s.
        assert!(close(r.timeout_us(1.0), 1_225_000.0), "{}", r.timeout_us(1.0));
    }

    #[test]
    fn lb05_rfc_6298_floor_and_granularity() {
        let mut r = Rto::default();
        r.sample(1_000.0);
        assert!(close(r.timeout_us(1.0), 1e6), "under 1 s rounds up to 1 s (§2.4): {}", r.timeout_us(1.0));
        // Steady 2 s measurements: RTTVAR decays toward 0, and G bounds the margin from below.
        let mut r = Rto::default();
        for _ in 0..400 {
            r.sample(2_000_000.0);
        }
        assert!(close(r.timeout_us(500_000.0), 2_500_000.0), "{}", r.timeout_us(500_000.0));
        assert!(r.timeout_us(1.0) < 2_000_100.0, "{}", r.timeout_us(1.0));
        // What isn't a measurement changes nothing.
        let before = r;
        r.sample(f64::NAN);
        r.sample(-5.0);
        assert_eq!(r, before);
        assert!(r.measured() && !Rto::default().measured());
    }
}
