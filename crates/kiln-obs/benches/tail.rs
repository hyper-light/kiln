//! Live tail on recorded logs (`docs/observe.md` §12.14): what a feed costs the replica that applies
//! (none attached, attached with no one following, followed), what a follower's read costs a row
//! for real searches and allocates a tick, and how the replica applies with followers reading on
//! threads of their own.
//!
//! `KOBS_LOGS=<journal>:<pods> cargo bench -p kiln-obs --bench tail`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use hyper_measure::alloc;
use kiln_obs::batch::Builder;
use kiln_obs::command;
use kiln_obs::logs::search::parse;
use kiln_obs::logs::tail::{Feed, Sample};
use kiln_obs::store::Store;

#[path = "common/loglines.rs"]
mod loglines;

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

/// Records an entry carries: an OTLP export's worth.
const PER_ENTRY: usize = 1_000;

/// The records as the group's entries, each written as of a minute after the newest record (a
/// sender's clock before its records' would make them all ahead of it, and dropped).
fn entries(records: &[kiln_obs::logs::record::Record]) -> Vec<Vec<u8>> {
    let at = records.last().map_or(0, |r| r.ts / 1_000_000) + 60_000;
    let mut b = Builder::new();
    records
        .chunks(PER_ENTRY)
        .enumerate()
        .map(|(i, chunk)| {
            b.logs(chunk);
            command::write(7, i as u64 + 1, at, &b.finish())
        })
        .collect()
}

/// Applies every entry to a fresh store with `feed` (if any): seconds, and allocations.
fn apply_all(entries: &[Vec<u8>], feed: Option<Arc<Feed>>) -> (f64, u64) {
    let mut store = Store::default();
    store.set_feed(feed);
    alloc::begin();
    let t = Instant::now();
    for (i, e) in entries.iter().enumerate() {
        store.apply_entry(i as u64 + 1, e).unwrap();
    }
    let el = t.elapsed().as_secs_f64();
    (el, alloc::end().calls())
}

fn main() {
    let (records, raw_bytes) = match loglines::load() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return;
        }
    };
    let n = records.len();
    let entries = entries(&records);
    println!("records: {n} ({:.1} MB of lines), {} entries of {PER_ENTRY}", raw_bytes as f64 / 1e6, entries.len());

    // What applying costs with a feed: none, attached with no one following, followed (the follower
    // reads after every entry, its read timed apart). Nine rounds, the three taken in turn in each
    // (so a machine's drift falls on all three alike); the median of each.
    println!("\n{:<34} {:>14} {:>16}", "applying (median of 9)", "records/s", "allocations/rec");
    let followed_once = |entries: &[Vec<u8>]| -> (f64, u64) {
        let feed = Feed::new();
        let mut f = feed.follow().unwrap();
        let all = parse("*").unwrap();
        let mut sample = Sample::new(10, 1);
        let mut store = Store::default();
        store.set_feed(Some(feed.clone()));
        let mut applying = Duration::ZERO;
        let mut allocs = 0;
        for (i, e) in entries.iter().enumerate() {
            alloc::begin();
            let t = Instant::now();
            store.apply_entry(i as u64 + 1, e).unwrap();
            applying += t.elapsed();
            allocs += alloc::end().calls();
            f.read(&all, &mut sample).unwrap();
            sample.clear(10);
        }
        (applying.as_secs_f64(), allocs)
    };
    let (mut none, mut idle, mut fol) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..9 {
        none.push(apply_all(&entries, None));
        idle.push(apply_all(&entries, Some(Feed::new())));
        fol.push(followed_once(&entries));
    }
    let median = |mut v: Vec<(f64, u64)>| {
        v.sort_by(|a, b| a.0.total_cmp(&b.0));
        v[v.len() / 2]
    };
    for (what, runs) in [("no feed", none), ("a feed, no one following", idle), ("followed (the apply alone)", fol)] {
        let (secs, allocs) = median(runs);
        println!("{:<34} {:>14.0} {:>16.3}", what, n as f64 / secs, allocs as f64 / n as f64);
    }

    // A follower's read: every row matched, a uniform sample kept; a tick is 5 entries (5,000 rows,
    // a fifth of a second at 25,000 records a second).
    let queries = ["*", "status:error", "service:kube status:warn", "\"connection refused\"", "@application:kiln-namespaces", "service:argocd -status:info"];
    println!("\n{:<34} {:>8} {:>10} {:>12} {:>14} {:>16}", "a follower reads (5,000 a tick)", "kept", "matched", "ns/row", "rows/s", "allocs/tick");
    for max in [10usize, 200] {
        for q in queries {
            let search = parse(q).unwrap();
            let feed = Feed::new();
            let mut f = feed.follow().unwrap();
            let mut sample = Sample::new(max, 1);
            let mut store = Store::default();
            store.set_feed(Some(feed.clone()));
            let (mut reading, mut ticks, mut matched, mut kept) = (Duration::ZERO, 0u64, 0u64, 0usize);
            let mut allocs = 0u64;
            for (i, e) in entries.iter().enumerate() {
                store.apply_entry(i as u64 + 1, e).unwrap();
                if i % 5 == 4 {
                    alloc::begin();
                    let t = Instant::now();
                    f.read(&search, &mut sample).unwrap();
                    kept += sample.records().count();
                    reading += t.elapsed();
                    let a = alloc::end().calls();
                    // The first ticks grow the reservoir's buffers; what a tick costs after is the rest.
                    if ticks >= 5 {
                        allocs += a;
                    }
                    matched += sample.matched;
                    sample.clear(max);
                    ticks += 1;
                }
            }
            let rows = (ticks as usize * 5 * PER_ENTRY).min(n);
            println!(
                "{:<34} {:>8} {:>10} {:>12.1} {:>14.0} {:>16.2}",
                format!("{q} (max {max})"),
                kept,
                matched,
                reading.as_nanos() as f64 / rows as f64,
                rows as f64 / reading.as_secs_f64(),
                allocs as f64 / (ticks - 5) as f64
            );
        }
    }

    // Followers on threads of their own, each reading every 50 ms (as a tail does), while the replica
    // applies everything as fast as it can.
    println!("\n{:<34} {:>14} {:>14}", "followers on threads", "records/s", "missed rows");
    for followers in [0usize, 1, 4, 16] {
        let feed = Feed::new();
        let done = Arc::new(AtomicBool::new(false));
        let mut store = Store::default();
        store.set_feed(Some(feed.clone()));
        let (secs, missed) = std::thread::scope(|s| {
            let readers: Vec<_> = (0..followers)
                .map(|_| {
                    let mut f = feed.follow().unwrap();
                    let done = done.clone();
                    s.spawn(move || {
                        let search = parse("status:error").unwrap();
                        let mut sample = Sample::new(10, 1);
                        let mut missed = 0;
                        loop {
                            let last = done.load(Ordering::Acquire);
                            f.read(&search, &mut sample).unwrap();
                            missed += sample.missed;
                            sample.clear(10);
                            if last {
                                return missed;
                            }
                            std::thread::sleep(Duration::from_millis(50));
                        }
                    })
                })
                .collect();
            let t = Instant::now();
            for (i, e) in entries.iter().enumerate() {
                store.apply_entry(i as u64 + 1, e).unwrap();
            }
            let secs = t.elapsed().as_secs_f64();
            done.store(true, Ordering::Release);
            let missed: u64 = readers.into_iter().map(|r| r.join().unwrap()).sum();
            (secs, missed)
        });
        println!("{:<34} {:>14.0} {:>14}", format!("{followers} following"), n as f64 / secs, missed);
    }
}
