//! The gate's guard (LB05, docs/protect.md "The guard"): what stops kiln's heaviest work, the gate
//! (`ci/gate.sh`, through `kiln protect`), when the machine's own services suffer, as a state
//! machine: what was measured and when in ([`Input`]), what to do and why out ([`Decision`]).
//!
//! It replaces the gate's loop of 2026-10-28 (audit item G11), which looked every 10 s at
//! system.slice's I/O pressure ("full" avg10) and at how long a URL took to answer, and stopped the
//! gate three looks running past 30 % or 3 s. Nothing here looks on a clock:
//!
//! - **The kernel wakes it** ([`Decision::Trigger`]) once a window of the machine's stall is enough
//!   that some service may have passed a CUSUM's reference ([`super::stall::wake_us`]), and it reads
//!   then how long each service it protects was stalled: a whole deadline of it in a window is harm;
//!   the CUSUM, calibrated on the guard's own record against the person's false-alarm budget, alarms
//!   sooner on a rise the record says isn't noise.
//! - **A kiln server says its own strain** (`GET /healthz/strain`): its queues standing past their
//!   calm as they begin and end, and, at least once a deadline (the guard's heartbeat,
//!   [`Guard::within_us`]), how late its ready work ran, read by headroom's detector against the
//!   server's own tolerance. A line later than RFC 6298 allows past when it was due is a server that
//!   stopped answering ([`super::rto`]).
//! - **A stop is made only where kiln's work can be the cause** ([`super::cause`]); what isn't known
//!   stops it, the side that protects the service. A server's own queue standing while its shard
//!   ran most of the time is its own work, not kiln's.
//! - **Before the work starts**, a service already strained keeps it from starting: overshoot is
//!   prevented, not reacted to.
//!
//! Every decision says why, with its numbers and where they come from, and the inputs replay
//! ([`replay`]): the same inputs from the same learned state give the same decisions.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::budget;
use super::cause::{self, Resource, Use, Verdict};
use super::objective::FLOW_US;
use super::rto::Rto;
use super::stall::{Limits, Read, Stall};
use crate::headroom::detect::{self, Detector};

/// The version of what's learned this build reads as its own.
pub const LEARNED_VERSION: u32 = 1;

/// What a person decides (docs/protect.md "Settings").
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    /// The deadline the machine's services are held to (µs): a person's flow of thought by default
    /// ([`FLOW_US`]).
    pub deadline_us: f64,
    /// How many times a day the guard may stop work that harmed nothing: headroom's default, one.
    pub false_alarms_per_day: f64,
}

impl Default for Policy {
    fn default() -> Self {
        Self { deadline_us: FLOW_US, false_alarms_per_day: 1.0 }
    }
}

/// What the guard can watch on its machine, as the caller found it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Setup {
    pub policy: Policy,
    /// The watcher has `CAP_SYS_RESOURCE`: any PSI window up to 10 s, looked at ten times a window;
    /// else whole 2 s windows looked at every 2 s (`super::budget`).
    #[serde(default)]
    pub privileged: bool,
    /// The resources a kernel trigger can wake it for: those whose pressure file took one.
    #[serde(default)]
    pub resources: Vec<Resource>,
    /// The kiln servers whose strain it reads, by URL.
    #[serde(default)]
    pub servers: Vec<String>,
    /// The clock's granularity (µs): RFC 6298's G.
    #[serde(default)]
    pub granularity_us: f64,
    /// The machine's CPUs (online): how far PSI's averaging can dilute one service's stall in the
    /// machine's (`super::stall::wake_us`).
    #[serde(default)]
    pub cpus: f64,
}

/// What the guard learned, kept between runs (one file a machine).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Learned {
    #[serde(default)]
    pub version: u32,
    /// Each resource's stall watch, by its name.
    #[serde(default)]
    pub stalls: BTreeMap<String, Stall>,
    /// Each server's delay, by its URL: headroom's detector against the server's own tolerance.
    #[serde(default)]
    pub servers: BTreeMap<String, Detector>,
    /// The longest a stop took to take effect (µs): the reaction's acting part.
    #[serde(default)]
    pub act_us: Option<f64>,
    /// The time watched in all (µs), the servers' detectors' clock: their false-alarm budget is
    /// spread over the time they watched, not over the days between gates.
    #[serde(default)]
    pub watched_us: f64,
}

/// A line of a kiln server's strain stream (`GET /healthz/strain`, the server's JSON), every time in
/// seconds. Fields only grow, each new one optional.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Line {
    /// Sent because the watcher's deadline for a line came (a heartbeat), not for an event.
    #[serde(default)]
    pub beat: bool,
    /// The server's shards' figures, as `/healthz/delay` says them: every probe's lateness added up,
    /// how many ran, a hold under way past their period, and that period (the server's tolerance).
    pub late_s: f64,
    pub probes: f64,
    #[serde(default)]
    pub held_s: f64,
    pub period_s: f64,
    /// The worst shard's delay now.
    #[serde(default)]
    pub delay_s: f64,
    /// What each shard would shed now (0 nothing, 1 background, 2 the API too): its queue stood past
    /// its calm while it ran most of the time, on its own work (`kiln::overload`).
    #[serde(default)]
    pub shards: Vec<u8>,
    /// Its pools' (`kiln::pool_health::QUEUES`) the same, by name: their lines stood past their calm.
    #[serde(default)]
    pub pools: BTreeMap<String, u8>,
    /// Requests it answered 503 at once for load so far, by class (`background`, `normal`).
    #[serde(default)]
    pub shed: BTreeMap<String, u64>,
    /// What it says of its standing, when anything stands.
    #[serde(default)]
    pub why: Option<String>,
}

