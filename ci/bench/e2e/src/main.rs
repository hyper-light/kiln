//! kiln's open-loop end-to-end generator (LB14; `run.py` drives it, `analyze.py` reads it).
//!
//! One engine, three paths, so every system it times is timed the same way:
//!
//! - `reach`: an ask to a runner's reach socket (`crates/kiln/src/reach/local.rs`) to run a no-op
//!   command, detached, with no tree to copy. Timed to its answer (the socket's first word: `Picked`,
//!   or an error) and to its start (`Started`: the doing runner's word, relayed back, that the
//!   process started). The kiln path.
//! - `spawn`: the same no-op started directly on this machine as kiln's doer starts it (`bash -c`,
//!   a process group of its own, stdin, stdout and stderr piped), timed to `spawn` returning. The
//!   direct path the kiln path is compared with.
//! - `http`: a GET on a keep-alive HTTP/1.1 connection, timed to the whole answer: a proxy's path
//!   or a backend's direct one, to check wrk2's numbers with this engine's.
//!
//! **Open loop**, wrk2's model: W workers, worker w makes asks w, w + W, w + 2W, ...; ask i is due
//! at `t0 + i / rate` and goes out then, whatever became of the earlier ones, and every latency is
//! taken from the due time, so an ask sent late because its worker was still busy counts its wait
//! (no coordinated omission). The schedule is kept by one dispatcher thread that spins to each due
//! time and hands the ask to its worker (a channel's wakeup, an event): no sleep is in it, since a
//! sleep's wakeup is a timer, and timers are coalesced (on the bench's Mac, under an agent's run's
//! policy, sleeping 10 ms came back 40 ms late). Each ask's lateness (its worker's start minus its
//! due time: the handoff) is kept too, and is the same on every path.
//!
//! **After the run** (`stats.rs`): the warm-up is cut by MSER-5 on the latency series in send
//! order; what is left is tested for a trend (Mann-Kendall on `floor(sqrt(n))` batch means), whose
//! rise at 95% says the rate wasn't sustained. Latencies are kept in HdrHistogram's layout
//! (`hist.rs`) and exactly (sorted, for percentiles and their 95% order-statistic intervals).
//!
//! Usage: kiln-bench-e2e reach --socket PATH --target T [--command CMD]
//!        kiln-bench-e2e spawn [--command CMD]
//!        kiln-bench-e2e http --addr HOST:PORT [--path /]
//!        then: --rate PER_S --seconds S --workers W [--json OUT] [--label L]
//! The JSON summary goes to stdout and, with --json, to OUT.

mod hist;
mod proto;
mod stats;

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hist::Hist;
use proto::{Deframer, Ev, Req, RunReq};

#[derive(Clone, Debug)]
enum Path {
    Reach { socket: String, target: String, command: String },
    Spawn { command: String },
    Http { addr: String, path: String },
}

impl Path {
    fn name(&self) -> &'static str {
        match self {
            Path::Reach { .. } => "reach",
            Path::Spawn { .. } => "spawn",
            Path::Http { .. } => "http",
        }
    }
}

#[derive(Clone, Debug)]
struct Args {
    path: Path,
    rate: f64,
    seconds: f64,
    workers: u64,
    json: Option<String>,
    label: String,
}

const USAGE: &str = "usage: kiln-bench-e2e reach --socket PATH --target T [--command CMD] | spawn [--command CMD] | http --addr HOST:PORT [--path P]; then --rate PER_S --seconds S --workers W [--json OUT] [--label L]";

