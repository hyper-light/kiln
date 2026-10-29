//! Grok's own matcher against `regex` (`docs/observe.md` §12.17): kiln's nginx rules (Datadog's)
//! read lines of each kind, by grok's own matcher and by `regex` alone: a combined access line (the
//! second rule), a common one (the first), an error line (the third), and a line none reads. For
//! each, ns a line (the median of five rounds), allocations a line, and how many matched (both
//! engines alike, or it says so). Each line's attributes are handed back once read, as the
//! collector hands back a record's once its row is written.
//!
//! `cargo bench -p kiln --bench grok` (`KOBS_LINES`: lines a case, 200,000; `KOBS_CASE`: a part of
//! the names of the cases to run; `KOBS_ENGINE=grok` or `regex`: one engine alone, for a profile).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::cast_precision_loss, clippy::panic, clippy::indexing_slicing)]

use std::time::{Duration, Instant};

use hyper_measure::alloc;
use kiln::observe::pipeline::grok::{Grok, Reading};
use kiln::observe::pipeline::{KindDef, builtin};

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

fn combined(i: u64) -> String {
    let path = if i % 10 == 3 { "/healthz".to_owned() } else { format!("/api/v1/orders/{i}") };
    format!(
        "10.{}.{}.{} - - [18/Oct/2026:12:{:02}:{:02} +0000] \"GET {path} HTTP/1.1\" 200 {} \"-\" \"Mozilla/5.0 (X11; Linux x86_64) Firefox/131.0\"",
        i % 250,
        (i / 250) % 250,
        i % 7,
        (i / 60) % 60,
        i % 60,
        200 + i % 5000
    )
}

fn common(i: u64) -> String {
    format!("192.0.2.{} - frank [18/Oct/2026:12:{:02}:{:02} -0700] \"POST /cart/{i} HTTP/2.0\" {} {}", i % 250, (i / 60) % 60, i % 60, [200, 201, 404, 502][(i % 4) as usize], 100 + i % 900)
}

fn error(i: u64) -> String {
    format!(
        "2026/10/18 12:{:02}:{:02} [error] 8409#8409: *{i} open() \"/usr/share/nginx/html/favicon.ico\" failed (2: No such file or directory), client: 192.0.2.{}, server: localhost, request: \"GET /favicon.ico HTTP/1.1\", host: \"example.com\"",
        (i / 60) % 60,
        i % 60,
        i % 250
    )
}

fn none(i: u64) -> String {
    format!("worker {i} picked up job {} from the queue after {} ms", i * 7, i % 1000)
}

fn main() {
    let n: u64 = std::env::var("KOBS_LINES").ok().and_then(|v| v.parse().ok()).unwrap_or(200_000);
    let (_, nginx) = builtin::all().into_iter().find(|(id, _)| *id == "builtin:nginx").expect("kiln's nginx pipeline");
    let Some(KindDef::GrokParser { grok, .. }) = nginx.processors.first().map(|p| &p.kind) else { panic!("its first processor parses") };
    let ours = Grok::compile(&grok.match_rules, &grok.support_rules).expect("its rules compile");
    let theirs = Grok::compile(&grok.match_rules, &grok.support_rules).expect("its rules compile").regex_only();
    let now_ns = 1_792_324_800_000_000_000;
    println!("{n} lines a case; ns a line, the median of 5 rounds; allocations of one");
    println!("{:<20} {:>8} {:>12} {:>12} {:>14} {:>14} {:>9}", "case", "matched", "ns (grok's)", "ns (regex)", "allocs (grok's)", "allocs (regex)", "speedup");
    let cases: [(&str, fn(u64) -> String); 4] = [("combined access", combined), ("common access", common), ("error", error), ("none reads it", none)];
    let only = std::env::var("KOBS_CASE").unwrap_or_default();
    let engine = std::env::var("KOBS_ENGINE").unwrap_or_default();
    for (name, make) in cases.into_iter().filter(|(name, _)| name.contains(only.as_str())) {
        let lines: Vec<String> = (0..n).map(make).collect();
        let mut row = Vec::new();
        let mut matched = Vec::new();
        for (which, g) in [("grok", &ours), ("regex", &theirs)] {
            if !engine.is_empty() && engine != which {
                row.push((f64::NAN, f64::NAN));
                matched.push(0);
                continue;
            }
            let mut reading = Reading::default();
            let mut out = kiln_obs::logs::buf::Attrs::default();
            let mut times = Vec::new();
            let mut counted = alloc::Counts::default();
            let mut hits = 0u64;
            for rep in 0..5 {
                if rep == 1 {
                    alloc::begin();
                }
                let t0 = Instant::now();
                for l in &lines {
                    hits += u64::from(g.parse_with(l, now_ns, &mut reading, &mut out).is_some());
                    out.clear();
                }
                times.push(t0.elapsed());
                if rep == 1 {
                    counted = alloc::end();
                }
            }
            times.sort();
            row.push((times.get(2).map_or(f64::NAN, Duration::as_secs_f64) * 1e9 / n as f64, counted.calls() as f64 / n as f64));
            matched.push(hits / 5);
        }
        if engine.is_empty() {
            assert_eq!(matched.first(), matched.get(1), "{name}: both engines match the same lines");
        }
        let ((a, aa), (b, ba)) = (row[0], row[1]);
        println!("{:<20} {:>8} {:>12.0} {:>12.0} {:>14.2} {:>14.2} {:>8.2}x", name, matched[0].max(matched[1]), a, b, aa, ba, b / a);
    }
}
