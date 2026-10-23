//! Log search (docs/ci.md W19-R04, W17-R07, W20's `ci.legs.search`, `ci.legs.failure_excerpt`).
//!
//! Finished legs' step logs go into a trigram index (`ci_log_index`, SQLite's FTS5 with the
//! trigram tokenizer: any substring of three characters or more, case-insensitive) in segments of
//! [`SEGMENT_LINES`] lines, escape codes stripped. A sweep every half minute (and as soon as a leg
//! finishes) indexes what isn't yet, in the order legs finished, so the index builds
//! incrementally and catches up after a restart; pruned legs leave it with their logs.
//!
//! - `GET /api/ci/logs/search?q=&repo=&workflow=&branch=&conclusion=&since=&until=&limit=`:
//!   matching lines across runs, newest first.
//! - `GET /api/ci/legs/:id/logs/search?q=`: one leg's matching lines, read from its files (so a
//!   running leg's too).
//! - `GET /api/ci/legs/:id/logs/failure`: the failed step's excerpt, around its first error.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures::FutureExt as _;
use http::StatusCode;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};

use super::Fail;
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;

/// Lines per indexed segment: a hit's place is its segment, then the line within it.
pub const SEGMENT_LINES: usize = 100;
/// Legs indexed per pass.
const PER_PASS: usize = 200;
/// A step's log past this isn't indexed beyond it (the log keeps it).
const MAX_INDEXED: usize = 8 << 20;

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// A log's text as read: escape codes gone, `\r\n` as `\n`, a line rewritten with `\r` as its
/// last version (what a terminal shows).
pub fn plain(raw: &[u8]) -> String {
    let text = crate::harness::claude::strip_ansi(&String::from_utf8_lossy(raw));
    text.split('\n').map(|l| l.trim_end_matches('\r').rsplit('\r').next().unwrap_or_default()).collect::<Vec<_>>().join("\n")
}

/// Indexes one step's log: its segments, as rows of `ci_log_index`. The rowids they took.
pub fn index_step(c: &Connection, leg: &str, step: u32, text: &str) -> rusqlite::Result<Option<(i64, i64)>> {
    let text = crate::util::truncate(text, MAX_INDEXED);
    let lines: Vec<&str> = text.split('\n').collect();
    let mut first = None;
    let mut last = None;
    let mut st = c.prepare_cached("INSERT INTO ci_log_index (text, leg, step, line) VALUES (?1, ?2, ?3, ?4)")?;
    for (n, chunk) in lines.chunks(SEGMENT_LINES).enumerate() {
        let body = chunk.join("\n");
        if body.trim().is_empty() {
            continue;
        }
        st.execute(params![body, leg, step, i64::try_from(n * SEGMENT_LINES + 1).unwrap_or(i64::MAX)])?;
        let id = c.last_insert_rowid();
        first.get_or_insert(id);
        last = Some(id);
    }
    Ok(first.zip(last))
}

/// What to look for, and where.
#[derive(Debug, Clone, Default)]
pub struct Query {
    pub q: String,
    pub repo: Option<String>,
    pub workflow: Option<String>,
    pub branch: Option<String>,
    pub conclusion: Option<String>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub limit: usize,
}

/// A matching line.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Hit {
    pub run: String,
    pub number: i64,
    pub repo: String,
    pub workflow: String,
    pub branch: String,
    pub conclusion: Option<String>,
    pub created_at: i64,
    pub leg: String,
    pub job: String,
    pub step: u32,
    pub step_name: Option<String>,
    /// 1-based, within the step's log.
    pub line: usize,
    pub text: String,
    /// Where the match is in `text` (bytes).
    pub at: usize,
}

/// FTS5 phrase syntax for a literal string.
fn phrase(q: &str) -> String {
    format!("\"{}\"", q.replace('"', "\"\""))
}

/// The lines of `body` (starting at line `first`) that hold `q`, case-insensitive: (line, text,
/// where).
fn lines_with(body: &str, first: usize, q: &str) -> Vec<(usize, String, usize)> {
    let needle = q.to_lowercase();
    body.split('\n')
        .enumerate()
        .filter_map(|(i, l)| {
            let at = l.to_lowercase().find(&needle)?;
            Some((first + i, crate::util::truncate(l, 2000).to_owned(), at))
        })
        .collect()
}

