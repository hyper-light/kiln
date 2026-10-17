//! Schedules (docs/ci.md W03-R10): GitHub's five-field cron (minute, hour, day of month, month,
//! day of week; `*`, `,`, `-`, `/`, and the names `JAN`–`DEC`, `SUN`–`SAT`), in UTC; no `@daily`
//! and friends, as on GitHub. When both day fields are restricted a day matches if either does
//! (POSIX; a field starting with `*` counts as unrestricted).
//!
//! kiln's opt-in `timezone:` reads the schedule in local time, with DST handled: a local time
//! that doesn't exist (clocks forward) runs at the first minute that does, and one that happens
//! twice (clocks back) runs once, the first time. The tz database is built in, so every host
//! decides alike.

use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp};

/// A parsed schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    any_day: bool,
    any_weekday: bool,
}

const MONTHS: [&str; 12] = ["JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC"];
const DAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

fn value(s: &str, names: &[&str], base: u32) -> Option<u32> {
    if let Ok(n) = s.parse::<u32>() {
        return Some(n);
    }
    names.iter().position(|n| n.eq_ignore_ascii_case(s)).and_then(|i| u32::try_from(i).ok()).map(|i| i + base)
}

/// One field as a bit set over `lo..=hi`.
fn field(src: &str, lo: u32, hi: u32, names: &[&str], what: &str) -> Result<u64, String> {
    let base = lo;
    let mut bits = 0u64;
    for item in src.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().ok().filter(|s| *s > 0).ok_or_else(|| format!("`{item}` in the {what} field: the step must be a positive number"))?),
            None => (item, 1),
        };
        let (a, b) = if range == "*" {
            (lo, hi)
        } else if let Some((x, y)) = range.split_once('-') {
            let a = value(x, names, base).ok_or_else(|| format!("`{x}` is not a valid {what}"))?;
            let b = value(y, names, base).ok_or_else(|| format!("`{y}` is not a valid {what}"))?;
            (a, b)
        } else {
            let a = value(range, names, base).ok_or_else(|| format!("`{range}` is not a valid {what}"))?;
            // `a/n` runs from `a` to the end, as cron reads it.
            (a, if item.contains('/') { hi } else { a })
        };
        if a < lo || b > hi || a > b {
            return Err(format!("`{item}` is outside the {what} field's {lo}–{hi}"));
        }
        let mut v = a;
        while v <= b {
            bits |= 1u64 << v;
            v += step;
        }
    }
    Ok(bits)
}

impl Cron {
    pub fn parse(src: &str) -> Result<Cron, String> {
        let s = src.trim();
        if s.starts_with('@') {
            return Err(format!("`{s}`: GitHub's cron has five fields; `@` shorthands are not supported"));
        }
        let f: Vec<&str> = s.split_whitespace().collect();
        let [mi, h, dom, mon, dow] = f.as_slice() else { return Err(format!("`{s}` is not a cron schedule: five fields (minute hour day month weekday)")) };
        let weekdays = field(dow, 0, 7, &DAYS, "day of the week")?;
        // 7 is Sunday too.
        let weekdays = u8::try_from((weekdays | (weekdays >> 7)) & 0x7f).unwrap_or(0x7f);
        Ok(Cron {
            minutes: field(mi, 0, 59, &[], "minute")?,
            hours: u32::try_from(field(h, 0, 23, &[], "hour")?).unwrap_or(0),
            days: u32::try_from(field(dom, 1, 31, &[], "day of the month")?).unwrap_or(0),
            months: u16::try_from(field(mon, 1, 12, &MONTHS, "month")?).unwrap_or(0),
            weekdays,
            any_day: dom.starts_with('*'),
            any_weekday: dow.starts_with('*'),
        })
    }

