//! Metrics from logs, live (docs/observe.md §8.3, §13): a kiln server and a runner as processes of
//! their own. Records come to the server's intake as an app sends them, and from a file the runner
//! reads; then late (for windows already counted), too late (past the twenty minutes), across a
//! restart of the server. Each window's points are held against the records sent (counts, sums,
//! the least and most, percentiles within the distribution's bound) and against what log analytics
//! counts of the records stored; a value above what was sent is a record counted twice, and fails
//! at once.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]

mod common;

use std::collections::BTreeMap;
use std::io::Write as _;
use std::time::{Duration, Instant};

use common::{Fleet, at, caught_up, now_ms};
use serde_json::{Value, json};

const WINDOW: i64 = 10_000;

/// A record as the test sent it.
#[derive(Debug, Clone)]
struct Sent {
    ts: i64,
    service: &'static str,
    error: bool,
    code: u16,
    env: &'static str,
    duration: f64,
    bytes: Option<f64>,
}

/// A small generator of its own (xorshift), so a failure repeats.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn f64(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A record of a time from `base` to `base + spread` (ms).
fn record(rng: &mut Rng, base: i64, spread: u64) -> Sent {
    let ts = base + rng.below(spread.max(1)) as i64;
    let service = if rng.below(2) == 0 { "checkout" } else { "search" };
    let error = rng.below(5) == 0;
    let code = if error { [500, 502, 503][rng.below(3) as usize] } else { [200, 200, 200, 404][rng.below(4) as usize] };
    // Latencies log-normal, as they are (a median of some 20 ms), to a microsecond.
    let n = (-2.0 * rng.f64().max(1e-300).ln()).sqrt() * (std::f64::consts::TAU * rng.f64()).cos();
    let duration = ((3.0 + 0.8 * n).exp() * 1000.0).round() / 1000.0;
    let bytes = (rng.below(3) != 0).then(|| (100 + rng.below(10_000)) as f64);
    Sent { ts, service, error, code, env: if rng.below(3) == 0 { "staging" } else { "prod" }, duration, bytes }
}

fn send(f: &Fleet, records: &[Sent]) -> anyhow::Result<()> {
    let body: Vec<Value> = records
        .iter()
        .map(|r| {
            let mut attrs = json!({ "http": { "status_code": r.code }, "env": r.env, "duration": r.duration });
            if let Some(b) = r.bytes {
                attrs["bytes"] = json!(b);
            }
            json!({ "message": format!("GET /{} {}", r.service, r.code), "timestamp": r.ts, "status": if r.error { "error" } else { "info" }, "service": r.service, "hostname": "app-1", "source": "app", "attributes": attrs })
        })
        .collect();
    let (code, b) = f.request("POST", "/api/observe/v1/logs", Some(&Value::Array(body)))?;
    anyhow::ensure!(code == 202, "the intake: {code} {}", String::from_utf8_lossy(&b));
    Ok(())
}

fn window(ts: i64) -> i64 {
    ts - ts.rem_euclid(WINDOW)
}

/// Each window's values of a query, by its group (the tags' values, joined), from `from` to `to`.
fn metric(f: &Fleet, q: &str, from: i64, to: i64) -> anyhow::Result<BTreeMap<(String, i64), f64>> {
    let a = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": q, "from": from, "to": to, "step": WINDOW })))?;
    let start = at(&a, &["grid", "from"]).as_i64().unwrap_or(from);
    let mut out = BTreeMap::new();
    for s in at(&a, &["series"]).as_array().into_iter().flatten() {
        let group = at(s, &["tags"]).as_object().map(|o| o.values().filter_map(Value::as_str).collect::<Vec<_>>().join(",")).unwrap_or_default();
        for (i, v) in at(s, &["values"]).as_array().into_iter().flatten().enumerate() {
            if let Some(v) = v.as_f64() {
                out.insert((group.clone(), start + i as i64 * WINDOW), v);
            }
        }
    }
    Ok(out)
}

