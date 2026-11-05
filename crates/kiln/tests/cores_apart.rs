//! Goal 9, plan 01a1a87b2cec step 4: a server with cores of its own keeps its other threads off its
//! shards' cores. The thread that fixed the shards keeps off them, and so does every thread it
//! starts after; a thread a task on a shard starts (which would run on that shard's core) is moved
//! off within a probe's period or so, and a child process a shard starts is moved as it starts; the
//! shards keep their cores. A process of its own: `rt::pin` and the runtime are the process's.

#[cfg(target_os = "linux")]
#[test]
fn g9_s4_threads_and_children_a_shard_starts_leave_its_core() -> anyhow::Result<()> {
    use std::time::{Duration, Instant};

    use rustix::thread::{CpuSet, sched_getaffinity};

    let cpus = |set: &CpuSet| -> Vec<u32> { (0..CpuSet::MAX_CPU).filter(|c| set.is_set(*c)).filter_map(|c| u32::try_from(c).ok()).collect() };
    let allowed = cpus(&sched_getaffinity(None)?);
    // Two shards on cores of their own and one CPU for the rest: three at least.
    let Some(cores) = allowed.get(..3).map(<[u32]>::to_vec) else { return Ok(()) };
    let shard_cores: Vec<u32> = cores.iter().skip(1).copied().collect();
    let on_shard_cores = |set: &CpuSet| shard_cores.iter().any(|c| set.is_set(*c as usize));
    kiln::rt::configure(kiln::rt::Profile { shards: 2, ..kiln::rt::Profile::tool() });
    kiln::rt::pin(cores.clone());
    anyhow::ensure!(kiln::rt::shard_count() == 2, "{} shards", kiln::rt::shard_count());
    anyhow::ensure!(!on_shard_cores(&sched_getaffinity(None)?), "the thread that fixed the shards runs on their cores");
    let later = std::thread::spawn(move || sched_getaffinity(None).map(|s| cpus(&s))).join().map_err(|_| anyhow::anyhow!("a thread panicked"))??;
    anyhow::ensure!(later.iter().all(|c| !cores.iter().skip(1).any(|s| s == c)), "a thread started after runs on the shards' cores: {later:?}");

    // Each shard on its own core, and a thread and a child started from it.
    let (tx, rx) = std::sync::mpsc::channel::<(usize, Vec<u32>, u32, Option<kiln::proc::Running>)>();
    for shard in 0..2 {
        let tx = tx.clone();
        anyhow::ensure!(
            kiln::rt::spawn_on(shard, async move {
                let own = sched_getaffinity(None).map(|s| (0..CpuSet::MAX_CPU).filter(|c| s.is_set(*c)).filter_map(|c| u32::try_from(c).ok()).collect()).unwrap_or_default();
                // A thread of the shard's starting, kept running while the test looks at it.
                let (tid_tx, tid_rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tid_tx.send(kiln::os::thread_id().unwrap_or(0));
                    std::thread::sleep(Duration::from_secs(5));
                });
                let tid = tid_rx.recv_timeout(Duration::from_secs(5)).unwrap_or(0);
                let mut sleep = std::process::Command::new("sleep");
                sleep.arg("5");
                let _ = tx.send((shard, own, tid, kiln::proc::spawn_group(sleep).ok()));
            }),
            "shard {shard} refused the task"
        );
    }
    let mut seen = Vec::new();
    for _ in 0..2 {
        seen.push(rx.recv_timeout(Duration::from_secs(20))?);
    }
    for (shard, own, tid, running) in seen {
        let core = shard_cores.get(shard).copied();
        anyhow::ensure!(own.len() == 1 && own.first().copied() == core, "shard {shard} runs on {own:?}, not {core:?} alone");
        let child = running.as_ref().map_or(0, |r| r.pgid);
        anyhow::ensure!(tid != 0 && child > 0, "shard {shard}'s thread ({tid}) or child ({child}) didn't start");
        let child_cpus = sched_getaffinity(rustix::process::Pid::from_raw(child))?;
        anyhow::ensure!(!on_shard_cores(&child_cpus), "shard {shard}'s child runs on its core: {:?}", cpus(&child_cpus));
        let thread = rustix::process::Pid::from_raw(i32::try_from(tid)?).ok_or_else(|| anyhow::anyhow!("no thread {tid}"))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while on_shard_cores(&sched_getaffinity(Some(thread))?) {
            anyhow::ensure!(Instant::now() < deadline, "shard {shard}'s thread still runs on its core after 5 s");
            std::thread::sleep(Duration::from_millis(10));
        }
        if let Some(r) = running {
            r.kill_group();
        }
    }
    Ok(())
}