/// kiln's own work's counters, cumulative, from its cgroup (`cause::Use` spans them).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Own {
    pub cpu_us: f64,
    /// None where its cgroup has no io controller to count it (`cause::Use::io_bytes`).
    #[serde(default)]
    pub io_bytes: Option<f64>,
    pub dirty_bytes: f64,
    pub mem_bytes: f64,
}

impl Own {
    /// What it did since `before` (and holds now).
    fn since(self, before: Own) -> Use {
        let io_bytes = self.io_bytes.zip(before.io_bytes).map(|(now, then)| (now - then).max(0.0));
        Use { cpu_us: (self.cpu_us - before.cpu_us).max(0.0), io_bytes, dirty_bytes: self.dirty_bytes, mem_bytes: self.mem_bytes }
    }
}

/// A protected cgroup's PSI for a resource: its "full" total (µs) and its "full" avg10 (percent).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pressure {
    pub cgroup: String,
    pub full_us: f64,
    #[serde(default)]
    pub avg10: f64,
}

/// What came in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Input {
    /// The kernel's counters, read now: each protected cgroup's pressure for each resource watched
    /// (`woke`: those whose trigger woke this read), kiln's own work's counters, and what the
    /// machine's disks wrote a second over the span.
    Read {
        now_us: u64,
        #[serde(default)]
        woke: Vec<Resource>,
        pressure: Vec<(Resource, Vec<Pressure>)>,
        #[serde(default)]
        own: Option<Own>,
        #[serde(default)]
        written_bps: Option<f64>,
    },
    /// A server's line, with kiln's own work's counters as it came.
    Line {
        now_us: u64,
        server: String,
        line: Line,
        #[serde(default)]
        own: Option<Own>,
    },
    /// A server's stream ended, or never opened.
    Gone { now_us: u64, server: String, why: String },
    /// Every server's first word is in, and the first read: may the work start?
    Admit { now_us: u64 },
    /// The time it asked to be woken at came.
    Due { now_us: u64 },
    /// What it stopped stopped, `took_us` after it was told to.
    Stopped { now_us: u64, took_us: f64 },
    /// The work ended.
    Ended { now_us: u64 },
    /// The kernel took away the trigger for `resource`, or never took it (an older kernel takes one
    /// from root alone): its stalls go unwatched.
    Lost { now_us: u64, resource: Resource, why: String },
}

/// What to do.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Decision {
    /// Start the work: nothing watched is strained.
    Start { why: String },
    /// Don't: what it protects is strained now.
    Refuse { why: String },
    /// Stop the work now.
    Stop { why: String },
    /// Ask the kernel to wake the guard once a window of `window_us` holds `stall_us` of "some"
    /// stall on `resource` on the whole machine: what any service's "full" stall at the reference
    /// forces it to (`stall::wake_us`).
    Trigger { resource: Resource, stall_us: u64, window_us: u64 },
    /// Wake at `at_us` if nothing else comes first: a server's line is due.
    Due { at_us: u64 },
    /// What was seen, and why nothing was done.
    Note { what: String },
}

/// A server as the guard follows it.
#[derive(Clone, Debug, Default)]
struct Server {
    rto: Rto,
    /// When its last line came, and the line.
    last: Option<(u64, Line)>,
    gone: Option<String>,
    /// kiln's own counters as its last line came.
    own: Option<Own>,
}

pub struct Guard {
    pub setup: Setup,
    pub learned: Learned,
    began_us: u64,
    started: bool,
    stopped: bool,
    /// The read before: when, each cgroup's "full" total by resource, and kiln's own counters.
    last_read: Option<(u64, BTreeMap<(Resource, String), f64>, Option<Own>)>,
    /// Each resource's trigger as asked (µs a window): the reference in force until the next.
    armed: BTreeMap<Resource, f64>,
    servers: BTreeMap<String, Server>,
    /// Before the work starts: each protected cgroup's "full" avg10 (percent), by resource, as read.
    avg10: Vec<(Resource, String, f64)>,
}

/// A time in words.
fn secs(us: f64) -> String {
    format!("{:.2} s", us / 1e6)
}

fn mb(b: f64) -> String {
    format!("{:.0} MB", b / 1e6)
}

impl Guard {
    /// A guard begun at `now_us`, from what it learned before (an older build's let go of).
    pub fn new(setup: Setup, learned: Learned, now_us: u64) -> Self {
        let learned = if learned.version == LEARNED_VERSION { learned } else { Learned { version: LEARNED_VERSION, ..Learned::default() } };
        let servers = setup.servers.iter().map(|s| (s.clone(), Server::default())).collect();
        Self { setup, learned, began_us: now_us, started: false, stopped: false, last_read: None, armed: BTreeMap::new(), servers, avg10: Vec::new() }
    }

    /// How often it asks each server for a line at least (µs): the deadline, so a server silent for a
    /// whole deadline is one it can't vouch for.
    pub fn within_us(&self) -> f64 {
        self.setup.policy.deadline_us
    }

