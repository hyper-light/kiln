//! A run's profile: what its threads and its broker spent over the measured window, in all, per
//! message and per delivery (bench/README.md, "The profile"). The profiler is a thread of its own,
//! started before any of a run's: it takes its readings at the window's edges and samples the
//! memory held between them, and its own work is left out of every figure.
//!
//! - Allocations (`alloc.rs`): every thread's allocations, reallocations, frees and bytes, from
//!   slots each thread counts into beside the publishes and deliveries it made; a profile's
//!   messages and deliveries are what those slots counted between the same two readings.
//! - The kernel's account (`os.rs`): the process's CPU, page faults and context switches less the
//!   profiler's; the publisher's and the subscribers' threads' own, with their time waiting for a
//!   CPU; resident memory at its peak in the window; the process's I/O.
//! - The CPU's counters on the publisher's and the subscribers' threads (`perf.rs`).
//! - The node's accounted memory at its peak in the window, and per message it held (Quiver's).
//! - The broker's, read a quarter second outside the window (`broker.rs`).
//!
//! A figure the machine won't give says why: never a zero in its place.

pub mod alloc;
#[cfg(target_os = "linux")]
pub mod broker;
mod os;
mod perf;

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

pub use alloc::{Role, delivered, published};

/// A figure: what was counted, or why it wasn't.
#[derive(Clone, Debug)]
pub enum Figure {
    Is(f64),
    /// Refused by the kernel or the system; the words say why ("refused: …").
    Refused(String),
    /// Nothing here counts it; the words say why ("n/a: …").
    Uncounted(String),
}

impl Figure {
    /// A reason as it came: one saying "n/a" is uncounted, any other refused.
    pub fn refused_or_not(why: &str) -> Self {
        if why.starts_with("n/a") {
            Self::Uncounted(why.to_owned())
        } else {
            Self::Refused(why.to_owned())
        }
    }

    fn count(v: u64) -> Self {
        Self::Is(v as f64)
    }

    /// As the runs file holds it: the number, or the reason.
    fn field(&self) -> String {
        match self {
            Self::Is(v) => format!("{v}"),
            Self::Refused(why) | Self::Uncounted(why) => why.replace(['\t', '\n'], " "),
        }
    }

    /// As the record shows it: the number (`per` dividing it), or the reason, whole in a total's
    /// cell and its first word in a rate's.
    fn cell(&self, per: Option<u64>, total: bool) -> String {
        match (self, per) {
            (Self::Is(v), None) => show(*v),
            (Self::Is(_), Some(0)) => "-".to_owned(),
            (Self::Is(v), Some(n)) => show(*v / n as f64),
            (Self::Refused(why) | Self::Uncounted(why), _) if total => why.replace('|', "/"),
            (Self::Refused(_), _) => "refused".to_owned(),
            (Self::Uncounted(_), _) => "n/a".to_owned(),
        }
    }
}

/// A number to four significant figures, with k, M, G or T past ten thousand.
pub fn show(v: f64) -> String {
    if v == 0.0 {
        return "0".to_owned();
    }
    let (v, unit) = [(1e12, "T"), (1e9, "G"), (1e6, "M"), (1e4, "k")]
        .iter()
        .find(|(scale, _)| v.abs() >= *scale)
        .map_or((v, ""), |&(scale, unit)| {
            (v / if unit == "k" { 1e3 } else { scale }, unit)
        });
    let a = v.abs();
    let digits = if a >= 1000.0 {
        0
    } else if a >= 100.0 {
        1
    } else if a >= 10.0 {
        2
    } else if a >= 1.0 {
        3
    } else {
        ((-a.log10()).floor() as usize + 3).min(9)
    };
    format!("{v:.digits$}{unit}")
}

/// A count over a span, or an amount held at a moment.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Count,
    Held,
}

/// One figure of a profile, for each part of the run that has it.
struct Row {
    key: &'static str,
    label: &'static str,
    kind: Kind,
    process: Option<Figure>,
    publisher: Option<Figure>,
    subscribers: Option<Figure>,
    broker: Option<Figure>,
}

/// A profile's span: how long, and the messages published and deliveries made in it.
#[derive(Clone, Copy, Default, Debug)]
pub struct Span {
    pub seconds: f64,
    pub messages: u64,
    pub deliveries: u64,
}

