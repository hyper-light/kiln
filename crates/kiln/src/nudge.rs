//! The shards' nudge, off until declared and, declared `auto`, paid for only where it buys something (goal 8, plan
//! 01a1a59c2b29 step 7; goal 6, plan 01a1a59c2abe step 3; docs/priority.md, "A nudge for the shards").
//!
//! On a Linux kernel that preempts lazily (fedora's), a shard woken while another thread runs on its CPU in user
//! space waits for that CPU's next interrupt, its tick at the latest: wake-2 measured a wake from another CPU at
//! p99 1.00 ms behind every kind of hog (docs/priority.md, "The kernel's preemption"). A nudged shard's waits in
//! its driver end every period, so its CPU takes an interrupt of the shard's own at least that often while it
//! waits, and a held wake runs then (`hyper_rt::compute::nudge_current_thread`, `crate::rt::nudge`). It costs a
//! wake of the waiting shard each period, which a calm server shouldn't pay. So `[server] nudge = "auto"`, the mode
//! a config declares (the default is off until then: docs/ci.md §0.3, "Opt-in automation"), has each shard nudge only
//! while its host holds its wakes ([`Mode`]):
//!
//! - **What a shard reads**, once a period of its health probe (`crate::shard_health::PERIOD`, 17.07 ms), on its own
//!   thread: the kernel's count of how long the thread waited, runnable, for a CPU (the run delay of
//!   `/proc/thread-self/schedstat`, a file kept open and read again, one system call), and its parks by what ended
//!   them (`hyper_rt`'s counters): another thread's kick or its I/O, or its own timers. A shard that nudges on no
//!   judgment of its own (off, or on) only shows its wait, and reads it at its first period and then for `/metrics`'
//!   next read, a period before it at the most (`crate::shard_health::Reads`; consts-audit B3: it read it every 5th
//!   period; goal 9, plan 01a1a87b2cec step 5: a system call on every shard every period, which their busy share counts).
//! - **Held** ([`Judge`]): the period's wait, less what a calm CPU makes a wake by the thread's own timer wait
//!   ([`TIMER_WAKE_NS`]), over the wakes others made: how long its host held each of them. At [`HELD_NS`] or more
//!   the period was held. While the shard nudges, its own timers wake it every period, and on a busy CPU each of
//!   those waits longer than a calm one's, which only adds to the held wait it reads: it never reads its own
//!   nudging as calm.
//! - **Nudging** while a held period was within the last [`LINGER`], and not otherwise: a shard that no other thread
//!   nor any I/O woke for that long is idle and stops, as does one whose host lets it run again. Nudging, a held
//!   wake waits at most the nudge's bound (its period and its timer's lateness, 266.7 µs at kiln's period:
//!   `crate::rt::nudge_period_ns`), about 130 µs on average and past [`HELD_NS`], so a shard nudges as long as its
//!   host holds it, and the lingering keeps one held now and then nudging between.
//! - **A calm server pays nothing for it** but the reading: a calm shard's wakes waited 1.2 µs each (wake-2, no
//!   hog), well under [`HELD_NS`], as do those of a kernel that preempts at once (adacachy's: a wake interrupts the
//!   busy CPU), where the nudge would buy nothing.
//!
//! What it did is on `/metrics` (`kiln_shard_nudged_seconds_total`, `kiln_shard_nudge_starts_total`,
//! `kiln_shard_run_wait_seconds_total`, `kiln_shard_held_wake_seconds`), in `/healthz/server`
//! (`priority.nudge.armed`), and in the log as each shard first starts (why: what its wakes waited).

use std::cell::RefCell;
use std::fmt::Write as _;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How the shards nudge their CPUs (`[server] nudge`; docs/priority.md, "A nudge for the shards").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Never: the default, until a config declares another (docs/ci.md §0.3, "Opt-in automation").
    #[default]
    Off,
    /// Every wait, from the shard's start.
    On,
    /// While the shard's host holds its wakes (the module's notes): the mode a config declares.
    Auto,
}

