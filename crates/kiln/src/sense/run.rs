//! What each finished run did on this runner (docs/needs.md): the observations the learned
//! run-time model ([`kiln_ci::runtime`]) is fitted from. A command (`kiln run`, MCP `run`) or a
//! CI leg is recorded from its start to its end, nothing sampled between:
//!
//! - wall time, by the monotonic clock;
//! - CPU time: its cgroup's `cpu.stat` (Linux, everything it started however parented), its job
//!   object's accounting (Windows), else its process tree's as `wait4` returns it (the child and
//!   every descendant it waited for);
//! - I/O: its cgroup's `io.stat` where the io controller is delegated to it, its job object's I/O
//!   counters (Windows), else `wait4`'s blocks (Linux counts them in 512-byte units, of actual
//!   storage I/O; macOS counts operations);
//! - the share of its time its tasks were stalled on I/O: its cgroup's `io.pressure`;
//! - the work device's activity over it: [`super::disk::Activity`] between the runner's sensor's
//!   reads at its start and end, and the most the device was seen to move before it.
//!
//! The runner's own sensor is read at each run's start and end (an event, never a timer), so its
//! idle CPUs at a run's start cover the interval since the previous run event.
//!
//! Each observation goes to the server in the runner's facts (`RunnerFacts.runs`) as its run
//! ends; the uplink keeps it while the runner is disconnected. The server keeps it once (by its
//! id) and folds it into the key's statistics.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use kiln_ci::runtime::Observation;

use super::disk::Counters;
use super::{PsiLine, Sensor};

/// What a run's processes used, as its OS accounts for them, cumulative from their start.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    pub cpu_us: Option<u64>,
    pub read_bytes: Option<u64>,
    pub write_bytes: Option<u64>,
    /// I/O operations (reads and writes; macOS's `wait4` gives only these).
    pub ops: Option<u64>,
    /// Its cgroup's `io.pressure` lines.
    pub stall_some: Option<PsiLine>,
    pub stall_full: Option<PsiLine>,
    /// The memory its largest process held at its peak (`wait4`'s `ru_maxrss`), bytes.
    pub peak_rss: Option<u64>,
    /// OOM kills in its cgroup (`memory.events` `oom_kill`, where the memory controller is).
    pub oom_kills: Option<u64>,
}

impl Usage {
    /// `self`, with what it lacks from `other` (a cgroup's figures first, `wait4`'s after).
    pub fn or(self, other: Usage) -> Usage {
        Usage {
            cpu_us: self.cpu_us.or(other.cpu_us),
            read_bytes: self.read_bytes.or(other.read_bytes),
            write_bytes: self.write_bytes.or(other.write_bytes),
            ops: self.ops.or(other.ops),
            stall_some: self.stall_some.or(other.stall_some),
            stall_full: self.stall_full.or(other.stall_full),
            peak_rss: self.peak_rss.or(other.peak_rss),
            oom_kills: self.oom_kills.or(other.oom_kills),
        }
    }
}

impl Usage {
    /// Two counts added (a leg's steps, each a job object of its own).
    pub fn plus(self, o: Usage) -> Usage {
        let add = |a: Option<u64>, b: Option<u64>| match (a, b) {
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
            (a, b) => a.or(b),
        };
        Usage {
            cpu_us: add(self.cpu_us, o.cpu_us),
            read_bytes: add(self.read_bytes, o.read_bytes),
            write_bytes: add(self.write_bytes, o.write_bytes),
            ops: add(self.ops, o.ops),
            stall_some: self.stall_some.or(o.stall_some),
            stall_full: self.stall_full.or(o.stall_full),
            peak_rss: match (self.peak_rss, o.peak_rss) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            },
            oom_kills: add(self.oom_kills, o.oom_kills),
        }
    }
}

