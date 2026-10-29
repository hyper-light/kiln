//! W24's reaper, pure (docs/ci.md W24-R31, AC15): what each cache may hold on its volume, when a
//! reap is due, and what a reap evicts, each eviction with the rule and scope that decided it.
//!
//! - **Policies** ([`Policy`]) come from four scopes ([`Scope`]). [`resolve`] merges them: the
//!   hardware limits (`floor`, `ceiling`, `target`) come from the system and runner scopes only, and
//!   the stricter of the two wins; for what to keep (`keep_last`, `keep_within`, `max_age`) and the
//!   `schedule`, the most specific scope that sets a key wins. Defaults are R15's.
//! - **Triggers** ([`due`]): free space below the floor, a write past the ceiling, or the schedule.
//! - **The choice** ([`plan`]): what `max_age` drops goes first; then, to reach the target, the
//!   entries no keep rule protects, least time saved per byte first, then least recently used;
//!   protected entries go only when the floor can't be held otherwise, and say so.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

const GIB: u64 = 1 << 30;

/// A size: bytes, or a share of the volume in basis points (`10%` is 1000).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Size {
    Bytes(u64),
    Share(u32),
}

impl Size {
    /// `20GB`, `1.5GiB`, `512000000`, `10%`, `12.5%`.
    pub fn parse(s: &str) -> Result<Size, String> {
        let t = s.trim();
        if let Some(p) = t.strip_suffix('%') {
            let v: f64 = p.trim().parse().map_err(|_| format!("`{s}` is not a share"))?;
            if !(0.0..=100.0).contains(&v) {
                return Err(format!("`{s}`: a share is between 0% and 100%"));
            }
            return Ok(Size::Share((v * 100.0).round() as u32));
        }
        let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
        let (n, unit) = t.split_at(split);
        let v: f64 = n.parse().map_err(|_| format!("`{s}` is not a size"))?;
        let mult: f64 = match unit.trim().to_ascii_lowercase().as_str() {
            "" | "b" => 1.0,
            "kb" => 1e3,
            "mb" => 1e6,
            "gb" => 1e9,
            "tb" => 1e12,
            "kib" | "k" => 1024.0,
            "mib" | "m" => 1024.0 * 1024.0,
            "gib" | "g" => GIB as f64,
            "tib" | "t" => GIB as f64 * 1024.0,
            u => return Err(format!("`{s}`: unknown unit `{u}`")),
        };
        if !v.is_finite() || v < 0.0 {
            return Err(format!("`{s}` is not a size"));
        }
        Ok(Size::Bytes((v * mult) as u64))
    }

    /// In bytes, on a volume of `volume` bytes.
    pub fn bytes(self, volume: u64) -> u64 {
        match self {
            Size::Bytes(b) => b,
            Size::Share(bp) => ((u128::from(volume) * u128::from(bp.min(10_000))) / 10_000) as u64,
        }
    }
}

/// When a reap runs by itself: every so many seconds, or on a cron expression (UTC; minute, hour,
/// day of month, month, day of week; `*`, numbers, lists, ranges and steps).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Schedule {
    Every(u64),
    Cron(String),
}

/// The scopes a policy is set at, from the broadest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    System,
    Runner,
    Repo,
    Session,
}

/// One scope's policy: each key it leaves unset comes from elsewhere.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    pub floor: Option<Size>,
    pub ceiling: Option<Size>,
    pub target: Option<Size>,
    pub keep_last: Option<u32>,
    /// Seconds.
    pub keep_within: Option<u64>,
    /// Seconds.
    pub max_age: Option<u64>,
    pub schedule: Option<Schedule>,
}

/// The policy in force for one cache on one volume, and the scope each key came from (`None`:
/// the default).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Resolved {
    pub floor: u64,
    pub ceiling: u64,
    pub target: u64,
    pub keep_last: u32,
    pub keep_within: u64,
    pub max_age: u64,
    pub schedule: Schedule,
    pub from: BTreeMap<&'static str, Option<Scope>>,
}

