//! DORA's ledger at a large kiln's size (`docs/observe.md` §10.7, §12.29): 200 services in two
//! environments, a deployment of each every 4 hours or so for 400 days (the ledger's whole keep), each
//! with commits read, one in fifteen rolled back to, one in thirty reverting an earlier one's commit.
//! What a deployment's coming costs the ledger (kept, in its place, what it marks found and marked);
//! the four keys over 30 days, every service's and one's; a list's search over every deployment; the
//! ledger read from its file as the server starts; and a deployment taken whole, its line written and
//! synced. The median of five passes; allocations of one.
//!
//! `cargo bench -p kiln --bench dora` (`KOBS_DORA_SERVICES` and `KOBS_DORA_DAYS` to change its size)
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use std::io::Write as _;
use std::time::Instant;

use hyper_measure::alloc;
use kiln::observe::dora::ledger::{self, Commit, DAY_NS, Deployment, Git, Ledger, NS_PER_S, Query};
use kiln::observe::dora::{Dora, Found};

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

const HOUR: i64 = 3_600 * NS_PER_S;

/// SplitMix64: the same history every run.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A commit's sha, as random as a real one's (a commit's number mixed).
fn sha(n: u64) -> String {
    let mut r = Rng(n);
    format!("{:016x}{:016x}{:08x}", r.next(), r.next(), r.next() >> 32)
}

/// This process's resident memory (bytes), where the system says it.
fn rss() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    let kb: u64 = s.lines().find_map(|l| l.strip_prefix("VmRSS:"))?.trim().trim_end_matches("kB").trim().parse().ok()?;
    Some(kb * 1024)
}

/// Every deployment of the history, in the order they finished, each with the commits it brings.
fn history(services: usize, days: i64, now: i64) -> Vec<(Deployment, Vec<Commit>)> {
    let mut rng = Rng(7);
    // A day inside the ledger's keep: none of it has aged out by the time it is read back.
    let start = now - (days - 1) * DAY_NS;
    let mut out = Vec::new();
    for s in 0..services {
        for env in ["prod", "staging"] {
            let mut t = start + rng.below(4 * HOUR as u64) as i64;
            let mut shas: Vec<u64> = Vec::new();
            let mut n = 0u64;
            while t < now {
                n += 1;
                let id = format!("bench-deploy-{s:04}-{env}-{n:06}");
                let (commit, commits) = if n > 3 && rng.below(15) == 0 {
                    // A rollback: an earlier commit again.
                    let back = shas[shas.len() - 3];
                    (back, Vec::new())
                } else {
                    let k = 1 + rng.below(6);
                    let mut c = Vec::new();
                    for j in 0..k {
                        let id = (s as u64) << 40 | n << 8 | j;
                        let title = if n > 10 && j == 0 && rng.below(30) == 0 { format!("Revert \"change {}\"", n - 5) } else { format!("change {n}") };
                        let reverts = title.starts_with("Revert").then(|| sha((s as u64) << 40 | (n - 5) << 8));
                        c.push(Commit { sha: sha(id), author_s: (t - (j as i64 + 1) * HOUR) / NS_PER_S, title, merge: false, reverts });
                    }
                    ((s as u64) << 40 | n << 8, c)
                };
                shas.push(commit);
                let d = Deployment {
                    id,
                    service: format!("service-{s:03}"),
                    env: env.into(),
                    team: format!("team-{}", s % 20),
                    version: format!("1.{n}"),
                    started_at: t - 120 * NS_PER_S,
                    finished_at: t,
                    git: Some(Git { commit_sha: sha(commit), repository_url: format!("https://github.com/acme/service-{s:03}") }),
                    custom_tags: Vec::new(),
                    source: "api".into(),
                    lead: None,
                    change_failure: None,
                    remedies: Vec::new(),
                    shas: Vec::new(),
                    titles: Vec::new(),
                };
                out.push((d, commits));
                t += 2 * HOUR + rng.below(4 * HOUR as u64) as i64;
            }
        }
    }
    out.sort_by_key(|(d, _)| d.finished_at);
    out
}