impl Mode {
    /// The setting's word for it.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::On => "on",
            Mode::Auto => "auto",
        }
    }

    /// The mode a setting's word names: `off`, `on` or `auto`, and the words a flag is said with (`true`, `1`,
    /// `yes`; `false`, `0`, `no`), as the lab's scripts say it (`KILN_RT_NUDGE=1`, scripts/priority_lab/s7c.sh).
    pub fn parse(word: &str) -> Option<Mode> {
        match word.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Mode::Auto),
            "on" | "true" | "1" | "yes" => Some(Mode::On),
            "off" | "false" | "0" | "no" => Some(Mode::Off),
            _ => None,
        }
    }

    /// `KILN_RT_NUDGE`'s value as the lab sets it (`kiln-lab ab --env-b KILN_RT_NUDGE=auto`): a word [`Mode::parse`]
    /// knows; none for any other, which the config refuses, saying the words.
    pub fn from_env(value: &str) -> Option<Mode> {
        Mode::parse(value)
    }

    /// Its index in a byte, as `crate::rt` keeps it.
    pub fn index(self) -> u8 {
        match self {
            Mode::Off => 0,
            Mode::On => 1,
            Mode::Auto => 2,
        }
    }

    /// The mode kept as [`Mode::index`]: `off` for any other byte.
    pub fn of_index(index: u8) -> Mode {
        match index {
            1 => Mode::On,
            2 => Mode::Auto,
            _ => Mode::Off,
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for Mode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let word = String::deserialize(d)?;
        Mode::parse(&word).ok_or_else(|| serde::de::Error::custom(format!("nudge = \"{word}\": \"auto\", \"on\" or \"off\"")))
    }
}

/// A thread's run, its wake held by no other thread, at the most it waited for its CPU in wake-2 (fedora, Linux 6.19
/// with lazy preemption at 1000 Hz, 2026-11-05; `run_delay_per_slice_us`, the kernel's run delay over the thread's
/// runs): 4.4 µs, beside C compilers, woken by a socket from its own CPU. With no hog, 1.2 µs.
const UNHELD_MOST_NS: u64 = 4_400;

/// A thread's run, its wake held by another thread on its CPU, at the least it waited in wake-2: 67.4 µs, beside hogs
/// that enter the kernel every 20 µs, woken from another CPU (spinning hogs held it 368 to 436 µs, compilers 294 to
/// 352, other sessions' work alone 200 to 223).
const HELD_LEAST_NS: u64 = 67_400;

/// A shard's period was held when its host held each wake others made it at least this long, on average,
/// nanoseconds: the geometric mean of [`UNHELD_MOST_NS`] and [`HELD_LEAST_NS`], 17.2 µs, as far (a factor of 3.9)
/// from each of the two groups wake-2 measured, which stand a factor of fifteen apart.
pub const HELD_NS: u64 = (UNHELD_MOST_NS * HELD_LEAST_NS).isqrt();

/// What a wake by the thread's own timer makes it wait for its CPU on a calm machine, at the most, nanoseconds
/// (wake-2, no hog: 0.4 to 0.7 µs a run). A nudging shard's own wakes are its timers': only what they wait past this
/// is read as held, so a calm shard's own nudging never reads as its host holding it.
pub const TIMER_WAKE_NS: u64 = 700;

/// How long a held period keeps a shard nudging: a tolerance (`crate::headroom::settings::DEFAULT_TOLERANCE_MS`,
/// a person's flow of thought, 1 s: LB05), the interval `crate::overload` judges a queue standing over. A shard
/// whose host holds it now and then nudges between, and one no other thread or I/O woke for that long stops.
pub const LINGER: Duration = Duration::from_millis(crate::headroom::settings::DEFAULT_TOLERANCE_MS as u64);

/// What a shard's probe reads of it at the end of a period: counts since the shard's thread began.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reading {
    /// How long the thread waited, runnable, for a CPU, nanoseconds (the kernel's run delay).
    pub run_wait_ns: u64,
    /// Its parks another thread's kick or its I/O ended.
    pub woken_by_others: u64,
    /// Its parks its own timers ended (a nudge's among them).
    pub woken_by_timers: u64,
}

/// What a period's judgment says ([`Judge::period`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verdict {
    /// Whether the shard nudges through the next period.
    pub nudge: bool,
    /// How long its host held each wake others made it over the period, on average, nanoseconds: none with no such
    /// wake, and at the first reading.
    pub held_ns: Option<u64>,
}

