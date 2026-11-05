//! `kiln-lab`: kiln's load lab (docs/runtime-lab.md; goal 1 of the API runtime program, plan
//! 01a1a59c2a68 steps 3 to 5). It starts a given build of kiln (a server and its runners), replays
//! realistic use against it, with CPU hogs beside it when asked, and reports each class's latency
//! (median, p99, p99.9 and the most), the shards' delay, throughput and the server's CPU for each
//! request, with the noise it measured across rounds. `ab` runs two builds interleaved, round by
//! round in turn (A B, B A, ...), and says of each number whether B differs from A beyond noise.
//!
//! ```text
//! kiln-lab run --kiln target/release/kiln --rounds 8 [--hogs 1x] [--out DIR]
//! kiln-lab ab --a main/kiln --b branch/kiln --rounds 8 [--hogs 2x]
//! kiln-lab run --kiln kiln --server-cpus 16-31 --layout production --hogs 1x   # the lab on its own CPUs
//! kiln-lab compare first/run.json second/run.json
//! kiln-lab report DIR/run.json [--quantum-us 266.666]
//! ```

mod client;
mod fleet;
mod lab;
mod measure;
mod report;
mod standin;
mod stats;

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};

use crate::lab::{Build, Profile};
use crate::report::{Comparison, Run};

#[derive(Parser)]
#[command(name = "kiln-lab", about = "kiln's load lab: realistic use against a build, and A/B between two")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// What a run takes besides its builds.
#[derive(clap::Args, Clone)]
struct Common {
    /// Rounds for each build: each a fresh kiln, filled, then measured.
    #[arg(long, default_value_t = 8)]
    rounds: usize,
    /// CPU hogs beside the server: a count, or a multiple of the lab's CPUs (`1x`, `2x`), or of the
    /// server's (`--server-cpus`).
    #[arg(long, default_value = "0")]
    hogs: String,
    /// The CPUs the server and the hogs run on (Linux, `taskset`'s list: `16-31`), so the lab's clients,
    /// runners and stand-ins keep the CPUs the lab was started on to themselves and it times the
    /// server, not itself (goal 10, plan 01a1a87b2dae step 1). Everything on the lab's CPUs otherwise.
    #[arg(long)]
    server_cpus: Option<String>,
    /// `shared`: the hogs beside everything in the lab's own cgroup; `production`: the server in a
    /// scope of its own at `CPUWeight=1000` and the hogs in one at `CPUWeight=1`, as kiln.slice and
    /// CI legs and agents run on bare metal (docs/priority.md; Linux with systemd's user manager).
    #[arg(long, default_value = "shared")]
    layout: String,
    /// `realistic` (production's shape at a busy hour), `overload` (that, with no think time), `flood` (that, with
    /// 64 readers and 8 agents calling MCP), `many` (all of production's live sessions at work at once) or `small`
    /// (to try the lab out).
    #[arg(long, default_value = "realistic")]
    profile: String,
    /// Seconds each round measures (the profile's own otherwise).
    #[arg(long)]
    window: Option<u64>,
    /// Where the run's report and its rounds go (`~/.cache/kiln-lab/<when>` otherwise).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Keep each round's kiln (its data, its logs) rather than removing it.
    #[arg(long)]
    keep: bool,
    /// The server's `[bus] memory` in bytes, written into the kiln.toml each round's server starts
    /// with (kiln's default otherwise, 256 MiB): so the lab measures the bus past it (goal 11, plan
    /// 01a1a87b2e56 step 3).
    #[arg(long)]
    bus_memory: Option<u64>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Measure one build.
    Run {
        /// The build's `kiln` binary.
        #[arg(long)]
        kiln: PathBuf,
        /// What to call it in the report.
        #[arg(long, default_value = "build")]
        label: String,
        /// `KEY=VALUE` for its server's environment (repeatable).
        #[arg(long = "env")]
        env: Vec<String>,
        #[command(flatten)]
        common: Common,
    },
    /// Measure two builds, interleaved, and say how B differs from A.
    Ab {
        #[arg(long)]
        a: PathBuf,
        #[arg(long)]
        b: PathBuf,
        #[arg(long, default_value = "A")]
        label_a: String,
        #[arg(long, default_value = "B")]
        label_b: String,
        /// `KEY=VALUE` for A's server's environment (repeatable).
        #[arg(long = "env-a")]
        env_a: Vec<String>,
        /// `KEY=VALUE` for B's server's environment (repeatable).
        #[arg(long = "env-b")]
        env_b: Vec<String>,
        #[command(flatten)]
        common: Common,
    },
    /// Say how a second run of one build differs from a first, number by number.
    Compare {
        first: PathBuf,
        second: PathBuf,
        /// Which build of each run (its first otherwise).
        #[arg(long)]
        build: Option<String>,
    },
    /// A run's report, in Markdown.
    Report {
        run: PathBuf,
        /// The step quantum to hold what held the shards to, µs, in place of the one each round's build said
        /// (goal 9, plan 01a1a87b2cec step 3: a build from before it said, or a quantum it didn't run with).
        #[arg(long)]
        quantum_us: Option<f64>,
    },
    /// Runs side by side: each number's median and noise in each (a baseline's table).
    Table {
        /// `label=path/run.json`, or a path (labelled by its folder).
        runs: Vec<String>,
    },
    /// A CPU hog (the lab starts these itself): `threads` threads, each spinning.
    #[command(hide = true)]
    Hog {
        #[arg(long, default_value_t = 1)]
        threads: usize,
    },
}

