//! The broker's side of a profile, read a quarter second before the window and a quarter second
//! after it, so no reading lands in what's measured: its container's account (cgroup v2:
//! `cpu.stat`'s CPU, `memory.stat`'s page faults, `memory.current` sampled through the window for
//! its peak, `io.stat`'s storage reads and writes), and its own runtime's:
//!
//! - NATS: Go's `runtime.MemStats` (objects allocated and freed, bytes allocated, collections, the
//!   heap in use, and the last 256 collections' pauses, of which the window's are summed), from the
//!   heap profile its profiling port serves (`nats-server --profile`, `debug=1`); the window's
//!   allocation profile (`allocs?seconds=`) and the heap profile after it saved beside the run, for
//!   `go tool pprof`.
//! - Redis: jemalloc's allocation requests (`MEMORY MALLOC-STATS`, every arena merged) and the
//!   memory Redis says it uses (`INFO memory`); `MEMORY STATS` and the allocator's statistics after
//!   the window saved.
//! - Kafka: a Java Flight Recorder recording over the run, started and stopped in its JVM by `jcmd`
//!   from this host (the JVM runs as this user, and a JDK's attach reaches into its container), its
//!   threads' allocated bytes sampled every 100 ms: the bytes the JVM allocated in the window, its
//!   collections and their pauses, the heap after the last; the recording saved, its allocation
//!   samples and their stacks for JDK Mission Control or `jfr print`.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::{BrokerFigures, Figure, alloc};
use crate::System;

/// NATS's profiling port, as `bench/compare.sh` starts it.
const NATS_PROFILE: &str = "127.0.0.1:16060";
/// Kafka's JFR events the profile reads.
const JFR_EVENTS: &str = "jdk.ThreadAllocationStatistics,jdk.GarbageCollection,jdk.GCHeapSummary";

/// A broker under profile.
pub struct Target {
    pub system: System,
    pub container: String,
    /// Its main process on this host, and its cgroup.
    pub pid: Option<u32>,
    pub cgroup: Option<PathBuf>,
    pub endpoint: String,
    /// Where its saved profiles go, and the run's name for them.
    pub profiles: Option<PathBuf>,
    pub tag: String,
}

impl Target {
    /// The broker in Docker container `container`: its process by `docker inspect`, its cgroup from
    /// the process's `/proc/<pid>/cgroup`.
    pub fn new(
        system: System,
        container: &str,
        endpoint: &str,
        profiles: Option<PathBuf>,
        tag: String,
    ) -> Self {
        let pid = Command::new("docker")
            .args(["inspect", "-f", "{{.State.Pid}}", container])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|&p| p > 0);
        Self {
            system,
            container: container.to_owned(),
            pid,
            cgroup: pid.and_then(cgroup_of),
            endpoint: endpoint.to_owned(),
            profiles,
            tag,
        }
    }
}

/// Process `pid`'s cgroup v2 directory.
pub fn cgroup_of(pid: u32) -> Option<PathBuf> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let path = text.lines().find_map(|l| l.strip_prefix("0::"))?;
    Some(PathBuf::from(format!("/sys/fs/cgroup{}", path.trim())))
}

/// Go's allocator, as `runtime.MemStats` counts it.
#[derive(Clone, Default)]
struct Go {
    mallocs: u64,
    frees: u64,
    total_alloc: u64,
    heap_inuse: u64,
    num_gc: u64,
    /// The last collections' pauses, nanoseconds: collection `k` (from 1) at `(k - 1) % 256`.
    pauses: Vec<u64>,
}

impl Go {
    /// The pauses of the collections since `earlier`, if Go still keeps them all.
    fn pauses_since(&self, earlier: &Self) -> Figure {
        let n = self.num_gc.saturating_sub(earlier.num_gc);
        let kept = self.pauses.len() as u64;
        if n > kept {
            return Figure::Refused(format!(
                "refused: {n} collections in the span, past the {kept} pauses Go keeps"
            ));
        }
        let sum: u64 = (earlier.num_gc + 1..=self.num_gc)
            .map(|k| {
                self.pauses
                    .get(((k - 1) % kept.max(1)) as usize)
                    .copied()
                    .unwrap_or(0)
            })
            .sum();
        Figure::Is(sum as f64)
    }
}

