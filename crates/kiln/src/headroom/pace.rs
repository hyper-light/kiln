//! The work headroom can slow, how hard each runs, and the actuator: a group paced at `d`% runs
//! the first `d`% of every period and is stopped for the rest, so slowing is continuous rather than
//! on or off. Everything here is the runner's own, so none of it needs privileges.
//!
//! Linux: each agent session's cgroup (`crate::os::contain`), and, unless the settings say not to,
//! every transient unit the user's service manager runs (`systemd-run --user`: a build an agent
//! started to outlive its command), stopped and started with `cgroup.freeze`. macOS and Windows:
//! each agent session's processes as `crate::os::contain` holds them, metered process by process
//! and stopped process by process (SIGSTOP and SIGCONT; every thread suspended and resumed).
//!
//! Every group paced is noted in a file, so a runner that dies with work stopped starts it again
//! when it starts (and so does the next one, should this one never come back).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// A group of processes headroom can see and slow.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Group {
    /// Where it is (a cgroup's folder).
    pub dir: PathBuf,
    /// What it is: an agent session's id, or a unit's name.
    pub label: String,
    /// The agent session it is, when it's one.
    pub session: Option<String>,
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

/// The actuator: groups and their shares, applied on a thread of its own every 50 ms.
pub struct Pacer {
    shares: Arc<Mutex<HashMap<PathBuf, u8>>>,
    period: Arc<Mutex<Duration>>,
    /// The groups stopped now, each with what starts it again (`imp::release`).
    held: Arc<Mutex<HashMap<PathBuf, Vec<String>>>>,
    stop: Arc<AtomicBool>,
    record: PathBuf,
}

impl Pacer {
    /// Starts groups a previous runner left stopped (as its record says), then the thread.
    pub fn start(dir: &Path) -> Self {
        let record = dir.join("paced.json");
        release_recorded(&record);
        let shares: Arc<Mutex<HashMap<PathBuf, u8>>> = Arc::default();
        let held: Arc<Mutex<HashMap<PathBuf, Vec<String>>>> = Arc::default();
        let period = Arc::new(Mutex::new(Duration::from_millis(1000)));
        let stop = Arc::new(AtomicBool::new(false));
        let (sh, pe, he, st, rec) = (Arc::clone(&shares), Arc::clone(&period), Arc::clone(&held), Arc::clone(&stop), record.clone());
        let spawned = std::thread::Builder::new().name("kiln-pacer".into()).spawn(move || {
            let base = Instant::now();
            while !st.load(Ordering::Relaxed) {
                let p = pe.lock().as_millis().max(1) as u64;
                let now = base.elapsed().as_millis() as u64;
                let want: Vec<(PathBuf, u8)> = sh.lock().iter().map(|(d, s)| (d.clone(), *s)).collect();
                let mut held = he.lock();
                let mut changed = false;
                for (dir, share) in &want {
                    // Each group's period starts at its own offset, so they don't all start at once.
                    let offset = dir.to_string_lossy().bytes().fold(0u64, |a, b| a.wrapping_mul(31).wrapping_add(u64::from(b))) % p;
                    let run = (now + offset) % p < p * u64::from(*share) / 100;
                    if run {
                        if let Some(keys) = held.remove(dir) {
                            keys.iter().for_each(|k| imp::release(k));
                            changed = true;
                        }
                    } else if !held.contains_key(dir)
                        && let Ok(keys) = imp::stop(dir)
                    {
                        held.insert(dir.clone(), keys);
                        changed = true;
                    }
                }
                // Groups no longer paced run again.
                held.retain(|dir, keys| {
                    let keep = want.iter().any(|(d, _)| d == dir);
                    if !keep {
                        keys.iter().for_each(|k| imp::release(k));
                        changed = true;
                    }
                    keep
                });
                if changed {
                    note(&rec, &held);
                }
                drop(held);
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "headroom: the pacer couldn't start; work past the window still waits to start");
        }
        Self { shares, period, held, stop, record }
    }

    /// Each paced group's share from now (percent; groups at 100 aren't listed).
    pub fn set(&self, paced: HashMap<PathBuf, u8>, period: Duration) {
        *self.period.lock() = period;
        *self.shares.lock() = paced;
    }
}

/// Notes what starts every group stopped now, for a runner that starts after this one dies.
fn note(record: &Path, held: &HashMap<PathBuf, Vec<String>>) {
    let keys: Vec<&String> = held.values().flatten().collect();
    if let Err(e) = crate::util::write_secret_unsynced(record, &serde_json::to_vec(&keys).unwrap_or_default(), 0o644) {
        tracing::warn!(error = %e, "headroom: couldn't note what is paced");
    }
}

impl Drop for Pacer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for keys in std::mem::take(&mut *self.held.lock()).into_values() {
            keys.iter().for_each(|k| imp::release(k));
        }
        let _ = std::fs::remove_file(&self.record);
    }
}

