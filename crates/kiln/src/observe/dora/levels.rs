//! DORA's software delivery performance levels (`docs/observe.md` §10.7): the 2024 Accelerate
//! State of DevOps report's four clusters (p. 13; found by cluster analysis of some 39,000 answers),
//! each metric's range in each. A cluster is all four together, so a level here says which
//! cluster's range one metric falls in, nothing more.

use serde::Serialize;

use super::ledger::DAY_NS;

/// A cluster of the 2024 report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Elite,
    High,
    Medium,
    Low,
}

const HOUR_S: f64 = 3_600.0;
#[allow(clippy::cast_precision_loss)]
const DAY_S: f64 = (DAY_NS / 1_000_000_000) as f64;
const WEEK_S: f64 = 7.0 * DAY_S;
/// The report's month: a deployment "monthly", a lead time "a month".
const MONTH_S: f64 = 30.0 * DAY_S;

/// Change lead time (s): under a day (elite), a day to a week (high), a week to a month (medium), a
/// month to six months (low; past six months, low still).
pub fn lead_time(s: f64) -> Option<Level> {
    if !s.is_finite() || s < 0.0 {
        return None;
    }
    Some(if s < DAY_S {
        Level::Elite
    } else if s < WEEK_S {
        Level::High
    } else if s < MONTH_S {
        Level::Medium
    } else {
        Level::Low
    })
}

/// Deployment frequency (a day): on demand, several a day (elite); daily to weekly (high); weekly to
/// monthly (medium); monthly to every six months (low; less often, low still).
pub fn frequency(per_day: f64) -> Option<Level> {
    if !per_day.is_finite() || per_day < 0.0 {
        return None;
    }
    Some(if per_day > 1.0 {
        Level::Elite
    } else if per_day >= 1.0 / 7.0 {
        Level::High
    } else if per_day >= 1.0 / 30.0 {
        Level::Medium
    } else {
        Level::Low
    })
}

/// Failed deployment recovery time (s): under an hour (elite), under a day (high, and medium the
/// same), a week to a month (low). A day to a week is no cluster's: none.
pub fn recovery(s: f64) -> Option<Level> {
    if !s.is_finite() || s < 0.0 {
        return None;
    }
    if s < HOUR_S {
        Some(Level::Elite)
    } else if s < DAY_S {
        Some(Level::High)
    } else if s < WEEK_S {
        None
    } else {
        Some(Level::Low)
    }
}

/// Each cluster's change failure rate in the report (elite, high, medium, low): not ordered (its
/// medium cluster failed less often than its high one), so no rate is given a level.
pub const FAILURE_RATES: [(Level, f64); 4] = [(Level::Elite, 0.05), (Level::High, 0.20), (Level::Medium, 0.10), (Level::Low, 0.40)];

#[cfg(test)]
mod tests {
    use super::*;

    /// Each band's edges as the report draws them.
    #[test]
    fn levels_follow_the_reports_bands() {
        assert_eq!(lead_time(DAY_S - 1.0), Some(Level::Elite));
        assert_eq!(lead_time(DAY_S), Some(Level::High));
        assert_eq!(lead_time(WEEK_S), Some(Level::Medium));
        assert_eq!(lead_time(MONTH_S), Some(Level::Low));
        assert_eq!(lead_time(400.0 * DAY_S), Some(Level::Low));
        assert_eq!(lead_time(f64::NAN), None);
        assert_eq!(frequency(3.0), Some(Level::Elite));
        assert_eq!(frequency(1.0), Some(Level::High));
        assert_eq!(frequency(1.0 / 7.0), Some(Level::High));
        assert_eq!(frequency(1.0 / 8.0), Some(Level::Medium));
        assert_eq!(frequency(1.0 / 30.0), Some(Level::Medium));
        assert_eq!(frequency(1.0 / 31.0), Some(Level::Low));
        assert_eq!(frequency(0.0), Some(Level::Low));
        assert_eq!(recovery(59.0 * 60.0), Some(Level::Elite));
        assert_eq!(recovery(HOUR_S), Some(Level::High));
        assert_eq!(recovery(2.0 * DAY_S), None);
        assert_eq!(recovery(WEEK_S), Some(Level::Low));
    }
}
