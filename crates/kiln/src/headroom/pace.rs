//! The work headroom can slow, how hard each runs, and the actuator: a group paced at `d`% runs
//! the first `d`% of every period and is stopped for the rest, so slowing is continuous rather than
//! on or off. Everything here is the runner's own, so none of it needs privileges.
//!
//! Linux: each agent session's cgroup (`crate::os::contain`), and, unless the settings say not to,
//! every transient unit the user's service manager runs (`systemd-run --user`: a build an agent
//! started to outlive its command), stopped and started with `cgroup.freeze`. Elsewhere there is
//! nothing to pace yet (work past the window still waits to start).
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

/// How the machine paces work: `"cgroup"`, or `"none"`.
pub fn how() -> &'static str {
    if cfg!(target_os = "linux") && imp::available() { "cgroup" } else { "none" }
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
    stop: Arc<AtomicBool>,
    record: PathBuf,
}

impl Pacer {
    /// Starts groups a previous runner left stopped (as its record says), then the thread.
    pub fn start(dir: &Path) -> Self {
        let record = dir.join("paced.json");
        release_recorded(&record);
        let shares: Arc<Mutex<HashMap<PathBuf, u8>>> = Arc::default();
        let period = Arc::new(Mutex::new(Duration::from_millis(1000)));
        let stop = Arc::new(AtomicBool::new(false));
        let (sh, pe, st) = (Arc::clone(&shares), Arc::clone(&period), Arc::clone(&stop));
        let spawned = std::thread::Builder::new().name("kiln-pacer".into()).spawn(move || {
            let base = Instant::now();
            let mut stopped: HashMap<PathBuf, bool> = HashMap::new();
            while !st.load(Ordering::Relaxed) {
                let p = pe.lock().as_millis().max(1) as u64;
                let now = base.elapsed().as_millis() as u64;
                let want: Vec<(PathBuf, u8)> = sh.lock().iter().map(|(d, s)| (d.clone(), *s)).collect();
                for (dir, share) in &want {
                    // Each group's period starts at its own offset, so they don't all start at once.
                    let offset = dir.to_string_lossy().bytes().fold(0u64, |a, b| a.wrapping_mul(31).wrapping_add(u64::from(b))) % p;
                    let run = (now + offset) % p < p * u64::from(*share) / 100;
                    if stopped.get(dir).copied() != Some(!run) && imp::freeze(dir, !run).is_ok() {
                        stopped.insert(dir.clone(), !run);
                    }
                }
                // Groups no longer paced run again.
                stopped.retain(|dir, was| {
                    if want.iter().any(|(d, _)| d == dir) {
                        return true;
                    }
                    if *was {
                        let _ = imp::freeze(dir, false);
                    }
                    false
                });
                std::thread::sleep(Duration::from_millis(50));
            }
            for (dir, was) in stopped {
                if was {
                    let _ = imp::freeze(&dir, false);
                }
            }
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "headroom: the pacer couldn't start; work past the window still waits to start");
        }
        Self { shares, period, stop, record }
    }

    /// Each paced group's share from now (percent; groups at 100 aren't listed).
    pub fn set(&self, paced: HashMap<PathBuf, u8>, period: Duration) {
        *self.period.lock() = period;
        let changed = *self.shares.lock() != paced;
        if changed {
            let dirs: Vec<String> = paced.keys().map(|d| d.display().to_string()).collect();
            if let Err(e) = crate::util::write_secret_unsynced(&self.record, &serde_json::to_vec(&dirs).unwrap_or_default(), 0o644) {
                tracing::warn!(error = %e, "headroom: couldn't note what is paced");
            }
            *self.shares.lock() = paced;
        }
    }
}

impl Drop for Pacer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for dir in self.shares.lock().keys() {
            let _ = imp::freeze(dir, false);
        }
        let _ = std::fs::remove_file(&self.record);
    }
}

/// Starts every group a record says was paced.
fn release_recorded(record: &Path) {
    let Ok(raw) = std::fs::read(record) else { return };
    let dirs: Vec<String> = serde_json::from_slice(&raw).unwrap_or_default();
    for d in &dirs {
        let _ = imp::freeze(Path::new(d), false);
    }
    if !dirs.is_empty() {
        tracing::info!(groups = dirs.len(), "headroom: work a previous runner left paced runs again");
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

    pub fn freeze(dir: &Path, stop: bool) -> std::io::Result<()> {
        fs::write(dir.join("cgroup.freeze"), if stop { "1" } else { "0" })
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::path::Path;

    use super::Group;

    pub fn available() -> bool {
        false
    }

    pub fn groups(_escapes: bool) -> Vec<Group> {
        Vec::new()
    }

    pub fn counters(_dir: &Path) -> (u64, u64, u64) {
        (0, 0, 0)
    }

    pub fn freeze(_dir: &Path, _stop: bool) -> std::io::Result<()> {
        Err(std::io::Error::other("no pacing on this system yet"))
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
