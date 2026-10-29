//! The Service Catalog, live (docs/observe.md §10.8, §13): a kiln server as a process of its own,
//! mirroring a git repository whose `service.datadog.yaml` defines two services; a third defined
//! through Datadog's API; a fourth only seen, by its traces. Each listed with where its definition
//! came from; the repository's defining one a monitor reads, an SLO reads, a deployment (its team the
//! catalog's) and logs carrying its traces' ids: every scorecard rule of it passed. What is refused,
//! saying why; the API's definitions kept, and the repository's read again, once the server starts
//! again.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Fleet, file_url, now_ms, raw};
use serde_json::{Value, json};

const DEFINED: &str = r#"
schema-version: v2.2
dd-service: shop-api
team: checkout
description: Takes orders.
tier: "1"
lifecycle: production
type: web
contacts:
  - name: Checkout
    type: email
    contact: checkout@example.com
links:
  - name: Runbook
    type: runbook
    url: https://wiki.example.com/shop/runbook
integrations:
  pagerduty:
    service-url: https://acme.pagerduty.com/service-directory/PSHOP12
---
schema-version: v2.2
dd-service: shop-worker
team: checkout
"#;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "e2e")
        .env("GIT_AUTHOR_EMAIL", "e2e@example.com")
        .env("GIT_COMMITTER_NAME", "e2e")
        .env("GIT_COMMITTER_EMAIL", "e2e@example.com")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// A request to Datadog's paths, the key `key`, a body of `content_type`.
fn datadog(f: &Fleet, key: &str, method: &str, path: &str, content_type: &str, body: &[u8]) -> (u16, String) {
    let (code, b) = raw(f.http, method, path, &format!("DD-API-KEY: {key}\r\nContent-Type: {content_type}\r\n"), body).unwrap();
    (code, String::from_utf8_lossy(&b).into_owned())
}

/// OTLP/JSON spans of `service`: `n` requests (server spans) ending now, one in four an error, each
/// its own trace.
fn spans(service: &str, n: u64) -> Vec<u8> {
    let now = now_ms() as u64 * 1_000_000;
    let spans: Vec<Value> = (0..n)
        .map(|i| {
            json!({
                "traceId": format!("{:032x}", 0xca7a_0000_u64 + i), "spanId": format!("{:016x}", 0x5a_0000_u64 + i),
                "name": "GET /cart", "kind": 2,
                "startTimeUnixNano": (now - 50_000_000 - i * 1_000).to_string(), "endTimeUnixNano": (now - i * 1_000).to_string(),
                "status": if i % 4 == 0 { json!({ "code": 2 }) } else { json!({}) },
            })
        })
        .collect();
    serde_json::to_vec(&json!({ "resourceSpans": [{ "resource": { "attributes": [{ "key": "service.name", "value": { "stringValue": service } }] }, "scopeSpans": [{ "spans": spans }] }] })).unwrap()
}

