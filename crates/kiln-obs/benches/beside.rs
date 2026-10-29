//! What a wide query costs the work beside it (`docs/observe.md` §12.19): the store of the `wide`
//! bench, the widest query read over and over, while another thread does what a write request does
//! between its bytes and its answer (a soak's request: 1,000 points, gzip'd, unpacked and decoded)
//! every 2 ms and times each. The query reads with 0, 1, 3, 7 and the default helper threads
//! (`KOBS_HELPERS`, a list), each a phase of `KOBS_SECS` (10 s); what the work beside it took, and
//! how fast the query was, are compared.
//!
//! `cargo bench -p kiln-obs --bench beside` (`KOBS_SERIES`, 4,000; `KOBS_MINUTES`, 10)
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use kiln_obs::batch::{Builder, Kind, SeriesKey};
use kiln_obs::hash::Keyed;
use kiln_obs::manifest::Point;
use kiln_obs::query::eval::{Request, eval, set_helpers};
use kiln_obs::store::Store;

fn env(k: &str, d: usize) -> usize {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

/// The minor page faults this process has taken (`/proc/self/stat`'s tenth field).
fn minflt() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let rest = stat.rsplit_once(')').map_or("", |(_, r)| r);
    rest.split_whitespace().nth(7).and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// This process's anonymous resident memory, MiB (`/proc/self/status`'s `RssAnon`).
fn anon_mib() -> f64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status.lines().find(|l| l.starts_with("RssAnon:")).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0) / 1024.0
}

fn pct(sorted: &[u64], q: f64) -> f64 {
    sorted.get(((sorted.len().max(1) - 1) as f64 * q).round() as usize).copied().unwrap_or(0) as f64 / 1000.0
}

fn main() {
    let (series, minutes, secs) = (env("KOBS_SERIES", 4_000), env("KOBS_MINUTES", 10) as i64, env("KOBS_SECS", 10) as u64);
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path(), Keyed::new()).unwrap();
    let keys: Vec<SeriesKey> = (0..series).map(|i| SeriesKey::new("load.v", [("host", format!("load-{}", i % 4).as_str()), ("s", i.to_string().as_str())], Kind::Gauge).unwrap()).collect();
    let t0 = 1_792_324_800_000i64;
    let mut index = 0u64;
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
    let to = t0 + minutes * 60_000;
    let q = "sum:load.v{*} by {host}";
    let default = kiln_obs::query::eval::default_helpers();
    println!("{series} series, {minutes} minutes; {} cores, {default} helpers by default", std::thread::available_parallelism().map_or(1, std::num::NonZero::get));
    let sweep: Vec<usize> = std::env::var("KOBS_HELPERS").ok().map_or_else(|| vec![0, 1, 3, 7, default], |v| v.split(',').filter_map(|x| x.trim().parse().ok()).collect());
    // A soak's request: 1,000 points of one host, gzip'd as an exporter sends it.
    let body = {
        use std::io::Write as _;
        let mut w = kiln_obs::otlp::pb::Writer::default();
        w.message(1, |rm| {
            rm.message(1, |res| {
                res.message(1, |kv| {
                    kv.str(1, "host.name");
                    kv.message(2, |av| av.str(1, "load-0"));
                });
            });
            rm.message(2, |sm| {
                sm.message(2, |m| {
                    m.str(1, "load.v");
                    m.message(5, |g| {
                        for j in 0..1_000 {
                            g.message(1, |dp| {
                                dp.message(7, |kv| {
                                    kv.str(1, "s");
                                    kv.message(2, |av| av.str(1, &j.to_string()));
                                });
                                dp.fixed64(3, 1_792_324_800_000_000_000);
                                dp.double(4, 1.0);
                            });
                        }
                    });
                });
            });
        });
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(&w.buf).unwrap();
        Arc::new(e.finish().unwrap())
    };
    for helpers in sweep {
        set_helpers(helpers);
        let stop = Arc::new(AtomicBool::new(false));
        // The work beside: the request unpacked and decoded, every 2 ms.
        let beside = {
            let (stop, body) = (stop.clone(), body.clone());
            std::thread::spawn(move || {
                use std::io::Read as _;
                let mut took = Vec::new();
                let mut next = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    let t = Instant::now();
                    let mut raw = Vec::new();
                    flate2::read::GzDecoder::new(body.as_slice()).read_to_end(&mut raw).unwrap();
                    let d = kiln_obs::otlp::metrics::decode(&raw, 1_792_324_800_000).unwrap();
                    std::hint::black_box(&d);
                    drop(d);
                    took.push(t.elapsed().as_nanos() as u64);
                    next += Duration::from_millis(2);
                    if let Some(wait) = next.checked_duration_since(Instant::now()) {
                        std::thread::sleep(wait);
                    } else {
                        next = Instant::now();
                    }
                }
                took
            })
        };
        let (mut queries, mut query_ms, faults0, started, anon0) = (0u64, Vec::new(), minflt(), Instant::now(), anon_mib());
        let mut anon_most = anon0;
        while started.elapsed() < Duration::from_secs(secs) {
            let t = Instant::now();
            let a = eval(&s, &Request::new(q, to - 15 * 60_000, to)).unwrap();
            assert_eq!(a.series.len(), 4.min(series));
            query_ms.push(t.elapsed().as_micros() as u64);
            queries += 1;
            if queries % 10 == 0 {
                anon_most = anon_most.max(anon_mib());
            }
        }
        let faults = minflt() - faults0;
        stop.store(true, Ordering::Relaxed);
        let mut took = beside.join().unwrap();
        took.sort_unstable();
        query_ms.sort_unstable();
        println!(
            "helpers {helpers:>2}: {queries} queries, median {:.1} ms; {:.0} page faults a query; anonymous memory {anon0:.0} MiB, most {anon_most:.0}, after {:.0}; beside, {} requests: p50 {:.1} µs, p99 {:.1} µs, p99.9 {:.1} µs, most {:.1} µs",
            pct(&query_ms, 0.5),
            faults as f64 / queries.max(1) as f64,
            anon_mib(),
            took.len(),
            pct(&took, 0.5),
            pct(&took, 0.99),
            pct(&took, 0.999),
            pct(&took, 1.0)
        );
    }
}
