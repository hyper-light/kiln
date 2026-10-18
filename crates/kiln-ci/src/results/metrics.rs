//! Benchmark metrics (docs/ci.md W16-R07): what steps write to `$KILN_METRICS`, one JSON object
//! a line, `{ name, value, unit, better: lower | higher, tags, samples? }`, and converters from
//! the common tools' outputs to it (`kiln metrics <tool> <path>`): criterion, hyperfine, `go test
//! -bench`, pytest-benchmark, Google Benchmark, libtest's bench output and k6's summary.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::stats::Better;

/// One measurement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub name: String,
    pub value: f64,
    #[serde(default)]
    pub unit: String,
    #[serde(default)]
    pub better: Better,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
    /// The runs behind `value`, when the tool kept them: a regression check resamples these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub samples: Vec<f64>,
}

impl Point {
    fn new(name: impl Into<String>, value: f64, unit: &str, better: Better) -> Point {
        Point { name: name.into(), value, unit: unit.to_owned(), better, tags: BTreeMap::new(), samples: Vec::new() }
    }
}

/// The most points a step may send.
pub const MAX_POINTS: usize = 10_000;

/// `$KILN_METRICS`: its points, and what was wrong with the lines that aren't.
pub fn parse_lines(text: &str) -> (Vec<Point>, Vec<String>) {
    let (mut out, mut bad) = (Vec::new(), Vec::new());
    for (n, line) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
        match serde_json::from_str::<Point>(line.trim()) {
            Ok(p) if p.name.trim().is_empty() => bad.push(format!("line {}: a metric needs a name", n + 1)),
            Ok(p) if !p.value.is_finite() => bad.push(format!("line {}: `{}` isn't a number", n + 1, p.name)),
            Ok(p) => out.push(p),
            Err(e) => bad.push(format!("line {}: {e}", n + 1)),
        }
        if out.len() >= MAX_POINTS {
            bad.push(format!("more than {MAX_POINTS} metrics: the rest are dropped"));
            break;
        }
    }
    (out, bad)
}

/// The tools [`convert`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Criterion,
    Hyperfine,
    GoBench,
    PytestBenchmark,
    GoogleBenchmark,
    LibtestBench,
    K6,
}

impl Tool {
    pub fn parse(s: &str) -> Option<Tool> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "criterion" => Tool::Criterion,
            "hyperfine" => Tool::Hyperfine,
            "gobench" | "go-bench" | "go" => Tool::GoBench,
            "pytest-benchmark" | "pytest" => Tool::PytestBenchmark,
            "gbench" | "google-benchmark" => Tool::GoogleBenchmark,
            "libtest" | "cargo-bench" | "libtest-bench" => Tool::LibtestBench,
            "k6" => Tool::K6,
            _ => return None,
        })
    }
}

fn json(text: &str) -> Result<Value, String> {
    serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))
}

/// Converts a tool's output to points. `files` are `(path, text)`: criterion's directory as its
/// `…/new/estimates.json` (and `sample.json`) files, every other tool's one output.
pub fn convert(tool: Tool, files: &[(String, String)]) -> Result<Vec<Point>, String> {
    let one = || files.first().map(|(_, t)| t.as_str()).ok_or("no output to read");
    match tool {
        Tool::Criterion => criterion(files),
        Tool::Hyperfine => hyperfine(one()?),
        Tool::GoBench => Ok(go_bench(one()?)),
        Tool::PytestBenchmark => pytest_benchmark(one()?),
        Tool::GoogleBenchmark => google_benchmark(one()?),
        Tool::LibtestBench => Ok(libtest_bench(one()?)),
        Tool::K6 => k6(one()?),
    }
}

