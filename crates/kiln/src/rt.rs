//! kiln's runtime: kiln-rt, its own copy of hyper-raft's hyper-rt (thread-per-core shards with an
//! epoll driver and a timing wheel, a blocking pool, a compute pool, signals, DNS) behind the handful
//! of primitives the rest of kiln uses.
//!
//! * One process-wide [`hyper_rt::Runtime`], started on first use with the sizing chosen by
//!   [`configure`] (server, agent or CLI). There is no preemption, and shards take each other's work
//!   only where the server's placement makes its requests movable tasks (goal 7, `crate::placement`),
//!   so CPU-heavy work goes to the compute pool ([`compute`], [`compute_pieces`]: off the shards and
//!   below them in the OS's priority) and calls that block go to the blocking pool ([`blocking`]).
//! * [`spawn`] keeps a task on the calling shard (locality: sockets and timers stay where they were
//!   registered); spawns from outside the runtime are spread round-robin over the shards. Accepted
//!   connections go where the placement says ([`spawn_connection`]).
//! * A task gives up its shard on time with no hand-placed yields: hyper-rt charges its leaf futures
//!   (sockets, timers, its channels) to the polling task, and the cross-task primitives here charge
//!   the same budget (`hyper_rt::coop`, kiln's goal 6), so a loop whose waits are always ready yields
//!   at its share; a long computation calls [`consume_budget`] between pieces.
//! * hyper-rt's leaf futures (timers, socket readiness, its sync channels) accept only the polling
//!   task's own waker. Everything exported here works under any waker: timers fall back to a
//!   helper task ([`Bridge`]) when polled through a combinator or library that substitutes its own
//!   waker, and the cross-task primitives work with any executor: [`Notify`], [`Shutdown`] and [`Changed`]
//!   wait on `crate::memory::queue`'s lock-free list of waiters (goal 5), [`Broadcast`] on
//!   `crate::memory::lanes`, and the channels here are `async-channel`'s.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::{Pin, pin};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use bytes::Bytes;
use futures::FutureExt as _;
use hyper_rt::{RtError, Runtime, RuntimeConfig, SlotHolder, TaskId};

// ------------------------------------------------------------------------------------- sizing

/// How big the process's runtime is. Chosen once, before first use ([`configure`]).
#[derive(Clone, Copy, Debug)]
pub struct Profile {
    /// Shard threads.
    pub shards: u16,
    /// Task slots per shard (about 650 B of RSS each with timers and interests at 2x).
    pub tasks_per_shard: usize,
    /// Places in the blocking pool's shares: (dns, store, fs). The blocking pool is for calls that block a
    /// thread; CPU work goes to the compute pool ([`compute`]; goal 3, plan 01a1a59c2a8e step 4), which the
    /// pool's `fs` share carried before (the UI's compression, password hashing, digests of bytes in
    /// memory). The server's come from what the lab measured of each share (goal 3, step 4; the series
    /// `kiln_blocking_*` on /metrics, `crate::pool_health`; docs/runtime-lab.md, "Goal 3"), by Little's law
    /// as hyper-rt sizes a shard's tasks (`hyper_rt::runtime::admission_limit`): the jobs a share holds at
    /// once are the rate they come at times the time each runs, here its p99 ([`AtOnce`]):
    /// - `fs` (files, `/proc`, archives, a child's wait): [`SERVER_FS`];
    /// - `store` (kiln.db's reads, each on a read-only connection of its own; its writes go to its writer's
    ///   thread, goal 5, plan 01a1a59c2aae): [`SERVER_STORE`];
    /// - `dns` (`getaddrinfo`): the lab's kiln resolves no name (every address it dials is a literal), so
    ///   the lab can't size it: four, kiln's count from its move onto hyper-rt (f202896cb2d), until
    ///   production's `kiln_blocking_run_seconds{share="dns"}` says how many it holds at once.
    ///
    /// A runner's and a command's are kiln's counts from that move: the lab reads only its server's
    /// /metrics.
    pub pool: (usize, usize, usize),
    /// Whether the runtime says how it was sized at `info` (a server's and a runner's logs), or only at
    /// `debug` (a command's, whose output is the person's).
    pub announce: bool,
}

/// What the lab measured of one of the server's blocking pool shares where it held the most (goal 3, plan
/// 01a1a59c2a8e step 4; docs/runtime-lab.md, "Goal 3"): its jobs at once by Little's law, each round's jobs a
/// second times their p99 run (kiln-lab's `blocking.<share>.little_p99`), the median of a run's rounds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AtOnce(pub f64);

impl AtOnce {
    /// The places that is: rounded up to whole places, at least one.
    pub fn places(self) -> usize {
        if self.0.is_finite() && self.0 > 1.0 { self.0.ceil() as usize } else { 1 }
    }
}

/// The server's `fs` share where its jobs ran longest, with as many CPU hogs as the lab's CPUs: 24.9 jobs a
/// second at a p99 run of 1,056 ms, 25.5 at once (four rounds, 2026-11-04; calm, 47.8 a second at a p99 of
/// 82 ms, 3.8 at once). Against the 8 places it had, 26 made no difference beyond noise in any of the lab's
/// 94 numbers there (six rounds each, interleaved): its jobs waited 4.7 and 4.8 ms at p99 for a place, the
/// pool's threads' own wait for a CPU beside the hogs, not a share that's full.
pub const SERVER_FS: AtOnce = AtOnce(25.54);

/// The server's `store` share: kiln.db's reads, each on a read-only connection of its own (`crate::store`,
/// goal 5 of the API runtime program, plan 01a1a59c2aae), as many at once as the lab measured its calls
/// hold: 982 a second at a p99 run of 24.9 ms calm, 23.9 at once (goal 3's measure, when every call, reads
/// and writes, took the one connection in turn, and two places held it). Its writes go to the writer's
/// thread, as many as SQLite takes at once: one.
pub const SERVER_STORE: AtOnce = AtOnce(23.9);

/// The server's `dns` share: kiln's count from its move onto hyper-rt, which the lab can't measure
/// ([`Profile::pool`]).
pub const SERVER_DNS: usize = 4;

impl Profile {
    /// The server: HTTP connections, websockets, the scheduler and GitOps loops.
    pub fn server() -> Self {
        let cpus = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        let shards = u16::try_from((cpus / 4).clamp(2, 4)).unwrap_or(2);
        Self { shards, tasks_per_shard: 8192, pool: (SERVER_DNS, SERVER_STORE.places(), SERVER_FS.places()), announce: true }
    }

    /// An agent: a few jobs, their output pumps, terminals.
    pub fn agent() -> Self {
        Self { shards: 2, tasks_per_shard: 2048, pool: (2, 1, 6), announce: true }
    }

    /// One-shot CLI commands and tests.
    pub fn tool() -> Self {
        Self { shards: 2, tasks_per_shard: 4096, pool: (2, 2, 4), announce: false }
    }

    fn with_env_overrides(mut self) -> Self {
        if let Some(n) = std::env::var("KILN_RT_SHARDS").ok().and_then(|v| v.parse::<u16>().ok()) {
            self.shards = n.clamp(1, 64);
        }
        if let Some(n) = std::env::var("KILN_RT_TASKS").ok().and_then(|v| v.parse::<usize>().ok()) {
            self.tasks_per_shard = n.clamp(256, 1 << 20);
        }
        // The blocking pool's places, `dns,store,fs` (the lab's A/B of share sizes on one build).
        if let Some(pool) = std::env::var("KILN_RT_BLOCKING").ok().as_deref().and_then(places) {
            self.pool = pool;
        }
        self
    }
}

/// `dns,store,fs` as three counts of places, each at least one.
fn places(v: &str) -> Option<(usize, usize, usize)> {
    let mut it = v.split(',').map(|n| n.trim().parse::<usize>().ok().filter(|n| *n > 0));
    let pool = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(pool)
}

static PROFILE: OnceLock<Profile> = OnceLock::new();

/// Choose the runtime's size. Only the first call before the runtime starts has any effect.
pub fn configure(profile: Profile) {
    let _ = PROFILE.set(profile);
}

/// The runtime's size as it starts (or started): the profile chosen, with the environment's overrides.
pub fn profile() -> Profile {
    PROFILE.get().copied().unwrap_or_else(Profile::tool).with_env_overrides()
}

/// The cores of the process's own its shards are fixed to (docs/priority.md, "Cores of its own";
/// goal 8, plan 01a1a59c2b29 step 6), as [`pin`] chose them.
static CORES: OnceLock<Vec<u32>> = OnceLock::new();

/// Whether the process's other threads are kept off its shards' cores ([`pin`]): its compute pool
/// is then sized to the cores they have, the shards not among them.
static SHARDS_APART: AtomicBool = AtomicBool::new(false);

/// Fix the runtime's shards to `cores`, cores of the process's own (the server's `[server] cores`),
/// in their order, one shard to a core, and keep every other thread the process starts from here on
/// off the shards' cores, so a shard waking finds its core free (goal 9, plan 01a1a87b2cec step 4: under
/// Linux's lazy preemption a waking thread waits up to a tick, 1 ms, for a busy core's thread to
/// give way, research 01a1a59c2b2a f36). Call it on the thread that starts the runtime, before it
/// starts: only the first call has any effect.
pub fn pin(cores: Vec<u32>) {
    if CORES.get().is_some() {
        return;
    }
    let mut shards = shard_cores(&cores, profile().shards);
    shards.sort_unstable();
    shards.dedup();
    // Apart only where the other threads keep a core: the first, which `shard_cores` leaves them.
    let apart = !shards.is_empty() && shards.len() < cores.len();
    let _ = CORES.set(cores);
    if apart {
        match crate::os::keep_threads_off(&shards) {
            Ok(()) => SHARDS_APART.store(true, Ordering::Relaxed),
            Err(why) => tracing::warn!(why = %why, "the server's other threads share its shards' cores"),
        }
    }
}

/// The real-time priority the shards ask for ([`realtime`]): the lowest (sched(7): 1 to 99), above
/// every thread of the normal policies whatever its nice value and below the kernel's own real-time
/// threads; what `kiln install` gives the server's unit room for (`LimitRTPRIO=`).
pub const REALTIME_PRIORITY: i32 = hyper_rt::compute::REALTIME_LOWEST;

/// Whether the shards ask for Linux's round-robin real-time class ([`realtime`]).
static REALTIME: AtomicBool = AtomicBool::new(false);

/// The shards' answers as each took the real-time class or was refused it: those that took it
/// counted in units of one, those refused in units of [`REFUSED_ONE`], in one atomic, so exactly one
/// shard, the last to answer, sees every answer in.
static REALTIME_ANSWERS: AtomicU32 = AtomicU32::new(0);

/// A refused shard's count in [`REALTIME_ANSWERS`]: past the most shards a runtime has
/// (`Profile::with_env_overrides`' 64), so the two counts never meet.
const REFUSED_ONE: u32 = 1 << 16;

/// How many shards asked for the real-time class: the runtime's, once it started.
static REALTIME_SHARDS: AtomicU32 = AtomicU32::new(0);

/// The first refusal a shard met, for why.
static REALTIME_REFUSAL: OnceLock<hyper_rt::compute::Refusal> = OnceLock::new();

/// The server's shards in Linux's round-robin real-time class (`[server] realtime`; docs/priority.md,
/// "A real-time class for the shards"; goal 8, plan 01a1a59c2b29 step 7; goal 9, plan 01a1a87b2cec
/// step 4): each shard asks as it starts, at [`REALTIME_PRIORITY`], so nothing it starts inherits it
/// (reset on fork). Under Linux's lazy preemption a shard of the normal class waking where a process
/// is busy in user space waits up to a tick, 1 ms, for it; a real-time one takes the CPU at once.
/// Where the system refuses it (no `CAP_SYS_NICE` or `RLIMIT_RTPRIO` room; macOS and Windows), a shard
/// runs as before, with a slice of its step, and the log and `/healthz/server` say why
/// ([`realtime_status`]). Call it before the runtime starts: later calls change nothing.
pub fn realtime(on: bool) {
    REALTIME.store(on, Ordering::Relaxed);
}

/// How the shards took the real-time class ([`realtime`]), for the log and `/healthz/server`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Realtime {
    /// Whether it was asked for (`[server] realtime`).
    pub asked: bool,
    /// The shards that asked.
    pub shards: u32,
    /// Of them, those that took it.
    pub took: u32,
    /// And those refused it, which run at the normal class with a slice of their step.
    pub refused: u32,
    /// Why those refused were, for a person.
    pub why: Option<String>,
}

impl Realtime {
    /// The line it's said with once every shard has answered; `None` before, or not asked for.
    pub fn said(&self) -> Option<String> {
        if !self.asked || self.took.saturating_add(self.refused) < self.shards {
            return None;
        }
        Some(match (self.took, &self.why) {
            (took, _) if self.refused == 0 => format!("the server's {took} shards run in Linux's round-robin real-time class at priority {REALTIME_PRIORITY}: a shard waking takes its CPU at once, whatever runs there"),
            (0, why) => format!("the server's {} shards run at the normal class, with a slice of their step: the real-time class was refused ({})", self.refused, why.as_deref().unwrap_or("no reason given")),
            (took, why) => format!("{took} of the server's {} shards run in the real-time class, {} at the normal class: it was refused ({})", self.shards, self.refused, why.as_deref().unwrap_or("no reason given")),
        })
    }
}

/// Why the real-time class was refused, for a person, from what refused it.
fn realtime_why(refusal: &hyper_rt::compute::Refusal) -> String {
    match refusal.code.map(|c| io::Error::from_raw_os_error(c).kind()) {
        Some(io::ErrorKind::PermissionDenied) => format!(
            "{refusal}: this process has neither CAP_SYS_NICE nor RLIMIT_RTPRIO room for priority {REALTIME_PRIORITY} (`kiln install` gives the server's unit LimitRTPRIO={REALTIME_PRIORITY}, which a user's service manager grants only as far as its own limit), or its kernel schedules real-time groups and gave the server's cgroup no real-time runtime"
        ),
        Some(io::ErrorKind::InvalidInput) => format!("{refusal}: the kernel doesn't take the round-robin real-time class here"),
        Some(_) => refusal.to_string(),
        None => refusal.call.to_owned(),
    }
}

/// How the shards took the real-time class so far ([`realtime`]).
pub fn realtime_status() -> Realtime {
    realtime_of(REALTIME_ANSWERS.load(Ordering::Acquire))
}

/// [`Realtime`] from the shards' `answers` ([`REALTIME_ANSWERS`]).
fn realtime_of(answers: u32) -> Realtime {
    Realtime {
        asked: REALTIME.load(Ordering::Relaxed),
        shards: REALTIME_SHARDS.load(Ordering::Relaxed),
        took: answers % REFUSED_ONE,
        refused: answers / REFUSED_ONE,
        why: REALTIME_REFUSAL.get().map(realtime_why),
    }
}

/// A shard asks for the real-time class ([`realtime`]): whether it took it. The last shard to answer
/// says how they all did, once.
fn took_realtime() -> bool {
    let took = hyper_rt::compute::realtime_current_thread(REALTIME_PRIORITY);
    if let Err(refusal) = took {
        let _ = REALTIME_REFUSAL.set(refusal);
    }
    let one = if took.is_ok() { 1 } else { REFUSED_ONE };
    let answers = REALTIME_ANSWERS.fetch_add(one, Ordering::AcqRel).wrapping_add(one);
    let status = realtime_of(answers);
    if status.took.saturating_add(status.refused) == status.shards {
        // The room the server's unit gave the shards (`LimitRTPRIO=`) is theirs alone: nothing the server
        // starts from here on, its own runner's legs and agents among them, inherits it (goal 9, plan
        // 01a1a87b2cec step 4). The shards keep the class they took.
        if let Err(why) = crate::os::drop_realtime_room(None) {
            tracing::warn!(why = %why, "what the server starts keeps its room for the real-time class");
        }
        if let Some(line) = status.said() {
            if status.refused == 0 {
                tracing::info!("{line}");
            } else {
                tracing::warn!("{line}");
            }
        }
    }
    took.is_ok()
}

/// How the shards nudge their CPUs while they wait ([`nudge`]): a [`crate::nudge::Mode`]'s index, off until the
/// server's config says.
static NUDGE: AtomicU8 = AtomicU8::new(0);

/// The shards' answers to the nudge ([`nudge`]), three counts in one atomic so exactly one shard, the
/// last to answer, sees every answer in: those nudged in units of one, those that took the real-time
/// class instead in units of [`NUDGE_REALTIME_ONE`], those refused in units of [`NUDGE_REFUSED_ONE`].
static NUDGE_ANSWERS: AtomicU32 = AtomicU32::new(0);

/// A shard in the real-time class's count in [`NUDGE_ANSWERS`]: past the most shards a runtime has
/// (`Profile::with_env_overrides`' 64) in ten bits, so no count reaches the next.
const NUDGE_REALTIME_ONE: u32 = 1 << 10;

/// A refused shard's count in [`NUDGE_ANSWERS`], as [`NUDGE_REALTIME_ONE`] ten bits further.
const NUDGE_REFUSED_ONE: u32 = 1 << 20;

/// How many shards answer the nudge: the runtime's, once it started.
static NUDGE_SHARDS: AtomicU32 = AtomicU32::new(0);

/// The nudge's period, nanoseconds, once the runtime started ([`nudge_period_ns`]).
static NUDGE_PERIOD_NS: AtomicU64 = AtomicU64::new(0);

/// The first refusal a shard met, for why.
static NUDGE_REFUSAL: OnceLock<hyper_rt::compute::Refusal> = OnceLock::new();

/// How each of the server's shards nudges its CPU while it waits (`[server] nudge`; docs/priority.md, "A nudge
/// for the shards"; goal 8, plan 01a1a59c2b29 step 7; goal 6, plan 01a1a59c2abe step 3): a nudged shard's
/// waits in its driver end by the period [`nudge_period_ns`] gives, so a shard woken behind a thread busy in
/// user space on a kernel that preempts lazily runs at its own timer's interrupt, not at the kernel's next
/// tick (up to 1 ms at 1000 Hz). `on` nudges every wait; `auto`, the mode a config declares, only while the
/// shard's host holds its wakes, judged by its probe each period (`crate::nudge`), so a calm server pays nothing;
/// `off`, the default until a config declares another (docs/ci.md §0.3, "Opt-in automation"), never. In the
/// real-time class's place where that's refused: a shard that took the real-time class
/// ([`realtime`]) preempts at once and doesn't nudge. The log and `/healthz/server` say how the shards took it
/// ([`nudge_status`]). Call it before the runtime starts: later calls change nothing. A process that never
/// calls it (a runner, a command) doesn't nudge.
pub fn nudge(mode: crate::nudge::Mode) {
    NUDGE.store(mode.index(), Ordering::Relaxed);
}

/// The nudge asked for ([`nudge`]).
fn nudge_mode() -> crate::nudge::Mode {
    crate::nudge::Mode::of_index(NUDGE.load(Ordering::Relaxed))
}

/// The nudge's period: `KILN_RT_NUDGE_US` microseconds where it's set and above 0 (the lab's A/B of
/// periods, as `KILN_RT_QUANTUM_US` the quantum's), else the longest that keeps a shard woken behind a busy
/// thread within a step quantum of its CPU (goal 9, plan 01a1a87b2cec step 4: a wait's timeout derived from
/// the tick). The quantum is the host's share of a ready task's delay ([`STEP_BUDGET_NS`]: its CPU, where
/// another thread holds it, for about a quantum), and each of a nudged shard's waits ends within its period
/// and the timer slack the kernel adds to every timed wait, the timers' tick (hyper-rt's `Shard::build`), so
/// the period is the quantum less that tick, 166.7 µs at kiln's: an interrupt of the shard's own at least
/// every 266.7 µs while it waits, 3,750 a second, where the timers' tick as the period gave one every 200 µs,
/// 5,000 a second, for no bar it holds. Never shorter than that tick, the finest wait a shard keeps (a lab's
/// quantum under two ticks). The kernel's own tick bounds the same wait too, at a millisecond on every HZ
/// Linux offers (1000 at most), longer than any quantum the bar allows, so the nudge never leaves it to that.
fn nudge_period_ns(step_budget_ns: u64, timer_tick_ns: u64, env_us: Option<&str>) -> u64 {
    env_us
        .and_then(|us| us.trim().parse::<u64>().ok())
        .filter(|us| *us > 0)
        .map_or_else(|| step_budget_ns.saturating_sub(timer_tick_ns).max(timer_tick_ns), |us| us.saturating_mul(1000))
}

/// What a shard asks the OS for as it starts, past the real-time class (goal 3, plan 01a1a59c2a8e
/// step 1; goal 8, plan 01a1a59c2b29 step 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ShardAsks {
    /// A slice of its step (`hyper_rt::compute::prefer_current_thread`).
    slice: bool,
    /// A nudge (`hyper_rt::compute::nudge_current_thread`).
    nudge: bool,
}

/// What a shard asks for once it knows whether it took the real-time class: in it, nothing more (a
/// real-time thread has no slice of the normal class's to ask for, and asking would put it back in that
/// class; and its wake preempts at once, so it needs no nudge); at the normal class, a slice of its
/// step, and the nudge where `[server] nudge` asks for it, in the real-time class's place.
fn shard_asks(took_realtime: bool, nudge: bool) -> ShardAsks {
    if took_realtime {
        return ShardAsks { slice: false, nudge: false };
    }
    ShardAsks { slice: true, nudge }
}

