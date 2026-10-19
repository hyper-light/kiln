//! DogStatsD on the server and every runner (`docs/observe.md` §8.4): listeners the server keeps
//! (`<observe>/statsd.json`) and tells each runner its own, as it tells scrape targets. A listener
//! listens where it says, over UDP (Datadog's Agent's `dogstatsd_port`, 8125 on 127.0.0.1) and on
//! Unix over a datagram socket (its `dogstatsd_socket`), reads datagrams on a thread a socket, and
//! aggregates them as the Agent does (`kiln_obs::statsd`): a bucket's points written once it is
//! over, events and service checks as records as they come.
//!
//! A datagram has no sender to tell to wait: what observe has no room for is held and offered
//! again (a minute of buckets and [`HELD_RECORDS`] records at the most; past that the oldest go,
//! counted as dropped).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use kiln_obs::logs::record::Record;
use kiln_obs::statsd::{Aggregate, Aggregator, Settings};
use serde::{Deserialize, Serialize};

use super::ingest::Ack;
pub use super::scrape::{EVERY, SERVER};

/// Listeners kept at the most, and the addresses one listens on.
pub const LISTENERS: usize = 16;
const ADDRESSES: usize = 4;
/// A datagram's room: the largest UDP payload (65,507 bytes over IPv4); a Unix datagram socket's
/// are as large as a client makes them, this at the most (longer ones are cut by the system).
const DATAGRAM: usize = 64 << 10;
/// The receive buffer each socket asks for. Linux charges a datagram its buffer's true size (some
/// 768 bytes for a small one), so 4 MiB holds some 5,000: 50 ms of 100,000 datagrams a second,
/// what a busy host's DogStatsD sees, through a thread's scheduling delay on a loaded machine. The
/// Agent leaves the system's (`dogstatsd_so_rcvbuf` 0: Linux's 208 KiB, 3 ms of that); the system
/// may grant less than asked (`net.core.rmem_max`).
const RCVBUF: usize = 4 << 20;
/// How often a listener's flusher looks: a bucket's points are written within this of its end,
/// and what observe refused is offered again.
const TICK: Duration = Duration::from_millis(500);
/// How long a socket's thread waits for a datagram before it looks whether to stop.
const WAIT: Duration = Duration::from_millis(200);
/// Flushes held while observe has no room (a minute of ten-second buckets), and records.
const HELD_FLUSHES: usize = 6;
pub const HELD_RECORDS: usize = 10_000;

fn on_default() -> String {
    SERVER.to_owned()
}

/// A listener: where it listens, what it adds, how its histograms are written, who listens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listener {
    /// `udp://127.0.0.1:8125` (`udp://0.0.0.0:8125` for other machines' too),
    /// `unix:///var/run/kiln/dsd.socket`.
    pub listen: Vec<String>,
    /// `key:value` tags on every metric, event and check (a message's own win on the same key).
    #[serde(default)]
    pub tags: Vec<String>,
    /// A histogram's aggregates: `max`, `median`, `avg`, `count` unless said; `min`, `sum`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub histogram_aggregates: Option<Vec<String>>,
    /// Its percentiles, 0.01 to 0.99 in hundredths: 0.95 unless said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub histogram_percentiles: Option<Vec<Percentile>>,
    /// `server`, a runner's name, or `*` (every machine, each its own).
    #[serde(default = "on_default")]
    pub on: String,
}

/// Where a listener listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Addr {
    Udp(SocketAddr),
    Unix(PathBuf),
}

pub fn address(s: &str) -> Result<Addr, String> {
    if let Some(a) = s.strip_prefix("udp://") {
        return a.parse::<SocketAddr>().map(Addr::Udp).map_err(|_| format!("a UDP address's host and port (an IP address; 0.0.0.0 for every one): {s}"));
    }
    if let Some(p) = s.strip_prefix("unix://") {
        if !p.starts_with('/') || p.len() > 100 || p.contains('\0') {
            return Err(format!("a Unix socket's path is absolute, 100 bytes at the most: {s}"));
        }
        return Ok(Addr::Unix(PathBuf::from(p)));
    }
    Err(format!("where to listen: udp://host:port or unix:///path, not {s:?}"))
}

