//! Datadog's Agent, live (docs/observe.md §8.6, §13): a kiln server as a process, and Datadog's
//! Agent itself (7.84, in Docker) pointed at it (`DD_DD_URL`, its logs' `logs_dd_url`), its API key
//! observe's ingest token. What a user relies on: what an Agent fleet submits arrives in kiln as
//! Datadog would show it: its own checks' metrics (a host's load and CPU, of its hostname), what
//! applications send its DogStatsD (counts as counted, a gauge's last value, a distribution's
//! count and percentiles), its events and service checks, a log file it tails; a key that is not
//! one is refused as Datadog refuses it.
//!
//! It needs Docker: `cargo test -p kiln --test observe_ddagent -- --ignored --nocapture`.
#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::net::UdpSocket;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Fleet, at, free_port, now_ms};
use serde_json::{Value, json};

const IMAGE: &str = "gcr.io/datadoghq/agent:7.84.4";
const HOST: &str = "kiln-agent-e2e";

/// The Agent in a container on the host's network.
struct Agent {
    name: String,
}

impl Agent {
    fn start(http: u16, key: &str, dogstatsd: u16, conf: &std::path::Path, logs: &std::path::Path) -> anyhow::Result<Self> {
        let name = format!("kiln-ddagent-{}-{http}", std::process::id());
        let (cmd_port, expvar) = (free_port()?, free_port()?);
        let env = [
            ("DD_API_KEY", key.to_owned()),
            ("DD_DD_URL", format!("http://127.0.0.1:{http}")),
            ("DD_HOSTNAME", HOST.to_owned()),
            ("DD_LOGS_ENABLED", "true".to_owned()),
            ("DD_LOGS_CONFIG_LOGS_DD_URL", format!("127.0.0.1:{http}")),
            ("DD_LOGS_CONFIG_LOGS_NO_SSL", "true".to_owned()),
            ("DD_LOGS_CONFIG_FORCE_USE_HTTP", "true".to_owned()),
            ("DD_DOGSTATSD_PORT", dogstatsd.to_string()),
            ("DD_APM_ENABLED", "false".to_owned()),
            ("DD_PROCESS_AGENT_ENABLED", "false".to_owned()),
            ("DD_CMD_PORT", cmd_port.to_string()),
            ("DD_EXPVAR_PORT", expvar.to_string()),
            ("DD_INVENTORIES_ENABLED", "false".to_owned()),
        ];
        let mut c = Command::new("docker");
        c.args(["run", "-d", "--rm", "--name", &name, "--network", "host"]);
        c.args(["-v", &format!("{}:/etc/datadog-agent/conf.d/e2e.d:ro", conf.display()), "-v", &format!("{}:/e2e-logs", logs.display())]);
        for (k, v) in &env {
            c.args(["-e", &format!("{k}={v}")]);
        }
        let out = c.arg(IMAGE).output()?;
        anyhow::ensure!(out.status.success(), "docker run: {}", String::from_utf8_lossy(&out.stderr));
        Ok(Self { name })
    }

    fn logs(&self) -> String {
        Command::new("docker").args(["logs", "--tail", "40", &self.name]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr)).unwrap_or_default()
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.name]).output();
    }
}

/// A query's values over `[from, to)` on a 10 s grid, by tag `key`.
fn by(f: &Fleet, q: &str, key: &str, from: i64, to: i64) -> anyhow::Result<BTreeMap<String, Vec<Option<f64>>>> {
    let a = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": q, "from": from, "to": to, "step": 10_000 })))?;
    Ok(at(&a, &["series"]).as_array().into_iter().flatten().map(|s| (at(s, &["tags", key]).as_str().unwrap_or("").to_owned(), at(s, &["values"]).as_array().into_iter().flatten().map(Value::as_f64).collect())).collect())
}

fn total(f: &Fleet, q: &str, from: i64) -> anyhow::Result<f64> {
    Ok(by(f, q, "host", from, now_ms() + 60_000)?.values().flatten().flatten().sum())
}

fn last(f: &Fleet, q: &str, from: i64) -> anyhow::Result<Option<f64>> {
    Ok(by(f, q, "host", from, now_ms() + 60_000)?.values().filter_map(|v| v.iter().rev().find_map(|x| *x)).reduce(f64::max))
}

fn records(f: &Fleet, query: &str, from: i64) -> anyhow::Result<Vec<Value>> {
    let found = f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": query, "from": from, "to": now_ms() + 60_000, "limit": 100 })))?;
    Ok(at(&found, &["records"]).as_array().cloned().unwrap_or_default())
}

