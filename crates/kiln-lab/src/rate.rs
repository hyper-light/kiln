//! `kiln-lab rate`: servers at fixed request rates (docs/runtime-lab.md, "Fixed rates"; goal 10, Ada's fixed-rate
//! benchmark of kiln, production's kiln and T3 Code). Where `run` replays a realistic mix, this asks one read at a
//! time at each rate (100 to a million a second by default), open loop and without coordinated omission (wrk2's
//! model, `rate::lane`): each request's latency runs from when the schedule said to send it. For each target (a
//! file: how to start its server, seed it, and its reads; `rate::target`), each rate and each read: a warm-up, the
//! measured window, and the drain; its latency (p50, p99, p99.9, the most), what it achieved and its errors, the
//! server's CPU and memory through the window, and the generator's own CPU and lateness, saying when the generator
//! (or the connections), not the server, is what couldn't keep up. One JSON line each, and a Markdown table.

mod load;
mod hist;
mod lane;
mod os;
mod sample;
mod serve;
pub mod target;
mod wire;

use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::lab::{Build, Place, Profile};
use target::{Kind, Server, Target, Workload};

/// The shortest window: wrk's and wrk2's default run (10 s), and ten of the server's samples on macOS
/// (`sample::EVERY`).
const MIN_WINDOW_S: f64 = 10.0;

/// The fewest requests a window holds: p99.9 then rests on ten samples or more past it.
const TAIL_REQUESTS: f64 = 10_000.0;

/// Connections are counted so each is busy at most half the time at the expected latency: an answer twice as slow
/// as expected still doesn't hold the next request back.
const HEADROOM: f64 = 2.0;

/// A rate is kept up when at least this share of it was answered, and at most the rest erred: p99 then rests on
/// answers.
const KEPT: f64 = 0.99;

/// A generator thread on the CPU nine-tenths of the window has no slack left to send on time: a single server
/// that busy keeps a queue of 4.5 of its own service times on average (M/D/1, rho / 2(1 - rho)).
const GEN_BUSY: f64 = 0.9;

/// The generator's own lateness, at p99, past a tenth of the latency's p99: the number is the generator's as much
/// as the server's.
const LAG_SHARE: f64 = 0.1;

/// Files the lab keeps open besides its connections: each generator thread's poller (three on Linux: epoll, its
/// timer and its waker) and its own few dozen (its standard streams, logs, the servers' pipes).
fn files_kept(lanes: usize) -> u64 {
    64 + 4 * lanes as u64
}

#[derive(clap::Args)]
pub struct Args {
    /// A target file (repeatable; docs/runtime-lab.md, "Fixed rates"): kiln's is crates/kiln-lab/targets/kiln.toml.
    #[arg(long = "target", required = true)]
    targets: Vec<PathBuf>,
    /// A kiln binary for the kiln targets, `label=path` or a path (repeatable): each kiln target runs once with each.
    #[arg(long = "kiln")]
    kilns: Vec<String>,
    /// The rates, requests a second, in order (`1k` and `1m` too).
    #[arg(long, default_value = "100,1000,10000,100000,1000000")]
    rates: String,
    /// Seconds of warm-up at each rate before its window (the lab's realistic profile's, 5, by default).
    #[arg(long)]
    warmup_s: Option<f64>,
    /// Seconds each rate is measured (by default the longer of 10 s and 10,000 requests' worth).
    #[arg(long)]
    window_s: Option<f64>,
    /// Connections at every rate (by default Little's law: the rate x --expect-ms x 2, over the requests a
    /// connection carries at once, at least one for each generator thread).
    #[arg(long)]
    conns: Option<usize>,
    /// Generator threads (by default a quarter of the CPUs this process may use, at least one, so the server keeps
    /// three quarters; all of them with --server-cpus, which gives the server its own).
    #[arg(long)]
    gen_cpus: Option<usize>,
    /// The latency connections are counted for, ms (goal 10's bar for an API read: 1).
    #[arg(long, default_value_t = 1.0)]
    expect_ms: f64,
    /// How long a request may wait for its answer, seconds (the lab's own patience, `client::PATIENCE`: 90).
    #[arg(long)]
    timeout_s: Option<f64>,
    /// Seconds between runs, so what one left queued in the server is served before the next begins.
    #[arg(long, default_value_t = 1.0)]
    settle_s: f64,
    /// The profile kiln targets are filled with, as `run`'s (realistic: 32 sessions, 2 of 20,000 events).
    #[arg(long, default_value = "realistic")]
    profile: String,
    /// The CPUs servers run on (Linux, taskset's list), as `run`'s; the lab keeps the CPUs it was started on.
    #[arg(long)]
    server_cpus: Option<String>,
    /// `shared` or `production` (Linux with systemd's user manager), as `run`'s.
    #[arg(long, default_value = "shared")]
    layout: String,
    /// `KEY=VALUE` for kiln servers' environment (repeatable).
    #[arg(long = "env")]
    env: Vec<String>,
    /// kiln servers' `[bus] memory`, as `run`'s.
    #[arg(long)]
    bus_memory: Option<u64>,
    /// Where the results go (`~/.cache/kiln-lab/rate-<when>` otherwise).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Keep each server's data rather than removing it (its logs are kept either way).
    #[arg(long)]
    keep: bool,
}

