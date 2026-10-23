//! A run's results: its row as the record prints it, its line in the runs file (`--out`), and the
//! comparison the `report` mode makes of those files.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;

use crate::System;
use crate::follow::{Follow, Schedule, Seen};

/// One run's results.
pub struct Run {
    pub system: System,
    /// Its subscribers' threads polled without waiting (`--busy`).
    pub busy: bool,
    pub rate: f64,
    pub subscribers: usize,
    /// The subscriptions it checked: one each, or one each waiter thread of the wake-up's floor.
    pub subscriptions: usize,
    pub workers: usize,
    pub published: u64,
    /// Messages the schedule still held when its time was over: the system held its publisher
    /// back so far that they were never offered, and count as undelivered.
    pub unoffered: u64,
    /// Publishes the system refused or failed: their messages count as undelivered.
    pub publish_errors: u64,
    pub seconds: f64,
    /// The measured window's length: deliveries of its messages over it are the rate delivered.
    pub window_s: u64,
    pub seen: Seen,
    /// The window's messages a subscriber never took, and of those, the ones no lag notice came before.
    pub undelivered: u64,
    pub silent: u64,
    pub lags: u64,
    pub doubled: u64,
    pub kept_up: usize,
    pub cpu_s: f64,
    /// The publisher's own thread's CPU: its open loop's pacing spins, which isn't the system's work.
    pub publisher_cpu_s: f64,
    /// The broker's CPU, as its container's cgroup counted it (`--broker-cgroup`).
    pub broker_cpu_s: Option<f64>,
    pub load_before: Option<f64>,
    pub load_after: Option<f64>,
}

/// What a run's subscribers missed of the window, counted into `seen`'s histogram past every
/// delivery.
pub struct Tally {
    pub undelivered: u64,
    pub silent: u64,
    pub lags: u64,
    pub doubled: u64,
    pub kept_up: usize,
}

pub fn tally(seen: &mut Seen, follows: &[Follow], schedule: &Schedule) -> Tally {
    let mut t = Tally {
        undelivered: 0,
        silent: 0,
        lags: 0,
        doubled: 0,
        kept_up: 0,
    };
    for f in follows {
        let m = f.missed(schedule.first, schedule.last);
        t.undelivered += m.undelivered;
        t.silent += m.silent;
        t.lags += f.lags;
        t.doubled += f.doubled;
        t.kept_up += usize::from(f.kept_up(schedule.offered()));
    }
    seen.latency.undelivered += t.undelivered;
    t
}

fn us(ns: Option<u64>) -> String {
    ns.map_or_else(
        || "undelivered".to_owned(),
        |ns| format!("{:.1}", ns as f64 / 1000.0),
    )
}

fn waits(busy: bool) -> &'static str {
    if busy { "busy" } else { "wait" }
}

pub fn header() -> &'static str {
    "| run | system | subscribers' threads | published | /s achieved | never offered | publish errors | deliveries | measured | undelivered | p50 µs | p90 µs | p99 µs | p99.9 µs | max µs | lag notices | lost silently | kept up | CPU s | publisher's CPU s | broker's CPU s | load before / after |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"
}

pub fn row(i: &str, r: &Run) -> String {
    let l = &r.seen.latency;
    let load = |l: Option<f64>| l.map_or_else(|| "-".to_owned(), |l| format!("{l:.2}"));
    format!(
        "| {i} | {} | {} × {} | {} | {:.0} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {}/{} | {:.1} | {:.1} | {} | {} / {} |",
        r.system.name(),
        r.workers,
        waits(r.busy),
        r.published,
        r.published as f64 / r.seconds,
        r.unoffered,
        r.publish_errors,
        r.seen.deliveries,
        r.seen.measured,
        r.undelivered,
        us(l.quantile(0.5)),
        us(l.quantile(0.9)),
        us(l.quantile(0.99)),
        us(l.quantile(0.999)),
        us(l.largest()),
        r.lags,
        r.silent,
        r.kept_up,
        r.subscriptions,
        r.cpu_s,
        r.publisher_cpu_s,
        r.broker_cpu_s
            .map_or_else(|| "-".to_owned(), |s| format!("{s:.1}")),
        load(r.load_before),
        load(r.load_after),
    )
}