/// jemalloc's requests, and the memory Redis says it uses.
#[derive(Clone, Copy, Default)]
struct Jemalloc {
    requests: u64,
    used: u64,
}

enum Own {
    Go(Result<Go, String>),
    Jemalloc(Result<Jemalloc, String>),
    None,
}

/// The broker's account at one moment.
pub struct Snap {
    pub at: Instant,
    /// Nanoseconds since the epoch, as Kafka's recording stamps its events.
    wall: i128,
    cpu_us: Option<u64>,
    faults: Option<(u64, u64)>,
    memory: Option<u64>,
    io: Option<[u64; 4]>,
    own: Own,
}

fn value(dir: &Path, file: &str, key: &str) -> Option<u64> {
    std::fs::read_to_string(dir.join(file))
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix(' ')?.trim().parse().ok())
}

/// `io.stat`'s bytes read and written and reads and writes, every device's together.
fn io_stat(dir: &Path) -> Option<[u64; 4]> {
    let text = std::fs::read_to_string(dir.join("io.stat")).ok()?;
    let mut sum = [0u64; 4];
    for kv in text.lines().flat_map(|l| l.split_whitespace().skip(1)) {
        let Some((k, v)) = kv.split_once('=') else {
            continue;
        };
        let i = match k {
            "rbytes" => 0,
            "wbytes" => 1,
            "rios" => 2,
            "wios" => 3,
            _ => continue,
        };
        sum[i] += v.parse::<u64>().unwrap_or(0);
    }
    Some(sum)
}

fn wall() -> i128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i128::try_from(d.as_nanos()).unwrap_or(0))
}

/// An HTTP/1.0 GET of `path` from `addr`: the body of a 200, or why not.
fn get(addr: &str, path: &str, timeout: Duration) -> Result<Vec<u8>, String> {
    let to: std::net::SocketAddr = addr.parse().map_err(|e| format!("{addr}: {e}"))?;
    let mut s = TcpStream::connect_timeout(&to, Duration::from_secs(2))
        .map_err(|e| format!("{addr}: {e}"))?;
    s.set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    write!(s, "GET {path} HTTP/1.0\r\nHost: {addr}\r\n\r\n").map_err(|e| e.to_string())?;
    let mut answer = Vec::new();
    s.read_to_end(&mut answer).map_err(|e| e.to_string())?;
    let split = answer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| format!("{path}: no HTTP answer"))?;
    let head = String::from_utf8_lossy(&answer[..split]);
    let status = head.lines().next().unwrap_or_default();
    if status.split_whitespace().nth(1) != Some("200") {
        return Err(format!("{path}: {status}"));
    }
    Ok(answer[split + 4..].to_vec())
}

fn go() -> Result<Go, String> {
    let body = get(
        NATS_PROFILE,
        "/debug/pprof/heap?debug=1",
        Duration::from_secs(10),
    )
    .map_err(|e| format!("refused: no profiling port ({e}); nats-server --profile 16060"))?;
    let text = String::from_utf8_lossy(&body);
    let field = |name: &str| {
        text.lines()
            .find_map(|l| {
                l.strip_prefix("# ")?
                    .strip_prefix(name)?
                    .strip_prefix(" = ")?
                    .trim()
                    .parse::<u64>()
                    .ok()
            })
            .ok_or_else(|| format!("refused: runtime.MemStats has no {name}"))
    };
    let pauses = text
        .lines()
        .find_map(|l| l.strip_prefix("# PauseNs = "))
        .map(|v| {
            v.trim_matches(|c: char| c == '[' || c == ']' || c.is_whitespace())
                .split_whitespace()
                .filter_map(|p| p.parse::<u64>().ok())
                .collect::<Vec<_>>()
        })
        .ok_or("refused: runtime.MemStats has no PauseNs")?;
    Ok(Go {
        mallocs: field("Mallocs")?,
        frees: field("Frees")?,
        total_alloc: field("TotalAlloc")?,
        heap_inuse: field("HeapInuse")?,
        num_gc: field("NumGC")?,
        pauses,
    })
}