/// What log analytics counts of the records stored, each window, by `by`.
fn analytics(f: &Fleet, query: &str, by: &[&str], from: i64, to: i64) -> anyhow::Result<BTreeMap<(String, i64), f64>> {
    let a = f.call("POST", "/api/observe/v1/logs/aggregate", Some(&json!({ "query": query, "from": from, "to": to - 1, "by": by, "step": WINDOW })))?;
    let start = at(&a, &["grid", "from"]).as_i64().unwrap_or(from);
    let mut out = BTreeMap::new();
    for s in at(&a, &["series"]).as_array().into_iter().flatten() {
        let group = at(s, &["group"]).as_object().map(|o| o.values().filter_map(Value::as_str).collect::<Vec<_>>().join(",")).unwrap_or_default();
        for (i, v) in at(s, &["values"]).as_array().into_iter().flatten().enumerate() {
            if let Some(v) = v.as_f64().filter(|v| *v > 0.0) {
                out.insert((group.clone(), start + i as i64 * WINDOW), v);
            }
        }
    }
    Ok(out)
}

/// Waits until `got()` equals `want` (zeros left out); a value above the one wanted fails at once:
/// a record counted twice never comes right.
fn holds(f: &Fleet, what: &str, want: &BTreeMap<(String, i64), f64>, within: Duration, got: impl Fn() -> anyhow::Result<BTreeMap<(String, i64), f64>>) -> anyhow::Result<()> {
    let start = Instant::now();
    let want: BTreeMap<(String, i64), f64> = want.iter().filter(|(_, v)| **v != 0.0).map(|(k, v)| (k.clone(), *v)).collect();
    loop {
        let g: BTreeMap<(String, i64), f64> = got()?.into_iter().filter(|(_, v)| *v != 0.0).collect();
        let over: Vec<_> = g.iter().filter(|(k, v)| want.get(*k).is_none_or(|w| **v > *w + 1e-6 * w.abs())).take(5).collect();
        anyhow::ensure!(over.is_empty(), "{what}: more than was sent, {over:?} (want {:?})", over.iter().map(|(k, _)| want.get(*k)).collect::<Vec<_>>());
        if g.len() == want.len() && g.iter().all(|(k, v)| want.get(k).is_some_and(|w| (v - w).abs() <= 1e-6 * w.abs().max(1.0))) {
            return Ok(());
        }
        if start.elapsed() > within {
            let short: Vec<_> = want.iter().filter(|(k, w)| g.get(*k).is_none_or(|v| (v - **w).abs() > 1e-6 * w.abs().max(1.0))).take(8).map(|(k, w)| (k.clone(), *w, g.get(k).copied())).collect();
            let counting = f.call("GET", "/api/observe/v1/log_metrics", None).map(|v| at(&v, &["counting"]).clone()).unwrap_or(Value::Null);
            anyhow::bail!("{what}: not within {within:?}; {} of {} windows short, the first (window, want, got) {short:?}; counting {counting:#}\n{}", short.len(), want.len(), f.logs());
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
}

/// Per window and group, what the records sent come to.
fn expect(records: &[Sent], keep: impl Fn(&Sent) -> Option<(String, f64)>) -> BTreeMap<(String, i64), Vec<f64>> {
    let mut out: BTreeMap<(String, i64), Vec<f64>> = BTreeMap::new();
    for r in records {
        if let Some((g, v)) = keep(r) {
            out.entry((g, window(r.ts))).or_default().push(v);
        }
    }
    out
}

fn of(e: &BTreeMap<(String, i64), Vec<f64>>, f: impl Fn(&[f64]) -> f64) -> BTreeMap<(String, i64), f64> {
    e.iter().map(|(k, v)| (k.clone(), f(v))).collect()
}

/// Every check of what the metrics hold, against `sent` (to the intake) and against what is stored.
fn check(f: &Fleet, sent: &[Sent], from: i64, to: i64, within: Duration) -> anyhow::Result<()> {
    let errors = expect(sent, |r| (r.service == "checkout" && r.error).then(|| (r.code.to_string(), 1.0)));
    holds(f, "checkout.errors by status code", &of(&errors, |v| v.len() as f64), within, || metric(f, "sum:checkout.errors{*} by {status_code}.as_count()", from, to))?;
    let latency = expect(sent, |r| (r.service == "checkout").then(|| (r.env.to_owned(), r.duration)));
    holds(f, "checkout.latency's count", &of(&latency, |v| v.len() as f64), within, || metric(f, "count:checkout.latency{*} by {env}", from, to))?;
    holds(f, "checkout.latency's sum", &of(&latency, |v| v.iter().sum()), within, || metric(f, "sum:checkout.latency{*} by {env}", from, to))?;
    holds(f, "checkout.latency's least", &of(&latency, |v| v.iter().copied().fold(f64::INFINITY, f64::min)), within, || metric(f, "min:checkout.latency{*} by {env}", from, to))?;
    holds(f, "checkout.latency's most", &of(&latency, |v| v.iter().copied().fold(f64::NEG_INFINITY, f64::max)), within, || metric(f, "max:checkout.latency{*} by {env}", from, to))?;
    // Percentiles: within the distribution's bound (1.09%) of the value of rank q(n - 1).
    let bound = (2f64.powi(-5) * 0.5).exp2() - 1.0;
    for (name, q) in [("p50", 0.5), ("p99", 0.99)] {
        let got = metric(f, &format!("{name}:checkout.latency{{*}} by {{env}}"), from, to)?;
        for (k, v) in &latency {
            let mut v = v.clone();
            v.sort_by(f64::total_cmp);
            let want = v[((v.len() - 1) as f64 * q).floor() as usize];
            let g = got.get(k).copied();
            anyhow::ensure!(g.is_some_and(|g| ((g - want) / want).abs() <= bound * 1.000_001), "{name} of {k:?}: {g:?} for {want} ({} values)", v.len());
        }
    }
    // A distribution without percentiles: its average is its sum over its count; no percentile.
    let bytes = expect(sent, |r| r.bytes.map(|b| (String::new(), b)));
    holds(f, "bytes' count", &of(&bytes, |v| v.len() as f64), within, || metric(f, "count:bytes{*}", from, to))?;
    let avg = metric(f, "avg:bytes{*}", from, to)?;
    for (k, v) in of(&bytes, |v| v.iter().sum::<f64>() / v.len() as f64) {
        anyhow::ensure!(avg.get(&k).is_some_and(|g| (g - v).abs() <= 1e-9 * v), "bytes' average at {k:?}: {:?} for {v}", avg.get(&k));
    }
    anyhow::ensure!(metric(f, "p50:bytes{*}", from, to)?.is_empty(), "bytes keeps no percentiles");
    // Every record, by service: the runner's too. Each window as log analytics counts the records
    // stored; the intake's as sent.
    let requests = expect(sent, |r| Some((r.service.to_owned(), 1.0)));
    let mine = |m: BTreeMap<(String, i64), f64>| -> BTreeMap<(String, i64), f64> { m.into_iter().filter(|((g, _), _)| g == "checkout" || g == "search").collect() };
    holds(f, "requests of the intake's services", &of(&requests, |v| v.len() as f64), within, || metric(f, "sum:requests{*} by {service}.as_count()", from, to).map(mine))?;
    like_stored(f, "requests, as log analytics counts the records stored", "sum:requests{*} by {service}.as_count()", "", &["service"], from, to, within)
}

/// Waits until a metric's windows are what log analytics counts of the records stored; asked
/// after the metric, analytics holds every record the metric counted, so a value above it is a
/// record counted twice, and fails at once.
#[allow(clippy::too_many_arguments)]
fn like_stored(f: &Fleet, what: &str, q: &str, search: &str, by: &[&str], from: i64, to: i64, within: Duration) -> anyhow::Result<()> {
    let start = Instant::now();
    loop {
        let m = metric(f, q, from, to)?;
        let a = analytics(f, search, by, from, to)?;
        let over: Vec<_> = m.iter().filter(|(k, v)| **v > a.get(*k).copied().unwrap_or(0.0)).take(5).collect();
        anyhow::ensure!(over.is_empty(), "{what}: more than stored, {over:?}");
        if m == a {
            return Ok(());
        }
        if start.elapsed() > within {
            let short: Vec<_> = a.iter().filter(|(k, v)| m.get(*k) != Some(*v)).take(8).map(|(k, v)| (k.clone(), *v, m.get(k).copied())).collect();
            anyhow::bail!("{what}: not within {within:?}; the first (window, stored, counted) {short:?}; counting {}\n{}", counting(f).unwrap_or(Value::Null), f.logs());
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
}

fn counting(f: &Fleet) -> anyhow::Result<Value> {
    Ok(at(&f.call("GET", "/api/observe/v1/log_metrics", None)?, &["counting"]).clone())
}

#[test]
#[ignore = "starts real processes"]
fn metrics_from_logs_count_each_window_once_through_late_records_and_a_restart() -> anyhow::Result<()> {
    let mut f = Fleet::new()?;
    f.start_runner("r1")?;
    f.until(Duration::from_secs(90), "r1 a learner, caught up", |s| caught_up(s, &["r1"]))?;

    // A metric of Datadog's, as their API takes it; the rest in kiln's shape.
    f.call("POST", "/api/observe/v1/log_metrics", Some(&json!({ "data": { "id": "checkout.errors", "type": "logs_metrics", "attributes": { "compute": { "aggregation_type": "count" }, "filter": { "query": "service:checkout status:error" }, "group_by": [{ "path": "@http.status_code", "tag_name": "status_code" }] } } })))?;
    f.call("POST", "/api/observe/v1/log_metrics", Some(&json!({ "name": "checkout.latency", "compute": { "aggregation_type": "distribution", "path": "@duration", "include_percentiles": true }, "filter": { "query": "service:checkout" }, "group_by": [{ "path": "@env" }] })))?;
    f.call("POST", "/api/observe/v1/log_metrics", Some(&json!({ "name": "bytes", "compute": { "aggregation_type": "distribution", "path": "@bytes" } })))?;
    f.call("POST", "/api/observe/v1/log_metrics", Some(&json!({ "name": "requests", "compute": { "aggregation_type": "count" }, "group_by": [{ "path": "service" }] })))?;
    // Refused: kiln's names, a change of what one computes, and series another writes.
    let (code, _) = f.request("POST", "/api/observe/v1/log_metrics", Some(&json!({ "name": "kiln.mine", "compute": { "aggregation_type": "count" } })))?;
    anyhow::ensure!(code == 400, "kiln.*: {code}");
    let (code, _) = f.request("PUT", "/api/observe/v1/log_metrics/bytes", Some(&json!({ "name": "bytes", "compute": { "aggregation_type": "count" } })))?;
    anyhow::ensure!(code == 400, "a change of what it computes: {code}");
    f.write("app.requests", &["h"], now_ms() - 60_000, 3, |_, i| i as f64)?;
    let start = Instant::now();
    while f.query("count:app.requests{*}", now_ms() - 120_000, now_ms())?.is_empty() {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(10), "the series written never stored");
        std::thread::sleep(Duration::from_millis(200));
    }
    let (code, b) = f.request("POST", "/api/observe/v1/log_metrics", Some(&json!({ "name": "app.requests", "compute": { "aggregation_type": "count" } })))?;
    anyhow::ensure!(code == 409, "a name whose series the API writes: {code} {}", String::from_utf8_lossy(&b));

    // The runner's file, read from its start.
    let dir = f.root.path().join("applogs");
    std::fs::create_dir_all(&dir)?;
    let file = dir.join("svc.log");
    std::fs::write(&file, "")?;
    let (code, b) = f.request("PUT", "/api/observe/v1/log_sources/svc", Some(&json!({ "type": "file", "paths": [file.display().to_string()], "start": "beginning", "service": "filesvc", "on": "r1" })))?;
    anyhow::ensure!(code == 204, "the log source: {code} {}", String::from_utf8_lossy(&b));

    // Records from the next window on (the metrics' first), as they come: 80 a second for 40
    // seconds, and lines to the runner's file beside them.
    // (A record is stamped up to 200 ms before it is sent: none before the first window.)
    let t0 = window(now_ms()) + WINDOW;
    while now_ms() < t0 + 300 {
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut sent: Vec<Sent> = Vec::new();
    let mut lines = 0u64;
    let mut out = std::fs::OpenOptions::new().append(true).open(&file)?;
    while now_ms() < t0 + 40_000 {
        let now = now_ms();
        let batch: Vec<Sent> = (0..20).map(|_| record(&mut rng, now - 200, 200)).collect();
        send(&f, &batch)?;
        sent.extend(batch);
        for _ in 0..5 {
            writeln!(out, "{}", json!({ "msg": "served", "n": lines }))?;
            lines += 1;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let to = window(now_ms()) + WINDOW;
    check(&f, &sent, t0, to, Duration::from_secs(60))?;
    let c = counting(&f)?;
    let settle = at(&c, &["settle_ms"]).as_i64().unwrap_or(0);
    anyhow::ensure!((2_000..=30_000).contains(&settle), "the settle, learned: {c:#}");
    eprintln!("on time: {c}");

    // Late: records for windows counted a while ago, all counted again; too late (25 minutes
    // back): stored, never counted.
    let late: Vec<Sent> = (0..400).map(|_| record(&mut rng, t0, 20_000)).collect();
    send(&f, &late)?;
    sent.extend(late);
    let too_late: Vec<Sent> = (0..50).map(|_| record(&mut rng, t0 - 25 * 60_000, 10_000)).collect();
    send(&f, &too_late)?;
    check(&f, &sent, t0, to, Duration::from_secs(60))?;
    let c = counting(&f)?;
    // Their two windows counted again, and no others: a batch of records old and new reaches its
    // records' windows, not every one between; and one late batch holds no window back.
    let again = at(&c, &["windows_recounted"]).as_u64().unwrap_or(0);
    anyhow::ensure!((2..=6).contains(&again), "the late records' windows counted again: {c:#}");
    anyhow::ensure!(at(&c, &["settle_ms"]).as_i64().is_some_and(|s| s < 10_000), "a late batch held the windows back: {c:#}");
    let back = metric(&f, "sum:requests{*}.as_count()", t0 - 26 * 60_000, t0 - 24 * 60_000)?;
    anyhow::ensure!(back.is_empty(), "records 25 minutes late counted: {back:?}");
    eprintln!("late: {c}");

    // A restart: what was counted stays as it was (counted again whole, as the server starts),
    // and records late for windows before it are counted with the rest.
    f.stop_gently();
    f.start_server()?;
    f.start_runner("r1")?;
    // The late ones sent with the first new ones, as an exporter back from a restart sends what it
    // held: one batch, as late as its newest (alone, a batch of them would say records come some
    // thirty seconds late, and be right to).
    let mut late: Option<Vec<Sent>> = Some((0..300).map(|_| record(&mut rng, t0, 40_000)).collect());
    let t1 = now_ms();
    while now_ms() < t1 + 15_000 {
        let now = now_ms();
        let mut batch: Vec<Sent> = late.take().unwrap_or_default();
        batch.extend((0..20).map(|_| record(&mut rng, now - 200, 200)));
        send(&f, &batch)?;
        sent.extend(batch);
        std::thread::sleep(Duration::from_millis(250));
    }
    let to = window(now_ms()) + WINDOW;
    check(&f, &sent, t0, to, Duration::from_secs(90))?;
    let c = counting(&f)?;
    // Entries applied again as the server opened say nothing of how late records come.
    anyhow::ensure!(at(&c, &["settle_ms"]).as_i64().is_some_and(|s| s < 10_000), "the entries applied again held the windows back: {c:#}");
    eprintln!("restarted: {c}");

    // What the test endpoint counts of the records held: the metric's own counts.
    let tried = f.call("POST", "/api/observe/v1/log_metric_test", Some(&json!({ "metric": { "name": "x", "compute": { "aggregation_type": "count" }, "filter": { "query": "service:checkout status:error" } }, "from": t0, "to": to - 1 })))?;
    let want = sent.iter().filter(|r| r.service == "checkout" && r.error).count() as u64;
    anyhow::ensure!(at(&tried, &["records"]).as_u64() == Some(want), "the test endpoint: {} of {want}", at(&tried, &["records"]));

    // Removed: counted no more.
    let (code, _) = f.request("DELETE", "/api/observe/v1/log_metrics/requests", None)?;
    anyhow::ensure!(code == 204, "delete: {code}");
    let gone = window(now_ms()) + 2 * WINDOW;
    while now_ms() < gone + 5_000 {
        let now = now_ms();
        send(&f, &[record(&mut rng, now, 1)])?;
        std::thread::sleep(Duration::from_millis(250));
    }
    std::thread::sleep(Duration::from_secs(35));
    let after = metric(&f, "sum:requests{*}.as_count()", gone, now_ms())?;
    anyhow::ensure!(after.is_empty(), "a metric removed is counted still: {after:?}");
    let listed = f.call("GET", "/api/observe/v1/log_metrics", None)?;
    let names: Vec<&str> = at(&listed, &["log_metrics"]).as_array().into_iter().flatten().filter_map(|m| at(m, &["name"]).as_str()).collect();
    anyhow::ensure!(names == ["checkout.errors", "checkout.latency", "bytes"], "{names:?}");
    let st = at(&listed, &["log_metrics"]).as_array().and_then(|a| a.first()).map(|m| at(m, &["status"]).clone()).unwrap_or(Value::Null);
    anyhow::ensure!(at(&st, &["window"]).as_i64().is_some_and(|w| w >= t0), "its status: {st:#}");
    f.stop_gently();
    Ok(())
}
