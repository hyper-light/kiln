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
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_kiln");

/// The value at `path` in `v` (null where there is none), as `serde_json`'s indexing gives it.
fn at<'a>(v: &'a Value, path: &[&str]) -> &'a Value {
    static NULL: Value = Value::Null;
    path.iter().try_fold(v, |v, k| v.get(*k)).unwrap_or(&NULL)
}

fn free_port() -> anyhow::Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// One process of the fleet; killed (SIGKILL) when dropped.
struct Proc {
    child: Child,
    log: PathBuf,
}

impl Proc {
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.kill();
    }
}

fn tail(path: &Path, lines: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all.get(all.len().saturating_sub(lines)..).unwrap_or(&[]).join("\n")
}

struct Fleet {
    root: tempfile::TempDir,
    http: u16,
    admin: String,
    server: Option<Proc>,
    runners: BTreeMap<String, Option<Proc>>,
    /// Each runner's OTLP receiver's port.
    otlp: BTreeMap<String, u16>,
}

impl Fleet {
    fn new() -> anyhow::Result<Self> {
        let root = tempfile::tempdir()?;
        let (http, quic) = (free_port()?, free_port()?);
        let data = root.path().join("server");
        let cfg = format!(
            r#"
[server]
http_addr = "127.0.0.1:{http}"
quic_addr = "127.0.0.1:{quic}"
public_url = "http://127.0.0.1:{http}"
data_dir = "{data}"

[gitops]
auto_pipelines = false

[local_agent]
enabled = false

[discovery]
enabled = false

[kubernetes]
enabled = false

[cd]
enabled = false

[telemetry]
# Runners' host samples, and every node's own figures, once an hour: the test's writes are the
# writes (and the leader's cutoff, each minute).
sample_seconds = 3600

[observe]
enabled = true
"#,
            data = data.display()
        );
        std::fs::write(root.path().join("kiln.toml"), cfg)?;
        let mut f = Self { root, http, admin: String::new(), server: None, runners: BTreeMap::new(), otlp: BTreeMap::new() };
        f.start_server()?;
        f.admin = std::fs::read_to_string(data.join("admin.token"))?.trim().to_owned();
        Ok(f)
    }

    fn command(&self, home: &Path) -> anyhow::Result<Command> {
        std::fs::create_dir_all(home)?;
        let mut c = Command::new(BIN);
        // A clean environment and a home of its own: nothing of this machine's leaks in.
        c.env_clear().env("HOME", home).env("PATH", "/usr/local/bin:/usr/bin:/bin").env("LANG", "C.UTF-8").env("RUST_LOG", "info,kiln::observe=debug");
        Ok(c)
    }

