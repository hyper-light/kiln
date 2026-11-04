//! `kiln-lab browser`: the browser lab (docs/browser-lab.md; goal 1 of the browser protocol
//! program, plan 01a1a832e9cc76f79fdd303f52bc658b). Real browsers over emulated links, made without
//! root in namespaces of the lab's own, measuring what each target (kiln today, the HTTP/2 and
//! HTTP/3 reference servers, and later goals' carriers) costs per operation in bytes, server CPU and
//! server-event-to-client time.
//!
//! ```text
//! kiln-lab browser probe                     what the machine can do, stopping at the first gap
//! kiln-lab browser profiles                  the network profiles, and the commands each makes
//! kiln-lab browser check-profiles --out DIR  each profile measured against what it sets
//! kiln-lab browser record --kiln K --out DIR  the real UI recorded, for the catalogue
//! kiln-lab browser plan --k 8                 the run plan: rounds and hours for each cell
//! kiln-lab browser run --kiln K ... --out DIR the matrix (or a part): rounds of every target
//! kiln-lab browser report DIR/run.json        a run's tables
//! kiln-lab browser compare A B [--b-run R2]   two targets (or two runs) cell by cell
//! ```
//!
//! Linux only: on macOS and Windows every command but `profiles` refuses, saying why.

mod cells;
pub mod link;
mod plan;
mod probe;
mod report;
mod runfile;
pub mod standins;
mod targets;

#[cfg(target_os = "linux")]
mod check;
#[cfg(target_os = "linux")]
mod record;
#[cfg(target_os = "linux")]
mod run;
#[cfg(target_os = "linux")]
mod servers;
#[cfg(target_os = "linux")]
mod tools;
#[cfg(target_os = "linux")]
mod topo;

use std::path::PathBuf;

#[derive(clap::Subcommand, Clone, Debug)]
pub enum Cmd {
    /// Check what the lab needs on this machine, stopping at the first thing missing.
    Probe {
        /// The lab's web driver (its `node_modules` installed).
        #[arg(long)]
        web: Option<PathBuf>,
        /// The browsers the lab will run, each required to start (Firefox is checked either way).
        #[arg(long, default_value = "chromium,webkit")]
        browsers: String,
    },
    /// The network profiles, and for one condition the commands that make it.
    Profiles {
        /// Render this profile's commands (with `--loss`, `--seed`, `--client`).
        #[arg(long)]
        profile: Option<String>,
        #[arg(long, default_value = "0")]
        loss: String,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        #[arg(long, default_value_t = 0)]
        client: usize,
    },
    /// Measure each of the bar's profiles against what it sets (throughput, RTT, a full queue,
    /// seeded loss).
    CheckProfiles {
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 1)]
        seed: u64,
    },
    /// Record the real UI on the lab kiln (step 3's catalogue comes from it).
    Record {
        #[arg(long)]
        kiln: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 600)]
        seconds: u64,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        #[arg(long)]
        web: Option<PathBuf>,
    },
    /// The run plan: rounds and hours for every cell at K links.
    Plan {
        #[arg(long, default_value_t = 8)]
        k: u64,
        #[arg(long, default_value = "kiln-today,h2o-h2,caddy-h2,h2o-h3,caddy-h3")]
        targets: String,
        #[arg(long, default_value = "chromium,webkit")]
        browsers: String,
        #[arg(long, default_value = "stream,burst,request")]
        cases: String,
        #[arg(long, default_value_t = 5)]
        background_s: u64,
        #[arg(long, default_value_t = 120.0)]
        budget_h: f64,
    },
    /// Rounds of the given targets over the given conditions (the pilot, the A/A, the matrix).
    Run(Box<RunArgs>),
    /// Whether docs/browser-lab.md's tables are a run's, line by line.
    DocCheck { doc: PathBuf, run: PathBuf },
    /// A run's tables, in Markdown.
    Report { run: PathBuf },
    /// Target `a` against target `b`, cell by cell, in one run (an A/A) or, with `--b-run`, in two.
    Compare {
        run: PathBuf,
        a: String,
        b: String,
        #[arg(long)]
        b_run: Option<PathBuf>,
    },
    /// A network program the lab runs inside its namespaces.
    #[cfg(target_os = "linux")]
    #[command(hide = true, subcommand)]
    Tool(tools::Tool),
}