/// Waits until `done` holds.
fn until(what: &str, within: Duration, mut done: impl FnMut() -> anyhow::Result<bool>, agent: &Agent, f: &Fleet) -> anyhow::Result<()> {
    let start = Instant::now();
    loop {
        if done()? {
            return Ok(());
        }
        anyhow::ensure!(start.elapsed() < within, "{what}\n--- agent\n{}\n{}", agent.logs(), f.logs());
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[test]
#[ignore = "needs Docker: runs Datadog's Agent, and a kiln server as a process"]
fn datadog_s_agent_submits_to_kiln_as_to_datadog() -> anyhow::Result<()> {
    let t0 = now_ms() - 60_000;
    let dir = tempfile::tempdir()?;
    let f = Fleet::new()?;
    let ingest = std::fs::read_to_string(f.root.path().join("server").join("observe").join("ingest.token"))?.trim().to_owned();
    // A key that is not one: what Datadog answers.
    let (code, body) = common::raw(f.http, "POST", "/api/v2/series", "DD-API-KEY: nope\r\nContent-Type: application/x-protobuf\r\n", b"")?;
    anyhow::ensure!(code == 403 && String::from_utf8_lossy(&body).contains("API key is missing or invalid"), "{code}");
    let (code, _) = common::raw(f.http, "GET", &format!("/api/v1/validate?api_key={ingest}"), "", b"")?;
    anyhow::ensure!(code == 200);
    // A log file the Agent tails.
    let (conf, logs) = (dir.path().join("conf"), dir.path().join("logs"));
    std::fs::create_dir_all(&conf)?;
    std::fs::create_dir_all(&logs)?;
    std::fs::write(conf.join("conf.yaml"), "logs:\n  - type: file\n    path: /e2e-logs/app.log\n    service: e2e-svc\n    source: e2e-src\n")?;
    std::fs::write(logs.join("app.log"), "")?;
    for p in [&conf, &logs] {
        std::process::Command::new("chmod").args(["-R", "a+rwX"]).arg(p).output()?;
    }
    let dogstatsd = UdpSocket::bind("127.0.0.1:0")?.local_addr()?.port();
    let agent = Agent::start(f.http, &ingest, dogstatsd, &conf, &logs)?;

    // Its own checks' metrics, of its host.
    until(
        "the Agent's load check",
        Duration::from_secs(120),
        || Ok(last(&f, &format!("max:system.load.1{{host:{HOST}}}"), t0)?.is_some_and(|v| v >= 0.0) && last(&f, &format!("avg:system.cpu.user{{host:{HOST}}}"), t0)?.is_some()),
        &agent,
        &f,
    )?;
    // Its own service checks.
    until("the Agent's up check", Duration::from_secs(60), || Ok(!records(&f, "source:service_check @check:datadog.agent.up", t0)?.is_empty()), &agent, &f)?;

    // What applications send its DogStatsD.
    let udp = UdpSocket::bind("127.0.0.1:0")?;
    let to = ("127.0.0.1", dogstatsd);
    for i in 0..30 {
        udp.send_to(b"e2e.agent.hits:1|c|#route:/a", to)?;
        udp.send_to(format!("e2e.agent.temp:{}|g", 20 + i % 3).as_bytes(), to)?;
        udp.send_to(format!("e2e.agent.size:{}|d", 1 + i % 10).as_bytes(), to)?;
    }
    udp.send_to(b"_e{9,4}:e2e event|text|t:error|#env:e2e", to)?;
    udp.send_to(b"_sc|e2e.agent.check|2|#env:e2e|m:refused", to)?;
    let sent_at = now_ms() - 30_000;
    until("the counts, through the Agent", Duration::from_secs(90), || Ok(total(&f, "sum:e2e.agent.hits{route:/a}.as_count()", sent_at)? == 30.0), &agent, &f)?;
    let temp = last(&f, "max:e2e.agent.temp{*}", sent_at)?;
    anyhow::ensure!(temp.is_some_and(|t| (20.0..=22.0).contains(&t)), "the gauge: {temp:?}");
    until("the distribution's count", Duration::from_secs(60), || Ok(total(&f, "sum:e2e.agent.size.count{*}.as_count()", sent_at)? == 30.0), &agent, &f)?;
    // 1 to 10 three times each: a median of 5 (or 6), the sketch's within its 0.78% and kiln's 1.09%.
    let p50 = last(&f, "p50:e2e.agent.size{*}", sent_at)?.unwrap_or(0.0);
    anyhow::ensure!((4.9..=6.2).contains(&p50), "the distribution's median: {p50}");
    until("the event", Duration::from_secs(60), || Ok(!records(&f, "source:kiln.event @title:\"e2e event\"", sent_at)?.is_empty()), &agent, &f)?;
    let ev = records(&f, "source:kiln.event @title:\"e2e event\"", sent_at)?;
    anyhow::ensure!(ev.first().is_some_and(|e| at(e, &["status"]) == "error" && at(e, &["host"]) == HOST && at(e, &["attributes", "env"]) == "e2e"), "{ev:?}");
    until("the service check", Duration::from_secs(60), || Ok(!records(&f, "source:service_check @check:e2e.agent.check status:error", sent_at)?.is_empty()), &agent, &f)?;

    // A log file it tails.
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new().append(true).open(logs.join("app.log"))?;
    for i in 0..5 {
        writeln!(file, "2026-10-19T22:00:0{i}Z ERROR e2e line {i}")?;
    }
    drop(file);
    until("the tailed lines", Duration::from_secs(60), || Ok(records(&f, "service:e2e-svc source:e2e-src", sent_at)?.len() == 5), &agent, &f)?;
    let lines = records(&f, "service:e2e-svc", sent_at)?;
    anyhow::ensure!(lines.iter().all(|l| at(l, &["host"]) == HOST && at(l, &["attributes", "filename"]) == "app.log"), "{lines:?}");
    eprintln!("  agent      its checks, DogStatsD through it (counts, gauge, distribution, event, service check), a tailed file: all in kiln");
    Ok(())
}
