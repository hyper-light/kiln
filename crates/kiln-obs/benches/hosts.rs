//! The infrastructure list's reading of the store (`docs/observe.md` §12.31), on fleets of hosts
//! each reporting as a kiln runner or Datadog's Agent reports one (its System Check's 50 series,
//! an app's ten): each host's series, newest sample, namespaces and platform tags
//! ([`kiln_obs::store::Store::tag_seen`]), and each of the list's figures: a query of every host at
//! once over its last ten minutes, as the list first read them, and each series' newest sample by
//! host (`Store::newest_by`), as it reads them now. Time (the median of five), allocations, and the
//! series held.
//!
//! `cargo bench -p kiln-obs --bench hosts` (`KOBS_HOSTS`: the fleets' sizes, `1000,5000,20000`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss)]

use std::time::{Duration, Instant};

use hyper_measure::alloc;
use kiln_obs::batch::{Builder, Kind, SeriesKey};
use kiln_obs::hash::Keyed;
use kiln_obs::query::eval::{Request, eval};
use kiln_obs::store::{Scratch, Store};

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

const NOW: i64 = 1_792_324_800_000;

/// The list's figures' queries, as the server asks them.
const FIGURES: [&str; 10] = [
    "avg:system.cpu.idle{*} by {host}",
    "avg:system.cpu.iowait{*} by {host}",
    "avg:system.load.15{*} by {host}",
    "avg:system.load.norm.15{*} by {host}",
    "avg:system.mem.pct_usable{*} by {host}",
    "max:system.disk.in_use{*} by {host}",
    "avg:system.cpu.num_cores{*} by {host}",
    "avg:system.mem.total{*} by {host}",
    "avg:system.cpu.pct_usage{*} by {host}",
    "avg:system.mem.used{*} by {host}",
];

/// What a host's series carry that says its platform.
const PLATFORM: [&str; 12] = [
    "host.name",
    "os.type",
    "os.description",
    "host.arch",
    "host.type",
    "cloud.provider",
    "cloud.platform",
    "cloud.region",
    "cloud.availability_zone",
    "k8s.cluster.name",
    "k8s.node.name",
    "kube_cluster_name",
];

/// A host's metrics as the System Check reports them (two disks, two interfaces), and one app's.
fn metrics(app: usize) -> Vec<(String, Vec<(&'static str, &'static str)>)> {
    let mut out: Vec<(String, Vec<(&'static str, &'static str)>)> = Vec::new();
    for m in ["user", "system", "idle", "iowait", "stolen", "guest", "num_cores"] {
        out.push((format!("system.cpu.{m}"), Vec::new()));
    }
    for m in ["1", "5", "15", "norm.1", "norm.5", "norm.15"] {
        out.push((format!("system.load.{m}"), Vec::new()));
    }
    for m in ["total", "used", "free", "usable", "pct_usable", "cached", "buffered"] {
        out.push((format!("system.mem.{m}"), Vec::new()));
    }
    for m in ["total", "used", "free"] {
        out.push((format!("system.swap.{m}"), Vec::new()));
    }
    for device in ["/dev/nvme0n1p1", "/dev/nvme0n1p2"] {
        for m in ["total", "used", "free", "in_use"] {
            out.push((format!("system.disk.{m}"), vec![("device", device)]));
        }
        for m in ["r_s", "w_s", "util"] {
            out.push((format!("system.io.{m}"), vec![("device", device)]));
        }
    }
    for iface in ["eth0", "eth1"] {
        for m in ["bytes_rcvd", "bytes_sent", "packets_in.count", "packets_out.count"] {
            out.push((format!("system.net.{m}"), vec![("device", iface)]));
        }
    }
    let ns = ["nginx", "postgresql", "redis", "kafka"][app % 4];
    for i in 0..10 {
        out.push((format!("{ns}.metric_{i}"), Vec::new()));
    }
    out
}

