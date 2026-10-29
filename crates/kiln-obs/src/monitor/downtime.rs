//! Downtimes (`docs/observe.md` §9.3), Datadog's: when a monitor's notifications are held, once
//! (from a time to a time) or recurring (an RFC 5545 recurrence rule, its occurrences begun at a
//! wall-clock time in a timezone and lasting a duration), for the groups a scope names
//! (`env:prod host:(web-1 OR web-2)`).
//!
//! The recurrence rules taken are RFC 5545's that downtimes use: `FREQ` of `DAILY`, `WEEKLY`,
//! `MONTHLY` or `YEARLY`, `INTERVAL`, `BYDAY` (with an ordinal in a month or a year: `2TU`,
//! `-1FR`), `BYMONTHDAY` (from the end when negative), `BYMONTH`, `COUNT`, `UNTIL` and `WKST`
//! (weeks begin on Monday unless it says). An occurrence begins at the start's time of day, on
//! each day the rule names on or after the start's date; a time a change of offset skips or makes
//! twice is taken as RFC 5545 takes it (`crate::query::calendar::Zone::civil_ns`).

use crate::query::calendar::{Zone, civil_from_days, days_from_civil, days_in_month};

/// Occurrences a rule is expanded to at the most when asked about a time: some 270 years of a
/// daily one.
pub const MAX_OCCURRENCES: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

/// A wall-clock date and time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Local {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    pub hour: i64,
    pub minute: i64,
    pub second: i64,
}

impl Local {
    /// `2026-10-20T02:00:00` (or with a space, or seconds left out).
    pub fn parse(s: &str) -> Result<Self, String> {
        let bad = || format!("a start: YYYY-MM-DDTHH:MM[:SS] ({s})");
        let s = s.trim();
        let (d, t) = s.split_once(['T', ' ']).unwrap_or((s, "00:00:00"));
        let mut dp = d.split('-').map(|x| x.parse::<i64>().map_err(|_| bad()));
        let (year, month, day) = (dp.next().ok_or_else(bad)??, dp.next().ok_or_else(bad)??, dp.next().ok_or_else(bad)??);
        let mut tp = t.split(':').map(|x| x.parse::<i64>().map_err(|_| bad()));
        let (hour, minute) = (tp.next().ok_or_else(bad)??, tp.next().ok_or_else(bad)??);
        let second = tp.next().transpose()?.unwrap_or(0);
        if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) || !(0..24).contains(&hour) || !(0..60).contains(&minute) || !(0..60).contains(&second) {
            return Err(bad());
        }
        Ok(Self { year, month, day, hour, minute, second })
    }

    fn days(self) -> i64 {
        days_from_civil(self.year, self.month, self.day)
    }

    /// When it is in `zone` (ns since the epoch).
    pub fn at(self, zone: Zone) -> Option<i64> {
        let n = |v: i64| i8::try_from(v).ok();
        zone.civil_ns(i16::try_from(self.year).ok()?, n(self.month)?, n(self.day)?, n(self.hour)?, n(self.minute)?, n(self.second)?, 0)
    }
}

/// Monday 0 to Sunday 6, of a day counted from 1970-01-01 (a Thursday).
fn weekday(days: i64) -> i64 {
    (days + 3).rem_euclid(7)
}

/// A recurrence rule, RFC 5545's (the part downtimes use).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rrule {
    pub freq: Freq,
    pub interval: i64,
    /// Weekdays (Monday 0), each with its ordinal in the month or year if any.
    pub by_day: Vec<(Option<i64>, i64)>,
    pub by_month_day: Vec<i64>,
    pub by_month: Vec<i64>,
    pub count: Option<u64>,
    /// The last time an occurrence may begin: UTC, or (no `Z`) a wall-clock time in the zone.
    pub until: Option<Until>,
    pub week_start: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Until {
    Utc(i64),
    Local(Local),
}

