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
//! request), `KOBS_LOAD_GZIP` (1), `KOBS_LOAD_READ` (1: a reader alongside; 0: none); `KOBS_LOAD_HOLD=<seconds>` waits that long before the flood, the
//! server's and the runner's pids written to `KOBS_LOAD_PIDS`, for a profiler to attach;
//! `KOBS_LOAD_RATE=<requests a second a sender>` paces the runner's senders when its server is away;
//! `KOBS_PROFILE=<folder>` runs each node under samply (`SAMPLY`: its path), its profile saved
//! there as `server.json.gz` and `r1.json.gz`.
//!
//! The soak (`a_steady_load_held_for_long_keeps_its_memory_level_and_loses_nothing`):
//! `KOBS_SOAK_SECS` (300; hours for a soak worth the name), `KOBS_SOAK_RATE` (points a second,
//! 200,000), `KOBS_SOAK_LOGS` (log records a second, 2,000), `KOBS_SOAK_HELPERS` (the server's
//! `query_helpers`; unset, its default).

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

/// A dashboard query read in the soak: when, which, how long in all, and where that went.
struct Read {
    at_s: f64,
    which: usize,
    us: u64,
    queued_us: u64,
    waited_us: u64,
    took_us: u64,
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
    /// The time of its last request taken (ms): the newest of what it sent.
    last_ms: Option<i64>,
    taken: u64,
    refused: u64,
    requests: u64,
    errors: Vec<String>,
    latencies_us: Vec<u64>,
    /// When each request was sent (µs into the run), where a sender says.
    sent_at_us: Vec<u64>,
    /// A paced sender's schedule started over, behind by more than a second.
    late: u64,
}

thread_local! {
    /// The minor page faults the last `usage` read.
    static MINFLT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A process's CPU seconds so far and its peak resident memory (Linux's /proc); its minor page
/// faults so far in `MINFLT`.
fn usage(pid: u32) -> (f64, u64) {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    // Fields after the command's closing parenthesis: utime and stime are the 12th and 13th.
    let after = stat.rsplit_once(')').map_or("", |(_, r)| r);
    let f: Vec<&str> = after.split_whitespace().collect();
    let ticks = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    let cpu = (ticks(11) + ticks(12)) / 100.0;
    MINFLT.with(|m| m.set(ticks(7) as u64));
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

/// A process's resident anonymous memory by where it lies (`/proc/<pid>/smaps`), bytes: the main
/// heap; the C library's thread arenas' heaps (each a reservation of 64 MiB at the most, aligned
/// to it), each one's; and the rest (mmapped allocations, stacks).
#[derive(Debug, Default)]
struct Anon {
    heap: u64,
    arenas: Vec<u64>,
    other: u64,
}

fn anon_of(pid: u32) -> Anon {
    let smaps = std::fs::read_to_string(format!("/proc/{pid}/smaps")).unwrap_or_default();
    let mut out = Anon::default();
    // The mapping being read: its start, length, and whether it is the heap; None for a file's.
    let mut current: Option<(u64, u64, bool)> = None;
    let add = |out: &mut Anon, c: Option<(u64, u64, bool)>, bytes: u64| match c {
        Some((_, _, true)) => out.heap += bytes,
        Some((start, len, false)) if len <= 64 << 20 && start % (64 << 20) == 0 => out.arenas.push(bytes),
        Some(_) => out.other += bytes,
        None => {}
    };
    let mut bytes = 0u64;
    for line in smaps.lines() {
        let mut parts = line.split_whitespace();
        let first = parts.next().unwrap_or("");
        let range = first.split_once('-').and_then(|(a, b)| Some((u64::from_str_radix(a, 16).ok()?, u64::from_str_radix(b, 16).ok()?)));
        if let Some((a, b)) = range {
            add(&mut out, current.take(), bytes);
            bytes = 0;
            let rest: Vec<&str> = parts.collect();
            // An address range's line: perms, offset, device, inode, then a name if any.
            let name = rest.get(4).copied().unwrap_or("");
            current = if name.is_empty() || name == "[heap]" { Some((a, b.saturating_sub(a), name == "[heap]")) } else { None };
        } else if first == "Anonymous:" {
            bytes = parts.next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) * 1024;
        }
    }
    add(&mut out, current.take(), bytes);
    out
}

fn anon_by_mapping(pid: u32) -> String {
    let a = anon_of(pid);
    let mib = |b: u64| b as f64 / 1048576.0;
    let mut each = a.arenas.clone();
    each.sort_unstable_by(|x, y| y.cmp(x));
    let n = each.iter().filter(|&&b| b > 0).count();
    let top: Vec<String> = each.iter().take(4).map(|&b| format!("{:.0}", mib(b))).collect();
    format!("heap {:.0} MiB, {n} arena heaps {:.0} MiB (the largest {} MiB), other {:.0} MiB", mib(a.heap), mib(each.iter().sum()), top.join(", "), mib(a.other))
}

/// The breakdown counts each resident anonymous byte once: its parts add up to what the process
/// says it holds anonymously, give or take what changed between the two reads.
#[test]
fn a_process_s_memory_by_where_it_lies_adds_up_to_what_it_holds() {
    let held: Vec<u8> = vec![1; 32 << 20];
    let a = anon_of(std::process::id());
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let anon_kib: u64 = status.lines().find(|l| l.starts_with("RssAnon:")).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse().ok()).unwrap_or(0);
    let total = a.heap + a.arenas.iter().sum::<u64>() + a.other;
    let anon = anon_kib * 1024;
    assert!(total <= anon + (8 << 20) && total + (8 << 20) >= anon, "{a:?}: {total} of {anon}");
    assert!(held.iter().all(|&b| b == 1));
}

fn pct(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() as f64 - 1.0) * q).round() as usize;
    sorted.get(i).copied().unwrap_or(0)
}

/// The CPU time a process has had (user and system, every thread's), in clock ticks, from
/// `/proc/<pid>/stat`: 0 where there is none.
fn cpu_ticks(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    // Its name, in parentheses, may hold spaces: the fields after the last `)`, from its state on.
    let rest = stat.rsplit_once(')').map_or("", |(_, r)| r);
    rest.split_whitespace().skip(11).take(2).filter_map(|v| v.parse::<u64>().ok()).sum()
}

/// One GET of `path` on `port` (with `token` when given), within `within`: how long it took, or why
/// it did not answer.
fn timed_get(port: u16, path: &str, token: &str, within: Duration) -> Result<Duration, String> {
    let t = Instant::now();
    let mut s = TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], port)), within).map_err(|e| format!("connect: {e}"))?;
    s.set_read_timeout(Some(within)).map_err(|e| e.to_string())?;
    let auth = if token.is_empty() { String::new() } else { format!("Authorization: Bearer {token}\r\n") };
    write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{auth}Connection: close\r\n\r\n").map_err(|e| format!("write: {e}"))?;
    let mut out = Vec::new();
    s.read_to_end(&mut out).map_err(|e| format!("no answer within {} s ({e})", within.as_secs()))?;
    if !out.starts_with(b"HTTP/1.1 200") {
        return Err(String::from_utf8_lossy(out.get(..out.len().min(80)).unwrap_or(&[])).into_owned());
    }
    Ok(t.elapsed())
}

/// Every thread's stack of process `pid` (eu-stack, else gdb), written to `KOBS_STACKS` (or the
/// temporary folder): where a process that stopped answering is. The file's path.
fn stacks(pid: u32, why: &str) -> Option<std::path::PathBuf> {
    let dir = std::env::var_os("KOBS_STACKS").map_or_else(std::env::temp_dir, std::path::PathBuf::from);
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("stacks-{pid}-{}.txt", common::now_ms()));
    let out = std::process::Command::new("eu-stack").args(["-p", &pid.to_string()]).output().ok().filter(|o| o.status.success()).or_else(|| std::process::Command::new("gdb").args(["-batch", "-p", &pid.to_string(), "-ex", "thread apply all bt"]).output().ok())?;
    let mut text = format!("{why}\n\n").into_bytes();
    text.extend_from_slice(&out.stdout);
    text.extend_from_slice(&out.stderr);
    std::fs::write(&path, text).ok()?;
    Some(path)
}

/// The machine's CPU time, busy and in all (every core's), in clock ticks, and its cores, from
/// `/proc/stat`.
fn machine_ticks() -> (u64, u64, usize) {
    let stat = std::fs::read_to_string("/proc/stat").unwrap_or_default();
    let mut lines = stat.lines();
    // user, nice, system, idle, iowait, irq, softirq, steal (guest time is in user's already).
    let v: Vec<u64> = lines.next().filter(|l| l.starts_with("cpu ")).map(|l| l.split_whitespace().skip(1).take(8).filter_map(|v| v.parse().ok()).collect()).unwrap_or_default();
    let total: u64 = v.iter().sum();
    let idle = v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0);
    let cores = lines.filter(|l| l.strip_prefix("cpu").is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))).count();
    (total.saturating_sub(idle), total, cores)
}

