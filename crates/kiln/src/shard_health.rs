//! Each shard's health on `/metrics` (goal 1, plan 01a1a59c2a68 step 1): how late ready work runs on
//! it, how much of the time it's busy, and its slow polls by cause.
//!
//! A probe pinned to each shard ([`crate::rt::spawn_on`]) sleeps a period on the shard's timers and
//! notes, as it wakes, how late it ran: work that was ready waited that long behind whatever held the
//! shard (Node's `monitorEventLoopDelay` measures its loop the same way, sampling every 10 ms by
//! default: research 01a1a59c2a6a, f8). Each wake it also copies, on the shard itself, what hyper-rt
//! counts there (`hyper_rt::futures::shard_counters`: polls, polls that held the shard past twice its
//! step quantum and who held them) and the CPU time of the shard's thread, into atomics of its own.
//! `/metrics` reads only those atomics, from whatever thread serves it: reading never touches a shard,
//! takes no lock and asks nothing of the shard's loop.
//!
//! - **Delay** (`kiln_shard_delay_seconds`): how late each probe ran against when its timer fell
//!   due. It includes up to one tick of the timer wheel (100 µs, `crate::rt::runtime_config`) by
//!   design. A shard held for good stops probing: `kiln_shard_stalled_seconds` says how long since
//!   its probe last ran, before any late wake can be noted.
//! - **Busy** (`kiln_shard_busy_seconds_total`): the CPU time the shard's thread has run, so its rate
//!   is the share of the time the shard was on the CPU (Linux and macOS; Windows keeps a thread's
//!   times at the scheduler's tick, so no series there). Time the host held a ready shard off the
//!   CPU isn't busy: it shows in the delay, and as `host` slow polls.
//! - **Slow polls** (`kiln_shard_slow_polls_total`, by `cause`): hyper-rt's long polls, each one that
//!   held its shard past twice the step quantum (250 µs here, so past 500 µs), attributed as the
//!   runtime does (`hyper_rt`'s `attribution`): `task` (it ran on the CPU), `blocking` (it waited
//!   inside a call), `host` (it was runnable and off the CPU: preempted), `unknown` (where the OS
//!   can't tell: off the CPU on macOS, every one on Windows).

use std::fmt::Write as _;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::metrics::{Histogram, STALL_BUCKETS};

/// How often each probe wakes, unless `KILN_SHARD_PROBE_MS` says (`0`: no probes): Node's default
/// sampling of its loop's delay (research 01a1a59c2a6a, f8).
pub const PERIOD: Duration = Duration::from_millis(10);

/// One shard's figures, written by its probe alone, read by `/metrics`. On a cache line of its own
/// (the largest we target, Apple silicon's 128 bytes, as hyper-rt's own pulse is): each probe writes
/// only its own.
#[repr(align(128))]
pub struct Shard {
    delay: Histogram,
    /// The latest probe: when it ran (nanoseconds since [`base`]), how late, and the most late yet.
    last_ns: AtomicU64,
    late_ns: AtomicU64,
    most_late_ns: AtomicU64,
    /// The shard thread's CPU time, nanoseconds (`u64::MAX`: not known on this OS).
    cpu_ns: AtomicU64,
    polls: AtomicU64,
    slow_task: AtomicU64,
    slow_blocking: AtomicU64,
    slow_host: AtomicU64,
    slow_unknown: AtomicU64,
    longest_poll_ns: AtomicU64,
    overrun_ns: AtomicU64,
}

impl Shard {
    fn new() -> Self {
        Self {
            delay: Histogram::new(STALL_BUCKETS),
            last_ns: AtomicU64::new(0),
            late_ns: AtomicU64::new(0),
            most_late_ns: AtomicU64::new(0),
            cpu_ns: AtomicU64::new(u64::MAX),
            polls: AtomicU64::new(0),
            slow_task: AtomicU64::new(0),
            slow_blocking: AtomicU64::new(0),
            slow_host: AtomicU64::new(0),
            slow_unknown: AtomicU64::new(0),
            longest_poll_ns: AtomicU64::new(0),
            overrun_ns: AtomicU64::new(0),
        }
    }

