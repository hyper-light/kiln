//! Placement in a runner's node (docs/needs.md E5, docs/reach.md §2): the shell around kiln-ci's
//! pure core. The node a run is asked of owns it (`kiln_ci::place::owner::Owner`); the runner that
//! has the room admits it (`kiln_ci::place::runner::Admission`), by the engine's policy. Both live
//! here, in the node's one task, so a run asked of this machine for this machine is admitted with
//! no message at all, and nothing here waits on a lock.
//!
//! - **On the wire** (docs/wire.md, "Named features"): an owner's word goes in `Ask::Place`, a
//!   runner's in `Say::Place`, each `{"epoch": the sender's epoch, "spec": the run's spec id when it
//!   starts one, "msg": the core's message}` as JSON, and only between runners that named `place`.
//!   A runner from before is placed on as it was: the owner's `Fallback`.
//! - **Room** is measured, never assumed: the runner's admission is told each reading of its host
//!   sampler as it comes (until `lb/sense` gives it counters read at decision time), and admits
//!   against that.
//! - **Time** comes in: [`Placer::timer`] says when to be told it again, and the node sets a timer
//!   for then, not a poll.
//! - **No run starts twice**: a spec that comes for a run the admission didn't say `New` to is
//!   refused, never started; one that comes from an asker that doesn't place starts as before.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use kiln_ci::facts::RunnerFacts;
use kiln_ci::place::answer::Answer;
use kiln_ci::place::ask::{Ask, Peer, Sample};
use kiln_ci::place::msg::{ToOwner, ToRunner};
use kiln_ci::place::owner::{Act, Machine, Owner, Size};
use kiln_ci::place::runner::{Admission, Init, Records, Settings, Start};
use kiln_ci::sched::state::{Cpus, Fence, Resources};

use super::directory::RunnerInfo;
use crate::proto::feature;

/// The owner a CI leg's room is held for in a runner's admission (AUDIT M4, docs/needs.md E5):
/// the CI engine that assigned it. No runner can be named so (a `:` is in no host name).
pub const CI: &str = "ci:engine";

/// A waiting reason in words, as a refused leg says why.
fn reason_words(r: &kiln_ci::place::msg::Reason) -> String {
    use kiln_ci::place::msg::Reason;
    match r {
        Reason::Room { short } => {
            let dims: Vec<String> = short.iter().map(|s| format!("{:?}", s.dim).to_lowercase()).collect();
            if dims.is_empty() { "no room for it beside what kiln placed here".to_owned() } else { format!("too little {} free beside what kiln placed here", dims.join(", ")) }
        }
        Reason::Kept { .. } => "room here is kept for a run that waited first".to_owned(),
        Reason::Line => "runs ahead of it in this runner's line go first".to_owned(),
        Reason::Whole { jobs } => format!("it takes the runner whole, and {jobs} other pieces of kiln's work run here"),
        Reason::Draining => "this runner is draining".to_owned(),
        Reason::Away => "its owner's link is down".to_owned(),
        Reason::Strained { why } => format!("this machine is under strain: {why}"),
        Reason::Barred { why, .. } => why.clone(),
    }
}

/// A placement word on a link, either way.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope<M> {
    /// The sender's epoch: its node's, one past the last it used.
    pub epoch: u64,
    /// The spec id of the run a start is for (the `Ask::Run` that follows it on the link).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<u64>,
    pub msg: M,
}

/// What a local run's asker hears from placement.
#[derive(Debug, Clone, PartialEq)]
pub enum Placed {
    /// Its answer, the first and each change after.
    Answer(Answer),
    /// It's to start on `runner` under claim `fence`: copy its tree there, then start it.
    Go { runner: String, fence: Fence },
    /// No runner that places can start it now; older ones could: pick among them as before.
    Fallback { runners: Vec<String> },
}

/// What the node is to do for placement.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// Send `Ask::Place { json }` to runner `peer`.
    Tell { peer: String, json: String },
    /// Send `Say::Place { json }` to the owner `peer` (on its session's link).
    Say { peer: String, json: String },
    /// Tell the local run `run`'s asker.
    Local { run: String, placed: Placed },
}

