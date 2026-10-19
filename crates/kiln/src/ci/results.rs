//! Test results (docs/ci.md W16): reports a step sends (`test-results:`), kept in the store and
//! parsed here into tests and their results (`kiln_ci::results`); failures annotated where they
//! are; quarantine judged against trunk's `.kiln/quarantine.yaml`; and what the run page and a
//! repository's tests page read.
//!
//! - `PUT /_kiln/test-results?step=&format=&path=&root=`: a report, with the leg's runtime token.
//!   Answers its counts, its failures and quarantine's verdict, which the runner acts on.
//! - `GET /api/ci/runs/:id/tests`, `GET /api/ci/legs/:id/tests`: the tests, failures first, each
//!   with its message, trace, time, quarantine and flakiness.
//! - `GET /api/ci/repos/:repo/tests?sort=failing|slowest|flaky&branch=&days=`: tests across runs.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use http::StatusCode;
use kiln_ci::results::{self, Case, Flakiness, Format, Outcome, Seen, quarantine};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use super::services::{blob, cas};
use super::{Fail, token};
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

/// The largest report taken.
const MAX_REPORT: u64 = 64 << 20;
/// Failures annotated per report, at most (GitHub shows ten per step; more help nobody).
const MAX_ANNOTATIONS: usize = 50;
/// How far back flakiness looks: results per test.
const WINDOW: usize = 30;
/// A test flipping in more than this share of consecutive runs is probably flaky (W16-R04).
const FLIP_RATE: f64 = 0.2;

fn internal(e: &anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// The leg a report belongs to.
struct LegOf {
    run: String,
    key: String,
    /// The run's attempt the leg is of.
    attempt: i64,
    repo: String,
    branch: String,
    sha: String,
}

async fn leg_of(hub: &Hub, leg: &str) -> anyhow::Result<Option<LegOf>> {
    let leg = leg.to_owned();
    hub.store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT j.run_id, COALESCE(j.key, j.id), COALESCE(j.run_attempt, 1), r.repo_id, r.branch, r.sha FROM jobs j JOIN runs r ON r.id = j.run_id WHERE j.id = ?1",
                [&leg],
                |r| Ok(LegOf { run: r.get(0)?, key: r.get(1)?, attempt: r.get(2)?, repo: r.get(3)?, branch: r.get(4)?, sha: r.get(5)? }),
            )
            .optional()?)
        })
        .await
}

/// Trunk's quarantine list, and why it couldn't be read.
async fn quarantine_of(hub: &Hub, repo_id: &str) -> (Vec<quarantine::Entry>, Option<String>) {
    let Ok(repos) = hub.store.repos().await else { return (Vec::new(), None) };
    let Some(repo) = repos.into_iter().find(|r| r.id == repo_id) else { return (Vec::new(), None) };
    match super::repo_config::trunk_file(hub, &repo, &[".kiln/quarantine.yaml", ".kiln/quarantine.yml"]).await {
        Some((_, text)) => match quarantine::parse(&String::from_utf8_lossy(&text)) {
            Ok(list) => (list, None),
            Err(e) => (Vec::new(), Some(e)),
        },
        None => (Vec::new(), None),
    }
}

/// A report's path as the repository sees it: `root` (the leg's workspace) taken off.
fn relative(path: &str, root: Option<&str>) -> String {
    let p = path.replace('\\', "/");
    match root.map(|r| r.trim_end_matches('/')).filter(|r| !r.is_empty()) {
        Some(r) => p.strip_prefix(r).map(|x| x.trim_start_matches('/').to_owned()).unwrap_or(p),
        None => p,
    }
}