/// A shard's judgment of its host, period by period (the module's notes): pure, so it's tested on every host.
#[derive(Debug, Default)]
pub struct Judge {
    last: Option<Reading>,
    held_at: Option<Instant>,
}

impl Judge {
    /// The period ending `now` with `reading`: whether the shard nudges through the next, and what held it.
    pub fn period(&mut self, now: Instant, reading: Reading) -> Verdict {
        let held_ns = self.last.replace(reading).and_then(|last| held_per_wake(last, reading));
        if held_ns.is_some_and(|held| held >= HELD_NS) {
            self.held_at = Some(now);
        }
        let nudge = self.held_at.is_some_and(|at| now.saturating_duration_since(at) < LINGER);
        Verdict { nudge, held_ns }
    }
}

/// How long the host held each wake others made the shard between `last` and `now`, on average: the run delay
/// accrued, less [`TIMER_WAKE_NS`] for each wake of its own timers, over the others' wakes; none with no such wake.
fn held_per_wake(last: Reading, now: Reading) -> Option<u64> {
    let woken = now.woken_by_others.checked_sub(last.woken_by_others).filter(|n| *n > 0)?;
    let timers = now.woken_by_timers.saturating_sub(last.woken_by_timers);
    let waited = now.run_wait_ns.saturating_sub(last.run_wait_ns);
    waited.saturating_sub(timers.saturating_mul(TIMER_WAKE_NS)).checked_div(woken)
}

// ------------------------------------------------------------------------------------- the thread's wait

/// The run delay of a `schedstat` line, `<ran ns> <waited ns> <runs>` (the kernel's `proc_pid_schedstat`): none
/// where the kernel keeps none, which it says as zeros (no `CONFIG_SCHED_INFO`), or for a line it didn't write.
pub fn run_wait_of(text: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(text).ok()?;
    let mut words = text.split_ascii_whitespace().map(|w| w.parse::<u64>().ok());
    let (ran, waited, runs) = (words.next()??, words.next()??, words.next()??);
    (ran > 0 || runs > 0).then_some(waited)
}

/// The calling thread's wait for a CPU as the kernel counts it: `/proc/thread-self/schedstat`, opened on the thread
/// whose it is and read again from its start each time (one `pread`). Linux only.
#[derive(Debug)]
pub struct RunWait {
    #[cfg(target_os = "linux")]
    file: std::fs::File,
}

impl RunWait {
    /// The calling thread's; none where the kernel keeps no thread's wait (not Linux, no `/proc`, no
    /// `CONFIG_SCHED_INFO`).
    pub fn open() -> Option<RunWait> {
        #[cfg(target_os = "linux")]
        {
            let file = std::fs::File::open("/proc/thread-self/schedstat").ok()?;
            let it = RunWait { file };
            it.read()?;
            Some(it)
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    /// How long its thread has waited, runnable, for a CPU so far, nanoseconds.
    pub fn read(&self) -> Option<u64> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::FileExt as _;
            // "%llu %llu %lu\n": three numbers of at most 20 digits.
            let mut buf = [0u8; 80];
            let n = self.file.read_at(&mut buf, 0).ok()?;
            run_wait_of(buf.get(..n)?)
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }
}

// ------------------------------------------------------------------------------------- each shard's

/// What `/metrics` and `/healthz/server` read of one shard's nudge, written by its probe on its thread.
#[derive(Debug, Default)]
struct Figures {
    /// Whether its waits are nudged now.
    armed: AtomicBool,
    /// How long they were nudged, nanoseconds, as its probe's periods ended.
    nudged_ns: AtomicU64,
    /// Times it began nudging on its own ([`Mode::Auto`]).
    starts: AtomicU64,
    /// Its thread's wait for a CPU so far, nanoseconds (the kernel's run delay), as last read.
    run_wait_ns: AtomicU64,
    /// How long its host held each wake others made it over its last period, on average, nanoseconds.
    held_ns: AtomicU64,
    /// Whether its run wait was read.
    read: AtomicBool,
}

/// Every shard's, made at the first probe's first period, one for each of the runtime's shards.
static FIGURES: OnceLock<Box<[Figures]>> = OnceLock::new();

fn figures_of(shard: usize) -> Option<&'static Figures> {
    FIGURES.get_or_init(|| (0..crate::rt::shard_count()).map(|_| Figures::default()).collect()).get(shard)
}