fn day_code(s: &str) -> Option<i64> {
    Some(match s {
        "MO" => 0,
        "TU" => 1,
        "WE" => 2,
        "TH" => 3,
        "FR" => 4,
        "SA" => 5,
        "SU" => 6,
        _ => return None,
    })
}

impl Rrule {
    /// `FREQ=WEEKLY;BYDAY=SA,SU` (an `RRULE:` before it taken off).
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let s = s.strip_prefix("RRULE:").unwrap_or(s);
        let mut r = Self { freq: Freq::Daily, interval: 1, by_day: Vec::new(), by_month_day: Vec::new(), by_month: Vec::new(), count: None, until: None, week_start: 0 };
        let mut freq = None;
        for part in s.split(';').filter(|p| !p.is_empty()) {
            let (k, v) = part.split_once('=').ok_or_else(|| format!("a rule's part: NAME=VALUE ({part})"))?;
            let bad = || format!("{k}: {v}");
            match k.to_ascii_uppercase().as_str() {
                "FREQ" => {
                    freq = Some(match v.to_ascii_uppercase().as_str() {
                        "DAILY" => Freq::Daily,
                        "WEEKLY" => Freq::Weekly,
                        "MONTHLY" => Freq::Monthly,
                        "YEARLY" => Freq::Yearly,
                        other => return Err(format!("FREQ: {other} (DAILY, WEEKLY, MONTHLY or YEARLY: a downtime recurs no oftener than daily)")),
                    })
                }
                "INTERVAL" => r.interval = v.parse::<i64>().ok().filter(|i| (1..=10_000).contains(i)).ok_or_else(bad)?,
                "COUNT" => r.count = Some(v.parse::<u64>().ok().filter(|c| *c >= 1).ok_or_else(bad)?),
                "UNTIL" => r.until = Some(until(v).ok_or_else(bad)?),
                "WKST" => r.week_start = day_code(&v.to_ascii_uppercase()).ok_or_else(bad)?,
                "BYDAY" => {
                    for d in v.split(',') {
                        let d = d.trim().to_ascii_uppercase();
                        let split = d.len().checked_sub(2).ok_or_else(bad)?;
                        let (ord, code) = (d.get(..split).unwrap_or_default(), d.get(split..).unwrap_or_default());
                        let ord = if ord.is_empty() { None } else { Some(ord.trim_start_matches('+').parse::<i64>().ok().filter(|o| *o != 0 && o.abs() <= 53).ok_or_else(bad)?) };
                        r.by_day.push((ord, day_code(code).ok_or_else(bad)?));
                    }
                }
                "BYMONTHDAY" => {
                    for d in v.split(',') {
                        r.by_month_day.push(d.trim().parse::<i64>().ok().filter(|d| *d != 0 && d.abs() <= 31).ok_or_else(bad)?);
                    }
                }
                "BYMONTH" => {
                    for m in v.split(',') {
                        r.by_month.push(m.trim().parse::<i64>().ok().filter(|m| (1..=12).contains(m)).ok_or_else(bad)?);
                    }
                }
                other => return Err(format!("{other}: not a part a downtime's rule takes (FREQ, INTERVAL, BYDAY, BYMONTHDAY, BYMONTH, COUNT, UNTIL, WKST)")),
            }
        }
        r.freq = freq.ok_or("a rule's FREQ: DAILY, WEEKLY, MONTHLY or YEARLY")?;
        if r.count.is_some() && r.until.is_some() {
            return Err("COUNT or UNTIL, not both".into());
        }
        if r.by_day.iter().any(|(o, _)| o.is_some()) && matches!(r.freq, Freq::Daily | Freq::Weekly) {
            return Err("BYDAY's ordinals (2TU) are a monthly or yearly rule's".into());
        }
        Ok(r)
    }

    /// The days (counted from 1970-01-01) of the period that begins at `period` (its index from
    /// the start's), sorted: the candidates its parts name.
    fn days_of(&self, start: Local, period: i64, out: &mut Vec<i64>) {
        out.clear();
        match self.freq {
            Freq::Daily => {
                let d = start.days() + period * self.interval;
                let (y, m, md) = civil_from_days(d);
                if self.month_ok(m) && self.month_day_ok(y, m, md) && self.weekday_ok(d) {
                    out.push(d);
                }
            }
            Freq::Weekly => {
                // The week the start falls in, as `WKST` begins weeks.
                let first = start.days() - (weekday(start.days()) - self.week_start).rem_euclid(7) + period * self.interval * 7;
                let wanted: Vec<i64> = if self.by_day.is_empty() { vec![weekday(start.days())] } else { self.by_day.iter().map(|(_, w)| *w).collect() };
                for i in 0..7 {
                    let d = first + i;
                    let (_, m, _) = civil_from_days(d);
                    if wanted.contains(&weekday(d)) && self.month_ok(m) {
                        out.push(d);
                    }
                }
            }
            Freq::Monthly => {
                let months = start.year * 12 + (start.month - 1) + period * self.interval;
                let (y, m) = (months.div_euclid(12), months.rem_euclid(12) + 1);
                if self.month_ok(m) {
                    self.days_in(y, m, start, out);
                }
            }
            Freq::Yearly => {
                let y = start.year + period * self.interval;
                let months: Vec<i64> = if self.by_month.is_empty() { vec![start.month] } else { self.by_month.clone() };
                for m in months {
                    self.days_in(y, m, start, out);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
    }

    /// The days of month `m` of year `y` the rule names: by day of the month, by weekday (each,
    /// or the nth), both (the days both name), or the start's day of the month.
    fn days_in(&self, y: i64, m: i64, start: Local, out: &mut Vec<i64>) {
        let n = days_in_month(y, m);
        let first = days_from_civil(y, m, 1);
        let by_md = |md: i64| -> bool { self.by_month_day.iter().any(|&x| x == md || (x < 0 && n + 1 + x == md)) };
        let by_wd = |md: i64| -> bool {
            let w = weekday(first + md - 1);
            self.by_day.iter().any(|&(ord, wd)| {
                wd == w
                    && match ord {
                        None => true,
                        // The nth of its weekday in the month, from the start or the end.
                        Some(o) if o > 0 => (md - 1) / 7 + 1 == o,
                        Some(o) => (n - md) / 7 + 1 == -o,
                    }
            })
        };
        for md in 1..=n {
            let keep = match (self.by_month_day.is_empty(), self.by_day.is_empty()) {
                (true, true) => md == start.day,
                (false, true) => by_md(md),
                (true, false) => by_wd(md),
                (false, false) => by_md(md) && by_wd(md),
            };
            if keep {
                out.push(first + md - 1);
            }
        }
    }

    fn month_ok(&self, m: i64) -> bool {
        self.by_month.is_empty() || self.by_month.contains(&m)
    }

    fn month_day_ok(&self, y: i64, m: i64, md: i64) -> bool {
        let n = days_in_month(y, m);
        self.by_month_day.is_empty() || self.by_month_day.iter().any(|&x| x == md || (x < 0 && n + 1 + x == md))
    }

    fn weekday_ok(&self, d: i64) -> bool {
        self.by_day.is_empty() || self.by_day.iter().any(|&(_, w)| w == weekday(d))
    }
}

/// `UNTIL`'s value: `20261231T235959Z` (UTC), `20261231T235959` or `20261231` (wall-clock).
fn until(v: &str) -> Option<Until> {
    let v = v.trim();
    let n = |r: std::ops::Range<usize>| v.get(r).and_then(|x| x.parse::<i64>().ok());
    let (y, m, d) = (n(0..4)?, n(4..6)?, n(6..8)?);
    let (h, mi, s) = if v.len() >= 15 && v.as_bytes().get(8) == Some(&b'T') { (n(9..11)?, n(11..13)?, n(13..15)?) } else { (23, 59, 59) };
    let local = Local { year: y, month: m, day: d, hour: h, minute: mi, second: s };
    if v.ends_with('Z') {
        Some(Until::Utc((days_from_civil(y, m, d) * 86_400 + h * 3600 + mi * 60 + s) * 1_000_000_000))
    } else {
        Some(Until::Local(local))
    }
}

/// A recurring downtime's occurrences: a rule, from a wall-clock start in a zone, each lasting a
/// duration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recurrence {
    pub rule: Rrule,
    pub start: Local,
    pub duration_ms: i64,
    pub zone: Zone,
}

impl Recurrence {
    /// Each occurrence's start and end (ns), in order, while `f` says to go on, up to
    /// [`MAX_OCCURRENCES`].
    pub fn each(&self, mut f: impl FnMut(i64, i64) -> bool) {
        let mut days = Vec::new();
        let mut emitted: u64 = 0;
        let start_day = self.start.days();
        let until_ns = match self.rule.until {
            Some(Until::Utc(t)) => Some(t),
            Some(Until::Local(l)) => l.at(self.zone),
            None => None,
        };
        // Periods with no day the rule names in a row, at the most, before it is taken to name
        // none again (a 31st of February).
        let mut empty = 0;
        for period in 0.. {
            if emitted as usize >= MAX_OCCURRENCES || empty > 1_000 {
                return;
            }
            self.rule.days_of(self.start, period, &mut days);
            if days.is_empty() {
                empty += 1;
                continue;
            }
            empty = 0;
            for &d in &days {
                if d < start_day {
                    continue;
                }
                let (y, m, md) = civil_from_days(d);
                let local = Local { year: y, month: m, day: md, ..self.start };
                let Some(at) = local.at(self.zone) else { continue };
                if until_ns.is_some_and(|u| at > u) || self.rule.count.is_some_and(|c| emitted >= c) {
                    return;
                }
                emitted += 1;
                if !f(at, at.saturating_add(self.duration_ms.saturating_mul(1_000_000))) {
                    return;
                }
            }
        }
    }

    /// The occurrence `now` falls in (ns), if any.
    pub fn active(&self, now: i64) -> Option<(i64, i64)> {
        let mut found = None;
        self.each(|s, e| {
            if s > now {
                return false;
            }
            if now < e {
                found = Some((s, e));
            }
            true
        });
        found
    }

    /// The first occurrence that ends after `now`: the one it falls in, or the next.
    pub fn next_after(&self, now: i64) -> Option<(i64, i64)> {
        let mut found = None;
        self.each(|s, e| {
            if e > now {
                found = Some((s, e));
                return false;
            }
            true
        });
        found
    }
}

/// `1h30m`, `2d`, `45m`, `90s` as ms; a day 24 hours.
pub fn duration_ms(s: &str) -> Result<i64, String> {
    let bad = || format!("a duration: 2h, 1h30m, 1d ({s})");
    let s = s.trim();
    if s.is_empty() {
        return Err(bad());
    }
    let mut total = 0i64;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: i64 = num.parse().map_err(|_| bad())?;
        num.clear();
        total += n * match c {
            'w' => 7 * 86_400_000,
            'd' => 86_400_000,
            'h' => 3_600_000,
            'm' => 60_000,
            's' => 1_000,
            _ => return Err(bad()),
        };
    }
    if !num.is_empty() || total <= 0 {
        return Err(bad());
    }
    Ok(total)
}