/// criterion: each `<id>/new/estimates.json`'s median (ns), with `sample.json`'s per-iteration
/// times as samples. The benchmark's id is its path below the criterion directory.
fn criterion(files: &[(String, String)]) -> Result<Vec<Point>, String> {
    let mut out = Vec::new();
    for (path, text) in files.iter().filter(|(p, _)| p.ends_with("new/estimates.json")) {
        let id = path.trim_end_matches("new/estimates.json").trim_end_matches('/');
        let id = id.split_once("criterion/").map_or(id, |(_, rest)| rest).to_owned();
        let v = json(text)?;
        let Some(median) = v.pointer("/median/point_estimate").and_then(Value::as_f64) else { continue };
        let mut p = Point::new(id, median, "ns", Better::Lower);
        let sample_path = path.replace("estimates.json", "sample.json");
        if let Some((_, s)) = files.iter().find(|(p2, _)| *p2 == sample_path)
            && let Ok(sv) = json(s)
        {
            let iters = sv.get("iters").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_f64).collect::<Vec<_>>()).unwrap_or_default();
            let times = sv.get("times").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_f64).collect::<Vec<_>>()).unwrap_or_default();
            p.samples = iters.iter().zip(&times).filter(|(i, _)| **i > 0.0).map(|(i, t)| t / i).collect();
        }
        out.push(p);
    }
    if out.is_empty() {
        return Err("no criterion estimates (…/new/estimates.json)".into());
    }
    Ok(out)
}

/// hyperfine `--export-json`: each command's median (s), its runs as samples.
fn hyperfine(text: &str) -> Result<Vec<Point>, String> {
    let v = json(text)?;
    let results = v.get("results").and_then(Value::as_array).ok_or("hyperfine's JSON has `results`")?;
    Ok(results
        .iter()
        .filter_map(|r| {
            let name = r.get("command").and_then(Value::as_str)?;
            let mut p = Point::new(name, r.get("median").or_else(|| r.get("mean")).and_then(Value::as_f64)?, "s", Better::Lower);
            p.samples = r.get("times").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
            Some(p)
        })
        .collect())
}

/// `go test -bench`: `BenchmarkX-8  1000  1053 ns/op  128 B/op  2 allocs/op`, each measure a
/// point; repeated runs (`-count`) are samples, their median the value.
fn go_bench(text: &str) -> Vec<Point> {
    let mut by: BTreeMap<(String, String), Vec<f64>> = BTreeMap::new();
    let mut order: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let Some(name) = it.next().filter(|n| n.starts_with("Benchmark")) else { continue };
        // The name without its GOMAXPROCS suffix (`-8`).
        let name = match name.rsplit_once('-') {
            Some((n, cpus)) if cpus.bytes().all(|b| b.is_ascii_digit()) => n,
            _ => name,
        };
        let rest: Vec<&str> = it.collect();
        if rest.first().is_none_or(|n| n.parse::<u64>().is_err()) {
            continue;
        }
        for pair in rest.get(1..).unwrap_or_default().chunks(2) {
            let (Some(v), Some(unit)) = (pair.first().and_then(|v| v.parse::<f64>().ok()), pair.get(1)) else { continue };
            let key = (name.to_owned(), (*unit).to_owned());
            if !by.contains_key(&key) {
                order.push(key.clone());
            }
            by.entry(key).or_default().push(v);
        }
    }
    order
        .into_iter()
        .filter_map(|key| {
            let mut v = by.get(&key)?.clone();
            let (name, unit) = key;
            let better = if unit.contains("MB/s") || unit.ends_with("/s") { Better::Higher } else { Better::Lower };
            let label = if unit == "ns/op" { name } else { format!("{name} {unit}") };
            let mut p = Point::new(label, crate::stats::median(&mut v.clone())?, &unit, better);
            if v.len() > 1 {
                v.sort_by(f64::total_cmp);
                p.samples = v;
            }
            Some(p)
        })
        .collect()
}

/// pytest-benchmark `--benchmark-json`: each benchmark's median (s), its rounds as samples when
/// kept (`--benchmark-save-data`).
fn pytest_benchmark(text: &str) -> Result<Vec<Point>, String> {
    let v = json(text)?;
    let list = v.get("benchmarks").and_then(Value::as_array).ok_or("pytest-benchmark's JSON has `benchmarks`")?;
    Ok(list
        .iter()
        .filter_map(|b| {
            let name = b.get("fullname").or_else(|| b.get("name")).and_then(Value::as_str)?;
            let stats = b.get("stats")?;
            let mut p = Point::new(name, stats.get("median").or_else(|| stats.get("mean")).and_then(Value::as_f64)?, "s", Better::Lower);
            p.samples = stats.get("data").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
            Some(p)
        })
        .collect())
}