    /// Whether a local date and time is one the schedule names.
    pub fn names(&self, dt: DateTime) -> bool {
        let bit32 = |set: u32, v: i8| u32::try_from(v).ok().is_some_and(|v| v < 32 && set & (1 << v) != 0);
        let minute = u32::try_from(dt.minute()).ok().is_some_and(|m| self.minutes & (1u64 << m) != 0);
        let month = u32::try_from(dt.month()).ok().is_some_and(|m| u32::from(self.months) & (1 << m) != 0);
        let dom = bit32(self.days, dt.day());
        let dow = u32::try_from(dt.weekday().to_sunday_zero_offset()).ok().is_some_and(|d| u32::from(self.weekdays) & (1 << d) != 0);
        let day = if self.any_day || self.any_weekday { dom && dow } else { dom || dow };
        minute && bit32(self.hours, dt.hour()) && month && day
    }

    /// Whether the schedule runs at the minute starting at `t` (unix seconds), read in `tz`.
    pub fn fires_at(&self, t: i64, tz: &TimeZone) -> bool {
        let Ok(ts) = Timestamp::from_second(t - t.rem_euclid(60)) else { return false };
        let local = ts.to_zoned(tz.clone()).datetime();
        if self.names(local) {
            // A local time that happens twice runs at its first instant only.
            return tz.to_ambiguous_timestamp(local).earlier().is_ok_and(|first| first == ts);
        }
        // The first minute after clocks go forward also runs what the gap skipped.
        let Ok(prev) = Timestamp::from_second(ts.as_second() - 60) else { return false };
        let before = prev.to_zoned(tz.clone()).datetime();
        let Ok(mut m) = before.checked_add(Span::new().minutes(1)) else { return false };
        let mut guard = 0;
        while m < local && guard < 24 * 60 {
            if self.names(m) {
                return true;
            }
            guard += 1;
            match m.checked_add(Span::new().minutes(1)) {
                Ok(n) => m = n,
                Err(_) => break,
            }
        }
        false
    }

    /// The minutes (unix seconds) in `(from, to]` at which it runs, at most `max` of them, looking
    /// at most 60 days ahead.
    pub fn fires_in(&self, from: i64, to: i64, tz: &TimeZone, max: usize) -> Vec<i64> {
        let mut out = Vec::new();
        let mut t = from - from.rem_euclid(60) + 60;
        let end = to.min(from.saturating_add(60 * 24 * 3600));
        while t <= end && out.len() < max {
            if self.fires_at(t, tz) {
                out.push(t);
            }
            t += 60;
        }
        out
    }
}

/// A time zone by IANA name (`Europe/Berlin`); `UTC` when `None`.
pub fn zone(name: Option<&str>) -> Result<TimeZone, String> {
    match name {
        None => Ok(TimeZone::UTC),
        Some(n) => TimeZone::get(n).map_err(|_| format!("`{n}` is not a time zone (an IANA name such as Europe/Berlin)")),
    }
}

