//! What bears on how fast a run would finish on a runner (W05-R26, W13-R01; docs/needs.md, "Pace"):
//! its cores' speed, the contention it measures now, its clocks, its work device and its power.
//! Pure, as the rest of kiln-ci: the runner reads its machine and the server keeps its history;
//! this turns what they pass in into the three integers a runner's word carries ([`Pace`]).
//!
//! - [`speed`]: per-core speed, from kiln's own run history: the same kind of work on different
//!   machines, a two-way least-squares fit, shrunk toward the fleet by empirical Bayes.
//! - [`slow`]: contention now as slowness: a queueing form (processor sharing's time dilation)
//!   whose weights are learned from the runner's own runs against the signals at their start; a
//!   thermal term turns the cores' clocks now into speed now.
//! - [`device`]: the work device's strain and its rate under its load now, where the kernel keeps
//!   no stall times, from Little's law and M/M/1, calibrated on the device's own history.
//! - [`power`]: what a runner on battery does with others' work, a person's setting.
//!
//! Every number here is measured, follows from a measurement by a cited result, or is a person's
//! setting (docs/needs.md §21): nothing is a threshold of kiln's own.

pub mod device;
pub mod power;
pub mod slow;
pub mod speed;

use serde::{Deserialize, Serialize};

/// The unit of [`Pace::speed`] and [`Pace::slowness`]: 1/1024 (the load-balancing kernel's
/// `SPEED_ONE`, docs/lb.md "Rank").
pub const ONE: u32 = 1024;

/// What a runner says of its pace in its word to owners (docs/lb.md "Rank"). Each is 0 when it
/// isn't measured, and an owner from before reads none of it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct Pace {
    /// Per-core speed now, in 1/1024 of the fleet's reference core's: the fleet's fit
    /// ([`speed::Fit`]) times what the cores' clocks now say against their usual
    /// ([`slow::Weights::speed_now`]).
    #[serde(skip_serializing_if = "is_zero")]
    pub speed: u32,
    /// Per-core slowness under the contention it measures now, in 1/1024 of uncontended (1024 or
    /// more once measured: contention never speeds a run up).
    #[serde(skip_serializing_if = "is_zero")]
    pub slowness: u32,
    /// The rate its work device would give a new run's I/O under its load now, bytes a second
    /// ([`device::rate`]).
    #[serde(skip_serializing_if = "is_zero_u64")]
    pub disk_bps: u64,
}

impl Pace {
    /// Nothing measured.
    pub fn is_unmeasured(&self) -> bool {
        *self == Pace::default()
    }
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

/// `x` (a factor against 1) in 1/1024, rounded; 0 for one that isn't a positive finite number,
/// the most a `u32` holds past that.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn in_ones(x: f64) -> u32 {
    let v = (x * f64::from(ONE)).round();
    if !(x.is_finite() && x > 0.0) {
        return 0;
    }
    if v >= f64::from(u32::MAX) { u32::MAX } else { (v as u32).max(1) }
}

/// A dense square matrix, row by row, read and written by position without indexing: a position
/// past it reads 0 and writes nothing.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Dense {
    pub n: usize,
    pub a: Vec<f64>,
}

impl Dense {
    pub fn zero(n: usize) -> Dense {
        Dense { n, a: vec![0.0; n.saturating_mul(n)] }
    }

    pub fn get(&self, i: usize, j: usize) -> f64 {
        if i >= self.n || j >= self.n {
            return 0.0;
        }
        self.a.get(i * self.n + j).copied().unwrap_or(0.0)
    }

    pub fn add(&mut self, i: usize, j: usize, x: f64) {
        if i >= self.n || j >= self.n {
            return;
        }
        let n = self.n;
        if let Some(v) = self.a.get_mut(i * n + j) {
            *v += x;
        }
    }

    pub fn set(&mut self, i: usize, j: usize, x: f64) {
        if i >= self.n || j >= self.n {
            return;
        }
        let n = self.n;
        if let Some(v) = self.a.get_mut(i * n + j) {
            *v = x;
        }
    }