/// Parsing of cgroup v2's `io.stat` (`maj:min rbytes=… wbytes=… rios=… wios=… dbytes=… dios=…`
/// per device): read bytes, written bytes, and operations, over every device.
pub fn io_stat(text: &str) -> Option<(u64, u64, u64)> {
    let mut any = false;
    let (mut r, mut w, mut ops) = (0u64, 0u64, 0u64);
    for line in text.lines() {
        for kv in line.split_whitespace().skip(1) {
            let Some((k, v)) = kv.split_once('=') else { continue };
            let Ok(v) = v.parse::<u64>() else { continue };
            any = true;
            match k {
                "rbytes" => r = r.saturating_add(v),
                "wbytes" => w = w.saturating_add(v),
                "rios" | "wios" => ops = ops.saturating_add(v),
                _ => {}
            }
        }
    }
    any.then_some((r, w, ops))
}

/// A sensor per file system runs work on, shared by every run on it and read at their starts
/// and ends.
struct Shared {
    sensors: BTreeMap<String, Sensor>,
    /// The last read's measure, of whichever file system.
    last: Option<super::Measure>,
}

fn shared() -> &'static Mutex<Shared> {
    static S: OnceLock<Mutex<Shared>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Shared { sensors: BTreeMap::new(), last: None }))
}

/// The file system `dir` is on, as a key: its device number on Unix, its volume elsewhere.
fn volume_key(dir: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if let Ok(m) = std::fs::metadata(dir) {
            return format!("dev:{}", m.dev());
        }
    }
    dir.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned()).unwrap_or_default()
}

/// One read of the shared sensor of `dir`'s file system: its idle CPUs over the interval since
/// its previous read, its device's counters now, and the most the device was seen to move.
fn read_shared(dir: &Path) -> (super::Measure, Option<Counters>) {
    let key = volume_key(dir);
    let Ok(mut s) = shared().lock() else { return (super::Measure::default(), None) };
    let sensor = s.sensors.entry(key).or_insert_with(|| Sensor::new(dir));
    let m = sensor.measure();
    let device = sensor.last_device();
    s.last = Some(m.clone());
    // What a run event's read shows cleared of the runner's health goes now (S6).
    if let Ok(mut h) = super::health::runner().lock() {
        h.measured(&m);
    }
    (m, device)
}

/// OOM kills in the runner's own cgroup so far (Linux; none elsewhere).
fn own_oom_kills() -> u64 {
    crate::os::cgroup::memory().map_or(0, |m| m.oom_kills)
}

/// The machine's pressure over the interval the last run event's read covered, as facts say it
/// (percent): what placement weighs a runner by until it next says (docs/needs.md).
fn pressure_now() -> Option<kiln_ci::facts::WorkFacts> {
    let s = shared().lock().ok()?;
    let m = s.last.as_ref()?;
    let pct = |v: Option<f64>| v.map(|v| v * 100.0);
    Some(kiln_ci::facts::WorkFacts { io_full: pct(m.pressure.io.full), mem_full: pct(m.pressure.memory.full), memory_level: m.memory_level.map(|l| l.as_str().to_owned()) })
}

/// A run being recorded.
pub struct Recorder {
    key: String,
    kind: &'static str,
    label: Option<String>,
    dir: PathBuf,
    started: Instant,
    started_ms: i64,
    idle_cpus: Option<f64>,
    device: Option<Counters>,
    peak_bps: Option<f64>,
    usage_at_start: Usage,
    own_oom_kills: u64,
    p: Option<f64>,
    /// The key of the same work on every machine (W05-R26, [`work_key`]).
    work_key: Option<String>,
    /// The machine's contention and clocks as the run started ([`super::pace::at_start`]).
    signals: Option<kiln_ci::pace::slow::Signals>,
}

impl Recorder {
    /// A run of `key` starting now in `dir`, whose processes' usage is `usage` so far (a fresh
    /// cgroup's is nothing; a reused one's is what came before). `p`: the probability its
    /// placement chose this runner with, where the placer said.
    pub fn start(key: String, kind: &'static str, label: Option<String>, dir: &Path, usage: Usage, p: Option<f64>) -> Recorder {
        let (m, device) = read_shared(dir);
        Recorder { key, kind, label, dir: dir.to_path_buf(), started: Instant::now(), started_ms: crate::util::now_ms(), idle_cpus: m.idle_cpus(), device, peak_bps: m.device_peak_bps, usage_at_start: usage, own_oom_kills: own_oom_kills(), p, work_key: None, signals: super::pace::at_start() }
    }