/// Starts everything a record says was stopped.
fn release_recorded(record: &Path) {
    let Ok(raw) = std::fs::read(record) else { return };
    let keys: Vec<String> = serde_json::from_slice(&raw).unwrap_or_default();
    keys.iter().for_each(|k| imp::release(k));
    if !keys.is_empty() {
        tracing::info!(stopped = keys.len(), "headroom: work a previous runner left paced runs again");
    }
    let _ = std::fs::remove_file(record);
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

    fn own() -> Option<String> {
        fs::read_to_string("/proc/self/cgroup").ok()?.lines().find_map(|l| l.strip_prefix("0::")).map(|p| p.trim().to_owned())
    }

    pub fn groups(escapes: bool) -> Vec<Group> {
        let mut out = Vec::new();
        if let Some(dir) = crate::os::contain::agents_dir()
            && let Ok(rd) = fs::read_dir(&dir)
        {
            for e in rd.flatten() {
                if e.path().is_dir() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    out.push(Group { dir: e.path(), label: name.clone(), session: Some(name) });
                }
            }
        }
        if escapes && let Some(own) = own() {
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
                            } else if (name.ends_with(".service") || name.ends_with(".scope")) && !desktop(&name) && transient.join(&name).exists() && !mine.starts_with(&p) {
                                out.push(Group { dir: p, label: name, session: None });
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

    /// Stops the whole cgroup at once; it starts again by its folder.
    pub fn stop(dir: &Path) -> std::io::Result<Vec<String>> {
        freeze(dir, true).map(|()| vec![dir.display().to_string()])
    }

    pub fn release(key: &str) {
        let _ = freeze(Path::new(key), false);
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

    /// A session's group's `dir`: no folder here, the session it is.
    const SESSION: &str = "session:";

    /// Pacing by stopping processes: macOS by signal, Windows thread by thread.
    pub fn available() -> bool {
        cfg!(any(target_os = "macos", windows))
    }

    /// A group for each agent session with processes held (`crate::os::contain`): a job object's
    /// on Windows, those followed fork by fork on macOS. A unit outside them (a build an agent
    /// started to outlive its command) isn't one: nothing here holds it.
    pub fn groups(_escapes: bool) -> Vec<Group> {
        crate::os::contain::sessions().into_iter().map(|s| Group { dir: PathBuf::from(format!("{SESSION}{s}")), label: s.clone(), session: Some(s) }).collect()
    }

    /// The session's processes' counters, summed.
    pub fn counters(dir: &Path) -> (u64, u64, u64) {
        super::processes(&pids(dir))
    }

    fn pids(dir: &Path) -> Vec<u32> {
        let Some(session) = dir.to_str().and_then(|d| d.strip_prefix(SESSION)) else { return Vec::new() };
        crate::os::contain::Group::existing(session).map(|g| g.pids()).unwrap_or_default()
    }

    /// Stops each of the session's processes, each starting again by its id: one started
    /// meanwhile runs until the next stop.
    pub fn stop(dir: &Path) -> std::io::Result<Vec<String>> {
        stop_pids(&pids(dir))
    }

    /// Stops processes `pids`: SIGSTOP on macOS, every thread suspended on Windows.
    #[cfg(target_os = "macos")]
    pub fn stop_pids(pids: &[u32]) -> std::io::Result<Vec<String>> {
        use nix::sys::signal::{Signal, kill};
        Ok(pids
            .iter()
            .copied()
            .filter(|p| i32::try_from(*p).is_ok_and(|p| kill(nix::unistd::Pid::from_raw(p), Signal::SIGSTOP).is_ok()))
            .map(|p| format!("pid:{p}"))
            .collect())
    }

    #[cfg(windows)]
    pub fn stop_pids(pids: &[u32]) -> std::io::Result<Vec<String>> {
        Ok(crate::os::contain::suspend_all(pids).into_iter().map(|p| format!("pid:{p}")).collect())
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    pub fn stop_pids(_pids: &[u32]) -> std::io::Result<Vec<String>> {
        Err(std::io::Error::other("no pacing on this system"))
    }

    pub fn release(key: &str) {
        let Some(pid) = key.strip_prefix("pid:").and_then(|p| p.parse::<u32>().ok()) else { return };
        #[cfg(target_os = "macos")]
        if let Ok(pid) = i32::try_from(pid) {
            let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGCONT);
        }
        #[cfg(windows)]
        crate::os::contain::resume_all(&[pid]);
        #[cfg(not(any(target_os = "macos", windows)))]
        let _ = pid;
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
            let child = std::process::Command::new(&exe)
                .args([me.as_str(), "--exact", "--nocapture", "--test-threads=1"])
                .env(WRITER, dir.path().join("w"))
                .stdout(std::process::Stdio::null())
                .spawn()?;
            std::thread::sleep(Duration::from_millis(300));
            Ok(Self { child, _dir: dir })
        }

        fn pid(&self) -> [u32; 1] {
            [self.child.id()]
        }

        /// What it did over `d`: CPU µs and bytes written.
        fn over(&self, d: Duration) -> (u64, u64) {
            let (c0, w0, _) = super::processes(&self.pid());
            std::thread::sleep(d);
            let (c1, w1, _) = super::processes(&self.pid());
            (c1.saturating_sub(c0), w1.saturating_sub(w0))
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
        let (cpu, written) = w.over(Duration::from_millis(700));
        anyhow::ensure!(cpu > 0, "no CPU time");
        anyhow::ensure!(written > 0, "nothing written");
        anyhow::ensure!(mem > 0, "no memory");
        Ok(())
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn a_stopped_process_does_nothing_and_works_again_once_released() -> anyhow::Result<()> {
        write_if_asked()?;
        let w = Writer::start("a_stopped_process_does_nothing_and_works_again_once_released")?;
        let keys = super::imp::stop_pids(&w.pid())?;
        anyhow::ensure!(keys.len() == 1, "{keys:?}");
        // What was under way when it stopped lands first.
        std::thread::sleep(Duration::from_millis(200));
        let (cpu, written) = w.over(Duration::from_millis(700));
        anyhow::ensure!(cpu == 0 && written == 0, "stopped, yet {cpu} µs of CPU and {written} bytes written");
        keys.iter().for_each(|k| super::imp::release(k));
        let (cpu, written) = w.over(Duration::from_millis(700));
        anyhow::ensure!(cpu > 0 && written > 0, "released, yet {cpu} µs of CPU and {written} bytes written");
        Ok(())
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

    #[test]
    fn a_record_left_behind_starts_its_groups_again_and_goes() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        // A "group" that's only a folder: thawing writes its cgroup.freeze, as on a real cgroup.
        let g = dir.path().join("g");
        std::fs::create_dir(&g)?;
        std::fs::write(g.join("cgroup.freeze"), "1")?;
        std::fs::write(dir.path().join("paced.json"), serde_json::to_vec(&[g.display().to_string()])?)?;
        let p = Pacer::start(dir.path());
        anyhow::ensure!(std::fs::read_to_string(g.join("cgroup.freeze"))? == "0");
        anyhow::ensure!(!dir.path().join("paced.json").exists());
        drop(p);
        Ok(())
    }

    #[test]
    fn a_paced_group_is_stopped_part_of_each_period_and_runs_once_let_go() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let g = dir.path().join("g");
        std::fs::create_dir(&g)?;
        std::fs::write(g.join("cgroup.freeze"), "0")?;
        let p = Pacer::start(dir.path());
        p.set(HashMap::from([(g.clone(), 0)]), Duration::from_millis(200));
        std::thread::sleep(Duration::from_millis(300));
        anyhow::ensure!(std::fs::read_to_string(g.join("cgroup.freeze"))? == "1", "0% is stopped");
        anyhow::ensure!(dir.path().join("paced.json").exists(), "what is paced is noted");
        p.set(HashMap::new(), Duration::from_millis(200));
        std::thread::sleep(Duration::from_millis(300));
        anyhow::ensure!(std::fs::read_to_string(g.join("cgroup.freeze"))? == "0", "let go, it runs");
        Ok(())
    }
}