/// Merges the scopes' policies for a volume of `volume` bytes (W24-R31's precedence).
pub fn resolve(volume: u64, layers: &[(Scope, &Policy)]) -> Resolved {
    let mut from = BTreeMap::new();
    let hw = |pick: fn(&Policy) -> Option<Size>, stricter: fn(u64, u64) -> bool| -> Option<(u64, Scope)> {
        let mut best: Option<(u64, Scope)> = None;
        for (scope, p) in layers.iter().filter(|(s, _)| matches!(s, Scope::System | Scope::Runner)) {
            if let Some(v) = pick(p).map(|s| s.bytes(volume))
                && best.is_none_or(|(b, _)| stricter(v, b))
            {
                best = Some((v, *scope));
            }
        }
        best
    };
    let floor_default = (volume / 10).min(100 * GIB);
    let (floor, f) = hw(|p| p.floor, |a, b| a > b).map_or((floor_default, None), |(v, s)| (v, Some(s)));
    let (ceiling, c) = hw(|p| p.ceiling, |a, b| a < b).map_or(((volume / 4).min(300 * GIB), None), |(v, s)| (v, Some(s)));
    let (target, t) = hw(|p| p.target, |a, b| a > b).map_or((floor.saturating_add(volume / 20), None), |(v, s)| (v, Some(s)));
    from.insert("floor", f);
    from.insert("ceiling", c);
    from.insert("target", t);
    let mut ordered: Vec<&(Scope, &Policy)> = layers.iter().collect();
    ordered.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
    macro_rules! specific {
        ($key:literal, $field:ident, $default:expr) => {{
            let found = ordered.iter().find_map(|(s, p)| p.$field.clone().map(|v| (v, *s)));
            from.insert($key, found.as_ref().map(|(_, s)| *s));
            found.map_or($default, |(v, _)| v)
        }};
    }
    let keep_last = specific!("keep_last", keep_last, 2);
    let keep_within = specific!("keep_within", keep_within, 86_400);
    let max_age = specific!("max_age", max_age, 30 * 86_400);
    let schedule = specific!("schedule", schedule, Schedule::Every(86_400));
    Resolved { floor, ceiling, target: target.max(floor), keep_last, keep_within, max_age, schedule, from }
}

/// Why a reap runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// Free space is below the floor.
    Pressure,
    /// A write would take the cache past its ceiling.
    Ceiling,
    /// The schedule says so.
    Schedule,
}

/// Whether a reap is due now (unix seconds): `free` bytes free on the volume, `held` what the cache
/// holds, `incoming` what it's about to write, `last` when it last reaped on schedule.
pub fn due(r: &Resolved, free: u64, held: u64, incoming: u64, last: Option<i64>, now: i64) -> Option<Trigger> {
    if free < r.floor {
        return Some(Trigger::Pressure);
    }
    if held.saturating_add(incoming) > r.ceiling {
        return Some(Trigger::Ceiling);
    }
    let next = match last {
        None => return Some(Trigger::Schedule),
        Some(l) => next_after(&r.schedule, l)?,
    };
    (next <= now).then_some(Trigger::Schedule)
}

/// The schedule's next time after `t` (unix seconds); `None` for a cron expression that can't be
/// read or never fires within a year.
pub fn next_after(s: &Schedule, t: i64) -> Option<i64> {
    match s {
        Schedule::Every(secs) => Some(t.saturating_add(i64::try_from((*secs).max(60)).unwrap_or(i64::MAX))),
        Schedule::Cron(e) => Cron::parse(e).ok()?.next_after(t),
    }
}

/// A cron expression's five fields, as sets.
struct Cron {
    minute: BTreeSet<u32>,
    hour: BTreeSet<u32>,
    dom: BTreeSet<u32>,
    month: BTreeSet<u32>,
    dow: BTreeSet<u32>,
    dom_any: bool,
    dow_any: bool,
}

impl Cron {
    fn parse(e: &str) -> Result<Cron, String> {
        let f: Vec<&str> = e.split_whitespace().collect();
        let [m, h, d, mo, w] = f.as_slice() else { return Err(format!("`{e}`: a cron expression has five fields")) };
        let dow = field(w, 0, 7)?.into_iter().map(|d| d % 7).collect();
        Ok(Cron { minute: field(m, 0, 59)?, hour: field(h, 0, 23)?, dom: field(d, 1, 31)?, month: field(mo, 1, 12)?, dow, dom_any: *d == "*", dow_any: *w == "*" })
    }

