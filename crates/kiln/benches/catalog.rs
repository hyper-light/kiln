//! The Service Catalog's list at a large kiln's size (`docs/observe.md` §10.8, §12.30): 2,000
//! services in two environments with 240 deployments each (960,000 in DORA's ledger), 500 monitors
//! reading them. What each service's facts of deployments cost (its last, its 30 days' count and
//! failures), and the monitors' index by service; the time (the median of five) and allocations of
//! one.
//!
//! `cargo bench -p kiln --bench catalog` (`KOBS_CAT_SERVICES` to change how many services)
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use std::time::Instant;

use hyper_measure::alloc;
use kiln::observe::catalog::{by_service, deployments_of};
use kiln::observe::dora::ledger::{DAY_NS, Deployment, Git, Ledger, NS_PER_S};

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// `f` five times: ns a call (the median pass), allocations a call (the first).
fn time<R>(calls: usize, mut f: impl FnMut(usize) -> R) -> (f64, f64) {
    let (mut allocs, mut times) = (0.0, Vec::new());
    for pass in 0..5 {
        if pass == 0 {
            alloc::begin();
        }
        let t = Instant::now();
        for i in 0..calls {
            std::hint::black_box(f(i));
        }
        times.push(t.elapsed().as_nanos() as f64 / calls as f64);
        if pass == 0 {
            allocs = alloc::end().calls() as f64 / calls as f64;
        }
    }
    (median(times), allocs)
}

fn main() {
    let services: usize = std::env::var("KOBS_CAT_SERVICES").ok().and_then(|v| v.parse().ok()).unwrap_or(2_000);
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let now = now_ms * 1_000_000;
    let mut l = Ledger::default();
    let per = 240;
    for s in 0..services {
        for env in ["prod", "staging"] {
            for n in 0..per {
                let t = now - (per - n) as i64 * (399 * DAY_NS / per as i64);
                l.put(Deployment {
                    id: format!("cat-bench-{s:05}-{env}-{n:04}"),
                    service: format!("service-{s:05}"),
                    env: env.into(),
                    team: format!("team-{}", s % 40),
                    version: format!("1.{n}"),
                    started_at: t - 60 * NS_PER_S,
                    finished_at: t,
                    git: Some(Git { commit_sha: format!("{:040x}", (s * 1_000_000 + n) as u64), repository_url: format!("https://github.com/acme/service-{s:05}") }),
                    custom_tags: Vec::new(),
                    source: "api".into(),
                    lead: None,
                    change_failure: None,
                    remedies: Vec::new(),
                    shas: Vec::new(),
                    titles: Vec::new(),
                });
            }
        }
    }
    println!("{} deployments of {services} services in two environments", l.len().0);
    let names: Vec<String> = (0..services).map(|s| format!("service-{s:05}")).collect();
    let (ns, a) = time(services, |i| deployments_of(&l, &names[i % services], now_ms));
    println!("a service's deployments (its last, its 30 days'): {:.1} µs, {a:.1} allocations; every service's {:.1} ms", ns / 1e3, ns * services as f64 / 1e6);
    // The first cut's: its last deployment found by a look at every deployment.
    let (ns, _) = time(5, |i| l.rows().filter(|r| r.service_name() == names[i % services]).map(|r| r.finished_at()).max());
    println!("the first cut's last deployment, a look at every one: {:.1} ms a service; every service's {:.1} s", ns / 1e6, ns * services as f64 / 1e9);
    let monitors: Vec<(String, String, String, Vec<String>, String)> = (0..500)
        .map(|m| {
            (
                format!("m{m}"),
                format!("monitor {m}"),
                format!("avg(last_5m):avg:trace.http.request.duration{{service:service-{:05},env:prod}} > 5", m * 4 % services.max(1)),
                vec![format!("service:service-{:05}", (m * 4 + 1) % services.max(1)), "team:x".into()],
                "OK".into(),
            )
        })
        .collect();
    let (ns, a) = time(20, |_| by_service(monitors.clone()));
    println!("500 monitors indexed by the services they read: {:.2} ms, {a:.0} allocations (their clone's included)", ns / 1e6);
}