/// What came of a spec a placing asker sent ([`Placer::spec`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spec {
    /// Start it: the admission said so, for run `run`.
    Start { run: String },
    /// Don't: it ran here already, or its claim isn't live (why).
    Refuse(String),
    /// Not placed: an asker from before placement, or a run it didn't place; as before.
    Unplaced,
}

/// Placement in one node: its owner and its runner's admission.
pub struct Placer {
    me: String,
    epoch: u64,
    owner: Owner,
    admission: Admission,
    /// Specs a placing asker's start was answered for: by (asker, spec id), the run and whether to
    /// start it.
    specs: BTreeMap<(String, u64), (String, u64, Result<(), String>)>,
    /// Runs started here for an owner, by (asker, spec id): its epoch and run.
    started: BTreeMap<(String, u64), (u64, String)>,
    /// The last directory, for the owner's machines.
    machines: Vec<Machine>,
    /// This runner's own facts and labels, as its owner's machine.
    facts: RunnerFacts,
    labels: Vec<String>,
    /// Owners whose link went down: back with their next word.
    away: std::collections::BTreeSet<String>,
    /// The runners this node's owner has told something: linked to again when the server is back,
    /// so what waits there hears where it stands.
    told_to: std::collections::BTreeSet<String>,
    /// Its admission's hint as last said (the `offers` feature, AUDIT O1), and the runs of this
    /// node's own that wait (as the owner's answers say) and those no longer waiting since.
    hinted: Option<crate::offers::hint::Hint>,
    waits: BTreeMap<String, crate::offers::hint::Wait>,
    gone: Vec<String>,
    waits_moved: bool,
    /// Its next hint is the whole of what waits (the server's first since it connected).
    whole: bool,
    /// What it measures of its machine when it decides (AUDIT L1, L2; none: it admits against the
    /// readings it's fed), the last measure, and whether its machine is under strain, why.
    sensor: Option<crate::sense::Sensor>,
    last: Option<crate::sense::Measure>,
    strained: Option<String>,
    /// Probes its health bars (L2), by owner and epoch: answered so, and probed once a measure
    /// shows the cause cleared.
    parked: Vec<(String, u64, std::sync::Arc<Ask>)>,
}

/// Placement's epoch for the node whose state lives in `dir`: one past the last it used, kept in
/// `dir/place-epoch` (docs/needs.md §8: a claim from before a restart is never taken for one after).
pub fn next_epoch(dir: &std::path::Path) -> u64 {
    let path = dir.join("place-epoch");
    let last = std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0);
    let next = last.saturating_add(1);
    if let Err(e) = std::fs::write(&path, next.to_string()) {
        tracing::warn!(error = %e, path = %path.display(), "couldn't keep placement's epoch: a restart may take up this one again");
    }
    next
}

/// What sense measured as placement's sample: the machine (its CPUs, memory, work volume) and
/// what's in use of it, by everything. A share its interval was too short to show (no CPU tick
/// passed) is the last measure's.
pub fn sample_of_measure(m: &crate::sense::Measure, last: Option<&crate::sense::Measure>) -> Sample {
    let idle = m.cpu_idle.or_else(|| last.and_then(|l| l.cpu_idle)).unwrap_or(1.0);
    let busy = ((1.0 - idle.clamp(0.0, 1.0)) * f64::from(m.cpus) * 1000.0).round();
    let busy = if busy.is_finite() && busy >= 0.0 { busy as u64 } else { 0 };
    let (disk_total, disk_free) = m.space.map_or((0, 0), |s| (s.total, s.free));
    Sample {
        size: Resources { cpu: Cpus::whole(u64::from(m.cpus)), mem: m.mem_total, disk: disk_total, gpu: 0 },
        used: Resources { cpu: Cpus::milli(busy), mem: m.mem_total.saturating_sub(m.mem_available), disk: disk_total.saturating_sub(disk_free), gpu: 0 },
        ..Sample::default()
    }
}