/// Matching lines across runs, newest first (at most five a segment, `limit` in all).
pub fn search(c: &Connection, q: &Query) -> rusqlite::Result<Vec<Hit>> {
    let mut st = c.prepare_cached(
        "SELECT s.leg, s.step, s.line, s.text, j.run_id, j.key, r.number, r.repo_id, COALESCE(r.workflow_id, r.pipeline, ''), r.branch, r.conclusion, r.created_at,
                (SELECT name FROM steps WHERE job_id = j.id AND idx = s.step)
         FROM ci_log_index s JOIN jobs j ON j.id = s.leg JOIN runs r ON r.id = j.run_id
         WHERE ci_log_index MATCH ?1 AND (?2 IS NULL OR r.repo_id = ?2) AND (?3 IS NULL OR r.workflow_id = ?3) AND (?4 IS NULL OR r.branch = ?4)
           AND (?5 IS NULL OR r.conclusion = ?5) AND (?6 IS NULL OR r.created_at >= ?6) AND (?7 IS NULL OR r.created_at < ?7)
         ORDER BY s.rowid DESC LIMIT ?8",
    )?;
    let limit = q.limit.clamp(1, 1000);
    let rows = st.query_map(params![phrase(&q.q), q.repo, q.workflow, q.branch, q.conclusion, q.since, q.until, i64::try_from(limit).unwrap_or(1000)], |x| {
        Ok((
            x.get::<_, String>(0)?,
            x.get::<_, u32>(1)?,
            usize::try_from(x.get::<_, i64>(2)?).unwrap_or(1),
            x.get::<_, String>(3)?,
            (x.get::<_, String>(4)?, x.get::<_, String>(5)?, x.get::<_, i64>(6)?, x.get::<_, String>(7)?, x.get::<_, String>(8)?, x.get::<_, String>(9)?, x.get::<_, Option<String>>(10)?, x.get::<_, i64>(11)?, x.get::<_, Option<String>>(12)?),
        ))
    })?;
    let mut out = Vec::new();
    for r in rows {
        let (leg, step, first, body, (run, job, number, repo, workflow, branch, conclusion, created_at, step_name)) = r?;
        for (line, text, at) in lines_with(&body, first, &q.q).into_iter().take(5) {
            out.push(Hit { run: run.clone(), number, repo: repo.clone(), workflow: workflow.clone(), branch: branch.clone(), conclusion: conclusion.clone(), created_at, leg: leg.clone(), job: job.clone(), step, step_name: step_name.clone(), line, text, at });
            if out.len() >= limit {
                return Ok(out);
            }
        }
    }
    Ok(out)
}

/// The indexer's wake-up: a leg finished.
static KICK: LazyLock<(async_channel::Sender<()>, async_channel::Receiver<()>)> = LazyLock::new(|| async_channel::bounded(1));

/// Indexes legs as they finish, for as long as the server runs.
pub fn spawn(hub: Arc<Hub>) {
    crate::rt::spawn(async move {
        loop {
            match pass(&hub).await {
                // Behind: go on at once.
                Ok(n) if n >= PER_PASS => continue,
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "logs could not be indexed"),
            }
            futures::select_biased! {
                _ = KICK.1.recv().fuse() => {}
                () = crate::rt::sleep(Duration::from_secs(30)).fuse() => {}
            }
        }
    });
}

/// Wakes the indexer: a leg finished.
pub fn kick() {
    let _ = KICK.0.try_send(());
}

