//! Calendar periods (`docs/observe.md` §6): the days, weeks and months of
//! `.rollup(<agg>, daily|weekly|monthly[, <alignment>][, '<timezone>'])` and the shifts of
//! `calendar_shift()`, in a timezone of the tz database Kiln carries in its binary (jiff's bundled
//! copy) rather than the machine's own, so every machine of a kiln answers alike, Windows and
//! minimal containers without a zoneinfo included.

use std::sync::{Mutex, OnceLock, PoisonError};

use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::{TimeZone, TimeZoneDatabase};

const DAY_MS: i64 = 86_400_000;

/// A calendar's period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Daily,
    Weekly,
    Monthly,
}

const WEEKDAYS: [&str; 7] = ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"];

impl Unit {
    pub fn parse(s: &str) -> Option<Self> {
        [Self::Daily, Self::Weekly, Self::Monthly].into_iter().find(|u| u.as_str().eq_ignore_ascii_case(s))
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
        }
    }

    /// The period's usual length (ms): a day, a week, the Gregorian calendar's mean month (365.2425
    /// / 12 days); a calendar grid's step, where a function wants one.
    pub fn nominal_ms(self) -> i64 {
        match self {
            Self::Daily => DAY_MS,
            Self::Weekly => 7 * DAY_MS,
            Self::Monthly => 2_629_746_000,
        }
    }

    /// Datadog's: a day begins at midnight, a week on Sunday, a month on its 1st.
    pub fn default_alignment(self) -> u32 {
        match self {
            Self::Daily => 0,
            Self::Weekly => 6,
            Self::Monthly => 1,
        }
    }

    /// An alignment as [`Calendar::align`] holds it: for days a time of day (`12am`, `9:30am`,
    /// `9:30pm`, `21:30`); for weeks a day (`monday`, `mon`); for months a day of the month (`15`,
    /// `15th`, `1st`; 1 to 31).
    pub fn alignment(self, s: &str) -> Option<u32> {
        let s = s.trim();
        match self {
            Self::Daily => time_of_day(s),
            Self::Weekly => WEEKDAYS.iter().position(|d| d.eq_ignore_ascii_case(s) || (s.len() == 3 && d.get(..3).is_some_and(|p| p.eq_ignore_ascii_case(s)))).and_then(|i| u32::try_from(i).ok()),
            Self::Monthly => {
                let digits = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
                let (n, suffix) = s.split_at(digits);
                let n: u32 = n.parse().ok().filter(|n| (1..=31).contains(n))?;
                let ordinal = match (n % 10, n / 10 % 10) {
                    (_, 1) => "th",
                    (1, _) => "st",
                    (2, _) => "nd",
                    (3, _) => "rd",
                    _ => "th",
                };
                (suffix.is_empty() || suffix.eq_ignore_ascii_case(ordinal)).then_some(n)
            }
        }
    }
}

/// Seconds into the day of `12am`, `9am`, `9:30pm` (12am is midnight, 12pm noon), or of a 24-hour
/// `21:30`.
fn time_of_day(s: &str) -> Option<u32> {
    let half = |suffix: &str| s.len().checked_sub(2).and_then(|at| s.get(at..).filter(|t| t.eq_ignore_ascii_case(suffix)).and_then(|_| s.get(..at)));
    let (body, pm) = match (half("am"), half("pm")) {
        (Some(b), _) => (b, Some(false)),
        (_, Some(b)) => (b, Some(true)),
        _ => (s, None),
    };
    let (h, m) = match body.split_once(':') {
        Some((h, m)) => (h, Some(m)),
        None => (body, None),
    };
    let number = |d: &str, width: std::ops::RangeInclusive<usize>| -> Option<u32> { if width.contains(&d.len()) && d.bytes().all(|b| b.is_ascii_digit()) { d.parse().ok() } else { None } };
    let h = number(h, 1..=2)?;
    let m = match m {
        Some(m) => number(m, 2..=2).filter(|&m| m < 60)?,
        None => 0,
    };
    let h = match pm {
        Some(pm) if (1..=12).contains(&h) => h % 12 + if pm { 12 } else { 0 },
        Some(_) => return None,
        // 24-hour time takes its minutes (`21:30`, `9:00`), so a bare number is no time.
        None if body.contains(':') && h < 24 => h,
        None => return None,
    };
    Some(h * 3600 + m * 60)
}

