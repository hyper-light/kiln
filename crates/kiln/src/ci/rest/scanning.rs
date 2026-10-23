//! Code scanning on the facade (docs/ci.md W14-R11), GitHub's shape, so scanners' starter
//! workflows and `github/codeql-action/upload-sarif` run on kiln:
//!
//! - `PUT /repos/{o}/{r}/code-scanning/analysis` (what codeql-action uploads with) and
//!   `POST /repos/{o}/{r}/code-scanning/sarifs` (the public API): SARIF 2.1.0, gzipped and in
//!   base64, checked (a `runs` array, each with `results`), kept, and read into alerts at once;
//!   so `GET /repos/{o}/{r}/code-scanning/sarifs/{id}` says `complete`.
//! - Alerts are the results, one per fingerprint (the result's `partialFingerprints`, else its
//!   rule, file, line and message) on a ref. A later analysis of the same ref, tool and category
//!   that no longer finds one marks it fixed; one found again is open again.
//! - `GET /repos/{o}/{r}/code-scanning/alerts` (`ref`, `state`, `tool_name`, `severity`),
//!   `/alerts/{number}` and `/analyses` (`ref`, `tool_name`, `sarif_id`).
//! - The uploading leg gets the new alerts as annotations (the first 50), where the run shows
//!   them.

use std::collections::{BTreeMap, HashSet};
use std::io::Read as _;

use base64::Engine as _;
use http::StatusCode;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::repos::Site;
use crate::ci::Fail;
use crate::ci::token::Job;
use crate::http::server::{self as srv, Response};

/// The most SARIF an upload may unpack to.
const MAX_SARIF: u64 = 64 << 20;
/// Alerts a leg is told of, at most.
const MAX_ANNOTATIONS: usize = 50;

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn invalid(m: &str) -> Response {
    srv::json(StatusCode::UNPROCESSABLE_ENTITY, &json!({ "message": m, "documentation_url": "https://docs.github.com/rest/code-scanning" }))
}

fn rfc3339(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix).ok().and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()).unwrap_or_default()
}

/// One result of a SARIF run, as an alert would be.
#[derive(Debug, Clone, PartialEq)]
struct Finding {
    fingerprint: String,
    rule_id: String,
    rule_name: Option<String>,
    rule_description: Option<String>,
    /// `error`, `warning` or `note`.
    severity: String,
    security_severity: Option<String>,
    message: String,
    path: String,
    start_line: Option<i64>,
    end_line: Option<i64>,
    start_column: Option<i64>,
    end_column: Option<i64>,
}

/// One run of an upload: its tool, and what it found.
#[derive(Debug, Clone)]
struct Run {
    tool: String,
    version: Option<String>,
    category: String,
    rules: usize,
    findings: Vec<Finding>,
}

/// The upload's SARIF, unpacked: gzipped and in base64, or (as some clients send) plain.
fn unpack(sarif: &str) -> Result<Value, String> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(sarif.trim()).map_err(|e| format!("rejecting SARIF: it isn't base64: {e}"))?;
    let mut text = Vec::new();
    let raw = if bytes.starts_with(&[0x1f, 0x8b]) {
        flate2::read::GzDecoder::new(bytes.as_slice()).take(MAX_SARIF + 1).read_to_end(&mut text).map_err(|e| format!("rejecting SARIF: it isn't gzip: {e}"))?;
        text
    } else {
        bytes
    };
    if raw.len() as u64 > MAX_SARIF {
        return Err(format!("rejecting SARIF: over {MAX_SARIF} bytes unpacked"));
    }
    serde_json::from_slice(&raw).map_err(|e| format!("rejecting SARIF: it isn't JSON: {e}"))
}