/// What groups a downtime holds: each term a tag the group has (`host:web-1`), one of some
/// (`host:(web-1 OR web-2)`), any value (`host:*`), or not (`-env:dev`); `*` every group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    terms: Vec<(bool, String, Vec<String>)>,
}

impl Scope {
    pub fn parse(s: &str) -> Result<Self, String> {
        let mut terms = Vec::new();
        let s = s.trim();
        if s.is_empty() || s == "*" {
            return Ok(Self { terms });
        }
        let mut rest = s;
        while !rest.trim_start().is_empty() {
            rest = rest.trim_start();
            if let Some(r) = rest.strip_prefix("AND ") {
                rest = r;
                continue;
            }
            let (neg, r) = match rest.strip_prefix('-') {
                Some(r) => (true, r),
                None => (false, rest),
            };
            let colon = r.find(':').ok_or_else(|| format!("a scope's term: tag:value ({r})"))?;
            let key = r.get(..colon).unwrap_or_default().trim().to_owned();
            let after = r.get(colon + 1..).unwrap_or_default();
            let (values, next) = if let Some(group) = after.strip_prefix('(') {
                let close = group.find(')').ok_or("a scope's ( without its )")?;
                let vals: Vec<String> = group.get(..close).unwrap_or_default().split(" OR ").map(|v| v.trim().to_owned()).filter(|v| !v.is_empty()).collect();
                (vals, group.get(close + 1..).unwrap_or_default())
            } else {
                let end = after.find(char::is_whitespace).unwrap_or(after.len());
                (vec![after.get(..end).unwrap_or_default().to_owned()], after.get(end..).unwrap_or_default())
            };
            if key.is_empty() || values.is_empty() || values.iter().any(String::is_empty) {
                return Err(format!("a scope's term: tag:value ({r})"));
            }
            terms.push((neg, key, values));
            rest = next;
        }
        Ok(Self { terms })
    }

