//! Error Tracking's issues on the server (`docs/observe.md` §8.8, §12.24), at the registry's bound
//! (10,000 issues, each erring in three series: two versions of an env, one of another): the
//! registry's read of what the metric says, every ten seconds, every issue known; and an Error
//! Tracking monitor's evaluation, of impact (every issue watched) and of new issues (none new). The
//! time of each (the median of eleven) and its allocations; what each is handed is made before. The
//! metric's own read (`kiln_obs::errors::seen`) is `kiln-obs`'s errors bench.
//!
//! `cargo bench -p kiln --bench issues`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::cast_precision_loss, clippy::indexing_slicing)]

use std::time::Instant;

use hyper_measure::alloc;
use kiln::observe::errors::{Issues, MAX_ISSUES, Sample};
use kiln_obs::errors::Seen;

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

const ROUNDS: usize = 11;

fn median(mut v: Vec<u64>) -> u64 {
    v.sort_unstable();
    v[v.len() / 2]
}

fn main() {
    let dir = std::env::temp_dir().join(format!("kiln-issues-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let is = Issues::open(&dir);
    let t = 1_800_000_000_000i64;
    let rows: Vec<Seen> = (0..MAX_ISSUES as u64)
        .flat_map(|i| {
            [("prod", "1.4.2"), ("prod", "1.4.3"), ("staging", "1.5.0")].map(|(env, version)| Seen {
                issue: format!("{:016x}", i.wrapping_mul(0x9e37_79b9_7f4a_7c15)),
                service: format!("svc-{}", i % 100),
                env: env.into(),
                version: version.into(),
                errors: 3.0,
                first: t - 600_000,
                last: t - 10_000,
            })
        })
        .collect();
    let sample = |id: &str, _: i64, _: i64| {
        Some(Sample { source: "span".into(), kind: "ValueError".into(), message: format!("bad item in cart {id}"), file: "/app/cart.py".into(), function: format!("total_{}", &id[..4]) })
    };
    let started = Instant::now();
    is.scan(t, |_, _| rows.clone(), sample);
    println!("{MAX_ISSUES} issues made from {} series: {:.1} ms", rows.len(), started.elapsed().as_secs_f64() * 1e3);

    let mut copies: Vec<Vec<Seen>> = (0..=ROUNDS).map(|_| rows.clone()).collect();
    let times: Vec<u64> = (0..ROUNDS)
        .map(|r| {
            let rows = copies.pop().unwrap();
            let at = Instant::now();
            is.scan(t + (r as i64 + 1) * 10_000, move |_, _| rows, |_, _, _| None);
            at.elapsed().as_nanos() as u64
        })
        .collect();
    let once = copies.pop().unwrap();
    alloc::begin();
    is.scan(t + 200_000, move |_, _| once, |_, _, _| None);
    let c = alloc::end();
    println!("{:<34} {:>8.2} ms {:>9} allocations ({:.2} a series)", "a read, every issue known", median(times) as f64 / 1e6, c.calls(), c.calls() as f64 / rows.len() as f64);

    let all = kiln_obs::logs::search::parse("*").unwrap();
    let some = kiln_obs::logs::search::parse("service:svc-7 env:prod").unwrap();
    let by_issue = vec!["issue.id".to_owned()];
    let by_env = vec!["issue.id".to_owned(), "env".to_owned()];
    for (name, search, new, by) in [
        ("impact, by issue", &all, false, &by_issue),
        ("impact, by issue and env", &all, false, &by_env),
        ("impact, a service's in an env", &some, false, &by_issue),
        ("new issues, none new", &all, true, &by_issue),
    ] {
        let mut copies: Vec<Vec<Seen>> = (0..=ROUNDS).map(|_| rows.clone()).collect();
        let mut groups = 0;
        // What it is handed is taken when it reads (and dropped there, as a read's is), else kept
        // out of the time.
        let times: Vec<u64> = (0..ROUNDS)
            .map(|_| {
                let mut held = copies.pop().unwrap();
                let at = Instant::now();
                let v = is.monitor_values(search, new, by, t + 300_000, t + 400_000, |_| Ok::<_, ()>(std::mem::take(&mut held))).unwrap();
                let ns = at.elapsed().as_nanos() as u64;
                groups = v.len();
                ns
            })
            .collect();
        let mut held = copies.pop().unwrap();
        alloc::begin();
        let v = is.monitor_values(search, new, by, t + 300_000, t + 400_000, |_| Ok::<_, ()>(std::mem::take(&mut held))).unwrap();
        let c = alloc::end();
        drop((v, held));
        println!("{name:<34} {:>8.2} ms {:>9} allocations, {groups} groups", median(times) as f64 / 1e6, c.calls());
    }
    let _ = std::fs::remove_dir_all(&dir);
}