    /// The key its work is known by on every machine (W05-R26, [`work_key`]).
    #[must_use]
    pub fn work(mut self, key: String) -> Recorder {
        self.work_key = Some(key);
        self
    }

    /// The run ended with `exit_code`, its processes having used `usage` all told, its output
    /// looked over by `scan` where it was: its observation, with what of the machine's ended it
    /// (S6), which the runner's health takes in at once.
    pub fn finish(self, exit_code: Option<i32>, usage: Usage, scan: Option<&super::health::Scan>) -> Observation {
        let wall = self.started.elapsed();
        let wall_us = u64::try_from(wall.as_micros()).unwrap_or(u64::MAX);
        let (end, device_end) = read_shared(&self.dir);
        let ending = super::health::Ending {
            exit_code,
            said_no_space: scan.is_some_and(|s| s.no_space),
            said_missing: scan.and_then(|s| s.missing.clone()),
            oom_kills: own_oom_kills().saturating_sub(self.own_oom_kills).saturating_add(usage.oom_kills.unwrap_or(0).saturating_sub(self.usage_at_start.oom_kills.unwrap_or(0))),
            memory_level: end.memory_level,
        };
        let failure = super::health::classify(&ending);
        // A tool not found: what the facts say of their tools may be wrong now (W13-R01).
        if matches!(failure, Some(super::health::Failure::MissingTool { .. })) {
            crate::runner::probe::tool_missing();
        }
        let wrote = usage.write_bytes.unwrap_or(0).saturating_sub(self.usage_at_start.write_bytes.unwrap_or(0));
        if let Ok(mut h) = super::health::runner().lock() {
            h.ended(&self.key, failure.as_ref(), &end, wrote, usage.peak_rss.unwrap_or(0));
        }
        let activity = self.device.as_ref().zip(device_end.as_ref()).and_then(|(a, b)| super::disk::Activity::between(a, b, wall_us));
        let s = self.usage_at_start;
        let d = |end: Option<u64>, start: Option<u64>| end.map(|e| e.saturating_sub(start.unwrap_or(0)));
        let stall = |end: Option<PsiLine>, start: Option<PsiLine>| end.map(|e| e.total_us.saturating_sub(start.map_or(0, |s| s.total_us))).filter(|_| wall_us > 0).map(|us| (us as f64 / wall_us as f64).clamp(0.0, 1.0));
        let latency = activity.and_then(|a| {
            let (rops, wops) = (a.read_ops_per_s, a.write_ops_per_s);
            let ops = rops + wops;
            (ops > 0.0).then(|| (a.read_latency_us.unwrap_or(0.0) * rops + a.write_latency_us.unwrap_or(0.0) * wops) / ops)
        });
        Observation {
            id: uuid::Uuid::now_v7().simple().to_string(),
            key: self.key,
            kind: self.kind.to_owned(),
            label: self.label,
            started_ms: self.started_ms,
            wall_s: wall.as_secs_f64(),
            exit_code,
            cpu_s: d(usage.cpu_us, s.cpu_us).map(|us| us as f64 / 1e6),
            idle_cpus: self.idle_cpus,
            read_bytes: d(usage.read_bytes, s.read_bytes),
            write_bytes: d(usage.write_bytes, s.write_bytes),
            io_ops: d(usage.ops, s.ops),
            io_stall_some: stall(usage.stall_some, s.stall_some),
            io_stall_full: stall(usage.stall_full, s.stall_full),
            device_latency_us: latency,
            device_bps: activity.map(|a| a.read_bytes_per_s + a.write_bytes_per_s),
            device_peak_bps: self.peak_bps,
            p: self.p,
            peak_rss: usage.peak_rss,
            cause: failure.as_ref().map(super::health::Failure::as_str),
            work_key: self.work_key,
            signals: self.signals,
        }
    }
}

/// A CI leg being recorded: its observation is made and sent when this is dropped, as the leg's
/// executor returns (before the leg's cgroup goes: declare it after `freeze::begin`'s guard).
pub struct Leg {
    job: String,
    recorder: Option<Recorder>,
}