/// `100,1000,10k,1m`: the rates, each at least one a second.
fn rates(spec: &str) -> anyhow::Result<Vec<u64>> {
    spec.split(',')
        .map(|r| {
            let r = r.trim().to_ascii_lowercase();
            let (digits, times) = match r.strip_suffix('m') {
                Some(d) => (d.to_owned(), 1_000_000),
                None => match r.strip_suffix('k') {
                    Some(d) => (d.to_owned(), 1_000),
                    None => (r.clone(), 1),
                },
            };
            let n = digits.parse::<u64>().map_err(|_| anyhow::anyhow!("a rate of {r:?}: a number a second (1k, 1m)"))?.checked_mul(times).ok_or_else(|| anyhow::anyhow!("a rate of {r:?} past u64"))?;
            anyhow::ensure!(n > 0, "a rate of 0");
            Ok(n)
        })
        .collect()
}

/// The kiln builds `--kiln` names.
fn builds(a: &Args) -> anyhow::Result<Vec<Build>> {
    let env = crate::env(&a.env)?;
    let many = a.kilns.len() > 1;
    let mut out: Vec<Build> = Vec::new();
    for (i, k) in a.kilns.iter().enumerate() {
        let (label, path) = match k.split_once('=') {
            Some((l, p)) if !l.is_empty() => (l.to_owned(), p),
            _ => (if many { format!("kiln-{}", i + 1) } else { "kiln".to_owned() }, k.as_str()),
        };
        let kiln = std::path::absolute(path)?;
        anyhow::ensure!(kiln.is_file(), "no kiln at {}", kiln.display());
        anyhow::ensure!(out.iter().all(|b| b.label != label), "two --kiln labelled {label}");
        out.push(Build { label, kiln, env: env.clone(), bus_memory: a.bus_memory });
    }
    Ok(out)
}

/// What a rate's run is: its phases, and its connections, `capped` where the open-files limit held them.
fn plan(rate: u64, a: &Args, lanes: usize, depth: u32, room: Option<u64>) -> (load::Plan, Option<usize>) {
    #[allow(clippy::cast_precision_loss)]
    let r = rate as f64;
    let window = a.window_s.unwrap_or_else(|| MIN_WINDOW_S.max((TAIL_REQUESTS / r).ceil()));
    #[allow(clippy::cast_precision_loss)]
    let warm = a.warmup_s.unwrap_or(Profile::realistic().warmup_s as f64);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let little = (r * a.expect_ms / 1000.0 * HEADROOM / f64::from(depth.max(1))).ceil() as usize;
    let mut conns = a.conns.unwrap_or(little).max(lanes);
    let mut capped = None;
    if let Some(room) = room.and_then(|r| usize::try_from(r).ok())
        && conns > room
    {
        capped = Some(conns);
        conns = room.max(lanes);
    }
    let secs = |s: f64| Duration::try_from_secs_f64(s.max(0.0)).unwrap_or(Duration::MAX);
    let timeout = secs(a.timeout_s.unwrap_or(crate::client::PATIENCE.as_secs_f64()));
    (load::Plan { rate, lanes, conns, warm: secs(warm), window: secs(window), grace: secs(warm), timeout }, capped)
}

/// The generator's side of a run.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GenSeen {
    /// The generator process's CPUs' worth over the window.
    pub cpu_share: Option<f64>,
    /// Its busiest thread's work (its CPU, less its spinning before sends) and its threads' mean spinning, as
    /// shares of the window.
    pub busy_max: Option<f64>,
    pub spin_share: Option<f64>,
    /// Its own lateness sending (from when a request fell due, or a connection came free after), µs.
    pub lag_p50_us: Option<f64>,
    pub lag_p99_us: Option<f64>,
    pub lag_max_us: Option<f64>,
    /// The share of the window's requests that found every connection busy, and how long they waited at p99.
    pub waited_share: f64,
    pub wait_p99_ms: Option<f64>,
    /// What its threads got of their OS to be punctual.
    pub punctual: String,
}