/// Why this runner's health bars `ask`'s kind of work now (L2, sense's `Health::check` but its
/// strain, which the admission keeps): every run works in the work volume, so a full one, or its
/// device's errors rising, bars all work; memory and tools as asked.
fn barred(ask: &Ask, m: &crate::sense::Measure) -> Option<crate::sense::health::Refusal> {
    let a = crate::sense::health::Ask { key: None, disk: ask.needs.request.disk.max(1), mem: ask.needs.request.mem, tools: Vec::new() };
    let h = crate::sense::health::runner().lock().ok()?;
    // Its strain is the admission's (it waits, saying so): only the causes its runs met bar here.
    h.check(&a, m, 0.0).err().filter(|r| r.kind != "strained")
}

/// The host sampler's reading as placement's sample: the machine and what's in use of it.
pub fn sample_of(s: &crate::metrics::HostSample) -> Sample {
    let busy = (f64::from(s.cpu_pct.clamp(0.0, 100.0)) * f64::from(s.cpus) * 10.0).round();
    let busy = if busy.is_finite() && busy >= 0.0 { busy as u64 } else { 0 };
    Sample {
        size: Resources { cpu: Cpus::whole(u64::from(s.cpus)), mem: s.mem_total, disk: s.disk_total, gpu: 0 },
        used: Resources { cpu: Cpus::milli(busy), mem: s.mem_used, disk: s.disk_used, gpu: 0 },
        ..Sample::default()
    }
}

/// A directory entry as the owner's machine: its facts, its size as measured (none until it is),
/// and whether it places.
pub fn machine_of(r: &RunnerInfo) -> Machine {
    let facts = r.facts.as_deref().cloned().unwrap_or_else(|| RunnerFacts { os: r.os.clone(), arch: r.arch.clone(), ..RunnerFacts::default() });
    let size = Size { cpu: (r.cpus > 0).then(|| Cpus::whole(u64::from(r.cpus))), mem: (r.mem_total > 0).then_some(r.mem_total), disk: None, gpu: None };
    Machine { name: r.name.clone(), facts, labels: r.labels.clone(), size, epoch: 0, connected: r.connected && r.reach, places: feature::speaks(&r.features, feature::PLACE), idle: None }
}

impl Placer {
    /// Node `me` in `epoch`, its runner's facts and labels, its first reading and settings.
    pub fn new(me: &str, epoch: u64, facts: RunnerFacts, labels: Vec<String>, sample: Sample, settings: Settings, now: i64) -> Placer {
        let init = Init { me: me.to_owned(), epoch, facts: facts.clone(), labels: labels.clone(), sample, records: Records::new(), settings };
        Placer { me: me.to_owned(), epoch, owner: Owner::seeded(me, epoch, rand::random()), admission: Admission::new(init, now), specs: BTreeMap::new(), started: BTreeMap::new(), machines: Vec::new(), facts, labels, away: std::collections::BTreeSet::new(), told_to: std::collections::BTreeSet::new(), hinted: None, waits: BTreeMap::new(), gone: Vec::new(), waits_moved: false, whole: true, sensor: None, last: None, strained: None, parked: Vec::new() }
    }

    /// It measures its machine itself when it decides, with `sensor` (AUDIT L1, L2).
    #[must_use]
    pub fn sensing(mut self, sensor: Option<crate::sense::Sensor>) -> Placer {
        self.sensor = sensor;
        // Its size as measured, before anything asks (its owner's own machine is that size).
        self.measure_now(crate::util::now_ms(), &mut Vec::new());
        self
    }

    /// Its machine measured now, at decision time (docs/needs.md E5: room is measured, never
    /// assumed): the room its admission admits against; whether the machine is under strain past
    /// the scheduler's reserve (sense's predicate, the gate guard's: L1, the admission offers
    /// nothing new until a measure shows it isn't); and the causes sense's health keeps that this
    /// measure shows cleared (L2), the probes they parked probed now. Nothing to do without a
    /// sensor.
    fn measure_now(&mut self, now: i64, said: &mut Vec<(String, ToOwner)>) {
        let Some(sensor) = self.sensor.as_mut() else { return };
        let m = sensor.measure();
        let sample = sample_of_measure(&m, self.last.as_ref());
        self.admission.measured(sample, now, said);
        let target = kiln_ci::sched::state::knee(m.cpus);
        let strained = crate::sense::health::strained(&m, target);
        if let Ok(mut h) = crate::sense::health::runner().lock() {
            h.measured(&m);
        }
        if strained != self.strained {
            self.admission.strain(strained.clone(), now, said);
            self.strained = strained;
        }
        for (peer, epoch, ask) in std::mem::take(&mut self.parked) {
            match barred(&ask, &m) {
                Some(_) => self.parked.push((peer, epoch, ask)),
                None => self.admission.probe(Peer { name: &peer, epoch }, ask, None, now, said),
            }
        }
        self.last = Some(m);
    }