/// Seconds since the epoch as a UTC time (`2026-11-04T08:30:00Z`).
fn utc(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Hogs `spec` asks for: a count, or a multiple of `cpus`.
fn hogs(spec: &str, cpus: usize) -> anyhow::Result<usize> {
    match spec.strip_suffix('x') {
        Some(k) => Ok(cpus.saturating_mul(k.parse::<usize>()?)),
        None => Ok(spec.parse()?),
    }
}

fn env(pairs: &[String]) -> anyhow::Result<Vec<(String, String)>> {
    pairs.iter().map(|p| p.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned())).ok_or_else(|| anyhow::anyhow!("--env takes KEY=VALUE, not {p}"))).collect()
}

fn build(label: &str, kiln: &Path, env: Vec<(String, String)>, c: &Common) -> anyhow::Result<Build> {
    let kiln = std::path::absolute(kiln)?;
    anyhow::ensure!(kiln.is_file(), "no kiln at {}", kiln.display());
    Ok(Build { label: label.to_owned(), kiln, env, bus_memory: c.bus_memory })
}

/// Runs `builds` in turn, `c.rounds` each (A B, then B A, ...), writing the run as it goes.
fn measure(builds: &[Build], c: &Common) -> anyhow::Result<Run> {
    let started_secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let out = match &c.out {
        Some(o) => o.clone(),
        None => PathBuf::from(std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("no HOME"))?).join(".cache/kiln-lab").join(utc(started_secs).replace(':', "")),
    };
    std::fs::create_dir_all(&out)?;
    let mut profile = match c.profile.as_str() {
        "small" => Profile::small(),
        "realistic" => Profile::realistic(),
        "overload" => Profile::overload(),
        "many" => Profile::many(),
        "flood" => Profile::flood(),
        p => anyhow::bail!("no profile {p}: realistic, overload, flood, many or small"),
    };
    if let Some(w) = c.window {
        profile.window_s = w;
    }
    let place = lab::Place::of(c.server_cpus.as_deref(), &c.layout)?;
    let cpus = place.cpus.as_deref().map_or_else(|| std::thread::available_parallelism().map_or(1, std::num::NonZero::get), |list| lab::cpus_in(list).len().max(1));
    let hogs = hogs(&c.hogs, cpus)?;
    let bin = lab::stand_in(&out)?;
    let mut run = Run {
        command: std::env::args().collect::<Vec<_>>().join(" "),
        started: utc(started_secs),
        profile: profile.clone(),
        hogs,
        layout: place.layout.clone(),
        server_cpus: place.cpus.clone(),
        rounds: Vec::new(),
        failures: Vec::new(),
    };
    let mut index = 0;
    for i in 0..c.rounds {
        let order: Vec<&Build> = if i % 2 == 0 { builds.iter().collect() } else { builds.iter().rev().collect() };
        for b in order {
            index += 1;
            let dir = out.join("rounds").join(format!("{index:03}-{}", b.label));
            eprintln!("kiln-lab: round {index} ({}) in {}", b.label, dir.display());
            match lab::round(b, &profile, hogs, &place, &dir, &bin, index) {
                Ok(r) => {
                    eprintln!("kiln-lab: round {index} done: {}", summary_line(&r));
                    run.rounds.push(r);
                }
                Err(e) => {
                    eprintln!("kiln-lab: round {index} ({}) failed: {e:#}", b.label);
                    run.failures.push(format!("round {index} ({}): {e:#}", b.label));
                }
            }
            if !c.keep {
                lab::remove_paced(&dir);
            }
            std::fs::write(out.join("run.json"), serde_json::to_vec_pretty(&run)?)?;
        }
    }
    let md = report::markdown(&run);
    std::fs::write(out.join("report.md"), &md)?;
    println!("{md}");
    eprintln!("kiln-lab: the run is in {}", out.display());
    Ok(run)
}

