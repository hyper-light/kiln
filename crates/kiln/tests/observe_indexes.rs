//! Log indexes, live (docs/observe.md §8.10, §13): a kiln server and a runner as processes of their
//! own. `main` as kiln makes it; refusals saying why; `audit` made and put first, a daily quota of
//! 100 and a warning at 80%, then `payments`; `main` given exclusion filters (every debug record;
//! half of `web`'s).
//! Records through the server's API and the runner's OTLP receiver (routed there: the runner told
//! the indexes as it joins): each found in its index; those excluded not found by a search, and
//! still counted by a metric from logs and by Datadog's estimated usage; `audit`'s past its quota
//! not found, its warning and its reaching said as events once each, `is_rate_limited`; a search of
//! one index; a viewer reads indexes and may not change them; `audit` removed: its records no
//! longer found, its name refused again; the server stopped and started: the indexes, their parts
//! and their records as they were (`payments`' of a part of its own, let go by nothing but a
//! removal).
//!
//! `cargo test -p kiln --test observe_indexes -- --ignored --nocapture`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss)]

mod common;

use std::time::{Duration, Instant};

use common::{Fleet, at, caught_up, now_ms, raw};
use serde_json::{Value, json};

const CONFIG: &str = "/api/observe/v1/logs/config";

/// The records a search finds (1,000 at the most), of the indexes named (none: every one).
fn search(f: &Fleet, query: &str, from: i64, indexes: Option<&[&str]>) -> anyhow::Result<Vec<Value>> {
    let mut body = json!({ "query": query, "from": from, "to": now_ms() + 60_000, "limit": 1000 });
    if let Some(i) = indexes {
        body["indexes"] = json!(i);
    }
    Ok(f.call("POST", "/api/observe/v1/logs/search", Some(&body))?["records"].as_array().cloned().unwrap_or_default())
}

