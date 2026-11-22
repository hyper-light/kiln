//! Whether this runner should be offered a piece of work, by its own health (S6; L1, L2): the
//! one predicate its admission consults ([`check`]), from what it measures and what its runs met.
//!
//! **Strain.** A runner is strained when the time its tasks all stalled on memory and on I/O
//! (Linux PSI's `full` shares, exact over the measure's interval) is more than the headroom its
//! scheduler keeps: `1 − target_use` (`[ci.scheduler] target_use`, the person's setting). That is
//! the engine's own rule ([`kiln_ci::sched::state::Headroom::fullness`]: a machine is full past
//! `target_use` when its larger share in use plus its stalls passes it) with nothing in use: stall
//! time is capacity lost, and past the reserve no idle CPU makes room. It is weighed where the
//! gate's guard looks (`ci/gate.sh`): the machine, the runner's own cgroup, and kiln's other
//! services beside it (`kiln*.service`), the worst of them, so a runner the gate would yield on
//! isn't offered work. No threshold is picked here: the reserve is the setting's.
//!
//! **Machine-caused failures.** A run that ended for the machine's sake, not its own
//! ([`Failure`]: no space left on the work volume, killed for memory in the runner's own group or
//! by the OS's memory pressure, a tool not found), bars work of its kind until the cause clears;
//! so do the work device's errors while they rise. Each clears by an event, never by a timer: a
//! measure showing the space freed, the memory back, the device quiet through I/O, or facts
//! probed again with the tool found. Asking for more disk than the work volume has free is
//! refused on its own.

use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use super::Measure;

/// What a run ended of, when the machine was the cause.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cause", rename_all = "snake_case")]
pub enum Failure {
    /// The work volume was full (`ENOSPC`: "No space left on device", Windows' `ERROR_DISK_FULL`).
    NoSpace,
    /// Killed for memory: the OOM killer in the runner's own cgroup (Linux `memory.events`
    /// `oom_kill`), jetsam under critical pressure (macOS: killed by a signal while the level was
    /// critical), or out of memory or commit (Windows' `STATUS_NO_MEMORY`,
    /// `STATUS_COMMITMENT_LIMIT`).
    OutOfMemory,
    /// A program it ran isn't here (exit 127 from a shell, 9009 from `cmd`), by name where the
    /// shell said it.
    MissingTool { tool: Option<String> },
}

impl Failure {
    /// Its name as an observation keeps it.
    pub fn as_str(&self) -> String {
        match self {
            Failure::NoSpace => "no_space".to_owned(),
            Failure::OutOfMemory => "out_of_memory".to_owned(),
            Failure::MissingTool { tool: Some(t) } => format!("missing_tool:{t}"),
            Failure::MissingTool { tool: None } => "missing_tool".to_owned(),
        }
    }
}

/// What a finished run's end says of the machine, from its exit, what it printed, and what its
/// OS counted while it ran. Pure.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ending {
    /// Its exit code; none when a signal ended it.
    pub exit_code: Option<i32>,
    /// It said `ENOSPC` in its output ([`Scan`]).
    pub said_no_space: bool,
    /// The tool its shell said it couldn't find ([`Scan`]).
    pub said_missing: Option<String>,
    /// OOM kills in the runner's own cgroup (and the run's) while it ran.
    pub oom_kills: u64,
    /// macOS's memory pressure level as the run ended.
    pub memory_level: Option<super::MemoryLevel>,
}

/// Windows' `STATUS_NO_MEMORY` and `STATUS_COMMITMENT_LIMIT` as exit codes.
const WINDOWS_NO_MEMORY: [i32; 2] = [0xC000_0017_u32 as i32, 0xC000_012D_u32 as i32];

/// Whether a run's end was the machine's doing, and which.
pub fn classify(e: &Ending) -> Option<Failure> {
    if e.exit_code == Some(0) {
        return None;
    }
    if e.oom_kills > 0 || (e.exit_code.is_none() && e.memory_level == Some(super::MemoryLevel::Critical)) || e.exit_code.is_some_and(|c| WINDOWS_NO_MEMORY.contains(&c)) {
        return Some(Failure::OutOfMemory);
    }
    if e.said_no_space {
        return Some(Failure::NoSpace);
    }
    // 127: POSIX sh's "command not found"; 9009: cmd's "is not recognized".
    if matches!(e.exit_code, Some(127 | 9009)) || e.said_missing.is_some() && e.exit_code.is_some() {
        return Some(Failure::MissingTool { tool: e.said_missing.clone() });
    }
    None
}

