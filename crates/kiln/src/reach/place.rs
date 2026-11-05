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
        Placer { me: me.to_owned(), epoch, owner: Owner::new(me, epoch), admission: Admission::new(init, now), specs: BTreeMap::new(), started: BTreeMap::new(), machines: Vec::new(), facts, labels, away: std::collections::BTreeSet::new(), told_to: std::collections::BTreeSet::new() }
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
            ToRunner::Probe { ask } => self.admission.probe(from, *ask, None, now, &mut said),
            ToRunner::Withdraw { run } => self.admission.withdraw(from, &run, now, &mut said),
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

    /// A reading of this machine: the room it admits against.
    pub fn measured(&mut self, sample: Sample, now: i64) -> Vec<Out> {
        let mut said = Vec::new();
        self.admission.measured(sample, now, &mut said);
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

    /// The runner's admission, for views (`/api/reach/offers`).
    pub fn admission(&self) -> &Admission {
        &self.admission
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
                        ToRunner::Probe { ask } => self.admission.probe(me, *ask, None, now, &mut said),
                        ToRunner::Withdraw { run } => self.admission.withdraw(me, &run, now, &mut said),
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
                Act::Answer { run, answer } => out.push(Out::Local { run, placed: Placed::Answer(answer) }),
                Act::Go { run, runner, fence } => out.push(Out::Local { run, placed: Placed::Go { runner, fence } }),
                Act::Fallback { run, runners } => out.push(Out::Local { run, placed: Placed::Fallback { runners } }),
                // A start said again: the local run's shell says it again with its spec.
                Act::Restart { run, runner, fence } => out.push(Out::Local { run, placed: Placed::Go { runner, fence } }),
            }
        }
        out
    }
}