impl Listener {
    pub fn check(&self) -> Result<(), String> {
        if self.listen.is_empty() || self.listen.len() > ADDRESSES {
            return Err(format!("a listener listens somewhere: udp://127.0.0.1:8125, unix:///var/run/kiln/dsd.socket ({ADDRESSES} at the most)"));
        }
        for l in &self.listen {
            address(l)?;
        }
        for t in &self.tags {
            if t.split_once(':').is_none_or(|(k, v)| k.is_empty() || v.is_empty()) {
                return Err(format!("a tag is key:value: {t}"));
            }
        }
        if let Some(a) = &self.histogram_aggregates {
            if a.is_empty() && self.histogram_percentiles.as_ref().is_none_or(Vec::is_empty) {
                return Err("a histogram is written as something: an aggregate or a percentile".into());
            }
            for x in a {
                Aggregate::parse(x).ok_or_else(|| format!("a histogram aggregate: max, median, avg, count, min or sum, not {x:?}"))?;
            }
        }
        if self.on.is_empty() || self.on.len() > 255 {
            return Err("on: `server`, a runner's name, or `*`".into());
        }
        Ok(())
    }

    /// Whether `machine` listens (`server` for the server).
    pub fn read_by(&self, machine: &str) -> bool {
        self.on == EVERY || self.on == machine
    }

    /// What its aggregator is told on `host`.
    fn settings(&self, host: &str) -> Settings {
        let d = Settings::default();
        Settings {
            host: host.to_owned(),
            tags: self.tags.iter().filter_map(|t| t.split_once(':')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect(),
            aggregates: self.histogram_aggregates.as_ref().map_or(d.aggregates, |a| a.iter().filter_map(|x| Aggregate::parse(x)).collect()),
            percentiles: self.histogram_percentiles.as_ref().map_or(d.percentiles, |p| p.iter().map(|x| x.0).collect()),
        }
    }
}

/// A percentile in hundredths (95: `.95percentile`, as the Agent names it), given as 0.95 or as the
/// Agent's configuration gives its `histogram_percentiles`, "0.95".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Percentile(pub u8);

impl Serialize for Percentile {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_f64(f64::from(self.0) / 100.0)
    }
}

impl<'de> Deserialize<'de> for Percentile {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Given {
            Number(f64),
            Text(String),
        }
        let p = match Given::deserialize(d)? {
            Given::Number(x) => x,
            Given::Text(t) => t.trim().parse::<f64>().map_err(|_| D::Error::custom(format!("a percentile: 0.01 to 0.99, not {t:?}")))?,
        };
        hundredths(p).map(Percentile).map_err(D::Error::custom)
    }
}

/// A percentile in hundredths: 0.01 to 0.99, two places.
fn hundredths(p: f64) -> Result<u8, String> {
    let h = (p * 100.0).round();
    if !(1.0..=99.0).contains(&h) || (p * 100.0 - h).abs() > 1e-9 {
        return Err(format!("a percentile: 0.01 to 0.99, in hundredths (0.95), not {p}"));
    }
    Ok(h as u8)
}

/// How a machine's listener goes, as its status says.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// The addresses it listens on (those that would not bind are in `error`), and the least
    /// receive buffer the system granted their sockets (bytes; the system's bound may be under what
    /// was asked: Linux's `net.core.rmem_max`).
    pub listening: Vec<String>,
    pub rcvbuf: u64,
    pub datagrams: u64,
    pub metrics: u64,
    pub events: u64,
    pub checks: u64,
    /// Messages that did not read, values that are not numbers.
    pub invalid: u64,
    /// Samples refused: a context past the bound, or a name or tags no series can have.
    pub refused: u64,
    /// Tags whose key came twice with another value (the first kept).
    pub conflicts: u64,
    pub contexts: u64,
    /// Points written, and points and records dropped once more than [`HELD_FLUSHES`] flushes or
    /// [`HELD_RECORDS`] records waited for room.
    pub points: u64,
    pub dropped: u64,
    /// Points and records waiting for room now.
    pub held: u64,
    /// Unix ms of its last datagram.
    pub last_ms: Option<i64>,
    pub error: Option<String>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// --------------------------------------------------------------------------------------- store

/// The server's listeners by name, kept in `<observe>/statsd.json`.
pub struct Listeners {
    path: PathBuf,
    all: Mutex<BTreeMap<String, Listener>>,
}

impl Listeners {
    pub fn open(dir: &Path) -> Self {
        let path = dir.join("statsd.json");
        let all = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { path, all: Mutex::new(all) }
    }

