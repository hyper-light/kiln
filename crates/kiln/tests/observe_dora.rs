//! DORA metrics, live (docs/observe.md §10.7, §13): a kiln server as a process of its own, a git
//! repository it mirrors (kiln's `[[repos]]`), its history made with known author times: a feature
//! merged, a revert, a hotfix merged. Deployments of it sent to Datadog's API with the ingest token
//! are read as Datadog reads them: each commit's lead time from its author's time, merges left out;
//! the revert marks the deployment that brought what it reverts, the hotfix the one before it, a
//! rollback the one after the deployment it went back to, each with its recovery. The four keys over
//! the range, the lists, the metrics and the events say the same; a mark by hand and a failure with
//! its end sent later; what is refused, saying why; all of it the same once the server starts again.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::path::Path;
use std::process::Command;

use common::{Fleet, now_ms, raw};
use serde_json::{Value, json};

const HOUR: i64 = 3_600;

/// `git` in `dir` with its author and committer times `at` (s), failing on its failure: its stdout.
fn git(dir: &Path, at: i64, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_DATE", format!("@{at} +0000"))
        .env("GIT_COMMITTER_DATE", format!("@{at} +0000"))
        .env("GIT_AUTHOR_NAME", "e2e")
        .env("GIT_AUTHOR_EMAIL", "e2e@example.com")
        .env("GIT_COMMITTER_NAME", "e2e")
        .env("GIT_COMMITTER_EMAIL", "e2e@example.com")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A commit of `file` (its content `what`) titled `title`, at `at`: its sha.
fn commit(dir: &Path, at: i64, file: &str, what: &str, title: &str) -> String {
    std::fs::write(dir.join(file), what).unwrap();
    git(dir, at, &["add", file]);
    git(dir, at, &["commit", "-q", "-m", title]);
    git(dir, at, &["rev-parse", "HEAD"])
}

/// A deployment event as Datadog's API takes it.
fn deployment(id: &str, finished_s: i64, sha: &str, repo: &str) -> Value {
    json!({ "data": { "attributes": {
        "id": id, "service": "shop", "env": "prod", "team": "checkout", "version": format!("v-{}", &sha[..7]),
        "started_at": finished_s - 120, "finished_at": finished_s * 1000,
        "git": { "commit_sha": sha, "repository_url": repo },
    } } })
}

/// A request to Datadog's paths with `key` as its API key.
fn datadog(f: &Fleet, key: &str, method: &str, path: &str, body: &Value) -> (u16, Value) {
    let (code, b) = raw(f.http, method, path, &format!("DD-API-KEY: {key}\r\nContent-Type: application/json\r\n"), &serde_json::to_vec(body).unwrap()).unwrap();
    (code, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

fn attrs(d: &Value) -> &Value {
    &d["data"]["attributes"]
}

/// Whether `v` is the number `n` (a whole one or not).
fn is(v: &Value, n: i64) -> bool {
    #[allow(clippy::cast_precision_loss)]
    v.as_f64().is_some_and(|x| (x - n as f64).abs() < 1e-6)
}

/// A deployment as kiln's API shows it.
fn get(f: &Fleet, id: &str) -> anyhow::Result<Value> {
    f.call("GET", &format!("/api/observe/v1/dora/deployments/{id}"), None)
}

/// A deployment's mark and recovery.
fn failed(f: &Fleet, id: &str) -> anyhow::Result<(Value, Value)> {
    let a = attrs(&get(f, id)?).clone();
    Ok((a["change_failure"].clone(), a["recovery_s"].clone()))
}

/// The four keys over `from`..`to` (s), `q` more of the query.
fn summary(f: &Fleet, from: i64, to: i64, q: &str) -> anyhow::Result<Value> {
    f.call("GET", &format!("/api/observe/v1/dora/summary?from={}&to={}{q}", from * 1000, to * 1000), None)
}

/// Waits up to `secs` for `done`, or fails saying `what` with the server's logs.
fn until(f: &Fleet, secs: u64, what: &str, done: impl Fn() -> bool) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    while !done() {
        anyhow::ensure!(start.elapsed() < std::time::Duration::from_secs(secs), "never: {what}\n{}", f.logs());
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    Ok(())
}

#[test]
#[ignore = "starts real processes"]
fn deployments_are_read_as_datadogs_dora_metrics_reads_them() -> anyhow::Result<()> {
    let now = now_ms() / 1000;
    let t = now - now % HOUR;
    let at = |hours_ago: i64| t - hours_ago * HOUR;
    // The repository: c1; c2 "Add search", c3; a feature merged; c2 reverted; a hotfix merged.
    let tmp = tempfile::tempdir()?;
    let repo = tmp.path().join("shop");
    std::fs::create_dir_all(&repo)?;
    git(&repo, at(240), &["init", "-q", "-b", "main"]);
    let c1 = commit(&repo, at(240), "readme.txt", "shop", "Initial");
    let c2 = commit(&repo, at(50), "search.txt", "search", "Add search");
    let c3 = commit(&repo, at(30), "cart.txt", "fast", "Faster cart");
    git(&repo, at(20), &["checkout", "-q", "-b", "feature"]);
    commit(&repo, at(20), "feature.txt", "x", "Feature work");
    git(&repo, at(19), &["checkout", "-q", "main"]);
    git(&repo, at(19), &["merge", "-q", "--no-ff", "feature", "-m", "Merge branch 'feature'"]);
    let m1 = git(&repo, at(19), &["rev-parse", "HEAD"]);
    git(&repo, at(10), &["revert", "--no-edit", &c2]);
    let c5 = git(&repo, at(10), &["rev-parse", "HEAD"]);
    git(&repo, at(6), &["checkout", "-q", "-b", "hotfix/login"]);
    commit(&repo, at(6), "login.txt", "fixed", "Fix login");
    git(&repo, at(5), &["checkout", "-q", "main"]);
    git(&repo, at(5), &["merge", "-q", "--no-ff", "hotfix/login", "-m", "Merge branch 'hotfix/login'"]);
    let m2 = git(&repo, at(5), &["rev-parse", "HEAD"]);
    let url = format!("file://{}", repo.display());

    let mut f = Fleet::with(|_| format!("[[repos]]\nname = \"acme/shop\"\nclone_url = \"{url}\"\nbranches = [\"main\"]\npoll_seconds = 3600\n"))?;
    let ingest = std::fs::read_to_string(f.root.path().join("server/observe/ingest.token"))?.trim().to_owned();

    // What is refused, saying why.
    let (code, _) = datadog(&f, "not-the-token", "POST", "/api/v2/dora/deployment", &deployment("refused-deployment-1", at(1), &c1, &url));
    anyhow::ensure!(code == 403, "a key that is not one: {code}");
    for (body, says) in [
        (json!({ "data": { "attributes": { "service": "", "started_at": at(2), "finished_at": at(1) } } }), "service"),
        (json!({ "data": { "attributes": { "service": "shop", "id": "short", "started_at": at(2), "finished_at": at(1) } } }), "16 to 128"),
        (json!({ "data": { "attributes": { "service": "shop", "started_at": at(1), "finished_at": at(2) } } }), "after finished_at"),
        (json!({ "data": { "attributes": { "service": "shop", "started_at": at(2), "finished_at": (at(1) as i128 * 1_000_000) as i64 } } }), "nanoseconds"),
    ] {
        let (code, b) = datadog(&f, &ingest, "POST", "/api/v2/dora/deployment", &body);
        anyhow::ensure!(code == 400 && b.to_string().contains(says), "{body}: {code} {b}");
    }

    // The deployments, the third in by the kiln's name for the repository; the last a rollback to c3.
    let deploys = [
        ("dora-e2e-deploy-1", at(216), &c1, url.as_str()),
        ("dora-e2e-deploy-2", at(24), &c3, url.as_str()),
        ("dora-e2e-deploy-3", at(18), &m1, "acme/shop"),
        ("dora-e2e-deploy-4", at(9), &c5, url.as_str()),
        ("dora-e2e-deploy-5", at(4), &m2, url.as_str()),
        ("dora-e2e-deploy-6", at(2), &c3, url.as_str()),
    ];
    for (id, finished, sha, r) in deploys {
        let (code, b) = datadog(&f, &ingest, "POST", "/api/v2/dora/deployment", &deployment(id, finished, sha, r));
        anyhow::ensure!(code == 200 && b["data"]["id"] == id && b["data"]["type"] == "dora_deployment", "{id}: {code} {b}\n{}", f.logs());
    }
    // Lead times, from each commit's author's time to the deployment's finish, merges left out.
    let lead = |id: &str| -> anyhow::Result<Value> { Ok(attrs(&get(&f, id)?)["lead"].clone()) };
    anyhow::ensure!(lead("dora-e2e-deploy-1")?.is_null(), "the first deployment has none");
    let l2 = lead("dora-e2e-deploy-2")?;
    anyhow::ensure!(l2["commits"] == 2 && is(&l2["mean_s"], 16 * HOUR) && is(&l2["min_s"], 6 * HOUR) && is(&l2["max_s"], 26 * HOUR), "c2 and c3: {l2}");
    let l3 = lead("dora-e2e-deploy-3")?;
    anyhow::ensure!(l3["commits"] == 1 && is(&l3["mean_s"], 2 * HOUR), "the feature's commit, its merge left out (by the kiln's name for the repository): {l3}");
    anyhow::ensure!(is(&lead("dora-e2e-deploy-4")?["mean_s"], HOUR), "the revert");
    anyhow::ensure!(is(&lead("dora-e2e-deploy-5")?["mean_s"], 2 * HOUR), "the hotfix's commit");
    anyhow::ensure!(lead("dora-e2e-deploy-6")?.is_null(), "a rollback brings no commit");

    // What each marked failed, and its recovery.
    let (cf, rec) = failed(&f, "dora-e2e-deploy-2")?;
    anyhow::ensure!(
        cf["found"] == "revert" && cf["remediation"]["id"] == "dora-e2e-deploy-4" && cf["remediation"]["type"] == "rollforward" && is(&rec, 15 * HOUR),
        "the revert of c2 marks what brought it: {cf} {rec}"
    );
    let (cf, rec) = failed(&f, "dora-e2e-deploy-4")?;
    anyhow::ensure!(cf["found"] == "hotfix" && cf["remediation"]["id"] == "dora-e2e-deploy-5" && is(&rec, 5 * HOUR), "the hotfix marks the one before it: {cf} {rec}");
    let (cf, rec) = failed(&f, "dora-e2e-deploy-3")?;
    anyhow::ensure!(
        cf["found"] == "rollback" && cf["remediation"]["id"] == "dora-e2e-deploy-6" && cf["remediation"]["type"] == "rollback" && is(&rec, 16 * HOUR),
        "back to c3 marks the one after it: {cf} {rec}"
    );
    anyhow::ensure!(failed(&f, "dora-e2e-deploy-5")?.0.is_null() && failed(&f, "dora-e2e-deploy-1")?.0.is_null(), "the others stand");

    // The four keys over the last ten days.
    let from = (t - 240 * HOUR) * 1000;
    let s = summary(&f, t - 240 * HOUR, t, "")?;
    anyhow::ensure!(s["deployments"] == 6 && s["change_failures"] == 3 && s["failure_rate"] == 0.5 && s["recovered"] == 3, "{s}");
    anyhow::ensure!(is(&s["lead_time_s"], 2 * HOUR) && s["lead_deployments"] == 4, "the median of 16 h, 2 h, 1 h and 2 h: {s}");
    anyhow::ensure!(is(&s["recovery_s"], 15 * HOUR) && s["recovery_level"] == "high", "the median of 15 h, 5 h and 16 h: under a day: {s}");
    anyhow::ensure!(s["lead_time_level"] == "elite" && s["frequency_level"] == "high" && (s["per_day"].as_f64().unwrap() - 0.6).abs() < 1e-9, "six in ten days: daily to weekly: {s}");
    anyhow::ensure!(s["services"] == json!(["shop"]) && s["envs"] == json!(["prod"]) && s["teams"] == json!(["checkout"]), "{s}");
    anyhow::ensure!(summary(&f, t - 240 * HOUR, t, "&service=cart")?["deployments"] == 0, "another service has none");

    // The lists: a search of their fields, an order.
    let list = |body: Value| -> anyhow::Result<Vec<String>> {
        let b = f.call("POST", "/api/observe/v1/dora/deployments", Some(&body))?;
        Ok(b["data"].as_array().unwrap().iter().map(|d| d["id"].as_str().unwrap().to_owned()).collect())
    };
    let window = |q: &str, sort: &str| json!({ "from": from, "to": t * 1000, "query": q, "sort": sort });
    anyhow::ensure!(list(window("@change_failure:true", "finished_at"))? == ["dora-e2e-deploy-2", "dora-e2e-deploy-3", "dora-e2e-deploy-4"], "the failed ones, oldest first");
    anyhow::ensure!(list(window(&format!("@git.commit_sha:{c3}"), "-finished_at"))? == ["dora-e2e-deploy-6", "dora-e2e-deploy-2"], "c3's deployments, newest first");
    let (code, _) = f.request("POST", "/api/observe/v1/dora/deployments", Some(&json!({ "sort": "sideways" })))?;
    anyhow::ensure!(code == 400, "an order that is none: {code}");

    // The metrics (of the last two days: the five deployments, the three failures) and the events.
    let total = |q: &str| -> f64 {
        let b = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": q, "from": (t - 48 * HOUR) * 1000, "to": t * 1000 + 60_000, "step": 3_600_000 }))).unwrap_or(Value::Null);
        b["series"].as_array().into_iter().flatten().flat_map(|s| s["values"].as_array().into_iter().flatten().filter_map(Value::as_f64)).sum()
    };
    until(&f, 30, "the deployments counted", || total("sum:dora.deployments{service:shop}.as_count()") == 5.0)?;
    anyhow::ensure!(total("sum:dora.change_failures{service:shop,env:prod,team:checkout}.as_count()") == 3.0, "the change failures counted");
    let events = |q: &str| -> anyhow::Result<usize> {
        let b = f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": q, "from": from, "to": now_ms() + 60_000, "limit": 100 })))?;
        Ok(b["records"].as_array().map_or(0, Vec::len))
    };
    until(&f, 30, "the events", || events("source:dora @dora.kind:deployment service:shop").is_ok_and(|n| n == 6))?;
    anyhow::ensure!(events("source:dora @dora.kind:change_failure")? == 3, "an event each failure found");

    // By hand, through Datadog's path: the first failed, remedied by the second.
    let (code, b) = datadog(
        &f,
        &ingest,
        "PATCH",
        "/api/v2/dora/deployments/dora-e2e-deploy-1",
        &json!({ "data": { "attributes": { "change_failure": true, "remediation": { "id": "dora-e2e-deploy-2", "type": "rollforward" } } } }),
    );
    anyhow::ensure!(code == 200 && attrs(&b)["change_failure"]["found"] == "manual" && is(&attrs(&b)["recovery_s"], 192 * HOUR), "{code} {b}");
    let (code, _) =
        datadog(&f, &ingest, "PATCH", "/api/v2/dora/deployments/dora-e2e-deploy-1", &json!({ "data": { "attributes": { "remediation": { "id": "dora-e2e-deploy-1", "type": "rollback" } } } }));
    anyhow::ensure!(code == 400, "its own remediation: {code}");

    // A failure, then its end.
    let incident = |end: Option<i64>| json!({ "data": { "attributes": { "id": "dora-e2e-incident-1", "services": ["shop"], "env": "prod", "name": "checkout down", "severity": "high", "started_at": at(3), "finished_at": end } } });
    anyhow::ensure!(datadog(&f, &ingest, "POST", "/api/v2/dora/failure", &incident(None)).0 == 200);
    anyhow::ensure!(summary(&f, t - 240 * HOUR, t, "")?["open_failures"] == 1);
    anyhow::ensure!(datadog(&f, &ingest, "POST", "/api/v2/dora/incident", &incident(Some(at(3) + 1_800))).0 == 200, "its end, at Datadog's earlier path");
    let s = summary(&f, t - 240 * HOUR, t, "")?;
    anyhow::ensure!(s["failures"] == 1 && s["open_failures"] == 0 && is(&s["restore_s"], 1_800), "{s}");

    // The same once the server starts again.
    let before = (summary(&f, t - 240 * HOUR, t, "")?, get(&f, "dora-e2e-deploy-2")?, get(&f, "dora-e2e-deploy-1")?);
    f.kill_server();
    f.start_server()?;
    let after = (summary(&f, t - 240 * HOUR, t, "")?, get(&f, "dora-e2e-deploy-2")?, get(&f, "dora-e2e-deploy-1")?);
    anyhow::ensure!(before == after, "the ledger read back:\n{before:?}\n{after:?}");
    // Removed: gone, and so is what it remedied's remedy.
    anyhow::ensure!(datadog(&f, &ingest, "DELETE", "/api/v2/dora/deployment/dora-e2e-deploy-4", &Value::Null).0 == 204);
    anyhow::ensure!(f.request("GET", "/api/observe/v1/dora/deployments/dora-e2e-deploy-4", None)?.0 == 404);
    let (cf, rec) = failed(&f, "dora-e2e-deploy-2")?;
    anyhow::ensure!(cf["found"] == "revert" && cf["remediation"].is_null() && rec.is_null(), "failed still, its remedy gone: {cf} {rec}");
    Ok(())
}