/// A timezone, by its place in this process's list of those queries have named (0: UTC): a small
/// copyable handle, so a grid stays one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Zone(u16);

fn database() -> &'static TimeZoneDatabase {
    static DB: OnceLock<TimeZoneDatabase> = OnceLock::new();
    DB.get_or_init(TimeZoneDatabase::bundled)
}

/// The zones named, each once (as first spelled): no more than the database has names.
fn zones() -> &'static Mutex<Vec<(String, TimeZone)>> {
    static ZONES: OnceLock<Mutex<Vec<(String, TimeZone)>>> = OnceLock::new();
    ZONES.get_or_init(|| Mutex::new(vec![("UTC".to_owned(), TimeZone::UTC)]))
}

impl Zone {
    pub const UTC: Self = Self(0);

    /// An IANA name (any case: `America/New_York`, `europe/paris`), or UTC.
    pub fn named(name: &str) -> Result<Self, String> {
        let name = name.trim();
        if name.eq_ignore_ascii_case("utc") {
            return Ok(Self::UTC);
        }
        let mut zones = zones().lock().unwrap_or_else(PoisonError::into_inner);
        let i = match zones.iter().position(|(n, _)| n.eq_ignore_ascii_case(name)) {
            Some(i) => i,
            None => {
                let tz = database().get(name).map_err(|_| format!("no timezone {name:?}: an IANA name (America/New_York, Europe/Paris) or UTC"))?;
                zones.push((name.to_owned(), tz));
                zones.len() - 1
            }
        };
        u16::try_from(i).map(Self).map_err(|_| "too many timezones".to_owned())
    }

    pub fn tz(self) -> TimeZone {
        let zones = zones().lock().unwrap_or_else(PoisonError::into_inner);
        zones.get(usize::from(self.0)).map_or(TimeZone::UTC, |z| z.1.clone())
    }

    /// A wall-clock time in the zone, as nanoseconds since the epoch: one a change of offset makes
    /// twice, the earlier, and one it skips, as if the change came after it (jiff's "compatible",
    /// as RFC 5545 has it). None for a date that is none, or past the years jiff holds.
    #[allow(clippy::too_many_arguments)]
    pub fn civil_ns(self, year: i16, month: i8, day: i8, hour: i8, minute: i8, second: i8, nanos: i32) -> Option<i64> {
        let dt = DateTime::new(year, month, day, hour, minute, second, nanos).ok()?;
        let zoned = dt.to_zoned(self.tz()).ok()?;
        i64::try_from(zoned.timestamp().as_nanosecond()).ok()
    }

    /// The year it is in the zone at `ns` (nanoseconds since the epoch).
    pub fn year_at(self, ns: i64) -> Option<i16> {
        let t = Timestamp::from_nanosecond(i128::from(ns)).ok()?;
        Some(t.to_zoned(self.tz()).year())
    }
}

/// A calendar's periods: days begun at a time of day, weeks on a weekday, months on a day of the
/// month, in a timezone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Calendar {
    pub unit: Unit,
    /// Days: the second of the day one begins at; weeks: the day one begins on, Monday 0 to Sunday
    /// 6; months: the day of the month one begins on, 1 to 31 (a shorter month's last day where it
    /// has no such day).
    pub align: u32,
    pub zone: Zone,
}