/// Indexes up to [`PER_PASS`] finished legs not yet in the index, oldest first, so the index's
/// order is the order they finished in (searches read it newest first): how many.
async fn pass(hub: &Hub) -> anyhow::Result<usize> {
    let legs: Vec<(String, Vec<u32>)> = hub
        .store
        .call(|c| {
            let mut st = c.prepare(
                "SELECT j.id FROM jobs j WHERE j.finished_at IS NOT NULL AND NOT EXISTS (SELECT 1 FROM ci_log_indexed x WHERE x.leg = j.id) ORDER BY j.finished_at, j.rowid LIMIT ?1",
            )?;
            let ids: Vec<String> = st.query_map([i64::try_from(PER_PASS).unwrap_or(200)], |x| x.get(0))?.collect::<Result<_, _>>()?;
            let mut out = Vec::new();
            let mut steps = c.prepare("SELECT idx FROM steps WHERE job_id = ?1 ORDER BY idx")?;
            for id in ids {
                let s: Vec<u32> = steps.query_map([&id], |x| x.get(0))?.collect::<Result<_, _>>()?;
                out.push((id, s));
            }
            Ok(out)
        })
        .await?;
    let n = legs.len();
    for (leg, steps) in legs {
        // A leg still writing (a late step) waits for the next pass.
        if hub.logs.is_live(&leg) {
            continue;
        }
        let mut texts = Vec::new();
        for s in steps {
            let raw = hub.logs.read(&leg, s).await;
            if !raw.is_empty() {
                texts.push((s, raw));
            }
        }
        let leg2 = leg.clone();
        // The index is this node's own, built from its own logs: past the cluster group.
        hub.store
            .call_local(move |c| {
                let tx = crate::store::tx(c)?;
                let (mut first, mut last, mut bytes) = (None::<i64>, None::<i64>, 0usize);
                for (s, raw) in &texts {
                    bytes += raw.len();
                    let text = plain(raw);
                    if let Some((a, b)) = index_step(&tx, &leg2, *s, &text)? {
                        first = Some(first.map_or(a, |f| f.min(a)));
                        last = Some(last.map_or(b, |l| l.max(b)));
                    }
                }
                tx.execute(
                    "INSERT OR REPLACE INTO ci_log_indexed (leg, at, bytes, first_row, last_row) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![leg2, crate::util::now_unix(), i64::try_from(bytes).unwrap_or(i64::MAX), first, last],
                )?;
                tx.commit()?;
                Ok(())
            })
            .await?;
    }
    Ok(n)
}

/// Pruned legs leave the index with their logs.
pub async fn forget(hub: &Hub, legs: Vec<String>) {
    let _ = hub
        .store
        .call_local(move |c| {
            let tx = crate::store::tx(c)?;
            for leg in &legs {
                let rows: Option<(Option<i64>, Option<i64>)> = tx.query_row("SELECT first_row, last_row FROM ci_log_indexed WHERE leg = ?1", [leg], |x| Ok((x.get(0)?, x.get(1)?))).optional()?;
                if let Some((Some(a), Some(b))) = rows {
                    tx.execute("DELETE FROM ci_log_index WHERE rowid BETWEEN ?1 AND ?2 AND leg = ?3", params![a, b, leg])?;
                }
                tx.execute("DELETE FROM ci_log_indexed WHERE leg = ?1", [leg])?;
            }
            tx.commit()?;
            Ok(())
        })
        .await;
}

async fn repo_id(hub: &Hub, repo: &str) -> Result<String, Fail> {
    let rs = hub.store.repos().await.map_err(internal)?;
    rs.into_iter().find(|r| r.id == repo || r.name == repo).map(|r| r.id).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no repository `{repo}`")))
}

/// `GET /api/ci/logs/search`.
pub async fn route(hub: &Hub, q: &HashMap<String, String>) -> Result<Response, Fail> {
    let text = q.get("q").map(|s| s.trim().to_owned()).unwrap_or_default();
    if text.chars().count() < 3 {
        return Err((StatusCode::BAD_REQUEST, "search for three characters or more".into()));
    }
    let some = |k: &str| q.get(k).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
    let repo = match some("repo") {
        Some(r) => Some(repo_id(hub, &r).await?),
        None => None,
    };
    let time = |k: &str| some(k).and_then(|v| v.parse::<i64>().ok());
    let since = time("since").or_else(|| some("days").and_then(|d| d.parse::<i64>().ok()).map(|d| crate::util::now_unix() - d.clamp(1, 3650) * 86_400));
    let query = Query { q: text, repo, workflow: some("workflow"), branch: some("branch"), conclusion: some("conclusion"), since, until: time("until"), limit: some("limit").and_then(|l| l.parse().ok()).unwrap_or(100) };
    let started = std::time::Instant::now();
    let hits = hub.store.call(move |c| Ok(search(c, &query)?)).await.map_err(internal)?;
    let names: HashMap<String, String> = hub.store.repos().await.map_err(internal)?.into_iter().map(|r| (r.id, r.name)).collect();
    let behind: i64 = hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM jobs j WHERE j.finished_at IS NOT NULL AND NOT EXISTS (SELECT 1 FROM ci_log_indexed x WHERE x.leg = j.id)", [], |x| x.get(0))?)).await.unwrap_or(0);
    let hits: Vec<Value> = hits
        .into_iter()
        .map(|h| {
            let mut v = serde_json::to_value(&h).unwrap_or_default();
            if let Some(o) = v.as_object_mut() {
                o.insert("repo_name".into(), json!(names.get(&h.repo)));
            }
            v
        })
        .collect();
    Ok(srv::json(StatusCode::OK, &json!({ "hits": hits, "took_ms": started.elapsed().as_millis(), "unindexed_legs": behind })))
}