/// How the shards took the nudge ([`nudge`]), for the log and `/healthz/server`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Nudge {
    /// Whether it was asked for (`[server] nudge` on or auto).
    pub asked: bool,
    /// How (`[server] nudge`): `on`, `auto` or `off`.
    pub mode: crate::nudge::Mode,
    /// Its period, microseconds.
    pub period_us: u64,
    /// The shards that answered it.
    pub shards: u32,
    /// Of them, those that nudge their CPUs: every wait where it's on; where it's auto, while their host holds
    /// their wakes.
    pub nudged: u32,
    /// Those that took the real-time class instead, whose wakes preempt at once.
    pub realtime: u32,
    /// Those refused it, which run as without it.
    pub refused: u32,
    /// Why those refused were.
    pub why: Option<String>,
    /// The shards nudging now (`crate::nudge`'s, as their probes left it): where it's auto, those whose host holds
    /// their wakes.
    pub armed: u32,
    /// The times shards began nudging on their own, all of them together (auto).
    pub starts: u64,
}

impl Nudge {
    /// The line it's said with once every shard has answered; `None` before, or not asked for.
    pub fn said(&self) -> Option<String> {
        let answered = self.nudged.saturating_add(self.realtime).saturating_add(self.refused);
        if !self.asked || self.shards == 0 || answered < self.shards {
            return None;
        }
        let nudged = match self.mode {
            crate::nudge::Mode::Auto => format!("nudge their CPUs while their host holds their wakes (auto: past {:.1} µs a wake on average, as each shard's probe reads the kernel's count of its wait for a CPU), each wait then ending within {} µs plus its timer slack: on a kernel that preempts lazily a shard woken behind a busy thread runs then, not at the kernel's next tick; a calm shard never nudges", crate::nudge::HELD_NS as f64 / 1e3, self.period_us),
            _ => format!("nudge their CPUs while they wait, each wait ending within {} µs plus its timer slack: on a kernel that preempts lazily a shard woken behind a busy thread runs then, not at the kernel's next tick", self.period_us),
        };
        Some(match (self.nudged, self.realtime, &self.why) {
            (n, 0, _) if self.refused == 0 => format!("the server's {n} shards {nudged}"),
            (0, r, _) if self.refused == 0 => format!("the server's {r} shards took the real-time class and need no nudge"),
            (n, r, _) if self.refused == 0 => format!("{n} of the server's {} shards {nudged}; {r} took the real-time class and need none", self.shards),
            (n, _, why) => format!("{n} of the server's {} shards nudge their CPUs, {} run without it: it was refused ({})", self.shards, self.refused, why.as_deref().unwrap_or("no reason given")),
        })
    }
}

/// How the shards took the nudge so far ([`nudge`]).
pub fn nudge_status() -> Nudge {
    nudge_of(NUDGE_ANSWERS.load(Ordering::Acquire))
}

/// [`Nudge`] from the shards' `answers` ([`NUDGE_ANSWERS`]).
fn nudge_of(answers: u32) -> Nudge {
    let mode = nudge_mode();
    Nudge {
        asked: mode != crate::nudge::Mode::Off,
        mode,
        period_us: NUDGE_PERIOD_NS.load(Ordering::Relaxed) / 1000,
        shards: NUDGE_SHARDS.load(Ordering::Relaxed),
        nudged: answers % NUDGE_REALTIME_ONE,
        realtime: (answers / NUDGE_REALTIME_ONE) % NUDGE_REALTIME_ONE,
        refused: answers / NUDGE_REFUSED_ONE,
        why: NUDGE_REFUSAL.get().map(nudge_why),
        armed: crate::nudge::armed_now(),
        starts: crate::nudge::starts(),
    }
}

/// Why the nudge was refused, for a person: the OS's error where it gave one, else what refused it
/// (an OS without lazy preemption says so).
fn nudge_why(refusal: &hyper_rt::compute::Refusal) -> String {
    match refusal.code {
        Some(_) => refusal.to_string(),
        None => refusal.call.to_owned(),
    }
}

/// A shard answers the nudge asked for ([`nudge`], `mode`): where it `nudges`, nudged at `period_ns` from here on
/// (`on`) or allowed to nudge at it while its host holds its wakes (`auto`, `crate::nudge::allow`); else counted
/// with those in the real-time class, the one reason a shard asked to nudge doesn't ([`shard_asks`]); or refused.
/// The last shard to answer says how they all did, once.
fn answer_nudge(mode: crate::nudge::Mode, nudges: bool, period_ns: u64) {
    let one = if !nudges {
        NUDGE_REALTIME_ONE
    } else {
        let asked = match mode {
            crate::nudge::Mode::Auto => crate::nudge::allow(period_ns),
            _ => hyper_rt::compute::nudge_current_thread(period_ns).inspect(|_| crate::nudge::always()),
        };
        match asked {
            Ok(()) => 1,
            Err(refusal) => {
                let _ = NUDGE_REFUSAL.set(refusal);
                NUDGE_REFUSED_ONE
            }
        }
    };
    let status = nudge_of(NUDGE_ANSWERS.fetch_add(one, Ordering::AcqRel).wrapping_add(one));
    if status.nudged.saturating_add(status.realtime).saturating_add(status.refused) == status.shards
        && let Some(line) = status.said()
    {
        if status.refused == 0 {
            tracing::info!("{line}");
        } else {
            tracing::warn!("{line}");
        }
    }
}

/// What each of the server's shards asks of its spin before a park (goal 9, plan 01a1a87b2cec step 4;
/// `hyper_rt::park_spin`; docs/priority.md, "A spin before the park"): `KILN_RT_SPIN_US` microseconds,
/// `fixed_us`, where it's set and above 0, at every idle moment whatever it costs (the lab's measure of what a
/// spin buys and costs); else, where `KILN_RT_SPIN` is on, `on`, the spin each shard's own gaps make cheapest
/// in CPU, up to 16 µs (`hyper_rt::park_spin::CAP_NS`), so it spends less of the shard's CPU than parking at
/// once wherever it spins; else none, as before. Off until asked: the lab's A/B decides whether it becomes a
/// default (docs/runtime-lab.md, "The shard's own wake path").
fn spin_ask(on: Option<&str>, fixed_us: Option<&str>) -> hyper_rt::park_spin::Ask {
    use hyper_rt::park_spin::{Ask, CAP_NS};
    if let Some(us) = fixed_us.and_then(|us| us.trim().parse::<u64>().ok()).filter(|us| *us > 0) {
        return Ask::Fixed { ns: us.saturating_mul(1000) };
    }
    match on.map(str::trim) {
        Some(v) if !v.is_empty() && !matches!(v, "0" | "false" | "off" | "no") => Ask::Sized { cap_ns: CAP_NS },
        _ => Ask::Off,
    }
}

/// The spin before a park the shards were asked for ([`spin_ask`]), once the runtime started.
static SPIN: OnceLock<hyper_rt::park_spin::Ask> = OnceLock::new();

/// How many shards answer the spin: the runtime's, where one was asked for.
static SPIN_SHARDS: AtomicU32 = AtomicU32::new(0);

/// The shards that took the spin, and those refused it (a task off its shard's thread: never, here), in one atomic
/// as the nudge's answers are, refusals in units of [`REFUSED_ONE`].
static SPIN_ANSWERS: AtomicU32 = AtomicU32::new(0);

/// How the shards took the spin before a park ([`spin_ask`]), for the log and `/healthz/server`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Spin {
    /// What was asked: `off`, `sized` (as long as each shard's own gaps make cheapest in CPU) or `fixed`.
    pub asked: &'static str,
    /// Its length, microseconds: the sized spin's cap, the fixed spin's time; 0 when off.
    pub us: u64,
    /// The shards that answered it.
    pub shards: u32,
    /// Of them, those that took it.
    pub took: u32,
}

impl Spin {
    /// The line it's said with once every shard has answered; `None` before, or not asked for.
    pub fn said(&self) -> Option<String> {
        if self.asked == "off" || self.shards == 0 || self.took < self.shards {
            return None;
        }
        Some(match self.asked {
            "sized" => format!("the server's {} shards spin before each park as long as their own gaps make cheapest in CPU, up to {} µs: a gap a spin catches needs no wake (KILN_RT_SPIN)", self.took, self.us),
            _ => format!("the server's {} shards spin {} µs before each park, whatever it costs (KILN_RT_SPIN_US, the lab's measure)", self.took, self.us),
        })
    }
}

/// How the shards took the spin so far ([`spin_ask`]).
pub fn spin_status() -> Spin {
    spin_of(SPIN.get().copied().unwrap_or_default(), SPIN_ANSWERS.load(Ordering::Acquire))
}

/// [`Spin`] from what was asked and the shards' `answers` ([`SPIN_ANSWERS`]).
fn spin_of(ask: hyper_rt::park_spin::Ask, answers: u32) -> Spin {
    use hyper_rt::park_spin::Ask;
    let (asked, ns) = match ask {
        Ask::Off => ("off", 0),
        Ask::Sized { cap_ns } => ("sized", cap_ns),
        Ask::Fixed { ns } => ("fixed", ns),
    };
    Spin { asked, us: ns / 1000, shards: SPIN_SHARDS.load(Ordering::Relaxed), took: answers % REFUSED_ONE }
}

/// A shard takes the spin asked for ([`spin_ask`]), on its own thread; the last to answer says how they all did, once.
fn answer_spin(ask: hyper_rt::park_spin::Ask) {
    let one = if hyper_rt::park_spin::ask_current_thread(ask).is_ok() { 1 } else { REFUSED_ONE };
    let answers = SPIN_ANSWERS.fetch_add(one, Ordering::AcqRel).wrapping_add(one);
    let status = spin_of(ask, answers);
    if status.took.saturating_add(answers / REFUSED_ONE) == status.shards {
        match status.said() {
            Some(line) => tracing::info!("{line}"),
            None => tracing::warn!(refused = answers / REFUSED_ONE, "a shard was refused its spin before a park: it parks at once"),
        }
    }
}

/// The core each of `shards` shards is fixed to, from cores of the process's own: the first kept
/// for its threads that aren't shards (its blocking pool, its timers' helpers), as hyper-rt's
/// placement keeps one, and each shard on the next, around again when there are more shards than
/// cores; with a single core, every shard on it.
fn shard_cores(cores: &[u32], shards: u16) -> Vec<u32> {
    let rest = cores.get(1..).filter(|r| !r.is_empty()).unwrap_or(cores);
    (0..usize::from(shards)).filter_map(|i| rest.get(i.checked_rem(rest.len())?).copied()).collect()
}

/// The cores this runtime's shards are fixed to from cores of the process's own, `cores`, as [`pin`]
/// fixes them, each once: what the server's own runner keeps its legs and agents off where the server's
/// slice holds it on the server's cores (`crate::os::keep_work_off`; goal 9, plan 01a1a87b2cec step 4).
pub fn shard_cores_for(cores: &[u32]) -> Vec<u32> {
    let mut shards = shard_cores(cores, profile().shards);
    shards.sort_unstable();
    shards.dedup();
    shards
}

/// The cores of the server's own as its shards took them, for `/healthz/server` (goal 9, plan
/// 01a1a87b2cec step 4): those of `[server] cores` it may run on, the shards' among them, and the
/// cpuset partition it runs in as the kernel says it (`crate::os::cgroup::partition`), so a person and
/// the lab see whether the partition a system install makes as the server starts took.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Cores {
    /// Its cores, as a CPU list.
    pub cores: String,
    /// Those of them its shards are fixed to.
    pub shards: String,
    /// The partition it runs in (`/kiln.slice: root`), or none.
    pub partition: Option<String>,
}

/// [`Cores`]; none where the server has no cores of its own ([`pin`] wasn't called).
pub fn cores_status() -> Option<Cores> {
    let cores = CORES.get()?;
    Some(Cores {
        cores: crate::os::cpu_list_text(cores),
        shards: crate::os::cpu_list_text(&shard_cores_for(cores)),
        partition: crate::os::cgroup::partition().map(|(path, state)| format!("{path}: {state}")),
    })
}


/// The shards' timer wheel's tick, nanoseconds ([`runtime_config`]): the finest wait a shard keeps, and its
/// thread's timer slack (hyper-rt's `Driver::set_timer_slack_ns`), so a sleep ends up to two ticks late.
pub const TIMER_TICK_NS: u64 = 100_000;

/// The shard delay goal 9 holds every shard's ready work to at p99.9, nanoseconds: a millisecond (Ada,
/// 2026-11-05; plan 01a1a87b2cec).
const DELAY_BAR_NS: u64 = 1_000_000;

/// A ready task's floor, nanoseconds: its timer falls due on the wheel's next tick and the OS wakes the
/// shard's thread within its timer slack, a tick again. The lab's calm shards ran their probes 192 µs late at
/// the median (g6c2 calm on rt5, 2026-11-05: 191 to 194 µs over six rounds), the floor every wake pays.
const DELAY_FLOOR_NS: u64 = 2 * TIMER_TICK_NS;

/// The busiest shard's busy share in the lab, parts in a million: shard 1's 0.065 in g6c2 calm on rt5 (the
/// shards' mean 0.052).
const LAB_BUSY_PPM: u64 = 65_000;

/// The polls a ready task finds in hand or ahead of it at p99.9, on a shard busy `busy_ppm` parts in a million
/// of the time: the most n whose chance, about ρ^n (a single server's queue, as what arrives finds it), is a
/// thousandth or more. A shard busy all the time has no such bound.
const fn polls_ahead_at_p999(busy_ppm: u64) -> u64 {
    if busy_ppm >= 1_000_000 {
        return u64::MAX;
    }
    // ρ^n, in parts in a million.
    let (mut n, mut chance_ppm) = (0u64, 1_000_000u64);
    while chance_ppm.saturating_mul(busy_ppm) / 1_000_000 >= 1_000 {
        chance_ppm = chance_ppm.saturating_mul(busy_ppm) / 1_000_000;
        n = n.saturating_add(1);
    }
    n
}

/// The shards' step quantum, nanoseconds, derived from the lab's measurements (goal 9, plan 01a1a87b2cec step 3;
/// docs/runtime-lab.md, "What moved (step 3)"): the longest a poll may hold its shard, so every shard runs its
/// ready work within `DELAY_BAR_NS` at p99.9. A ready task waits its floor (`DELAY_FLOOR_NS`, 200 µs); then
/// the polls it finds in hand or ahead of it, two at p99.9 on the lab's busiest shard (`polls_ahead_at_p999`:
/// ρ² is 0.42%, ρ³ 0.027%), each a quantum at most where no poll holds a shard past it (step 3's done-when); then
/// its CPU, where another thread holds it, for about a quantum again: the slice the shard asks the OS for is its
/// quantum (`hyper_rt::compute::prefer_current_thread`; beside four pool threads on one core a shard's p99 wake
/// took 0.21 ms with a 250 µs slice). The quantum is the largest that keeps the floor and three quanta within
/// the bar, (1 ms − 200 µs) / 3, and it holds ordinary work's long polls (0.246 ms on average at the most, of a
/// site step 2 didn't list: `reads.rs:338`), so ordinary work isn't cut finer than its own polls. With 250 µs the
/// lab's two shards ran 12.1 polls a second past twice it (g6c2 calm on rt5): at 6 a shard and 500 µs each, a
/// shard was held so 0.3% of the time or more, past the 0.1% its p99.9 can spare, so its p99.9 was 1295 µs. Under
/// lazy preemption (fedora's) the host's wait is the CPU's next tick instead, up to 1 ms (docs/priority.md,
/// wake-2): step 4's to keep off (cores of their own), not the quantum's. `KILN_RT_QUANTUM_US` sets another,
/// for the lab's A/B of quanta.
pub const STEP_BUDGET_NS: u64 = DELAY_BAR_NS.saturating_sub(DELAY_FLOOR_NS) / polls_ahead_at_p999(LAB_BUSY_PPM).saturating_add(1);

/// The most tasks a shard's step polls (hyper-rt's `RuntimeConfig::batch`), as kiln's shards have run since hyper-rt
/// came in: hyper-rt's watchdog holds a poll to a step's worth of polls each a quantum, this many times
/// [`STEP_BUDGET_NS`] (`hyper_rt::watch`'s limit, 17.07 ms), and the shards' health probe wakes that often
/// (`crate::shard_health::PERIOD`; goal 9, plan 01a1a87b2cec step 5).
pub const STEP_BATCH: usize = 64;

/// hyper-rt configuration (see the sizing rules in hyper-rt's docs): timers at twice the tasks so the
/// wheel never runs dry, the step quantum derived from the lab ([`STEP_BUDGET_NS`]), no spin.
pub fn runtime_config(shards: u16, tasks: usize) -> RuntimeConfig {
    RuntimeConfig {
        shards,
        tasks_per_shard: tasks,
        timers_per_shard: tasks.saturating_mul(2),
        interests_per_shard: hyper_rt::runtime::interests_for(tasks),
        ring_entries: 1024,
        step_budget_ns: STEP_BUDGET_NS,
        timer_tick_ns: TIMER_TICK_NS,
        batch: STEP_BATCH,
        pin: false,
        cores: Vec::new(),
        page_bytes: 4096,
        spin_ns: 0,
        wake_tracking: None,
        latency_objective_ns: None,
    }
}

/// The blocking pool's shares.
pub const SHARE_FS: &str = "fs";
pub const SHARE_STORE: &str = "store";

struct Rt {
    /// Held for the life of the process: dropping it would stop the shards.
    runtime: Runtime,
    holders: Vec<SlotHolder>,
    next: AtomicUsize,
}

static RT: OnceLock<Result<Rt, String>> = OnceLock::new();

fn start() -> Result<Rt, String> {
    let p = profile();
    let (dns, store, fs) = p.pool;
    // Each job's wait and run, by share, on /metrics: what the shares' sizes are derived from (goal 3, plan
    // 01a1a59c2a8e step 4; `crate::pool_health`).
    let _ = hyper_rt::blocking::observe(crate::pool_health::blocking_job);
    let pool = hyper_rt::blocking::Config {
        workers: dns + store + fs,
        shares: vec![
            hyper_rt::blocking::Share { name: hyper_rt::dns::SHARE, limit: dns, waiting: 256 },
            hyper_rt::blocking::Share { name: SHARE_STORE, limit: store, waiting: 16_384 },
            hyper_rt::blocking::Share { name: SHARE_FS, limit: fs, waiting: 8_192 },
        ],
    };
    match hyper_rt::blocking::start(pool) {
        Ok(()) | Err(RtError::BadConfig { .. }) => {}
        Err(e) => return Err(format!("blocking pool: {e}")),
    }
    let mut config = runtime_config(p.shards, p.tasks_per_shard);
    // The step quantum from `KILN_RT_QUANTUM_US`: the lab's A/B of quanta on one build (goal 9, plan
    // 01a1a87b2cec step 3).
    if let Some(us) = std::env::var("KILN_RT_QUANTUM_US").ok().and_then(|v| v.trim().parse::<u64>().ok()).filter(|us| *us > 0) {
        config.step_budget_ns = us.saturating_mul(1000);
    }
    if let Some(cores) = CORES.get() {
        config.cores = shard_cores(cores, p.shards);
        config.pin = !config.cores.is_empty();
    }
    start_compute(&p, &config);
    let runtime = Runtime::start(&config).map_err(|e| format!("runtime: {e}"))?;
    let holders: Vec<SlotHolder> = runtime.shard_ids().iter().filter_map(|id| runtime.holder_of(*id)).collect();
    if holders.is_empty() {
        return Err("runtime started without shards".to_owned());
    }
    // The shards' own arenas, where they're to have them (goal 9, plan 01a1a87b2cec step 3; `crate::os::alloc`):
    // made before any shard's first task, in which each takes its own.
    crate::os::alloc::make_shard_arenas(holders.len());
    if let Some(said) = crate::os::alloc::shard_arenas_said() {
        if p.announce {
            tracing::info!("{said}");
        } else {
            tracing::debug!("{said}");
        }
    }
    // Each shard asks for a slice of its step (goal 3, plan 01a1a59c2a8e step 1): waking on a CPU a compute
    // pool thread holds, it takes the CPU at once (`hyper_rt::compute::prefer_current_thread`; Linux). Where
    // the real-time class is asked for and granted ([`realtime`]), it takes that instead ([`shard_asks`]).
    // Where the nudge is asked for ([`nudge`]), a shard at the normal class nudges its CPU too, in the
    // real-time class's place (goal 8, plan 01a1a59c2b29 step 7).
    let realtime = REALTIME.load(Ordering::Relaxed);
    let shards = u32::try_from(holders.len()).unwrap_or(u32::MAX);
    if realtime {
        REALTIME_SHARDS.store(shards, Ordering::Relaxed);
    }
    let mode = nudge_mode();
    let nudge = mode != crate::nudge::Mode::Off;
    let period_ns = nudge_period_ns(config.step_budget_ns, config.timer_tick_ns, std::env::var("KILN_RT_NUDGE_US").ok().as_deref());
    if nudge {
        NUDGE_PERIOD_NS.store(period_ns, Ordering::Relaxed);
        NUDGE_SHARDS.store(shards, Ordering::Relaxed);
    }
    // The spin before each park, where the lab asks for one (goal 9, plan 01a1a87b2cec step 4; [`spin_ask`]).
    let spin = spin_ask(std::env::var("KILN_RT_SPIN").ok().as_deref(), std::env::var("KILN_RT_SPIN_US").ok().as_deref());
    let _ = SPIN.set(spin);
    if spin != hyper_rt::park_spin::Ask::Off {
        SPIN_SHARDS.store(shards, Ordering::Relaxed);
    }
    for (shard, holder) in holders.iter().enumerate() {
        let slice = config.step_budget_ns;
        let _ = hyper_rt::runtime::submit_to_holder(*holder, async move {
            // Its own arena, where the shards have them (goal 9, plan 01a1a87b2cec step 3; `crate::os::alloc`).
            let _ = crate::os::alloc::shard_started(shard);
            // Its stack takes no huge pages (goal 11, plan 01a1a87b2e56 step 5; `crate::os::stack`): a shard reaches
            // 0.40 to 0.75 MB deep (the lab's smaps), which where every anonymous mapping takes them would be 2 MB.
            let _ = crate::os::stack::no_huge_pages();
            let took = realtime && took_realtime();
            let asks = shard_asks(took, nudge);
            if asks.slice
                && let Err(refusal) = hyper_rt::compute::prefer_current_thread(slice)
            {
                tracing::debug!(%refusal, "a shard kept the OS's slice");
            }
            if nudge {
                answer_nudge(mode, asks.nudge, period_ns);
            }
            if spin != hyper_rt::park_spin::Ask::Off {
                answer_spin(spin);
            }
        });
    }
    if SHARDS_APART.load(Ordering::Relaxed) {
        keep_apart(&holders);
    }
    tracing::debug!(shards = p.shards, tasks = p.tasks_per_shard, "hyper-rt runtime started");
    Ok(Rt { runtime, holders, next: AtomicUsize::new(0) })
}