    /// Why its machine is under strain now, if it is, as its last measure said (the hint's state).
    pub fn strain(&self) -> Option<String> {
        self.strained.clone()
    }

    /// A probe from `peer` (`epoch`): to its admission, measured first; or, when this runner's
    /// health bars its kind of work (L2), parked here and answered so, until a measure clears it.
    fn probe(&mut self, peer: &str, epoch: u64, ask: std::sync::Arc<Ask>, now: i64, said: &mut Vec<(String, ToOwner)>) {
        self.measure_now(now, said);
        if self.strained.is_none()
            && let Some(m) = &self.last
            && let Some(r) = barred(&ask, m)
        {
            let free = self.admission.hint();
            said.push((peer.to_owned(), ToOwner::Waiting { run: ask.run.clone(), reason: kiln_ci::place::msg::Reason::Barred { cause: r.kind, why: r.why }, place: kiln_ci::place::msg::Place::default(), start: kiln_ci::place::msg::Estimate::default(), predicted: None, free }));
            self.parked.retain(|(p, _, a)| !(p == peer && a.run == ask.run));
            self.parked.push((peer.to_owned(), epoch, ask));
            return;
        }
        self.admission.probe(Peer { name: peer, epoch }, ask, None, now, said);
    }

    /// The directory came (or changed): the owner's machines, this one among them as it measures
    /// itself.
    pub fn directory(&mut self, runners: &[RunnerInfo], now: i64) -> Vec<Out> {
        let mut ms: Vec<Machine> = runners.iter().filter(|r| r.name != self.me).map(machine_of).collect();
        let cap = self.admission.cap();
        ms.push(Machine { name: self.me.clone(), facts: self.facts.clone(), labels: self.labels.clone(), size: Size { cpu: Some(Cpus::milli(cap[0])), mem: Some(cap[1]), disk: Some(cap[2]).filter(|d| *d > 0), gpu: Some(u32::try_from(cap[3]).unwrap_or(u32::MAX)) }, epoch: self.epoch, connected: true, places: true, idle: None });
        if ms == self.machines {
            return Vec::new();
        }
        self.machines = ms.clone();
        let acts = self.owner.machines(ms);
        self.acts(acts, now)
    }

    /// A run asked of this node: owned here, probed where it could run.
    pub fn ask(&mut self, ask: Ask, now: i64) -> Vec<Out> {
        let wait = crate::offers::hint::Wait { run: ask.run.clone(), need: ask.needs.describe(), ask: kiln_ci::place::ask::amounts(&ask.needs.request), class: ask.class, since: now, reason: String::new() };
        self.waits.insert(ask.run.clone(), wait);
        let acts = self.owner.ask(ask, now);
        self.acts(acts, now)
    }

    /// Its asker doesn't want it any more.
    pub fn cancel(&mut self, run: &str, now: i64) -> Vec<Out> {
        let acts = self.owner.cancel(run);
        self.acts(acts, now)
    }

    /// The local run `run` is about to start on `runner` under `fence`: whether it still may
    /// ([`Owner::starting`]); for this runner, its admission says whether it starts (`New`).
    pub fn starting(&mut self, run: &str, runner: &str, fence: Fence, now: i64) -> (bool, Vec<Out>) {
        if !self.owner.starting(run, runner, fence) {
            return (false, Vec::new());
        }
        if runner != self.me {
            return (true, Vec::new());
        }
        let mut said = Vec::new();
        let me = Peer { name: &self.me, epoch: self.epoch };
        let started = self.admission.start(me, run, fence, now, &mut said);
        let out = self.said(said, now);
        (started == Some(Start::New), out)
    }

    /// The start of run `run` on `runner` under `fence`, said with the spec id it's for.
    pub fn start_word(&self, run: &str, fence: Fence, spec: u64) -> String {
        serde_json::to_string(&Envelope { epoch: self.epoch, spec: Some(spec), msg: ToRunner::Start { run: run.to_owned(), fence } }).unwrap_or_default()
    }

