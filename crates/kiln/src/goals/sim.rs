//! The goal driver's model (CP-T6.1): kiln's goal driver (its own round of the stacks every tick,
//! adopting, resuming and stepping them, as `Driver::drive` runs it) as a task of the server's
//! node under kiln-sim's seeded scheduler,
//! its time a simulated clock, its stacks in a test build's inline store (each call a wait a crash
//! can come at), its agents on kiln-seams' `SimHarness`.
//!
//! Each agent behaves as a harness does: one takes `/goal` (Claude Code's), one a goal control
//! (Codex's), one only messages (its reply ends `GOAL MET`); each works on what it's sent, then
//! meets it, stops short with it still set, gives up, or pauses it. The nemesis, until it stops:
//! the server crashing at any wait of the driver's and coming back on its store; agents' machines
//! going away; harnesses starting again and losing their goal; usage limits pausing agents;
//! people clearing an agent's goal. After it, agents meet what they're sent.
//!
//! The checker: never two goals under way in a stack; a goal goes under way only when every goal
//! before it ended, and an ended goal never comes back; nothing goes to an agent paused for a
//! limit; a goal isn't sent again within 30 s of the last time unless something cut the agent off
//! from it (the server's crash, the agent's restart, its machine away); and, once the nemesis has
//! stopped, every stack runs to its end, each goal met (or skipped, cleared by a person).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use kiln_seams::clock::{SIM_EPOCH_MS, SimClock};
use kiln_seams::harness::SimHarness;
use kiln_seams::spawn::spawn;
use kiln_sim::net::Node;
use kiln_sim::{Queue, Rng, Trace, World};
use rusqlite::params;
use serde_json::{Value, json};

use super::{Driver, REMIND_MS, touch};
use crate::harness::model::{Command, Objective, Pause, Status};
use crate::harness::session::Info;
use crate::rt::Shutdown;
use crate::store::Store;

/// How long the model runs at most after the nemesis stops: every goal met by then, reminders
/// backed off to 15 minutes and long turns and all. It stops sooner, once every stack is done.
const SETTLE: Duration = Duration::from_secs(90 * 60);
/// Most polls and events one seed makes.
const STEPS_MAX: u64 = 5_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Takes `/goal <text>` (Claude Code).
    Goal,
    /// Takes a goal control (Codex's thread goal).
    Control,
    /// Takes messages only; says `GOAL MET`.
    Message,
}

#[derive(Debug)]
enum Ev {
    /// An agent's turn `n` ends.
    Done(usize, u64),
    Away(usize),
    Back(usize),
    Restart(usize),
    Pause(usize),
    Unpause(usize),
    Clear(usize),
    ServerCrash,
    ServerUp,
}

struct Agent {
    id: String,
    kind: Kind,
    /// Deliveries it has taken in.
    heard: usize,
    /// The goal it works on, by its words.
    on: Option<String>,
    turn: u64,
    /// Goals a person cleared in it.
    cleared: Vec<String>,
    /// Its goals, in order.
    goals: Vec<String>,
    /// Each goal it was sent, and when.
    sent: Vec<(Duration, String)>,
    /// When something last cut it off from its goal (its restart, its machine back, a pause's end).
    cut_at: Option<Duration>,
    /// Since when it's paused for a usage limit.
    paused: Option<Duration>,
}

/// What a run counted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    pub goals: u64,
    pub met: u64,
    pub sends: u64,
    pub server_crashes: u64,
    pub restarts: u64,
    pub aways: u64,
    pub pauses: u64,
    pub clears: u64,
    pub stops_short: u64,
    pub polls: u64,
    /// Server crashes at a wait of the driver's, mid-tick.
    pub crashes_mid: u64,
}

impl Counts {
    pub fn add(&mut self, o: &Self) {
        self.goals += o.goals;
        self.met += o.met;
        self.sends += o.sends;
        self.server_crashes += o.server_crashes;
        self.restarts += o.restarts;
        self.aways += o.aways;
        self.pauses += o.pauses;
        self.clears += o.clears;
        self.stops_short += o.stops_short;
        self.polls += o.polls;
        self.crashes_mid += o.crashes_mid;
    }
}