/// One target's read at one rate.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Line {
    pub started: String,
    pub host: String,
    pub target: String,
    /// The `--kiln` label, for kiln's targets.
    pub build: Option<String>,
    pub workload: String,
    /// The read's place in its target's file, from 1: targets' reads are compared by it.
    pub place: usize,
    pub kind: String,
    pub rate: u64,
    pub warmup_s: f64,
    pub window_s: f64,
    pub timeout_s: f64,
    pub gen_threads: usize,
    pub conns: usize,
    /// What Little's law asked for, where the open-files limit held the connections under it.
    pub conns_wanted: Option<usize>,
    pub in_flight_per_conn: u32,
    pub opened: usize,
    /// Sent during the window, and answered during it (whatever fell due when), a second.
    pub sent_rps: f64,
    pub achieved_rps: f64,
    pub ok: u64,
    /// Requests of the window that failed: answered badly, timed out, their connection gone, or never sent.
    pub errors: u64,
    pub bad: u64,
    pub timeouts: u64,
    pub closed: u64,
    pub unsent: u64,
    /// Connections that wouldn't open, and connections opened again.
    pub refused: u64,
    pub reconnects: u64,
    pub unmatched: u64,
    /// Good answers' latency from when each fell due, ms.
    pub p50_ms: Option<f64>,
    pub p99_ms: Option<f64>,
    pub p999_ms: Option<f64>,
    pub max_ms: Option<f64>,
    pub mean_ms: Option<f64>,
    /// The same timed from each send: what a measurement with coordinated omission would have said.
    pub service_p50_ms: Option<f64>,
    pub service_p99_ms: Option<f64>,
    pub service_p999_ms: Option<f64>,
    pub bytes_per_answer: Option<f64>,
    pub server: sample::Window,
    pub generator: GenSeen,
    pub kept_up: bool,
    /// Who couldn't keep up (`generator`, `connections`, `server`), where one couldn't, or the generator was late
    /// enough to be in the latencies.
    pub limited_by: Option<String>,
    pub why: String,
    pub errors_seen: Vec<String>,
    /// Why the run didn't run.
    pub failed: Option<String>,
}

#[allow(clippy::cast_precision_loss)]
fn ms(ns: u64) -> f64 {
    ns as f64 / 1e6
}

#[allow(clippy::cast_precision_loss)]
fn us(ns: u64) -> f64 {
    ns as f64 / 1e3
}

impl Line {
    /// What a run's threads and the server's samples came to.
    fn of(&mut self, ran: &load::Ran, server: sample::Window, gen_cpu: Option<f64>) {
        let s = &ran.seen;
        #[allow(clippy::cast_precision_loss)]
        let per_s = |n: u64| n as f64 / self.window_s.max(1e-9);
        (self.sent_rps, self.achieved_rps) = (per_s(s.sent_in_window), per_s(s.answered_in_window));
        (self.ok, self.bad, self.timeouts, self.closed, self.unsent) = (s.ok, s.bad, s.timeouts, s.closed, ran.unsent);
        self.errors = s.bad + s.timeouts + s.closed + ran.unsent;
        (self.opened, self.refused, self.reconnects, self.unmatched) = (ran.opened, ran.refused, ran.reconnects, ran.unmatched);
        let q = |h: &hist::Hist, q: f64| h.quantile(q).map(ms);
        (self.p50_ms, self.p99_ms, self.p999_ms, self.max_ms) = (q(&s.latency, 0.5), q(&s.latency, 0.99), q(&s.latency, 0.999), s.latency.max().map(ms));
        self.mean_ms = s.latency.mean().map(|m| m / 1e6);
        (self.service_p50_ms, self.service_p99_ms, self.service_p999_ms) = (q(&s.service, 0.5), q(&s.service, 0.99), q(&s.service, 0.999));
        #[allow(clippy::cast_precision_loss)]
        let bytes = (s.ok > 0).then(|| s.bytes as f64 / s.ok as f64);
        self.bytes_per_answer = bytes;
        self.server = server;
        #[allow(clippy::cast_precision_loss)]
        let waited = if s.sent > 0 { s.waited as f64 / s.sent as f64 } else { 0.0 };
        #[allow(clippy::cast_precision_loss)]
        let spin = (!ran.spin.is_empty()).then(|| ran.spin.iter().sum::<f64>() / ran.spin.len() as f64);
        self.generator = GenSeen {
            cpu_share: gen_cpu,
            busy_max: ran.busy.iter().flatten().copied().reduce(f64::max),
            spin_share: spin,
            lag_p50_us: s.lag.quantile(0.5).map(us),
            lag_p99_us: s.lag.quantile(0.99).map(us),
            lag_max_us: s.lag.max().map(us),
            waited_share: waited,
            wait_p99_ms: s.wait.quantile(0.99).map(ms),
            punctual: ran.punctual.clone(),
        };
        self.errors_seen.clone_from(&ran.errors);
        self.verdict();
    }