    /// A run of this node's started here: its process is up (the runner's word).
    pub fn local_spawned(&mut self, run: &str, now: i64) -> Vec<Out> {
        let mut said = Vec::new();
        let me = Peer { name: &self.me, epoch: self.epoch };
        self.admission.spawned(me, run, &mut said);
        self.said(said, now)
    }

    /// A run of this node's here ended (`ok`: its process exited).
    pub fn local_ended(&mut self, run: &str, ok: bool, now: i64) -> Vec<Out> {
        let mut said = Vec::new();
        let me = Peer { name: &self.me, epoch: self.epoch };
        self.admission.ended(me, run, ok, now, &mut said);
        self.admission.ack(me, run);
        self.measure_now(now, &mut said);
        self.said(said, now)
    }

    /// An owner's word, from runner `peer`'s link (`Ask::Place`).
    pub fn told(&mut self, peer: &str, json: &str, now: i64) -> Vec<Out> {
        let Ok(e) = serde_json::from_str::<Envelope<ToRunner>>(json) else {
            tracing::debug!(%peer, "a placement word this runner couldn't read");
            return Vec::new();
        };
        let from = Peer { name: peer, epoch: e.epoch };
        let mut said = Vec::new();
        if self.away.remove(peer) {
            self.admission.back(from, now, &mut said);
        }
        match e.msg {
            ToRunner::Probe { ask } => self.probe(peer, e.epoch, ask, now, &mut said),
            ToRunner::Withdraw { run } => {
                self.parked.retain(|(p, _, a)| !(p == peer && a.run == run));
                self.admission.withdraw(from, &run, now, &mut said);
            }
            ToRunner::Take { run, fence } => self.admission.take(from, &run, fence, now, &mut said),
            ToRunner::Decline { run, fence } => self.admission.decline(from, &run, fence, now, &mut said),
            ToRunner::Start { run, fence } => {
                let started = self.admission.start(from, &run, fence, now, &mut said);
                if let Some(spec) = e.spec {
                    let verdict = match started {
                        Some(Start::New) => Ok(()),
                        Some(Start::Already) => Err(format!("{run} ran here already: it isn't started again")),
                        None => Err(format!("{run}'s claim here isn't live: it isn't started")),
                    };
                    self.specs.insert((peer.to_owned(), spec), (run, e.epoch, verdict));
                }
            }
            ToRunner::Ack { run } => self.admission.ack(from, &run),
        }
        self.said(said, now)
    }

    /// A spec came from runner `peer` (`Ask::Run`): what to do with it.
    pub fn spec(&mut self, peer: &str, id: u64) -> Spec {
        match self.specs.remove(&(peer.to_owned(), id)) {
            Some((run, epoch, Ok(()))) => {
                self.started.insert((peer.to_owned(), id), (epoch, run.clone()));
                Spec::Start { run }
            }
            Some((_, _, Err(why))) => Spec::Refuse(why),
            None => Spec::Unplaced,
        }
    }

    /// A run started here for `peer` (spec `id`) is up.
    pub fn spawned(&mut self, peer: &str, id: u64, now: i64) -> Vec<Out> {
        let Some((epoch, run)) = self.started.get(&(peer.to_owned(), id)).cloned() else { return Vec::new() };
        let mut said = Vec::new();
        self.admission.spawned(Peer { name: peer, epoch }, &run, &mut said);
        self.said(said, now)
    }

    /// A run started here for `peer` (spec `id`) ended.
    pub fn ended(&mut self, peer: &str, id: u64, ok: bool, now: i64) -> Vec<Out> {
        let Some((epoch, run)) = self.started.remove(&(peer.to_owned(), id)) else { return Vec::new() };
        let mut said = Vec::new();
        self.admission.ended(Peer { name: peer, epoch }, &run, ok, now, &mut said);
        self.measure_now(now, &mut said);
        self.said(said, now)
    }

