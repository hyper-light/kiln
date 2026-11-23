//! The work headroom can slow, how hard each runs, and the actuator: a group paced at `d`% is
//! stopped for the rest of every cycle and runs its `d`% at the end, switched at each due time
//! (`kiln_ci::headroom::pace::Duty`), so slowing is continuous rather than on or off. Everything
//! here is the runner's own, so none of it needs privileges.
//!
//! Linux: each agent session's cgroup (`crate::os::contain`), each CI job's (a leg's microVM's
//! among them: docs/priority.md), and, for the machine's installed runner unless the settings say
//! not to, every transient unit the user's service manager runs (`systemd-run --user`: a build an
//! agent started to outlive its command), stopped and started with `cgroup.freeze`. macOS and Windows: each agent session's processes as `crate::os::contain`
//! holds them, metered process by process and stopped process by process (SIGSTOP and SIGCONT;
//! every thread suspended and resumed). What someone else stopped is never touched.
//!
//! Nothing stays stopped once its pacer is gone (docs/headroom.md, "Failing safe"). Each pacer has
//! a guard, a small process of its own (`kiln headroom-guard`) placed to outlive it, and tells it
//! everything it may stop before it stops any of it. The guard starts all of that again the moment
//! the pacer's end of its input closes, which happens however the pacer ends, or the moment the
//! pacer ends, by the kernel's word (`super::ends`). Each pacer's record on disk says the same, for a
//! pacer that starts, or runs, after both pacer and guard died.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use serde::{Deserialize, Serialize};

use super::ends;

/// The argument that makes kiln a pacer's guard ([`guard`]).
pub const GUARD_ARG: &str = "headroom-guard";
/// What the guard says once it runs: nothing is stopped before.
const GUARD_READY: &str = "ready";
/// The pacers' records, one each, in headroom's folder.
const RECORDS: &str = "paced";

/// Granular role of a paced process group for priority ordering (`Tool` / `Job` / `Escape` slowed
/// FIRST before touching any `Harness`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub enum GroupKind {
    /// An agent or subagent session's harness process.
    #[default]
    Harness,
    /// Spawned child tool/build processes (e.g. `cargo`, `rustc`) or agent `reach` command.
    Tool,
    /// A CI job's cgroup.
    Job,
    /// An escaped transient user systemd unit.
    Escape,
}

/// A group of processes headroom can see and slow.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Group {
    /// Where it is (a cgroup's folder, `session:<id>`, or `tools:<id>`).
    pub dir: PathBuf,
    /// What it is: an agent session's id, or a unit's name.
    pub label: String,
    /// The agent session it is, when it's one.
    pub session: Option<String>,
    /// What tier of work this group holds.
    pub kind: GroupKind,
}

/// How the machine paces work: `"cgroup"` (Linux), `"signals"` (macOS), `"threads"` (Windows), or
/// `"none"`.
pub fn how() -> &'static str {
    match imp::available() {
        false => "none",
        true if cfg!(target_os = "linux") => "cgroup",
        true if cfg!(windows) => "threads",
        true => "signals",
    }
}

/// The groups there are now.
pub fn groups(escapes: bool) -> Vec<Group> {
    imp::groups(escapes)
}

/// `dir` is frozen for something else while `held` (a CI job paused): the pacer leaves it be.
pub fn held_elsewhere(dir: &Path, held: bool) {
    #[cfg(target_os = "linux")]
    imp::held_elsewhere(dir, held);
    #[cfg(not(target_os = "linux"))]
    let _ = (dir, held);
}

/// How hard a group works.
#[derive(Debug, Clone, Copy, Default)]
pub struct Load {
    /// Percent of one CPU.
    pub cpu_pct: f64,
    /// Bytes written a second.
    pub write_rate: f64,
    /// Bytes of memory it holds now.
    pub mem_bytes: f64,
}

/// Turns counters into rates, between one look and the next.
#[derive(Default)]
pub struct Meter {
    prev: HashMap<PathBuf, (Instant, u64, u64)>,
}

impl Meter {
    pub fn load(&mut self, groups: &[Group]) -> Vec<Load> {
        let now = Instant::now();
        let out = groups
            .iter()
            .map(|g| {
                let (cpu, written, mem) = imp::counters(&g.dir);
                let load = match self.prev.get(&g.dir) {
                    Some((at, c0, w0)) => {
                        let secs = now.duration_since(*at).as_secs_f64().max(f64::MIN_POSITIVE);
                        Load { cpu_pct: cpu.saturating_sub(*c0) as f64 / 1e4 / secs, write_rate: written.saturating_sub(*w0) as f64 / secs, mem_bytes: mem as f64 }
                    }
                    None => Load { mem_bytes: mem as f64, ..Load::default() },
                };
                self.prev.insert(g.dir.clone(), (now, cpu, written));
                load
            })
            .collect();
        self.prev.retain(|d, _| groups.iter().any(|g| &g.dir == d));
        out
    }
}

/// How finely the machine's idle CPU is counted: the clock tick its counters advance by. Unix:
/// `sysconf(_SC_CLK_TCK)`, the unit of `/proc/stat` on Linux and of `host_processor_info` on macOS
/// (100 a second on both). Windows: its default clock interval, 15.625 ms (64 a second: Microsoft,
/// "Timers, Timer Resolution, and Development of Efficient Code", 2010), the processor times' unit.
fn cpu_tick() -> Duration {
    #[cfg(unix)]
    let tick = Duration::from_nanos(1_000_000_000 / rustix::param::clock_ticks_per_second().max(1));
    #[cfg(not(unix))]
    let tick = Duration::from_micros(15_625);
    tick
}

/// The least span the machine and its groups are measured over (LB05, G11: replaced `MIN_PERIOD`,
/// a chosen 1 s): a span reads each CPU's idle time to within a tick ([`cpu_tick`]), so over
/// [`kiln_ci::headroom::pace::SHARE_STEPS`] ticks the error over all of them is within a pacer's
/// step of the machine's cores, the resolution the engine judges the CPU short by. 1 s where the
/// clock ticks 100 times a second.
pub fn measured_over() -> Duration {
    cpu_tick().mul_f64(kiln_ci::headroom::pace::SHARE_STEPS)
}

/// The engine's share (1: full speed) as the whole percent the pacer holds a group to; above 0 it is
/// at least 1, never a stop.
pub fn step_of(share: f64) -> u8 {
    let steps = (share * kiln_ci::headroom::pace::SHARE_STEPS).floor();
    if share > 0.0 { steps.clamp(1.0, 99.0) as u8 } else { 0 }
}

/// How long a group has been held stopped in all, for the share of a stretch it was let run.
#[derive(Default)]
struct Stopped {
    total: Duration,
    since: Option<Instant>,
}

impl Stopped {
    fn hold(&mut self, now: Instant) {
        self.since.get_or_insert(now);
    }

    fn release(&mut self, now: Instant) {
        if let Some(s) = self.since.take() {
            self.total += now.saturating_duration_since(s);
        }
    }

    fn total(&self, now: Instant) -> Duration {
        self.total + self.since.map_or(Duration::ZERO, |s| now.saturating_duration_since(s))
    }
}

/// What the pacer's thread waits on: told of new shares, of its guard, or to end, and woken for
/// the soonest switch of a group or a retry of its guard: nothing else (LB05, G11).
#[derive(Default)]
struct Wake {
    told: Mutex<bool>,
    told_cv: Condvar,
}

impl Wake {
    fn tell(&self) {
        *self.told.lock() = true;
        self.told_cv.notify_all();
    }

    /// Until told, or until `due` (none: until told).
    fn wait(&self, due: Option<Instant>) {
        let mut told = self.told.lock();
        if !*told {
            match due {
                Some(at) => {
                    self.told_cv.wait_until(&mut told, at);
                }
                None => self.told_cv.wait(&mut told),
            }
        }
        *told = false;
    }
}

/// The actuator: groups and their shares, applied on a thread of its own at each group's next
/// switch, a due time of that group's alone (`kiln_ci::headroom::pace::Duty`); a share is a rate
/// being enforced, so a clock must stay, and this is its due time, computed from the shares.
pub struct Pacer {
    shares: Arc<Mutex<HashMap<PathBuf, u8>>>,
    period: Arc<Mutex<Duration>>,
    stopped: Arc<Mutex<HashMap<PathBuf, Stopped>>>,
    /// Groups currently paced via smooth kernel CPU throttling (`cpu.max` on Linux, Job Object
    /// hard cap on Windows) rather than freeze/stop.
    soft_throttled: Arc<Mutex<HashSet<PathBuf>>>,
    stop: Arc<AtomicBool>,
    wake: Arc<Wake>,
    /// No guard runs, and the last couldn't start: told of every set, which tries again.
    unguarded: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// How a pacer's guard starts, given the record it guards: kiln's own [`kiln_guard`] (the crate's
/// tests start one of their own).
pub type GuardCmd = fn(&Path) -> std::io::Result<Command>;

/// `kiln headroom-guard <record>`, from this very binary.
pub fn kiln_guard(record: &Path) -> std::io::Result<Command> {
    let mut c = Command::new(std::env::current_exe()?);
    c.arg(GUARD_ARG).arg(record);
    Ok(c)
}

impl Pacer {
    /// Starts what pacers before this one left stopped (their records say what), then the thread,
    /// guarded by kiln's own guard.
    pub fn start(dir: &Path) -> Self {
        Self::start_with(dir, kiln_guard)
    }