/// The runs of a SARIF log, read as GitHub reads them; why not, when it can't be.
fn runs_of(sarif: &Value, category: &str) -> Result<Vec<Run>, String> {
    let runs = sarif.get("runs").and_then(Value::as_array).ok_or("rejecting SARIF: Invalid SARIF. Missing 'runs' array.")?;
    let mut out = Vec::new();
    for run in runs {
        let results = run.get("results").and_then(Value::as_array).ok_or("rejecting SARIF: Invalid SARIF. Missing 'results' array in run.")?;
        let driver = run.pointer("/tool/driver").cloned().unwrap_or_default();
        let tool = driver.get("name").and_then(Value::as_str).unwrap_or("unknown").to_owned();
        let version = driver.get("semanticVersion").or_else(|| driver.get("version")).and_then(Value::as_str).map(str::to_owned);
        // Each run's category: its automation details' id, else the upload's.
        let cat = run.pointer("/automationDetails/id").and_then(Value::as_str).map_or_else(|| category.to_owned(), |c| c.trim_end_matches('/').to_owned());
        let rules: Vec<Value> = driver.get("rules").and_then(Value::as_array).cloned().unwrap_or_default();
        let rule_of = |id: &str, index: Option<usize>| -> Option<&Value> { index.and_then(|i| rules.get(i)).or_else(|| rules.iter().find(|r| r.get("id").and_then(Value::as_str) == Some(id))) };
        let mut findings = Vec::new();
        for r in results {
            let index = r.get("ruleIndex").and_then(Value::as_u64).and_then(|i| usize::try_from(i).ok());
            let rule_id = r.get("ruleId").and_then(Value::as_str).or_else(|| r.pointer("/rule/id").and_then(Value::as_str)).or_else(|| index.and_then(|i| rules.get(i)).and_then(|x| x.get("id")).and_then(Value::as_str)).unwrap_or("unknown").to_owned();
            let rule = rule_of(&rule_id, index);
            let level = r.get("level").and_then(Value::as_str).or_else(|| rule.and_then(|x| x.pointer("/defaultConfiguration/level")).and_then(Value::as_str)).unwrap_or("warning");
            let severity = match level {
                "error" => "error",
                "note" | "none" => "note",
                _ => "warning",
            };
            let message = r.pointer("/message/text").and_then(Value::as_str).or_else(|| r.pointer("/message/markdown").and_then(Value::as_str)).unwrap_or_default().to_owned();
            let loc = r.pointer("/locations/0/physicalLocation").cloned().unwrap_or_default();
            let path = loc.pointer("/artifactLocation/uri").and_then(Value::as_str).unwrap_or_default().trim_start_matches("file://").to_owned();
            let region = loc.get("region").cloned().unwrap_or_default();
            let n = |k: &str| region.get(k).and_then(Value::as_i64);
            let start_line = n("startLine");
            let fingerprint = match r.get("partialFingerprints").and_then(Value::as_object).filter(|f| !f.is_empty()) {
                // The tool's own, as GitHub keys alerts.
                Some(f) => {
                    let mut keys: Vec<(&String, &Value)> = f.iter().collect();
                    keys.sort_by(|a, b| a.0.cmp(b.0));
                    hex::encode(Sha256::digest(format!("{tool}\0{rule_id}\0{path}\0{}", serde_json::to_string(&keys).unwrap_or_default()).as_bytes()))
                }
                None => hex::encode(Sha256::digest(format!("{tool}\0{rule_id}\0{path}\0{}\0{message}", start_line.unwrap_or(0)).as_bytes())),
            };
            findings.push(Finding {
                fingerprint,
                rule_id,
                rule_name: rule.and_then(|x| x.get("name")).and_then(Value::as_str).map(str::to_owned),
                rule_description: rule.and_then(|x| x.pointer("/shortDescription/text")).and_then(Value::as_str).map(str::to_owned),
                severity: severity.to_owned(),
                security_severity: rule.and_then(|x| x.pointer("/properties/security-severity")).and_then(|v| v.as_str().map(str::to_owned).or_else(|| v.as_f64().map(|f| f.to_string()))),
                message,
                path,
                start_line,
                end_line: n("endLine").or(start_line),
                start_column: n("startColumn"),
                end_column: n("endColumn"),
            });
        }
        out.push(Run { tool, version, category: cat, rules: rules.len(), findings });
    }
    Ok(out)
}