/// A streaming look over a run's output for what [`classify`] reads: `ENOSPC`'s message and a
/// shell's "not found". It keeps only the line being read, no longer than the longest message
/// with a file name in it (`NAME_MAX`, 255 bytes on Linux, macOS and Windows alike).
#[derive(Debug, Default)]
pub struct Scan {
    line: Vec<u8>,
    pub no_space: bool,
    pub missing: Option<String>,
}

/// The longest line kept: a message and a name of `NAME_MAX` bytes in it.
const LINE_KEPT: usize = 255 + 64;

impl Scan {
    pub fn feed(&mut self, data: &[u8]) {
        for &b in data {
            if b == b'\n' {
                self.line_ends();
            } else if self.line.len() < LINE_KEPT {
                self.line.push(b);
            }
        }
    }

    /// The output ended: its last line counts too.
    pub fn end(&mut self) {
        self.line_ends();
    }

    /// Two streams' looks as one (a run's stdout and stderr, each read apart): `ENOSPC` said in
    /// either, the tool `other` named first.
    pub fn with(self, other: Scan) -> Scan {
        Scan { line: Vec::new(), no_space: self.no_space || other.no_space, missing: other.missing.or(self.missing) }
    }

    fn line_ends(&mut self) {
        let line = String::from_utf8_lossy(&self.line).trim_end_matches('\r').to_owned();
        self.line.clear();
        if line.contains("No space left on device") || line.contains("There is not enough space on the disk") {
            self.no_space = true;
        }
        if let Some(tool) = missing_tool(&line) {
            self.missing = Some(tool);
        }
    }
}

/// The program a shell said it couldn't find, from its line: bash's `bash: foo: command not
/// found` (and `bash: line 1: foo: …`), dash's `sh: 1: foo: not found`, zsh's `zsh: command not
/// found: foo`, cmd's `'foo' is not recognized as an internal or external command`.
pub fn missing_tool(line: &str) -> Option<String> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("zsh: command not found: ") {
        return Some(rest.trim().to_owned()).filter(|t| !t.is_empty());
    }
    if let Some(name) = line.strip_prefix('\'').and_then(|r| r.split_once("' is not recognized as an internal or external command")).map(|(n, _)| n) {
        return Some(name.to_owned()).filter(|t| !t.is_empty());
    }
    let before = line.strip_suffix(": command not found").or_else(|| line.strip_suffix(": not found"))?;
    let name = before.rsplit(": ").next()?.trim();
    (!name.is_empty() && !name.contains(' ')).then(|| name.to_owned())
}

/// What a piece of work needs of the machine, as admission asks it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ask {
    /// Its key (`cmd:…`, `leg:…`), when known: a cause its own runs met bars it here.
    pub key: Option<String>,
    /// Bytes of the work volume it needs free (0: it didn't say).
    pub disk: u64,
    /// Bytes of memory it asks (0: it didn't say).
    pub mem: u64,
    /// Programs it needs (`needs` `tool:…`).
    pub tools: Vec<String>,
}

/// Why admission shouldn't offer the work here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Refusal {
    /// `strained`, `disk`, `io_errors`, `memory`, `tool`.
    pub kind: String,
    pub why: String,
}

/// A cause that bars work here until it clears.
#[derive(Debug, Clone, PartialEq)]
enum Barred {
    /// No space: until the work volume has more free than it had then, by what the run wrote.
    Space { key: String, free_then: u64, wrote: u64 },
    /// Killed for memory: until memory available is more than the run's largest process held.
    Memory { key: String, needed: u64 },
    /// The work device's errors rose: until a measure shows I/O done with none.
    IoErrors { errors: u64 },
    /// A program wasn't found: until facts are probed with it there.
    Tool { key: String, tool: Option<String> },
}

/// This runner's health: the causes that bar work now.
#[derive(Debug, Default)]
pub struct Health {
    barred: Vec<Barred>,
}