fn parse(argv: &[String]) -> Result<Args, String> {
    let Some((mode, rest)) = argv.split_first() else { return Err(USAGE.into()) };
    let mut kv: HashMap<String, String> = HashMap::new();
    for pair in rest.chunks(2) {
        let (Some(k), Some(v)) = (pair.first(), pair.get(1)) else { return Err(format!("{pair:?}: a flag without a value; {USAGE}")) };
        let Some(k) = k.strip_prefix("--") else { return Err(format!("{k}: not a flag; {USAGE}")) };
        kv.insert(k.to_owned(), v.clone());
    }
    let mut take = |k: &str| kv.remove(k);
    let command = take("command").unwrap_or_else(|| "true".into());
    let path = match mode.as_str() {
        "reach" => Path::Reach { socket: take("socket").ok_or("reach needs --socket")?, target: take("target").ok_or("reach needs --target")?, command },
        "spawn" => Path::Spawn { command },
        "http" => Path::Http { addr: take("addr").ok_or("http needs --addr")?, path: take("path").unwrap_or_else(|| "/".into()) },
        m => return Err(format!("{m}: no such path; {USAGE}")),
    };
    let num = |v: Option<String>, k: &str| -> Result<f64, String> { v.ok_or(format!("--{k} is needed"))?.parse::<f64>().map_err(|e| format!("--{k}: {e}")) };
    let rate = num(take("rate"), "rate")?;
    let seconds = num(take("seconds"), "seconds")?;
    let workers = num(take("workers"), "workers")?;
    let json = take("json");
    let label = take("label").unwrap_or_default();
    if let Some(k) = kv.keys().next() {
        return Err(format!("--{k}: no such flag; {USAGE}"));
    }
    if !(rate > 0.0 && seconds > 0.0 && workers >= 1.0 && workers.fract() == 0.0) {
        return Err(format!("--rate and --seconds must be positive and --workers a whole number of at least 1; {USAGE}"));
    }
    Ok(Args { path, rate, seconds, workers: workers as u64, json, label })
}

/// What became of one ask.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Ok,
    /// Answered with an error, or ended before it started.
    Refused,
    /// The path couldn't take it at all (connect, send or read failed, or no answer by the end).
    Failed,
}

/// One ask, its times in nanoseconds: from its due time to when it left (`late`), to its first
/// answer and to its start (`answer`, `started`: [`NONE`] when there was none), from when it left
/// to its start (`since_sent`: what a closed-loop tool reports), and its start from `t0`.
#[derive(Clone, Copy)]
struct Rec {
    i: u64,
    late: u64,
    answer: u64,
    started: u64,
    since_sent: u64,
    at: u64,
    /// The runner its answer named (an index into the worker's names), or [`NONE32`].
    runner: u32,
    outcome: Outcome,
}

const NONE: u64 = u64::MAX;
const NONE32: u32 = u32::MAX;

fn ns(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// What a worker saw: its asks, the runners named, and the first few reasons asks were refused
/// or failed.
#[derive(Default)]
struct Seen {
    recs: Vec<Rec>,
    names: Vec<String>,
    index: HashMap<String, u32>,
    why: Vec<String>,
}

impl Seen {
    fn name(&mut self, n: &str) -> u32 {
        if let Some(&i) = self.index.get(n) {
            return i;
        }
        let i = u32::try_from(self.names.len()).unwrap_or(NONE32);
        self.names.push(n.to_owned());
        self.index.insert(n.to_owned(), i);
        i
    }

    fn because(&mut self, why: String) {
        if self.why.len() < 8 && !self.why.contains(&why) {
            self.why.push(why);
        }
    }
}

/// A worker's HTTP connection, kept alive between its asks.
struct Http {
    stream: Option<TcpStream>,
    buf: Vec<u8>,
}

/// Reads one HTTP/1.1 answer from `s` (its head, then a body by `content-length` or chunked):
/// its status and whether the connection stays open.
fn http_answer(s: &mut TcpStream, buf: &mut Vec<u8>) -> Result<(u16, bool), String> {
    buf.clear();
    let mut chunk = [0u8; 16 * 1024];
    let head_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
        let n = s.read(&mut chunk).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("closed before an answer".into());
        }
        buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
    };
    let head = String::from_utf8_lossy(buf.get(..head_end).unwrap_or_default()).to_ascii_lowercase();
    let status = head.split_whitespace().nth(1).and_then(|c| c.parse::<u16>().ok()).ok_or_else(|| format!("no status in {head:?}"))?;
    let keep = !head.contains("\r\nconnection: close");
    let length = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok());
    let chunked = head.contains("\r\ntransfer-encoding: chunked");
    // 1xx, 204 and 304 carry no body.
    let bodiless = status == 204 || status == 304 || (100..200).contains(&status);
    if !bodiless {
        if let Some(len) = length {
            while buf.len() < head_end + len {
                let n = s.read(&mut chunk).map_err(|e| format!("read body: {e}"))?;
                if n == 0 {
                    return Err("closed in a body".into());
                }
                buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
            }
        } else if chunked {
            while !buf.get(head_end..).unwrap_or_default().windows(5).any(|w| w == b"0\r\n\r\n") {
                let n = s.read(&mut chunk).map_err(|e| format!("read chunks: {e}"))?;
                if n == 0 {
                    return Err("closed in a chunked body".into());
                }
                buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
            }
        } else {
            return Err(format!("status {status} with neither content-length nor chunks"));
        }
    }
    Ok((status, keep))
}