fn summary_line(r: &lab::Round) -> String {
    // Latencies to the microsecond (goal 10, plan 01a1a87b2dae step 1).
    let get = |k: &str| r.numbers.get(k).map_or_else(|| "-".to_owned(), |v| format!("{v:.3}"));
    format!(
        "api_read p50 {} p99.9 {} ms, websocket p50 {} p99.9 {} ms, attach p99 {} ms, shards' delay p99.9 {} µs (worst shard {} µs), shards' busy share {}, server CPU {} ms a request",
        get("api_read.p50_ms"),
        get("api_read.p999_ms"),
        get("websocket.p50_ms"),
        get("websocket.p999_ms"),
        get("attach.p99_ms"),
        get("shards.delay_p999_us"),
        get("shards.worst_delay_p999_us"),
        get("shards.busy_share"),
        get("server.cpu_ms_per_request")
    )
}

fn load(path: &Path) -> anyhow::Result<Run> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    // Run as `claude` by a runner: the stand-in.
    let me = args.first().map(|a| Path::new(a).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()).unwrap_or_default();
    if me == "claude" {
        return standin::main(args.get(1..).unwrap_or_default());
    }
    match Cli::parse().cmd {
        Cmd::Run { kiln, label, env: e, common } => {
            measure(&[build(&label, &kiln, env(&e)?, &common)?], &common)?;
        }
        Cmd::Ab { a, b, label_a, label_b, env_a, env_b, common } => {
            anyhow::ensure!(label_a != label_b, "the two builds need labels of their own");
            measure(&[build(&label_a, &a, env(&env_a)?, &common)?, build(&label_b, &b, env(&env_b)?, &common)?], &common)?;
        }
        Cmd::Compare { first, second, build } => {
            let (one, two) = (load(&first)?, load(&second)?);
            let pick = |r: &Run| build.clone().or_else(|| r.builds().first().cloned()).ok_or_else(|| anyhow::anyhow!("a run without rounds"));
            let (a, b) = (pick(&one)?, pick(&two)?);
            let c = Comparison::of(&format!("{a} (first)"), &one.numbers(&a), &format!("{b} (second)"), &two.numbers(&b))
                .on(report::machine_line(one.rounds.iter().filter(|r| r.build == a)), report::machine_line(two.rounds.iter().filter(|r| r.build == b)));
            print!("{}", report::comparison_markdown(&c));
        }
        Cmd::Report { run, quantum_us } => {
            let mut run = load(&run)?;
            if let Some(q) = quantum_us {
                for r in &mut run.rounds {
                    r.quantum_us = Some(q);
                }
            }
            print!("{}", report::markdown(&run));
        }
        Cmd::Table { runs } => {
            let mut loaded = Vec::new();
            for r in &runs {
                let (label, path) = match r.split_once('=') {
                    Some((l, p)) => (l.to_owned(), PathBuf::from(p)),
                    None => (Path::new(r).parent().and_then(Path::file_name).map_or_else(|| r.clone(), |n| n.to_string_lossy().into_owned()), PathBuf::from(r)),
                };
                loaded.push((label, load(&path)?));
            }
            print!("{}", report::table(&loaded));
        }
        Cmd::Hog { threads } => lab::hog(threads),
    }
    Ok(())
}
