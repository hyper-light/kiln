//! Goal 9, plan 01a1a87b2cec step 2: what held a shard past 100 µs, on /metrics
//! (`kiln_shard_held_*`). A task that holds its shard shows under its spawn site, here, and the
//! label its code set; a shard nothing held shows nothing of it. A process of its own: the runtime
//! and the probes are the process's.

use std::time::{Duration, Instant};

/// How long each hold keeps its shard, and how many holds.
const HELD: Duration = Duration::from_millis(30);
const HOLDS: u64 = 3;

static LABEL: hyper_rt::slow::Label = hyper_rt::slow::Label { name: "a test's hold" };

/// The value of the `kiln_shard_held_*` series `family` for `shard` whose site is in this file and
/// whose label is `label`, from /metrics.
fn held(family: &str, shard: usize, label: &str) -> Option<f64> {
    let text = kiln::metrics::render();
    let head = format!("{family}{{shard=\"{shard}\",site=\"");
    text.lines()
        .filter(|l| l.starts_with(&head) && l.contains("tests/held_polls.rs:") && l.contains(&format!("label=\"{label}\"}}")))
        .find_map(|l| l.rsplit_once(' ').and_then(|(_, v)| v.parse().ok()))
}

#[test]
fn g9_s2_a_hold_shows_under_its_spawn_site_and_label() -> anyhow::Result<()> {
    kiln::rt::configure(kiln::rt::Profile { shards: 2, ..kiln::rt::Profile::tool() });
    anyhow::ensure!(kiln::rt::shard_count() == 2, "{} shards", kiln::rt::shard_count());
    anyhow::ensure!(kiln::shard_health::start(Duration::from_millis(10)) == 2, "a shard refused its probe");
    // Shard 1 held three times, each hold labeled; a poll that sleeps holds its shard as a blocking
    // call would.
    for _ in 0..HOLDS {
        anyhow::ensure!(kiln::rt::spawn_on(1, kiln::rt::labeled(&LABEL, async { std::thread::sleep(HELD) })), "shard 1 refused the hold");
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while held("kiln_shard_held_polls_total", 1, LABEL.name).unwrap_or(0.0) < HOLDS as f64 {
        anyhow::ensure!(Instant::now() < deadline, "the holds never showed: {:?}", held("kiln_shard_held_polls_total", 1, LABEL.name));
        std::thread::sleep(Duration::from_millis(20));
    }
    let seconds = held("kiln_shard_held_seconds_total", 1, LABEL.name).unwrap_or(0.0);
    anyhow::ensure!(seconds >= (HELD * 3).as_secs_f64() * 0.9, "the holds added up to {seconds} s");
    let longest = held("kiln_shard_held_longest_seconds", 1, LABEL.name).unwrap_or(0.0);
    anyhow::ensure!(longest >= HELD.as_secs_f64() * 0.9, "the longest hold was {longest} s");
    // Shard 0 took none of them.
    anyhow::ensure!(held("kiln_shard_held_polls_total", 0, LABEL.name).is_none(), "shard 0 shows a hold it never ran");
    Ok(())
}