/// `PUT /_kiln/test-results`.
pub async fn ingest(hub: &Arc<Hub>, req: Request) -> Result<Response, Fail> {
    let Some(claims) = token::of_request(hub, req.headers()) else { return Err((StatusCode::UNAUTHORIZED, "a leg's runtime token is needed".into())) };
    let q = srv::query(&req);
    let step: Option<u32> = q.get("step").and_then(|s| s.parse().ok());
    let format = match q.get("format").filter(|f| !f.is_empty()) {
        Some(f) => Some(Format::parse(f).ok_or_else(|| (StatusCode::BAD_REQUEST, format!("`{f}` isn't a format kiln reads: {}", Format::ALL.map(Format::name).join(", "))))?),
        None => None,
    };
    let path = q.get("path").cloned().unwrap_or_default();
    let root = q.get("root").cloned();
    let staged = cas::staging(hub).join(format!("report-{}", crate::util::new_id()));
    if let Some(dir) = staged.parent() {
        std::fs::create_dir_all(dir).map_err(|e| internal(&e.into()))?;
    }
    let received = blob::receive(req, &staged, MAX_REPORT).await;
    let cleanup = |p: &std::path::Path| {
        let _ = std::fs::remove_file(p);
    };
    if let Err(e) = received {
        cleanup(&staged);
        return Err((StatusCode::BAD_REQUEST, format!("the report: {e:#}")));
    }
    let text = std::fs::read(&staged).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    let object = cas::put_file(hub, &staged).await;
    cleanup(&staged);
    let object = object.map_err(|e| internal(&e))?;
    let mut report = match results::parse(&text, format) {
        Ok(r) => r,
        Err(e) => return Err((StatusCode::UNPROCESSABLE_ENTITY, format!("{path}: {e}"))),
    };
    for c in &mut report.cases {
        c.file = c.file.as_deref().map(|f| relative(f, root.as_deref()));
    }
    let leg = claims.sub.clone();
    let Some(of) = leg_of(hub, &leg).await.map_err(|e| internal(&e))? else { return Err((StatusCode::NOT_FOUND, "the leg is gone".into())) };
    let repo_id = of.repo.clone();
    let now = crate::util::now_unix();
    let (entries, bad_list) = quarantine_of(hub, &of.repo).await;
    let verdict = quarantine::judge(&entries, &report.cases, now);
    let quarantined: std::collections::HashSet<String> = report.cases.iter().filter(|c| c.outcome.is_failure() && entries.iter().any(|e| e.active(now) && e.matches(c))).map(Case::id).collect();
    let cases = report.cases.clone();
    let (fmt, obj, p, l) = (report.format.name().to_owned(), object.digest.clone(), path.clone(), leg.clone());
    let failed = cases.iter().filter(|c| c.outcome.is_failure()).count();
    let report_id: i64 = hub
        .store
        .call(move |c| {
            let tx = c.transaction()?;
            tx.execute(
                "INSERT INTO test_reports (leg, step_idx, path, format, object, cases, failed, ts) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![l, step, p, fmt, obj, i64::try_from(cases.len()).unwrap_or(i64::MAX), i64::try_from(failed).unwrap_or(i64::MAX), now],
            )?;
            let report = tx.last_insert_rowid();
            let mut annotated = 0usize;
            for case in &cases {
                let id = case.id();
                tx.execute("INSERT OR IGNORE INTO test_cases (repo_id, id, suite, name, file, first_seen) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![of.repo, id, case.suite, case.name, case.file, now])?;
                let q = quarantined.contains(&id);
                tx.execute(
                    "INSERT INTO test_results (repo_id, case_id, run_id, leg, leg_key, attempt, branch, sha, report, outcome, duration_ms, message, trace, line, retried, quarantined, ts)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
                    params![of.repo, id, of.run, l, of.key, of.attempt, of.branch, of.sha, report, case.outcome.as_str(), case.duration_ms.and_then(|d| i64::try_from(d).ok()), case.message, case.trace, case.line, case.retried, q, now],
                )?;
                // Failures at their file and line (W16-R03), a quarantined one as a warning.
                if case.outcome.is_failure() && annotated < MAX_ANNOTATIONS {
                    annotated += 1;
                    let title = if case.suite.is_empty() { case.name.clone() } else { format!("{} {}", case.suite, case.name) };
                    let msg = case.message.clone().or_else(|| case.trace.as_deref().and_then(|t| t.lines().next()).map(str::to_owned)).unwrap_or_else(|| "failed".into());
                    let msg = if q { format!("{msg} (quarantined)") } else { msg };
                    tx.execute(
                        "INSERT INTO ci_annotations (leg_id, step_idx, level, title, message, file, line) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![l, step, if q { "warning" } else { "error" }, title, msg, case.file, case.line],
                    )?;
                }
            }
            tx.commit()?;
            Ok(report)
        })
        .await
        .map_err(|e| internal(&e))?;
    hub.emit_job(&leg).await;
    // The repository's next run shards by these times too (W16-R06).
    TIMING.lock().remove(&repo_id);
    let label = |c: &Case| if c.suite.is_empty() { c.name.clone() } else { format!("{} {}", c.suite, c.name) };
    let failures: Vec<String> = report.cases.iter().filter(|c| c.outcome.is_failure()).take(20).map(label).collect();
    let mut warnings: Vec<String> = Vec::new();
    if let Some(e) = bad_list {
        warnings.push(e);
    }
    warnings.extend(verdict.expired.iter().map(|t| format!("the quarantine of `{t}` has expired: take it off .kiln/quarantine.yaml, or extend its `until`")));
    Ok(srv::json(
        StatusCode::CREATED,
        &json!({
            "report": report_id, "object": object.digest, "format": report.format.name(), "cases": report.cases.len(),
            "passed": report.count(Outcome::Passed), "failed": report.count(Outcome::Failed), "skipped": report.count(Outcome::Skipped), "errors": report.count(Outcome::Error),
            "failures": failures,
            "quarantine": { "only_quarantined": verdict.only_quarantined(), "quarantined": verdict.quarantined, "failing": verdict.failing, "expired": verdict.expired },
            "warnings": warnings,
        }),
    ))
}

/// A result row as the API shows it.
struct Row {
    case: String,
    suite: String,
    name: String,
    file: Option<String>,
    leg: String,
    leg_key: String,
    attempt: i64,
    outcome: String,
    duration_ms: Option<i64>,
    message: Option<String>,
    trace: Option<String>,
    line: Option<i64>,
    retried: i64,
    quarantined: bool,
}

/// Each test's flakiness from its history in the repository (its results on `branch`, by input:
/// the commit and the leg).
async fn flakiness(hub: &Hub, repo: &str, branch: &str, cases: Vec<String>) -> anyhow::Result<HashMap<String, Flakiness>> {
    if cases.is_empty() {
        return Ok(HashMap::new());
    }
    let (repo, branch) = (repo.to_owned(), branch.to_owned());
    let rows: Vec<(String, String, String, u32)> = hub
        .store
        .call(move |c| {
            let mut out = Vec::new();
            let mut st = c.prepare("SELECT case_id, sha || ':' || leg_key, outcome, retried FROM test_results WHERE repo_id = ?1 AND case_id = ?2 AND branch = ?3 ORDER BY ts DESC LIMIT ?4")?;
            for case in &cases {
                let found = st.query_map(params![repo, case, branch, i64::try_from(WINDOW * 2).unwrap_or(60)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
                let mut got = found.collect::<Result<Vec<_>, _>>()?;
                got.reverse();
                out.extend(got);
            }
            Ok(out)
        })
        .await?;
    let mut by: BTreeMap<String, Vec<Seen>> = BTreeMap::new();
    for (case, input, outcome, retried) in rows {
        by.entry(case).or_default().push(Seen { input, outcome: Outcome::parse(&outcome).unwrap_or(Outcome::Error), retried });
    }
    Ok(by.into_iter().map(|(k, seen)| (k, results::flaky(&seen, WINDOW, FLIP_RATE))).collect())
}

/// The tests a run was flaky on, by name (W16-R04's first rule, on its own results): a tool's
/// retry passed them, or one attempt of a leg failed them and another passed them.
pub async fn flaky_in_run(hub: &Hub, run: &str) -> anyhow::Result<Vec<String>> {
    let r = run.to_owned();
    let rows: Vec<(String, String, String, i64)> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT k.suite || ' ' || k.name, t.leg_key, t.outcome, t.retried FROM test_results t JOIN test_cases k ON k.repo_id = t.repo_id AND k.id = t.case_id WHERE t.run_id = ?1")?;
            let rows = st.query_map([&r], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
    // (passed, failed) per test and leg; a retry's pass is flaky at once.
    let mut seen: BTreeMap<(String, String), (bool, bool)> = BTreeMap::new();
    let mut flaky: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (name, leg, outcome, retried) in rows {
        let name = name.trim().to_owned();
        let e = seen.entry((name.clone(), leg)).or_default();
        match outcome.as_str() {
            "passed" if retried > 0 => {
                flaky.insert(name);
            }
            "passed" => e.0 = true,
            "failed" | "error" => e.1 = true,
            _ => {}
        }
    }
    flaky.extend(seen.into_iter().filter(|(_, (p, f))| *p && *f).map(|((name, _), _)| name));
    Ok(flaky.into_iter().collect())
}

fn flaky_json(f: Option<&Flakiness>) -> Value {
    match f {
        Some(Flakiness::Flaky { why }) => json!({ "class": "flaky", "why": why }),
        Some(Flakiness::ProbablyFlaky { flip_rate, runs }) => json!({ "class": "probably_flaky", "why": format!("its outcome changed in {:.0}% of its last {runs} runs", flip_rate * 100.0) }),
        _ => Value::Null,
    }
}

/// `GET /api/ci/runs/:id/tests?attempt=` (`leg` none: the run's latest attempt unless said, or
/// `all`) and `GET /api/ci/legs/:id/tests`.
pub async fn of(hub: &Hub, run: Option<&str>, leg: Option<&str>, q: &HashMap<String, String>) -> Result<Response, Fail> {
    let (run, leg) = (run.map(str::to_owned), leg.map(str::to_owned));
    let attempt = q.get("attempt").cloned().unwrap_or_default();
    let (rows, scope): (Vec<Row>, Option<(String, String)>) = hub
        .store
        .call(move |c| {
            let (sql, key) = match (&run, &leg) {
                (_, Some(l)) => ("t.leg = ?1".to_owned(), l.clone()),
                (Some(r), None) => match attempt.as_str() {
                    "all" => ("t.run_id = ?1".to_owned(), r.clone()),
                    a => match a.parse::<i64>() {
                        Ok(n) => (format!("t.run_id = ?1 AND t.attempt = {n}"), r.clone()),
                        Err(_) => ("t.run_id = ?1 AND t.attempt = (SELECT attempt FROM runs WHERE id = ?1)".to_owned(), r.clone()),
                    },
                },
                (None, None) => ("1 = 0".to_owned(), String::new()),
            };
            let mut st = c.prepare(&format!(
                "SELECT t.case_id, k.suite, k.name, k.file, t.leg, t.leg_key, t.attempt, t.outcome, t.duration_ms, t.message, t.trace, t.line, t.retried, t.quarantined, t.repo_id, t.branch
                 FROM test_results t JOIN test_cases k ON k.repo_id = t.repo_id AND k.id = t.case_id WHERE {sql} ORDER BY t.id"
            ))?;
            let mut scope = None;
            let rows = st
                .query_map([&key], |r| {
                    Ok((
                        Row {
                            case: r.get(0)?,
                            suite: r.get(1)?,
                            name: r.get(2)?,
                            file: r.get(3)?,
                            leg: r.get(4)?,
                            leg_key: r.get(5)?,
                            attempt: r.get(6)?,
                            outcome: r.get(7)?,
                            duration_ms: r.get(8)?,
                            message: r.get(9)?,
                            trace: r.get(10)?,
                            line: r.get(11)?,
                            retried: r.get(12)?,
                            quarantined: r.get(13)?,
                        },
                        (r.get::<_, String>(14)?, r.get::<_, String>(15)?),
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let rows: Vec<Row> = rows
                .into_iter()
                .map(|(row, s)| {
                    scope.get_or_insert(s);
                    row
                })
                .collect();
            Ok((rows, scope))
        })
        .await
        .map_err(|e| internal(&e))?;
    let flaky = match &scope {
        Some((repo, branch)) => {
            let mut cases: Vec<String> = rows.iter().map(|r| r.case.clone()).collect();
            cases.sort();
            cases.dedup();
            flakiness(hub, repo, branch, cases).await.map_err(|e| internal(&e))?
        }
        None => HashMap::new(),
    };
    let rank = |o: &str| match o {
        "failed" | "error" => 0,
        "skipped" => 2,
        _ => 1,
    };
    let mut list: Vec<&Row> = rows.iter().collect();
    list.sort_by_key(|r| (rank(&r.outcome), r.leg_key.clone(), r.suite.clone(), r.name.clone()));
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for r in &rows {
        *counts.entry(r.outcome.as_str()).or_default() += 1;
    }
    let tests: Vec<Value> = list
        .iter()
        .map(|r| {
            json!({
                "id": r.case, "suite": r.suite, "name": r.name, "file": r.file, "line": r.line, "leg": r.leg, "leg_key": r.leg_key, "attempt": r.attempt,
                "outcome": r.outcome, "duration_ms": r.duration_ms, "message": r.message, "trace": r.trace, "retried": r.retried, "quarantined": r.quarantined,
                "flaky": flaky_json(flaky.get(&r.case)),
            })
        })
        .collect();
    Ok(srv::json(StatusCode::OK, &json!({ "counts": counts, "tests": tests })))
}

/// `GET /api/ci/repos/:repo/tests?sort=failing|slowest|flaky&branch=&days=&limit=`: each test's
/// runs, failures, p50 and p90 over the period on a branch (the default branch unless said), and
/// its flakiness.
pub async fn repo(hub: &Hub, repo: &str, q: &HashMap<String, String>) -> Result<Response, Fail> {
    let r = hub.store.repos().await.map_err(|e| internal(&e))?.into_iter().find(|x| x.id == repo || x.name == repo).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no repository `{repo}`")))?;
    let branch = match q.get("branch").filter(|b| !b.is_empty()) {
        Some(b) => b.clone(),
        None => super::events::facts(hub, &r).await.default_branch,
    };
    let days: i64 = q.get("days").and_then(|d| d.parse().ok()).unwrap_or(14).clamp(1, 365);
    let limit: usize = q.get("limit").and_then(|d| d.parse().ok()).unwrap_or(50).clamp(1, 500);
    let sort = q.get("sort").map_or("failing", String::as_str).to_owned();
    let since = crate::util::now_unix() - days * 86_400;
    let (repo_id, b) = (r.id.clone(), branch.clone());
    type Agg = (String, String, String, Option<String>, i64, i64, Vec<i64>);
    let rows: Vec<Agg> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT t.case_id, k.suite, k.name, k.file, t.outcome, t.duration_ms FROM test_results t JOIN test_cases k ON k.repo_id = t.repo_id AND k.id = t.case_id
                 WHERE t.repo_id = ?1 AND t.branch = ?2 AND t.ts >= ?3 ORDER BY t.ts DESC LIMIT 200000",
            )?;
            let mut agg: HashMap<String, Agg> = HashMap::new();
            let rows = st.query_map(params![repo_id, b, since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, String>(4)?, r.get::<_, Option<i64>>(5)?)))?;
            for row in rows {
                let (case, suite, name, file, outcome, ms) = row?;
                let e = agg.entry(case.clone()).or_insert_with(|| (case, suite, name, file, 0, 0, Vec::new()));
                e.4 += 1;
                if matches!(outcome.as_str(), "failed" | "error") {
                    e.5 += 1;
                }
                if let Some(ms) = ms {
                    e.6.push(ms);
                }
            }
            Ok(agg.into_values().collect())
        })
        .await
        .map_err(|e| internal(&e))?;
    let pct = |v: &mut Vec<i64>, p: f64| -> Option<i64> {
        if v.is_empty() {
            return None;
        }
        v.sort_unstable();
        let i = ((v.len() - 1) as f64 * p).round() as usize;
        v.get(i).copied()
    };
    let mut out: Vec<(Value, i64, i64)> = Vec::new();
    for (case, suite, name, file, runs, fails, mut ms) in rows {
        let (p50, p90) = (pct(&mut ms, 0.5), pct(&mut ms, 0.9));
        out.push((json!({ "id": case, "suite": suite, "name": name, "file": file, "runs": runs, "failures": fails, "p50_ms": p50, "p90_ms": p90 }), fails, p90.unwrap_or(0)));
    }
    match sort.as_str() {
        "slowest" => out.sort_by_key(|t| std::cmp::Reverse(t.2)),
        _ => out.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2))),
    }
    let mut tests: Vec<Value> = out.into_iter().map(|(v, _, _)| v).collect();
    // Flakiness for what's shown: the most failing or slowest first, or, sorted by it, the flaky.
    let ids: Vec<String> = tests.iter().filter_map(|t| t.get("id").and_then(Value::as_str).map(str::to_owned)).take(if sort == "flaky" { 2000 } else { limit }).collect();
    let flaky = flakiness(hub, &r.id, &branch, ids).await.map_err(|e| internal(&e))?;
    for t in &mut tests {
        let f = t.get("id").and_then(Value::as_str).and_then(|id| flaky.get(id));
        if let Some(o) = t.as_object_mut() {
            o.insert("flaky".into(), flaky_json(f));
        }
    }
    if sort == "flaky" {
        tests.retain(|t| !t.get("flaky").is_some_and(Value::is_null));
    }
    tests.truncate(limit);
    Ok(srv::json(StatusCode::OK, &json!({ "repo": r.name, "branch": branch, "days": days, "sort": sort, "tests": tests })))
}

/// How many default-branch runs a metric's baseline takes (W16-R08).
const BASELINE_RUNS: usize = 10;
/// A change smaller than this, across the whole interval, isn't reported.
const REPORT_THRESHOLD: f64 = 0.05;
/// Bootstrap resamples per comparison.
const ROUNDS: usize = 2000;

/// The baseline of metric `name` (W16-R08): its values in the last default-branch runs, on
/// runners with this `fingerprint` unless `any_runner`, other than `run`.
async fn baseline(hub: &Hub, repo: &str, branch: &str, name: &str, fingerprint: Option<&str>, run: &str) -> anyhow::Result<Vec<f64>> {
    let (repo, branch, name, fp, run) = (repo.to_owned(), branch.to_owned(), name.to_owned(), fingerprint.map(str::to_owned), run.to_owned());
    hub.store
        .call(move |c| {
            // One value a run: its latest.
            let mut st = c.prepare(
                "SELECT value FROM metrics WHERE id IN (SELECT MAX(id) FROM metrics WHERE repo_id = ?1 AND branch = ?2 AND name = ?3 AND (?4 IS NULL OR fingerprint = ?4) AND run_id != ?5 GROUP BY run_id)
                 ORDER BY ts DESC LIMIT ?6",
            )?;
            Ok(st.query_map(params![repo, branch, name, fp, run, i64::try_from(BASELINE_RUNS).unwrap_or(10)], |r| r.get::<_, f64>(0))?.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

/// A comparison as stored and shown.
fn comparison(v: kiln_ci::stats::Verdict, i: kiln_ci::stats::Interval, runs: usize, better: kiln_ci::stats::Better) -> Value {
    use kiln_ci::stats::{Better, Verdict};
    // As a change for the worse: `+0.10` is 10 % worse, whichever way is better.
    let worse = |ratio: f64| if better == Better::Lower { ratio - 1.0 } else { 1.0 - ratio };
    let (lo, hi) = (worse(i.low).min(worse(i.high)), worse(i.low).max(worse(i.high)));
    let verdict = match v {
        Verdict::Regressed { .. } => "regressed",
        Verdict::Improved { .. } => "improved",
        Verdict::NoClearDifference => "no_clear_difference",
    };
    json!({ "verdict": verdict, "change": worse(i.mid), "low": lo, "high": hi, "runs": runs })
}

/// What a step wrote to `$KILN_METRICS` (W16-R07): kept, each compared with its baseline (R08),
/// and a clear change annotated on the step.
pub async fn metrics(hub: &Hub, leg: &str, idx: u32, points: &[Value], fingerprint: &str) -> anyhow::Result<()> {
    use kiln_ci::results::metrics::Point;
    let Some(of) = leg_of(hub, leg).await? else { return Ok(()) };
    let repo = hub.store.repos().await?.into_iter().find(|r| r.id == of.repo);
    let trunk = match &repo {
        Some(r) => super::events::facts(hub, r).await.default_branch,
        None => "main".into(),
    };
    let now = crate::util::now_unix();
    let mut rows = Vec::new();
    for v in points {
        let Ok(p) = serde_json::from_value::<Point>(v.clone()) else { continue };
        let base = baseline(hub, &of.repo, &trunk, &p.name, Some(fingerprint), &of.run).await.unwrap_or_default();
        let current = if p.samples.is_empty() { vec![p.value] } else { p.samples.clone() };
        let cmp = (base.len() >= 3).then(|| kiln_ci::stats::ratio_interval(&base, &current, ROUNDS, seed(&format!("{}{}", of.run, p.name)))).flatten().map(|i| comparison(kiln_ci::stats::judge(i, REPORT_THRESHOLD, p.better), i, base.len(), p.better));
        rows.push((p, cmp));
    }
    let leg_s = leg.to_owned();
    let fp = fingerprint.to_owned();
    let annotate: Vec<(String, String)> = rows
        .iter()
        .filter_map(|(p, c)| {
            let c = c.as_ref()?;
            let pct = |x: f64| format!("{:.1}%", x * 100.0);
            let n = c.get("runs").and_then(Value::as_u64).unwrap_or(0);
            let (lo, hi, ch) = (c.get("low").and_then(Value::as_f64)?, c.get("high").and_then(Value::as_f64)?, c.get("change").and_then(Value::as_f64)?);
            match c.get("verdict").and_then(Value::as_str)? {
                "regressed" => Some(("warning".into(), format!("{}: {} worse than {trunk} (95% interval {} to {}, against {n} runs on this kind of runner)", p.name, pct(ch), pct(lo), pct(hi)))),
                "improved" => Some(("notice".into(), format!("{}: {} better than {trunk} (95% interval {} to {}, against {n} runs)", p.name, pct(-ch), pct(-hi), pct(-lo)))),
                _ => None,
            }
        })
        .collect();
    hub.store
        .call(move |c| {
            let tx = c.transaction()?;
            for (p, cmp) in &rows {
                tx.execute(
                    "INSERT INTO metrics (repo_id, run_id, leg, leg_key, attempt, branch, sha, step_idx, name, value, unit, better, tags, samples, fingerprint, comparison, ts)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
                    params![
                        of.repo, of.run, leg_s, of.key, of.attempt, of.branch, of.sha, idx, p.name, p.value, p.unit,
                        if p.better == kiln_ci::stats::Better::Higher { "higher" } else { "lower" },
                        serde_json::to_string(&p.tags).ok(), (!p.samples.is_empty()).then(|| serde_json::to_string(&p.samples).unwrap_or_default()), fp,
                        cmp.as_ref().map(Value::to_string), now
                    ],
                )?;
            }
            for (level, message) in annotate.iter().take(MAX_ANNOTATIONS) {
                tx.execute("INSERT INTO ci_annotations (leg_id, step_idx, level, title, message) VALUES (?1, ?2, ?3, 'Benchmark', ?4)", params![leg_s, idx, level, message])?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
}

/// A seed from text (the run and the metric), so a comparison is the same each time it's made.
fn seed(s: &str) -> u64 {
    use sha2::Digest as _;
    sha2::Sha256::digest(s.as_bytes()).iter().take(8).fold(0u64, |acc, b| (acc << 8) | u64::from(*b))
}

/// An annotation on a leg (`level`: error, warning, notice).
pub async fn annotate(hub: &Hub, leg: &str, level: &str, title: &str, message: &str) {
    let (l, lv, t, m) = (leg.to_owned(), level.to_owned(), title.to_owned(), message.to_owned());
    if let Err(e) = hub.store.call(move |c| Ok(c.execute("INSERT INTO ci_annotations (leg_id, level, title, message) VALUES (?1, ?2, ?3, ?4)", params![l, lv, t, m]).map(|_| ())?)).await {
        tracing::warn!(leg, error = %format!("{e:#}"), "an annotation could not be kept");
    }
}

/// A job's `bench-gate` on a leg that passed (W16-R09): the metrics it covers that got worse by
/// more than it allows, judged against their baselines. `Some((fail, why))` when one did.
pub async fn gate(hub: &Hub, leg: &str, g: &kiln_ci::results::metrics::Gate) -> anyhow::Result<Option<(bool, String)>> {
    use kiln_ci::stats::{Better, Verdict};
    let Some(of) = leg_of(hub, leg).await? else { return Ok(None) };
    let repo = hub.store.repos().await?.into_iter().find(|r| r.id == of.repo);
    let trunk = match &repo {
        Some(r) => super::events::facts(hub, r).await.default_branch,
        None => "main".into(),
    };
    let l = leg.to_owned();
    let points: Vec<(String, f64, String, Option<String>, String)> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT name, value, better, samples, fingerprint FROM metrics WHERE leg = ?1 ORDER BY id")?;
            Ok(st.query_map([&l], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
    let mut worse = Vec::new();
    for (name, value, better, samples, fp) in points.into_iter().filter(|p| g.covers(&p.0)) {
        let base = baseline(hub, &of.repo, &trunk, &name, g.same_fingerprint.then_some(fp.as_str()), &of.run).await?;
        if base.len() < 3 {
            continue;
        }
        let current: Vec<f64> = samples.and_then(|s| serde_json::from_str(&s).ok()).filter(|v: &Vec<f64>| !v.is_empty()).unwrap_or_else(|| vec![value]);
        let better = if better == "higher" { Better::Higher } else { Better::Lower };
        if let Some(i) = kiln_ci::stats::ratio_interval(&base, &current, ROUNDS, seed(&format!("{}{name}", of.run)))
            && let Verdict::Regressed { change } = kiln_ci::stats::judge(i, g.max_regression, better)
        {
            worse.push(format!("{name} is {:.1}% worse than {trunk}", change * 100.0));
        }
    }
    if worse.is_empty() {
        return Ok(None);
    }
    Ok(Some((g.fail, format!("bench-gate: {} (more than the {:.0}% it allows)", worse.join("; "), g.max_regression * 100.0))))
}

/// `GET /api/ci/runs/:id/metrics`: the run's metrics, each with its comparison.
pub async fn run_metrics(hub: &Hub, run: &str) -> Result<Response, Fail> {
    let r = run.to_owned();
    let rows: Vec<Value> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT leg, leg_key, step_idx, name, value, unit, better, tags, fingerprint, comparison FROM metrics WHERE run_id = ?1 AND attempt = (SELECT attempt FROM runs WHERE id = ?1) ORDER BY id")?;
            let rows = st.query_map([&r], |x| {
                let parse = |s: Option<String>| s.and_then(|s| serde_json::from_str::<Value>(&s).ok()).unwrap_or(Value::Null);
                Ok(json!({
                    "leg": x.get::<_, String>(0)?, "leg_key": x.get::<_, String>(1)?, "step": x.get::<_, Option<i64>>(2)?, "name": x.get::<_, String>(3)?,
                    "value": x.get::<_, f64>(4)?, "unit": x.get::<_, String>(5)?, "better": x.get::<_, String>(6)?, "tags": parse(x.get(7)?),
                    "fingerprint": x.get::<_, String>(8)?, "comparison": parse(x.get(9)?),
                }))
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(|e| internal(&e))?;
    Ok(srv::json(StatusCode::OK, &json!({ "metrics": rows })))
}

/// `GET /api/ci/repos/:repo/metrics?name=&branch=&days=`: a metric's values over time on a
/// branch, by fingerprint (W16-AC05's trends); without `name`, the metrics there are.
pub async fn repo_metrics(hub: &Hub, repo: &str, q: &HashMap<String, String>) -> Result<Response, Fail> {
    let r = hub.store.repos().await.map_err(|e| internal(&e))?.into_iter().find(|x| x.id == repo || x.name == repo).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no repository `{repo}`")))?;
    let branch = match q.get("branch").filter(|b| !b.is_empty()) {
        Some(b) => b.clone(),
        None => super::events::facts(hub, &r).await.default_branch,
    };
    let days: i64 = q.get("days").and_then(|d| d.parse().ok()).unwrap_or(30).clamp(1, 365);
    let since = crate::util::now_unix() - days * 86_400;
    let (repo_id, b, name) = (r.id.clone(), branch.clone(), q.get("name").cloned());
    let out: Value = hub
        .store
        .call(move |c| {
            Ok(match name {
                None => {
                    let mut st = c.prepare("SELECT name, unit, better, COUNT(*), MAX(ts) FROM metrics WHERE repo_id = ?1 AND branch = ?2 AND ts >= ?3 GROUP BY name ORDER BY name")?;
                    let rows = st.query_map(params![repo_id, b, since], |x| Ok(json!({ "name": x.get::<_, String>(0)?, "unit": x.get::<_, String>(1)?, "better": x.get::<_, String>(2)?, "points": x.get::<_, i64>(3)?, "last": x.get::<_, i64>(4)? })))?;
                    json!({ "metrics": rows.collect::<Result<Vec<_>, _>>()? })
                }
                Some(n) => {
                    let mut st = c.prepare("SELECT fingerprint, sha, run_id, value, unit, ts FROM metrics WHERE repo_id = ?1 AND branch = ?2 AND name = ?3 AND ts >= ?4 ORDER BY ts")?;
                    let mut by: BTreeMap<String, Vec<Value>> = BTreeMap::new();
                    for x in st.query_map(params![repo_id, b, n, since], |x| Ok((x.get::<_, String>(0)?, json!({ "sha": x.get::<_, String>(1)?, "run": x.get::<_, String>(2)?, "value": x.get::<_, f64>(3)?, "unit": x.get::<_, String>(4)?, "ts": x.get::<_, i64>(5)? }))))? {
                        let (fp, v) = x?;
                        by.entry(fp).or_default().push(v);
                    }
                    json!({ "name": n, "series": by.into_iter().map(|(fp, points)| json!({ "fingerprint": fp, "points": points })).collect::<Vec<_>>() })
                }
            })
        })
        .await
        .map_err(|e| internal(&e))?;
    Ok(srv::json(StatusCode::OK, &json!({ "repo": r.name, "branch": branch, "days": days, "data": out })))
}

/// The most items a run's timing table keeps.
const MAX_TIMING: usize = 5000;

type TimingCache = parking_lot::Mutex<HashMap<String, (std::time::Instant, Arc<HashMap<String, f64>>)>>;

/// Each repository's timing table, for ten minutes or until its next report.
static TIMING: std::sync::LazyLock<TimingCache> = std::sync::LazyLock::new(Default::default);

/// A repository's recorded durations (W16-R06), in seconds, for shards balanced by timing
/// (W04-R14): each test file's (the sum of its tests in a leg, the median across legs), and each
/// test's (by name, and by suite and name), over the last two weeks. Cached for ten minutes.
pub async fn timing(hub: &Hub, repo_id: &str) -> Arc<HashMap<String, f64>> {
    if let Some((at, t)) = TIMING.lock().get(repo_id)
        && at.elapsed() < std::time::Duration::from_secs(600)
    {
        return Arc::clone(t);
    }
    let (repo, since) = (repo_id.to_owned(), crate::util::now_unix() - 14 * 86_400);
    let rows: Vec<(String, Option<String>, String, String, i64)> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT t.leg, k.file, k.suite, k.name, t.duration_ms FROM test_results t JOIN test_cases k ON k.repo_id = t.repo_id AND k.id = t.case_id
                 WHERE t.repo_id = ?1 AND t.ts >= ?2 AND t.duration_ms IS NOT NULL ORDER BY t.ts DESC LIMIT 200000",
            )?;
            Ok(st.query_map(params![repo, since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap_or_default();
    let mut files: HashMap<String, HashMap<String, i64>> = HashMap::new();
    let mut tests: HashMap<String, Vec<i64>> = HashMap::new();
    for (leg, file, suite, name, ms) in rows {
        if let Some(f) = file {
            *files.entry(f).or_default().entry(leg).or_default() += ms;
        }
        let mut names = vec![name.clone()];
        if !suite.is_empty() {
            names.extend([format!("{suite}::{name}"), format!("{suite}.{name}"), format!("{suite} {name}")]);
        }
        for n in names {
            tests.entry(n).or_default().push(ms);
        }
    }
    let median = |mut v: Vec<i64>| -> f64 {
        v.sort_unstable();
        v.get(v.len() / 2).copied().unwrap_or(0) as f64 / 1000.0
    };
    let mut out: HashMap<String, f64> = HashMap::new();
    for (f, by_leg) in files {
        out.insert(f, median(by_leg.into_values().collect()));
    }
    for (n, v) in tests {
        if out.len() >= MAX_TIMING {
            break;
        }
        out.entry(n).or_insert_with(|| median(v));
    }
    let t = Arc::new(out);
    TIMING.lock().insert(repo_id.to_owned(), (std::time::Instant::now(), Arc::clone(&t)));
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w16_r03_paths_relative_to_the_workspace() {
        assert_eq!(relative("/work/w/1/app/src/a.test.js", Some("/work/w/1/app")), "src/a.test.js");
        assert_eq!(relative("/work/w/1/app/src/a.test.js", Some("/work/w/1/app/")), "src/a.test.js");
        assert_eq!(relative("src/a.test.js", Some("/work/w/1/app")), "src/a.test.js");
        assert_eq!(relative("C:\\w\\app\\a.cs", Some("C:/w/app")), "a.cs");
    }
}
