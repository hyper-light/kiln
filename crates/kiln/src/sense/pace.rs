//! What bears on how fast a run would finish on this runner, read on events and never on a timer
//! (W05-R26, W13-R01; docs/runners.md "Pace", docs/needs.md "Pace"): its cores' clocks, its
//! contention, its swapping, its work device's load and its battery, turned into its pace by
//! [`kiln_ci::pace`] and said in its word to owners (`crate::reach::place`).
//!
//! - **What's read** ([`Raw`], `crate::os::pace`, unprivileged on every OS): Linux's pressure stall
//!   totals, per-CPU ticks and clocks (cpufreq), swap (`/proc/meminfo`, `/proc/vmstat`, the runner
//!   cgroup's `memory.swap.current`) and `/sys/class/power_supply`; macOS's thermal pressure level
//!   (notify(3)), load average, swap (`vm.swapusage`, `host_statistics64`) and IOKit's power
//!   sources; Windows' processor clocks against their limit (`CallNtPowerInformation`), run queue
//!   and paging (PDH), page file (`GlobalMemoryStatusEx`) and `GetSystemPowerStatus`; and the work
//!   device's counters on all three (`super::disk`).
//! - **When:** as a run starts ([`at_start`], the signals its slowdown is learned against) and
//!   ends ([`ended`], learned), when the fleet's fit gives this runner a new speed ([`set_speed`]),
//!   and on the OS's own events ([`start`]'s thread, blocked until one comes): a PSI trigger on
//!   each of `/proc/pressure/{cpu,memory,io}` (a window's stall past one 1/1024 of it, the least
//!   that moves slowness by one of its units; then once a window later, to see it end), the
//!   kernel's power-supply uevents; macOS's thermal and power-source notifications; Windows' power
//!   source change. Nothing runs between them; no read is on placement's decision path, which only
//!   carries the last pace in its words.
//! - **Learned here, kept here** (`<state>/pace.json`): the slowdown weights from this runner's own
//!   runs ([`kiln_ci::pace::slow::Learner`]) and its work device's uncontended service time and
//!   most seen moved ([`kiln_ci::pace::device::Calibration`]).
//! - **Root would add** (Linux): PSI windows of 500 ms, not 2 s (`CAP_SYS_RESOURCE`), and triggers on
//!   system services' own cgroup files; macOS: CPU clocks (`powermetrics`); Windows: thermal zone
//!   temperatures (WMI's `MSAcpi_ThermalZoneTemperature`). None of them is needed for the figures.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use kiln_ci::pace::Pace;
use kiln_ci::pace::device::{self, Calibration};
use kiln_ci::pace::power::OnBattery;
use kiln_ci::pace::slow::{self, Learner, Signals};
use kiln_ci::runtime::Observation;
use serde::{Deserialize, Serialize};

use super::disk::{Activity, Counters};

/// One read of this machine's counters, as `crate::os::pace` makes it: each none where this OS
/// keeps no such count, or it couldn't be read.
#[derive(Debug, Clone, Default)]
pub struct Raw {
    /// The machine's cumulative `some` stall of CPU, memory and I/O, µs (Linux's PSI).
    pub stall_us: [Option<u64>; 3],
    /// Each CPU's busy and total ticks so far, by its number (Linux: `/proc/stat`).
    pub ticks: BTreeMap<u32, (u64, u64)>,
    /// Each CPU's clock now and its most, kHz, by its number (Linux: cpufreq).
    pub clocks: BTreeMap<u32, (u64, u64)>,
    /// The clocks' share of their most where the OS says it whole (Windows: Σ `MhzLimit` / Σ
    /// `MaxMhz`, "the maximum specified processor clock frequency multiplied by the current
    /// processor thermal throttle limit").
    pub clock: Option<f64>,
    /// macOS's thermal pressure level.
    pub level: Option<u8>,
    /// Tasks wanting the CPUs, running or waiting, on average now: macOS's 1-minute load average;
    /// Windows' processor queue plus the CPUs busy.
    pub runnable: Option<f64>,
    pub cpus: u32,
    /// Pages swapped in so far (Linux `pswpin`, macOS `swapins`).
    pub swapins: Option<u64>,
    /// Pages read in a second to resolve faults, as the OS rates it (Windows' `Memory\Pages
    /// Input/sec`, over the interval since its previous read).
    pub swapin_rate: Option<f64>,
    /// Swap space, total and in use, bytes (Linux's swap; macOS's `vm.swapusage`; Windows' page
    /// files).
    pub swap: Option<(u64, u64)>,
    /// What of it the runner's own cgroup holds (Linux `memory.swap.current`).
    pub swap_own: Option<u64>,
    /// The work device's counters.
    pub device: Option<Counters>,
    /// On battery (true) or its charger (false); none without a battery.
    pub on_battery: Option<bool>,
    pub percent: Option<u8>,
}

