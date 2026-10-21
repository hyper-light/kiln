//! `kiln ci bench` (docs/ci.md W22-AC08): §1.5's performance targets, measured.
//!
//! - In this process: the scheduler (W05-AC08: 1,000 runners, 10,000 queued legs, then the
//!   completions that let each next one in), expressions (W01-AC07) and a 2,000-line workflow's
//!   parsing (W02-AC07).
//! - Against a hub of its own (an in-memory store, the CI engine running, no runners or HTTP):
//!   run creation (W03-AC09: 1,000 pushes, each from its event to its run in the store, git
//!   stubbed by workflows already read), log ingest (§1.5: 16 legs' output at once, and a line's way
//!   to a live viewer) and the cache's store (W08-AC08: a 1 GiB entry saved, then restored). The
//!   network's part of those (the runner's link, HTTP, the browser) is the live cluster's to
//!   confirm, as §1.5 says.
//! - Against a store on disk of its own: the runs list's first page with a million runs in it
//!   (W21-AC04's server time).
//! - Each measure is the median of `--runs` runs, against its target. They're written to
//!   `target/ci-bench.json` (`--out`), and, in a leg, to `$KILN_METRICS` with their runs as
//!   samples, where a job's `bench-gate` holds them to their history (W16-R09): kiln's own CI
//!   fails a result more than 10 % worse than its baseline, by W16's interval rule.
//! - It exits 1 when a target is missed. Debug builds measure, but only release builds are
//!   held to the targets.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use kiln_ci::expr::{Budget, Dialect, Expr, MapContexts, Value, eval};
use kiln_ci::facts::RunnerFacts;
use kiln_ci::results::metrics::Point;
use kiln_ci::sched::routing::{LegRoute, RunnerView};
use kiln_ci::sched::{Command, Config, Input, LegIn, Outcome, RunnerIn, Scheduler};
use kiln_ci::stats::Better;
use serde::Serialize;

use crate::hub::Hub;

/// `kiln ci …`.
#[derive(clap::Args)]
pub struct CiArgs {
    #[command(subcommand)]
    cmd: CiCmd,
}

#[derive(clap::Subcommand)]
enum CiCmd {
    /// §1.5's performance targets, measured (docs/ci.md W22-AC08): the scheduler, expressions
    /// and parsing; written to `target/ci-bench.json` and `$KILN_METRICS`; exit 1 on a miss.
    Bench(BenchArgs),
}

#[derive(clap::Args)]
pub struct BenchArgs {
    /// The scheduler's alone (W05-AC08).
    #[arg(long)]
    scheduler: bool,
    /// Only these: `scheduler`, `expressions`, `parsing`, `runs`, `logs`, `cache`, `list`, `restart` (comma-separated).
    #[arg(long, value_delimiter = ',')]
    only: Vec<String>,
    /// Pushes for run creation.
    #[arg(long, default_value_t = 1_000)]
    pushes: usize,
    /// MiB of log output the 16 legs write.
    #[arg(long, default_value_t = 1_024)]
    log_mib: u64,
    /// MiB of the cache entry.
    #[arg(long, default_value_t = 1_024)]
    cache_mib: u64,
    /// Runs in the store the runs list's first page is read from (W21-AC04).
    #[arg(long, default_value_t = 1_000_000)]
    list_runs: usize,
    /// Queued legs rebuilt after a restart (W05-AC09).
    #[arg(long, default_value_t = 10_000)]
    queued_legs: usize,
    /// Runs of each measure; its median is the result.
    #[arg(long, default_value_t = 5)]
    runs: usize,
    #[arg(long, default_value = "target/ci-bench.json")]
    out: PathBuf,
    /// Where the hub's measures keep their data while they run (on disk: a few GiB), removed after.
    #[arg(long, default_value = "target/ci-bench-data")]
    data: PathBuf,
}

/// One measure against its target.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Measure {
    /// `ci.bench.<what>`, as `$KILN_METRICS` names it.
    pub name: String,
    pub value: f64,
    pub unit: String,
    pub better: Better,
    pub target: f64,
    pub ok: bool,
    pub samples: Vec<f64>,
}

impl Measure {
    fn new(name: &str, unit: &str, better: Better, target: f64, mut samples: Vec<f64>) -> Measure {
        samples.retain(|s| s.is_finite());
        let value = median(&samples);
        let ok = match better {
            Better::Higher => value >= target,
            Better::Lower => value <= target,
        };
        Measure { name: format!("ci.bench.{name}"), value, unit: unit.to_owned(), better, target, ok, samples }
    }