    /// Its inverse, by Gauss-Jordan elimination with partial pivoting; none when it's singular: a
    /// pivot within its entries' own rounding (the largest entry times n times f64's ε).
    pub fn inverse(&self) -> Option<Dense> {
        let n = self.n;
        let scale = self.a.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        if n == 0 || !(scale.is_finite() && scale > 0.0) {
            return None;
        }
        let tiny = scale * f64::EPSILON * n as f64;
        let mut m = self.clone();
        let mut inv = Dense::zero(n);
        for i in 0..n {
            inv.set(i, i, 1.0);
        }
        for col in 0..n {
            let pivot = (col..n).max_by(|x, y| m.get(*x, col).abs().total_cmp(&m.get(*y, col).abs()))?;
            if m.get(pivot, col).abs() <= tiny {
                return None;
            }
            if pivot != col {
                for j in 0..n {
                    let (a, b) = (m.get(col, j), m.get(pivot, j));
                    m.set(col, j, b);
                    m.set(pivot, j, a);
                    let (a, b) = (inv.get(col, j), inv.get(pivot, j));
                    inv.set(col, j, b);
                    inv.set(pivot, j, a);
                }
            }
            let p = m.get(col, col);
            for j in 0..n {
                m.set(col, j, m.get(col, j) / p);
                inv.set(col, j, inv.get(col, j) / p);
            }
            for i in 0..n {
                if i == col {
                    continue;
                }
                let f = m.get(i, col);
                if f == 0.0 {
                    continue;
                }
                for j in 0..n {
                    m.add(i, j, -f * m.get(col, j));
                    inv.add(i, j, -f * inv.get(col, j));
                }
            }
        }
        Some(inv)
    }

    /// `self · x`.
    pub fn times(&self, x: &[f64]) -> Vec<f64> {
        (0..self.n).map(|i| (0..self.n).map(|j| self.get(i, j) * x.get(j).copied().unwrap_or(0.0)).sum()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1/1024 figure rounds, and is never 0 for a positive factor (0 says unmeasured).
    #[test]
    fn w05_r26_in_ones_rounds_and_keeps_zero_for_unmeasured() {
        assert_eq!(in_ones(1.0), 1024);
        assert_eq!(in_ones(0.5), 512);
        assert_eq!(in_ones(1e-9), 1);
        assert_eq!(in_ones(0.0), 0);
        assert_eq!(in_ones(f64::NAN), 0);
        assert_eq!(in_ones(-2.0), 0);
        assert_eq!(in_ones(1e12), u32::MAX);
    }

    /// An inverse times its matrix is the identity; a singular matrix has none.
    #[test]
    fn w05_r26_dense_inverse() -> Result<(), String> {
        let mut m = Dense::zero(3);
        for (i, row) in [[4.0, 1.0, 0.5], [1.0, 3.0, 0.2], [0.5, 0.2, 2.0]].iter().enumerate() {
            for (j, v) in row.iter().enumerate() {
                m.set(i, j, *v);
            }
        }
        let inv = m.inverse().ok_or("invertible")?;
        for i in 0..3 {
            let col: Vec<f64> = (0..3).map(|j| inv.get(j, i)).collect();
            let e = m.times(&col);
            for (j, v) in e.iter().enumerate() {
                let want = if i == j { 1.0 } else { 0.0 };
                assert!((v - want).abs() < 1e-12, "{i} {j} {v}");
            }
        }
        let mut s = Dense::zero(2);
        s.set(0, 0, 1.0);
        s.set(0, 1, 2.0);
        s.set(1, 0, 2.0);
        s.set(1, 1, 4.0);
        assert!(s.inverse().is_none());
        Ok(())
    }

    /// An unmeasured pace says nothing on the wire, and an owner from before reads a measured
    /// one's absence as unmeasured.
    #[test]
    fn w05_r26_pace_is_absent_when_unmeasured() -> Result<(), String> {
        assert_eq!(serde_json::to_string(&Pace::default()).map_err(|e| e.to_string())?, "{}");
        let p = Pace { speed: 1100, slowness: 1300, disk_bps: 0 };
        let j = serde_json::to_string(&p).map_err(|e| e.to_string())?;
        assert_eq!(j, r#"{"speed":1100,"slowness":1300}"#);
        let back: Pace = serde_json::from_str(&j).map_err(|e| e.to_string())?;
        assert_eq!(back, p);
        Ok(())
    }
}