    fn start_server(&mut self) -> anyhow::Result<()> {
        let log = self.root.path().join("server.log");
        let out = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
        let child = self.command(&self.root.path().join("home-server"))?.arg("--config").arg(self.root.path().join("kiln.toml")).arg("server").stdout(Stdio::from(out.try_clone()?)).stderr(Stdio::from(out)).spawn()?;
        self.server = Some(Proc { child, log });
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(60) {
            if self.get_raw("/healthz").is_ok_and(|(code, _)| code == 200) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        anyhow::bail!("the server did not come up:\n{}", self.logs())
    }

    fn start_runner(&mut self, name: &str) -> anyhow::Result<()> {
        let join = std::fs::read_to_string(self.root.path().join("server").join("join.token"))?.trim().to_owned();
        let dir = self.root.path().join(name);
        let log = self.root.path().join(format!("{name}.log"));
        let otlp = match self.otlp.get(name) {
            Some(&p) => p,
            None => free_port()?,
        };
        self.otlp.insert(name.to_owned(), otlp);
        let out = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
        let child = self
            .command(&dir.join("home"))?
            .args(["agent", "--server", &format!("http://127.0.0.1:{}", self.http), "--token", &join, "--name", name, "--slots", "1", "--no-terminal", "--no-reach"])
            .env("KILN_OTLP_ADDR", format!("127.0.0.1:{otlp}"))
            .arg("--state-dir")
            .arg(dir.join("state"))
            .arg("--work-dir")
            .arg(dir.join("work"))
            .stdout(Stdio::from(out.try_clone()?))
            .stderr(Stdio::from(out))
            .spawn()?;
        self.runners.insert(name.to_owned(), Some(Proc { child, log }));
        Ok(())
    }

    fn kill_runner(&mut self, name: &str) {
        if let Some(Some(mut p)) = self.runners.insert(name.to_owned(), None) {
            p.kill();
        }
    }

    fn kill_server(&mut self) {
        if let Some(mut p) = self.server.take() {
            p.kill();
        }
    }

    fn logs(&self) -> String {
        let mut out = String::new();
        if let Some(s) = &self.server {
            out.push_str(&format!("--- server\n{}\n", tail(&s.log, 40)));
        }
        for (name, p) in &self.runners {
            let log = p.as_ref().map_or_else(|| self.root.path().join(format!("{name}.log")), |p| p.log.clone());
            out.push_str(&format!("--- {name}\n{}\n", tail(&log, 40)));
        }
        out
    }

    /// A request to the server: its status and its body.
    fn request(&self, method: &str, path: &str, body: Option<&Value>) -> anyhow::Result<(u16, Vec<u8>)> {
        let body = body.map(serde_json::to_vec).transpose()?.unwrap_or_default();
        let auth = if self.admin.is_empty() { String::new() } else { format!("Authorization: Bearer {}\r\n", self.admin) };
        raw(self.http, method, path, &format!("{auth}Content-Type: application/json\r\n"), &body)
    }
}

/// A request to `127.0.0.1:port` with `headers` (each ending in CRLF) and `body`: its status and
/// its body, chunked or not.
fn raw(port: u16, method: &str, path: &str, headers: &str, body: &[u8]) -> anyhow::Result<(u16, Vec<u8>)> {
    {
        let mut s = TcpStream::connect(("127.0.0.1", port))?;
        s.set_read_timeout(Some(Duration::from_secs(30)))?;
        write!(s, "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n{headers}Content-Length: {}\r\n\r\n", body.len())?;
        s.write_all(body)?;
        let mut raw = Vec::new();
        s.read_to_end(&mut raw)?;
        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| anyhow::anyhow!("no header end"))?;
        let head = String::from_utf8_lossy(raw.get(..split).unwrap_or(&[])).into_owned();
        let code: u16 = head.split_whitespace().nth(1).ok_or_else(|| anyhow::anyhow!("no status"))?.parse()?;
        let mut rest = raw.get(split + 4..).unwrap_or(&[]).to_vec();
        if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
            let mut out = Vec::new();
            let mut at = rest.as_slice();
            loop {
                let line_end = at.windows(2).position(|w| w == b"\r\n").ok_or_else(|| anyhow::anyhow!("chunk size"))?;
                let size = usize::from_str_radix(String::from_utf8_lossy(at.get(..line_end).unwrap_or(&[])).trim(), 16)?;
                at = at.get(line_end + 2..).unwrap_or(&[]);
                if size == 0 {
                    break;
                }
                out.extend_from_slice(at.get(..size).ok_or_else(|| anyhow::anyhow!("chunk"))?);
                at = at.get(size + 2..).unwrap_or(&[]);
            }
            rest = out;
        }
        Ok((code, rest))
    }
}

impl Fleet {
    fn get_raw(&self, path: &str) -> anyhow::Result<(u16, Vec<u8>)> {
        self.request("GET", path, None)
    }

    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> anyhow::Result<Value> {
        let (code, b) = self.request(method, path, body)?;
        anyhow::ensure!((200..300).contains(&code), "{method} {path}: {code} {}", String::from_utf8_lossy(&b));
        Ok(serde_json::from_slice(&b)?)
    }

    fn status(&self) -> anyhow::Result<Value> {
        self.call("GET", "/api/observe/v1/status", None)
    }