struct Sim {
    clock: SimClock,
    world: World,
    server: Node,
    rng: Rng,
    trace: Trace,
    queue: Queue<Ev>,
    store: Store,
    port: SimHarness<Info, Command>,
    shutdown: Shutdown,
    agents: Vec<Agent>,
    until: Duration,
    end: Duration,
    up: bool,
    /// When the server last crashed.
    crashed_at: Option<Duration>,
    /// How often agent events are let in between the driver's waits, per thousand.
    meddle: u64,
    /// How often the server crashes right after one of the driver's polls (at the wait it reached),
    /// per thousand polls, while the nemesis lasts.
    crash_mid: u64,
    counts: Counts,
}

/// The driver's loop, as `Driver::drive` runs it: its own round of the stacks every tick (what
/// it does then for agents no stack drives, `keep`, is the process's, shared with other tests: the
/// model's agents have it off). Its own lock for each change: `CHANGING` is the process's too.
async fn driver(store: Store, clock: SimClock, port: SimHarness<Info, Command>, shutdown: Shutdown) {
    // Its own counters and no planner, as the driver's tests have (the process's are shared).
    let metrics = crate::metrics::AgentMetrics::default();
    let d = Driver { store: &store, clock: &clock, shutdown: &shutdown, port: &port, hub: None, metrics: &metrics, planner: None };
    let changing = async_lock::Mutex::new(());
    loop {
        if !d.tick().await {
            return;
        }
        d.round(&changing).await;
    }
}

fn catalog(kind: Kind) -> Value {
    match kind {
        Kind::Goal => json!({"commands": [{"name": "goal", "route": {"kind": "prompt"}}]}),
        Kind::Control => json!({"commands": [{"name": "goal", "route": {"kind": "control"}}]}),
        Kind::Message => json!({"commands": []}),
    }
}