/// A leg's steps: (idx, name, status, exit code), in order.
async fn steps_of(hub: &Hub, leg: &str) -> Result<Vec<(u32, String, String, Option<i64>)>, Fail> {
    let leg = leg.to_owned();
    hub.store
        .call(move |c| {
            let mut st = c.prepare("SELECT idx, name, COALESCE(conclusion, status), exit_code FROM steps WHERE job_id = ?1 ORDER BY COALESCE(started_seq, 1000000000 + idx)")?;
            let rows = st.query_map([&leg], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(internal)
}

/// `GET /api/ci/legs/:id/logs/search?q=` and `/logs/failure`.
pub async fn leg(hub: &Hub, id: &str, what: &str, q: &HashMap<String, String>) -> Result<Response, Fail> {
    let steps = steps_of(hub, id).await?;
    if steps.is_empty() && hub.store.job(id.to_owned()).await.map_err(internal)?.is_none() {
        return Err((StatusCode::NOT_FOUND, format!("no leg `{id}`")));
    }
    match what {
        "search" => {
            let text = q.get("q").map(|s| s.trim().to_owned()).unwrap_or_default();
            if text.is_empty() {
                return Err((StatusCode::BAD_REQUEST, "say what to search for: ?q=".into()));
            }
            let mut hits = Vec::new();
            for (idx, name, ..) in &steps {
                let body = plain(&hub.logs.read(id, *idx).await);
                for (line, t, at) in lines_with(&body, 1, &text).into_iter().take(200) {
                    hits.push(json!({ "step": idx, "step_name": name, "line": line, "text": t, "at": at }));
                }
            }
            Ok(srv::json(StatusCode::OK, &json!({ "leg": id, "hits": hits })))
        }
        _ => {
            let lines: usize = q.get("lines").and_then(|l| l.parse().ok()).unwrap_or(40).clamp(5, 400);
            Ok(srv::json(StatusCode::OK, &failure_of(hub, id, &steps, lines).await))
        }
    }
}

/// What to read first when leg `id` failed (`lines` of it): `{ leg, step, step_name, status,
/// exit_code, total_lines, lines: [{ line, text }] }`. For whoever follows a failure (an agent
/// whose change it is, `crate::workspace::follow`).
pub async fn failure(hub: &Hub, id: &str, lines: usize) -> Result<Value, Fail> {
    let steps = steps_of(hub, id).await?;
    Ok(failure_of(hub, id, &steps, lines.clamp(5, 400)).await)
}

async fn failure_of(hub: &Hub, id: &str, steps: &[(u32, String, String, Option<i64>)], lines: usize) -> Value {
    // The step that failed (else the last that ran), around its first error line, else its end.
    let failed = steps.iter().find(|s| matches!(s.2.as_str(), "failure" | "failed" | "timed_out")).or_else(|| steps.iter().rev().find(|s| s.2 != "skipped" && s.2 != "pending"));
    let Some((idx, name, status, exit_code)) = failed.cloned() else { return json!({ "leg": id, "step": null, "lines": [] }) };
    let body = plain(&hub.logs.read(id, idx).await);
    let body = body.trim_end();
    let all: Vec<&str> = if body.is_empty() { Vec::new() } else { body.split('\n').collect() };
    let (from, to) = excerpt(&all, lines);
    let shown: Vec<Value> = all.get(from..to).unwrap_or_default().iter().enumerate().map(|(i, l)| json!({ "line": from + i + 1, "text": crate::util::truncate(l, 2000) })).collect();
    json!({ "leg": id, "step": idx, "step_name": name, "status": status, "exit_code": exit_code, "total_lines": all.len(), "lines": shown })
}

/// Words that mark a line as an error's.
const ERROR_MARKS: &[&str] = &["##[error]", "error:", "error[", "fatal:", "panicked at", "exception", "traceback (most recent call last)", "failed", "fail:", "--- fail", "assertionerror", "npm err!"];

/// How a line that surely is an error's starts (a compiler's, a test's, a tool's), its indent
/// aside; and what it has wherever.
const ERROR_STARTS: &[&str] = &["##[error]", "error:", "error[", "fatal:", "fail:", "--- fail", "fail\t", "npm err!", "traceback (most recent call last)"];
const ERROR_ANYWHERE: &[&str] = &["##[error]", "panicked at", "assertionerror"];

/// A line (lower case, its indent aside) that says it isn't an error's though it has one of their
/// words: a log record at a lower level (`level=info msg="fetch failed …"`, a JSON record's
/// `"level":"debug"`, `[INFO]`, `warning:`), or a test runner's progress (Go's `=== RUN`,
/// `--- PASS`, `ok`).
fn lesser(l: &str) -> bool {
    const BELOW: &[&str] = &["info", "debug", "trace", "warning", "warn", "notice"];
    let level = ["level=", "\"level\":", "lvl="]
        .iter()
        .any(|k| l.match_indices(k).any(|(at, _)| l.get(at + k.len()..).is_some_and(|rest| BELOW.iter().any(|b| rest.trim_start_matches([' ', '"']).starts_with(b)))));
    let tag = l.strip_prefix('[').unwrap_or(l);
    let tagged = BELOW.iter().any(|b| tag.strip_prefix(b).is_some_and(|after| after.starts_with([':', ' ', ']'])));
    let progress = ["=== run", "=== pause", "=== cont", "=== name", "--- pass", "--- skip", "ok ", "ok\t"].iter().any(|p| l.starts_with(p));
    level || tagged || progress
}

/// The excerpt of `lines` (`n` of them) to show for a failure: from a few lines before the first
/// line that surely reads like an error, else the first that may, else the last `n`. A line that
/// says it's a lesser one's (`lesser`) is neither.
pub fn excerpt(lines: &[&str], n: usize) -> (usize, usize) {
    let lower: Vec<String> = lines.iter().map(|l| l.trim_start().to_lowercase()).collect();
    let sure = lower.iter().position(|l| !lesser(l) && (ERROR_STARTS.iter().any(|m| l.starts_with(m)) || ERROR_ANYWHERE.iter().any(|m| l.contains(m))));
    let first = sure.or_else(|| lower.iter().position(|l| !lesser(l) && ERROR_MARKS.iter().any(|m| l.contains(m))));
    match first {
        // A few lines before it (fewer in a short excerpt, so it's in it), and no fewer than `n`
        // when it's near the end.
        Some(i) => {
            let from = i.saturating_sub(5.min(n.saturating_sub(1) / 2)).min(lines.len().saturating_sub(n));
            (from, (from + n).min(lines.len()))
        }
        None => (lines.len().saturating_sub(n), lines.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A database with the index and just enough of runs, jobs and steps.
    fn db() -> anyhow::Result<Connection> {
        let c = Connection::open_in_memory()?;
        c.execute_batch(
            "CREATE TABLE runs (id TEXT PRIMARY KEY, number INTEGER, repo_id TEXT, workflow_id TEXT, pipeline TEXT, branch TEXT, conclusion TEXT, status TEXT, created_at INTEGER);
             CREATE TABLE jobs (id TEXT PRIMARY KEY, run_id TEXT, key TEXT, finished_at INTEGER);
             CREATE TABLE steps (job_id TEXT, idx INTEGER, name TEXT);
             CREATE VIRTUAL TABLE ci_log_index USING fts5(text, leg UNINDEXED, step UNINDEXED, line UNINDEXED, tokenize = 'trigram');",
        )?;
        Ok(c)
    }

    #[test]
    fn w19_r04_lines() -> anyhow::Result<()> {
        assert_eq!(plain(b"\x1b[31mred\x1b[0m\r\nprogress 10%\rprogress 100%\nlast"), "red\nprogress 100%\nlast");
        let lines = ["ok", "compiling", "error[E0308]: mismatched types", "  --> src/a.rs:3", "done"];
        assert_eq!(excerpt(&lines, 3), (1, 4));
        assert_eq!(excerpt(&["a", "b", "c", "d"], 2), (2, 4), "no error line: the end");
        // A log record at a lower level, or a test's progress, isn't an error's, and a line that
        // starts as one comes before one that only has its words (buildx's integration tests).
        let go = [
            "time=\"2026-10-23T20:22:41Z\" level=info msg=\"fetch failed after status: 404 Not Found\"",
            "=== RUN   TestIntegration/TestBuildFailedNoParallel",
            "--- PASS: TestIntegration/TestBuildFailedNoParallel (2.22s)",
            "{\"level\":\"debug\",\"msg\":\"retry failed\"}",
            "[INFO] a step failed, retrying",
            "    dap_build.go:937:",
            "        \tError Trace:\t/src/tests/dap_build.go:937",
            "        \tError:      \ttimeout reached",
            "--- FAIL: TestIntegration (70.54s)",
            "FAIL",
        ];
        assert_eq!(excerpt(&go, 3), (6, 9), "{:?}", go.get(6..9));
        assert_eq!(excerpt(&["x"; 60].iter().copied().chain(["error: no such file"]).collect::<Vec<_>>(), 40), (21, 61), "near the end: the last 40");
        assert_eq!(excerpt(&["ok", "Build failed: 2 errors", "done"], 2), (1, 3), "only a mark: the first");
        assert_eq!(excerpt(&["warning: the build failed for an optional part", "information: failed", "x"], 1), (1, 2), "a warning is lesser; information isn't");
        let c = db()?;
        c.execute("INSERT INTO runs VALUES ('r1', 7, 'repo', 'ci.yml', '', 'main', 'failure', 'failed', 100)", [])?;
        c.execute("INSERT INTO jobs VALUES ('l1', 'r1', 'test', 100)", [])?;
        c.execute("INSERT INTO steps VALUES ('l1', 1, 'Run tests')", [])?;
        let text: String = (1..=250).map(|i| if i == 175 { "thread 'main' panicked at Needle-42 here".to_owned() } else { format!("line {i}") }).collect::<Vec<_>>().join("\n");
        assert_eq!(index_step(&c, "l1", 1, &text)?.map(|(a, b)| b - a), Some(2), "250 lines: three segments");
        let hits = search(&c, &Query { q: "needle-42".into(), limit: 10, ..Query::default() })?;
        assert_eq!(hits.iter().map(|h| (h.line, h.step, h.step_name.as_deref(), h.number)).collect::<Vec<_>>(), vec![(175, 1, Some("Run tests"), 7)]);
        assert_eq!(hits.first().map(|h| h.at), Some(26));
        // Filters narrow it; quotes are literal.
        assert!(search(&c, &Query { q: "needle-42".into(), branch: Some("dev".into()), limit: 10, ..Query::default() })?.is_empty());
        assert!(search(&c, &Query { q: "a \"quoted\" thing".into(), limit: 10, ..Query::default() }).is_ok());
        Ok(())
    }

    /// W19-AC06 (a bench; `cargo test -p kiln --release -- --ignored w19_ac06`): a string one step
    /// printed, among 10,000 runs' logs, is found in under 500 ms.
    #[test]
    #[ignore]
    fn w19_ac06_search_bench() -> anyhow::Result<()> {
        let c = db()?;
        c.execute_batch("BEGIN")?;
        let words = ["compiling", "running", "test", "ok", "warning", "downloaded", "finished", "cargo", "serde", "tokio"];
        let mut seed = 7u64;
        for r in 0..10_000i64 {
            c.execute("INSERT INTO runs VALUES (?1, ?2, 'repo', 'ci.yml', '', 'main', 'success', 'succeeded', ?2)", params![format!("r{r}"), r])?;
            for leg in 0..2i64 {
                let id = format!("r{r}l{leg}");
                c.execute("INSERT INTO jobs VALUES (?1, ?2, 'build', ?3)", params![id, format!("r{r}"), r])?;
                for step in 0..3u32 {
                    let text: String = (0..40)
                        .map(|i| {
                            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                            let w = |k: u64| words.get(usize::try_from((seed >> k) % 10).unwrap_or(0)).copied().unwrap_or("x");
                            if r == 4321 && leg == 1 && step == 2 && i == 17 { "the unique-marker-9f3c string".to_owned() } else { format!("{} {} v{}.{} {}", w(13), w(21), (seed >> 7) % 40, (seed >> 29) % 10, w(37)) }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    index_step(&c, &id, step, &text)?;
                }
            }
        }
        c.execute_batch("COMMIT")?;
        let t = std::time::Instant::now();
        let hits = search(&c, &Query { q: "unique-marker-9f3c".into(), limit: 50, ..Query::default() })?;
        let took = t.elapsed();
        eprintln!("10,000 runs, 60,000 step logs: found {} in {took:?}", hits.len());
        assert_eq!(hits.iter().map(|h| (h.run.as_str(), h.leg.as_str(), h.step, h.line)).collect::<Vec<_>>(), vec![("r4321", "r4321l1", 2, 18)]);
        assert!(took < std::time::Duration::from_millis(500), "{took:?}");
        Ok(())
    }
}