    /// Whether the rate was kept up, and if not (or if the generator's lateness is in the latencies), who
    /// couldn't keep up, and why.
    fn verdict(&mut self) {
        #[allow(clippy::cast_precision_loss)]
        let (rate, due) = (self.rate as f64, (self.ok + self.errors) as f64);
        #[allow(clippy::cast_precision_loss)]
        let erred = self.errors as f64;
        self.kept_up = self.achieved_rps >= KEPT * rate && erred <= (1.0 - KEPT) * due.max(1.0);
        let g = &self.generator;
        let busy = g.busy_max.filter(|b| *b >= GEN_BUSY);
        let lag = match (g.lag_p99_us, self.p99_ms) {
            (Some(lag), Some(p99)) if lag / 1000.0 >= LAG_SHARE * p99 => Some((lag, lag / 1000.0 / p99)),
            _ => None,
        };
        let connections = g.waited_share > 1.0 - KEPT;
        let (by, why) = if let Some(b) = busy {
            (Some("generator"), format!("the generator: its busiest thread worked {:.0}% of the window (more --gen-cpus)", b * 100.0))
        } else if let Some((lag, share)) = lag {
            (Some("generator"), format!("the generator: its own lateness at p99, {lag:.0} µs, is {:.0}% of the latency's p99, with no thread past {:.0}% busy: the OS woke its threads late or kept them off the CPU (give the server CPUs of its own: --server-cpus)", share * 100.0, GEN_BUSY * 100.0))
        } else if !self.kept_up && connections {
            (Some("connections"), format!("every connection was busy for {:.1}% of the requests (they waited {} ms at p99): at {} connections the server's answers took longer than this rate allows (more --conns)", g.waited_share * 100.0, g.wait_p99_ms.map_or("-".to_owned(), |w| format!("{w:.3}")), self.conns))
        } else if !self.kept_up {
            (Some("server"), format!("the server: answered {:.0} of {} a second, {} errors ({} bad, {} timed out, {} cut off, {} never sent)", self.achieved_rps, self.rate, self.errors, self.bad, self.timeouts, self.closed, self.unsent))
        } else if connections {
            (None, format!("{:.1}% of the requests found every connection busy, and their latency counts that wait", g.waited_share * 100.0))
        } else {
            (None, String::new())
        };
        self.limited_by = by.map(str::to_owned);
        self.why = why;
    }

    /// The target as the table names it.
    fn who(&self) -> String {
        match &self.build {
            Some(b) => format!("{} ({b})", self.target),
            None => self.target.clone(),
        }
    }

    /// Its line on stderr as it's done.
    fn summary(&self) -> String {
        if let Some(f) = &self.failed {
            return format!("{} {} at {}/s: failed: {f}", self.who(), self.workload, self.rate);
        }
        let f = |v: Option<f64>| v.map_or_else(|| "-".to_owned(), |v| format!("{v:.3}"));
        format!(
            "{} {} at {}/s: achieved {:.0}/s, {} ok, {} errors, p50 {} p99 {} p99.9 {} max {} ms, server {} CPUs {} MB, generator {} CPUs{}",
            self.who(),
            self.workload,
            self.rate,
            self.achieved_rps,
            self.ok,
            self.errors,
            f(self.p50_ms),
            f(self.p99_ms),
            f(self.p999_ms),
            f(self.max_ms),
            f(self.server.cpu_share),
            f(self.server.rss_mb),
            f(self.generator.cpu_share),
            if self.why.is_empty() { String::new() } else { format!(": {}", self.why) }
        )
    }
}

/// A name as a folder's.
fn slug(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect()
}

