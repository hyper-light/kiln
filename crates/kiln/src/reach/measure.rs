//! What a reach run uses, measured where it runs (docs/reach.md, "Sizes from history"; audit H5):
//! from its own containment group (`crate::os::contain`), read as it starts and as it ends, and
//! never sampled in between (nothing here runs on a timer, so no sampling period is picked).
//!
//! - **Linux:** its cgroup's `cpu.stat` `usage_usec`, every process ever in the group, those that
//!   ended before it included (a core file of cgroup v2: there with no controller on). Where its
//!   group has the memory controller: `memory.peak`, the kernel's own high-water mark of what the
//!   group was charged (page cache included, so it's never under what the run held), and its own
//!   `memory.events` `oom_kill`, counting its processes killed by any OOM killer, the machine's
//!   included (Documentation/admin-guide/cgroup-v2.rst). A runner's run groups usually have no
//!   controllers: the runner's own cgroup holds its process, and cgroup v2 gives controllers only
//!   to the children of a cgroup that holds none ("no internal processes"). There memory isn't
//!   measured and kills aren't seen, and history doesn't size runs (`kiln_ci::sizes::predict`).
//! - **macOS, Windows:** not measured here, and no kill is seen: macOS's memory pressure kills
//!   (jetsam) and Windows' failed commits leave no count a runner can read for one process tree.
//!
//! TODO(lb/sense): once lb/sense commits its per-run observations (`crate::sense::run::Recorder`
//! and `Group::usage`, asks/history.md), measure through them; this is history's own until then
//! (lb/sense had no commit past 3dab626f on 2026-11-04).

use std::time::Instant;

use kiln_ci::sizes::{Counted, How, Usage};

/// A run measured here, from its start to its end.
#[derive(Debug)]
pub struct Measure {
    started: Instant,
    /// Its group's folder, where the system counts what's in it (Linux).
    dir: Option<std::path::PathBuf>,
    /// Its own group's OOM kills when it started, where they're counted.
    oom_before: Option<u64>,
}

impl Measure {
    /// The run started now, its processes `group`'s (none: they're held by nothing here).
    pub fn start(group: Option<&crate::os::contain::Group>) -> Measure {
        let dir = group_dir(group);
        let oom_before = dir.as_deref().and_then(oom_kills);
        Measure { started: Instant::now(), dir, oom_before }
    }

    /// It ended with `exit`: what it used, all told, with the bytes its processes read and wrote
    /// as the system counted them for its tree (`io`: its group's, or its job object's; W05-R26).
    /// Call it before its group goes (its counts are read there). Blocks a moment (it reads a few
    /// small files): call it off the shards.
    pub fn finish(self, exit: Option<i32>, io: (Option<u64>, Option<u64>)) -> Usage {
        let duration_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let u = Usage { duration_ms, exit, read_bytes: io.0, write_bytes: io.1, ..Usage::default() };
        match &self.dir {
            Some(dir) => counted(dir, self.oom_before, u),
            None => u,
        }
    }
}

/// `u` with what the system counted of the group in `dir` since it started: its CPU, its peak
/// memory and its OOM kills, each where it's kept.
fn counted(dir: &std::path::Path, oom_before: Option<u64>, mut u: Usage) -> Usage {
    let read = |f: &str| std::fs::read_to_string(dir.join(f)).ok();
    if let Some(us) = read("cpu.stat").and_then(|t| kiln_ci::sizes::keyed(&t, "usage_usec")) {
        (u.cpu_us, u.cpu_how) = (us, How::Exact);
    }
    if let Some(peak) = read("memory.peak").and_then(|t| t.trim().parse::<u64>().ok()) {
        (u.peak_mem, u.mem_how) = (peak, How::Exact);
    }
    if let Some(n) = kiln_ci::sizes::grew(oom_before, oom_kills(dir)) {
        (u.oom_kills, u.oom_counted) = (n, Counted::Group);
    }
    u
}

/// The folder the system counts `group` in.
#[cfg(target_os = "linux")]
fn group_dir(group: Option<&crate::os::contain::Group>) -> Option<std::path::PathBuf> {
    group.map(|g| g.dir().to_path_buf())
}

#[cfg(not(target_os = "linux"))]
fn group_dir(_group: Option<&crate::os::contain::Group>) -> Option<std::path::PathBuf> {
    None
}