/// What this runner learns of itself and keeps across restarts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Kept {
    pub learner: Learner,
    pub calibration: Calibration,
    /// The most bytes a second its work device was seen to move (a lower bound on what it can).
    pub peak_bps: f64,
}

/// What two reads `interval_us` apart say.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Read {
    pub signals: Signals,
    /// The work device's utilization by M/M/1 against its own uncontended latency.
    pub device_rho: Option<f64>,
    /// The work device's latency now against its uncontended: R / S₀.
    pub device_stretch: Option<f64>,
    /// What it would give a new run's I/O now, bytes a second (0 unmeasured).
    pub disk_bps: u64,
}

/// The share of `interval_us` a cumulative µs count grew by.
#[allow(clippy::cast_precision_loss)]
fn share(a: Option<u64>, b: Option<u64>, interval_us: u64) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) if interval_us > 0 => Some((b.saturating_sub(a) as f64 / interval_us as f64).clamp(0.0, 1.0)),
        _ => None,
    }
}

/// The clocks the CPUs at work ran at against their most, each CPU weighted by the ticks it was
/// busy over the interval: none when none was busy (an idle core clocks down, which is no
/// throttle).
#[allow(clippy::cast_precision_loss)]
pub fn clock_of(earlier: &Raw, later: &Raw) -> Option<f64> {
    let (mut sum, mut busy) = (0.0, 0.0);
    for (cpu, (b1, _)) in &later.ticks {
        let (Some((b0, _)), Some((cur, max))) = (earlier.ticks.get(cpu), later.clocks.get(cpu)) else { continue };
        let w = b1.saturating_sub(*b0) as f64;
        if *max == 0 || w <= 0.0 {
            continue;
        }
        sum += w * (*cur as f64 / *max as f64);
        busy += w;
    }
    (busy > 0.0).then(|| sum / busy)
}