    fn point(&self) -> Point {
        Point { name: self.name.clone(), value: self.value, unit: self.unit.clone(), better: self.better, tags: BTreeMap::new(), samples: self.samples.clone() }
    }
}

fn median(xs: &[f64]) -> f64 {
    let mut v = xs.to_vec();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    match n {
        0 => f64::NAN,
        _ if n % 2 == 1 => v.get(n / 2).copied().unwrap_or(f64::NAN),
        _ => (v.get(n / 2 - 1).copied().unwrap_or(f64::NAN) + v.get(n / 2).copied().unwrap_or(f64::NAN)) / 2.0,
    }
}

/// One run of W05-AC08's load: (assignments a second, an input's p99 and mean, in ms).
fn scheduler_once() -> (f64, f64, f64) {
    let mut s = Scheduler::new(Config::default());
    let facts = RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() };
    for i in 0..1_000 {
        let r = RunnerIn {
            id: format!("r{i}"),
            view: RunnerView { name: format!("r{i}"), labels: vec![format!("pool-{}", i % 50)], facts: facts.clone(), caps: 0, pool: None },
            slots: 4,
            capacity: None,
            executor: None,
        };
        s.handle(Input::RunnerUp(Box::new(r)), 0);
    }
    let mut lat: Vec<f64> = Vec::with_capacity(20_000);
    let mut running: VecDeque<String> = VecDeque::new();
    let mut assigned = 0usize;
    let mut handle = |s: &mut Scheduler, i: Input, now: i64, running: &mut VecDeque<String>| {
        let t = Instant::now();
        let out = s.handle(i, now);
        lat.push(t.elapsed().as_secs_f64() * 1000.0);
        for c in out {
            if let Command::Assign { leg, .. } = c {
                assigned += 1;
                running.push_back(leg);
            }
        }
    };
    let start = Instant::now();
    for j in 0..10_000 {
        let l = LegIn { id: format!("l{j}"), run: format!("run{}", j / 10), repo: format!("acme/r{}", j % 7), route: LegRoute { labels: vec![format!("pool-{}", j % 50)], ..LegRoute::default() }, priority: 60, ..LegIn::default() };
        handle(&mut s, Input::LegQueued(Box::new(l)), 1, &mut running);
    }
    let mut now = 2;
    while let Some(leg) = running.pop_front() {
        now += 1;
        handle(&mut s, Input::AttemptFinished { leg, attempt: 1, outcome: Outcome::Success }, now, &mut running);
    }
    let secs = start.elapsed().as_secs_f64();
    lat.sort_by(f64::total_cmp);
    let mean = lat.iter().sum::<f64>() / lat.len().max(1) as f64;
    (assigned as f64 / secs, lat.get(lat.len() * 99 / 100).copied().unwrap_or(f64::NAN), mean)
}

/// W05-AC08 and §1.5's scheduler rows.
fn scheduler(runs: usize) -> Vec<Measure> {
    let all: Vec<(f64, f64, f64)> = (0..runs).map(|_| scheduler_once()).collect();
    vec![
        Measure::new("scheduler.assignments_per_second", "1/s", Better::Higher, 2_000.0, all.iter().map(|r| r.0).collect()),
        Measure::new("scheduler.input_p99", "ms", Better::Lower, 50.0, all.iter().map(|r| r.1).collect()),
        Measure::new("scheduler.input_mean", "ms", Better::Lower, 5.0, all.iter().map(|r| r.2).collect()),
    ]
}