/// Google Benchmark `--benchmark_format=json`: each benchmark's real time in its `time_unit`;
/// repetitions are samples, an aggregate median the value when there is one.
fn google_benchmark(text: &str) -> Result<Vec<Point>, String> {
    let v = json(text)?;
    let list = v.get("benchmarks").and_then(Value::as_array).ok_or("Google Benchmark's JSON has `benchmarks`")?;
    let mut by: BTreeMap<String, (String, Vec<f64>, Option<f64>)> = BTreeMap::new();
    let mut order = Vec::new();
    for b in list {
        let name = b.get("run_name").or_else(|| b.get("name")).and_then(Value::as_str).unwrap_or_default().to_owned();
        let unit = b.get("time_unit").and_then(Value::as_str).unwrap_or("ns").to_owned();
        let Some(t) = b.get("real_time").and_then(Value::as_f64) else { continue };
        if !by.contains_key(&name) {
            order.push(name.clone());
        }
        let e = by.entry(name).or_insert_with(|| (unit, Vec::new(), None));
        match (b.get("run_type").and_then(Value::as_str), b.get("aggregate_name").and_then(Value::as_str)) {
            (Some("aggregate"), Some("median")) => e.2 = Some(t),
            (Some("aggregate"), _) => {}
            _ => e.1.push(t),
        }
    }
    Ok(order
        .into_iter()
        .filter_map(|name| {
            let (unit, samples, med) = by.get(&name)?.clone();
            let value = med.or_else(|| crate::stats::median(&mut samples.clone()))?;
            let mut p = Point::new(name, value, &unit, Better::Lower);
            if samples.len() > 1 {
                p.samples = samples;
            }
            Some(p)
        })
        .collect())
}

/// libtest's bench output: `test name ... bench:   1,234 ns/iter (+/- 56)`.
fn libtest_bench(text: &str) -> Vec<Point> {
    text.lines()
        .filter_map(|l| {
            let rest = l.trim().strip_prefix("test ")?;
            let (name, after) = rest.split_once(" ... bench:")?;
            let mut it = after.split_whitespace();
            let v: f64 = it.next()?.replace(',', "").parse().ok()?;
            let unit = it.next().unwrap_or("ns/iter");
            Some(Point::new(name.trim(), v, unit, Better::Lower))
        })
        .collect()
}

/// k6's summary (`--summary-export`, or `handleSummary`'s JSON): each trend's median and p95,
/// and each counter's rate.
fn k6(text: &str) -> Result<Vec<Point>, String> {
    let v = json(text)?;
    let metrics = v.get("metrics").and_then(Value::as_object).ok_or("k6's summary has `metrics`")?;
    let mut out = Vec::new();
    for (name, m) in metrics {
        let vals = m.get("values").unwrap_or(m);
        let unit = if name.contains("duration") || name.starts_with("http_req_") && !name.ends_with("failed") { "ms" } else { "" };
        for (key, label) in [("med", "p50"), ("p(95)", "p95")] {
            if let Some(x) = vals.get(key).and_then(Value::as_f64) {
                out.push(Point::new(format!("k6 {name} {label}"), x, unit, Better::Lower));
            }
        }
        if let Some(r) = vals.get("rate").and_then(Value::as_f64) {
            let better = if name.contains("failed") || name.contains("errors") { Better::Lower } else { Better::Higher };
            out.push(Point::new(format!("k6 {name} rate"), r, "/s", better));
        }
    }
    if out.is_empty() {
        return Err("no k6 trends or rates in the summary".into());
    }
    Ok(out)
}

/// A job's `bench-gate` (W16-R09): `{ metric: "<glob>", max-regression: 5%, same-fingerprint:
/// true, action: fail | warn }`.
#[derive(Debug, Clone, PartialEq)]
pub struct Gate {
    /// Which metrics, `*` matching any run of characters.
    pub metric: String,
    /// The most a metric may get worse, a fraction (`0.05`).
    pub max_regression: f64,
    /// Compare only with baselines from runners of the same fingerprint.
    pub same_fingerprint: bool,
    /// Fail the job, or only warn.
    pub fail: bool,
}