/// The shards nudging now.
pub fn armed_now() -> u32 {
    FIGURES.get().map_or(0, |all| u32::try_from(all.iter().filter(|f| f.armed.load(Ordering::Relaxed)).count()).unwrap_or(u32::MAX))
}

/// The times shards began nudging on their own, all of them together.
pub fn starts() -> u64 {
    FIGURES.get().map_or(0, |all| all.iter().map(|f| f.starts.load(Ordering::Relaxed)).fold(0, u64::saturating_add))
}

/// What one shard's probe keeps of its nudge, on the shard's thread.
#[derive(Debug, Default)]
struct Here {
    /// Its nudge's period where it nudges on its own ([`Mode::Auto`], the class and the OS allowing), else 0.
    may_ns: u64,
    /// Whether its waits are nudged now: [`Mode::On`]'s always, [`Mode::Auto`]'s while held.
    armed: bool,
    /// Whether it said why it first nudged.
    said: bool,
    judge: Judge,
    /// Its thread's wait, once opened; `tried` once it was asked for.
    run_wait: Option<RunWait>,
    tried: bool,
    /// When its last period ended.
    last: Option<Instant>,
    /// Periods it ended: while it nudges on no judgment of its own, its wait is read only at its first and for
    /// `/metrics`' reads ([`Here::reads_wait`]).
    periods: u64,
}

thread_local! {
    /// The shard's own, on its thread: set as it starts (`crate::rt`), then by its probe each period ([`tick`]).
    static HERE: RefCell<Option<Here>> = const { RefCell::new(None) };
}

/// `f` on the calling thread's [`Here`], made as it first asks; nothing on a thread that is ending.
fn with_here<R>(f: impl FnOnce(&mut Here) -> R) -> Option<R> {
    HERE.try_with(|cell| cell.try_borrow_mut().ok().map(|mut slot| f(slot.get_or_insert_with(Here::default)))).ok().flatten()
}

/// The calling shard nudges every wait from here on ([`Mode::On`], asked of the OS by its caller): counted as nudged.
pub fn always() {
    let _ = with_here(|here| here.armed = true);
}

/// The calling shard may nudge its CPU at `period_ns` on its own ([`Mode::Auto`]): whether the OS takes a nudge
/// here (asked with nothing held, `hyper_rt::compute::nudge_current_thread(0)`) and the kernel says how long its
/// thread waits for a CPU, which it judges by. Nothing is nudged until its host holds it.
pub fn allow(period_ns: u64) -> Result<(), hyper_rt::compute::Refusal> {
    hyper_rt::compute::nudge_current_thread(0)?;
    let Some(run_wait) = RunWait::open() else {
        return Err(hyper_rt::compute::Refusal {
            call: "auto judges a shard's host by how long its thread waits for a CPU, which this kernel doesn't say (/proc/thread-self/schedstat)",
            code: None,
        });
    };
    let _ = with_here(|here| {
        here.may_ns = period_ns;
        here.run_wait = Some(run_wait);
        here.tried = true;
    });
    Ok(())
}

/// Shard `shard`'s probe ends a period at `now`, the shard's counts as it read them (`counters`): its wait for a
/// CPU read and shown, its host judged, and its nudge begun or ended where it nudges on its own. On the shard's
/// thread (`crate::shard_health`'s probe).
pub fn tick(shard: usize, now: Instant, counters: Option<&hyper_rt::shard_loop::Counters>) {
    let Some(c) = counters else { return };
    let reading = |run_wait_ns| Reading { run_wait_ns, woken_by_others: c.woken_by_kick.saturating_add(c.woken_by_io), woken_by_timers: c.woken_by_timer };
    let _ = with_here(|here| here.period(shard, now, reading));
}

