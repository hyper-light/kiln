//! docs/observe.md §13: observe under load, end to end. A kiln server and a runner as processes of
//! their own; exporters send OTLP/HTTP (protobuf, gzip, kept-alive connections, as a collector
//! does) as fast as they are answered, half to the server and half to the runner's own receiver,
//! while a reader asks what a dashboard asks. What it proves is what a user relies on: every point
//! answered 200 is stored, exactly once (a point answered 503 was not taken, and its sender, told
//! when to come back, does); every replica holds the same; and what it measures is what they see:
//! points a second taken, how long an answer takes, how long until a point can be read, and what it
//! costs the server and the runner in CPU and memory.
//!
//! It starts real processes and loads them, so it is not run by default:
//! `cargo test --release -p kiln --test observe_load -- --ignored --nocapture`, with
//! `KOBS_LOAD_SECS` (20), `KOBS_LOAD_SENDERS` (8), `KOBS_LOAD_SERIES` (1000 a sender: points a
//! request), `KOBS_LOAD_GZIP` (1); `KOBS_LOAD_HOLD=<seconds>` waits that long before the flood, the
//! server's and the runner's pids written to `KOBS_LOAD_PIDS`, for a profiler to attach;
//! `KOBS_PROFILE=<folder>` runs each node under samply (`SAMPLY`: its path), its profile saved
//! there as `server.json.gz` and `r1.json.gz`.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use kiln_obs::otlp::pb::Writer;
use serde_json::json;

mod common;
use common::{Fleet, at, caught_up};

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// One HTTP/1.1 connection kept alive, a request at a time, as an exporter sends them; opened
/// again when the server closed it.
struct Conn {
    port: u16,
    s: Option<BufReader<TcpStream>>,
}

struct Answered {
    code: u16,
    retry_after: Option<u64>,
    body: Vec<u8>,
}

impl Conn {
    fn new(port: u16) -> Self {
        Self { port, s: None }
    }