    /// The same, its guard started by `guard`.
    pub fn start_with(dir: &Path, guard: GuardCmd) -> Self {
        release_left(dir);
        let shares: Arc<Mutex<HashMap<PathBuf, u8>>> = Arc::default();
        let period = Arc::new(Mutex::new(Duration::ZERO));
        let stopped: Arc<Mutex<HashMap<PathBuf, Stopped>>> = Arc::default();
        let soft_throttled: Arc<Mutex<HashSet<PathBuf>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(Wake::default());
        let unguarded = Arc::new(AtomicBool::new(false));
        let (sh, pe, sp, sf, st, wa, ug, dir) = (Arc::clone(&shares), Arc::clone(&period), Arc::clone(&stopped), Arc::clone(&soft_throttled), Arc::clone(&stop), Arc::clone(&wake), Arc::clone(&unguarded), dir.to_owned());
        let spawned = std::thread::Builder::new().name("kiln-pacer".into()).spawn(move || match Proc::me() {
            Some(me) => {
                let records = dir.join(RECORDS);
                let record = records.join(format!("{}-{}.json", me.pid, me.start));
                // Records of pacers that end with their guards while this one runs (a runner it took
                // over from, killed with its guard): by event, never a look on a clock.
                let watch = watch_records(records);
                run(&Shared { shares: &sh, period: &pe, stopped: &sp, soft_throttled: &sf, stop: &st, wake: &wa, unguarded: &ug }, &Paced { me, record, how: guard });
                drop(watch);
            }
            None => tracing::warn!("headroom: this process's own start is unknown, so nothing is paced"),
        });
        let thread = spawned.map_err(|e| tracing::warn!(error = %e, "headroom: the pacer couldn't start; work past the window still waits to start")).ok();
        Self { shares, period, stopped, soft_throttled, stop, wake, unguarded, thread }
    }

    /// How long each paced group has been held stopped in all, as of now.
    pub fn stopped(&self) -> HashMap<PathBuf, Duration> {
        let now = Instant::now();
        self.stopped.lock().iter().map(|(d, s)| (d.clone(), s.total(now))).collect()
    }

    /// Whether `dir` is currently slowed smoothly by kernel CPU throttling (`cpu.max` / Windows
    /// Job Object hard cap) rather than stopped/frozen.
    pub fn is_soft_throttled(&self, dir: &Path) -> bool {
        self.soft_throttled.lock().contains(dir)
    }

    /// Each paced group's share from now (percent; groups at 100 aren't listed). Only a change is
    /// told to the pacer's thread (the same shares don't wake it), or any shares while no guard
    /// could start: each is a try again, the engine's pace and not a clock's.
    pub fn set(&self, paced: HashMap<PathBuf, u8>, period: Duration) {
        let mut sh = self.shares.lock();
        let mut pe = self.period.lock();
        let changed = *sh != paced || *pe != period || (!paced.is_empty() && self.unguarded.load(Ordering::Acquire));
        *pe = period;
        *sh = paced;
        drop(pe);
        drop(sh);
        if changed {
            self.wake.tell();
        }
    }
}

impl Drop for Pacer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.wake.tell();
        // The thread starts what it stopped and lets its guard go before it ends.
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A process, by its id and when it started: an id alone may be another process's by now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Proc {
    pid: u32,
    /// Seconds since the epoch, as the system has it.
    start: u64,
}

impl Proc {
    fn me() -> Option<Self> {
        Self::of(std::process::id())
    }

    fn of(pid: u32) -> Option<Self> {
        started(pid).map(|start| Self { pid, start })
    }

    /// Whether it still runs (its id is still its own).
    fn alive(&self) -> bool {
        started(self.pid) == Some(self.start)
    }
}

/// When process `pid` started, while it runs.
fn started(pid: u32) -> Option<u64> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let pid = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, ProcessRefreshKind::nothing());
    sys.process(pid).map(sysinfo::Process::start_time)
}

/// A pacer's record: whose it is, who guards it, and everything it may have stopped.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Record {
    owner: Option<Proc>,
    guard: Option<Proc>,
    stopped: Vec<String>,
}

/// What a pacer shares with its thread.
#[derive(Clone, Copy)]
struct Shared<'a> {
    shares: &'a Mutex<HashMap<PathBuf, u8>>,
    period: &'a Mutex<Duration>,
    stopped: &'a Mutex<HashMap<PathBuf, Stopped>>,
    soft_throttled: &'a Mutex<HashSet<PathBuf>>,
    stop: &'a AtomicBool,
    wake: &'a Arc<Wake>,
    unguarded: &'a AtomicBool,
}

/// What a pacer's thread works with.
struct Paced {
    me: Proc,
    record: PathBuf,
    how: GuardCmd,
}

/// Maximum pacing cycle (µs) applied when lag spikes: bounds single-cycle freeze duration so
/// a laptop sleep/resume pause or slow disk write in `tell()` can never turn `lag * 100` into a
/// multi-minute or multi-hour freeze of all jobs and sessions on the runner.
const MAX_CYCLE_US: u64 = 2_000_000;
/// Maximum plausible single actuator-switch lag (µs): delays larger than this (or longer than
/// the cycle itself) reflect OS sleep/resume, VM pause, or disk I/O in `tell()`, not scheduler
/// switch jitter, and are rejected from the RTO lag estimator.
const MAX_LAG_SAMPLE_US: f64 = 50_000.0;

pub(crate) fn bounded_cycle_us(asked_us: u64, lag_us: Option<f64>) -> u64 {
    kiln_ci::headroom::pace::cycle_us(asked_us, lag_us).min(asked_us.max(MAX_CYCLE_US))
}

pub(crate) fn bounded_lag_sample(late_us: u64, cycle_us: u64) -> Option<f64> {
    let late = late_us as f64;
    if !late.is_finite() || late > (cycle_us.max(MAX_CYCLE_US) as f64) {
        return None;
    }
    Some(late.min(MAX_LAG_SAMPLE_US))
}

/// The pacer's thread: each paced group stopped for its share of every cycle, at that group's own
/// due times, and only while a guard runs that will start it again. Between passes nothing runs:
/// it's told of a change or woken for the next due time (LB05, G11; it was a look every 50 ms).
fn run(sh: &Shared<'_>, p: &Paced) {
    let Shared { shares, period, stopped, soft_throttled, stop, wake, unguarded } = *sh;
    let base = Instant::now();
    let now_us = || u64::try_from(base.elapsed().as_micros()).unwrap_or(u64::MAX);
    let mut duty = kiln_ci::headroom::pace::Duty::default();
    // How long a switch takes to take effect, from when it was due (RFC 6298's estimator, without
    // its floor): timer granularity and slack, the scheduler, the actuator's own work.
    let mut lag = kiln_ci::protect::rto::Rto::default();
    // When each group's next switch was due, for how late it came.
    let mut due_at: HashMap<PathBuf, u64> = HashMap::new();
    // The groups stopped now, each with what starts it again (`imp::release`).
    let mut held: HashMap<PathBuf, Vec<String>> = HashMap::new();
    // Groups smoothly slowed by kernel CPU throttling (`cpu.max` on Linux, Job Object hard cap on
    // Windows) rather than frozen: (share_pct, guard_release_key).
    let mut throttled: HashMap<PathBuf, (u8, String)> = HashMap::new();
    // What the guard and the record list, group by group: all a paced group has had stopped or
    // throttled.
    let mut listed: HashMap<PathBuf, BTreeSet<String>> = HashMap::new();
    let mut guard: Option<Guard> = None;
    // Guards that ended, kept until they're reaped.
    let mut gone: Vec<std::process::Child> = Vec::new();
    // A scope of its own where one outlives the runner's unit; none once that fails.
    let mut scope = imp::guard_in_scope();
    // A guard that can't start is tried again when told of shares (`Pacer::set`), not on a clock.
    let mut warned = false;
    while !stop.load(Ordering::Relaxed) {
        let asked = u64::try_from(period.lock().as_micros()).unwrap_or(u64::MAX);
        let cycle = bounded_cycle_us(asked, lag.estimate_us(0.0));
        let want: Vec<(PathBuf, u8)> = shares.lock().iter().map(|(d, s)| (d.clone(), *s)).collect();
        let now = now_us();
        gone.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
        // A guard that ended guards nothing: what it held runs again before anything stops anew.
        if guard.as_ref().is_some_and(Guard::ended) {
            let was_ready = guard.as_ref().is_some_and(Guard::ready);
            if was_ready {
                tracing::warn!("headroom: the pacer's guard ended; what it held runs again");
            } else {
                // It never ran: started in a scope that couldn't be made, it starts without one.
                scope = false;
            }
            gone.extend(guard.take().map(|g| g.child));
            release_all(&mut held, &mut throttled, &mut listed, &mut guard, p);
        }
        if guard.is_none() && !want.is_empty() {
            let tried = match Guard::start(p, scope, wake) {
                Err(_) if scope => {
                    scope = false;
                    Guard::start(p, false, wake)
                }
                r => r,
            };
            match tried {
                Ok(g) => {
                    guard = Some(g);
                    unguarded.store(false, Ordering::Release);
                    warned = false;
                }
                Err(e) => {
                    unguarded.store(true, Ordering::Release);
                    if !warned {
                        warned = true;
                        tracing::warn!(error = %e, "headroom: the pacer's guard couldn't start, so nothing is paced");
                    }
                }
            }
        }
        let guarded = guard.as_ref().is_some_and(Guard::ready);
        let mut unheard = false;
        let mut soonest: Option<u64> = None;
        for (dir, share) in &want {
            // Prefer smooth kernel CPU throttling (`cpu.max` on Linux, Job Object hard cap on
            // Windows) whenever `share > 0` instead of stopping/freezing the processes.
            if guarded && *share > 0 && *share < 100 {
                if throttled.get(dir).is_some_and(|(s, _)| *s == *share) {
                    continue;
                }
                if let Some(key) = imp::soft_throttle_key(dir, *share, throttled.get(dir).map(|(_, k)| k.as_str())) {
                    let mine = listed.entry(dir.clone()).or_default();
                    if !mine.contains(&key) {
                        if let Some((_, old)) = throttled.get(dir) { mine.remove(old); }
                        mine.insert(key.clone());
                        if !tell(&mut guard, &listed, p) {
                            unheard = true;
                            break;
                        }
                    }
                    if imp::apply_soft_throttle(dir, *share, &key) {
                        if let Some(keys) = held.remove(dir) {
                            keys.iter().for_each(|k| imp::release(k));
                        }
                        throttled.insert(dir.clone(), (*share, key));
                        continue;
                    }
                }
            }
            if let Some((_, key)) = throttled.remove(dir) {
                imp::release(&key);
            }
            // On macOS (signal-based child duty cycling), keep non-zero soft cycles short (<= 500 ms)
            // so throttled builds pause smoothly in sub-half-second slices rather than multi-second stalls.
            let group_cycle = if cfg!(target_os = "macos") && *share > 0 { cycle.min(500_000) } else { cycle };
            let phase = duty.phase(&dir.to_string_lossy(), *share, now, group_cycle);
            soonest = match (soonest, phase.next_us) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            if phase.runs {
                if let Some(keys) = held.remove(dir) {
                    keys.iter().for_each(|k| imp::release(k));
                    if let Some(due) = due_at.get(dir)
                        && let Some(sample) = bounded_lag_sample(now_us().saturating_sub(*due), group_cycle)
                    {
                        lag.sample(sample);
                    }
                }
            } else if guarded && !held.contains_key(dir) {
                let keys = imp::keys(dir);
                if keys.is_empty() {
                    if let Some(next) = phase.next_us {
                        due_at.insert(dir.clone(), next);
                    }
                    continue;
                }
                // Told first: the guard and the record list it all before any of it stops.
                let mine = listed.entry(dir.clone()).or_default();
                let mut wrote_record = false;
                if !keys.iter().all(|k| mine.contains(k)) {
                    mine.extend(keys.iter().cloned());
                    wrote_record = true;
                    if !tell(&mut guard, &listed, p) {
                        unheard = true;
                        break;
                    }
                }
                held.insert(dir.clone(), imp::stop_keys(&keys));
                // Do not feed synchronous disk-write latency from `tell()` into the actuator
                // switch lag estimator: under disk load that turns `cycle = lag * 100` into a
                // multi-minute freeze.
                if !wrote_record
                    && let Some(due) = due_at.get(dir)
                    && let Some(sample) = bounded_lag_sample(now_us().saturating_sub(*due), group_cycle)
                {
                    lag.sample(sample);
                }
            }
            if let Some(next) = phase.next_us {
                due_at.insert(dir.clone(), next);
            }
        }
        if unheard {
            // A guard that didn't hear is let go: what it held runs again, and nothing stops until
            // a guard runs that hears.
            if let Some(mut g) = guard.take() {
                let _ = g.child.kill();
                let _ = g.child.wait();
            }
            release_all(&mut held, &mut throttled, &mut listed, &mut guard, p);
        }
        // Groups no longer paced run again and come off the list.
        held.retain(|dir, keys| {
            let keep = want.iter().any(|(d, _)| d == dir);
            if !keep {
                keys.iter().for_each(|k| imp::release(k));
            }
            keep
        });
        throttled.retain(|dir, (_, key)| {
            let keep = want.iter().any(|(d, _)| d == dir);
            if !keep {
                imp::release(key);
            }
            keep
        });
        *soft_throttled.lock() = throttled.keys().cloned().collect();
        let before = listed.len();
        listed.retain(|dir, _| want.iter().any(|(d, _)| d == dir));
        if listed.len() != before {
            tell(&mut guard, &listed, p);
        }
        let ids: Vec<String> = want.iter().map(|(d, _)| d.to_string_lossy().into_owned()).collect();
        duty.keep(ids.iter().map(String::as_str));
        due_at.retain(|d, _| want.iter().any(|(w, _)| w == d));
        let at = Instant::now();
        let mut clocks = stopped.lock();
        for dir in held.keys() {
            clocks.entry(dir.clone()).or_default().hold(at);
        }
        clocks.retain(|dir, s| {
            if !held.contains_key(dir) {
                s.release(at);
            }
            held.contains_key(dir) || want.iter().any(|(d, _)| d == dir)
        });
        drop(clocks);
        // Woken for the soonest due time, a group's next switch, or when told.
        wake.wait(soonest.map(|us| base + Duration::from_micros(us)));
    }
    // Ending: everything runs again, the guard hears nothing is left, and it goes.
    release_all(&mut held, &mut throttled, &mut listed, &mut guard, p);
    soft_throttled.lock().clear();
    if let Some(mut g) = guard {
        g.finish();
    }
    for mut c in gone {
        let _ = c.wait();
    }
}