    fn next_after(&self, t: i64) -> Option<i64> {
        let mut m = t.div_euclid(60).saturating_add(1);
        for _ in 0..(366 * 24 * 60) {
            let days = m.div_euclid(1440);
            let (_, month, day) = civil(days);
            let wd = (days + 4).rem_euclid(7) as u32;
            let day_ok = match (self.dom_any, self.dow_any) {
                (true, true) => true,
                (false, true) => self.dom.contains(&day),
                (true, false) => self.dow.contains(&wd),
                (false, false) => self.dom.contains(&day) || self.dow.contains(&wd),
            };
            if !self.month.contains(&month) || !day_ok {
                m = (days + 1) * 1440;
                continue;
            }
            let min_of_day = m.rem_euclid(1440) as u32;
            if !self.hour.contains(&(min_of_day / 60)) {
                m = m - i64::from(min_of_day % 60) + 60;
                continue;
            }
            if self.minute.contains(&(min_of_day % 60)) {
                return Some(m * 60);
            }
            m += 1;
        }
        None
    }
}

/// One cron field: `*`, `*/n`, `a`, `a-b`, `a-b/n`, and lists of them.
fn field(s: &str, lo: u32, hi: u32) -> Result<BTreeSet<u32>, String> {
    let mut out = BTreeSet::new();
    for part in s.split(',') {
        let (range, step) = part.split_once('/').map_or((part, "1"), |(r, st)| (r, st));
        let step: u32 = step.parse().ok().filter(|s| *s > 0).ok_or_else(|| format!("`{part}`: a bad step"))?;
        let (a, b) = if range == "*" {
            (lo, hi)
        } else if let Some((a, b)) = range.split_once('-') {
            (a.parse().map_err(|_| format!("`{part}`"))?, b.parse().map_err(|_| format!("`{part}`"))?)
        } else {
            let v: u32 = range.parse().map_err(|_| format!("`{part}`"))?;
            (v, if part.contains('/') { hi } else { v })
        };
        if a < lo || b > hi || a > b {
            return Err(format!("`{part}` is outside {lo}-{hi}"));
        }
        out.extend((a..=b).step_by(step as usize));
    }
    Ok(out)
}

/// The civil date (year, month, day) of a day count since 1970-01-01 (Howard Hinnant's).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// One entry of a cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    /// Its lane or key: `keep_last` keeps the newest of each.
    pub group: String,
    pub bytes: u64,
    /// When it was last used (unix seconds).
    pub used: i64,
    /// The time it saves each time it's used (ms).
    pub saved_ms: u64,
}

/// Why an entry goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// Unused past `max_age`.
    MaxAge,
    /// To bring free space to the target (or the cache under its ceiling).
    Target,
    /// Protected by a keep rule, but the floor couldn't be held otherwise.
    Floor,
}

/// One eviction, and what decided it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Eviction {
    pub id: String,
    pub bytes: u64,
    pub reason: Reason,
    /// The policy key and the scope it came from (`None`: the default).
    pub rule: &'static str,
    pub scope: Option<Scope>,
}