/// What reads `earlier` and `later`, `interval_us` apart, say, the device's history in `kept`
/// taking in what this interval showed of it.
#[allow(clippy::cast_precision_loss)]
pub fn between(earlier: &Raw, later: &Raw, interval_us: u64, kept: &mut Kept) -> Read {
    let [c0, m0, i0] = earlier.stall_us;
    let [c1, m1, i1] = later.stall_us;
    let mut s = Signals { cpu: share(c0, c1, interval_us), memory: share(m0, m1, interval_us), io: share(i0, i1, interval_us), ..Signals::default() };
    if s.cpu.is_none() {
        s.cpu = later.runnable.and_then(|r| slow::cpu_share(r, later.cpus));
    }
    s.clock = later.clock.or_else(|| clock_of(earlier, later));
    s.level = later.level;
    let secs = interval_us as f64 / 1e6;
    let activity = earlier.device.as_ref().zip(later.device.as_ref()).and_then(|(a, b)| Activity::between(a, b, interval_us));
    let (mut rho, mut stretch) = (None, None);
    if let Some(a) = &activity {
        let ops = a.read_ops_per_s + a.write_ops_per_s;
        let latency = device::mean_latency(a.read_latency_us, a.read_ops_per_s, a.write_latency_us, a.write_ops_per_s);
        if let Some(l) = latency {
            kept.calibration.add(l, a.in_flight, ops * secs);
        }
        let moved = a.read_bytes_per_s + a.write_bytes_per_s;
        if moved.is_finite() && moved > kept.peak_bps {
            kept.peak_bps = moved;
        }
        match (kept.calibration.service_us(), latency) {
            (Some(s0), Some(l)) => {
                rho = device::utilization(s0, l);
                stretch = Some(l / s0);
            }
            // No request in the interval: nothing waited.
            (_, None) if ops <= 0.0 => rho = Some(0.0),
            _ => {}
        }
    }
    if s.io.is_none() {
        s.io = rho;
    }
    let rate = later.swapin_rate.or_else(|| match (earlier.swapins, later.swapins) {
        (Some(a), Some(b)) if secs > 0.0 => Some(b.saturating_sub(a) as f64 / secs),
        _ => None,
    });
    s.swap = rate.zip(kept.calibration.service_us()).and_then(|(r, s0)| device::swap_share(r, s0));
    let disk_bps = rho.map_or(0, |r| device::rate(kept.peak_bps, r));
    Read { signals: s, device_rho: rho, device_stretch: stretch, disk_bps }
}

/// Who's told this runner's pace, and whether it drains on battery (and why), each time it changes.
type Tell = Box<dyn Fn(Pace, Option<String>) + Send>;

/// This runner's own: its reader, its last read, what it learned, and who's told.
struct Keeper {
    reader: Option<crate::os::pace::Reader>,
    last: Option<(Instant, Raw)>,
    kept: Kept,
    path: Option<PathBuf>,
    read: Read,
    speed: u32,
    policy: OnBattery,
    on_battery: Option<bool>,
    tell: Option<Tell>,
    told: Option<(Pace, Option<String>)>,
}

fn keeper() -> &'static Mutex<Keeper> {
    static K: OnceLock<Mutex<Keeper>> = OnceLock::new();
    K.get_or_init(|| Mutex::new(Keeper { reader: None, last: None, kept: Kept::default(), path: None, read: Read::default(), speed: 0, policy: OnBattery::default(), on_battery: None, tell: None, told: None }))
}

/// How many times the OS's events woke this runner's pace so far, whatever woke it: an idle
/// runner's stay where they are.
pub fn wakes() -> u64 {
    crate::os::pace::wakes()
}

impl Keeper {
    /// Reads now: what the interval since the last read says, kept.
    fn measure(&mut self) {
        let Some(reader) = self.reader.as_mut() else { return };
        let now = (Instant::now(), reader.read());
        if let Some((at, before)) = &self.last {
            let us = u64::try_from(now.0.duration_since(*at).as_micros()).unwrap_or(u64::MAX);
            self.read = between(before, &now.1, us, &mut self.kept);
        }
        self.on_battery = now.1.on_battery;
        self.last = Some(now);
    }

    /// The pace as it stands, and why it drains if it does.
    fn pace(&self) -> (Pace, Option<String>) {
        let w = self.kept.learner.weights();
        let s = &self.read.signals;
        let measured = self.last.is_some() && (s.cpu.is_some() || s.memory.is_some() || s.io.is_some());
        let pace = Pace { speed: w.speed_now(self.speed, s, self.kept.learner.mean_thermal()), slowness: if measured { w.slowness(s) } else { 0 }, disk_bps: self.read.disk_bps };
        let drain = self.policy.drains(self.on_battery).then(|| self.policy.why().to_owned());
        (pace, drain)
    }