impl Health {
    /// A run of `key` ended with `failure` (none: nothing of the machine's), the machine as `m`
    /// read at its end; `wrote` and `peak` what it wrote and its largest process's memory.
    pub fn ended(&mut self, key: &str, failure: Option<&Failure>, m: &Measure, wrote: u64, peak: u64) {
        let key = key.to_owned();
        let new = match failure {
            Some(Failure::NoSpace) => Barred::Space { key, free_then: m.space.map_or(0, |s| s.free), wrote },
            Some(Failure::OutOfMemory) => Barred::Memory { key, needed: peak },
            Some(Failure::MissingTool { tool }) => Barred::Tool { key, tool: tool.clone() },
            None => {
                // The same key running clean here clears what its own runs met of a tool.
                self.barred.retain(|b| !matches!(b, Barred::Tool { key: k, tool: None } if *k == key));
                return;
            }
        };
        self.barred.retain(|b| std::mem::discriminant(b) != std::mem::discriminant(&new) || !same_key(b, &new));
        self.barred.push(new);
    }

    /// A measure was read: causes it shows cleared go, and the device's errors rising bar I/O.
    pub fn measured(&mut self, m: &Measure) {
        let free = m.space.map(|s| s.free);
        let errors = m.device.as_ref().map(|d| d.errors);
        let did_io = m.device.as_ref().is_some_and(|d| d.read_ops_per_s + d.write_ops_per_s > 0.0);
        self.barred.retain(|b| match b {
            Barred::Space { free_then, wrote, .. } => free.is_none_or(|f| f <= free_then.saturating_add(*wrote)),
            Barred::Memory { needed, .. } => m.mem_available <= *needed,
            Barred::IoErrors { .. } => !(did_io && errors.is_some_and(|e| !e.any() || e.errors.unwrap_or(0) + e.timeouts.unwrap_or(0) == 0)),
            Barred::Tool { .. } => true,
        });
        if let Some(e) = errors {
            let rose = e.errors.unwrap_or(0) + e.timeouts.unwrap_or(0);
            if rose > 0 {
                self.barred.retain(|b| !matches!(b, Barred::IoErrors { .. }));
                self.barred.push(Barred::IoErrors { errors: rose });
            }
        }
    }

    /// Facts were probed: a missing tool now found (`tools`, or on the search path) clears.
    pub fn probed(&mut self, tools: &dyn Fn(&str) -> bool) {
        self.probed_by(tools, true);
    }

    /// Facts were probed, `looked`: asked for by something that may have changed the machine (a
    /// connection, the server, a folder changing), not by a run's tool not found alone. A missing
    /// tool now found clears; one its shell didn't name clears at a look, never at the probe its
    /// own failure asked for (W13-R01: that one is asked as the failure comes, and would clear it
    /// at once).
    pub fn probed_by(&mut self, tools: &dyn Fn(&str) -> bool, looked: bool) {
        self.barred.retain(|b| match b {
            Barred::Tool { tool: Some(t), .. } => !tools(t),
            // A tool its shell didn't name: probed facts are the next look the machine gets.
            Barred::Tool { tool: None, .. } => !looked,
            _ => true,
        });
    }

    /// Whether `ask` should be offered here now, the machine as `m` reads, its scheduler filling
    /// machines to `target_use`.
    pub fn check(&self, ask: &Ask, m: &Measure, target_use: f64) -> Result<(), Refusal> {
        if let Some(why) = strained(m, target_use) {
            return Err(Refusal { kind: "strained".into(), why });
        }
        if ask.disk > 0
            && let Some(s) = m.space
            && s.free.max(s.important.unwrap_or(0)) < ask.disk
        {
            return Err(Refusal { kind: "disk".into(), why: format!("its work volume has {} free, under the {} asked", bytes(s.free.max(s.important.unwrap_or(0))), bytes(ask.disk)) });
        }
        let key = ask.key.as_deref();
        for b in &self.barred {
            match b {
                Barred::Space { key: k, .. } if key == Some(k) || ask.disk > 0 => return Err(Refusal { kind: "disk".into(), why: "a run here ran out of space on its work volume, and none has been freed since".into() }),
                Barred::IoErrors { errors } if ask.disk > 0 || key.is_some() => return Err(Refusal { kind: "io_errors".into(), why: format!("its work device's errors rose ({errors} in the last measure)") }),
                Barred::Memory { key: k, needed } if key == Some(k) || (ask.mem > 0 && ask.mem >= m.mem_available) => {
                    return Err(Refusal { kind: "memory".into(), why: format!("a run here was killed for memory, and no more than {} is free of the {} it held", bytes(m.mem_available), bytes(*needed)) });
                }
                Barred::Tool { key: k, tool } if key == Some(k) || tool.as_ref().is_some_and(|t| ask.tools.iter().any(|a| a == t)) => {
                    return Err(Refusal { kind: "tool".into(), why: format!("{} isn't here (a run here couldn't find it)", tool.as_deref().unwrap_or("a program it runs")) });
                }
                _ => {}
            }
        }
        Ok(())
    }
}