/// A deployment's coming, as the server takes it: kept, what it marks found and marked.
fn take(l: &mut Ledger, mut d: Deployment, commits: &[Commit]) -> usize {
    let (lead, _, shas, titles) = ledger::read_commits(d.finished_at, commits);
    (d.lead, d.shas, d.titles) = (lead, shas, titles);
    let id = d.id.clone();
    l.put(d);
    let mut marked = 0;
    for m in l.detect(&id, commits) {
        marked += usize::from(l.mark(&m.failed, m.found, Some((&id, m.kind))));
    }
    marked
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// `f` five times: ns a call (the median pass), allocations a call (the first).
fn time<R>(calls: usize, mut f: impl FnMut() -> R) -> (f64, f64) {
    let (mut allocs, mut times) = (0.0, Vec::new());
    for pass in 0..5 {
        if pass == 0 {
            alloc::begin();
        }
        let t = Instant::now();
        for _ in 0..calls {
            std::hint::black_box(f());
        }
        times.push(t.elapsed().as_nanos() as f64 / calls as f64);
        if pass == 0 {
            allocs = alloc::end().calls() as f64 / calls as f64;
        }
    }
    (median(times), allocs)
}

fn main() {
    let services: usize = std::env::var("KOBS_DORA_SERVICES").ok().and_then(|v| v.parse().ok()).unwrap_or(200);
    let days: i64 = std::env::var("KOBS_DORA_DAYS").ok().and_then(|v| v.parse().ok()).unwrap_or(400);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as i64;
    let all = history(services, days, now);
    let commits: usize = all.iter().map(|(_, c)| c.len()).sum();
    println!("{} deployments ({services} services, 2 environments, {days} days), {commits} commits", all.len());

    // Taken in turn: what a deployment's coming costs the ledger.
    let mut l = Ledger::default();
    let before = rss();
    alloc::begin();
    let t = Instant::now();
    let mut marked = 0;
    for (d, c) in &all {
        marked += take(&mut l, d.clone(), c);
    }
    let took = t.elapsed().as_nanos() as f64 / all.len() as f64;
    let a = alloc::end().calls() as f64 / all.len() as f64;
    let found = |f: Found| l.deployments().filter(|d| d.change_failure.as_ref().is_some_and(|cf| cf.found == f)).count();
    println!("taken: {took:.0} ns and {a:.1} allocations a deployment (its clone's included); {marked} marked: {} rollbacks, {} reverts", found(Found::Rollback), found(Found::Revert));
    if let (Some(b), Some(a)) = (before, rss()) {
        println!("held: {:.0} MB, {:.0} bytes a deployment", a.saturating_sub(b) as f64 / 1e6, a.saturating_sub(b) as f64 / all.len() as f64);
    }

    let month = Query { from: now - 30 * DAY_NS, to: now, ..Query::default() };
    let in_month = l.summary(&month).deployments;
    let (ns, a) = time(20, || l.summary(&month));
    println!("the four keys, 30 days, every service ({in_month} deployments): {:.2} ms, {a:.0} allocations", ns / 1e6);
    let one = Query { service: Some("service-017".into()), ..month.clone() };
    let (ns, a) = time(2_000, || l.summary(&one));
    println!("the four keys, 30 days, one service ({} deployments): {:.1} µs, {a:.0} allocations", l.summary(&one).deployments, ns / 1e3);
    let year = Query { from: now - 365 * DAY_NS, to: now, ..Query::default() };
    let (ns, _) = time(3, || l.summary(&year));
    println!("the four keys, a year, every service ({} deployments): {:.1} ms", l.summary(&year).deployments, ns / 1e6);
    let (ns, _) = time(20, || l.facets());
    println!("facets: {:.2} ms", ns / 1e6);
    let search = kiln_obs::logs::search::parse("service:service-017 @env:prod @change_failure:true").unwrap();
    let mut s = kiln_obs::logs::search::RowScratch::default();
    let (ns, _) = time(5, || l.deployments().filter(|d| search.fields(*d, &mut s)).count());
    println!("a list's search over every deployment ({}): {:.2} ms ({:.0} ns a deployment)", l.len().0, ns / 1e6, ns / l.len().0 as f64);

    // The ledger's file read as the server starts (a line each, as `Dora` writes them).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dora.jsonl");
    let mut f = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    for d in l.deployments() {
        writeln!(f, "{{\"deployment\":{}}}", serde_json::to_string(d).unwrap()).unwrap();
    }
    drop(f);
    let bytes = std::fs::metadata(&path).unwrap().len();
    let mut opened = Vec::new();
    for _ in 0..3 {
        let t = Instant::now();
        let d = Dora::open(dir.path(), None);
        opened.push(t.elapsed().as_secs_f64() * 1e3);
        assert_eq!(d.read(Ledger::len).0, l.len().0);
    }
    println!("read at start: {} lines, {:.1} MB: {:.0} ms", l.len().0, bytes as f64 / 1e6, median(opened));

    // A deployment taken whole: its line appended and synced.
    let fresh = tempfile::tempdir().unwrap();
    let dora = Dora::open(fresh.path(), None);
    let n = 500;
    let mine: Vec<Deployment> = all.iter().filter(|(d, _)| d.service == "service-000" && d.env == "prod").take(n).map(|(d, _)| d.clone()).collect();
    let taker = dora.clone();
    let took = kiln::rt::block_on(async move {
        let t = Instant::now();
        for d in mine {
            taker.deploy(d, false).await;
        }
        anyhow::Ok(t.elapsed())
    })
    .unwrap();
    println!("taken whole, its line synced: {:.2} ms a deployment ({} taken)", took.as_secs_f64() * 1e3 / n as f64, dora.read(Ledger::len).0);
}