    /// A probe's wake, `late` after its timer fell due, at `now`; on the shard, so what it reads of
    /// the shard is the shard's own.
    fn note(&self, late: Duration, now: Instant) {
        let late_ns = nanos(late);
        self.delay.observe(late.as_secs_f64());
        self.late_ns.store(late_ns, Ordering::Relaxed);
        self.most_late_ns.fetch_max(late_ns, Ordering::Relaxed);
        self.last_ns.store(nanos(now.saturating_duration_since(base())), Ordering::Relaxed);
        if let Some(c) = hyper_rt::futures::shard_counters() {
            self.polls.store(c.polls, Ordering::Relaxed);
            self.slow_task.store(c.long_steps.saturating_sub(c.blocked_steps), Ordering::Relaxed);
            self.slow_blocking.store(c.blocked_steps, Ordering::Relaxed);
            self.slow_host.store(c.preempted_steps, Ordering::Relaxed);
            self.slow_unknown.store(c.unattributed_steps, Ordering::Relaxed);
            self.longest_poll_ns.store(c.longest_step_ns, Ordering::Relaxed);
            self.overrun_ns.store(c.scheduler_overrun_ns, Ordering::Relaxed);
        }
        if let Some(cpu) = thread_cpu_ns() {
            self.cpu_ns.store(cpu, Ordering::Relaxed);
        }
    }

    /// How late its probes ran, each noted.
    pub fn delay(&self) -> &Histogram {
        &self.delay
    }

    /// What the shard's probe has noted, as it stands.
    pub fn snapshot(&self) -> Snapshot {
        let last = self.last_ns.load(Ordering::Relaxed);
        Snapshot {
            probes: self.delay.count(),
            late_sum: Duration::from_micros(self.delay.sum_us()),
            late: Duration::from_nanos(self.late_ns.load(Ordering::Relaxed)),
            most_late: Duration::from_nanos(self.most_late_ns.load(Ordering::Relaxed)),
            since_probe: (last > 0).then(|| Duration::from_nanos(nanos(base().elapsed()).saturating_sub(last))),
            busy: Some(self.cpu_ns.load(Ordering::Relaxed)).filter(|n| *n != u64::MAX).map(Duration::from_nanos),
            polls: self.polls.load(Ordering::Relaxed),
            slow: Slow {
                task: self.slow_task.load(Ordering::Relaxed),
                blocking: self.slow_blocking.load(Ordering::Relaxed),
                host: self.slow_host.load(Ordering::Relaxed),
                unknown: self.slow_unknown.load(Ordering::Relaxed),
            },
            longest_poll: Duration::from_nanos(self.longest_poll_ns.load(Ordering::Relaxed)),
            overrun: Duration::from_nanos(self.overrun_ns.load(Ordering::Relaxed)),
        }
    }
}

/// Slow polls by cause (see the module's notes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Slow {
    pub task: u64,
    pub blocking: u64,
    pub host: u64,
    pub unknown: u64,
}

impl Slow {
    pub fn total(&self) -> u64 {
        self.task.saturating_add(self.blocking).saturating_add(self.host).saturating_add(self.unknown)
    }
}

/// One shard's figures as read.
#[derive(Clone, Copy, Debug)]
pub struct Snapshot {
    /// Probes noted.
    pub probes: u64,
    /// How late they ran, added up: with `probes`, how late its ready work ran on average between
    /// two reads (what headroom watches: docs/priority.md).
    pub late_sum: Duration,
    /// How late the latest probe ran, and the most late of any.
    pub late: Duration,
    pub most_late: Duration,
    /// How long since the latest probe ran (none ran yet: `None`).
    pub since_probe: Option<Duration>,
    /// The CPU time the shard's thread has run, where the OS keeps it per thread.
    pub busy: Option<Duration>,
    pub polls: u64,
    pub slow: Slow,
    pub longest_poll: Duration,
    pub overrun: Duration,
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// The instant probes' times are counted from.
fn base() -> Instant {
    static BASE: OnceLock<Instant> = OnceLock::new();
    *BASE.get_or_init(Instant::now)
}

static SHARDS: OnceLock<Box<[Shard]>> = OnceLock::new();

/// Every shard's figures, in the runtime's order (empty before [`start`]).
pub fn shards() -> &'static [Shard] {
    SHARDS.get().map_or(&[], |s| &s[..])
}

/// The probes' period from `KILN_SHARD_PROBE_MS` (`0`: none), else [`PERIOD`].
pub fn period_from_env() -> Option<Duration> {
    match std::env::var("KILN_SHARD_PROBE_MS").ok().and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
        None => Some(PERIOD),
    }
}

/// Starts a probe on every shard, waking every `period`: once a process; later calls do nothing.
/// How many shards took theirs.
pub fn start(period: Duration) -> usize {
    let mut fresh = false;
    let shards = SHARDS.get_or_init(|| {
        fresh = true;
        (0..crate::rt::shard_count()).map(|_| Shard::new()).collect()
    });
    if !fresh {
        return 0;
    }
    let _ = base();
    let period = period.max(Duration::from_millis(1));
    let mut started = 0;
    for i in 0..shards.len() {
        if crate::rt::spawn_on(i, probe(i, period)) {
            started += 1;
        } else {
            tracing::warn!(shard = i, "a shard refused its health probe: /metrics won't show it");
        }
    }
    started
}

