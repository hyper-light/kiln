//! docs/observe.md §13: the observe group live. A kiln server and two runners run as processes of
//! their own on this machine, on real sockets and disks, each with a home of its own and a clean
//! environment; they are killed (SIGKILL: nothing flushed, nothing said) and started again, and a
//! runner that was away while the group compacted takes an image whose files it fetches over HTTP.
//! What the scenario proves is what the API answers, and that every replica holds the same: the
//! full digest of each (every sample, every record) at one index of the log.
//!
//! It starts real processes, so it is not run by default:
//! `cargo test -p kiln --test observe_live -- --ignored --nocapture`.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

mod common;
use common::{Fleet, at, caught_up, now_ms, raw};

#[test]
#[ignore = "starts a kiln server and two runners as processes"]
fn the_group_answers_alike_on_every_replica_through_kills_and_an_image() -> anyhow::Result<()> {
    let mut f = Fleet::new()?;
    let s = f.until(Duration::from_secs(30), "the server leads", |s| at(s, &["role"]) == "server" && at(s, &["leader"]) == 1)?;
    anyhow::ensure!(*at(&s, &["voters"]) == json!([1]), "the server alone votes: {s}");
    let now = crate_now();
    // On the query's grid (10 s), so the buckets are the points.
    let t0 = (now - 600_000) / 10_000 * 10_000;
    // Points before any runner: the server takes them alone.
    f.write("e2e.temp", &["a", "b", "c"], t0, 30, |h, i| f64::from(h.bytes().next().unwrap_or(0)) + i as f64 / 4.0)?;
    let want = |hosts: &[&str], n: i64| -> BTreeMap<String, Vec<Option<f64>>> { hosts.iter().map(|h| ((*h).to_owned(), (0..n).map(|i| Some(f64::from(h.bytes().next().unwrap_or(0)) + i as f64 / 4.0)).collect())).collect() };
    let read = |f: &Fleet| f.query("avg:e2e.temp{*} by {host}", t0, t0 + 300_000 - 1);
    let start = Instant::now();
    while read(&f)? != want(&["a", "b", "c"], 30) {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(10), "the points never read back: {:?}\n{}", read(&f)?, f.logs());
        std::thread::sleep(Duration::from_millis(100));
    }
    // Two runners join: each a learner, each catching up from the log.
    f.start_runner("r1")?;
    f.start_runner("r2")?;
    f.until(Duration::from_secs(60), "both runners caught up", |s| caught_up(s, &["r1", "r2"]))?;
    // Their own figures came through their own writes (runner → server → group).
    let start = Instant::now();
    loop {
        let names = f.call("GET", "/api/observe/v1/metrics?match=kiln.process.*", None)?;
        let hosts = f.call("GET", "/api/observe/v1/tags?key=host", None)?;
        let has = |v: &Value, x: &str| v.as_array().is_some_and(|a| a.iter().any(|y| y == x));
        if has(at(&names, &["metrics"]), "kiln.process.rss_bytes") && has(at(&hosts, &["values"]), "r1") && has(at(&hosts, &["values"]), "r2") {
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "runners' own writes never arrived: {names} {hosts}\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(200));
    }
    // Logs, searched newest first.
    let recs: Vec<Value> = (0..200).map(|i| json!({ "message": format!("request {i} {}", if i % 10 == 0 { "failed: connection reset" } else { "ok" }), "timestamp": t0 + i * 1000, "status": if i % 10 == 0 { "error" } else { "info" }, "service": "e2e", "hostname": "a" })).collect();
    f.call("POST", "/api/observe/v1/logs", Some(&Value::Array(recs)))?;
    let start = Instant::now();
    loop {
        let found = f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": "service:e2e status:error \"connection reset\"", "from": t0, "to": now, "limit": 3 })))?;
        let msgs: Vec<&str> = at(&found, &["records"]).as_array().into_iter().flatten().filter_map(|r| at(r, &["message"]).as_str()).collect();
        if msgs == ["request 190 failed: connection reset", "request 180 failed: connection reset", "request 170 failed: connection reset"] {
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(10), "the log search answered {found}\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(100));
    }
    f.until(Duration::from_secs(30), "both runners hold the logs too", |s| caught_up(s, &["r1", "r2"]))?;

    // OTLP: the specification's own JSON example, gzip, to the server with the ingest token.
    let ingest = std::fs::read_to_string(f.root.path().join("server").join("observe").join("ingest.token"))?.trim().to_owned();
    let example = include_str!("../src/observe/fixtures/otlp-metrics.json");
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(example.as_bytes())?;
    let gz = gz.finish()?;
    let (code, body) = raw(f.http, "POST", "/v1/metrics", &format!("Authorization: Bearer {ingest}\r\nContent-Type: application/json\r\nContent-Encoding: gzip\r\n"), &gz)?;
    anyhow::ensure!(code == 200 && body == b"{}", "OTLP/JSON: {code} {}", String::from_utf8_lossy(&body));
    let (code, _) = raw(f.http, "POST", "/v1/metrics", "Authorization: Bearer wrong\r\nContent-Type: application/json\r\n", example.as_bytes())?;
    anyhow::ensure!(code == 401, "a wrong token: {code}");
    let (code, _) = raw(f.http, "POST", "/v1/metrics", &format!("Authorization: Bearer {ingest}\r\nContent-Type: application/x-protobuf\r\n"), &[0x0a, 0xff, 0x01])?;
    anyhow::ensure!(code == 400, "a malformed body: {code}");
    // The example's points are of 2018: past the samples' cutoff? No: 455 days is 15 months, and
    // 2018 is older. They are taken and dropped as expired, alike everywhere; its names are not
    // listed. Points of now, in protobuf, to a runner's own receiver, come through it.
    let r1 = *f.otlp.get("r1").ok_or_else(|| anyhow::anyhow!("r1's receiver"))?;
    let now_ns = u64::try_from(crate_now()).unwrap_or(0) * 1_000_000;
    let mut req = kiln_obs::otlp::pb::Writer::default();
    req.message(1, |rl| {
        rl.message(1, |res| {
            res.message(1, |kv| {
                kv.str(1, "service.name");
                kv.message(2, |v| v.str(1, "checkout"));
            });
        });
        rl.message(2, |sl| {
            sl.message(2, |m| {
                m.str(1, "e2e.otlp.requests");
                m.message(5, |g| {
                    g.message(1, |p| {
                        p.fixed64(3, now_ns);
                        p.double(4, 42.0);
                    });
                });
            });
        });
    });
    let (code, _) = raw(r1, "POST", "/v1/metrics", "Content-Type: application/x-protobuf\r\n", &req.buf)?;
    anyhow::ensure!(code == 200, "the runner's receiver: {code}");
    let mut logs = kiln_obs::otlp::pb::Writer::default();
    logs.message(1, |rl| {
        rl.message(2, |sl| {
            sl.message(2, |r| {
                r.fixed64(1, now_ns);
                r.varint(2, 17);
                r.message(5, |b| b.str(1, "otlp from an app on r1"));
            });
        });
    });
    let (code, _) = raw(r1, "POST", "/v1/logs", "Content-Type: application/x-protobuf\r\n", &logs.buf)?;
    anyhow::ensure!(code == 200, "the runner's log receiver: {code}");
    let start = Instant::now();
    loop {
        let q = f.query("sum:e2e.otlp.requests{service:checkout}", crate_now() - 120_000, crate_now() + 60_000)?;
        let found = f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": "\"otlp from an app\"", "from": crate_now() - 120_000, "to": crate_now() + 60_000, "limit": 5 })))?;
        let got_log = at(&found, &["records"]).as_array().is_some_and(|a| a.len() == 1);
        if q.values().any(|v| v.contains(&Some(42.0))) && got_log {
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "OTLP through r1 never arrived: {q:?} {found}\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(200));
    }
    f.until(Duration::from_secs(30), "both runners hold the OTLP points too", |s| caught_up(s, &["r1", "r2"]))?;
    let (index, digest) = f.verify(3)?;
    eprintln!("three replicas agree at index {index}: {digest:016x}");

    // r2's process ends at once; writes go on; the leader compacts its log past what r2 holds.
    f.kill_runner("r2");
    f.write("e2e.temp", &["a", "b", "c"], t0 + 300_000, 30, |h, i| f64::from(h.bytes().next().unwrap_or(0)) + (i + 30) as f64 / 4.0)?;
    f.write("e2e.away", &["x"], t0, 60, |_, i| i as f64)?;
    let s = f.until(Duration::from_secs(30), "r2 seen gone", |s| at(s, &["machines"]).as_array().into_iter().flatten().any(|m| at(m, &["machine"]) == "r2" && at(m, &["up"]).as_bool() == Some(false)))?;
    let before = at(&s, &["applied"]).as_u64().unwrap_or(0);
    f.call("POST", "/api/observe/v1/compact", Some(&json!({})))?;
    f.until(Duration::from_secs(30), "the leader compacted", |s| at(s, &["durable"]).as_u64().unwrap_or(0) >= before)?;
    f.start_runner("r2")?;
    f.until(Duration::from_secs(120), "r2 back by an image", |s| caught_up(s, &["r1", "r2"]))?;
    let r2_store = f.root.path().join("r2").join("state").join("observe").join("store");
    let blocks = std::fs::read_dir(&r2_store)?.filter_map(Result::ok).filter(|e| e.file_name().to_string_lossy().ends_with(".block")).count();
    anyhow::ensure!(blocks > 0, "r2 holds the image's blocks (it never cut on its own)");
    let (index, digest) = f.verify(3)?;
    eprintln!("after r2's image: three replicas agree at index {index}: {digest:016x}");

    // The server's process ends at once, and starts again: what it answered, it answers again.
    let before = read(&f)?;
    f.kill_server();
    f.start_server()?;
    f.until(Duration::from_secs(60), "the server leads again", |s| at(s, &["leader"]) == 1)?;
    let start = Instant::now();
    while read(&f)? != before {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(10), "after the server's restart it answers {:?}, before {before:?}", read(&f)?);
        std::thread::sleep(Duration::from_millis(100));
    }
    f.write("e2e.temp", &["d"], t0, 30, |h, i| f64::from(h.bytes().next().unwrap_or(0)) + i as f64 / 4.0)?;
    f.until(Duration::from_secs(60), "both runners caught up after the server's restart", |s| caught_up(s, &["r1", "r2"]))?;
    let (index, digest) = f.verify(3)?;
    eprintln!("after the server's restart: three replicas agree at index {index}: {digest:016x}");
    let members = f.members(&f.status()?);
    anyhow::ensure!(members.len() == 2 && members.values().all(|&m| m >= 2), "each runner kept its member: {members:?}");
    Ok(())
}

fn crate_now() -> i64 {
    i64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)).unwrap_or(0)
}

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let out = Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=kiln", "-c", "user.email=kiln@example.invalid", "-c", "commit.gpgsign=false"]).args(args).output()?;
    anyhow::ensure!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    Ok(())
}

