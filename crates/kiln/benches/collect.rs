//! Logs collected from files (`docs/observe.md` §12.17): what a machine's reading of a source
//! costs, a line at a time. Each case is a file written here and read whole by the collector's own
//! code (its writes answered at once): an nginx access log (text), JSON lines, a pod's CRI log
//! (JSON in it), Java stack traces made one record each by a `multi_line` rule, and the access log
//! again through an exclude and a mask, and through kiln's nginx pipeline (§8.2: grok, the date,
//! a category, the status, the URL and the user agent read). For each: lines a second and MB a
//! second (the median of three reads), records, and per line the allocations and bytes allocated,
//! and the page faults.
//!
//! `cargo bench -p kiln --bench collect` (`KOBS_LINES`: lines a file, 300,000; `KOBS_CASE`: a part
//! of the name of the cases to run, all unless given).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::cast_precision_loss)]

use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use hyper_measure::{alloc, faults};
use kiln::observe::collect::{Encoding, Format, Kind, Rule, RuleKind, Source, Start};

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

fn source(dir: &Path, glob: &str, format: Format, rules: Vec<Rule>) -> Source {
    Source {
        kind: Kind::File,
        paths: vec![format!("{}/{glob}", dir.display())],
        exclude: Vec::new(),
        format,
        start: Start::Beginning,
        encoding: Encoding::Utf8,
        units: Vec::new(),
        listen: Vec::new(),
        timezone: None,
        predicate: None,
        level: None,
        channels: Vec::new(),
        query: None,
        service: Some("bench".into()),
        source: None,
        tags: vec!["env:bench".into()],
        rules,
        on: "server".into(),
    }
}

fn rule(kind: RuleKind, pattern: &str, with: Option<&str>) -> Rule {
    Rule { kind, name: String::new(), pattern: pattern.into(), replace_placeholder: with.map(str::to_owned) }
}

fn write(path: &Path, lines: impl Iterator<Item = String>) -> std::io::Result<u64> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut n = 0;
    for l in lines {
        f.write_all(l.as_bytes())?;
        n += 1;
    }
    f.flush()?;
    Ok(n)
}

fn access(i: u64) -> String {
    let path = if i % 10 == 3 { "/healthz".to_owned() } else { format!("/api/v1/orders/{i}") };
    format!("10.{}.{}.{} - - [18/Oct/2026:12:{:02}:{:02} +0000] \"GET {path} HTTP/1.1\" 200 {} \"-\" \"Mozilla/5.0 (X11; Linux x86_64) Firefox/131.0\"\n", i % 250, (i / 250) % 250, i % 7, (i / 60) % 60, i % 60, 200 + i % 5000)
}

fn json_line(i: u64) -> String {
    format!(
        "{{\"ts\":\"2026-10-18T12:00:{:02}.{:06}Z\",\"level\":\"{}\",\"msg\":\"order {i} paid\",\"order_id\":{i},\"amount\":12.5,\"user\":{{\"id\":\"u{}\",\"plan\":\"pro\"}},\"trace_id\":\"0af7651916cd43dd8448eb211c80319c\"}}\n",
        i % 60,
        i % 1_000_000,
        if i.is_multiple_of(100) { "error" } else { "info" },
        i % 1000
    )
}

fn main() {
    let n: u64 = std::env::var("KOBS_LINES").ok().and_then(|v| v.parse().ok()).unwrap_or(300_000);
    let dir = tempfile::tempdir().expect("a folder");
    let d = dir.path();
    let mut cases: Vec<(&str, Source, u64)> = Vec::new();
    let lines = write(&d.join("access.log"), (0..n).map(access)).unwrap();
    cases.push(("text (nginx access)", source(d, "access.log", Format::Auto, Vec::new()), lines));
    let lines = write(&d.join("app.json"), (0..n).map(json_line)).unwrap();
    cases.push(("JSON lines", source(d, "app.json", Format::Auto, Vec::new()), lines));
    let lines = write(&d.join("pod.log"), (0..n).map(|i| format!("2026-10-18T12:00:{:02}.{:09}Z {} F {}", i % 60, i % 1_000_000_000, if i % 9 == 0 { "stderr" } else { "stdout" }, json_line(i)))).unwrap();
    cases.push(("CRI (JSON in it)", Source { kind: Kind::Kubernetes, ..source(d, "pod.log", Format::Cri, Vec::new()) }, lines));
    let trace = |i: u64| -> String {
        let mut s = format!("2026-10-18 12:00:{:02},{:03} ERROR [http-nio-8080-exec-{}] c.s.Shop - payment {i} failed\n", i % 60, i % 1000, i % 16);
        s.push_str("java.lang.IllegalStateException: card declined\n");
        for f in 0..8 {
            s.push_str(&format!("\tat com.shop.Payments.charge{f}(Payments.java:{})\n", 40 + f));
        }
        s
    };
    let lines = write(&d.join("traces.log"), (0..n / 10).map(trace)).unwrap() * 10;
    cases.push(("Java traces (multi_line)", source(d, "traces.log", Format::Text, vec![rule(RuleKind::MultiLine, r"^\d{4}-\d{2}-\d{2}", None)]), lines));
    cases.push((
        "text, exclude and mask",
        source(d, "access.log", Format::Auto, vec![rule(RuleKind::ExcludeAtMatch, "healthz", None), rule(RuleKind::MaskSequences, r"\b\d{1,3}(\.\d{1,3}){3}\b", Some("[ip]"))]),
        n,
    ));
    cases.push(("text, kiln's nginx pipeline", Source { source: Some("nginx".into()), ..source(d, "access.log", Format::Auto, Vec::new()) }, n));
    println!("{n} lines a file; the median of 3 reads; allocations and faults of one");
    println!("{:<26} {:>9} {:>9} {:>12} {:>9} {:>11} {:>11} {:>8}", "case", "MB", "records", "lines/s", "MB/s", "allocs/line", "bytes/line", "faults");
    let only = std::env::var("KOBS_CASE").unwrap_or_default();
    for (i, (name, s, lines)) in cases.iter().enumerate().filter(|(_, c)| c.0.contains(only.as_str())) {
        // kiln's own pipelines on for the case that names one (they read `source:nginx` alone).
        let pipelines: Vec<_> = if name.contains("pipeline") { kiln::observe::pipeline::builtin::all().into_iter().map(|(_, p)| p).collect() } else { Vec::new() };
        kiln::observe::pipeline::kept::activate(&pipelines);
        let mut times: Vec<Duration> = Vec::new();
        let mut out = (0, 0);
        let mut counted = alloc::Counts::default();
        let mut faulted = 0;
        for rep in 0..3 {
            let places = d.join(format!("places-{i}-{rep}.json"));
            let f0 = faults::read().ok();
            let t0 = Instant::now();
            if rep == 0 {
                alloc::begin();
            }
            out = kiln::observe::collect::read_whole(name, s, &places).expect("read");
            if rep == 0 {
                counted = alloc::end();
                faulted = f0.zip(faults::read().ok()).map_or(0, |(a, b)| b.since(&a).total());
            }
            times.push(t0.elapsed());
        }
        times.sort();
        let t = times.get(1).map_or(f64::NAN, Duration::as_secs_f64);
        let (records, bytes) = out;
        println!(
            "{:<26} {:>9.1} {:>9} {:>12.0} {:>9.1} {:>11.2} {:>11.0} {:>8}",
            name,
            bytes as f64 / 1e6,
            records,
            *lines as f64 / t,
            bytes as f64 / 1e6 / t,
            counted.calls() as f64 / *lines as f64,
            counted.bytes as f64 / *lines as f64,
            faulted
        );
    }
}
