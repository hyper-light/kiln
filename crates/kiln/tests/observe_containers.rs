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
    // Each kill counted once: what kiln counts grows as the kernel's own count does (its
    // `memory.events`, read in the container), but for the kills of a look or so at either end;
    // never more than the kernel's. (The container ran before the fleet started: the kills before
    // kiln's first look are not counted, as Datadog's are not, a count beginning at its first
    // sample.)
    let kills_of = || -> anyhow::Result<f64> {
        let a = f.call(
            "POST",
            "/api/observe/v1/query",
            Some(&json!({ "query": format!("sum:container.memory.oom_events{{container_name:{oom}}}.as_count()"), "from": since, "to": now_ms(), "step": 60_000 })),
        )?;
        Ok(a["series"].as_array().into_iter().flatten().flat_map(|s| s["values"].as_array().cloned().unwrap_or_default()).filter_map(|v| v.as_f64()).sum())
    };
    let kernel = || -> anyhow::Result<f64> {
        let events = docker(&["exec", &oom, "cat", "/sys/fs/cgroup/memory.events"])?;
        events.lines().find_map(|l| l.strip_prefix("oom_kill ")).and_then(|v| v.trim().parse::<f64>().ok()).ok_or_else(|| anyhow::anyhow!("no oom_kill in {events}"))
    };
    let (c0, k0) = (kills_of()?, kernel()?);
    std::thread::sleep(Duration::from_secs(20));
    let (c1, k1) = (kills_of()?, kernel()?);
    anyhow::ensure!(k1 - k0 >= 10.0, "the kernel kept killing it: {k0} then {k1}");
    anyhow::ensure!(c1 <= k1 && ((c1 - c0) - (k1 - k0)).abs() <= 4.0, "each kill counted once: {c0} then {c1} counted, the kernel's {k0} then {k1}");
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

/// The OpenTelemetry Collector the scenario runs (contrib: its `docker_stats` receiver).
const COLLECTOR: &str = "otel/opentelemetry-collector-contrib:0.162.0";