/// Everything held or throttled runs at full speed again and nothing is listed any more.
fn release_all(held: &mut HashMap<PathBuf, Vec<String>>, throttled: &mut HashMap<PathBuf, (u8, String)>, listed: &mut HashMap<PathBuf, BTreeSet<String>>, guard: &mut Option<Guard>, p: &Paced) {
    for keys in std::mem::take(held).into_values() {
        keys.iter().for_each(|k| imp::release(k));
    }
    for (_, key) in std::mem::take(throttled).into_values() {
        imp::release(&key);
    }
    listed.clear();
    tell(guard, listed, p);
}

/// Tells the guard, then the record, all that's listed now; whether the guard heard it.
fn tell(guard: &mut Option<Guard>, listed: &HashMap<PathBuf, BTreeSet<String>>, p: &Paced) -> bool {
    let all: BTreeSet<&String> = listed.values().flatten().collect();
    let heard = guard.as_mut().is_some_and(|g| g.tell(&all));
    if all.is_empty() {
        let _ = std::fs::remove_file(&p.record);
    } else {
        let rec = Record { owner: Some(p.me), guard: guard.as_ref().and_then(|g| g.proc), stopped: all.into_iter().cloned().collect() };
        let Ok(data) = serde_json::to_vec(&rec) else { return false };
        let saved = if rec.stopped.iter().any(|key| key.starts_with("cpumax2:")) {
            crate::util::write_secret(&p.record, &data, 0o600).and_then(|()| p.record.parent().map_or(Ok(()), crate::blobs::chunks::sync_dir))
        } else {
            crate::util::write_secret_unsynced(&p.record, &data, 0o644)
        };
        if let Err(e) = saved {
            tracing::warn!(error = %e, "headroom: couldn't record recovery before pacing");
            return false;
        }
    }
    heard
}

/// A pacer's guard, as its pacer holds it.
struct Guard {
    child: std::process::Child,
    /// Its input: this pacer's to write to and, by ending, to close.
    input: Option<std::process::ChildStdin>,
    ready: Arc<AtomicBool>,
    /// Its output ended (`read`): the guard ended, by event (LB05, G11; it was asked every tick).
    ended: Arc<AtomicBool>,
    proc: Option<Proc>,
}

impl Guard {
    /// Starts it (in a scope of its own when `scope`) apart from this process's group; `wake` is
    /// told when it says it runs and when its output ends.
    fn start(p: &Paced, scope: bool, wake: &Arc<Wake>) -> std::io::Result<Self> {
        let inner = (p.how)(&p.record)?;
        #[cfg(target_os = "linux")]
        let mut cmd = if scope { imp::in_scope(&inner, &p.record) } else { inner };
        #[cfg(not(target_os = "linux"))]
        let mut cmd = {
            let _ = scope;
            inner
        };
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = spawn_apart(&mut cmd)?;
        let (input, out) = (child.stdin.take(), child.stdout.take());
        let ready = Arc::new(AtomicBool::new(false));
        let ended = Arc::new(AtomicBool::new(false));
        let (heard, over, told) = (Arc::clone(&ready), Arc::clone(&ended), Arc::clone(wake));
        // Its word that it runs, then whatever else it says, read to the end so it never blocks;
        // the end of its output is its end, and the pacer's told.
        let read = out.ok_or_else(|| std::io::Error::other("the guard's output wasn't kept")).and_then(|out| {
            std::thread::Builder::new().name("kiln-pacer-guard".into()).spawn(move || {
                use std::io::BufRead as _;
                for line in std::io::BufReader::new(out).lines() {
                    match line {
                        Ok(l) if l.trim() == GUARD_READY => {
                            heard.store(true, Ordering::Release);
                            told.tell();
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                over.store(true, Ordering::Release);
                told.tell();
            })
        });
        if let Err(e) = read {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        let proc = Proc::of(child.id());
        Ok(Self { child, input, ready, ended, proc })
    }

    fn ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Whether it ended: its output closed, the kernel's word, not a look.
    fn ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }

    /// One line: all that's listed now (the guard keeps the last whole line it gets).
    fn tell(&mut self, all: &BTreeSet<&String>) -> bool {
        use std::io::Write as _;
        let Some(input) = self.input.as_mut() else { return false };
        let mut line = serde_json::to_vec(all).unwrap_or_default();
        line.push(b'\n');
        input.write_all(&line).and_then(|()| input.flush()).is_ok()
    }

    /// Its input closed, it ends: waited for.
    fn finish(&mut self) {
        drop(self.input.take());
        let _ = self.child.wait();
    }
}

/// Starts `cmd` apart from this process: in a process group of its own (what ends this one's,
/// a terminal's or launchd's, doesn't end it), and on Windows out of this process's job where the
/// job lets it.
fn spawn_apart(cmd: &mut Command) -> std::io::Result<std::process::Child> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0).spawn()
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const ERROR_ACCESS_DENIED: i32 = 5;
        match cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB | CREATE_NO_WINDOW).spawn() {
            // A job that keeps its processes' children: the guard stays in it.
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW).spawn(),
            r => r,
        }
    }
}

/// A pacer's guard (`kiln headroom-guard <record>`): says it runs, keeps the last whole line its
/// pacer sends (everything the pacer may stop, sent before any of it stops), and once the pacer's
/// end of its input closes, or the pacer ends with its input's end still open elsewhere (a process
/// that came to hold it too), however the pacer ended, starts all of it again and removes the
/// record. The pacer is the one its record is named for (`<pid>-<start>`).
pub fn guard(record: &Path) -> std::process::ExitCode {
    use std::io::Write as _;
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{GUARD_READY}").and_then(|()| out.flush());
    let (tx, rx) = std::sync::mpsc::channel();
    let mut last: Vec<String> = Vec::new();
    // Its pacer's end, as the kernel says it (LB05, G11: replaced a look every 2 s). Where it can't
    // say (a kernel without pidfds, a platform with no event for it), the input's end alone does.
    if let Some(pacer) = pacer_of(record) {
        let to = tx.clone();
        let _ = std::thread::Builder::new().name("kiln-guard-pacer".into()).spawn(move || {
            if ends::await_end(pacer.pid, || pacer.alive()) {
                let _ = to.send(Word::Ended);
            }
        });
    }
    match std::thread::Builder::new().name("kiln-guard-input".into()).spawn(move || read_lists(&tx)) {
        // Each list as it comes, until its input or its pacer ends.
        Ok(_) => {
            while let Ok(Word::List(l)) = rx.recv() {
                last = l;
            }
        }
        // No thread to read with: its input alone says when.
        Err(_) => {
            let (tx, rx) = std::sync::mpsc::channel();
            read_lists(&tx);
            last = rx.try_iter().filter_map(|w| if let Word::List(l) = w { Some(l) } else { None }).last().unwrap_or_default();
        }
    }
    // The pacer can die immediately after persisting a new quota transaction, before its
    // last complete pipe message is consumed. Verify that the record still names this guard.
    let durable = std::fs::read(record).ok().and_then(|raw| serde_json::from_slice::<Record>(&raw).ok());
    if let Some(rec) = durable {
        if rec.guard.is_some() && rec.guard != Proc::me() { return std::process::ExitCode::SUCCESS; }
        last.extend(rec.stopped.into_iter().filter(|k| k.starts_with("cpumax2:")));
    }
    last.iter().for_each(|k| imp::release(k));
    let _ = std::fs::remove_file(record);
    std::process::ExitCode::SUCCESS
}