/// Keeps the process's other threads off its shards' cores for as long as it runs ([`pin`]; goal 9,
/// plan 01a1a87b2cec step 4): a thread starts with the CPUs of the thread that started it, and the
/// server starts most of its threads from tasks on its shards, each fixed to its core. Each shard
/// tells its thread's kernel id; then a thread of its own, once each period of the shards' probes
/// (`crate::shard_health::PERIOD`, so a thread a shard started shares its core for at most a probe's
/// period), moves each thread it hasn't seen yet that may run on a shard's core
/// (`crate::os::keep_off_shards`). A child process a shard starts is moved as it starts
/// (`crate::os::keep_child_off_shards`).
fn keep_apart(holders: &[SlotHolder]) {
    let tids: Arc<Vec<std::sync::atomic::AtomicU32>> = Arc::new(holders.iter().map(|_| std::sync::atomic::AtomicU32::new(0)).collect());
    for (i, holder) in holders.iter().enumerate() {
        let tids = Arc::clone(&tids);
        let _ = hyper_rt::runtime::submit_to_holder(*holder, async move {
            if let (Some(slot), Some(id)) = (tids.get(i), crate::os::thread_id()) {
                slot.store(id, Ordering::Release);
            }
        });
    }
    let spawned = std::thread::Builder::new().name("kiln-apart".into()).spawn(move || {
        let mut seen = std::collections::HashSet::new();
        loop {
            std::thread::sleep(crate::shard_health::PERIOD);
            let shards: Vec<u32> = tids.iter().map(|t| t.load(Ordering::Acquire)).collect();
            // Not until every shard has said which thread is its own.
            if shards.contains(&0) {
                continue;
            }
            let moved = crate::os::keep_off_shards(&shards, &mut seen);
            if moved > 0 {
                tracing::debug!(moved, "threads moved off the shards' cores");
            }
        }
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "the server's other threads may share its shards' cores: no thread to keep them off");
    }
}

/// The compute pool (goal 3 of the API runtime program, plan 01a1a59c2a8e step 1; `hyper_rt::compute`): as
/// many threads as the CPU the process is granted (its cgroup's quota or its cpuset, else the OS's count)
/// less its shards, at a lower OS priority than the shards that still has its share of the CPU against
/// other work in the process's cgroup, as requests wait on its jobs (`Rank::Below`); a piece of a job as
/// long as a shard's step, so a long job gives its thread to a waiting one as a task gives its shard; room
/// for a job for every task the shards can hold, each of which awaits its own. Said with how its size was
/// derived.
fn start_compute(p: &Profile, config: &RuntimeConfig) {
    if hyper_rt::compute::running() {
        return;
    }
    let grant = hyper_rt::compute::Grant::of_process();
    let shards = usize::from(p.shards);
    // Shards on cores of their own aren't among the cores the pool's threads may take ([`pin`]).
    let workers = hyper_rt::compute::workers_for(&grant, if SHARDS_APART.load(Ordering::Relaxed) { 0 } else { shards });
    let pool = hyper_rt::compute::Config { workers, queued: shards.saturating_mul(p.tasks_per_shard), piece_ns: config.step_budget_ns, rank: hyper_rt::compute::Rank::Below };
    // Each job's wait for a thread and each piece's time, on /metrics (step 3; `crate::pool_health`).
    let _ = hyper_rt::compute::observe(crate::pool_health::compute_job);
    match hyper_rt::compute::start(pool) {
        Ok(started) => {
            let priority = match started.priority {
                Ok(priority) => priority.to_string(),
                Err(refusal) => format!("the process's own ({refusal})"),
            };
            let size = if SHARDS_APART.load(Ordering::Relaxed) {
                format!("{} threads: {grant}, its shards on cores of their own", started.workers)
            } else {
                format!("{} threads: {grant}, less {shards} shards", started.workers)
            };
            if p.announce {
                tracing::info!(size = %size, priority = %priority, lowered = started.lowered, "compute pool started");
            } else {
                tracing::debug!(size = %size, priority = %priority, lowered = started.lowered, "compute pool started");
            }
        }
        Err(e) => tracing::warn!(error = %e, "the compute pool didn't start: CPU-heavy jobs run on threads of their own"),
    }
}

fn rt() -> anyhow::Result<&'static Rt> {
    RT.get_or_init(start).as_ref().map_err(|e| anyhow::anyhow!("hyper-rt failed to start: {e}"))
}

/// The runtime's shards' registrations, in order (it's started if it hasn't been): what work is placed on
/// and what kiln-rt's watchdog reports by (`crate::overload`).
pub fn shard_holders() -> Vec<SlotHolder> {
    rt().map_or_else(|_| Vec::new(), |rt| rt.holders.clone())
}

/// [`shard_holders`], borrowed: empty when the runtime couldn't start.
pub(crate) fn shard_holders_ref() -> &'static [SlotHolder] {
    rt().map_or(&[], |rt| rt.holders.as_slice())
}

/// What the runtime's movable tasks did so far (goal 7, `crate::placement`); `None` when it couldn't start.
pub fn movable_stats() -> Option<hyper_rt::movable::Stats> {
    rt().ok().map(|rt| rt.runtime.movable_stats())
}

/// Whether the caller runs on a hyper-rt shard (inside a task).
pub fn on_shard() -> bool {
    hyper_rt::futures::shard_id().is_some()
}

/// The place among the runtime's shards ([`shard_count`], as [`spawn_on`] and [`spawn_owner`] take it) of the shard the
/// caller runs on; `None` off them (goal 10, plan 01a1a87b2dae step 5).
pub fn shard_index() -> Option<usize> {
    let here = hyper_rt::futures::shard_id()?.0;
    shard_holders_ref().iter().position(|h| h.shard() == here)
}

// ------------------------------------------------------------------------------------ spawning

pub(crate) type Boxed = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// A future handed to the runtime through a slot, so a refused submission (full control queue,
/// full arena) leaves it in place for the next attempt instead of dropping it.
pub(crate) fn handoff(fut: Boxed) -> Arc<Mutex<Option<Boxed>>> {
    Arc::new(Mutex::new(Some(fut)))
}

pub(crate) fn bootstrap(slot: &Arc<Mutex<Option<Boxed>>>) -> impl Future<Output = ()> + Send + 'static {
    let slot = Arc::clone(slot);
    async move {
        let fut = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(fut) = fut {
            fut.await;
        }
    }
}

#[track_caller]
fn submit(slot: &Arc<Mutex<Option<Boxed>>>) -> bool {
    // On a shard: keep the task here (no cross-thread hop; its sockets and timers stay local).
    if on_shard() && hyper_rt::futures::spawn_detached(bootstrap(slot)).is_ok() {
        return true;
    }
    submit_spread(slot)
}

/// Round-robin over the shards, as work that may move (`hyper_rt::runtime::submit_movable`): a shard the
/// watchdog sees held in one poll past its limit (`hyper_rt::watch::stalled`) is passed over while any
/// other takes the task, and a task waiting for a shard that becomes held is handed to another by the
/// watchdog (kiln's goal 6, plan 01a1a59c2abe step 2). Admitted tasks stay where they are.
#[track_caller]
fn submit_spread(slot: &Arc<Mutex<Option<Boxed>>>) -> bool {
    submit_spread_as(slot, hyper_rt::coop::Class::Normal)
}

/// [`submit_spread`], the task starting in `class` (`hyper_rt::coop::Class`).
#[track_caller]
fn submit_spread_as(slot: &Arc<Mutex<Option<Boxed>>>, class: hyper_rt::coop::Class) -> bool {
    let Ok(rt) = rt() else { return false };
    let n = rt.holders.len();
    let start = rt.next.fetch_add(1, Ordering::Relaxed);
    // Shards not held first; only when every one is held do the held ones take it.
    for held in [false, true] {
        for i in 0..n {
            if let Some(holder) = rt.holders.get(start.wrapping_add(i) % n)
                && hyper_rt::watch::stalled(*holder) == held
                && hyper_rt::runtime::submit_movable_as(*holder, class, bootstrap(slot)).is_ok()
            {
                return true;
            }
        }
    }
    false
}

/// Whether the runtime took the task (refused on every shard: dropped, and said so).
#[track_caller]
fn spawn_boxed(fut: Boxed) -> bool {
    let slot = handoff(fut);
    if submit(&slot) {
        return true;
    }
    if !on_shard() {
        // Off the runtime (startup, tests): wait out a burst that filled the control queues.
        for _ in 0..2000 {
            std::thread::sleep(Duration::from_millis(2));
            if submit(&slot) {
                return true;
            }
        }
    }
    crate::metrics::global().panics_caught.inc();
    tracing::error!("hyper-rt refused a task on every shard (arenas or control queues full); task dropped");
    false
}

/// Wait for a spawned task's output. Dropping the handle detaches the task (it keeps running).
pub struct JoinHandle<T> {
    rx: futures::channel::oneshot::Receiver<T>,
    abort: futures::future::AbortHandle,
}

impl<T> JoinHandle<T> {
    /// Stop the task at its next poll (its future is dropped).
    pub fn abort(&self) {
        self.abort.abort();
    }
}

impl<T> Future for JoinHandle<T> {
    /// `Err` when the task was aborted, panicked, or could not be started.
    type Output = Result<T, futures::channel::oneshot::Canceled>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.rx).poll(cx)
    }
}

/// Run `fut` as its own task: on the current shard when called from one, otherwise on the next
/// shard round-robin. A panic ends only that task (hyper-rt catches it; the handle reports `Err`).
/// The caller's place is the task's spawn site, which the watchdog names should it hold its shard.
#[track_caller]
pub fn spawn<F>(fut: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let (tx, rx) = futures::channel::oneshot::channel();
    let (fut, abort) = futures::future::abortable(fut);
    let _ = spawn_boxed(Box::pin(async move {
        if let Ok(v) = fut.await {
            let _ = tx.send(v);
        }
    }));
    JoinHandle { rx, abort }
}

/// The runtime as kiln-seams' [`kiln_seams::Spawn`]: a task it can't take is refused, typed.
#[derive(Debug, Clone, Copy, Default)]
pub struct RtSpawn;

impl kiln_seams::Spawn for RtSpawn {
    fn spawn_boxed(&self, task: futures::future::BoxFuture<'static, ()>) -> Result<(), kiln_seams::spawn::Refused> {
        if spawn_boxed(task) {
            return Ok(());
        }
        let p = PROFILE.get().copied().unwrap_or_else(Profile::tool);
        Err(kiln_seams::spawn::Refused::Full { limit: usize::from(p.shards).saturating_mul(p.tasks_per_shard) })
    }
}

/// Run `fut` on the next shard round-robin wherever the caller is: spreads accepted connections
/// over every shard, passing over one the watchdog sees held ([`submit_spread`]). Falls back to the
/// current shard when the others refuse.
#[track_caller]
pub fn spawn_spread<F: Future<Output = ()> + Send + 'static>(fut: F) {
    let slot = handoff(Box::pin(fut));
    if submit_spread(&slot) || submit(&slot) {
        return;
    }
    tracing::error!("hyper-rt refused a connection task on every shard; connection dropped");
}

/// [`spawn`], placed as [`spawn_spread`] places a task: on the next shard round-robin wherever the
/// caller is, passing over one the watchdog sees held. For a service the process starts and keeps (a
/// supervised child, a feed's loop): started from a task on one shard, as a server's are by its
/// start-up on the shard that runs it, [`spawn`] would keep every one of them on that shard (goal 9,
/// plan 01a1a87b2cec step 3: the lab's calm baseline found the server's shard 0 busy 0.24 of the time
/// against 0.016 on each of the others, and its delay p99.9 94 ms against about 1 ms).
#[track_caller]
pub fn spawn_spread_joined<F>(fut: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let (tx, rx) = futures::channel::oneshot::channel();
    let (fut, abort) = futures::future::abortable(fut);
    let slot = handoff(Box::pin(async move {
        if let Ok(v) = fut.await {
            let _ = tx.send(v);
        }
    }));
    let mut placed = submit_spread(&slot) || submit(&slot);
    // Off the runtime (start-up, tests), a burst that filled the control queues is waited out, as
    // `spawn_boxed` waits it out (a loop, not a closure, so the caller's place stays the spawn site).
    if !placed && !on_shard() {
        for _ in 0..2000 {
            std::thread::sleep(Duration::from_millis(2));
            if submit_spread(&slot) {
                placed = true;
                break;
            }
        }
    }
    if !placed {
        crate::metrics::global().panics_caught.inc();
        tracing::error!("hyper-rt refused a task on every shard (arenas or control queues full); task dropped");
    }
    JoinHandle { rx, abort }
}

/// Run an accepted connection on the next shard round-robin, as [`spawn_spread`] does, or, where the
/// placement steers (goal 7, `crate::placement`), on the less delayed of two shards drawn at random; its
/// task starting Critical: its first request's head must be read before the request's class is known, and
/// reading a head costs little (`crate::overload::Connection`, goal 6, plan 01a1a59c2abe step 3).
#[track_caller]
pub fn spawn_connection<F: Future<Output = ()> + Send + 'static>(fut: F) {
    let slot = handoff(Box::pin(fut));
    let critical = hyper_rt::coop::Class::Critical;
    let steered = crate::placement::get().steers()
        && crate::placement::connection_shard().is_some_and(|holder| hyper_rt::runtime::submit_movable_as(holder, critical, bootstrap(&slot)).is_ok());
    if steered || submit_spread_as(&slot, critical) || submit(&slot) {
        return;
    }
    tracing::error!("hyper-rt refused a connection task on every shard; connection dropped");
}

/// The runtime's shards (it's started if it hasn't been): 0 when it couldn't start.
pub fn shard_count() -> usize {
    rt().map_or(0, |rt| rt.holders.len())
}

/// Run `fut` on shard `i` (of [`shard_count`]) and no other, for a task whose work is that shard's
/// own: a probe of its health (`crate::shard_health`), or a test holding one shard and not the
/// rest. Whether the shard took it. The caller's place is the task's spawn site.
#[track_caller]
pub fn spawn_on<F: Future<Output = ()> + Send + 'static>(i: usize, fut: F) -> bool {
    let Ok(rt) = rt() else { return false };
    let Some(holder) = rt.holders.get(i) else { return false };
    hyper_rt::runtime::submit_to_holder(*holder, fut).is_ok()
}

/// Run `fut` as its own task on shard `shard` (of [`shard_count`]) for its whole life, wherever the
/// caller is: a task that owns what belongs to that shard (each agent session's copy,
/// `crate::harness::copies`). Refused there (its arena or control queue full), it waits as
/// [`spawn`] does off the runtime, then runs on whichever shard takes it, said, rather than not at
/// all, where [`spawn_on`] gives up. Goal 4, plan 01a1a59c2a9c step 2. The caller's place is the
/// task's spawn site.
#[track_caller]
pub fn spawn_owner<F: Future<Output = ()> + Send + 'static>(shard: usize, fut: F) {
    let slot = handoff(Box::pin(fut));
    if owner_takes(shard, &slot) {
        return;
    }
    if !on_shard() {
        for _ in 0..2000 {
            std::thread::sleep(Duration::from_millis(2));
            if owner_takes(shard, &slot) {
                return;
            }
        }
    }
    tracing::warn!(shard, "a shard refused a task that belongs on it: it runs on another shard");
    if submit(&slot) {
        return;
    }
    crate::metrics::global().panics_caught.inc();
    tracing::error!("hyper-rt refused a task on every shard (arenas or control queues full); task dropped");
}

/// Whether shard `shard` (of [`shard_count`], around again past the last) took the task in `slot`.
#[track_caller]
fn owner_takes(shard: usize, slot: &Arc<Mutex<Option<Boxed>>>) -> bool {
    let Some(holder) = rt().ok().and_then(|rt| rt.holders.get(shard.checked_rem(rt.holders.len())?).copied()) else { return false };
    hyper_rt::runtime::submit_to_holder(holder, bootstrap(slot)).is_ok()
}

/// `fut`, each of its polls labeled `label` for the shards' table of slow polls: should a poll of it hold
/// its shard past 100 µs, the shard counts it against `label` as well as its task's spawn site
/// (`hyper_rt::slow`; goal 9, plan 01a1a87b2cec step 2). A request's route, say
/// (`crate::route_latency::label_of`).
pub async fn labeled<F: Future>(label: &'static hyper_rt::slow::Label, fut: F) -> F::Output {
    let mut fut = pin!(fut);
    std::future::poll_fn(move |cx| {
        hyper_rt::futures::label(label);
        fut.as_mut().poll(cx)
    })
    .await
}

/// Run a `!Send` future as a task on the current shard. Refused off the runtime.
#[track_caller]
pub fn spawn_local<F: Future<Output = ()> + 'static>(fut: F) -> Result<TaskId, RtError> {
    hyper_rt::futures::spawn_detached(fut)
}

/// Run `fut` to completion on the runtime, blocking the calling (non-runtime) thread: `main` and
/// tests. A panic inside it becomes an error.
pub fn block_on<T, F>(fut: F) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>> + Send + 'static,
    T: Send + 'static,
{
    anyhow::ensure!(!on_shard(), "block_on called from inside the runtime");
    rt()?;
    let (tx, mut rx) = hyper_rt::sync::oneshot::<anyhow::Result<T>>().map_err(|e| anyhow::anyhow!("oneshot: {e}"))?;
    let _ = spawn_boxed(Box::pin(async move {
        // A panic says what it was (a failed assertion, in a test), rather than leaving it lost on
        // the thread that ran it.
        let out = match std::panic::AssertUnwindSafe(fut).catch_unwind().await {
            Ok(r) => r,
            Err(p) => Err(anyhow::anyhow!("the task panicked: {}", crate::sup::panic_message(p.as_ref()))),
        };
        let _ = tx.send(out);
    }));
    match rx.blocking_recv() {
        Ok(r) => r,
        Err(_) => anyhow::bail!("the task ended without a result (it could not start)"),
    }
}

// ------------------------------------------------------------------------------- blocking pool

/// Run `f` on the blocking pool's `fs` share: files, `/proc`, archives, anything that blocks. Work that
/// only computes goes to [`compute`].
pub async fn blocking<R, F>(f: F) -> anyhow::Result<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    blocking_in(SHARE_FS, f).await
}

// --------------------------------------------------------------------------------- compute pool

pub use hyper_rt::compute::{Step, Turn};

/// Run `f` on the compute pool: CPU-heavy work (building snapshots, serializing big payloads,
/// compressing, diffing, hashing), off every shard and below them in the OS's priority, so one heavy
/// request holds up no other (goal 3, plan 01a1a59c2a8e; `hyper_rt::compute`). Its result comes back
/// here. Waits while the pool is at its bound; with no pool running, `f` gets a thread of its own. A
/// job that can run long goes in pieces: [`compute_pieces`].
pub async fn compute<R, F>(f: F) -> anyhow::Result<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    rt()?;
    let (tx, rx) = futures::channel::oneshot::channel::<(R, u64)>();
    // Handed over from a shard: its round trip, past the job's own run, is what work kept on a shard is weighed against
    // ([`POOL_TRIP`], [`here`]; consts-audit B1).
    let handed = on_shard().then(Instant::now);
    let job = move || {
        let began = Instant::now();
        let out = f();
        let _ = tx.send((out, nanos_since(began)));
    };
    to_compute(job, |job| hyper_rt::compute::run(job).map(drop), |job| job()).await?;
    let (out, ran_ns) = rx.await.map_err(|_| anyhow::anyhow!("compute job failed (it panicked)"))?;
    if let Some(handed) = handed {
        POOL_TRIP.learn(nanos_since(handed).saturating_sub(ran_ns));
    }
    Ok(out)
}