/// The OOM kills the group in `dir` counts of its own processes now: none without the memory
/// controller there (no `memory.events`).
fn oom_kills(dir: &std::path::Path) -> Option<u64> {
    std::fs::read_to_string(dir.join("memory.events")).ok().and_then(|t| kiln_ci::sizes::keyed(&t, "oom_kill"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// H5: a run's CPU is its whole group's, a child that ended before it included: the cgroup's
    /// own count, read as it ends, not a scan of what's still running.
    #[cfg(target_os = "linux")]
    #[test]
    fn h5_a_runs_cpu_counts_its_children_that_ended_first() -> anyhow::Result<()> {
        if std::process::Command::new("perl").arg("-e").arg("1").output().is_err() {
            eprintln!("skipped: no perl here");
            return Ok(());
        }
        crate::os::contain::hold_agents();
        let Some(group) = crate::os::contain::Group::of(&format!("run-h5test{}", std::process::id())) else {
            eprintln!("skipped: this machine holds no process groups (no cgroup v2 to write)");
            return Ok(());
        };
        // A child busy for a second (by the clock to the µs: perl's own `time` is whole seconds),
        // which says the CPU it used (µs) and ends; then its shell ends with nothing else running.
        let busy = "perl -MTime::HiRes=time -e '$t = time; 1 while time - $t < 1; @c = times; printf \"%d\\n\", ($c[0] + $c[1]) * 1e6'";
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", &format!("{busy}; true")]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped());
        group.enter(&mut cmd);
        let m = Measure::start(Some(&group));
        let child = cmd.spawn()?;
        let inside = group.started(child.id());
        let out = child.wait_with_output()?;
        let used = m.finish(out.status.code(), (None, None));
        let has = |f: &str| group.dir().join(f).exists();
        let (events, peak) = (has("memory.events"), has("memory.peak"));
        group.kill();
        group.remove();
        anyhow::ensure!(inside, "the test's shell didn't start in its group");
        let busy_us: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0);
        anyhow::ensure!(used.exit == Some(0) && busy_us > 0 && used.duration_ms >= 1000, "{used:?}, the child said {busy_us} µs");
        anyhow::ensure!(used.cpu_how == How::Exact && used.cpu_us >= busy_us, "CPU {} µs, under the ended child's own {busy_us} µs: {used:?}", used.cpu_us);
        // Memory and kills only where the group has the memory controller; said either way.
        anyhow::ensure!((used.oom_counted == Counted::Group) == events && (used.mem_how == How::Exact) == peak, "memory.events {events}, memory.peak {peak}: {used:?}");
        Ok(())
    }

    /// H4, H5: what a group's own files say is read, each figure where it's kept: kills counted
    /// only where its own `memory.events` is, and a counter made again counts from nothing.
    #[test]
    fn h5_a_groups_own_counts_are_read_where_theyre_kept() -> anyhow::Result<()> {
        let d = tempfile::tempdir()?;
        std::fs::write(d.path().join("cpu.stat"), "usage_usec 2500000\nuser_usec 2000000\nsystem_usec 500000\n")?;
        let none = counted(d.path(), None, Usage::default());
        anyhow::ensure!((none.cpu_us, none.cpu_how, none.mem_how, none.oom_counted) == (2_500_000, How::Exact, How::None, Counted::Nowhere), "{none:?}");
        std::fs::write(d.path().join("memory.peak"), "6442450944\n")?;
        std::fs::write(d.path().join("memory.events"), "low 0\nhigh 0\nmax 3\noom 1\noom_kill 2\noom_group_kill 0\n")?;
        let all = counted(d.path(), oom_kills(d.path()).map(|n| n - 1), Usage::default());
        anyhow::ensure!((all.peak_mem, all.mem_how, all.oom_kills, all.oom_counted, all.killed()) == (6 << 30, How::Exact, 1, Counted::Group, Some(true)), "{all:?}");
        Ok(())
    }

    /// H4, H5: where nothing holds a run's processes, nothing is measured but its time, and no kill
    /// is taken for seen.
    #[test]
    fn h4_a_run_held_by_nothing_has_its_kills_unseen() {
        let used = Measure::start(None).finish(Some(0), (None, None));
        assert_eq!((used.mem_how, used.cpu_how, used.oom_counted, used.killed()), (How::None, How::None, Counted::Nowhere, None));
    }
}
