//! Containers live (docs/observe.md §10.10, §13): a kiln server and a runner as processes of their
//! own on a machine with Docker, the runner looking every 2 s. Two containers of its own: one busy
//! under a quota of half a core and 64 MiB, one whose `awk` the kernel kills for its memory again
//! and again under 32 MiB, the container living on. Each listed with its name, image, engine and
//! limits; the busy one's CPU near its quota and throttled; the other's OOM kills counted; every
//! container Docker says runs found; one found by the first 12 of its id; removed, stopped.
//!
//! `cargo test -p kiln --test observe_containers -- --ignored --nocapture` (Docker, `alpine:latest`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use common::{Fleet, at, now_ms};
use serde_json::{Value, json};

fn docker(args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("docker").args(args).output()?;
    anyhow::ensure!(out.status.success(), "docker {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr));
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Removes its containers however the test ends.
struct Started(Vec<String>);

impl Drop for Started {
    fn drop(&mut self) {
        for name in &self.0 {
            let _ = Command::new("docker").args(["rm", "-f", name]).output();
        }
    }
}

/// Waits up to `within` for `done`.
fn wait(f: &Fleet, what: &str, within: Duration, mut done: impl FnMut() -> anyhow::Result<bool>) -> anyhow::Result<()> {
    let start = Instant::now();
    loop {
        if done()? {
            return Ok(());
        }
        anyhow::ensure!(start.elapsed() < within, "never: {what}\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn listed(f: &Fleet, query: &str) -> anyhow::Result<Vec<Value>> {
    Ok(f.call("GET", &format!("/api/observe/v1/containers?{query}"), None)?["containers"].as_array().cloned().unwrap_or_default())
}

fn named<'a>(all: &'a [Value], name: &str) -> Option<&'a Value> {
    all.iter().find(|c| c["name"] == name)
}

#[test]
#[ignore = "needs Docker: runs containers, and a kiln server and runner as processes"]
fn a_machine_s_containers_are_listed_with_their_limits_throttling_and_oom_kills() -> anyhow::Result<()> {
    let run = format!("{:x}", now_ms() % 0xffff_ffff);
    let (busy, oom) = (format!("kiln-e2e-busy-{run}"), format!("kiln-e2e-oom-{run}"));
    let started = Started(vec![busy.clone(), oom.clone()]);
    let busy_id = docker(&["run", "-d", "--name", &busy, "--cpus", "0.5", "--memory", "64m", "alpine:latest", "sh", "-c", "while :; do :; done"])?;
    let oom_id = docker(&[
        "run",
        "-d",
        "--name",
        &oom,
        "--memory",
        "32m",
        "--memory-swap",
        "32m",
        "alpine:latest",
        "sh",
        "-c",
        // Its string doubled until the kernel kills it (an append at a time copies it each time:
        // quadratic, never reaching its limit), every second again.
        "while :; do awk 'BEGIN { s = \"x\"; while (1) s = s s }'; sleep 1; done",
    ])?;
    let mut f = Fleet::with_env(|_| String::new(), &[("KILN_OBSERVE_CONTAINER_RUNNING_MS", "10000")])?;
    f.start_runner_with("r1", &[("KILN_OBSERVE_HOST_SECONDS", "2")])?;

    // Listed, its figures its own: the busy one near its quota (half a core), throttled.
    wait(&f, "the busy container listed with its CPU", Duration::from_secs(120), || {
        Ok(named(&listed(&f, &format!("filter={busy}"))?, &busy)
            .is_some_and(|c| c["figures"]["cpu_nanocores"].as_f64().is_some_and(|v| v > 1e8) && c["figures"]["cpu_throttled_ns"].as_f64().is_some_and(|v| v > 0.0)))
    })?;
    // Its CPU over a minute of looks, against its quota.
    std::thread::sleep(Duration::from_secs(6));
    let all = listed(&f, &format!("filter={busy}"))?;
    let c = named(&all, &busy).ok_or_else(|| anyhow::anyhow!("listed"))?;
    anyhow::ensure!(
        c["id"] == busy_id.as_str() && c["state"] == "running" && c["runtime"] == "docker" && c["image"]["name"] == "alpine" && c["image"]["tag"] == "latest",
        "named as Docker names it: {c}"
    );
    let cpu = c["figures"]["cpu_nanocores"].as_f64().unwrap_or(0.0);
    anyhow::ensure!((3.5e8..=5.5e8).contains(&cpu), "near its quota of half a core: {cpu} nanocores");
    anyhow::ensure!(c["figures"]["cpu_limit_nanocores"] == 5e8 && c["figures"]["memory_limit_bytes"] == 67_108_864.0, "its limits: {c}");
    anyhow::ensure!(c["figures"]["pids"].as_f64().is_some_and(|p| p >= 1.0), "its processes: {c}");

    // The other's OOM kills, counted as the kernel kills its awk, the container living on.
    let since = now_ms() - 600_000;
    wait(&f, "its OOM kills counted", Duration::from_secs(120), || {
        let a = f.call(
            "POST",
            "/api/observe/v1/query",
            Some(&json!({ "query": format!("sum:container.memory.oom_events{{container_name:{oom}}}.as_count()"), "from": since, "to": now_ms(), "step": 60_000 })),
        )?;
        let kills: f64 = a["series"].as_array().into_iter().flatten().flat_map(|s| s["values"].as_array().cloned().unwrap_or_default()).filter_map(|v| v.as_f64()).sum();
        Ok(kills >= 1.0)
    })?;
    let o = named(&listed(&f, &format!("filter={oom}"))?, &oom).cloned().ok_or_else(|| anyhow::anyhow!("the OOM one listed"))?;
    anyhow::ensure!(o["id"] == oom_id.as_str() && o["state"] == "running" && o["figures"]["memory_limit_bytes"] == 33_554_432.0, "alive under its limit: {o}");

    // Every container Docker says runs is found (and a minikube node's pods besides, where one
    // runs): those running before and after 20 s (others' jobs' containers come and go here).
    let before: Vec<String> = docker(&["ps", "-q", "--no-trunc"])?.lines().map(str::to_owned).collect();
    std::thread::sleep(Duration::from_secs(20));
    let after = docker(&["ps", "-q", "--no-trunc"])?;
    let stable: Vec<&String> = before.iter().filter(|id| after.lines().any(|a| a == id.as_str())).collect();
    anyhow::ensure!(stable.len() >= 2, "its own two at least: {stable:?}");
    wait(&f, "every container Docker runs listed", Duration::from_secs(60), || {
        let all = listed(&f, "count=1000")?;
        Ok(stable.iter().all(|id| all.iter().any(|c| c["id"] == id.as_str())))
    })?;
    // One by the first 12 of its id, as `docker ps` shows it.
    let one = f.call("GET", &format!("/api/observe/v1/containers/{}", &busy_id[..12]), None)?;
    anyhow::ensure!(one["name"] == busy.as_str(), "found by its short id: {one}");
    anyhow::ensure!(f.request("GET", "/api/observe/v1/containers/0123456789ab", None)?.0 == 404, "none by an id that is not");

    // Removed: stopped (a sample no longer coming), listed among the stopped.
    docker(&["rm", "-f", &busy])?;
    wait(&f, "the busy container stopped", Duration::from_secs(90), || Ok(named(&listed(&f, &format!("state=stopped&filter={busy}"))?, &busy).is_some()))?;
    anyhow::ensure!(named(&listed(&f, &format!("filter={busy}"))?, &busy).is_none(), "no longer among the running");
    let totals = &f.call("GET", "/api/observe/v1/containers?count=1", None)?["totals"];
    anyhow::ensure!(at(totals, &["stopped"]).as_u64().is_some_and(|n| n >= 1), "counted among the stopped: {totals}");
    drop(started);
    Ok(())
}