/// What the guard's told, by its two sources: a list, or that its pacer (or its input) ended.
enum Word {
    List(Vec<String>),
    Ended,
}

/// Each whole line of the guard's input, as the list it says; then, once the input ends, that it
/// ended.
fn read_lists(tx: &std::sync::mpsc::Sender<Word>) {
    use std::io::BufRead as _;
    let mut input = std::io::stdin().lock();
    let mut line = String::new();
    loop {
        line.clear();
        match input.read_line(&mut line) {
            Ok(n) if n > 0 && line.ends_with('\n') => {
                if let Ok(l) = serde_json::from_str(line.trim_end())
                    && tx.send(Word::List(l)).is_err()
                {
                    return;
                }
            }
            // The end, or a line cut short: one the pacer ended telling, nothing on it stopped yet.
            _ => {
                let _ = tx.send(Word::Ended);
                return;
            }
        }
    }
}

/// The pacer a record is named for (`<pid>-<start>.json`).
fn pacer_of(record: &Path) -> Option<Proc> {
    let (pid, start) = record.file_stem()?.to_str()?.split_once('-')?;
    Some(Proc { pid: pid.parse().ok()?, start: start.parse().ok()? })
}

/// Starts again what pacers before this one left stopped: the old shared record's list, and each
/// pacer's record whose pacer and guard have both ended.
fn release_left(dir: &Path) {
    let old = dir.join("paced.json");
    if let Ok(raw) = std::fs::read(&old) {
        let keys: Vec<String> = serde_json::from_slice(&raw).unwrap_or_default();
        keys.iter().for_each(|k| imp::release(k));
        if !keys.is_empty() {
            tracing::info!(stopped = keys.len(), "headroom: work a previous runner left paced runs again");
        }
        let _ = std::fs::remove_file(&old);
    }
    release_ended(&dir.join(RECORDS));
}

/// Starts again what each pacer whose pacer and guard have both ended left stopped, and removes its
/// record (a guard still running does it itself; a running pacer's is its own).
fn release_ended(records: &Path) {
    let Ok(rd) = std::fs::read_dir(records) else { return };
    for e in rd.flatten() {
        let path = e.path();
        if path.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let Some(rec) = std::fs::read(&path).ok().and_then(|raw| serde_json::from_slice::<Record>(&raw).ok()) else { continue };
        if rec.owner.is_some_and(|o| o.alive()) || rec.guard.is_some_and(|g| g.alive()) {
            continue;
        }
        rec.stopped.iter().for_each(|k| imp::release(k));
        if !rec.stopped.is_empty() {
            tracing::info!(stopped = rec.stopped.len(), "headroom: work a pacer that ended left stopped runs again");
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// Watches the pacers' records: runs [`release_ended`] each time a record appears or a running
/// pacer or guard of one ends, the kernel's word (`ends::Watch`); nothing is looked at between.
/// Dropping what it returns ends it. None where this platform can't say (`release_ended` then
/// runs only when this pacer starts, the only event it has).
fn watch_records(records: PathBuf) -> Option<std::io::PipeWriter> {
    let _ = std::fs::create_dir_all(&records);
    let (stop, go) = std::io::pipe().ok()?;
    let spawned = std::thread::Builder::new().name("kiln-pacer-records".into()).spawn(move || {
        let Ok(mut watch) = ends::Watch::new(&records, stop) else { return };
        loop {
            // Each running pacer's and guard's end is armed before the records are read, so one
            // that ends after is the event this waits for.
            let running = running_of(&records);
            watch.keep(&running);
            for pid in &running {
                watch.process(*pid);
            }
            release_ended(&records);
            if !watch.wait() {
                return;
            }
        }
    });
    spawned.ok().map(|_| go)
}

/// The processes the records of `records` hold running: each record's pacer and guard still running.
fn running_of(records: &Path) -> HashSet<u32> {
    let Ok(rd) = std::fs::read_dir(records) else { return HashSet::new() };
    rd.flatten()
        .filter_map(|e| serde_json::from_slice::<Record>(&std::fs::read(e.path()).ok()?).ok())
        .flat_map(|r| [r.owner, r.guard])
        .flatten()
        .filter(Proc::alive)
        .map(|p| p.pid)
        .collect()
}

#[cfg(target_os = "linux")]
mod imp {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::Group;

    const ROOT: &str = "/sys/fs/cgroup";

    pub fn available() -> bool {
        Path::new(ROOT).join("cgroup.controllers").is_file()
    }

    /// A pacer's guard's own scope (`kiln-headroom-guard-<pid>-<start>.scope`): never paced.
    const GUARD_UNIT: &str = "kiln-headroom-guard-";

    /// Whether this process paces the user's own units: the machine's installed runner (the one its
    /// supervisor started; a test's runner or one started by hand paces only its own sessions)
    /// holding `$XDG_RUNTIME_DIR/kiln-headroom-escapes.lock`, so two runners on a machine never pace
    /// the same units. Tried again each time until held, so a runner that took over from another
    /// (blue/green) takes it once that one is gone.
    fn escapes_ours() -> bool {
        static HELD: parking_lot::Mutex<Option<fs::File>> = parking_lot::Mutex::new(None);
        static TOLD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !crate::os::supervise::started_by_supervisor() {
            return false;
        }
        let mut held = HELD.lock();
        if held.is_none() {
            let dir = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(|| PathBuf::from(format!("/run/user/{}", nix::unistd::getuid())), PathBuf::from);
            *held = take_lock(&dir.join("kiln-headroom-escapes.lock"));
            if held.is_none() && !TOLD.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::info!("headroom: another kiln on this machine paces the user's units; this one paces its own sessions until it can");
            }
        }
        held.is_some()
    }

    /// The lock at `path`, when no one else holds it.
    pub(super) fn take_lock(path: &Path) -> Option<fs::File> {
        let f = fs::OpenOptions::new().create(true).truncate(false).write(true).open(path).ok()?;
        f.try_lock().ok().map(|()| f)
    }

    fn own() -> Option<String> {
        fs::read_to_string("/proc/self/cgroup").ok()?.lines().find_map(|l| l.strip_prefix("0::")).map(|p| p.trim().to_owned())
    }

    /// Whether a pacer's guard starts in a scope of its own: where the user's service manager runs
    /// this runner as its unit, which a restart stops with every process left in it.
    pub fn guard_in_scope() -> bool {
        crate::os::supervise::started_by_supervisor() && own().is_some_and(|o| o.contains("/user@"))
    }

    /// `inner` run in a scope of its own by the user's service manager, still this process's child
    /// (`systemd-run --scope` runs it itself), so its input is still this process's to close.
    pub fn in_scope(inner: &std::process::Command, record: &Path) -> std::process::Command {
        let name = record.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let mut c = std::process::Command::new("systemd-run");
        c.args(["--user", "--scope", "--quiet", "--collect"]).arg(format!("--unit={GUARD_UNIT}{name}")).arg("--").arg(inner.get_program()).args(inner.get_args());
        for (k, v) in inner.get_envs() {
            match v {
                Some(v) => c.env(k, v),
                None => c.env_remove(k),
            };
        }
        c
    }

    pub fn groups(escapes: bool) -> Vec<Group> {
        let mut out = Vec::new();
        if let Some(dir) = crate::os::contain::agents_dir()
            && let Ok(rd) = fs::read_dir(&dir)
        {
            for e in rd.flatten() {
                if e.path().is_dir() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let kind = if name.starts_with(crate::os::RUN_GROUP) { super::GroupKind::Tool } else { super::GroupKind::Harness };
                    out.push(Group { dir: e.path(), label: name.clone(), session: Some(name), kind });
                }
            }
        }
        // CI jobs' groups, a leg's microVM's among them: legs are paced as agents are
        // (docs/priority.md; goal 8, plan 01a1a59c2b29 step 5), one paused the pause's alone
        // ([`held_elsewhere`]).
        for dir in crate::os::contain::jobs_dirs() {
            for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
                if e.path().is_dir() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    out.push(Group { dir: e.path(), label: format!("job {name}"), session: None, kind: super::GroupKind::Job });
                }
            }
        }
        if escapes && escapes_ours() && let Some(own) = own() {
            // The user's service manager (`.../user@<uid>.service`), when the runner runs under one.
            if let Some(at) = own.find(".service/").filter(|_| own.contains("/user@")) {
                let manager = Path::new(ROOT).join(own[..at + ".service".len()].trim_start_matches('/'));
                let mine = Path::new(ROOT).join(own.trim_start_matches('/'));
                let transient = PathBuf::from(format!("/run/user/{}/systemd/transient", nix::unistd::getuid()));
                let mut look = vec![manager];
                let mut depth = 0;
                while depth < 3 && !look.is_empty() {
                    let mut next = Vec::new();
                    for dir in look {
                        let Ok(rd) = fs::read_dir(&dir) else { continue };
                        for e in rd.flatten() {
                            let p = e.path();
                            let name = e.file_name().to_string_lossy().into_owned();
                            if !p.is_dir() {
                                continue;
                            }
                            if name.ends_with(".slice") {
                                next.push(p);
                            } else if (name.ends_with(".service") || name.ends_with(".scope")) && !desktop(&name) && !name.starts_with(GUARD_UNIT) && transient.join(&name).exists() && !mine.starts_with(&p) {
                                out.push(Group { dir: p, label: name, session: None, kind: super::GroupKind::Escape });
                            }
                        }
                    }
                    look = next;
                    depth += 1;
                }
            }
        }
        out
    }

    /// A unit the desktop started for a person (an app, a terminal window, D-Bus activation): never
    /// paced, whatever runs in it (systemd's names for them: `app-…`, `dbus-…`, `xdg-…`).
    pub fn desktop(name: &str) -> bool {
        ["app-", "dbus-", "xdg-", "flatpak-", "snap."].iter().any(|p| name.starts_with(p))
    }

    /// CPU time (µs), bytes written and memory held, by the group and everything under it.
    pub fn counters(dir: &Path) -> (u64, u64, u64) {
        let cpu = fs::read_to_string(dir.join("cpu.stat")).ok().and_then(|t| t.lines().find_map(|l| l.strip_prefix("usage_usec ")).and_then(|v| v.trim().parse().ok())).unwrap_or(0);
        // The kernel's own count, where the group has the io controller; else its processes' own.
        // The kernel's own counts where the group has the controllers; else its processes' own.
        let mut pids: Option<Vec<u32>> = None;
        let mut pids_of = |dir: &Path| {
            pids.get_or_insert_with(|| {
                let mut v = Vec::new();
                pids_under(dir, usize::MAX, &mut v);
                v
            })
            .clone()
        };
        let written = match fs::read_to_string(dir.join("io.stat")) {
            Ok(t) if !t.trim().is_empty() => t.split_whitespace().filter_map(|w| w.strip_prefix("wbytes=")).filter_map(|v| v.parse::<u64>().ok()).sum(),
            _ => pids_of(dir).iter().filter_map(|p| fs::read_to_string(format!("/proc/{p}/io")).ok()).filter_map(|t| t.lines().find_map(|l| l.strip_prefix("write_bytes:")).and_then(|v| v.trim().parse::<u64>().ok())).sum(),
        };
        let mem = match fs::read_to_string(dir.join("memory.current")).ok().and_then(|t| t.trim().parse::<u64>().ok()) {
            Some(m) => m,
            None => pids_of(dir)
                .iter()
                .filter_map(|p| fs::read_to_string(format!("/proc/{p}/status")).ok())
                .filter_map(|t| t.lines().find_map(|l| l.strip_prefix("VmRSS:")).and_then(|v| v.split_whitespace().next()).and_then(|v| v.parse::<u64>().ok()))
                .map(|kib| kib * 1024)
                .sum(),
        };
        (cpu, written, mem)
    }

    fn pids_under(dir: &Path, depth: usize, out: &mut Vec<u32>) {
        if let Ok(t) = fs::read_to_string(dir.join("cgroup.procs")) {
            out.extend(t.lines().filter_map(|l| l.trim().parse::<u32>().ok()));
        }
        if depth == 0 {
            return;
        }
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.flatten() {
                if e.path().is_dir() {
                    pids_under(&e.path(), depth - 1, out);
                }
            }
        }
    }

    fn freeze(dir: &Path, stop: bool) -> std::io::Result<()> {
        fs::write(dir.join("cgroup.freeze"), if stop { "1" } else { "0" })
    }

    /// Groups this process freezes for something else while it wants them so (a CI job paused,
    /// `crate::runner::freeze`): never stopped or let go on here, so a slice the pacer began
    /// can't thaw what was paused during it.
    static ELSEWHERE: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashSet<PathBuf>>> = std::sync::LazyLock::new(parking_lot::Mutex::default);

    pub fn held_elsewhere(dir: &Path, held: bool) {
        let mut e = ELSEWHERE.lock();
        if held {
            e.insert(dir.to_path_buf());
        } else {
            e.remove(dir);
        }
    }

    /// What stopping the group stops: the whole cgroup at once, by its folder; nothing if it's
    /// frozen already (someone else's freeze, left as it is and never thawed here), or held
    /// elsewhere.
    pub fn keys(dir: &Path) -> Vec<String> {
        if ELSEWHERE.lock().contains(dir) {
            return Vec::new();
        }
        match fs::read_to_string(dir.join("cgroup.freeze")) {
            Ok(v) if v.trim() == "0" => vec![dir.display().to_string()],
            _ => Vec::new(),
        }
    }

    /// Stops what `keys` name; says which it stopped.
    pub fn stop_keys(keys: &[String]) -> Vec<String> {
        keys.iter().filter(|k| freeze(Path::new(k), true).is_ok()).cloned().collect()
    }

    use super::cpu_quota::CpuLimit;

    pub fn soft_throttle_key(dir: &Path, share: u8, previous: Option<&str>) -> Option<String> {
        if ELSEWHERE.lock().contains(dir) { return None; }
        super::cpu_quota::key(dir, share, previous)
    }

    pub fn apply_soft_throttle(_dir: &Path, _share: u8, key: &str) -> bool {
        CpuLimit::decode(key).is_some_and(|limit| limit.change(false).is_ok())
    }

    pub fn release(key: &str) {
        if let Some(limit) = CpuLimit::decode(key) {
            if let Err(e) = limit.change(true) { tracing::warn!(error = %e, "headroom: restoring CPU quota"); }
            return;
        }
        if key.starts_with("cpumax:") || key.starts_with("cpumax2:") { return; }
        if !ELSEWHERE.lock().contains(Path::new(key)) {
            let _ = freeze(Path::new(key), false);
        }
    }
}