fn redis_connection(endpoint: &str) -> Result<redis::Connection, String> {
    redis::Client::open(format!("redis://{endpoint}"))
        .and_then(|c| c.get_connection_with_timeout(Duration::from_secs(2)))
        .map_err(|e| format!("refused: {endpoint}: {e}"))
}

/// The merged arenas' allocation requests in jemalloc's statistics (malloc_stats_print's
/// "Merged arenas stats", its `total:` row's `nrequests`).
fn jemalloc_requests(stats: &str) -> Option<u64> {
    let merged = stats.split_once("Merged arenas stats:")?.1;
    let mut column = None;
    for line in merged.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        match (column, tokens.first()) {
            (None, _) if tokens.contains(&"allocated") && tokens.contains(&"nrequests") => {
                column = tokens.iter().position(|t| *t == "nrequests");
            }
            (Some(c), Some(&"total:")) => return tokens.get(c + 1)?.parse().ok(),
            _ => {}
        }
    }
    None
}

fn jemalloc(endpoint: &str) -> Result<(Jemalloc, String), String> {
    let mut c = redis_connection(endpoint)?;
    let info: String = redis::cmd("INFO")
        .arg("memory")
        .query(&mut c)
        .map_err(|e| format!("refused: INFO memory: {e}"))?;
    let field = |name: &str| {
        info.lines()
            .find_map(|l| Some(l.strip_prefix(name)?.strip_prefix(':')?.trim()))
    };
    let used = field("used_memory")
        .and_then(|v| v.parse().ok())
        .ok_or("refused: INFO memory has no used_memory")?;
    let allocator = field("mem_allocator").unwrap_or("unknown");
    if !allocator.starts_with("jemalloc") {
        return Err(format!(
            "n/a: Redis's allocator is {allocator}, which counts no requests"
        ));
    }
    let stats: String = redis::cmd("MEMORY")
        .arg("MALLOC-STATS")
        .query(&mut c)
        .map_err(|e| format!("refused: MEMORY MALLOC-STATS: {e}"))?;
    let requests = jemalloc_requests(&stats)
        .ok_or("refused: MEMORY MALLOC-STATS has no merged arenas' total")?;
    Ok((Jemalloc { requests, used }, stats))
}

/// A Redis reply as text, one key a line where it pairs keys with values.
fn render(v: &redis::Value, depth: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    let scalar = |v: &redis::Value| match v {
        redis::Value::BulkString(b) => Some(String::from_utf8_lossy(b).into_owned()),
        redis::Value::SimpleString(s) => Some(s.clone()),
        redis::Value::Int(i) => Some(i.to_string()),
        redis::Value::Double(d) => Some(d.to_string()),
        redis::Value::Nil => Some("nil".to_owned()),
        _ => None,
    };
    match v {
        redis::Value::Array(items) if items.len() % 2 == 0 => {
            for pair in items.chunks(2) {
                let key = scalar(&pair[0]).unwrap_or_else(|| format!("{:?}", pair[0]));
                match scalar(&pair[1]) {
                    Some(s) => out.push_str(&format!("{pad}{key}: {s}\n")),
                    None => {
                        out.push_str(&format!("{pad}{key}:\n"));
                        render(&pair[1], depth + 1, out);
                    }
                }
            }
        }
        other => out.push_str(&format!(
            "{pad}{}\n",
            scalar(other).unwrap_or_else(|| format!("{other:?}"))
        )),
    }
}

/// `name` on the PATH, its links followed.
fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
        .and_then(|p| p.canonicalize().ok())
}

fn jcmd(pid: u32, args: &[&str], expect: &str) -> Result<String, String> {
    let out = Command::new("jcmd")
        .arg(pid.to_string())
        .args(args)
        .output()
        .map_err(|e| format!("refused: no jcmd here ({e})"))?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() || !text.contains(expect) {
        return Err(format!(
            "refused: jcmd {}: {}",
            args.first().copied().unwrap_or_default(),
            text.trim().replace('\n', " ")
        ));
    }
    Ok(text)
}