impl Calendar {
    /// Period `k`'s start (ms): days and weeks counted from the epoch's, months from January 1970.
    /// None past the years jiff holds (±9999).
    pub fn start(self, tz: &TimeZone, k: i64) -> Option<i64> {
        let (days, secs) = match self.unit {
            Unit::Daily => (k, i64::from(self.align)),
            Unit::Weekly => (k.checked_mul(7)?.checked_add(week_shift(self.align))?, 0),
            Unit::Monthly => {
                let (y, m) = (1970 + k.div_euclid(12), k.rem_euclid(12) + 1);
                (days_from_civil(y, m, i64::from(self.align).min(days_in_month(y, m))), 0)
            }
        };
        local_ms(tz, days, secs, 0)
    }

    /// The period `t` (ms) is in: the `k` whose start is at or before it and whose next one's after.
    pub fn period(self, tz: &TimeZone, t: i64) -> Option<i64> {
        let local = tz.to_datetime(Timestamp::from_millisecond(t).ok()?);
        let (y, m, d) = (i64::from(local.year()), i64::from(local.month()), i64::from(local.day()));
        let day = days_from_civil(y, m, d);
        let secs = i64::from(local.hour()) * 3600 + i64::from(local.minute()) * 60 + i64::from(local.second());
        let mut k = match self.unit {
            Unit::Daily if secs < i64::from(self.align) => day - 1,
            Unit::Daily => day,
            Unit::Weekly => (day - week_shift(self.align)).div_euclid(7),
            Unit::Monthly => {
                let k = (y - 1970) * 12 + (m - 1);
                if d < i64::from(self.align).min(days_in_month(y, m)) { k - 1 } else { k }
            }
        };
        // Where a clock jumps, a local time and the instant disagree by the jump: step to the period
        // whose span holds `t` (one step: no jump is a period long).
        for _ in 0..4 {
            if self.start(tz, k)? > t {
                k -= 1;
            } else if self.start(tz, k + 1)? <= t {
                k += 1;
            } else {
                break;
            }
        }
        Some(k)
    }
}

/// Days from the epoch (a Thursday) to the first week's start on weekday `w` (Monday 0).
fn week_shift(w: u32) -> i64 {
    (i64::from(w) - 3).rem_euclid(7)
}

/// The instant (ms) of local day `days` (from the epoch's), `secs` and `ms` into it. A local time a
/// clock skips is the instant after the gap, one it repeats the first (jiff's "compatible", RFC
/// 5545's).
fn local_ms(tz: &TimeZone, days: i64, secs: i64, ms: i64) -> Option<i64> {
    let (y, m, d) = civil_from_days(days);
    let small = |v: i64| i8::try_from(v).ok();
    let dt = DateTime::new(i16::try_from(y).ok()?, small(m)?, small(d)?, small(secs / 3600)?, small(secs % 3600 / 60)?, small(secs % 60)?, 0).ok()?;
    Some(tz.to_ambiguous_timestamp(dt).compatible().ok()?.as_millisecond() + ms)
}

/// `calendar_shift`'s shift (ms, negative: earlier) of `to`: the same local time `days` days, or the
/// same local day and time `months` months (the day clamped to the month's last), before it, less
/// it. In a timezone with daylight saving a day back is 23 or 25 hours where the clocks change.
pub fn shift_back(tz: &TimeZone, to: i64, days: i64, months: i64) -> Option<i64> {
    let local = tz.to_datetime(Timestamp::from_millisecond(to).ok()?);
    let (y, m, d) = (i64::from(local.year()), i64::from(local.month()), i64::from(local.day()));
    let secs = i64::from(local.hour()) * 3600 + i64::from(local.minute()) * 60 + i64::from(local.second());
    let ms = to.rem_euclid(1000);
    let day = if months > 0 {
        let total = y * 12 + (m - 1) - months;
        let (ny, nm) = (total.div_euclid(12), total.rem_euclid(12) + 1);
        days_from_civil(ny, nm, d.min(days_in_month(ny, nm)))
    } else {
        days_from_civil(y, m, d) - days
    };
    Some(local_ms(tz, day, secs, ms)? - to)
}