#[cfg(any(target_os = "linux", all(unix, test)))]
mod cpu_quota {
    use std::fs;
    use std::path::{Path, PathBuf};

    /// W05-R03. The independent guard and the restart record both receive the exact
    /// previous setting before a write. A reused path or a later external setting is left alone.
    #[derive(serde::Serialize, serde::Deserialize)]
    pub(super) struct CpuLimit {
        dir: PathBuf,
        device: u64,
        inode: u64,
        original: String,
        previous: String,
        intended: String,
    }

    impl CpuLimit {
        pub(super) fn decode(key: &str) -> Option<Self> { serde_json::from_str(key.strip_prefix("cpumax2:")?).ok() }

        fn matches(&self, meta: &fs::Metadata) -> bool {
            use std::os::unix::fs::MetadataExt as _;
            meta.dev() == self.device && meta.ino() == self.inode
        }

        pub(super) fn change(&self, restore: bool) -> std::io::Result<()> {
            use std::io::{Read as _, Seek as _, Write as _};
            let mut file = fs::OpenOptions::new().read(true).write(true).open(self.dir.join("cpu.max"))?;
            if !self.matches(&file.metadata()?) { return Err(std::io::Error::other("cgroup changed")); }
            let mut value = String::new();
            let bytes_read = file.read_to_string(&mut value)?;
            let value = value.trim();
            let replacement = if restore {
                if value != self.intended && value != self.previous { return Ok(()); }
                &self.original
            } else {
                if value != self.previous { return Err(std::io::Error::other("cgroup quota changed")); }
                &self.intended
            };
            file.rewind()?;
            file.write_all(replacement.as_bytes())?;
            // A kernfs attribute accepts one complete command and may report a synthetic file
            // size. Only a backing file with actual trailing bytes needs truncation.
            if bytes_read > replacement.len() && file.metadata()?.len() == bytes_read as u64 { file.set_len(replacement.len() as u64)?; }
            Ok(())
        }
    }

    pub(super) fn key(dir: &Path, share: u8, previous: Option<&str>) -> Option<String> {
        use std::os::unix::fs::MetadataExt as _;
        let path = dir.join("cpu.max");
        let meta = fs::metadata(&path).ok()?;
        let current = fs::read_to_string(&path).ok()?.trim().to_owned();
        let original = if let Some(prev) = previous.and_then(CpuLimit::decode) {
            if !prev.matches(&meta) || current != prev.intended { return None; }
            prev.original
        } else { current.clone() };
        let (quota, period) = original.split_once(' ')?;
        let period = period.parse::<u64>().ok().filter(|p| *p > 0)?;
        let cpus = fs::read_to_string(dir.join("cpuset.cpus.effective")).ok()
            .and_then(|s| crate::os::cpu_list(s.trim()).ok()).map(|c| c.len()).filter(|n| *n > 0)
            .or_else(|| std::thread::available_parallelism().ok().map(std::num::NonZero::get))?;
        let mut capacity = (cpus as u64).saturating_mul(period);
        if quota != "max" { capacity = capacity.min(quota.parse().ok()?); }
        for parent in dir.ancestors().skip(1) {
            let Ok(limit) = fs::read_to_string(parent.join("cpu.max")) else { break };
            let (q, p) = limit.trim().split_once(' ')?;
            if q != "max" {
                capacity = capacity.min(q.parse::<u64>().ok()?.saturating_mul(period).checked_div(p.parse().ok()?)?);
            }
        }
        // Linux CFS bandwidth requires a quota of at least 1 ms.
        let target = (capacity.saturating_mul(u64::from(share)) / 100).max(1000);
        let rec = CpuLimit { dir: dir.into(), device: meta.dev(), inode: meta.ino(), original, previous: current, intended: format!("{target} {period}") };
        Some(format!("cpumax2:{}", serde_json::to_string(&rec).ok()?))
    }

    #[cfg(test)]
    #[test]
    fn w05_r03_preserves_quota_period_and_external_changes() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let group = root.path().join("group");
        fs::create_dir(&group)?;
        fs::write(root.path().join("cpu.max"), "6000 100000")?;
        let file = group.join("cpu.max");
        fs::write(&file, "5000 50000")?;
        fs::write(group.join("cpuset.cpus.effective"), "0-3")?;
        let first = key(&group, 50, None).ok_or_else(|| anyhow::anyhow!("no first throttle"))?;
        let limit = CpuLimit::decode(&first).ok_or_else(|| anyhow::anyhow!("malformed quota record"))?;
        limit.change(false)?;
        assert_eq!(fs::read_to_string(&file)?, "1500 50000");
        let second = key(&group, 40, Some(&first)).and_then(|v| CpuLimit::decode(&v)).ok_or_else(|| anyhow::anyhow!("no second throttle"))?;
        second.change(false)?;
        second.change(true)?;
        assert_eq!(fs::read_to_string(&file)?, "5000 50000");
        limit.change(false)?;
        fs::write(&file, "4000 50000")?;
        limit.change(true)?;
        assert_eq!(fs::read_to_string(&file)?, "4000 50000");
        fs::rename(&file, group.join("old"))?;
        fs::write(&file, "1500 50000")?;
        assert!(limit.change(true).is_err());
        Ok(())
    }
}