/// Nanoseconds since the epoch of an ISO 8601 instant, as JFR prints one
/// (`2026-10-23T23:06:31.607170865Z`, or an offset in place of `Z`).
fn instant_ns(s: &str) -> Option<i128> {
    let (date, rest) = s.split_once('T')?;
    let mut d = date.split('-').map(str::parse::<i64>);
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let (time, offset_s) = match rest.strip_suffix('Z') {
        Some(t) => (t, 0),
        None => {
            let at = rest.rfind(['+', '-'])?;
            let (t, off) = rest.split_at(at);
            let sign = if off.starts_with('-') { -1 } else { 1 };
            let (h, m) = off.get(1..)?.split_once(':')?;
            (
                t,
                sign * (h.parse::<i64>().ok()? * 3600 + m.parse::<i64>().ok()? * 60),
            )
        }
    };
    let (hms, fraction) = time.split_once('.').unwrap_or((time, ""));
    let mut t = hms.split(':').map(str::parse::<i64>);
    let (h, mi, sec) = (t.next()?.ok()?, t.next()?.ok()?, t.next()?.ok()?);
    let nanos: i64 = if fraction.is_empty() {
        0
    } else {
        format!("{fraction:0<9}").get(..9)?.parse().ok()?
    };
    // Days from 1970-01-01 to the civil date (Hinnant's days_from_civil).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + day - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    let seconds = days * 86_400 + h * 3600 + mi * 60 + sec - offset_s;
    Some(i128::from(seconds) * 1_000_000_000 + i128::from(nanos))
}

/// Nanoseconds of an ISO 8601 duration as JFR prints one (`PT0.015001259S`, `PT1M2.5S`).
fn duration_ns(s: &str) -> Option<u64> {
    let mut rest = s.strip_prefix("PT")?;
    let mut ns = 0u64;
    for (unit, scale) in [('H', 3_600_000_000_000u64), ('M', 60_000_000_000)] {
        if let Some((n, r)) = rest.split_once(unit) {
            ns += n.parse::<u64>().ok()? * scale;
            rest = r;
        }
    }
    if let Some(seconds) = rest.strip_suffix('S') {
        let (whole, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
        ns += whole.parse::<u64>().ok()? * 1_000_000_000;
        if !fraction.is_empty() {
            ns += format!("{fraction:0<9}").get(..9)?.parse::<u64>().ok()?;
        }
    }
    Some(ns)
}

/// What Kafka's JVM did in `[from, until)` (nanoseconds since the epoch), from its recording's
/// events as `jfr print --json` gives them.
struct Jvm {
    bytes: u64,
    collections: u64,
    pause_ns: u64,
    heap: Option<u64>,
}

fn jvm(json: &[u8], from: i128, until: i128) -> Result<Jvm, String> {
    let v: serde_json::Value =
        serde_json::from_slice(json).map_err(|e| format!("refused: jfr's JSON: {e}"))?;
    let events = v
        .pointer("/recording/events")
        .and_then(serde_json::Value::as_array)
        .ok_or("refused: jfr's JSON has no events")?;
    let mut threads: BTreeMap<i64, Vec<(i128, u64)>> = BTreeMap::new();
    let (mut collections, mut pause_ns, mut heap) = (0u64, 0u64, None::<(i128, u64)>);
    let mut first = i128::MAX;
    for e in events {
        let values = &e["values"];
        let Some(at) = values["startTime"].as_str().and_then(instant_ns) else {
            continue;
        };
        match e["type"].as_str() {
            Some("jdk.ThreadAllocationStatistics") => {
                let id = values
                    .pointer("/thread/javaThreadId")
                    .or_else(|| values.pointer("/thread/osThreadId"))
                    .and_then(serde_json::Value::as_i64);
                if let (Some(id), Some(allocated)) = (id, values["allocated"].as_u64()) {
                    first = first.min(at);
                    threads.entry(id).or_default().push((at, allocated));
                }
            }
            Some("jdk.GarbageCollection") if (from..until).contains(&at) => {
                collections += 1;
                pause_ns += values["sumOfPauses"]
                    .as_str()
                    .and_then(duration_ns)
                    .unwrap_or(0);
            }
            Some("jdk.GCHeapSummary")
                if values["when"].as_str() == Some("After GC") && at < until =>
            {
                if let Some(used) = values["heapUsed"].as_u64()
                    && heap.is_none_or(|(t, _)| t < at)
                {
                    heap = Some((at, used));
                }
            }
            _ => {}
        }
    }
    if first > from {
        return Err(
            "refused: the recording's first thread sample came after the window began".to_owned(),
        );
    }
    // A thread's allocated bytes at `t`: between its samples, the line through them; before its
    // first (a thread begun since), none; after its last, its last.
    let at = |samples: &[(i128, u64)], t: i128| -> f64 {
        match samples.iter().position(|&(s, _)| s > t) {
            Some(0) => 0.0,
            Some(i) => {
                let ((t0, v0), (t1, v1)) = (samples[i - 1], samples[i]);
                v0 as f64 + (v1 as f64 - v0 as f64) * (t - t0) as f64 / (t1 - t0).max(1) as f64
            }
            None => samples.last().map_or(0.0, |&(_, v)| v as f64),
        }
    };
    let bytes = threads
        .values_mut()
        .map(|samples| {
            samples.sort_unstable();
            (at(samples, until) - at(samples, from)).max(0.0)
        })
        .sum::<f64>();
    Ok(Jvm {
        bytes: bytes as u64,
        collections,
        pause_ns,
        heap: heap.map(|(_, used)| used),
    })
}

/// One run's profile of its broker.
pub struct Session<'a> {
    target: &'a Target,
    /// Kafka's recording's name, or why there's none.
    recording: Option<Result<String, String>>,
    /// NATS's allocation profile of the window, saved as it ends.
    window: Option<std::thread::JoinHandle<Result<PathBuf, String>>>,
    files: Vec<PathBuf>,
}