/// The broker's figures over its span (`broker.rs`).
pub struct BrokerFigures {
    pub cpu_ns: Figure,
    pub minor_faults: Figure,
    pub major_faults: Figure,
    pub memory_peak: Figure,
    pub memory: Figure,
    pub storage_read: Figure,
    pub storage_written: Figure,
    pub storage_reads: Figure,
    pub storage_writes: Figure,
    pub allocations: Figure,
    pub reallocations: Figure,
    pub frees: Figure,
    pub bytes: Figure,
    pub collections: Figure,
    pub pause_ns: Figure,
    pub heap: Figure,
    pub files: Vec<PathBuf>,
}

/// No broker off Linux, where the comparison doesn't run (bench/compare.sh).
#[cfg(not(target_os = "linux"))]
pub mod broker {
    use std::time::{Duration, Instant};

    use super::BrokerFigures;

    pub enum Target {}

    pub struct Session<'a>(&'a Target);

    pub struct Snap {
        pub at: Instant,
    }

    impl<'a> Session<'a> {
        pub fn begin(target: &'a Target) -> Self {
            match *target {}
        }

        pub fn memory(&self) -> Option<u64> {
            match *self.0 {}
        }

        pub fn snap(&mut self, _: bool, _: Duration) -> Snap {
            match *self.0 {}
        }

        pub fn end(self, _: &Snap, _: &Snap, _: Option<u64>) -> BrokerFigures {
            match *self.0 {}
        }
    }
}

/// The parts of a run a figure may be of, as the runs file's fields begin.
const PARTS: [&str; 4] = ["process", "publisher", "subscribers", "broker"];

/// Every figure's key, in the order the runs file's header names them.
fn keys() -> Vec<&'static str> {
    let mut keys = vec![
        "cpu_ns",
        "cpu_wait_ns",
        "allocations",
        "reallocations",
        "moved",
        "frees",
        "alloc_bytes",
        "minor_faults",
        "major_faults",
        "voluntary_switches",
        "involuntary_switches",
    ];
    keys.extend(perf::EVENTS.iter().map(|e| e.key));
    keys.extend([
        "storage_read",
        "storage_written",
        "storage_reads",
        "storage_writes",
        "collections",
        "pause_ns",
        "memory_peak",
        "memory",
        "heap",
    ]);
    keys
}

/// Every field a profile may give the runs file, in its header's order.
pub fn field_names() -> Vec<String> {
    let mut names: Vec<String> = [
        "profile_seconds",
        "profile_messages",
        "profile_deliveries",
        "broker_profile_seconds",
        "broker_profile_messages",
        "broker_profile_deliveries",
        "node_peak_bytes",
        "node_peak_messages",
    ]
    .map(String::from)
    .to_vec();
    for key in keys() {
        names.extend(PARTS.iter().map(|part| format!("{part}_{key}")));
    }
    names
}

/// A run's profile.
pub struct Profile {
    /// The process's span: the window, as the profiler's readings fell.
    pub span: Span,
    pub broker_span: Option<Span>,
    rows: Vec<Row>,
    /// The node's accounted memory at its peak in the window, bytes, and the messages it held then.
    node: Option<(u64, u64)>,
    /// The broker's profiles saved beside the run.
    files: Vec<PathBuf>,
}

/// A thread that took a role, with its CPU counters.
struct Named {
    role: Role,
    tid: i32,
    counters: perf::Counters,
}

static NAMED: Mutex<Vec<Named>> = Mutex::new(Vec::new());

thread_local! {
    static NAMED_ALREADY: Cell<bool> = const { Cell::new(false) };
}

/// The calling thread is `role` to the run from here: its allocations count as that role's, and a
/// publisher's or a subscriber's thread opens its CPU counters and is read by its id.
pub fn take_role(role: Role) {
    alloc::take_role(role);
    if matches!(role, Role::Publisher | Role::Subscriber)
        && !NAMED_ALREADY.with(|n| n.replace(true))
    {
        let named = Named {
            role,
            tid: os::tid(),
            counters: perf::Counters::open(),
        };
        NAMED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(named);
    }
}