#[derive(clap::Args, Clone, Debug)]
pub struct RunArgs {
        #[arg(long, default_value = "kiln-today,h2o-h2,caddy-h2,h2o-h3,caddy-h3")]
        targets: String,
        #[arg(long, default_value = "good,100k,50k")]
        profiles: String,
        #[arg(long, default_value = "0,1,5")]
        losses: String,
        #[arg(long, default_value = "chromium,webkit")]
        browsers: String,
        #[arg(long, default_value = "stream,burst,request")]
        cases: String,
        #[arg(long, default_value_t = 5)]
        rounds: usize,
        #[arg(long, default_value_t = 4)]
        k: usize,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Seconds each job runs (each case's own on each profile otherwise).
        #[arg(long)]
        duration: Option<u64>,
        #[arg(long, default_value_t = 1000)]
        window_ms: u64,
        #[arg(long, default_value_t = 5)]
        background_s: u64,
        #[arg(long)]
        kiln: Option<PathBuf>,
        #[arg(long)]
        h2o: Option<PathBuf>,
        #[arg(long)]
        caddy: Option<PathBuf>,
        /// Step 3's replay (replay.json and docroot/), for the reference servers.
        #[arg(long)]
        payloads: Option<PathBuf>,
        #[arg(long)]
        web: Option<PathBuf>,
        #[arg(long)]
        out: PathBuf,
    }

/// The web driver's folder: `--web`, `KILN_LAB_WEB`, else the one beside this crate's source.
fn web(given: Option<PathBuf>) -> PathBuf {
    given.or_else(|| std::env::var_os("KILN_LAB_WEB").map(PathBuf::from)).unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web"))
}

fn list(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_owned).collect()
}

