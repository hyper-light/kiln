//! Downtimes (`docs/observe.md` §9.3), Datadog's: monitors' notifications held for a while, once
//! or recurring, for the monitors named (by id, or by tags they all carry) and the groups a scope
//! holds. A group held says nothing (its changes are still events, marked so); when a downtime's
//! window ends (it expires, or is canceled), each group it held that is in one of its end states is
//! told once, and, if the downtime says so, the first recovery after is not. Kept by the server,
//! `<observe>/downtimes.json`; a downtime canceled or ended is kept 30 days, then forgotten.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use kiln_obs::monitor::downtime::{Local, Recurrence, Rrule, Scope, duration_ms};
use kiln_obs::query::calendar::Zone;
use serde::{Deserialize, Serialize};

/// Downtimes kept at the most.
pub const DOWNTIMES: usize = 1_000;
/// How long a downtime that ended or was canceled is kept.
const KEPT_MS: i64 = 30 * 86_400_000;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Which monitors a downtime holds: one, or those that carry every tag given.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorIdentifier {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub monitor_tags: Vec<String>,
}

/// A time as the API takes one: ms since the epoch, or RFC 3339.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Time {
    Ms(i64),
    Text(String),
}

impl Time {
    pub fn ms(&self) -> Result<i64, String> {
        match self {
            Self::Ms(ms) => Ok(*ms),
            Self::Text(t) => time::OffsetDateTime::parse(t, &time::format_description::well_known::Rfc3339)
                .map(|d| i64::try_from(d.unix_timestamp_nanos() / 1_000_000).unwrap_or(i64::MAX))
                .map_err(|_| format!("a time: ms since the epoch, or RFC 3339 ({t})")),
        }
    }
}

/// One recurrence: a rule, the wall-clock time its first occurrence may begin at, how long each
/// lasts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecurrenceDef {
    pub rrule: String,
    /// `2026-10-24T02:00:00`, in the schedule's timezone.
    pub start: String,
    /// `4h`, `1h30m`, `2d`.
    pub duration: String,
}

/// When a downtime holds: from a time (now, when none) to a time (until canceled, when none); or
/// recurring, in a timezone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Schedule {
    Recurring {
        recurrences: Vec<RecurrenceDef>,
        #[serde(default = "utc")]
        timezone: String,
    },
    Once {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<Time>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end: Option<Time>,
    },
}

fn utc() -> String {
    "UTC".into()
}

fn end_states() -> Vec<String> {
    ["alert", "no data", "warn"].into_iter().map(str::to_owned).collect()
}

fn end_types() -> Vec<String> {
    vec!["expired".to_owned()]
}

fn all() -> String {
    "*".into()
}

/// A downtime as the API takes and shows it (Datadog's v2 downtime).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Downtime {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub message: String,
    pub monitor_identifier: MonitorIdentifier,
    /// The groups held: `env:prod host:(web-1 OR web-2)`; `*` every one.
    #[serde(default = "all")]
    pub scope: String,
    pub schedule: Schedule,
    #[serde(default)]
    pub mute_first_recovery_notification: bool,
    /// The states a group held is told of as the downtime ends: `alert`, `no data`, `warn`.
    #[serde(default = "end_states")]
    pub notify_end_states: Vec<String>,
    /// The ends told of: `expired`, `canceled`.
    #[serde(default = "end_types")]
    pub notify_end_types: Vec<String>,
    /// Unix ms it was canceled at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canceled: Option<i64>,
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub modified: i64,
}

/// A downtime's schedule, compiled.
#[derive(Debug, Clone)]
enum When {
    Once { start: i64, end: Option<i64> },
    Recurring(Vec<Recurrence>),
}

impl Downtime {
    fn when(&self) -> Result<When, String> {
        match &self.schedule {
            Schedule::Once { start, end } => {
                let start = start.as_ref().map(Time::ms).transpose()?.unwrap_or(self.created);
                let end = end.as_ref().map(Time::ms).transpose()?;
                if end.is_some_and(|e| e <= start) {
                    return Err("a downtime's end comes after its start".into());
                }
                Ok(When::Once { start, end })
            }
            Schedule::Recurring { recurrences, timezone } => {
                if recurrences.is_empty() || recurrences.len() > 5 {
                    return Err("a recurring downtime's recurrences: 1 to 5".into());
                }
                let zone = Zone::named(timezone)?;
                let mut out = Vec::with_capacity(recurrences.len());
                for r in recurrences {
                    let duration = duration_ms(&r.duration)?;
                    if duration > 365 * 86_400_000 {
                        return Err("an occurrence lasts a year at the most".into());
                    }
                    out.push(Recurrence { rule: Rrule::parse(&r.rrule)?, start: Local::parse(&r.start)?, duration_ms: duration, zone });
                }
                Ok(When::Recurring(out))
            }
        }
    }