/// The run's settings, as the report states them.
struct Settings {
    command: String,
    started: String,
    host: String,
    os: String,
    cpus: usize,
    lanes: usize,
    files: Option<u64>,
    timeout_s: f64,
    settle_s: f64,
    warmup: String,
    window: String,
    conns: String,
    profile: String,
}

/// The run as Markdown: its settings, then each read at each rate, targets side by side.
fn markdown(set: &Settings, lines: &[Line], failures: &[String]) -> String {
    let mut s = format!("# kiln-lab rate: {}\n\n`{}`\n\n", set.started, set.command);
    s.push_str(&format!(
        "On {} ({}, {} CPUs; open files {}). Open loop, without coordinated omission: each request's latency runs from \
when the schedule said to send it, so a stall is charged to every request it held back. Each rate: a warm-up of {} \
at the rate, then the measured window ({}), then the drain (the window's backlog still sent for a warm-up's length; \
each request answered within {} s or timed out), then {} s to settle. Connections: {}. Generator: {} threads. kiln \
targets filled with the {} profile. Latency: good answers, ms. Server CPU: CPUs' worth through the window (1.00 is \
one CPU throughout); server RSS: MB, the window's mean; both the server's whole process tree (`/proc` on Linux, `ps` \
on macOS). Gen CPU: the generator process's CPUs' worth.\n\n",
        set.host,
        set.os,
        set.cpus,
        set.files.map_or_else(|| "not limited".to_owned(), |f| f.to_string()),
        set.warmup,
        set.window,
        set.timeout_s,
        set.settle_s,
        set.conns,
        set.lanes,
        set.profile
    ));
    let mut rows: Vec<&Line> = lines.iter().collect();
    rows.sort_by_key(|l| (l.place, l.rate));
    s.push_str("| read | rate/s | target | achieved/s | ok | errors | p50 | p99 | p99.9 | max | server CPU | server RSS MB | gen CPU | conns | kept up |\n");
    s.push_str("|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|\n");
    let f = |v: Option<f64>, d: usize| v.map_or_else(|| "-".to_owned(), |v| format!("{v:.d$}"));
    for l in &rows {
        if l.failed.is_some() {
            s.push_str(&format!("| {}. {} | {} | {} | - | - | - | - | - | - | - | - | - | - | - | failed |\n", l.place, l.workload, l.rate, l.who()));
            continue;
        }
        let kept = match (&l.limited_by, l.kept_up) {
            (None, true) => "yes".to_owned(),
            (Some(by), true) => format!("yes ({by} late)"),
            (Some(by), false) => format!("no: {by}"),
            (None, false) => "no".to_owned(),
        };
        s.push_str(&format!(
            "| {}. {} | {} | {} | {:.0} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {kept} |\n",
            l.place,
            l.workload,
            l.rate,
            l.who(),
            l.achieved_rps,
            l.ok,
            l.errors,
            f(l.p50_ms, 3),
            f(l.p99_ms, 3),
            f(l.p999_ms, 3),
            f(l.max_ms, 3),
            f(l.server.cpu_share, 2),
            f(l.server.rss_mb, 1),
            f(l.generator.cpu_share, 2),
            l.conns,
        ));
    }
    let notes: Vec<String> = rows
        .iter()
        .filter(|l| !l.why.is_empty() || l.failed.is_some() || !l.errors_seen.is_empty() || l.conns_wanted.is_some())
        .map(|l| {
            let mut n = format!("- {}. {} at {}/s, {}: ", l.place, l.workload, l.rate, l.who());
            let mut parts: Vec<String> = Vec::new();
            parts.extend(l.failed.iter().map(|f| format!("failed: {f}")));
            if !l.why.is_empty() {
                parts.push(l.why.clone());
            }
            if let Some(w) = l.conns_wanted {
                parts.push(format!("Little's law asked {w} connections; the open-files limit held them to {}", l.conns));
            }
            if !l.errors_seen.is_empty() {
                parts.push(format!("errors seen: {}", l.errors_seen.join("; ")));
            }
            n.push_str(&parts.join(". "));
            n
        })
        .collect();
    if !notes.is_empty() {
        s.push_str("\n## Notes\n\n");
        s.push_str(&notes.join("\n"));
        s.push('\n');
    }
    if !failures.is_empty() {
        s.push_str("\n## Targets that didn't run\n\n");
        for f in failures {
            s.push_str(&format!("- {f}\n"));
        }
    }
    s
}

