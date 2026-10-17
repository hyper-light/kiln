//! Drives an [`Engine`] against simulated clusters ([`crate::sim`]) on a simulated clock: the
//! owner kiln's server is, with renders, API requests and watches answered after configurable
//! delays. Tests use it to watch syncs happen; `kiln cd bench` uses it to measure the engine at
//! 10,000 to 1,000,000 apps.
//!
//! Time is simulated, so a ten-minute rollout takes as long as the work in it. The harness counts
//! the wall time spent inside the engine separately from its own, so a benchmark reports what the
//! controller costs, not what the simulator does.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::time::Instant;

use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::Value;

use crate::engine::{Action, Engine, Input, LiveChange, Record, Summary};
use crate::model::{Health, Sync};
use crate::obj::{GroupKind, Str};
use crate::sim::{self, Event};

/// Renders an app's inputs: `(app, revision, key)` to objects.
pub type Renderer = Box<dyn FnMut(&str, &str, u64) -> Result<Vec<Value>, String>>;

/// The engine, its clusters and a clock.
pub struct Harness {
    /// The engine under test.
    pub engine: Engine,
    /// Simulated clusters by name.
    pub clusters: FxHashMap<Str, sim::Cluster>,
    /// Simulated time, in milliseconds.
    pub now: u64,
    /// How long an API request takes.
    pub latency_ms: u64,
    /// How long a render takes.
    pub render_ms: u64,
    /// The latest status of every app.
    pub statuses: FxHashMap<Str, Summary>,
    /// Every record, in order.
    pub records: Vec<Record>,
    /// Wall time spent inside the engine.
    pub engine_ns: u128,
    /// Inputs given to the engine.
    pub inputs: u64,
    /// Apps whose latest status is synced and healthy.
    pub settled: usize,
    renderer: Renderer,
    /// When each simulated cluster's controllers next have work.
    due: BinaryHeap<Reverse<(u64, Str)>>,
    queue: BinaryHeap<Reverse<(u64, u64)>>,
    payloads: FxHashMap<u64, Input>,
    seq: u64,
    watched: FxHashMap<Str, FxHashSet<GroupKind>>,
    events: Vec<Event>,
}

impl Harness {
    /// A harness around `engine`, rendering with `renderer`.
    pub fn new(engine: Engine, renderer: Renderer) -> Self {
        Self {
            engine,
            clusters: FxHashMap::default(),
            now: 0,
            latency_ms: 5,
            render_ms: 20,
            statuses: FxHashMap::default(),
            records: Vec::new(),
            engine_ns: 0,
            inputs: 0,
            settled: 0,
            renderer,
            due: BinaryHeap::new(),
            queue: BinaryHeap::new(),
            payloads: FxHashMap::default(),
            seq: 0,
            watched: FxHashMap::default(),
            events: Vec::new(),
        }
    }

    /// Adds a simulated cluster and tells the engine about it.
    pub fn add_cluster(&mut self, cluster: sim::Cluster, qps: Option<f64>) {
        let name = cluster.name.clone();
        self.clusters.insert(name.clone(), cluster);
        self.input(Input::UpsertCluster { name, qps, inflight: None });
    }

    /// Gives the engine an input now.
    pub fn input(&mut self, input: Input) {
        let t = Instant::now();
        self.engine.handle(self.now, input);
        self.engine_ns += t.elapsed().as_nanos();
        self.inputs += 1;
        self.drain();
    }

    /// Gives the engine an input after `delay_ms`.
    pub fn later(&mut self, delay_ms: u64, input: Input) {
        self.seq += 1;
        self.queue.push(Reverse((self.now + delay_ms, self.seq)));
        self.payloads.insert(self.seq, input);
    }

    fn drain(&mut self) {
        let mut actions = Vec::new();
        loop {
            self.engine.drain(&mut actions);
            if actions.is_empty() {
                return;
            }
            for a in actions.drain(..) {
                self.act(a);
            }
        }
    }

    fn act(&mut self, a: Action) {
        match a {
            Action::Render { key, app, revision } => {
                let result = (self.renderer)(&app, &revision, key);
                self.later(self.render_ms, Input::Rendered { key, result });
            }
            Action::Watch { cluster, kind } => {
                let objects = self.clusters.get(&cluster).map(|c| c.list(&kind)).unwrap_or_default();
                self.watched.entry(cluster.clone()).or_default().insert(kind.clone());
                self.later(self.latency_ms, Input::Live { cluster, change: LiveChange::Listed { kind, objects } });
            }
            Action::Apply { id, cluster, object, dry_run, manager, .. } => {
                let now = self.now;
                let result = match self.clusters.get_mut(&cluster) {
                    Some(c) => c.apply(now, object, &manager, dry_run),
                    None => Err(format!("no cluster {cluster}")),
                };
                if let Some(t) = self.clusters.get(&cluster).and_then(sim::Cluster::next_due) {
                    self.due.push(Reverse((t, cluster.clone())));
                }
                match result {
                    Ok((v, ev)) => {
                        self.later(self.latency_ms, Input::Answer { id, result: Ok(Some(v)) });
                        if let Some(ev) = ev {
                            self.deliver(&cluster, ev, self.latency_ms + 1);
                        }
                    }
                    Err(e) => self.later(self.latency_ms, Input::Answer { id, result: Err(e) }),
                }
            }
            Action::Delete { id, cluster, key, .. } => {
                let result = match self.clusters.get_mut(&cluster) {
                    Some(c) => c.delete(&key),
                    None => Err(format!("no cluster {cluster}")),
                };
                match result {
                    Ok(ev) => {
                        self.later(self.latency_ms, Input::Answer { id, result: Ok(None) });
                        if let Some(ev) = ev {
                            self.deliver(&cluster, ev, self.latency_ms + 1);
                        }
                    }
                    Err(e) => self.later(self.latency_ms, Input::Answer { id, result: Err(e) }),
                }
            }
            Action::Status(s) => {
                let now_settled = is_settled(&s);
                let was = self.statuses.insert(s.app.clone(), s).is_some_and(|old| is_settled(&old));
                match (was, now_settled) {
                    (false, true) => self.settled += 1,
                    (true, false) => self.settled = self.settled.saturating_sub(1),
                    _ => {}
                }
            }
            Action::Record(r) => self.records.push(r),
        }
    }