/// Whole nanoseconds since `t`.
fn nanos_since(t: Instant) -> u64 {
    u64::try_from(t.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// A sample smoothed as RFC 6298 (§2) smooths a round trip, nanoseconds: its mean (a new sample's weight 1/8) and its
/// variation (1/4); the first sample sets the mean to itself and the variation to half of it (§2.2). Many writers, each a
/// load and a store, so a race between two loses one sample at the most, and no lock is taken: the compute pool's round
/// trip ([`POOL_TRIP`]), every session copy's quiet gaps (`crate::harness::copies`).
pub struct Smoothed {
    mean: AtomicU64,
    dev: AtomicU64,
    learned: AtomicBool,
}

impl Smoothed {
    /// A [`Smoothed`] with nothing learned.
    pub const fn nothing_yet() -> Self {
        Self { mean: AtomicU64::new(0), dev: AtomicU64::new(0), learned: AtomicBool::new(false) }
    }

    /// Its mean, nanoseconds: none until a sample was learned.
    pub fn mean(&self) -> Option<u64> {
        self.learned.load(Ordering::Relaxed).then(|| self.mean.load(Ordering::Relaxed))
    }

    /// Its variation, nanoseconds: 0 until a sample was learned.
    pub fn dev(&self) -> u64 {
        self.dev.load(Ordering::Relaxed)
    }

    /// Its mean and four times its variation, as RFC 6298 (§2.3, K = 4) bounds a round trip: none until a sample was
    /// learned.
    pub fn bound(&self) -> Option<u64> {
        self.mean().map(|mean| mean.saturating_add(self.dev().saturating_mul(4)))
    }

    /// A sample of `took` nanoseconds. As RFC 6298 (§2.3) takes a round trip: its variation first, against the mean before
    /// it, x ← x + (|sample − mean| − x)/4, then its mean, x ← x + (sample − x)/8.
    pub fn learn(&self, took: u64) {
        let (mean, dev) = if self.learned.load(Ordering::Relaxed) {
            let (mean, dev) = (self.mean.load(Ordering::Relaxed), self.dev.load(Ordering::Relaxed));
            ((mean - mean / 8).saturating_add(took / 8), (dev - dev / 4).saturating_add(took.abs_diff(mean) / 4))
        } else {
            (took, took / 2)
        };
        self.mean.store(mean, Ordering::Relaxed);
        self.dev.store(dev, Ordering::Relaxed);
        self.learned.store(true, Ordering::Relaxed);
    }
}

/// What handing a job to the compute pool and taking its result back costs a shard's task, past the job's own run
/// ([`compute`]): its wait for a worker, the worker's wake, the result's way back and the task's next turn on its shard,
/// smoothed ([`Smoothed`]). Learned from every job a task on a shard hands the pool; what [`here`] weighs work kept on a
/// shard against (consts-audit B1: that was a slow poll, `hyper_rt::slow::SLOW_POLL_NS`'s 100 µs, whatever the pool's
/// round trip was).
pub static POOL_TRIP: Smoothed = Smoothed::nothing_yet();

/// The two sides work may be done on, as where it's asked weighs them ([`Sides::take_here`]; consts-audit B1, goal 9, plan
/// 01a1a87b2cec step 3): the shard's quantum, the most one poll may hold it (none off a shard, where work is done where it
/// is), and the compute pool's round trip as the shards learned it ([`POOL_TRIP`]; none until learned).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sides {
    pub quantum_ns: Option<u64>,
    pub trip_ns: Option<u64>,
}

impl Sides {
    /// Where it's asked: the calling shard's quantum (`hyper_rt::futures::step_budget_ns`) and the pool's learned round trip.
    pub fn here() -> Self {
        Self { quantum_ns: hyper_rt::futures::step_budget_ns(), trip_ns: POOL_TRIP.mean() }
    }

    /// Whether work expected to take `expected_ns` where it's asked, `bound_ns` at the most, is done there rather than
    /// handed to the compute pool. Off a shard, always. On one, only where it fits a turn, its bound within the quantum (no
    /// poll past it: goal 9 step 3), and within that where it's no dearer than the pool's round trip: the cheaper side does
    /// it. Work past the quantum goes to the pool however long the pool's round trip (or its caller cuts it in pieces), so
    /// a loaded pool keeps no long poll on a shard. Work expected to cost nothing (nothing learned of it yet, which its
    /// caller learns as it does it) is done where it's asked; with nothing learned of the pool, other work goes there,
    /// whose first job teaches it. Whether it fits what's left of the turn is the caller's [`Turns`], which gives the shard
    /// up first where it doesn't.
    pub fn take_here(&self, expected_ns: u64, bound_ns: u64) -> bool {
        let Some(quantum) = self.quantum_ns else { return true };
        bound_ns <= quantum && (expected_ns == 0 || self.trip_ns.is_some_and(|trip| expected_ns <= trip))
    }
}

/// [`Sides::take_here`] where it's asked ([`Sides::here`]).
pub fn here(expected_ns: u64, bound_ns: u64) -> bool {
    Sides::here().take_here(expected_ns, bound_ns)
}

/// Run a job in pieces on the compute pool: `step` is called with `state` and its piece's [`Turn`]
/// until it returns [`Step::Done`]. Between units of its work it asks [`Turn::over`], and returns
/// [`Step::More`] when told yes: its piece has had a shard's step and another job waits for the thread,
/// which it then gives up, going on after (big JSON written a part at a time, a long diff a file at a
/// time). Otherwise as [`compute`].
pub async fn compute_pieces<S, T, F>(state: S, mut step: F) -> anyhow::Result<T>
where
    S: Send + 'static,
    T: Send + 'static,
    F: FnMut(&mut S, &Turn<'_>) -> Step<T> + Send + 'static,
{
    rt()?;
    let (tx, rx) = futures::channel::oneshot::channel::<T>();
    let piece = move |(state, tx): &mut (S, Option<futures::channel::oneshot::Sender<T>>), turn: &Turn<'_>| match step(state, turn) {
        Step::Done(value) => {
            if let Some(tx) = tx.take() {
                let _ = tx.send(value);
            }
            Step::Done(())
        }
        Step::More => Step::More,
    };
    to_compute(((state, Some(tx)), piece), |(state, piece)| hyper_rt::compute::run_pieces(state, piece).map(drop), |(state, piece)| hyper_rt::compute::run_here(state, piece)).await?;
    rx.await.map_err(|_| anyhow::anyhow!("compute job failed (it panicked)"))
}

/// `f` of each item on the compute pool, in pieces ([`compute_pieces`]): an item a unit, its piece
/// ended where another job waits, so a long list holds no pool thread long. The outputs in the items'
/// order.
pub async fn compute_map<T, R, F>(items: Vec<T>, f: F) -> anyhow::Result<Vec<R>>
where
    T: Send + 'static,
    R: Send + 'static,
    F: FnMut(T) -> R + Send + 'static,
{
    let out = Vec::with_capacity(items.len());
    compute_pieces((items.into_iter(), out, f), |(todo, out, f), turn| {
        for item in todo.by_ref() {
            out.push(f(item));
            if turn.over() {
                return Step::More;
            }
        }
        Step::Done(std::mem::take(out))
    })
    .await
}

/// [`compute_pieces`] for a thread of its own, which may block (never a shard): the job runs on the compute
/// pool, in pieces, and this thread waits for its result. At the pool's bound it waits and asks again, as
/// [`to_compute`] does; with no pool running the job runs here. For a thread that works through what comes
/// to it in order (the conversation feed's intake: `crate::convo::feed`), which keeps its order by waiting.
pub fn compute_pieces_blocking<S, T, F>(state: S, mut step: F) -> anyhow::Result<T>
where
    S: Send + 'static,
    T: Send + 'static,
    F: FnMut(&mut S, &Turn<'_>) -> Step<T> + Send + 'static,
{
    // The pool starts with the runtime; where neither can, the job is refused and runs here.
    let _ = rt();
    let (tx, rx) = std::sync::mpsc::sync_channel::<T>(1);
    let piece = move |(state, tx): &mut (S, Option<std::sync::mpsc::SyncSender<T>>), turn: &Turn<'_>| match step(state, turn) {
        Step::Done(value) => {
            if let Some(tx) = tx.take() {
                let _ = tx.send(value);
            }
            Step::Done(())
        }
        Step::More => Step::More,
    };
    let mut job = ((state, Some(tx)), piece);
    let mut delay = Duration::from_millis(1);
    loop {
        match hyper_rt::compute::run_pieces(job.0, job.1) {
            Ok(_) => break,
            Err(refused) if matches!(refused.error, RtError::Capacity { .. }) => {
                job = refused.job;
                std::thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_millis(100));
            }
            Err(refused) => {
                // Here, inside an unwind boundary as the pool's are: one that unwinds drops its sender.
                let (state, piece) = refused.job;
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || hyper_rt::compute::run_here(state, piece)));
                break;
            }
        }
    }
    rx.recv().map_err(|_| anyhow::anyhow!("compute job failed (it panicked)"))
}

/// Hands `job` to the compute pool by `submit`. At the pool's bound it waits, as [`blocking_in`] does
/// for a full share, and asks again with the job handed back; with no pool running it runs the job by
/// `alone` on a thread of its own.
async fn to_compute<J: Send + 'static>(mut job: J, submit: impl Fn(J) -> Result<(), hyper_rt::compute::Refused<J>>, alone: fn(J)) -> anyhow::Result<()> {
    let mut delay = Duration::from_millis(1);
    loop {
        match submit(job) {
            Ok(()) => return Ok(()),
            Err(refused) if matches!(refused.error, RtError::Capacity { .. }) => {
                job = refused.job;
                sleep(delay).await;
                delay = (delay * 2).min(Duration::from_millis(100));
            }
            Err(refused) => {
                tracing::warn!(error = %refused.error, "compute pool unavailable; running on a thread of its own");
                let job = refused.job;
                std::thread::Builder::new().name("kiln-compute".into()).spawn(move || alone(job)).context("spawn a compute thread")?;
                return Ok(());
            }
        }
    }
}

/// Run `f` on a thread of its own, not the pool's: for a job that mostly waits on other work, for
/// seconds to a minute (an MCP program waiting on its calls), so it holds none of the pool's few
/// places while it waits (the work it waits on may need them).
pub async fn on_thread<R, F>(name: &str, f: F) -> anyhow::Result<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let (tx, rx) = futures::channel::oneshot::channel::<R>();
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .context("spawn a thread")?;
    rx.await.map_err(|_| anyhow::anyhow!("the job's thread ended without a result (it panicked)"))
}

/// Gives the shard to its other tasks once, where any of them want it (hyper-rt's `maybe_yield`,
/// after Seastar's): a loop whose waits are all ready at once (a channel that always has the next
/// item) otherwise holds its shard for as long as they are, and every socket and timer on it waits,
/// the HTTP listener among them. Where nothing else wants the shard it goes on at once.
pub async fn maybe_yield() {
    hyper_rt::futures::maybe_yield().await;
}

/// The yield point of a long computation on a shard, between its pieces: past the task's share of
/// its shard (its step quantum) with other work waiting, the task yields once and goes on at its next
/// turn; otherwise it goes on at once (`hyper_rt::coop::consume_budget`, kiln's goal 6). Loops whose
/// waits are hyper-rt's or this module's need no call: those charge the same budget.
pub async fn consume_budget() {
    hyper_rt::coop::consume_budget().await;
}

/// A loop's turns on its shard: it works on while its next piece, taken to be as long as the longest it
/// has done this turn, would still end within one of the shard's steps (its step quantum: hyper-rt's
/// online estimate of what a wake costs, else its configured step budget), then gives the shard to its
/// other tasks where they want it ([`maybe_yield`]). So a turn's poll ends within the quantum where its
/// pieces don't grow (goal 9, plan 01a1a87b2cec step 3: no poll past the quantum); a turn that looked
/// only for its quantum to have passed held its shard a piece past it.
pub struct Turns {
    began: Instant,
    /// When the loop last looked, nanoseconds since `began`, and the longest piece between two looks
    /// this turn: atomics so a `&Turns` goes wherever a reference may (one task's, never contended).
    looked_ns: AtomicU64,
    piece_ns: AtomicU64,
}

impl Turns {
    pub fn new() -> Self {
        Self { began: Instant::now(), looked_ns: AtomicU64::new(0), piece_ns: AtomicU64::new(0) }
    }

    /// After a piece of the loop's work: its turn given up once its next piece would pass its step's quantum.
    pub async fn after(&mut self) {
        if self.over() {
            maybe_yield().await;
            *self = Self::new();
        }
    }

    /// After a piece of the loop's work: as [`Turns::after`], its turn over once its next piece would pass its step's
    /// quantum, but its poll ends there too, whether or not anything else wants the shard now (`yield_now`, where
    /// `after` asks `maybe_yield`). Goal 9, plan 01a1a87b2cec step 3: no poll past the quantum. Told that nothing
    /// wanted the shard, the hub's net loop went on into its next turn in the same poll, and a burst of runners' frames
    /// held its shard for two turns and more a poll (0.4 to 0.6 ms on the CPU in rt9's calm rounds). The poll's end
    /// costs a turn of the shard's loop, which reads its inputs and comes back to the loop at once when nothing else
    /// is ready: once a quantum at the most, and only while frames wait.
    pub async fn end(&mut self) {
        if self.over() {
            hyper_rt::futures::yield_now().await;
            *self = Self::new();
        }
    }

    /// Before a piece of the loop's work expected to take `expected_ns` past what its pieces take on their own (a runner
    /// frame's read: `crate::wire::payload::expected_ns`): where that, after a piece as long as the longest this turn has
    /// done, would take the turn past its step's quantum, the turn ends first, its poll with it ([`Turns::end`]), and the
    /// piece is the next turn's first. A turn's first piece goes whatever it's expected to take: no turn would take it
    /// sooner. Goal 9, plan 01a1a87b2cec step 3: a turn of short pieces took one long read last, past the quantum, the
    /// pieces before it having said nothing of it.
    pub async fn before(&mut self, expected_ns: u64) {
        if expected_ns > 0 && hyper_rt::futures::step_budget_ns().is_some_and(|q| self.wont_fit_at(Instant::now(), q, expected_ns)) {
            hyper_rt::futures::yield_now().await;
            *self = Self::new();
        }
    }

    /// Before a piece of the loop's work that may hold its poll `bound_ns` at the most, all of it (a runner frame's, its
    /// reads and its handler: `crate::wire::payload::Piece::bound_ns`): where that would take the turn past its step's
    /// quantum, the turn ends first, its poll with it ([`Turns::end`]), and the piece is the next turn's first. A turn's
    /// first piece goes whatever its bound: no turn would take it sooner. Goal 9, plan 01a1a87b2cec step 3: asked only of
    /// a frame's read ([`Turns::before`]), with a piece as long as the turn's longest for the rest, a turn took a piece whose
    /// handler or read ran longer than that last, past the quantum (train 16's lab, g9p16: the hub's net loop's labels to
    /// 0.36 ms on the CPU, 0.48 ms custody's).
    pub async fn room_for(&mut self, bound_ns: u64) {
        if hyper_rt::futures::step_budget_ns().is_some_and(|q| self.no_room_at(Instant::now(), q, bound_ns)) {
            hyper_rt::futures::yield_now().await;
            *self = Self::new();
        }
    }

    /// [`Turns::room_for`]'s test at `now`, against a quantum of `quantum_ns`: whether a piece done this turn (one has been
    /// looked at) and `bound_ns` more would take the turn past `quantum_ns`.
    fn no_room_at(&self, now: Instant, quantum_ns: u64, bound_ns: u64) -> bool {
        // One task's: loads alone, nothing changed (the piece is the next look's).
        if self.looked_ns.load(Ordering::Relaxed) == 0 {
            return false;
        }
        let at_ns = u64::try_from(now.saturating_duration_since(self.began).as_nanos()).unwrap_or(u64::MAX);
        at_ns.saturating_add(bound_ns) > quantum_ns
    }

    /// [`Turns::before`]'s test at `now`, against a quantum of `quantum_ns`: whether a piece done this turn (one has been
    /// looked at), the longest one's time again and `expected_ns` more would take the turn past `quantum_ns`.
    fn wont_fit_at(&self, now: Instant, quantum_ns: u64, expected_ns: u64) -> bool {
        // One task's: loads alone, nothing changed (the piece is the next look's).
        if self.looked_ns.load(Ordering::Relaxed) == 0 {
            return false;
        }
        let at_ns = u64::try_from(now.saturating_duration_since(self.began).as_nanos()).unwrap_or(u64::MAX);
        at_ns.saturating_add(self.piece_ns.load(Ordering::Relaxed)).saturating_add(expected_ns) >= quantum_ns
    }

    /// The loop's next item off `rx`: one waiting already goes on in this turn; one it waited for starts its turn
    /// afresh, as the wait gave its shard to its other work, as the copies' owners take their inboxes
    /// (`crate::harness::copies`). `None` once `rx` is closed and empty. Goal 10, plan 01a1a87b2dae step 5: the
    /// hub's loop counted its idle wait as a piece of its turn, so after any quiet spell its first frame spent
    /// the turn, the loop gave its shard away, and the frames queued behind that one waited on the shard's other
    /// tasks (runners' frames, `to_loop`: p50 44 µs, p99 6.1 ms in g11alloc2's calm rounds).
    pub async fn next<T>(&mut self, rx: &crate::memory::queue::Receiver<T>) -> Option<T> {
        match rx.try_recv() {
            Ok(item) => Some(item),
            Err(async_channel::TryRecvError::Closed) => None,
            Err(async_channel::TryRecvError::Empty) => {
                let item = rx.recv().await.ok()?;
                *self = Self::new();
                Some(item)
            }
        }
    }

    /// Whether the loop's next piece, as long as the longest since its turn began (the one just done among
    /// them), would take its turn past its step's quantum: what it does next waits for [`Turns::after`].
    pub fn over(&self) -> bool {
        hyper_rt::futures::step_budget_ns().is_some_and(|q| self.over_at(Instant::now(), q))
    }

    /// [`Turns::over`] at `now`, against a quantum of `quantum_ns`.
    fn over_at(&self, now: Instant, quantum_ns: u64) -> bool {
        // One task's: a load and a store each, no read-modify-write.
        let at_ns = u64::try_from(now.saturating_duration_since(self.began).as_nanos()).unwrap_or(u64::MAX);
        let piece_ns = at_ns.saturating_sub(self.looked_ns.load(Ordering::Relaxed));
        self.looked_ns.store(at_ns, Ordering::Relaxed);
        let longest_ns = self.piece_ns.load(Ordering::Relaxed).max(piece_ns);
        self.piece_ns.store(longest_ns, Ordering::Relaxed);
        at_ns.saturating_add(longest_ns) >= quantum_ns
    }
}

impl Default for Turns {
    fn default() -> Self {
        Self::new()
    }
}

/// Run `f` on a named share of the blocking pool. Waits (instead of failing) while the share's
/// queue is full; if the pool is unavailable the job gets a thread of its own.
pub async fn blocking_in<R, F>(share: &'static str, f: F) -> anyhow::Result<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    rt()?;
    let (tx, rx) = futures::channel::oneshot::channel::<R>();
    let job: Arc<Mutex<Option<(F, futures::channel::oneshot::Sender<R>)>>> = Arc::new(Mutex::new(Some((f, tx))));
    let mut delay = Duration::from_millis(1);
    loop {
        let j = Arc::clone(&job);
        let submitted = hyper_rt::blocking::run(share, move || {
            let taken = j.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some((f, tx)) = taken {
                let _ = tx.send(f());
            }
        });
        match submitted {
            Ok(_receiver) => break,
            Err(RtError::Capacity { .. }) => {
                sleep(delay).await;
                delay = (delay * 2).min(Duration::from_millis(100));
            }
            Err(e) => {
                tracing::warn!(error = %e, share, "blocking pool unavailable; running on a thread");
                let j = Arc::clone(&job);
                std::thread::Builder::new()
                    .name("kiln-blocking".into())
                    .spawn(move || {
                        let taken = j.lock().unwrap_or_else(PoisonError::into_inner).take();
                        if let Some((f, tx)) = taken {
                            let _ = tx.send(f());
                        }
                    })
                    .context("spawn blocking thread")?;
                break;
            }
        }
    }
    rx.await.map_err(|_| anyhow::anyhow!("blocking job failed (it panicked)"))
}

// ------------------------------------------------------------------------------------- bridging

/// Map a hyper-rt refusal to `io::Error`, keeping the OS errno so `ErrorKind` stays meaningful.
pub fn to_io(error: RtError) -> io::Error {
    match error {
        RtError::DriverRefused { code: Some(code), .. } => io::Error::from_raw_os_error(code),
        RtError::WouldBlock { .. } => io::Error::from(io::ErrorKind::WouldBlock),
        other => io::Error::other(other),
    }
}

/// Whether `cx` carries the waker of the hyper-rt task being polled right now (what hyper-rt's leaf
/// futures require). False under a combinator's or library's own waker, or off the runtime.
pub fn is_native(cx: &Context<'_>) -> bool {
    match (hyper_rt::waker::word_of(cx.waker()), hyper_rt::futures::current_task()) {
        (Some(word), Some(task)) => word == task.0,
        _ => false,
    }
}

struct BridgeState<T> {
    outcome: Option<T>,
    waker: Option<Waker>,
}

/// A hyper-rt leaf future awaited by a helper task on the current shard, so it can be awaited under
/// a foreign waker: the helper polls it with its own waker, stores the output and wakes whichever
/// waker polled the bridge last. Dropping the bridge cancels the helper.
pub struct Bridge<T: Send + 'static> {
    shared: Arc<Mutex<BridgeState<T>>>,
    task: TaskId,
}

impl<T: Send + 'static> Bridge<T> {
    pub fn spawn<F: Future<Output = T> + 'static>(leaf: F, waker: &Waker) -> Result<Self, RtError> {
        let shared = Arc::new(Mutex::new(BridgeState { outcome: None, waker: Some(waker.clone()) }));
        let helper = Arc::clone(&shared);
        let task = hyper_rt::futures::spawn_detached(async move {
            let outcome = leaf.await;
            let waker = {
                let mut state = helper.lock().unwrap_or_else(PoisonError::into_inner);
                state.outcome = Some(outcome);
                state.waker.take()
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        })?;
        Ok(Self { shared, task })
    }

    pub fn poll(&self, cx: &mut Context<'_>) -> Poll<T> {
        let mut state = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        match state.outcome.take() {
            Some(outcome) => Poll::Ready(outcome),
            None => {
                if !state.waker.as_ref().is_some_and(|w| w.will_wake(cx.waker())) {
                    state.waker = Some(cx.waker().clone());
                }
                Poll::Pending
            }
        }
    }
}