    /// Whether it holds `group` (a monitor's group's tags; none for a monitor of one group, which
    /// only `*` holds).
    pub fn holds(&self, group: &[(String, String)]) -> bool {
        self.terms.iter().all(|(neg, key, values)| {
            let has = group.iter().any(|(k, v)| k == key && values.iter().any(|want| want == "*" || want == v || crate::index::glob(want, v)));
            has != *neg
        })
    }

    pub fn is_all(&self) -> bool {
        self.terms.is_empty()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn rec(rule: &str, start: &str, dur: &str, zone: &str) -> Recurrence {
        Recurrence { rule: Rrule::parse(rule).unwrap(), start: Local::parse(start).unwrap(), duration_ms: duration_ms(dur).unwrap(), zone: Zone::named(zone).unwrap() }
    }

    /// The first `n` occurrences' wall-clock starts, in UTC (`YYYY-MM-DD HH:MM`).
    fn firsts(r: &Recurrence, n: usize) -> Vec<String> {
        let mut out = Vec::new();
        r.each(|s, _| {
            let secs = s / 1_000_000_000;
            let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
            let t = secs.rem_euclid(86_400);
            out.push(format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", t / 3600, t % 3600 / 60));
            out.len() < n
        });
        out
    }

    #[test]
    fn rules_expand_to_the_days_they_name() {
        // Weekends at 02:00 UTC, for four hours: the start a Tuesday, so its Saturday first.
        assert_eq!(firsts(&rec("FREQ=WEEKLY;BYDAY=SA,SU", "2026-10-20T02:00", "4h", "UTC"), 4), ["2026-10-24 02:00", "2026-10-25 02:00", "2026-10-31 02:00", "2026-11-01 02:00"]);
        // Every other day, three of them.
        assert_eq!(firsts(&rec("FREQ=DAILY;INTERVAL=2;COUNT=3", "2026-10-30T23:30", "30m", "UTC"), 9), ["2026-10-30 23:30", "2026-11-01 23:30", "2026-11-03 23:30"]);
        // The second Tuesday of each month (Patch Tuesday), in New York: EDT, then EST.
        assert_eq!(firsts(&rec("FREQ=MONTHLY;BYDAY=2TU", "2026-10-01T22:00", "2h", "America/New_York"), 3), ["2026-10-14 02:00", "2026-11-11 03:00", "2026-12-09 03:00"]);
        // The last Friday of each month.
        assert_eq!(firsts(&rec("FREQ=MONTHLY;BYDAY=-1FR", "2026-10-01T18:00", "1h", "UTC"), 3), ["2026-10-30 18:00", "2026-11-27 18:00", "2026-12-25 18:00"]);
        // The last day of each month, and the 31st only where there is one.
        assert_eq!(firsts(&rec("FREQ=MONTHLY;BYMONTHDAY=-1", "2026-01-15T00:00", "1h", "UTC"), 3), ["2026-01-31 00:00", "2026-02-28 00:00", "2026-03-31 00:00"]);
        assert_eq!(firsts(&rec("FREQ=MONTHLY", "2026-01-31T00:00", "1h", "UTC"), 3), ["2026-01-31 00:00", "2026-03-31 00:00", "2026-05-31 00:00"]);
        // Yearly, on the first Monday of September (Labor Day), until a day.
        assert_eq!(firsts(&rec("FREQ=YEARLY;BYMONTH=9;BYDAY=1MO;UNTIL=20280101", "2026-01-01T00:00", "1d", "UTC"), 9), ["2026-09-07 00:00", "2027-09-06 00:00"]);
        // A wall-clock time a change of offset skips (02:30 on 2027-03-14 in New York): as RFC
        // 5545 has it, after the change (03:30 EDT, 07:30 UTC).
        assert_eq!(firsts(&rec("FREQ=DAILY;COUNT=2", "2027-03-14T02:30", "1h", "America/New_York"), 2), ["2027-03-14 07:30", "2027-03-15 06:30"]);
    }

    #[test]
    fn a_time_falls_in_the_occurrence_it_falls_in() {
        let r = rec("FREQ=WEEKLY;BYDAY=SA", "2026-10-24T02:00", "4h", "UTC");
        let at = |s: &str| Local::parse(s).unwrap().at(Zone::UTC).unwrap();
        assert_eq!(r.active(at("2026-10-31T03:59:59")), Some((at("2026-10-31T02:00"), at("2026-10-31T06:00"))));
        assert_eq!(r.active(at("2026-10-31T06:00")), None, "an occurrence ends where it ends");
        assert_eq!(r.next_after(at("2026-10-31T06:00")), Some((at("2026-11-07T02:00"), at("2026-11-07T06:00"))));
        assert_eq!(r.active(at("2026-10-24T01:59")), None, "before its start");
    }

    #[test]
    fn rules_and_scopes_are_checked() {
        for bad in ["FREQ=HOURLY", "BYDAY=MO", "FREQ=WEEKLY;BYDAY=2MO", "FREQ=DAILY;COUNT=2;UNTIL=20270101", "FREQ=DAILY;BYSETPOS=1", "FREQ=MONTHLY;BYMONTHDAY=32"] {
            assert!(Rrule::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(duration_ms("1h30m").unwrap(), 5_400_000);
        assert!(duration_ms("0m").is_err() && duration_ms("2").is_err() && duration_ms("h").is_err());
        let g = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect::<Vec<_>>();
        let s = Scope::parse("env:prod AND host:(web-1 OR web-2) -region:eu").unwrap();
        assert!(s.holds(&g(&[("env", "prod"), ("host", "web-2"), ("region", "us")])));
        assert!(!s.holds(&g(&[("env", "prod"), ("host", "web-3")])));
        assert!(!s.holds(&g(&[("env", "prod"), ("host", "web-1"), ("region", "eu")])));
        assert!(!s.holds(&[]), "a group without the tag is not held");
        assert!(Scope::parse("*").unwrap().holds(&[]) && Scope::parse("host:web-*").unwrap().holds(&g(&[("host", "web-9")])));
        assert!(Scope::parse("host").is_err() && Scope::parse("host:(a OR").is_err());
    }
}