fn same_key(a: &Barred, b: &Barred) -> bool {
    let key = |x: &Barred| match x {
        Barred::Space { key, .. } | Barred::Memory { key, .. } | Barred::Tool { key, .. } => Some(key.clone()),
        Barred::IoErrors { .. } => None,
    };
    key(a) == key(b)
}

fn bytes(n: u64) -> String {
    crate::reach::directory::human_bytes(n)
}

/// Minimum mean I/O latency (µs) required before a non-PSI device (macOS, Windows, Linux without
/// PSI) is treated as genuinely queueing past its knee: parallel multi-queue NVMe SSDs routinely
/// serve 2-8 concurrent requests at 50-500 µs with zero contention, whereas saturated NVMe
/// (e.g. 2026-11-21 Mac re-index: 12.7 in-flight at 2194 µs) and queueing HDDs exceed 2 ms.
const IO_QUEUE_LATENCY_US: f64 = 2_000.0;
/// Extreme non-PSI latency floor (µs) before cordoning an entire runner: local headroom throttling
/// handles 2-10 ms I/O bursts cleanly; cordoning is reserved for sustained >= 10 ms saturation
/// (or >= 25 ms on slower queues).
const EXTREME_IO_LATENCY_US: f64 = 10_000.0;
const EXTREME_HDD_LATENCY_US: f64 = 25_000.0;
/// Maximum `target_use` used for emergency runner cordoning (at least 50% full-stall reserve),
/// preventing mild 11-15% transient PSI bursts on 32-64 core machines from cordoning the runner.
const EXTREME_CORDON_TARGET_USE: f64 = 0.50;

/// Why the machine is strained, or none (see the module doc): the worst of its own, the runner's
/// cgroup's and kiln's other services' memory and I/O full stalls, past `1 − target_use`.
pub fn strained(m: &Measure, target_use: f64) -> Option<String> {
    strained_with(m, target_use, super::pace::stand_ins(), false)
}

/// Emergency-only strain predicate used by placement before cordoning an entire runner: requires
/// extreme machine-wide saturation (>= 50% full stalls on PSI/swap, >= 10 ms deep queueing on
/// non-PSI devices, or macOS `MemoryLevel::Critical`) so moderate/spiky load is handled by
/// granular local headroom pacing instead of cordoning the runner.
pub fn extreme_strained(m: &Measure, target_use: f64) -> Option<String> {
    strained_with(m, target_use.min(EXTREME_CORDON_TARGET_USE), super::pace::stand_ins(), true)
}

