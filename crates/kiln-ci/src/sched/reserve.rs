//! Reservations (docs/ci.md W05-R12), opt-in: a pool or runners kept for matching legs during
//! windows (`[[ci.reservations]]`, or a pool's `reserve`):
//!
//! ```toml
//! [[ci.reservations]]
//! name = "nightly-bench"
//! runners = "custom.rack == bench"   # which runners (a constraint over their facts), or
//! pool = "fc-linux"                  # a pool's executors
//! for = "workflow == 'bench'"        # the legs it's for (as a routing rule's `match.job`)
//! cron = "0 2 * * *"                 # when each window starts (GitHub's cron, UTC)
//! duration = "2h"
//! timezone = "Europe/Berlin"         # optional: the cron read in local time
//! ```
//!
//! In a window, other legs aren't assigned there (they wait with reason `reservation`, until it
//! ends); legs already running finish.

use super::routing::{self, Constraint, LegRoute};
use crate::trigger::cron::{self, Cron, TimeZone};

/// A reservation, checked.
#[derive(Debug, Clone)]
pub struct Reservation {
    pub name: String,
    /// The runners it keeps: all of these hold (none: no runner by its facts).
    pub runners: Vec<(String, Constraint)>,
    /// Or a pool's executors.
    pub pool: Option<String>,
    /// The legs it's for.
    pub matches: String,
    pub cron: Cron,
    pub tz: TimeZone,
    pub duration_s: i64,
}

/// What a reservation's configuration says.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReservationSpec {
    pub name: String,
    pub runners: Option<String>,
    pub pool: Option<String>,
    #[serde(rename = "for")]
    pub matches: String,
    pub cron: String,
    pub duration: String,
    pub timezone: Option<String>,
}

impl ReservationSpec {
    /// The reservation, or what's wrong with it.
    pub fn reservation(&self) -> Result<Reservation, String> {
        let name = if self.name.trim().is_empty() { self.cron.clone() } else { self.name.trim().to_owned() };
        let runners = match self.runners.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
            Some(src) => vec![(src.to_owned(), Constraint::parse(src).map_err(|e| format!("reservation `{name}`: `runners`: {e}"))?)],
            None => Vec::new(),
        };
        let pool = self.pool.as_deref().map(str::trim).filter(|p| !p.is_empty()).map(str::to_owned);
        if runners.is_empty() == pool.is_none() {
            return Err(format!("reservation `{name}`: `runners` or `pool`, one of them"));
        }
        if self.matches.trim().is_empty() {
            return Err(format!("reservation `{name}`: `for`, the legs it keeps them for"));
        }
        let cron = Cron::parse(&self.cron).map_err(|e| format!("reservation `{name}`: `cron`: {e}"))?;
        let tz = cron::zone(self.timezone.as_deref()).map_err(|e| format!("reservation `{name}`: {e}"))?;
        let duration_s = cron::seconds(&self.duration).filter(|s| *s > 0).ok_or_else(|| format!("reservation `{name}`: `duration` is a length of time (`90m`, `2h`), not `{}`", self.duration))?;
        Ok(Reservation { name, runners, pool, matches: self.matches.trim().to_owned(), cron, tz, duration_s: i64::try_from(duration_s).unwrap_or(i64::MAX) })
    }
}

impl Reservation {
    /// When the window `now` (unix ms) is in ends, if it's in one.
    pub fn window_end(&self, now: i64) -> Option<i64> {
        let t = now.div_euclid(1000);
        // The latest start whose window still runs.
        let starts = self.cron.fires_in(t - self.duration_s, t, &self.tz, 100_000);
        starts.last().map(|s| (s + self.duration_s) * 1000).filter(|end| *end > now)
    }

    /// When the next window after `now` starts, looking a week ahead.
    pub fn next_start(&self, now: i64) -> Option<i64> {
        let t = now.div_euclid(1000);
        self.cron.fires_in(t, t + 7 * 86_400, &self.tz, 1).first().map(|s| s * 1000)
    }

    /// Whether it keeps this runner.
    pub fn holds(&self, view: &routing::RunnerView) -> bool {
        !self.runners.is_empty() && self.runners.iter().all(|(_, c)| c.eval(&view.facts, &view.all_labels()))
    }

    /// Whether a leg is one it's for.
    pub fn admits(&self, leg: &LegRoute) -> bool {
        routing::job_condition(&self.matches, leg)
    }
}
