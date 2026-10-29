//! Indexes' daily quotas, as Datadog keeps them (`docs/observe.md` §8.10): the records each index
//! kept since its day began (its reset hour, 14:00 UTC unless said), counted on the server from the
//! writes it proposes (each write's items' rows, read from their heads alone); a warning said once
//! a day at its threshold, and at its quota the index over it until its day turns: what would go
//! into it then goes where records not kept go, on every node (the server tells them), a write or
//! two of records already on their way past it at the most. At a start, a day's count is read
//! from the store, a block at a time (a block reaching back past the day counted whole: a little
//! more, never less).

use std::sync::{Mutex, OnceLock, PoisonError};

use super::Kept;
use super::route::day_start;

/// An index's day, against its quota.
#[derive(Debug, Clone, PartialEq)]
pub struct Day {
    pub part: u16,
    pub name: String,
    pub limit: u64,
    /// The count its warning is said at, if it has one.
    pub warn_at: Option<u64>,
    /// Its day's hour and offset from UTC.
    pub reset: (i64, i64),
    /// When its day began (ms), and what it kept since.
    pub start: i64,
    pub count: u64,
    pub warned: bool,
    pub over: bool,
    /// Its day so far not yet read from the store (none was open when its quota was set): read once
    /// one is ([`Quotas::read_unread`]).
    pub unread: bool,
}

impl Day {
    /// When its next day begins (ms).
    pub fn next_reset(&self) -> i64 {
        self.start + 86_400_000
    }
}

/// What a count said: a warning, or a quota reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Said {
    Warned { name: String, count: u64, limit: u64 },
    Reached { name: String, limit: u64 },
}

/// Every index's day that has a quota.
#[derive(Debug, Default)]
pub struct Quotas {
    days: Vec<Day>,
}

impl Quotas {
    /// The quotas of `kept` from now: a day an index had already kept, kept (its count) while it is
    /// the same day and the same quota; one new, its count `counted` (its part's records since its
    /// day began, ns), or read later where no store is open yet (none).
    pub fn set(&mut self, kept: &[Kept], now_ms: i64, counted: Option<&dyn Fn(u16, i64) -> u64>) {
        let mut days = Vec::new();
        for k in kept {
            let Some(limit) = k.def.daily_limit else { continue };
            let reset = k.def.reset();
            let start = day_start(now_ms, reset.0, reset.1);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
            let warn_at = k.def.daily_limit_warning_threshold_percentage.map(|p| ((limit as f64) * p / 100.0).ceil() as u64);
            let day = match self.days.iter().find(|d| d.part == k.part && d.start == start) {
                Some(d) => Day { name: k.def.name.clone(), limit, warn_at, reset, over: d.count >= limit, warned: d.warned && warn_at.is_some_and(|w| d.count >= w), ..d.clone() },
                None => {
                    let count = counted.map_or(0, |c| c(k.part, start.saturating_mul(1_000_000)));
                    Day { part: k.part, name: k.def.name.clone(), limit, warn_at, reset, start, count, warned: warn_at.is_some_and(|w| count >= w), over: count >= limit, unread: counted.is_none() }
                }
            };
            days.push(day);
        }
        self.days = days;
    }

    /// A proposed write's records counted against their indexes' quotas: what that said.
    pub fn count(&mut self, entry: &[u8], now_ms: i64) -> Vec<Said> {
        let mut said = Vec::new();
        if self.days.is_empty() {
            return said;
        }
        let Ok(kiln_obs::command::Command::Write { batch, .. }) = kiln_obs::command::decode(entry) else { return said };
        let Ok(mut items) = kiln_obs::batch::Reader::new(batch) else { return said };
        self.tick(now_ms);
        while let Ok(Some(item)) = items.next_item() {
            let kiln_obs::batch::Item::Logs { part, rows, .. } = item else { continue };
            let Some(d) = self.days.iter_mut().find(|d| d.part == part) else { continue };
            d.count = d.count.saturating_add(u64::from(rows));
            if !d.warned && d.warn_at.is_some_and(|w| d.count >= w) && d.count < d.limit {
                d.warned = true;
                said.push(Said::Warned { name: d.name.clone(), count: d.count, limit: d.limit });
            }
            if !d.over && d.count >= d.limit {
                d.over = true;
                d.warned = true;
                said.push(Said::Reached { name: d.name.clone(), limit: d.limit });
            }
        }
        said
    }

    /// The days not yet read from the store, read (`counted`: a part's records since a time, ns),
    /// what was counted since kept where it is more; their warning and quota marked as passed where
    /// they are (said before the start, not again). Whether any was read.
    pub fn read_unread(&mut self, counted: &dyn Fn(u16, i64) -> u64) -> bool {
        let mut read = false;
        for d in self.days.iter_mut().filter(|d| d.unread) {
            d.count = d.count.max(counted(d.part, d.start.saturating_mul(1_000_000)));
            d.warned |= d.warn_at.is_some_and(|w| d.count >= w);
            d.over |= d.count >= d.limit;
            d.unread = false;
            read = true;
        }
        read
    }