/// When an ask was due and left, the run's start, and when an answer stops being waited for.
#[derive(Clone, Copy)]
struct When {
    t0: Instant,
    due: Instant,
    sent: Instant,
    deadline: Instant,
}

/// Ask i, due and sent `at`, along `path`.
fn ask(path: &Path, frame: &[u8], http: &mut Http, reaper: &mpsc::Sender<Child>, at: When, i: u64, seen: &mut Seen) {
    let When { t0, due, sent, deadline } = at;
    let mut rec = Rec { i, late: ns(sent - due), answer: NONE, started: NONE, since_sent: NONE, at: NONE, runner: NONE32, outcome: Outcome::Failed };
    let started = |rec: &mut Rec, now: Instant| {
        rec.started = ns(now.saturating_duration_since(due));
        rec.since_sent = ns(now.saturating_duration_since(sent));
        rec.at = ns(now.saturating_duration_since(t0));
    };
    match path {
        Path::Reach { socket, .. } => {
            let mut s = match UnixStream::connect(socket) {
                Ok(s) => s,
                Err(e) => {
                    seen.because(format!("connect: {e}"));
                    seen.recs.push(rec);
                    return;
                }
            };
            if let Err(e) = s.write_all(frame) {
                seen.because(format!("send: {e}"));
                seen.recs.push(rec);
                return;
            }
            let mut de = Deframer::default();
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                let ev = match de.take() {
                    Ok(Some(ev)) => ev,
                    Ok(None) => {
                        let left = deadline.saturating_duration_since(Instant::now());
                        if left.is_zero() || s.set_read_timeout(Some(left)).is_err() {
                            seen.because("no answer by the end of the run".into());
                            break;
                        }
                        match s.read(&mut buf) {
                            Ok(0) => {
                                seen.because("the socket closed before a start".into());
                                break;
                            }
                            Ok(n) => {
                                de.push(buf.get(..n).unwrap_or_default());
                                continue;
                            }
                            Err(e) => {
                                seen.because(format!("read: {e}"));
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        seen.because(e);
                        break;
                    }
                };
                let now = Instant::now();
                if rec.answer == NONE {
                    rec.answer = ns(now.saturating_duration_since(due));
                }
                match ev {
                    Ev::Picked { runner, .. } => rec.runner = seen.name(&runner),
                    Ev::Started { runner, .. } => {
                        if rec.runner == NONE32 && !runner.is_empty() {
                            rec.runner = seen.name(&runner);
                        }
                        started(&mut rec, now);
                        rec.outcome = Outcome::Ok;
                        break;
                    }
                    Ev::Error(why) | Ev::Exit { error: Some(why), .. } => {
                        seen.because(why);
                        rec.outcome = Outcome::Refused;
                        break;
                    }
                    Ev::Exit { code, .. } => {
                        seen.because(format!("exited ({code:?}) before it said it started"));
                        rec.outcome = Outcome::Refused;
                        break;
                    }
                    _ => {}
                }
            }
        }
        Path::Spawn { command } => {
            let home = std::env::var_os("HOME").unwrap_or_else(|| "/".into());
            let mut c = Command::new("/bin/bash");
            c.arg("-c").arg(command).env_clear().env("PATH", "/usr/bin:/bin").env("HOME", &home).current_dir(&home);
            c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
            match c.spawn() {
                Ok(child) => {
                    let now = Instant::now();
                    rec.answer = ns(now.saturating_duration_since(due));
                    started(&mut rec, now);
                    rec.outcome = Outcome::Ok;
                    let _ = reaper.send(child);
                }
                Err(e) => seen.because(format!("spawn: {e}")),
            }
        }
        Path::Http { addr, path } => {
            let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n\r\n");
            let mut tried = false;
            loop {
                if http.stream.is_none() {
                    match TcpStream::connect(addr) {
                        Ok(s) => {
                            let _ = s.set_nodelay(true);
                            http.stream = Some(s);
                        }
                        Err(e) => {
                            seen.because(format!("connect: {e}"));
                            break;
                        }
                    }
                }
                let Some(s) = http.stream.as_mut() else { break };
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() || s.set_read_timeout(Some(left)).is_err() {
                    seen.because("no answer by the end of the run".into());
                    break;
                }
                let got = s.write_all(req.as_bytes()).map_err(|e| format!("send: {e}")).and_then(|()| http_answer(s, &mut http.buf));
                match got {
                    Ok((status, keep)) => {
                        let now = Instant::now();
                        rec.answer = ns(now.saturating_duration_since(due));
                        started(&mut rec, now);
                        if (200..400).contains(&status) {
                            rec.outcome = Outcome::Ok;
                        } else {
                            rec.outcome = Outcome::Refused;
                            seen.because(format!("status {status}"));
                        }
                        if !keep {
                            http.stream = None;
                        }
                        break;
                    }
                    // A kept connection the server closed meanwhile: once more on a new one.
                    Err(e) => {
                        http.stream = None;
                        if tried {
                            seen.because(e);
                            break;
                        }
                        tried = true;
                    }
                }
            }
        }
    }
    seen.recs.push(rec);
}

/// A worker: each ask the dispatcher hands it, in turn, until the schedule ends.
fn run_worker(a: &Args, total: u64, t0: Instant, deadline: Instant, asks: mpsc::Receiver<(u64, Instant)>, reaper: mpsc::Sender<Child>) -> Seen {
    let mut seen = Seen { recs: Vec::with_capacity(usize::try_from(total / a.workers + 1).unwrap_or(0)), ..Seen::default() };
    let frame = match &a.path {
        Path::Reach { target, command, .. } => {
            let req = Req::Run(Box::new(RunReq { target: target.clone(), command: command.clone(), detach: true, ..RunReq::default() }));
            match proto::frame(&req) {
                Ok(f) => f,
                Err(e) => {
                    seen.because(e);
                    return seen;
                }
            }
        }
        _ => Vec::new(),
    };
    let mut http = Http { stream: None, buf: Vec::with_capacity(16 * 1024) };
    for (i, due) in asks {
        let sent = Instant::now();
        ask(&a.path, &frame, &mut http, &reaper, When { t0, due, sent, deadline }, i, &mut seen);
    }
    seen
}

/// A string as JSON.
fn js(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// The percentiles of a sorted sample and their 95% intervals, as JSON.
fn spread(xs: &[u64]) -> String {
    let q = |p: f64| stats::quantile(xs, p);
    let ci = |p: f64| {
        let (lo, hi) = stats::quantile_ci(xs, p);
        format!("[{lo},{hi}]")
    };
    let mean = if xs.is_empty() { 0.0 } else { xs.iter().map(|&v| v as f64).sum::<f64>() / xs.len() as f64 };
    format!(
        "{{\"n\":{},\"mean_ns\":{:.0},\"p50_ns\":{},\"p50_ci_ns\":{},\"p90_ns\":{},\"p99_ns\":{},\"p99_ci_ns\":{},\"p999_ns\":{},\"max_ns\":{}}}",
        xs.len(),
        mean,
        q(0.5),
        ci(0.5),
        q(0.9),
        q(0.99),
        ci(0.99),
        q(0.999),
        xs.last().copied().unwrap_or(0)
    )
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let a = match parse(&argv) {
        Ok(a) => Arc::new(a),
        Err(e) => {
            eprintln!("kiln-bench-e2e: {e}");
            return ExitCode::from(2);
        }
    };
    let total = (a.seconds * a.rate).round() as u64;
    let span = Duration::from_secs_f64(a.seconds);
    let (reap_tx, reap_rx) = mpsc::channel::<Child>();
    let reaper = std::thread::spawn(move || {
        for child in reap_rx {
            let _ = child.wait_with_output();
        }
    });
    // The schedule starts once every worker is up (none is late for a thread still being made):
    // each says so at the first barrier, t0 is taken, and the second lets them read it.
    let barrier = Arc::new(std::sync::Barrier::new(usize::try_from(a.workers).unwrap_or(usize::MAX).saturating_add(1)));
    let start: Arc<std::sync::OnceLock<Instant>> = Arc::new(std::sync::OnceLock::new());
    let mut hand: Vec<mpsc::Sender<(u64, Instant)>> = Vec::new();
    let workers: Vec<_> = (0..a.workers)
        .map(|_| {
            let (a, tx, barrier, start) = (Arc::clone(&a), reap_tx.clone(), Arc::clone(&barrier), Arc::clone(&start));
            let (give, asks) = mpsc::channel::<(u64, Instant)>();
            hand.push(give);
            std::thread::spawn(move || {
                barrier.wait();
                barrier.wait();
                let t0 = start.get().copied().unwrap_or_else(Instant::now);
                // Asks unanswered by the end of the run plus its length again are counted failed.
                run_worker(&a, total, t0, t0 + span + span, asks, tx)
            })
        })
        .collect();
    drop(reap_tx);
    barrier.wait();
    let t0 = Instant::now();
    let _ = start.set(t0);
    let wall0 = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    barrier.wait();
    // The dispatcher: ask i at t0 + i / rate to worker i mod W, spinning to each due time.
    for i in 0..total {
        let due = t0 + Duration::from_secs_f64(i as f64 / a.rate);
        while Instant::now() < due {
            std::hint::spin_loop();
        }
        let w = usize::try_from(i % a.workers).unwrap_or(0);
        if let Some(give) = hand.get(w) {
            let _ = give.send((i, due));
        }
    }
    drop(hand);
    let mut recs: Vec<Rec> = Vec::with_capacity(usize::try_from(total).unwrap_or(0));
    let mut names: Vec<String> = Vec::new();
    let mut index: HashMap<String, u32> = HashMap::new();
    let mut why: Vec<String> = Vec::new();
    for w in workers {
        let Ok(s) = w.join() else { continue };
        for mut r in s.recs {
            if let Some(n) = s.names.get(r.runner as usize) {
                let next = u32::try_from(names.len()).unwrap_or(NONE32);
                r.runner = *index.entry(n.clone()).or_insert_with(|| {
                    names.push(n.clone());
                    next
                });
            }
            recs.push(r);
        }
        for y in s.why {
            if why.len() < 8 && !why.contains(&y) {
                why.push(y);
            }
        }
    }
    let _ = reaper.join();
    recs.sort_by_key(|r| r.i);
    let (ok, refused, failed) = recs.iter().fold((0u64, 0u64, 0u64), |(o, r, f), x| match x.outcome {
        Outcome::Ok => (o + 1, r, f),
        Outcome::Refused => (o, r + 1, f),
        Outcome::Failed => (o, r, f + 1),
    });
    // The warm-up, by MSER-5 on the started latency of the asks that started, in send order.
    let done: Vec<&Rec> = recs.iter().filter(|r| r.outcome == Outcome::Ok).collect();
    let series: Vec<f64> = done.iter().map(|r| r.started as f64).collect();
    let cut = stats::mser5(&series);
    let kept: Vec<&Rec> = done.get(cut..).unwrap_or_default().to_vec();
    let trend = stats::trend(&kept.iter().map(|r| r.started as f64).collect::<Vec<_>>());
    // The measured window: from the first kept ask's due time to the end of the schedule.
    let from_s = kept.first().map_or(a.seconds, |r| r.i as f64 / a.rate);
    let window = (a.seconds - from_s).max(0.0);
    let due_in = recs.iter().filter(|r| r.i as f64 / a.rate >= from_s).count() as u64;
    let started_in = recs.iter().filter(|r| r.outcome == Outcome::Ok && r.at != NONE && r.at as f64 / 1e9 >= from_s && (r.at as f64 / 1e9) < a.seconds).count() as u64;
    let lost_in = recs.iter().filter(|r| r.outcome != Outcome::Ok && r.i as f64 / a.rate >= from_s).count() as u64;
    let sorted = |f: &dyn Fn(&Rec) -> u64| -> Vec<u64> {
        let mut v: Vec<u64> = kept.iter().map(|r| f(r)).filter(|&v| v != NONE).collect();
        v.sort_unstable();
        v
    };
    let started_v = sorted(&|r| r.started);
    let answer_v = sorted(&|r| r.answer);
    let since_v = sorted(&|r| r.since_sent);
    let late_v = sorted(&|r| r.late);
    let mut hist = Hist::default();
    for &v in &started_v {
        hist.add(Duration::from_nanos(v));
    }
    // Which runner each kept ask went to: the spread of the work (quality beside speed).
    let mut picks: Vec<u64> = vec![0; names.len()];
    for r in &kept {
        if let Some(c) = picks.get_mut(r.runner as usize) {
            *c += 1;
        }
    }
    let picks_json: Vec<String> = names.iter().zip(&picks).map(|(n, c)| format!("{}:{c}", js(n))).collect();
    let (path_json, target) = match &a.path {
        Path::Reach { socket, target, command } => (format!("{{\"socket\":{},\"target\":{},\"command\":{}}}", js(socket), js(target), js(command)), target.clone()),
        Path::Spawn { command } => (format!("{{\"command\":{}}}", js(command)), String::new()),
        Path::Http { addr, path } => (format!("{{\"addr\":{},\"path\":{}}}", js(addr), js(path)), addr.clone()),
    };
    let json = format!(
        concat!(
            "{{\"who\":\"kiln-bench-e2e\",\"path\":{},\"label\":{},\"at\":{:.3},\"target\":{},\"how\":{},",
            "\"rate\":{},\"seconds\":{},\"workers\":{},\"scheduled\":{},\"ok\":{},\"refused\":{},\"failed\":{},",
            "\"warmup\":{{\"cut\":{},\"of\":{},\"seconds\":{:.3}}},\"window_s\":{:.3},\"due_in_window\":{},\"started_in_window\":{},\"lost_in_window\":{},",
            "\"achieved_per_s\":{:.2},\"trend\":{{\"tau\":{:.4},\"z\":{:.3},\"p\":{:.4},\"batches\":{},\"rising\":{}}},\"sustained\":{},",
            "\"started\":{},\"answer\":{},\"since_sent\":{},\"late\":{},\"hdr\":{},\"picks\":{{{}}},\"why\":[{}]}}"
        ),
        js(a.path.name()),
        js(&a.label),
        wall0,
        js(&target),
        path_json,
        a.rate,
        a.seconds,
        a.workers,
        total,
        ok,
        refused,
        failed,
        cut,
        done.len(),
        from_s,
        window,
        due_in,
        started_in,
        lost_in,
        if window > 0.0 { started_in as f64 / window } else { 0.0 },
        trend.tau,
        trend.z,
        trend.p,
        trend.batches,
        trend.rising(),
        !trend.rising() && lost_in == 0,
        spread(&started_v),
        spread(&answer_v),
        spread(&since_v),
        spread(&late_v),
        hist.json(),
        picks_json.join(","),
        why.iter().map(|w| js(w)).collect::<Vec<_>>().join(",")
    );
    println!("{json}");
    if let Some(p) = &a.json
        && std::fs::write(p, format!("{json}\n")).is_err()
    {
        eprintln!("kiln-bench-e2e: cannot write {p}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;
    use std::os::unix::net::UnixListener;

    /// What the fake socket says: `reach::local::Ev`'s first variants, by position.
    #[derive(Serialize)]
    enum Say {
        _Status(String),
        _Runners(String),
        Picked { runner: String, why: String },
        _Syncing { files: u64, bytes: u64 },
        _Synced { root: String, sent: u32, removed: u32, bytes: u64, ms: u64 },
        Started { runner: String, pid: u32, cwd: String },
    }

    /// LB14: latency is taken from the intended send time (no coordinated omission): an ask due
    /// 10 ms before it left, answered 2 ms after and started 5 ms after, counts 12 and 15 ms.
    #[test]
    fn lb14_open_loop_measures_from_intended_time() -> Result<(), String> {
        let dir = std::env::var_os("CARGO_TARGET_TMPDIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let path = dir.join(format!("e2e-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let l = UnixListener::bind(&path).map_err(|e| e.to_string())?;
        std::thread::spawn(move || {
            for s in l.incoming().take(1) {
                let Ok(mut s) = s else { continue };
                let mut b = [0u8; 4096];
                let _ = s.read(&mut b);
                std::thread::sleep(Duration::from_millis(2));
                let _ = s.write_all(&proto::frame(&Say::Picked { runner: "b".into(), why: "room".into() }).unwrap_or_default());
                std::thread::sleep(Duration::from_millis(3));
                let _ = s.write_all(&proto::frame(&Say::Started { runner: "b".into(), pid: 1, cwd: "/".into() }).unwrap_or_default());
            }
        });
        let p = Path::Reach { socket: path.to_string_lossy().into_owned(), target: "b".into(), command: "true".into() };
        let frame = proto::frame(&Req::Run(Box::new(RunReq { target: "b".into(), command: "true".into(), detach: true, ..RunReq::default() })))?;
        let (tx, _rx) = mpsc::channel();
        let mut http = Http { stream: None, buf: Vec::new() };
        let mut seen = Seen::default();
        let now = Instant::now();
        let due = now - Duration::from_millis(10);
        ask(&p, &frame, &mut http, &tx, When { t0: due, due, sent: now, deadline: now + Duration::from_secs(10) }, 0, &mut seen);
        let _ = std::fs::remove_file(&path);
        let r = seen.recs.first().copied().ok_or("no record")?;
        if r.outcome != Outcome::Ok || seen.names.first().map(String::as_str) != Some("b") {
            return Err(format!("{:?}", seen.why));
        }
        if r.answer < 12_000_000 || r.started < 15_000_000 || r.since_sent < 5_000_000 || r.since_sent >= r.started || r.late < 10_000_000 {
            return Err(format!("answer {} started {} since_sent {} late {}", r.answer, r.started, r.since_sent, r.late));
        }
        Ok(())
    }

    /// LB14: the HTTP path reads a 204 (no body) and a 200 with a body on one kept connection.
    #[test]
    fn lb14_http_answers_with_and_without_bodies() -> Result<(), String> {
        let l = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        let addr = l.local_addr().map_err(|e| e.to_string())?.to_string();
        std::thread::spawn(move || {
            if let Some(Ok(mut s)) = l.incoming().next() {
                let mut b = [0u8; 4096];
                let _ = s.read(&mut b);
                let _ = s.write_all(b"HTTP/1.1 204 No Content\r\n\r\n");
                let _ = s.read(&mut b);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello");
            }
        });
        let p = Path::Http { addr, path: "/".into() };
        let (tx, _rx) = mpsc::channel();
        let mut http = Http { stream: None, buf: Vec::new() };
        let mut seen = Seen::default();
        for i in 0..2 {
            let now = Instant::now();
            ask(&p, &[], &mut http, &tx, When { t0: now, due: now, sent: now, deadline: now + Duration::from_secs(10) }, i, &mut seen);
        }
        if seen.recs.len() != 2 || seen.recs.iter().any(|r| r.outcome != Outcome::Ok) {
            return Err(format!("{:?}", seen.why));
        }
        Ok(())
    }
}