/// Every point of `load.v` the store holds: each one 1.0, summed bucket by bucket.
fn stored(f: &Fleet, from: i64, to: i64) -> anyhow::Result<u64> {
    // Whole five-minute buckets, the rollups' tier (docs/observe.md §4.7): an hours-long soak's
    // billion points counted in a moment, not read one by one.
    let step = 300_000;
    let (from, to) = (from.div_euclid(step) * step, (to + step - 1).div_euclid(step) * step);
    let points = ((to - from) / step).clamp(1, 2_000);
    let a = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": "sum:load.v{*}.rollup(sum)", "from": from, "to": to, "step": step, "points": points })))?;
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
    // What each holds before the flood: the rest of kiln's, so what the flood adds reads apart.
    let rss = |pid: u32| -> (f64, f64) {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        let kib = |k: &str| status.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0) / 1024.0;
        (kib("VmRSS:"), kib("RssAnon:"))
    };
    let (before_server, before_runner) = (rss(server_pid), rss(runner_pid));
    let (s_cpu0, _) = usage(server_pid);
    let s_flt0 = MINFLT.with(std::cell::Cell::get);
    let (r_cpu0, _) = usage(runner_pid);
    let r_flt0 = MINFLT.with(std::cell::Cell::get);
    let (s_threads0, r_threads0) = (cpu_by_thread(server_pid), cpu_by_thread(runner_pid));

    // Points stand 1 ms apart in each series, from two hours ago: each request's its own time, all
    // of them in the past however long the flood (a minute of it is some 60,000 requests a sender).
    let t0_ms = common::now_ms() - 7_200_000;
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
                let ts_ns = (t0_ms as u64 + i) * 1_000_000;
                let body = request(sender, series, ts_ns);
                let body = if compress { gzip(&body).unwrap_or(body) } else { body };
                let t = Instant::now();
                let answer = conn.send("/v1/metrics", &headers, &body);
                sent.latencies_us.push(t.elapsed().as_micros() as u64);
                sent.requests += 1;
                match answer {
                    Ok(a) if a.code == 200 => {
                        sent.taken += series;
                        sent.last_ms = Some((ts_ns / 1_000_000) as i64);
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
        let t0 = t0_ms;
        std::thread::spawn(move || -> (Vec<u64>, Vec<u64>, Vec<String>) {
            let (mut one, mut all, mut errors) = (Vec::new(), Vec::new(), Vec::new());
            let mut conn = Conn::new(port);
            while !stop.load(Ordering::Relaxed) && env("KOBS_LOAD_READ", 1) == 1 {
                // What came in the last 15 seconds of the flood's own time (its points' times).
                let now = t0 + started.elapsed().as_millis() as i64 * 3;
                for (q, into) in [("avg:load.v{host:load-0,s:7}", &mut one), ("sum:load.v{*} by {host}", &mut all)] {
                    let body = serde_json::to_vec(&json!({ "query": q, "from": now - 15_000, "to": now, "points": 120 })).unwrap_or_default();
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
    let mut newest: Vec<(u64, i64)> = Vec::new();
    for (sender, t) in threads.into_iter().enumerate() {
        let s = t.join().map_err(|_| anyhow::anyhow!("a sender panicked"))?;
        newest.extend(s.last_ms.map(|ms| (sender as u64, ms)));
        all.taken += s.taken;
        all.refused += s.refused;
        all.requests += s.requests;
        all.errors.extend(s.errors);
        all.latencies_us.extend(s.latencies_us);
    }
    let (mut one, mut every, read_errors) = reader.join().map_err(|_| anyhow::anyhow!("the reader panicked"))?;
    anyhow::ensure!(all.errors.is_empty(), "senders' errors (but 503s): {:?}\n{}", all.errors.iter().take(5).collect::<Vec<_>>(), f.logs());
    anyhow::ensure!(read_errors.is_empty(), "the reader's errors: {read_errors:?}");

    // Readable: each sender's newest request, all its points, as a dashboard would ask for them.
    let settled = Instant::now();
    for (sender, ms) in &newest {
        loop {
            let a = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": format!("sum:load.v{{host:load-{sender}}}.rollup(sum)"), "from": ms, "to": ms + 1, "step": 1 })))?;
            let n: f64 = at(&a, &["series"]).as_array().into_iter().flatten().flat_map(|s| at(s, &["values"]).as_array().cloned().unwrap_or_default()).filter_map(|v| v.as_f64()).sum();
            if n.round() as u64 >= series {
                break;
            }
            anyhow::ensure!(settled.elapsed() < Duration::from_secs(120), "sender {sender}'s newest points never readable: {n}\n{}", f.logs());
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let lag = settled.elapsed();
    // Every point taken, readable, once: the count settles at what was taken, no more, no less.
    let to = newest.iter().map(|(_, ms)| *ms).max().unwrap_or(t0_ms) + 1_000;
    let mut have = 0;
    while settled.elapsed() < Duration::from_secs(180) {
        have = stored(&f, t0_ms - 60_000, to)?;
        if have >= all.taken {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let s = f.status()?;
    let dropped = at(&s, &["dropped"]).as_u64().unwrap_or(0) + at(&s, &["machines"]).as_array().into_iter().flatten().map(|m| at(m, &["status", "dropped"]).as_u64().unwrap_or(0)).sum::<u64>();
    anyhow::ensure!(dropped == 0, "{dropped} points dropped for room after being taken\n{}", f.logs());
    anyhow::ensure!(have == all.taken, "{} points taken, {have} stored\n{}", all.taken, f.logs());
    let (s_cpu1, s_rss) = usage(server_pid);
    let s_flt = MINFLT.with(std::cell::Cell::get) - s_flt0;
    let (r_cpu1, r_rss) = usage(runner_pid);
    let r_flt = MINFLT.with(std::cell::Cell::get) - r_flt0;
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
    eprintln!("  readable   every sender's newest points {:.0} ms after the flood", lag.as_secs_f64() * 1000.0);
    eprintln!("  one series p50 {:.2} ms, p99 {:.2} ms ({} reads)", ms(pct(&one, 0.5)), ms(pct(&one, 0.99)), one.len());
    eprintln!("  by host    p50 {:.2} ms, p99 {:.2} ms ({} reads, {} series)", ms(pct(&every, 0.5)), ms(pct(&every, 0.99)), every.len(), senders * series);
    eprintln!("  server     {:.1} CPU s ({:.2} cores), peak RSS {:.0} MiB, {:.3} minor faults a point", s_cpu1 - s_cpu0, (s_cpu1 - s_cpu0) / flood.as_secs_f64(), s_rss as f64 / 1048576.0, s_flt as f64 / all.taken.max(1) as f64);
    eprintln!("    by thread: {}", spent(&s_threads0, &s_threads1));
    eprintln!("  runner     {:.1} CPU s ({:.2} cores), peak RSS {:.0} MiB, {:.3} minor faults a point", r_cpu1 - r_cpu0, (r_cpu1 - r_cpu0) / flood.as_secs_f64(), r_rss as f64 / 1048576.0, r_flt as f64 / all.taken.max(1) as f64);
    eprintln!("    by thread: {}", spent(&r_threads0, &r_threads1));
    // What each holds once the flood is over and its writes are done: memory kept, not used.
    std::thread::sleep(Duration::from_secs(5));
    let ((s_now, s_anon), (r_now, r_anon)) = (rss(server_pid), rss(runner_pid));
    eprintln!("  before it  the server held {:.0} MiB ({:.0} anonymous), the runner {:.0} MiB ({:.0} anonymous)", before_server.0, before_server.1, before_runner.0, before_runner.1);
    eprintln!("  after it   the server holds {s_now:.0} MiB ({s_anon:.0} anonymous), the runner {r_now:.0} MiB ({r_anon:.0} anonymous)");
    eprintln!("  where      the server's {}; the runner's {}", anon_by_mapping(server_pid), anon_by_mapping(runner_pid));
    let mib = |k: &str| at(&s, &[k]).as_u64().unwrap_or(0) as f64 / 1048576.0;
    eprintln!(
        "  the server's store: {} blocks, {:.0} MiB on disk; in memory symbols {:.1} MiB, index {:.1} MiB, head samples {:.1} MiB",
        at(&s, &["blocks"]).as_u64().unwrap_or(0),
        mib("disk_bytes"),
        mib("heap_symbols"),
        mib("heap_index"),
        mib("heap_samples")
    );
    eprintln!("  replicas   agree at index {index}: {digest:016x}");
    if std::env::var("KOBS_LOAD_FILES").is_ok() {
        let dir = f.root.path().join("server").join("observe").join("store");
        let mut files: Vec<(String, u64)> = std::fs::read_dir(&dir)?.flatten().filter_map(|e| Some((e.file_name().to_string_lossy().into_owned(), e.metadata().ok()?.len()))).collect();
        files.sort();
        eprintln!("  the server's store files: {files:?}");
        let log = std::fs::read_to_string(f.root.path().join("server.log")).unwrap_or_default();
        for l in log.lines().filter(|l| l.contains("rewrite") || l.contains("maintenance looks") || l.contains("WARN") || l.contains("not taken")).take(40) {
            eprintln!("  server log: {l}");
        }
    }
    // Under a profiler (KOBS_PROFILE), each node stops gently so its profile is saved.
    f.stop_gently();
    Ok(())
}

/// An OTLP logs request of `records` records of `sender`'s, as an SDK's batch processor sends
/// them (its resource's nine attributes, four a record, its trace's ids, one an error in fifty),
/// record `n` on from `first`, each `n` its own (`seq`), its time `now_ns`'s.
fn logs_request(sender: u64, first: u64, records: u64, now_ns: u64) -> Vec<u8> {
    let mut w = Writer::default();
    w.message(1, |rl| {
        rl.message(1, |res| {
            for (k, v) in [
                ("service.name", "flood".to_owned()),
                ("host.name", format!("load-{sender}")),
                ("deployment.environment.name", "load".to_owned()),
                ("service.version", "4.2.0".to_owned()),
                ("service.instance.id", format!("6f1c2a8e-{sender:04}")),
                ("telemetry.sdk.name", "opentelemetry".to_owned()),
                ("telemetry.sdk.language", "java".to_owned()),
                ("telemetry.sdk.version", "1.43.0".to_owned()),
                ("k8s.pod.name", format!("flood-7d9f8b6c5-{sender:05}")),
            ] {
                kv(res, 1, k, &v);
            }
        });
        rl.message(2, |sl| {
            sl.message(1, |scope| scope.str(1, "com.shop.Flood"));
            for n in first..first + records {
                sl.message(2, |r| {
                    r.fixed64(1, now_ns + (n - first) * 1_000);
                    let error = n % 50 == 0;
                    r.varint(2, if error { 17 } else { 9 });
                    r.str(3, if error { "ERROR" } else { "INFO" });
                    r.message(5, |b| b.str(1, &format!("order {n} paid in {} ms by a card ending 4242", n % 900)));
                    kv(r, 6, "seq", &((sender << 40) | n).to_string());
                    kv(r, 6, "http.route", &format!("/api/v1/orders/{}", n % 37));
                    kv(r, 6, "user.id", &format!("u{}", n % 9973));
                    kv(r, 6, "thread.name", &format!("http-nio-8080-exec-{}", n % 16));
                    r.bytes(9, &trace_id(sender, n));
                    r.bytes(10, &((sender << 48) | (n + 1)).to_be_bytes());
                });
            }
        });
    });
    w.buf
}

/// docs/observe.md §13: an OTLP logs flood, as the metrics' (above): senders as fast as they are
/// answered, half to the server and half to the runner's receiver, a reader searching meanwhile.
/// What it proves: every record answered 200 is stored once (counted by its sender's host), none
/// dropped for room, and the replicas agree; what it measures: records a second, a request's time,
/// a search's under the flood, until the last records are found, and what each node spends (by
/// thread) and holds. `KOBS_LOGS_SECS` (20), `KOBS_LOGS_SENDERS` (8), `KOBS_LOGS_RECORDS` (500 a
/// request).
#[test]
#[ignore = "starts real processes and loads them"]
fn an_otlp_log_flood_is_taken_or_refused_never_lost_and_replicas_agree() -> anyhow::Result<()> {
    let secs = env("KOBS_LOGS_SECS", 20);
    let senders = env("KOBS_LOGS_SENDERS", 8);
    let records = env("KOBS_LOGS_RECORDS", 500);
    let mut f = Fleet::new()?;
    f.start_runner("r1")?;
    f.until(Duration::from_secs(90), "r1 a learner, caught up", |s| caught_up(s, &["r1"]))?;
    let ingest = std::fs::read_to_string(f.root.path().join("server").join("observe").join("ingest.token"))?.trim().to_owned();
    let r1 = *f.otlp.get("r1").ok_or_else(|| anyhow::anyhow!("r1's receiver"))?;
    let server_pid = f.server.as_ref().map_or(0, |p| p.child.id());
    let runner_pid = f.runners.get("r1").and_then(Option::as_ref).map_or(0, |p| p.child.id());
    let (s_cpu0, _) = usage(server_pid);
    let s_flt0 = MINFLT.with(std::cell::Cell::get);
    let (r_cpu0, _) = usage(runner_pid);
    let r_flt0 = MINFLT.with(std::cell::Cell::get);
    let (s_threads0, r_threads0) = (cpu_by_thread(server_pid), cpu_by_thread(runner_pid));
    let from_ms = common::now_ms() - 60_000;
    let stop = Arc::new(AtomicBool::new(false));
    let started = Instant::now();
    let mut threads = Vec::new();
    for sender in 0..senders {
        let (stop, ingest) = (stop.clone(), ingest.clone());
        let (port, auth) = if sender % 2 == 0 { (f.http, format!("Authorization: Bearer {ingest}\r\n")) } else { (r1, String::new()) };
        threads.push(std::thread::spawn(move || -> Sent {
            let mut sent = Sent::default();
            let mut conn = Conn::new(port);
            let headers = format!("{auth}Content-Type: application/x-protobuf\r\nContent-Encoding: gzip\r\n");
            let mut n: u64 = 0;
            while !stop.load(Ordering::Relaxed) {
                let body = gzip(&logs_request(sender, n, records, common::now_ms() as u64 * 1_000_000)).unwrap_or_default();
                let t = Instant::now();
                let answer = conn.send("/v1/logs", &headers, &body);
                sent.latencies_us.push(t.elapsed().as_micros() as u64);
                sent.requests += 1;
                match answer {
                    Ok(a) if a.code == 200 => {
                        sent.taken += records;
                        n += records;
                    }
                    Ok(a) if a.code == 503 => {
                        sent.refused += records;
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
    // What the logs explorer asks meanwhile: the last minute's errors, and its records by status.
    let reader = {
        let (stop, port, admin) = (stop.clone(), f.http, f.admin.clone());
        // Each read: what it took the reader, and what the server says it waited for the store and
        // took reading (µs).
        std::thread::spawn(move || -> (Vec<[u64; 3]>, Vec<[u64; 3]>, Vec<String>) {
            let (mut search, mut facets, mut errors) = (Vec::new(), Vec::new(), Vec::new());
            let mut conn = Conn::new(port);
            let headers = format!("Authorization: Bearer {admin}\r\nContent-Type: application/json\r\n");
            while !stop.load(Ordering::Relaxed) {
                let now = common::now_ms();
                for (path, body, into) in [
                    ("/api/observe/v1/logs/search", json!({ "query": "service:flood status:error", "from": now - 60_000, "to": now, "limit": 50 }), &mut search),
                    ("/api/observe/v1/logs/aggregate", json!({ "query": "service:flood", "from": now - 60_000, "to": now, "by": ["status"] }), &mut facets),
                ] {
                    let t = Instant::now();
                    match conn.send(path, &headers, &serde_json::to_vec(&body).unwrap_or_default()) {
                        Ok(a) if a.code == 200 => {
                            let v: serde_json::Value = serde_json::from_slice(&a.body).unwrap_or(serde_json::Value::Null);
                            let us = |k: &str| (at(&v, &[k]).as_f64().unwrap_or(0.0) * 1000.0) as u64;
                            into.push([t.elapsed().as_micros() as u64, us("waited_ms"), us("took_ms")]);
                        }
                        Ok(a) => errors.push(format!("{path}: {}", a.code)),
                        Err(e) => errors.push(format!("{path}: {e}")),
                    }
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            (search, facets, errors)
        })
    };
    std::thread::sleep(Duration::from_secs(secs));
    stop.store(true, Ordering::Relaxed);
    let flood = started.elapsed();
    let mut all = Sent::default();
    let mut taken_by: Vec<u64> = Vec::new();
    for t in threads {
        let s = t.join().map_err(|_| anyhow::anyhow!("a sender panicked"))?;
        taken_by.push(s.taken);
        all.taken += s.taken;
        all.refused += s.refused;
        all.requests += s.requests;
        all.errors.extend(s.errors);
        all.latencies_us.extend(s.latencies_us);
    }
    let (mut search, mut facets, read_errors) = reader.join().map_err(|_| anyhow::anyhow!("the reader panicked"))?;
    anyhow::ensure!(all.errors.is_empty(), "senders' errors (but 503s): {:?}\n{}", all.errors.iter().take(5).collect::<Vec<_>>(), f.logs());
    anyhow::ensure!(read_errors.is_empty(), "the reader's errors: {read_errors:?}");
    // Every record taken, stored once: each sender's count settles at what it had taken.
    let settled = Instant::now();
    let to_ms = common::now_ms() + 60_000;
    let mut have: Vec<u64> = vec![0; taken_by.len()];
    while settled.elapsed() < Duration::from_secs(180) {
        let a = f.call("POST", "/api/observe/v1/logs/aggregate", Some(&json!({ "query": "service:flood", "from": from_ms, "to": to_ms, "by": ["host"] })))?;
        have.iter_mut().for_each(|h| *h = 0);
        for g in at(&a, &["groups"]).as_array().into_iter().flatten() {
            let host = at(g, &["group"]).as_object().and_then(|o| o.values().next()).and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
            if let Some(h) = host.strip_prefix("load-").and_then(|n| n.parse::<usize>().ok()).and_then(|n| have.get_mut(n)) {
                *h = at(g, &["value"]).as_f64().unwrap_or(0.0) as u64;
            }
        }
        if have.iter().zip(&taken_by).all(|(h, t)| h >= t) {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let lag = settled.elapsed();
    let s = f.status()?;
    let dropped = at(&s, &["dropped"]).as_u64().unwrap_or(0) + at(&s, &["machines"]).as_array().into_iter().flatten().map(|m| at(m, &["status", "dropped"]).as_u64().unwrap_or(0)).sum::<u64>();
    anyhow::ensure!(dropped == 0, "{dropped} records dropped for room after being taken\n{}", f.logs());
    anyhow::ensure!(have == taken_by, "taken by sender {taken_by:?}, stored {have:?}\n{}", f.logs());
    let (s_cpu1, s_rss) = usage(server_pid);
    let s_flt = MINFLT.with(std::cell::Cell::get) - s_flt0;
    let (r_cpu1, r_rss) = usage(runner_pid);
    let r_flt = MINFLT.with(std::cell::Cell::get) - r_flt0;
    let (s_threads1, r_threads1) = (cpu_by_thread(server_pid), cpu_by_thread(runner_pid));
    let (index, digest) = f.verify(2)?;
    all.latencies_us.sort_unstable();
    let ms = |us: u64| us as f64 / 1000.0;
    // Each kind of read's whole time, its wait for the store and its reading, p50 and p99 of each.
    let parts = |reads: &mut Vec<[u64; 3]>| -> String {
        let mut out = Vec::new();
        for (i, what) in ["all", "waited", "reading"].iter().enumerate() {
            let mut xs: Vec<u64> = reads.iter().filter_map(|r| r.get(i).copied()).collect();
            xs.sort_unstable();
            out.push(format!("{what} p50 {:.2} ms, p99 {:.2} ms", ms(pct(&xs, 0.5)), ms(pct(&xs, 0.99))));
        }
        format!("{} ({} reads)", out.join("; "), reads.len())
    };
    eprintln!("\nan OTLP logs flood: {senders} senders × {records} records a request, gzip, {:.1} s", flood.as_secs_f64());
    eprintln!("  taken      {:>12} records, {:>10.0} records/s, {} requests", all.taken, all.taken as f64 / flood.as_secs_f64(), all.requests);
    eprintln!("  refused    {:>12} records (503, sent again when told), {:.2}% of requests", all.refused, 100.0 * (all.refused / records.max(1)) as f64 / all.requests.max(1) as f64);
    eprintln!("  a request  p50 {:.2} ms, p99 {:.2} ms, max {:.2} ms", ms(pct(&all.latencies_us, 0.5)), ms(pct(&all.latencies_us, 0.99)), ms(pct(&all.latencies_us, 1.0)));
    eprintln!("  stored     every sender's records counted {:.0} ms after the flood", lag.as_secs_f64() * 1000.0);
    eprintln!("  errors     the last minute's newest 50: {}", parts(&mut search));
    eprintln!("  by status  the last minute counted: {}", parts(&mut facets));
    eprintln!(
        "  server     {:.1} CPU s ({:.2} cores), peak RSS {:.0} MiB, {:.4} minor faults a record",
        s_cpu1 - s_cpu0,
        (s_cpu1 - s_cpu0) / flood.as_secs_f64(),
        s_rss as f64 / 1048576.0,
        s_flt as f64 / all.taken.max(1) as f64
    );
    eprintln!("    by thread: {}", spent(&s_threads0, &s_threads1));
    eprintln!(
        "  runner     {:.1} CPU s ({:.2} cores), peak RSS {:.0} MiB, {:.4} minor faults a record",
        r_cpu1 - r_cpu0,
        (r_cpu1 - r_cpu0) / flood.as_secs_f64(),
        r_rss as f64 / 1048576.0,
        r_flt as f64 / all.taken.max(1) as f64
    );
    eprintln!("    by thread: {}", spent(&r_threads0, &r_threads1));
    eprintln!("  replicas   agree at index {index}: {digest:016x}");
    f.stop_gently();
    Ok(())
}

/// An OTLP traces request of `traces` traces of `sender`'s service (`load-svc-<sender>`), trace `n`
/// on from `first`, each a server's span, its call out and its database's query (both clients'):
/// every id its own, the times now's.
fn traces_request(sender: u64, first: u64, traces: u64, now_ns: u64) -> Vec<u8> {
    let mut w = Writer::default();
    w.message(1, |rs| {
        rs.message(1, |res| {
            kv(res, 1, "service.name", &format!("load-svc-{sender}"));
            kv(res, 1, "host.name", &format!("load-{sender}"));
            kv(res, 1, "deployment.environment.name", "load");
        });
        rs.message(2, |ss| {
            ss.message(1, |sc| sc.str(1, "load"));
            for t in first..first + traces {
                let trace = trace_id(sender, t);
                let span = |k: u64| ((sender << 48) | (t * 3 + k + 1)).to_be_bytes();
                let start = now_ns + (t - first) * 1_000;
                for (k, kind, name, parent) in [(0u64, 2u64, "GET /orders/{id}", None), (1, 3, "GET", Some(0u64)), (2, 3, "SELECT orders", Some(1u64))] {
                    ss.message(2, |sp| {
                        sp.bytes(1, &trace);
                        sp.bytes(2, &span(k));
                        if let Some(p) = parent {
                            sp.bytes(4, &span(p));
                        }
                        sp.str(5, name);
                        sp.varint(6, kind);
                        sp.fixed64(7, start + k * 100_000);
                        sp.fixed64(8, start + 3_000_000 - k * 1_000_000);
                        match k {
                            0 => kv(sp, 9, "http.route", "/orders/{id}"),
                            1 => kv(sp, 9, "server.address", "orders.internal"),
                            _ => kv(sp, 9, "db.system.name", "postgresql"),
                        }
                    });
                }
            }
        });
    });
    w.buf
}

/// Trace `t` of `sender`'s id.
fn trace_id(sender: u64, t: u64) -> [u8; 16] {
    let mut id = [0u8; 16];
    let (hi, lo) = id.split_at_mut(8);
    hi.copy_from_slice(&(sender + 1).to_be_bytes());
    lo.copy_from_slice(&(t + 1).to_be_bytes());
    id
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// docs/observe.md §13: traces under load, end to end. Senders flood OTLP/HTTP trace requests
/// (protobuf, gzip, kept alive) as fast as they are answered, half to the server and half to the
/// runner's receiver, while a reader looks traces up by their ids and asks for the services. What it
/// proves: every span answered 200 is stored once (one answered 503 was not taken, and is sent
/// again), each trace whole and found by its id; Datadog's trace metrics count each entry span and
/// each call out once; every replica holds the same. What it measures: spans a second taken, how
/// long an answer and a trace's lookup take, how long until a trace can be read, and the server's
/// and the runner's CPU, memory and page faults.
///
/// `cargo test --release -p kiln --test observe_load -- --ignored --nocapture a_trace_flood`, with
/// `KOBS_LOAD_SECS` (20), `KOBS_LOAD_SENDERS` (8), `KOBS_LOAD_TRACES` (170 a request: 510 spans, a
/// batch span processor's 512 at the most).
#[test]
#[ignore = "starts real processes and loads them"]
fn a_trace_flood_is_taken_or_refused_never_lost_counted_once_and_replicas_agree() -> anyhow::Result<()> {
    let secs = env("KOBS_LOAD_SECS", 20);
    let senders = env("KOBS_LOAD_SENDERS", 8);
    let traces = env("KOBS_LOAD_TRACES", 170);
    let mut f = Fleet::new()?;
    f.start_runner("r1")?;
    f.until(Duration::from_secs(90), "r1 a learner, caught up", |s| caught_up(s, &["r1"]))?;
    let ingest = std::fs::read_to_string(f.root.path().join("server").join("observe").join("ingest.token"))?.trim().to_owned();
    let r1 = *f.otlp.get("r1").ok_or_else(|| anyhow::anyhow!("r1's receiver"))?;
    let server_pid = f.server.as_ref().map_or(0, |p| p.child.id());
    let runner_pid = f.runners.get("r1").and_then(Option::as_ref).map_or(0, |p| p.child.id());
    let (s_cpu0, _) = usage(server_pid);
    let s_flt0 = MINFLT.with(std::cell::Cell::get);
    let (r_cpu0, _) = usage(runner_pid);
    let r_flt0 = MINFLT.with(std::cell::Cell::get);
    let (s_threads0, r_threads0) = (cpu_by_thread(server_pid), cpu_by_thread(runner_pid));

    let t0_ms = common::now_ms();
    let stop = Arc::new(AtomicBool::new(false));
    // Each sender's last trace taken: what the reader looks up.
    let latest: Arc<std::sync::Mutex<Vec<Option<[u8; 16]>>>> = Arc::new(std::sync::Mutex::new(vec![None; senders as usize]));
    let started = Instant::now();
    let mut threads = Vec::new();
    for sender in 0..senders {
        let (stop, ingest, latest) = (stop.clone(), ingest.clone(), latest.clone());
        let (port, auth) = if sender % 2 == 0 { (f.http, format!("Authorization: Bearer {ingest}\r\n")) } else { (r1, String::new()) };
        threads.push(std::thread::spawn(move || -> (Sent, u64) {
            let mut sent = Sent::default();
            let mut conn = Conn::new(port);
            let headers = format!("{auth}Content-Type: application/x-protobuf\r\nContent-Encoding: gzip\r\n");
            let mut next = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let now_ns = common::now_ms() as u64 * 1_000_000;
                let body = traces_request(sender, next, traces, now_ns);
                let body = gzip(&body).unwrap_or(body);
                let t = Instant::now();
                let answer = conn.send("/v1/traces", &headers, &body);
                sent.latencies_us.push(t.elapsed().as_micros() as u64);
                sent.requests += 1;
                match answer {
                    // A partial success says spans were refused: none should be.
                    Ok(a) if a.code == 200 && a.body.is_empty() => {
                        sent.taken += traces * 3;
                        next += traces;
                        if let Ok(mut l) = latest.lock()
                            && let Some(slot) = l.get_mut(sender as usize)
                        {
                            *slot = Some(trace_id(sender, next - 1));
                        }
                    }
                    Ok(a) if a.code == 503 => {
                        sent.refused += traces * 3;
                        std::thread::sleep(Duration::from_millis(a.retry_after.map_or(100, |s| s * 1000).min(2_000)));
                    }
                    Ok(a) => sent.errors.push(format!("{}: {}", a.code, String::from_utf8_lossy(&a.body))),
                    Err(e) => sent.errors.push(e.to_string()),
                }
                if sent.errors.len() > 20 {
                    break;
                }
            }
            (sent, next)
        }));
    }
    // A trace looked up by its id meanwhile, each sender's newest in turn, and the services.
    let reader = {
        let (stop, port, admin, latest) = (stop.clone(), f.http, f.admin.clone(), latest.clone());
        std::thread::spawn(move || -> (Vec<u64>, Vec<u64>, u64, Vec<String>) {
            let (mut lookups, mut services, mut not_yet, mut errors) = (Vec::new(), Vec::new(), 0u64, Vec::new());
            let mut turn = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let id = latest.lock().ok().and_then(|l| l.get(turn % l.len().max(1)).copied().flatten());
                turn += 1;
                if let Some(id) = id {
                    match timed_get(port, &format!("/api/observe/v1/traces/{}", hex(&id)), &admin, Duration::from_secs(30)) {
                        Ok(d) => lookups.push(d.as_micros() as u64),
                        // Taken and not yet written: a lookup of a trace a moment too soon.
                        Err(e) if e.contains("404") => not_yet += 1,
                        Err(e) => errors.push(format!("trace: {e}")),
                    }
                }
                if turn.is_multiple_of(8) {
                    let to = common::now_ms();
                    match timed_get(port, &format!("/api/observe/v1/apm/services?from={}&to={to}", to - 900_000), &admin, Duration::from_secs(30)) {
                        Ok(d) => services.push(d.as_micros() as u64),
                        Err(e) => errors.push(format!("services: {e}")),
                    }
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            (lookups, services, not_yet, errors)
        })
    };
    std::thread::sleep(Duration::from_secs(secs));
    stop.store(true, Ordering::Relaxed);
    let flood = started.elapsed();
    let mut all = Sent::default();
    let mut sent_traces: Vec<u64> = Vec::new();
    for t in threads {
        let (s, n) = t.join().map_err(|_| anyhow::anyhow!("a sender panicked"))?;
        all.taken += s.taken;
        all.refused += s.refused;
        all.requests += s.requests;
        all.errors.extend(s.errors);
        all.latencies_us.extend(s.latencies_us);
        sent_traces.push(n);
    }
    let (mut lookups, mut services, not_yet, read_errors) = reader.join().map_err(|_| anyhow::anyhow!("the reader panicked"))?;
    anyhow::ensure!(all.errors.is_empty(), "senders' errors (but 503s): {:?}\n{}", all.errors.iter().take(5).collect::<Vec<_>>(), f.logs());
    anyhow::ensure!(read_errors.is_empty(), "the reader's errors: {:?}", read_errors.iter().take(5).collect::<Vec<_>>());

    // Readable: each sender's newest trace, whole, by its id.
    let settled = Instant::now();
    for (sender, n) in sent_traces.iter().enumerate() {
        let Some(last) = n.checked_sub(1) else { continue };
        let id = hex(&trace_id(sender as u64, last));
        loop {
            let t = f.call("GET", &format!("/api/observe/v1/traces/{id}"), None).ok();
            if t.as_ref().is_some_and(|t| at(t, &["spans"]).as_array().is_some_and(|s| s.len() == 3)) {
                break;
            }
            anyhow::ensure!(settled.elapsed() < Duration::from_secs(120), "sender {sender}'s newest trace never readable\n{}", f.logs());
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let lag = settled.elapsed();
    // Every span taken stored once: their count settles at what was taken, no more, no less.
    let (from, to) = (t0_ms - 60_000, common::now_ms() + 60_000);
    let count = |q: &str| -> anyhow::Result<u64> {
        let a = f.call("POST", "/api/observe/v1/spans/aggregate", Some(&json!({ "query": q, "from": from, "to": to, "measure": "count" })))?;
        Ok(at(&a, &["groups"]).as_array().into_iter().flatten().filter_map(|g| at(g, &["value"]).as_f64()).sum::<f64>().round() as u64)
    };
    let mut have = 0;
    while settled.elapsed() < Duration::from_secs(180) {
        have = count("service:load-svc-*")?;
        if have >= all.taken {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    anyhow::ensure!(have == all.taken, "{} spans taken, {have} stored\n{}", all.taken, f.logs());
    // Datadog's trace metrics: each trace's request an entry, its two calls exits, counted once as
    // their windows settle.
    let taken_traces = all.taken / 3;
    let metric = |q: &str| -> anyhow::Result<f64> {
        let a = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": q, "from": from, "to": to, "step": 10_000 })))?;
        Ok(at(&a, &["series"]).as_array().into_iter().flatten().filter(|s| at(s, &["tags", "service"]).as_str().is_some_and(|v| v.starts_with("load-svc-"))).flat_map(|s| at(s, &["values"]).as_array().cloned().unwrap_or_default()).filter_map(|v| v.as_f64()).sum())
    };
    let counted_at = Instant::now();
    let (mut hits, mut outbound) = (0.0, 0.0);
    while counted_at.elapsed() < Duration::from_secs(120) {
        hits = metric("sum:trace.hits{*} by {service}.as_count()")?;
        outbound = metric("sum:trace.outbound.hits{*} by {service}.as_count()")?;
        if hits >= taken_traces as f64 && outbound >= 2.0 * taken_traces as f64 {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let counted = counted_at.elapsed();
    anyhow::ensure!(hits == taken_traces as f64 && outbound == 2.0 * taken_traces as f64, "{taken_traces} traces taken: trace.hits {hits}, trace.outbound.hits {outbound}\n{}", f.logs());
    let s = f.status()?;
    let dropped = at(&s, &["dropped"]).as_u64().unwrap_or(0) + at(&s, &["machines"]).as_array().into_iter().flatten().map(|m| at(m, &["status", "dropped"]).as_u64().unwrap_or(0)).sum::<u64>();
    anyhow::ensure!(dropped == 0, "{dropped} dropped for room after being taken\n{}", f.logs());
    let (s_cpu1, s_rss) = usage(server_pid);
    let s_flt = MINFLT.with(std::cell::Cell::get) - s_flt0;
    let (r_cpu1, r_rss) = usage(runner_pid);
    let r_flt = MINFLT.with(std::cell::Cell::get) - r_flt0;
    let (s_threads1, r_threads1) = (cpu_by_thread(server_pid), cpu_by_thread(runner_pid));
    let (index, digest) = f.verify(2)?;

    all.latencies_us.sort_unstable();
    lookups.sort_unstable();
    services.sort_unstable();
    let ms = |us: u64| us as f64 / 1000.0;
    eprintln!("\ntraces under load: {senders} senders × {} spans a request (gzip), {:.1} s", traces * 3, flood.as_secs_f64());
    eprintln!("  taken      {:>12} spans, {:>10.0} spans/s, {} requests", all.taken, all.taken as f64 / flood.as_secs_f64(), all.requests);
    eprintln!("  refused    {:>12} spans (503, sent again when told), {:.2}% of requests", all.refused, 100.0 * (all.refused / (traces * 3).max(1)) as f64 / all.requests.max(1) as f64);
    eprintln!("  a request  p50 {:.2} ms, p99 {:.2} ms, max {:.2} ms", ms(pct(&all.latencies_us, 0.5)), ms(pct(&all.latencies_us, 0.99)), ms(pct(&all.latencies_us, 1.0)));
    eprintln!("  a trace    by its id, p50 {:.2} ms, p99 {:.2} ms ({} lookups, {not_yet} a moment too soon)", ms(pct(&lookups, 0.5)), ms(pct(&lookups, 0.99)), lookups.len());
    eprintln!("  services   p50 {:.2} ms, p99 {:.2} ms ({} reads)", ms(pct(&services, 0.5)), ms(pct(&services, 0.99)), services.len());
    eprintln!("  readable   every sender's newest trace {:.0} ms after the flood; trace metrics counted {:.1} s after", lag.as_secs_f64() * 1000.0, counted.as_secs_f64());
    eprintln!("  server     {:.1} CPU s ({:.2} cores), peak RSS {:.0} MiB, {:.4} minor faults a span", s_cpu1 - s_cpu0, (s_cpu1 - s_cpu0) / flood.as_secs_f64(), s_rss as f64 / 1048576.0, s_flt as f64 / all.taken.max(1) as f64);
    eprintln!("    by thread: {}", spent(&s_threads0, &s_threads1));
    eprintln!("  runner     {:.1} CPU s ({:.2} cores), peak RSS {:.0} MiB, {:.4} minor faults a span", r_cpu1 - r_cpu0, (r_cpu1 - r_cpu0) / flood.as_secs_f64(), r_rss as f64 / 1048576.0, r_flt as f64 / all.taken.max(1) as f64);
    eprintln!("    by thread: {}", spent(&r_threads0, &r_threads1));
    eprintln!("  where      the server's {}; the runner's {}", anon_by_mapping(server_pid), anon_by_mapping(runner_pid));
    eprintln!("  replicas   agree at index {index}: {digest:016x}");
    f.stop_gently();
    Ok(())
}

/// A runner flooded while its server is away (killed: SIGKILL) keeps taking only while its queue's
/// wait is short, then refuses (503) in a couple of seconds, its memory bounded; the server back,
/// it drains, and everything it took is stored once, every replica alike.
#[test]
#[ignore = "starts real processes and kills one"]
fn a_runner_whose_server_is_away_refuses_in_time_and_loses_nothing_it_took() -> anyhow::Result<()> {
    let series = env("KOBS_LOAD_SERIES", 1000);
    let mut f = Fleet::new()?;
    f.start_runner("r1")?;
    f.until(Duration::from_secs(90), "r1 a learner, caught up", |s| caught_up(s, &["r1"]))?;
    let r1 = *f.otlp.get("r1").ok_or_else(|| anyhow::anyhow!("r1's receiver"))?;
    let runner_pid = f.runners.get("r1").and_then(Option::as_ref).map_or(0, |p| p.child.id());
    let t0_ms = common::now_ms() - 7_200_000;
    let stop = Arc::new(AtomicBool::new(false));
    // When each sender was first refused, as ms since the server went away (0: not yet).
    let away = Arc::new(std::sync::Mutex::new(None::<Instant>));
    let senders: Vec<_> = (0..2u64)
        .map(|sender| {
            let (stop, away) = (stop.clone(), away.clone());
            std::thread::spawn(move || -> (Sent, Option<Duration>) {
                let mut sent = Sent::default();
                let mut conn = Conn::new(r1);
                let mut first_refusal = None;
                let mut i: u64 = 0;
                // KOBS_LOAD_RATE: requests a second a sender (none: as fast as answered).
                let pace = std::env::var("KOBS_LOAD_RATE").ok().and_then(|r| r.parse::<f64>().ok()).map(|r| Duration::from_secs_f64(1.0 / r));
                let began = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    if let Some(p) = pace {
                        let due = p.mul_f64(sent.requests as f64);
                        if let Some(wait) = due.checked_sub(began.elapsed()) {
                            std::thread::sleep(wait);
                        }
                    }
                    let ts_ns = (t0_ms as u64 + i) * 1_000_000;
                    let body = gzip(&request(sender, series, ts_ns)).unwrap_or_default();
                    match conn.send("/v1/metrics", "Content-Type: application/x-protobuf\r\nContent-Encoding: gzip\r\n", &body) {
                        Ok(a) if a.code == 200 => {
                            sent.taken += series;
                            sent.last_ms = Some((ts_ns / 1_000_000) as i64);
                            i += 1;
                        }
                        Ok(a) if a.code == 503 => {
                            sent.refused += series;
                            if first_refusal.is_none()
                                && let Some(t) = *away.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                            {
                                first_refusal = Some(t.elapsed());
                            }
                            std::thread::sleep(Duration::from_millis(a.retry_after.map_or(100, |s| s * 1000).min(2_000)));
                        }
                        Ok(a) => sent.errors.push(format!("{}: {}", a.code, String::from_utf8_lossy(&a.body))),
                        Err(e) => sent.errors.push(e.to_string()),
                    }
                    sent.requests += 1;
                    if sent.errors.len() > 20 {
                        break;
                    }
                }
                (sent, first_refusal)
            })
        })
        .collect();
    std::thread::sleep(Duration::from_secs(3));
    f.kill_server();
    *away.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
    std::thread::sleep(Duration::from_secs(10));
    let (_, away_rss) = usage(runner_pid);
    f.start_server()?;
    std::thread::sleep(Duration::from_secs(8));
    stop.store(true, Ordering::Relaxed);
    let mut all = Sent::default();
    let mut refusals = Vec::new();
    for t in senders {
        let (s, first) = t.join().map_err(|_| anyhow::anyhow!("a sender panicked"))?;
        all.taken += s.taken;
        all.refused += s.refused;
        all.requests += s.requests;
        all.errors.extend(s.errors);
        refusals.push(first);
    }
    anyhow::ensure!(all.errors.is_empty(), "senders' errors (but 503s): {:?}\n{}", all.errors.iter().take(5).collect::<Vec<_>>(), f.logs());
    let first = refusals.iter().flatten().min().copied().ok_or_else(|| anyhow::anyhow!("no refusal while the server was away\n{}", f.logs()))?;
    // Everything taken, stored once.
    let to = t0_ms + 3_600_000;
    let settled = Instant::now();
    let mut have = 0;
    while settled.elapsed() < Duration::from_secs(120) {
        have = stored(&f, t0_ms - 60_000, to)?;
        if have >= all.taken {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let s = f.status()?;
    let dropped = at(&s, &["dropped"]).as_u64().unwrap_or(0) + at(&s, &["machines"]).as_array().into_iter().flatten().map(|m| at(m, &["status", "dropped"]).as_u64().unwrap_or(0)).sum::<u64>();
    anyhow::ensure!(dropped == 0 && have == all.taken, "{} taken, {have} stored, {dropped} dropped\n{}", all.taken, f.logs());
    f.until(Duration::from_secs(60), "r1 caught up again", |s| caught_up(s, &["r1"]))?;
    let (index, digest) = f.verify(2)?;
    eprintln!("\na runner's server away for 10 s, two senders × {series} points a request");
    eprintln!("  first refused {:.2} s after the server went away (the queue's target: {:?} kept for {:?})", first.as_secs_f64(), Duration::from_secs(1), Duration::from_secs(1));
    eprintln!("  taken {} points, refused {} (sent again), {} requests; the runner's peak RSS {:.0} MiB", all.taken, all.refused, all.requests, away_rss as f64 / 1048576.0);
    eprintln!("  all of it stored once; replicas agree at index {index}: {digest:016x}");
    anyhow::ensure!(first < Duration::from_secs(5), "refused only {first:?} after the server went away");
    Ok(())
}

/// This machine's dynamic loader, wherever its distribution keeps it (`/usr/lib`, `/usr/lib64`,
/// `/usr/lib/x86_64-linux-gnu`), found from the path its ABI names: its folder (its libraries
/// beside it) and its name.
fn host_loader() -> anyhow::Result<(String, String)> {
    let abi = if cfg!(target_arch = "aarch64") { "/lib/ld-linux-aarch64.so.1" } else { "/lib64/ld-linux-x86-64.so.2" };
    let real = std::fs::canonicalize(abi)?;
    let dir = real.parent().and_then(|d| d.to_str()).ok_or_else(|| anyhow::anyhow!("{}: no folder", real.display()))?;
    let name = real.file_name().and_then(|n| n.to_str()).ok_or_else(|| anyhow::anyhow!("{}: no name", real.display()))?;
    Ok((dir.to_owned(), name.to_owned()))
}

/// A container's last lines, what it wrote to either stream.
fn container_log(name: &str) -> String {
    std::process::Command::new("docker")
        .args(["logs", "--tail", "80", name])
        .output()
        .map(|o| format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
        .unwrap_or_default()
}

/// Runs `docker` with `args`, its output.
fn docker(args: &[&str]) -> anyhow::Result<String> {
    let out = std::process::Command::new("docker").args(args).output()?;
    anyhow::ensure!(out.status.success(), "docker {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// A server whose disk fills (its data on a 96 MiB tmpfs in a container, a 30 MiB balloon file
/// beside it): it refuses exporters before the disk is full (room kept for the cut that frees its
/// log), never spins while refusing, takes again once room is freed, and stores everything it took.
/// The container runs this machine's build with this machine's loader and libraries (mounted
/// read-only), so it needs Docker and an image with a shell (`KOBS_FULLDISK_IMAGE`).
#[test]
#[ignore = "needs Docker; fills a disk"]
fn a_full_disk_is_refused_in_time_never_spun_on_and_nothing_it_took_is_lost() -> anyhow::Result<()> {
    let image = std::env::var("KOBS_FULLDISK_IMAGE").unwrap_or_else(|_| "mcr.microsoft.com/playwright:v1.64.1-noble".into());
    let port = common::free_port()?;
    let dir = tempfile::tempdir()?;
    let cfg = dir.path().join("kiln.toml");
    std::fs::write(
        &cfg,
        format!(
            "[server]\nhttp_addr = \"127.0.0.1:{port}\"\nquic_addr = \"127.0.0.1:{}\"\npublic_url = \"http://127.0.0.1:{port}\"\ndata_dir = \"/data/server\"\n[gitops]\nauto_pipelines = false\n[local_agent]\nenabled = false\n[discovery]\nenabled = false\n[kubernetes]\nenabled = false\n[cd]\nenabled = false\n[observe]\nenabled = true\nhost_seconds = 3600\n",
            common::free_port()?
        ),
    )?;
    let name = format!("kiln-fulldisk-{port}");
    let bin = common::BIN;
    let (libs, loader) = host_loader()?;
    let _ = docker(&["rm", "-f", &name]);
    docker(&[
        "run", "-d", "--name", &name, "--network", "host", "--user", "1000:1000", "--tmpfs", "/data:size=96m,mode=1777,uid=1000,gid=1000",
        "-v", &format!("{bin}:/work/kiln:ro"), "-v", &format!("{libs}:/hostlib:ro"), "-v", &format!("{}:/cfg/kiln.toml:ro", cfg.display()), "-e", "HOME=/data/home",
        // No UI server: the one kiln carries unpacks into its data folder, a third of this disk.
        "-e", "KILN_UI_DIR=/nonexistent",
        "--entrypoint", &format!("/hostlib/{loader}"), &image, "--library-path", "/hostlib", "/work/kiln", "--config", "/cfg/kiln.toml", "server",
    ])?;
    struct Gone(String);
    impl Drop for Gone {
        fn drop(&mut self) {
            let _ = std::process::Command::new("docker").args(["rm", "-f", &self.0]).output();
        }
    }
    let _gone = Gone(name.clone());
    let start = Instant::now();
    while common::raw(port, "GET", "/healthz", "", b"").map_or(true, |(c, _)| c != 200) {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "the server did not come up: {}", docker(&["logs", "--tail", "20", &name]).unwrap_or_default());
        std::thread::sleep(Duration::from_millis(200));
    }
    let ingest = docker(&["exec", &name, "cat", "/data/server/observe/ingest.token"])?;
    let admin = docker(&["exec", &name, "cat", "/data/server/admin.token"])?;
    docker(&["exec", &name, "dd", "if=/dev/zero", "of=/data/balloon", "bs=1M", "count=30"])?;
    let pid: u32 = docker(&["inspect", "-f", "{{.State.Pid}}", &name])?.parse()?;
    let t0_ms = common::now_ms() - 7_200_000;
    // Points as fast as the server takes them, for `at_most`, or (`until_refused`) until it refuses
    // and two seconds more: however fast this machine fills the disk.
    // Each request taken, by sender: its index and when (ms into the test), to name any lost.
    let begun = Instant::now();
    let took: Arc<std::sync::Mutex<Vec<Vec<(u64, u64)>>>> = Arc::new(std::sync::Mutex::new(vec![Vec::new(), Vec::new()]));
    let flood = |from_i: u64, at_most: Duration, until_refused: bool| -> (u64, u64, u64) {
        let stop = Arc::new(AtomicBool::new(false));
        let refusals = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let senders: Vec<_> = (0..2u64)
            .map(|sender| {
                let (stop, ingest, refusals, took) = (stop.clone(), ingest.clone(), refusals.clone(), took.clone());
                std::thread::spawn(move || {
                    let (mut taken, mut refused, mut i) = (0u64, 0u64, from_i);
                    let mut mine = Vec::new();
                    let mut conn = Conn::new(port);
                    let headers = format!("Authorization: Bearer {ingest}\r\nContent-Type: application/x-protobuf\r\nContent-Encoding: gzip\r\n");
                    while !stop.load(Ordering::Relaxed) {
                        let body = gzip(&request(sender, 1000, (t0_ms as u64 + i) * 1_000_000)).unwrap_or_default();
                        match conn.send("/v1/metrics", &headers, &body) {
                            Ok(a) if a.code == 200 => {
                                taken += 1000;
                                mine.push((i, begun.elapsed().as_millis() as u64));
                                i += 1;
                            }
                            Ok(a) if a.code == 503 => {
                                refused += 1000;
                                refusals.fetch_add(1, Ordering::Relaxed);
                                std::thread::sleep(Duration::from_millis(500));
                            }
                            _ => std::thread::sleep(Duration::from_millis(500)),
                        }
                    }
                    if let Some(all) = took.lock().ok().as_mut().and_then(|t| t.get_mut(sender as usize)) {
                        all.extend(mine);
                    }
                    (taken, refused, i)
                })
            })
            .collect();
        let start = Instant::now();
        while start.elapsed() < at_most && !(until_refused && refusals.load(Ordering::Relaxed) > 0) {
            std::thread::sleep(Duration::from_millis(100));
        }
        if until_refused {
            std::thread::sleep(Duration::from_secs(2));
        }
        stop.store(true, Ordering::Relaxed);
        senders.into_iter().filter_map(|t| t.join().ok()).fold((0, 0, 0), |a, (t, r, i)| (a.0 + t, a.1 + r, a.2.max(i)))
    };
    let filling = Instant::now();
    let (taken, refused, last_i) = flood(0, Duration::from_secs(120), true);
    let filled = filling.elapsed();
    anyhow::ensure!(refused > 0, "never refused: {taken} taken on a 96 MiB disk in {filled:?}");
    // Refusing, it does not spin: under a tenth of a core over five seconds.
    let (c0, _) = usage(pid);
    std::thread::sleep(Duration::from_secs(5));
    let (c1, _) = usage(pid);
    anyhow::ensure!(c1 - c0 < 0.5, "{:.2} CPU s in 5 s while the disk was full", c1 - c0);
    // Room freed: it takes again.
    docker(&["exec", &name, "rm", "/data/balloon"])?;
    let (more, _, _) = flood(last_i + 1, Duration::from_secs(15), false);
    anyhow::ensure!(more > 0, "nothing taken once room was freed");
    // Everything it took, stored: once written (a point answered 200 is taken, and written a flush
    // and a write later).
    let total = || -> anyhow::Result<f64> {
        let body = serde_json::to_vec(&json!({ "query": "sum:load.v{*}.rollup(sum)", "from": t0_ms - 60_000, "to": t0_ms + 3_600_000, "points": 100 }))?;
        let (code, b) = common::raw(port, "POST", "/api/observe/v1/query", &format!("Authorization: Bearer {admin}\r\nContent-Type: application/json\r\n"), &body)?;
        anyhow::ensure!(code == 200, "{code}");
        let a: serde_json::Value = serde_json::from_slice(&b)?;
        Ok(at(&a, &["series"]).as_array().into_iter().flatten().flat_map(|s| at(s, &["values"]).as_array().cloned().unwrap_or_default()).filter_map(|v| v.as_f64()).sum())
    };
    let waited = Instant::now();
    let mut stored = total()?;
    while stored.round() as u64 != taken + more && waited.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(500));
        stored = total()?;
    }
    if stored.round() as u64 != taken + more {
        // Where: each series' count (each sender's should be its requests taken), then the first
        // short one read a millisecond (a request) at a time.
        let ask = |q: &str, from: i64, to: i64, step: Option<i64>| -> anyhow::Result<serde_json::Value> {
            let req = match step {
                Some(s) => json!({ "query": q, "from": from, "to": to, "max_series": 10_000, "step": s }),
                None => json!({ "query": q, "from": from, "to": to, "max_series": 10_000, "points": 1 }),
            };
            let (code, b) = common::raw(port, "POST", "/api/observe/v1/query", &format!("Authorization: Bearer {admin}\r\nContent-Type: application/json\r\n"), &serde_json::to_vec(&req)?)?;
            anyhow::ensure!(code == 200, "{code} {}", String::from_utf8_lossy(&b));
            Ok(serde_json::from_slice(&b)?)
        };
        let took = took.lock().map_err(|_| anyhow::anyhow!("poisoned"))?.clone();
        let counts = ask("sum:load.v{*} by {host,s}.rollup(sum)", t0_ms - 60_000, t0_ms + 3_600_000, None)?;
        let mut short = Vec::new();
        for series in at(&counts, &["series"]).as_array().into_iter().flatten() {
            let (host, s) = (at(series, &["tags", "host"]).as_str().unwrap_or(""), at(series, &["tags", "s"]).as_str().unwrap_or(""));
            let sender: usize = host.strip_prefix("load-").and_then(|v| v.parse().ok()).unwrap_or(0);
            let n: f64 = at(series, &["values"]).as_array().into_iter().flatten().filter_map(|v| v.as_f64()).sum();
            let want = took.get(sender).map_or(0, Vec::len) as f64;
            if (n - want).abs() > 0.5 {
                short.push((format!("host:{host},s:{s}"), sender, n, want));
            }
        }
        let series_n = at(&counts, &["series"]).as_array().map_or(0, Vec::len);
        // One series' total at each step: raw points (1 ms) and coarser.
        let one = "sum:load.v{host:load-0,s:0}.rollup(sum)";
        let mut by_step = Vec::new();
        for step in [1_000i64, 10_000, 60_000, 600_000, 3_600_000] {
            let a = ask(one, t0_ms - 3_600_000, t0_ms + 3_600_000, Some(step))?;
            let n: f64 = at(&a, &["series"]).as_array().into_iter().flatten().flat_map(|s| at(s, &["values"]).as_array().cloned().unwrap_or_default()).filter_map(|v| v.as_f64()).sum();
            by_step.push((step, n));
        }
        let mut missing = Vec::new();
        if let Some((tags, sender, _, _)) = short.first() {
            let mine = took.get(*sender).cloned().unwrap_or_default();
            let last = mine.iter().map(|&(i, _)| i).max().unwrap_or(0);
            let mut present = std::collections::HashSet::new();
            let mut from = 0;
            while from <= last {
                let a = ask(&format!("sum:load.v{{{tags}}}.rollup(sum)"), t0_ms + from as i64, t0_ms + (from + 10_000) as i64, Some(1))?;
                let start = at(&a, &["grid", "from"]).as_i64().unwrap_or(0);
                let step = at(&a, &["grid", "step"]).as_i64().unwrap_or(1).max(1);
                for s in at(&a, &["series"]).as_array().into_iter().flatten() {
                    for (k, v) in at(s, &["values"]).as_array().into_iter().flatten().enumerate() {
                        if v.as_f64().is_some() {
                            present.insert(((start + k as i64 * step) - t0_ms) as u64);
                        }
                    }
                }
                from += 10_000;
            }
            missing.extend(mine.iter().filter(|(i, _)| !present.contains(i)).copied());
        }
        anyhow::bail!(
            "{} taken, {stored} stored after 30 s; {series_n} series, {} short (tags, sender, stored, taken) {:?}; the first's missing (index, ms taken): {} {:?}; host:load-0,s:0 by step (ms, sum) {by_step:?} of {} taken; first refused at {:.1} s\n{}",
            taken + more,
            short.len(),
            short.iter().take(12).collect::<Vec<_>>(),
            missing.len(),
            missing.iter().take(30).collect::<Vec<_>>(),
            took.first().map_or(0, Vec::len),
            filled.as_secs_f64(),
            container_log(&name)
        );
    }
    eprintln!(
        "\na full disk: {taken} points taken in {:.1} s, then refused; idle while full ({:.2} CPU s in 5 s); {more} more once 30 MiB were freed; all {} stored",
        filled.as_secs_f64(),
        c1 - c0,
        taken + more
    );
    Ok(())
}

/// A sample of a soak: when, each process's resident and anonymous memory, the server's store
/// (head and index bytes, blocks, its log's bytes), and what had been taken by then.
struct Sampled {
    at_s: f64,
    server: (f64, f64),
    runner: (f64, f64),
    head_mib: f64,
    index_mib: f64,
    blocks: u64,
    log_mib: f64,
    taken: u64,
}

/// docs/observe.md §13, the soak: a steady load held for long, at a rate a busy fleet sends, half to
/// the server and half to the runner's receiver, log records beside it, a dashboard reading every
/// 2 s and a monitor evaluating every 10 s. What it proves: every point and record taken is stored
/// once and the replicas agree at the end; the store cuts and the log compacts all through; and
/// each process's memory holds a level, its run's last third no higher than its second by more than
/// a tenth (a leak climbs; a high-water mark, a buffer grown once, does not).
#[test]
#[ignore = "starts real processes and loads them for minutes"]
fn a_steady_load_held_for_long_keeps_its_memory_level_and_loses_nothing() -> anyhow::Result<()> {
    use std::sync::atomic::AtomicU64;
    let secs = env("KOBS_SOAK_SECS", 300);
    let rate = env("KOBS_SOAK_RATE", 200_000);
    let log_rate = env("KOBS_SOAK_LOGS", 2_000);
    let (senders, series) = (4u64, 1000u64);
    // A cut comes once the log holds half a cadence (`kiln_obs::group::due`): about a minute of
    // this load's points, so a soak shorter than two would judge none.
    anyhow::ensure!(secs >= 120, "a soak of 120 s at the least (KOBS_SOAK_SECS): a cut comes about once a minute at 200,000 points a second");
    // The server's `[observe]` section, as the fleet's configuration leaves it open.
    let helpers = std::env::var("KOBS_SOAK_HELPERS").ok().filter(|v| !v.is_empty());
    let mut f = Fleet::with(|_| helpers.as_ref().map_or_else(String::new, |n| format!("query_helpers = {n}")))?;
    f.start_runner("r1")?;
    f.until(Duration::from_secs(90), "r1 a learner, caught up", |s| caught_up(s, &["r1"]))?;
    let ingest = std::fs::read_to_string(f.root.path().join("server").join("observe").join("ingest.token"))?.trim().to_owned();
    let r1 = *f.otlp.get("r1").ok_or_else(|| anyhow::anyhow!("r1's receiver"))?;
    let server_pid = f.server.as_ref().map_or(0, |p| p.child.id());
    let runner_pid = f.runners.get("r1").and_then(Option::as_ref).map_or(0, |p| p.child.id());
    // A monitor of the soak's own points, evaluated every 10 s, that never alerts.
    f.call(
        "POST",
        "/api/observe/v1/monitors",
        Some(&json!({ "name": "soak", "query": "avg(last_1m):avg:load.v{*} by {host} > 1000000", "message": "never", "options": { "every": 10, "thresholds": { "critical": 1_000_000 } } })),
    )?;
    let rss = |pid: u32| -> (f64, f64) {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        let kib = |k: &str| status.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0) / 1024.0;
        (kib("VmRSS:"), kib("RssAnon:"))
    };
    let stop = Arc::new(AtomicBool::new(false));
    let taken = Arc::new(AtomicU64::new(0));
    // What the machine, the server, the runner and this process (its senders and reader) spend.
    let cpu_then = (machine_ticks(), cpu_ticks(server_pid), cpu_ticks(runner_pid), cpu_ticks(std::process::id()));
    let started = Instant::now();
    let t0_ms = common::now_ms();
    // Each sender: its `series` points (one each) a request, paced to its share of the rate, their
    // time the request's own (now, a millisecond on from its last at the least).
    let interval = Duration::from_secs_f64(series as f64 * senders as f64 / rate.max(1) as f64);
    let mut threads = Vec::new();
    for sender in 0..senders {
        let (stop, ingest, taken) = (stop.clone(), ingest.clone(), taken.clone());
        let (port, auth) = if sender % 2 == 0 { (f.http, format!("Authorization: Bearer {ingest}\r\n")) } else { (r1, String::new()) };
        threads.push(std::thread::spawn(move || -> Sent {
            let mut sent = Sent::default();
            let mut conn = Conn::new(port);
            let headers = format!("{auth}Content-Type: application/x-protobuf\r\nContent-Encoding: gzip\r\n");
            let mut next = Instant::now();
            let mut last_ms = 0i64;
            let mut late = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let now = Instant::now();
                if now < next {
                    std::thread::sleep(next - now);
                } else if now - next > Duration::from_secs(1) {
                    // Behind by more than a second (the machine busy): the schedule starts over
                    // rather than bursting to catch up.
                    late += 1;
                    next = now;
                }
                next += interval;
                let ms = common::now_ms().max(last_ms + 1);
                let body = gzip(&request(sender, series, ms as u64 * 1_000_000)).unwrap_or_default();
                let t = Instant::now();
                let answer = conn.send("/v1/metrics", &headers, &body);
                sent.latencies_us.push(t.elapsed().as_micros() as u64);
                sent.sent_at_us.push(t.duration_since(started).as_micros() as u64);
                sent.requests += 1;
                match answer {
                    Ok(a) if a.code == 200 => {
                        sent.taken += series;
                        taken.fetch_add(series, Ordering::Relaxed);
                        sent.last_ms = Some(ms);
                        last_ms = ms;
                    }
                    Ok(a) if a.code == 503 => {
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
            sent.late = late;
            sent
        }));
    }
    // Log records: a tenth of a second's worth every tenth of a second, through the API.
    let logs = {
        let (stop, port, admin) = (stop.clone(), f.http, f.admin.clone());
        std::thread::spawn(move || -> (u64, Vec<String>) {
            let (mut posted, mut errors) = (0u64, Vec::new());
            let mut conn = Conn::new(port);
            let batch = (log_rate / 10).max(1);
            let mut next = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                let now = Instant::now();
                if now < next {
                    std::thread::sleep(next - now);
                }
                next += Duration::from_millis(100);
                let records: Vec<serde_json::Value> = (0..batch).map(|i| json!({ "message": format!("soak line {} of {posted}", i), "service": "soak", "hostname": "soak", "status": if i % 10 == 0 { "error" } else { "info" }, "attributes": { "i": i } })).collect();
                let body = serde_json::to_vec(&records).unwrap_or_default();
                match conn.send("/api/observe/v1/logs", &format!("Authorization: Bearer {admin}\r\nContent-Type: application/json\r\n"), &body) {
                    Ok(a) if a.code == 202 => posted += batch,
                    // Behind: sent again a moment later, as a client would.
                    Ok(a) if a.code == 503 => std::thread::sleep(Duration::from_millis(100)),
                    Ok(a) => errors.push(format!("logs: {}", a.code)),
                    Err(e) => errors.push(format!("logs: {e}")),
                }
                if errors.len() > 20 {
                    break;
                }
            }
            (posted, errors)
        })
    };
    // A dashboard: one series and every host, over the last 15 minutes, every 2 s.
    let reader = {
        let (stop, port, admin) = (stop.clone(), f.http, f.admin.clone());
        std::thread::spawn(move || -> (Vec<Read>, Vec<String>) {
            let (mut took, mut errors) = (Vec::new(), Vec::new());
            let mut conn = Conn::new(port);
            while !stop.load(Ordering::Relaxed) {
                let now = common::now_ms();
                for (which, q) in ["avg:load.v{host:load-0,s:7}", "sum:load.v{*} by {host}"].into_iter().enumerate() {
                    let body = serde_json::to_vec(&json!({ "query": q, "from": now - 900_000, "to": now })).unwrap_or_default();
                    let t = Instant::now();
                    match conn.send("/api/observe/v1/query", &format!("Authorization: Bearer {admin}\r\nContent-Type: application/json\r\n"), &body) {
                        Ok(a) if a.code == 200 => {
                            let v: serde_json::Value = serde_json::from_slice(&a.body).unwrap_or_default();
                            let us = |k: &str| (at(&v, &[k]).as_f64().unwrap_or(0.0) * 1000.0) as u64;
                            took.push(Read { at_s: started.elapsed().as_secs_f64(), which, us: t.elapsed().as_micros() as u64, queued_us: us("queued_ms"), waited_us: us("waited_ms"), took_us: us("took_ms") });
                        }
                        Ok(a) => errors.push(format!("{q}: {}", a.code)),
                        Err(e) => errors.push(format!("{q}: {e}")),
                    }
                }
                std::thread::sleep(Duration::from_secs(2));
            }
            (took, errors)
        })
    };
    // A watchdog: the server asked twice a second, its health (nothing in the way) and observe's
    // status (through the API's authentication), each within 5 s; an answer slower than 2 s has the
    // server's every thread's stack saved (three times at the most): where a stall is.
    let watchdog = {
        let (stop, port, admin) = (stop.clone(), f.http, f.admin.clone());
        std::thread::spawn(move || -> (Vec<u64>, Vec<u64>, Vec<String>) {
            let (mut health, mut status, mut slow) = (Vec::new(), Vec::new(), Vec::new());
            while !stop.load(Ordering::Relaxed) {
                for (path, token, into) in [("/healthz", "", &mut health), ("/api/observe/v1/status", admin.as_str(), &mut status)] {
                    let at = started.elapsed().as_secs_f64();
                    match timed_get(port, path, token, Duration::from_secs(5)) {
                        Ok(d) => {
                            into.push(d.as_micros() as u64);
                            if d >= Duration::from_secs(2) && slow.len() < 3 {
                                let file = stacks(server_pid, &format!("{path} took {:.1} s at {at:.0} s", d.as_secs_f64()));
                                slow.push(format!("{path} took {:.1} s at {at:.0} s (stacks: {file:?})", d.as_secs_f64()));
                            }
                        }
                        Err(e) => {
                            into.push(5_000_000);
                            if slow.len() < 3 {
                                let file = stacks(server_pid, &format!("{path} at {at:.0} s: {e}"));
                                slow.push(format!("{path} at {at:.0} s: {e} (stacks: {file:?})"));
                            }
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            (health, status, slow)
        })
    };
    // A sample every 10 s.
    let mut samples: Vec<Sampled> = Vec::new();
    let mib = |s: &serde_json::Value, k: &str| at(s, &[k]).as_u64().unwrap_or(0) as f64 / 1048576.0;
    eprintln!("\nobserve, a soak: {rate} points a second for {secs} s ({senders} senders of {series} points a request, half to the runner), {log_rate} log records a second");
    eprintln!("  {:>6} {:>16} {:>16} {:>8} {:>8} {:>7} {:>8} {:>12}", "s", "server MiB", "runner MiB", "head", "index", "blocks", "log MiB", "taken");
    while started.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_secs(10).min(Duration::from_secs(secs).saturating_sub(started.elapsed())).max(Duration::from_millis(100)));
        let st = match f.status() {
            Ok(st) => st,
            Err(e) => {
                let file = stacks(server_pid, &format!("observe's status at {:.0} s: {e:#}", started.elapsed().as_secs_f64()));
                stop.store(true, Ordering::Relaxed);
                anyhow::bail!("observe's status at {:.0} s: {e:#} (the server's stacks: {file:?})\n{}", started.elapsed().as_secs_f64(), f.logs());
            }
        };
        let x = Sampled { at_s: started.elapsed().as_secs_f64(), server: rss(server_pid), runner: rss(runner_pid), head_mib: mib(&st, "heap_samples"), index_mib: mib(&st, "heap_index"), blocks: at(&st, &["blocks"]).as_u64().unwrap_or(0), log_mib: mib(&st, "log_bytes"), taken: taken.load(Ordering::Relaxed) };
        eprintln!(
            "  {:>6.0} {:>9.0} ({:>4.0}) {:>9.0} ({:>4.0}) {:>8.1} {:>8.1} {:>7} {:>8.1} {:>12}",
            x.at_s, x.server.0, x.server.1, x.runner.0, x.runner.1, x.head_mib, x.index_mib, x.blocks, x.log_mib, x.taken
        );
        samples.push(x);
        // At each third's end, where the server's anonymous memory lies: what a climb between the
        // thirds the memory check compares is made of.
        if samples.len().is_multiple_of((secs as usize / 30).max(1)) {
            eprintln!("  where      the server's {}", anon_by_mapping(server_pid));
        }
    }
    stop.store(true, Ordering::Relaxed);
    let ran = started.elapsed();
    let cpu_now = (machine_ticks(), cpu_ticks(server_pid), cpu_ticks(runner_pid), cpu_ticks(std::process::id()));
    let mut all = Sent::default();
    let mut newest = 0i64;
    // Each node's requests' times apart: the even senders send to the server, the odd to the runner.
    let (mut to_server, mut to_runner) = (Vec::new(), Vec::new());
    // Every request: when it was sent and how long it took (µs).
    let mut each: Vec<(u64, u64)> = Vec::new();
    for (i, t) in threads.into_iter().enumerate() {
        let s = t.join().map_err(|_| anyhow::anyhow!("a sender panicked"))?;
        newest = newest.max(s.last_ms.unwrap_or(0));
        all.taken += s.taken;
        all.refused += s.refused;
        all.requests += s.requests;
        all.errors.extend(s.errors);
        if i % 2 == 0 { &mut to_server } else { &mut to_runner }.extend_from_slice(&s.latencies_us);
        each.extend(s.sent_at_us.iter().copied().zip(s.latencies_us.iter().copied()));
        all.latencies_us.extend(s.latencies_us);
        all.late += s.late;
    }
    to_server.sort_unstable();
    to_runner.sort_unstable();
    let (posted, log_errors) = logs.join().map_err(|_| anyhow::anyhow!("the log sender panicked"))?;
    let (read, read_errors) = reader.join().map_err(|_| anyhow::anyhow!("the reader panicked"))?;
    let (mut health, mut status_us, slow) = watchdog.join().map_err(|_| anyhow::anyhow!("the watchdog panicked"))?;
    anyhow::ensure!(all.errors.is_empty() && log_errors.is_empty() && read_errors.is_empty(), "errors: {:?} {:?} {:?}\n{}", all.errors.iter().take(5).collect::<Vec<_>>(), log_errors.iter().take(5).collect::<Vec<_>>(), read_errors.iter().take(5).collect::<Vec<_>>(), f.logs());
    // Every point and record taken, stored once.
    let settled = Instant::now();
    let mut have = 0;
    while settled.elapsed() < Duration::from_secs(180) {
        have = stored(&f, t0_ms - 60_000, newest + 1_000)?;
        if have >= all.taken {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    anyhow::ensure!(have == all.taken, "{} points taken, {have} stored\n{}", all.taken, f.logs());
    let mut kept = 0.0;
    while settled.elapsed() < Duration::from_secs(240) {
        let found = f.call("POST", "/api/observe/v1/logs/aggregate", Some(&json!({ "query": "service:soak", "from": t0_ms - 60_000, "to": common::now_ms() + 60_000 })))?;
        kept = at(&found, &["groups"]).as_array().and_then(|g| g.first()).and_then(|g| at(g, &["value"]).as_f64()).unwrap_or(0.0);
        if kept >= posted as f64 {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    anyhow::ensure!(kept == posted as f64, "{posted} log records taken, {kept} stored");
    let st = f.status()?;
    let dropped = at(&st, &["dropped"]).as_u64().unwrap_or(0) + at(&st, &["machines"]).as_array().into_iter().flatten().map(|m| at(m, &["status", "dropped"]).as_u64().unwrap_or(0)).sum::<u64>();
    anyhow::ensure!(dropped == 0, "{dropped} dropped for room after being taken");
    let (index, digest) = f.verify(2)?;
    all.latencies_us.sort_unstable();
    let ms = |us: u64| us as f64 / 1000.0;
    // Where a query's time went, its slowest hundredth apart: a thread to read on, the store, the
    // reading, or the rest (the request's way there and back).
    for (which, name) in [(0, "one series"), (1, "by host")] {
        let mut mine: Vec<&Read> = read.iter().filter(|r| r.which == which).collect();
        mine.sort_unstable_by_key(|r| r.us);
        let slow = mine.get(mine.len() * 99 / 100..).unwrap_or(&[]);
        let med = |f: fn(&Read) -> u64| {
            let mut v: Vec<u64> = slow.iter().map(|r| f(r)).collect();
            v.sort_unstable();
            v.get(v.len() / 2).copied().unwrap_or(0)
        };
        let lat: Vec<u64> = mine.iter().map(|r| r.us).collect();
        eprintln!(
            "  {name:<10} p50 {:.2} ms, p99 {:.2} ms, most {:.2} ms; its slowest hundredth (median): queued {:.2}, waited for the store {:.2}, read {:.2} ms; at {:?} s",
            ms(pct(&lat, 0.5)),
            ms(pct(&lat, 0.99)),
            ms(pct(&lat, 1.0)),
            ms(med(|r| r.queued_us)),
            ms(med(|r| r.waited_us)),
            ms(med(|r| r.took_us)),
            slow.iter().rev().take(8).map(|r| r.at_s.round() as u64).collect::<Vec<_>>()
        );
    }
    // The widest query's readings, from when it was asked to when it was answered (µs into the run).
    let mut wide: Vec<(u64, u64)> = read
        .iter()
        .filter(|r| r.which == 1)
        .map(|r| {
            let end = (r.at_s * 1e6) as u64;
            (end.saturating_sub(r.us), end)
        })
        .collect();
    wide.sort_unstable();
    let mut read: Vec<u64> = read.iter().map(|r| r.us).collect();
    read.sort_unstable();
    eprintln!("  taken      {} points ({:.0} a second), {} refused (503, sent again), the senders {} times a second behind; {posted} log records", all.taken, all.taken as f64 / ran.as_secs_f64(), all.refused, all.late);
    eprintln!("  a request  p50 {:.2} ms, p99 {:.2} ms; a dashboard query p50 {:.2} ms, p99 {:.2} ms", ms(pct(&all.latencies_us, 0.5)), ms(pct(&all.latencies_us, 0.99)), ms(pct(&read, 0.5)), ms(pct(&read, 0.99)));
    eprintln!(
        "  requests   the server's p50 {:.2} ms, p99 {:.2} ms, p99.9 {:.2} ms; the runner's p50 {:.2} ms, p99 {:.2} ms, p99.9 {:.2} ms",
        ms(pct(&to_server, 0.5)),
        ms(pct(&to_server, 0.99)),
        ms(pct(&to_server, 0.999)),
        ms(pct(&to_runner, 0.5)),
        ms(pct(&to_runner, 0.99)),
        ms(pct(&to_runner, 0.999))
    );
    // Requests while the widest query read (its helpers with it, if it took any) and while none did:
    // what a wide query costs what is written meanwhile, the machine's other load the same for both.
    let (mut during, mut outside) = (Vec::new(), Vec::new());
    for &(at, us) in &each {
        // The first query to end after the request was sent: the one it may overlap.
        let k = wide.partition_point(|&(_, end)| end < at);
        let overlaps = wide.get(k).is_some_and(|&(start, _)| start <= at + us);
        if overlaps { &mut during } else { &mut outside }.push(us);
    }
    during.sort_unstable();
    outside.sort_unstable();
    eprintln!(
        "  beside     requests while the by-host query read: p50 {:.2} ms, p99 {:.2} ms ({}); while none did: p50 {:.2} ms, p99 {:.2} ms ({})",
        ms(pct(&during, 0.5)),
        ms(pct(&during, 0.99)),
        during.len(),
        ms(pct(&outside, 0.5)),
        ms(pct(&outside, 0.99)),
        outside.len()
    );
    health.sort_unstable();
    status_us.sort_unstable();
    eprintln!(
        "  watchdog   health p99 {:.2} ms, most {:.2} ms; status p99 {:.2} ms, most {:.2} ms{}",
        ms(pct(&health, 0.99)),
        ms(pct(&health, 1.0)),
        ms(pct(&status_us, 0.99)),
        ms(pct(&status_us, 1.0)),
        if slow.is_empty() { String::new() } else { format!("; slow: {}", slow.join("; ")) }
    );
    anyhow::ensure!(slow.iter().all(|s| !s.contains("no answer") && !s.contains("connect")), "the server stopped answering: {}", slow.join("; "));
    let ((busy0, total0, cores), server0, runner0, me0) = cpu_then;
    let ((busy1, total1, _), server1, runner1, me1) = cpu_now;
    // A process's ticks as cores busy over the run: every core's ticks in all are the run's length.
    let all_ticks = total1.saturating_sub(total0).max(1) as f64;
    let busy = |ticks: u64| ticks as f64 / all_ticks * cores as f64;
    let (machine, server, runner, me) = (busy(busy1.saturating_sub(busy0)), busy(server1.saturating_sub(server0)), busy(runner1.saturating_sub(runner0)), busy(me1.saturating_sub(me0)));
    eprintln!(
        "  machine    {cores} cores, {machine:.1} busy: the server {server:.1}, the runner {runner:.1}, the senders and reader {me:.1}, the rest {:.1} (other work)",
        (machine - server - runner - me).max(0.0)
    );
    eprintln!("  where      the server's {} (of {:.0} MiB anonymous); the runner's {} (of {:.0})", anon_by_mapping(server_pid), rss(server_pid).1, anon_by_mapping(runner_pid), rss(runner_pid).1);
    eprintln!("  replicas   agree at index {index}: {digest:016x}");
    // The store cut and its log compacted all through.
    let blocks_made = samples.windows(2).filter(|w| matches!(w, [a, b] if b.blocks != a.blocks)).count();
    anyhow::ensure!(blocks_made > 0, "no cut in {secs} s");
    // Memory holds a level: each third's highest, the last no more than a tenth over the second
    // (and 16 MiB: a buffer's doubling, a merge's block).
    let third = samples.len() / 3;
    anyhow::ensure!(third >= 2, "too short a soak to judge memory: {} samples", samples.len());
    let peak = |range: &[Sampled], which: fn(&Sampled) -> f64| range.iter().map(which).fold(0.0f64, f64::max);
    for (name, which) in [("server", (|x: &Sampled| x.server.0) as fn(&Sampled) -> f64), ("runner", |x: &Sampled| x.runner.0)] {
        let (second, last) = (peak(samples.get(third..2 * third).unwrap_or(&[]), which), peak(samples.get(2 * third..).unwrap_or(&[]), which));
        eprintln!("  memory     the {name}'s highest: {second:.0} MiB in the second third, {last:.0} in the last");
        anyhow::ensure!(last <= second * 1.1 + 16.0, "the {name}'s memory climbs: {second:.0} MiB, then {last:.0}\n{}", f.logs());
    }
    Ok(())
}