    pub fn list(&self) -> Vec<(String, Listener)> {
        lock(&self.all).iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    pub fn put(&self, name: &str, l: Listener) -> Result<(), String> {
        if name.is_empty() || name.len() > 64 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)) {
            return Err("a listener's name is 1 to 64 letters, digits, '-', '_' and '.'".into());
        }
        l.check()?;
        let mut all = lock(&self.all);
        if !all.contains_key(name) && all.len() >= LISTENERS {
            return Err(format!("{LISTENERS} listeners at the most"));
        }
        all.insert(name.to_owned(), l);
        self.save(&all).map_err(|e| format!("{e:#}"))
    }

    pub fn delete(&self, name: &str) -> anyhow::Result<bool> {
        let mut all = lock(&self.all);
        let had = all.remove(name).is_some();
        self.save(&all)?;
        Ok(had)
    }

    fn save(&self, all: &BTreeMap<String, Listener>) -> anyhow::Result<()> {
        crate::util::write_secret(&self.path, &serde_json::to_vec_pretty(all)?, 0o600)?;
        Ok(())
    }
}

// ------------------------------------------------------------------------------------ listening

/// A machine's listeners, each its own threads.
pub struct Statsd {
    machine: String,
    host: String,
    running: Mutex<HashMap<String, Running>>,
    stop: crate::rt::Shutdown,
}

struct Running {
    listener: Listener,
    stop: crate::rt::Shutdown,
    state: Arc<Mutex<State>>,
}

static STATSD: OnceLock<Arc<Statsd>> = OnceLock::new();

/// This process's listeners, started once: `machine` the name listeners give it (`server` for the
/// server), `host` the host its metrics are of (unless a message's `host:` tag says).
pub fn start(machine: &str, host: &str, stop: crate::rt::Shutdown) -> &'static Arc<Statsd> {
    STATSD.get_or_init(|| Arc::new(Statsd { machine: machine.to_owned(), host: host.to_owned(), running: Mutex::new(HashMap::new()), stop }))
}

pub fn get() -> Option<&'static Arc<Statsd>> {
    STATSD.get()
}

impl Statsd {
    /// Listens as `all` (every machine's: those of this one kept) says from now: one unchanged goes
    /// on as it was, a changed one starts again, one gone stops.
    pub fn set(&self, all: &[(String, Listener)]) {
        let mine: HashMap<&str, &Listener> = all.iter().filter(|(_, l)| l.read_by(&self.machine)).map(|(n, l)| (n.as_str(), l)).collect();
        let mut running = lock(&self.running);
        running.retain(|name, r| {
            let keep = mine.get(name.as_str()).is_some_and(|l| **l == r.listener);
            if !keep {
                r.stop.cancel();
            }
            keep
        });
        for (name, l) in mine {
            if running.contains_key(name) {
                continue;
            }
            let r = Running { listener: l.clone(), stop: crate::rt::Shutdown::new(), state: Arc::new(Mutex::new(State::default())) };
            listen(name, l, &self.host, &r.stop, &self.stop, &r.state);
            running.insert(name.to_owned(), r);
        }
    }

    /// How each of this machine's listeners goes.
    pub fn states(&self) -> Vec<(String, State)> {
        let mut out: Vec<(String, State)> = lock(&self.running).iter().map(|(n, r)| (n.clone(), lock(&r.state).clone())).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

/// What a listener's threads share.
struct Shared {
    agg: Mutex<Aggregator>,
    /// Records observe had no room for, oldest first.
    records: Mutex<VecDeque<Record>>,
    state: Arc<Mutex<State>>,
    /// Socket threads still reading.
    receivers: AtomicUsize,
}

/// A socket bound (a Unix one with its path and the file it made, removed when it is done only
/// if it is still that one).
enum Socket {
    Udp(std::net::UdpSocket),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixDatagram, PathBuf, (u64, u64)),
}

impl Socket {
    /// The receive buffer the system granted it.
    fn rcvbuf(&self) -> u64 {
        let got = match self {
            Self::Udp(u) => socket2::SockRef::from(u).recv_buffer_size(),
            #[cfg(unix)]
            Self::Unix(u, _, _) => socket2::SockRef::from(u).recv_buffer_size(),
        };
        got.map_or(0, |n| n as u64)
    }
}

fn bind(a: &Addr) -> std::io::Result<Socket> {
    match a {
        Addr::Udp(addr) => {
            let domain = if addr.is_ipv4() { socket2::Domain::IPV4 } else { socket2::Domain::IPV6 };
            let s = socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
            // Asked for, not insisted on: the system grants what its bound allows.
            let _ = s.set_recv_buffer_size(RCVBUF);
            s.bind(&(*addr).into())?;
            let u: std::net::UdpSocket = s.into();
            u.set_read_timeout(Some(WAIT))?;
            Ok(Socket::Udp(u))
        }
        #[cfg(unix)]
        Addr::Unix(path) => {
            use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
            // A socket left by a process that stopped is taken over. One a process still reads is
            // its (a Datadog Agent's; or a listener changed a moment ago, letting go): left alone,
            // and any other file too.
            if let Ok(meta) = std::fs::symlink_metadata(path) {
                if !meta.file_type().is_socket() {
                    return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "there is a file there that is not a socket"));
                }
                if std::os::unix::net::UnixDatagram::unbound().and_then(|probe| probe.connect(path)).is_ok() {
                    return Err(std::io::Error::new(std::io::ErrorKind::AddrInUse, "a process listens on that socket"));
                }
                std::fs::remove_file(path)?;
            }
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let s = std::os::unix::net::UnixDatagram::bind(path)?;
            let _ = socket2::SockRef::from(&s).set_recv_buffer_size(RCVBUF);
            // Anyone on the machine may send, as to the Agent's socket; only its owner may read.
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o722))?;
            s.set_read_timeout(Some(WAIT))?;
            let meta = std::fs::symlink_metadata(path)?;
            Ok(Socket::Unix(s, path.clone(), (meta.dev(), meta.ino())))
        }
        #[cfg(not(unix))]
        Addr::Unix(_) => Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "Unix datagram sockets are not on this system; listen over UDP")),
    }
}