/// What a reap evicts, in order: `free` bytes free on the volume now, `held` what the cache holds.
pub fn plan(r: &Resolved, entries: &[Entry], free: u64, held: u64, now: i64) -> Vec<Eviction> {
    let protected = protected(r, entries, now);
    let mut order: Vec<&Entry> = entries.iter().collect();
    order.sort_by(|a, b| worth(a).cmp(&worth(b)).then(a.used.cmp(&b.used)).then(a.id.cmp(&b.id)));
    let (mut free, mut held) = (free, held);
    let mut out = Vec::new();
    let mut gone: BTreeSet<&str> = BTreeSet::new();
    let take = |e: &Entry, reason: Reason, rule: &'static str, out: &mut Vec<Eviction>, free: &mut u64, held: &mut u64| {
        *free = free.saturating_add(e.bytes);
        *held = held.saturating_sub(e.bytes);
        out.push(Eviction { id: e.id.clone(), bytes: e.bytes, reason, rule, scope: r.from.get(rule).copied().flatten() });
    };
    for e in &order {
        if !protected.contains(e.id.as_str()) && now.saturating_sub(e.used) > i64::try_from(r.max_age).unwrap_or(i64::MAX) {
            take(e, Reason::MaxAge, "max_age", &mut out, &mut free, &mut held);
            gone.insert(&e.id);
        }
    }
    for e in &order {
        if free >= r.target && held <= r.ceiling {
            break;
        }
        if !protected.contains(e.id.as_str()) && !gone.contains(e.id.as_str()) {
            let rule = if free < r.target { "target" } else { "ceiling" };
            take(e, Reason::Target, rule, &mut out, &mut free, &mut held);
            gone.insert(&e.id);
        }
    }
    for e in &order {
        if free >= r.floor {
            break;
        }
        if !gone.contains(e.id.as_str()) {
            take(e, Reason::Floor, "floor", &mut out, &mut free, &mut held);
            gone.insert(&e.id);
        }
    }
    out
}

/// The entries a keep rule protects: the newest `keep_last` of each group, and anything used
/// within `keep_within`.
pub fn protected<'a>(r: &Resolved, entries: &'a [Entry], now: i64) -> BTreeSet<&'a str> {
    let mut groups: BTreeMap<&str, Vec<&Entry>> = BTreeMap::new();
    for e in entries {
        groups.entry(e.group.as_str()).or_default().push(e);
    }
    let mut out = BTreeSet::new();
    for list in groups.values_mut() {
        list.sort_by(|a, b| b.used.cmp(&a.used).then(a.id.cmp(&b.id)));
        out.extend(list.iter().take(r.keep_last as usize).map(|e| e.id.as_str()));
    }
    let window = i64::try_from(r.keep_within).unwrap_or(i64::MAX);
    out.extend(entries.iter().filter(|e| now.saturating_sub(e.used) <= window).map(|e| e.id.as_str()));
    out
}

/// An entry's worth per byte, for ordering (lower goes first): its time saved over its bytes, as a
/// fraction scaled to an integer.
fn worth(e: &Entry) -> u128 {
    (u128::from(e.saved_ms) << 32) / u128::from(e.bytes.max(1))
}

/// The keys a policy has, as people write them.
pub const KEYS: &[&str] = &["floor", "ceiling", "target", "keep_last", "keep_within", "max_age", "schedule"];

/// A policy as people write it (the API's and the UI's form): sizes as `20GB` or `10%`, durations
/// as `90s`, `30m`, `6h`, `1d` or seconds, the schedule as an interval or a cron expression. A key
/// left out, or `null`, is left to other scopes; an unknown key is an error.
pub fn parse_policy(v: &serde_json::Value) -> Result<Policy, String> {
    let Some(obj) = v.as_object() else {
        return if v.is_null() { Ok(Policy::default()) } else { Err("a policy is an object of its keys".into()) };
    };
    if let Some(k) = obj.keys().find(|k| !KEYS.contains(&k.as_str())) {
        return Err(format!("`{k}` isn't a policy key: {}", KEYS.join(", ")));
    }
    let text = |k: &str| -> Result<Option<String>, String> {
        match obj.get(k) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
            Some(serde_json::Value::Number(n)) => Ok(Some(n.to_string())),
            Some(_) => Err(format!("`{k}` is a string or a number")),
        }
    };
    let size = |k: &str| text(k)?.map(|s| Size::parse(&s).map_err(|e| format!("{k}: {e}"))).transpose();
    let duration = |k: &str| text(k)?.map(|s| parse_duration(&s).map_err(|e| format!("{k}: {e}"))).transpose();
    let keep_last = match text("keep_last")? {
        None => None,
        Some(s) => Some(s.parse::<u32>().ok().filter(|n| *n <= 1000).ok_or_else(|| format!("keep_last: `{s}` isn't a count from 0 to 1000"))?),
    };
    let schedule = text("schedule")?.map(|s| parse_schedule(&s).map_err(|e| format!("schedule: {e}"))).transpose()?;
    Ok(Policy { floor: size("floor")?, ceiling: size("ceiling")?, target: size("target")?, keep_last, keep_within: duration("keep_within")?, max_age: duration("max_age")?, schedule })
}

