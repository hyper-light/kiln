//! A dashboard's widest query on the soak's data (`docs/observe.md` §12.19): `sum:load.v{*} by
//! {host}` over the last 15 minutes of 4,000 series written 50 times a second (the soak's 200,000
//! points a second), the store cut each minute and merged as maintenance merges; the query's
//! median time, the samples it reads, and nanoseconds a sample.
//!
//! `cargo bench -p kiln-obs --bench wide` (`KOBS_SERIES`, 4,000; `KOBS_MINUTES`, 15;
//! `KOBS_ROUNDS`, 9)
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use std::time::Instant;

use kiln_obs::batch::{Builder, Kind, SeriesKey};
use kiln_obs::hash::Keyed;
use kiln_obs::manifest::Point;
use kiln_obs::query::eval::{Request, eval};
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
}