    /// Days turned by `now_ms`: their quotas over again from nothing. Whether any did.
    pub fn tick(&mut self, now_ms: i64) -> bool {
        let mut turned = false;
        for d in &mut self.days {
            let start = day_start(now_ms, d.reset.0, d.reset.1);
            if start > d.start {
                (d.start, d.count, d.warned, d.over, d.unread) = (start, 0, false, false, false);
                turned = true;
            }
        }
        turned
    }

    /// The parts over their quotas now.
    pub fn over(&self) -> Vec<u16> {
        self.days.iter().filter(|d| d.over).map(|d| d.part).collect()
    }

    pub fn days(&self) -> &[Day] {
        &self.days
    }
}

/// The server's quotas, as its API reads them (the owner's thread counts).
pub fn global() -> &'static Mutex<Quotas> {
    static Q: OnceLock<Mutex<Quotas>> = OnceLock::new();
    Q.get_or_init(Mutex::default)
}

/// An index's day now, as the server counts it.
pub fn day_of(name: &str) -> Option<Day> {
    global().lock().unwrap_or_else(PoisonError::into_inner).days.iter().find(|d| d.name == name).cloned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::super::{IndexDef, Kept, Reset, main_def};
    use super::*;

    fn write(parts: &[(u16, u32)]) -> Vec<u8> {
        let mut b = kiln_obs::batch::Builder::new();
        for &(part, n) in parts {
            let mut raw = Vec::new();
            let mut prev = 0;
            for i in 0..n {
                let r = kiln_obs::logs::record::Record { ts: i64::from(i) * 1_000_000, message: "x".into(), ..Default::default() };
                kiln_obs::logs::record::put_row(&mut raw, &r, prev);
                prev = r.ts;
            }
            b.log_rows_in(part, &raw, n);
        }
        b.finish_write(1, 1, 0)
    }

    /// Counted from writes' rows, by part; the warning said once at its threshold, the quota once
    /// at its count, the index over it then; its day turned at its hour, from nothing again; a
    /// day's count kept when the indexes change but its quota stays.
    #[test]
    fn a_quota_counts_warns_stops_and_turns_with_its_day() {
        let h = 3_600_000;
        let day0 = 86_400_000 * 20_000;
        let limited = IndexDef {
            daily_limit: Some(100),
            daily_limit_warning_threshold_percentage: Some(80.0),
            daily_limit_reset: Some(Reset { reset_time: "06:00".into(), reset_utc_offset: "+00:00".into() }),
            ..main_def()
        };
        let kept = vec![Kept { def: limited, part: 0, created: 0, modified: 0 }, Kept { def: IndexDef { name: "other".into(), ..main_def() }, part: 1, created: 0, modified: 0 }];
        let mut q = Quotas::default();
        q.set(&kept, day0 + 10 * h, Some(&|_, _| 30));
        assert_eq!((q.days().len(), q.days()[0].count, q.days()[0].start), (1, 30, day0 + 6 * h), "a day's count read where it begins");
        assert!(q.count(&write(&[(0, 40), (1, 1000)]), day0 + 11 * h).is_empty(), "70: the other index's no quota's");
        assert_eq!(q.count(&write(&[(0, 15)]), day0 + 11 * h), [Said::Warned { name: "main".into(), count: 85, limit: 100 }]);
        assert!(q.count(&write(&[(0, 5)]), day0 + 11 * h).is_empty(), "warned once");
        assert_eq!(q.count(&write(&[(0, 20)]), day0 + 12 * h), [Said::Reached { name: "main".into(), limit: 100 }]);
        assert_eq!(q.over(), [0]);
        assert!(q.count(&write(&[(0, 20)]), day0 + 13 * h).is_empty(), "reached once");
        q.set(&kept, day0 + 14 * h, Some(&|_, _| 0));
        assert_eq!((q.days()[0].count, q.over()), (130, vec![0]), "the same day: its count kept, not read again");
        assert!(q.tick(day0 + 30 * h), "06:00 the next day");
        assert_eq!((q.days()[0].count, q.over()), (0, vec![]));
        // Set with no store open: read once one is, over its quota said already.
        let mut q = Quotas::default();
        q.set(&kept, day0 + 10 * h, None);
        assert!(q.days()[0].unread && q.count(&write(&[(0, 10)]), day0 + 10 * h).is_empty());
        assert!(q.read_unread(&|_, _| 120) && !q.read_unread(&|_, _| 0), "read once");
        assert_eq!((q.days()[0].count, q.over()), (120, vec![0]));
    }
}