#[test]
#[ignore = "starts a kiln server and a runner as processes, and runs a CI job"]
fn a_ci_job_that_runs_is_in_the_metrics_and_the_events() -> anyhow::Result<()> {
    // A repository on disk, with a pipeline of one job, run by hand.
    let repo_dir = tempfile::tempdir()?;
    std::fs::create_dir_all(repo_dir.path().join(".kiln"))?;
    std::fs::write(repo_dir.path().join(".kiln/ci.yaml"), "name: ci\non:\n  manual: true\njobs:\n  hello:\n    runs-on: [baremetal]\n    steps:\n      - name: say\n        run: echo hello from e2e\n")?;
    git(repo_dir.path(), &["init", "-q", "-b", "main"])?;
    git(repo_dir.path(), &["add", "."])?;
    git(repo_dir.path(), &["commit", "-q", "-m", "a pipeline"])?;
    let url = format!("file://{}", repo_dir.path().display());
    let mut f = Fleet::with(|_| format!("[[repos]]\nname = \"e2e\"\nclone_url = \"{url}\"\nbranches = [\"main\"]\npoll_seconds = 2\n"))?;
    f.until(Duration::from_secs(30), "the server leads", |s| at(s, &["leader"]).as_u64() == Some(1))?;
    f.start_runner("r1")?;
    f.until(Duration::from_secs(60), "r1 caught up", |s| caught_up(s, &["r1"]))?;
    let start = Instant::now();
    let id = loop {
        let repos = f.call("GET", "/api/repos", None)?;
        if let Some(id) = repos.as_array().into_iter().flatten().find(|r| at(r, &["name"]) == "e2e").and_then(|r| at(r, &["id"]).as_str().map(str::to_owned)) {
            break id;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "the repository never appeared: {repos}\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(200));
    };
    let start = Instant::now();
    loop {
        // The first poll records the branch; the trigger takes it after.
        if f.call("POST", &format!("/api/repos/{id}/trigger"), Some(&json!({ "branch": "main", "pipeline": "ci" }))).is_ok() {
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "the pipeline never triggered\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(500));
    }
    // The job runs on r1; its end is a point of each metric and an event.
    let start = Instant::now();
    loop {
        let now = crate_now();
        let jobs = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": "sum:kiln.ci.jobs.finished{repo:e2e} by {job,status,runner}.as_count()", "from": now - 600_000, "to": now + 60_000, "step": 60_000 })))?;
        let events = f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": "source:kiln.event @event.type:ci.job.finished @repo:e2e", "from": now - 600_000, "to": now + 60_000, "limit": 5 })))?;
        let series = at(&jobs, &["series"]).as_array().cloned().unwrap_or_default();
        let one = series.iter().any(|s| at(s, &["tags", "job"]) == "hello" && at(s, &["tags", "runner"]) == "r1" && at(s, &["tags", "status"]) == "succeeded" && at(s, &["values"]).as_array().is_some_and(|v| v.iter().filter_map(Value::as_f64).sum::<f64>() == 1.0));
        let event = at(&events, &["records"]).as_array().and_then(|a| a.first()).cloned();
        if one && let Some(e) = event {
            anyhow::ensure!(at(&e, &["service"]) == "kiln.ci" && at(&e, &["host"]) == "r1" && at(&e, &["attributes", "job"]) == "hello", "the event: {e}");
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(120), "the job's metrics and event never came: {jobs} {events}\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(500));
    }
    let d = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": "max:kiln.ci.job.duration{repo:e2e,job:hello}", "from": crate_now() - 600_000, "to": crate_now() + 60_000, "step": 60_000 })))?;
    anyhow::ensure!(at(&d, &["series"]).as_array().is_some_and(|s| !s.is_empty()), "the job's duration: {d}");
    // kiln's registry, as Prometheus scraped it (once, as the server started: the families it had
    // then), with the families observe reports itself left out.
    let names = f.call("GET", "/api/observe/v1/metrics?match=kiln_*", None)?;
    let listed = |n: &str| at(&names, &["metrics"]).as_array().is_some_and(|a| a.iter().any(|x| x == n));
    anyhow::ensure!(listed("kiln_runs_started_total") && listed("kiln_job_duration_seconds_bucket") && !listed("kiln_agent_cpu_percent"), "the registry's families: {names}");
    Ok(())
}

/// What a mock service received: each request's path, headers (lowercased names) and body.
#[derive(Debug, Clone)]
struct Got {
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Got {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// A service for monitors to notify, on a port of its own: it takes every POST, answers 200 (or,
/// for the first `refuse` requests to a path, 429 with `Retry-After: 1`), and keeps what came.
struct Mock {
    port: u16,
    got: std::sync::Arc<std::sync::Mutex<Vec<Got>>>,
}

impl Mock {
    fn start(refuse: &[(&str, usize)]) -> anyhow::Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let got: std::sync::Arc<std::sync::Mutex<Vec<Got>>> = std::sync::Arc::default();
        let refused: std::sync::Arc<std::sync::Mutex<BTreeMap<String, usize>>> = std::sync::Arc::new(std::sync::Mutex::new(refuse.iter().map(|(p, n)| ((*p).to_owned(), *n)).collect()));
        let g = got.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let (g, refused) = (g.clone(), refused.clone());
                std::thread::spawn(move || {
                    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    // Headers, then the body by its length.
                    let head_end = loop {
                        let Ok(n) = s.read(&mut chunk) else { return };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(chunk.get(..n).unwrap_or(&[]));
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i;
                        }
                    };
                    let head = String::from_utf8_lossy(buf.get(..head_end).unwrap_or(&[])).into_owned();
                    let mut lines = head.lines();
                    let path = lines.next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("").to_owned();
                    let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned())).collect();
                    let len: usize = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
                    let mut body = buf.get(head_end + 4..).unwrap_or(&[]).to_vec();
                    while body.len() < len {
                        let Ok(n) = s.read(&mut chunk) else { break };
                        if n == 0 {
                            break;
                        }
                        body.extend_from_slice(chunk.get(..n).unwrap_or(&[]));
                    }
                    let refuse = {
                        let mut r = refused.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        match r.get_mut(&path) {
                            Some(n) if *n > 0 => {
                                *n -= 1;
                                true
                            }
                            _ => false,
                        }
                    };
                    g.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(Got { path, headers, body });
                    let answer = if refuse { "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n" } else { "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok" };
                    let _ = s.write_all(answer.as_bytes());
                });
            }
        });
        Ok(Self { port, got })
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn at(&self, path: &str) -> Vec<Got> {
        self.got.lock().unwrap_or_else(std::sync::PoisonError::into_inner).iter().filter(|g| g.path == path).cloned().collect()
    }
}

#[test]
#[ignore = "starts a kiln server as a process, and services for it to notify"]
fn a_monitor_alerts_every_integration_it_names_recovers_and_keeps_quiet_muted() -> anyhow::Result<()> {
    let f = Fleet::new()?;
    f.until(Duration::from_secs(30), "the server leads", |s| at(s, &["leader"]).as_u64() == Some(1))?;
    // The webhook refuses its first delivery (429, Retry-After 1): it is tried again.
    let mock = Mock::start(&[("/hook", 1)])?;
    for (name, body) in [
        ("ops", json!({ "kind": "slack", "url": mock.url("/slack") })),
        ("ops-d", json!({ "kind": "discord", "url": mock.url("/discord") })),
        ("ops-t", json!({ "kind": "teams", "url": mock.url("/teams") })),
        ("oncall", json!({ "kind": "pagerduty", "routing_key": "R0UT1NGKEY", "url": mock.url("/pagerduty") })),
        ("deploys", json!({ "kind": "webhook", "url": mock.url("/hook"), "secret": "s3cret" })),
    ] {
        let (code, b) = f.request("PUT", &format!("/api/observe/v1/integrations/{name}"), Some(&body))?;
        anyhow::ensure!(code == 204, "integration {name}: {code} {}", String::from_utf8_lossy(&b));
    }
    let listed = f.call("GET", "/api/observe/v1/integrations", None)?;
    anyhow::ensure!(!listed.to_string().contains("R0UT1NGKEY") && !listed.to_string().contains("s3cret"), "secrets are never shown: {listed}");
    let bad = f.call("POST", "/api/observe/v1/monitors/validate", Some(&json!({ "query": "avg(last_5m):avg:x{*}" })))?;
    anyhow::ensure!(at(&bad, &["ok"]).as_bool() == Some(false), "a query without a comparison: {bad}");
    let m = f.call(
        "POST",
        "/api/observe/v1/monitors",
        Some(&json!({
            "name": "e2e temp high",
            "query": "last(last_1m):max:e2e.temp{*} by {host} > 100",
            "message": "{{#is_alert}}temp on {{host.name}} is {{value}} @slack-ops @discord-ops-d @teams-ops-t @pagerduty-oncall @webhook-deploys{{/is_alert}}{{#is_recovery}}{{host.name}} recovered @slack-ops @pagerduty-oncall @webhook-deploys{{/is_recovery}}",
            "options": { "every": 10, "thresholds": { "critical": 100 } },
            "tags": ["team:e2e"],
        })),
    )?;
    let id = at(&m, &["id"]).as_str().unwrap_or("").to_owned();
    let hot = |f: &Fleet, v: f64| -> anyhow::Result<()> {
        let now = crate_now();
        f.write("e2e.temp", &["a"], now - 50_000, 6, |_, _| v)
    };
    // Hot: every integration hears of it.
    hot(&f, 150.0)?;
    let start = Instant::now();
    while ["/slack", "/discord", "/teams", "/pagerduty", "/hook"].iter().any(|p| mock.at(p).is_empty()) {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "not every integration heard: {:?}\n{}", mock.got.lock().map(|g| g.iter().map(|x| x.path.clone()).collect::<Vec<_>>()).unwrap_or_default(), f.logs());
        std::thread::sleep(Duration::from_millis(250));
    }
    let slack = mock.at("/slack").first().map(Got::json).unwrap_or_default();
    anyhow::ensure!(at(&slack, &["text"]).as_str().is_some_and(|t| t.contains("[Triggered] e2e temp high on host:a") && t.contains("150")), "slack: {slack}");
    let discord = mock.at("/discord").first().map(Got::json).unwrap_or_default();
    let embed = at(&discord, &["embeds"]).as_array().and_then(|e| e.first()).cloned().unwrap_or_default();
    anyhow::ensure!(at(&embed, &["color"]).as_u64() == Some(0xE0_1E5A) && at(&embed, &["description"]).as_str().is_some_and(|d| d.contains("temp on a is 150") && !d.contains('@')), "discord: {discord}");
    let pd = mock.at("/pagerduty").first().map(Got::json).unwrap_or_default();
    anyhow::ensure!(at(&pd, &["event_action"]) == "trigger" && at(&pd, &["routing_key"]) == "R0UT1NGKEY" && at(&pd, &["payload", "severity"]) == "critical" && at(&pd, &["payload", "source"]) == "a", "pagerduty: {pd}");
    let dedup = at(&pd, &["dedup_key"]).as_str().unwrap_or("").to_owned();
    // The webhook: refused once, delivered on the second try, signed.
    let start = Instant::now();
    while mock.at("/hook").len() < 2 {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(20), "the webhook was not tried again after its 429");
        std::thread::sleep(Duration::from_millis(100));
    }
    let hook = mock.at("/hook").last().cloned().ok_or_else(|| anyhow::anyhow!("the webhook"))?;
    let want = format!("sha256={}", hmac_sha256_hex(b"s3cret", &hook.body));
    anyhow::ensure!(hook.header("x-kiln-signature") == Some(want.as_str()), "the signature: {:?} want {want}", hook.header("x-kiln-signature"));
    anyhow::ensure!(at(&hook.json(), &["notice", "state"]) == "ALERT");
    let state = f.call("GET", &format!("/api/observe/v1/monitors/{id}"), None)?;
    anyhow::ensure!(at(&state, &["state"]) == "ALERT", "the monitor: {state}");
    // Cool: it recovers, and PagerDuty's incident is resolved by the same key.
    hot(&f, 50.0)?;
    let start = Instant::now();
    while !mock.at("/pagerduty").iter().any(|g| at(&g.json(), &["event_action"]) == "resolve") {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "never recovered\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(250));
    }
    let resolve = mock.at("/pagerduty").into_iter().map(|g| g.json()).find(|v| at(v, &["event_action"]) == "resolve").unwrap_or_default();
    anyhow::ensure!(at(&resolve, &["dedup_key"]).as_str() == Some(dedup.as_str()), "resolve: {resolve}");
    anyhow::ensure!(mock.at("/slack").iter().any(|g| at(&g.json(), &["text"]).as_str().is_some_and(|t| t.contains("[Recovered]"))), "slack hears the recovery");
    anyhow::ensure!(mock.at("/discord").len() == 1, "discord is not named for recoveries");
    // Muted: it still changes state (an event), and says nothing.
    f.call("POST", &format!("/api/observe/v1/monitors/{id}/mute"), Some(&json!({})))?;
    let before = mock.got.lock().map(|g| g.len()).unwrap_or(0);
    hot(&f, 200.0)?;
    f.until(Duration::from_secs(60), "the muted monitor alerting", |_| f.call("GET", &format!("/api/observe/v1/monitors/{id}"), None).is_ok_and(|m| at(&m, &["state"]) == "ALERT"))?;
    std::thread::sleep(Duration::from_secs(3));
    anyhow::ensure!(mock.got.lock().map(|g| g.len()).unwrap_or(0) == before, "a muted monitor notified");
    let events = f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": format!("source:kiln.event @monitor_id:{id} @event.type:monitor.alert"), "from": crate_now() - 600_000, "to": crate_now() + 60_000, "limit": 10 })))?;
    let alerts: Vec<Value> = at(&events, &["records"]).as_array().cloned().unwrap_or_default();
    anyhow::ensure!(alerts.len() == 2 && alerts.iter().any(|e| at(e, &["attributes", "muted"]) == "true"), "two alerts, the second muted: {events}");
    let delivered = f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": "source:kiln.event @event.type:monitor.notification @result:delivered", "from": crate_now() - 600_000, "to": crate_now() + 60_000, "limit": 100 })))?;
    anyhow::ensure!(at(&delivered, &["records"]).as_array().is_some_and(|a| a.len() >= 8), "every delivery recorded: {delivered}");
    Ok(())
}

fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    use hmac::{KeyInit as _, Mac as _};
    match hmac::Hmac::<sha2::Sha256>::new_from_slice(key) {
        Ok(mut m) => {
            m.update(msg);
            hex::encode(m.finalize().into_bytes())
        }
        Err(_) => String::new(),
    }
}

#[test]
#[ignore = "starts a kiln server as a process"]
fn github_s_webhooks_and_posted_events_are_events_and_counts() -> anyhow::Result<()> {
    let f = Fleet::new()?;
    f.until(Duration::from_secs(30), "the server leads", |s| at(s, &["leader"]).as_u64() == Some(1))?;
    let now_s = crate_now() / 1000;
    let iso = |s: i64| time::OffsetDateTime::from_unix_timestamp(s).ok().and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()).unwrap_or_default();
    let push = json!({ "ref": "refs/heads/main", "compare": "https://github.com/acme/web/compare/a...b", "commits": [{}, {}, {}], "repository": { "full_name": "acme/web" }, "sender": { "login": "ada" } });
    let run = json!({ "action": "completed", "workflow_run": { "name": "ci", "conclusion": "failure", "head_branch": "main", "run_started_at": iso(now_s - 120), "updated_at": iso(now_s), "html_url": "https://github.com/acme/web/actions/runs/1" }, "repository": { "full_name": "acme/web" }, "sender": { "login": "ada" } });
    for (event, body) in [("push", &push), ("workflow_run", &run)] {
        let (code, b) = raw(f.http, "POST", "/api/hooks/github", &format!("X-GitHub-Event: {event}\r\nX-GitHub-Delivery: d-{event}\r\nContent-Type: application/json\r\n"), &serde_json::to_vec(body)?)?;
        anyhow::ensure!(code == 202, "the {event} webhook: {code} {}", String::from_utf8_lossy(&b));
    }
    f.call("POST", "/api/observe/v1/events", Some(&json!({ "title": "deploy of web 1.2.3", "text": "rolled out by ada", "tags": ["service:web", "env:prod"], "alert_type": "success", "source_type_name": "deployer" })))?;
    let start = Instant::now();
    loop {
        let found = f.call("POST", "/api/observe/v1/events/search", Some(&json!({ "query": "*", "from": crate_now() - 600_000, "to": crate_now() + 60_000, "limit": 20 })))?;
        let msgs: Vec<String> = at(&found, &["records"]).as_array().into_iter().flatten().filter_map(|r| at(r, &["message"]).as_str().map(str::to_owned)).collect();
        let has = |s: &str| msgs.iter().any(|m| m.contains(s));
        if has("ada pushed 3 commit(s) to acme/web main") && has("acme/web ci failure") && has("deploy of web 1.2.3") {
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(20), "events: {msgs:?}\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(200));
    }
    let failed = f.call("POST", "/api/observe/v1/events/search", Some(&json!({ "query": "service:github status:error @conclusion:failure", "from": crate_now() - 600_000, "to": crate_now() + 60_000, "limit": 5 })))?;
    anyhow::ensure!(at(&failed, &["records"]).as_array().is_some_and(|a| a.len() == 1), "the failed run is an error event: {failed}");
    let d = f.query("max:github.actions.run.duration{repo:acme/web} by {conclusion}", crate_now() - 600_000, crate_now() + 60_000)?;
    anyhow::ensure!(d.values().any(|v| v.contains(&Some(120.0))), "the run's duration: {d:?}");
    let agg = f.call("POST", "/api/observe/v1/logs/aggregate", Some(&json!({ "query": "source:kiln.event", "from": crate_now() - 600_000, "to": crate_now() + 60_000, "by": ["service"] })))?;
    let by: BTreeMap<String, f64> = at(&agg, &["groups"]).as_array().into_iter().flatten().map(|g| (at(g, &["group", "service"]).as_str().unwrap_or("").to_owned(), at(g, &["value"]).as_f64().unwrap_or(0.0))).collect();
    anyhow::ensure!(by.get("github") == Some(&2.0) && by.get("web") == Some(&1.0), "events counted by service: {by:?}");
    Ok(())
}