    /// Whether it is well-formed.
    pub fn check(&self) -> Result<(), String> {
        let i = &self.monitor_identifier;
        if i.monitor_id.as_deref().is_none_or(str::is_empty) == i.monitor_tags.is_empty() {
            return Err("a downtime names its monitors: a monitor_id, or monitor_tags (not both)".into());
        }
        if i.monitor_tags.len() > 32 || i.monitor_tags.iter().any(|t| t.trim().is_empty()) {
            return Err("monitor_tags: 1 to 32 tags".into());
        }
        Scope::parse(&self.scope)?;
        self.when()?;
        if self.message.len() > 4_000 {
            return Err("a message: 4,000 characters at the most".into());
        }
        if let Some(s) = self.notify_end_states.iter().find(|s| !matches!(s.as_str(), "alert" | "no data" | "warn")) {
            return Err(format!("{s}: notify_end_states are alert, no data and warn"));
        }
        if let Some(t) = self.notify_end_types.iter().find(|t| !matches!(t.as_str(), "canceled" | "expired")) {
            return Err(format!("{t}: notify_end_types are canceled and expired"));
        }
        Ok(())
    }

    /// The window it holds in at `now` (ms), if any.
    pub fn active(&self, now: i64) -> Option<(i64, i64)> {
        if self.canceled.is_some_and(|c| c <= now) {
            return None;
        }
        match self.when().ok()? {
            When::Once { start, end } => (start <= now && end.is_none_or(|e| now < e)).then_some((start, end.unwrap_or(i64::MAX))),
            When::Recurring(rs) => {
                let ns = now.saturating_mul(1_000_000);
                rs.iter().filter_map(|r| r.active(ns)).map(|(s, e)| (s / 1_000_000, e / 1_000_000)).min_by_key(|w| w.0)
            }
        }
    }

    /// Its window now, or its next: none once it has ended or been canceled.
    pub fn window(&self, now: i64) -> Option<(i64, i64)> {
        if self.canceled.is_some_and(|c| c <= now) {
            return None;
        }
        match self.when().ok()? {
            When::Once { start, end } => end.is_none_or(|e| now < e).then_some((start, end.unwrap_or(i64::MAX))),
            When::Recurring(rs) => {
                let ns = now.saturating_mul(1_000_000);
                rs.iter().filter_map(|r| r.next_after(ns)).map(|(s, e)| (s / 1_000_000, e / 1_000_000)).min_by_key(|w| w.0)
            }
        }
    }

    /// `active`, `scheduled`, `ended` or `canceled`, at `now`.
    pub fn status(&self, now: i64) -> &'static str {
        if self.canceled.is_some_and(|c| c <= now) {
            return "canceled";
        }
        match (self.active(now), self.window(now)) {
            (Some(_), _) => "active",
            (None, Some(_)) => "scheduled",
            (None, None) => "ended",
        }
    }

    /// Whether it names monitor `id` (of `tags`).
    pub fn names(&self, id: &str, tags: &[String]) -> bool {
        let i = &self.monitor_identifier;
        match &i.monitor_id {
            Some(m) if !m.is_empty() => m == id,
            // `*`: every monitor (Datadog's `monitor_tags: ["*"]`, what a host's mute makes).
            _ => !i.monitor_tags.is_empty() && i.monitor_tags.iter().all(|t| t == "*" || tags.contains(t)),
        }
    }

    /// Whether it holds `group` of monitor `id` (of `tags`) at `now`.
    pub fn holds(&self, id: &str, tags: &[String], group: &[(String, String)], now: i64) -> bool {
        self.names(id, tags) && self.active(now).is_some() && Scope::parse(&self.scope).is_ok_and(|s| s.holds(group))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    downtimes: BTreeMap<String, Downtime>,
}

/// A window that ended: the downtime, and whether it expired or was canceled.
#[derive(Debug, Clone)]
pub struct Ended {
    pub downtime: Downtime,
    pub how: &'static str,
}