/// Pure core of [`strained`] and [`extreme_strained`] taking the pace stand-ins explicitly so
/// every platform's edge cases are tested without global state.
fn strained_with(m: &Measure, target_use: f64, stand_ins: (Option<f64>, Option<f64>, Option<f64>, Option<f64>), extreme: bool) -> Option<String> {
    let reserve = (1.0 - target_use).clamp(0.0, 1.0);
    let stall = |p: &super::Pressure| p.memory.full.unwrap_or(0.0) + p.io.full.unwrap_or(0.0);
    let mut worst: Option<(f64, String)> = None;
    let mut weigh = |v: f64, who: &str| {
        if worst.as_ref().is_none_or(|(w, _)| v > *w) {
            worst = Some((v, who.to_owned()));
        }
    };
    let (swap, pace_rho, stretch, pace_latency_us) = stand_ins;
    // On macOS, when XNU itself reports `MemoryLevel::Normal` AND at least 10% of physical memory
    // is genuinely free (`VmPages::room`, which already deducts compressor pressure), transient
    // background swap-ins are not machine-wide thrashing and must not cordon a healthy Mac.
    let swap_effective = if m.memory_level == Some(super::MemoryLevel::Normal) && m.mem_total > 0 && m.mem_available.saturating_mul(10) >= m.mem_total {
        None
    } else {
        swap
    };
    let (memory, swapping) = machine_memory(m.pressure.memory.full, swap_effective);
    weigh(memory + m.pressure.io.full.unwrap_or(0.0), if swapping { "the machine (swapping)" } else { "the machine" });
    if let Some(c) = &m.cgroup {
        weigh(stall(&c.pressure), "the runner's own cgroup");
    }
    for s in &m.services {
        weigh(stall(&s.pressure), &s.name);
    }
    // macOS keeps no stall times: its own critical level says its working set no longer fits.
    if m.memory_level == Some(super::MemoryLevel::Critical) {
        return Some("macOS says its memory pressure is critical: even compressed, its working set no longer fits".to_owned());
    }
    // Where the kernel keeps no I/O stall time (macOS, Windows, Linux without PSI), check the work
    // device. Prefer the fresh `m.device` from this measure over the pace singleton so an idle
    // disk clears strain immediately instead of sticking on a stale event read. Parallel multi-queue
    // NVMe SSDs routinely run `in_flight > 1` (`rho > 0.50`) at sub-millisecond latency with zero
    // queueing; require real queueing latency (`>= 2 ms`, or `>= 10 ms` for emergency runner
    // cordoning) so healthy NVMe concurrency never falsely cordons a runner.
    if m.pressure.io.full.is_none() {
        let (io_rho, io_latency_us) = match &m.device {
            Some(act) => (
                kiln_ci::pace::device::utilization(act.in_flight),
                kiln_ci::pace::device::mean_latency(act.read_latency_us, act.read_ops_per_s, act.write_latency_us, act.write_ops_per_s),
            ),
            None => (pace_rho, pace_latency_us),
        };
        if let Some(rho) = io_rho {
            let knee_single = kiln_ci::sched::state::knee(1);
            let deep = target_use.max(0.85);
            let io_strained = if extreme {
                match io_latency_us {
                    Some(lat) => (rho > 0.85 && lat >= EXTREME_IO_LATENCY_US) || (rho > 0.75 && lat >= EXTREME_HDD_LATENCY_US),
                    None => rho > 0.92,
                }
            } else {
                match io_latency_us {
                    Some(lat) => rho > knee_single && (lat >= IO_QUEUE_LATENCY_US || (rho > deep && stretch.is_some_and(|s| s >= 8.0) && lat >= 1_000.0)),
                    None => rho > deep,
                }
            };
            if io_strained {
                let lat_str = io_latency_us.map(|l| format!("mean latency {:.1} ms, ", l / 1_000.0)).unwrap_or_default();
                return Some(format!("its work device is past its knee: {lat_str}{:.0}% queued by M/M/1, past the {:.0}% threshold", rho * 100.0, knee_single * 100.0));
            }
        }
    }
    let (v, who) = worst?;
    (v > reserve).then(|| format!("{who}'s tasks all stalled on memory and I/O {:.0}% of the time, past the {:.0}% the scheduler keeps in reserve (1 − target_use)", v * 100.0, reserve * 100.0))
}

/// The machine's memory stall, and whether swapping stands for it. Swapping is stall time,
/// capacity lost, as memory's stall is (W05-R26), but the kernel's own stall time comes first
/// where it keeps one: Linux's PSI counts every swap-in's wait, from whatever device the swap is
/// on. Swap-ins' share by the utilization law (U = X·S) prices each at the work device's service
/// time, which is no swap device's when swap is zram (compressed memory, microseconds to read
/// back): a machine with its memory stalled 2% of the time read as stalled past 100% and took no
/// new session. So that share stands in only where the kernel keeps no memory stall time.
fn machine_memory(full: Option<f64>, swap: Option<f64>) -> (f64, bool) {
    match (full, swap) {
        (Some(full), _) => (full, false),
        (None, Some(swap)) => (swap, true),
        (None, None) => (0.0, false),
    }
}

/// This runner's health, kept as its runs end and as it measures.
pub fn runner() -> &'static Mutex<Health> {
    static H: OnceLock<Mutex<Health>> = OnceLock::new();
    H.get_or_init(|| Mutex::new(Health::default()))
}

/// The predicate the place core's runner admission consults when it decides (lb/core; see
/// `~/.cache/kiln-lb/notes/asks/core.md`): the measure it just read applied first (what it shows
/// cleared goes), then whether `ask` should be offered here.
pub fn check(ask: &Ask, m: &Measure, target_use: f64) -> Result<(), Refusal> {
    let Ok(mut h) = runner().lock() else { return Ok(()) };
    h.measured(m);
    h.check(ask, m, target_use)
}

#[cfg(test)]
mod tests {
    use super::super::{Cgroup, Pressure, Space, Stall, disk};
    use super::*;