    /// The PSI window and the stall in it that is harm, where the kernel takes a window for the
    /// deadline.
    pub fn limits(&self) -> Option<Limits> {
        budget::psi_window_us(self.setup.policy.deadline_us, self.setup.privileged).map(|window_us| Limits { window_us, harm_us: self.setup.policy.deadline_us })
    }

    fn budget(&self) -> f64 {
        self.setup.policy.false_alarms_per_day
    }

    /// The detectors' clock at `now_us`: the time watched before this run, and this run's so far.
    fn clock(&self, now_us: u64) -> u64 {
        (self.learned.watched_us.max(0.0) + now_us.saturating_sub(self.began_us) as f64) as u64
    }

    /// What to arm before the first read: a trigger for each resource watched, and when each
    /// server's first line is due (RFC 6298's timeout before anything was measured, past the
    /// deadline).
    pub fn begin(&mut self) -> Vec<Decision> {
        let mut out = self.triggers();
        if self.limits().is_none() && !self.setup.resources.is_empty() {
            out.push(Decision::Note { what: format!("no PSI window holds a deadline of {}: the kernel takes 10 s at most; stalls unwatched", secs(self.setup.policy.deadline_us)) });
        }
        out.extend(self.due());
        out
    }

    /// Triggers whose reference moved since they were armed: each set at the machine's "some" a
    /// service's stall at the reference forces (`stall::wake_us`), the reference kept as the one the
    /// next span is read against.
    fn triggers(&mut self) -> Vec<Decision> {
        let Some(l) = self.limits() else { return Vec::new() };
        let mut out = Vec::new();
        for r in self.setup.resources.clone() {
            let k = self.learned.stalls.get(r.name()).map_or_else(|| Stall::default().trigger_us(l), |s| s.trigger_us(l));
            if self.armed.get(&r) != Some(&k) {
                self.armed.insert(r, k);
                out.push(Decision::Trigger { resource: r, stall_us: super::stall::wake_us(k, self.setup.cpus, l.window_us) as u64, window_us: l.window_us as u64 });
            }
        }
        out
    }

    /// When the next server line is due: the earliest of each one's last line (or the guard's
    /// beginning) plus the deadline plus its RFC 6298 timeout.
    fn next_due(&self) -> Option<u64> {
        let g = self.setup.granularity_us;
        self.servers.values().filter(|s| s.gone.is_none()).map(|s| s.last.as_ref().map_or(self.began_us, |(at, _)| *at).saturating_add((self.within_us() + s.rto.timeout_us(g)) as u64)).min()
    }

    fn due(&self) -> Vec<Decision> {
        self.next_due().map(|at_us| Decision::Due { at_us }).into_iter().collect()
    }

    /// One input; what to do.
    pub fn step(&mut self, input: &Input) -> Vec<Decision> {
        match input {
            Input::Read { now_us, woke, pressure, own, written_bps } => self.read(*now_us, woke, pressure, *own, *written_bps),
            Input::Line { now_us, server, line, own } => self.line(*now_us, server, line, *own),
            Input::Gone { now_us, server, why } => self.gone(*now_us, server, why),
            Input::Admit { now_us } => self.admit(*now_us),
            Input::Due { now_us } => self.due_now(*now_us),
            Input::Stopped { took_us, .. } => {
                if took_us.is_finite() && *took_us >= 0.0 {
                    self.learned.act_us = Some(self.learned.act_us.map_or(*took_us, |a| a.max(*took_us)));
                }
                Vec::new()
            }
            Input::Ended { now_us } => {
                self.forget_excursions();
                self.learned.watched_us = self.clock(*now_us) as f64;
                Vec::new()
            }
            Input::Lost { resource, why, .. } => {
                self.setup.resources.retain(|r| r != resource);
                self.armed.remove(resource);
                vec![Decision::Note { what: format!("{} stalls unwatched: {why}", resource.name()) }]
            }
        }
    }

    /// The excursions under way end unseen: a stop ended them, or the guard goes.
    fn forget_excursions(&mut self) {
        for s in self.learned.stalls.values_mut() {
            s.forget_excursion();
        }
        for d in self.learned.servers.values_mut() {
            d.forget_excursion();
        }
    }

    /// Stop, once: the excursions under way calibrate nothing.
    fn stop(&mut self, why: String) -> Vec<Decision> {
        if self.stopped {
            return vec![Decision::Note { what: format!("stopping already; also: {why}") }];
        }
        if !self.started {
            return vec![Decision::Refuse { why }];
        }
        self.stopped = true;
        self.forget_excursions();
        vec![Decision::Stop { why }]
    }

    /// Acts on a slowdown on `r` where kiln's work could be its cause; notes it where it couldn't.
    fn blame(&mut self, r: Resource, used: Option<Use>, what: String, drain: Option<String>) -> Vec<Decision> {
        match cause::could_be_kiln(r, used) {
            v @ (Verdict::May(_) | Verdict::Unknown(_)) => self.stop(format!("{what}; {}{}", v.why(), drain.unwrap_or_default())),
            Verdict::Not(why) => vec![Decision::Note { what: format!("{what}; not kiln's: {why}, so stopping it couldn't help") }],
        }
    }