/// `addresses` bound: a port another listener of this process is letting go of (one changed a
/// moment ago) is tried again for [`REBIND`].
fn bind_all(addresses: &[String], stop: &crate::rt::Shutdown) -> (Vec<(String, Socket)>, Vec<String>) {
    let (mut bound, mut errors) = (Vec::new(), Vec::new());
    for a in addresses {
        let addr = match address(a) {
            Ok(x) => x,
            Err(e) => {
                errors.push(e);
                continue;
            }
        };
        let began = std::time::Instant::now();
        loop {
            match bind(&addr) {
                Ok(s) => {
                    bound.push((a.clone(), s));
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && began.elapsed() < REBIND && !stop.is_cancelled() => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => {
                    errors.push(format!("{a}: {e}"));
                    break;
                }
            }
        }
    }
    (bound, errors)
}

/// How long a listener tries an address in use: a listener stopping lets go of its sockets within
/// [`WAIT`].
const REBIND: Duration = Duration::from_secs(2);

/// A listener started: a thread binds its sockets (off the caller's thread: an address a listener
/// changed a moment ago may still be held), starts a thread a socket to read them, and then writes
/// what they aggregate.
fn listen(name: &str, l: &Listener, host: &str, stop: &crate::rt::Shutdown, server: &crate::rt::Shutdown, state: &Arc<Mutex<State>>) {
    let shared = Arc::new(Shared { agg: Mutex::new(Aggregator::new(l.settings(host))), records: Mutex::new(VecDeque::new()), state: state.clone(), receivers: AtomicUsize::new(0) });
    let (addresses, name, stop, server) = (l.listen.clone(), name.to_owned(), stop.clone(), server.clone());
    let spawned = std::thread::Builder::new().name("kiln-statsd-flush".into()).spawn(move || {
        let (bound, mut errors) = bind_all(&addresses, &stop);
        let mut listening = Vec::new();
        let rcvbuf = bound.iter().map(|(_, s)| s.rcvbuf()).min().unwrap_or(0);
        for (a, sock) in bound {
            let (sh, stop2, server2) = (shared.clone(), stop.clone(), server.clone());
            shared.receivers.fetch_add(1, Ordering::AcqRel);
            match std::thread::Builder::new().name("kiln-statsd".into()).spawn(move || receive(&sock, &sh, &stop2, &server2)) {
                Ok(_) => listening.push(a),
                Err(e) => {
                    shared.receivers.fetch_sub(1, Ordering::AcqRel);
                    errors.push(format!("{a}: {e}"));
                }
            }
        }
        if !errors.is_empty() {
            tracing::warn!(listener = %name, error = %errors.join("; "), "a DogStatsD listener did not listen everywhere");
        }
        {
            let mut s = lock(&shared.state);
            s.listening = listening;
            s.rcvbuf = rcvbuf;
            s.error = (!errors.is_empty()).then(|| errors.join("; "));
        }
        flush_loop(&shared, &stop, &server);
    });
    if let Err(e) = spawned {
        lock(state).error = Some(format!("it did not start: {e}"));
    }
}

/// A socket's thread: each datagram aggregated as it comes, its records offered at once.
fn receive(sock: &Socket, shared: &Shared, stop: &crate::rt::Shutdown, server: &crate::rt::Shutdown) {
    let mut buf = vec![0u8; DATAGRAM];
    let mut records: Vec<Record> = Vec::new();
    while !(stop.is_cancelled() || server.is_cancelled()) {
        let got = match sock {
            Socket::Udp(u) => u.recv(&mut buf),
            #[cfg(unix)]
            Socket::Unix(u, _, _) => u.recv(&mut buf),
        };
        let n = match got {
            Ok(n) => n,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted) => continue,
            Err(e) => {
                lock(&shared.state).error = Some(format!("reading: {e}"));
                std::thread::sleep(WAIT);
                continue;
            }
        };
        let now = super::clock_ms();
        lock(&shared.agg).take(buf.get(..n).unwrap_or_default(), now, &mut records);
        if !records.is_empty() {
            offer_records(shared, std::mem::take(&mut records));
        }
    }
    #[cfg(unix)]
    if let Socket::Unix(_, path, (dev, ino)) = sock {
        use std::os::unix::fs::MetadataExt as _;
        // Its own file only: a listener that took the path over since keeps its.
        if std::fs::symlink_metadata(path).is_ok_and(|m| m.dev() == *dev && m.ino() == *ino) {
            let _ = std::fs::remove_file(path);
        }
    }
    shared.receivers.fetch_sub(1, Ordering::AcqRel);
}