    const GIB: u64 = 1 << 30;

    fn calm() -> Measure {
        Measure { cpus: 8, mem_total: 32 * GIB, mem_available: 16 * GIB, space: Some(Space { total: 100 * GIB, free: 50 * GIB, important: None }), ..Measure::default() }
    }

    /// S6: strained past the scheduler's reserve, by the worst of the machine, the runner's
    /// cgroup and kiln's other services; at 0.85 the reserve is 15%.
    #[test]
    fn s6_strain_is_stalls_past_the_reserve() {
        let p = |mem: f64, io: f64| Pressure { memory: Stall { some: None, full: Some(mem) }, io: Stall { some: None, full: Some(io) }, ..Pressure::default() };
        let mut m = calm();
        m.pressure = p(0.05, 0.09);
        assert_eq!(strained(&m, 0.85), None, "14% stalled is within the 15% kept");
        m.pressure = p(0.05, 0.11);
        assert!(strained(&m, 0.85).is_some_and(|w| w.starts_with("the machine's")), "{:?}", strained(&m, 0.85));
        let mut c = calm();
        c.cgroup = Some(Cgroup { path: "/kiln.service".into(), pressure: p(0.0, 0.3), ..Cgroup::default() });
        assert!(strained(&c, 0.85).is_some_and(|w| w.starts_with("the runner's own cgroup")));
        let mut s = calm();
        s.services = vec![super::super::Service { name: "kiln.service".into(), pressure: p(0.2, 0.0) }];
        assert!(strained(&s, 0.85).is_some_and(|w| w.starts_with("kiln.service")));
        assert_eq!(strained(&s, 0.7), None, "a 30% reserve holds 20%");
        assert!(Health::default().check(&Ask::default(), &m, 0.85).is_err_and(|r| r.kind == "strained"));
    }

    /// W05-R26: the kernel's memory stall, where it keeps one, is the machine's; swap-ins priced
    /// at the work device's service time stand in only where it keeps none (adacachy, zram swap:
    /// PSI's 2% read as past 100%, and no session started).
    #[test]
    fn w05_r26_swap_stands_in_only_without_the_kernels_stall() {
        assert_eq!(machine_memory(Some(0.02), Some(0.99)), (0.02, false), "PSI's measure, not the estimate");
        assert_eq!(machine_memory(None, Some(0.3)), (0.3, true));
        assert_eq!(machine_memory(None, None), (0.0, false));
    }

    /// Cross-platform non-PSI strain guard: parallel multi-queue NVMe I/O at sub-millisecond
    /// latency must not falsely cordon macOS/Windows runners (`in_flight > 1.0` gives M/M/1
    /// `rho > 0.50` even at 100 µs latency!), while genuine queueing (>= 2 ms) still cordons,
    /// fresh idle `m.device` immediately overrides stale stand-ins, and macOS `MemoryLevel::Normal`
    /// with ample free RAM ignores transient swap-in bursts.
    #[test]
    fn non_psi_nvme_parallel_io_and_normal_macos_memory_do_not_falsely_cordon() {
        let m = calm();
        // Healthy parallel NVMe build: 3 requests in flight (rho = 0.75 > 0.50), fast 120 µs latency -> NOT strained.
        assert_eq!(strained_with(&m, 0.85, (None, Some(0.75), Some(2.0), Some(120.0)), false), None);
        // Real Mac NVMe thrashing (2026-11-21 re-index: 12.67 in flight -> rho = 0.927, 2194 µs) -> strained for normal check, NOT extreme cordon (local headroom handles 2 ms bursts).
        assert!(strained_with(&m, 0.85, (None, Some(0.927), Some(116.0), Some(2_194.0)), false).is_some());
        assert_eq!(strained_with(&m, 0.50, (None, Some(0.927), Some(116.0), Some(2_194.0)), true), None);
        // Extreme NVMe saturation (rho = 0.93, 15 ms latency) -> triggers extreme cordon.
        assert!(strained_with(&m, 0.50, (None, Some(0.93), Some(150.0), Some(15_000.0)), true).is_some());
        // Real HDD queueing (1.5 in flight -> rho = 0.60, 15 ms latency) -> strained.
        assert!(strained_with(&m, 0.85, (None, Some(0.60), Some(3.0), Some(15_000.0)), false).is_some());
        // Fresh idle `m.device` overrides stale thrashing stand-ins immediately.
        let mut fresh_idle = calm();
        fresh_idle.device = Some(disk::Activity { read_ops_per_s: 20.0, read_latency_us: Some(90.0), in_flight: 0.05, interval_us: 1_000_000, ..disk::Activity::default() });
        assert_eq!(strained_with(&fresh_idle, 0.85, (None, Some(0.95), Some(50.0), Some(5_000.0)), false), None);
        // macOS `MemoryLevel::Normal` with 50% RAM free ignores transient swap stand-in.
        let mut mac_roomy = calm();
        mac_roomy.memory_level = Some(super::super::MemoryLevel::Normal);
        assert_eq!(strained_with(&mac_roomy, 0.85, (Some(0.40), None, None, None), false), None);
        // macOS `MemoryLevel::Normal` with <10% RAM free (like 2026-11-13 swapping Macs) still catches real swap thrashing.
        let mut mac_tight = mac_roomy;
        mac_tight.mem_available = GIB; // 1 GiB / 32 GiB = 3.1% < 10%
        assert!(strained_with(&mac_tight, 0.85, (Some(0.40), None, None, None), false).is_some());
        // Extreme Linux PSI cordon requires > 50% full stalls (not a 20% build burst on a 64-core box).
        let mut linux_mod = calm();
        linux_mod.pressure = Pressure { memory: Stall { some: None, full: Some(0.05) }, io: Stall { some: None, full: Some(0.20) }, ..Pressure::default() };
        assert_eq!(extreme_strained(&linux_mod, 0.90), None);
        let mut linux_extreme = calm();
        linux_extreme.pressure = Pressure { memory: Stall { some: None, full: Some(0.25) }, io: Stall { some: None, full: Some(0.35) }, ..Pressure::default() };
        assert!(extreme_strained(&linux_extreme, 0.90).is_some());
    }

