//! W25 step 1: replays kiln's recorded week (ci/sim/trace.json) under each policy and prints the
//! report ci/sim/README.md keeps; with `--sweeps`, also the sweeps its defaults come from; with
//! `--late`, instead, which of ci's commits on main P3 and P4 leave without an answer an hour after
//! their push, and what held each (W28-AC51's rest).
//!
//!     cargo run -p kiln-ci --example replay -- ci/sim/trace.json [--sweeps | --late]

use std::process::ExitCode;

use kiln_ci::sched::sim::replay::report::{SEEDS, late_table, report, sweeps};
use kiln_ci::sched::sim::replay::{Policy, Trace};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args.iter().find(|a| !a.starts_with("--")).cloned().unwrap_or_else(|| "ci/sim/trace.json".into());
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let trace = match Trace::parse(&text) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    if args.iter().any(|a| a == "--late") {
        for p in [Policy::p3(), Policy::p4()] {
            print!("{}, seeds 1-8:\n\n{}\n", p.name, late_table(&trace, &p, &SEEDS));
        }
        return ExitCode::SUCCESS;
    }
    print!("{}", report(&trace, &Policy::all(), &SEEDS));
    if args.iter().any(|a| a == "--sweeps") {
        print!("{}", sweeps(&trace, &SEEDS));
    }
    ExitCode::SUCCESS
}
