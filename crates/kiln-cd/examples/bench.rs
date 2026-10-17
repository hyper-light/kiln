//! `cargo run --release -p kiln-cd --example bench -- --apps 100000 --clusters 100`
//!
//! The same benchmark `kiln cd bench` runs, without building the server.

use kiln_cd::bench::{self, Bench};

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    let mut b = Bench::default();
    let mut distinct = None;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        if flag == "--no-dry-run" {
            b.dry_run = false;
            continue;
        }
        let value = it.next().map(String::as_str).unwrap_or("");
        let num = || value.parse::<f64>().unwrap_or(0.0);
        match flag.as_str() {
            "--apps" => b.apps = num() as usize,
            "--clusters" => b.clusters = num() as usize,
            "--resources" => b.resources = num() as usize,
            "--distinct" => distinct = Some(num() as usize),
            "--churn" => b.churn = num(),
            "--drift" => b.drift = num(),
            "--qps" => b.qps = num(),
            "--latency-ms" => b.latency_ms = num() as u64,
            "--rollout-ms" => b.rollout_ms = num() as u64,
            "--shards" => b.shards = num() as usize,
            other => eprintln!("unknown flag {other}"),
        }
    }
    b.distinct = distinct.unwrap_or(b.apps).max(1);
    let report = bench::run(&b, |p| {
        eprintln!(
            "{:<20} simulated {:>8.1} s · wall {:>7.2} s · engine {:>7.2} s · {} inputs{}",
            p.name,
            p.sim_ms as f64 / 1000.0,
            p.wall_ms as f64 / 1000.0,
            p.engine_ms as f64 / 1000.0,
            p.inputs,
            if p.done { "" } else { " · DID NOT SETTLE" }
        )
    });
    println!("{}", bench::table(&b, &report));
}