impl Here {
    /// Whether the period ending now reads its thread's wait, the period counted (goal 9, plan 01a1a87b2cec step 5). A
    /// shard that judges nothing by its wait (off, the default; or on, whatever its host does) only shows it: read at its
    /// first period and where a read of `/metrics` may come before the next (`due`: `crate::shard_health::copies_due`),
    /// as the probe copies the shard's other counts, and what held it judged over the periods since (its counts'
    /// differences span them). Each read is a `pread` of procfs: kernel time on every shard every period, which the
    /// shards' busy share counts and their user-time profiles don't show. `auto` reads every period: it acts on each.
    fn reads_wait(&mut self, due: bool) -> bool {
        let first = self.periods == 0;
        self.periods = self.periods.wrapping_add(1);
        self.may_ns != 0 || first || due
    }

    fn period(&mut self, shard: usize, now: Instant, reading: impl FnOnce(u64) -> Reading) {
        let figures = figures_of(shard);
        if let Some(f) = figures {
            f.armed.store(self.armed, Ordering::Relaxed);
        }
        if let (Some(last), Some(f)) = (self.last.replace(now), figures)
            && self.armed
        {
            let ns = u64::try_from(now.saturating_duration_since(last).as_nanos()).unwrap_or(u64::MAX);
            f.nudged_ns.fetch_add(ns, Ordering::Relaxed);
        }
        if !self.tried {
            self.tried = true;
            self.run_wait = RunWait::open();
        }
        if !self.reads_wait(crate::shard_health::copies_due(now)) {
            return;
        }
        let Some(waited) = self.run_wait.as_ref().and_then(RunWait::read) else { return };
        let verdict = self.judge.period(now, reading(waited));
        if let Some(f) = figures {
            f.run_wait_ns.store(waited, Ordering::Relaxed);
            f.read.store(true, Ordering::Relaxed);
            if let Some(held) = verdict.held_ns {
                f.held_ns.store(held, Ordering::Relaxed);
            }
        }
        if self.may_ns == 0 || verdict.nudge == self.armed {
            return;
        }
        let period = if verdict.nudge { self.may_ns } else { 0 };
        if hyper_rt::compute::nudge_current_thread(period).is_err() {
            return;
        }
        self.armed = verdict.nudge;
        if let Some(f) = figures {
            f.armed.store(self.armed, Ordering::Relaxed);
            if self.armed {
                f.starts.fetch_add(1, Ordering::Relaxed);
            }
        }
        let held_us = verdict.held_ns.map_or(0.0, |ns| ns as f64 / 1e3);
        if self.armed && !self.said {
            self.said = true;
            tracing::info!(shard, held_us, past_us = HELD_NS as f64 / 1e3, period_us = self.may_ns / 1000, "a shard nudges its CPU while it waits: its host held each wake others made it {held_us:.1} µs on average over its last probe period, past {:.1} µs (`[server] nudge = \"auto\"`)", HELD_NS as f64 / 1e3);
        } else if self.armed {
            tracing::debug!(shard, held_us, "a shard nudges its CPU again: its host holds its wakes");
        } else {
            tracing::debug!(shard, "a shard stops nudging its CPU: its host held none of its wakes for a tolerance, or none came");
        }
    }
}