fn target_list(s: &str) -> anyhow::Result<Vec<targets::Target>> {
    list(s).iter().map(|n| targets::named(n).ok_or_else(|| anyhow::anyhow!("no target {n}: {}", targets::targets().iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ")))).collect()
}

use runfile as report_run;

#[allow(dead_code)]
fn refuse() -> anyhow::Result<()> {
    anyhow::bail!("the browser lab needs Linux (user and network namespaces, netem, ifb): this is {}", std::env::consts::OS)
}

/// The lab command's own arguments, for running it again inside its namespaces.
fn again() -> Vec<String> {
    std::env::args().skip(1).collect()
}

pub fn run(cmd: Cmd) -> anyhow::Result<()> {
    match cmd {
        Cmd::Profiles { profile, loss, seed, client } => {
            match profile {
                None => {
                    println!("| profile | in the bar | down | up | base RTT | queue (bytes) | netem limit (packets) | what it is |\n| --- | --- | --- | --- | --- | --- | --- | --- |");
                    for p in link::profiles() {
                        let w = |w: &link::Way| format!("{} kbit/s, {} ms ± {} ms, queue {} ms", w.rate_kbit, w.delay_ms, w.jitter_ms, w.queue_ms);
                        println!("| {} | {} | {} | {} | {} ms | {} | {} | {} |", p.name, if p.in_bar { "yes" } else { "no" }, w(&p.down), w(&p.up), p.rtt_ms(), p.down.queue_bytes(), p.down.limit(), p.about);
                    }
                }
                Some(name) => {
                    let profile = link::profile_named(&name).ok_or_else(|| anyhow::anyhow!("no profile {name}"))?;
                    let loss = link::loss_named(&loss).ok_or_else(|| anyhow::anyhow!("no loss {loss}: 0, 1, 5 or ge1"))?;
                    for c in link::shape(client, &link::Condition { profile, loss, seed }) {
                        println!("[{:?}] {}", c.ns, c.argv.join(" "));
                    }
                }
            }
            Ok(())
        }
        Cmd::Probe { web: w, browsers } => probe_cmd(&web(w), &list(&browsers)),
        Cmd::Plan { k, targets: t, browsers, cases, background_s, budget_h } => {
            let ts = target_list(&t)?;
            let p = plan::plan(k, &ts, &list(&browsers), &list(&cases), background_s, budget_h);
            print!("{}", plan::markdown(&p));
            Ok(())
        }
        Cmd::Report { run } => {
            let r: report_run::BrowserRun = serde_json::from_slice(&std::fs::read(&run)?)?;
            print!("{}", report::tables(&r));
            Ok(())
        }
        Cmd::Compare { run, a, b, b_run } => {
            let r1: report_run::BrowserRun = serde_json::from_slice(&std::fs::read(&run)?)?;
            let r2: report_run::BrowserRun = match &b_run {
                Some(p) => serde_json::from_slice(&std::fs::read(p)?)?,
                None => r1.clone(),
            };
            let ja: Vec<&report_run::JobRecord> = r1.jobs.iter().filter(|j| j.target == a).collect();
            let jb: Vec<&report_run::JobRecord> = r2.jobs.iter().filter(|j| j.target == b).collect();
            let (differ, of, which) = report::compare(&ja, &jb);
            #[allow(clippy::cast_precision_loss)]
            let share = if of == 0 { 0.0 } else { differ as f64 / of as f64 * 100.0 };
            println!("{b} against {a}: {differ} of {of} numbers differ beyond noise ({share:.1}%), each held so all {of} hold together at 95% (Bonferroni).");
            for w in which {
                println!("- {w}");
            }
            Ok(())
        }
        Cmd::Record { kiln, out, seconds, seed, web: w } => record_cmd(&kiln, &web(w), &out, seconds, seed),
        Cmd::DocCheck { doc, run } => {
            let r: report_run::BrowserRun = serde_json::from_slice(&std::fs::read(&run)?)?;
            let d = report::doc_differs(&std::fs::read_to_string(&doc)?, &r);
            for (a, b) in d.iter().take(20) {
                println!("doc: {a}\nrun: {b}\n");
            }
            anyhow::ensure!(d.is_empty(), "{} lines of {} differ from {}", d.len(), doc.display(), run.display());
            println!("{}'s tables are {}'s.", doc.display(), run.display());
            Ok(())
        }
        Cmd::Run(args) => {
            let RunArgs { targets: t, profiles, losses, browsers, cases, rounds, k, seed, duration, window_ms, background_s, kiln, h2o, caddy, payloads, web: w, out } = *args;
            let settings = report_run::Settings {
                targets: target_list(&t)?,
                profiles: list(&profiles).iter().map(|p| link::profile_named(p).ok_or_else(|| anyhow::anyhow!("no profile {p}"))).collect::<anyhow::Result<_>>()?,
                losses: list(&losses).iter().map(|l| link::loss_named(l).ok_or_else(|| anyhow::anyhow!("no loss {l}"))).collect::<anyhow::Result<_>>()?,
                browsers: list(&browsers),
                cases: list(&cases),
                rounds,
                k,
                seed,
                duration_s: duration,
                window_ms,
                background_s,
                kiln: kiln.map(std::path::absolute).transpose()?,
                h2o: h2o.map(std::path::absolute).transpose()?,
                caddy: caddy.map(std::path::absolute).transpose()?,
                payloads: payloads.map(std::path::absolute).transpose()?,
                web: web(w),
            };
            run_cmd(&settings, &out)
        }
        Cmd::CheckProfiles { out, seed } => check_profiles(out, seed),
        #[cfg(target_os = "linux")]
        Cmd::Tool(t) => tools::run(t),
    }
}

#[cfg(target_os = "linux")]
fn probe_cmd(web: &std::path::Path, browsers: &[String]) -> anyhow::Result<()> {
    if !topo::inside() {
        let checks = probe::outside();
        let (table, missing) = probe::say(&checks);
        print!("{table}");
        if let Some(m) = missing {
            anyhow::bail!("the lab can't run here: {} is missing ({})", m.name, m.detail);
        }
        return topo::reenter(&again());
    }
    let checks = probe::inside(web, browsers);
    let (table, missing) = probe::say(&checks);
    print!("{table}");
    if let Some(m) = missing {
        anyhow::bail!("the lab can't run here: {} is missing ({})", m.name, m.detail);
    }
    println!("\nEvery capability the lab needs is here.");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn probe_cmd(_web: &std::path::Path, _browsers: &[String]) -> anyhow::Result<()> {
    let (table, _) = probe::say(&probe::outside());
    print!("{table}");
    refuse()
}

#[cfg(target_os = "linux")]
fn check_profiles(out: PathBuf, seed: u64) -> anyhow::Result<()> {
    if !topo::inside() {
        std::fs::create_dir_all(&out)?;
        return topo::reenter(&again());
    }
    let c = check::run(&out, seed)?;
    print!("{}", check::markdown(&c));
    anyhow::ensure!(c.pass, "not every profile check passed (in {})", out.display());
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn check_profiles(_out: PathBuf, _seed: u64) -> anyhow::Result<()> {
    refuse()
}

#[cfg(target_os = "linux")]
fn record_cmd(kiln: &std::path::Path, web: &std::path::Path, out: &std::path::Path, seconds: u64, seed: u64) -> anyhow::Result<()> {
    if !topo::inside() {
        std::fs::create_dir_all(out)?;
        return topo::reenter(&again());
    }
    record::run(&std::path::absolute(kiln)?, web, &std::path::absolute(out)?, seconds, seed)
}

#[cfg(not(target_os = "linux"))]
fn record_cmd(_kiln: &std::path::Path, _web: &std::path::Path, _out: &std::path::Path, _seconds: u64, _seed: u64) -> anyhow::Result<()> {
    refuse()
}

#[cfg(target_os = "linux")]
fn run_cmd(s: &report_run::Settings, out: &std::path::Path) -> anyhow::Result<()> {
    if !topo::inside() {
        std::fs::create_dir_all(out)?;
        return topo::reenter(&again());
    }
    let r = run::run(s, &std::path::absolute(out)?)?;
    print!("{}", report::tables(&r));
    if !r.failures.is_empty() {
        eprintln!("kiln-lab: {} jobs failed: {}", r.failures.len(), r.failures.join("; "));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn run_cmd(_s: &report_run::Settings, _out: &std::path::Path) -> anyhow::Result<()> {
    refuse()
}