impl<T: Send + 'static> Drop for Bridge<T> {
    fn drop(&mut self) {
        // A helper that already finished is a stale id: refused, harmlessly. Dropped on another shard than its
        // helper's (a movable task's bridge, the task moved since: goal 7, `hyper_rt::movable`), it is cancelled
        // there by a message, so its wait doesn't outlive what it waited for.
        if hyper_rt::futures::shard_id().is_some_and(|here| here.0 == self.task.0.shard()) {
            let _ = hyper_rt::futures::cancel(self.task);
        } else {
            let _ = hyper_rt::runtime::cancel_task(self.task);
        }
    }
}

/// `fut` polled under its own task's waker, which hyper-rt's leaves need (a connect, a socket's readiness wait):
/// as it is where its caller's poll is native, else in a helper task on this shard ([`Bridge`]) that wakes the
/// caller when it ends: a movable task's poll (goal 7, `hyper_rt::movable`), or one through a combinator's own
/// waker. Refused (`NotOnShardThread`) off the runtime's shards.
pub async fn native<F>(fut: F) -> Result<F::Output, RtError>
where
    F: Future + 'static,
    F::Output: Send + 'static,
{
    Native { fut: Some(Box::pin(fut)), bridged: None }.await
}

/// The future of [`native`].
struct Native<F: Future>
where
    F::Output: Send + 'static,
{
    fut: Option<Pin<Box<F>>>,
    bridged: Option<Bridge<F::Output>>,
}

impl<F: Future + 'static> Future for Native<F>
where
    F::Output: Send + 'static,
{
    type Output = Result<F::Output, RtError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(bridge) = &this.bridged {
            return bridge.poll(cx).map(Ok);
        }
        if is_native(cx) {
            return match this.fut.as_mut() {
                Some(fut) => fut.as_mut().poll(cx).map(Ok),
                None => Poll::Ready(Err(RtError::NotOnShardThread)),
            };
        }
        let Some(fut) = this.fut.take() else { return Poll::Ready(Err(RtError::NotOnShardThread)) };
        match Bridge::spawn(fut, cx.waker()) {
            Ok(bridge) => {
                let polled = bridge.poll(cx);
                this.bridged = Some(bridge);
                polled.map(Ok)
            }
            Err(refused) => Poll::Ready(Err(refused)),
        }
    }
}

// --------------------------------------------------------------------------------------- timers

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// A sleep until a deadline on the shard's timing wheel; under a foreign waker it runs in a
/// [`Bridge`]. If the runtime refuses the timer it completes rather than hang. `Send + Sync`, so it
/// also serves as hyper's `Sleep`.
pub struct Sleep {
    deadline: Instant,
    native: Option<hyper_rt::futures::Sleep>,
    bridged: Option<Bridge<Result<(), RtError>>>,
}

impl Sleep {
    pub fn until(deadline: Instant) -> Self {
        Self { deadline, native: None, bridged: None }
    }

    fn remaining_ns(&self) -> u64 {
        nanos(self.deadline.saturating_duration_since(Instant::now()))
    }
}

impl Future for Sleep {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if Instant::now() >= self.deadline {
            // A deadline already passed completes at once, as kiln's Clock seam holds every clock to
            // (`kiln_seams::clock`, "a sleep until now"), so it is no charge that may refuse: a sleep that
            // waits is charged by hyper-rt's timer (`hyper_rt::coop`, goal 6, plan 01a1a59c2abe step 1).
            self.native = None;
            self.bridged = None;
            return Poll::Ready(());
        }
        if is_native(cx) {
            self.bridged = None;
            let remaining = self.remaining_ns();
            let sleep = self.native.get_or_insert_with(|| hyper_rt::futures::sleep(remaining));
            return match Pin::new(sleep).poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(_) => Poll::Ready(()),
            };
        }
        self.native = None;
        if self.bridged.is_none() {
            let remaining = self.remaining_ns();
            match Bridge::spawn(hyper_rt::futures::sleep(remaining), cx.waker()) {
                Ok(bridge) => self.bridged = Some(bridge),
                Err(_) => return Poll::Ready(()),
            }
        }
        match self.bridged.as_ref().map(|bridge| bridge.poll(cx)) {
            Some(Poll::Pending) => Poll::Pending,
            _ => {
                self.bridged = None;
                Poll::Ready(())
            }
        }
    }
}

pub fn sleep(d: Duration) -> Sleep {
    Sleep::until(kiln_seams::clock::later(Instant::now(), d))
}

/// Real time, on the runtime's timers, as kiln-seams' [`kiln_seams::Clock`]: what production code
/// that takes a clock is given (tests give it a `kiln_seams::clock::SimClock`).
#[derive(Debug, Clone, Copy, Default)]
pub struct RtClock;

impl kiln_seams::Clock for RtClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn unix_ms(&self) -> i64 {
        crate::util::now_ms()
    }
    fn sleep_until(&self, deadline: Instant) -> kiln_seams::clock::Sleep {
        Box::pin(sleep_until(deadline))
    }
}

/// The process's real clock.
pub fn clock() -> Arc<dyn kiln_seams::Clock> {
    Arc::new(RtClock)
}

pub fn sleep_until(deadline: Instant) -> Sleep {
    Sleep::until(deadline)
}

/// The deadline passed before the future finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed;

impl std::fmt::Display for Elapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("deadline elapsed")
    }
}

impl std::error::Error for Elapsed {}

/// `f` of each item, at most `n` at once, all on this task: each future polled under this task's
/// own waker, which hyper-rt's I/O futures require (futures' `buffered` and `FuturesUnordered`
/// poll under wakers of their own, and a connect there is refused). The outputs in the items'
/// order.
pub async fn each<T, R, Fut: Future<Output = R>>(items: impl IntoIterator<Item = T>, n: usize, f: impl Fn(T) -> Fut) -> Vec<R> {
    let mut items = items.into_iter().enumerate();
    let n = n.max(1);
    let mut running: Vec<(usize, Pin<Box<Fut>>)> = Vec::with_capacity(n);
    let mut done: Vec<(usize, R)> = Vec::new();
    std::future::poll_fn(|cx| {
        loop {
            while running.len() < n {
                let Some((i, t)) = items.next() else { break };
                running.push((i, Box::pin(f(t))));
            }
            if running.is_empty() {
                return Poll::Ready(());
            }
            let before = running.len();
            let mut k = 0;
            while k < running.len() {
                let polled = match running.get_mut(k) {
                    Some((_, fut)) => fut.as_mut().poll(cx),
                    None => break,
                };
                match polled {
                    Poll::Ready(r) => {
                        let (i, _) = running.swap_remove(k);
                        done.push((i, r));
                    }
                    Poll::Pending => k += 1,
                }
            }
            // Nothing finished: each future pending has this task's waker.
            if running.len() == before {
                return Poll::Pending;
            }
        }
    })
    .await;
    done.sort_by_key(|(i, _)| *i);
    done.into_iter().map(|(_, r)| r).collect()
}

/// As [`each`], each output handed to `done` in the items' order as soon as it and every one before
/// it are ready; an item is begun (`f` called) only once one of the `n` places is free, after
/// `done` had every output ready before it, so what `done` changes `f` sees. Outputs ready before
/// their turn hold a place: `n` at most are begun and not yet handed over.
pub async fn each_in_order<T, R, Fut: Future<Output = R>>(items: impl IntoIterator<Item = T>, n: usize, f: impl Fn(T) -> Fut, mut done: impl FnMut(R)) {
    let mut items = items.into_iter().enumerate();
    let n = n.max(1);
    let mut running: Vec<(usize, Pin<Box<Fut>>)> = Vec::with_capacity(n);
    let mut ready: std::collections::BTreeMap<usize, R> = std::collections::BTreeMap::new();
    let mut next = 0usize;
    std::future::poll_fn(|cx| {
        loop {
            while let Some(r) = ready.remove(&next) {
                done(r);
                next += 1;
            }
            while running.len() + ready.len() < n {
                let Some((i, t)) = items.next() else { break };
                running.push((i, Box::pin(f(t))));
            }
            if running.is_empty() {
                return Poll::Ready(());
            }
            let before = running.len();
            let mut k = 0;
            while k < running.len() {
                let polled = match running.get_mut(k) {
                    Some((_, fut)) => fut.as_mut().poll(cx),
                    None => break,
                };
                match polled {
                    Poll::Ready(r) => {
                        let (i, _) = running.swap_remove(k);
                        ready.insert(i, r);
                    }
                    Poll::Pending => k += 1,
                }
            }
            // Nothing finished: each future pending has this task's waker.
            if running.len() == before {
                return Poll::Pending;
            }
        }
    })
    .await;
}

/// `fut`'s output, or `Err(Elapsed)` once `d` has passed (the future is then dropped). When both are
/// ready the output wins.
pub async fn timeout<F: Future>(d: Duration, fut: F) -> Result<F::Output, Elapsed> {
    let fut = pin!(fut);
    let deadline = pin!(sleep(d));
    match futures::future::select(fut, deadline).await {
        futures::future::Either::Left((v, _)) => Ok(v),
        futures::future::Either::Right(_) => Err(Elapsed),
    }
}

/// A periodic tick: the first `tick()` completes at once, then one per period. Ticks missed while
/// busy are skipped, not replayed. `tick()` is cancel-safe (state changes only when it completes).
pub struct Interval {
    period: Duration,
    next: Instant,
}

impl Interval {
    pub fn new(period: Duration) -> Self {
        Self { period: period.max(Duration::from_millis(1)), next: Instant::now() }
    }

    pub async fn tick(&mut self) {
        sleep_until(self.next).await;
        let now = Instant::now();
        self.next += self.period;
        if self.next <= now {
            self.next = now + self.period;
        }
    }
}

// ---------------------------------------------------------------------------------- shutdown

/// The waits on a [`Shutdown`] or a [`Notify`], with no lock (goal 5, plan 01a1a59c2aae, its top goal): each
/// wait waits in a place on memory::queue's own lock-free list of waiters (`crate::memory::queue::Waiters`, a
/// Dekker handshake, so no wake is lost), and a wake wakes every place listed then. Their waits were
/// event-listener's, whose list sits behind a std `Mutex` that every wait, every wait dropped and every wake
/// took: a terminal's relay listed and dropped two a frame on its shard while the hub's loop rang one on its
/// own, and each loop selecting on the server's shutdown listed and dropped one a turn (goal 5's census of
/// train 8's tree on fedora: event-listener the one lock the shards still shared, waiting 0.24 ms a second with
/// hogs, 1.87 ms at the longest).
///
/// A wait's place goes back to `spares` as the wait ends or is dropped, listed still or not, and the next
/// wait takes it up: a loop that waits each turn beside other arms (a select another arm wins) keeps one place
/// listed, not one more a turn, and there are never more places than there were waits at once.
struct Waits {
    list: crate::memory::queue::Waiters,
    spares: concurrent_queue::ConcurrentQueue<Arc<crate::memory::queue::Place>>,
}

impl Default for Waits {
    fn default() -> Self {
        Self { list: crate::memory::queue::Waiters::new(), spares: concurrent_queue::ConcurrentQueue::unbounded() }
    }
}

impl Waits {
    /// Waits until `ready` says so (it may take what it finds: a permit): at once if it does now, else in a
    /// place on the list, looking again once the place is listed and each time it's woken.
    async fn until(&self, ready: impl Fn() -> bool) {
        if ready() {
            return;
        }
        let held = Held { place: self.spares.pop().unwrap_or_else(|_| crate::memory::queue::Place::new()), spares: &self.spares };
        std::future::poll_fn(|cx| {
            if ready() {
                return Poll::Ready(());
            }
            self.list.wait(&held.place, cx.waker());
            if ready() { Poll::Ready(()) } else { Poll::Pending }
        })
        .await;
    }

    /// Wakes every wait listed now, each once.
    fn wake(&self) {
        self.list.wake();
    }
}

/// A wait's place, back among its spares as the wait ends or is dropped ([`Waits`]), the waker it held let go
/// first: a wake that finds it listed meanwhile wakes no one.
struct Held<'a> {
    place: Arc<crate::memory::queue::Place>,
    spares: &'a concurrent_queue::ConcurrentQueue<Arc<crate::memory::queue::Place>>,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.place.let_go();
        // Unbounded and never closed: the push always goes.
        let _ = self.spares.push(Arc::clone(&self.place));
    }
}

#[derive(Default)]
struct ShutdownInner {
    cancelled: AtomicBool,
    waits: Waits,
}

/// A cancellation signal any number of tasks can wait on (process shutdown, a job's cancel), with no lock
/// ([`Waits`]): a cancel wakes every wait once.
#[derive(Clone, Default)]
pub struct Shutdown(Arc<ShutdownInner>);

impl Shutdown {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        if !self.0.cancelled.swap(true, Ordering::SeqCst) {
            self.0.waits.wake();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }

    /// Completes once [`Self::cancel`] has been called: if it already was, at once, or a turn later
    /// when the task has used its share of a shard other work wants (a charge of its turn,
    /// `hyper_rt::coop`).
    pub async fn cancelled(&self) {
        hyper_rt::coop::consume_budget().await;
        self.0.waits.until(|| self.is_cancelled()).await;
    }

    /// Sleep for `d`: `true` when the time passed, `false` when cancelled first.
    pub async fn sleep(&self, d: Duration) -> bool {
        if self.is_cancelled() {
            return false;
        }
        let cancelled = pin!(self.cancelled());
        let slept = pin!(sleep(d));
        matches!(futures::future::select(cancelled, slept).await, futures::future::Either::Right(_))
    }
}

// ------------------------------------------------------------------------------------- notify

/// Wake one waiter, or leave a permit for the next one (like a binary semaphore). With no lock ([`Waits`]): a
/// notify wakes every wait listed, the first to look takes the permit, and the rest wait on.
#[derive(Default)]
pub struct Notify {
    permit: AtomicBool,
    waits: Waits,
}

impl Notify {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn notify(&self) {
        self.permit.store(true, Ordering::SeqCst);
        self.waits.wake();
    }

    /// A notification, taken; a charge of the task's turn on its shard (`hyper_rt::coop`), so a loop on a
    /// notifier that is always set yields at its share.
    pub async fn notified(&self) {
        hyper_rt::coop::consume_budget().await;
        self.waits.until(|| self.permit.swap(false, Ordering::SeqCst)).await;
    }
}

// ------------------------------------------------------------------------------------ changed

/// Something changed (a store's commit, a session let in, a task's news): a notify ends every wait begun before
/// it, and a wait begun after it waits for the next; nothing is kept for a wait to come. So a waiter listens first
/// ([`Changed::listen`]), looks at what it waits for, then waits: a change made after its look is a notify after
/// its listen. With no lock ([`Waits`]; goal 5, plan 01a1a59c2aae, its top goal): it was event-listener's `Event`,
/// whose list of listeners sits behind a std `Mutex` that every listen, every wait given up and every notify finding
/// a listener took. The records' change feed, the conversation store's writer's room, a sync task's news, a
/// runner's harness host and cache uploads, and headroom's held sessions waited so (the census of train 8's tree:
/// event-listener's the one lock the shards still shared, 1.8 ms a second waited with hogs, docs/runtime-lab.md).
#[derive(Default)]
pub struct Changed {
    /// The notifies so far: a wait is over once they're more than when it began.
    count: AtomicU64,
    waits: Waits,
}

impl Changed {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ends every wait begun before it.
    pub fn notify(&self) {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.waits.wake();
    }

    /// A wait for the next notify, begun now: a notify between this and its await still ends it. Awaited, or on
    /// a thread of its own ([`Listen::wait_deadline`]).
    pub fn listen(&self) -> Listen<'_> {
        Listen { changed: self, seen: self.count.load(Ordering::SeqCst), held: None }
    }
}

/// A wait on a [`Changed`], begun as it was made: a future (a charge of the task's turn on its shard first,
/// `hyper_rt::coop`, as [`Notify::notified`]'s), or a thread's wait ([`Listen::wait_deadline`]). Its place on the
/// list is one of the changed's spares, back among them as it's dropped ([`Waits`]).
#[must_use = "a wait does nothing unless it's awaited"]
pub struct Listen<'a> {
    changed: &'a Changed,
    seen: u64,
    held: Option<Held<'a>>,
}

impl<'a> Listen<'a> {
    /// Whether a notify came since it began.
    fn over(&self) -> bool {
        self.changed.count.load(Ordering::SeqCst) != self.seen
    }

    /// Its place on the list: a spare's, taken up as it first waits.
    fn place(&mut self) -> Arc<crate::memory::queue::Place> {
        let changed: &'a Changed = self.changed;
        let held = self.held.get_or_insert_with(|| Held { place: changed.waits.spares.pop().unwrap_or_else(|_| crate::memory::queue::Place::new()), spares: &changed.waits.spares });
        Arc::clone(&held.place)
    }

    /// Waits, blocking its thread (a thread of its own, never a shard's), until a notify or `until`: whether a
    /// notify came.
    pub fn wait_deadline(mut self, until: Instant) -> bool {
        let waker = crate::memory::queue::thread_waker();
        loop {
            if self.over() {
                return true;
            }
            let place = self.place();
            self.changed.waits.list.wait(&place, &waker);
            if self.over() {
                return true;
            }
            let now = Instant::now();
            if now >= until {
                return false;
            }
            std::thread::park_timeout(until.saturating_duration_since(now));
        }
    }
}

impl Future for Listen<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        if hyper_rt::coop::poll_proceed(cx).is_pending() {
            return Poll::Pending;
        }
        if this.over() {
            return Poll::Ready(());
        }
        let place = this.place();
        this.changed.waits.list.wait(&place, cx.waker());
        if this.over() { Poll::Ready(()) } else { Poll::Pending }
    }
}

// ---------------------------------------------------------------------------------- broadcast

/// Fan-out to every current subscriber, with no lock on any path (goal 4, plan 01a1a59c2a9c step 5; goal 5's
/// rule): one lane of `crate::memory::lanes`, each subscriber its own channel. async-broadcast's one channel,
/// which it was, took its `Mutex` for every send and every subscriber's receive. A subscriber that falls
/// behind loses what it has no room for (it sees `RecvError::Overflowed`, and goes on from after what it
/// missed) instead of holding up the sender or the others.
pub struct Broadcast<T: Clone> {
    lanes: crate::memory::lanes::Lanes<Plain<T>>,
    capacity: usize,
}

/// A broadcast's message, counted at its place alone: what a broadcast carries is small (CD's changes, an id
/// each), and none of it is on `/metrics` (`crate::memory::owners::BROADCASTS`).
#[derive(Clone)]
struct Plain<T>(T);

impl<T> crate::memory::queue::Weigh for Plain<T> {
    fn weight(&self) -> usize {
        size_of::<Self>()
    }
}

impl<T: Clone + Send + Sync + 'static> Broadcast<T> {
    /// A broadcast each of whose subscribers holds `capacity` messages (one at the least).
    pub fn new(capacity: usize) -> Self {
        Self { lanes: crate::memory::lanes::Lanes::new(&crate::memory::owners::BROADCASTS, 1, 0), capacity: capacity.max(1) }
    }

    /// Deliver to the current subscribers (dropped when there are none).
    pub fn send(&self, value: T) {
        self.lanes.send(0, Plain(value));
    }

    /// A subscriber from now. When the runtime has no channel to give it (said in the log), one that has ended.
    pub fn subscribe(&self) -> Subscription<T> {
        Subscription(self.lanes.subscribe_or_ended(self.capacity))
    }

    pub fn receiver_count(&self) -> usize {
        self.lanes.receiver_count()
    }
}

/// A subscriber of a [`Broadcast`]: its own channel, whose receive charges the task's turn on its shard
/// (`hyper_rt::coop`, kiln's goal 6), so a subscriber whose channel is always full yields at its share.
pub struct Subscription<T>(crate::memory::lanes::Reader<Plain<T>>);

impl<T: Clone> Subscription<T> {
    /// The next message, after a charge of the task's turn: `Overflowed` with how many it missed, `Closed` once
    /// the broadcast is gone and nothing waits.
    pub async fn recv(&mut self) -> Result<T, async_broadcast::RecvError> {
        self.0.recv().await.map(|p| p.0)
    }
}

/// As a stream (what `select`s and `map`s over subscriptions take), each item a charge of the task's turn too;
/// what was missed is passed over.
impl<T: Clone> futures::Stream for Subscription<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        if hyper_rt::coop::poll_proceed(cx).is_pending() {
            return Poll::Pending;
        }
        loop {
            match self.0.poll_recv(cx) {
                Poll::Ready(Ok(p)) => return Poll::Ready(Some(p.0)),
                Poll::Ready(Err(async_broadcast::RecvError::Overflowed(_))) => {}
                Poll::Ready(Err(async_broadcast::RecvError::Closed)) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

// --------------------------------------------------------------------------------------- files

/// The next value of a queue's receiver (`crate::memory::queue`, lock-free: goal 5, plan 01a1a59c2aae, its top
/// goal; it was an async-channel's, whose waits took event-listener's `Mutex`), after a charge of the task's turn on
/// its shard (`hyper_rt::coop`, kiln's goal 6): a loop that drains a queue that is always full yields at its share,
/// as one over hyper-rt's own channels does.
pub async fn recv<T>(rx: &crate::memory::queue::Receiver<T>) -> Result<T, async_channel::RecvError> {
    hyper_rt::coop::consume_budget().await;
    rx.recv().await
}

/// Read size for file streams.
const FILE_CHUNK: usize = 256 * 1024;
/// Bytes a file sink gathers before one write.
const SINK_BATCH: usize = 1 << 20;

/// Stream a file in chunks. Opening happens now (so a missing file is an error here); each chunk is
/// read on the blocking pool, and no thread waits while the consumer is slow. The stream ends at
/// end of file (or a read error, which is logged). Its chunks wait in a queue of four
/// (`crate::memory::queue`, lock-free: goal 5, plan 01a1a59c2aae, its top goal), as in the
/// async-channel it was, whose every chunk took event-listener's `Mutex` (a download's, an artifact's).
pub fn file_source(path: impl Into<PathBuf>) -> anyhow::Result<crate::memory::queue::Receiver<Bytes>> {
    let path = path.into();
    let file = Arc::new(std::fs::File::open(&path).with_context(|| format!("open {}", path.display()))?);
    let (tx, rx) = crate::memory::queue::places::<Bytes>(&crate::memory::owners::CHANNELS, 4);
    spawn(async move {
        let mut offset = 0u64;
        loop {
            let f = Arc::clone(&file);
            let read = blocking(move || {
                let mut buf = vec![0u8; FILE_CHUNK];
                let n = crate::os::read_at(&f, &mut buf, offset)?;
                buf.truncate(n);
                Ok::<_, io::Error>(buf)
            })
            .await;
            match read {
                Ok(Ok(buf)) if buf.is_empty() => return,
                Ok(Ok(buf)) => {
                    offset = offset.saturating_add(u64::try_from(buf.len()).unwrap_or(u64::MAX));
                    // A send is a charge of the task's turn (`hyper_rt::coop`).
                    consume_budget().await;
                    if tx.send(Bytes::from(buf)).await.is_err() {
                        return;
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, path = %path.display(), "file read failed mid-stream");
                    return;
                }
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), path = %path.display(), "file read failed mid-stream");
                    return;
                }
            }
        }
    });
    Ok(rx)
}