/// Days from 1970-01-01 to a proleptic Gregorian date (Hinnant's `days_from_civil`).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// [`days_from_civil`]'s inverse: (year, month, day).
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3_600_000;

    fn utc(y: i64, m: i64, d: i64, h: i64) -> i64 {
        days_from_civil(y, m, d) * DAY_MS + h * HOUR
    }

    #[test]
    fn alignments_read_as_datadog_writes_them() {
        assert_eq!(Unit::Daily.alignment("12am"), Some(0));
        assert_eq!(Unit::Daily.alignment("12pm"), Some(12 * 3600));
        assert_eq!(Unit::Daily.alignment("9:30am"), Some(9 * 3600 + 1800));
        assert_eq!(Unit::Daily.alignment("9:30PM"), Some(21 * 3600 + 1800));
        assert_eq!(Unit::Daily.alignment("21:30"), Some(21 * 3600 + 1800));
        assert_eq!(Unit::Daily.alignment("0:00"), Some(0));
        for bad in ["13pm", "0am", "24:00", "9", "9:3", "9:60am", "noon", "", "am"] {
            assert_eq!(Unit::Daily.alignment(bad), None, "{bad}");
        }
        assert_eq!(Unit::Weekly.alignment("monday"), Some(0));
        assert_eq!(Unit::Weekly.alignment("Sunday"), Some(6));
        assert_eq!(Unit::Weekly.alignment("wed"), Some(2));
        assert_eq!(Unit::Weekly.alignment("we"), None);
        assert_eq!(Unit::Monthly.alignment("1st"), Some(1));
        assert_eq!(Unit::Monthly.alignment("22nd"), Some(22));
        assert_eq!(Unit::Monthly.alignment("13th"), Some(13));
        assert_eq!(Unit::Monthly.alignment("31"), Some(31));
        for bad in ["0", "32", "1th", "11st", "2rd", "x"] {
            assert_eq!(Unit::Monthly.alignment(bad), None, "{bad}");
        }
    }

    #[test]
    fn utc_periods_begin_where_the_calendar_says() {
        let tz = TimeZone::UTC;
        let day = Calendar { unit: Unit::Daily, align: 9 * 3600, zone: Zone::UTC };
        // 2026-10-18 08:00 is still in the day begun 2026-10-17 09:00.
        let k = day.period(&tz, utc(2026, 10, 18, 8)).unwrap();
        assert_eq!(day.start(&tz, k), Some(utc(2026, 10, 17, 9)));
        assert_eq!(day.start(&tz, k + 1), Some(utc(2026, 10, 18, 9)));
        // Weeks on Monday: 2026-10-18 is a Sunday, in the week begun Monday 2026-10-12.
        let week = Calendar { unit: Unit::Weekly, align: 0, zone: Zone::UTC };
        let k = week.period(&tz, utc(2026, 10, 18, 23)).unwrap();
        assert_eq!(week.start(&tz, k), Some(utc(2026, 10, 12, 0)));
        // Sunday's weeks (Datadog's default): that Sunday begins one.
        let sunday = Calendar { unit: Unit::Weekly, align: 6, zone: Zone::UTC };
        assert_eq!(sunday.start(&tz, sunday.period(&tz, utc(2026, 10, 18, 0)).unwrap()), Some(utc(2026, 10, 18, 0)));
        // Months on the 31st: February's begins on its last day.
        let month = Calendar { unit: Unit::Monthly, align: 31, zone: Zone::UTC };
        let k = month.period(&tz, utc(2026, 3, 1, 0)).unwrap();
        assert_eq!(month.start(&tz, k), Some(utc(2026, 2, 28, 0)));
        assert_eq!(month.start(&tz, k + 1), Some(utc(2026, 3, 31, 0)));
        // Every instant in a period maps back to it.
        for c in [day, week, sunday, month] {
            for k in -30..30 {
                let (a, b) = (c.start(&tz, k).unwrap(), c.start(&tz, k + 1).unwrap());
                assert!(a < b);
                for t in [a, a + 1, (a + b) / 2, b - 1] {
                    assert_eq!(c.period(&tz, t), Some(k), "{c:?} {k} {t}");
                }
            }
        }
    }

    #[test]
    fn a_local_day_is_23_or_25_hours_where_the_clocks_change() {
        let ny = Zone::named("america/new_york").unwrap();
        assert_eq!(Zone::named("America/New_York").unwrap(), ny, "one entry, whatever the case");
        let tz = ny.tz();
        let day = Calendar { unit: Unit::Daily, align: 0, zone: ny };
        // 2026-03-08: New York springs forward at 2am, so that day is 23 hours; 2026-11-01 falls back.
        let k = day.period(&tz, utc(2026, 3, 8, 12)).unwrap();
        assert_eq!(day.start(&tz, k), Some(utc(2026, 3, 8, 5)), "midnight EST is 05:00 UTC");
        assert_eq!(day.start(&tz, k + 1).unwrap() - day.start(&tz, k).unwrap(), 23 * HOUR);
        let k = day.period(&tz, utc(2026, 11, 1, 12)).unwrap();
        assert_eq!(day.start(&tz, k), Some(utc(2026, 11, 1, 4)), "midnight EDT is 04:00 UTC");
        assert_eq!(day.start(&tz, k + 1).unwrap() - day.start(&tz, k).unwrap(), 25 * HOUR);
        // A 2:30am start on the day 2:30am never happens: the instant after the gap, 3:30am EDT.
        let late = Calendar { unit: Unit::Daily, align: 2 * 3600 + 1800, zone: ny };
        let k = late.period(&tz, utc(2026, 3, 8, 12)).unwrap();
        assert_eq!(late.start(&tz, k), Some(utc(2026, 3, 8, 7) + 1_800_000));
        // Every instant maps back to its period across both changes.
        for c in [day, late, Calendar { unit: Unit::Monthly, align: 1, zone: ny }] {
            let k0 = c.period(&tz, utc(2026, 1, 1, 0)).unwrap();
            for k in k0..k0 + 400 {
                let (a, b) = (c.start(&tz, k).unwrap(), c.start(&tz, k + 1).unwrap());
                assert!(a < b);
                for t in [a, a + 1, (a + b) / 2, b - 1] {
                    assert_eq!(c.period(&tz, t), Some(k), "{c:?} {k} {t}");
                }
            }
        }
        // Samoa skipped 2011-12-30: that day is no time at all.
        let apia = Zone::named("Pacific/Apia").unwrap();
        let tz = apia.tz();
        let day = Calendar { unit: Unit::Daily, align: 0, zone: apia };
        let k = days_from_civil(2011, 12, 30);
        assert_eq!(day.start(&tz, k), day.start(&tz, k + 1));
        assert!(Zone::named("Mars/Olympus_Mons").is_err());
    }

    #[test]
    fn a_shift_back_keeps_the_local_time() {
        let tz = Zone::named("Europe/Paris").unwrap().tz();
        // 2026-03-30 12:00 Paris (10:00 UTC, CEST), a day back: 2026-03-29 12:00 (also CEST, the
        // clocks went forward at 2026-03-29 02:00), 24 hours; two days back: 2026-03-28 12:00 CET,
        // 47 (the day the clocks went forward had 23).
        let to = utc(2026, 3, 30, 10);
        assert_eq!(shift_back(&tz, to, 1, 0), Some(-24 * HOUR));
        assert_eq!(shift_back(&tz, to, 2, 0), Some(-47 * HOUR));
        // A month back from 2026-03-31 12:00 CEST: 2026-02-28 12:00 CET.
        let to = utc(2026, 3, 31, 10);
        assert_eq!(shift_back(&tz, to, 0, 1), Some(utc(2026, 2, 28, 11) - to));
        // In UTC, whole days.
        assert_eq!(shift_back(&TimeZone::UTC, to + 123, 7, 0), Some(-7 * DAY_MS));
    }
}