/// What processes `pids` have done between them, as a cgroup's counters say it: CPU time (µs),
/// bytes written, memory held now. A process that ends takes its counts with it, so a group's next
/// rate starts from what's left (the meter never goes below zero): it reads low for that one look.
#[cfg(any(not(target_os = "linux"), test))]
fn processes(pids: &[u32]) -> (u64, u64, u64) {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    static SYSTEM: std::sync::OnceLock<Mutex<System>> = std::sync::OnceLock::new();
    if pids.is_empty() {
        return (0, 0, 0);
    }
    let pids: Vec<Pid> = pids.iter().map(|p| Pid::from_u32(*p)).collect();
    let mut sys = SYSTEM.get_or_init(|| Mutex::new(System::new())).lock();
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&pids), true, ProcessRefreshKind::nothing().with_cpu().with_disk_usage().with_memory().without_tasks());
    pids.iter().filter_map(|p| sys.process(*p)).fold((0, 0, 0), |(c, w, m), p| {
        (c + p.accumulated_cpu_time().saturating_mul(1000), w + p.disk_usage().total_written_bytes, m + p.memory())
    })
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::path::{Path, PathBuf};

    use super::Group;

    /// A session's harness group (`session:<id>`) or its spawned child tool processes (`tools:<id>`).
    const SESSION: &str = "session:";
    const TOOLS: &str = "tools:";

    /// Pacing by smooth throttling or stopping processes: macOS by child-first signal duty cycling,
    /// Windows by Job Object CPU rate caps (falling back to thread suspension only on freeze).
    pub fn available() -> bool {
        cfg!(any(target_os = "macos", windows))
    }

    /// Groups for each agent session / reach command held (`crate::os::contain`): when an agent
    /// session has spawned child processes (e.g. `cargo`, `rustc`), those children are emitted as
    /// a separate `tools:<session>` group (`GroupKind::Tool`) so headroom slows individual tool
    /// calls FIRST without touching the interactive harness process (`session:<session>`).
    pub fn groups(_escapes: bool) -> Vec<Group> {
        let mut out = Vec::new();
        for s in crate::os::contain::sessions() {
            if s.starts_with(crate::os::RUN_GROUP) {
                out.push(Group { dir: PathBuf::from(format!("{SESSION}{s}")), label: s.clone(), session: Some(s), kind: super::GroupKind::Tool });
                continue;
            }
            let hpid = super::super::harness_pid(&s);
            let all = crate::os::contain::Group::existing(&s).map(|g| g.pids()).unwrap_or_default();
            let has_children = hpid.is_some_and(|h| all.iter().any(|p| *p != h));
            if has_children {
                out.push(Group { dir: PathBuf::from(format!("{TOOLS}{s}")), label: format!("{s} (tools)"), session: Some(s.clone()), kind: super::GroupKind::Tool });
            }
            out.push(Group { dir: PathBuf::from(format!("{SESSION}{s}")), label: s.clone(), session: Some(s), kind: super::GroupKind::Harness });
        }
        out
    }

    /// The group's processes' counters, summed.
    pub fn counters(dir: &Path) -> (u64, u64, u64) {
        super::processes(&pids(dir))
    }

    fn pids(dir: &Path) -> Vec<u32> {
        let Some(d) = dir.to_str() else { return Vec::new() };
        if let Some(session) = d.strip_prefix(TOOLS) {
            let hpid = super::super::harness_pid(session);
            return crate::os::contain::Group::existing(session)
                .map(|g| g.pids().into_iter().filter(|p| Some(*p) != hpid).collect())
                .unwrap_or_default();
        }
        let Some(session) = d.strip_prefix(SESSION) else { return Vec::new() };
        let all = crate::os::contain::Group::existing(session).map(|g| g.pids()).unwrap_or_default();
        if !session.starts_with(crate::os::RUN_GROUP)
            && let Some(hpid) = super::super::harness_pid(session)
            && all.contains(&hpid)
            && all.len() > 1
        {
            return vec![hpid];
        }
        all
    }

    /// Smooth kernel CPU rate-cap key on Windows (Job Object `HARD_CAP`); none on macOS.
    #[cfg(windows)]
    pub fn soft_throttle_key(dir: &Path, _share: u8, _previous: Option<&str>) -> Option<String> {
        let d = dir.to_str()?;
        let session = d.strip_prefix(TOOLS).or_else(|| d.strip_prefix(SESSION))?;
        Some(format!("winjob:{session}"))
    }

    #[cfg(not(windows))]
    pub fn soft_throttle_key(_dir: &Path, _share: u8, _previous: Option<&str>) -> Option<String> {
        None
    }

    /// Applies Windows Job Object `JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP` so demanding builds slow
    /// smoothly in kernel space without suspending any thread (`SuspendThread`).
    #[cfg(windows)]
    pub fn apply_soft_throttle(dir: &Path, share_pct: u8, _key: &str) -> bool {
        let Some(d) = dir.to_str() else { return false };
        let Some(session) = d.strip_prefix(TOOLS).or_else(|| d.strip_prefix(SESSION)) else { return false };
        crate::os::contain::set_cpu_cap(session, Some(share_pct))
    }

    #[cfg(not(windows))]
    pub fn apply_soft_throttle(_dir: &Path, _share_pct: u8, _key: &str) -> bool {
        false
    }

    /// What stopping the session stops: each of its processes now, by id and start (an id may be
    /// another's by the time it's started again); one started meanwhile runs until the next stop.
    pub fn keys(dir: &Path) -> Vec<String> {
        pids(dir).into_iter().filter(|p| !stopped_already(*p)).filter_map(super::Proc::of).map(|p| key(&p)).collect()
    }

    pub(super) fn key(p: &super::Proc) -> String {
        format!("pid:{}:{}", p.pid, p.start)
    }

    fn parse(key: &str) -> Option<super::Proc> {
        let (pid, start) = key.strip_prefix("pid:")?.split_once(':')?;
        Some(super::Proc { pid: pid.parse().ok()?, start: start.parse().ok()? })
    }

    /// A process someone else stopped (macOS: SIGSTOP isn't counted, so it's left alone and never
    /// continued here). Windows counts each suspension, so each undoes only its own.
    #[cfg(target_os = "macos")]
    fn stopped_already(pid: u32) -> bool {
        use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};
        let pid = Pid::from_u32(pid);
        let mut sys = System::new();
        sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, ProcessRefreshKind::nothing());
        sys.process(pid).is_some_and(|p| p.status() == ProcessStatus::Stop)
    }

    #[cfg(not(target_os = "macos"))]
    fn stopped_already(_pid: u32) -> bool {
        false
    }

    /// Stops what `keys` name: SIGSTOP on macOS, every thread suspended on Windows; says which it
    /// stopped.
    pub fn stop_keys(keys: &[String]) -> Vec<String> {
        let procs: Vec<(&String, u32)> = keys.iter().filter_map(|k| parse(k).map(|p| (k, p.pid))).collect();
        #[cfg(target_os = "macos")]
        let stopped: Vec<u32> = procs.iter().map(|(_, p)| *p).filter(|p| i32::try_from(*p).is_ok_and(|p| nix::sys::signal::kill(nix::unistd::Pid::from_raw(p), nix::sys::signal::Signal::SIGSTOP).is_ok())).collect();
        #[cfg(windows)]
        let stopped = crate::os::contain::suspend_all(&procs.iter().map(|(_, p)| *p).collect::<Vec<_>>());
        #[cfg(not(any(target_os = "macos", windows)))]
        let stopped: Vec<u32> = Vec::new();
        procs.into_iter().filter(|(_, p)| stopped.contains(p)).map(|(k, _)| k.clone()).collect()
    }

    /// Starts again (or removes smooth CPU cap on) what a key names, while it's still the process
    /// or job it was.
    pub fn release(key: &str) {
        #[cfg(windows)]
        if let Some(session) = key.strip_prefix("winjob:") {
            let _ = crate::os::contain::set_cpu_cap(session, None);
            return;
        }
        let Some(p) = parse(key).filter(super::Proc::alive) else { return };
        #[cfg(target_os = "macos")]
        if let Ok(pid) = i32::try_from(p.pid) {
            let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGCONT);
        }
        #[cfg(windows)]
        crate::os::contain::resume_all(&[p.pid]);
        #[cfg(not(any(target_os = "macos", windows)))]
        let _ = p;
    }

    /// No unit outside kiln's own groups is paced here, so the guard needs no scope of its own.
    pub fn guard_in_scope() -> bool {
        false
    }
}

#[cfg(test)]
mod portable_tests {
    use std::io::Write as _;

    /// Set in a child this test starts: the child writes there until it's ended.
    const WRITER: &str = "KILN_HEADROOM_TEST_WRITER";

    use std::time::Duration;

    /// A child process writing (and syncing) a file until it's ended: this test binary again, run
    /// as the test named `test`, which writes when it finds [`WRITER`] set.
    struct Writer {
        child: std::process::Child,
        _dir: tempfile::TempDir,
    }

    impl Writer {
        fn start(test: &str) -> anyhow::Result<Self> {
            // On a real disk next to the test binary (a tmpfs would never count a write).
            let exe = std::env::current_exe()?;
            let dir = tempfile::tempdir_in(exe.parent().ok_or_else(|| anyhow::anyhow!("no folder"))?)?;
            let me = format!("{}::{test}", module_path!().split_once("::").map_or("", |(_, m)| m));
            let file = dir.path().join("w");
            let child = std::process::Command::new(&exe)
                .args([me.as_str(), "--exact", "--nocapture", "--test-threads=1"])
                .env(WRITER, &file)
                .stdout(std::process::Stdio::null())
                .spawn()?;
            let w = Self { child, _dir: dir };
            // Once it writes: a loaded machine can take a while to start a test binary.
            anyhow::ensure!(within(Duration::from_secs(60), || std::fs::metadata(&file).is_ok_and(|m| m.len() > 0)), "the writer never wrote");
            Ok(w)
        }

        fn pid(&self) -> [u32; 1] {
            [self.child.id()]
        }