/// Write a stream of chunks to a new file. Send chunks into the returned sender, drop it, then
/// await the future for the byte count. Writing runs concurrently (bounded), so the sender sees
/// back-pressure rather than unbounded buffering; exceeding `max` bytes fails the write. Its chunks
/// wait in a queue of sixteen (`crate::memory::queue`, lock-free: goal 5, plan 01a1a59c2aae, its top
/// goal), as in the async-channel it was, whose every chunk took event-listener's `Mutex` (an upload's).
pub fn file_sink(
    path: impl Into<PathBuf>,
    max: u64,
) -> anyhow::Result<(crate::memory::queue::Sender<Bytes>, impl Future<Output = anyhow::Result<u64>> + Send + 'static)> {
    let path = path.into();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let file = Arc::new(std::fs::File::create(&path).with_context(|| format!("create {}", path.display()))?);
    let (tx, rx) = crate::memory::queue::places::<Bytes>(&crate::memory::owners::CHANNELS, 16);
    let writer = spawn(async move {
        let mut total = 0u64;
        // Each receive a charge of the task's turn (`hyper_rt::coop`).
        while let Ok(first) = recv(&rx).await {
            let mut batch = vec![first];
            let mut size: usize = batch.iter().map(Bytes::len).sum();
            while size < SINK_BATCH {
                match rx.try_recv() {
                    Ok(more) => {
                        size = size.saturating_add(more.len());
                        batch.push(more);
                    }
                    Err(_) => break,
                }
            }
            let at = total;
            total = total.saturating_add(u64::try_from(size).unwrap_or(u64::MAX));
            anyhow::ensure!(total <= max, "upload exceeds {max} bytes");
            let f = Arc::clone(&file);
            blocking(move || {
                let mut off = at;
                for chunk in &batch {
                    crate::os::write_all_at(&f, chunk, off)?;
                    off = off.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
                }
                Ok::<_, io::Error>(())
            })
            .await??;
        }
        let f = Arc::clone(&file);
        blocking(move || f.sync_data()).await??;
        Ok(total)
    });
    let done = async move { writer.await.map_err(|_| anyhow::anyhow!("file writer stopped"))? };
    Ok((tx, done))
}

// ----------------------------------------------------------------------------------------- dns

/// Resolve `host:port` (or an address literal) to one address, preferring IPv4.
pub async fn resolve(addr: &str) -> anyhow::Result<SocketAddr> {
    if let Ok(a) = addr.parse::<SocketAddr>() {
        return Ok(a);
    }
    let (host, port) = addr.rsplit_once(':').with_context(|| format!("{addr}: expected host:port"))?;
    let port: u16 = port.parse().with_context(|| format!("{addr}: bad port"))?;
    let all = resolve_host(host, port).await?;
    all.iter()
        .find(|a| a.is_ipv4())
        .or_else(|| all.first())
        .copied()
        .with_context(|| format!("{addr}: no address"))
}

/// Every address for `host` (IPv6 brackets allowed), through hyper-rt's DNS (getaddrinfo on the
/// blocking pool).
pub async fn resolve_host(host: &str, port: u16) -> anyhow::Result<Vec<SocketAddr>> {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    rt()?;
    hyper_rt::dns::resolve(bare, port, 8).await.map_err(|e| anyhow::anyhow!("resolve {bare}: {e}"))
}

// -------------------------------------------------------------------------------------- signals

pub const SIGHUP: i32 = 1;
pub const SIGINT: i32 = 2;
pub const SIGTERM: i32 = 15;

/// SIGINT / SIGTERM / SIGHUP as a stream. The subscription lives as long as this value (dropping
/// the last one restores the default action, i.e. the next SIGTERM kills the process).
pub struct Signals {
    rx: crate::memory::queue::Receiver<i32>,
}