/// Records offered: what observe refuses held, behind what is held already (the oldest dropped
/// past [`HELD_RECORDS`]).
fn offer_records(shared: &Shared, records: Vec<Record>) {
    let mut held = lock(&shared.records);
    if held.is_empty()
        && let Some(o) = super::api::get()
    {
        match o.offer_logs_acked(records, Ack::new()) {
            Ok(()) => return,
            Err(back) => held.extend(back),
        }
    } else {
        held.extend(records);
    }
    let over = held.len().saturating_sub(HELD_RECORDS);
    if over > 0 {
        held.drain(..over);
        lock(&shared.state).dropped += over as u64;
    }
}

/// The listener's flusher: each bucket written once it is over, what was refused offered again,
/// the listener's state kept. At the end (its sockets' threads gone), the bucket under way is
/// written too: what came is not lost.
fn flush_loop(shared: &Shared, stop: &crate::rt::Shutdown, server: &crate::rt::Shutdown) {
    let mut held: VecDeque<kiln_obs::otlp::metrics::Decoded> = VecDeque::new();
    let mut seen = 0u64;
    loop {
        let ending = stop.is_cancelled() || server.is_cancelled();
        if ending {
            let began = std::time::Instant::now();
            while shared.receivers.load(Ordering::Acquire) > 0 && began.elapsed() < REBIND {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let now = super::clock_ms();
        let mut dropped = 0u64;
        let (stats, out) = {
            let mut agg = lock(&shared.agg);
            agg.flush(if ending { now.saturating_add(kiln_obs::statsd::BUCKET_MS) } else { now });
            let out = match (agg.pending(), super::api::get()) {
                (true, Some(o)) => {
                    let mut d = o.decoding();
                    let n = agg.drain(&mut d.keys, &mut d.samples);
                    d.points = n as u64;
                    Some(d)
                }
                _ => None,
            };
            (agg.stats(), out)
        };
        if let Some(d) = out {
            held.push_back(d);
        }
        if let Some(o) = super::api::get() {
            while let Some(d) = held.pop_front() {
                if let Err(back) = o.offer_samples_or_back(d) {
                    held.push_front(*back);
                    break;
                }
            }
            while held.len() > HELD_FLUSHES || (ending && !held.is_empty()) {
                if let Some(d) = held.pop_front() {
                    dropped += d.samples.len() as u64;
                    o.give_decoding(d);
                }
            }
        }
        // Records refused before: offered again, oldest first.
        let waiting: Vec<Record> = lock(&shared.records).drain(..).collect();
        if !waiting.is_empty() {
            offer_records(shared, waiting);
        }
        {
            let held_records = lock(&shared.records).len() as u64;
            let mut s = lock(&shared.state);
            s.datagrams = stats.datagrams;
            s.metrics = stats.metrics;
            s.events = stats.events;
            s.checks = stats.checks;
            s.invalid = stats.invalid;
            s.refused = stats.refused;
            s.conflicts = stats.conflicts;
            s.contexts = stats.contexts;
            s.points = stats.points;
            s.dropped += dropped;
            s.held = held.iter().map(|d| d.samples.len() as u64).sum::<u64>() + held_records;
            if stats.datagrams != seen {
                seen = stats.datagrams;
                s.last_ms = Some(now);
            }
        }
        if ending {
            return;
        }
        std::thread::sleep(TICK);
    }
}