/// A runner whose clock is an hour ahead of the server's: the server measures it from the
/// runner's writes, says so (an event, and the machine's status), and drops the runner's own
/// figures an hour ahead (counted, not stored), while points the runner takes with true times are
/// stored. Started again with a true clock, it is said to agree again.
#[test]
#[ignore = "starts real processes"]
fn a_runner_whose_clock_is_an_hour_ahead_is_said_so_and_its_figures_from_the_future_are_not_kept() -> anyhow::Result<()> {
    let mut f = Fleet::new()?;
    f.start_runner_with("r1", &[("KILN_OBSERVE_CLOCK_SKEW_MS", "3600000"), ("KILN_OBSERVE_HOST_SECONDS", "2")])?;
    let skewed = f.until(Duration::from_secs(120), "r1's clock measured an hour ahead, its figures dropped as ahead", |s| {
        at(s, &["machines"]).as_array().into_iter().flatten().any(|m| {
            at(m, &["machine"]) == "r1"
                && at(m, &["clock_skew_ms"]).as_i64().is_some_and(|k| (3_590_000..=3_610_000).contains(&k))
                && at(m, &["status", "ahead"]).as_u64().unwrap_or(0) > 0
        })
    })?;
    let _ = skewed;
    // Said once, as an event: written as any record is, so searchable a moment after the status
    // shows the skew, not at once.
    let skew_events = |f: &Fleet| -> anyhow::Result<Vec<String>> {
        let now = crate_now();
        let said = f.call("POST", "/api/observe/v1/events/search", Some(&json!({ "query": "@event.type:observe.clock_skew", "from": now - 600_000, "to": now + 60_000, "limit": 10 })))?;
        Ok(at(&said, &["records"]).as_array().into_iter().flatten().filter_map(|r| at(r, &["message"]).as_str().map(str::to_owned)).collect())
    };
    let start = Instant::now();
    let mut messages = skew_events(&f)?;
    while messages.is_empty() && start.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(200));
        messages = skew_events(&f)?;
    }
    // Said once: a moment more, and no second one.
    std::thread::sleep(Duration::from_secs(2));
    let messages = skew_events(&f)?;
    anyhow::ensure!(messages.len() == 1 && messages.first().is_some_and(|m| m.contains("r1's clock is 1 h 0 min ahead")), "{messages:?}");
    // Points with true times, taken by the skewed runner, are stored.
    let now = crate_now();
    let r1 = *f.otlp.get("r1").ok_or_else(|| anyhow::anyhow!("r1's receiver"))?;
    let mut req = kiln_obs::otlp::pb::Writer::default();
    req.message(1, |rm| {
        rm.message(2, |sm| {
            sm.message(2, |m| {
                m.str(1, "e2e.true.time");
                m.message(5, |g| {
                    g.message(1, |dp| {
                        dp.fixed64(3, (now as u64) * 1_000_000);
                        dp.double(4, 7.0);
                    });
                });
            });
        });
    });
    let (code, _) = raw(r1, "POST", "/v1/metrics", "Content-Type: application/x-protobuf\r\n", &req.buf)?;
    anyhow::ensure!(code == 200, "{code}");
    let start = Instant::now();
    loop {
        let q = f.query("sum:e2e.true.time{*}", now - 60_000, now + 60_000)?;
        if q.values().any(|v| v.contains(&Some(7.0))) {
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "a point with a true time never stored: {q:?}\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(200));
    }
    // Its clock set right (started again without the skew): it agrees again, and is said to.
    f.kill_runner("r1");
    f.start_runner("r1")?;
    f.until(Duration::from_secs(120), "r1's clock agreeing again", |s| {
        at(s, &["machines"]).as_array().into_iter().flatten().any(|m| at(m, &["machine"]) == "r1" && at(m, &["clock_skew_ms"]).as_i64().is_some_and(|k| k.abs() < 5_000))
    })?;
    let start = Instant::now();
    loop {
        let messages = skew_events(&f)?;
        if messages.iter().any(|m| m.contains("r1's clock agrees with the server's again")) {
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "{messages:?}");
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// Ten days of a daily cycle every five minutes, two hosts, b's last point a spike: `anomalies()`
/// bands both from their history, a's points inside, b's spike past; an anomaly monitor (the share
/// of its window past the band) alerts b's group alone, and says so to a webhook.
#[test]
#[ignore = "starts a kiln server as a process, and a service for it to notify"]
fn an_anomaly_is_banded_from_its_history_and_its_monitor_alerts_the_group_it_is_in() -> anyhow::Result<()> {
    let f = Fleet::new()?;
    f.until(Duration::from_secs(30), "the server leads", |s| at(s, &["leader"]).as_u64() == Some(1))?;
    let mock = Mock::start(&[])?;
    let (code, _) = f.request("PUT", "/api/observe/v1/integrations/anomalies", Some(&json!({ "kind": "webhook", "url": mock.url("/anomaly") })))?;
    anyhow::ensure!(code == 204);
    let end = crate_now().div_euclid(300_000) * 300_000;
    let n: i64 = 288 * 10;
    let mut seed = 21u64;
    let mut noise = move || {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        ((seed >> 11) as f64 / (1u64 << 53) as f64) - 0.5
    };
    let mut series = Vec::new();
    for host in ["a", "b"] {
        let points: Vec<Value> = (0..n)
            .map(|i| {
                let t = end - (n - i) * 300_000;
                let v = 50.0 + 20.0 * (t as f64 / 86_400_000.0 * std::f64::consts::TAU).sin() + noise();
                json!([t as f64 / 1000.0, if host == "b" && i == n - 1 { v + 60.0 } else { v }])
            })
            .collect();
        series.push(json!({ "metric": "e2e.cycle", "type": "gauge", "tags": [format!("host:{host}")], "points": points }));
    }
    f.call("POST", "/api/observe/v1/series", Some(&json!({ "series": series })))?;
    let spike = end - 300_000;
    let band = |k: f64| json!({ "query": format!("anomalies(avg:e2e.cycle{{*}} by {{host}}, 'robust', {k})"), "from": end - 6 * 3_600_000, "to": end, "step": 300_000 });
    // At 3 deviations, the series' own 1-in-500 error: of 143 ordinary points, a few at the most
    // past it (on average 0.3).
    let start = Instant::now();
    let answer = loop {
        let a = f.call("POST", "/api/observe/v1/query", Some(&band(3.0)))?;
        let ready = at(&a, &["series"]).as_array().is_some_and(|s| s.len() == 2 && s.iter().all(|x| at(x, &["values"]).as_array().is_some_and(|v| v.iter().filter(|p| p.is_number()).count() >= 70)));
        if ready {
            break a;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "the points were not read: {a}\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(250));
    };
    let past = |a: &Value| -> Vec<(String, i64)> {
        let (from, step) = (at(a, &["grid", "from"]).as_i64().unwrap_or(0), at(a, &["grid", "step"]).as_i64().unwrap_or(300_000));
        let mut out = Vec::new();
        for s in at(a, &["series"]).as_array().into_iter().flatten() {
            let b = at(s, &["bounds"]);
            for (i, v) in at(s, &["values"]).as_array().into_iter().flatten().enumerate() {
                let (Some(v), Some(lo), Some(hi)) = (v.as_f64(), at(b, &["lower"]).get(i).and_then(Value::as_f64), at(b, &["upper"]).get(i).and_then(Value::as_f64)) else { continue };
                if v < lo || v > hi {
                    out.push((at(s, &["tags", "host"]).as_str().unwrap_or("").to_owned(), from + step * i as i64));
                }
            }
        }
        out
    };
    let at3 = past(&answer);
    anyhow::ensure!(at3.contains(&("b".to_owned(), spike)) && at3.len() <= 4, "at 3 deviations: the spike, and few others: {at3:?}");
    // At 5, every ordinary point is inside, and the spike still past.
    let answer = f.call("POST", "/api/observe/v1/query", Some(&band(5.0)))?;
    anyhow::ensure!(past(&answer) == vec![("b".to_owned(), spike)], "at 5 deviations, the spike alone: {:?}", past(&answer));
    let from = at(&answer, &["grid", "from"]).as_i64().unwrap_or(0);
    let step = at(&answer, &["grid", "step"]).as_i64().unwrap_or(300_000);
    for s in at(&answer, &["series"]).as_array().into_iter().flatten() {
        let host = at(s, &["tags", "host"]).as_str().unwrap_or("").to_owned();
        let b = at(s, &["bounds"]);
        anyhow::ensure!(at(b, &["algorithm"]) == "robust" && at(b, &["k"]).as_f64() == Some(5.0), "{b}");
        let values = at(s, &["values"]).as_array().cloned().unwrap_or_default();
        for (i, v) in values.iter().enumerate() {
            let Some(v) = v.as_f64() else { continue };
            let (lo, hi) = (at(b, &["lower"]).get(i).and_then(Value::as_f64), at(b, &["upper"]).get(i).and_then(Value::as_f64));
            let (Some(lo), Some(hi)) = (lo, hi) else { anyhow::bail!("{host} bucket {i} has no band: {b}") };
            let t = from + step * i as i64;
            if host == "b" && t == spike {
                anyhow::ensure!(v > hi, "b's spike {v} is past {hi}");
            } else {
                anyhow::ensure!(lo <= v && v <= hi, "{host} at {t}: {v} in [{lo}, {hi}]");
            }
        }
    }
    // The monitor: the share of the last 15 minutes past the band, by host.
    let m = f.call(
        "POST",
        "/api/observe/v1/monitors",
        Some(&json!({
            "name": "e2e cycle anomalous",
            "query": "avg(last_15m):anomalies(avg:e2e.cycle{*} by {host}, 'robust', 5) >= 0.3",
            "message": "{{host.name}} is anomalous @webhook-anomalies",
            "options": { "every": 10, "thresholds": { "critical": 0.3 } },
        })),
    )?;
    let id = at(&m, &["id"]).as_str().unwrap_or("").to_owned();
    let start = Instant::now();
    let state = loop {
        let s = f.call("GET", &format!("/api/observe/v1/monitors/{id}"), None)?;
        if at(&s, &["state"]) == "ALERT" {
            break s;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "the anomaly monitor never alerted: {s}\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(250));
    };
    let groups = at(&state, &["groups"]).as_array().cloned().unwrap_or_default();
    let alerting: Vec<String> = groups.iter().filter(|g| at(g, &["state"]) == "ALERT").map(|g| g.to_string()).collect();
    anyhow::ensure!(alerting.len() == 1 && alerting.iter().all(|g| g.contains("host:b") || g.contains("\"b\"")), "b alone alerts: {state}");
    let start = Instant::now();
    while mock.at("/anomaly").is_empty() {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "the webhook heard nothing");
        std::thread::sleep(Duration::from_millis(200));
    }
    let notice = mock.at("/anomaly").first().map(Got::json).unwrap_or_default();
    anyhow::ensure!(at(&notice, &["notice", "state"]) == "ALERT" && at(&notice, &["notice", "message"]).as_str().is_some_and(|m| m.contains("b is anomalous")), "{notice}");
    Ok(())
}

/// Two monitors grouped by host, one alerting on web01–05 and the other on web04–09, and a
/// composite of both (`a && b`, Datadog's): it alerts the hosts both alert on, web04 and web05, and
/// no other, and a webhook hears of those two alone; web04 recovering in one of them recovers it in
/// the composite, and is said; a monitor the composite reads is not deleted while it does, and a
/// composite of a composite, or of a monitor that isn't there, is refused.
#[test]
#[ignore = "starts a kiln server as a process, and a service for it to notify"]
fn a_composite_alerts_the_groups_its_monitors_share_and_keeps_what_it_reads() -> anyhow::Result<()> {
    let f = Fleet::new()?;
    f.until(Duration::from_secs(30), "the server leads", |s| at(s, &["leader"]).as_u64() == Some(1))?;
    let mock = Mock::start(&[])?;
    let (code, _) = f.request("PUT", "/api/observe/v1/integrations/both", Some(&json!({ "kind": "webhook", "url": mock.url("/both") })))?;
    anyhow::ensure!(code == 204);
    let monitor = |name: &str, metric: &str| -> anyhow::Result<String> {
        let m = f.call("POST", "/api/observe/v1/monitors", Some(&json!({ "name": name, "query": format!("max(last_1m):max:{metric}{{*}} by {{host}} > 10"), "options": { "every": 10, "thresholds": { "critical": 10 } } })))?;
        Ok(at(&m, &["id"]).as_str().unwrap_or("").to_owned())
    };
    let a = monitor("e2e a", "e2e.ca")?;
    let b = monitor("e2e b", "e2e.cb")?;
    let (code, _) = f.request("POST", "/api/observe/v1/monitors", Some(&json!({ "name": "nowhere", "type": "composite", "query": format!("{a} && mnosuch") })))?;
    anyhow::ensure!(code == 400, "a composite of a monitor that isn't there: {code}");
    let c = f.call(
        "POST",
        "/api/observe/v1/monitors",
        Some(&json!({
            "name": "e2e both",
            "type": "composite",
            "query": format!("{a} && {b}"),
            "message": "{{#is_alert}}both on {{host.name}} @webhook-both{{/is_alert}}{{#is_recovery}}{{host.name}} recovered @webhook-both{{/is_recovery}}",
            "options": { "every": 10 },
        })),
    )?;
    let cid = at(&c, &["id"]).as_str().unwrap_or("").to_owned();
    anyhow::ensure!(at(&c, &["composite", "expression"]) == "a && b" && at(&c, &["composite", "named"]) == "e2e a && e2e b", "{c}");
    let (code, _) = f.request("POST", "/api/observe/v1/monitors", Some(&json!({ "name": "nested", "type": "composite", "query": format!("{cid} || {a}") })))?;
    anyhow::ensure!(code == 400, "a composite of a composite: {code}");
    let hosts = |r: std::ops::RangeInclusive<u32>| r.map(|i| format!("web{i:02}")).collect::<Vec<_>>();
    let write = |metric: &str, hs: &[String], v: f64| -> anyhow::Result<()> {
        let refs: Vec<&str> = hs.iter().map(String::as_str).collect();
        f.write(metric, &refs, crate_now() - 30_000, 4, |_, _| v)
    };
    write("e2e.ca", &hosts(1..=5), 20.0)?;
    write("e2e.cb", &hosts(4..=9), 20.0)?;
    // A group's first tag's value (a group is a list of [key, value]).
    let host = |group: &Value| group.as_array().and_then(|g| g.first()).and_then(Value::as_array).and_then(|kv| kv.get(1)).and_then(Value::as_str).unwrap_or("").to_owned();
    let groups = |id: &str| -> BTreeMap<String, String> {
        let m = f.call("GET", &format!("/api/observe/v1/monitors/{id}"), None).unwrap_or_default();
        at(&m, &["groups"]).as_array().into_iter().flatten().map(|g| (host(at(g, &["group"])), at(g, &["state"]).as_str().unwrap_or("").to_owned())).collect()
    };
    let both: BTreeMap<String, String> = [("web04", "ALERT"), ("web05", "ALERT")].iter().map(|(h, s)| ((*h).to_owned(), (*s).to_owned())).collect();
    let start = Instant::now();
    while groups(&cid) != both {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "the composite's groups: {:?} (a: {:?}, b: {:?})\n{}", groups(&cid), groups(&a), groups(&b), f.logs());
        std::thread::sleep(Duration::from_millis(250));
    }
    let said = |mock: &Mock| -> Vec<(String, String)> {
        mock.at("/both")
            .iter()
            .map(|g| {
                let v = g.json();
                (at(&v, &["notice", "state"]).as_str().unwrap_or("").to_owned(), host(at(&v, &["notice", "group"])))
            })
            .collect()
    };
    let start = Instant::now();
    while said(&mock).len() < 2 {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "the webhook heard {:?}", said(&mock));
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_secs(2));
    let mut heard = said(&mock);
    heard.sort();
    anyhow::ensure!(heard == [("ALERT".to_owned(), "web04".to_owned()), ("ALERT".to_owned(), "web05".to_owned())], "the webhook heard {heard:?}");
    // web04 recovers in b: the composite's web04 recovers, and is said.
    write("e2e.cb", &hosts(4..=4), 0.0)?;
    let start = Instant::now();
    while groups(&cid).get("web04").map(String::as_str) != Some("OK") || said(&mock).len() < 3 {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "web04 never recovered: {:?}, heard {:?}", groups(&cid), said(&mock));
        std::thread::sleep(Duration::from_millis(250));
    }
    anyhow::ensure!(said(&mock).last() == Some(&("OK".to_owned(), "web04".to_owned())) && groups(&cid).get("web05").map(String::as_str) == Some("ALERT"), "heard {:?}, groups {:?}", said(&mock), groups(&cid));
    // A monitor the composite reads is kept; once the composite goes, it may go.
    let (code, body) = f.request("DELETE", &format!("/api/observe/v1/monitors/{a}"), None)?;
    anyhow::ensure!(code == 409 && String::from_utf8_lossy(&body).contains("e2e both"), "deleting what a composite reads: {code} {}", String::from_utf8_lossy(&body));
    let (code, _) = f.request("DELETE", &format!("/api/observe/v1/monitors/{cid}"), None)?;
    anyhow::ensure!(code == 204);
    let (code, _) = f.request("DELETE", &format!("/api/observe/v1/monitors/{a}"), None)?;
    anyhow::ensure!(code == 204);
    Ok(())
}