impl Leg {
    /// Leg `job` of `key` starts now in `dir` (its workspace's file system).
    pub fn start(job: &str, key: String, label: Option<String>, dir: &Path) -> Leg {
        let so_far = crate::runner::freeze::usage(job);
        Leg { job: job.to_owned(), recorder: Some(Recorder::start(key, "leg", label, dir, so_far, None)) }
    }
}

impl Drop for Leg {
    fn drop(&mut self) {
        if let Some(r) = self.recorder.take() {
            report(r.finish(None, crate::runner::freeze::usage(&self.job), None));
        }
    }
}

/// A leg's key: its repository, workflow, job and matrix values (a matrix's legs do different
/// work).
pub fn leg_key(repo: &str, workflow: &str, job: &str, matrix: &str) -> String {
    let digest = crate::util::sha256_hex(format!("{repo}\0{workflow}\0{job}\0{matrix}").as_bytes());
    format!("leg:{}", digest.get(..32).unwrap_or(&digest))
}

/// The key of a command run in `cwd`: its text's digest, with the folder's (a repository's
/// `cargo test` isn't another's).
pub fn command_key(command: &str, cwd: &Path) -> String {
    let digest = crate::util::sha256_hex(format!("{}\0{}", cwd.display(), command.trim()).as_bytes());
    format!("cmd:{}", digest.get(..32).unwrap_or(&digest))
}

/// The key of a command's work on every machine (W05-R26): what the fleet's per-core speeds
/// compare across runners (`kiln_ci::pace::speed::key_of`). [`command_key`] has the folder as this
/// machine names it, which differs between machines; this has it as every machine names it: the
/// synced copy's key and the folder inside it (`mirror`, `cwd` relative to it) when the run is in a
/// synced copy, else the folder below the home folder (`~/...`), else the folder itself.
pub fn work_key(mirror: Option<&str>, cwd: Option<&str>, command: &str, here: &Path) -> String {
    let place = match mirror {
        Some(m) => format!("mirror\0{m}\0{}", cwd.map(str::trim).unwrap_or_default()),
        None => {
            let home = crate::os::home_dir();
            match here.strip_prefix(&home) {
                Ok(rest) => format!("home\0~/{}", rest.display()),
                Err(_) => format!("dir\0{}", here.display()),
            }
        }
    };
    let digest = crate::util::sha256_hex(format!("{place}\0{}", command.trim()).as_bytes());
    format!("work:{}", digest.get(..32).unwrap_or(&digest))
}

/// A command's first line, for a person.
pub fn command_label(command: &str) -> String {
    command.trim().lines().next().unwrap_or_default().trim().to_owned()
}

/// Where finished runs' observations go: the runner's uplink, set when it starts.
fn uplink() -> &'static Mutex<Option<crate::exec::Uplink>> {
    static U: OnceLock<Mutex<Option<crate::exec::Uplink>>> = OnceLock::new();
    U.get_or_init(|| Mutex::new(None))
}

/// Observations go to the server through `up` from now on (a runner's start).
pub fn install(up: crate::exec::Uplink) {
    if let Ok(mut u) = uplink().lock() {
        *u = Some(up);
    }
}