/// One read at one rate against a server that's up.
#[allow(clippy::too_many_arguments)]
fn one(started: &str, host: &str, t: &Target, build: Option<&Build>, up: &serve::Up, place: usize, w: &Workload, rate: u64, a: &Args, lanes: usize, room: Option<u64>, tree: &mut sample::Tree) -> Line {
    let (kind, depth) = match &w.kind {
        Kind::Http(h) => ("http", h.pipeline),
        Kind::WsRpc(r) => ("ws-rpc", r.in_flight),
    };
    let (plan, wanted) = plan(rate, a, lanes, depth, room);
    let mut line = Line {
        started: started.to_owned(),
        host: host.to_owned(),
        target: t.name.clone(),
        build: build.map(|b| b.label.clone()),
        workload: w.name.clone(),
        place,
        kind: kind.to_owned(),
        rate,
        warmup_s: plan.warm.as_secs_f64(),
        window_s: plan.window.as_secs_f64(),
        timeout_s: plan.timeout.as_secs_f64(),
        gen_threads: plan.lanes,
        conns: plan.conns,
        conns_wanted: wanted,
        in_flight_per_conn: depth,
        ..Line::default()
    };
    let job = match load::Job::new(w, &up.vars, up.port) {
        Ok(j) => j,
        Err(e) => {
            line.failed = Some(e);
            return line;
        }
    };
    let addr = SocketAddr::from(([127, 0, 0, 1], up.port));
    let (ran, (server, gen_cpu)) = load::run(addr, &job, &plan, |from, to| {
        std::thread::sleep(from.saturating_duration_since(Instant::now()));
        let before = os::process_cpu_ns();
        let server = sample::window(tree, from, to);
        let span = Instant::now().saturating_duration_since(from).as_secs_f64();
        #[allow(clippy::cast_precision_loss)]
        let cpu = match (before, os::process_cpu_ns()) {
            (Some(b), Some(e)) if span > 0.0 => Some(e.saturating_sub(b) as f64 / 1e9 / span),
            _ => None,
        };
        (server, cpu)
    });
    line.of(&ran, server, gen_cpu);
    line
}

/// Copies `dir`'s logs to `to` (a server's data goes; its logs stay).
fn keep_logs(dir: &Path, to: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let _ = std::fs::create_dir_all(to);
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "log" || x == "out") {
            let _ = std::fs::copy(&p, to.join(e.file_name()));
        }
    }
}