/// The process at one moment, as the profiler's thread reads it.
struct Snap {
    at: Instant,
    slots: Vec<(Role, alloc::Counts)>,
    usage: Option<os::Usage>,
    /// The profiler's own thread's, which comes off the process's.
    own_usage: Option<os::Usage>,
    io: Option<os::Io>,
    own_io: Option<os::Io>,
    threads: Vec<(Role, Option<os::Thread>, perf::Reading)>,
}

impl Snap {
    fn take() -> Self {
        let slots = alloc::read();
        let threads = NAMED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|n| (n.role, os::thread(n.tid), n.counters.read()))
            .collect();
        Self {
            at: Instant::now(),
            slots,
            usage: os::process(),
            own_usage: os::this_thread(),
            io: os::io("/proc/self/io"),
            own_io: os::io("/proc/thread-self/io"),
            threads,
        }
    }
}

/// The slots' counts between two readings: every thread's, the publisher's, the subscribers'.
fn slots(before: &[(Role, alloc::Counts)], after: &[(Role, alloc::Counts)]) -> [alloc::Counts; 3] {
    let mut sums = [alloc::Counts::default(); 3];
    for (i, (role, a)) in after.iter().enumerate() {
        if *role == Role::Profiler {
            continue;
        }
        let d = a.since(&before.get(i).map(|(_, c)| *c).unwrap_or_default());
        sums[0] = sums[0].plus(&d);
        match role {
            Role::Publisher => sums[1] = sums[1].plus(&d),
            Role::Subscriber => sums[2] = sums[2].plus(&d),
            Role::Other | Role::Profiler => {}
        }
    }
    sums
}

/// A role's threads between two readings: the kernel's figures summed, or why there are none; and
/// per CPU counter, the count summed, or why there's none. `None` where no thread took the role.
type Threads = Option<(Result<os::Thread, String>, Vec<Figure>)>;

fn threads(before: &Snap, after: &Snap, role: Role) -> Threads {
    let mine: Vec<usize> = (0..after.threads.len())
        .filter(|&i| after.threads[i].0 == role)
        .collect();
    if mine.is_empty() {
        return None;
    }
    let mut sum = os::Thread::default();
    let mut read = 0;
    for &i in &mine {
        if let Some(a) = after.threads[i].1 {
            let b = before.threads.get(i).and_then(|t| t.1).unwrap_or_default();
            sum = sum.plus(&a.since(&b));
            read += 1;
        }
    }
    let kernel = if read > 0 {
        Ok(sum)
    } else if cfg!(target_os = "linux") {
        Err("refused: /proc/self/task/ didn't read".to_owned())
    } else {
        Err("n/a: a thread's figures are Linux's".to_owned())
    };
    let counters = (0..perf::EVENTS.len())
        .map(|e| {
            let (mut total, mut ran) = (0.0, false);
            for &i in &mine {
                let a = match &after.threads[i].2[e] {
                    Ok(a) => a,
                    Err(why) => return Figure::refused_or_not(why),
                };
                let b = match before.threads.get(i).map(|t| &t.2[e]) {
                    Some(Ok(b)) => b.as_slice(),
                    _ => &[],
                };
                if let Some(v) = perf::between(b, a) {
                    total += v;
                    ran = true;
                }
            }
            if ran {
                Figure::Is(total)
            } else {
                Figure::Uncounted("n/a: never on the PMU (multiplexed out)".to_owned())
            }
        })
        .collect();
    Some((kernel, counters))
}

/// What the profiler samples through the window: the most each held.
#[derive(Default)]
struct Peaks {
    resident: u64,
    node: Option<(u64, u64)>,
    broker: Option<u64>,
}

/// What a run asks the profiler to watch.
pub struct Watch {
    base: Instant,
    from: u64,
    until: u64,
    node: Option<Arc<dyn Fn() -> (u64, u64) + Send + Sync>>,
    broker: Option<broker::Target>,
}

impl Watch {
    /// The window `[from, until)`, nanoseconds from `base`.
    pub fn new(base: Instant, from: u64, until: u64) -> Self {
        Self {
            base,
            from,
            until,
            node: None,
            broker: None,
        }
    }

    /// The node's accounted memory and the messages it holds, read as the window runs.
    pub fn node(mut self, read: Arc<dyn Fn() -> (u64, u64) + Send + Sync>) -> Self {
        self.node = Some(read);
        self
    }