    fn read(&mut self, now: u64, woke: &[Resource], pressure: &[(Resource, Vec<Pressure>)], own: Option<Own>, written_bps: Option<f64>) -> Vec<Decision> {
        let mut out = Vec::new();
        let totals: BTreeMap<(Resource, String), f64> = pressure.iter().flat_map(|(r, ps)| ps.iter().filter(|p| p.full_us.is_finite()).map(move |p| ((*r, p.cgroup.clone()), p.full_us))).collect();
        let Some(l) = self.limits() else {
            self.last_read = Some((now, totals, own));
            return out;
        };
        if self.stopped {
            // What follows a stop is its drain: no calm, and nothing to calibrate on.
            self.last_read = Some((now, totals, own));
            return out;
        }
        if !self.started {
            // Before the work starts: the services strained now keep it from starting (Admit).
            self.avg10 = pressure.iter().flat_map(|(r, ps)| ps.iter().map(move |p| (*r, p.cgroup.clone(), p.avg10))).collect();
            self.last_read = Some((now, totals, own));
            return out;
        }
        let Some((then, before, own_before)) = self.last_read.take() else {
            self.last_read = Some((now, totals, own));
            return out;
        };
        let span = now.saturating_sub(then) as f64;
        let used = own.zip(own_before).map(|(o, b)| o.since(b));
        let budget_per_day = self.budget();
        for r in self.setup.resources.clone() {
            // The most stalled service over the span, of those read both times.
            let worst = totals
                .iter()
                .filter(|((rr, _), _)| *rr == r)
                .filter_map(|((_, cg), now_us)| before.get(&(r, cg.clone())).map(|b| ((now_us - b).max(0.0), cg)))
                .fold(None::<(f64, &String)>, |best, (d, cg)| if best.is_none_or(|(bd, _)| d > bd) { Some((d, cg)) } else { best });
            let Some((stall_us, cgroup)) = worst else { continue };
            let cgroup = cgroup.clone();
            let reference = self.armed.get(&r).copied().unwrap_or_else(|| Stall::default().trigger_us(l));
            let watch = self.learned.stalls.entry(r.name().to_owned()).or_default();
            let read = watch.span(stall_us, span, woke.contains(&r), reference, l, budget_per_day);
            let calm = watch.calm_us();
            let recorded = watch.recorded();
            let what = match read {
                Read::Harm { per_window_us } => Some(format!(
                    "{}: {cgroup} was fully stalled {} a {} window over the last {}, past the deadline of {} (harm: every request of it needing {} waited a deadline in that window, or could have)",
                    r.name(),
                    secs(per_window_us),
                    secs(l.window_us),
                    secs(span),
                    secs(l.harm_us),
                    r.name()
                )),
                Read::Alarm { s, threshold, per_window_us } => Some(format!(
                    "{}: {cgroup}'s full stall rose toward harm: {} a {} window over the last {}, its CUSUM {} past a reference of {} (halfway from its calm, {}, to harm, {}), over its threshold {} (calibrated on {recorded} harmless excursions at {} false alarms a day)",
                    r.name(),
                    secs(per_window_us),
                    secs(l.window_us),
                    secs(span),
                    secs(s),
                    secs(reference),
                    secs(calm),
                    secs(l.harm_us),
                    secs(threshold),
                    budget_per_day
                )),
                Read::Calm { .. } => None,
            };
            if let Some(what) = what {
                let drain = (r == Resource::Io).then(|| used.zip(written_bps).and_then(|(u, w)| budget::drain_us(u.dirty_bytes, w).map(|d| format!(" (its {} still to write back drain in about {} at the disks' {}/s)", mb(u.dirty_bytes), secs(d), mb(w))))).flatten();
                out.extend(self.blame(r, used, what, drain));
            }
        }
        self.last_read = Some((now, totals, own));
        out.extend(self.triggers());
        out
    }