/// Each shard's nudge on `/metrics`, once any probe has read one (the module's notes).
pub fn render(o: &mut String) {
    let Some(all) = FIGURES.get() else { return };
    let secs = |ns: u64| ns as f64 / 1e9;
    let mut family = |name: &str, kind: &str, help: &str, value: &dyn Fn(&Figures) -> Option<f64>| {
        let _ = writeln!(o, "# HELP {name} {help}");
        let _ = writeln!(o, "# TYPE {name} {kind}");
        for (shard, f) in all.iter().enumerate() {
            if let Some(v) = value(f) {
                let _ = writeln!(o, "{name}{{shard=\"{shard}\"}} {v}");
            }
        }
    };
    family(
        "kiln_shard_nudged_seconds_total",
        "counter",
        "How long each shard's waits were nudged ([server] nudge; goal 8 step 7): always where it's on, while its host held its wakes where it's auto.",
        &|f| Some(secs(f.nudged_ns.load(Ordering::Relaxed))),
    );
    family("kiln_shard_nudge_starts_total", "counter", "Times each shard began nudging on its own, its host holding its wakes ([server] nudge = \"auto\").", &|f| Some(f.starts.load(Ordering::Relaxed) as f64));
    family("kiln_shard_nudging", "gauge", "Whether each shard's waits are nudged now (1) or not (0).", &|f| Some(if f.armed.load(Ordering::Relaxed) { 1.0 } else { 0.0 }));
    family(
        "kiln_shard_run_wait_seconds_total",
        "counter",
        "How long each shard's thread waited, runnable, for a CPU: the kernel's run delay (Linux's /proc/thread-self/schedstat).",
        &|f| f.read.load(Ordering::Relaxed).then(|| secs(f.run_wait_ns.load(Ordering::Relaxed))),
    );
    family(
        "kiln_shard_held_wake_seconds",
        "gauge",
        "How long each shard's host held each wake another thread or its I/O made it over its probe's last period with any, on average, past what its own timers' wakes wait on a calm CPU.",
        &|f| f.read.load(Ordering::Relaxed).then(|| secs(f.held_ns.load(Ordering::Relaxed))),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERIOD: Duration = Duration::from_millis(10);

    /// A reading `waited_us` of run wait, `others` wakes by others and `timers` by its timers, past `last`.
    fn after(last: Reading, waited_us: u64, others: u64, timers: u64) -> Reading {
        Reading { run_wait_ns: last.run_wait_ns + waited_us * 1000, woken_by_others: last.woken_by_others + others, woken_by_timers: last.woken_by_timers + timers }
    }

    /// Goal 8, plan 01a1a59c2b29 step 7: the threshold is the geometric mean of wake-2's two groups, 17.2 µs, a factor of
    /// 3.9 from each; a calm wake and a kernel that preempts at once fall under it, every held kind wake-2 measured over.
    #[test]
    fn g8_s7_held_is_past_the_geometric_mean_of_wake_2_s_two_groups() {
        assert_eq!(HELD_NS, 17_220);
        assert_eq!((HELD_NS / UNHELD_MOST_NS, HELD_LEAST_NS / HELD_NS), (3, 3), "as far, by a factor, from each");
        // wake-2's runs, their wakes not held: no hog (1.2 µs), beside hogs woken on their own CPU (1.6 to 4.4 µs).
        for unheld_ns in [1_200_u64, 1_600, 2_700, 3_100, 4_400] {
            assert!(unheld_ns < HELD_NS, "{unheld_ns} ns a run, not held");
        }
        // Held: hogs entering the kernel, other sessions' work, compilers, spinning hogs, woken from another CPU.
        for held_us in [67_u64, 101, 200, 294, 368, 436] {
            assert!(held_us * 1000 > HELD_NS, "{held_us} µs a run, held");
        }
    }

    /// Goal 8, plan 01a1a59c2b29 step 7: what a period reads as held is the run wait past its timers' calm cost over the
    /// wakes others made; none with no such wake, and none at the first reading.
    #[test]
    fn g8_s7_a_period_s_held_wait_is_its_run_wait_past_its_timers_over_others_wakes() {
        let base = Reading { run_wait_ns: 5_000_000, woken_by_others: 100, woken_by_timers: 1000 };
        // 10 wakes by others held 450 µs each, 2 probe timers at 1 µs: 4,502 µs, less 2 × 0.7.
        assert_eq!(held_per_wake(base, after(base, 4_502, 10, 2)), Some(450_060));
        assert_eq!(held_per_wake(base, after(base, 900, 0, 70)), None, "only its timers woke it: no wake to judge");
        // Nudging on a calm CPU: 70 timer wakes at 0.5 µs and 3 others at 1 µs read as nothing held.
        assert_eq!(held_per_wake(base, after(base, 38, 3, 70)), Some(0));
        // Nudging on a busy one: its own wakes at 2 µs each add to what its 3 held wakes (≤ 200 µs) read.
        assert!(held_per_wake(base, after(base, 140 + 300, 3, 70)).is_some_and(|ns| ns >= 100_000));
        // Counts that went back (a new thread's) judge nothing past what they can.
        assert_eq!(held_per_wake(after(base, 0, 5, 0), base), None);
    }

    /// Goal 8, plan 01a1a59c2b29 step 7: a calm shard never nudges; one its host holds nudges from the period that showed
    /// it, through the periods it stays held (its nudged wakes still read held), and for a tolerance past the last held
    /// one, then stops: also when nothing but its own timers woke it (idle).
    /// A judge's periods, one after another, each [`PERIOD`] long.
    struct Periods {
        judge: Judge,
        at: Instant,
        r: Reading,
    }

    impl Periods {
        fn new() -> Self {
            Self { judge: Judge::default(), at: Instant::now(), r: Reading::default() }
        }

        /// The next period: `waited_us` of run wait, `others` wakes by others, `timers` by its timers.
        fn next(&mut self, waited_us: u64, others: u64, timers: u64) -> Verdict {
            self.at += PERIOD;
            self.r = after(self.r, waited_us, others, timers);
            self.judge.period(self.at, self.r)
        }

        /// The periods `next` gives, until one doesn't nudge: how long after `from` that one ended (none in 200).
        fn stops(&mut self, from: Instant, waited_us: u64, others: u64, timers: u64) -> Option<Duration> {
            (0..200).find_map(|_| (!self.next(waited_us, others, timers).nudge).then(|| self.at.saturating_duration_since(from)))
        }
    }

    #[test]
    fn g8_s7_a_shard_nudges_while_its_host_holds_it_and_a_tolerance_past() {
        let mut p = Periods::new();
        assert_eq!(p.next(0, 0, 0), Verdict { nudge: false, held_ns: None }, "the first reading judges nothing");
        // Calm: 15 wakes by others at 1.2 µs, a probe's timer.
        for _ in 0..50 {
            assert!(!p.next(18 + 1, 15, 1).nudge, "calm never nudges");
        }
        // Hogs: 15 wakes held 450 µs each.
        let held = p.next(15 * 450, 15, 1);
        assert!(held.nudge && held.held_ns.is_some_and(|ns| ns > HELD_NS), "{held:?}");
        // Nudging under the hogs: its held wakes wait 100 µs on average, its 66 timer wakes 2 µs each.
        for _ in 0..300 {
            assert!(p.next(15 * 100 + 66 * 2, 15, 66).nudge, "held while it nudges: it goes on");
        }
        // The hogs gone: calm wakes, its own nudging on a calm CPU (66 timer wakes at 0.5 µs).
        let gone = p.at;
        let stopped = p.stops(gone, 15 + 33, 15, 66);
        assert!(stopped.is_some_and(|d| d >= LINGER - PERIOD && d <= LINGER + PERIOD), "stops a tolerance after the last held period: {stopped:?}");
        // Held once more, then idle (no wake but its timers'): nudges a tolerance, then stops.
        assert!(p.next(15 * 450, 15, 1).nudge);
        let idle = p.at;
        let stopped = p.stops(idle, 66, 0, 66);
        assert!(stopped.is_some_and(|d| d <= LINGER + PERIOD), "an idle shard stops: {stopped:?}");
    }

    /// Goal 8, plan 01a1a59c2b29 step 7: a shard held now and then (one period in ten held) nudges throughout, its
    /// tolerance bridging the periods between.
    #[test]
    fn g8_s7_a_shard_held_now_and_then_nudges_between() {
        let t0 = Instant::now();
        let mut judge = Judge::default();
        let mut r = Reading::default();
        let mut nudged = 0;
        for i in 1..=500_u32 {
            let held = i % 10 == 0;
            r = after(r, if held { 4 * 450 } else { 4 }, 4, 1);
            let v = judge.period(t0 + PERIOD * i, r);
            if i > 10 && v.nudge {
                nudged += 1;
            }
        }
        assert_eq!(nudged, 490, "from the first held period on, every period");
    }

    /// Goal 8, plan 01a1a59c2b29 step 7: the kernel's `schedstat` line gives the run delay, its second number; zeros (no
    /// `CONFIG_SCHED_INFO`) and anything else give none.
    #[test]
    fn g8_s7_the_run_wait_is_schedstat_s_second_number() {
        assert_eq!(run_wait_of(b"123456789 4567 89\n"), Some(4567));
        assert_eq!(run_wait_of(b"1 0 1\n"), Some(0));
        assert_eq!(run_wait_of(b"0 0 0\n"), None, "the kernel keeps none");
        assert_eq!(run_wait_of(b"12 34\n"), None);
        assert_eq!(run_wait_of(b"twelve 34 56\n"), None);
        assert_eq!(run_wait_of(b""), None);
    }

    /// Goal 8, plan 01a1a59c2b29 step 7: on Linux a thread reads its own wait, which only grows; elsewhere there's none.
    #[test]
    fn g8_s7_a_thread_reads_its_own_wait_for_a_cpu() {
        let seen = std::thread::spawn(|| {
            let wait = RunWait::open()?;
            let first = wait.read()?;
            let mut spin = 0_u64;
            for i in 0..1_000_000_u64 {
                spin = spin.wrapping_add(std::hint::black_box(i));
            }
            std::thread::sleep(Duration::from_millis(5));
            Some((first, wait.read()?, spin))
        })
        .join()
        .ok()
        .flatten();
        // A kernel without `CONFIG_SCHED_INFO` keeps none (zeros), as every OS but Linux.
        let kept = cfg!(target_os = "linux") && std::fs::read("/proc/self/schedstat").ok().and_then(|text| run_wait_of(&text)).is_some();
        if kept {
            assert!(seen.is_some_and(|(first, then, _)| then >= first), "{seen:?}");
        } else {
            assert_eq!(seen, None);
        }
    }

    /// Goal 9, plan 01a1a87b2cec step 5; consts-audit B3: a shard that judges nothing by its wait (off, the default; or
    /// on, whatever its host does) reads it at its first probe period and where a read of `/metrics` is due, as the probe
    /// copies its other counts (it read it every 5th period, `COUNTS_EVERY`); one that nudges on its own (auto) reads it
    /// every period, as it judges each. Do: twenty periods, a read of `/metrics` due at the 7th and 8th and the 19th.
    /// Expect: off and on read at the first and those; auto at every one.
    #[test]
    fn b3_a_shard_that_judges_nothing_reads_its_wait_for_metrics_reads() {
        let due = |p: u64| [7, 8, 19].contains(&p);
        let reads = |mut here: Here| (0..20).filter(|p| here.reads_wait(due(*p))).collect::<Vec<u64>>();
        assert_eq!(reads(Here::default()), [0, 7, 8, 19], "off");
        assert_eq!(reads(Here { armed: true, ..Here::default() }), [0, 7, 8, 19], "on");
        assert_eq!(reads(Here { may_ns: 166_700, ..Here::default() }), (0..20).collect::<Vec<u64>>(), "auto");
    }

    /// Goal 8, plan 01a1a59c2b29 step 7: `[server] nudge` takes a mode's word; the lab's `KILN_RT_NUDGE` too, and the
    /// words a flag is said with, as its scripts set it. The flag the setting was before it had modes (`nudge = true`),
    /// which no config of kiln's says, and an environment's word it doesn't know are refused, not read as on.
    #[test]
    fn g8_s7_nudge_takes_a_modes_word() -> anyhow::Result<()> {
        #[derive(serde::Deserialize)]
        struct Server {
            nudge: Mode,
        }
        for (said, mode) in [("\"auto\"", Mode::Auto), ("\"on\"", Mode::On), ("\"off\"", Mode::Off), ("\"Auto\"", Mode::Auto)] {
            let s: Server = toml::from_str(&format!("nudge = {said}"))?;
            anyhow::ensure!(s.nudge == mode, "nudge = {said}: {:?}", s.nudge);
        }
        for refused in ["\"sometimes\"", "true", "false"] {
            anyhow::ensure!(toml::from_str::<Server>(&format!("nudge = {refused}")).is_err(), "nudge = {refused} is refused");
        }
        anyhow::ensure!(Mode::default() == Mode::Off, "off until declared (docs/ci.md §0.3, opt-in automation)");
        anyhow::ensure!(crate::config::Server::default().nudge == Mode::Off, "a server's config: off until it declares a mode");
        for (v, mode) in [("1", Some(Mode::On)), ("true", Some(Mode::On)), ("0", Some(Mode::Off)), ("off", Some(Mode::Off)), ("no", Some(Mode::Off)), (" auto ", Some(Mode::Auto)), ("yes please", None)] {
            anyhow::ensure!(Mode::from_env(v) == mode, "KILN_RT_NUDGE={v}");
        }
        for mode in [Mode::Off, Mode::On, Mode::Auto] {
            anyhow::ensure!(Mode::of_index(mode.index()) == mode && Mode::parse(mode.as_str()) == Some(mode));
        }
        Ok(())
    }
}