/// Two disks, one filling a percent an hour from 62 and one steady at 40, two days of a point every
/// ten minutes: the query API forecasts both past now (the filling one carried on, its band about
/// it), and Datadog's forecast monitor, `max(next_2d):forecast(..., 'linear', 2) >= 95`, alerts the
/// filling one alone, which a webhook hears of.
#[test]
#[ignore = "starts a kiln server as a process, and a service for it to notify"]
fn a_forecast_monitor_alerts_the_disk_that_will_fill_and_says_so() -> anyhow::Result<()> {
    let f = Fleet::new()?;
    f.until(Duration::from_secs(30), "the server leads", |s| at(s, &["leader"]).as_u64() == Some(1))?;
    let mock = Mock::start(&[])?;
    let (code, _) = f.request("PUT", "/api/observe/v1/integrations/disks", Some(&json!({ "kind": "webhook", "url": mock.url("/disks") })))?;
    anyhow::ensure!(code == 204);
    let now = crate_now() / 600_000 * 600_000;
    let n: i64 = 6 * 48;
    let mut seed = 31u64;
    let mut noise = move || {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        ((seed >> 11) as f64 / (1u64 << 53) as f64) - 0.5
    };
    let mut series = Vec::new();
    for (host, start, rate) in [("filling", 62.0, 1.0), ("steady", 40.0, 0.0)] {
        let points: Vec<Value> = (0..=n)
            .map(|i| {
                let t = now - (n - i) * 600_000;
                json!([t as f64 / 1000.0, start + rate * (t - now) as f64 / 3_600_000.0 + 0.3 * noise()])
            })
            .collect();
        series.push(json!({ "metric": "e2e.disk", "type": "gauge", "tags": [format!("host:{host}")], "points": points }));
    }
    f.call("POST", "/api/observe/v1/series", Some(&json!({ "series": series })))?;
    // The query API: up to now the disks themselves, past it their forecasts.
    let q = json!({ "query": "forecast(avg:e2e.disk{*} by {host}, 'linear', 2)", "from": now - 86_400_000, "to": now + 12 * 3_600_000, "step": 600_000 });
    let start = Instant::now();
    let answer = loop {
        let a = f.call("POST", "/api/observe/v1/query", Some(&q))?;
        if at(&a, &["series"]).as_array().is_some_and(|s| s.len() == 2 && s.iter().all(|x| at(x, &["bounds", "forecast_from"]).as_u64().is_some())) {
            break a;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "no forecast: {a}");
        std::thread::sleep(Duration::from_millis(250));
    };
    for s in at(&answer, &["series"]).as_array().into_iter().flatten() {
        let host = at(s, &["tags", "host"]).as_str().unwrap_or("").to_owned();
        let values: Vec<f64> = at(s, &["values"]).as_array().into_iter().flatten().map(|v| v.as_f64().unwrap_or(f64::NAN)).collect();
        let from = at(s, &["bounds", "forecast_from"]).as_u64().unwrap_or(0) as usize;
        let (last, end) = (values.get(from - 1).copied().unwrap_or(f64::NAN), values.last().copied().unwrap_or(f64::NAN));
        let upper = at(s, &["bounds", "upper"]).as_array().and_then(|u| u.last()).and_then(Value::as_f64).unwrap_or(f64::NAN);
        let lower = at(s, &["bounds", "lower"]).as_array().and_then(|u| u.last()).and_then(Value::as_f64).unwrap_or(f64::NAN);
        anyhow::ensure!(from > 0 && from < values.len(), "{host}: the forecast starts at {from} of {}", values.len());
        match host.as_str() {
            // Twelve hours on: 74, as the disk will be.
            "filling" => anyhow::ensure!((end - 74.0).abs() < 2.0 && lower < end && end < upper && (last - 62.0).abs() < 1.5, "filling: {last} now, {end} in [{lower}, {upper}] in 12 h"),
            _ => anyhow::ensure!((end - 40.0).abs() < 1.0 && lower < 40.0 && 40.0 < upper, "steady: {end} in [{lower}, {upper}]"),
        }
    }
    // The monitor: the filling disk alone.
    let m = f.call(
        "POST",
        "/api/observe/v1/monitors",
        Some(&json!({
            "name": "e2e disk fills",
            "query": "max(next_2d):forecast(avg:e2e.disk{*} by {host}, 'linear', 2) >= 95",
            "message": "{{#is_alert}}{{host.name}} will be full within two days @webhook-disks{{/is_alert}}",
            "options": { "every": 10, "thresholds": { "critical": 95 } },
        })),
    )?;
    let id = at(&m, &["id"]).as_str().unwrap_or("").to_owned();
    let check = f.call("POST", "/api/observe/v1/monitors/validate", Some(&json!({ "query": "max(next_2d):forecast(avg:e2e.disk{*} by {host}, 'linear', 2) >= 95" })))?;
    anyhow::ensure!(at(&check, &["kind"]) == "forecast" && at(&check, &["window_ms"]).as_i64() == Some(2 * 86_400_000), "{check}");
    let start = Instant::now();
    loop {
        let m = f.call("GET", &format!("/api/observe/v1/monitors/{id}"), None)?;
        let groups: Vec<(String, String)> = at(&m, &["groups"]).as_array().into_iter().flatten().map(|g| {
            let host = at(g, &["group"]).as_array().and_then(|kv| kv.first()).and_then(Value::as_array).and_then(|kv| kv.get(1)).and_then(Value::as_str).unwrap_or("").to_owned();
            (host, at(g, &["state"]).as_str().unwrap_or("").to_owned())
        }).collect();
        if groups.len() == 2 && groups.iter().any(|g| g == &("filling".to_owned(), "ALERT".to_owned())) && groups.iter().any(|g| g == &("steady".to_owned(), "OK".to_owned())) {
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "the monitor's groups: {groups:?}\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(250));
    }
    let start = Instant::now();
    while mock.at("/disks").is_empty() {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "the webhook heard nothing");
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_secs(2));
    let heard: Vec<Value> = mock.at("/disks").iter().map(Got::json).collect();
    let [one] = heard.as_slice() else { anyhow::bail!("the webhook heard {} notices: {heard:?}", heard.len()) };
    anyhow::ensure!(at(one, &["notice", "state"]) == "ALERT" && at(one, &["notice", "message"]).as_str().is_some_and(|m| m.contains("filling will be full")), "{one}");
    Ok(())
}