const FIELDS: [&str; 17] = [
    "system",
    "busy",
    "rate",
    "subscribers",
    "workers",
    "published",
    "unoffered",
    "publish_errors",
    "measured",
    "undelivered",
    "silent",
    "kept_up",
    "p50_ns",
    "p99_ns",
    "p999_ns",
    "max_ns",
    "window_s",
];

/// Appends `r` to the runs file at `path`, its header first if the file is new.
pub fn record(path: &Path, r: &Run) {
    let new = std::fs::metadata(path).map_or(true, |m| m.len() == 0);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    if new {
        writeln!(file, "{}", FIELDS.join("\t")).unwrap();
    }
    let ns = |v: Option<u64>| v.map_or_else(|| "undelivered".to_owned(), |v| v.to_string());
    let l = &r.seen.latency;
    let fields = [
        r.system.name().to_owned(),
        waits(r.busy).to_owned(),
        format!("{}", r.rate),
        r.subscribers.to_string(),
        r.workers.to_string(),
        r.published.to_string(),
        r.unoffered.to_string(),
        r.publish_errors.to_string(),
        r.seen.measured.to_string(),
        r.undelivered.to_string(),
        r.silent.to_string(),
        r.kept_up.to_string(),
        ns(l.quantile(0.5)),
        ns(l.quantile(0.99)),
        ns(l.quantile(0.999)),
        ns(l.largest()),
        r.window_s.to_string(),
    ];
    writeln!(file, "{}", fields.join("\t")).unwrap();
}

/// One line of a runs file.
struct Line {
    fields: BTreeMap<&'static str, String>,
}

impl Line {
    fn get(&self, name: &str) -> &str {
        self.fields.get(name).map_or("", String::as_str)
    }

    /// A quantile in nanoseconds; `None` where a delivery owed never came.
    fn ns(&self, name: &str) -> Option<u64> {
        self.get(name).parse().ok()
    }
}

fn read(path: &Path) -> Vec<Line> {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let mut lines = text.lines();
    let names: Vec<&str> = lines.next().unwrap_or_default().split('\t').collect();
    lines
        .filter(|l| !l.is_empty())
        .map(|l| Line {
            fields: names
                .iter()
                .zip(l.split('\t'))
                .filter_map(|(n, v)| FIELDS.iter().find(|f| *f == n).map(|f| (*f, v.to_owned())))
                .collect(),
        })
        .collect()
}

/// A tail at least as good as another's: a quantile that came beats one that didn't.
fn key(ns: Option<u64>) -> u64 {
    ns.unwrap_or(u64::MAX)
}

fn cell(ns: Option<u64>) -> String {
    us(ns)
}

/// How many times the competitor's tail is Quiver's.
fn lead(quiver: Option<u64>, competitor: Option<u64>) -> (String, bool) {
    match (quiver, competitor) {
        (None, _) => ("Quiver's undelivered".to_owned(), false),
        (Some(_), None) => ("∞ (the competitor's undelivered)".to_owned(), true),
        (Some(q), Some(c)) => {
            let x = c as f64 / q.max(1) as f64;
            (format!("{x:.1}×"), x >= 10.0)
        }
    }
}