fn fleet(hosts: usize) -> Store {
    let mut s = Store::new(Keyed::with_keys(1, 2));
    let names: Vec<String> = (0..hosts).map(|h| format!("host-{h:06}")).collect();
    for chunk in (0..hosts).collect::<Vec<_>>().chunks(200) {
        let mut b = Builder::new();
        let mut keys = Vec::new();
        for &h in chunk {
            let mut base: Vec<(&str, &str)> = vec![("host", names[h].as_str()), ("env", "prod")];
            if h % 3 == 0 {
                base.push(("os.type", "linux"));
                base.push(("host.name", names[h].as_str()));
            }
            if h % 5 == 0 {
                base.push(("cloud.provider", "aws"));
            }
            for (metric, extra) in metrics(h) {
                let tags: Vec<(&str, &str)> = base.iter().copied().chain(extra.iter().copied()).collect();
                let k = SeriesKey::new(&metric, tags, Kind::Gauge).unwrap();
                b.define(&k);
                keys.push((k, h));
            }
        }
        for (k, h) in &keys {
            let v = (h % 100) as f64;
            b.run(k.fp, [(NOW - 30_000, v), (NOW - 15_000, v + 1.0)]);
        }
        s.apply(&b.finish()).unwrap();
    }
    s
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn main() {
    let sizes: Vec<usize> = std::env::var("KOBS_HOSTS").ok().map_or_else(|| vec![1000, 5000, 20000], |v| v.split(',').filter_map(|s| s.trim().parse().ok()).collect());
    println!("{:<9} {:>9} {:<44} {:>10} {:>9} {:>9}", "hosts", "series", "what", "time", "allocs", "answers");
    for hosts in sizes {
        let store = fleet(hosts);
        let series = store.series_count();
        let mut times = Vec::new();
        let mut counted = alloc::Counts::default();
        let mut seen = 0;
        for i in 0..5 {
            let t = Instant::now();
            if i == 1 {
                alloc::begin();
            }
            let s = store.tag_seen("host", NOW - 7 * 86_400_000, &PLATFORM);
            if i == 1 {
                counted = alloc::end();
            }
            times.push(t.elapsed());
            seen = s.len();
        }
        assert_eq!(seen, hosts, "every host seen");
        println!("{:<9} {:>9} {:<44} {:>8.2}ms {:>9} {:>9}", hosts, series, "each host's series, newest, apps, platform", median(times).as_secs_f64() * 1e3, counted.calls(), seen);
        let mut all = Vec::new();
        for q in FIGURES {
            let mut r = Request::new(q, NOW - 600_000, NOW);
            r.step = Some(60_000);
            r.max_series = 200_000;
            let mut times = Vec::new();
            let mut answers = 0;
            let mut counted = alloc::Counts::default();
            for i in 0..5 {
                let t = Instant::now();
                if i == 1 {
                    alloc::begin();
                }
                let a = eval(&store, &r).unwrap();
                if i == 1 {
                    counted = alloc::end();
                }
                times.push(t.elapsed());
                answers = a.series.len();
            }
            let m = median(times);
            all.push(m);
            println!("{:<9} {:>9} {:<44} {:>8.2}ms {:>9} {:>9}", "", "", q, m.as_secs_f64() * 1e3, counted.calls(), answers);
        }
        let figures: Duration = all.iter().sum();
        println!("{:<9} {:>9} {:<44} {:>8.2}ms", "", "", "the ten figures' queries in all", figures.as_secs_f64() * 1e3);
        // The same figures by each series' newest sample, a host's taken together.
        let metrics: Vec<&str> = FIGURES.iter().map(|q| q.split(':').nth(1).and_then(|m| m.split('{').next()).unwrap_or("")).collect();
        let mut times = Vec::new();
        let mut counted = alloc::Counts::default();
        let mut answers = 0;
        let mut scratch = Scratch::default();
        for i in 0..5 {
            let t = Instant::now();
            if i == 1 {
                alloc::begin();
            }
            let mut hosts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
            let mut sums: Vec<f64> = Vec::new();
            for m in &metrics {
                store.newest_by(m, "host", NOW - 600_000, &mut scratch, &mut |h, v| {
                    let at = match hosts.get(h) {
                        Some(&at) => at,
                        None => {
                            hosts.insert(h.to_owned(), sums.len());
                            sums.push(0.0);
                            sums.len() - 1
                        }
                    };
                    sums[at] += v;
                });
            }
            if i == 1 {
                counted = alloc::end();
            }
            times.push(t.elapsed());
            answers = hosts.len();
        }
        println!("{:<9} {:>9} {:<44} {:>8.2}ms {:>9} {:>9}", "", "", "the ten figures by newest samples", median(times).as_secs_f64() * 1e3, counted.calls(), answers);
    }
}