/// Containers an OpenTelemetry Collector reports (§10.11), and nothing else does: a kiln server
/// and a runner as processes of their own (each node looks at its host's containers once, as it
/// starts, then not for an hour), then a container of the test's own, busy under half a core and
/// 64 MiB and writing a megabyte a second to its disk, and the Collector (contrib
/// 0.162.0) reading Docker every 2 s with its `docker_stats` receiver (its CPU limit, throttled
/// time, processes and uptime asked for) and sending OTLP to the runner. The container listed in
/// kiln's units from the Collector's names: its CPU near its quota (Docker's cumulative
/// nanoseconds read as a rate), its limits as Docker has them, throttled, its working set (Docker's
/// usage less the cache), its processes, its writes, its uptime; its name, image and engine from
/// the Collector's attributes. Its page's CPU chart the Collector's series' query, a rate of
/// cumulative nanoseconds, and that query's answer its CPU in cores.
#[test]
#[ignore = "needs Docker: runs containers and an OpenTelemetry Collector, and a kiln server and runner as processes"]
fn a_collector_s_containers_are_listed_in_kiln_s_units() -> anyhow::Result<()> {
    let mut f = Fleet::new()?;
    f.start_runner("r1")?;
    let otlp = *f.otlp.get("r1").ok_or_else(|| anyhow::anyhow!("r1's receiver"))?;
    // Each node's one look as it starts, done before the containers are.
    std::thread::sleep(Duration::from_secs(5));
    let run = now_ms();
    let (busy, collector) = (format!("kiln-e2e-otel-busy-{run}"), format!("kiln-e2e-otelcol-{run}"));
    let started = Started(vec![busy.clone(), collector.clone()]);
    let busy_id = docker(&[
        "run",
        "-d",
        "--name",
        &busy,
        "--cpus",
        "0.5",
        "--memory",
        "64m",
        "alpine:latest",
        "sh",
        "-c",
        // A megabyte a second written and flushed (its writeback the container's), and a core's
        // worth of spinning under half a core's quota.
        "while :; do dd if=/dev/zero of=/tmp/f bs=1M count=1 conv=fsync 2>/dev/null; sleep 1; done & while :; do :; done",
    ])?;
    let dir = tempfile::tempdir()?;
    let config = format!(
        "receivers:
  docker_stats:
    endpoint: unix:///var/run/docker.sock
    api_version: \"1.44\"
    collection_interval: 2s
    metrics:
      container.cpu.limit:
        enabled: true
      container.cpu.throttling_data.throttled_time:
        enabled: true
      container.pids.count:
        enabled: true
      container.uptime:
        enabled: true
processors:
  resource:
    attributes:
      - key: host.name
        value: otel-e2e-{run}
        action: upsert
exporters:
  otlphttp:
    endpoint: http://127.0.0.1:{otlp}
service:
  telemetry:
    metrics:
      level: none
  pipelines:
    metrics:
      receivers: [docker_stats]
      processors: [resource]
      exporters: [otlphttp]
"
    );
    std::fs::write(dir.path().join("config.yaml"), config)?;
    let socket_group = docker(&["run", "--rm", "-v", "/var/run/docker.sock:/s:ro", "alpine:latest", "stat", "-c", "%g", "/s"])?;
    docker(&[
        "run",
        "-d",
        "--name",
        &collector,
        "--network",
        "host",
        "--group-add",
        &socket_group,
        "-v",
        "/var/run/docker.sock:/var/run/docker.sock:ro",
        "-v",
        &format!("{}:/etc/otelcol-contrib/config.yaml:ro", dir.path().join("config.yaml").display()),
        COLLECTOR,
    ])?;

    // Listed from the Collector's series alone, in kiln's units.
    let mut seen = Value::Null;
    wait(&f, "the busy container listed from the Collector's series", Duration::from_secs(120), || {
        seen = named(&listed(&f, &format!("filter={busy}"))?, &busy).cloned().unwrap_or(Value::Null);
        let g = &seen["figures"];
        Ok(g["cpu_nanocores"].as_f64().is_some_and(|c| c > 1e8) && g["cpu_throttled_ns"].as_f64().is_some_and(|t| t > 0.0) && g["io_write_bytes"].as_f64().is_some_and(|w| w > 0.0))
    })
    .map_err(|e| anyhow::anyhow!("{e}\nlast seen: {seen}\nthe Collector's log: {}", docker(&["logs", "--tail", "30", &collector]).unwrap_or_default()))?;
    let g = &seen["figures"];
    anyhow::ensure!(
        seen["id"] == busy_id.as_str() && seen["runtime"] == "docker" && seen["image"]["name"].as_str().is_some_and(|i| i.starts_with("alpine")) && seen["state"] == "running",
        "its name, engine and image: {seen}"
    );
    let cpu = g["cpu_nanocores"].as_f64().unwrap_or(0.0);
    anyhow::ensure!((3.5e8..=5.5e8).contains(&cpu), "near its quota of half a core: {cpu} nanocores");
    anyhow::ensure!(g["cpu_limit_nanocores"] == 5e8 && g["memory_limit_bytes"] == 67_108_864.0, "its limits as Docker has them: {g}");
    anyhow::ensure!(g["working_set_bytes"].as_f64().is_some_and(|w| w > 0.0 && w < 67_108_864.0) && g["memory_bytes"].is_null(), "Docker's usage less the cache, its working set: {g}");
    anyhow::ensure!(g["pids"].as_f64().is_some_and(|p| p >= 2.0) && g["uptime_seconds"].as_f64().is_some_and(|u| u > 0.0), "its processes and uptime: {g}");

    // Its page draws its CPU from the Collector's series, and that query answers its cores.
    let one = f.call("GET", &format!("/api/observe/v1/containers/{busy_id}"), None)?;
    let query = one["charts"]["cpu"][0]["query"].as_str().unwrap_or_default().to_owned();
    anyhow::ensure!(query == format!("sum:container.cpu.usage.total{{container.id:{busy_id}}}.as_rate() / 1000000000"), "the Collector's series drawn: {}", one["charts"]);
    anyhow::ensure!(
        one["charts"]["io"].as_array().is_some_and(|l| l.len() == 2) && one["charts"]["memory"].as_array().is_some_and(|l| l.iter().any(|l| l["name"] == "Working set")),
        "its I/O and working set drawn: {}",
        one["charts"]
    );
    // Its buckets' cores but the newest (still filling: a counter's rise over a whole bucket's
    // width), their median.
    let a = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": query, "from": now_ms() - 120_000, "to": now_ms(), "step": 10_000 })))?;
    let mut cores: Vec<f64> = a["series"][0]["values"].as_array().into_iter().flatten().filter_map(Value::as_f64).collect();
    cores.pop();
    cores.sort_by(f64::total_cmp);
    let median = cores.get(cores.len() / 2).copied().unwrap_or(f64::NAN);
    anyhow::ensure!(cores.len() >= 3 && (0.35..=0.55).contains(&median), "the chart's query, its cores: {median} of {cores:?} ({a})");
    drop(started);
    Ok(())
}