    /// A runner's word, from `peer`'s link (`Say::Place`).
    pub fn heard(&mut self, peer: &str, json: &str, now: i64) -> Vec<Out> {
        let Ok(e) = serde_json::from_str::<Envelope<ToOwner>>(json) else {
            tracing::debug!(%peer, "a placement word this owner couldn't read");
            return Vec::new();
        };
        let acts = self.owner.heard(peer, e.msg, now);
        self.acts(acts, now)
    }

    /// A reading of this machine: the room it admits against; where it measures itself, the
    /// moment to (the reading's figures aren't taken).
    pub fn measured(&mut self, sample: Sample, now: i64) -> Vec<Out> {
        let mut said = Vec::new();
        if self.sensor.is_some() {
            self.measure_now(now, &mut said);
        } else {
            self.admission.measured(sample, now, &mut said);
        }
        self.said(said, now)
    }

    /// This node's link to runner `peer` went down, or came back: what this node owns there waits
    /// for it, and is probed again when it's back.
    pub fn runner_link(&mut self, peer: &str, up: bool, now: i64) -> Vec<Out> {
        let acts = if up { self.owner.back(peer) } else { self.owner.away(peer) };
        self.acts(acts, now)
    }

    /// Owner `peer`'s link to this runner went down: its runs leave the line (a claim it took stays
    /// held), and come back at their places with its next word.
    pub fn owner_away(&mut self, peer: &str, now: i64) -> Vec<Out> {
        let mut said = Vec::new();
        self.admission.away(peer, now, &mut said);
        self.away.insert(peer.to_owned());
        self.said(said, now)
    }

    /// What this runner's admission holds, for a view: its budget and room, what its claims hold,
    /// how many wait in each class, its timer.
    pub fn view(&self) -> serde_json::Value {
        let (room, held) = self.admission.room();
        let queued: serde_json::Map<String, serde_json::Value> = kiln_ci::sched::state::Class::ALL.iter().zip(self.admission.queued_by_class()).map(|(c, n)| (c.as_str().to_owned(), n.into())).collect();
        serde_json::json!({
            "runner": self.me,
            "epoch": self.epoch,
            "cap": self.admission.cap(),
            "room": room,
            "held": held,
            "queued": queued,
            "next": self.admission.timer(),
        })
    }

    /// The time.
    pub fn tick(&mut self, now: i64) -> Vec<Out> {
        let acts = self.owner.tick(now);
        let mut out = self.acts(acts, now);
        let mut said = Vec::new();
        self.admission.tick(now, &mut said);
        out.extend(self.said(said, now));
        out
    }

    /// When to be told the time again.
    pub fn timer(&self) -> Option<i64> {
        [self.owner.timer(), self.admission.timer()].into_iter().flatten().min()
    }

    /// The runners this node's owner has told something (to link to again).
    pub fn told_to(&self) -> Vec<String> {
        self.told_to.iter().cloned().collect()
    }

    /// CI leg `ask.run`, assigned this runner by the CI engine (AUDIT M4, G1): admitted against
    /// the same ledger as the runs owners place here, now or not at all. Room for it now, its
    /// claim is taken and started at once (held until [`Self::leg_ended`]); none, refused saying
    /// why, and the engine places it elsewhere or later (it queues in the engine, never here).
    pub fn leg(&mut self, ask: Ask, now: i64) -> (Result<(), String>, Vec<Out>) {
        let run = ask.run.clone();
        let ci = Peer { name: CI, epoch: self.epoch };
        let mut said = Vec::new();
        self.probe(CI, self.epoch, std::sync::Arc::new(ask), now, &mut said);
        let mut verdict: Result<Fence, String> = Err("this runner's admission didn't answer".to_owned());
        let mut rest = Vec::new();
        for (to, msg) in said {
            if to != CI {
                rest.push((to, msg));
                continue;
            }
            match msg {
                ToOwner::Offer { run: r, fence, .. } if r == run => verdict = Ok(fence),
                ToOwner::Waiting { run: r, reason, .. } if r == run => verdict = Err(reason_words(&reason)),
                ToOwner::Never { run: r, why } if r == run => verdict = Err(why),
                _ => {}
            }
        }
        let mut more = Vec::new();
        let verdict = match verdict {
            Ok(fence) => {
                self.admission.take(ci, &run, fence, now, &mut more);
                match self.admission.start(ci, &run, fence, now, &mut more) {
                    Some(Start::New) => {
                        self.admission.spawned(ci, &run, &mut more);
                        Ok(())
                    }
                    _ => {
                        self.admission.withdraw(ci, &run, now, &mut more);
                        Err("its room here lapsed before it could be held".to_owned())
                    }
                }
            }
            Err(why) => {
                self.parked.retain(|(p, _, a)| !(p == CI && a.run == run));
                self.admission.withdraw(ci, &run, now, &mut more);
                Err(why)
            }
        };
        rest.extend(more.into_iter().filter(|(to, _)| to != CI));
        (verdict, self.said(rest, now))
    }