/// The comparison of the runs files at `paths`: for each load (rate, subscribers, how their
/// threads wait), each system's best run (the least p99, CLAUDE.md §7), and Quiver's lead over the
/// best competitor at p99 and p99.9, an order of magnitude being the bar.
pub fn compare(paths: &[std::path::PathBuf]) -> String {
    let lines: Vec<Line> = paths.iter().flat_map(|p| read(p)).collect();
    let mut loads: BTreeMap<(String, u64, u64), BTreeMap<String, &Line>> = BTreeMap::new();
    for l in &lines {
        let rate = l.get("rate").parse::<f64>().unwrap_or(0.0) as u64;
        let subscribers = l.get("subscribers").parse().unwrap_or(0);
        let systems = loads
            .entry((l.get("busy").to_owned(), rate, subscribers))
            .or_default();
        let best = systems.entry(l.get("system").to_owned()).or_insert(l);
        if key(l.ns("p99_ns")) < key(best.ns("p99_ns")) {
            *best = l;
        }
    }
    let mut out = String::from(
        "| subscribers' threads | rate /s | subscribers | the wake-up's floor, p99 / p99.9 µs | Quiver | NATS | Redis | Kafka | the best competitor | Quiver's lead, p99 / p99.9 | an order of magnitude |\n|---|---|---|---|---|---|---|---|---|---|---|\n",
    );
    let pair = |l: Option<&&Line>| {
        l.map_or_else(
            || "-".to_owned(),
            |l| format!("{} / {}", cell(l.ns("p99_ns")), cell(l.ns("p999_ns"))),
        )
    };
    for ((busy, rate, subscribers), systems) in &loads {
        let quiver = systems.get(System::Quiver.name());
        let best = [System::Nats, System::Redis, System::Kafka]
            .iter()
            .filter_map(|s| systems.get(s.name()).map(|l| (s.name(), *l)))
            .min_by_key(|(_, l)| key(l.ns("p99_ns")));
        let (lead_p99, lead_p999, verdict) = match (quiver, best) {
            (Some(q), Some((_, c))) => {
                let (a, ok_a) = lead(q.ns("p99_ns"), c.ns("p99_ns"));
                let (b, ok_b) = lead(q.ns("p999_ns"), c.ns("p999_ns"));
                (a, b, if ok_a && ok_b { "yes" } else { "no" })
            }
            _ => ("-".to_owned(), "-".to_owned(), "-"),
        };
        out.push_str(&format!(
            "| {busy} | {rate} | {subscribers} | {} | {} | {} | {} | {} | {} | {lead_p99} / {lead_p999} | {verdict} |\n",
            pair(systems.get(System::Wake.name())),
            pair(quiver),
            pair(systems.get(System::Nats.name())),
            pair(systems.get(System::Redis.name())),
            pair(systems.get(System::Kafka.name())),
            best.map_or_else(|| "-".to_owned(), |(name, _)| name.to_owned()),
        ));
    }
    out.push_str(&delivered(&loads));
    out
}

/// The deliveries a second of the window's messages each system's best run made, against what
/// was offered (the rate times the subscribers): where a broker can't fan out as fast as messages
/// come, its tail is undelivered and this says how far short it fell.
fn delivered(loads: &BTreeMap<(String, u64, u64), BTreeMap<String, &Line>>) -> String {
    let rate_of = |l: &Line| -> Option<f64> {
        let measured: f64 = l.get("measured").parse().ok()?;
        let window: f64 = l.get("window_s").parse().ok()?;
        (window > 0.0).then(|| measured / window)
    };
    let cell = |l: Option<&&Line>| {
        l.and_then(|l| rate_of(l))
            .map_or_else(|| "-".to_owned(), |r| format!("{:.2}M", r / 1e6))
    };
    let mut out = String::from(
        "\n| subscribers' threads | rate /s | subscribers | offered deliveries /s | Quiver | NATS | Redis | Kafka | Quiver's lead |\n|---|---|---|---|---|---|---|---|---|\n",
    );
    for ((busy, rate, subscribers), systems) in loads {
        let quiver = systems.get(System::Quiver.name());
        let best = [System::Nats, System::Redis, System::Kafka]
            .iter()
            .filter_map(|s| systems.get(s.name()).and_then(|l| rate_of(l)))
            .fold(None, |best: Option<f64>, r| {
                Some(best.map_or(r, |b| b.max(r)))
            });
        let lead = match (quiver.and_then(|l| rate_of(l)), best) {
            (Some(q), Some(c)) if c > 0.0 => format!("{:.1}×", q / c),
            (Some(_), Some(_)) => "∞".to_owned(),
            _ => "-".to_owned(),
        };
        out.push_str(&format!(
            "| {busy} | {rate} | {subscribers} | {:.2}M | {} | {} | {} | {} | {lead} |\n",
            (*rate as f64) * (*subscribers as f64) / 1e6,
            cell(quiver),
            cell(systems.get(System::Nats.name())),
            cell(systems.get(System::Redis.name())),
            cell(systems.get(System::Kafka.name())),
        ));
    }
    out
}
