//! Prometheus's remote write, live (docs/observe.md §8.5, §13): a kiln server and a runner as
//! processes of their own, and Prometheus itself (in Docker) scraping its own `/metrics` every 2 s
//! and writing what it scrapes three ways: to the server over protocol 1.0 (the ingest token as its
//! bearer token), to the server over 2.0 (the token as basic auth's password), and to the runner's
//! local receiver (no token: loopback). Each way's series carry a label of its own (`rw`, by
//! Prometheus's write relabelling). What a user relies on: every sample Prometheus holds is in kiln
//! at its time with its value, to the bit, whichever way it came; its counters count (`.as_count()`
//! over its scrapes), its histograms answer percentiles; what is not a writer's is refused.
//!
//! It needs Docker: `cargo test -p kiln --test observe_remote_write -- --ignored --nocapture`.

mod common;

use std::collections::BTreeMap;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Fleet, at, caught_up, free_port, now_ms};
use serde_json::{Value, json};

const IMAGE: &str = "prom/prometheus:v3.7.0";

/// Prometheus in a container, its config `yml`, listening on `port` (on this machine's network, or
/// published to it: [`common::DockerNet`]).
struct Prometheus {
    name: String,
    port: u16,
}

impl Prometheus {
    fn start(dir: &std::path::Path, yml: &str, port: u16) -> anyhow::Result<Self> {
        let config = dir.join("prometheus.yml");
        std::fs::write(&config, yml)?;
        let name = format!("kiln-prom-{}-{port}", std::process::id());
        let net = common::DockerNet::get();
        let out = Command::new("docker")
            .args(["run", "-d", "--rm", "--name", &name])
            .args(net.args(&[port], &[]))
            .args(["-v", &format!("{}:/etc/prometheus/prometheus.yml:ro", config.display()), IMAGE])
            .args(["--config.file=/etc/prometheus/prometheus.yml", &format!("--web.listen-address={}:{port}", net.listen()), "--storage.tsdb.path=/prometheus", "--enable-feature=native-histograms"])
            .output()?;
        anyhow::ensure!(out.status.success(), "docker run: {}", String::from_utf8_lossy(&out.stderr));
        let p = Self { name, port };
        let start = Instant::now();
        while common::http(port, "GET", "/-/ready", "", None).map_or(true, |(code, _)| code != 200) {
            anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "Prometheus did not come up: {}", p.logs());
            std::thread::sleep(Duration::from_millis(250));
        }
        Ok(p)
    }

    /// A query of Prometheus's own API, its `data.result`.
    fn query(&self, q: &str, at_s: f64) -> anyhow::Result<Vec<Value>> {
        let path = format!("/api/v1/query?query={}&time={at_s:.3}", urlencode(q));
        let (code, body) = common::http(self.port, "GET", &path, "", None)?;
        anyhow::ensure!(code == 200, "{q}: {code} {body}");
        let v: Value = serde_json::from_str(&body)?;
        Ok(at(&v, &["data", "result"]).as_array().cloned().unwrap_or_default())
    }

    fn logs(&self) -> String {
        Command::new("docker").args(["logs", "--tail", "40", &self.name]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr)).unwrap_or_default()
    }
}

impl Drop for Prometheus {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.name]).output();
    }
}

fn urlencode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// A kiln query's values on a grid of `step` ms over `[from, to)`, its series by tag `key`.
fn by(f: &Fleet, q: &str, key: &str, from: i64, to: i64, step: i64) -> anyhow::Result<BTreeMap<String, Vec<Option<f64>>>> {
    let a = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": q, "from": from, "to": to, "step": step })))?;
    Ok(at(&a, &["series"]).as_array().into_iter().flatten().map(|s| (at(s, &["tags", key]).as_str().unwrap_or("").to_owned(), at(s, &["values"]).as_array().into_iter().flatten().map(Value::as_f64).collect())).collect())
}