impl Gate {
    pub fn from_yval(v: &crate::wf::ir::YVal) -> Result<Gate, String> {
        use crate::wf::ir::YVal;
        let metric = v.get("metric").and_then(YVal::as_str).unwrap_or("*").to_owned();
        let max = match v.get("max-regression") {
            None => 0.05,
            Some(YVal::Num(n)) => if *n > 1.0 { n / 100.0 } else { *n },
            Some(YVal::Str(t)) => {
                let s = t.src.trim();
                match s.strip_suffix('%') {
                    Some(pct) => pct.trim().parse::<f64>().map_err(|_| format!("bench-gate: max-regression `{s}` isn't a percentage"))? / 100.0,
                    None => s.parse::<f64>().map_err(|_| format!("bench-gate: max-regression `{s}` isn't a percentage"))?,
                }
            }
            Some(_) => return Err("bench-gate: max-regression is a percentage, like 5%".into()),
        };
        if !(0.0..10.0).contains(&max) {
            return Err(format!("bench-gate: max-regression {:.0}% is out of range", max * 100.0));
        }
        let fail = match v.get("action").and_then(YVal::as_str).unwrap_or("fail") {
            "fail" => true,
            "warn" => false,
            other => return Err(format!("bench-gate: action is fail or warn, not `{other}`")),
        };
        Ok(Gate { metric, max_regression: max, same_fingerprint: v.get("same-fingerprint").and_then(YVal::as_bool).unwrap_or(true), fail })
    }

    /// Whether it gates metric `name`.
    pub fn covers(&self, name: &str) -> bool {
        glob(&self.metric, name)
    }
}

