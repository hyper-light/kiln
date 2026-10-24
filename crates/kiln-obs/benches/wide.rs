//! A dashboard's widest query on the soak's data (`docs/observe.md` §12.19): `sum:load.v{*} by
//! {host}` over the last 15 minutes of 4,000 series written 50 times a second (the soak's 200,000
//! points a second), the store cut each minute and merged as maintenance merges; the query's
//! median time, the samples it reads, and nanoseconds a sample. Then the same query read a part at
//! a time as the server asks it (`eval_shared`), the head holding a minute more, beside writes (a
//! point a series each 20 ms): its time, what it waited for the store, and what the writes waited
//! for it.
//!
//! `cargo bench -p kiln-obs --bench wide` (`KOBS_SERIES`, 4,000; `KOBS_MINUTES`, 15;
//! `KOBS_ROUNDS`, 9)
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use kiln_obs::batch::{Builder, Kind, SeriesKey};
use kiln_obs::hash::Keyed;
use kiln_obs::manifest::Point;
use kiln_obs::query::eval::{Request, eval, eval_shared};
use kiln_obs::store::Store;

fn env(k: &str, d: usize) -> usize {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

fn main() {
    let (series, minutes, rounds) = (env("KOBS_SERIES", 4_000), env("KOBS_MINUTES", 15) as i64, env("KOBS_ROUNDS", 9));
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path(), Keyed::new()).unwrap();
    let keys: Vec<SeriesKey> = (0..series).map(|i| SeriesKey::new("load.v", [("host", format!("load-{}", i % 4).as_str()), ("s", i.to_string().as_str())], Kind::Gauge).unwrap()).collect();
    let t0 = 1_792_324_800_000i64;
    let built = Instant::now();
    let mut index = 0u64;
    // A minute at a time: each series' 3,000 points of it (one each 20 ms), a batch a second.
    for minute in 0..minutes {
        for second in 0..60 {
            let mut b = Builder::new();
            for k in &keys {
                if index == 0 {
                    b.define(k);
                }
                let at = t0 + (minute * 60 + second) * 1000;
                b.run(k.fp, (0..50).map(|j| (at + j * 20, 1.0)));
            }
            index += 1;
            s.apply(&b.finish()).unwrap();
        }
        s.cut(Point { index, term: 1 }).unwrap();
        while let Some(p) = s.plan_merge() {
            s.commit(p.build().unwrap()).unwrap();
        }
    }
    let samples = series as f64 * minutes as f64 * 3_000.0;
    println!("{series} series, {minutes} minutes: {:.0} M samples written in {:.1} s, {} blocks", samples / 1e6, built.elapsed().as_secs_f64(), s.block_count());
    let to = t0 + minutes * 60_000;
    let q = "sum:load.v{*} by {host}";
    let mut times = Vec::new();
    let mut scanned = 0;
    for _ in 0..rounds {
        let t = Instant::now();
        let a = eval(&s, &Request::new(q, to - 15 * 60_000, to)).unwrap();
        times.push(t.elapsed().as_secs_f64());
        scanned = a.scanned_samples;
        assert_eq!(a.series.len(), 4.min(series));
    }
    times.sort_by(f64::total_cmp);
    let med = times[times.len() / 2];
    println!("{q} over 15 minutes: median {:.1} ms (least {:.1}, most {:.1}), {:.0} M samples read, {:.2} ns a sample", med * 1e3, times[0] * 1e3, times[times.len() - 1] * 1e3, scanned as f64 / 1e6, med * 1e9 / scanned.max(1) as f64);

    // The head holding a minute more (a cut's cadence at this load), then the query beside writes.
    for second in 0..60 {
        let mut b = Builder::new();
        for k in &keys {
            let at = to + second * 1000;
            b.run(k.fp, (0..50).map(|j| (at + j * 20, 1.0)));
        }
        s.apply(&b.finish()).unwrap();
    }
    // A snapshot of every series the query reads (`Store::metrics_into`), its heads a minute each:
    // what a query's one hold of the store costs a write waiting for it.
    let mut ids = Vec::new();
    s.select(&kiln_obs::index::Selector::metric("load.v"), &mut ids);
    let mut at = kiln_obs::store::MetricsAt::default();
    let mut snaps = Vec::new();
    for _ in 0..rounds {
        let t = Instant::now();
        let n = s.metrics_into(&ids, usize::MAX, &mut at);
        snaps.push(t.elapsed().as_secs_f64());
        assert_eq!(n, ids.len());
    }
    snaps.sort_by(f64::total_cmp);
    println!("a snapshot of its {} series: median {:.2} ms (most {:.2}), {:.1} MB copied", ids.len(), snaps[snaps.len() / 2] * 1e3, snaps[snaps.len() - 1] * 1e3, at.bytes() as f64 / 1e6);
    let store = std::sync::RwLock::new(s);
    let stop = AtomicBool::new(false);
    let (asked, (waits, holds)) = std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            // How long each write waited for the store, and how long it held it.
            let (mut waits, mut holds) = (Vec::new(), Vec::new());
            let mut at = to + 60_000;
            while !stop.load(Ordering::Relaxed) {
                let mut b = Builder::new();
                for k in &keys {
                    b.run(k.fp, std::iter::once((at, 1.0)));
                }
                let batch = b.finish();
                let t = Instant::now();
                let mut g = store.write().unwrap();
                waits.push(t.elapsed().as_secs_f64());
                let t = Instant::now();
                g.apply(&batch).unwrap();
                drop(g);
                holds.push(t.elapsed().as_secs_f64());
                at += 20;
                std::thread::sleep(Duration::from_millis(20));
            }
            (waits, holds)
        });
        let mut asked = Vec::new();
        for _ in 0..rounds.max(25) {
            let t = Instant::now();
            let (a, waited_ms) = eval_shared(&store, &Request::new(q, to - 15 * 60_000, to)).unwrap();
            asked.push((t.elapsed().as_secs_f64(), waited_ms));
            assert_eq!(a.series.len(), 4.min(series));
        }
        stop.store(true, Ordering::Relaxed);
        (asked, writer.join().unwrap())
    });
    let pct = |v: &mut Vec<f64>, p: f64| {
        v.sort_by(f64::total_cmp);
        v[((v.len() - 1) as f64 * p).round() as usize]
    };
    let (mut took, mut waited): (Vec<f64>, Vec<f64>) = asked.iter().map(|&(t, w)| (t * 1e3, w)).unzip();
    let mut writes: Vec<f64> = waits.iter().map(|w| w * 1e3).collect();
    let mut held: Vec<f64> = holds.iter().map(|h| h * 1e3).collect();
    println!(
        "beside writes, read a part at a time: median {:.1} ms (most {:.1}); waited for the store median {:.2} ms (most {:.2}); writes waited p50 {:.3} ms, p99 {:.2}, most {:.2} ({} writes), each held the store p50 {:.3} ms, p99 {:.3}",
        pct(&mut took, 0.5),
        pct(&mut took, 1.0),
        pct(&mut waited, 0.5),
        pct(&mut waited, 1.0),
        pct(&mut writes, 0.5),
        pct(&mut writes, 0.99),
        pct(&mut writes, 1.0),
        writes.len(),
        pct(&mut held, 0.5),
        pct(&mut held, 0.99)
    );
}