    /// CI leg `run` ended here (`ok`: its process exited): its room is free at once.
    pub fn leg_ended(&mut self, run: &str, ok: bool, now: i64) -> Vec<Out> {
        let ci = Peer { name: CI, epoch: self.epoch };
        let mut said = Vec::new();
        self.admission.ended(ci, run, ok, now, &mut said);
        self.admission.ack(ci, run);
        self.measure_now(now, &mut said);
        said.retain(|(to, _)| to != CI);
        self.said(said, now)
    }

    /// Whether it measures its machine itself.
    pub fn senses(&self) -> bool {
        self.sensor.is_some()
    }

    /// The runner's admission, for views (`/api/reach/offers`).
    pub fn admission(&self) -> &Admission {
        &self.admission
    }

    /// One of this node's runs was answered `a`: it waits (why, as the answer counts it first),
    /// or it doesn't any more.
    fn waited(&mut self, run: &str, a: &Answer) {
        match a {
            Answer::Queued { reasons, .. } => {
                let reason = reasons.first().map(|c| c.reason.clone()).unwrap_or_default();
                if let Some(w) = self.waits.get_mut(run).filter(|w| w.reason != reason) {
                    w.reason = reason;
                    self.waits_moved = true;
                }
            }
            _ => {
                // Said gone only if it was said to wait.
                if self.waits.remove(run).is_some_and(|w| !w.reason.is_empty()) {
                    self.gone.push(run.to_owned());
                }
            }
        }
    }

    /// The server's link came back (it takes hints, `offers`), or went: its next hint says all.
    pub fn hints_anew(&mut self) {
        self.hinted = None;
        self.whole = true;
    }

    /// Its admission's hint (the `offers` feature, docs/needs.md E5, AUDIT O1), JSON, when it
    /// changed something decided by since the last said (`crate::offers::relevant`, AUDIT O2): an
    /// answer for a size waiting or held here flips, its state, budget or line's counts move, a
    /// dimension's room goes to none or comes back (an owner choosing where to probe by hints
    /// takes a runner with none of what its run asks for one it doesn't fit), or this node's own
    /// runs that wait. Never per sample. `strained`: why its machine is, if it is (sense's
    /// health, as the gate's guard sees it).
    pub fn hint_due(&mut self, now: i64, strained: Option<String>) -> Option<String> {
        use crate::offers::hint::{Hint, Taking};
        use crate::offers::relevant::{Said, relevant};
        let state = match strained {
            Some(why) => Taking::Strained { why },
            None => Taking::Taking,
        };
        let room = kiln_ci::place::ask::amounts(&self.admission.hint());
        let mut hint = Hint { epoch: self.epoch, seq: 0, at: now, budget: self.admission.cap(), room, queued: self.admission.queued_by_class(), state, quotes: Vec::new(), waits: Vec::new(), gone: Vec::new(), whole: self.whole };
        let changed = match &self.hinted {
            None => true,
            Some(last) => {
                let roomy = |h: &Hint| h.room.map(|a| a > 0);
                // The sizes that wait or hold room here: what a change of room is decided by.
                let sizes: std::collections::BTreeSet<[u64; 4]> = self.admission.probes().into_iter().map(|p| p.ask).collect();
                let (was, is) = (Said { hint: last.clone(), heard: last.at }, Said { hint: hint.clone(), heard: hint.at });
                self.whole || self.waits_moved || !self.gone.is_empty() || roomy(last) != roomy(&hint) || relevant(&was, &is, sizes.iter())
            }
        };
        if !changed {
            return None;
        }
        hint.seq = self.hinted.as_ref().map_or(1, |h| h.seq.saturating_add(1));
        hint.waits = if self.whole || self.waits_moved { self.waits.values().filter(|w| !w.reason.is_empty()).cloned().collect() } else { Vec::new() };
        hint.gone = std::mem::take(&mut self.gone);
        (self.whole, self.waits_moved) = (false, false);
        let json = serde_json::to_string(&hint).ok()?;
        self.hinted = Some(Hint { waits: Vec::new(), gone: Vec::new(), ..hint });
        Some(json)
    }

