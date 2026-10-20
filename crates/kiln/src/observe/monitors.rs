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
//! A composite (`type: "composite"`, Datadog's) reads other monitors' states, its query an
//! expression of their ids (`kiln_obs::monitor::composite`): evaluated right after any monitor it
//! reads, and at its own interval besides. A monitor a composite reads is not deleted while it does.
//!
//! Definitions are `<observe>/monitors.json`, states `<observe>/monitor-states.json` (saved as they
//! change, so a restart neither forgets an alert nor says it again).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kiln_obs::monitor::composite::{self, Composite, Group};
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

/// Datadog's type for a composite monitor.
pub const COMPOSITE: &str = "composite";
/// Datadog's types of the monitors whose kind their query says, as its API names them.
pub const TYPES: [&str; 6] = ["metric alert", "query alert", "log alert", "trace-analytics alert", "slo alert", "error-tracking alert"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Monitor {
    #[serde(default)]
    pub id: String,
    pub name: String,
    /// Datadog's monitor type: [`COMPOSITE`] for a composite, its query an expression of other
    /// monitors' ids; none (or one of [`TYPES`]) for the rest, whose kind their query says.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
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

/// An SLO monitor's value (`monitor::Kind::Slo`): the lesser of its SLO's burn rates over the long
/// window and the short (both must pass for it to; none while either has no data), or its error
/// budget left, percent.
fn slo_values(store: &kiln_obs::store::Store, id: &str, burn: Option<(i64, i64)>, now: i64) -> Result<Vec<monitor::Value>, monitor::MonitorError> {
    let def = super::api::slo_def(id).ok_or_else(|| monitor::MonitorError::Eval(format!("no SLO {id}")))?;
    let slo = def.slo().map_err(monitor::MonitorError::Eval)?;
    let eval = |e: kiln_obs::slo::SloError| monitor::MonitorError::Eval(e.to_string());
    let value = match burn {
        Some((long, short)) => match (kiln_obs::slo::burn_rate(store, &slo, long, now).map_err(eval)?, kiln_obs::slo::burn_rate(store, &slo, short, now).map_err(eval)?) {
            (Some(l), Some(s)) => Some(l.min(s)),
            _ => None,
        },
        None => kiln_obs::slo::status(store, &slo, now).map_err(eval)?.budget_left,
    };
    Ok(value.map(|v| vec![monitor::Value { group: Vec::new(), value: v }]).unwrap_or_default())
}

/// An Error Tracking monitor's values (`monitor::Kind::ErrorTracking`): its issues' errors over
/// its window (`super::errors::Issues::monitor_values`), of the issues it watches since `since`
/// (when it was made or last changed).
fn error_values(store: &kiln_obs::store::Store, search: &str, new: bool, by: &[String], window_ms: i64, since: i64, now: i64) -> Result<Vec<monitor::Value>, monitor::MonitorError> {
    let eval = |e: String| monitor::MonitorError::Eval(e);
    let issues = super::api::issues().map_err(|_| eval("Error Tracking's issues are kept by the server".into()))?;
    let search = kiln_obs::logs::search::parse(search).map_err(|e| eval(e.to_string()))?;
    let from = now.saturating_sub(window_ms);
    issues.monitor_values(&search, new, by, since, now, |few| {
        match few {
            Some(ids) => kiln_obs::errors::seen_of(store, ids, from, now),
            None => kiln_obs::errors::seen(store, &[], from, now),
        }
        .map_err(|e| eval(e.to_string()))
    })
}

/// The issue an Error Tracking monitor's group is of (none for other monitors' groups).
fn issue_of(m: &Monitor, g: &GroupState) -> Option<super::errors::Issue> {
    if !matches!(m.compiled(), Ok(Compiled::Query(q, _)) if matches!(q.kind, monitor::Kind::ErrorTracking { .. })) {
        return None;
    }
    let id = g.group.iter().find(|(k, _)| k == "issue.id").map(|(_, v)| v.as_str())?;
    super::api::issues().ok()?.get(id)
}

/// An issue's variables for its monitor's message, Datadog's (`issue.attributes.error.*`): its
/// type, message and file, and its latest error's stack when the message asks for it (a search);
/// what only a browser's or a phone's error says (`is_crash`, `category`, `handling`) nothing.
fn issue_vars(store: &kiln_obs::store::Store, i: &super::errors::Issue, template: &str, now: i64) -> Vec<(String, String)> {
    let var = |k: &str, v: &str| (format!("issue.attributes.error.{k}"), v.to_owned());
    let mut out = vec![var("type", &i.kind), var("message", &i.message), var("file", &i.file), var("is_crash", ""), var("category", ""), var("handling", "")];
    if template.contains("issue.attributes.error.stack") {
        out.push(var("stack", &super::errors::latest_stack(store, i, now).unwrap_or_default()));
    }
    out
}

/// A monitor, compiled: a query of the store and its thresholds, or a composite of other monitors.
#[derive(Debug, Clone)]
pub enum Compiled {
    Query(monitor::Query, Thresholds),
    Composite(Composite),
}

impl Monitor {
    pub fn is_composite(&self) -> bool {
        self.kind.as_deref() == Some(COMPOSITE)
    }

    /// Its query parsed and its thresholds, the query's critical one and the options' others (the
    /// options' critical must be the query's, as Datadog requires); a composite's expression.
    pub fn compiled(&self) -> Result<Compiled, String> {
        if let Some(e) = self.options.every
            && e < EVERY_MIN_S
        {
            return Err(format!("evaluated every {e} s: at most every {EVERY_MIN_S}"));
        }
        match self.kind.as_deref() {
            Some(COMPOSITE) => return composite::parse(&self.query).map(Compiled::Composite).map_err(|e| e.to_string()),
            None => {}
            Some(t) if TYPES.contains(&t) => {}
            Some(other) => return Err(format!("a monitor's type: {other} ({COMPOSITE}, {}, or none: the query says)", TYPES.join(", "))),
        }
        let q = monitor::parse(&self.query).map_err(|e| e.to_string())?;
        if matches!(q.kind, monitor::Kind::ErrorTracking { .. }) && self.options.notify_no_data {
            return Err("an Error Tracking monitor has no NO DATA: an issue without errors in its window counts none".into());
        }
        let t = &self.options.thresholds;
        if let Some(c) = t.critical
            && c != q.threshold
        {
            return Err(format!("the critical threshold {c} is not the query's {}", q.threshold));
        }
        let t = Thresholds { critical: q.threshold, warning: t.warning, critical_recovery: t.critical_recovery, warning_recovery: t.warning_recovery };
        Ok(Compiled::Query(q, t))
    }

    /// The monitors a composite reads (none for the rest).
    pub fn reads(&self) -> Vec<String> {
        if !self.is_composite() {
            return Vec::new();
        }
        composite::parse(&self.query).map(|c| c.ids).unwrap_or_default()
    }

    fn window_ms(q: &monitor::Query) -> i64 {
        match &q.kind {
            monitor::Kind::Metric { window_ms, .. }
            | monitor::Kind::Change { window_ms, .. }
            | monitor::Kind::Logs { window_ms, .. }
            | monitor::Kind::Spans { window_ms, .. }
            | monitor::Kind::ErrorTracking { window_ms, .. } => *window_ms,
            // Ahead, not behind: an hour without a forecast is two "windows" without data.
            monitor::Kind::Forecast { .. } => 3_600_000,
            monitor::Kind::Slo { burn, .. } => burn.map_or(3_600_000, |b| b.0),
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
    /// A downtime that held it ended with it in trouble, asking that its first recovery after say
    /// nothing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub quiet_recovery: bool,
}

/// What a monitor's messages say of its thresholds (a composite has none: NaN, null in a notice).
#[derive(Debug, Clone, Copy)]
struct Said {
    threshold: f64,
    warning: Option<f64>,
}

/// What a monitor's messages say of its thresholds, as its definition has them.
fn said_of(m: &Monitor) -> Said {
    match m.compiled() {
        Ok(Compiled::Query(q, t)) => Said { threshold: q.threshold, warning: t.warning },
        _ => Said { threshold: f64::NAN, warning: None },
    }
}

/// Why a monitor was not deleted.
#[derive(Debug)]
pub enum Refused {
    /// Composites read it (their names).
    Read(Vec<String>),
    Io(String),
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
    /// Downtimes (`super::downtimes`): when what monitors say is held.
    pub downtimes: super::downtimes::Downtimes,
}

fn group_key(g: &[(String, String)]) -> String {
    g.iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join(",")
}

impl Monitors {
    pub fn open(dir: &Path, http: Arc<crate::http::client::Client>, link_base: &str) -> Arc<Self> {
        let load = |name: &str| std::fs::read(dir.join(name)).ok();
        let defs = load("monitors.json").and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let states = load("monitor-states.json").and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Arc::new(Self {
            dir: dir.to_owned(),
            defs: Mutex::new(defs),
            states: Mutex::new(states),
            integrations: Integrations::open(dir),
            notifier: Notifier::start(http),
            link_base: link_base.trim_end_matches('/').to_owned(),
            due: Mutex::new(Due::default()),
            downtimes: super::downtimes::Downtimes::open(dir),
        })
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

    /// Creates or replaces a monitor; its query and options checked first, and a composite's
    /// monitors (each there, none a composite, not itself).
    pub fn put(&self, mut m: Monitor) -> Result<Monitor, String> {
        let compiled = m.compiled()?;
        if m.name.trim().is_empty() {
            return Err("a monitor's name".into());
        }
        let now = crate::util::now_ms();
        let mut defs = lock(&self.defs);
        if let Compiled::Composite(c) = &compiled {
            for id in &c.ids {
                match defs.get(id) {
                    _ if *id == m.id => return Err("a composite does not read itself".into()),
                    None => return Err(format!("no monitor {id}")),
                    Some(o) if o.is_composite() => return Err(format!("{} is a composite: a composite reads other kinds of monitor", o.name)),
                    Some(_) => {}
                }
            }
        }
        // A monitor composites read cannot become one: they read other kinds.
        if m.is_composite() && !m.id.is_empty() {
            let readers: Vec<&str> = defs.values().filter(|c| c.reads().contains(&m.id)).map(|c| c.name.as_str()).collect();
            if !readers.is_empty() {
                return Err(format!("composites read it ({}): it cannot be one", readers.join(", ")));
            }
        }
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

    /// Deletes a monitor, unless a composite reads it (then `Refused::Read`, naming them).
    pub fn delete(&self, id: &str) -> Result<bool, Refused> {
        let mut defs = lock(&self.defs);
        let readers: Vec<String> = defs.values().filter(|c| c.reads().iter().any(|r| r == id)).map(|c| c.name.clone()).collect();
        if !readers.is_empty() {
            return Err(Refused::Read(readers));
        }
        let had = defs.remove(id).is_some();
        drop(defs);
        lock(&self.states).remove(id);
        self.save().map_err(|e| Refused::Io(format!("{e:#}")))?;
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
        // Queries first: composites read what they found.
        let mut evaluated: HashSet<&str> = HashSet::new();
        for m in due.iter().filter(|m| !m.is_composite()) {
            changed |= self.evaluate(store, m, now);
            evaluated.insert(m.id.as_str());
        }
        let composites: Vec<Monitor> = lock(&self.defs).values().filter(|m| m.is_composite()).cloned().collect();
        for c in composites {
            let Ok(Compiled::Composite(x)) = c.compiled() else { continue };
            if due.iter().any(|d| d.id == c.id) || x.ids.iter().any(|id| evaluated.contains(id.as_str())) {
                changed |= self.evaluate_composite(store, &c, &x, now);
            }
        }
        changed |= self.tell_ends(store, now);
        if changed {
            self.save_states();
        }
    }

    /// Downtimes' windows that ended since last looked at: each group one held that is in one of
    /// its end states told once (as a renotification), and asked, if the downtime says so, to keep
    /// its first recovery quiet. Whether any state changed.
    fn tell_ends(&self, store: &kiln_obs::store::Store, now: i64) -> bool {
        let ended = self.downtimes.ended(now);
        if ended.is_empty() {
            return false;
        }
        let defs: Vec<Monitor> = lock(&self.defs).values().cloned().collect();
        let mut changed = false;
        for e in ended {
            let d = &e.downtime;
            let Ok(scope) = kiln_obs::monitor::downtime::Scope::parse(&d.scope) else { continue };
            let tell = d.notify_end_types.iter().any(|t| t == e.how);
            for m in defs.iter().filter(|m| d.names(&m.id, &m.tags)) {
                let groups: Vec<GroupState> = lock(&self.states).get(&m.id).map(|gs| gs.values().filter(|g| scope.holds(&g.group)).cloned().collect()).unwrap_or_default();
                for g in groups {
                    let state = State::parse(&g.state).unwrap_or(State::Ok);
                    if state == State::Ok {
                        continue;
                    }
                    if d.mute_first_recovery_notification
                        && let Some(gs) = lock(&self.states).get_mut(&m.id).and_then(|gs| gs.get_mut(&group_key(&g.group)))
                    {
                        gs.quiet_recovery = true;
                        changed = true;
                    }
                    let named = match state {
                        State::Alert => "alert",
                        State::Warn => "warn",
                        State::NoData => "no data",
                        State::Ok => "ok",
                    };
                    if tell && d.notify_end_states.iter().any(|s| s == named) {
                        tracing::debug!(monitor = %m.name, downtime = %d.id, how = e.how, group = %group_key(&g.group), "a downtime ended with a group in trouble: told");
                        self.say(store, m, said_of(m), &g, state, state, now);
                    }
                }
            }
        }
        changed
    }

    /// One monitor's evaluation: each group's next state, events and notifications for changes.
    /// Whether any state changed.
    fn evaluate(&self, store: &kiln_obs::store::Store, m: &Monitor, now: i64) -> bool {
        let Ok(Compiled::Query(q, t)) = m.compiled() else { return false };
        let delay = i64::from(m.options.evaluation_delay.unwrap_or(0)) * 1000;
        let values = match &q.kind {
            monitor::Kind::Slo { id, burn } => slo_values(store, id, *burn, now - delay),
            monitor::Kind::ErrorTracking { search, new, by, window_ms } => error_values(store, search, *new, by, *window_ms, m.modified, now - delay),
            _ => monitor::evaluate(store, &q, now, delay),
        };
        let values = match values {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(monitor = %m.name, error = %e, "a monitor did not evaluate");
                return false;
            }
        };
        let no_data_ms = m.options.no_data_timeframe.map_or(2 * Monitor::window_ms(&q), |mins| i64::from(mins) * 60_000);
        let mut found: Vec<(Group, Option<f64>, State)> = values.into_iter().map(|v| (v.group, Some(v.value), State::Ok)).collect();
        // An Error Tracking monitor's group in trouble that it no longer finds (its issue resolved,
        // ignored, no longer new, or without errors in the window) counts none, and recovers: a
        // count of a group gone quiet is zero, as Datadog evaluates its event monitors' groups.
        if matches!(q.kind, monitor::Kind::ErrorTracking { .. }) {
            let have: HashSet<&Group> = found.iter().map(|(g, _, _)| g).collect();
            let gone: Vec<Group> =
                lock(&self.states).get(&m.id).map(|gs| gs.values().filter(|g| g.state != State::Ok.as_str() && !have.contains(&g.group)).map(|g| g.group.clone()).collect()).unwrap_or_default();
            found.extend(gone.into_iter().map(|g| (g, Some(0.0), State::Ok)));
        }
        let said = Said { threshold: q.threshold, warning: t.warning };
        self.apply(store, m, now, no_data_ms, found, |prev, value, _| monitor::next(prev, value, q.op, &t, m.options.notify_no_data), said)
    }

    /// A composite's evaluation: its groups' states from the states of the monitors it reads, as
    /// they last found them. A group that comes out NO DATA keeps its last state, unless the
    /// composite asks to be told (Datadog's "last known state").
    fn evaluate_composite(&self, store: &kiln_obs::store::Store, m: &Monitor, c: &Composite, now: i64) -> bool {
        let inputs: Vec<Vec<(Group, State)>> = {
            let all = lock(&self.states);
            c.ids.iter().map(|id| all.get(id).map(|gs| gs.values().map(|g| (g.group.clone(), State::parse(&g.state).unwrap_or(State::Ok))).collect()).unwrap_or_default()).collect()
        };
        let Some(results) = composite::evaluate(c, &inputs) else { return false };
        let every_ms = i64::from(m.options.every.unwrap_or(EVERY_S).max(EVERY_MIN_S)) * 1000;
        let no_data_ms = m.options.no_data_timeframe.map_or(2 * every_ms, |mins| i64::from(mins) * 60_000);
        let notify = m.options.notify_no_data;
        let found: Vec<(Group, Option<f64>, State)> = results.into_iter().map(|(g, s)| (g, None, s)).collect();
        self.apply(store, m, now, no_data_ms, found, |prev, _, s| if s == State::NoData && !notify { prev } else { s }, Said { threshold: f64::NAN, warning: None })
    }

    /// What an evaluation found, applied: each group found moves from its last state to what
    /// `next(last, its value, what the evaluation made of it)` says; groups not found go to NO DATA
    /// after `no_data_ms` when the monitor asks to be told, and are forgotten after a day. Changes
    /// are said, and groups in trouble said again when due. Whether any state changed.
    #[allow(clippy::too_many_arguments)]
    fn apply(
        &self,
        store: &kiln_obs::store::Store,
        m: &Monitor,
        now: i64,
        no_data_ms: i64,
        found: Vec<(Group, Option<f64>, State)>,
        next: impl Fn(State, Option<f64>, State) -> State,
        said: Said,
    ) -> bool {
        let renotify_ms = m.options.renotify_interval.map(|mins| i64::from(mins) * 60_000).filter(|&r| r > 0);
        let mut transitions: Vec<(GroupState, State)> = Vec::new();
        let mut renotify: Vec<GroupState> = Vec::new();
        let mut changed = false;
        {
            let mut all = lock(&self.states);
            let groups = all.entry(m.id.clone()).or_default();
            let mut seen: HashSet<String> = HashSet::with_capacity(found.len());
            for (group, value, made) in found {
                let key = group_key(&group);
                if !groups.contains_key(&key) && groups.len() >= GROUPS {
                    continue;
                }
                seen.insert(key.clone());
                let g = groups.entry(key).or_insert_with(|| GroupState { group, state: State::Ok.as_str().into(), value: None, since: now, last_data: now, notified: 0, quiet_recovery: false });
                let prev = State::parse(&g.state).unwrap_or(State::Ok);
                let to = next(prev, value, made);
                g.value = value;
                g.last_data = now;
                if to != prev {
                    transitions.push((g.clone(), prev));
                    g.state = to.as_str().into();
                    g.since = now;
                    changed = true;
                } else if to != State::Ok && renotify_ms.is_some_and(|r| now - g.notified >= r) {
                    renotify.push(g.clone());
                }
            }
            // Groups not found now: NO DATA after the timeframe, when asked; forgotten after a day.
            groups.retain(|key, g| seen.contains(key) || now - g.last_data < FORGET_MS);
            for (key, g) in groups.iter_mut() {
                if seen.contains(key) || now - g.last_data < no_data_ms {
                    continue;
                }
                let prev = State::parse(&g.state).unwrap_or(State::Ok);
                let to = if m.options.notify_no_data { State::NoData } else { prev };
                if to != prev {
                    transitions.push((g.clone(), prev));
                    g.state = to.as_str().into();
                    g.since = now;
                    g.value = None;
                    changed = true;
                }
            }
        }
        for (g, prev) in transitions {
            // The group as it is now (its new state and value).
            let now_g = lock(&self.states).get(&m.id).and_then(|gs| gs.get(&group_key(&g.group)).cloned()).unwrap_or(g);
            let to = State::parse(&now_g.state).unwrap_or(State::Ok);
            self.say(store, m, said, &now_g, prev, to, now);
        }
        for g in renotify {
            let s = State::parse(&g.state).unwrap_or(State::Ok);
            self.say(store, m, said, &g, s, s, now);
        }
        changed
    }

    /// A group's change of state (or its renotification): its event, and, unless the monitor is
    /// muted, its message to every integration the message mentions. An Error Tracking monitor's
    /// group is an issue's: its message given the issue's variables, its link the issue's page,
    /// and nothing said of it while the issue is ignored (Datadog's).
    #[allow(clippy::too_many_arguments)]
    fn say(&self, store: &kiln_obs::store::Store, m: &Monitor, said: Said, g: &GroupState, from: State, to: State, now: i64) {
        let issue = issue_of(m, g);
        let link = match &issue {
            Some(i) => format!("{}/observe/errors/{}", self.link_base, i.id),
            None => format!("{}/observe/monitors/{}", self.link_base, m.id),
        };
        let extra = issue.as_ref().map(|i| issue_vars(store, i, &m.message, now)).unwrap_or_default();
        let tr = Transition { from, to };
        let vars = Vars { monitor: &m.name, value: g.value, threshold: said.threshold, warn_threshold: said.warning, group: &g.group, link: &link, extra: &extra };
        let message = monitor::render(&m.message, tr, &vars);
        let downtime = self.downtimes.holding(&m.id, &m.tags, &g.group, now);
        // A recovery a downtime that ended asked to keep quiet: this one, once.
        let quiet = to == State::Ok && from != State::Ok && {
            let mut all = lock(&self.states);
            let gs = all.get_mut(&m.id).and_then(|gs| gs.get_mut(&group_key(&g.group)));
            gs.is_some_and(|gs| std::mem::take(&mut gs.quiet_recovery))
        };
        let ignored = issue.as_ref().is_some_and(|i| i.status == super::errors::Status::Ignored);
        let muted = m.muted(now) || downtime.is_some() || quiet || ignored;
        if let Some(o) = super::api::get() {
            let mut attrs: Vec<(String, String)> = vec![
                ("event.type".into(), format!("monitor.{}", to.as_str().to_ascii_lowercase().replace(' ', "_"))),
                ("monitor_id".into(), m.id.clone()),
                ("monitor".into(), m.name.clone()),
                ("state".into(), to.as_str().into()),
                ("previous".into(), from.as_str().into()),
                ("muted".into(), muted.to_string()),
            ];
            if let Some(id) = &downtime {
                attrs.push(("downtime_id".into(), id.clone()));
            }
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
        let notice = Notice { monitor_id: m.id.clone(), monitor: m.name.clone(), state: to.as_str(), previous: from.as_str(), group: g.group.clone(), value: g.value, threshold: said.threshold, message: message.clone(), link, at: now, priority: m.priority, tags: m.tags.clone() };
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Datadog's types of monitor are taken (an Error Tracking monitor's among them), another is
    /// not; an Error Tracking monitor asking for NO DATA is refused (an issue without errors counts
    /// none).
    #[test]
    fn datadog_s_types_are_taken_and_an_error_tracking_monitor_has_no_no_data() {
        let m = |kind: Option<&str>, notify_no_data: bool| Monitor {
            id: String::new(),
            name: "new issues".into(),
            kind: kind.map(str::to_owned),
            query: r#"error-tracking("service:shop").source("backend").new().rollup("count").by("issue.id").last("1d") > 0"#.into(),
            message: String::new(),
            tags: Vec::new(),
            priority: None,
            options: Options { notify_no_data, ..Options::default() },
            muted_until: None,
            created: 0,
            modified: 0,
        };
        assert!(m(Some("error-tracking alert"), false).compiled().is_ok());
        assert!(m(None, false).compiled().is_ok());
        assert!(m(Some("rum alert"), false).compiled().is_err());
        assert!(m(Some("error-tracking alert"), true).compiled().is_err());
    }
}
