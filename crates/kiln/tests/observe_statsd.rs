//! DogStatsD, live (docs/observe.md §8.4, §13): a kiln server and a runner as processes of their
//! own, each opening the listener the server keeps for it, on real sockets. What a user relies on:
//! what an application sends a DogStatsD client comes out of kiln's queries as Datadog's Agent
//! would have written it (counts summed over their sample rates, a count's zeros after it stops,
//! gauges' last values, sets' distinct values, histograms' aggregates, distributions' percentiles,
//! a gauge's own time), over UDP and a Unix socket, from the server and a runner; events in the
//! event stream; service checks as records; and what does not read is counted, not taken.
//!
//! Datagrams are sent as the protocol spells them, and, when `KOBS_DOGSTATSD_PYTHON` names a
//! Python with Datadog's `datadog` package (or one is at
//! `~/.cache/claude-scratch/dsd-client/venv/bin/python`), by Datadog's own client too.
//!
//! `cargo test -p kiln --test observe_statsd -- --ignored --nocapture`
#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::net::UdpSocket;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{Fleet, at, caught_up, now_ms};
use serde_json::{Value, json};

fn free_udp_port() -> anyhow::Result<u16> {
    Ok(UdpSocket::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// Waits until a bucket of ten seconds has just begun, so that what is sent now falls in one (a
/// bucket's last gauge, its set, its histogram).
fn early_in_bucket() {
    while !(200..4_000).contains(&now_ms().rem_euclid(10_000)) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Each listener's state on each machine: listener → machine → state.
fn states(f: &Fleet) -> anyhow::Result<BTreeMap<String, BTreeMap<String, Value>>> {
    let all = f.call("GET", "/api/observe/v1/statsd", None)?;
    Ok(at(&all, &["statsd"])
        .as_array()
        .into_iter()
        .flatten()
        .map(|l| {
            let machines = at(l, &["machines"]).as_array().into_iter().flatten().map(|m| (at(m, &["machine"]).as_str().unwrap_or("").to_owned(), at(m, &["state"]).clone())).collect();
            (at(l, &["name"]).as_str().unwrap_or("").to_owned(), machines)
        })
        .collect())
}

/// A query's series over `[from, to)` on a 10 s grid, by the value of tag `key`: each one's values.
fn by(f: &Fleet, q: &str, key: &str, from: i64, to: i64) -> anyhow::Result<BTreeMap<String, Vec<Option<f64>>>> {
    let a = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": q, "from": from, "to": to, "step": 10_000 })))?;
    Ok(at(&a, &["series"]).as_array().into_iter().flatten().map(|s| (at(s, &["tags", key]).as_str().unwrap_or("").to_owned(), at(s, &["values"]).as_array().into_iter().flatten().map(Value::as_f64).collect())).collect())
}

/// A query's values summed over the range, all its series together.
fn total(f: &Fleet, q: &str, from: i64, to: i64) -> anyhow::Result<f64> {
    Ok(by(f, q, "host", from, to)?.values().flatten().flatten().sum())
}

/// A query's last value (its last point that has one), all series together: the largest of theirs.
fn last(f: &Fleet, q: &str, from: i64, to: i64) -> anyhow::Result<Option<f64>> {
    Ok(by(f, q, "host", from, to)?.values().filter_map(|v| v.iter().rev().find_map(|x| *x)).reduce(f64::max))
}

/// Waits until `q`'s total over the range is `want`.
fn until_total(f: &Fleet, q: &str, want: f64, from: i64, within: Duration) -> anyhow::Result<()> {
    let start = Instant::now();
    loop {
        let got = total(f, q, from, now_ms() + 60_000)?;
        if (got - want).abs() < 1e-9 {
            return Ok(());
        }
        anyhow::ensure!(start.elapsed() < within, "{q}: {got}, not {want}\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn records(f: &Fleet, query: &str, from: i64) -> anyhow::Result<Vec<Value>> {
    let found = f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": query, "from": from, "to": now_ms() + 60_000, "limit": 100 })))?;
    Ok(at(&found, &["records"]).as_array().cloned().unwrap_or_default())
}

/// The Python with Datadog's client, if there is one.
fn datadog_python() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("KOBS_DOGSTATSD_PYTHON") {
        return Some(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME")?;
    let p = Path::new(&home).join(".cache/claude-scratch/dsd-client/venv/bin/python");
    p.is_file().then_some(p)
}

/// What Datadog's client sends: `script` run (isolated: `-I`) by the Python given.
fn run_client(python: &Path, dir: &Path, script: &str, args: &[String]) -> anyhow::Result<()> {
    let path = dir.join("client.py");
    std::fs::write(&path, script)?;
    let out = std::process::Command::new(python).arg("-I").arg(&path).args(args).output()?;
    anyhow::ensure!(out.status.success(), "the client failed: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    Ok(())
}

const CLIENT: &str = r#"
import sys, time
from datadog.dogstatsd import DogStatsd

port, socket_path, backfill = int(sys.argv[1]), sys.argv[2], int(sys.argv[3])
udp = DogStatsd(host="127.0.0.1", port=port, disable_telemetry=True, constant_tags=["client:py"])
for _ in range(40):
    udp.increment("py.requests", tags=["route:/x"])
udp.increment("py.requests", value=5, tags=["route:/y"])
udp.decrement("py.inflight", tags=["route:/x"])
udp.gauge("py.queue", 3)
udp.gauge("py.queue", 9)
for user in ["ada", "bob", "ada", "cy"]:
    udp.set("py.users", user)
for ms in range(1, 101):
    udp.timing("py.latency", ms)
for v in [1.0, 2.0, 4.0, 8.0]:
    udp.distribution("py.size", v)
udp.gauge_with_timestamp("py.backfill", 77, backfill)
udp.event("py deploy", "rolled out\nv2", alert_type="success", tags=["service:web"])
udp.service_check("py.can_connect", DogStatsd.CRITICAL, tags=["service:db"], message="refused")
udp.flush()
udp.close_socket()
uds = DogStatsd(socket_path=socket_path, disable_telemetry=True)
for _ in range(25):
    uds.increment("py.uds")
uds.flush()
uds.close_socket()
"#;

#[test]
#[ignore = "starts a kiln server and a runner as processes and sends them DogStatsD"]
fn dogstatsd_is_taken_as_datadog_s_agent_takes_it_on_every_machine() -> anyhow::Result<()> {
    let t0 = now_ms();
    let dir = tempfile::tempdir()?;
    let mut f = Fleet::new()?;
    f.start_runner("r1")?;
    f.until(Duration::from_secs(90), "r1 a learner, caught up", |s| caught_up(s, &["r1"]))?;
    let (p_server, p_runner) = (free_udp_port()?, free_udp_port()?);
    let socket = dir.path().join("dsd.socket");
    let listeners = [
        ("edge", json!({ "listen": [format!("udp://127.0.0.1:{p_server}"), format!("unix://{}", socket.display())], "tags": ["env:e2e"], "on": "server" })),
        ("r1", json!({ "listen": [format!("udp://127.0.0.1:{p_runner}")], "histogram_aggregates": ["max", "min", "count"], "histogram_percentiles": ["0.99"], "on": "r1" })),
    ];
    for (name, body) in &listeners {
        let (code, b) = f.request("PUT", &format!("/api/observe/v1/statsd/{name}"), Some(body))?;
        anyhow::ensure!(code == 204, "{name}: {code} {}", String::from_utf8_lossy(&b));
    }
    // What the API refuses.
    for bad in [json!({ "listen": [] }), json!({ "listen": ["tcp://127.0.0.1:8125"] }), json!({ "listen": ["udp://127.0.0.1:8125"], "histogram_percentiles": [0.999] }), json!({ "listen": ["udp://127.0.0.1:8125"], "histogram_aggregates": ["p50"] })] {
        let (code, _) = f.request("PUT", "/api/observe/v1/statsd/bad", Some(&bad))?;
        anyhow::ensure!(code == 400, "{bad}: {code}");
    }
    // A socket another process reads (a Datadog Agent's, here the test's) is left to it: the
    // listener says why it could not listen there.
    let theirs_path = dir.path().join("agent.socket");
    let theirs = UnixDatagram::bind(&theirs_path)?;
    let (code, _) = f.request("PUT", "/api/observe/v1/statsd/taken", Some(&json!({ "listen": [format!("unix://{}", theirs_path.display())], "on": "server" })))?;
    anyhow::ensure!(code == 204);
    let start = Instant::now();
    loop {
        let st = states(&f)?.get("taken").and_then(|m| m.get("server")).cloned().unwrap_or(Value::Null);
        if at(&st, &["error"]).as_str().is_some_and(|e| e.contains("a process listens")) {
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "the taken socket: {st}");
        std::thread::sleep(Duration::from_millis(250));
    }
    UnixDatagram::unbound()?.send_to(b"still mine", &theirs_path)?;
    let mut got = [0u8; 16];
    theirs.set_read_timeout(Some(Duration::from_secs(5)))?;
    anyhow::ensure!(theirs.recv(&mut got)? == 10, "the other process's socket still its own");
    let (code, _) = f.request("DELETE", "/api/observe/v1/statsd/taken", None)?;
    anyhow::ensure!(code == 204 && theirs_path.exists());
    // Each machine opens its own, and says so.
    let start = Instant::now();
    loop {
        let s = states(&f)?;
        let listening = |l: &str, m: &str, n: usize| s.get(l).and_then(|x| x.get(m)).is_some_and(|st| at(st, &["listening"]).as_array().map_or(0, Vec::len) == n);
        if listening("edge", "server", 2) && listening("r1", "r1", 1) && s.get("edge").is_some_and(|x| !x.contains_key("r1")) {
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "the listeners never opened: {s:?}\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(250));
    }

    // As the protocol spells it: to the server over UDP and its Unix socket, to the runner over UDP.
    let udp = UdpSocket::bind("127.0.0.1:0")?;
    let to_server = ("127.0.0.1", p_server);
    early_in_bucket();
    for _ in 0..100 {
        udp.send_to(b"e2e.hits:1|c|#route:/a", to_server)?;
    }
    let backfill_s = (t0 - 120_000) / 1000;
    let lat: Vec<String> = (1..=20).map(|v| format!("e2e.lat:{v}|ms")).collect();
    let batch = [
        "e2e.hits:2:3|c|@0.5|#route:/a".to_owned(),
        "e2e.hits:5|c|#route:/b,host:edge-7".to_owned(),
        "e2e.temp:20|g".to_owned(),
        "e2e.temp:21|g".to_owned(),
        "e2e.users:ada|s".to_owned(),
        "e2e.users:bob:ada|s".to_owned(),
        lat.join("\n"),
        "e2e.lat:100|h|@0.5".to_owned(),
        "e2e.size:1:2:2|d".to_owned(),
        "e2e.size:1000|d|@0.5".to_owned(),
        format!("e2e.backfill:42|g|T{backfill_s}"),
        "_e{6,4}:Deploy|v1.2|t:error|#service:api".to_owned(),
        "_sc|e2e.db|2|#service:db|m:refused".to_owned(),
        "not a metric".to_owned(),
        "e2e.bad:abc|c".to_owned(),
    ];
    udp.send_to(batch.join("\n").as_bytes(), to_server)?;
    let uds = UnixDatagram::unbound()?;
    for _ in 0..10 {
        uds.send_to(b"e2e.unix:1|c", &socket)?;
    }
    udp.send_to(b"e2e.runner:7|c\ne2e.rlat:1:2:3:4|h", ("127.0.0.1", p_runner))?;

    // Each bucket written once it is over: within ten seconds and a flush.
    let within = Duration::from_secs(40);
    until_total(&f, "sum:e2e.hits{route:/a}.as_count()", 110.0, t0, within)?;
    let to = || now_ms() + 60_000;
    let hosts = by(&f, "sum:e2e.hits{*} by {host}.as_count()", "host", t0, to())?;
    anyhow::ensure!(hosts.len() == 2 && hosts.get("edge-7").is_some_and(|v| v.iter().flatten().sum::<f64>() == 5.0), "a host: tag is the host: {hosts:?}");
    anyhow::ensure!(total(&f, "sum:e2e.hits{env:e2e}.as_count()", t0, to())? == 115.0, "the listener's tags on every metric");
    anyhow::ensure!(last(&f, "max:e2e.temp{*}", t0, to())? == Some(21.0), "a gauge's last value");
    anyhow::ensure!(last(&f, "max:e2e.users{*}", t0, to())? == Some(2.0), "a set's distinct values");
    // The Agent's histogram: 1..=20 once each and 100 twice (a rate of a half).
    until_total(&f, "sum:e2e.lat.count{*}.as_count()", 22.0, t0, within)?;
    for (q, want) in [("max:e2e.lat.max{*}", 100.0), ("max:e2e.lat.median{*}", 11.0), ("max:e2e.lat.95percentile{*}", 100.0), ("max:e2e.lat.avg{*}", 410.0 / 22.0)] {
        let got = last(&f, q, t0, to())?;
        anyhow::ensure!(got.is_some_and(|g| (g - want).abs() < 1e-9), "{q}: {got:?}, not {want}");
    }
    // A distribution, 1, 2, 2 and 1000 twice (a rate of a half): its percentiles read from its
    // buckets (within 1.09%), its count and sum exact.
    until_total(&f, "sum:e2e.size.count{*}.as_count()", 5.0, t0, within)?;
    anyhow::ensure!(total(&f, "sum:e2e.size.sum{*}.as_count()", t0, to())? == 2005.0, "the distribution's sum");
    let p50 = last(&f, "p50:e2e.size{*}", t0, to())?.unwrap_or(0.0);
    let p99 = last(&f, "p99:e2e.size{*}", t0, to())?.unwrap_or(0.0);
    anyhow::ensure!((p50 - 2.0).abs() <= 2.0 * 0.0109 && (p99 - 1000.0).abs() <= 1000.0 * 0.0109, "the distribution's p50 {p50}, p99 {p99}");
    // A gauge with its own time is written at it: two minutes before the test began.
    anyhow::ensure!(last(&f, "max:e2e.backfill{*}", t0 - 180_000, t0 - 60_000)? == Some(42.0), "a gauge at its timestamp");
    until_total(&f, "sum:e2e.unix{*}.as_count()", 10.0, t0, within)?;
    // The runner's own, of its host, its histogram as its listener says.
    until_total(&f, "sum:e2e.runner{host:r1}.as_count()", 7.0, t0, within)?;
    for (q, want) in [("max:e2e.rlat.max{host:r1}", 4.0), ("max:e2e.rlat.min{host:r1}", 1.0), ("max:e2e.rlat.99percentile{host:r1}", 4.0)] {
        anyhow::ensure!(last(&f, q, t0, to())? == Some(want), "{q}");
    }
    until_total(&f, "sum:e2e.rlat.count{host:r1}.as_count()", 4.0, t0, within)?;
    anyhow::ensure!(by(&f, "max:e2e.rlat.median{*}", "host", t0, to())?.is_empty(), "no aggregate the listener did not ask for");
    // An event in the event stream; a service check a record.
    let events = records(&f, "source:kiln.event @event.source:dogstatsd", t0)?;
    let e = events.first().cloned().unwrap_or_default();
    anyhow::ensure!(events.len() == 1 && at(&e, &["status"]) == "error" && at(&e, &["service"]) == "api" && at(&e, &["attributes", "title"]) == "Deploy", "{events:?}");
    let checks = records(&f, "source:service_check @check:e2e.db", t0)?;
    let c = checks.first().cloned().unwrap_or_default();
    anyhow::ensure!(checks.len() == 1 && at(&c, &["status"]) == "error" && at(&c, &["message"]) == "refused" && at(&c, &["attributes", "check_status"]) == "critical", "{checks:?}");
    // A count that stopped falls to zero rather than ending where it stopped.
    let start = Instant::now();
    loop {
        let v = by(&f, "sum:e2e.hits{route:/a}.as_count()", "host", t0, to())?;
        let tail: Vec<Option<f64>> = v.values().next().map(|s| s.iter().copied().skip_while(|x| x.is_none_or(|x| x == 0.0)).collect()).unwrap_or_default();
        if tail.iter().flatten().any(|&x| x == 0.0) {
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(40), "no zero after the count stopped: {v:?}");
        std::thread::sleep(Duration::from_secs(1));
    }
    // What did not read is counted.
    let s = states(&f)?;
    let edge = s.get("edge").and_then(|m| m.get("server")).cloned().unwrap_or(Value::Null);
    anyhow::ensure!(at(&edge, &["invalid"]).as_u64() == Some(2) && at(&edge, &["events"]).as_u64() == Some(1) && at(&edge, &["checks"]).as_u64() == Some(1), "{edge}");

    // Datadog's own client, when there is one.
    match datadog_python() {
        Some(python) => {
            early_in_bucket();
            let t1 = now_ms();
            let backfill = (t1 - 90_000) / 1000;
            run_client(&python, dir.path(), CLIENT, &[p_server.to_string(), socket.display().to_string(), backfill.to_string()])?;
            until_total(&f, "sum:py.requests{route:/x,client:py}.as_count()", 40.0, t1, within)?;
            until_total(&f, "sum:py.requests{route:/y}.as_count()", 5.0, t1, within)?;
            until_total(&f, "sum:py.inflight{*}.as_count()", -1.0, t1, within)?;
            anyhow::ensure!(last(&f, "max:py.queue{*}", t1, to())? == Some(9.0), "py.queue");
            anyhow::ensure!(last(&f, "max:py.users{*}", t1, to())? == Some(3.0), "py.users");
            until_total(&f, "sum:py.latency.count{*}.as_count()", 100.0, t1, within)?;
            // Nearest rank over 1..=100 ms: the 50th and the 95th.
            for (q, want) in [("max:py.latency.median{*}", 50.0), ("max:py.latency.95percentile{*}", 95.0), ("max:py.latency.max{*}", 100.0), ("max:py.latency.avg{*}", 50.5)] {
                anyhow::ensure!(last(&f, q, t1, to())? == Some(want), "{q}");
            }
            until_total(&f, "sum:py.size.count{*}.as_count()", 4.0, t1, within)?;
            anyhow::ensure!(last(&f, "max:py.backfill{*}", t1 - 150_000, t1 - 30_000)? == Some(77.0), "gauge_with_timestamp");
            until_total(&f, "sum:py.uds{*}.as_count()", 25.0, t1, within)?;
            let ev = records(&f, "source:kiln.event @title:\"py deploy\"", t1)?;
            anyhow::ensure!(ev.len() == 1 && ev.first().is_some_and(|e| at(e, &["message"]).as_str().is_some_and(|m| m.contains("rolled out\nv2"))), "{ev:?}");
            anyhow::ensure!(records(&f, "source:service_check @check:py.can_connect status:error", t1)?.len() == 1, "the client's service check");
            eprintln!("  Datadog's client (datadog-py): every metric, event and check as sent");
        }
        None => eprintln!("  Datadog's client: no Python with the datadog package (KOBS_DOGSTATSD_PYTHON); the protocol's datagrams only"),
    }

    // Removed, a listener is closed where it was opened: its socket file goes too.
    let (code, _) = f.request("DELETE", "/api/observe/v1/statsd/edge", None)?;
    anyhow::ensure!(code == 204);
    let start = Instant::now();
    while socket.exists() {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(10), "the Unix socket outlived its listener");
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::ensure!(!states(&f)?.contains_key("edge"));
    Ok(())
}

/// The system's UDP receive errors so far (`/proc/net/snmp`: datagrams dropped for want of room in
/// a socket's buffer), every socket's.
fn udp_dropped() -> u64 {
    let snmp = std::fs::read_to_string("/proc/net/snmp").unwrap_or_default();
    let mut lines = snmp.lines().filter(|l| l.starts_with("Udp:"));
    let (Some(names), Some(values)) = (lines.next(), lines.next()) else { return 0 };
    names.split_whitespace().zip(values.split_whitespace()).find(|(n, _)| *n == "RcvbufErrors").and_then(|(_, v)| v.parse().ok()).unwrap_or(0)
}

/// A process's CPU ticks (user and system), `/proc/<pid>/stat`.
fn ticks(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    let rest = stat.rsplit_once(')').map_or("", |(_, r)| r);
    let f: Vec<&str> = rest.split_whitespace().collect();
    f.get(11).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) + f.get(12).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0)
}

/// A flood over UDP: `KOBS_STATSD_RATE` messages a second (500,000 unless said) for
/// `KOBS_STATSD_SECS` (20), four senders, 13 counts a datagram of 1,432 bytes at the most (as
/// Datadog's clients pack them) over 10,000 contexts. Every datagram the listener read is counted
/// exactly once in what kiln stores; what the system dropped before it was read is reported, with
/// the server's CPU.
#[test]
#[ignore = "starts a kiln server and floods its DogStatsD listener"]
fn a_dogstatsd_flood_is_counted_exactly_as_it_was_read() -> anyhow::Result<()> {
    let env = |k: &str, d: u64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let (rate, secs) = (env("KOBS_STATSD_RATE", 500_000), env("KOBS_STATSD_SECS", 20));
    const PER: u64 = 13;
    const CONTEXTS: u64 = 10_000;
    const SENDERS: u64 = 4;
    let f = Fleet::new()?;
    let pid = f.server.as_ref().map(|p| p.child.id()).unwrap_or(0);
    let port = free_udp_port()?;
    let (code, _) = f.request("PUT", "/api/observe/v1/statsd/flood", Some(&json!({ "listen": [format!("udp://127.0.0.1:{port}")] })))?;
    anyhow::ensure!(code == 204);
    let start = Instant::now();
    while states(&f)?.get("flood").and_then(|m| m.get("server")).is_none_or(|st| at(st, &["listening"]).as_array().is_none_or(Vec::is_empty)) {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(20), "never listened\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(100));
    }
    let t0 = now_ms();
    let (dropped0, ticks0) = (udp_dropped(), ticks(pid));
    let began = Instant::now();
    let senders: Vec<std::thread::JoinHandle<anyhow::Result<u64>>> = (0..SENDERS)
        .map(|s| {
            std::thread::spawn(move || -> anyhow::Result<u64> {
                let sock = UdpSocket::bind("127.0.0.1:0")?;
                sock.connect(("127.0.0.1", port))?;
                // Datagrams this sender sends a second, paced in slices of 10 ms.
                let per_sender = rate / SENDERS / PER;
                let mut sent = 0u64;
                let mut d = String::with_capacity(1432);
                let started = Instant::now();
                let mut i = s;
                while started.elapsed() < Duration::from_secs(secs) {
                    let due = (started.elapsed().as_secs_f64() * per_sender as f64) as u64;
                    while sent < due {
                        d.clear();
                        for _ in 0..PER {
                            if !d.is_empty() {
                                d.push('\n');
                            }
                            d.push_str(&format!("flood.c:1|c|#ctx:c{}", i % CONTEXTS));
                            i += SENDERS;
                        }
                        sock.send(d.as_bytes())?;
                        sent += 1;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(sent)
            })
        })
        .collect();
    let mut sent = 0u64;
    for s in senders {
        sent += s.join().map_err(|_| anyhow::anyhow!("a sender panicked"))??;
    }
    let took = began.elapsed().as_secs_f64();
    let cpu = ticks(pid).saturating_sub(ticks0);
    // Read by the listener, and written: every count it read stored once.
    let start = Instant::now();
    let (read, stored) = loop {
        let st = states(&f)?.get("flood").and_then(|m| m.get("server")).cloned().unwrap_or(Value::Null);
        let read = at(&st, &["datagrams"]).as_u64().unwrap_or(0);
        let stored = total(&f, "sum:flood.c{*}.as_count()", t0 - 20_000, now_ms() + 60_000)?;
        if stored == (read * PER) as f64 && read > 0 && start.elapsed() > Duration::from_secs(12) {
            break (read, stored);
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(90), "{} counts stored of {} read ({read} datagrams); {st}", stored, read * PER);
        std::thread::sleep(Duration::from_secs(1));
    };
    let dropped = udp_dropped().saturating_sub(dropped0);
    let rcvbuf = states(&f)?.get("flood").and_then(|m| m.get("server")).map_or(0, |st| at(st, &["rcvbuf"]).as_u64().unwrap_or(0));
    let hz = 100.0;
    eprintln!(
        "  flood      {sent} datagrams ({:.0} messages a second asked, {:.0} sent) in {took:.1} s; the listener read {read} ({:.3}% not), the system dropped {dropped} (its buffer {} KiB); {stored} counts stored, every one read; the server {:.2} cores while it came",
        rate as f64,
        (sent * PER) as f64 / took,
        100.0 * sent.saturating_sub(read) as f64 / sent.max(1) as f64,
        rcvbuf / 1024,
        cpu as f64 / hz / took
    );
    anyhow::ensure!(read <= sent, "read more than was sent");
    Ok(())
}