    /// Told when it changed.
    fn tell(&mut self) {
        let now = self.pace();
        if self.told.as_ref() == Some(&now) {
            return;
        }
        if let Some(t) = &self.tell {
            t(now.0, now.1.clone());
        }
        if now.1 != self.told.as_ref().and_then(|t| t.1.clone()) {
            match &now.1 {
                Some(why) => tracing::info!(%why, "this runner takes nothing new on battery"),
                None if self.told.is_some() => tracing::info!("this runner takes work again"),
                None => {}
            }
        }
        self.told = Some(now);
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        let Ok(json) = serde_json::to_vec(&self.kept) else { return };
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, path)) {
            tracing::warn!(error = %e, path = %path.display(), "couldn't keep what this runner learned of its pace");
        }
    }
}

/// This runner measures its pace from now on: its state in `state_dir` (`pace.json`), its work
/// device the one `work_dir` is on, `policy` on battery, `tell` told each change; and a thread
/// waits on the OS's events. Once; a second call changes nothing.
pub fn start(state_dir: &Path, work_dir: &Path, policy: OnBattery, tell: Tell) {
    let Ok(mut k) = keeper().lock() else { return };
    if k.reader.is_some() {
        return;
    }
    let path = state_dir.join("pace.json");
    k.kept = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    k.path = Some(path);
    k.policy = policy;
    k.reader = Some(crate::os::pace::Reader::new(work_dir));
    k.tell = Some(tell);
    k.measure();
    k.tell();
    drop(k);
    hear();
}

/// What the OS said that bears on this machine's pace or power, and runs' starts and ends: each
/// counted, and told to whoever waits on it (goal 9): the runner's loop takes its samples on these
/// ([`moved_since`]) and reads its battery on the power ones ([`powered_since`]), never on a clock.
struct Heard {
    moved: crate::rt::Changed,
    moves: AtomicU64,
    powered: crate::rt::Changed,
    powers: AtomicU64,
}

fn heard() -> &'static Heard {
    static H: OnceLock<Heard> = OnceLock::new();
    H.get_or_init(|| Heard { moved: crate::rt::Changed::new(), moves: AtomicU64::new(0), powered: crate::rt::Changed::new(), powers: AtomicU64::new(0) })
}

/// Something moved: a run started or ended, or the OS's word came (`power`: of its power source or
/// its battery's charge).
fn moved(power: bool) {
    let h = heard();
    h.moves.fetch_add(1, Ordering::SeqCst);
    h.moved.notify();
    if power {
        h.powers.fetch_add(1, Ordering::SeqCst);
        h.powered.notify();
    }
}

/// The OS's events heard from now on (once; [`start`] calls it too): its thread measures the pace
/// when placement does ([`start`]), and tells this runner's loop. Placement measuring itself or
/// not, the runner's samples and battery wait on them.
pub fn hear() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        crate::os::pace::watch(|power| {
            if let Ok(mut k) = keeper().lock()
                && k.reader.is_some()
            {
                k.measure();
                k.tell();
            }
            moved(power);
        });
    });
}

/// How many moves so far ([`moved_since`]).
pub fn moves() -> u64 {
    heard().moves.load(Ordering::SeqCst)
}

/// How many of them were power's ([`powered_since`]).
pub fn powers() -> u64 {
    heard().powers.load(Ordering::SeqCst)
}

/// Waits until there were more moves than `seen` (at once if there are already).
pub async fn moved_since(seen: u64) {
    let h = heard();
    let wait = h.moved.listen();
    if h.moves.load(Ordering::SeqCst) != seen {
        return;
    }
    wait.await;
}

/// Waits until there were more power events than `seen` (at once if there are already).
pub async fn powered_since(seen: u64) {
    let h = heard();
    let wait = h.powered.listen();
    if h.powers.load(Ordering::SeqCst) != seen {
        return;
    }
    wait.await;
}

/// A run starts: the machine read now, and its signals for the run's observation (none before
/// [`start`]).
pub fn at_start() -> Option<Signals> {
    moved(false);
    let mut k = keeper().lock().ok()?;
    k.reader.as_ref()?;
    k.measure();
    k.tell();
    Some(k.read.signals)
}