/// Who may do what with observe, as kiln's roles say: a viewer's token reads (a query, a search,
/// dashboards, monitors, the integrations' list with their secrets left out) and is refused (403)
/// writing anything; a developer's makes monitors and dashboards and posts data, and is refused
/// integrations, clouds and the store's operations; a maintainer's manages integrations. The OpenAPI
/// document says what each of observe's routes needs.
#[test]
#[ignore = "starts a kiln server as a process"]
fn observe_s_routes_are_each_role_s_as_its_capabilities_say() -> anyhow::Result<()> {
    let f = Fleet::new()?;
    f.until(Duration::from_secs(30), "the server leads", |s| at(s, &["leader"]).as_u64() == Some(1))?;
    let mint = |grant: &str| -> anyhow::Result<String> {
        let t = f.call("POST", "/api/tokens", Some(&json!({ "grants": [grant], "user": "owner", "note": format!("e2e {grant}") })))?;
        at(&t, &["token"]).as_str().map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token: {t}"))
    };
    let (viewer, developer, maintainer) = (mint("viewer")?, mint("developer")?, mint("maintainer")?);
    let ask = |token: &str, method: &str, path: &str, body: Option<Value>| -> anyhow::Result<u16> {
        let b = body.map(|b| serde_json::to_vec(&b)).transpose()?.unwrap_or_default();
        Ok(raw(f.http, method, path, &format!("Authorization: Bearer {token}\r\nContent-Type: application/json\r\n"), &b)?.0)
    };
    let now = crate_now();
    let query = json!({ "query": "avg:kiln.process.cpu_seconds{*}", "from": now - 600_000, "to": now });
    let search = json!({ "query": "*", "from": now - 600_000, "to": now });
    let monitor = json!({ "name": "e2e roles", "query": "avg(last_5m):avg:e2e.roles{*} > 10", "options": { "thresholds": { "critical": 10 } } });
    let hook = json!({ "kind": "webhook", "url": "http://127.0.0.1:9/hook" });
    let series = json!({ "series": [{ "metric": "e2e.roles", "type": "gauge", "points": [[(now / 1000) as f64, 1.0]] }] });
    let reads: [(&str, &str, Option<Value>); 7] = [
        ("GET", "/api/observe/v1/status", None),
        ("POST", "/api/observe/v1/query", Some(query.clone())),
        ("POST", "/api/observe/v1/logs/search", Some(search.clone())),
        ("GET", "/api/observe/v1/dashboards", None),
        ("GET", "/api/observe/v1/monitors", None),
        ("GET", "/api/observe/v1/integrations", None),
        ("POST", "/api/observe/v1/monitors/validate", Some(json!({ "query": "avg(last_5m):avg:x{*} > 1" }))),
    ];
    for (method, path, body) in reads {
        let code = ask(&viewer, method, path, body)?;
        anyhow::ensure!(code == 200, "a viewer reading {method} {path}: {code}");
    }
    for (method, path, body) in [
        ("POST", "/api/observe/v1/monitors", Some(monitor.clone())),
        ("POST", "/api/observe/v1/dashboards", Some(json!({ "title": "e2e", "widgets": [] }))),
        ("POST", "/api/observe/v1/series", Some(series.clone())),
        ("PUT", "/api/observe/v1/integrations/e2e", Some(hook.clone())),
        ("POST", "/api/observe/v1/compact", Some(json!({}))),
    ] {
        let code = ask(&viewer, method, path, body)?;
        anyhow::ensure!(code == 403, "a viewer writing {method} {path}: {code}");
    }
    for (method, path, body, want) in [
        ("POST", "/api/observe/v1/monitors", Some(monitor.clone()), 201),
        ("POST", "/api/observe/v1/dashboards", Some(json!({ "title": "e2e", "widgets": [] })), 201),
        ("POST", "/api/observe/v1/series", Some(series.clone()), 202),
        ("PUT", "/api/observe/v1/integrations/e2e", Some(hook.clone()), 403),
        ("PUT", "/api/observe/v1/clouds/e2e", Some(json!({ "kind": "aws", "region": "us-east-1" })), 403),
        ("POST", "/api/observe/v1/verify", Some(json!({})), 403),
    ] {
        let code = ask(&developer, method, path, body)?;
        anyhow::ensure!(code == want, "a developer's {method} {path}: {code}, not {want}");
    }
    anyhow::ensure!(ask(&maintainer, "PUT", "/api/observe/v1/integrations/e2e", Some(hook))? == 204, "a maintainer adds an integration");
    anyhow::ensure!(ask(&maintainer, "DELETE", "/api/observe/v1/integrations/e2e", None)? == 204, "and removes it");
    let doc = f.call("GET", "/api/openapi.json", None)?;
    let needs = |path: &str, method: &str| at(&doc, &["paths", path, method, "x-kiln-capability"]).as_str().unwrap_or("").to_owned();
    anyhow::ensure!(needs("/api/observe/v1/query", "post") == "observe.read" && needs("/api/observe/v1/compact", "post") == "observe.manage", "the OpenAPI document: {}", at(&doc, &["paths"]).as_object().map(|p| p.keys().filter(|k| k.contains("observe")).cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default());
    Ok(())
}

/// Five hosts' CPU over an hour, one running hot: `outliers()` says it alone is one (DBSCAN, MAD),
/// and Datadog's outlier monitor, `avg(last_30m):outliers(..., 'dbscan', 1) > 0`, alerts it alone
/// and a webhook hears of it.
#[test]
#[ignore = "starts a kiln server as a process, and a service for it to notify"]
fn an_outlier_monitor_alerts_the_host_unlike_its_peers() -> anyhow::Result<()> {
    let f = Fleet::new()?;
    f.until(Duration::from_secs(30), "the server leads", |s| at(s, &["leader"]).as_u64() == Some(1))?;
    let mock = Mock::start(&[])?;
    let (code, _) = f.request("PUT", "/api/observe/v1/integrations/peers", Some(&json!({ "kind": "webhook", "url": mock.url("/peers") })))?;
    anyhow::ensure!(code == 204);
    let now = crate_now() / 60_000 * 60_000;
    let mut seed = 41u64;
    let mut noise = move || {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        ((seed >> 11) as f64 / (1u64 << 53) as f64) - 0.5
    };
    let mut series = Vec::new();
    for (host, level) in [("web-1", 40.0), ("web-2", 42.0), ("web-3", 39.0), ("web-4", 41.0), ("web-5", 80.0)] {
        let points: Vec<Value> = (0..60).map(|i| json!([(now - (59 - i) * 60_000) as f64 / 1000.0, level + 4.0 * noise()])).collect();
        series.push(json!({ "metric": "e2e.cpu", "type": "gauge", "tags": [format!("host:{host}")], "points": points }));
    }
    f.call("POST", "/api/observe/v1/series", Some(&json!({ "series": series })))?;
    for algo in ["dbscan', 1", "mad', 3, 20"] {
        let q = json!({ "query": format!("outliers(avg:e2e.cpu{{*}} by {{host}}, '{algo})"), "from": now - 3_600_000, "to": now, "step": 60_000 });
        let start = Instant::now();
        loop {
            let a = f.call("POST", "/api/observe/v1/query", Some(&q))?;
            let flagged: Vec<String> = at(&a, &["series"]).as_array().into_iter().flatten().filter(|s| at(s, &["outlier"]).as_bool() == Some(true)).filter_map(|s| at(s, &["tags", "host"]).as_str().map(str::to_owned)).collect();
            if at(&a, &["series"]).as_array().is_some_and(|s| s.len() == 5) {
                anyhow::ensure!(flagged == ["web-5"], "{algo}: {flagged:?}");
                break;
            }
            anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "five series: {a}");
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    let m = f.call(
        "POST",
        "/api/observe/v1/monitors",
        Some(&json!({
            "name": "e2e unlike its peers",
            "query": "avg(last_30m):outliers(avg:e2e.cpu{*} by {host}, 'dbscan', 1) > 0",
            "message": "{{#is_alert}}{{host.name}} is unlike its peers @webhook-peers{{/is_alert}}",
            "options": { "every": 10, "thresholds": { "critical": 0 } },
        })),
    )?;
    let id = at(&m, &["id"]).as_str().unwrap_or("").to_owned();
    let start = Instant::now();
    loop {
        let m = f.call("GET", &format!("/api/observe/v1/monitors/{id}"), None)?;
        let states: BTreeMap<String, String> = at(&m, &["groups"])
            .as_array()
            .into_iter()
            .flatten()
            .map(|g| (at(g, &["group"]).as_array().and_then(|kv| kv.first()).and_then(Value::as_array).and_then(|kv| kv.get(1)).and_then(Value::as_str).unwrap_or("").to_owned(), at(g, &["state"]).as_str().unwrap_or("").to_owned()))
            .collect();
        if states.len() == 5 && states.get("web-5").map(String::as_str) == Some("ALERT") && states.iter().filter(|(h, _)| *h != "web-5").all(|(_, s)| s == "OK") {
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "the monitor's groups: {states:?}\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(250));
    }
    let start = Instant::now();
    while mock.at("/peers").is_empty() {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "the webhook heard nothing");
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_secs(2));
    let heard: Vec<Value> = mock.at("/peers").iter().map(Got::json).collect();
    let [one] = heard.as_slice() else { anyhow::bail!("the webhook heard {} notices: {heard:?}", heard.len()) };
    anyhow::ensure!(at(one, &["notice", "message"]).as_str().is_some_and(|m| m.contains("web-5 is unlike its peers")), "{one}");
    Ok(())
}

/// Calendar rollups and `weighted()` through the API: three days of a count of one an hour, summed
/// by UTC days begun at 9am and by New York's days (each a local midnight, 4am or 5am UTC, and as
/// many ones as the day has hours); and Datadog's pods, a minute's share each.
#[test]
#[ignore = "starts a kiln server as a process"]
fn calendar_rollups_and_weights_read_as_the_calendar_and_the_pods_say() -> anyhow::Result<()> {
    const H: i64 = 3_600_000;
    let f = Fleet::new()?;
    f.until(Duration::from_secs(30), "the server leads", |s| at(s, &["leader"]).as_u64() == Some(1))?;
    let now = crate_now() / H * H;
    let first = now - 72 * H;
    let points: Vec<Value> = (0..72).map(|i| json!([(first + i * H) as f64 / 1000.0, 1.0])).collect();
    // Datadog's pods: a 12, 16 or 18 for one, two or three 10 s samples of a past minute.
    let minute = (crate_now() / 60_000 - 10) * 60_000;
    let mut series = vec![json!({ "metric": "e2e.hits", "type": "count", "tags": [], "points": points })];
    for (i, (version, value, n)) in [("1", 12.0, 1), ("1", 12.0, 3), ("1", 12.0, 2), ("2", 12.0, 1), ("2", 16.0, 3), ("2", 18.0, 2)].into_iter().enumerate() {
        let pts: Vec<Value> = (0..n).map(|j| json!([(minute + j * 10_000) as f64 / 1000.0, value])).collect();
        series.push(json!({ "metric": "e2e.pod.uptime", "type": "gauge", "tags": [format!("version:{version}"), format!("app:app{i}")], "points": pts }));
    }
    f.call("POST", "/api/observe/v1/series", Some(&json!({ "series": series })))?;
    let ask = |q: &str, from: i64, to: i64, step: Option<i64>| -> anyhow::Result<Value> {
        let start = Instant::now();
        loop {
            let a = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": q, "from": from, "to": to, "step": step })))?;
            let total: f64 = at(&a, &["series"]).as_array().into_iter().flatten().flat_map(|s| at(s, &["values"]).as_array().cloned().unwrap_or_default()).filter_map(|v| v.as_f64()).sum();
            if total > 0.0 {
                return Ok(a);
            }
            anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "{q}: no data: {a}");
            std::thread::sleep(Duration::from_millis(250));
        }
    };
    let numbers = |a: &Value, path: &[&str]| -> Vec<f64> { at(a, path).as_array().into_iter().flatten().map(|v| v.as_f64().unwrap_or(f64::NAN)).collect() };
    // UTC days begun at 9am: every bucket's start a 9am, a day apart; the sum, every hour written.
    let a = ask("sum:e2e.hits{*}.rollup(sum, daily, 9am)", first, now, None)?;
    anyhow::ensure!(at(&a, &["grid", "calendar"]).as_str() == Some("daily"), "{a}");
    let times: Vec<i64> = numbers(&a, &["grid", "times"]).into_iter().map(|t| t as i64).collect();
    anyhow::ensure!(!times.is_empty() && times.iter().all(|t| t.rem_euclid(86_400_000) == 9 * H) && times.windows(2).all(|w| matches!(w, [a, b] if b - a == 86_400_000)), "{times:?}");
    let values = numbers(at(&a, &["series"]).get(0).unwrap_or(&Value::Null), &["values"]);
    anyhow::ensure!(values.iter().filter(|v| v.is_finite()).sum::<f64>() == 72.0, "{values:?}");
    // New York's days: a local midnight each (4am UTC in summer time, 5am in winter's), and each
    // whole day as many ones as its hours.
    let a = ask("sum:e2e.hits{*}.rollup(sum, daily, 'America/New_York')", first, now, None)?;
    let times: Vec<i64> = numbers(&a, &["grid", "times"]).into_iter().map(|t| t as i64).collect();
    let values = numbers(at(&a, &["series"]).get(0).unwrap_or(&Value::Null), &["values"]);
    anyhow::ensure!(times.iter().all(|t| [4 * H, 5 * H].contains(&t.rem_euclid(86_400_000))), "{times:?}");
    for (i, w) in times.windows(2).enumerate() {
        let &[start, end] = w else { continue };
        let hours = (end - start) / H;
        anyhow::ensure!([23, 24, 25].contains(&hours), "{times:?}");
        // A day wholly inside the three written: one an hour.
        if start >= first && end <= now {
            anyhow::ensure!(values.get(i) == Some(&(hours as f64)), "day {i}: {values:?} of {times:?}");
        }
    }
    anyhow::ensure!(values.iter().filter(|v| v.is_finite()).sum::<f64>() == 72.0, "{values:?}");
    // The pods: plain, 36 and 46; weighted, 12 and 16.
    let sums = |q: &str| -> anyhow::Result<Vec<f64>> {
        let a = ask(q, minute, minute + 60_000, Some(60_000))?;
        Ok(at(&a, &["series"]).as_array().into_iter().flatten().map(|s| at(s, &["values"]).get(0).and_then(Value::as_f64).unwrap_or(f64::NAN)).collect())
    };
    anyhow::ensure!(sums("sum:e2e.pod.uptime{*} by {version}")? == [36.0, 46.0]);
    let w = sums("sum:e2e.pod.uptime{*} by {version}.weighted()")?;
    anyhow::ensure!(matches!(w.as_slice(), [a, b] if (a - 12.0).abs() < 1e-9 && (b - 16.0).abs() < 1e-9), "{w:?}");
    // What the calendar refuses, it says.
    let (code, body) = f.request("POST", "/api/observe/v1/query", Some(&json!({ "query": "sum:e2e.hits{*}.rollup(sum, weekly, 'Mars/Olympus_Mons')", "from": first, "to": now })))?;
    let body = String::from_utf8_lossy(&body);
    anyhow::ensure!(code == 400 && body.contains("Mars/Olympus_Mons"), "{code} {body}");
    Ok(())
}

/// Distributions (docs/observe.md §4.4, §13): three hosts' latencies posted as Datadog's
/// distribution points, two more hosts' as OTLP exponential histograms (one delta; one cumulative,
/// exported twice). p50, p99 and p99.9 across all of them are within the buckets' relative error of
/// the exact percentiles, and each host's p99 of its own; the average and the count exact; and a
/// heatmap holds every value once.
#[test]
#[ignore = "starts a kiln server as a process"]
fn a_distribution_s_percentiles_merge_across_hosts_from_the_api_and_otlp() -> anyhow::Result<()> {
    let f = Fleet::new()?;
    f.until(Duration::from_secs(30), "the server leads", |s| at(s, &["leader"]).as_u64() == Some(1))?;
    let ingest = std::fs::read_to_string(f.root.path().join("server").join("observe").join("ingest.token"))?.trim().to_owned();
    // After the server began: a cumulative series begun now is new to its receiver.
    let begun_ns = (crate_now() as u64) * 1_000_000;
    // Every point in one bucket of the query's grid, its step's own, wholly past: the last ten
    // minutes' bucket before this one.
    const STEP: i64 = 600_000;
    let end = crate_now() / STEP * STEP;
    let (from, t) = (end - STEP, end - 60_000);
    let mut seed = 77u64;
    let mut uniform = move || {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        ((seed >> 11) as f64 / (1u64 << 53) as f64).max(1e-300)
    };
    let mut lognormal = |level: f64| -> f64 { ((-2.0 * uniform().ln()).sqrt() * (std::f64::consts::TAU * uniform()).cos() * 0.5 + level).exp() };
    let mut hosts: Vec<(&str, Vec<f64>)> = Vec::new();
    // Datadog's distribution points: hosts a, b, c.
    let mut series = Vec::new();
    for (h, level) in [("a", -4.0), ("b", -3.0), ("c", -2.5)] {
        let values: Vec<f64> = (0..2000).map(|_| lognormal(level)).collect();
        series.push(json!({ "metric": "e2e.lat", "host": h, "points": [[t as f64 / 1000.0, values]] }));
        hosts.push((h, values));
    }
    let posted = f.call("POST", "/api/observe/v1/distribution_points", Some(&json!({ "series": series })))?;
    anyhow::ensure!(at(&posted, &["values"]) == 6000, "{posted}");
    // OTLP: d's delta histogram; e's cumulative, exported twice (the second all the first's and more).
    let histogram = |host: &str, cumulative: bool, start_ns: u64, at_ms: i64, values: &[f64]| -> Value {
        let mut counts: BTreeMap<i64, u64> = BTreeMap::new();
        for &v in values {
            if let Some(kiln_obs::dist::Bucket::Pos(i)) = kiln_obs::dist::bucket_of(v, 5) {
                *counts.entry(i).or_default() += 1;
            }
        }
        let (lo, hi) = (counts.keys().next().copied().unwrap_or(0), counts.keys().next_back().copied().unwrap_or(0));
        let dense: Vec<String> = (lo..=hi).map(|i| counts.get(&i).copied().unwrap_or(0).to_string()).collect();
        let point = json!({
            "startTimeUnixNano": start_ns.to_string(), "timeUnixNano": (at_ms as u64 * 1_000_000).to_string(),
            "count": values.len().to_string(), "sum": values.iter().sum::<f64>(), "scale": 5, "zeroCount": "0",
            "positive": { "offset": lo, "bucketCounts": dense },
            "min": values.iter().copied().fold(f64::MAX, f64::min), "max": values.iter().copied().fold(f64::MIN, f64::max),
        });
        json!({ "resourceMetrics": [{ "resource": { "attributes": [{ "key": "host.name", "value": { "stringValue": host } }] },
            "scopeMetrics": [{ "metrics": [{ "name": "e2e.lat", "exponentialHistogram": { "aggregationTemporality": if cumulative { 2 } else { 1 }, "dataPoints": [point] } }] }] }] })
    };
    let send = |body: &Value| -> anyhow::Result<()> {
        let (code, answer) = raw(f.http, "POST", "/v1/metrics", &format!("Authorization: Bearer {ingest}\r\nContent-Type: application/json\r\n"), body.to_string().as_bytes())?;
        anyhow::ensure!(code == 200, "{code} {}", String::from_utf8_lossy(&answer));
        Ok(())
    };
    let d: Vec<f64> = (0..3000).map(|_| lognormal(-3.5)).collect();
    send(&histogram("d", false, 0, t, &d))?;
    let e1: Vec<f64> = (0..1000).map(|_| lognormal(-2.0)).collect();
    let e2: Vec<f64> = (0..1500).map(|_| lognormal(-2.0)).collect();
    send(&histogram("e", true, begun_ns, t - 30_000, &e1))?;
    let both: Vec<f64> = e1.iter().chain(&e2).copied().collect();
    send(&histogram("e", true, begun_ns, t, &both))?;
    hosts.push(("d", d));
    hosts.push(("e", both));
    let mut all: Vec<f64> = hosts.iter().flat_map(|(_, v)| v.iter().copied()).collect();
    all.sort_by(f64::total_cmp);
    let ask = |q: &str| -> anyhow::Result<Vec<(String, f64)>> {
        let a = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": q, "from": from, "to": end, "step": STEP })))?;
        Ok(at(&a, &["series"]).as_array().into_iter().flatten().map(|s| (at(s, &["tags", "host"]).as_str().unwrap_or("").to_owned(), s.get("values").and_then(|v| v.get(0)).and_then(Value::as_f64).unwrap_or(f64::NAN))).collect())
    };
    let start = Instant::now();
    loop {
        let n = ask("count:e2e.lat{*}")?.first().map_or(0.0, |x| x.1);
        if n == all.len() as f64 {
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "{n} of {} values counted\n{}", all.len(), f.logs());
        std::thread::sleep(Duration::from_millis(250));
    }
    let exact = |v: &[f64], q: f64| v.get((v.len().saturating_sub(1) as f64 * q).floor() as usize).copied().unwrap_or(f64::NAN);
    let bound = ((2f64).powi(-kiln_obs::dist::SCALE) * 0.5).exp2() - 1.0;
    for (q, p) in [("p50", 0.5), ("p99", 0.99), ("p99.9", 0.999)] {
        let got = ask(&format!("{q}:e2e.lat{{*}}"))?;
        let want = exact(&all, p);
        anyhow::ensure!(got.len() == 1 && got.first().is_some_and(|g| ((g.1 - want) / want).abs() <= bound), "{q}: {got:?} for {want} (within {bound})");
    }
    let by_host = ask("p99:e2e.lat{*} by {host}")?;
    for (h, values) in &mut hosts {
        values.sort_by(f64::total_cmp);
        let want = exact(values, 0.99);
        let got = by_host.iter().find(|(x, _)| x == h).map_or(f64::NAN, |x| x.1);
        anyhow::ensure!(((got - want) / want).abs() <= bound, "host {h}: {got} for {want}");
    }
    let mean = all.iter().sum::<f64>() / all.len() as f64;
    let avg = ask("avg:e2e.lat{*}")?.first().map_or(f64::NAN, |x| x.1);
    anyhow::ensure!(((avg - mean) / mean).abs() < 1e-9, "avg {avg} for {mean}");
    // A heatmap of it: every value once, in rows of its buckets.
    let h = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": "p50:e2e.lat{*}", "from": from, "to": end, "step": STEP, "heatmap": 40 })))?;
    let total: f64 = at(&h, &["heatmap", "counts"]).as_array().into_iter().flatten().flat_map(|row| row.as_array().cloned().unwrap_or_default()).filter_map(|c| c.as_f64()).sum();
    let rows = at(&h, &["heatmap", "rows"]);
    anyhow::ensure!(at(&h, &["heatmap", "distribution"]) == true && total == all.len() as f64 && rows.as_array().is_some_and(|r| r.len() <= 40), "{total} of {} in the heatmap: {rows}", all.len());
    Ok(())
}

/// SLOs (docs/observe.md §9.4): requests counted a minute for three hours, a thousandth failing but
/// in the last half hour, when half do; a ratio SLO of them and a time slice SLO of a latency; the
/// standing as the workbook reckons it; a burn-rate monitor of the SLO (the long window and the
/// short both past 14.4) alerting; an SLO a monitor reads not deleted until the monitor is.
#[test]
#[ignore = "starts a kiln server as a process"]
fn an_slo_s_standing_and_its_burn_rate_alert_are_the_workbook_s() -> anyhow::Result<()> {
    let f = Fleet::new()?;
    let base = "/api/observe/v1";
    // Three hours of minutes ending at the last five-minute edge, the last 70 of them bad: every
    // window the server reads ends at its own last whole step, a few minutes later, so the hour's
    // and the quarter's hold bad minutes alone, and the week every one.
    let end = now_ms().div_euclid(300_000) * 300_000;
    let minutes: Vec<i64> = (0..180).map(|i| end - 180 * 60_000 + i * 60_000).collect();
    let bad = |t: i64| t >= end - 70 * 60_000;
    let pts = |f: &dyn Fn(i64) -> f64| -> Vec<Value> { minutes.iter().map(|&t| json!([t as f64 / 1000.0, f(t)])).collect() };
    f.call(
        "POST",
        &format!("{base}/series"),
        Some(&json!({ "series": [
            { "metric": "slo.requests", "type": "count", "tags": ["status:ok"], "points": pts(&|t| if bad(t) { 50.0 } else { 99.9 }) },
            { "metric": "slo.requests", "type": "count", "tags": ["status:error"], "points": pts(&|t| if bad(t) { 50.0 } else { 0.1 }) },
            { "metric": "slo.latency.p99", "type": "gauge", "tags": ["service:web"], "points": pts(&|t| if bad(t) { 0.9 } else { 0.12 }) },
        ] })),
    )?;
    let ratio = f.call(
        "POST",
        &format!("{base}/slos"),
        Some(&json!({ "name": "requests", "type": "ratio", "good": "sum:slo.requests{status:ok}.as_count()", "total": "sum:slo.requests{*}.as_count()", "target": 99.0, "warning": 99.5, "timeframe": "7d", "tags": ["team:web"] })),
    )?;
    let id = at(&ratio, &["id"]).as_str().unwrap_or("").to_owned();
    // Every minute applied before it is reckoned.
    let start = Instant::now();
    let standing = loop {
        let s = f.call("GET", &format!("{base}/slos/{id}"), None)?;
        if at(&s, &["standing", "total"]).as_f64() == Some(18_000.0) {
            break s;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "{s:#}");
        std::thread::sleep(Duration::from_millis(250));
    };
    // 110 minutes of 0.1 failing and 70 of 50: the SLI, the budget left, the burn rates.
    let (failed, total) = (110.0 * 0.1 + 70.0 * 50.0, 18_000.0);
    let sli = (1.0 - failed / total) * 100.0;
    let st = at(&standing, &["standing"]);
    anyhow::ensure!(at(st, &["state"]) == "BREACHED" && (at(st, &["sli"]).as_f64().unwrap_or(0.0) - sli).abs() < 1e-9, "{sli} {st:#}");
    let left = 100.0 - (failed / total) / 0.01 * 100.0;
    anyhow::ensure!((at(st, &["budget_left"]).as_f64().unwrap_or(0.0) - left).abs() < 1e-6, "{left} {st:#}");
    anyhow::ensure!((at(st, &["burn_rates", "1h"]).as_f64().unwrap_or(0.0) - 50.0).abs() < 1e-9, "the hour, all bad: {st:#}");
    let all = failed / total / 0.01;
    anyhow::ensure!((at(st, &["burn_rates", "6h"]).as_f64().unwrap_or(0.0) - all).abs() < 1e-9, "{all} {st:#}");
    anyhow::ensure!(at(st, &["days"]).as_array().is_some_and(|d| !d.is_empty()), "{st:#}");
    // A time slice SLO: the five-minute slices whose p99 is under 300 ms.
    let slices = f.call("POST", &format!("{base}/slos"), Some(&json!({ "name": "latency", "type": "slices", "query": "max:slo.latency.p99{service:web}", "condition": "< 0.3", "slice_s": 300, "target": 99.0, "timeframe": "7d" })))?;
    let sid = at(&slices, &["id"]).as_str().unwrap_or("").to_owned();
    let s2 = f.call("GET", &format!("{base}/slos/{sid}"), None)?;
    let (met, all) = (at(&s2, &["standing", "good"]).as_f64().unwrap_or(0.0), at(&s2, &["standing", "total"]).as_f64().unwrap_or(0.0));
    anyhow::ensure!(all == 36.0 && met == 22.0, "180 minutes are 36 slices, the last fourteen not met: {met} of {all}");
    // The burn-rate alert: the hour's and the quarter hour's 50, both past 14.4.
    let m = f.call(
        "POST",
        &format!("{base}/monitors"),
        Some(&json!({ "name": "requests burning fast", "query": format!("slo(\"{id}\").burn_rate(\"1h\", \"15m\") > 14.4"), "message": "budget burning", "options": { "every": 10 } })),
    )?;
    let mid = at(&m, &["id"]).as_str().unwrap_or("").to_owned();
    let start = Instant::now();
    loop {
        let m = f.call("GET", &format!("{base}/monitors/{mid}"), None)?;
        if at(&m, &["state"]) == "ALERT" {
            let value = at(&m, &["groups"]).get(0).and_then(|g| g.get("value")).and_then(Value::as_f64).unwrap_or(0.0);
            anyhow::ensure!((value - 50.0).abs() < 1e-9, "the lesser of the two windows' rates: {value} {m:#}");
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "not alerting: {m:#}");
        std::thread::sleep(Duration::from_millis(500));
    }
    let shown = f.call("GET", &format!("{base}/slos/{id}"), None)?;
    anyhow::ensure!(at(&shown, &["alerts"]).as_array().is_some_and(|a| a.len() == 1 && a.first().is_some_and(|x| at(x, &["state"]) == "ALERT")), "{shown:#}");
    // An SLO a monitor reads is not deleted until the monitor is.
    let (code, _) = f.request("DELETE", &format!("{base}/slos/{id}"), None)?;
    anyhow::ensure!(code == 409, "{code}");
    anyhow::ensure!(f.request("DELETE", &format!("{base}/monitors/{mid}"), None)?.0 == 204);
    anyhow::ensure!(f.request("DELETE", &format!("{base}/slos/{id}"), None)?.0 == 204);
    let listed = f.call("GET", &format!("{base}/slos"), None)?;
    anyhow::ensure!(at(&listed, &["slos"]).as_array().is_some_and(|l| l.len() == 1), "{listed:#}");
    Ok(())
}

/// Whether `v` is what `schema` says, `doc` holding the schemas it names: OpenAPI 3.1's JSON Schema
/// as far as the document uses it (`$ref`, `type`, `properties`, `required`, `items`, `enum`,
/// `anyOf`, `oneOf`, `allOf`, `additionalProperties`); where it is not, the path to what is not.
fn conforms(doc: &Value, schema: &Value, v: &Value, at: &str) -> Result<(), String> {
    if let Some(r) = schema.get("$ref").and_then(Value::as_str) {
        let target = doc.pointer(r.trim_start_matches('#')).ok_or_else(|| format!("{at}: no schema {r}"))?;
        return conforms(doc, target, v, at);
    }
    for (key, all) in [("allOf", true), ("anyOf", false), ("oneOf", false)] {
        if let Some(list) = schema.get(key).and_then(Value::as_array) {
            let tried: Vec<Result<(), String>> = list.iter().map(|s| conforms(doc, s, v, at)).collect();
            let ok = if all { tried.iter().all(Result::is_ok) } else { tried.iter().any(Result::is_ok) };
            if !ok {
                return Err(tried.into_iter().filter_map(Result::err).collect::<Vec<_>>().join(" | "));
            }
        }
    }
    if let Some(t) = schema.get("type").and_then(Value::as_str) {
        let fits = match t {
            "string" => v.is_string(),
            "integer" => v.is_i64() || v.is_u64(),
            "number" => v.is_number(),
            "boolean" => v.is_boolean(),
            "array" => v.is_array(),
            "object" => v.is_object(),
            "null" => v.is_null(),
            _ => true,
        };
        if !fits {
            return Err(format!("{at}: {v} is not {t}"));
        }
    }
    if let (Some(allowed), Some(s)) = (schema.get("enum").and_then(Value::as_array), v.as_str())
        && !allowed.iter().any(|a| a == s)
    {
        return Err(format!("{at}: {s:?} is none of {allowed:?}"));
    }
    if let Some(o) = v.as_object() {
        for r in schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if !o.contains_key(r) {
                return Err(format!("{at}: no {r} in {}", Value::Object(o.clone())));
            }
        }
        let props = schema.get("properties").and_then(Value::as_object);
        for (k, x) in o {
            match (props.and_then(|p| p.get(k)), schema.get("additionalProperties")) {
                (Some(s), _) => conforms(doc, s, x, &format!("{at}.{k}"))?,
                (None, Some(s)) if s.is_object() => conforms(doc, s, x, &format!("{at}.{k}"))?,
                _ => {}
            }
        }
    }
    if let (Some(items), Some(a)) = (schema.get("items"), v.as_array()) {
        for (i, x) in a.iter().enumerate() {
            conforms(doc, items, x, &format!("{at}[{i}]"))?;
        }
    }
    Ok(())
}