/// `PUT /repos/{o}/{r}/code-scanning/analysis` and `POST /repos/{o}/{r}/code-scanning/sarifs`.
pub async fn upload(s: &Site<'_>, claims: &Job, body: &Value) -> Result<Response, Fail> {
    let text = |k: &str| body.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned);
    let Some(sarif) = text("sarif") else { return Ok(invalid("sarif is required")) };
    let Some(commit) = text("commit_oid").or_else(|| text("commit_sha")) else { return Ok(invalid("commit_sha is required")) };
    if commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(invalid("commit_sha must be a full 40-character commit SHA"));
    }
    let Some(git_ref) = text("ref") else { return Ok(invalid("ref is required")) };
    if !git_ref.starts_with("refs/") {
        return Ok(invalid("ref must be a fully qualified ref (refs/heads/…, refs/pull/…/merge)"));
    }
    // What the upload is of: codeql-action's analysis key (the workflow and job) and category.
    let category = text("analysis_key").map(|k| match text("analysis_name") {
        Some(n) if n != k => format!("{k}:{n}"),
        _ => k,
    });
    let parsed = unpack(&sarif).and_then(|v| runs_of(&v, category.as_deref().unwrap_or_default()));
    let runs = match parsed {
        Ok(r) => r,
        Err(why) => return Ok(srv::json(StatusCode::BAD_REQUEST, &json!({ "message": why }))),
    };
    let sarif_id = crate::util::new_id();
    let (repo, sid, cm, rf, leg, gz) = (s.repo.id.clone(), sarif_id.clone(), commit.clone(), git_ref.clone(), claims.leg.clone(), sarif.into_bytes());
    let fresh: Vec<Finding> = s
        .hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let now = crate::util::now_unix();
            let mut fresh = Vec::new();
            for run in &runs {
                tx.execute(
                    "INSERT INTO ci_code_scanning_analyses (repo_id, sarif_id, commit_sha, git_ref, category, tool, tool_version, results, rules, sarif, leg_id, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    params![repo, sid, cm, rf, run.category, run.tool, run.version, i64::try_from(run.findings.len()).unwrap_or(i64::MAX), i64::try_from(run.rules).unwrap_or(i64::MAX), gz, leg, now],
                )?;
                let analysis = tx.last_insert_rowid();
                let mut seen: HashSet<String> = HashSet::new();
                for f in &run.findings {
                    if !seen.insert(f.fingerprint.clone()) {
                        continue;
                    }
                    let known: Option<(i64, String)> = tx
                        .query_row("SELECT number, state FROM ci_code_scanning_alerts WHERE repo_id = ?1 AND git_ref = ?2 AND fingerprint = ?3", params![repo, rf, f.fingerprint], |r| Ok((r.get(0)?, r.get(1)?)))
                        .optional()?;
                    match known {
                        Some((number, state)) => {
                            // Found again: open, unless someone dismissed it.
                            let state = if state == "dismissed" { state } else { "open".to_owned() };
                            tx.execute(
                                "UPDATE ci_code_scanning_alerts SET state = ?3, message = ?4, start_line = ?5, end_line = ?6, start_column = ?7, end_column = ?8, commit_sha = ?9, last_analysis = ?10, updated_at = ?11, fixed_at = NULL
                                 WHERE repo_id = ?1 AND number = ?2",
                                params![repo, number, state, f.message, f.start_line, f.end_line, f.start_column, f.end_column, cm, analysis, now],
                            )?;
                        }
                        None => {
                            let number: i64 = tx.query_row("SELECT COALESCE(MAX(number), 0) + 1 FROM ci_code_scanning_alerts WHERE repo_id = ?1", [&repo], |r| r.get(0))?;
                            tx.execute(
                                "INSERT INTO ci_code_scanning_alerts (repo_id, number, git_ref, fingerprint, tool, category, rule_id, rule_name, rule_description, severity, security_severity,
                                    message, path, start_line, end_line, start_column, end_column, commit_sha, state, first_analysis, last_analysis, created_at, updated_at)
                                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, 'open', ?19, ?19, ?20, ?20)",
                                params![
                                    repo, number, rf, f.fingerprint, run.tool, run.category, f.rule_id, f.rule_name, f.rule_description, f.severity, f.security_severity, f.message, f.path, f.start_line,
                                    f.end_line, f.start_column, f.end_column, cm, analysis, now
                                ],
                            )?;
                            fresh.push(f.clone());
                        }
                    }
                }
                // What this tool and category found on this ref before, and no longer: fixed.
                let mut q = tx.prepare("SELECT number, fingerprint FROM ci_code_scanning_alerts WHERE repo_id = ?1 AND git_ref = ?2 AND tool = ?3 AND category = ?4 AND state = 'open'")?;
                let open: Vec<(i64, String)> = q.query_map(params![repo, rf, run.tool, run.category], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
                drop(q);
                for (number, fp) in open.into_iter().filter(|(_, fp)| !seen.contains(fp)) {
                    let _ = fp;
                    tx.execute("UPDATE ci_code_scanning_alerts SET state = 'fixed', fixed_at = ?3, updated_at = ?3, last_analysis = ?4 WHERE repo_id = ?1 AND number = ?2", params![repo, number, now, analysis])?;
                }
            }
            tx.commit()?;
            Ok(fresh)
        })
        .await
        .map_err(internal)?;
    // The uploading leg says what's new, where the run shows it.
    for f in fresh.iter().take(MAX_ANNOTATIONS) {
        let level = if f.severity == "error" { "error" } else { "warning" };
        let title = format!("Code scanning: {}", f.rule_name.as_deref().unwrap_or(&f.rule_id));
        let line = f.start_line.and_then(|l| u32::try_from(l).ok());
        let end = f.end_line.and_then(|l| u32::try_from(l).ok());
        let _ = crate::ci::runs::annotation(s.hub, &claims.leg, None, level, Some(title), &f.message, Some(f.path.clone()).filter(|p| !p.is_empty()), line, end, None, None).await;
    }
    let url = format!("{}/code-scanning/sarifs/{sarif_id}", s.api);
    Ok(srv::json(StatusCode::ACCEPTED, &json!({ "id": sarif_id, "url": url })))
}