    fn send(&mut self, path: &str, headers: &str, body: &[u8]) -> anyhow::Result<Answered> {
        let mut last = None;
        for _ in 0..2 {
            match self.once(path, headers, body) {
                Ok(a) => return Ok(a),
                Err(e) => {
                    self.s = None;
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("no answer")))
    }

    fn once(&mut self, path: &str, headers: &str, body: &[u8]) -> anyhow::Result<Answered> {
        if self.s.is_none() {
            let s = TcpStream::connect(("127.0.0.1", self.port))?;
            s.set_nodelay(true)?;
            s.set_read_timeout(Some(Duration::from_secs(60)))?;
            self.s = Some(BufReader::new(s));
        }
        let s = self.s.as_mut().ok_or_else(|| anyhow::anyhow!("no connection"))?;
        let head = format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{headers}Content-Length: {}\r\n\r\n", body.len());
        s.get_mut().write_all(head.as_bytes())?;
        s.get_mut().write_all(body)?;
        let mut line = String::new();
        s.read_line(&mut line)?;
        let code: u16 = line.split_whitespace().nth(1).ok_or_else(|| anyhow::anyhow!("no status: {line:?}"))?.parse()?;
        let (mut length, mut chunked, mut close, mut retry_after) = (0usize, false, false, None);
        loop {
            line.clear();
            s.read_line(&mut line)?;
            let l = line.trim_end();
            if l.is_empty() {
                break;
            }
            let (k, v) = l.split_once(':').unwrap_or((l, ""));
            match k.to_ascii_lowercase().as_str() {
                "content-length" => length = v.trim().parse()?,
                "transfer-encoding" => chunked = v.trim().eq_ignore_ascii_case("chunked"),
                "connection" => close = v.trim().eq_ignore_ascii_case("close"),
                "retry-after" => retry_after = v.trim().parse().ok(),
                _ => {}
            }
        }
        let mut out = Vec::new();
        if chunked {
            loop {
                line.clear();
                s.read_line(&mut line)?;
                let size = usize::from_str_radix(line.trim(), 16)?;
                let mut chunk = vec![0; size + 2];
                s.read_exact(&mut chunk)?;
                chunk.truncate(size);
                out.extend_from_slice(&chunk);
                if size == 0 {
                    break;
                }
            }
        } else {
            out.resize(length, 0);
            s.read_exact(&mut out)?;
        }
        if close {
            self.s = None;
        }
        Ok(Answered { code, retry_after, body: out })
    }
}

fn kv(w: &mut Writer, field: u32, k: &str, v: &str) {
    w.message(field, |kv| {
        kv.str(1, k);
        kv.message(2, |av| av.str(1, v));
    });
}

/// An OTLP metrics request: gauge `load.v`, 1.0 at `ts_ns` for each of `series` series of host
/// `load-<sender>` (opentelemetry-proto v1.5.0's field numbers).
fn request(sender: u64, series: u64, ts_ns: u64) -> Vec<u8> {
    let mut w = Writer::default();
    w.message(1, |rm| {
        rm.message(1, |res| kv(res, 1, "host.name", &format!("load-{sender}")));
        rm.message(2, |sm| {
            sm.message(2, |m| {
                m.str(1, "load.v");
                m.message(5, |g| {
                    for j in 0..series {
                        g.message(1, |dp| {
                            kv(dp, 7, "s", &j.to_string());
                            dp.fixed64(3, ts_ns);
                            dp.double(4, 1.0);
                        });
                    }
                });
            });
        });
    });
    w.buf
}

fn gzip(b: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(b)?;
    Ok(e.finish()?)
}

/// What a sender saw.
#[derive(Default)]
struct Sent {
    taken: u64,
    refused: u64,
    requests: u64,
    errors: Vec<String>,
    latencies_us: Vec<u64>,
}

/// A process's CPU seconds so far and its peak resident memory (Linux's /proc).
fn usage(pid: u32) -> (f64, u64) {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    // Fields after the command's closing parenthesis: utime and stime are the 12th and 13th.
    let after = stat.rsplit_once(')').map_or("", |(_, r)| r);
    let f: Vec<&str> = after.split_whitespace().collect();
    let ticks = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    let cpu = (ticks(11) + ticks(12)) / 100.0;
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    let hwm = status.lines().find(|l| l.starts_with("VmHWM:")).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    (cpu, hwm * 1024)
}

/// Each of a process's threads' CPU seconds so far, summed by the thread's name (its numbers left
/// out: `kiln-rt-3` counts as `kiln-rt-`): where its time goes, without a profiler.
fn cpu_by_thread(pid: u32) -> std::collections::BTreeMap<String, f64> {
    let mut out = std::collections::BTreeMap::new();
    let Ok(dir) = std::fs::read_dir(format!("/proc/{pid}/task")) else { return out };
    for t in dir.flatten() {
        let stat = std::fs::read_to_string(t.path().join("stat")).unwrap_or_default();
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else { continue };
        let name: String = stat.get(open + 1..close).unwrap_or("").trim_end_matches(|c: char| c.is_ascii_digit()).to_owned();
        let f: Vec<&str> = stat.get(close + 1..).unwrap_or("").split_whitespace().collect();
        let ticks = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
        *out.entry(name).or_insert(0.0) += (ticks(11) + ticks(12)) / 100.0;
    }
    out
}

/// What each thread name spent between two readings, most first.
fn spent(before: &std::collections::BTreeMap<String, f64>, after: &std::collections::BTreeMap<String, f64>) -> String {
    let mut d: Vec<(String, f64)> = after.iter().map(|(k, v)| (k.clone(), v - before.get(k).copied().unwrap_or(0.0))).filter(|(_, v)| *v >= 0.05).collect();
    d.sort_by(|a, b| b.1.total_cmp(&a.1));
    d.iter().map(|(k, v)| format!("{k} {v:.1}")).collect::<Vec<_>>().join(", ")
}

fn pct(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() as f64 - 1.0) * q).round() as usize;
    sorted.get(i).copied().unwrap_or(0)
}

/// Every point of `load.v` the store holds: each one 1.0, summed bucket by bucket.
fn stored(f: &Fleet, from: i64, to: i64) -> anyhow::Result<u64> {
    let a = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": "sum:load.v{*}.rollup(sum)", "from": from, "to": to, "points": 200 })))?;
    let total: f64 = at(&a, &["series"]).as_array().into_iter().flatten().flat_map(|s| at(s, &["values"]).as_array().cloned().unwrap_or_default()).filter_map(|v| v.as_f64()).sum();
    Ok(total.round() as u64)
}