    #[test]
    fn s6_machine_caused_failures_are_told_apart() {
        let e = |exit_code, said_no_space, said_missing: Option<&str>, oom_kills, memory_level| Ending { exit_code, said_no_space, said_missing: said_missing.map(str::to_owned), oom_kills, memory_level };
        assert_eq!(classify(&e(Some(0), true, None, 0, None)), None, "a run that succeeded failed of nothing");
        assert_eq!(classify(&e(Some(1), false, None, 0, None)), None, "its own failure");
        assert_eq!(classify(&e(Some(1), true, None, 0, None)), Some(Failure::NoSpace));
        assert_eq!(classify(&e(None, false, None, 1, None)), Some(Failure::OutOfMemory));
        assert_eq!(classify(&e(None, false, None, 0, Some(super::super::MemoryLevel::Critical))), Some(Failure::OutOfMemory), "macOS jetsam (E9)");
        assert_eq!(classify(&e(None, false, None, 0, Some(super::super::MemoryLevel::Normal))), None, "a kill with memory to spare isn't memory's");
        assert_eq!(classify(&e(Some(0xC000_0017_u32 as i32), false, None, 0, None)), Some(Failure::OutOfMemory), "Windows (E9)");
        assert_eq!(classify(&e(Some(127), false, Some("cargo"), 0, None)), Some(Failure::MissingTool { tool: Some("cargo".into()) }));
        assert_eq!(classify(&e(Some(9009), false, None, 0, None)), Some(Failure::MissingTool { tool: None }));
        for (line, tool) in [
            ("bash: cargo: command not found", Some("cargo")),
            ("bash: line 1: nvcc: command not found", Some("nvcc")),
            ("sh: 1: go: not found", Some("go")),
            ("zsh: command not found: node", Some("node")),
            ("'dotnet' is not recognized as an internal or external command,", Some("dotnet")),
            ("error: the file was not found", None),
        ] {
            assert_eq!(missing_tool(line).as_deref(), tool, "{line}");
        }
        let mut s = Scan::default();
        s.feed(b"compiling\nerror: failed to write: No space left");
        s.feed(b" on device (os error 28)\nbash: line 3: cargo: command not fo");
        s.feed(b"und");
        s.end();
        assert!(s.no_space && s.missing.as_deref() == Some("cargo"), "{s:?}");
    }