/// The goal a command carries, as the agent takes it.
fn goal_of(c: &Command) -> Option<String> {
    match c {
        Command::Send { text, .. } => {
            if let Some(g) = text.strip_prefix("/goal ") {
                return Some(g.trim().to_owned());
            }
            for head in ["Your goal isn't met yet: ", "Your goal: "] {
                if let Some(rest) = text.strip_prefix(head) {
                    return Some(rest.split("\n\n").next().unwrap_or(rest).trim().to_owned());
                }
            }
            None
        }
        Command::Control { id, value } if id == "goal" => value.as_ref().and_then(|v| v.get("text")).and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

impl Sim {
    fn new(seed: u64, keep: usize) -> Result<Self, String> {
        let mut rng = Rng::new(seed);
        let clock = SimClock::new();
        let mut world = World::new(clock.clone(), rng.stream("scheduler"));
        let server = world.add_node();
        let until = rng.span(Duration::from_secs(5 * 60), Duration::from_secs(30 * 60));
        let store = Store::in_memory_inline().map_err(|e| format!("{e:#}"))?;
        let port: SimHarness<Info, Command> = SimHarness::new(Arc::new(clock.clone()));
        let mut agents = Vec::new();
        let now_ms = SIM_EPOCH_MS;
        for i in 0..rng.range(1, 3) {
            let kind = *rng.pick(&[Kind::Goal, Kind::Control, Kind::Message]).unwrap_or(&Kind::Goal);
            let id = format!("sim-goals-{seed}-{i}");
            let goals: Vec<String> = (0..rng.range(1, 5)).map(|k| format!("goal {k} of agent {i}")).collect();
            port.open(&id, Info { id: id.clone(), status: Status::Idle, keep_off: true, last_activity: now_ms, ..Info::default() });
            port.set_catalog(&id, catalog(kind));
            let (a, gs) = (id.clone(), goals.clone());
            futures::executor::block_on(store.call(move |c| {
                touch(c, &a, Some("running"), None, now_ms)?;
                for (pos, text) in gs.iter().enumerate() {
                    c.execute("INSERT INTO goal_items (id, agent, pos, text, status) VALUES (?1, ?2, ?3, ?4, 'pending')", params![format!("{a}-g{pos}"), a, i64::try_from(pos).unwrap_or(0), text])?;
                }
                Ok(())
            }))
            .map_err(|e| format!("{e:#}"))?;
            agents.push(Agent { id, kind, heard: 0, on: None, turn: 0, cleared: Vec::new(), goals, sent: Vec::new(), cut_at: None, paused: None });
        }
        let meddle = rng.range(0, 300);
        let crash_mid = rng.range(0, 8);
        let mut sim = Self {
            clock,
            world,
            server,
            trace: Trace::new(seed, keep),
            queue: Queue::new(),
            store,
            port,
            shutdown: Shutdown::new(),
            until,
            end: until.saturating_add(SETTLE),
            up: false,
            crashed_at: None,
            meddle,
            crash_mid,
            counts: Counts::default(),
            agents,
            rng,
        };
        sim.counts.goals = sim.agents.iter().map(|a| u64::try_from(a.goals.len()).unwrap_or(0)).sum();
        sim.server_up()?;
        sim.plan();
        Ok(sim)
    }

    fn now(&self) -> Duration {
        self.clock.elapsed()
    }

    fn unix_ms(&self) -> i64 {
        SIM_EPOCH_MS.saturating_add(i64::try_from(self.now().as_millis()).unwrap_or(i64::MAX))
    }

    fn after(&mut self, d: Duration, ev: Ev) {
        let at = self.now().saturating_add(d);
        self.queue.push(at, ev);
    }

    fn plan(&mut self) {
        let until = self.until;
        let n = self.agents.len();
        let at = |rng: &mut Rng| rng.span(Duration::ZERO, until);
        for _ in 0..self.rng.range(0, 3) {
            let t = at(&mut self.rng);
            self.queue.push(t, Ev::ServerCrash);
        }
        for i in 0..n {
            for _ in 0..self.rng.range(0, 2) {
                let t = at(&mut self.rng);
                self.queue.push(t, Ev::Away(i));
            }
            for _ in 0..self.rng.range(0, 2) {
                let t = at(&mut self.rng);
                self.queue.push(t, Ev::Restart(i));
            }
            for _ in 0..self.rng.range(0, 1) {
                let t = at(&mut self.rng);
                self.queue.push(t, Ev::Pause(i));
            }
            if self.rng.chance(200) {
                let t = at(&mut self.rng);
                self.queue.push(t, Ev::Clear(i));
            }
        }
    }

    fn server_up(&mut self) -> Result<(), String> {
        let sp = self.world.spawner(self.server).ok_or("no server node")?;
        spawn(sp.as_ref(), driver(self.store.clone(), self.clock.clone(), self.port.clone(), self.shutdown.clone())).map_err(|e| format!("{e}"))?;
        self.up = true;
        Ok(())
    }

    fn update(&self, i: usize, f: impl FnOnce(&mut Info)) {
        if let Some(a) = self.agents.get(i) {
            self.port.update(&a.id, f);
        }
    }

    /// The agents take in what was delivered to them, at once.
    fn hear(&mut self) -> Result<(), String> {
        let now = self.now();
        let now_ms = self.unix_ms();
        for i in 0..self.agents.len() {
            let Some(a) = self.agents.get(i) else { continue };
            let heard = self.port.heard(&a.id);
            let new: Vec<Command> = heard.into_iter().skip(a.heard).map(|d| d.command).collect();
            if new.is_empty() {
                continue;
            }
            for c in new {
                let Some(goal) = goal_of(&c) else { continue };
                self.counts.sends += 1;
                self.trace.fold(kiln_sim::rng::fnv(goal.as_bytes()) ^ u64::try_from(i).unwrap_or(0));
                let Some(a) = self.agents.get_mut(i) else { continue };
                // Paused before the driver last looked (one that paused as the driver went from looking
                // to sending is a race no driver can close: its harness holds the message).
                if a.paused.is_some_and(|since| since < now) {
                    return Err(format!("{:?}: {} was sent its goal ({goal}) while paused for a usage limit", now, a.id));
                }
                // Not again within 30 s of the last time, unless something cut it off from it since.
                if let Some((last_at, _)) = a.sent.iter().rev().find(|(_, g)| *g == goal) {
                    let gap = now.saturating_sub(*last_at);
                    let cut = a.cut_at.is_some_and(|c| c >= *last_at) || self.crashed_at.is_some_and(|c| c >= *last_at);
                    if gap < Duration::from_millis(u64::try_from(REMIND_MS).unwrap_or(30_000)) && !cut {
                        return Err(format!("{:?}: {} was sent {goal} again {gap:?} after the last time", now, a.id));
                    }
                }
                a.sent.push((now, goal.clone()));
                a.turn = a.turn.saturating_add(1);
                a.on = Some(goal.clone());
                let (id, kind, turn) = (a.id.clone(), a.kind, a.turn);
                self.port.update(&id, |info| {
                    info.status = Status::Running;
                    info.last_activity = now_ms;
                    info.summary.clear();
                    if kind != Kind::Message {
                        info.objective = Some(Objective { text: goal.clone(), status: "active".into(), reason: None, at: now_ms });
                    }
                });
                let work = if self.rng.chance(200) { self.rng.span(Duration::from_secs(120), Duration::from_secs(600)) } else { self.rng.span(Duration::from_secs(3), Duration::from_secs(90)) };
                self.after(work, Ev::Done(i, turn));
            }
            if let Some(a) = self.agents.get_mut(i) {
                a.heard = self.port.heard(&a.id).len();
            }
        }
        Ok(())
    }

    fn act(&mut self, ev: Ev) -> Result<(), String> {
        let now = self.now();
        let now_ms = self.unix_ms();
        let faulty = now < self.until;
        match ev {
            Ev::Done(i, turn) => {
                let Some(a) = self.agents.get_mut(i) else { return Ok(()) };
                if a.turn != turn {
                    return Ok(());
                }
                let Some(goal) = a.on.take() else { return Ok(()) };
                // After the nemesis, every goal is met.
                let roll = if faulty { self.rng.below(100) } else { 0 };
                let (kind, id) = (a.kind, a.id.clone());
                if roll < 60 {
                    self.counts.met += 1;
                } else {
                    self.counts.stops_short += 1;
                }
                self.port.update(&id, |info| {
                    info.status = Status::Idle;
                    info.last_activity = now_ms;
                    match kind {
                        Kind::Message => info.summary = if roll < 60 { "It's done.\n\nGOAL MET".into() } else { "I'll pick this up next.".into() },
                        Kind::Goal | Kind::Control => {
                            let status = match roll {
                                0..60 => "met",
                                60..80 => "active",
                                80..90 => "unmet",
                                _ => "paused",
                            };
                            info.objective = Some(Objective { text: goal.clone(), status: status.into(), reason: None, at: now_ms });
                        }
                    }
                });
                self.trace.note(|| format!("{now:?} {id} ends its turn on {goal}: roll {roll}"));
            }
            Ev::Away(i) => {
                if !faulty {
                    return Ok(());
                }
                if let Some(a) = self.agents.get(i) {
                    self.port.away(&a.id, true);
                }
                self.update(i, |info| info.status = Status::Disconnected);
                self.counts.aways += 1;
                let back = self.rng.span(Duration::from_secs(5), Duration::from_secs(300));
                self.after(back, Ev::Back(i));
            }
            Ev::Back(i) => {
                let working = self.agents.get(i).is_some_and(|a| a.on.is_some());
                if let Some(a) = self.agents.get_mut(i) {
                    self.port.away(&a.id, false);
                    a.cut_at = Some(now);
                }
                self.update(i, |info| info.status = if working { Status::Running } else { Status::Idle });
            }
            Ev::Restart(i) => {
                if !faulty {
                    return Ok(());
                }
                // Its harness starts again under it: its goal (and the turn under way) lost.
                if let Some(a) = self.agents.get_mut(i) {
                    a.turn = a.turn.saturating_add(1);
                    a.on = None;
                    a.cut_at = Some(now);
                }
                self.update(i, |info| {
                    info.status = Status::Idle;
                    info.objective = None;
                    info.last_activity = now_ms;
                });
                self.counts.restarts += 1;
            }
            Ev::Pause(i) => {
                if !faulty {
                    return Ok(());
                }
                if let Some(a) = self.agents.get_mut(i) {
                    a.paused = Some(now);
                    a.turn = a.turn.saturating_add(1);
                    a.on = None;
                }
                self.update(i, |info| {
                    info.status = Status::Idle;
                    info.pause = Some(Pause { reason: "limit".into(), since: now_ms, auto: true, by: "harness".into(), ..Pause::default() });
                });
                self.counts.pauses += 1;
                let back = self.rng.span(Duration::from_secs(60), Duration::from_secs(900));
                self.after(back, Ev::Unpause(i));
            }
            Ev::Unpause(i) => {
                if let Some(a) = self.agents.get_mut(i) {
                    a.paused = None;
                    a.cut_at = Some(now);
                }
                self.update(i, |info| info.pause = None);
            }
            Ev::Clear(i) => {
                // A person clears the agent's goal in the agent itself.
                let Some(a) = self.agents.get_mut(i) else { return Ok(()) };
                if a.kind == Kind::Message {
                    return Ok(());
                }
                let mut cleared = None;
                self.port.update(&a.id, |info| {
                    if let Some(o) = info.objective.as_mut().filter(|o| o.status == "active") {
                        o.status = "cleared".into();
                        o.at = now_ms;
                        cleared = Some(o.text.clone());
                    }
                });
                if let Some(text) = cleared {
                    a.cleared.push(text);
                    a.turn = a.turn.saturating_add(1);
                    a.on = None;
                    self.update(i, |info| info.status = Status::Idle);
                    self.counts.clears += 1;
                }
            }
            Ev::ServerCrash => {
                if faulty && self.up {
                    self.world.crash(self.server);
                    self.up = false;
                    self.crashed_at = Some(now);
                    self.counts.server_crashes += 1;
                    let back = self.rng.span(Duration::from_secs(1), Duration::from_secs(120));
                    self.after(back, Ev::ServerUp);
                }
            }
            Ev::ServerUp => {
                if !self.up {
                    self.server_up()?;
                    self.crashed_at = Some(now);
                }
            }
        }
        Ok(())
    }

    /// The stacks as the store holds them: each agent's goals, in order, with how each stands.
    fn stacks(&self) -> Result<BTreeMap<String, (String, Vec<(String, String)>)>, String> {
        futures::executor::block_on(self.store.call(|c| {
            let mut out: BTreeMap<String, (String, Vec<(String, String)>)> = BTreeMap::new();
            let mut st = c.prepare("SELECT agent, state FROM goal_stacks")?;
            for row in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
                let (agent, state) = row?;
                out.entry(agent).or_default().0 = state;
            }
            let mut it = c.prepare("SELECT agent, text, status FROM goal_items ORDER BY agent, pos")?;
            for row in it.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))? {
                let (agent, text, status) = row?;
                out.entry(agent).or_default().1.push((text, status));
            }
            Ok(out)
        }))
        .map_err(|e| format!("{e:#}"))
    }

    /// Between the driver's ticks: one goal under way at most, in order, ended ones never back.
    /// Whether every stack is done.
    fn check_stacks(&self, ended: &mut BTreeMap<(String, String), String>) -> Result<bool, String> {
        let now = self.now();
        let mut all_done = true;
        for (agent, (state, items)) in self.stacks()? {
            all_done &= state == "done";
            let mut open_before = false;
            let mut active = 0;
            for (text, status) in &items {
                let key = (agent.clone(), text.clone());
                if let Some(was) = ended.get(&key)
                    && was != status
                {
                    return Err(format!("{now:?}: {agent}'s goal {text} ended {was}, and is {status} now"));
                }
                match status.as_str() {
                    "active" => {
                        active += 1;
                        if open_before {
                            return Err(format!("{now:?}: {agent}'s goal {text} is under way before a goal ahead of it ended: {items:?}"));
                        }
                    }
                    "pending" => open_before = true,
                    "met" | "skipped" | "unmet" => {
                        ended.insert(key, status.clone());
                    }
                    _ => {}
                }
            }
            if active > 1 {
                return Err(format!("{now:?}: {agent} has {active} goals under way: {items:?}"));
            }
        }
        Ok(all_done)
    }

    fn run(&mut self) -> Result<(), String> {
        let mut ended = BTreeMap::new();
        for _ in 0..STEPS_MAX {
            self.hear()?;
            // Now and then, what an agent does next comes in between the driver's waits.
            if self.rng.chance(self.meddle)
                && self.queue.next_at().is_some_and(|t| t <= self.now().saturating_add(Duration::from_secs(2)))
                && let Some((_, ev)) = self.queue.pop()
            {
                self.act(ev)?;
                continue;
            }
            if self.world.step().is_some() {
                self.counts.polls += 1;
                // A crash at the wait the driver just reached.
                if self.now() < self.until && self.up && self.rng.chance(self.crash_mid) {
                    self.act(Ev::ServerCrash)?;
                    self.counts.crashes_mid += 1;
                }
                continue;
            }
            // Nothing ready: the driver waits for its next tick.
            if self.check_stacks(&mut ended)? && self.now() >= self.until {
                break;
            }
            let next = match (self.queue.next_at(), self.world.next_deadline()) {
                (Some(a), Some(b)) => a.min(b),
                (a, b) => match a.or(b) {
                    Some(t) => t,
                    None => break,
                },
            };
            if next > self.end {
                break;
            }
            self.world.advance_to(next);
            self.trace.fold(u64::try_from(next.as_millis()).unwrap_or(0));
            while let Some((_, ev)) = self.queue.pop_due(next) {
                self.act(ev)?;
            }
        }
        self.check_end()
    }

    /// Once the nemesis has stopped: every stack ran to its end, each goal met, or skipped by a
    /// person's clear.
    fn check_end(&self) -> Result<(), String> {
        let stacks = self.stacks()?;
        for a in &self.agents {
            let Some((state, items)) = stacks.get(&a.id) else { return Err(format!("{}'s stack is gone", a.id)) };
            for (text, status) in items {
                let fine = status == "met" || (status == "skipped" && a.cleared.contains(text));
                if !fine {
                    return Err(format!("{}'s goal {text} ended {status} ({state}) though the nemesis stopped long ago: {items:?}", a.id));
                }
            }
            if state != "done" {
                return Err(format!("{}'s stack is {state}, not done: {items:?}", a.id));
            }
        }
        Ok(())
    }
}