    fn deliver(&mut self, cluster: &Str, ev: Event, delay: u64) {
        let watched = self.watched.get(cluster);
        let change = match ev {
            Event::Upsert(v) => {
                let gk = GroupKind::new(crate::obj::group(&v), crate::obj::kind(&v));
                if !watched.is_some_and(|w| w.contains(&gk)) {
                    return;
                }
                LiveChange::Upsert(v)
            }
            Event::Delete(k) => {
                if !watched.is_some_and(|w| w.contains(&k.group_kind())) {
                    return;
                }
                LiveChange::Delete(k)
            }
        };
        self.later(delay, Input::Live { cluster: cluster.clone(), change });
    }

    /// Someone edits an object in a cluster (drift); the engine hears of it after the latency.
    pub fn edit(&mut self, cluster: &str, key: &crate::obj::ResKey, f: impl FnOnce(&mut Value)) {
        let ev = self.clusters.get_mut(cluster).and_then(|c| c.edit(key, f));
        if let Some(ev) = ev {
            let name: Str = cluster.into();
            self.deliver(&name, ev, self.latency_ms);
        }
    }

    /// A status-only change to an object (a controller's update): the engine hears of it.
    pub fn touch(&mut self, cluster: &str, key: &crate::obj::ResKey, f: impl FnOnce(&mut serde_json::Map<String, Value>)) {
        let ev = self.clusters.get_mut(cluster).and_then(|c| c.touch_status(key, f));
        if let Some(ev) = ev {
            let name: Str = cluster.into();
            self.deliver(&name, ev, self.latency_ms);
        }
    }

    /// The next moment anything happens.
    fn next(&self) -> Option<u64> {
        let q = self.queue.peek().map(|Reverse((t, _))| *t);
        let e = self.engine.deadline();
        let c = self.due.peek().map(|Reverse((t, _))| *t);
        [q, e, c].into_iter().flatten().min()
    }

    /// Runs until `stop` holds, nothing is left to happen, or `limit_ms` of simulated time
    /// passes. Returns whether `stop` held.
    pub fn run_until(&mut self, limit_ms: u64, mut stop: impl FnMut(&Harness) -> bool) -> bool {
        let end = self.now + limit_ms;
        loop {
            if stop(self) {
                return true;
            }
            let Some(t) = self.next() else { return stop(self) };
            if t > end {
                self.now = end;
                return stop(self);
            }
            self.now = self.now.max(t);
            self.step_due();
        }
    }

    /// Runs `ms` of simulated time.
    pub fn run_for(&mut self, ms: u64) {
        let end = self.now + ms;
        while let Some(t) = self.next().filter(|t| *t <= end) {
            self.now = self.now.max(t);
            self.step_due();
        }
        self.now = end;
    }

    fn step_due(&mut self) {
        let now = self.now;
        // Controllers first: what they do is delivered after the latency, like any event.
        let mut events = std::mem::take(&mut self.events);
        let mut names: Vec<Str> = Vec::new();
        while let Some(Reverse((t, name))) = self.due.peek().cloned() {
            if t > now {
                break;
            }
            self.due.pop();
            if !names.contains(&name) {
                names.push(name);
            }
        }
        for name in names {
            if let Some(c) = self.clusters.get_mut(&name) {
                c.advance(now, &mut events);
                if let Some(t) = c.next_due() {
                    self.due.push(Reverse((t, name.clone())));
                }
            }
            for ev in events.drain(..) {
                self.deliver(&name, ev, self.latency_ms);
            }
        }
        self.events = events;
        while let Some(Reverse((t, seq))) = self.queue.peek().copied() {
            if t > now {
                break;
            }
            self.queue.pop();
            if let Some(input) = self.payloads.remove(&seq) {
                self.input(input);
            }
        }
        if self.engine.deadline().is_some_and(|d| d <= now) {
            self.input(Input::Tick);
        }
    }

    /// Whether every app is synced and healthy, with no sync running.
    pub fn all_settled(&self) -> bool {
        self.engine.app_count() > 0 && self.settled == self.engine.app_count()
    }
}

/// Synced, healthy, and no sync running.
pub fn is_settled(s: &Summary) -> bool {
    s.sync == Sync::Synced
        && s.health == Health::Healthy
        && s.operation.as_ref().is_none_or(|o| !matches!(o.phase, crate::engine::OpPhase::Running | crate::engine::OpPhase::DryRun))
}