#[test]
#[ignore = "starts real processes and loads them"]
fn an_otlp_flood_is_taken_or_refused_never_lost_and_replicas_agree() -> anyhow::Result<()> {
    let secs = env("KOBS_LOAD_SECS", 20);
    let senders = env("KOBS_LOAD_SENDERS", 8);
    let series = env("KOBS_LOAD_SERIES", 1000);
    let compress = env("KOBS_LOAD_GZIP", 1) == 1;
    let mut f = Fleet::new()?;
    f.start_runner("r1")?;
    f.until(Duration::from_secs(90), "r1 a learner, caught up", |s| caught_up(s, &["r1"]))?;
    let ingest = std::fs::read_to_string(f.root.path().join("server").join("observe").join("ingest.token"))?.trim().to_owned();
    let r1 = *f.otlp.get("r1").ok_or_else(|| anyhow::anyhow!("r1's receiver"))?;
    let server_pid = f.server.as_ref().map_or(0, |p| p.child.id());
    let runner_pid = f.runners.get("r1").and_then(Option::as_ref).map_or(0, |p| p.child.id());
    if let Some(hold) = std::env::var("KOBS_LOAD_HOLD").ok().and_then(|v| v.parse::<u64>().ok()) {
        // Time to attach a profiler to the server and the runner (their pids, in KOBS_LOAD_PIDS).
        if let Ok(path) = std::env::var("KOBS_LOAD_PIDS") {
            std::fs::write(path, format!("{server_pid} {runner_pid}\n"))?;
        }
        std::thread::sleep(Duration::from_secs(hold));
    }
    let (s_cpu0, _) = usage(server_pid);
    let (r_cpu0, _) = usage(runner_pid);
    let (s_threads0, r_threads0) = (cpu_by_thread(server_pid), cpu_by_thread(runner_pid));

    // Points stand 10 ms apart in each series, from ten minutes ago: each request's its own time.
    let t0_ms = common::now_ms() - 600_000;
    let stop = Arc::new(AtomicBool::new(false));
    let started = Instant::now();
    let mut threads = Vec::new();
    for sender in 0..senders {
        let (stop, ingest) = (stop.clone(), ingest.clone());
        // Even senders to the server, odd ones to the runner's receiver.
        let (port, auth) = if sender % 2 == 0 { (f.http, format!("Authorization: Bearer {ingest}\r\n")) } else { (r1, String::new()) };
        threads.push(std::thread::spawn(move || -> Sent {
            let mut sent = Sent::default();
            let mut conn = Conn::new(port);
            let headers = format!("{auth}Content-Type: application/x-protobuf\r\n{}", if compress { "Content-Encoding: gzip\r\n" } else { "" });
            let mut i: u64 = 0;
            while !stop.load(Ordering::Relaxed) {
                let ts_ns = (t0_ms as u64 + i * 10) * 1_000_000;
                let body = request(sender, series, ts_ns);
                let body = if compress { gzip(&body).unwrap_or(body) } else { body };
                let t = Instant::now();
                let answer = conn.send("/v1/metrics", &headers, &body);
                sent.latencies_us.push(t.elapsed().as_micros() as u64);
                sent.requests += 1;
                match answer {
                    Ok(a) if a.code == 200 => {
                        sent.taken += series;
                        i += 1;
                    }
                    Ok(a) if a.code == 503 => {
                        // Not taken: the same points again once told to.
                        sent.refused += series;
                        std::thread::sleep(Duration::from_millis(a.retry_after.map_or(100, |s| s * 1000).min(2_000)));
                    }
                    Ok(a) => sent.errors.push(format!("{}: {}", a.code, String::from_utf8_lossy(&a.body))),
                    Err(e) => sent.errors.push(e.to_string()),
                }
                if sent.errors.len() > 20 {
                    break;
                }
            }
            sent
        }));
    }
    // What a dashboard asks meanwhile: one series, and every series by host.
    let reader = {
        let (stop, port, admin) = (stop.clone(), f.http, f.admin.clone());
        std::thread::spawn(move || -> (Vec<u64>, Vec<u64>, Vec<String>) {
            let (mut one, mut all, mut errors) = (Vec::new(), Vec::new(), Vec::new());
            let mut conn = Conn::new(port);
            while !stop.load(Ordering::Relaxed) {
                let now = common::now_ms();
                for (q, into) in [("avg:load.v{host:load-0,s:7}", &mut one), ("sum:load.v{*} by {host}", &mut all)] {
                    let body = serde_json::to_vec(&json!({ "query": q, "from": now - 900_000, "to": now, "points": 120 })).unwrap_or_default();
                    let t = Instant::now();
                    match conn.send("/api/observe/v1/query", &format!("Authorization: Bearer {admin}\r\nContent-Type: application/json\r\n"), &body) {
                        Ok(a) if a.code == 200 => into.push(t.elapsed().as_micros() as u64),
                        Ok(a) => errors.push(format!("{q}: {}", a.code)),
                        Err(e) => errors.push(format!("{q}: {e}")),
                    }
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            (one, all, errors)
        })
    };
    std::thread::sleep(Duration::from_secs(secs));
    stop.store(true, Ordering::Relaxed);
    let flood = started.elapsed();
    let mut all = Sent::default();
    for t in threads {
        let s = t.join().map_err(|_| anyhow::anyhow!("a sender panicked"))?;
        all.taken += s.taken;
        all.refused += s.refused;
        all.requests += s.requests;
        all.errors.extend(s.errors);
        all.latencies_us.extend(s.latencies_us);
    }
    let (mut one, mut every, read_errors) = reader.join().map_err(|_| anyhow::anyhow!("the reader panicked"))?;
    anyhow::ensure!(all.errors.is_empty(), "senders' errors (but 503s): {:?}\n{}", all.errors.iter().take(5).collect::<Vec<_>>(), f.logs());
    anyhow::ensure!(read_errors.is_empty(), "the reader's errors: {read_errors:?}");

    // Every point taken, readable, once: the count settles at what was taken, no more, no less.
    let to = common::now_ms() + 60_000;
    let settled = Instant::now();
    let mut have = 0;
    while settled.elapsed() < Duration::from_secs(120) {
        have = stored(&f, t0_ms - 60_000, to)?;
        if have >= all.taken {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let lag = settled.elapsed();
    let s = f.status()?;
    let dropped = at(&s, &["dropped"]).as_u64().unwrap_or(0) + at(&s, &["machines"]).as_array().into_iter().flatten().map(|m| at(m, &["status", "dropped"]).as_u64().unwrap_or(0)).sum::<u64>();
    anyhow::ensure!(dropped == 0, "{dropped} points dropped for room after being taken\n{}", f.logs());
    anyhow::ensure!(have == all.taken, "{} points taken, {have} stored\n{}", all.taken, f.logs());
    let (s_cpu1, s_rss) = usage(server_pid);
    let (r_cpu1, r_rss) = usage(runner_pid);
    let (s_threads1, r_threads1) = (cpu_by_thread(server_pid), cpu_by_thread(runner_pid));
    let (index, digest) = f.verify(2)?;

    all.latencies_us.sort_unstable();
    one.sort_unstable();
    every.sort_unstable();
    let ms = |us: u64| us as f64 / 1000.0;
    eprintln!("\nobserve under load: {senders} senders × {series} points a request, gzip {compress}, {:.1} s", flood.as_secs_f64());
    eprintln!("  taken      {:>12} points, {:>10.0} points/s, {} requests", all.taken, all.taken as f64 / flood.as_secs_f64(), all.requests);
    eprintln!("  refused    {:>12} points (503, sent again when told), {:.2}% of requests", all.refused, 100.0 * (all.refused / series.max(1)) as f64 / all.requests.max(1) as f64);
    eprintln!("  a request  p50 {:.2} ms, p99 {:.2} ms, max {:.2} ms", ms(pct(&all.latencies_us, 0.5)), ms(pct(&all.latencies_us, 0.99)), ms(pct(&all.latencies_us, 1.0)));
    eprintln!("  readable   all of it {:.2} s after the last was taken", lag.as_secs_f64());
    eprintln!("  one series p50 {:.2} ms, p99 {:.2} ms ({} reads)", ms(pct(&one, 0.5)), ms(pct(&one, 0.99)), one.len());
    eprintln!("  by host    p50 {:.2} ms, p99 {:.2} ms ({} reads, {} series)", ms(pct(&every, 0.5)), ms(pct(&every, 0.99)), every.len(), senders * series);
    eprintln!("  server     {:.1} CPU s ({:.2} cores), peak RSS {:.0} MiB", s_cpu1 - s_cpu0, (s_cpu1 - s_cpu0) / flood.as_secs_f64(), s_rss as f64 / 1048576.0);
    eprintln!("    by thread: {}", spent(&s_threads0, &s_threads1));
    eprintln!("  runner     {:.1} CPU s ({:.2} cores), peak RSS {:.0} MiB", r_cpu1 - r_cpu0, (r_cpu1 - r_cpu0) / flood.as_secs_f64(), r_rss as f64 / 1048576.0);
    eprintln!("    by thread: {}", spent(&r_threads0, &r_threads1));
    eprintln!("  replicas   agree at index {index}: {digest:016x}");
    // Under a profiler (KOBS_PROFILE), each node stops gently so its profile is saved.
    f.stop_gently();
    Ok(())
}