/// Waits up to a minute for `done` of the catalog's list, or fails with the server's logs.
fn listed(f: &Fleet, what: &str, done: impl Fn(&[Value]) -> bool) -> anyhow::Result<Vec<Value>> {
    let start = Instant::now();
    loop {
        let all = f.call("GET", "/api/observe/v1/catalog", None)?["services"].as_array().cloned().unwrap_or_default();
        if done(&all) {
            return Ok(all);
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "never: {what}\n{all:?}\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn one<'a>(all: &'a [Value], name: &str) -> Option<&'a Value> {
    all.iter().find(|s| s["service"] == name)
}

#[test]
#[ignore = "starts real processes"]
fn services_are_defined_seen_and_scored() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let repo = tmp.path().join("shop");
    std::fs::create_dir_all(&repo)?;
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("service.datadog.yaml"), DEFINED)?;
    git(&repo, &["add", "service.datadog.yaml"]);
    git(&repo, &["commit", "-q", "-m", "Define the shop's services"]);
    let url = file_url(&repo);
    let mut f = Fleet::with_env(|_| format!("[[repos]]\nname = \"acme/shop\"\nclone_url = \"{url}\"\nbranches = [\"main\"]\npoll_seconds = 2\n"), &[("KILN_OBSERVE_CATALOG_EVERY_MS", "1000")])?;
    let ingest = std::fs::read_to_string(f.root.path().join("server/observe/ingest.token"))?.trim().to_owned();

    // The repository's two, once kiln mirrors it.
    let all = listed(&f, "the repository's services", |all| one(all, "shop-api").is_some() && one(all, "shop-worker").is_some())?;
    let api = one(&all, "shop-api").unwrap();
    anyhow::ensure!(api["source"] == "repository" && api["repo"] == "acme/shop" && api["team"] == "checkout" && api["tier"] == "1" && api["type"] == "web", "{api}");

    // One defined through Datadog's API (YAML); what is refused, saying why.
    let (code, b) = datadog(&f, &ingest, "POST", "/api/v2/services/definitions", "application/x-yaml", b"schema-version: v2.2\ndd-service: billing\nteam: payments\n");
    anyhow::ensure!(code == 200 && b.contains("\"ingestion-source\":\"api\""), "{code} {b}");
    for (body, code_wanted, says) in [
        (&b"schema-version: v2.2\ndd-service: shop-api\nteam: someone-else\n"[..], 409, "acme/shop"),
        (&b"schema-version: v2.2\ndd-service: x\nowner: me\n"[..], 400, "owner"),
        (&b"schema-version: v2.2\ndd-service: x\ncontacts: [{type: email, contact: nobody}]\n"[..], 400, "email address"),
    ] {
        let (code, b) = datadog(&f, &ingest, "POST", "/api/v2/services/definitions", "application/x-yaml", body);
        anyhow::ensure!(code == code_wanted && b.contains(says), "{}: {code} {b}", String::from_utf8_lossy(body));
    }
    anyhow::ensure!(datadog(&f, "not-the-token", "POST", "/api/v2/services/definitions", "application/x-yaml", b"schema-version: v2.2\ndd-service: y\n").0 == 403);
    anyhow::ensure!(datadog(&f, &ingest, "GET", "/api/v2/services/definitions", "application/json", b"").0 == 403, "read through kiln's API");

    // One only seen, by its traces.
    let (code, _) = raw(f.http, "POST", "/v1/traces", &format!("Authorization: Bearer {ingest}\r\nContent-Type: application/json\r\n"), &spans("cart", 40))?;
    anyhow::ensure!(code == 200, "the spans: {code}");

    // The repository's shop-api: a monitor and an SLO that read it, a deployment that names no team,
    // logs that carry a trace id.
    let m = f.call(
        "POST",
        "/api/observe/v1/monitors",
        Some(&json!({ "name": "shop-api latency", "type": "metric alert", "query": "avg(last_5m):avg:trace.http.request.duration{service:shop-api} > 5", "message": "slow" })),
    )?;
    let slo = f.call(
        "POST",
        "/api/observe/v1/slos",
        Some(&json!({ "name": "shop-api availability", "type": "ratio", "good": "sum:trace.http.request.hits{service:shop-api,http.status_code:200}.as_count()", "total": "sum:trace.http.request.hits{service:shop-api}.as_count()", "target": 99.9, "timeframe": "30d", "tags": ["service:shop-api"] })),
    )?;
    let finished = now_ms() / 1000 - 600;
    let (code, b) = datadog(
        &f,
        &ingest,
        "POST",
        "/api/v2/dora/deployment",
        "application/json",
        serde_json::to_vec(
            &json!({ "data": { "attributes": { "id": "catalog-e2e-deploy-0001", "service": "shop-api", "env": "prod", "version": "1.4.0", "started_at": finished - 60, "finished_at": finished } } }),
        )?
        .as_slice(),
    );
    anyhow::ensure!(code == 200, "the deployment: {code} {b}");
    // Logs that carry a trace's ids come over OTLP (as an SDK's log bridge sends them).
    let logs = json!({ "resourceLogs": [{ "resource": { "attributes": [{ "key": "service.name", "value": { "stringValue": "shop-api" } }] }, "scopeLogs": [{ "logRecords": [{
        "timeUnixNano": (now_ms() * 1_000_000).to_string(), "severityNumber": 9, "body": { "stringValue": "order placed" },
        "traceId": format!("{:032x}", 0xabc_u64), "spanId": format!("{:016x}", 0xdef_u64),
    }] }] }] });
    let (code, _) = raw(f.http, "POST", "/v1/logs", &format!("Authorization: Bearer {ingest}\r\nContent-Type: application/json\r\n"), &serde_json::to_vec(&logs)?)?;
    anyhow::ensure!(code == 200, "the logs: {code}");
    let dep = f.call("GET", "/api/observe/v1/dora/deployments/catalog-e2e-deploy-0001", None)?;
    anyhow::ensure!(dep["data"]["attributes"]["team"] == "checkout", "a deployment that names no team takes the catalog's: {dep}");

    // Every one listed, where it came from; cart seen by its traces, its requests counted.
    let all = listed(&f, "cart seen, billing defined", |all| one(all, "billing").is_some() && one(all, "cart").is_some_and(|c| c["requests"].as_f64().unwrap_or(0.0) > 0.0))?;
    let cart = one(&all, "cart").unwrap();
    anyhow::ensure!(cart["source"] == "seen" && cart["team"].is_null() && cart["error_rate"].as_f64().is_some_and(|r| r > 0.2 && r < 0.3), "a quarter of cart's requests failed: {cart}");
    anyhow::ensure!(one(&all, "billing").unwrap()["source"] == "api", "billing from the API");

    // shop-api's page: its definition, what reads it, its deployment, every rule passed.
    let start = Instant::now();
    let page = loop {
        let p = f.call("GET", "/api/observe/v1/catalog/shop-api", None)?;
        if p["score"]["passed"] == 10 {
            break p;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "shop-api's rules: {}\n{}", p["rules"], f.logs());
        std::thread::sleep(Duration::from_millis(500));
    };
    anyhow::ensure!(page["monitor_list"][0]["id"] == m["id"] && page["slo_list"][0]["id"] == slo["id"], "{page}");
    anyhow::ensure!(page["deployments"][0]["id"] == "catalog-e2e-deploy-0001" && page["deployments_30d"] == 1, "{page}");
    anyhow::ensure!(page["definition"]["attributes"]["schema"]["contacts"][0]["contact"] == "checkout@example.com", "{page}");
    // shop-worker, defined in the repository and nothing else: its team and repository, no more.
    let w = f.call("GET", "/api/observe/v1/catalog/shop-worker", None)?;
    let passed: Vec<&str> = w["rules"].as_array().unwrap().iter().filter(|r| r["passed"] == true).map(|r| r["id"].as_str().unwrap()).collect();
    anyhow::ensure!(passed == ["team", "repos"], "shop-worker passes {passed:?}");
    anyhow::ensure!(f.request("GET", "/api/observe/v1/catalog/nothing-here", None)?.0 == 404);

    // The server started again: the API's kept, the repository's read again.
    f.kill_server();
    f.start_server()?;
    let all = listed(&f, "every definition back", |all| ["billing", "shop-api", "shop-worker"].iter().all(|n| one(all, n).is_some()))?;
    anyhow::ensure!(one(&all, "billing").unwrap()["team"] == "payments");
    // Removed: billing from the API; shop-api, the repository's, refused.
    anyhow::ensure!(datadog(&f, &ingest, "DELETE", "/api/v2/services/definitions/billing", "application/json", b"").0 == 204);
    anyhow::ensure!(datadog(&f, &ingest, "DELETE", "/api/v2/services/definitions/billing", "application/json", b"").0 == 404);
    let (code, b) = datadog(&f, &ingest, "DELETE", "/api/v2/services/definitions/shop-api", "application/json", b"");
    anyhow::ensure!(code == 409 && b.contains("service.datadog.yaml"), "{code} {b}");
    Ok(())
}
