//! W29-AC25's bench: a TypeScript workflow planned on the server, for each of kiln's own workflows
//! written as programs (`tests/fixtures/code/kiln`): erased, run in QuickJS, read by W02, planned by
//! W04 for a push. Each workflow's p50 and p99 over its calls, then all of them together; AC25 asks
//! under 20 ms at p99, on a release build.
//!
//! `cargo bench -p kiln --bench code_workflows` (`KILN_CODE_CALLS` calls each, 200 by default)
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use kiln::ci::code::{NoTree, workflow};
use kiln_ci::plan::{NoSource, RunInput, RunPlan, Settings};
use serde_json::json;

/// The `q` quantile of sorted `v`.
fn at(v: &[f64], q: f64) -> f64 {
    v[((v.len() as f64 * q) as usize).min(v.len() - 1)]
}

fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/code/kiln");
    let calls: usize = std::env::var("KILN_CODE_CALLS").ok().and_then(|v| v.parse().ok()).unwrap_or(200);
    let push = json!({ "ref": "refs/heads/main", "after": "0".repeat(40) });
    let mut all = Vec::new();
    for name in ["bench", "ci", "custody-lab", "e2e", "image", "release", "sim", "stress", "wire-lab"] {
        let ts = std::fs::read_to_string(dir.join(format!("{name}.ts"))).unwrap();
        let path = format!(".kiln/{name}.ts");
        let mut times = Vec::with_capacity(calls);
        for _ in 0..calls {
            let started = Instant::now();
            let (w, d) = workflow(&path, &ts, &push, Arc::new(NoTree));
            assert!(!d.iter().any(kiln_ci::wf::Diag::is_error), "{name}: {d:?}");
            let input = RunInput { github: json!({ "event_name": "push", "ref": "refs/heads/main", "event": push }), inputs: json!({}), vars: json!({}), changed: None, trusted: true };
            std::hint::black_box(RunPlan::preview(Arc::new(w.unwrap()), input, Settings::default(), &NoSource).unwrap());
            times.push(started.elapsed().as_secs_f64() * 1e3);
        }
        times.sort_by(f64::total_cmp);
        println!("{name:12} p50 {:6.2} ms  p99 {:6.2} ms", at(&times, 0.5), at(&times, 0.99));
        all.extend(times);
    }
    all.sort_by(f64::total_cmp);
    println!("all          p50 {:6.2} ms  p99 {:6.2} ms over {} calls (AC25: p99 under 20 ms)", at(&all, 0.5), at(&all, 0.99), all.len());
}