/// The server's downtimes.
pub struct Downtimes {
    path: PathBuf,
    saved: Mutex<Saved>,
    /// The windows that held when last looked at, by downtime: to tell their ends once.
    holding: Mutex<HashMap<String, (i64, i64)>>,
}

impl Downtimes {
    pub fn open(dir: &Path) -> Self {
        let path = dir.join("downtimes.json");
        let saved = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { path, saved: Mutex::new(saved), holding: Mutex::new(HashMap::new()) }
    }

    fn save(&self, saved: &Saved) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(saved).map_err(|e| e.to_string())?;
        crate::sync::group::save_atomic(&self.path, &bytes).map_err(|e| e.to_string())
    }

    /// Every downtime, newest first.
    pub fn list(&self) -> Vec<Downtime> {
        let mut out: Vec<Downtime> = lock(&self.saved).downtimes.values().cloned().collect();
        out.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.id.cmp(&b.id)));
        out
    }

    pub fn get(&self, id: &str) -> Option<Downtime> {
        lock(&self.saved).downtimes.get(id).cloned()
    }

    /// A downtime made (no id) or changed (its own): checked, kept.
    pub fn put(&self, mut d: Downtime) -> Result<Downtime, String> {
        let now = crate::util::now_ms();
        let mut saved = lock(&self.saved);
        // Those long over go.
        saved.downtimes.retain(|_, x| x.canceled.is_none_or(|c| now - c < KEPT_MS) && (x.status(now) != "ended" || now - x.modified < KEPT_MS));
        match saved.downtimes.get(&d.id) {
            Some(old) if !d.id.is_empty() => {
                d.created = old.created;
                d.canceled = old.canceled;
            }
            _ => {
                if saved.downtimes.len() >= DOWNTIMES {
                    return Err(format!("{DOWNTIMES} downtimes at the most"));
                }
                d.id = super::fresh_id("dt", |id| saved.downtimes.contains_key(id));
                d.created = now;
                d.canceled = None;
            }
        }
        d.modified = now;
        d.check()?;
        saved.downtimes.insert(d.id.clone(), d.clone());
        self.save(&saved)?;
        Ok(d)
    }

    /// A downtime canceled (kept, its status `canceled`): whether there was one.
    pub fn cancel(&self, id: &str) -> Result<bool, String> {
        let now = crate::util::now_ms();
        let mut saved = lock(&self.saved);
        let Some(d) = saved.downtimes.get_mut(id) else { return Ok(false) };
        if d.canceled.is_none() {
            d.canceled = Some(now);
            d.modified = now;
        }
        self.save(&saved)?;
        Ok(true)
    }

    /// The downtime that holds `group` of monitor `id` (of `tags`) at `now`, if one does.
    pub fn holding(&self, id: &str, tags: &[String], group: &[(String, String)], now: i64) -> Option<String> {
        lock(&self.saved).downtimes.values().find(|d| d.holds(id, tags, group, now)).map(|d| d.id.clone())
    }

    /// The downtimes that name monitor `id` (of `tags`) and hold now or will.
    pub fn of_monitor(&self, id: &str, tags: &[String], now: i64) -> Vec<Downtime> {
        lock(&self.saved).downtimes.values().filter(|d| d.names(id, tags) && d.window(now).is_some()).cloned().collect()
    }

    /// The windows that ended since last asked: each once, expired or canceled.
    pub fn ended(&self, now: i64) -> Vec<Ended> {
        let all: Vec<Downtime> = lock(&self.saved).downtimes.values().cloned().collect();
        let mut holding = lock(&self.holding);
        let mut out = Vec::new();
        for d in &all {
            let now_active = d.active(now);
            match (holding.get(&d.id).copied(), now_active) {
                // Still the same window.
                (Some(w), Some(a)) if w == a => {}
                // A window ended (and maybe the next begun): expired, or canceled while it held.
                (Some(w), next) => {
                    let how = if d.canceled.is_some_and(|c| c <= now && c < w.1) { "canceled" } else { "expired" };
                    out.push(Ended { downtime: d.clone(), how });
                    match next {
                        Some(a) => holding.insert(d.id.clone(), a),
                        None => holding.remove(&d.id),
                    };
                }
                (None, Some(a)) => {
                    holding.insert(d.id.clone(), a);
                }
                (None, None) => {}
            }
        }
        // Downtimes deleted while they held: forgotten.
        holding.retain(|id, _| all.iter().any(|d| &d.id == id));
        out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn dt(json: serde_json::Value) -> Downtime {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn a_downtime_holds_the_groups_of_the_monitors_it_names_while_it_is_active() {
        let d = dt(serde_json::json!({ "monitor_identifier": { "monitor_tags": ["team:pay"] }, "scope": "env:prod", "schedule": { "start": 1_000, "end": "1970-01-01T00:00:05Z" } }));
        d.check().unwrap();
        let tags = vec!["team:pay".to_owned(), "svc:api".to_owned()];
        let prod = vec![("env".to_owned(), "prod".to_owned())];
        assert!(d.holds("m1", &tags, &prod, 1_000) && d.holds("m1", &tags, &prod, 4_999));
        assert!(!d.holds("m1", &tags, &prod, 5_000), "its end is its end");
        assert!(!d.holds("m1", &tags, &[("env".to_owned(), "dev".to_owned())], 2_000), "not its scope's");
        assert!(!d.holds("m1", &["svc:api".to_owned()], &prod, 2_000), "not a monitor it names");
        assert_eq!((d.status(500), d.status(2_000), d.status(6_000)), ("scheduled", "active", "ended"));
        for bad in [
            serde_json::json!({ "monitor_identifier": {}, "schedule": {} }),
            serde_json::json!({ "monitor_identifier": { "monitor_id": "m1", "monitor_tags": ["a:b"] }, "schedule": {} }),
            serde_json::json!({ "monitor_identifier": { "monitor_id": "m1" }, "schedule": { "start": 5, "end": 5 } }),
            serde_json::json!({ "monitor_identifier": { "monitor_id": "m1" }, "scope": "host", "schedule": {} }),
            serde_json::json!({ "monitor_identifier": { "monitor_id": "m1" }, "schedule": { "recurrences": [{ "rrule": "FREQ=HOURLY", "start": "2026-10-24T02:00", "duration": "1h" }] } }),
            serde_json::json!({ "monitor_identifier": { "monitor_id": "m1" }, "schedule": {}, "notify_end_states": ["ok"] }),
        ] {
            assert!(dt(bad.clone()).check().is_err(), "{bad}");
        }
    }

    #[test]
    fn a_window_s_end_is_told_once_expired_or_canceled() {
        let dir = tempfile::tempdir().unwrap();
        let ds = Downtimes::open(dir.path());
        let now = crate::util::now_ms();
        let a = ds.put(dt(serde_json::json!({ "monitor_identifier": { "monitor_id": "m1" }, "schedule": { "start": now - 1_000, "end": now + 1_000 } }))).unwrap();
        let b = ds.put(dt(serde_json::json!({ "monitor_identifier": { "monitor_id": "m2" }, "schedule": { "start": now - 1_000 } }))).unwrap();
        assert!(ds.ended(now).is_empty(), "both begun, none ended");
        let ended = ds.ended(now + 2_000);
        assert_eq!(ended.iter().map(|e| (e.downtime.id.as_str(), e.how)).collect::<Vec<_>>(), vec![(a.id.as_str(), "expired")]);
        assert!(ds.ended(now + 3_000).is_empty(), "told once");
        assert!(ds.cancel(&b.id).unwrap());
        let ended = ds.ended(crate::util::now_ms() + 1);
        assert_eq!(ended.iter().map(|e| (e.downtime.id.as_str(), e.how)).collect::<Vec<_>>(), vec![(b.id.as_str(), "canceled")]);
        assert_eq!(ds.get(&b.id).unwrap().status(crate::util::now_ms() + 1), "canceled");
        // A recurring one: its windows each end in turn.
        let r = ds.put(dt(serde_json::json!({ "monitor_identifier": { "monitor_id": "m3" }, "schedule": { "recurrences": [{ "rrule": "FREQ=DAILY", "start": "2026-10-19T02:00", "duration": "1h" }], "timezone": "UTC" } }))).unwrap();
        let at = |s: &str| Local::parse(s).unwrap().at(Zone::UTC).unwrap() / 1_000_000;
        assert!(ds.ended(at("2026-10-20T02:30")).iter().all(|e| e.downtime.id != r.id));
        assert_eq!(ds.ended(at("2026-10-20T03:00")).iter().filter(|e| e.downtime.id == r.id).count(), 1);
        assert_eq!(r.window(at("2026-10-20T03:00")), Some((at("2026-10-21T02:00"), at("2026-10-21T03:00"))));
    }
}