/// A run ended: what it says of this machine's slowdown learned, kept, and the machine read again.
pub fn ended(o: &Observation) {
    moved(false);
    let Ok(mut k) = keeper().lock() else { return };
    if k.reader.is_none() {
        return;
    }
    if o.cause.is_none()
        && let Some(s) = &o.signals
    {
        k.kept.learner.observe(&o.key, o.wall_s, s);
        k.save();
    }
    k.measure();
    k.tell();
}

/// The fleet's fit gives this runner per-core speed `speed` (1/1024 of the reference core's; 0
/// unmeasured), as the server's directory says it.
pub fn set_speed(speed: u32) {
    let Ok(mut k) = keeper().lock() else { return };
    if k.speed == speed {
        return;
    }
    k.speed = speed;
    k.tell();
}

/// What stands for the kernel's stall times where it keeps none, as last read: swap-ins' share of
/// time (for memory's), and the work device's utilization and latency stretch (for I/O's).
pub fn stand_ins() -> (Option<f64>, Option<f64>, Option<f64>) {
    keeper().lock().map_or((None, None, None), |k| (k.read.signals.swap, k.read.device_rho, k.read.device_stretch))
}

/// The runner's pace as last told (for its page).
pub fn current() -> Option<Pace> {
    keeper().lock().ok().and_then(|k| k.told.as_ref().map(|t| t.0))
}

pub mod parse {
    //! The text of Linux's files the reads take (here, so they're tested on every platform).

    use std::collections::BTreeMap;

    /// `/proc/stat`'s per-CPU lines: each CPU's busy and total ticks, by its number (idle and
    /// iowait are idle: a CPU waiting on I/O is free to run anything else).
    pub fn per_cpu(text: &str) -> BTreeMap<u32, (u64, u64)> {
        let mut out = BTreeMap::new();
        for line in text.lines() {
            let mut w = line.split_whitespace();
            let Some(n) = w.next().and_then(|c| c.strip_prefix("cpu")).and_then(|n| n.parse::<u32>().ok()) else { continue };
            let v: Vec<u64> = w.filter_map(|x| x.parse().ok()).collect();
            if v.len() < 4 {
                continue;
            }
            let total: u64 = v.iter().sum();
            let idle = v.get(3).copied().unwrap_or(0).saturating_add(v.get(4).copied().unwrap_or(0));
            out.insert(n, (total.saturating_sub(idle), total));
        }
        out
    }