    /// Waits until `done` holds of the group's status, or fails with the fleet's logs.
    fn until(&self, within: Duration, what: &str, done: impl Fn(&Value) -> bool) -> anyhow::Result<Value> {
        let start = Instant::now();
        let mut last = Value::Null;
        while start.elapsed() < within {
            if let Ok(s) = self.status() {
                if done(&s) {
                    return Ok(s);
                }
                last = s;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        anyhow::bail!("{what}: not within {within:?}; the status was {last:#}\n{}", self.logs())
    }

    /// Every replica's full digest at one index, equal: asked again until one index holds them all
    /// (the leader's cutoff entry may come between two of them).
    fn verify(&self, replicas: usize) -> anyhow::Result<(u64, u64)> {
        for _ in 0..30 {
            self.call("POST", "/api/observe/v1/verify", Some(&json!({})))?;
            std::thread::sleep(Duration::from_millis(1500));
            let s = self.status()?;
            let mut seen: Vec<(u64, u64)> = Vec::new();
            let pair = |v: &Value| match v.as_array().map(Vec::as_slice) {
                Some([at, d]) => Some((at.as_u64().unwrap_or(0), d.as_u64().unwrap_or(0))),
                _ => None,
            };
            seen.extend(pair(at(&s, &["verified"])));
            for m in at(&s, &["machines"]).as_array().into_iter().flatten() {
                seen.extend(pair(at(m, &["status", "verified"])));
            }
            let first = seen.first().copied().unwrap_or_default();
            if seen.len() == replicas && seen.iter().all(|x| x.0 == first.0) {
                anyhow::ensure!(seen.iter().all(|x| x.1 == first.1), "replicas at index {} hold different contents: {seen:?}\n{}", first.0, self.logs());
                return Ok(first);
            }
        }
        anyhow::bail!("the replicas' digests never met at one index\n{}", self.logs())
    }

    /// Points of series `metric{host:h}` for each host, every 10 s over `[t0, t0 + n·10 s)`.
    fn write(&self, metric: &str, hosts: &[&str], t0: i64, n: i64, value: impl Fn(&str, i64) -> f64) -> anyhow::Result<()> {
        let series: Vec<Value> = hosts.iter().map(|h| json!({ "metric": metric, "type": "gauge", "tags": [format!("host:{h}")], "points": (0..n).map(|i| json!([(t0 + i * 10_000) as f64 / 1000.0, value(h, i)])).collect::<Vec<_>>() })).collect();
        self.call("POST", "/api/observe/v1/series", Some(&json!({ "series": series })))?;
        Ok(())
    }

    /// The query's values, by host, on a 10 s grid.
    fn query(&self, q: &str, from: i64, to: i64) -> anyhow::Result<BTreeMap<String, Vec<Option<f64>>>> {
        let a = self.call("POST", "/api/observe/v1/query", Some(&json!({ "query": q, "from": from, "to": to, "step": 10_000 })))?;
        let mut out = BTreeMap::new();
        for s in at(&a, &["series"]).as_array().into_iter().flatten() {
            let host = at(s, &["tags", "host"]).as_str().unwrap_or("").to_owned();
            out.insert(host, at(s, &["values"]).as_array().into_iter().flatten().map(Value::as_f64).collect());
        }
        Ok(out)
    }

    fn members(&self, s: &Value) -> BTreeMap<String, u64> {
        at(s, &["machines"]).as_array().into_iter().flatten().map(|m| (at(m, &["machine"]).as_str().unwrap_or("").to_owned(), at(m, &["member"]).as_u64().unwrap_or(0))).collect()
    }
}

/// Whether every runner named is a learner, up, and has applied all the leader has.
fn caught_up(s: &Value, names: &[&str]) -> bool {
    let applied = at(s, &["applied"]).as_u64().unwrap_or(0);
    let learners: Vec<u64> = at(s, &["learners"]).as_array().into_iter().flatten().filter_map(Value::as_u64).collect();
    names.iter().all(|n| {
        at(s, &["machines"]).as_array().into_iter().flatten().any(|m| at(m, &["machine"]) == *n && at(m, &["up"]).as_bool() == Some(true) && learners.contains(&at(m, &["member"]).as_u64().unwrap_or(0)) && at(m, &["status", "applied"]).as_u64().unwrap_or(0) >= applied && at(m, &["status", "digest"]) == at(s, &["digest"]))
    })
}

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