        /// What it did over `d`: CPU µs and bytes written.
        #[cfg(any(target_os = "macos", windows))]
        fn over(&self, d: Duration) -> (u64, u64) {
            let (c0, w0, _) = super::processes(&self.pid());
            std::thread::sleep(d);
            let (c1, w1, _) = super::processes(&self.pid());
            (c1.saturating_sub(c0), w1.saturating_sub(w0))
        }

        /// Whether it uses CPU and writes, both, within `limit` (one sync can hold it a while on a
        /// loaded disk).
        fn works_within(&self, limit: Duration) -> bool {
            let (c0, w0, _) = super::processes(&self.pid());
            within(limit, || {
                let (c, w, _) = super::processes(&self.pid());
                c > c0 && w > w0
            })
        }
    }

    impl Drop for Writer {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// In the child [`Writer`] starts: write until ended.
    fn write_if_asked() -> anyhow::Result<()> {
        let Some(path) = std::env::var_os(WRITER) else { return Ok(()) };
        let mut f = std::fs::File::create(path)?;
        let block = vec![7u8; 1 << 20];
        loop {
            f.write_all(&block)?;
            f.sync_data()?;
        }
    }

    #[test]
    fn a_processs_cpu_and_writes_rise_as_it_works() -> anyhow::Result<()> {
        write_if_asked()?;
        let w = Writer::start("a_processs_cpu_and_writes_rise_as_it_works")?;
        let (_, _, mem) = super::processes(&w.pid());
        anyhow::ensure!(mem > 0, "no memory");
        anyhow::ensure!(w.works_within(Duration::from_secs(30)), "no CPU time or nothing written");
        Ok(())
    }

    /// Sleep/resume pauses and slow disk writes in `tell()` must never inflate the pacer's RTO lag
    /// estimator or turn `cycle = lag * 100` into multi-minute/multi-hour runner freezes.
    #[test]
    fn sleep_or_disk_lag_spikes_never_produce_runaway_pacing_cycles() {
        // Normal 2 ms switch lag stays exact: 200 ms cycle.
        assert_eq!(super::bounded_cycle_us(5_000, Some(2_000.0)), 200_000);
        // Huge 500 ms lag estimate is bounded at 2 s max cycle (not 50 s!).
        assert_eq!(super::bounded_cycle_us(200_000, Some(500_000.0)), 2_000_000);
        // A 1-hour laptop sleep outlier is rejected completely from lag samples.
        assert_eq!(super::bounded_lag_sample(3_600_000_000, 1_000_000), None);
        // A 500 ms disk-stall outlier within a 2s window is clamped to 50 ms.
        assert_eq!(super::bounded_lag_sample(500_000, 1_000_000), Some(50_000.0));
        // Normal 3 ms wake jitter passes through intact.
        assert_eq!(super::bounded_lag_sample(3_000, 1_000_000), Some(3_000.0));
    }

    /// Set in a child these tests start as a pacer's guard: the record it guards.
    const GUARD: &str = "KILN_HEADROOM_TEST_GUARD";

    /// A pacer's guard in tests: this test binary again, run as [`guard_child`] (`kiln
    /// headroom-guard` is the kiln binary's own).
    pub(super) fn test_guard(record: &std::path::Path) -> std::io::Result<std::process::Command> {
        let mut c = std::process::Command::new(std::env::current_exe()?);
        c.args([this("guard_child").as_str(), "--exact", "--nocapture", "--quiet", "--test-threads=1"]).env(GUARD, record);
        Ok(c)
    }

    /// The name this module's test `test` runs by.
    fn this(test: &str) -> String {
        format!("{}::{test}", module_path!().split_once("::").map_or("", |(_, m)| m))
    }