    fn line(&mut self, now: u64, name: &str, line: &Line, own: Option<Own>) -> Vec<Decision> {
        let within = self.within_us();
        let budget_per_day = self.budget();
        let started = self.started;
        let clock = self.clock(now);
        let Some(srv) = self.servers.get_mut(name) else { return Vec::new() };
        if srv.gone.is_some() {
            return Vec::new();
        }
        if let (true, Some((at, _))) = (line.beat, &srv.last) {
            // A heartbeat: how late past when it was due, for RFC 6298's estimate.
            srv.rto.sample((now.saturating_sub(*at) as f64 - within).max(0.0));
        }
        let before = srv.last.replace((now, line.clone())).map(|(_, l)| l);
        let own_before = std::mem::replace(&mut srv.own, own);
        let used = match (own, own_before, &before) {
            (Some(o), Some(b), Some(_)) => Some(o.since(b)),
            _ => None,
        };
        if !started || self.stopped {
            // Before the work starts its first line is read for admission; after a stop, the drain.
            return self.due();
        }
        let mut out = Vec::new();
        let own_work = line.shards.iter().any(|l| *l >= 1);
        // How late its ready work ran since its last line, against its own tolerance (its period).
        let tolerance_us = line.period_s * 1e6;
        let mean_us = before.as_ref().and_then(|b| {
            let (probes, late) = (line.probes - b.probes, line.late_s - b.late_s);
            if probes > 0.0 && late >= 0.0 {
                Some(late / probes * 1e6)
            } else if probes <= 0.0 && line.held_s > 0.0 {
                Some(line.held_s * 1e6)
            } else {
                None
            }
        });
        if let Some(us) = mean_us.filter(|_| tolerance_us > 0.0) {
            let d = self.learned.servers.entry(name.to_owned()).or_insert_with(|| Detector::new(tolerance_us, clock));
            if d.tolerance_us != tolerance_us {
                *d = Detector::new(tolerance_us, clock);
            }
            d.window_us = Some(within);
            match d.sample(clock, us, false, budget_per_day) {
                detect::Event::Alarm(why) => {
                    let what = format!("{name}: its ready work ran {} late on average over its last {}: {why}", secs(us), secs(within));
                    if own_work {
                        out.push(Decision::Note { what: format!("{what}; not kiln's: its own shards' queues stood while they ran most of the time ({})", line.why.clone().unwrap_or_default()) });
                    } else {
                        out.extend(self.blame(Resource::Cpu, used, what, None));
                    }
                }
                detect::Event::Regime(why) => out.push(Decision::Note { what: format!("{name}: {why}") }),
                detect::Event::Clear | detect::Event::Same => {}
            }
        }
        // Its pools' lines standing: what they wait on.
        let was = before.as_ref().map(|b| b.pools.clone()).unwrap_or_default();
        for (pool, level) in &line.pools {
            if *level == 0 || was.get(pool).is_some_and(|w| w >= level) {
                continue;
            }
            let what = format!("{name}: work waiting in its {pool} pool stood past its calm ({})", line.why.clone().unwrap_or_default());
            match Resource::of_pool(pool) {
                Some(r) => out.extend(self.blame(r, used, what, None)),
                None => out.push(Decision::Note { what: format!("{what}; not kiln's: it waits on nothing kiln's work uses") }),
            }
        }
        // Requests it refused at once for load: failed service, laid where its standing says.
        let shed: u64 = line.shed.values().sum::<u64>().saturating_sub(before.as_ref().map_or(0, |b| b.shed.values().sum()));
        if shed > 0 && !line.pools.values().any(|l| *l >= 1) {
            let what = format!("{name}: it answered {shed} requests 503 at once for load (failed service, however fast)");
            if own_work {
                out.push(Decision::Note { what: format!("{what}; not kiln's: its own shards' queues stood while they ran most of the time") });
            } else {
                out.extend(self.blame(Resource::Cpu, used, what, None));
            }
        }
        out.extend(self.due());
        out
    }

    fn gone(&mut self, now: u64, name: &str, why: &str) -> Vec<Decision> {
        let Some(srv) = self.servers.get_mut(name) else { return Vec::new() };
        if srv.gone.is_some() {
            return Vec::new();
        }
        srv.gone = Some(why.to_owned());
        let _ = now;
        let mut out = if self.started { self.stop(format!("{name} stopped answering ({why}): kiln can't tell why, and stopping its work is the side that protects it")) } else { Vec::new() };
        out.extend(self.due());
        out
    }

    fn admit(&mut self, now: u64) -> Vec<Decision> {
        let mut why = Vec::new();
        for (name, s) in &self.servers {
            match (&s.gone, &s.last) {
                (Some(g), _) => why.push(format!("{name} doesn't answer ({g})")),
                (None, None) => why.push(format!("{name} hasn't said how it is")),
                (None, Some((_, l))) => {
                    if l.period_s > 0.0 && l.delay_s >= l.period_s {
                        why.push(format!("{name}'s worst shard runs ready work {} late, past its period of {}", secs(l.delay_s * 1e6), secs(l.period_s * 1e6)));
                    }
                    if l.shards.iter().chain(l.pools.values()).any(|v| *v >= 1) {
                        why.push(format!("{name}'s queues stand past their calm ({})", l.why.clone().unwrap_or_default()));
                    }
                }
            }
        }
        if let Some(l) = self.limits() {
            // The kernel's own average over the last 10 s: a service stalled at harm's rate now.
            let harm_pct = l.harm_us / l.window_us * 100.0;
            for (r, cgroup, avg10) in self.avg10.iter().filter(|(r, _, _)| self.setup.resources.contains(r)) {
                if *avg10 >= harm_pct {
                    why.push(format!("{cgroup} is fully stalled on {} {avg10:.1}% of the time (the kernel's avg10), at or past harm's {harm_pct:.0}% (a deadline of {} in a {} window)", r.name(), secs(l.harm_us), secs(l.window_us)));
                }
            }
        }
        let _ = now;
        if why.is_empty() {
            self.started = true;
            let mut out = vec![Decision::Start { why: self.plan() }];
            out.extend(self.due());
            out
        } else {
            vec![Decision::Refuse { why: why.join("; ") }]
        }
    }

    /// What the guard watches and how fast it can act, for its first word.
    fn plan(&self) -> String {
        let mut parts = Vec::new();
        match self.limits() {
            Some(l) if !self.setup.resources.is_empty() => {
                let sense = budget::psi_sense_us(l.window_us, self.setup.privileged);
                let names: Vec<&str> = self.setup.resources.iter().map(|r| r.name()).collect();
                parts.push(format!(
                    "the machine's services' {} stalls, woken by the kernel within {} of a {} window passing its trigger; harm is {} stalled in one",
                    names.join(", "),
                    secs(sense),
                    secs(l.window_us),
                    secs(l.harm_us)
                ));
            }
            _ => parts.push("no stalls (the kernel takes no trigger from this watcher)".to_owned()),
        }
        for name in self.servers.keys() {
            parts.push(format!("{name}'s own word, a line at least every {}", secs(self.within_us())));
        }
        let act = self.learned.act_us.map_or_else(|| "not yet measured".to_owned(), secs);
        format!("watching {}; a stop takes {act}", parts.join("; "))
    }