impl Signals {
    pub fn new(kinds: &[i32]) -> anyhow::Result<Self> {
        use hyper_rt::signal::Signal;
        let wanted: Vec<Signal> = kinds.iter().flat_map(|k| crate::os::signal_kinds(*k).iter().copied()).collect();
        anyhow::ensure!(!wanted.is_empty(), "no supported signal requested");
        rt()?;
        let stream = hyper_rt::signal::subscribe(&wanted).map_err(|e| anyhow::anyhow!("subscribe to signals: {e}"))?;
        let (tx, rx) = crate::memory::queue::unbounded(&crate::memory::owners::CHANNELS);
        // The stream is awaited at a task's top level (its own waker); consumers read the queue,
        // which works under any waker.
        spawn(async move {
            let mut failures = 0u32;
            loop {
                match stream.recv().await {
                    Ok(set) => {
                        failures = 0;
                        for s in set.iter() {
                            if tx.send(crate::os::signal_number(s)).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        failures += 1;
                        tracing::warn!(error = %e, "signal stream error");
                        if failures > 10 {
                            return;
                        }
                        sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        });
        Ok(Self { rx })
    }

    /// The next signal (`None` once the subscription is gone).
    pub async fn next(&self) -> Option<i32> {
        recv(&self.rx).await.ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 9 (plan 01a1a87b2cec step 3): the step quantum is derived from the lab's measurements, not set. The
    /// floor, the two polls a wake finds at p99.9 on the lab's busiest shard and the host's slice, a quantum each,
    /// come to the bar at most, and a nanosecond more each would pass it; the floor is at or above what the lab's
    /// calm shards measured; ordinary work's long polls fit a quantum; the runtime takes it. And the derivation
    /// follows its measurements: at step 5's busy share a wake finds one poll at p99.9, so 400 µs would do.
    #[test]
    fn g9_s3_the_step_quantum_is_derived_from_the_lab() {
        // What ordinary work's long polls hold a shard: the largest mean hold, of the polls past 100 µs, of a site
        // outside step 2's list in the lab's window (g6c2 calm on rt5: `reads.rs:338`, 0.246 ms with shedding on,
        // 0.232 ms with it off). A quantum below it would have ordinary work cut finer than its own polls.
        const ORDINARY_POLL_NS: u64 = 246_000;
        let ahead = polls_ahead_at_p999(LAB_BUSY_PPM);
        assert_eq!(ahead, 2, "two polls in hand or ahead at p99.9 on a shard busy 0.065 of the time");
        // ρ² a thousandth or more (4225 parts in a million), ρ³ less (274).
        let (two, three) = (LAB_BUSY_PPM * LAB_BUSY_PPM / 1_000_000, LAB_BUSY_PPM * LAB_BUSY_PPM / 1_000_000 * LAB_BUSY_PPM / 1_000_000);
        assert!(two >= 1_000 && three < 1_000, "{two} {three}");
        let within = |q: u64| DELAY_FLOOR_NS + (ahead + 1) * q <= DELAY_BAR_NS;
        assert!(within(STEP_BUDGET_NS), "the floor and three quanta within the bar");
        assert!(!within(STEP_BUDGET_NS + 1), "the largest quantum within it");
        assert_eq!(STEP_BUDGET_NS, 266_666);
        const { assert!(DELAY_FLOOR_NS >= 192_000, "the floor by design, against the lab's calm median of 192 µs") };
        const { assert!(STEP_BUDGET_NS >= ORDINARY_POLL_NS, "ordinary work's long polls fit a quantum") };
        let c = runtime_config(2, 64);
        assert_eq!((c.step_budget_ns, c.timer_tick_ns), (STEP_BUDGET_NS, TIMER_TICK_NS));
        // Step 5's bar, a busy share of 0.011: one poll at p99.9.
        assert_eq!(polls_ahead_at_p999(11_000), 1);
        assert_eq!(DELAY_BAR_NS.saturating_sub(DELAY_FLOOR_NS) / polls_ahead_at_p999(11_000).saturating_add(1), 400_000);
        assert_eq!(polls_ahead_at_p999(900), 0, "busy less than a thousandth of the time: no poll at p99.9");
        assert_eq!(polls_ahead_at_p999(1_000_000), u64::MAX, "busy all the time: no bound");
    }

    /// Goal 9 (plan 01a1a87b2cec step 3): a loop's turn ends before the piece that would take it past its quantum,
    /// by the longest piece it has done this turn, not once its quantum has passed: of 100 µs pieces against
    /// 266.666 µs it ends after the second, at 200 µs, where looking for the quantum alone ran a third, to 300 µs.
    #[test]
    fn g9_s3_a_turn_ends_before_the_piece_that_would_pass_its_quantum() {
        let t = Turns::new();
        let at = |us: u64| t.began + Duration::from_micros(us);
        assert!(!t.over_at(at(100), STEP_BUDGET_NS), "100 µs done and the next as long: 200 µs, within");
        assert!(t.over_at(at(200), STEP_BUDGET_NS), "200 µs done and the next as long: 300 µs, past");
        // A piece longer than those before it is what the next is taken to be.
        let t = Turns::new();
        let at = |us: u64| t.began + Duration::from_micros(us);
        assert!(!t.over_at(at(10), STEP_BUDGET_NS) && !t.over_at(at(20), STEP_BUDGET_NS));
        assert!(t.over_at(at(150), STEP_BUDGET_NS), "a 130 µs piece ending at 150 µs: the next as long ends at 280 µs");
        // Off a shard there's no quantum: never over.
        assert!(!Turns::new().over());
    }

    /// Goal 9 (plan 01a1a87b2cec step 3): a turn that's over ends its poll whether or not anything else wants the shard
    /// (`end`), where `after` goes on in the same poll when nothing does; and a piece expected to run past the quantum
    /// waits for the next turn (`before`), never a turn's first, nor one that fits.
    #[test]
    fn g9_s3_a_turn_over_ends_its_poll_and_a_piece_that_wont_fit_waits_for_the_next() -> anyhow::Result<()> {
        // Of 100 µs pieces against 266.666 µs: one done, the next 100 µs and 60 µs more of its read ends at 260 µs, within;
        // 70 µs more, at 270 µs, past. Never before the turn's first piece is done, whatever it's expected to take.
        let t = Turns::new();
        let at = |us: u64| t.began + Duration::from_micros(us);
        assert!(!t.wont_fit_at(at(0), STEP_BUDGET_NS, 10_000_000), "a turn's first piece goes whatever it's expected to take");
        assert!(!t.over_at(at(100), STEP_BUDGET_NS), "100 µs done and the next as long: 200 µs, within");
        assert!(!t.wont_fit_at(at(100), STEP_BUDGET_NS, 60_000), "100 + 100 + 60 µs: within");
        assert!(t.wont_fit_at(at(100), STEP_BUDGET_NS, 70_000), "100 + 100 + 70 µs: past");
        assert_eq!((t.looked_ns.load(Ordering::Relaxed), t.piece_ns.load(Ordering::Relaxed)), (100_000, 100_000), "asking changed nothing: the turn is where it was");
        block_on(async {
            // Over: the poll ends (pending once) though nothing else is on the shard, and the next turn starts afresh.
            let mut turns = Turns::new();
            turns.began = Instant::now().checked_sub(Duration::from_secs(1)).unwrap_or(turns.began);
            {
                let mut end = std::pin::pin!(turns.end());
                anyhow::ensure!(futures::poll!(end.as_mut()).is_pending(), "an over turn's poll ends there");
                anyhow::ensure!(futures::poll!(end.as_mut()).is_ready(), "and goes on at the next");
            }
            anyhow::ensure!(turns.began.elapsed() < Duration::from_secs(1), "the next turn starts afresh");
            // Not over: it goes on in the same poll.
            let mut fresh = Turns::new();
            anyhow::ensure!(futures::poll!(std::pin::pin!(fresh.end())).is_ready(), "a turn with room goes on");
            // A piece done (a microsecond's), then one expected to take a second: given up first; one expected to take
            // nothing goes on.
            let mut turns = Turns::new();
            turns.began = Instant::now().checked_sub(Duration::from_micros(1)).unwrap_or(turns.began);
            anyhow::ensure!(!turns.over(), "a microsecond's piece leaves the turn room");
            anyhow::ensure!(futures::poll!(std::pin::pin!(turns.before(0))).is_ready(), "nothing more expected: it goes on");
            {
                let mut before = std::pin::pin!(turns.before(1_000_000_000));
                anyhow::ensure!(futures::poll!(before.as_mut()).is_pending(), "a piece that won't fit waits for the next turn");
                anyhow::ensure!(futures::poll!(before.as_mut()).is_ready(), "which it starts");
            }
            anyhow::ensure!(futures::poll!(std::pin::pin!(turns.before(1_000_000_000))).is_ready(), "the next turn's first piece goes whatever it's expected to take");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: before a piece bounded as a whole (its reads and its handler), a turn with no room
    /// left for it ends first, its poll with it, and the piece is the next turn's first; the turn's longest piece doesn't
    /// count again (the bound has the piece's all), and a turn's first piece goes whatever its bound.
    #[test]
    fn g9_s3_a_piece_with_no_room_left_in_its_turn_waits_for_the_next() -> anyhow::Result<()> {
        // Against 266.666 µs: 100 µs done, a piece of 166 µs at the most fits; one of 167 µs passes it. The 100 µs piece
        // done doesn't count again, as `before` counts it.
        let t = Turns::new();
        let at = |us: u64| t.began + Duration::from_micros(us);
        assert!(!t.no_room_at(at(0), STEP_BUDGET_NS, u64::MAX), "a turn's first piece goes whatever its bound");
        assert!(!t.over_at(at(100), STEP_BUDGET_NS), "100 µs done and the next as long: 200 µs, within");
        assert!(!t.no_room_at(at(100), STEP_BUDGET_NS, 166_000), "100 + 166 µs: within");
        assert!(t.no_room_at(at(100), STEP_BUDGET_NS, 167_000), "100 + 167 µs: past");
        assert!(t.wont_fit_at(at(100), STEP_BUDGET_NS, 67_000), "`before` takes the longest piece again: 100 + 100 + 67 µs, past");
        assert_eq!((t.looked_ns.load(Ordering::Relaxed), t.piece_ns.load(Ordering::Relaxed)), (100_000, 100_000), "asking changed nothing: the turn is where it was");
        block_on(async {
            // A piece done (a microsecond's), then one bounded at a second: given up first; one bounded at nothing goes on.
            let mut turns = Turns::new();
            turns.began = Instant::now().checked_sub(Duration::from_micros(1)).unwrap_or(turns.began);
            anyhow::ensure!(!turns.over(), "a microsecond's piece leaves the turn room");
            anyhow::ensure!(futures::poll!(std::pin::pin!(turns.room_for(0))).is_ready(), "nothing more: it goes on");
            {
                let mut room = std::pin::pin!(turns.room_for(1_000_000_000));
                anyhow::ensure!(futures::poll!(room.as_mut()).is_pending(), "a piece with no room left waits for the next turn");
                anyhow::ensure!(futures::poll!(room.as_mut()).is_ready(), "which it starts");
            }
            anyhow::ensure!(futures::poll!(std::pin::pin!(turns.room_for(u64::MAX))).is_ready(), "the next turn's first piece goes whatever its bound");
            Ok(())
        })
    }

    /// Goal 9 (plan 01a1a87b2cec step 3): a loop that gives its turns up with `end` ends its poll with its
    /// turn, whether or not anything else wants its shard: of pieces that each take at least `PIECE`, a turn is over
    /// once the time done and a next piece as long reach the quantum, so no poll runs more than `quantum / PIECE`
    /// pieces, rounded up, less one. The host holding the thread only makes pieces longer, and turns shorter.
    #[test]
    fn g9_s3_a_loops_poll_ends_with_its_turn_though_nothing_else_asks_for_its_shard() -> anyhow::Result<()> {
        block_on(async {
            const PIECE: Duration = Duration::from_micros(40);
            let piece_ns = u64::try_from(PIECE.as_nanos()).unwrap_or(u64::MAX);
            let quantum_ns = hyper_rt::futures::step_budget_ns().unwrap_or(0);
            anyhow::ensure!(quantum_ns > 0, "a shard has a quantum");
            // Four quanta of pieces: polls enough to look at.
            let n = quantum_ns.saturating_mul(4) / piece_ns + 1;
            let (done, most_quantum_ns) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(quantum_ns)));
            let work = {
                let (done, most_quantum_ns) = (Arc::clone(&done), Arc::clone(&most_quantum_ns));
                async move {
                    let mut turns = Turns::new();
                    for _ in 0..n {
                        let until = Instant::now() + PIECE;
                        while Instant::now() < until {
                            std::hint::spin_loop();
                        }
                        // The quantum as the turn reads it (the shard's online estimate may move).
                        most_quantum_ns.fetch_max(hyper_rt::futures::step_budget_ns().unwrap_or(0), Ordering::SeqCst);
                        done.fetch_add(1, Ordering::SeqCst);
                        turns.end().await;
                    }
                }
            };
            let mut work = std::pin::pin!(work);
            let (mut polls, mut most_in_a_poll) = (0u64, 0u64);
            std::future::poll_fn(|cx| {
                let before = done.load(Ordering::SeqCst);
                let polled = work.as_mut().poll(cx);
                polls += 1;
                most_in_a_poll = most_in_a_poll.max(done.load(Ordering::SeqCst).saturating_sub(before));
                polled
            })
            .await;
            let bound = most_quantum_ns.load(Ordering::SeqCst).div_ceil(piece_ns).saturating_sub(1);
            anyhow::ensure!(done.load(Ordering::SeqCst) == n, "every piece ran");
            anyhow::ensure!(most_in_a_poll <= bound.max(1), "a poll ran {most_in_a_poll} pieces of at least {PIECE:?}, past its turn's {bound}");
            anyhow::ensure!(polls > 1, "{n} pieces in one poll");
            Ok(())
        })
    }

    /// Goal 10 (plan 01a1a87b2dae step 5): a loop takes an item waiting already in the turn it has, and starts its
    /// turn afresh with one it waited for, as the copies' owners do: the hub's loop counted its idle wait as a
    /// piece of its turn, so the first frame after any quiet spell spent it. Closed and empty, there's none.
    #[test]
    fn g10_s5_a_loops_wait_for_its_next_item_is_no_piece_of_its_turn() -> anyhow::Result<()> {
        block_on(async {
            // Counted as the hub's loop's own queue is (a few bytes, for a moment).
            let (tx, rx) = crate::memory::queue::bounded::<Vec<u8>>(&crate::memory::owners::NET_EVENTS, 1 << 20);
            let mut turns = Turns::new();
            let began = turns.began;
            // Waiting already: taken in the turn the loop has.
            anyhow::ensure!(tx.try_send(vec![1]).is_ok(), "sent");
            anyhow::ensure!(turns.next(&rx).await == Some(vec![1]), "the item waiting");
            anyhow::ensure!(turns.began == began, "an item waiting already goes on in the turn");
            // None waiting: the loop waits (polled once, it's pending), and the item it waited for starts its turn.
            let asked = Instant::now();
            {
                let mut next = std::pin::pin!(turns.next(&rx));
                anyhow::ensure!(futures::poll!(next.as_mut()).is_pending(), "nothing to take yet");
                anyhow::ensure!(tx.try_send(vec![2]).is_ok(), "sent");
                anyhow::ensure!(next.await == Some(vec![2]), "the item it waited for");
            }
            anyhow::ensure!(turns.began >= asked && turns.began != began, "its turn starts as the item it waited for comes");
            anyhow::ensure!(!turns.over_at(turns.began + Duration::from_micros(100), STEP_BUDGET_NS), "a 100 µs piece after the wait is within its turn");
            // Closed and empty: none.
            drop(tx);
            anyhow::ensure!(turns.next(&rx).await.is_none(), "closed and empty");
            Ok(())
        })
    }

    /// Goal 8 (plan 01a1a59c2b29 step 6): with cores of its own the server's shards each get one,
    /// the first core kept for its other threads; more shards than cores go around again, and a
    /// single core takes them all.
    #[test]
    fn g8_s6_shards_are_fixed_to_cores_of_the_servers_own() {
        assert_eq!(shard_cores(&[8, 9, 10, 11], 2), vec![9, 10]);
        assert_eq!(shard_cores(&[8, 9, 10], 4), vec![9, 10, 9, 10]);
        assert_eq!(shard_cores(&[5], 2), vec![5, 5]);
        assert!(shard_cores(&[], 2).is_empty());
    }

    /// Goal 8 (plan 01a1a59c2b29 step 7), goal 9 (plan 01a1a87b2cec step 4): how the shards took the
    /// real-time class is said once all of them answered, and never when it wasn't asked for; a refusal
    /// says why in a person's words, naming the grant `kiln install` writes.
    #[test]
    fn g9_s4_the_real_time_class_is_said_as_the_shards_took_it() {
        let status = |took, refused, why: Option<&str>| Realtime { asked: true, shards: 4, took, refused, why: why.map(str::to_owned) };
        assert_eq!(Realtime { asked: false, shards: 0, took: 0, refused: 0, why: None }.said(), None, "not asked for: nothing to say");
        assert_eq!(status(3, 0, None).said(), None, "not every shard has answered");
        assert!(status(4, 0, None).said().is_some_and(|s| s.contains("4 shards run in Linux's round-robin real-time class at priority 1")));
        assert!(status(0, 4, Some("no room")).said().is_some_and(|s| s.contains("4 shards run at the normal class") && s.contains("no room")));
        assert!(status(1, 3, Some("no room")).said().is_some_and(|s| s.starts_with("1 of the server's 4 shards")));
        // EPERM, as Unix numbers it.
        if cfg!(unix) {
            let eperm = hyper_rt::compute::Refusal { call: "sched_setattr", code: Some(1) };
            let why = realtime_why(&eperm);
            assert!(why.contains("RLIMIT_RTPRIO") && why.contains("LimitRTPRIO=1"), "{why}");
        }
        let elsewhere = hyper_rt::compute::Refusal { call: "no real-time class to ask for on macOS", code: None };
        assert_eq!(realtime_why(&elsewhere), "no real-time class to ask for on macOS");
        // Both kinds of answer counted in one atomic, each read back apart.
        let answers = 3 + 2 * REFUSED_ONE;
        assert_eq!((answers % REFUSED_ONE, answers / REFUSED_ONE), (3, 2));
    }

    /// Goal 8 (plan 01a1a59c2b29 step 7): the nudge takes the real-time class's place where that's refused or
    /// not asked for, and comes after it where it's granted: a shard in the real-time class asks for nothing
    /// more, neither a slice nor a nudge; one at the normal class asks for its slice, and the nudge only where
    /// `[server] nudge` asks for it.
    #[test]
    fn g8_s7_a_shard_nudges_in_the_real_time_class_s_place() {
        assert_eq!(shard_asks(true, true), ShardAsks { slice: false, nudge: false }, "granted the real-time class: neither");
        assert_eq!(shard_asks(true, false), ShardAsks { slice: false, nudge: false });
        assert_eq!(shard_asks(false, true), ShardAsks { slice: true, nudge: true }, "refused it, or not asked: the slice and the nudge");
        assert_eq!(shard_asks(false, false), ShardAsks { slice: true, nudge: false }, "the nudge only where asked for");
    }

    /// Goal 8 (plan 01a1a59c2b29 step 7), goal 9 (plan 01a1a87b2cec step 4): the nudge's period is the step
    /// quantum less the timers' tick, the slack the kernel adds to each wait, so a nudged shard's CPU takes an
    /// interrupt of its own within a quantum of the last while it waits: 166.666 µs at kiln's, where the tick as
    /// the period woke each waiting shard a third again as often. Never under the tick; the lab may set another
    /// in microseconds, and a value that isn't a positive count leaves the derived one.
    #[test]
    fn g9_s4_the_nudge_s_period_is_the_quantum_less_the_timers_slack() {
        let c = runtime_config(4, 64);
        let derived = nudge_period_ns(c.step_budget_ns, c.timer_tick_ns, None);
        assert_eq!(derived, 166_666);
        assert!(derived + c.timer_tick_ns <= c.step_budget_ns, "each nudge's interrupt within a quantum of the last");
        assert_eq!(1_000_000_000 / (derived + c.timer_tick_ns), 3_750, "wakes a second of a waiting shard");
        assert_eq!(1_000_000_000 / (2 * c.timer_tick_ns), 5_000, "with the tick as the period");
        assert_eq!(nudge_period_ns(150_000, TIMER_TICK_NS, None), TIMER_TICK_NS, "a quantum under two ticks: the tick");
        assert_eq!(nudge_period_ns(c.step_budget_ns, c.timer_tick_ns, Some("50")), 50_000);
        assert_eq!(nudge_period_ns(c.step_budget_ns, c.timer_tick_ns, Some(" 250 ")), 250_000);
        assert_eq!(nudge_period_ns(c.step_budget_ns, c.timer_tick_ns, Some("0")), derived, "0 isn't a period");
        assert_eq!(nudge_period_ns(c.step_budget_ns, c.timer_tick_ns, Some("soon")), derived);
    }

    /// Goal 9 (plan 01a1a87b2cec step 4): the spin before a park is off unless the lab asks for it: `KILN_RT_SPIN` on
    /// asks for the spin each shard's own gaps size, up to 16 µs; `KILN_RT_SPIN_US` for a fixed one, which goes first;
    /// anything else is none, as before.
    #[test]
    fn g9_s4_the_spin_before_a_park_is_the_labs_to_ask_for() {
        use hyper_rt::park_spin::{Ask, CAP_NS};
        assert_eq!(spin_ask(None, None), Ask::Off);
        for off in ["", "0", "false", "off", "no", " off "] {
            assert_eq!(spin_ask(Some(off), None), Ask::Off, "{off:?}");
        }
        for on in ["1", "on", "true", "auto", " yes "] {
            assert_eq!(spin_ask(Some(on), None), Ask::Sized { cap_ns: CAP_NS }, "{on:?}");
        }
        assert_eq!(CAP_NS, 16_000);
        assert_eq!(spin_ask(None, Some("16")), Ask::Fixed { ns: 16_000 });
        assert_eq!(spin_ask(Some("off"), Some(" 4 ")), Ask::Fixed { ns: 4_000 }, "a fixed spin asked for goes first");
        assert_eq!(spin_ask(Some("1"), Some("0")), Ask::Sized { cap_ns: CAP_NS }, "0 isn't a spin's length");
        assert_eq!(spin_ask(None, Some("soon")), Ask::Off);
    }

    /// Goal 9 (plan 01a1a87b2cec step 4): how the shards took the spin is said once every shard has answered, never
    /// when none was asked for; what was asked reads back from its ask, and the answers count in one atomic.
    #[test]
    fn g9_s4_the_spin_is_said_as_the_shards_took_it() {
        use hyper_rt::park_spin::Ask;
        let status = |asked, us, took| Spin { asked, us, shards: 4, took };
        assert_eq!(Spin { asked: "off", us: 0, shards: 0, took: 0 }.said(), None, "not asked for: nothing to say");
        assert_eq!(status("sized", 16, 3).said(), None, "not every shard has answered");
        assert!(status("sized", 16, 4).said().is_some_and(|s| s.contains("4 shards spin before each park") && s.contains("up to 16 µs")));
        assert!(status("fixed", 8, 4).said().is_some_and(|s| s.contains("spin 8 µs before each park") && s.contains("KILN_RT_SPIN_US")));
        let sized = spin_of(Ask::Sized { cap_ns: 16_000 }, 2 + REFUSED_ONE);
        assert_eq!((sized.asked, sized.us, sized.took), ("sized", 16, 2));
        assert_eq!((spin_of(Ask::Fixed { ns: 8_000 }, 0).asked, spin_of(Ask::Off, 0).asked), ("fixed", "off"));
    }

    /// Goal 8 (plan 01a1a59c2b29 step 7): how the shards took the nudge is said once all of them answered,
    /// never when it wasn't asked for or before the runtime started; those in the real-time class are said apart,
    /// and a refusal says why. Three kinds of answer count in one atomic, each read back apart.
    #[test]
    fn g8_s7_the_nudge_is_said_as_the_shards_took_it() {
        let status = |nudged, realtime, refused, why: Option<&str>| Nudge { asked: true, mode: crate::nudge::Mode::On, period_us: 100, shards: 4, nudged, realtime, refused, why: why.map(str::to_owned), armed: 0, starts: 0 };
        assert_eq!(Nudge { asked: false, ..status(4, 0, 0, None) }.said(), None, "not asked for: nothing to say");
        assert_eq!(Nudge { shards: 0, ..status(0, 0, 0, None) }.said(), None, "before the runtime started");
        assert_eq!(status(3, 0, 0, None).said(), None, "not every shard has answered");
        assert!(status(4, 0, 0, None).said().is_some_and(|s| s.starts_with("the server's 4 shards nudge their CPUs") && s.contains("100 µs")));
        assert!(status(0, 4, 0, None).said().is_some_and(|s| s.contains("4 shards took the real-time class and need no nudge")));
        assert!(status(1, 3, 0, None).said().is_some_and(|s| s.starts_with("1 of the server's 4 shards nudge") && s.contains("3 took the real-time class")));
        assert!(status(0, 0, 4, Some("no lazy preemption to nudge on macOS")).said().is_some_and(|s| s.contains("4 run without it") && s.contains("macOS")));
        let elsewhere = hyper_rt::compute::Refusal { call: "no lazy preemption to nudge on macOS", code: None };
        assert_eq!(nudge_why(&elsewhere), "no lazy preemption to nudge on macOS");
        let answers = 2 + 3 * NUDGE_REALTIME_ONE + 64 * NUDGE_REFUSED_ONE;
        assert_eq!((answers % NUDGE_REALTIME_ONE, (answers / NUDGE_REALTIME_ONE) % NUDGE_REALTIME_ONE, answers / NUDGE_REFUSED_ONE), (2, 3, 64));
    }

    /// Goal 8 (plan 01a1a59c2b29 step 7) and goal 6 (plan 01a1a59c2abe step 3): `auto`, the mode a config declares, is said as
    /// nudging only while a shard's host holds its wakes, with the threshold it judges by; `/healthz/server` carries the
    /// mode as its word and the shards nudging now; a process that never set it (a runner, a command) doesn't nudge.
    #[test]
    fn g6_s3_auto_is_said_as_nudging_only_while_held() -> anyhow::Result<()> {
        let auto = Nudge { asked: true, mode: crate::nudge::Mode::Auto, period_us: 100, shards: 4, nudged: 4, realtime: 0, refused: 0, why: None, armed: 1, starts: 3 };
        let said = auto.said().unwrap_or_default();
        anyhow::ensure!(said.starts_with("the server's 4 shards nudge their CPUs while their host holds their wakes") && said.contains("17.2 µs") && said.contains("a calm shard never nudges"), "{said}");
        let json = serde_json::to_value(&auto)?;
        anyhow::ensure!(json.get("mode").and_then(serde_json::Value::as_str) == Some("auto") && json.get("armed") == Some(&serde_json::json!(1)), "{json}");
        anyhow::ensure!(nudge_mode() == crate::nudge::Mode::Off, "off until the server's config says");
        Ok(())
    }

    /// `each` keeps the items' order, runs no more than `n` at once, runs them at once (four
    /// 50 ms waits take ~one), and polls them under the task's own waker: a hyper-rt connect inside
    /// it works, as it would not under futures' `buffered`.
    #[test]
    fn each_runs_n_at_once_in_order_under_the_task_s_own_waker() -> anyhow::Result<()> {
        block_on(async {
            let running = Arc::new(AtomicUsize::new(0));
            let most = Arc::new(AtomicUsize::new(0));
            let t = Instant::now();
            let out = each(0..12u64, 4, |i| {
                let (running, most) = (running.clone(), most.clone());
                async move {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(now, Ordering::SeqCst);
                    // Later items finish first: the order kept is the items', not the finishing.
                    sleep(Duration::from_millis(50 - (i % 4) * 10)).await;
                    running.fetch_sub(1, Ordering::SeqCst);
                    i * 10
                }
            })
            .await;
            assert_eq!(out, (0..12).map(|i| i * 10).collect::<Vec<_>>());
            assert_eq!(most.load(Ordering::SeqCst), 4);
            assert!(t.elapsed() < Duration::from_millis(400), "three rounds of 50 ms, not twelve: {:?}", t.elapsed());
            assert!(each(Vec::<u8>::new(), 3, |x| async move { x }).await.is_empty());
            // A connect under `each`: refused by the port, not by the runtime.
            let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| anyhow::anyhow!("{e}"))?;
            let addr = listener.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
            let http = crate::http::client::Client::public()?;
            let url = format!("http://{addr}/");
            let got = each([0u8, 1], 2, |_| async { http.get(&url).timeout(Duration::from_millis(300)).send().await.map(|_| ()).map_err(|e| format!("{e:#}")) }).await;
            for r in &got {
                assert!(r.as_ref().err().is_none_or(|e| !e.contains("not on a shard thread")), "{r:?}");
            }
            Ok(())
        })
    }

    /// Outputs handed over in the items' order as they become ready, not all at the end; an item
    /// begun only once a place is free and what came before was handed over (so it sees what the
    /// handing changed); `n` at once at the most, ready-and-waiting ones counted; a connect works.
    #[test]
    fn each_in_order_hands_each_over_in_turn_and_begins_the_next_after() -> anyhow::Result<()> {
        block_on(async {
            let running = Arc::new(AtomicUsize::new(0));
            let most = Arc::new(AtomicUsize::new(0));
            let handed = Mutex::new(Vec::<u64>::new());
            // What each item saw handed over when it was begun.
            let seen = Mutex::new(Vec::<(u64, usize)>::new());
            let t = Instant::now();
            each_in_order(
                0..12u64,
                3,
                |i| {
                    let (running, most) = (running.clone(), most.clone());
                    seen.lock().unwrap_or_else(PoisonError::into_inner).push((i, handed.lock().unwrap_or_else(PoisonError::into_inner).len()));
                    async move {
                        let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                        most.fetch_max(now, Ordering::SeqCst);
                        // Later items finish first: the order handed over is the items'.
                        sleep(Duration::from_millis(40 - (i % 3) * 10)).await;
                        running.fetch_sub(1, Ordering::SeqCst);
                        i * 10
                    }
                },
                |r| handed.lock().unwrap_or_else(PoisonError::into_inner).push(r),
            )
            .await;
            assert_eq!(*handed.lock().unwrap_or_else(PoisonError::into_inner), (0..12).map(|i| i * 10).collect::<Vec<_>>());
            assert!(most.load(Ordering::SeqCst) <= 3);
            // Three places: item i is begun only once i - 2 were handed over.
            for (i, before) in seen.lock().unwrap_or_else(PoisonError::into_inner).iter() {
                assert!(*before as u64 + 2 >= *i, "item {i} begun with {before} handed over");
            }
            assert!(t.elapsed() < Duration::from_millis(600), "about four rounds of 40 ms: {:?}", t.elapsed());
            let none = Mutex::new(0);
            each_in_order(Vec::<u8>::new(), 3, |x| async move { x }, |_| *none.lock().unwrap_or_else(PoisonError::into_inner) += 1).await;
            assert_eq!(*none.lock().unwrap_or_else(PoisonError::into_inner), 0);
            let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| anyhow::anyhow!("{e}"))?;
            let addr = listener.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
            let http = crate::http::client::Client::public()?;
            let url = format!("http://{addr}/");
            let got = Mutex::new(Vec::new());
            each_in_order(
                [0u8, 1],
                2,
                |_| async { http.get(&url).timeout(Duration::from_millis(300)).send().await.map(|_| ()).map_err(|e| format!("{e:#}")) },
                |r| got.lock().unwrap_or_else(PoisonError::into_inner).push(r),
            )
            .await;
            for r in got.lock().unwrap_or_else(PoisonError::into_inner).iter() {
                assert!(r.as_ref().err().is_none_or(|e| !e.contains("not on a shard thread")), "{r:?}");
            }
            Ok(())
        })
    }

    /// Goal 6, plan 01a1a59c2abe step 1: kiln's own primitives charge the shard's budget, so a loop draining a
    /// queue that always holds its next item (`recv`) yields at its share, and a timer due on its shard fires while
    /// it loops, where a loop on the queue's own receive would hold the shard until it ended.
    #[test]
    fn g6_s1_a_loop_on_kiln_s_charged_receive_lets_its_shard_s_timer_fire() -> anyhow::Result<()> {
        block_on(async {
            let (tx, rx) = crate::memory::queue::unbounded::<u64>(&crate::memory::owners::CHANNELS);
            let fired = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&fired);
            // Spawned from a task: on this shard.
            let _timer = spawn(async move {
                sleep(Duration::from_millis(5)).await;
                flag.store(true, Ordering::SeqCst);
            });
            let started = Instant::now();
            let mut items = 0u64;
            while !fired.load(Ordering::SeqCst) && started.elapsed() < Duration::from_secs(5) {
                // The channel always holds the next item: the receive never waits.
                let _ = tx.try_send(items);
                let _ = recv(&rx).await;
                items += 1;
                let until = Instant::now() + Duration::from_micros(10);
                while Instant::now() < until {
                    std::hint::spin_loop();
                }
            }
            anyhow::ensure!(fired.load(Ordering::SeqCst), "the timer fired while the loop ran ({items} items, {:?})", started.elapsed());
            Ok(())
        })
    }

    #[test]
    fn timers_spawn_and_cancellation() -> anyhow::Result<()> {
        block_on(async {
            let t = Instant::now();
            sleep(Duration::from_millis(30)).await;
            assert!(t.elapsed() >= Duration::from_millis(30));

            assert_eq!(timeout(Duration::from_millis(20), futures::future::pending::<()>()).await, Err(Elapsed));
            assert_eq!(timeout(Duration::from_secs(5), async { 7 }).await, Ok(7));

            let mut iv = Interval::new(Duration::from_millis(10));
            let t = Instant::now();
            for _ in 0..4 {
                iv.tick().await;
            }
            assert!(t.elapsed() >= Duration::from_millis(29));

            let h = spawn(async { 40 + 2 });
            assert_eq!(h.await.ok(), Some(42));

            let sd = Shutdown::new();
            let waiter = {
                let sd = sd.clone();
                spawn(async move { sd.cancelled().await })
            };
            let sd2 = sd.clone();
            spawn(async move {
                sleep(Duration::from_millis(10)).await;
                sd2.cancel();
            });
            assert!(timeout(Duration::from_secs(5), waiter).await.is_ok());
            assert!(!sd.sleep(Duration::from_secs(5)).await);
            assert!(Shutdown::new().sleep(Duration::from_millis(5)).await);

            let n = Arc::new(Notify::new());
            n.notify();
            timeout(Duration::from_secs(1), n.notified()).await?;

            let b = Broadcast::<u32>::new(4);
            let mut r = b.subscribe();
            b.send(1);
            assert_eq!(r.recv().await.ok(), Some(1));

            let v = blocking(|| 5 * 5).await?;
            assert_eq!(v, 25);
            let a = resolve("localhost:80").await?;
            assert_eq!(a.port(), 80);
            Ok(())
        })
    }

    /// Goal 3, plan 01a1a59c2a8e step 1: CPU-heavy work goes to the compute pool, started with the
    /// runtime: it runs on none of the shards, at a lower OS priority than theirs, and its result comes
    /// back to the task that asked; a job in pieces goes on to its end.
    #[test]
    fn g3_s1_compute_runs_off_the_shards_below_them_and_answers() -> anyhow::Result<()> {
        block_on(async {
            anyhow::ensure!(hyper_rt::compute::running(), "the compute pool started with the runtime");
            let here = hyper_rt::compute::thread_priority();
            let (on_a_shard, there) = compute(|| (on_shard(), hyper_rt::compute::thread_priority())).await?;
            anyhow::ensure!(!on_a_shard, "the job ran on a shard");
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            anyhow::ensure!(there.is_below(here), "the pool's thread at {there}, the shard's at {here}");
            #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
            let _ = (here, there);
            let sum = compute_pieces((0u64, 0u64), |(i, sum), turn| {
                while *i < 1_000_000 {
                    *sum += *i;
                    *i += 1;
                    if turn.over() {
                        return Step::More;
                    }
                }
                Step::Done(*sum)
            })
            .await?;
            anyhow::ensure!(sum == 499_999_500_000, "{sum}");
            Ok(())
        })
    }

    /// Goal 3, plan 01a1a59c2a8e step 3: a thread of its own (the conversation feed's intake) hands a job
    /// in pieces to the compute pool and waits there for its result: the job runs on a pool thread, below
    /// the caller, a piece at a time to its end; and its result comes back however many pieces it took.
    #[test]
    fn g3_s3_a_thread_of_its_own_waits_for_a_compute_job_in_pieces() -> anyhow::Result<()> {
        let here = hyper_rt::compute::thread_priority();
        // Three pieces, the last saying where it ran.
        let (thread, there, pieces) = compute_pieces_blocking(0u32, |pieces, _turn| {
            *pieces += 1;
            if *pieces < 3 { Step::More } else { Step::Done((std::thread::current().name().map(str::to_owned), hyper_rt::compute::thread_priority(), *pieces)) }
        })?;
        anyhow::ensure!(hyper_rt::compute::running(), "the compute pool started with the runtime");
        anyhow::ensure!(pieces == 3, "{pieces} pieces");
        anyhow::ensure!(thread.as_deref().is_some_and(|t| t.starts_with("kiln-compute-")), "the job ran on {thread:?}");
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        anyhow::ensure!(there.is_below(here), "the pool's thread at {there}, the caller's at {here}");
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        let _ = (here, there);
        Ok(())
    }

    /// Goal 3, plan 01a1a59c2a8e step 4: the blocking pool's places from `KILN_RT_BLOCKING`, `dns,store,fs`
    /// (the lab's A/B of share sizes on one build): three counts of at least one, or nothing taken.
    #[test]
    fn g3_s4_the_blocking_pools_places_from_the_environment() {
        assert_eq!(places("4,2,8"), Some((4, 2, 8)));
        assert_eq!(places(" 1, 3 ,12"), Some((1, 3, 12)));
        for refused in ["", "4,2", "4,2,8,1", "0,2,8", "a,2,8", "4,,8", "-1,2,8"] {
            assert_eq!(places(refused), None, "{refused:?}");
        }
    }

    /// Goal 3, plan 01a1a59c2a8e step 4: the server's shares from what the lab measured: the jobs a share
    /// holds at once by Little's law, rounded up to whole places, at least one; the store's reads too, each
    /// on a connection of its own (goal 5, plan 01a1a59c2aae step 2).
    #[test]
    fn g3_s4_the_servers_shares_are_derived() {
        assert_eq!(AtOnce(25.54).places(), 26);
        assert_eq!(AtOnce(3.0).places(), 3);
        assert_eq!(AtOnce(1.01).places(), 2);
        for none in [0.0, 0.4, 1.0, -3.0, f64::NAN, f64::INFINITY] {
            assert_eq!(AtOnce(none).places(), 1, "{none}");
        }
        let (dns, store, fs) = Profile::server().pool;
        assert_eq!((dns, store, fs), (SERVER_DNS, SERVER_STORE.places(), SERVER_FS.places()));
    }

    /// kiln's runtime passes kiln-seams' Clock suite on real time (CP-M2, AC-M2.2): the clock the
    /// goal driver and the harness host take their time by in production (AC-M2.1).
    struct RtClockFixture;

    impl kiln_seams::clock::ClockFixture for RtClockFixture {
        fn clock(&self) -> Arc<dyn kiln_seams::Clock> {
            clock()
        }
        fn pass(&self, d: Duration) -> futures::future::BoxFuture<'_, ()> {
            Box::pin(sleep(d))
        }
        fn quantum(&self) -> Duration {
            Duration::from_millis(50)
        }
    }

    /// CP-M2, AC-M2.1 and AC-M2.2: production's clock passes the Clock suite.
    #[test]
    fn the_runtime_passes_the_clock_suite() -> anyhow::Result<()> {
        block_on(async {
            let cases = kiln_seams::clock::cases::<RtClockFixture>();
            // Each case fails, named, if it hasn't ended in 30 s.
            let n = kiln_seams::conformance::run_within("Clock (kiln's runtime)", &cases, || async { RtClockFixture }, || sleep(Duration::from_secs(30))).await.into_result().map_err(anyhow::Error::msg)?;
            anyhow::ensure!(n == cases.len());
            Ok(())
        })
    }

    struct RtSpawnFixture;

    impl kiln_seams::spawn::SpawnFixture for RtSpawnFixture {
        fn spawner(&self) -> Arc<dyn kiln_seams::Spawn> {
            Arc::new(RtSpawn)
        }
        fn finish<'a, T: Send + 'a>(&'a self, fut: futures::future::BoxFuture<'a, T>) -> futures::future::BoxFuture<'a, Option<T>> {
            Box::pin(async move { timeout(Duration::from_secs(10), fut).await.ok() })
        }
    }

    /// CP-M2, AC-M2.2: kiln's runtime passes the Spawn suite.
    #[test]
    fn the_runtime_passes_the_spawn_suite() -> anyhow::Result<()> {
        block_on(async {
            let cases = kiln_seams::spawn::cases::<RtSpawnFixture>();
            let n = kiln_seams::conformance::run_within("Spawn (kiln's runtime)", &cases, || async { RtSpawnFixture }, || sleep(Duration::from_secs(30))).await.into_result().map_err(anyhow::Error::msg)?;
            anyhow::ensure!(n == cases.len());
            Ok(())
        })
    }

    #[test]
    fn files_round_trip() -> anyhow::Result<()> {
        block_on(async {
            let dir = tempfile::tempdir()?;
            let p = dir.path().join("a/b.bin");
            let (tx, done) = file_sink(p.clone(), 10 << 20)?;
            let data: Vec<u8> = (0..(3 << 20)).map(|i: u32| (i % 251) as u8).collect();
            for chunk in data.chunks(70_000) {
                tx.send(Bytes::copy_from_slice(chunk)).await?;
            }
            drop(tx);
            assert_eq!(done.await?, u64::try_from(data.len())?);
            let rx = file_source(&p)?;
            let mut back = Vec::new();
            while let Ok(c) = rx.recv().await {
                back.extend_from_slice(&c);
            }
            assert_eq!(back, data);

            let (tx, done) = file_sink(dir.path().join("small"), 10)?;
            let _ = tx.send(Bytes::from_static(b"0123456789abc")).await;
            drop(tx);
            assert!(done.await.is_err());
            assert!(file_source(dir.path().join("missing")).is_err());
            Ok(())
        })
    }

    /// The code of [`Shutdown`] and [`Notify`] and of their waits, outside comments: from rt.rs's shutdown
    /// section's head to its broadcast section's.
    fn shutdown_and_notify_code() -> String {
        let src = include_str!("rt.rs").replace("\r\n", "\n");
        let section = src.split("--- shutdown\n").nth(1).and_then(|rest| rest.split("--- broadcast\n").next()).unwrap_or_default();
        section.lines().map(|l| l.split("//").next().unwrap_or_default()).collect::<Vec<_>>().join("\n")
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal), run by kiln's CI with every test: a notifier, a shutdown and a
    /// changed take no lock and wait on nothing that does. Their waits were event-listener's, whose list sits behind
    /// a `Mutex` every wait and every wake took: goal 5's census of train 8's tree found it the one lock the shards
    /// still shared.
    #[test]
    fn g5_notify_and_shutdown_take_no_lock() {
        let code = shutdown_and_notify_code();
        assert!(code.contains("pub struct Notify") && code.contains("pub struct Shutdown") && code.contains("pub struct Changed"), "not Notify's, Shutdown's and Changed's code:\n{code}");
        for lock in ["Mutex", "RwLock", "Condvar", "parking_lot", "async_lock", "event_listener", "Event::new", "async_channel"] {
            assert!(!code.contains(lock), "rt::Notify, rt::Shutdown or rt::Changed takes a lock, or waits on something that does: {lock}");
        }
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal): a notify before its wait isn't lost, and two before a wait are
    /// one permit, which that wait takes; the next wait waits for the next notify.
    #[test]
    fn g5_a_notify_before_its_wait_is_kept_and_taken_once() -> anyhow::Result<()> {
        block_on(async {
            let n = Notify::new();
            n.notify();
            n.notify();
            timeout(Duration::from_secs(1), n.notified()).await?;
            anyhow::ensure!(!n.permit.load(Ordering::SeqCst), "two notifies before a wait were more than one permit");
            let mut next = std::pin::pin!(n.notified());
            anyhow::ensure!(futures::poll!(next.as_mut()).is_pending(), "a wait after the permit was taken ended at once");
            n.notify();
            timeout(Duration::from_secs(1), next).await?;
            Ok(())
        })
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal): notifies from another shard lose no wake. Two tasks hand a turn
    /// back and forth through two notifiers, each wait listing its place while the other side's notify may be on
    /// its way (the list's Dekker handshake has one of them see the other), and every turn is taken. Each side
    /// waits on one notifier one wait at a time, so each keeps one place, reused.
    #[test]
    fn g5_notifies_from_another_shard_lose_no_wake() -> anyhow::Result<()> {
        block_on(async {
            let (ping, pong) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
            let rounds = 5_000u32;
            let (p, q) = (Arc::clone(&ping), Arc::clone(&pong));
            let other = spawn_spread_joined(async move {
                for _ in 0..rounds {
                    p.notified().await;
                    q.notify();
                }
            });
            for round in 0..rounds {
                ping.notify();
                timeout(Duration::from_secs(5), pong.notified()).await.map_err(|_| anyhow::anyhow!("round {round}: a wake was lost"))?;
            }
            timeout(Duration::from_secs(5), other).await?.map_err(|_| anyhow::anyhow!("the other side ended"))?;
            for (what, n) in [("ping", &ping), ("pong", &pong)] {
                anyhow::ensure!(n.waits.list.listed() <= 1 && n.waits.spares.len() <= 1, "{what}: {} places listed, {} spare", n.waits.list.listed(), n.waits.spares.len());
            }
            Ok(())
        })
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal): a loop that waits on a notifier and a shutdown each turn beside an
    /// arm that wins (a terminal's relay, each loop on the server's shutdown) keeps one place on each, not one more
    /// a turn; and its next waits on them are woken, from another shard, by the notify and the cancel after.
    #[test]
    fn g5_waits_given_up_each_turn_keep_one_place() -> anyhow::Result<()> {
        use futures::FutureExt as _;
        block_on(async {
            let (n, sd) = (Arc::new(Notify::new()), Shutdown::new());
            for _ in 0..10_000 {
                let ev = futures::select_biased! {
                    () = sd.cancelled().fuse() => "cancelled",
                    () = n.notified().fuse() => "notified",
                    () = futures::future::ready(()).fuse() => "turn",
                };
                anyhow::ensure!(ev == "turn", "{ev}, with neither rung");
            }
            for (what, waits) in [("the notifier", &n.waits), ("the shutdown", &sd.0.waits)] {
                anyhow::ensure!(waits.list.listed() <= 1 && waits.spares.len() <= 1, "{what}: {} places listed, {} spare after 10,000 turns", waits.list.listed(), waits.spares.len());
            }
            let (n2, sd2) = (Arc::clone(&n), sd.clone());
            let ringing = spawn_spread_joined(async move {
                sleep(Duration::from_millis(20)).await;
                n2.notify();
                sleep(Duration::from_millis(20)).await;
                sd2.cancel();
            });
            timeout(Duration::from_secs(5), n.notified()).await?;
            timeout(Duration::from_secs(5), sd.cancelled()).await?;
            timeout(Duration::from_secs(5), ringing).await?.map_err(|_| anyhow::anyhow!("the ringer ended"))?;
            Ok(())
        })
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal): a cancel wakes every wait once, on every shard. Tasks wait at
    /// once, half of them after giving up a wait first (as a loop's turn does, its place taken up again); none
    /// ends before the cancel, each ends once after it, a second cancel changes nothing, and a wait after it ends
    /// at once.
    #[test]
    fn g5_a_cancel_wakes_every_wait_once() -> anyhow::Result<()> {
        block_on(async {
            let sd = Shutdown::new();
            let (waiters, ended) = (64usize, Arc::new(AtomicUsize::new(0)));
            let mut waiting = Vec::new();
            for i in 0..waiters {
                let (sd, ended) = (sd.clone(), Arc::clone(&ended));
                waiting.push(spawn_spread_joined(async move {
                    if i % 2 == 0 {
                        let _ = timeout(Duration::from_millis(1), sd.cancelled()).await;
                    }
                    sd.cancelled().await;
                    ended.fetch_add(1, Ordering::SeqCst);
                }));
            }
            sleep(Duration::from_millis(50)).await;
            anyhow::ensure!(ended.load(Ordering::SeqCst) == 0, "a wait ended before the cancel");
            sd.cancel();
            sd.cancel();
            for w in waiting {
                timeout(Duration::from_secs(5), w).await?.map_err(|_| anyhow::anyhow!("a waiter ended without its wake"))?;
            }
            anyhow::ensure!(ended.load(Ordering::SeqCst) == waiters, "{} of {waiters} waits ended", ended.load(Ordering::SeqCst));
            // Every wait over, every place is a spare (some listed still, for a wake that wakes no one), and there
            // were never more of them than waits at once.
            let (listed, spares) = (sd.0.waits.list.listed(), sd.0.waits.spares.len());
            anyhow::ensure!(listed <= spares && spares <= waiters, "{listed} places listed, {spares} spare, for {waiters} waits");
            timeout(Duration::from_millis(100), sd.cancelled()).await?;
            Ok(())
        })
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal): a notify wakes every wait listed, and one of them takes its
    /// permit. Of tasks waiting on one notifier on every shard, each notify ends one wait, never two, until all
    /// have ended.
    #[test]
    fn g5_each_notify_ends_one_wait_of_many() -> anyhow::Result<()> {
        block_on(async {
            let n = Arc::new(Notify::new());
            let (waiters, ended) = (16usize, Arc::new(AtomicUsize::new(0)));
            let mut waiting = Vec::new();
            for _ in 0..waiters {
                let (n, ended) = (Arc::clone(&n), Arc::clone(&ended));
                waiting.push(spawn_spread_joined(async move {
                    n.notified().await;
                    ended.fetch_add(1, Ordering::SeqCst);
                }));
            }
            sleep(Duration::from_millis(50)).await;
            anyhow::ensure!(ended.load(Ordering::SeqCst) == 0, "a wait ended before any notify");
            for k in 1..=waiters {
                n.notify();
                let until = Instant::now() + Duration::from_secs(5);
                while ended.load(Ordering::SeqCst) < k {
                    anyhow::ensure!(Instant::now() < until, "notify {k} ended no wait");
                    sleep(Duration::from_millis(1)).await;
                }
                sleep(Duration::from_millis(5)).await;
                anyhow::ensure!(ended.load(Ordering::SeqCst) == k, "notify {k} ended {} waits in all", ended.load(Ordering::SeqCst));
            }
            for w in waiting {
                timeout(Duration::from_secs(5), w).await?.map_err(|_| anyhow::anyhow!("a waiter ended without its notify"))?;
            }
            Ok(())
        })
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal): a changed's notify ends every wait begun before it, on every shard,
    /// one begun before it and awaited after it among them; it keeps nothing for a wait begun after it. Every wait
    /// over, its places are spares, never more than the waits at once.
    #[test]
    fn g5_a_changed_ends_every_wait_begun_before_it_and_keeps_nothing() -> anyhow::Result<()> {
        block_on(async {
            let c = Arc::new(Changed::new());
            let early = c.listen();
            c.notify();
            timeout(Duration::from_secs(1), early).await?;
            let mut after = std::pin::pin!(c.listen());
            anyhow::ensure!(futures::poll!(after.as_mut()).is_pending(), "a wait begun after a notify ended without one");
            let (waiters, listening, ended) = (32usize, Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
            let mut waiting = Vec::new();
            for _ in 0..waiters {
                let (c, listening, ended) = (Arc::clone(&c), Arc::clone(&listening), Arc::clone(&ended));
                waiting.push(spawn_spread_joined(async move {
                    let l = c.listen();
                    listening.fetch_add(1, Ordering::SeqCst);
                    l.await;
                    ended.fetch_add(1, Ordering::SeqCst);
                }));
            }
            let until = Instant::now() + Duration::from_secs(5);
            while listening.load(Ordering::SeqCst) < waiters {
                anyhow::ensure!(Instant::now() < until, "{} of {waiters} listening after 5 s", listening.load(Ordering::SeqCst));
                sleep(Duration::from_millis(1)).await;
            }
            sleep(Duration::from_millis(20)).await;
            anyhow::ensure!(ended.load(Ordering::SeqCst) == 0, "a wait ended before the notify");
            c.notify();
            for w in waiting {
                timeout(Duration::from_secs(5), w).await?.map_err(|_| anyhow::anyhow!("a waiter ended without its wake"))?;
            }
            anyhow::ensure!(ended.load(Ordering::SeqCst) == waiters, "{} of {waiters} waits ended", ended.load(Ordering::SeqCst));
            timeout(Duration::from_secs(1), after).await?;
            let (listed, spares) = (c.waits.list.listed(), c.waits.spares.len());
            anyhow::ensure!(listed <= spares && spares <= waiters + 1, "{listed} places listed, {spares} spare, for {} waits at most", waiters + 1);
            Ok(())
        })
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal): a thread's wait on a changed (the conversation store's senders'
    /// room) ends at its deadline with no notify, at once for a notify after its listen, and at a notify from a shard.
    #[test]
    fn g5_a_threads_wait_on_a_changed_ends_at_a_notify_or_its_deadline() -> anyhow::Result<()> {
        let c = Arc::new(Changed::new());
        let began = Instant::now();
        anyhow::ensure!(!c.listen().wait_deadline(began + Duration::from_millis(30)), "a wait ended with no notify");
        anyhow::ensure!(began.elapsed() >= Duration::from_millis(30), "a wait ended before its deadline with no notify");
        let begun = c.listen();
        c.notify();
        anyhow::ensure!(begun.wait_deadline(Instant::now() + Duration::from_secs(5)), "a notify after the listen was lost");
        let c2 = Arc::clone(&c);
        let ringer = std::thread::spawn(move || {
            block_on(async move {
                let until = Instant::now() + Duration::from_secs(5);
                while c2.waits.list.listed() == 0 {
                    anyhow::ensure!(Instant::now() < until, "the thread's wait never listed its place");
                    sleep(Duration::from_millis(1)).await;
                }
                c2.notify();
                Ok(())
            })
        });
        anyhow::ensure!(c.listen().wait_deadline(Instant::now() + Duration::from_secs(10)), "a notify from a shard didn't end a thread's wait");
        ringer.join().map_err(|_| anyhow::anyhow!("the ringer panicked"))??;
        Ok(())
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal): a loop that listens each turn beside an arm that wins (the records'
    /// waits beside their callers' timeouts) keeps one place, not one more a turn; its next wait is woken from
    /// another shard.
    #[test]
    fn g5_waits_on_a_changed_given_up_each_turn_keep_one_place() -> anyhow::Result<()> {
        use futures::FutureExt as _;
        block_on(async {
            let c = Arc::new(Changed::new());
            for _ in 0..10_000 {
                let ev = futures::select_biased! {
                    () = c.listen().fuse() => "changed",
                    () = futures::future::ready(()).fuse() => "turn",
                };
                anyhow::ensure!(ev == "turn", "{ev}, with no notify");
            }
            anyhow::ensure!(c.waits.list.listed() <= 1 && c.waits.spares.len() <= 1, "{} places listed, {} spare after 10,000 turns", c.waits.list.listed(), c.waits.spares.len());
            let next = c.listen();
            let c2 = Arc::clone(&c);
            let ringing = spawn_spread_joined(async move {
                sleep(Duration::from_millis(20)).await;
                c2.notify();
            });
            timeout(Duration::from_secs(5), next).await?;
            timeout(Duration::from_secs(5), ringing).await?.map_err(|_| anyhow::anyhow!("the ringer ended"))?;
            Ok(())
        })
    }

    /// Consts-audit B1 (goal 9, plan 01a1a87b2cec step 3): work stays on its shard only where it fits a turn (the quantum) and
    /// is no dearer than the compute pool's round trip as learned, with no fixed bound (it was a slow poll, 100 µs). Do: weigh
    /// 50 µs of work against a round trip learned from a fast pool (20 µs samples), then from a slow one (a millisecond's),
    /// and back; weigh work past the quantum against a round trip longer still (10 ms, a loaded host's); weigh work nothing's
    /// been learned of, and work against a pool nothing's been learned of; check RFC 6298's arithmetic; hand a job to the pool
    /// from a shard. Expect: the fast pool takes the 50 µs, the slow one leaves it on the shard; work past the quantum goes to
    /// the pool however long its round trip; work expected to cost nothing stays, other work goes to a pool nothing's been
    /// learned of, whose first job teaches it; off a shard work stays where it is.
    #[test]
    fn b1_work_stays_on_its_shard_only_within_the_quantum_and_where_its_cheaper() -> anyhow::Result<()> {
        const QUANTUM: u64 = 266_666;
        let trip = Smoothed::nothing_yet();
        let sides = |trip: &Smoothed| Sides { quantum_ns: Some(QUANTUM), trip_ns: trip.mean() };
        anyhow::ensure!(trip.mean().is_none() && !sides(&trip).take_here(1, 1), "nothing learned of the pool: there, which teaches it");
        anyhow::ensure!(sides(&trip).take_here(0, 0), "nothing expected of the work: where it is, which learns it");
        // RFC 6298's arithmetic: 40 µs, then 8 µs. The first sets the mean and half of it the variation; then the variation
        // 20 − 20/4 + |8 − 40|/4 = 23 µs, and the mean 40 − 40/8 + 8/8 = 36 µs.
        trip.learn(40_000);
        anyhow::ensure!(trip.mean() == Some(40_000) && trip.dev() == 20_000, "the first: {:?}, {}", trip.mean(), trip.dev());
        trip.learn(8_000);
        anyhow::ensure!(trip.mean() == Some(36_000) && trip.dev() == 23_000, "the second: {:?}, {}", trip.mean(), trip.dev());
        // A fast pool takes 50 µs of work; a slow one leaves it on the shard, and moving back, takes it again.
        for _ in 0..32 {
            trip.learn(20_000);
        }
        let fast = sides(&trip);
        anyhow::ensure!(!fast.take_here(50_000, 50_000) && fast.take_here(20_000, 20_000), "against a fast pool ({fast:?}): 50 µs goes there, 20 µs stays");
        for _ in 0..32 {
            trip.learn(1_000_000);
        }
        let slow = sides(&trip);
        anyhow::ensure!(slow.take_here(50_000, 50_000), "against a slow pool ({slow:?}): 50 µs stays");
        // Within the quantum the cheaper side does it; past it, the pool, however long its round trip.
        anyhow::ensure!(slow.take_here(200_000, QUANTUM) && !slow.take_here(200_000, QUANTUM + 1), "a bound past the quantum goes to the pool: {slow:?}");
        let loaded = Sides { quantum_ns: Some(QUANTUM), trip_ns: Some(10_000_000) };
        anyhow::ensure!(!loaded.take_here(1_900_000, 1_900_000), "1.9 ms of work against a 10 ms round trip: the pool, never a poll past the quantum");
        anyhow::ensure!(loaded.take_here(100_000, 150_000), "100 µs, 150 µs at the most, against a 10 ms round trip: the shard");
        for _ in 0..32 {
            trip.learn(20_000);
        }
        anyhow::ensure!(!sides(&trip).take_here(50_000, 50_000), "fast again ({:?}): 50 µs goes there", sides(&trip));
        anyhow::ensure!(Sides { quantum_ns: None, trip_ns: None }.take_here(u64::MAX, u64::MAX), "off a shard: where it is, whatever it costs");
        block_on(async {
            compute(|| ()).await?;
            anyhow::ensure!(POOL_TRIP.mean().is_some(), "a job handed to the pool from a shard taught its round trip");
            Ok(())
        })
    }
}
