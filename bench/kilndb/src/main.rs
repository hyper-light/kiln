//! KilnDB against clustered MySQL (Vitess) on the conversation workload (`docs/kilndb.md`, K0).
//!
//! One workload, one harness, a driver per database:
//! - **preload**: every session gets its history first (not measured, but timed);
//! - **mixed**: writers append events durably (a write returns once committed) while readers read
//!   sessions' latest pages, for a while;
//! - **search**: a word across every session, newest first.
//!
//! Each run prints one JSON line: throughput and latency percentiles of each part.

mod local;
mod vitess;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// An event as the workload writes it: kiln's event row (crates/kiln/src/convo/rows.rs).
#[derive(Clone)]
pub struct Event {
    pub session: String,
    pub id: String,
    pub seq: u64,
    pub at: i64,
    pub kind: &'static str,
    pub text: String,
    pub body: String,
}

/// A database the workload runs against.
pub trait Driver: Send + Sync {
    /// The events, durably: returns once they are committed.
    fn write(&self, events: &[Event]) -> anyhow::Result<()>;
    /// A session's latest `limit` events: how many came.
    fn history(&self, session: &str, limit: usize) -> anyhow::Result<usize>;
    /// Events whose text holds `word`, newest first, at most `limit`: how many came.
    fn search(&self, word: &str, limit: usize) -> anyhow::Result<usize>;
}

struct Opts {
    target: String,
    dir: String,
    dsn: String,
    sessions: usize,
    preload: usize,
    writers: usize,
    batch: usize,
    readers: usize,
    seconds: u64,
    searches: usize,
    text_bytes: usize,
}

fn opts() -> anyhow::Result<Opts> {
    let mut o = Opts {
        target: "local".into(),
        dir: "/tmp/kilndb-bench".into(),
        dsn: "mysql://root@127.0.0.1:33577/convo".into(),
        sessions: 1000,
        preload: 200,
        writers: 32,
        batch: 1,
        readers: 4,
        seconds: 30,
        searches: 20,
        text_bytes: 600,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut v = || args.next().ok_or_else(|| anyhow::anyhow!("{a} takes a value"));
        match a.as_str() {
            "--target" => o.target = v()?,
            "--dir" => o.dir = v()?,
            "--dsn" => o.dsn = v()?,
            "--sessions" => o.sessions = v()?.parse()?,
            "--preload" => o.preload = v()?.parse()?,
            "--writers" => o.writers = v()?.parse()?,
            "--batch" => o.batch = v()?.parse()?,
            "--readers" => o.readers = v()?.parse()?,
            "--seconds" => o.seconds = v()?.parse()?,
            "--searches" => o.searches = v()?.parse()?,
            "--text-bytes" => o.text_bytes = v()?.parse()?,
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    Ok(o)
}

/// A small, fast generator: the same workload every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % (n.max(1) as u64)).unwrap_or(0)
    }
}

const WORDS: [&str; 24] = [
    "the", "agent", "ran", "cargo", "test", "and", "read", "file", "then", "wrote", "patch", "store", "writer", "while", "feed", "waited", "build", "deploy", "review", "shard", "raft", "leader", "commit",
    "quorum",
];
/// A word few events hold, and one none does: the searches that look at the most.
const RARE: &str = "zephyr";
const NONE: &str = "zzqxv";

