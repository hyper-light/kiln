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
use common::{Fleet, at, caught_up, raw};

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