    /// Another runner's room as the server passed it on (`ServerMsg::Offers`): its owner chooses
    /// where to probe by it (perf's `Owner::hint`); its own and stale ones aren't taken.
    pub fn hinted(&mut self, json: &str) {
        let Ok(r) = serde_json::from_str::<crate::offers::hint::Room>(json) else {
            tracing::debug!("a room hint this owner couldn't read");
            return;
        };
        if r.runner == self.me {
            return;
        }
        let free = kiln_ci::place::ask::resources(&r.room);
        self.owner.hint(&r.runner, free, r.order());
    }

    /// What the runner's admission said: to this node's owner, heard at once; to another, sent.
    fn said(&mut self, said: Vec<(String, ToOwner)>, now: i64) -> Vec<Out> {
        let mut out = Vec::new();
        for (to, msg) in said {
            if to == self.me {
                let acts = self.owner.heard(&self.me.clone(), msg, now);
                out.extend(self.acts(acts, now));
            } else if let Ok(json) = serde_json::to_string(&Envelope { epoch: self.epoch, spec: None, msg }) {
                out.push(Out::Say { peer: to, json });
            }
        }
        out
    }

    /// What the owner is to do: words for this runner go to its admission at once.
    fn acts(&mut self, acts: Vec<Act>, now: i64) -> Vec<Out> {
        let mut out = Vec::new();
        let mut queue: std::collections::VecDeque<Act> = acts.into();
        while let Some(a) = queue.pop_front() {
            match a {
                Act::Tell { runner, msg } if runner == self.me => {
                    let mut said = Vec::new();
                    let me = Peer { name: &self.me, epoch: self.epoch };
                    match msg {
                        ToRunner::Probe { ask } => {
                            let (name, epoch) = (self.me.clone(), self.epoch);
                            self.probe(&name, epoch, ask, now, &mut said);
                        }
                        ToRunner::Withdraw { run } => {
                            self.parked.retain(|(p, _, a)| !(*p == self.me && a.run == run));
                            self.admission.withdraw(me, &run, now, &mut said);
                        }
                        ToRunner::Take { run, fence } => self.admission.take(me, &run, fence, now, &mut said),
                        ToRunner::Decline { run, fence } => self.admission.decline(me, &run, fence, now, &mut said),
                        ToRunner::Start { run, fence } => {
                            self.admission.start(me, &run, fence, now, &mut said);
                        }
                        ToRunner::Ack { run } => self.admission.ack(me, &run),
                    }
                    for (to, msg) in said {
                        if to == self.me {
                            queue.extend(self.owner.heard(&self.me.clone(), msg, now));
                        } else if let Ok(json) = serde_json::to_string(&Envelope { epoch: self.epoch, spec: None, msg }) {
                            out.push(Out::Say { peer: to, json });
                        }
                    }
                }
                Act::Tell { runner, msg } => {
                    self.told_to.insert(runner.clone());
                    if let Ok(json) = serde_json::to_string(&Envelope { epoch: self.epoch, spec: None, msg }) {
                        out.push(Out::Tell { peer: runner, json });
                    }
                }
                Act::Answer { run, answer } => {
                    self.waited(&run, &answer);
                    out.push(Out::Local { run, placed: Placed::Answer(answer) });
                }
                Act::Go { run, runner, fence } => out.push(Out::Local { run, placed: Placed::Go { runner, fence } }),
                Act::Fallback { run, runners } => out.push(Out::Local { run, placed: Placed::Fallback { runners } }),
                // A start said again: the local run's shell says it again with its spec.
                Act::Restart { run, runner, fence } => out.push(Out::Local { run, placed: Placed::Go { runner, fence } }),
            }
        }
        out
    }
}
