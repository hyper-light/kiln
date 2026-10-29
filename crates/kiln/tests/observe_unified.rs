//! macOS's unified log collected, live (docs/observe.md §8.1, §13): a kiln server and a runner on a
//! Mac, a `unified_log` source of the runner's reading the lines `logger` writes. Every line carries
//! its number, so each is counted where it is stored: through the runner stopped (the lines written
//! meanwhile read by `log show` as it starts again) and killed, one lost or repeated is named.
#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss, clippy::cast_possible_truncation)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::{Fleet, at, now_ms};
use serde_json::{Value, json};

/// Writes lines `seqs` to the unified log, `<tag> n=<seq>`, at about `per_s` a second.
fn write(tag: &str, seqs: std::ops::Range<u64>, per_s: u64, stop: &AtomicBool) -> u64 {
    let start = Instant::now();
    let mut next = seqs.start;
    while next < seqs.end && !stop.load(Ordering::Relaxed) {
        let _ = std::process::Command::new("logger").args(["-t", "kilne2e", &format!("{tag} n={next}")]).status();
        next += 1;
        let due = start + Duration::from_secs_f64((next - seqs.start) as f64 / per_s as f64);
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
    }
    next
}

/// Each line's count of records stored (by its `n=`).
fn counts(f: &Fleet, tag: &str, from: i64) -> anyhow::Result<BTreeMap<u64, u64>> {
    let a = f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": format!("\"{tag}\""), "from": from, "to": now_ms() + 60_000, "limit": 5_000 })))?;
    let mut out = BTreeMap::new();
    for r in at(&a, &["records"]).as_array().into_iter().flatten() {
        if let Some(n) = at(r, &["message"]).as_str().and_then(|m| m.rsplit("n=").next()).and_then(|n| n.parse::<u64>().ok()) {
            *out.entry(n).or_insert(0) += 1;
        }
    }
    Ok(out)
}

/// Waits until every line of `seqs` is stored; then their counts.
fn settled(f: &Fleet, tag: &str, from: i64, seqs: std::ops::Range<u64>, within: Duration) -> anyhow::Result<BTreeMap<u64, u64>> {
    let start = Instant::now();
    loop {
        let c = counts(f, tag, from)?;
        let missing: Vec<u64> = seqs.clone().filter(|s| !c.contains_key(s)).collect();
        if missing.is_empty() {
            return Ok(c);
        }
        anyhow::ensure!(start.elapsed() < within, "{} lines of {seqs:?} never stored, the first {:?}\n{}", missing.len(), &missing[..missing.len().min(20)], f.logs());
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn repeated(c: &BTreeMap<u64, u64>, seqs: std::ops::Range<u64>) -> Vec<(u64, u64)> {
    seqs.filter_map(|s| c.get(&s).filter(|&&n| n > 1).map(|&n| (s, n))).collect()
}

#[test]
#[ignore = "starts real processes, on a Mac"]
fn the_unified_log_is_read_once_each_through_a_stop_and_a_kill() -> anyhow::Result<()> {
    let mut f = Fleet::new()?;
    f.start_runner("r1")?;
    f.until_caught_up(Duration::from_secs(90), "r1 a learner, caught up", &["r1"])?;
    let t0 = now_ms() - 60_000;
    let tag = format!("kiln-e2e-{}", now_ms());
    let source = json!({ "type": "unified_log", "predicate": format!("eventMessage BEGINSWITH \"{tag}\""), "level": "default", "service": "e2e", "tags": ["team:mac"], "on": "r1" });
    // What it reads, tried on the server first (here, the same Mac): no error.
    let tried = f.call("POST", "/api/observe/v1/log_sources/mac/test", Some(&source))?;
    anyhow::ensure!(at(&tried, &["ok"]) == true, "{tried:#}");
    let (code, b) = f.request("PUT", "/api/observe/v1/log_sources/mac", Some(&source))?;
    anyhow::ensure!(code == 204, "PUT: {code} {}", String::from_utf8_lossy(&b));
    // The stream starts as the source does: lines after it are read.
    std::thread::sleep(Duration::from_secs(3));
    let never = AtomicBool::new(false);
    write(&tag, 0..50, 50, &never);
    let c = settled(&f, &tag, t0, 0..50, Duration::from_secs(60))?;
    anyhow::ensure!(repeated(&c, 0..50).is_empty(), "{:?}", repeated(&c, 0..50));
    let found = f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": format!("\"{tag} n=7\""), "from": t0, "to": now_ms() + 60_000, "limit": 5 })))?;
    let r = at(&found, &["records"]).as_array().and_then(|r| r.first()).cloned().unwrap_or(Value::Null);
    anyhow::ensure!(at(&r, &["service"]) == "e2e" && at(&r, &["host"]) == "r1" && at(&r, &["source"]) == "unified_log" && at(&r, &["attributes", "team"]) == "mac" && at(&r, &["attributes", "process.command"]) == "logger", "{r:#}");

    // Stopped, lines written meanwhile, started again: read from where it was (`log show`), none
    // lost, none repeated.
    f.stop_runner("r1");
    write(&tag, 50..100, 50, &never);
    f.start_runner("r1")?;
    let c = settled(&f, &tag, t0, 0..100, Duration::from_secs(90))?;
    anyhow::ensure!(repeated(&c, 0..100).is_empty(), "a stop repeated {:?}", repeated(&c, 0..100));

    // Killed while lines come: none lost; those in flight may be stored twice, no more.
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (tag, stop) = (tag.clone(), stop.clone());
        std::thread::spawn(move || write(&tag, 100..400, 100, &stop))
    };
    std::thread::sleep(Duration::from_millis(1200));
    f.kill_runner("r1");
    std::thread::sleep(Duration::from_millis(500));
    f.start_runner("r1")?;
    let wrote = writer.join().map_err(|_| anyhow::anyhow!("the writer panicked"))?;
    anyhow::ensure!(wrote == 400);
    let c = settled(&f, &tag, t0, 0..400, Duration::from_secs(90))?;
    let twice = repeated(&c, 100..400);
    anyhow::ensure!(twice.iter().all(|&(_, n)| n == 2) && twice.len() <= 200, "a kill repeated {} lines", twice.len());
    eprintln!("a kill repeated {} of 300 lines (in flight when it came); a stop none", twice.len());
    stop.store(true, Ordering::Relaxed);
    f.stop_gently();
    Ok(())
}