impl<'a> Session<'a> {
    /// Begins the run's profile of its broker: Kafka's recording starts here, seconds before the
    /// window, so starting it lands in none of what's measured.
    pub fn begin(target: &'a Target) -> Self {
        let recording = (target.system == System::Kafka).then(|| -> Result<String, String> {
            let pid = target
                .pid
                .ok_or("refused: no broker process (--broker-container)")?;
            let name = format!("quiver-{}", target.tag);
            jcmd(
                pid,
                &[
                    "JFR.start",
                    &format!("name={name}"),
                    "settings=profile",
                    "jdk.ThreadAllocationStatistics#period=100ms",
                ],
                "Started recording",
            )?;
            Ok(name)
        });
        Self {
            target,
            recording,
            window: None,
            files: Vec::new(),
        }
    }

    fn file(&self, name: &str) -> Option<PathBuf> {
        let dir = self.target.profiles.as_ref()?;
        std::fs::create_dir_all(dir).ok()?;
        Some(dir.join(format!("{}-{name}", self.target.tag)))
    }

    /// The memory charged to the broker's cgroup now.
    pub fn memory(&self) -> Option<u64> {
        std::fs::read_to_string(self.target.cgroup.as_ref()?.join("memory.current"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// The broker's account now. The first starts NATS's allocation profile, `span` long; the
    /// second saves what the broker's runtime keeps of the window.
    pub fn snap(&mut self, first: bool, span: Duration) -> Snap {
        let t = self.target;
        let own = match t.system {
            System::Nats => {
                if first {
                    let seconds = span.as_secs().max(1);
                    let file = self.file("nats-allocs.pb.gz");
                    self.window = Some(std::thread::spawn(move || {
                        alloc::take_role(alloc::Role::Profiler);
                        let body = get(
                            NATS_PROFILE,
                            &format!("/debug/pprof/allocs?seconds={seconds}"),
                            Duration::from_secs(seconds + 60),
                        )?;
                        let file = file.ok_or("no --profiles folder")?;
                        std::fs::write(&file, body).map_err(|e| e.to_string())?;
                        Ok(file)
                    }));
                }
                Own::Go(go())
            }
            System::Redis => Own::Jemalloc(jemalloc(&t.endpoint).map(|(j, stats)| {
                if !first {
                    let mut text = String::new();
                    if let Ok(v) = redis_connection(&t.endpoint).and_then(|mut c| {
                        redis::cmd("MEMORY")
                            .arg("STATS")
                            .query::<redis::Value>(&mut c)
                            .map_err(|e| e.to_string())
                    }) {
                        render(&v, 0, &mut text);
                    }
                    for (name, body) in [
                        ("redis-memory-stats.txt", text),
                        ("redis-malloc-stats.txt", stats),
                    ] {
                        if let Some(f) = self.file(name)
                            && std::fs::write(&f, body).is_ok()
                        {
                            self.files.push(f);
                        }
                    }
                }
                j
            })),
            _ => Own::None,
        };
        if t.system == System::Nats
            && !first
            && let Some(f) = self.file("nats-heap.pb.gz")
            && let Ok(body) = get(NATS_PROFILE, "/debug/pprof/heap", Duration::from_secs(10))
            && std::fs::write(&f, body).is_ok()
        {
            self.files.push(f);
        }
        let dir = t.cgroup.as_deref();
        Snap {
            at: Instant::now(),
            wall: wall(),
            cpu_us: dir.and_then(|d| value(d, "cpu.stat", "usage_usec")),
            faults: dir.and_then(|d| {
                Some((
                    value(d, "memory.stat", "pgfault")?,
                    value(d, "memory.stat", "pgmajfault")?,
                ))
            }),
            memory: self.memory(),
            io: dir.and_then(io_stat),
            own,
        }
    }

    /// Kafka's recording, if the run has one, stopped and read: what the JVM did between
    /// `before` and `after`.
    fn recording(&mut self, before: &Snap, after: &Snap) -> Option<Result<Jvm, String>> {
        Some(match self.recording.take()? {
            Ok(name) => self.stop(&name, before, after),
            Err(why) => Err(why),
        })
    }

    fn stop(&mut self, name: &str, before: &Snap, after: &Snap) -> Result<Jvm, String> {
        let t = self.target;
        let pid = t.pid.ok_or("refused: no broker process")?;
        let inside = format!("/tmp/{name}.jfr");
        jcmd(
            pid,
            &[
                "JFR.stop",
                &format!("name={name}"),
                &format!("filename={inside}"),
            ],
            &inside,
        )?;
        let host = PathBuf::from(format!("/proc/{pid}/root{inside}"));
        if let Some(f) = self.file("kafka.jfr")
            && std::fs::copy(&host, &f).is_ok()
        {
            self.files.push(f);
        }
        // The host's jfr beside its jcmd, or else the one in the broker's image.
        let print = match which("jcmd").and_then(|j| j.parent().map(|d| d.join("jfr"))) {
            Some(jfr) if jfr.is_file() => Command::new(jfr)
                .args(["print", "--json", "--events", JFR_EVENTS])
                .arg(&host)
                .output(),
            _ => Command::new("docker")
                .args(["exec", &t.container, "jfr", "print", "--json", "--events"])
                .args([JFR_EVENTS, &inside])
                .output(),
        };
        let _ = std::fs::remove_file(&host);
        let out = print.map_err(|e| format!("refused: jfr: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "refused: jfr print: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        jvm(&out.stdout, before.wall, after.wall)
    }

    /// The broker's figures between `before` and `after`, its memory's peak sampled between them.
    pub fn end(mut self, before: &Snap, after: &Snap, peak: Option<u64>) -> BrokerFigures {
        let jvm = self.recording(before, after);
        if let Some(window) = self.window.take() {
            match window.join() {
                Ok(Ok(file)) => self.files.push(file),
                Ok(Err(why)) => println!("- nats: no allocation profile of the window: {why}"),
                Err(_) => println!("- nats: the allocation profile's thread failed"),
            }
        }
        let cgroup = |why: &str| {
            if self.target.cgroup.is_some() {
                Figure::Refused(format!("refused: the cgroup's {why} didn't read"))
            } else {
                Figure::Uncounted("n/a: no broker container (--broker-container)".to_owned())
            }
        };
        let less = |a: Option<u64>, b: Option<u64>, why: &str| match (a, b) {
            (Some(a), Some(b)) => Figure::Is(a.saturating_sub(b) as f64),
            _ => cgroup(why),
        };
        let faults = |pick: fn((u64, u64)) -> u64| {
            less(
                after.faults.map(pick),
                before.faults.map(pick),
                "memory.stat",
            )
        };
        let io = |i: usize| less(after.io.map(|v| v[i]), before.io.map(|v| v[i]), "io.stat");
        let uncounted = |why: &str| Figure::Uncounted(format!("n/a: {why}"));
        let mut f = BrokerFigures {
            cpu_ns: match (after.cpu_us, before.cpu_us) {
                (Some(a), Some(b)) => Figure::Is(a.saturating_sub(b) as f64 * 1000.0),
                _ => cgroup("cpu.stat"),
            },
            minor_faults: faults(|(all, major)| all.saturating_sub(major)),
            major_faults: faults(|(_, major)| major),
            memory_peak: peak.map_or_else(|| cgroup("memory.current"), |p| Figure::Is(p as f64)),
            memory: after
                .memory
                .map_or_else(|| cgroup("memory.current"), |m| Figure::Is(m as f64)),
            storage_read: io(0),
            storage_written: io(1),
            storage_reads: io(2),
            storage_writes: io(3),
            allocations: uncounted("its runtime counts none"),
            reallocations: uncounted("its runtime counts none"),
            frees: uncounted("its runtime counts none"),
            bytes: uncounted("its runtime counts none"),
            collections: uncounted("its runtime counts none"),
            pause_ns: uncounted("its runtime counts none"),
            heap: uncounted("its runtime says none"),
            files: Vec::new(),
        };
        match (&before.own, &after.own) {
            (Own::Go(b), Own::Go(a)) => match (b, a) {
                (Ok(b), Ok(a)) => {
                    let d = |a: u64, b: u64| Figure::Is(a.saturating_sub(b) as f64);
                    f.allocations = d(a.mallocs, b.mallocs);
                    f.frees = d(a.frees, b.frees);
                    f.bytes = d(a.total_alloc, b.total_alloc);
                    f.collections = d(a.num_gc, b.num_gc);
                    f.pause_ns = a.pauses_since(b);
                    f.heap = Figure::Is(a.heap_inuse as f64);
                    f.reallocations = uncounted("Go grows a block by allocating another");
                }
                (Err(why), _) | (_, Err(why)) => {
                    for x in [
                        &mut f.allocations,
                        &mut f.frees,
                        &mut f.bytes,
                        &mut f.collections,
                        &mut f.pause_ns,
                        &mut f.heap,
                    ] {
                        *x = Figure::refused_or_not(why);
                    }
                }
            },
            (Own::Jemalloc(b), Own::Jemalloc(a)) => {
                let nothing = uncounted("jemalloc counts requests alone");
                f.reallocations = nothing.clone();
                f.frees = nothing.clone();
                f.bytes = nothing;
                f.collections = uncounted("Redis frees as it goes: no collector");
                f.pause_ns = uncounted("Redis frees as it goes: no collector");
                match (b, a) {
                    (Ok(b), Ok(a)) => {
                        f.allocations = Figure::Is(a.requests.saturating_sub(b.requests) as f64);
                        f.heap = Figure::Is(a.used as f64);
                    }
                    (Err(why), _) | (_, Err(why)) => {
                        f.allocations = Figure::refused_or_not(why);
                        f.heap = Figure::refused_or_not(why);
                    }
                }
            }
            _ => {}
        }
        if let Some(jvm) = jvm {
            f.allocations = uncounted(
                "the JVM counts bytes, not objects (its allocation samples are in the recording)",
            );
            f.reallocations = uncounted("the JVM counts bytes, not objects");
            f.frees = uncounted("a collector frees, uncounted");
            match jvm {
                Ok(j) => {
                    f.bytes = Figure::Is(j.bytes as f64);
                    f.collections = Figure::Is(j.collections as f64);
                    f.pause_ns = Figure::Is(j.pause_ns as f64);
                    f.heap = j.heap.map_or_else(
                        || uncounted("no collection in the run"),
                        |h| Figure::Is(h as f64),
                    );
                }
                Err(why) => {
                    for x in [
                        &mut f.bytes,
                        &mut f.collections,
                        &mut f.pause_ns,
                        &mut f.heap,
                    ] {
                        *x = Figure::refused_or_not(&why);
                    }
                }
            }
        }
        f.files = self.files;
        f
    }
}
