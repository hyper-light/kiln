//! Goal 1, plan 01a1a59c2a68 step 1: each shard's health on /metrics (`kiln::shard_health`). A
//! shard held for a known time shows its own delay and slow polls rise by about that, and the
//! others' stay flat. A process of its own: the runtime and the probes are the process's.

use std::time::{Duration, Instant};

use kiln::shard_health::{Snapshot, shards};

/// How long each hold keeps shard 1, and how many holds.
const HELD: Duration = Duration::from_millis(300);
const HOLDS: u64 = 3;

fn snapshots() -> Vec<Snapshot> {
    shards().iter().map(kiln::shard_health::Shard::snapshot).collect()
}

/// Waits until `done` says the shards are where it wants them, for at most 20 s.
fn until(what: &str, done: impl Fn(&[Snapshot]) -> bool) -> anyhow::Result<Vec<Snapshot>> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let now = snapshots();
        if done(&now) {
            return Ok(now);
        }
        anyhow::ensure!(Instant::now() < deadline, "{what}: {now:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Observations of `s`'s delay over `secs`.
fn over(i: usize, secs: f64) -> u64 {
    shards().get(i).map_or(0, |s| {
        let h = s.delay();
        let under = h.cumulative().iter().filter(|(bound, _)| *bound <= secs).map(|(_, n)| *n).max().unwrap_or(0);
        h.count().saturating_sub(under)
    })
}

#[test]
fn g1_s1_a_held_shard_shows_its_own_delay_and_slow_polls_alone() -> anyhow::Result<()> {
    kiln::rt::configure(kiln::rt::Profile { shards: 3, ..kiln::rt::Profile::tool() });
    anyhow::ensure!(kiln::rt::shard_count() == 3, "{} shards", kiln::rt::shard_count());
    anyhow::ensure!(kiln::shard_health::start(Duration::from_millis(10)) == 3, "a shard refused its probe");
    let before = until("every probe ran a few times", |s| s.len() == 3 && s.iter().all(|s| s.probes >= 5))?;
    let over_before: Vec<u64> = (0..3).map(|i| over(i, 0.25)).collect();

    // Shard 1 held: a task on it sleeps inside its poll, as a blocking call would, three times; its
    // probe runs between the holds.
    for k in 0..HOLDS {
        let seen = snapshots().get(1).map_or(0, |s| s.probes);
        anyhow::ensure!(kiln::rt::spawn_on(1, async { std::thread::sleep(HELD) }), "shard 1 refused the hold");
        until(&format!("shard 1's probe ran after hold {k}"), |s| s.get(1).is_some_and(|s| s.probes >= seen + 2 && s.slow.total() > 0))?;
    }
    let after = until("every probe ran since", |s| s.iter().zip(&before).all(|(a, b)| a.probes > b.probes + 5))?;

    // Its delay rose by about each hold, and its slow polls by one a hold.
    let held = after.get(1).ok_or_else(|| anyhow::anyhow!("no shard 1"))?;
    let b1 = before.get(1).ok_or_else(|| anyhow::anyhow!("no shard 1"))?;
    anyhow::ensure!(held.most_late >= HELD.mul_f64(0.9), "shard 1's probe was at most {:?} late", held.most_late);
    anyhow::ensure!(over(1, 0.25) >= over_before.get(1).copied().unwrap_or(0) + HOLDS, "shard 1's delay histogram didn't take its holds: {} over 0.25 s", over(1, 0.25));
    anyhow::ensure!(held.slow.total() >= b1.slow.total() + HOLDS, "shard 1's slow polls went {:?} to {:?}", b1.slow, held.slow);
    anyhow::ensure!(held.longest_poll >= HELD.mul_f64(0.9), "shard 1's longest poll: {:?}", held.longest_poll);
    // The others': flat. A stray poll the host held may count, never the holds.
    for i in [0, 2] {
        let (a, b) = (after.get(i).ok_or_else(|| anyhow::anyhow!("no shard {i}"))?, before.get(i).ok_or_else(|| anyhow::anyhow!("no shard {i}"))?);
        anyhow::ensure!(a.most_late < HELD / 2, "shard {i}'s probe was {:?} late", a.most_late);
        anyhow::ensure!(over(i, 0.25) == over_before.get(i).copied().unwrap_or(0), "shard {i}'s delay histogram moved over 0.25 s");
        anyhow::ensure!(a.longest_poll < HELD / 2, "shard {i}'s longest poll: {:?}", a.longest_poll);
        anyhow::ensure!(a.slow.total() <= b.slow.total() + 1, "shard {i}'s slow polls went {:?} to {:?}", b.slow, a.slow);
    }

    // On /metrics, every series for every shard (busy where the OS keeps a thread's CPU time).
    let text = kiln::metrics::render();
    for i in 0..3 {
        let mut series = vec![
            format!("kiln_shard_delay_seconds_bucket{{shard=\"{i}\",le=\"0.5\"}}"),
            format!("kiln_shard_delay_seconds_count{{shard=\"{i}\"}}"),
            format!("kiln_shard_stalled_seconds{{shard=\"{i}\"}}"),
            format!("kiln_shard_polls_total{{shard=\"{i}\"}}"),
            format!("kiln_shard_longest_poll_seconds{{shard=\"{i}\"}}"),
            format!("kiln_shard_scheduler_overrun_seconds{{shard=\"{i}\"}}"),
        ];
        for cause in ["task", "blocking", "host", "unknown"] {
            series.push(format!("kiln_shard_slow_polls_total{{shard=\"{i}\",cause=\"{cause}\"}}"));
        }
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            series.push(format!("kiln_shard_busy_seconds_total{{shard=\"{i}\"}}"));
        }
        for s in series {
            anyhow::ensure!(text.contains(&s), "/metrics lacks {s}");
        }
    }
    // Shard 1's busy share stays a CPU share: the holds slept, so they aren't busy.
    if let (Some(a), Some(b)) = (held.busy, b1.busy) {
        anyhow::ensure!(a.saturating_sub(b) < HELD, "shard 1 was busy {:?} on the CPU through holds that slept", a.saturating_sub(b));
    }
    Ok(())
}