#[test]
#[ignore = "needs Docker: runs Prometheus, and a kiln server and a runner as processes"]
fn prometheus_s_remote_write_is_stored_sample_for_sample() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut f = Fleet::new()?;
    f.start_runner("r1")?;
    f.until(Duration::from_secs(90), "r1 a learner, caught up", |s| caught_up(s, &["r1"]))?;
    let ingest = std::fs::read_to_string(f.root.path().join("server").join("observe").join("ingest.token"))?.trim().to_owned();
    let runner = f.otlp.get("r1").copied().unwrap_or(0);
    // What is not a writer's is refused: no token, a body that is not a request, a message of
    // neither protocol.
    let write = |headers: &str, body: &[u8]| common::raw(f.http, "POST", "/api/v1/write", headers, body);
    anyhow::ensure!(write("Content-Type: application/x-protobuf\r\nContent-Encoding: snappy\r\n", b"\x00")?.0 == 401);
    let auth = format!("Authorization: Bearer {ingest}\r\n");
    anyhow::ensure!(write(&format!("{auth}Content-Type: application/x-protobuf\r\nContent-Encoding: snappy\r\n"), b"\xff\xff\xff")?.0 == 400);
    anyhow::ensure!(write(&format!("{auth}Content-Type: application/json\r\n"), b"{}")?.0 == 415);

    let port = free_port()?;
    let yml = format!(
        r#"global:
  scrape_interval: 2s
scrape_configs:
  - job_name: prometheus
    static_configs:
      - targets: ["127.0.0.1:{port}"]
remote_write:
  - url: http://{host}:{http}/api/v1/write
    authorization:
      credentials: {ingest}
    queue_config: {{ batch_send_deadline: 1s }}
    metadata_config: {{ send_interval: 2s }}
    send_native_histograms: true
    write_relabel_configs: [{{ target_label: rw, replacement: v1 }}]
  - url: http://{host}:{http}/api/v1/write
    protobuf_message: io.prometheus.write.v2.Request
    basic_auth: {{ username: prometheus, password: {ingest} }}
    queue_config: {{ batch_send_deadline: 1s }}
    write_relabel_configs: [{{ target_label: rw, replacement: v2 }}]
  - url: http://{host}:{runner}/api/v1/write
    queue_config: {{ batch_send_deadline: 1s }}
    write_relabel_configs: [{{ target_label: rw, replacement: runner }}]
"#,
        http = f.http,
        host = common::DockerNet::get().host()
    );
    let t0 = now_ms();
    let prom = Prometheus::start(dir.path(), &yml, port)?;
    // Each way's `up`, once each has written.
    let start = Instant::now();
    loop {
        let up = by(&f, "max:up{job:prometheus} by {rw}", "rw", t0 - 10_000, now_ms() + 1000, 2000)?;
        if ["runner", "v1", "v2"].iter().all(|w| up.get(*w).is_some_and(|v| v.iter().flatten().any(|x| *x == 1.0))) {
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "up by way: {up:?}\n{}\n{}", prom.logs(), f.logs());
        std::thread::sleep(Duration::from_millis(500));
    }
    std::thread::sleep(Duration::from_secs(20));
    // Every sample Prometheus holds of a gauge over 20 s, in kiln at its time with its value.
    let end_s = (now_ms() - 5_000) as f64 / 1000.0;
    let held = prom.query("go_memstats_heap_alloc_bytes{job=\"prometheus\"}[20s]", end_s)?;
    let samples: Vec<(i64, f64)> = held
        .first()
        .and_then(|r| at(r, &["values"]).as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|p| Some(((p.get(0)?.as_f64()? * 1000.0).round() as i64, p.get(1)?.as_str()?.parse::<f64>().ok()?)))
        .collect();
    anyhow::ensure!(samples.len() >= 8, "Prometheus's samples: {held:?}");
    let (from, to) = (samples.first().map_or(0, |s| s.0) / 1000 * 1000, samples.last().map_or(0, |s| s.0) / 1000 * 1000 + 1000);
    for way in ["v1", "v2", "runner"] {
        let got = by(&f, &format!("max:go_memstats_heap_alloc_bytes{{job:prometheus,rw:{way}}} by {{rw}}"), "rw", from, to, 1000)?;
        let values = got.get(way).cloned().unwrap_or_default();
        for (ts, v) in &samples {
            let i = usize::try_from((ts - from) / 1000).unwrap_or(usize::MAX);
            let kept = values.get(i).copied().flatten();
            anyhow::ensure!(kept.is_some_and(|k| k.to_bits() == v.to_bits()), "{way}: at {ts} Prometheus holds {v}, kiln {kept:?} ({values:?})");
        }
    }
    eprintln!("  samples    {} of a gauge over 20 s, each in kiln to the bit, by 1.0, 2.0 and the runner", samples.len());
    // A counter counts: the /metrics handler's requests rise by one a scrape, and kiln counts over 20 s
    // what Prometheus holds of them: its samples' rise in the window and the one into its first
    // (kiln's bucket counts the rise from the sample before it), a sample at either edge apart. (Not
    // a scrape every 2 s: Docker Desktop's VM, on Windows, scraped 7 times in 20 s.)
    let window = (now_ms() - 25_000, now_ms() - 5_000);
    let scraped = prom.query("prometheus_http_requests_total{job=\"prometheus\",handler=\"/metrics\"}[20s]", window.1 as f64 / 1000.0)?;
    let held: Vec<f64> = scraped.first().and_then(|r| at(r, &["values"]).as_array().cloned()).unwrap_or_default().iter().filter_map(|p| p.get(1)?.as_str()?.parse().ok()).collect();
    anyhow::ensure!(held.len() >= 5, "Prometheus's samples of the /metrics handler's requests over 20 s: {scraped:?}");
    let rose = held.last().zip(held.first()).map_or(0.0, |(l, f)| l - f) + 1.0;
    for way in ["v1", "v2", "runner"] {
        let n: f64 = by(&f, &format!("sum:prometheus_http_requests_total{{handler:/metrics,rw:{way}}}.as_count()"), "rw", window.0, window.1, 20_000)?.values().flatten().flatten().sum();
        anyhow::ensure!((n - rose).abs() <= 1.0, "{way}: the /metrics handler counted {n} requests in 20 s, Prometheus's samples {rose} ({held:?})");
    }
    // A histogram answers percentiles from its buckets, as Prometheus's histogram_quantile does.
    let p99 = by(&f, "p99:prometheus_http_request_duration_seconds{handler:/metrics,rw:v1}", "rw", window.0, window.1, 20_000)?;
    let v = p99.values().flatten().flatten().copied().next();
    anyhow::ensure!(v.is_some_and(|x| x > 0.0 && x <= 120.0), "p99 of the /metrics handler's time: {p99:?}");
    // Its native histograms (Prometheus's own request durations, sent as they are scraped): kiln's
    // distributions, their buckets what rose between scrapes, read as percentiles.
    let start = Instant::now();
    loop {
        let native = by(&f, "sum:prometheus_http_request_duration_seconds.count{handler:/metrics,rw:v1} by {rw}.as_count()", "rw", window.0, now_ms(), 5_000)?;
        let p50 = by(&f, "p50:prometheus_http_request_duration_seconds{handler:/metrics,rw:v1} by {rw}", "rw", window.0, now_ms(), 5_000)?;
        let counted: f64 = native.values().flatten().flatten().sum();
        if counted > 0.0 && p50.values().flatten().flatten().any(|v| *v > 0.0 && *v < 10.0) {
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "the native histogram's count {native:?} and p50 {p50:?}\n{}", prom.logs());
        std::thread::sleep(Duration::from_secs(1));
    }
    // Its kinds as a scrape gives them: the counter a counter, the gauge a gauge.
    let summary = f.call("GET", "/api/observe/v1/metrics/summary/prometheus_http_requests_total", None)?;
    anyhow::ensure!(at(&summary, &["kinds"]).as_array().is_some_and(|k| k.iter().all(|x| at(x, &["kind"]) == "counter")), "{summary}");
    let summary = f.call("GET", "/api/observe/v1/metrics/summary/go_memstats_heap_alloc_bytes", None)?;
    anyhow::ensure!(at(&summary, &["kinds"]).as_array().is_some_and(|k| k.iter().all(|x| at(x, &["kind"]) == "gauge")), "{summary}");
    Ok(())
}
