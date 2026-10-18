//! Monitors (`docs/observe.md` §9): what the server keeps of them, evaluates, and says.
//!
//! A monitor is Datadog's: a query (`kiln_obs::monitor`'s syntax), a message (its template, its
//! `@` mentions naming integrations), and options: thresholds (warning, recoveries), NO DATA after
//! a while without data, renotification, an evaluation delay for late data, and how often it is
//! evaluated (every minute, Datadog's, unless it says; ten seconds at the most often). The server,
//! the group's leader, evaluates each on the blocking pool under the store's read lock; each
//! group's change of state is an event (`source:kiln.event @event.type:monitor.<state>`) and, unless
//! the monitor is muted, its message to every integration it mentions; a group that stays in
//! trouble is told again each renotification interval.
//!
//! Definitions are `<observe>/monitors.json`, states `<observe>/monitor-states.json` (saved as they
//! change, so a restart neither forgets an alert nor says it again).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kiln_obs::monitor::{self, State, Thresholds, Transition, Vars};
use serde::{Deserialize, Serialize};

use super::notify::{Integrations, Notice, Notifier};

/// How often a monitor is evaluated unless it says: Datadog's metric monitors' minute.
const EVERY_S: u32 = 60;
/// The most often it may be: ten seconds, kiln's finest resolution for most series.
const EVERY_MIN_S: u32 = 10;
/// Groups a monitor keeps states for at the most (an answer's series' bound), and how long a group
/// with no data at all is kept.
const GROUPS: usize = 10_000;
const FORGET_MS: i64 = 86_400_000;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThresholdsIn {
    pub critical: Option<f64>,
    pub warning: Option<f64>,
    pub critical_recovery: Option<f64>,
    pub warning_recovery: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Options {
    pub thresholds: ThresholdsIn,
    pub notify_no_data: bool,
    /// Minutes without data before NO DATA (two windows unless it says).
    pub no_data_timeframe: Option<u32>,
    /// Minutes between notifications while a group stays in trouble (none: once).
    pub renotify_interval: Option<u32>,
    /// Seconds the window ends before now: what late data is given.
    pub evaluation_delay: Option<u32>,
    /// Seconds between evaluations.
    pub every: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Monitor {
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub query: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub priority: Option<u8>,
    #[serde(default)]
    pub options: Options,
    /// Unix ms until which it says nothing (`i64::MAX`: until unmuted).
    #[serde(default)]
    pub muted_until: Option<i64>,
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub modified: i64,
}

impl Monitor {
    /// Its query parsed and its thresholds, the query's critical one and the options' others (the
    /// options' critical must be the query's, as Datadog requires).
    pub fn compiled(&self) -> Result<(monitor::Query, Thresholds), String> {
        let q = monitor::parse(&self.query).map_err(|e| e.to_string())?;
        let t = &self.options.thresholds;
        if let Some(c) = t.critical
            && c != q.threshold
        {
            return Err(format!("the critical threshold {c} is not the query's {}", q.threshold));
        }
        if let Some(e) = self.options.every
            && e < EVERY_MIN_S
        {
            return Err(format!("evaluated every {e} s: at most every {EVERY_MIN_S}"));
        }
        Ok((q.clone(), Thresholds { critical: q.threshold, warning: t.warning, critical_recovery: t.critical_recovery, warning_recovery: t.warning_recovery }))
    }

    fn window_ms(q: &monitor::Query) -> i64 {
        match &q.kind {
            monitor::Kind::Metric { window_ms, .. } | monitor::Kind::Change { window_ms, .. } | monitor::Kind::Logs { window_ms, .. } => *window_ms,
        }
    }

    pub fn muted(&self, now: i64) -> bool {
        self.muted_until.is_some_and(|u| u > now)
    }
}

/// A group's state, as the monitor last found it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupState {
    pub group: Vec<(String, String)>,
    pub state: String,
    pub value: Option<f64>,
    /// Unix ms: since when it is in this state, when it last had data, when it was last told.
    pub since: i64,
    pub last_data: i64,
    pub notified: i64,
}

#[derive(Default)]
struct Due {
    next: HashMap<String, i64>,
}

pub struct Monitors {
    dir: PathBuf,
    defs: Mutex<BTreeMap<String, Monitor>>,
    states: Mutex<BTreeMap<String, BTreeMap<String, GroupState>>>,
    pub integrations: Integrations,
    notifier: Notifier,
    link_base: String,
    due: Mutex<Due>,
}

fn group_key(g: &[(String, String)]) -> String {
    g.iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join(",")
}

impl Monitors {
    pub fn open(dir: &Path, http: Arc<crate::http::client::Client>, link_base: &str) -> Arc<Self> {
        let load = |name: &str| std::fs::read(dir.join(name)).ok();
        let defs = load("monitors.json").and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let states = load("monitor-states.json").and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Arc::new(Self { dir: dir.to_owned(), defs: Mutex::new(defs), states: Mutex::new(states), integrations: Integrations::open(dir), notifier: Notifier::start(http), link_base: link_base.trim_end_matches('/').to_owned(), due: Mutex::new(Due::default()) })
    }

    fn save(&self) -> anyhow::Result<()> {
        let defs = serde_json::to_vec_pretty(&*lock(&self.defs))?;
        crate::sync::group::save_atomic(&self.dir.join("monitors.json"), &defs)?;
        Ok(())
    }

    fn save_states(&self) {
        let Ok(b) = serde_json::to_vec(&*lock(&self.states)) else { return };
        if let Err(e) = crate::sync::group::save_atomic(&self.dir.join("monitor-states.json"), &b) {
            tracing::warn!(error = %e, "monitors' states were not saved");
        }
    }

    pub fn list(&self) -> Vec<Monitor> {
        lock(&self.defs).values().cloned().collect()
    }

    pub fn get(&self, id: &str) -> Option<Monitor> {
        lock(&self.defs).get(id).cloned()
    }

    /// Creates or replaces a monitor; its query and options checked first.
    pub fn put(&self, mut m: Monitor) -> Result<Monitor, String> {
        m.compiled()?;
        if m.name.trim().is_empty() {
            return Err("a monitor's name".into());
        }
        let now = crate::util::now_ms();
        let mut defs = lock(&self.defs);
        if m.id.is_empty() {
            m.id = super::fresh_id("m", |id| defs.contains_key(id));
            m.created = now;
        } else if let Some(old) = defs.get(&m.id) {
            m.created = old.created;
            // A changed query starts its groups over.
            if old.query != m.query {
                lock(&self.states).remove(&m.id);
            }
        } else {
            m.created = now;
        }
        m.modified = now;
        defs.insert(m.id.clone(), m.clone());
        drop(defs);
        lock(&self.due).next.remove(&m.id);
        self.save().map_err(|e| format!("{e:#}"))?;
        Ok(m)
    }

    pub fn delete(&self, id: &str) -> Result<bool, String> {
        let had = lock(&self.defs).remove(id).is_some();
        lock(&self.states).remove(id);
        self.save().map_err(|e| format!("{e:#}"))?;
        self.save_states();
        Ok(had)
    }

    pub fn mute(&self, id: &str, until: Option<i64>) -> Result<Option<Monitor>, String> {
        let mut defs = lock(&self.defs);
        let Some(m) = defs.get_mut(id) else { return Ok(None) };
        m.muted_until = until;
        let m = m.clone();
        drop(defs);
        self.save().map_err(|e| format!("{e:#}"))?;
        Ok(Some(m))
    }

    pub fn groups(&self, id: &str) -> Vec<GroupState> {
        lock(&self.states).get(id).map(|g| g.values().cloned().collect()).unwrap_or_default()
    }

    /// The monitor's overall state: its worst group's.
    pub fn overall(&self, id: &str) -> &'static str {
        let worst = lock(&self.states).get(id).and_then(|g| g.values().filter_map(|s| State::parse(&s.state)).max_by_key(|s| match s {
            State::Ok => 0,
            State::NoData => 1,
            State::Warn => 2,
            State::Alert => 3,
        }));
        worst.unwrap_or(State::Ok).as_str()
    }

    /// Evaluates every monitor due by `now`.
    fn tick(self: &Arc<Self>, store: &kiln_obs::store::Store, now: i64) {
        let due: Vec<Monitor> = {
            let mut d = lock(&self.due);
            lock(&self.defs)
                .values()
                .filter(|m| d.next.get(&m.id).is_none_or(|&t| t <= now))
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
                .inspect(|m| {
                    d.next.insert(m.id.clone(), now + i64::from(m.options.every.unwrap_or(EVERY_S).max(EVERY_MIN_S)) * 1000);
                })
                .collect()
        };
        let mut changed = false;
        for m in due {
            changed |= self.evaluate(store, &m, now);
        }
        if changed {
            self.save_states();
        }
    }

    /// One monitor's evaluation: each group's next state, events and notifications for changes.
    /// Whether any state changed.
    fn evaluate(&self, store: &kiln_obs::store::Store, m: &Monitor, now: i64) -> bool {
        let Ok((q, t)) = m.compiled() else { return false };
        let delay = i64::from(m.options.evaluation_delay.unwrap_or(0)) * 1000;
        let values = match monitor::evaluate(store, &q, now, delay) {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(monitor = %m.name, error = %e, "a monitor did not evaluate");
                return false;
            }
        };
        let no_data_ms = m.options.no_data_timeframe.map_or(2 * Monitor::window_ms(&q), |mins| i64::from(mins) * 60_000);
        let renotify_ms = m.options.renotify_interval.map(|mins| i64::from(mins) * 60_000).filter(|&r| r > 0);
        let mut transitions: Vec<(GroupState, State)> = Vec::new();
        let mut renotify: Vec<GroupState> = Vec::new();
        let mut changed = false;
        {
            let mut all = lock(&self.states);
            let groups = all.entry(m.id.clone()).or_default();
            let mut seen: Vec<String> = Vec::with_capacity(values.len());
            for v in &values {
                let key = group_key(&v.group);
                seen.push(key.clone());
                if !groups.contains_key(&key) && groups.len() >= GROUPS {
                    continue;
                }
                let g = groups.entry(key).or_insert_with(|| GroupState { group: v.group.clone(), state: State::Ok.as_str().into(), value: None, since: now, last_data: now, notified: 0 });
                let prev = State::parse(&g.state).unwrap_or(State::Ok);
                let next = monitor::next(prev, Some(v.value), q.op, &t, m.options.notify_no_data);
                g.value = Some(v.value);
                g.last_data = now;
                if next != prev {
                    transitions.push((g.clone(), prev));
                    g.state = next.as_str().into();
                    g.since = now;
                    changed = true;
                } else if next != State::Ok && renotify_ms.is_some_and(|r| now - g.notified >= r) {
                    renotify.push(g.clone());
                }
            }
            // Groups without data now: NO DATA after the timeframe, when asked; forgotten after a day.
            groups.retain(|key, g| seen.contains(key) || now - g.last_data < FORGET_MS);
            for (key, g) in groups.iter_mut() {
                if seen.contains(key) || now - g.last_data < no_data_ms {
                    continue;
                }
                let prev = State::parse(&g.state).unwrap_or(State::Ok);
                let next = monitor::next(prev, None, q.op, &t, m.options.notify_no_data);
                if next != prev {
                    transitions.push((g.clone(), prev));
                    g.state = next.as_str().into();
                    g.since = now;
                    g.value = None;
                    changed = true;
                }
            }
        }
        for (g, prev) in transitions {
            // The group as it is now (its new state and value).
            let now_g = lock(&self.states).get(&m.id).and_then(|gs| gs.get(&group_key(&g.group)).cloned()).unwrap_or(g);
            let next = State::parse(&now_g.state).unwrap_or(State::Ok);
            self.say(m, &q, &t, &now_g, prev, next, now);
        }
        for g in renotify {
            let s = State::parse(&g.state).unwrap_or(State::Ok);
            self.say(m, &q, &t, &g, s, s, now);
        }
        changed
    }

    /// A group's change of state (or its renotification): its event, and, unless the monitor is
    /// muted, its message to every integration the message mentions.
    #[allow(clippy::too_many_arguments)]
    fn say(&self, m: &Monitor, q: &monitor::Query, t: &Thresholds, g: &GroupState, from: State, to: State, now: i64) {
        let link = format!("{}/observe/monitors/{}", self.link_base, m.id);
        let tr = Transition { from, to };
        let vars = Vars { monitor: &m.name, value: g.value, threshold: q.threshold, warn_threshold: t.warning, group: &g.group, link: &link };
        let message = monitor::render(&m.message, tr, &vars);
        let muted = m.muted(now);
        if let Some(o) = super::api::get() {
            let mut attrs: Vec<(String, String)> = vec![
                ("event.type".into(), format!("monitor.{}", to.as_str().to_ascii_lowercase().replace(' ', "_"))),
                ("monitor_id".into(), m.id.clone()),
                ("monitor".into(), m.name.clone()),
                ("state".into(), to.as_str().into()),
                ("previous".into(), from.as_str().into()),
                ("muted".into(), muted.to_string()),
            ];
            if let Some(v) = g.value {
                attrs.push(("value".into(), v.to_string()));
            }
            for (k, v) in &g.group {
                attrs.push((format!("group.{k}"), v.clone()));
            }
            let severity = match to {
                State::Alert => 17,
                State::Warn | State::NoData => 13,
                State::Ok => 9,
            };
            let title = format!("{} {} → {}{}", m.name, from.as_str(), to.as_str(), if g.group.is_empty() { String::new() } else { format!(" on {}", group_key(&g.group)) });
            o.log(vec![kiln_obs::logs::record::Record { ts: now.saturating_mul(1_000_000), severity, service: "kiln.monitor".into(), host: String::new(), source: "kiln.event".into(), message: format!("{title}: {message}"), attrs, trace_id: None, span_id: None }]);
            if let Ok(k) = kiln_obs::batch::SeriesKey::new("kiln.monitor.transitions", [("monitor_id", m.id.as_str()), ("state", to.as_str())], kiln_obs::batch::Kind::Count) {
                o.write(vec![(k, vec![(now, 1.0)])]);
            }
        }
        if muted {
            return;
        }
        let notice = Notice { monitor_id: m.id.clone(), monitor: m.name.clone(), state: to.as_str(), previous: from.as_str(), group: g.group.clone(), value: g.value, threshold: q.threshold, message: message.clone(), link, at: now, priority: m.priority, tags: m.tags.clone() };
        for mention in monitor::mentions(&message) {
            match self.integrations.mentioned(&mention) {
                Some((name, i)) => self.notifier.send(&name, i, notice.clone()),
                None => tracing::debug!(monitor = %m.name, %mention, "a mention names no integration"),
            }
        }
        if let Some(gs) = lock(&self.states).get_mut(&m.id).and_then(|gs| gs.get_mut(&group_key(&g.group))) {
            gs.notified = now;
        }
    }

    /// A test notification to integration `name`.
    pub fn test(&self, name: &str) -> Result<(), String> {
        let i = self.integrations.get(name).ok_or_else(|| format!("no integration {name}"))?;
        let now = crate::util::now_ms();
        let notice = Notice { monitor_id: "test".into(), monitor: "A test from kiln".into(), state: "ALERT", previous: "OK", group: vec![("host".into(), "kiln".into())], value: Some(1.0), threshold: 0.0, message: "This is a test notification from kiln's monitors.".into(), link: format!("{}/observe/monitors", self.link_base), at: now, priority: None, tags: Vec::new() };
        self.notifier.send(name, i, notice);
        Ok(())
    }

    /// Evaluates due monitors every second, on the blocking pool, until the process ends.
    pub async fn run(self: Arc<Self>, shutdown: crate::rt::Shutdown) {
        while shutdown.sleep(Duration::from_secs(1)).await {
            let Some(o) = super::api::get() else { continue };
            // The leader evaluates; a replica that does not lead says nothing.
            let v = o.view();
            if v.leader == 0 || v.leader != v.member {
                continue;
            }
            let Some(store) = o.store() else { continue };
            let me = self.clone();
            let _ = crate::rt::blocking(move || {
                let s = store.read().unwrap_or_else(PoisonError::into_inner);
                me.tick(&s, crate::util::now_ms());
            })
            .await;
        }
    }
}