    /// The broker the run drives.
    #[cfg(target_os = "linux")]
    pub fn broker(mut self, target: broker::Target) -> Self {
        self.broker = Some(target);
        self
    }
}

struct Job {
    watch: Watch,
    done: mpsc::Sender<Profile>,
}

/// The profiler's thread.
pub struct Profiler {
    jobs: mpsc::Sender<Job>,
}

/// A profile under way.
pub struct Pending(mpsc::Receiver<Profile>);

impl Pending {
    /// The profile, once the profiler has read past the window.
    pub fn profile(self) -> Option<Profile> {
        self.0.recv().ok()
    }
}

impl Profiler {
    /// Starts the profiler's thread: before any of a run's, so their CPU counters and slots are
    /// theirs alone.
    pub fn start() -> Self {
        let (jobs, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("profiler".to_owned())
            .spawn(move || {
                take_role(Role::Profiler);
                let counting = alloc::counting();
                for job in rx {
                    let _ = job.done.send(profile(&job.watch, counting));
                }
            })
            .expect("the profiler's thread");
        Self { jobs }
    }

    /// Asks for the profile of a run's window.
    pub fn watch(&self, watch: Watch) -> Pending {
        let (done, rx) = mpsc::channel();
        let _ = self.jobs.send(Job { watch, done });
        Pending(rx)
    }
}

fn sleep_until(t: Instant) {
    let now = Instant::now();
    if t > now {
        std::thread::sleep(t - now);
    }
}

/// How far outside the window the broker's readings fall.
const EDGE: Duration = Duration::from_millis(250);
/// How often the profiler samples what's held.
const SAMPLE: Duration = Duration::from_millis(50);

fn profile(w: &Watch, counting: bool) -> Profile {
    let at = |ns: u64| w.base + Duration::from_nanos(ns);
    let (from, until) = (at(w.from), at(w.until));
    let early = from.checked_sub(EDGE).unwrap_or(w.base).max(w.base);
    let late = until + EDGE;
    let mut session = w.broker.as_ref().map(broker::Session::begin);
    if session.is_some() {
        sleep_until(early);
    }
    let b0 = session
        .as_mut()
        .map(|s| (s.snap(true, late - early), alloc::read()));
    sleep_until(from);
    let reset = os::reset_peak();
    let p0 = Snap::take();
    let mut peaks = Peaks::default();
    loop {
        if let Some((resident, _)) = os::resident() {
            peaks.resident = peaks.resident.max(resident);
        }
        if let Some(read) = &w.node {
            let (bytes, held) = read();
            if peaks.node.is_none_or(|(b, _)| bytes > b) {
                peaks.node = Some((bytes, held));
            }
        }
        if let Some(m) = session.as_ref().and_then(broker::Session::memory) {
            peaks.broker = Some(peaks.broker.map_or(m, |p| p.max(m)));
        }
        let now = Instant::now();
        if now >= until {
            break;
        }
        std::thread::sleep((until - now).min(SAMPLE));
    }
    let p1 = Snap::take();
    let resident = os::resident();
    if session.is_some() {
        sleep_until(late);
    }
    let b1 = session
        .as_mut()
        .map(|s| (s.snap(false, late - early), alloc::read()));
    let broker = match (session, b0, b1) {
        (Some(s), Some((b0, s0)), Some((b1, s1))) => {
            let [all, _, _] = slots(&s0, &s1);
            let span = Span {
                seconds: (b1.at - b0.at).as_secs_f64(),
                messages: all.published,
                deliveries: all.deliveries,
            };
            Some((span, s.end(&b0, &b1, peaks.broker)))
        }
        _ => None,
    };
    let peak = match resident {
        Some((_, hwm)) if reset => Some(hwm),
        Some(_) => Some(peaks.resident),
        None => None,
    };
    rows(
        &p0,
        &p1,
        counting,
        peak,
        resident.map(|(rss, _)| rss),
        peaks.node,
        broker,
    )
}

/// A figure's key, its label, and how to read it from what was counted.
type Pick<T> = (&'static str, &'static str, fn(&T) -> u64);

fn rows(
    p0: &Snap,
    p1: &Snap,
    counting: bool,
    resident_peak: Option<u64>,
    resident: Option<u64>,
    node: Option<(u64, u64)>,
    broker: Option<(Span, BrokerFigures)>,
) -> Profile {
    let [all, publisher, subscribers] = slots(&p0.slots, &p1.slots);
    let span = Span {
        seconds: (p1.at - p0.at).as_secs_f64(),
        messages: all.published,
        deliveries: all.deliveries,
    };
    let unix = |what: &str| {
        if cfg!(unix) {
            Figure::Refused(format!("refused: {what} didn't read"))
        } else {
            Figure::Uncounted(format!("n/a: {what} is Unix's"))
        }
    };
    let usage = match (p0.usage, p1.usage) {
        (Some(b), Some(a)) => {
            let own = match (p0.own_usage, p1.own_usage) {
                (Some(b), Some(a)) => a.since(&b),
                _ => os::Usage::default(),
            };
            Ok(a.since(&b).since(&own))
        }
        _ => Err(unix("getrusage")),
    };
    let io = match (p0.io, p1.io) {
        (Some(b), Some(a)) => {
            let own = match (p0.own_io, p1.own_io) {
                (Some(b), Some(a)) => a.since(&b),
                _ => os::Io::default(),
            };
            Ok(a.since(&b).since(&own))
        }
        _ => Err(if cfg!(target_os = "linux") {
            Figure::Refused("refused: /proc/self/io didn't read".to_owned())
        } else {
            Figure::Uncounted("n/a: /proc/self/io is Linux's".to_owned())
        }),
    };
    let publisher_threads = threads(p0, p1, Role::Publisher);
    let subscriber_threads = threads(p0, p1, Role::Subscriber);
    let by = |u: &Result<os::Usage, Figure>, pick: fn(&os::Usage) -> u64| {
        Some(
            u.as_ref()
                .map_or_else(Clone::clone, |u| Figure::count(pick(u))),
        )
    };
    let thread = |t: &Threads, pick: fn(&os::Thread) -> u64| {
        t.as_ref().map(|(k, _)| {
            k.as_ref().map_or_else(
                |why| Figure::refused_or_not(why),
                |k| Figure::count(pick(k)),
            )
        })
    };
    let allocations = |c: &alloc::Counts, pick: fn(&alloc::Counts) -> u64| {
        Some(if counting {
            Figure::count(pick(c))
        } else {
            Figure::Uncounted("n/a: the counting allocator isn't installed".to_owned())
        })
    };
    let io_by = |pick: fn(&os::Io) -> u64| {
        Some(
            io.as_ref()
                .map_or_else(Clone::clone, |io| Figure::count(pick(io))),
        )
    };
    let b = broker.as_ref().map(|(_, f)| f);
    let held = |v: Option<u64>, what: &str| {
        Some(v.map_or_else(
            || Figure::Refused(format!("refused: {what} didn't read")),
            Figure::count,
        ))
    };
    let mut rows = vec![
        Row {
            key: "cpu_ns",
            label: "CPU, ns",
            kind: Kind::Count,
            process: by(&usage, |u| u.cpu_ns),
            publisher: thread(&publisher_threads, |t| t.cpu_ns),
            subscribers: thread(&subscriber_threads, |t| t.cpu_ns),
            broker: b.map(|b| b.cpu_ns.clone()),
        },
        Row {
            key: "cpu_wait_ns",
            label: "waiting for a CPU, ns",
            kind: Kind::Count,
            process: None,
            publisher: thread(&publisher_threads, |t| t.wait_ns),
            subscribers: thread(&subscriber_threads, |t| t.wait_ns),
            broker: None,
        },
    ];
    let alloc_rows: [(Pick<alloc::Counts>, Option<&Figure>); 5] = [
        (
            ("allocations", "allocations", |c| c.allocations),
            b.map(|b| &b.allocations),
        ),
        (
            ("reallocations", "reallocations", |c| c.reallocations),
            b.map(|b| &b.reallocations),
        ),
        (
            ("moved", "reallocations that moved their block", |c| c.moved),
            None,
        ),
        (("frees", "frees", |c| c.frees), b.map(|b| &b.frees)),
        (
            ("alloc_bytes", "bytes allocated", |c| c.bytes),
            b.map(|b| &b.bytes),
        ),
    ];
    for ((key, label, pick), brokers) in alloc_rows {
        rows.push(Row {
            key,
            label,
            kind: Kind::Count,
            process: allocations(&all, pick),
            publisher: allocations(&publisher, pick),
            subscribers: allocations(&subscribers, pick),
            broker: brokers.cloned(),
        });
    }
    rows.extend([
        Row {
            key: "minor_faults",
            label: "minor page faults",
            kind: Kind::Count,
            process: by(&usage, |u| u.minor_faults),
            publisher: thread(&publisher_threads, |t| t.minor_faults),
            subscribers: thread(&subscriber_threads, |t| t.minor_faults),
            broker: b.map(|b| b.minor_faults.clone()),
        },
        Row {
            key: "major_faults",
            label: "major page faults",
            kind: Kind::Count,
            process: by(&usage, |u| u.major_faults),
            publisher: thread(&publisher_threads, |t| t.major_faults),
            subscribers: thread(&subscriber_threads, |t| t.major_faults),
            broker: b.map(|b| b.major_faults.clone()),
        },
        Row {
            key: "voluntary_switches",
            label: "voluntary context switches",
            kind: Kind::Count,
            process: by(&usage, |u| u.voluntary),
            publisher: thread(&publisher_threads, |t| t.voluntary),
            subscribers: thread(&subscriber_threads, |t| t.voluntary),
            broker: None,
        },
        Row {
            key: "involuntary_switches",
            label: "involuntary context switches",
            kind: Kind::Count,
            process: by(&usage, |u| u.involuntary),
            publisher: thread(&publisher_threads, |t| t.involuntary),
            subscribers: thread(&subscriber_threads, |t| t.involuntary),
            broker: None,
        },
    ]);
    for (e, event) in perf::EVENTS.iter().enumerate() {
        let counter = |t: &Threads| t.as_ref().map(|(_, c)| c[e].clone());
        rows.push(Row {
            key: event.key,
            label: event.label,
            kind: Kind::Count,
            process: None,
            publisher: counter(&publisher_threads),
            subscribers: counter(&subscriber_threads),
            broker: None,
        });
    }
    rows.extend([
        Row {
            key: "storage_read",
            label: "bytes read from storage",
            kind: Kind::Count,
            process: io_by(|io| io.storage_read),
            publisher: None,
            subscribers: None,
            broker: b.map(|b| b.storage_read.clone()),
        },
        Row {
            key: "storage_written",
            label: "bytes written to storage",
            kind: Kind::Count,
            process: io_by(|io| io.storage_written),
            publisher: None,
            subscribers: None,
            broker: b.map(|b| b.storage_written.clone()),
        },
        Row {
            key: "storage_reads",
            label: "storage reads",
            kind: Kind::Count,
            process: None,
            publisher: None,
            subscribers: None,
            broker: b.map(|b| b.storage_reads.clone()),
        },
        Row {
            key: "storage_writes",
            label: "storage writes",
            kind: Kind::Count,
            process: None,
            publisher: None,
            subscribers: None,
            broker: b.map(|b| b.storage_writes.clone()),
        },
        Row {
            key: "collections",
            label: "garbage collections",
            kind: Kind::Count,
            process: None,
            publisher: None,
            subscribers: None,
            broker: b.map(|b| b.collections.clone()),
        },
        Row {
            key: "pause_ns",
            label: "collectors' pauses, ns",
            kind: Kind::Count,
            process: None,
            publisher: None,
            subscribers: None,
            broker: b.map(|b| b.pause_ns.clone()),
        },
        Row {
            key: "memory_peak",
            label: "memory at its peak in the window, bytes (resident; the broker's cgroup's)",
            kind: Kind::Held,
            process: held(resident_peak, "/proc/self/status"),
            publisher: None,
            subscribers: None,
            broker: b.map(|b| b.memory_peak.clone()),
        },
        Row {
            key: "memory",
            label: "memory at the window's end, bytes (resident; the broker's cgroup's)",
            kind: Kind::Held,
            process: held(resident, "/proc/self/status"),
            publisher: None,
            subscribers: None,
            broker: b.map(|b| b.memory.clone()),
        },
        Row {
            key: "heap",
            label: "heap held at the end, bytes (the broker's runtime's)",
            kind: Kind::Held,
            process: None,
            publisher: None,
            subscribers: None,
            broker: b.map(|b| b.heap.clone()),
        },
    ]);
    let (broker_span, files) = broker.map_or((None, Vec::new()), |(span, f)| (Some(span), f.files));
    Profile {
        span,
        broker_span,
        rows,
        node,
        files,
    }
}

impl Profile {
    /// The profile's table, as a run's record shows it.
    pub fn table(&self) -> String {
        let s = &self.span;
        let mut out = format!(
            "| profile: the window, {:.1} s, {} messages, {} deliveries | this process | per message | per delivery | the publisher's thread, per message | the subscribers' threads, per delivery | the broker | per message | per delivery |\n|---|---|---|---|---|---|---|---|---|\n",
            s.seconds, s.messages, s.deliveries
        );
        let b = self.broker_span.unwrap_or_default();
        let dash = || "-".to_owned();
        for r in &self.rows {
            let cell = |f: &Option<Figure>, per: Option<u64>, total: bool| {
                f.as_ref().map_or_else(dash, |f| f.cell(per, total))
            };
            let counted = r.kind == Kind::Count;
            let rate = |f: &Option<Figure>, n: u64| {
                if counted {
                    cell(f, Some(n), false)
                } else {
                    dash()
                }
            };
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                r.label,
                cell(&r.process, None, true),
                rate(&r.process, s.messages),
                rate(&r.process, s.deliveries),
                rate(&r.publisher, s.messages),
                rate(&r.subscribers, s.deliveries),
                cell(&r.broker, None, true),
                rate(&r.broker, b.messages),
                rate(&r.broker, b.deliveries),
            ));
        }
        out.push('\n');
        // A thread's figure has no total's cell for its reason: it goes here.
        for r in &self.rows {
            for (part, f) in [
                ("the publisher's thread", &r.publisher),
                ("the subscribers' threads", &r.subscribers),
            ] {
                if let Some(Figure::Refused(why) | Figure::Uncounted(why)) = f {
                    out.push_str(&format!("- {}, {part}: {why}\n", r.label));
                }
            }
        }
        if let Some(b) = self.broker_span {
            out.push_str(&format!(
                "- the broker's span: {:.2} s from a quarter second before the window to a quarter second after it, {} messages, {} deliveries\n",
                b.seconds, b.messages, b.deliveries
            ));
        }
        if let Some((bytes, held)) = self.node {
            out.push_str(&format!(
                "- the node's accounted memory at its peak: {} bytes, holding {held} messages: {} bytes a message\n",
                show(bytes as f64),
                if held > 0 { show(bytes as f64 / held as f64) } else { "-".to_owned() },
            ));
        }
        for f in &self.files {
            out.push_str(&format!("- saved: {}\n", f.display()));
        }
        out
    }

    /// The profile's fields in the runs file: each part's total of each figure, and the spans.
    pub fn fields(&self) -> std::collections::BTreeMap<String, String> {
        let known = keys();
        if let Some(r) = self.rows.iter().find(|r| !known.contains(&r.key)) {
            println!(
                "- profile: {} has no field in the runs file's header",
                r.key
            );
        }
        let mut out = vec![
            (
                "profile_seconds".to_owned(),
                format!("{}", self.span.seconds),
            ),
            (
                "profile_messages".to_owned(),
                self.span.messages.to_string(),
            ),
            (
                "profile_deliveries".to_owned(),
                self.span.deliveries.to_string(),
            ),
        ];
        if let Some(b) = self.broker_span {
            out.extend([
                (
                    "broker_profile_seconds".to_owned(),
                    format!("{}", b.seconds),
                ),
                ("broker_profile_messages".to_owned(), b.messages.to_string()),
                (
                    "broker_profile_deliveries".to_owned(),
                    b.deliveries.to_string(),
                ),
            ]);
        }
        for r in &self.rows {
            for (part, f) in [
                ("process", &r.process),
                ("publisher", &r.publisher),
                ("subscribers", &r.subscribers),
                ("broker", &r.broker),
            ] {
                if let Some(f) = f {
                    out.push((format!("{part}_{}", r.key), f.field()));
                }
            }
        }
        if let Some((bytes, held)) = self.node {
            out.push(("node_peak_bytes".to_owned(), bytes.to_string()));
            out.push(("node_peak_messages".to_owned(), held.to_string()));
        }
        out.into_iter().collect()
    }
}