/// `*` as any run of characters.
fn glob(pat: &str, s: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == s;
    }
    let (first, last) = (parts.first().copied().unwrap_or_default(), parts.last().copied().unwrap_or_default());
    if !s.starts_with(first) || !s.ends_with(last) || s.len() < first.len() + last.len() {
        return false;
    }
    let mut rest = s.get(first.len()..s.len() - last.len()).unwrap_or_default();
    for p in parts.get(1..parts.len() - 1).unwrap_or_default() {
        match rest.find(p) {
            Some(at) => rest = rest.get(at + p.len()..).unwrap_or_default(),
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w16_r09_gates() {
        let parse = |y: &str| {
            let (w, _) = crate::wf::parse_workflow(".kiln/b.yaml", &format!("on: push\njobs:\n  b:\n    runs-on: x\n    bench-gate: {y}\n    steps: [{{run: b}}]\n"), crate::expr::Dialect::Kiln);
            w.and_then(|w| w.jobs.into_iter().next()).and_then(|j| j.kiln.bench_gate).map(|v| Gate::from_yval(&v))
        };
        let g = parse("{ metric: \"*/p99_ns\", max-regression: 5%, action: fail }");
        assert_eq!(g, Some(Ok(Gate { metric: "*/p99_ns".into(), max_regression: 0.05, same_fingerprint: true, fail: true })));
        assert!(g.and_then(Result::ok).is_some_and(|g| g.covers("parse/p99_ns") && !g.covers("parse/p50_ns")));
        assert_eq!(parse("{ max-regression: 10, action: warn, same-fingerprint: false }").and_then(Result::ok).map(|g| (g.max_regression, g.fail, g.same_fingerprint, g.metric)), Some((0.1, false, false, "*".into())));
        assert!(parse("{ action: explode }").is_some_and(|r| r.is_err()));
    }

    fn names(p: &[Point]) -> Vec<(String, f64, String)> {
        p.iter().map(|x| (x.name.clone(), x.value, x.unit.clone())).collect()
    }

    #[test]
    fn w16_r07_lines_and_converters() {
        let (p, bad) = parse_lines("{\"name\":\"parse/p99_ns\",\"value\":1200,\"unit\":\"ns\"}\n{\"name\":\"rps\",\"value\":5e3,\"better\":\"higher\",\"tags\":{\"route\":\"/\"}}\nnot json\n{\"name\":\"\",\"value\":1}\n");
        assert_eq!(names(&p), vec![("parse/p99_ns".into(), 1200.0, "ns".into()), ("rps".into(), 5000.0, String::new())]);
        assert_eq!(p.get(1).map(|x| x.better), Some(Better::Higher));
        assert_eq!(bad.len(), 2);

        let crit = vec![
            ("target/criterion/parse/small/new/estimates.json".to_owned(), r#"{"mean":{"point_estimate":1100.0},"median":{"point_estimate":1000.0}}"#.to_owned()),
            ("target/criterion/parse/small/new/sample.json".to_owned(), r#"{"sampling_mode":"Linear","iters":[10.0,20.0],"times":[10000.0,22000.0]}"#.to_owned()),
        ];
        let c = convert(Tool::Criterion, &crit).unwrap_or_default();
        assert_eq!(names(&c), vec![("parse/small".into(), 1000.0, "ns".into())]);
        assert_eq!(c.first().map(|p| p.samples.clone()), Some(vec![1000.0, 1100.0]));

        let h = convert(Tool::Hyperfine, &[("h.json".into(), r#"{"results":[{"command":"sleep 0.1","mean":0.11,"median":0.105,"times":[0.1,0.11,0.12]}]}"#.into())]).unwrap_or_default();
        assert_eq!(names(&h), vec![("sleep 0.1".into(), 0.105, "s".into())]);

        let g = convert(Tool::GoBench, &[("o".into(), "goos: linux\nBenchmarkParse-8   \t 1000000\t      1053 ns/op\t     128 B/op\t       2 allocs/op\nBenchmarkParse-8   \t 1000000\t      1100 ns/op\t     128 B/op\t       2 allocs/op\nPASS\n".into())]).unwrap_or_default();
        assert_eq!(names(&g).first().cloned(), Some(("BenchmarkParse".into(), 1076.5, "ns/op".into())));
        assert!(names(&g).iter().any(|x| x.0 == "BenchmarkParse allocs/op"));

        let pb = convert(Tool::PytestBenchmark, &[("p".into(), r#"{"benchmarks":[{"name":"test_parse","fullname":"tests/test_p.py::test_parse","stats":{"median":0.002,"mean":0.0021}}]}"#.into())]).unwrap_or_default();
        assert_eq!(names(&pb), vec![("tests/test_p.py::test_parse".into(), 0.002, "s".into())]);

        let gb = convert(Tool::GoogleBenchmark, &[("g".into(), r#"{"benchmarks":[{"name":"BM_Parse/repeats:2","run_name":"BM_Parse","run_type":"iteration","real_time":110.0,"time_unit":"ns"},{"name":"BM_Parse","run_name":"BM_Parse","run_type":"iteration","real_time":90.0,"time_unit":"ns"},{"name":"BM_Parse_median","run_name":"BM_Parse","run_type":"aggregate","aggregate_name":"median","real_time":100.0,"time_unit":"ns"}]}"#.into())]).unwrap_or_default();
        assert_eq!(names(&gb), vec![("BM_Parse".into(), 100.0, "ns".into())]);

        let lb = convert(Tool::LibtestBench, &[("l".into(), "running 1 test\ntest bench_parse ... bench:       1,234 ns/iter (+/- 56)\n".into())]).unwrap_or_default();
        assert_eq!(names(&lb), vec![("bench_parse".into(), 1234.0, "ns/iter".into())]);

        let k = convert(Tool::K6, &[("k".into(), r#"{"metrics":{"http_req_duration":{"avg":12.0,"med":10.0,"p(95)":30.0},"http_reqs":{"count":100,"rate":50.0}}}"#.into())]).unwrap_or_default();
        assert!(names(&k).contains(&("k6 http_req_duration p95".into(), 30.0, "ms".into())));
        assert!(k.iter().any(|p| p.name == "k6 http_reqs rate" && p.better == Better::Higher));

        assert!(convert(Tool::Criterion, &[]).is_err() && convert(Tool::Hyperfine, &[("x".into(), "{".into())]).is_err());
        assert_eq!(Tool::parse("gbench"), Some(Tool::GoogleBenchmark));
    }
}