/// One seed's run: what it counted and its digest, or how it failed.
pub fn run(seed: u64, keep: usize) -> Result<(Counts, u64), String> {
    let mut sim = Sim::new(seed, keep).map_err(|why| format!("seed {seed}: {why}"))?;
    let ran = sim.run();
    sim.shutdown.cancel();
    sim.world.crash(sim.server);
    match ran {
        Ok(()) => Ok((sim.counts.clone(), sim.trace.digest())),
        Err(why) => {
            let tail = if sim.trace.keeps() { format!("\n--- its last steps:\n{}", sim.trace.tail_text()) } else { String::new() };
            Err(format!("{why}{tail}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use kiln_sim::seeds;

    use super::*;

    /// CP-T6.1, AC-T6.1.1: the goal driver's model over the seed budget: stacks run in order, one
    /// goal at a time, nothing to a paused agent, no goal sent again too soon, and every stack runs
    /// to its end once the nemesis stops; and every fault happened.
    #[test]
    fn goal_stacks_run_in_order_to_their_end_under_the_nemesis() -> Result<(), String> {
        let mut all = Counts::default();
        let report = seeds::run_budget("goals", 50, |seed| {
            let keep = if seeds::replay().is_some() { 400 } else { 0 };
            let (counts, digest) = run(seed, keep)?;
            all.add(&counts);
            Ok(digest)
        });
        eprintln!("{}\n  counted: {all:?}", report.line());
        report.into_result()?;
        if seeds::replay().is_none() {
            let none = [("server crashes", all.server_crashes), ("crashes at a wait of the driver's", all.crashes_mid), ("restarts", all.restarts), ("machines away", all.aways), ("pauses", all.pauses), ("goals stopped short", all.stops_short), ("goals met", all.met)];
            if let Some((what, _)) = none.iter().find(|(_, n)| *n == 0) {
                return Err(format!("no {what} over the budget: the model didn't test what it says"));
            }
        }
        Ok(())
    }

    /// CP-T6.1, AC-T6.1.4: a seed runs the same every time.
    #[test]
    fn a_goals_seed_replays_exactly() -> Result<(), String> {
        for seed in [3, 41] {
            seeds::twice(seed, |s| run(s, 0).map(|(_, d)| d))?;
        }
        Ok(())
    }
}