/// W01-AC07: simple evaluations a second, on one core.
fn expressions(runs: usize) -> Measure {
    let github = Value::object(vec![("event_name".to_owned(), Value::str("push")), ("ref".to_owned(), Value::str("refs/heads/main"))]);
    let cx = MapContexts::default().with("github", github);
    let samples = match Expr::parse("github.event_name == 'push' && contains(github.ref, 'refs/heads/')", Dialect::Gha) {
        Ok(e) => (0..runs)
            .map(|_| {
                let n = 500_000u32;
                let start = Instant::now();
                for _ in 0..n {
                    let mut b = Budget::default();
                    let _ = eval(&e, &cx, &mut b);
                }
                f64::from(n) / start.elapsed().as_secs_f64()
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    Measure::new("expressions.evaluations_per_second", "1/s", Better::Higher, 1_000_000.0, samples)
}

/// W02-AC07: a 2,000-line workflow parsed and checked.
fn parsing(runs: usize) -> Measure {
    let mut src = String::from("on: [push, pull_request]\njobs:\n");
    while src.lines().count() < 2_000 {
        let n = src.lines().count();
        src.push_str(&format!(
            "  job{n}:\n    runs-on: ubuntu-latest\n    needs: []\n    if: github.event_name == 'push' && contains(github.ref, 'main')\n    steps:\n      - uses: actions/checkout@v4\n      - run: echo ${{{{ github.sha }}}} ${{{{ matrix.os }}}}\n        env: {{ A: '${{{{ github.ref }}}}' }}\n"
        ));
    }
    let samples = (0..runs)
        .map(|_| {
            let start = Instant::now();
            let _ = kiln_ci::wf::parse_workflow(".github/workflows/big.yml", &src, Dialect::Gha);
            start.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    Measure::new("parsing.workflow_2000_lines", "ms", Better::Lower, 20.0, samples)
}

/// A hub of the bench's own, in `dir`: an in-memory store, the CI engine running, no runners and
/// no HTTP.
fn bench_hub(dir: &Path) -> Result<Arc<Hub>> {
    bench_hub_on(dir, crate::store::Store::in_memory()?)
}

/// [`bench_hub`] on `store`.
fn bench_hub_on(dir: &Path, store: crate::store::Store) -> Result<Arc<Hub>> {
    let mut cfg = crate::config::Config::default();
    cfg.server.data_dir = dir.to_path_buf();
    cfg.terminal.enabled = false;
    cfg.ci.enabled = true;
    let pki = Arc::new(crate::pki::Pki::open(&dir.join("pki"))?);
    let leaf = pki.server_leaf(&[])?;
    let (net, _events) = crate::net::start(crate::net::Spec { bind: "127.0.0.1:0".parse()?, ca_der: pki.ca_der.clone(), leaf, server: true, dial_deadline: crate::net::DIAL_DEADLINE })?;
    let logs = crate::logs::Logs::new(dir.join("logs"), 1 << 20);
    let hub = Hub::new(Arc::new(cfg), store, logs, net, pki, crate::util::random_token(), crate::util::random_token(), crate::rt::Shutdown::new());
    crate::ci::dispatch::spawn(Arc::clone(&hub));
    Ok(hub)
}

/// The bench's repository, `name`, native, its trunk `main`.
async fn bench_repo(hub: &Arc<Hub>, name: &str) -> Result<crate::store::Repo> {
    hub.store
        .upsert_repo(crate::store::Repo {
            id: crate::util::new_id(),
            name: name.into(),
            clone_url: String::new(),
            branches: vec!["main".into()],
            poll_seconds: 0,
            enabled: true,
            created_at: crate::util::now_unix(),
            last_polled_at: None,
            last_error: None,
            mode: "native".into(),
            default_branch: Some("main".into()),
            mirror: None,
        })
        .await
}

/// W03-AC09: `pushes` pushes, each from its event to its run created (its id told back once the
/// run is in the store), the workflow already read (git stubbed): the p99 (≤ 500 ms) and mean.
async fn runs(hub: &Arc<Hub>, pushes: usize) -> Result<Vec<Measure>> {
    use kiln_ci::trigger::Event;
    let repo = bench_repo(hub, "bench/app").await?;
    let path = ".github/workflows/bench.yml";
    let text = "name: bench
on: push
jobs:
  build:
    runs-on: [self-hosted]
    steps:
      - run: echo built
  test:
    needs: build
    runs-on: [self-hosted]
    strategy:
      matrix:
        n: [1, 2, 3]
    steps:
      - run: echo test ${{ matrix.n }}
";
    let (workflow, _) = kiln_ci::wf::parse_workflow(path, text, Dialect::Gha);
    anyhow::ensure!(workflow.is_some(), "the bench's workflow didn't read");
    let loaded = crate::ci::load::Loaded { path: path.into(), dialect: Dialect::Gha, runs: true, workflow, diagnostics: Vec::new() };
    let mut lat = Vec::with_capacity(pushes);
    for i in 0..pushes {
        let event = Event { name: "push".into(), repo: repo.name.clone(), git_ref: "refs/heads/main".into(), sha: format!("{:040x}", i + 1), actor: "bench".into(), trusted: true, payload: serde_json::json!({}), ..Event::default() };
        let (tx, rx) = async_channel::bounded(1);
        let given = crate::ci::dispatch::Given { reply: Some(tx), ..crate::ci::dispatch::Given::default() };
        let t = Instant::now();
        crate::ci::events::start_given(hub, &repo, &event, &loaded, ("bench", "bench"), serde_json::Value::Null, given).await?;
        crate::rt::timeout(Duration::from_secs(30), rx.recv()).await.map_err(|_| anyhow::anyhow!("push {i}: no run within 30 s"))??;
        lat.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    lat.sort_by(f64::total_cmp);
    let p99 = lat.get(lat.len() * 99 / 100).copied().unwrap_or(f64::NAN);
    Ok(vec![Measure::new("runs.created_p99", "ms", Better::Lower, 500.0, vec![p99])])
}

/// W05-AC09 (§1.5): a server's queue rebuilt after a restart. One hub, on a store on disk, queues a
/// run of ten jobs of `legs / 10` legs each for runners there aren't; a second on that store, as
/// the server is when it starts again, replays it. Its dispatcher's rebuild: ≤ 2 s.
async fn restart(dir: &Path, legs: usize) -> Result<Vec<Measure>> {
    use kiln_ci::trigger::Event;
    let db = dir.join("restart.db");
    let per = (legs / 10).clamp(1, 1_000);
    let first = bench_hub_on(&dir.join("one"), crate::store::Store::open(&db)?)?;
    let queued = async {
        let repo = bench_repo(&first, "bench/queue").await?;
        let axis = (0..per).map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
        let jobs: String = (0..10).map(|j| format!("  q{j}:\n    runs-on: nowhere\n    matrix: {{ i: [{axis}] }}\n    steps:\n      - run: echo never\n")).collect();
        let (path, text) = (".kiln/queue.yaml", format!("name: queue\non: push\njobs:\n{jobs}"));
        let (workflow, _) = kiln_ci::wf::parse_workflow(path, &text, Dialect::Kiln);
        anyhow::ensure!(workflow.is_some(), "the bench's queue didn't read");
        let loaded = crate::ci::load::Loaded { path: path.into(), dialect: Dialect::Kiln, runs: true, workflow, diagnostics: Vec::new() };
        let event = Event {
            name: "push".into(),
            repo: repo.name.clone(),
            git_ref: "refs/heads/main".into(),
            sha: format!("{:040x}", 1),
            actor: "bench".into(),
            trusted: true,
            payload: serde_json::json!({}),
            ..Event::default()
        };
        let (tx, rx) = async_channel::bounded(1);
        let given = crate::ci::dispatch::Given { reply: Some(tx), ..crate::ci::dispatch::Given::default() };
        crate::ci::events::start_given(&first, &repo, &event, &loaded, ("bench", "bench"), serde_json::Value::Null, given).await?;
        let run = crate::rt::timeout(Duration::from_secs(60), rx.recv()).await.map_err(|_| anyhow::anyhow!("no run within a minute"))??;
        // Every leg in the store, queued.
        let want = per * 10;
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            let r = run.clone();
            let n: i64 = first.store.call(move |c| Ok(c.query_row("SELECT COUNT(*) FROM jobs WHERE run_id = ?1 AND status = 'queued'", [r], |x| x.get(0))?)).await?;
            if usize::try_from(n).is_ok_and(|n| n >= want) {
                return anyhow::Ok(want);
            }
            anyhow::ensure!(Instant::now() < deadline, "{n} of {want} legs queued within five minutes");
            crate::rt::sleep(Duration::from_millis(200)).await;
        }
    }
    .await;
    first.shutdown.cancel();
    let want = queued?;
    let second = bench_hub_on(&dir.join("two"), crate::store::Store::open(&db)?)?;
    let deadline = Instant::now() + Duration::from_secs(120);
    let restored = loop {
        if let Some(r) = second.ci.restored() {
            break r;
        }
        if Instant::now() > deadline {
            second.shutdown.cancel();
            anyhow::bail!("the second hub didn't rebuild its runs within two minutes");
        }
        crate::rt::sleep(Duration::from_millis(5)).await;
    };
    second.shutdown.cancel();
    anyhow::ensure!(restored.legs >= want, "{} of {want} legs rebuilt", restored.legs);
    Ok(vec![Measure::new("restart.rebuild_ms", "ms", Better::Lower, 2_000.0, vec![restored.took.as_secs_f64() * 1000.0])])
}

/// W21-AC04: the runs list's first page with `n` runs in a store on disk (twenty repositories, a
/// leg each), as `GET /api/runs` makes it (the store's query, the legs' runners and executors, the
/// JSON): newest first as the list opens, one repository's, and oldest first. Each ≤ 100 ms.
async fn list(dir: &Path, n: usize, runs: usize) -> Result<Vec<Measure>> {
    let store = crate::store::Store::open(&dir.join("list.db"))?;
    let mut repos = Vec::new();
    for i in 0..20 {
        let r = crate::store::Repo {
            id: crate::util::new_id(),
            name: format!("bench/app{i}"),
            clone_url: String::new(),
            branches: vec!["main".into()],
            poll_seconds: 0,
            enabled: true,
            created_at: 0,
            last_polled_at: None,
            last_error: None,
            mode: "native".into(),
            default_branch: Some("main".into()),
            mirror: None,
        };
        repos.push(store.upsert_repo(r).await?.id);
    }
    let ids = repos.clone();
    store
        .call(move |c| {
            let tx = c.transaction()?;
            {
                let mut run = tx.prepare(
                    "INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, message, author, status, created_at, started_at, finished_at)
                     VALUES (?1, ?2, ?3, '.github/workflows/ci.yml', 'push', 'main', ?4, 'A change', 'bench', 'success', ?5, ?5, ?6)",
                )?;
                let mut leg = tx.prepare(
                    "INSERT INTO jobs (id, run_id, key, display, status, labels, needs, spec, agent, executor) VALUES (?1, ?2, 'build', 'build', 'success', '[]', '[]', '{}', 'runner-1', 'host')",
                )?;
                let base = 1_700_000_000i64;
                for i in 0..n {
                    let repo = ids.get(i % ids.len()).cloned().unwrap_or_default();
                    let (at, number) = (base + i64::try_from(i).unwrap_or(0), i64::try_from(i / ids.len()).unwrap_or(0) + 1);
                    let id = format!("run{i:09}");
                    run.execute(rusqlite::params![id, repo, number, format!("{i:040x}"), at, at + 60])?;
                    leg.execute(rusqlite::params![format!("leg{i:09}"), id])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await?;
    let one = repos.get(7).cloned();
    let mut out = Vec::new();
    for (name, repo, sort) in [("list.first_page", None, None), ("list.first_page_repo", one, None), ("list.first_page_oldest", None, Some("oldest"))] {
        let mut samples = Vec::with_capacity(runs);
        for _ in 0..runs {
            let t = Instant::now();
            let page = match sort {
                Some(s) => store.runs_sorted(None, repo.clone(), s, 50, 0).await?,
                None => store.runs(repo.clone(), 50, None).await?,
            };
            let hosts = store.run_hosts(page.iter().map(|r| r.id.clone()).collect()).await?;
            let body = serde_json::to_vec(&page)?;
            anyhow::ensure!(page.len() == 50 && hosts.len() == 50 && !body.is_empty(), "{name}: {} runs, {} with their legs' hosts", page.len(), hosts.len());
            samples.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        out.push(Measure::new(name, "ms", Better::Lower, 100.0, samples));
    }
    Ok(out)
}

/// §1.5's log ingest: 16 legs writing `mib` MiB at once (≥ 200 MiB/s together), and a line's
/// way from its write to a live viewer (p99 < 250 ms; here the server's part of it).
async fn logs(dir: &Path, mib: u64) -> Result<Vec<Measure>> {
    let logs = crate::logs::Logs::new(dir.join("bench-logs"), u64::MAX);
    let line = "2026-10-19T08:00:00.000Z   Compiling kiln v0.2.0 (/work/kiln/crates/kiln): the quick brown fox\n";
    let chunk: Vec<u8> = line.as_bytes().iter().copied().cycle().take(64 << 10).collect();
    let per_leg = (mib.max(16) << 20) / 16 / (64 << 10);
    let start = Instant::now();
    std::thread::scope(|s| {
        for leg in 0..16 {
            let (logs, chunk) = (&logs, &chunk);
            s.spawn(move || {
                let job = format!("bench-leg-{leg}");
                for _ in 0..per_leg {
                    logs.append(&job, 0, chunk);
                }
                logs.finish(&job);
            });
        }
    });
    let secs = start.elapsed().as_secs_f64();
    let wrote = (per_leg * 16 * (64 << 10)) as f64 / f64::from(1u32 << 20);
    // A live viewer's lines: each timed from its write to its arrival.
    let mut rx = logs.subscribe("bench-live");
    let writer = {
        let logs = logs.clone();
        std::thread::spawn(move || {
            let epoch = Instant::now();
            for _ in 0..1_000 {
                logs.append("bench-live", 0, format!("{}\n", epoch.elapsed().as_nanos()).as_bytes());
                std::thread::sleep(Duration::from_millis(1));
            }
            epoch
        })
    };
    let mut seen: Vec<(u128, Instant)> = Vec::with_capacity(1_000);
    while seen.len() < 1_000 {
        match crate::rt::timeout(Duration::from_secs(10), rx.recv()).await {
            Ok(Ok(crate::logs::LogEvent::Data { data, .. })) => {
                let at = Instant::now();
                for l in String::from_utf8_lossy(&data).lines() {
                    if let Ok(n) = l.trim().parse::<u128>() {
                        seen.push((n, at));
                    }
                }
            }
            Ok(Ok(_)) => {}
            _ => break,
        }
    }
    let epoch = writer.join().map_err(|_| anyhow::anyhow!("the live writer panicked"))?;
    let mut lat: Vec<f64> = seen.iter().map(|(n, at)| (at.duration_since(epoch).as_nanos().saturating_sub(*n)) as f64 / 1e6).collect();
    lat.sort_by(f64::total_cmp);
    let p99 = lat.get(lat.len() * 99 / 100).copied().unwrap_or(f64::NAN);
    let _ = std::fs::remove_dir_all(dir.join("bench-logs"));
    Ok(vec![Measure::new("logs.ingest", "MiB/s", Better::Higher, 200.0, vec![wrote / secs]), Measure::new("logs.live_line_p99", "ms", Better::Lower, 250.0, vec![p99])])
}

/// W08-AC08's store half: a `mib` MiB entry put into the cache's content store, then restored
/// from it, streamed as a restore streams it (≥ 500 MiB/s).
async fn cache(hub: &Arc<Hub>, dir: &Path, mib: u64) -> Result<Vec<Measure>> {
    let file = dir.join("bench-cache.bin");
    // Not one repeated byte: chunking and hashing see data like a real entry's.
    let f = file.clone();
    crate::rt::blocking(move || -> std::io::Result<()> {
        let mut out = std::io::BufWriter::new(std::fs::File::create(&f)?);
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut block = vec![0u8; 1 << 20];
        for _ in 0..mib.max(1) {
            for b in block.chunks_mut(8) {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let n = b.len();
                b.copy_from_slice(x.to_le_bytes().get(..n).unwrap_or_default());
            }
            out.write_all(&block)?;
        }
        out.flush()
    })
    .await??;
    let t = Instant::now();
    let obj = crate::ci::services::cas::put_file(hub, &file).await?;
    let saved = mib as f64 / t.elapsed().as_secs_f64();
    let t = Instant::now();
    let rx = crate::ci::services::cas::read(hub, &obj, 0, obj.size).await;
    let mut got = 0u64;
    while let Ok(b) = rx.recv().await {
        got += b.len() as u64;
    }
    let restored = got as f64 / f64::from(1u32 << 20) / t.elapsed().as_secs_f64();
    anyhow::ensure!(got == obj.size, "restored {got} of {} bytes", obj.size);
    let _ = std::fs::remove_file(&file);
    Ok(vec![Measure::new("cache.restore", "MiB/s", Better::Higher, 500.0, vec![restored]), Measure::new("cache.save", "MiB/s", Better::Higher, 0.0, vec![saved])])
}

/// What a run of the bench found.
#[derive(Debug, Serialize)]
struct Report {
    kiln: &'static str,
    /// `release` or `debug`: only a release build is held to the targets.
    profile: &'static str,
    runs: usize,
    measures: Vec<Measure>,
}

/// `kiln ci …`; its exit code.
pub async fn run(args: CiArgs) -> Result<u8> {
    match args.cmd {
        CiCmd::Bench(a) => bench(a).await,
    }
}

async fn bench(a: BenchArgs) -> Result<u8> {
    let mut only: Vec<String> = a.only.iter().map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()).collect();
    if a.scheduler {
        only.push("scheduler".into());
    }
    for o in &only {
        anyhow::ensure!(
            matches!(o.as_str(), "scheduler" | "expressions" | "parsing" | "runs" | "logs" | "cache" | "list" | "restart"),
            "`{o}`: scheduler, expressions, parsing, runs, logs, cache, list or restart"
        );
    }
    let want = |name: &str| only.is_empty() || only.iter().any(|o| o == name);
    let runs = a.runs.clamp(1, 100);
    let mut measures = Vec::new();
    if want("scheduler") {
        measures.extend(scheduler(runs));
    }
    if want("expressions") {
        measures.push(expressions(runs));
    }
    if want("parsing") {
        measures.push(parsing(runs));
    }
    if want("runs") || want("logs") || want("cache") {
        let dir = a.data.join(crate::util::new_id());
        std::fs::create_dir_all(&dir)?;
        let hub = bench_hub(&dir)?;
        let measured = async {
            let mut m = Vec::new();
            if want("runs") {
                m.extend(self::runs(&hub, a.pushes.max(1)).await?);
            }
            if want("logs") {
                m.extend(logs(&dir, a.log_mib).await?);
            }
            if want("cache") {
                m.extend(cache(&hub, &dir, a.cache_mib).await?);
            }
            anyhow::Ok(m)
        }
        .await;
        hub.shutdown.cancel();
        let _ = std::fs::remove_dir_all(&dir);
        measures.extend(measured?);
    }
    if want("list") {
        let dir = a.data.join(crate::util::new_id());
        let measured = list(&dir, a.list_runs.max(100), runs).await;
        let _ = std::fs::remove_dir_all(&dir);
        measures.extend(measured?);
    }
    if want("restart") {
        let dir = a.data.join(crate::util::new_id());
        std::fs::create_dir_all(&dir)?;
        let measured = restart(&dir, a.queued_legs.max(10)).await;
        let _ = std::fs::remove_dir_all(&dir);
        measures.extend(measured?);
    }
    let release = !cfg!(debug_assertions);
    let report = Report { kiln: env!("CARGO_PKG_VERSION"), profile: if release { "release" } else { "debug" }, runs, measures };
    for m in &report.measures {
        println!("{} {:<45} {:>14.3} {:<4} (target {} {})", if m.ok { "ok  " } else { "MISS" }, m.name, m.value, m.unit, if m.better == Better::Higher { "≥" } else { "≤" }, m.target);
    }
    if let Some(dir) = a.out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&a.out, serde_json::to_vec_pretty(&report)?)?;
    eprintln!("written to {}", a.out.display());
    // In a leg: each measure, with its runs, for the job's bench-gate (W16-R09).
    if let Some(file) = std::env::var_os("KILN_METRICS").filter(|v| !v.is_empty()) {
        let lines: String = report.measures.iter().filter_map(|m| serde_json::to_string(&m.point()).ok()).map(|l| l + "\n").collect();
        std::fs::OpenOptions::new().create(true).append(true).open(&file)?.write_all(lines.as_bytes())?;
    }
    let missed = report.measures.iter().filter(|m| !m.ok).count();
    if !release {
        eprintln!("a debug build: measured, not held to the targets");
        return Ok(0);
    }
    Ok(if missed > 0 { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w22_ac08_measures() {
        let m = Measure::new("x", "ms", Better::Lower, 20.0, vec![30.0, 10.0, 12.0]);
        assert_eq!((m.name.as_str(), m.value, m.ok), ("ci.bench.x", 12.0, true));
        let m = Measure::new("y", "1/s", Better::Higher, 100.0, vec![50.0, 60.0]);
        assert_eq!((m.value, m.ok), (55.0, false));
        let p = m.point();
        assert_eq!((p.name.as_str(), p.better, p.samples.len()), ("ci.bench.y", Better::Higher, 2));
        assert!(!Measure::new("z", "ms", Better::Lower, 1.0, Vec::new()).ok, "nothing measured misses");
        let p = parsing(1);
        assert!(p.value > 0.0 && p.samples.len() == 1, "{p:?}");
    }
}