    fn due_now(&mut self, now: u64) -> Vec<Decision> {
        let g = self.setup.granularity_us;
        let within = self.within_us();
        let began = self.began_us;
        let late: Vec<(String, f64)> = self
            .servers
            .iter()
            .filter(|(_, s)| s.gone.is_none())
            .filter_map(|(n, s)| {
                let from = s.last.as_ref().map_or(began, |(at, _)| *at);
                let allowed = within + s.rto.timeout_us(g);
                (now.saturating_sub(from) as f64 >= allowed).then(|| (n.clone(), allowed))
            })
            .collect();
        let mut out = Vec::new();
        for (name, allowed) in late {
            if let Some(s) = self.servers.get_mut(&name) {
                s.gone = Some("silent".to_owned());
            }
            let what = format!(
                "{name} said nothing for {}: a line was due within the deadline, {}, and RFC 6298's timeout on its lateness, {}, has passed too",
                secs(allowed),
                secs(within),
                secs(allowed - within)
            );
            out.extend(self.stop(what));
        }
        out.extend(self.due());
        out
    }
}

/// The decisions a guard begun from `learned` makes on `inputs`, in order: a run replayed.
pub fn replay(setup: Setup, learned: Learned, began_us: u64, inputs: &[Input]) -> (Vec<Decision>, Vec<Vec<Decision>>) {
    let mut g = Guard::new(setup, learned, began_us);
    let first = g.begin();
    (first, inputs.iter().map(|i| g.step(i)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: &str = "http://127.0.0.1:7070";
    const SVC: &str = "system.slice/sshd.service";

    fn setup(resources: Vec<Resource>, servers: Vec<&str>) -> Setup {
        Setup { policy: Policy::default(), privileged: false, resources, servers: servers.into_iter().map(str::to_owned).collect(), granularity_us: 1.0, cpus: 32.0 }
    }

    fn calm_line(beat: bool, late_s: f64, probes: f64) -> Line {
        Line { beat, late_s, probes, held_s: 0.0, period_s: 0.017, delay_s: 0.0002, ..Line::default() }
    }

    fn read(now_us: u64, woke: &[Resource], full: &[(Resource, f64)], own: Option<Own>) -> Input {
        let pressure = full.iter().map(|(r, us)| (*r, vec![Pressure { cgroup: SVC.into(), full_us: *us, avg10: 0.0 }])).collect();
        Input::Read { now_us, woke: woke.to_vec(), pressure, own, written_bps: Some(5e8) }
    }

    fn stops(d: &[Decision]) -> bool {
        d.iter().any(|d| matches!(d, Decision::Stop { .. }))
    }

    /// A guard started on a calm machine: a baseline read, each server's first line, admitted.
    fn started(resources: Vec<Resource>, servers: Vec<&str>, own: Own) -> Guard {
        let mut g = Guard::new(setup(resources.clone(), servers.clone()), Learned::default(), 0);
        g.begin();
        let full: Vec<(Resource, f64)> = resources.iter().map(|r| (*r, 0.0)).collect();
        g.step(&read(1, &[], &full, Some(own)));
        for s in &servers {
            g.step(&Input::Line { now_us: 2, server: (*s).to_owned(), line: calm_line(false, 1.0, 1_000.0), own: Some(own) });
        }
        let d = g.step(&Input::Admit { now_us: 3 });
        assert!(d.iter().any(|d| matches!(d, Decision::Start { .. })), "{d:?}");
        g
    }

    /// LB05 step 1 and G11: a server answering requests 503 at once for load is failed service, not a
    /// healthy fast one; stopping kiln's work for it is supported only while kiln's work could be the
    /// cause: not while the server's own shards stand on their own work.
    #[test]
    fn lb05_g11_fast_503s_are_failed_service_laid_on_kiln_only_with_evidence() {
        let busy = Own { cpu_us: 0.0, ..Own::default() };
        let mut g = started(vec![], vec![S], busy);
        let mut shedding = calm_line(false, 1.0001, 1_100.0);
        shedding.shed.insert("normal".into(), 40);
        let d = g.step(&Input::Line { now_us: 1_000_000, server: S.into(), line: shedding.clone(), own: Some(Own { cpu_us: 4e6, ..busy }) });
        assert!(stops(&d), "fast 503s beside kiln's running work weren't failed service: {d:?}");
        // The same sheds while the server's own shards stand (they ran most of the time): its own work.
        let mut g = started(vec![], vec![S], busy);
        let mut own = shedding;
        own.shards = vec![1, 0];
        own.why = Some("overloaded: shard 0 sheds background requests".into());
        let d = g.step(&Input::Line { now_us: 1_000_000, server: S.into(), line: own, own: Some(Own { cpu_us: 4e6, ..busy }) });
        assert!(!stops(&d) && d.iter().any(|d| matches!(d, Decision::Note { what } if what.contains("not kiln's"))), "{d:?}");
    }

    /// G11's 3 s answer time, replaced: a server's line is due a deadline after the last, and silence
    /// past RFC 6298's timeout on top of it stops the work; a line in time doesn't. Without the
    /// timeout (the deadline alone), the Due at 1.5 s would stop it.
    #[test]
    fn lb05_g11_a_server_silent_past_its_deadline_and_rto_stops_the_work() {
        let mut g = started(vec![], vec![S], Own::default());
        let due: Vec<u64> = g.step(&Input::Due { now_us: 1_500_000 }).iter().filter_map(|d| if let Decision::Due { at_us } = d { Some(*at_us) } else { None }).collect();
        assert!(!stops(&g.step(&Input::Due { now_us: 1_500_000 })), "stopped within the deadline and its timeout");
        // The first line came at 2 µs: due by 2 µs + 1 s + RFC 6298's initial 1 s.
        assert_eq!(due.first().copied(), Some(2_000_002), "{due:?}");
        let d = g.step(&Input::Due { now_us: 2_000_002 });
        assert!(d.iter().any(|d| matches!(d, Decision::Stop { why } if why.contains("RFC 6298"))), "{d:?}");
        // A heartbeat in time moves the deadline on.
        let mut g = started(vec![], vec![S], Own::default());
        g.step(&Input::Line { now_us: 1_000_100, server: S.into(), line: calm_line(true, 1.0001, 1_060.0), own: Some(Own::default()) });
        assert!(!stops(&g.step(&Input::Due { now_us: 2_500_000 })), "a heartbeat in time didn't move the deadline");
    }

    /// LB05 step 2: the CPU-only and disk-only counterexamples. A service fully stalled on I/O while
    /// kiln's work only computed (no I/O, nothing of its own to write back) isn't kiln's to stop; one
    /// stalled on the CPU while kiln's work only wrote isn't either. With the matching use, it stops.
    #[test]
    fn lb05_g11_cpu_only_and_disk_only_counterexamples_stop_nothing() {
        let both = vec![Resource::Cpu, Resource::Io];
        let computing = Own { cpu_us: 0.0, io_bytes: Some(0.0), ..Own::default() };
        // I/O harm (1.2 s fully stalled of a 2 s window), kiln computing only.
        let mut g = started(both.clone(), vec![], computing);
        let d = g.step(&read(2_000_001, &[Resource::Io], &[(Resource::Io, 1.2e6), (Resource::Cpu, 0.0)], Some(Own { cpu_us: 3e6, ..computing })));
        assert!(!stops(&d), "an I/O stall laid on work that did no I/O: {d:?}");
        // CPU harm, kiln writing only.
        let writing = Own { io_bytes: Some(0.0), ..Own::default() };
        let mut g = started(both.clone(), vec![], writing);
        let d = g.step(&read(2_000_001, &[Resource::Cpu], &[(Resource::Cpu, 1.2e6), (Resource::Io, 0.0)], Some(Own { io_bytes: Some(9e8), dirty_bytes: 2e9, ..writing })));
        assert!(!stops(&d), "a CPU stall laid on work that used no CPU: {d:?}");
        // I/O harm with kiln's own dirty pages draining: its.
        let mut g = started(both, vec![], writing);
        let d = g.step(&read(2_000_001, &[Resource::Io], &[(Resource::Io, 1.2e6), (Resource::Cpu, 0.0)], Some(Own { io_bytes: Some(9e8), dirty_bytes: 2e9, ..writing })));
        assert!(d.iter().any(|d| matches!(d, Decision::Stop { why } if why.contains("harm") && why.contains("drain in about 4.00 s"))), "{d:?}");
    }

    /// The safe side: what kiln's work did unknown (its cgroup not found), harm stops it.
    #[test]
    fn lb05_g11_harm_with_kilns_use_unknown_stops() {
        let mut g = started(vec![Resource::Io], vec![], Own::default());
        g.last_read = g.last_read.take().map(|(t, b, _)| (t, b, None));
        let d = g.step(&read(2_000_001, &[Resource::Io], &[(Resource::Io, 1.5e6)], None));
        assert!(stops(&d), "{d:?}");
    }

    /// Prevent overshoot before the work starts: a server whose queues stand now, whose worst shard runs
    /// ready work a period late, a service the kernel averages at harm's rate, or a server that doesn't
    /// answer, keeps it from starting.
    #[test]
    fn lb05_g11_admission_refuses_beside_a_strained_service() {
        let refused = |line: Option<Line>, avg10: f64, gone: bool| {
            let mut g = Guard::new(setup(vec![Resource::Io], vec![S]), Learned::default(), 0);
            g.begin();
            g.step(&Input::Read { now_us: 1, woke: vec![], pressure: vec![(Resource::Io, vec![Pressure { cgroup: SVC.into(), full_us: 0.0, avg10 }])], own: None, written_bps: None });
            if let Some(l) = line {
                g.step(&Input::Line { now_us: 2, server: S.into(), line: l, own: None });
            }
            if gone {
                g.step(&Input::Gone { now_us: 2, server: S.into(), why: "connection refused".into() });
            }
            g.step(&Input::Admit { now_us: 3 }).iter().any(|d| matches!(d, Decision::Refuse { .. }))
        };
        assert!(!refused(Some(calm_line(false, 1.0, 10.0)), 3.0, false), "a calm machine refused");
        let mut standing = calm_line(false, 1.0, 10.0);
        standing.pools.insert("store_write".into(), 1);
        assert!(refused(Some(standing), 0.0, false));
        let mut late = calm_line(false, 1.0, 10.0);
        late.delay_s = 0.02;
        assert!(refused(Some(late), 0.0, false));
        assert!(refused(Some(calm_line(false, 1.0, 10.0)), 50.0, false), "a service at harm's rate (a 1 s deadline in a 2 s window: 50%)");
        assert!(!refused(Some(calm_line(false, 1.0, 10.0)), 49.0, false));
        assert!(refused(None, 0.0, true));
        assert!(refused(None, 0.0, false), "a server that hasn't said how it is");
    }

    /// The kernel trigger is the reference in force: halfway from no stall to harm at first, then
    /// halfway from what calm turns out to be; the CUSUM over a span runs against the one armed.
    #[test]
    fn lb05_g11_triggers_follow_the_reference_and_harm_is_a_deadline_in_a_window() {
        let mut g = Guard::new(setup(vec![Resource::Io], vec![]), Learned::default(), 0);
        let first = g.begin();
        // The reference 0.5 s: what a service's stall there forces on 32 CPUs in 2 s windows.
        assert!(first.contains(&Decision::Trigger { resource: Resource::Io, stall_us: 3_906, window_us: 2_000_000 }), "{first:?}");
        assert_eq!(g.armed.get(&Resource::Io), Some(&500_000.0), "the reference read against is the reference itself");
        g.step(&read(1, &[], &[(Resource::Io, 0.0)], Some(Own::default())));
        g.step(&Input::Admit { now_us: 2 });
        // A calm of 0.2 s a window for a minute: the trigger moves up toward 0.6 s.
        let mut out = Vec::new();
        for i in 1..=30u64 {
            out.extend(g.step(&read(1 + i * 2_000_000, &[], &[(Resource::Io, i as f64 * 0.2e6)], Some(Own::default()))));
        }
        let last = out.iter().rev().find_map(|d| if let Decision::Trigger { stall_us, .. } = d { Some(*stall_us) } else { None });
        // The reference 0.6 s now: 0.36e12 / (32 x 2e6) = 5,625 µs of the machine's "some".
        assert!(last.is_some_and(|w| (5_500..=5_625).contains(&w)), "{last:?}");
        assert!(g.armed.get(&Resource::Io).is_some_and(|k| (590_000.0..=600_000.0).contains(k)), "{:?}", g.armed);
        assert!(!stops(&out), "{out:?}");
    }

    /// Replayed from its inputs, a run makes the same decisions, through their JSON too: nothing
    /// decides from a clock, a disk or chance of its own.
    #[test]
    fn lb05_g11_decisions_replay_from_their_inputs() -> anyhow::Result<()> {
        let s = setup(vec![Resource::Io, Resource::Memory], vec![S]);
        let mut inputs = vec![
            read(1, &[], &[(Resource::Io, 0.0), (Resource::Memory, 0.0)], Some(Own::default())),
            Input::Line { now_us: 2, server: S.into(), line: calm_line(false, 1.0, 1_000.0), own: Some(Own::default()) },
            Input::Admit { now_us: 3 },
        ];
        for i in 1..=20u64 {
            let t = i * 2_000_000;
            inputs.push(read(t, &[Resource::Io], &[(Resource::Io, i as f64 * 0.4e6), (Resource::Memory, 0.0)], Some(Own { cpu_us: i as f64 * 1e6, io_bytes: Some(i as f64 * 1e8), dirty_bytes: 1e8, mem_bytes: 1e9 })));
            inputs.push(Input::Line { now_us: t + 1, server: S.into(), line: calm_line(true, 1.0 + i as f64 * 0.0001, 1_000.0 + i as f64 * 60.0), own: Some(Own { cpu_us: i as f64 * 1e6, ..Own::default() }) });
        }
        inputs.push(Input::Ended { now_us: 50_000_000 });
        let (a0, a) = replay(s.clone(), Learned::default(), 0, &inputs);
        let json = serde_json::to_string(&inputs)?;
        let back: Vec<Input> = serde_json::from_str(&json)?;
        let (b0, b) = replay(s, Learned::default(), 0, &back);
        anyhow::ensure!(a0 == b0 && a == b, "a replay decided otherwise");
        anyhow::ensure!(a.iter().flatten().any(|d| matches!(d, Decision::Trigger { .. })), "{a:?}");
        Ok(())
    }

    /// A stop is made once, its excursions calibrate nothing, and the time watched carries to the next
    /// run's clock.
    #[test]
    fn lb05_g11_one_stop_and_the_watched_clock_carries_over() {
        let mut g = started(vec![Resource::Io], vec![], Own::default());
        let d = g.step(&read(2_000_001, &[Resource::Io], &[(Resource::Io, 1.5e6)], Some(Own { io_bytes: Some(1e8), ..Own::default() })));
        assert!(stops(&d));
        let again = g.step(&read(4_000_001, &[Resource::Io], &[(Resource::Io, 3.0e6)], Some(Own { io_bytes: Some(2e8), ..Own::default() })));
        assert!(!stops(&again), "stopped twice: {again:?}");
        assert_eq!(g.learned.stalls.get("io").map(Stall::level), Some(0.0), "the stopped excursion is still under way");
        g.step(&Input::Stopped { now_us: 4_100_000, took_us: 120_000.0 });
        g.step(&Input::Ended { now_us: 5_000_000 });
        assert_eq!(g.learned.act_us, Some(120_000.0));
        assert_eq!(g.learned.watched_us, 5_000_000.0);
        let next = Guard::new(setup(vec![Resource::Io], vec![]), g.learned.clone(), 9_000_000_000);
        assert_eq!(next.clock(9_000_000_000), 5_000_000, "the next run's clock starts where this one's ended");
    }
}