/// `GET /repos/{o}/{r}/code-scanning/sarifs/{id}`: read at once, so complete when it's known.
pub async fn sarif(s: &Site<'_>, id: &str) -> Result<Response, Fail> {
    let (repo, sid) = (s.repo.id.clone(), id.to_owned());
    let known: bool = s.hub.store.call(move |c| Ok(c.query_row("SELECT 1 FROM ci_code_scanning_analyses WHERE repo_id = ?1 AND sarif_id = ?2", params![repo, sid], |_| Ok(())).optional()?.is_some())).await.map_err(internal)?;
    if !known {
        return Ok(srv::json(StatusCode::NOT_FOUND, &json!({ "message": "Not Found" })));
    }
    Ok(srv::json(StatusCode::OK, &json!({ "processing_status": "complete", "analyses_url": format!("{}/code-scanning/analyses?sarif_id={id}", s.api), "errors": Value::Null })))
}

type AnalysisRow = (i64, String, String, String, String, String, Option<String>, i64, i64, i64);

/// `GET /repos/{o}/{r}/code-scanning/analyses` (`ref`, `tool_name`, `sarif_id`), newest first.
pub async fn analyses(s: &Site<'_>, q: &std::collections::HashMap<String, String>) -> Result<Response, Fail> {
    let f = |k: &str| q.get(k).filter(|v| !v.is_empty()).cloned();
    let (repo, rf, tool, sid) = (s.repo.id.clone(), f("ref"), f("tool_name"), f("sarif_id"));
    let rows: Vec<AnalysisRow> = s
        .hub
        .store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT id, sarif_id, commit_sha, git_ref, category, tool, tool_version, results, rules, created_at FROM ci_code_scanning_analyses
                 WHERE repo_id = ?1 AND (?2 IS NULL OR git_ref = ?2) AND (?3 IS NULL OR tool = ?3) AND (?4 IS NULL OR sarif_id = ?4) ORDER BY id DESC LIMIT 100",
            )?;
            let rows = st.query_map(params![repo, rf, tool, sid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(internal)?;
    let list: Vec<Value> = rows
        .into_iter()
        .map(|(id, sid, sha, rf, cat, tool, ver, results, rules, at)| {
            json!({
                "id": id, "ref": rf, "commit_sha": sha, "analysis_key": cat.split(':').next().unwrap_or_default(), "category": cat, "environment": "{}", "error": "",
                "created_at": rfc3339(at), "results_count": results, "rules_count": rules, "url": format!("{}/code-scanning/analyses/{id}", s.api), "sarif_id": sid,
                "tool": { "name": tool, "guid": Value::Null, "version": ver }, "deletable": true, "warning": "",
            })
        })
        .collect();
    Ok(srv::json(StatusCode::OK, &Value::Array(list)))
}

const ALERT_COLUMNS: &str = "number, git_ref, tool, category, rule_id, rule_name, rule_description, severity, security_severity, message, path, start_line, end_line, start_column, end_column,
    commit_sha, state, created_at, updated_at, fixed_at";

fn alert_json(api: &str, html: &str, r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let number: i64 = r.get(0)?;
    let at = |i: usize| -> rusqlite::Result<Option<String>> { Ok(r.get::<_, Option<i64>>(i)?.map(rfc3339)) };
    Ok(json!({
        "number": number,
        "created_at": at(17)?,
        "updated_at": at(18)?,
        "url": format!("{api}/code-scanning/alerts/{number}"),
        "html_url": format!("{html}/security/code-scanning/{number}"),
        "instances_url": format!("{api}/code-scanning/alerts/{number}/instances"),
        "state": r.get::<_, String>(16)?,
        "fixed_at": at(19)?,
        "dismissed_by": Value::Null, "dismissed_at": Value::Null, "dismissed_reason": Value::Null, "dismissed_comment": Value::Null,
        "rule": {
            "id": r.get::<_, String>(4)?, "name": r.get::<_, Option<String>>(5)?, "description": r.get::<_, Option<String>>(6)?.unwrap_or_default(),
            "severity": r.get::<_, String>(7)?, "security_severity_level": r.get::<_, Option<String>>(8)?, "tags": [],
        },
        "tool": { "name": r.get::<_, String>(2)?, "guid": Value::Null, "version": Value::Null },
        "most_recent_instance": {
            "ref": r.get::<_, String>(1)?, "analysis_key": r.get::<_, String>(3)?.split(':').next().unwrap_or_default(), "category": r.get::<_, String>(3)?, "environment": "{}",
            "state": r.get::<_, String>(16)?, "commit_sha": r.get::<_, String>(15)?, "message": { "text": r.get::<_, String>(9)? }, "classifications": [],
            "location": { "path": r.get::<_, String>(10)?, "start_line": r.get::<_, Option<i64>>(11)?, "end_line": r.get::<_, Option<i64>>(12)?, "start_column": r.get::<_, Option<i64>>(13)?, "end_column": r.get::<_, Option<i64>>(14)? },
        },
    }))
}

/// `GET /repos/{o}/{r}/code-scanning/alerts` (`ref`, `state`, `tool_name`, `severity`), and one.
pub async fn alerts(s: &Site<'_>, number: Option<&str>, q: &std::collections::HashMap<String, String>) -> Result<Response, Fail> {
    let f = |k: &str| q.get(k).filter(|v| !v.is_empty()).cloned();
    let number = match number {
        Some(n) => match n.parse::<i64>() {
            Ok(n) => Some(n),
            Err(_) => return Ok(srv::json(StatusCode::NOT_FOUND, &json!({ "message": "Not Found" }))),
        },
        None => None,
    };
    let (repo, rf, state, tool, severity) = (s.repo.id.clone(), f("ref"), f("state"), f("tool_name"), f("severity"));
    let (api, html) = (s.api.clone(), s.html.clone());
    let list: Vec<Value> = s
        .hub
        .store
        .call(move |c| {
            let mut st = c.prepare(&format!(
                "SELECT {ALERT_COLUMNS} FROM ci_code_scanning_alerts WHERE repo_id = ?1 AND (?2 IS NULL OR number = ?2) AND (?3 IS NULL OR git_ref = ?3) AND (?4 IS NULL OR state = ?4)
                 AND (?5 IS NULL OR tool = ?5) AND (?6 IS NULL OR severity = ?6 OR security_severity = ?6) ORDER BY number DESC LIMIT 1000"
            ))?;
            let rows = st.query_map(params![repo, number, rf, state, tool, severity], |r| alert_json(&api, &html, r))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(internal)?;
    match number {
        Some(_) => Ok(list.into_iter().next().map_or_else(|| srv::json(StatusCode::NOT_FOUND, &json!({ "message": "Not Found" })), |a| srv::json(StatusCode::OK, &a))),
        None => Ok(srv::json(StatusCode::OK, &Value::Array(list))),
    }
}

/// What alerts a repository has open, by tool, for its page (and kiln's own API).
pub async fn open_by_tool(hub: &crate::hub::Hub, repo_id: &str) -> anyhow::Result<BTreeMap<String, i64>> {
    let repo = repo_id.to_owned();
    hub.store
        .call(move |c| {
            let mut st = c.prepare("SELECT tool, COUNT(*) FROM ci_code_scanning_alerts WHERE repo_id = ?1 AND state = 'open' GROUP BY tool")?;
            let rows = st.query_map([&repo], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            Ok(rows.collect::<Result<BTreeMap<_, _>, _>>()?)
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packed(v: &Value) -> String {
        use std::io::Write as _;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let _ = gz.write_all(v.to_string().as_bytes());
        base64::engine::general_purpose::STANDARD.encode(gz.finish().unwrap_or_default())
    }

    #[test]
    fn w14_sarif_is_read_as_github_reads_it() {
        let sarif = json!({
            "version": "2.1.0",
            "runs": [{
                "tool": { "driver": { "name": "ESLint", "version": "9.0.0", "rules": [{ "id": "no-eval", "name": "NoEval", "shortDescription": { "text": "eval is evil" }, "defaultConfiguration": { "level": "error" }, "properties": { "security-severity": "8.8" } }] } },
                "results": [
                    { "ruleId": "no-eval", "ruleIndex": 0, "message": { "text": "eval can be harmful" }, "locations": [{ "physicalLocation": { "artifactLocation": { "uri": "src/a.js" }, "region": { "startLine": 3, "startColumn": 5 } } }] },
                    { "ruleId": "no-eval", "level": "note", "message": { "text": "again" }, "partialFingerprints": { "primaryLocationLineHash": "abc:1" }, "locations": [{ "physicalLocation": { "artifactLocation": { "uri": "file://src/b.js" }, "region": { "startLine": 9, "endLine": 10 } } }] }
                ]
            }]
        });
        let runs = unpack(&packed(&sarif)).and_then(|v| runs_of(&v, "/language:js")).unwrap_or_default();
        assert_eq!(runs.len(), 1);
        let r = runs.first().cloned().unwrap_or_else(|| Run { tool: String::new(), version: None, category: String::new(), rules: 0, findings: Vec::new() });
        assert_eq!((r.tool.as_str(), r.version.as_deref(), r.category.as_str(), r.rules), ("ESLint", Some("9.0.0"), "/language:js", 1));
        let a = r.findings.first().cloned();
        let b = r.findings.get(1).cloned();
        assert_eq!(a.as_ref().map(|f| (f.severity.as_str(), f.path.as_str(), f.start_line, f.end_line, f.security_severity.as_deref(), f.rule_name.as_deref())), Some(("error", "src/a.js", Some(3), Some(3), Some("8.8"), Some("NoEval"))));
        assert_eq!(b.as_ref().map(|f| (f.severity.as_str(), f.path.as_str(), f.end_line)), Some(("note", "src/b.js", Some(10))));
        assert_ne!(a.map(|f| f.fingerprint), b.map(|f| f.fingerprint));
        // What GitHub refuses.
        assert!(runs_of(&json!({ "version": "2.1.0" }), "").is_err_and(|e| e.contains("Missing 'runs' array")));
        assert!(runs_of(&json!({ "runs": [{ "tool": {} }] }), "").is_err_and(|e| e.contains("results")));
        assert!(unpack("not base64!").is_err_and(|e| e.starts_with("rejecting SARIF")));
        // Plain base64 JSON is taken too.
        assert!(unpack(&base64::engine::general_purpose::STANDARD.encode(b"{\"runs\":[]}")).is_ok());
    }
}