/// A policy in the form [`parse_policy`] reads: only the keys it sets.
pub fn policy_text(p: &Policy) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for (k, v) in [("floor", p.floor), ("ceiling", p.ceiling), ("target", p.target)] {
        if let Some(v) = v {
            m.insert(k.into(), size_text(v).into());
        }
    }
    if let Some(n) = p.keep_last {
        m.insert("keep_last".into(), n.into());
    }
    for (k, v) in [("keep_within", p.keep_within), ("max_age", p.max_age)] {
        if let Some(v) = v {
            m.insert(k.into(), duration_text(v).into());
        }
    }
    if let Some(s) = &p.schedule {
        m.insert("schedule".into(), schedule_text(s).into());
    }
    serde_json::Value::Object(m)
}

/// W24-R31: hardware limits are set for the system or a runner, never for a repository or a
/// session.
pub fn check_scope(p: &Policy, scope: Scope) -> Result<(), String> {
    let hardware = p.floor.is_some() || p.ceiling.is_some() || p.target.is_some();
    if hardware && matches!(scope, Scope::Repo | Scope::Session) {
        return Err("floor, ceiling and target are set for the system or a runner, not a repository or a session".into());
    }
    Ok(())
}

/// `90s`, `30m`, `6h`, `1d`, `2w`, or seconds.
pub fn parse_duration(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (n, unit) = t.split_at(split);
    let n: u64 = n.parse().map_err(|_| format!("`{s}` isn't a duration (90s, 30m, 6h, 1d)"))?;
    let mult = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        _ => return Err(format!("`{s}` isn't a duration (90s, 30m, 6h, 1d)")),
    };
    n.checked_mul(mult).ok_or_else(|| format!("`{s}` is too long"))
}

/// An interval (`6h`, at least a minute) or a five-field cron expression.
pub fn parse_schedule(s: &str) -> Result<Schedule, String> {
    if s.split_whitespace().count() == 5 {
        Cron::parse(s)?;
        return Ok(Schedule::Cron(s.split_whitespace().collect::<Vec<_>>().join(" ")));
    }
    let secs = parse_duration(s)?;
    if secs < 60 {
        return Err(format!("`{s}`: a schedule's interval is at least a minute"));
    }
    Ok(Schedule::Every(secs))
}

fn size_text(s: Size) -> String {
    match s {
        Size::Share(bp) if bp % 100 == 0 => format!("{}%", bp / 100),
        Size::Share(bp) => format!("{}.{:02}%", bp / 100, bp % 100),
        Size::Bytes(b) if b > 0 && b % GIB == 0 => format!("{}GiB", b / GIB),
        Size::Bytes(b) => b.to_string(),
    }
}

fn duration_text(secs: u64) -> String {
    match secs {
        0 => "0s".into(),
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

fn schedule_text(s: &Schedule) -> String {
    match s {
        Schedule::Every(secs) => duration_text(*secs),
        Schedule::Cron(e) => e.clone(),
    }
}

/// The effective policy as people read it: each key's value and the scope it came from (`null`:
/// kiln's default), with the hardware limits in bytes on a volume of `volume` bytes.
pub fn effective_text(r: &Resolved, volume: u64) -> serde_json::Value {
    let from = |k: &str| r.from.get(k).copied().flatten();
    serde_json::json!({
        "volume": volume,
        "floor": { "bytes": r.floor, "from": from("floor") },
        "ceiling": { "bytes": r.ceiling, "from": from("ceiling") },
        "target": { "bytes": r.target, "from": from("target") },
        "keep_last": { "value": r.keep_last, "from": from("keep_last") },
        "keep_within": { "value": duration_text(r.keep_within), "from": from("keep_within") },
        "max_age": { "value": duration_text(r.max_age), "from": from("max_age") },
        "schedule": { "value": schedule_text(&r.schedule), "from": from("schedule") },
    })
}

#[cfg(test)]
mod tests;