    /// Whether `f` holds within `limit`, looked at every 10 ms.
    pub(super) fn within(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
        let end = std::time::Instant::now() + limit;
        loop {
            if f() {
                return true;
            }
            if std::time::Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn guard_child() {
        if let Some(record) = std::env::var_os(GUARD) {
            let _ = super::guard(std::path::Path::new(&record));
        }
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn a_stopped_process_does_nothing_and_works_again_once_released() -> anyhow::Result<()> {
        write_if_asked()?;
        let w = Writer::start("a_stopped_process_does_nothing_and_works_again_once_released")?;
        let p = super::Proc::of(w.child.id()).ok_or_else(|| anyhow::anyhow!("the writer's start is unknown"))?;
        let keys = super::imp::stop_keys(&[super::imp::key(&p)]);
        anyhow::ensure!(keys.len() == 1, "{keys:?}");
        // What was under way when it stopped lands first.
        std::thread::sleep(Duration::from_millis(200));
        let (cpu, written) = w.over(Duration::from_millis(700));
        anyhow::ensure!(cpu == 0 && written == 0, "stopped, yet {cpu} µs of CPU and {written} bytes written");
        keys.iter().for_each(|k| super::imp::release(k));
        anyhow::ensure!(w.works_within(Duration::from_secs(30)), "released, yet no CPU time or nothing written");
        Ok(())
    }

    /// Set in the child pacer [`a_pacer_killed_while_it_holds_a_process_stopped_leaves_it_running`]
    /// starts: what it stops, and its record.
    #[cfg(any(target_os = "macos", windows))]
    const STOP: &str = "KILN_HEADROOM_TEST_STOP";
    #[cfg(any(target_os = "macos", windows))]
    const RECORD: &str = "KILN_HEADROOM_TEST_RECORD";

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn a_pacer_killed_while_it_holds_a_process_stopped_leaves_it_running() -> anyhow::Result<()> {
        const NAME: &str = "a_pacer_killed_while_it_holds_a_process_stopped_leaves_it_running";
        write_if_asked()?;
        if let (Some(key), Some(record)) = (std::env::var_os(STOP), std::env::var_os(RECORD)) {
            return hold(&key.to_string_lossy(), std::path::Path::new(&record));
        }
        let w = Writer::start(NAME)?;
        let p = super::Proc::of(w.child.id()).ok_or_else(|| anyhow::anyhow!("the writer's start is unknown"))?;
        let dir = tempfile::tempdir()?;
        let mut pacer = std::process::Command::new(std::env::current_exe()?)
            .args([this(NAME).as_str(), "--exact", "--nocapture", "--quiet", "--test-threads=1"])
            .env(STOP, super::imp::key(&p))
            .env(RECORD, dir.path().join("pacer.json"))
            .stdout(std::process::Stdio::null())
            .spawn()?;
        let stopped = within(Duration::from_secs(20), || w.over(Duration::from_millis(200)).0 == 0);
        let _ = pacer.kill();
        let _ = pacer.wait();
        anyhow::ensure!(stopped, "the child pacer never stopped the writer");
        anyhow::ensure!(within(Duration::from_secs(1), || w.over(Duration::from_millis(50)).0 > 0), "still stopped 1 s after its pacer was killed");
        Ok(())
    }

    /// In the child pacer: its guard told first, the process stopped, then held until killed.
    #[cfg(any(target_os = "macos", windows))]
    fn hold(key: &str, record: &std::path::Path) -> anyhow::Result<()> {
        let me = super::Proc::me().ok_or_else(|| anyhow::anyhow!("this process's start is unknown"))?;
        let paced = super::Paced { me, record: record.to_owned(), how: test_guard };
        let wake = std::sync::Arc::new(super::Wake::default());
        let mut g = super::Guard::start(&paced, false, &wake)?;
        anyhow::ensure!(within(Duration::from_secs(20), || g.ready()), "the guard never said it runs");
        let key = key.to_owned();
        anyhow::ensure!(g.tell(&std::collections::BTreeSet::from([&key])), "the guard didn't hear");
        anyhow::ensure!(super::imp::stop_keys(std::slice::from_ref(&key)).len() == 1, "nothing stopped");
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
}

#[cfg(test)]
mod duty_tests {
    use super::*;

    #[test]
    fn lb05_g11_measuring_spans_a_pacers_step_of_ticks() {
        // A tick's error on each CPU, over the span, is a hundredth of it: a pacer's step.
        let tick = cpu_tick();
        assert!(!tick.is_zero());
        assert_eq!(measured_over(), tick.mul_f64(kiln_ci::headroom::pace::SHARE_STEPS));
        #[cfg(unix)]
        assert_eq!(rustix::param::clock_ticks_per_second(), 100, "USER_HZ, as Linux and macOS have it: measured over 1 s");
        #[cfg(unix)]
        assert_eq!(measured_over(), Duration::from_secs(1));
    }

    #[test]
    fn a_share_above_zero_is_a_step_of_at_least_one_and_never_a_stop() {
        assert_eq!(step_of(0.0), 0);
        assert_eq!(step_of(0.004), 1);
        assert_eq!(step_of(0.014), 1);
        assert_eq!(step_of(0.5), 50);
        assert_eq!(step_of(0.999), 99);
        assert_eq!(step_of(1.0), 99);
    }

    #[test]
    fn what_a_group_was_held_stopped_adds_up_across_holds() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut s = Stopped::default();
        s.hold(at(100));
        s.hold(at(150));
        assert_eq!(s.total(at(300)), Duration::from_millis(200));
        s.release(at(400));
        s.release(at(450));
        s.hold(at(500));
        assert_eq!(s.total(at(550)), Duration::from_millis(350));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn desktop_apps_are_never_paced_but_systemd_run_builds_are() {
        assert!(imp::desktop("app-com.mitchellh.ghostty.service"));
        assert!(imp::desktop("app-org.kde.konsole-1234.scope"));
        assert!(!imp::desktop("run-p50766-i22306.scope"));
        assert!(!imp::desktop("amphora-gate-order-1793058099.service"));
    }

    use super::portable_tests::{test_guard, within};

    /// A "group" that's only a folder: freezing and thawing write its cgroup.freeze, as on a real
    /// cgroup.
    fn group(dir: &Path, name: &str, frozen: bool) -> anyhow::Result<PathBuf> {
        let g = dir.join(name);
        std::fs::create_dir(&g)?;
        std::fs::write(g.join("cgroup.freeze"), if frozen { "1" } else { "0" })?;
        Ok(g)
    }

    fn frozen(g: &Path) -> bool {
        std::fs::read_to_string(g.join("cgroup.freeze")).is_ok_and(|v| v.trim() == "1")
    }

    /// The pacers' records in `dir`.
    fn records(dir: &Path) -> Vec<Record> {
        std::fs::read_dir(dir.join(RECORDS))
            .map(|rd| rd.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "json")).filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn a_record_left_behind_starts_its_groups_again_and_goes() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let g = group(dir.path(), "g", true)?;
        // The record pacers before these kept, one for all of them.
        std::fs::write(dir.path().join("paced.json"), serde_json::to_vec(&[g.display().to_string()])?)?;
        let p = Pacer::start_with(dir.path(), test_guard);
        anyhow::ensure!(!frozen(&g));
        anyhow::ensure!(!dir.path().join("paced.json").exists());
        drop(p);
        Ok(())
    }

    #[test]
    fn a_paced_group_is_stopped_part_of_each_period_and_runs_once_let_go() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let g = group(dir.path(), "g", false)?;
        let p = Pacer::start_with(dir.path(), test_guard);
        p.set(HashMap::from([(g.clone(), 0)]), Duration::from_millis(200));
        anyhow::ensure!(within(Duration::from_secs(20), || frozen(&g)), "0% is stopped once its guard runs");
        anyhow::ensure!(records(dir.path()).iter().any(|r| r.stopped.contains(&g.display().to_string())), "what is paced is noted");
        p.set(HashMap::new(), Duration::from_millis(200));
        anyhow::ensure!(within(Duration::from_secs(5), || !frozen(&g)), "let go, it runs");
        anyhow::ensure!(within(Duration::from_secs(5), || records(dir.path()).is_empty()), "nothing paced, nothing noted");
        Ok(())
    }

    /// Set in the child pacer [`a_pacer_killed_while_its_group_is_frozen_leaves_it_running`]
    /// starts: the folder its group is in.
    const PACER: &str = "KILN_HEADROOM_TEST_PACER";

    #[test]
    fn a_group_held_to_a_small_share_of_a_short_period_is_stopped_and_still_runs() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let g = group(dir.path(), "g", false)?;
        let p = Pacer::start_with(dir.path(), test_guard);
        // 14% of 5 ms: the pacer once gave every share under 20% of a period that short no time to run.
        p.set(HashMap::from([(g.clone(), 14)]), Duration::from_millis(5));
        anyhow::ensure!(within(Duration::from_secs(20), || frozen(&g)), "stopped once its guard runs");
        anyhow::ensure!(within(Duration::from_secs(10), || !frozen(&g)), "held to 14%, it never ran");
        anyhow::ensure!(p.stopped().get(&g).is_some_and(|d| !d.is_zero()), "what it was held stopped isn't counted");
        drop(p);
        Ok(())
    }

    #[test]
    fn a_pacer_killed_while_its_group_is_frozen_leaves_it_running() -> anyhow::Result<()> {
        const NAME: &str = "a_pacer_killed_while_its_group_is_frozen_leaves_it_running";
        if let Some(dir) = std::env::var_os(PACER) {
            // In the child: the group paced at 0% until this process is killed.
            let dir = PathBuf::from(dir);
            let p = Pacer::start_with(&dir, test_guard);
            p.set(HashMap::from([(dir.join("g"), 0)]), Duration::from_millis(200));
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
        let dir = tempfile::tempdir()?;
        let g = group(dir.path(), "g", false)?;
        let me = format!("{}::{NAME}", module_path!().split_once("::").map_or("", |(_, m)| m));
        let mut pacer = Command::new(std::env::current_exe()?).args([me.as_str(), "--exact", "--nocapture", "--quiet", "--test-threads=1"]).env(PACER, dir.path()).stdout(Stdio::null()).spawn()?;
        let stopped = within(Duration::from_secs(20), || frozen(&g));
        // SIGKILL: nothing of the pacer runs to start its group again.
        let _ = pacer.kill();
        let _ = pacer.wait();
        anyhow::ensure!(stopped, "the child pacer never stopped its group");
        anyhow::ensure!(within(Duration::from_secs(1), || !frozen(&g)), "still frozen 1 s after its pacer was killed");
        anyhow::ensure!(within(Duration::from_secs(1), || records(dir.path()).is_empty()), "its record is left");
        Ok(())
    }

    /// A process that has ended, by its id and start as a record has it.
    fn ended() -> anyhow::Result<Proc> {
        let mut c = Command::new("sleep").arg("60").spawn()?;
        let p = Proc::of(c.id()).ok_or_else(|| anyhow::anyhow!("no start for {}", c.id()))?;
        c.kill()?;
        c.wait()?;
        Ok(p)
    }

    /// The outage of 2026-10-29: a lab runner's pacer ended while a process it left kept its end
    /// of the guard's input open, so its guard never started what it had stopped.
    #[test]
    fn a_guard_whose_pacer_ended_releases_though_its_input_stays_open() -> anyhow::Result<()> {
        use std::io::Write as _;
        let dir = tempfile::tempdir()?;
        let g = group(dir.path(), "a", true)?;
        let mut pacer = Command::new("sleep").arg("60").spawn()?;
        let p = Proc::of(pacer.id()).ok_or_else(|| anyhow::anyhow!("no start for {}", pacer.id()))?;
        let record = dir.path().join(format!("{}-{}.json", p.pid, p.start));
        let mut guard = test_guard(&record)?.stdin(Stdio::piped()).stdout(Stdio::null()).spawn()?;
        let mut input = guard.stdin.take().ok_or_else(|| anyhow::anyhow!("the guard's input wasn't kept"))?;
        writeln!(input, "{}", serde_json::to_string(&[g.display().to_string()])?)?;
        input.flush()?;
        // Time for it to read the list (it's told nothing back).
        std::thread::sleep(Duration::from_millis(500));
        let held = frozen(&g);
        let at = Instant::now();
        pacer.kill()?;
        pacer.wait()?;
        let released = within(Duration::from_secs(10), || !frozen(&g));
        // At the pacer's end, by the kernel's word: not a look 2 s later (LB05, G11).
        let took = at.elapsed();
        drop(input);
        let _ = guard.kill();
        let _ = guard.wait();
        anyhow::ensure!(held, "started again while its pacer still ran");
        anyhow::ensure!(released, "still stopped after its pacer ended");
        anyhow::ensure!(took < Duration::from_secs(1), "released {took:?} after its pacer ended");
        Ok(())
    }

    #[test]
    fn a_record_whose_pacer_and_guard_are_gone_is_released_and_one_still_running_is_left() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (gone_g, live_g) = (group(dir.path(), "a", true)?, group(dir.path(), "b", true)?);
        let gone = ended()?;
        let me = Proc::me().ok_or_else(|| anyhow::anyhow!("this process's start is unknown"))?;
        let recs = dir.path().join(RECORDS);
        std::fs::create_dir(&recs)?;
        std::fs::write(recs.join("gone.json"), serde_json::to_vec(&Record { owner: Some(gone), guard: None, stopped: vec![gone_g.display().to_string()] })?)?;
        std::fs::write(recs.join("live.json"), serde_json::to_vec(&Record { owner: Some(me), guard: None, stopped: vec![live_g.display().to_string()] })?)?;
        let p = Pacer::start_with(dir.path(), test_guard);
        anyhow::ensure!(!frozen(&gone_g) && !recs.join("gone.json").exists(), "what an ended pacer left runs again");
        anyhow::ensure!(frozen(&live_g) && recs.join("live.json").exists(), "a running pacer's is its own");
        drop(p);
        Ok(())
    }

    #[test]
    fn a_record_left_while_a_pacer_runs_is_released_once_it_appears() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let g = group(dir.path(), "g", true)?;
        let p = Pacer::start_with(dir.path(), test_guard);
        p.set(HashMap::new(), Duration::from_millis(200));
        // A pacer that ended with its guard after this one started (one this runner took over
        // from, say): its record as it left it.
        let recs = dir.path().join(RECORDS);
        std::fs::create_dir_all(&recs)?;
        // Written as pacers write theirs, whole and renamed into place.
        crate::util::write_secret_unsynced(&recs.join("gone.json"), &serde_json::to_vec(&Record { owner: Some(ended()?), guard: None, stopped: vec![g.display().to_string()] })?, 0o644)?;
        anyhow::ensure!(within(Duration::from_secs(5), || !frozen(&g)), "still frozen");
        // Removed once what it lists runs again, a moment after.
        anyhow::ensure!(within(Duration::from_secs(5), || !recs.join("gone.json").exists()), "its record is left");
        drop(p);
        Ok(())
    }

    #[test]
    fn a_group_someone_else_froze_stays_frozen_through_pacing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (theirs, ours) = (group(dir.path(), "theirs", true)?, group(dir.path(), "ours", false)?);
        let p = Pacer::start_with(dir.path(), test_guard);
        p.set(HashMap::from([(theirs.clone(), 0), (ours.clone(), 0)]), Duration::from_millis(200));
        anyhow::ensure!(within(Duration::from_secs(20), || frozen(&ours)), "ours is stopped once its guard runs");
        anyhow::ensure!(records(dir.path()).iter().all(|r| !r.stopped.contains(&theirs.display().to_string())), "theirs is noted as ours");
        p.set(HashMap::new(), Duration::from_millis(200));
        anyhow::ensure!(within(Duration::from_secs(5), || !frozen(&ours)), "ours runs once let go");
        drop(p);
        anyhow::ensure!(frozen(&theirs), "theirs was thawed");
        Ok(())
    }

    #[test]
    fn a_runner_its_supervisor_did_not_start_paces_only_kilns_own_groups() {
        // This test process: started by cargo, not by kiln's supervisor.
        assert!(!crate::os::supervise::started_by_supervisor());
        let gs = groups(true);
        assert!(gs.iter().all(|g| g.session.is_some()), "{gs:?}");
    }

    #[test]
    fn the_machines_lock_goes_to_the_next_holder_once_let_go() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("escapes.lock");
        let first = imp::take_lock(&path).ok_or_else(|| anyhow::anyhow!("not taken"))?;
        anyhow::ensure!(imp::take_lock(&path).is_none(), "two holders");
        drop(first);
        // At the next try (a child another test is starting holds the lock's file until it execs).
        anyhow::ensure!(within(Duration::from_secs(1), || imp::take_lock(&path).is_some()), "not taken once let go");
        Ok(())
    }
}