/// Waits up to `within` for `done`, saying what never came with the fleet's logs.
fn wait(f: &Fleet, what: &str, within: Duration, mut done: impl FnMut() -> anyhow::Result<bool>) -> anyhow::Result<()> {
    let start = Instant::now();
    loop {
        if done()? {
            return Ok(());
        }
        anyhow::ensure!(start.elapsed() < within, "never: {what}\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// A count query's total over the last ten minutes.
fn total(f: &Fleet, query: &str) -> anyhow::Result<f64> {
    let a = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": query, "from": now_ms() - 600_000, "to": now_ms() + 60_000, "step": 60_000 })))?;
    Ok(a["series"].as_array().into_iter().flatten().flat_map(|s| s["values"].as_array().cloned().unwrap_or_default()).filter_map(|v| v.as_f64()).sum())
}

/// An OTLP/JSON request of records of `service`, each its body and severity number.
fn otlp(service: &str, records: &[(String, u8)]) -> Vec<u8> {
    let ns = (now_ms() * 1_000_000).to_string();
    let recs: Vec<Value> = records.iter().map(|(body, sev)| json!({ "timeUnixNano": ns, "severityNumber": sev, "body": { "stringValue": body } })).collect();
    serde_json::to_vec(&json!({ "resourceLogs": [{ "resource": { "attributes": [{ "key": "service.name", "value": { "stringValue": service } }] }, "scopeLogs": [{ "logRecords": recs }] }] })).unwrap()
}

#[test]
#[ignore = "starts real processes"]
fn records_are_kept_by_their_index_excluded_or_over_its_quota_as_datadog_keeps_them() -> anyhow::Result<()> {
    let mut f = Fleet::new()?;
    f.until(Duration::from_secs(30), "the server leads", |s| at(s, &["leader"]).as_u64() == Some(1))?;
    let t0 = now_ms() - 60_000;

    // `main`, as kiln makes it: every record, of the main part.
    let listed = f.call("GET", &format!("{CONFIG}/indexes"), None)?;
    let main = &listed["indexes"][0];
    anyhow::ensure!(
        listed["indexes"].as_array().map(Vec::len) == Some(1) && main["name"] == "main" && main["filter"]["query"] == "*" && main["is_rate_limited"] == false && main["kiln"]["part"] == 0,
        "main alone: {listed}"
    );
    // Refused, saying why.
    for (body, says) in [
        (json!({ "name": "Audit", "filter": { "query": "*" } }), "lowercase"),
        (json!({ "name": "x", "filter": { "query": "*" }, "exclusion_filters": [{ "name": "e", "is_enabled": true, "filter": { "query": "*", "sample_rate": 1.5 } }] }), "sample rate"),
        (json!({ "name": "y", "filter": { "query": "*" }, "daily_limit": 10, "daily_limit_reset": { "reset_time": "14:30", "reset_utc_offset": "+00:00" } }), "reset_time"),
        (json!({ "name": "z", "filter": { "query": "service:(" } }), "query"),
        (json!({ "name": "main", "filter": { "query": "*" } }), "already"),
    ] {
        let (code, b) = f.request("POST", &format!("{CONFIG}/indexes"), Some(&body))?;
        anyhow::ensure!(code == 400 && String::from_utf8_lossy(&b).contains(says), "{body}: {code} {}", String::from_utf8_lossy(&b));
    }
    anyhow::ensure!(f.request("GET", &format!("{CONFIG}/indexes/nope"), None)?.0 == 404);

    // `audit`, first: its records' own, a hundred a day, warned at eighty.
    let audit = f.call(
        "POST",
        &format!("{CONFIG}/indexes"),
        Some(&json!({ "name": "audit", "filter": { "query": "service:audit" }, "daily_limit": 100, "daily_limit_warning_threshold_percentage": 80, "num_retention_days": 30, "tags": ["team:sec"] })),
    )?;
    anyhow::ensure!(audit["kiln"]["part"].as_u64().is_some_and(|p| p >= 1) && audit["num_retention_days"] == 30, "made: {audit}");
    let payments = f.call("POST", &format!("{CONFIG}/indexes"), Some(&json!({ "name": "payments", "filter": { "query": "service:payments" } })))?;
    anyhow::ensure!(payments["kiln"]["part"].as_u64().is_some_and(|p| p >= 1 && p != audit["kiln"]["part"].as_u64().unwrap_or(0)), "a part of its own: {payments}");
    anyhow::ensure!(f.request("PUT", &format!("{CONFIG}/index-order"), Some(&json!({ "index_names": ["main", "audit"] })))?.0 == 400, "an order of every index");
    let order = f.call("PUT", &format!("{CONFIG}/index-order"), Some(&json!({ "index_names": ["audit", "payments", "main"] })))?;
    anyhow::ensure!(order["index_names"] == json!(["audit", "payments", "main"]), "{order}");
    // `main` excludes every debug record, and half of web's.
    let main = f.call(
        "PUT",
        &format!("{CONFIG}/indexes/main"),
        Some(&json!({ "filter": { "query": "*" }, "exclusion_filters": [
            { "name": "debug", "is_enabled": true, "filter": { "query": "status:debug", "sample_rate": 1.0 } },
            { "name": "half of web", "is_enabled": true, "filter": { "query": "service:web", "sample_rate": 0.5 } },
        ] })),
    )?;
    anyhow::ensure!(main["exclusion_filters"].as_array().map(Vec::len) == Some(2), "{main}");
    // A metric from logs of every web record: those excluded counted too.
    f.call("POST", "/api/observe/v1/log_metrics", Some(&json!({ "name": "e2e.web.records", "compute": { "aggregation_type": "count" }, "filter": { "query": "service:web" } })))?;
    // A viewer reads the indexes and may not change them.
    let viewer = {
        let t = f.call("POST", "/api/tokens", Some(&json!({ "grants": ["viewer"], "user": "owner", "note": "e2e indexes viewer" })))?;
        at(&t, &["token"]).as_str().map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token: {t}"))?
    };
    let as_viewer = |method: &str, path: &str, body: &Value| -> anyhow::Result<u16> {
        Ok(raw(f.http, method, path, &format!("Authorization: Bearer {viewer}\r\nContent-Type: application/json\r\n"), &serde_json::to_vec(body)?)?.0)
    };
    anyhow::ensure!(as_viewer("GET", &format!("{CONFIG}/indexes"), &Value::Null)? == 200, "a viewer reads them");
    anyhow::ensure!(as_viewer("PUT", &format!("{CONFIG}/indexes/main"), &json!({ "filter": { "query": "*" } }))? == 403, "a viewer may not change them");

    // The runner joins, told the indexes as it connects.
    f.start_runner_with("r1", &[("KILN_OBSERVE_HOST_SECONDS", "2")])?;
    f.until(Duration::from_secs(90), "r1 a learner, caught up", |s| caught_up(s, &["r1"]))?;

    // Through the server's API: 400 of web's, 20 debug records, then audit's 150, ten at a time.
    let web: Vec<Value> = (0..400).map(|i| json!({ "message": format!("web request {i}"), "service": "web", "status": "info" })).collect();
    anyhow::ensure!(f.request("POST", "/api/observe/v1/logs", Some(&Value::Array(web)))?.0 == 202);
    let debug: Vec<Value> = (0..20).map(|i| json!({ "message": format!("api detail {i}"), "service": "api", "status": "debug" })).collect();
    anyhow::ensure!(f.request("POST", "/api/observe/v1/logs", Some(&Value::Array(debug)))?.0 == 202);
    let charges: Vec<Value> = (0..30).map(|i| json!({ "message": format!("charge {i}"), "service": "payments", "status": "info" })).collect();
    anyhow::ensure!(f.request("POST", "/api/observe/v1/logs", Some(&Value::Array(charges)))?.0 == 202);
    for round in 0..15 {
        let batch: Vec<Value> = (0..10).map(|i| json!({ "message": format!("audit entry {}", round * 10 + i), "service": "audit", "status": "info" })).collect();
        anyhow::ensure!(f.request("POST", "/api/observe/v1/logs", Some(&Value::Array(batch)))?.0 == 202);
        std::thread::sleep(Duration::from_millis(400));
    }
    // Through the runner's OTLP receiver, routed there: its debug records excluded, its info kept.
    let r1 = *f.otlp.get("r1").ok_or_else(|| anyhow::anyhow!("r1's receiver"))?;
    let runner: Vec<(String, u8)> = (0..20).map(|i| (format!("runner line {i}"), if i % 2 == 0 { 5 } else { 9 })).collect();
    let (code, _) = raw(r1, "POST", "/v1/logs", "Content-Type: application/json\r\n", &otlp("runner-app", &runner))?;
    anyhow::ensure!(code == 200, "the runner's receiver: {code}");

    // Each where its index keeps it.
    wait(&f, "the runner's records found", Duration::from_secs(60), || Ok(search(&f, "service:runner-app", t0, None)?.len() == 10))?;
    let runner_found = search(&f, "service:runner-app", t0, None)?;
    anyhow::ensure!(runner_found.iter().all(|r| r["status"] == "info"), "the runner's debug records excluded there: {runner_found:?}");
    anyhow::ensure!(search(&f, "service:api", t0, None)?.is_empty(), "every debug record excluded");
    let web_kept = search(&f, "service:web", t0, None)?.len();
    anyhow::ensure!((150..=250).contains(&web_kept), "about half of web's kept: {web_kept}");
    let audit_kept = search(&f, "service:audit", t0, None)?.len();
    anyhow::ensure!((100..=120).contains(&audit_kept), "audit's quota of 100, a write or two past it at the most: {audit_kept}");
    anyhow::ensure!(search(&f, "service:payments", t0, None)?.len() == 30, "payments' every one");
    anyhow::ensure!(search(&f, "*", t0, Some(&["payments"]))?.len() == 30, "payments' alone in it");
    // A search of one index; of an index that is not, refused.
    anyhow::ensure!(search(&f, "*", t0, Some(&["audit"]))?.iter().all(|r| r["service"] == "audit"), "audit's alone");
    anyhow::ensure!(search(&f, "service:audit", t0, Some(&["main"]))?.is_empty(), "none of audit's in main");
    let (code, _) = f.request("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": "*", "from": t0, "to": now_ms(), "indexes": ["nope"] })))?;
    anyhow::ensure!(code == 400, "an index that is not: {code}");

    // Over its quota: said so, its warning and its reaching each an event, once.
    let audit = f.call("GET", &format!("{CONFIG}/indexes/audit"), None)?;
    anyhow::ensure!(audit["is_rate_limited"] == true && audit["kiln"]["today"]["count"].as_u64().is_some_and(|c| c >= 100) && audit["kiln"]["today"]["limit"] == 100, "over its quota: {audit}");
    wait(&f, "the quota's events", Duration::from_secs(30), || Ok(search(&f, "source:kiln.event @event.type:logs.quota @datadog_index:audit", t0, None)?.len() >= 2))?;
    let said = search(&f, "source:kiln.event @event.type:logs.quota @datadog_index:audit", t0, None)?;
    anyhow::ensure!(
        said.len() == 2 && said.iter().any(|e| e["message"].as_str().is_some_and(|m| m.contains("warning"))) && said.iter().any(|e| e["message"].as_str().is_some_and(|m| m.contains("reached"))),
        "{said:?}"
    );

    // Those not kept for search still counted: every web record by the metric from logs, and by
    // Datadog's estimated usage, each by its index and whether excluded.
    wait(&f, "every web record counted", Duration::from_secs(90), || Ok(total(&f, "sum:e2e.web.records{*}.as_count()")? >= 400.0))?;
    anyhow::ensure!(total(&f, "sum:e2e.web.records{*}.as_count()")? == 400.0, "each once");
    let usage = |index: &str, excluded: &str| total(&f, &format!("sum:datadog.estimated_usage.logs.ingested_events{{datadog_index:{index},datadog_is_excluded:{excluded}}}.as_count()"));
    wait(&f, "the usage written", Duration::from_secs(60), || {
        Ok(usage("audit", "false")? >= 150.0 && usage("payments", "false")? >= 30.0 && usage("main", "true")? + usage("main", "false")? >= 440.0)
    })?;
    anyhow::ensure!(usage("audit", "false")? == 150.0, "audit's 150, those over its quota too");
    anyhow::ensure!(usage("payments", "false")? == 30.0, "payments' 30");
    let (kept, excluded) = (usage("main", "false")?, usage("main", "true")?);
    anyhow::ensure!(kept + excluded == 440.0 && excluded >= 150.0 + 20.0 + 10.0, "main's: {kept} kept, {excluded} excluded");

    // `audit` removed: its records no longer found, its name not used again.
    anyhow::ensure!(f.request("DELETE", &format!("{CONFIG}/indexes/audit"), None)?.0 == 200);
    anyhow::ensure!(search(&f, "service:audit", t0, None)?.is_empty(), "its records gone from search");
    let (code, _) = f.request("POST", &format!("{CONFIG}/indexes"), Some(&json!({ "name": "audit", "filter": { "query": "*" } })))?;
    anyhow::ensure!(code == 400, "its name not used again: {code}");
    anyhow::ensure!(f.call("GET", &format!("{CONFIG}/index-order"), None)?["index_names"] == json!(["payments", "main"]));

    // Stopped and started: the indexes and what they kept as they were.
    let before = (search(&f, "service:web", t0, None)?.len(), search(&f, "service:runner-app", t0, None)?.len(), search(&f, "service:payments", t0, None)?.len());
    f.stop_gently();
    f.start_server()?;
    f.until(Duration::from_secs(60), "the server leads again", |s| at(s, &["leader"]).as_u64() == Some(1))?;
    let listed = f.call("GET", &format!("{CONFIG}/indexes"), None)?;
    let names: Vec<&str> = listed["indexes"].as_array().into_iter().flatten().filter_map(|i| i["name"].as_str()).collect();
    let main = listed["indexes"].as_array().into_iter().flatten().find(|i| i["name"] == "main");
    anyhow::ensure!(names == ["payments", "main"] && main.is_some_and(|m| m["exclusion_filters"].as_array().map(Vec::len) == Some(2)), "kept: {listed}");
    // The leader states parts' cutoffs as it begins to lead (then each minute), read at once: a
    // moment on, nothing but the removed index's let go.
    std::thread::sleep(Duration::from_secs(5));
    let after = (search(&f, "service:web", t0, None)?.len(), search(&f, "service:runner-app", t0, None)?.len(), search(&f, "service:payments", t0, None)?.len());
    anyhow::ensure!(after == before, "the records as they were: {before:?} then {after:?}");
    anyhow::ensure!(search(&f, "service:audit", t0, None)?.is_empty(), "the removed index's still not found");
    Ok(())
}