/// The opt-in `jitter:` (W03-R10): a delay in `[0, max)` seconds that is the same for the same
/// seed (repository, workflow and schedule), so schedules spread out without moving around.
pub fn jitter(seed: &str, max: u64) -> u64 {
    use sha2::{Digest, Sha256};
    if max == 0 {
        return 0;
    }
    let d = Sha256::digest(seed.as_bytes());
    let mut b = [0u8; 8];
    for (i, x) in d.iter().take(8).enumerate() {
        if let Some(slot) = b.get_mut(i) {
            *slot = *x;
        }
    }
    u64::from_le_bytes(b) % max
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> i64 {
        s.parse::<Timestamp>().map(|t| t.as_second()).unwrap_or(0)
    }

    #[test]
    fn syntax() -> Result<(), String> {
        let c = Cron::parse("*/15 * * * *")?;
        let utc = TimeZone::UTC;
        assert!(c.fires_at(at("2026-01-01T10:45:00Z"), &utc) && !c.fires_at(at("2026-01-01T10:46:00Z"), &utc));
        let c = Cron::parse("0 9 * * MON-FRI")?;
        assert!(c.fires_at(at("2026-10-16T09:00:00Z"), &utc), "a Friday");
        assert!(!c.fires_at(at("2026-10-17T09:00:00Z"), &utc), "a Saturday");
        // Both day fields restricted: either matches.
        let c = Cron::parse("0 0 1 * MON")?;
        assert!(c.fires_at(at("2026-10-01T00:00:00Z"), &utc), "the 1st, a Thursday");
        assert!(c.fires_at(at("2026-10-05T00:00:00Z"), &utc), "a Monday");
        assert!(!c.fires_at(at("2026-10-06T00:00:00Z"), &utc));
        // 7 is Sunday; names and ranges and steps.
        assert!(Cron::parse("0 0 * * 7")?.fires_at(at("2026-10-18T00:00:00Z"), &utc));
        assert!(Cron::parse("5 4 * jan-mar,dec sun")?.fires_at(at("2026-12-06T04:05:00Z"), &utc));
        assert!(Cron::parse("10-20/5 * * * *")?.fires_at(at("2026-01-01T00:15:00Z"), &utc));
        for bad in ["@daily", "60 * * * *", "* * * *", "0 0 L * *", "0 0 * * 8", "*/0 * * * *", "0 0 32 * *", "0 5-3 * * *"] {
            assert!(Cron::parse(bad).is_err(), "{bad}");
        }
        Ok(())
    }

    /// W03-AC06: both 2026 DST transitions in Europe/Berlin and America/New_York.
    #[test]
    fn w03_ac06_daylight_saving() -> Result<(), String> {
        let c = Cron::parse("30 2 * * *")?;
        let berlin = zone(Some("Europe/Berlin"))?;
        // 29 March: 02:00 → 03:00. 02:30 doesn't exist: it runs at 03:00 local (01:00 UTC), once.
        let spring = c.fires_in(at("2026-03-28T23:00:00Z"), at("2026-03-29T23:00:00Z"), &berlin, 10);
        assert_eq!(spring, vec![at("2026-03-29T01:00:00Z")]);
        // 25 October: 03:00 → 02:00. 02:30 happens twice: it runs the first time only.
        let autumn = c.fires_in(at("2026-10-24T22:00:00Z"), at("2026-10-25T23:00:00Z"), &berlin, 10);
        assert_eq!(autumn, vec![at("2026-10-25T00:30:00Z")]);
        // An ordinary day: once, at 02:30 local.
        assert_eq!(c.fires_in(at("2026-06-01T00:00:00Z"), at("2026-06-02T00:00:00Z"), &berlin, 10), vec![at("2026-06-01T00:30:00Z")]);
        let ny = zone(Some("America/New_York"))?;
        // 8 March: 02:00 → 03:00 EDT; 02:30 runs at 03:00 local (07:00 UTC).
        assert_eq!(c.fires_in(at("2026-03-08T04:00:00Z"), at("2026-03-09T04:00:00Z"), &ny, 10), vec![at("2026-03-08T07:00:00Z")]);
        // 1 November: 02:00 EDT → 01:00 EST; 01:30 happens twice and runs once, at 05:30 UTC.
        let c = Cron::parse("30 1 * * *")?;
        assert_eq!(c.fires_in(at("2026-11-01T04:00:00Z"), at("2026-11-02T04:00:00Z"), &ny, 10), vec![at("2026-11-01T05:30:00Z")]);
        assert!(zone(Some("Mars/Olympus")).is_err());
        Ok(())
    }

    #[test]
    fn jitter_is_stable_and_bounded() {
        let a = jitter("acme/app:ci.yml:0 3 * * *", 300);
        assert_eq!(a, jitter("acme/app:ci.yml:0 3 * * *", 300));
        assert!(a < 300);
        assert_eq!(jitter("x", 0), 0);
    }
}