/// A run ended: its observation goes to the server now, in a facts message of its own (one
/// that says no probe's facts, so the server keeps the runner's as they were), with the machine's
/// pressure as it read at the run's end, which the server keeps with its facts: said on change,
/// never on a timer.
pub fn report(o: Observation) {
    // What it says of this machine's slowdown under contention is learned here (W05-R26).
    super::pace::ended(&o);
    let up = uplink().lock().ok().and_then(|u| u.clone());
    let Some(up) = up else { return };
    let facts = kiln_ci::facts::RunnerFacts { os: std::env::consts::OS.to_owned(), arch: std::env::consts::ARCH.to_owned(), runs: vec![o], work: pressure_now(), ..kiln_ci::facts::RunnerFacts::default() };
    let Ok(json) = serde_json::to_string(&facts) else { return };
    crate::rt::spawn(async move { up.msg(crate::proto::AgentMsg::Facts { json }).await });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w05_io_stat_sums_its_devices() {
        let t = "259:0 rbytes=1000 wbytes=2000 rios=3 wios=4 dbytes=0 dios=0\n8:0 rbytes=10 wbytes=20 rios=1 wios=1 dbytes=0 dios=0\n";
        assert_eq!(io_stat(t), Some((1010, 2020, 9)));
        assert_eq!(io_stat(""), None);
    }

    /// docs/needs.md: a run's observation is its usage from start to end, its wall time, and the
    /// runner as it was when it started.
    #[test]
    fn w05_a_run_is_observed_from_start_to_end() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let at_start = Usage { cpu_us: Some(1_000_000), read_bytes: Some(10), ops: Some(1), stall_some: Some(PsiLine { avg10: 0.0, total_us: 0 }), ..Usage::default() };
        let r = Recorder::start(command_key("cargo test", dir.path()), "command", Some(command_label("cargo test --workspace")), dir.path(), at_start, Some(0.5));
        std::thread::sleep(std::time::Duration::from_millis(20));
        let end = Usage { cpu_us: Some(3_500_000), read_bytes: Some(4_106), write_bytes: Some(8_192), ops: Some(5), stall_some: Some(PsiLine { avg10: 0.0, total_us: 5_000 }), peak_rss: Some(1 << 20), ..Usage::default() };
        let o = r.finish(Some(0), end, None);
        assert!(o.key.starts_with("cmd:") && o.kind == "command" && o.label.as_deref() == Some("cargo test --workspace"), "{o:?}");
        assert!(o.wall_s >= 0.02, "{o:?}");
        assert_eq!((o.cpu_s, o.read_bytes, o.write_bytes, o.io_ops, o.p, o.exit_code), (Some(2.5), Some(4_096), Some(8_192), Some(4), Some(0.5), Some(0)));
        assert!(o.io_stall_some.is_some_and(|s| s > 0.0 && s <= 0.25), "{o:?}");
        // A sensor made for this run's volume has read once just before: over so short an
        // interval the CPU counters may not have moved (Windows' tick is 15.6 ms), and it says
        // none rather than a guess.
        assert!(o.idle_cpus.is_none_or(|c| c >= 0.0), "{o:?}");
        assert_ne!(command_key("cargo test", dir.path()), command_key("cargo test", Path::new("/elsewhere")));
        assert_eq!((o.peak_rss, o.cause.as_deref()), (Some(1 << 20), None));
        // S6: one the machine ended says so, and bars its key here until that clears.
        let r = Recorder::start(command_key("w05-missing", dir.path()), "command", None, dir.path(), Usage::default(), None);
        let mut scan = crate::sense::health::Scan::default();
        scan.feed(b"bash: line 1: w05-no-such-tool: command not found\n");
        let o = r.finish(Some(127), Usage::default(), Some(&scan));
        assert_eq!(o.cause.as_deref(), Some("missing_tool:w05-no-such-tool"));
        let ask = crate::sense::health::Ask { tools: vec!["w05-no-such-tool".into()], ..Default::default() };
        let m = crate::sense::Measure::default();
        assert!(crate::sense::health::runner().lock().is_ok_and(|h| h.check(&ask, &m, 1.0).is_err()));
        Ok(())
    }

    /// AUDIT O4: a run's CPU time counts what its exited children used: `wait4`'s rusage of the
    /// child takes in every descendant it waited for, and a cgroup's `cpu.stat` everything that
    /// ever ran in it.
    #[cfg(unix)]
    #[test]
    fn o4_a_runs_cpu_counts_its_exited_children() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let mut c = std::process::Command::new("sh");
            // A child that spins and has ended long before its parent does.
            c.args(["-c", "( i=0; while [ $i -lt 300000 ]; do i=$((i+1)); done ); sleep 0.2"]);
            let d = crate::proc::spawn_duplex(c)?;
            let crate::os::proc::Duplex { exit, usage, stdout, stderr, .. } = d;
            drop((stdout, stderr));
            let _ = exit.await;
            let u = match usage {
                Some(u) => u.await.unwrap_or_default(),
                None => Usage::default(),
            };
            anyhow::ensure!(u.cpu_us.is_some_and(|us| us > 10_000), "{u:?}");
            anyhow::ensure!(u.peak_rss.is_some_and(|b| b > 0), "{u:?}");
            Ok(())
        })
    }
}