/// Shard `i`'s probe: wakes every `period` on its shard's timers and notes how late it ran.
async fn probe(i: usize, period: Duration) {
    let Some(me) = shards().get(i) else { return };
    let mut due = Instant::now() + period;
    loop {
        crate::rt::sleep_until(due).await;
        let now = Instant::now();
        me.note(now.saturating_duration_since(due), now);
        // The next one a period from now: a shard that was held doesn't then run a burst of probes
        // to catch up, each noting the same hold again.
        due = now + period;
    }
}

/// The CPU time the calling thread has run, nanoseconds (Linux and macOS; elsewhere the OS keeps a
/// thread's times at its scheduler's tick, or not at all).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn thread_cpu_ns() -> Option<u64> {
    let t = nix::time::clock_gettime(nix::time::ClockId::CLOCK_THREAD_CPUTIME_ID).ok()?;
    u64::try_from(t.tv_sec()).ok()?.checked_mul(1_000_000_000)?.checked_add(u64::try_from(t.tv_nsec()).ok()?)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn thread_cpu_ns() -> Option<u64> {
    None
}

#[allow(clippy::cast_precision_loss)]
fn secs(d: Duration) -> f64 {
    d.as_nanos() as f64 / 1e9
}

/// The `kiln_shard_*` families, in the exposition.
pub fn render(o: &mut String) {
    let all = shards();
    let help = |o: &mut String, name: &str, kind: &str, h: &str| {
        let _ = writeln!(o, "# HELP {name} {h}\n# TYPE {name} {kind}");
    };
    let snaps: Vec<Snapshot> = all.iter().map(Shard::snapshot).collect();
    help(o, "kiln_shard_delay_seconds", "histogram", "How late ready work ran on each shard: a probe's timer, due every period, against when the probe ran (up to a timer tick, 100 us, by design).");
    for (i, s) in all.iter().enumerate() {
        s.delay.render(o, "kiln_shard_delay_seconds", &format!("shard=\"{i}\""));
    }
    help(o, "kiln_shard_stalled_seconds", "gauge", "How long since each shard's probe last ran: a shard held in one poll shows here before its delay can.");
    for (i, s) in snaps.iter().enumerate() {
        if let Some(d) = s.since_probe {
            let _ = writeln!(o, "kiln_shard_stalled_seconds{{shard=\"{i}\"}} {}", secs(d));
        }
    }
    help(o, "kiln_shard_busy_seconds_total", "counter", "CPU time each shard's thread has run (Linux, macOS): its rate is the share of the time the shard was busy on the CPU.");
    for (i, s) in snaps.iter().enumerate() {
        if let Some(d) = s.busy {
            let _ = writeln!(o, "kiln_shard_busy_seconds_total{{shard=\"{i}\"}} {}", secs(d));
        }
    }
    help(o, "kiln_shard_polls_total", "counter", "Task polls each shard ran.");
    for (i, s) in snaps.iter().enumerate() {
        let _ = writeln!(o, "kiln_shard_polls_total{{shard=\"{i}\"}} {}", s.polls);
    }
    help(
        o,
        "kiln_shard_slow_polls_total",
        "counter",
        "Polls that held a shard past twice its step quantum, by cause: task (on the CPU), blocking (waiting inside a call), host (runnable, off the CPU), unknown (the OS can't tell).",
    );
    for (i, s) in snaps.iter().enumerate() {
        for (cause, n) in [("task", s.slow.task), ("blocking", s.slow.blocking), ("host", s.slow.host), ("unknown", s.slow.unknown)] {
            let _ = writeln!(o, "kiln_shard_slow_polls_total{{shard=\"{i}\",cause=\"{cause}\"}} {n}");
        }
    }
    help(o, "kiln_shard_longest_poll_seconds", "gauge", "The longest single poll each shard has run.");
    for (i, s) in snaps.iter().enumerate() {
        let _ = writeln!(o, "kiln_shard_longest_poll_seconds{{shard=\"{i}\"}} {}", secs(s.longest_poll));
    }
    help(o, "kiln_shard_scheduler_overrun_seconds", "gauge", "How late each shard's steps ran after its waits, as the shard measured it (the host descheduling it), decaying as it recovers.");
    for (i, s) in snaps.iter().enumerate() {
        let _ = writeln!(o, "kiln_shard_scheduler_overrun_seconds{{shard=\"{i}\"}} {}", secs(s.overrun));
    }
}