    /// S6: a cause bars work of its kind until an event shows it cleared; none clears by time.
    #[test]
    fn s6_a_cause_bars_its_kind_until_an_event_clears_it() {
        let mut h = Health::default();
        let m = calm();
        let key = |k: &str| Ask { key: Some(k.into()), ..Ask::default() };
        // No space: the key, and any ask for disk, until more is free than then, by what it wrote.
        let mut full = calm();
        full.space = Some(Space { total: 100 * GIB, free: 0, important: None });
        h.ended("leg:a", Some(&Failure::NoSpace), &full, 2 * GIB, 0);
        assert!(h.check(&key("leg:a"), &full, 0.85).is_err_and(|r| r.kind == "disk"));
        assert!(h.check(&Ask { disk: GIB, ..Ask::default() }, &full, 0.85).is_err());
        assert_eq!(h.check(&key("leg:b"), &full, 0.85), Ok(()), "work that asks no disk still goes");
        let mut freed_a_little = calm();
        freed_a_little.space = Some(Space { total: 100 * GIB, free: GIB, important: None });
        h.measured(&freed_a_little);
        assert!(h.check(&key("leg:a"), &freed_a_little, 0.85).is_err(), "1 GiB freed, the run wrote 2");
        h.measured(&m);
        assert_eq!(h.check(&key("leg:a"), &m, 0.85), Ok(()));
        // More disk asked than is free: refused on its own.
        assert!(h.check(&Ask { disk: 60 * GIB, ..Ask::default() }, &m, 0.85).is_err_and(|r| r.kind == "disk"));
        // Killed for memory: until more is free than its largest process held.
        let mut tight = calm();
        tight.mem_available = GIB;
        h.ended("cmd:x", Some(&Failure::OutOfMemory), &tight, 0, 20 * GIB);
        assert!(h.check(&key("cmd:x"), &m, 0.85).is_err_and(|r| r.kind == "memory"), "16 GiB free of the 20 it held");
        assert!(h.check(&Ask { mem: 17 * GIB, ..Ask::default() }, &m, 0.85).is_err());
        let mut roomy = calm();
        roomy.mem_available = 24 * GIB;
        h.measured(&roomy);
        assert_eq!(h.check(&key("cmd:x"), &roomy, 0.85), Ok(()));
        // A tool not found: work needing it, and the key, until facts are probed with it there.
        h.ended("leg:c", Some(&Failure::MissingTool { tool: Some("nvcc".into()) }), &m, 0, 0);
        assert!(h.check(&Ask { tools: vec!["nvcc".into()], ..Ask::default() }, &m, 0.85).is_err_and(|r| r.kind == "tool"));
        h.measured(&m);
        assert!(h.check(&key("leg:c"), &m, 0.85).is_err(), "a measure doesn't bring a tool back");
        h.probed(&|t| t == "cargo");
        assert!(h.check(&key("leg:c"), &m, 0.85).is_err());
        h.probed(&|t| t == "nvcc");
        assert_eq!(h.check(&key("leg:c"), &m, 0.85), Ok(()));
        // A tool its shell didn't name: the probe its own failure asked for (W13-R01: asked as the
        // failure comes) doesn't clear it; the next look does.
        h.ended("leg:e", Some(&Failure::MissingTool { tool: None }), &m, 0, 0);
        h.probed_by(&|_| false, false);
        assert!(h.check(&key("leg:e"), &m, 0.85).is_err(), "the failure's own probe cleared its unnamed tool");
        h.probed_by(&|_| false, true);
        assert_eq!(h.check(&key("leg:e"), &m, 0.85), Ok(()), "a look clears it");
        // The device's errors rising: until a measure shows I/O done with none.
        let mut erring = calm();
        erring.device = Some(disk::Activity { read_ops_per_s: 10.0, errors: disk::Errors { errors: Some(3), ..disk::Errors::default() }, ..disk::Activity::default() });
        h.measured(&erring);
        assert!(h.check(&key("leg:d"), &erring, 0.85).is_err_and(|r| r.kind == "io_errors"));
        let mut idle = calm();
        idle.device = Some(disk::Activity { errors: disk::Errors { errors: Some(0), ..disk::Errors::default() }, ..disk::Activity::default() });
        h.measured(&idle);
        assert!(h.check(&key("leg:d"), &idle, 0.85).is_err(), "no I/O: nothing shown");
        let mut quiet = idle.clone();
        quiet.device = Some(disk::Activity { write_ops_per_s: 5.0, errors: disk::Errors { errors: Some(0), ..disk::Errors::default() }, ..disk::Activity::default() });
        h.measured(&quiet);
        assert_eq!(h.check(&key("leg:d"), &quiet, 0.85), Ok(()));
    }
}