/// Observe's OpenAPI document (`/api/observe/v1/openapi.json`) held to what the routes answer: each
/// route that answers without a service of its own outside (the clouds' and integrations' tests
/// would reach one) asked as a client would, its answer checked against the schema the document
/// gives that route and status, and an error against the error's.
#[test]
#[ignore = "starts a kiln server as a process"]
fn the_api_answers_as_its_openapi_document_says() -> anyhow::Result<()> {
    let f = Fleet::new()?;
    let doc = f.call("GET", "/api/observe/v1/openapi.json", None)?;
    anyhow::ensure!(at(&doc, &["openapi"]) == "3.1.0" && at(&doc, &["paths"]).as_object().is_some_and(|p| p.len() > 30), "{doc:#}");
    // The check finds what is wrong: a field of another type, one missing, a value of no enum.
    let record = json!({ "$ref": "#/components/schemas/Record" });
    let good = json!({ "ts_ns": 1, "status": "info", "severity": 9, "service": "", "host": "", "source": "", "message": "m", "attributes": {} });
    anyhow::ensure!(conforms(&doc, &record, &good, "r").is_ok());
    for bad in [json!({ "ts_ns": "1" }), json!({ "status": "loud" }), json!({ "attributes": { "a": 1 } })] {
        let mut v = good.clone();
        if let (Some(o), Value::Object(b)) = (v.as_object_mut(), bad) {
            o.extend(b);
        }
        anyhow::ensure!(conforms(&doc, &record, &v, "r").is_err(), "{v} passed");
    }
    let mut missing = good.clone();
    missing.as_object_mut().map(|o| o.remove("message"));
    anyhow::ensure!(conforms(&doc, &record, &missing, "r").is_err(), "a missing field passed");
    let check = |method: &str, path: &str, route: &str, body: Option<Value>, want: u16| -> anyhow::Result<Value> {
        let (code, b) = f.request(method, path, body.as_ref())?;
        let v: Value = if b.is_empty() { Value::Null } else { serde_json::from_slice(&b).map_err(|e| anyhow::anyhow!("{method} {path}: {code} not JSON ({e}): {}", String::from_utf8_lossy(&b)))? };
        anyhow::ensure!(code == want, "{method} {path}: {code}, not {want}: {v}");
        let op = doc.pointer(&format!("/paths/{}/{}", route.replace('~', "~0").replace('/', "~1"), method.to_ascii_lowercase())).ok_or_else(|| anyhow::anyhow!("{method} {route} is not in the document"))?;
        let answer = op.pointer(&format!("/responses/{code}")).ok_or_else(|| anyhow::anyhow!("{method} {route}: {code} is not among its answers"))?;
        if let Some(schema) = answer.pointer("/content/application~1json/schema") {
            conforms(&doc, schema, &v, &format!("{method} {route} {code}")).map_err(anyhow::Error::msg)?;
        }
        Ok(v)
    };
    let base = "/api/observe/v1";
    let now = now_ms();
    // What there is to read: points, a distribution, records, an event.
    let points: Vec<Value> = (0..30).map(|i| json!([(now - 600_000 + i * 20_000) as f64 / 1000.0, i as f64])).collect();
    check("POST", &format!("{base}/series"), &format!("{base}/series"), Some(json!({ "series": [{ "metric": "api.check", "type": "gauge", "tags": ["env:prod"], "host": "h1", "points": points }] })), 202)?;
    check("POST", &format!("{base}/distribution_points"), &format!("{base}/distribution_points"), Some(json!({ "series": [{ "metric": "api.lat", "host": "h1", "points": [[(now - 60_000) as f64 / 1000.0, [0.1, 0.2, 0.4]]] }] })), 202)?;
    check("POST", &format!("{base}/logs"), &format!("{base}/logs"), Some(json!([{ "message": "api check failed", "status": "error", "service": "web", "hostname": "h1", "timestamp": now - 30_000, "attributes": { "route": "/pay" } }, { "message": "api check ok", "service": "web", "timestamp": now - 20_000 }])), 202)?;
    check("POST", &format!("{base}/events"), &format!("{base}/events"), Some(json!({ "title": "deployed", "text": "web 1.2", "tags": ["service:web"], "alert_type": "info" })), 202)?;
    // Applied before it is read.
    let start = Instant::now();
    while at(&f.call("POST", &format!("{base}/logs/search"), Some(&json!({ "query": "service:web", "from": now - 3_600_000, "to": now + 60_000 })))?, &["records"]).as_array().is_none_or(|r| r.len() < 2) {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "the records were not applied");
        std::thread::sleep(Duration::from_millis(200));
    }
    let range = json!({ "from": now - 3_600_000, "to": now + 60_000 });
    let with = |extra: Value| -> Value {
        let mut b = range.clone();
        if let (Some(o), Value::Object(e)) = (b.as_object_mut(), extra) {
            o.extend(e);
        }
        b
    };
    let q = check("POST", &format!("{base}/query"), &format!("{base}/query"), Some(with(json!({ "query": "avg:api.check{*} by {host}" }))), 200)?;
    anyhow::ensure!(at(&q, &["series"]).as_array().is_some_and(|s| s.len() == 1), "{q}");
    check("POST", &format!("{base}/query"), &format!("{base}/query"), Some(with(json!({ "query": "p50:api.lat{*}", "heatmap": 20 }))), 200)?;
    check("POST", &format!("{base}/query"), &format!("{base}/query"), Some(with(json!({ "query": "sum:api.check{*}.rollup(sum, daily, 'America/New_York')" }))), 200)?;
    check("GET", &format!("{base}/query?query=avg:api.check%7B*%7D&from={}&to={}", now - 3_600_000, now), &format!("{base}/query"), None, 200)?;
    check("POST", &format!("{base}/query"), &format!("{base}/query"), Some(with(json!({ "query": "avg:api.check{" }))), 400)?;
    check("GET", &format!("{base}/metrics?match=api.*"), &format!("{base}/metrics"), None, 200)?;
    check("GET", &format!("{base}/tags?key=host"), &format!("{base}/tags"), None, 200)?;
    let found = check("POST", &format!("{base}/logs/search"), &format!("{base}/logs/search"), Some(with(json!({ "query": "service:web -source:kiln.event" }))), 200)?;
    anyhow::ensure!(at(&found, &["records"]).as_array().is_some_and(|r| r.len() == 2), "{found}");
    check("POST", &format!("{base}/logs/aggregate"), &format!("{base}/logs/aggregate"), Some(with(json!({ "query": "*", "by": ["service", "status"] }))), 200)?;
    check("POST", &format!("{base}/logs/aggregate"), &format!("{base}/logs/aggregate"), Some(with(json!({ "query": "*", "by": ["service"], "step": 60_000 }))), 200)?;
    check("POST", &format!("{base}/logs/facets"), &format!("{base}/logs/facets"), Some(with(json!({ "facets": ["service", "status", "@route"] }))), 200)?;
    check("POST", &format!("{base}/logs/patterns"), &format!("{base}/logs/patterns"), Some(with(json!({ "query": "*" }))), 200)?;
    check("POST", &format!("{base}/events/search"), &format!("{base}/events/search"), Some(with(json!({ "query": "*" }))), 200)?;
    check("GET", &format!("{base}/status"), &format!("{base}/status"), None, 200)?;
    // Monitors: made, read, validated, muted, unmuted, listed, deleted.
    let m = check("POST", &format!("{base}/monitors"), &format!("{base}/monitors"), Some(json!({ "name": "api check high", "query": "avg(last_5m):avg:api.check{*} by {host} > 100", "message": "high", "tags": ["team:web"], "options": { "thresholds": { "critical": 100.0, "warning": 50.0 } } })), 201)?;
    let id = at(&m, &["id"]).as_str().unwrap_or("").to_owned();
    let mpath = format!("{base}/monitors/{{id}}");
    check("GET", &format!("{base}/monitors/{id}"), &mpath, None, 200)?;
    check("PUT", &format!("{base}/monitors/{id}"), &mpath, Some(json!({ "name": "api check high", "query": "avg(last_5m):avg:api.check{*} by {host} > 200" })), 200)?;
    check("POST", &format!("{base}/monitors/validate"), &format!("{base}/monitors/validate"), Some(json!({ "query": "logs(\"status:error\").last(\"5m\") > 10" })), 200)?;
    check("POST", &format!("{base}/monitors/{id}/mute"), &format!("{base}/monitors/{{id}}/mute"), Some(json!({ "until": now + 3_600_000 })), 200)?;
    check("POST", &format!("{base}/monitors/{id}/unmute"), &format!("{base}/monitors/{{id}}/unmute"), None, 200)?;
    check("GET", &format!("{base}/monitors"), &format!("{base}/monitors"), None, 200)?;
    check("GET", &format!("{base}/monitors/nope"), &mpath, None, 404)?;
    check("DELETE", &format!("{base}/monitors/{id}"), &mpath, None, 204)?;
    // Dashboards: made, read, changed, listed, deleted.
    let d = check("POST", &format!("{base}/dashboards"), &format!("{base}/dashboards"), Some(json!({ "title": "api", "widgets": [{ "kind": "timeseries", "title": "check", "queries": ["avg:api.check{*}"] }, { "kind": "note", "text": "# hi" }] })), 201)?;
    let did = at(&d, &["id"]).as_str().unwrap_or("").to_owned();
    let dpath = format!("{base}/dashboards/{{id}}");
    check("GET", &format!("{base}/dashboards/{did}"), &dpath, None, 200)?;
    check("PUT", &format!("{base}/dashboards/{did}"), &dpath, Some(json!({ "title": "api, changed", "widgets": [] })), 200)?;
    check("GET", &format!("{base}/dashboards"), &format!("{base}/dashboards"), None, 200)?;
    check("DELETE", &format!("{base}/dashboards/{did}"), &dpath, None, 204)?;
    // SLOs: made, read with their standing, changed, listed, deleted.
    let slo = check("POST", &format!("{base}/slos"), &format!("{base}/slos"), Some(json!({ "name": "api", "type": "ratio", "good": "sum:api.check{*}.as_count()", "total": "sum:api.check{*}.as_count()", "target": 99.0, "timeframe": "7d" })), 201)?;
    let sid = at(&slo, &["id"]).as_str().unwrap_or("").to_owned();
    let spath = format!("{base}/slos/{{id}}");
    check("GET", &format!("{base}/slos/{sid}"), &spath, None, 200)?;
    check("PUT", &format!("{base}/slos/{sid}"), &spath, Some(json!({ "name": "api", "type": "slices", "query": "max:api.check{*}", "condition": "< 100", "slice_s": 60, "target": 99.5, "timeframe": "30d" })), 200)?;
    check("GET", &format!("{base}/slos"), &format!("{base}/slos"), None, 200)?;
    check("POST", &format!("{base}/slos"), &format!("{base}/slos"), Some(json!({ "name": "bad", "type": "ratio", "target": 100.0, "timeframe": "7d" })), 400)?;
    check("DELETE", &format!("{base}/slos/{sid}"), &spath, None, 204)?;
    // Integrations, scrapes and clouds as they are listed and kept (their tests reach outside).
    check("PUT", &format!("{base}/integrations/hook"), &format!("{base}/integrations/{{name}}"), Some(json!({ "kind": "webhook", "url": "https://example.invalid/hook", "secret": "s" })), 204)?;
    let listed = check("GET", &format!("{base}/integrations"), &format!("{base}/integrations"), None, 200)?;
    anyhow::ensure!(!listed.to_string().contains("\"s\""), "a secret is not shown: {listed}");
    check("DELETE", &format!("{base}/integrations/hook"), &format!("{base}/integrations/{{name}}"), None, 204)?;
    check("PUT", &format!("{base}/scrapes/node"), &format!("{base}/scrapes/{{name}}"), Some(json!({ "url": "http://127.0.0.1:9/metrics", "every_s": 60 })), 204)?;
    check("GET", &format!("{base}/scrapes"), &format!("{base}/scrapes"), None, 200)?;
    check("DELETE", &format!("{base}/scrapes/node"), &format!("{base}/scrapes/{{name}}"), None, 204)?;
    check("PUT", &format!("{base}/clouds/aws1"), &format!("{base}/clouds/{{name}}"), Some(json!({ "kind": "aws", "regions": ["us-east-1"], "paused": true, "log_groups": ["/aws/lambda/*"] })), 204)?;
    check("GET", &format!("{base}/clouds"), &format!("{base}/clouds"), None, 200)?;
    check("DELETE", &format!("{base}/clouds/aws1"), &format!("{base}/clouds/{{name}}"), None, 204)?;
    check("DELETE", &format!("{base}/clouds/aws1"), &format!("{base}/clouds/{{name}}"), None, 404)?;
    Ok(())
}