    /// A `/proc/vmstat` or `cpu.stat` count by its name.
    pub fn keyed(text: &str, key: &str) -> Option<u64> {
        text.lines().find_map(|l| {
            let mut w = l.split_whitespace();
            (w.next()? == key).then(|| w.next()?.parse().ok()).flatten()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 9: a run's start or end, or the OS's word, moves the runner's samples' wait; a power
    /// event moves its battery's too, and nothing else does.
    #[test]
    fn g9_moves_and_power_end_their_waits() -> anyhow::Result<()> {
        let (m, p) = (moves(), powers());
        moved(false);
        anyhow::ensure!(moves() > m && powers() == p, "a run's move is no power event");
        moved(true);
        anyhow::ensure!(powers() > p);
        crate::rt::block_on(async move {
            crate::rt::timeout(std::time::Duration::from_secs(10), moved_since(m)).await.map_err(|_| anyhow::anyhow!("moves past what was seen didn't end the wait"))?;
            crate::rt::timeout(std::time::Duration::from_secs(10), powered_since(p)).await.map_err(|_| anyhow::anyhow!("a power event didn't end its wait"))?;
            Ok(())
        })
    }

    fn raw(stall: [u64; 3], ticks: &[(u32, u64, u64)], clocks: &[(u32, u64, u64)]) -> Raw {
        Raw {
            stall_us: stall.map(Some),
            ticks: ticks.iter().map(|(c, b, t)| (*c, (*b, *t))).collect(),
            clocks: clocks.iter().map(|(c, f, m)| (*c, (*f, *m))).collect(),
            cpus: u32::try_from(ticks.len()).unwrap_or(0),
            ..Raw::default()
        }
    }

    /// W05-R26: two reads say each stall's share of the interval, exactly; and the clocks of the
    /// CPUs that worked, weighted by their work. An idle CPU's low clock is no throttle.
    #[test]
    fn w05_r26_between_takes_shares_and_working_clocks() {
        let a = raw([0, 0, 0], &[(0, 100, 1000), (1, 100, 1000)], &[]);
        let b = raw([500_000, 0, 250_000], &[(0, 900, 2000), (1, 100, 2000)], &[(0, 2_000_000, 4_000_000), (1, 800_000, 4_000_000)]);
        let r = between(&a, &b, 1_000_000, &mut Kept::default());
        assert_eq!(r.signals.cpu, Some(0.5));
        assert_eq!(r.signals.memory, Some(0.0));
        assert_eq!(r.signals.io, Some(0.25));
        assert_eq!(r.signals.clock, Some(0.5), "only CPU 0 worked: its clock, not idle CPU 1's");
        assert_eq!(clock_of(&a, &a), None);
    }

    /// W05-R26: where there's no PSI (macOS, Windows), the run queue stands for CPU stall and the
    /// device's latency against its own uncontended for I/O, and swap-ins' share is the utilization
    /// law's; the disk's rate is what its capacity leaves.
    #[test]
    fn w05_r26_between_without_psi() {
        let dev = |ops: u64, ns: u64, inflight_ns: u64, bytes: u64| Counters { read: super::super::disk::Dir { ops, bytes, time_ns: ns }, in_flight_ns: Some(inflight_ns), ..Counters::default() };
        let mut kept = Kept::default();
        // An idle-ish second calibrates: 1000 reads of 100 µs, under one in flight.
        let a = Raw { cpus: 8, runnable: Some(3.0), device: Some(dev(0, 0, 0, 0)), swapins: Some(0), ..Raw::default() };
        let b = Raw { cpus: 8, runnable: Some(15.0), device: Some(dev(1000, 100_000_000, 100_000_000, 1_000_000_000)), swapins: Some(500), ..Raw::default() };
        let r = between(&a, &b, 1_000_000, &mut kept);
        assert!(matches!(r.signals.cpu, Some(p) if (p - 0.5).abs() < 1e-12), "{r:?}");
        assert!(kept.calibration.service_us().is_some_and(|s| s > 0.0 && s <= 100.0), "{kept:?}");
        assert!(r.signals.swap.is_some_and(|s| s > 0.0), "{r:?}");
        // A loaded second: latency four times as long with the same requests: ρ near 3/4.
        let c = Raw { device: Some(dev(2000, 500_000_000, 1_400_000_000, 1_500_000_000)), ..b.clone() };
        let r2 = between(&b, &c, 1_000_000, &mut kept);
        assert!(r2.device_rho.is_some_and(|p| p > 0.5), "{r2:?}");
        assert_eq!(r2.signals.io, r2.device_rho);
        assert!(r2.disk_bps > 0 && (r2.disk_bps as f64) < kept.peak_bps, "{r2:?}");
    }

    /// W05-R26: the parsers of Linux's files.
    #[test]
    fn w05_r26_parse_per_cpu_and_keyed() {
        let stat = "cpu  10 0 10 80 0 0 0 0 0 0\ncpu0 5 0 5 40 10 0 0 0 0 0\ncpu1 5 0 5 40 0 0 0 0 0 0\nintr 1 2\n";
        let p = parse::per_cpu(stat);
        assert_eq!(p.get(&0), Some(&(10, 60)));
        assert_eq!(p.get(&1), Some(&(10, 50)));
        assert_eq!(p.len(), 2);
        assert_eq!(parse::keyed("pswpin 42\npswpout 7\n", "pswpin"), Some(42));
        assert_eq!(parse::keyed("pswpin x\n", "pswpin"), None);
    }
}