pub fn main(a: &Args) -> anyhow::Result<()> {
    let targets: Vec<Target> = a.targets.iter().map(|p| Target::load(p)).collect::<anyhow::Result<_>>()?;
    let builds = builds(a)?;
    let kiln_targets = targets.iter().any(|t| matches!(t.server, Server::Kiln));
    anyhow::ensure!(!kiln_targets || !builds.is_empty(), "a kiln target needs --kiln (the binaries to start)");
    anyhow::ensure!(kiln_targets || builds.is_empty(), "--kiln, but no kiln target (crates/kiln-lab/targets/kiln.toml)");
    let rates = rates(&a.rates)?;
    let profile = Profile::named(&a.profile)?;
    let place = Place::of(a.server_cpus.as_deref(), &a.layout)?;
    let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let lanes = a.gen_cpus.unwrap_or(if place.cpus.is_some() { cpus } else { (cpus / 4).max(1) }).max(1);
    // The lab's soft limit up to production's service's, before any server starts (they inherit it).
    let files = os::open_files(os::SERVICE_FILES);
    let room = files.map(|f| f.saturating_sub(files_kept(lanes)));
    let started_secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let started = crate::utc(started_secs);
    let out = match &a.out {
        Some(o) => o.clone(),
        None => PathBuf::from(std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("no HOME"))?).join(".cache/kiln-lab").join(format!("rate-{}", started.replace(':', ""))),
    };
    std::fs::create_dir_all(&out)?;
    let bin = if kiln_targets { Some(crate::lab::stand_in(&out)?) } else { None };
    let machine = crate::lab::machine();
    let jsonl = out.join("results.jsonl");
    let mut lines: Vec<Line> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    let mut n = 0;
    for t in &targets {
        let instances: Vec<Option<&Build>> = match t.server {
            Server::Kiln => builds.iter().map(Some).collect(),
            Server::Command(_) => vec![None],
        };
        for build in instances {
            n += 1;
            let who = build.map_or_else(|| t.name.clone(), |b| format!("{} ({})", t.name, b.label));
            let dir = out.join("targets").join(format!("{n:02}-{}", slug(&who)));
            let server_dir = dir.join("server");
            let unit = format!("kiln-lab-rate-{}-{n}", std::process::id());
            eprintln!("kiln-lab rate: {who}: starting its server in {}", server_dir.display());
            let up = match (&t.server, build, &bin) {
                (Server::Kiln, Some(b), Some(bin)) => serve::kiln(b, &profile, &place, &server_dir, bin, &unit),
                (Server::Command(c), _, _) => serve::command(c, &place, &server_dir, &unit),
                _ => Err(anyhow::anyhow!("a kiln target without a kiln")),
            };
            match up {
                Err(e) => {
                    eprintln!("kiln-lab rate: {who} didn't start: {e:#}");
                    failures.push(format!("{who}: {e:#}"));
                }
                Ok(up) => {
                    if let Some(f) = up.fill_s {
                        eprintln!("kiln-lab rate: {who}: up on {}, filled in {f:.1} s", up.port);
                    }
                    match t.check(&|name| up.vars.has(name)) {
                        Err(e) => failures.push(format!("{who}: {e}")),
                        Ok(()) => {
                            let mut tree = sample::Tree::of(up.pid);
                            for &rate in &rates {
                                for (i, w) in t.workloads.iter().enumerate() {
                                    let line = one(&started, &machine.host, t, build, &up, i + 1, w, rate, a, lanes, room, &mut tree);
                                    eprintln!("kiln-lab rate: {}", line.summary());
                                    let mut text = serde_json::to_string(&line)?;
                                    text.push('\n');
                                    std::fs::OpenOptions::new().create(true).append(true).open(&jsonl)?.write_all(text.as_bytes())?;
                                    lines.push(line);
                                    std::thread::sleep(Duration::try_from_secs_f64(a.settle_s.max(0.0)).unwrap_or(Duration::ZERO));
                                }
                            }
                        }
                    }
                    drop(up);
                }
            }
            keep_logs(&server_dir, &dir.join("logs"));
            if !a.keep {
                crate::lab::remove_paced(&server_dir);
            }
        }
    }
    let first = plan(rates.first().copied().unwrap_or(1), a, lanes, 1, room).0;
    let set = Settings {
        command: std::env::args().collect::<Vec<_>>().join(" "),
        started,
        host: machine.host.clone(),
        os: machine.os.clone(),
        cpus,
        lanes,
        files,
        timeout_s: first.timeout.as_secs_f64(),
        settle_s: a.settle_s,
        warmup: format!("{} s", first.warm.as_secs_f64()),
        window: a.window_s.map_or_else(|| format!("the longer of {MIN_WINDOW_S} s and {TAIL_REQUESTS} requests' worth"), |w| format!("{w} s")),
        conns: a.conns.map_or_else(|| format!("Little's law, the rate x {} ms x {HEADROOM} over the requests a connection carries at once, at least one a thread", a.expect_ms), |c| format!("{c} at every rate")),
        profile: a.profile.clone(),
    };
    let md = markdown(&set, &lines, &failures);
    std::fs::write(out.join("report.md"), &md)?;
    println!("{md}");
    eprintln!("kiln-lab rate: the results are in {}", out.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[derive(clap::Parser)]
    struct Cli {
        #[command(flatten)]
        args: Args,
    }

    fn args(extra: &[&str]) -> Result<Args, String> {
        let mut argv = vec!["rate", "--target", "kiln.toml"];
        argv.extend_from_slice(extra);
        Cli::try_parse_from(argv).map(|c| c.args).map_err(|e| e.to_string())
    }

    /// Goal 10 (Ada's fixed-rate benchmark): rates read as numbers with k and m; the defaults are derived and as
    /// stated: a window of the longer of 10 s and 10,000 requests, a warm-up of the realistic profile's, Little's law
    /// for the connections (at least one a thread, at most what the open files allow), the lab's patience.
    #[test]
    fn rate_defaults_are_derived_as_stated() -> Result<(), String> {
        assert_eq!(rates("100,1000,10k,100k,1m").ok(), Some(vec![100, 1000, 10_000, 100_000, 1_000_000]));
        assert!(rates("0").is_err() && rates("fast").is_err() && rates("").is_err());
        let a = args(&[])?;
        assert_eq!(a.rates, "100,1000,10000,100000,1000000");
        let (p, capped) = plan(100, &a, 8, 1, None);
        assert_eq!((p.window, p.warm, p.conns, p.lanes, p.timeout, capped), (Duration::from_secs(100), Duration::from_secs(5), 8, 8, Duration::from_secs(90), None));
        let (p, _) = plan(1_000_000, &a, 8, 1, None);
        assert_eq!((p.window, p.conns), (Duration::from_secs(10), 2000), "a million a second at 1 ms, each connection half busy");
        assert_eq!(plan(1_000_000, &a, 8, 4, None).0.conns, 500, "four in flight a connection: a quarter as many");
        let (p, capped) = plan(1_000_000, &a, 8, 1, Some(1000));
        assert_eq!((p.conns, capped), (1000, Some(2000)), "held to what the open files allow, and said");
        let a = args(&["--conns", "3", "--window-s", "2", "--warmup-s", "0.5", "--timeout-s", "4"])?;
        let (p, _) = plan(100_000, &a, 8, 1, None);
        assert_eq!((p.conns, p.window, p.warm, p.timeout, p.grace), (8, Duration::from_secs(2), Duration::from_millis(500), Duration::from_secs(4), Duration::from_millis(500)), "--conns is at least one a thread");
        assert!(args(&["--rates"]).is_err(), "--rates takes its list");
        Ok(())
    }

    fn ran(ok: u64, unsent: u64, busy: f64, lag_p99_ns: u64, latency_ns: u64, waited: u64) -> load::Ran {
        let mut seen = lane::Seen::default();
        for _ in 0..ok {
            seen.latency.add(latency_ns);
            seen.service.add(latency_ns);
            seen.lag.add(lag_p99_ns);
        }
        (seen.ok, seen.sent, seen.waited, seen.sent_in_window, seen.answered_in_window) = (ok, ok, waited, ok, ok);
        load::Ran { seen, unsent, opened: 4, busy: vec![Some(busy)], spin: vec![0.0], punctual: "timer slack 1 ns".into(), ..load::Ran::default() }
    }

    /// Goal 10 (Ada's fixed-rate benchmark): a run says who couldn't keep up. The generator when its busiest thread
    /// worked past 90% of the window, or its own lateness is a tenth of the latency's p99; the connections when the
    /// rate fell short while requests waited for one; the server when it fell short otherwise; no one when the rate
    /// was answered.
    #[test]
    fn rate_a_run_says_who_couldnt_keep_up() {
        let line = |r: load::Ran| {
            let mut l = Line { rate: 1000, window_s: 1.0, ..Line::default() };
            l.of(&r, sample::Window::default(), Some(0.1));
            l
        };
        let fine = line(ran(1000, 0, 0.2, 5_000, 300_000, 0));
        assert_eq!((fine.kept_up, fine.limited_by.as_deref(), fine.p99_ms.map(|p| (p * 1000.0).round())), (true, None, Some(300.0)));
        let busy = line(ran(1000, 0, 0.95, 5_000, 300_000, 0));
        assert_eq!((busy.kept_up, busy.limited_by.as_deref()), (true, Some("generator")), "{}", busy.why);
        let late = line(ran(1000, 0, 0.2, 60_000, 300_000, 0));
        assert_eq!(late.limited_by.as_deref(), Some("generator"), "60 µs late against a 300 µs p99: {}", late.why);
        let short = line(ran(800, 200, 0.2, 5_000, 300_000, 0));
        assert_eq!((short.kept_up, short.limited_by.as_deref(), short.errors), (false, Some("server"), 200), "{}", short.why);
        let starved = line(ran(800, 200, 0.2, 5_000, 300_000, 500));
        assert_eq!((starved.kept_up, starved.limited_by.as_deref()), (false, Some("connections")), "{}", starved.why);
        let md = markdown(
            &Settings { command: "kiln-lab rate".into(), started: "now".into(), host: "h".into(), os: "linux".into(), cpus: 8, lanes: 2, files: Some(1024), timeout_s: 90.0, settle_s: 1.0, warmup: "5 s".into(), window: "10 s".into(), conns: "Little's law".into(), profile: "realistic".into() },
            &[Line { target: "kiln".into(), build: Some("prod".into()), workload: "sessions".into(), place: 1, ..fine }, Line { target: "t3code".into(), workload: "threads".into(), place: 1, ..short }],
            &["x: didn't start".into()],
        );
        assert!(md.contains("| 1. sessions | 1000 | kiln (prod) | 1000 | 1000 | 0 | 0.300 |"), "{md}");
        assert!(md.contains("| 1. threads | 1000 | t3code | 800 |") && md.contains("no: server |"), "{md}");
        assert!(md.contains("## Targets that didn't run\n\n- x: didn't start"), "{md}");
    }
}