fn text(rng: &mut Rng, bytes: usize) -> String {
    let mut s = String::with_capacity(bytes + 16);
    // One event in a thousand says the rare word.
    if rng.below(1000) == 0 {
        s.push_str(RARE);
    }
    while s.len() < bytes {
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(WORDS[rng.below(WORDS.len())]);
    }
    s
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn event(rng: &mut Rng, session: usize, seq: u64, text_bytes: usize) -> Event {
    let kinds = ["assistant", "user", "tool", "thinking"];
    let kind = kinds[rng.below(kinds.len())];
    let text = text(rng, text_bytes);
    let body = serde_json::json!({ "kind": kind, "text": text, "n": seq }).to_string();
    Event { session: format!("s-{session:06}"), id: format!("e-{seq}"), seq, at: now_ms(), kind, text, body }
}

/// Percentiles of latencies (µs) in ms.
fn summary(mut us: Vec<u64>) -> serde_json::Value {
    if us.is_empty() {
        return serde_json::json!(null);
    }
    us.sort_unstable();
    let at = |q: f64| us[((us.len() as f64 * q).ceil() as usize).saturating_sub(1).min(us.len() - 1)] as f64 / 1000.0;
    serde_json::json!({ "n": us.len(), "p50": at(0.5), "p90": at(0.9), "p99": at(0.99), "p999": at(0.999), "max": at(1.0) })
}

fn main() -> anyhow::Result<()> {
    let o = opts()?;
    let driver: Arc<dyn Driver> = match o.target.as_str() {
        "local" => Arc::new(local::Local::open(&o.dir)?),
        "vitess" => Arc::new(vitess::Vitess::connect(&o.dsn, o.writers + o.readers + 2)?),
        t => anyhow::bail!("unknown target {t} (local, vitess)"),
    };

    // Preload: every session's history, a hundred events a write, writers side by side.
    let seqs: Arc<Vec<AtomicU64>> = Arc::new((0..o.sessions).map(|_| AtomicU64::new(0)).collect());
    let began = Instant::now();
    std::thread::scope(|s| -> anyhow::Result<()> {
        let mut handles = Vec::new();
        for w in 0..o.writers.max(1) {
            let (driver, seqs, o) = (driver.clone(), seqs.clone(), &o);
            handles.push(s.spawn(move || -> anyhow::Result<()> {
                let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ (w as u64 + 1));
                for session in (w..o.sessions).step_by(o.writers.max(1)) {
                    let mut left = o.preload;
                    while left > 0 {
                        let n = left.min(100);
                        let batch: Vec<Event> = (0..n).map(|_| event(&mut rng, session, seqs[session].fetch_add(1, Ordering::Relaxed), o.text_bytes)).collect();
                        driver.write(&batch)?;
                        left -= n;
                    }
                }
                Ok(())
            }));
        }
        for h in handles {
            h.join().map_err(|_| anyhow::anyhow!("a preload writer panicked"))??;
        }
        Ok(())
    })?;
    let preload_s = began.elapsed().as_secs_f64();

    // Mixed: writers append, readers read, for a while.
    let stop = Arc::new(AtomicBool::new(false));
    let until = Duration::from_secs(o.seconds);
    let (writes, reads) = std::thread::scope(|s| -> anyhow::Result<(Vec<u64>, Vec<u64>)> {
        let mut ws = Vec::new();
        for w in 0..o.writers {
            let (driver, seqs, stop, o) = (driver.clone(), seqs.clone(), stop.clone(), &o);
            ws.push(s.spawn(move || -> anyhow::Result<Vec<u64>> {
                let mut rng = Rng(0xd1b5_4a32_d192_ed03 ^ (w as u64 + 1));
                let mut lat = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    let session = rng.below(o.sessions);
                    let batch: Vec<Event> = (0..o.batch.max(1)).map(|_| event(&mut rng, session, seqs[session].fetch_add(1, Ordering::Relaxed), o.text_bytes)).collect();
                    let t = Instant::now();
                    driver.write(&batch)?;
                    lat.push(u64::try_from(t.elapsed().as_micros()).unwrap_or(u64::MAX));
                }
                Ok(lat)
            }));
        }
        let mut rs = Vec::new();
        for r in 0..o.readers {
            let (driver, stop, o) = (driver.clone(), stop.clone(), &o);
            rs.push(s.spawn(move || -> anyhow::Result<Vec<u64>> {
                let mut rng = Rng(0x2545_f491_4f6c_dd1d ^ (r as u64 + 1));
                let mut lat = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    let session = format!("s-{:06}", rng.below(o.sessions));
                    let t = Instant::now();
                    driver.history(&session, 50)?;
                    lat.push(u64::try_from(t.elapsed().as_micros()).unwrap_or(u64::MAX));
                }
                Ok(lat)
            }));
        }
        std::thread::sleep(until);
        stop.store(true, Ordering::Relaxed);
        let mut writes = Vec::new();
        for h in ws {
            writes.extend(h.join().map_err(|_| anyhow::anyhow!("a writer panicked"))??);
        }
        let mut reads = Vec::new();
        for h in rs {
            reads.extend(h.join().map_err(|_| anyhow::anyhow!("a reader panicked"))??);
        }
        Ok((writes, reads))
    })?;

    // Search: a common word, the rare one, and one none says.
    let mut searches = Vec::new();
    for i in 0..o.searches {
        let word = match i % 3 {
            0 => "quorum",
            1 => RARE,
            _ => NONE,
        };
        let t = Instant::now();
        driver.search(word, 20)?;
        searches.push(u64::try_from(t.elapsed().as_micros()).unwrap_or(u64::MAX));
    }

    let events_written = writes.len() * o.batch.max(1);
    let report = serde_json::json!({
        "target": o.target,
        "sessions": o.sessions,
        "preload_events": o.sessions * o.preload,
        "preload_s": preload_s,
        "preload_events_per_s": (o.sessions * o.preload) as f64 / preload_s.max(1e-9),
        "writers": o.writers,
        "batch": o.batch,
        "readers": o.readers,
        "seconds": o.seconds,
        "writes": writes.len(),
        "events_written": events_written,
        "write_events_per_s": events_written as f64 / o.seconds as f64,
        "write_ms": summary(writes),
        "read_ms": summary(reads),
        "search_ms": summary(searches),
    });
    println!("{report}");
    Ok(())
}
