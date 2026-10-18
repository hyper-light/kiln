//! Pull requests for jobs (docs/ci.md W14-R11): GitHub's pull request and issue comment endpoints
//! over kiln's diffs and change requests (a pull request's number is the diff's or change
//! request's, which share their numbers), so `actions/github-script`, `gh` and the actions that
//! comment on, review or read pull requests work unmodified:
//! - `GET /pulls`, `/pulls/{n}`, `/pulls/{n}/files`, `/pulls/{n}/commits`;
//! - `GET`/`POST /pulls/{n}/comments` (line comments: a thread on the version the commit is),
//!   `GET`/`PATCH`/`DELETE /pulls/comments/{id}`;
//! - `GET`/`POST /pulls/{n}/reviews` (`APPROVE`, `REQUEST_CHANGES`, `COMMENT`, or pending);
//! - `GET /issues/{n}`, `GET`/`POST /issues/{n}/comments`, `GET`/`PATCH`/`DELETE
//!   /issues/comments/{id}` (comments on the diff as a whole).
//!
//! A job writes as `github-actions[bot]`; its reviews are an agent's (they count only where
//! `agents-can-approve`). Comments' ids are numbers, as GitHub's are.

use http::{Method, StatusCode};
use kiln_ci::plan::perms::Level;
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use super::repos::Site;
use crate::authz::{Caller, Grant, Rights, Scope};
use crate::ci::Fail;
use crate::ci::diffs::{self, Diff, Version};
use crate::ci::token::Job;
use crate::http::server::{self as srv, Response};

fn rfc3339(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix).ok().and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()).unwrap_or_default()
}

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn not_found() -> Response {
    srv::json(StatusCode::NOT_FOUND, &json!({ "message": "Not Found" }))
}

fn invalid(m: &str) -> Response {
    srv::json(StatusCode::UNPROCESSABLE_ENTITY, &json!({ "message": "Validation Failed", "errors": [{ "message": m }] }))
}

fn forbidden(scope: &str) -> Response {
    srv::json(StatusCode::FORBIDDEN, &json!({ "message": format!("Resource not accessible by integration: the job's token lacks `{scope}: write`") }))
}

/// A principal as GitHub shows a user.
fn user(principal: &str) -> Value {
    if principal.starts_with("job:") {
        return json!({ "login": "github-actions[bot]", "id": 41898282, "type": "Bot" });
    }
    let (login, kind) = match principal.split_once(':') {
        Some(("user", name)) => (name.to_owned(), "User"),
        Some(("agent", name)) => (format!("{name}[bot]"), "Bot"),
        _ => (principal.to_owned(), "User"),
    };
    json!({ "login": login, "id": 0, "type": kind })
}

/// The job as a reviewer: it reads and reviews diffs in its repository (its token's
/// `pull-requests` permission decided whether it may write).
fn job_caller(j: &Job, repo: &str) -> Caller {
    let here = Scope::Repo { repo: repo.to_owned() };
    let grants = ["diffs.read", "diffs.review"].iter().map(|c| Grant { what: (*c).to_owned(), scope: here.clone(), deny: false }).collect();
    Caller::with(crate::authz::gate::Kind::Job, &format!("job:{}", j.leg), Some(Rights { grants, ..Rights::default() }), None)
}

/// The diff or change request numbered `n` in this repository.
async fn find(s: &Site<'_>, n: &str) -> Result<Option<Diff>, Fail> {
    if n.parse::<u32>().is_err() {
        return Ok(None);
    }
    Ok(diffs::find(s.hub, n).await.map_err(internal)?.filter(|d| d.repo.eq_ignore_ascii_case(&s.repo.name)))
}

/// Where a pull request landed: its commit and when.
async fn landed(s: &Site<'_>, d: &Diff) -> (Option<String>, Option<i64>) {
    let id = d.id.clone();
    s.hub.store.call(move |c| Ok(c.query_row("SELECT landed_commit, landed_at FROM diffs WHERE id = ?1", [&id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?.unwrap_or((None, None)))).await.unwrap_or((None, None))
}

/// How many comments a diff has: on it as a whole, and on its lines.
async fn counts(s: &Site<'_>, d: &Diff) -> (i64, i64) {
    let id = d.id.clone();
    s.hub
        .store
        .call(move |c| {
            let n = |file: bool| -> rusqlite::Result<i64> {
                c.query_row(
                    &format!("SELECT COUNT(*) FROM review_comments c JOIN review_threads t ON t.id = c.thread_id WHERE t.diff_id = ?1 AND c.published = 1 AND t.file IS {} NULL", if file { "NOT" } else { "" }),
                    [&id],
                    |r| r.get(0),
                )
            };
            Ok((n(false)?, n(true)?))
        })
        .await
        .unwrap_or((0, 0))
}

/// Makes sure the mirror has a commit (diffs' versions live in the hosted repository).
async fn have(s: &Site<'_>, sha: &str) {
    let _ = crate::gitops::ensure_commit(s.hub, &s.repo, sha).await;
}

/// A pull request in GitHub's shape.
async fn pr_json(s: &Site<'_>, d: &Diff, vs: &[Version]) -> Value {
    let latest = vs.last();
    let (head, base) = (latest.map(|v| v.commit.clone()).unwrap_or_default(), latest.map(|v| v.base.clone()).unwrap_or_default());
    let (merge_commit, merged_at) = landed(s, d).await;
    let (comments, review_comments) = counts(s, d).await;
    let closed = matches!(d.state.as_str(), "landed" | "abandoned");
    let n = d.number;
    let head_ref = d.branch.clone().unwrap_or_else(|| d.name.clone());
    let side = |r: &str, sha: &str| json!({ "label": format!("{}:{r}", s.owner()), "ref": r, "sha": sha, "user": user(&format!("user:{}", s.owner())), "repo": { "id": 0, "name": s.name(), "full_name": s.repo.name, "url": s.api, "html_url": s.html, "default_branch": s.default_branch } });
    let reviewers: Vec<Value> = {
        let id = d.id.clone();
        s.hub
            .store
            .call(move |c| {
                let mut st = c.prepare("SELECT reviewer FROM diff_reviewers WHERE diff_id = ?1 AND state IN ('not-looked', 'reviewing')")?;
                let rows = st.query_map([&id], |r| r.get::<_, String>(0))?;
                Ok(rows.filter_map(Result::ok).map(|r| user(&r)).collect())
            })
            .await
            .unwrap_or_default()
    };
    let (mut adds, mut dels, mut files) = (0i64, 0i64, 0i64);
    if let Some(v) = latest {
        have(s, &v.commit).await;
        if let Some(stat) = s.hub_text(&["diff", "--numstat", &v.parent, &v.commit]).await {
            for l in stat.lines() {
                let mut p = l.split('\t');
                adds += p.next().and_then(|a| a.parse::<i64>().ok()).unwrap_or(0);
                dels += p.next().and_then(|a| a.parse::<i64>().ok()).unwrap_or(0);
                files += 1;
            }
        }
    }
    let commits = match (&d.branch, latest) {
        (Some(_), Some(v)) => s.hub_text(&["rev-list", "--count", &format!("{}..{}", v.base, v.commit)]).await.and_then(|c| c.trim().parse::<i64>().ok()).unwrap_or(1),
        _ => 1,
    };
    let url = format!("{}/pulls/{n}", s.api);
    let mut v = json!({
        "url": url, "id": n, "node_id": format!("kiln-pr-{}", d.id), "number": n,
        "html_url": format!("{}/diffs/{}", s.origin, d.name), "diff_url": format!("{}/diffs/{}.diff", s.origin, d.name), "patch_url": format!("{}/diffs/{}.patch", s.origin, d.name),
        "issue_url": format!("{}/issues/{n}", s.api), "commits_url": format!("{url}/commits"), "review_comments_url": format!("{url}/comments"), "comments_url": format!("{}/issues/{n}/comments", s.api), "statuses_url": format!("{}/statuses/{head}", s.api),
        "state": if closed { "closed" } else { "open" }, "locked": false, "title": d.title, "body": d.summary, "user": user(&d.author),
        "labels": [], "milestone": null, "assignee": null, "assignees": [], "requested_reviewers": reviewers, "requested_teams": [],
    });
    let more = json!({
        "draft": d.state == "draft", "created_at": rfc3339(d.created_at), "updated_at": rfc3339(d.updated_at),
        "closed_at": if closed { Value::String(rfc3339(merged_at.unwrap_or(d.updated_at))) } else { Value::Null }, "merged_at": merged_at.map(rfc3339),
        "merged": d.state == "landed", "merge_commit_sha": merge_commit, "mergeable": if closed { Value::Null } else { Value::Bool(true) }, "mergeable_state": if d.state == "accepted" { "clean" } else { "blocked" },
        "head": side(&head_ref, &head), "base": side(&d.trunk, &base),
        "author_association": "MEMBER", "auto_merge": null, "comments": comments, "review_comments": review_comments, "commits": commits, "additions": adds, "deletions": dels, "changed_files": files,
        "kiln": { "id": d.name, "kind": d.kind, "state": d.state, "version": latest.map(|v| v.n) },
    });
    if let (Some(o), Value::Object(m)) = (v.as_object_mut(), more) {
        o.extend(m);
    }
    v
}

impl Site<'_> {
    /// git in the mirror, its output.
    pub(crate) async fn hub_text(&self, args: &[&str]) -> Option<String> {
        let mut a = vec!["-C", self.mirror.as_str()];
        a.extend_from_slice(args);
        crate::gitops::run_git(self.hub, &a, crate::gitops::LS_REMOTE_TIMEOUT).await.ok().map(|o| String::from_utf8_lossy(&o).into_owned())
    }
}

/// `GET /pulls?state=open|closed|all&base=&head=`.
pub async fn list(s: &Site<'_>, q: &std::collections::HashMap<String, String>) -> Result<Response, Fail> {
    let (repo, want) = (s.repo.id.clone(), q.get("state").cloned().unwrap_or_else(|| "open".into()));
    let base = q.get("base").cloned();
    let per_page: i64 = q.get("per_page").and_then(|p| p.parse().ok()).unwrap_or(30).clamp(1, 100);
    let page: i64 = q.get("page").and_then(|p| p.parse().ok()).unwrap_or(1).max(1);
    let keys: Vec<String> = s
        .hub
        .store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT id FROM diffs WHERE repo_id = ?1 AND (?2 = 'all' OR (?2 = 'open' AND state NOT IN ('landed', 'abandoned')) OR (?2 = 'closed' AND state IN ('landed', 'abandoned')))
                 AND (?3 IS NULL OR trunk = ?3) ORDER BY number DESC LIMIT ?4 OFFSET ?5",
            )?;
            let rows = st.query_map(params![repo, want, base, per_page, (page - 1) * per_page], |r| r.get::<_, String>(0))?;
            Ok(rows.filter_map(Result::ok).collect())
        })
        .await
        .map_err(internal)?;
    let head = q.get("head").map(|h| h.rsplit(':').next().unwrap_or(h).to_owned());
    let mut out = Vec::new();
    for k in keys {
        let Some(d) = diffs::find(s.hub, &k).await.map_err(internal)? else { continue };
        if head.as_deref().is_some_and(|h| d.branch.as_deref() != Some(h) && d.name != h) {
            continue;
        }
        let vs = diffs::versions(s.hub, &d).await.map_err(internal)?;
        out.push(pr_json(s, &d, &vs).await);
    }
    Ok(srv::json(StatusCode::OK, &out))
}

/// `GET /pulls/{n}`.
pub async fn get(s: &Site<'_>, n: &str) -> Result<Response, Fail> {
    let Some(d) = find(s, n).await? else { return Ok(not_found()) };
    let vs = diffs::versions(s.hub, &d).await.map_err(internal)?;
    Ok(srv::json(StatusCode::OK, &pr_json(s, &d, &vs).await))
}

/// `GET /pulls/{n}/files`: the latest version's.
pub async fn files(s: &Site<'_>, n: &str) -> Result<Response, Fail> {
    let Some(d) = find(s, n).await? else { return Ok(not_found()) };
    let Some(v) = diffs::versions(s.hub, &d).await.map_err(internal)?.pop() else { return Ok(srv::json(StatusCode::OK, &json!([]))) };
    have(s, &v.commit).await;
    let (list, _) = super::repos::diff_files(s, Some(&v.parent), &v.commit).await;
    // Each file's patch, as GitHub gives it.
    let mut out = Vec::new();
    for mut f in list {
        let name = f.get("filename").and_then(Value::as_str).unwrap_or_default().to_owned();
        let patch = s.hub_text(&["diff", "--no-color", &v.parent, &v.commit, "--", &name]).await.unwrap_or_default();
        let hunks: String = patch.split_once("\n@@").map(|(_, rest)| format!("@@{rest}")).unwrap_or_default();
        if let Some(o) = f.as_object_mut() {
            o.insert("patch".into(), json!(hunks.trim_end()));
        }
        out.push(f);
    }
    Ok(srv::json(StatusCode::OK, &out))
}

/// `GET /pulls/{n}/commits`.
pub async fn commits(s: &Site<'_>, n: &str) -> Result<Response, Fail> {
    let Some(d) = find(s, n).await? else { return Ok(not_found()) };
    let Some(v) = diffs::versions(s.hub, &d).await.map_err(internal)?.pop() else { return Ok(srv::json(StatusCode::OK, &json!([]))) };
    have(s, &v.commit).await;
    let list = s.hub_text(&["rev-list", "--reverse", &format!("{}..{}", v.parent, v.commit)]).await.unwrap_or_default();
    let mut out = Vec::new();
    for sha in list.lines().filter(|l| !l.is_empty()) {
        if let Some(c) = super::repos::commit_json(s, sha, false).await {
            out.push(c);
        }
    }
    Ok(srv::json(StatusCode::OK, &out))
}

/// A comment row: (rowid, thread, author, body, created, updated, version, file, side, start, end, published).
type Row = (i64, String, String, String, i64, i64, u32, Option<String>, Option<String>, Option<u32>, Option<u32>, bool);

async fn comment_rows(s: &Site<'_>, d: &Diff, inline: bool) -> Result<Vec<Row>, Fail> {
    let id = d.id.clone();
    s.hub
        .store
        .call(move |c| {
            let sql = format!(
                "SELECT c.rowid, c.thread_id, c.author, c.body, c.created_at, c.updated_at, t.version, t.file, t.side, t.start_line, t.end_line, c.published
                 FROM review_comments c JOIN review_threads t ON t.id = c.thread_id WHERE t.diff_id = ?1 AND c.published = 1 AND t.file IS {} NULL ORDER BY c.created_at, c.rowid",
                if inline { "NOT" } else { "" }
            );
            let mut st = c.prepare(&sql)?;
            let rows = st.query_map([&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?)))?;
            Ok(rows.collect::<Result<Vec<Row>, _>>()?)
        })
        .await
        .map_err(internal)
}

fn issue_comment_json(s: &Site<'_>, n: u32, r: &Row) -> Value {
    let (id, _, author, body, created, updated, ..) = r;
    json!({ "id": id, "node_id": format!("kiln-comment-{id}"), "url": format!("{}/issues/comments/{id}", s.api), "html_url": format!("{}/diffs/{n}#comment-{id}", s.origin),
            "issue_url": format!("{}/issues/{n}", s.api), "body": body, "user": user(author), "created_at": rfc3339(*created), "updated_at": rfc3339(*updated), "author_association": "MEMBER" })
}

fn review_comment_json(s: &Site<'_>, n: u32, vs: &[Version], r: &Row, first: Option<i64>) -> Value {
    let (id, _, author, body, created, updated, version, file, side, start, end, _) = r;
    let commit = vs.iter().find(|v| v.n == *version).map(|v| v.commit.clone()).unwrap_or_default();
    let mut v = json!({
        "id": id, "node_id": format!("kiln-comment-{id}"), "url": format!("{}/pulls/comments/{id}", s.api), "pull_request_url": format!("{}/pulls/{n}", s.api),
        "html_url": format!("{}/diffs/{n}#comment-{id}", s.origin), "path": file, "commit_id": commit, "original_commit_id": commit,
        "line": end, "original_line": end, "start_line": if start == end { Value::Null } else { json!(start) }, "side": if side.as_deref() == Some("old") { "LEFT" } else { "RIGHT" },
        "position": null, "original_position": null, "diff_hunk": "", "body": body, "user": user(author), "created_at": rfc3339(*created), "updated_at": rfc3339(*updated),
        "author_association": "MEMBER", "pull_request_review_id": null, "subject_type": "line",
    });
    if let (Some(f), Some(o)) = (first.filter(|f| f != id), v.as_object_mut()) {
        o.insert("in_reply_to_id".into(), json!(f));
    }
    v
}

/// The thread a comment (by its number) is in.
async fn thread_of(s: &Site<'_>, comment: i64) -> Option<String> {
    s.hub.store.call(move |c| Ok(c.query_row("SELECT thread_id FROM review_comments WHERE rowid = ?1", [comment], |r| r.get(0)).optional()?)).await.ok().flatten()
}

/// The number of the comment just made (by its id).
async fn rowid(s: &Site<'_>, id: &str) -> i64 {
    let key = id.to_owned();
    s.hub.store.call(move |c| Ok(c.query_row("SELECT rowid FROM review_comments WHERE id = ?1", [&key], |r| r.get(0)).optional()?.unwrap_or(0))).await.unwrap_or(0)
}

/// Turns GitHub's line comment into kiln's: version (by `commit_id`), file, side, lines.
fn anchor(vs: &[Version], body: &Value) -> Value {
    let version = body.get("commit_id").and_then(Value::as_str).and_then(|c| vs.iter().find(|v| v.commit == c)).or(vs.last()).map(|v| v.n);
    let line = body.get("line").and_then(Value::as_u64).or_else(|| body.get("position").and_then(Value::as_u64));
    let start = body.get("start_line").and_then(Value::as_u64).or(line);
    let side = if body.get("side").and_then(Value::as_str) == Some("LEFT") { "old" } else { "new" };
    let file_level = body.get("subject_type").and_then(Value::as_str) == Some("file");
    json!({ "version": version, "file": body.get("path"), "side": side, "start_line": if file_level { json!(1) } else { json!(start) }, "end_line": if file_level { json!(1) } else { json!(line) } })
}

/// `GET`/`POST /pulls/{n}/comments`.
pub async fn review_comments(s: &Site<'_>, n: &str, job: &Job, body: Option<&Value>) -> Result<Response, Fail> {
    let Some(d) = find(s, n).await? else { return Ok(not_found()) };
    let vs = diffs::versions(s.hub, &d).await.map_err(internal)?;
    if let Some(b) = body {
        if !job.may("pull-requests", Level::Write) {
            return Ok(forbidden("pull-requests"));
        }
        let caller = job_caller(job, &s.repo.name);
        let text = b.get("body").and_then(Value::as_str).unwrap_or_default();
        let mut ask = anchor(&vs, b);
        if let Some(o) = ask.as_object_mut() {
            o.insert("body".into(), json!(text));
            o.insert("publish".into(), json!(true));
            if let Some(reply) = b.get("in_reply_to").and_then(Value::as_i64) {
                let Some(t) = thread_of(s, reply).await else { return Ok(invalid("in_reply_to: no such comment")) };
                o.insert("thread".into(), json!(t));
            }
        }
        if ask.get("file").is_none_or(Value::is_null) || ask.get("start_line").is_none_or(Value::is_null) {
            return Ok(invalid("a review comment names its `path` and `line`"));
        }
        let made = match crate::ci::review::comment(s.hub, &d, &caller, &ask).await {
            Ok(m) => m,
            Err((_, m)) => return Ok(invalid(&m)),
        };
        let id = rowid(s, made.get("id").and_then(Value::as_str).unwrap_or_default()).await;
        let rows = comment_rows(s, &d, true).await?;
        let Some(r) = rows.iter().find(|r| r.0 == id) else { return Ok(not_found()) };
        let first = rows.iter().find(|x| x.1 == r.1).map(|x| x.0);
        return Ok(srv::json(StatusCode::CREATED, &review_comment_json(s, d.number, &vs, r, first)));
    }
    let rows = comment_rows(s, &d, true).await?;
    let out: Vec<Value> = rows.iter().map(|r| review_comment_json(s, d.number, &vs, r, rows.iter().find(|x| x.1 == r.1).map(|x| x.0))).collect();
    Ok(srv::json(StatusCode::OK, &out))
}

/// `GET`/`POST /issues/{n}/comments`: comments on the pull request as a whole.
pub async fn issue_comments(s: &Site<'_>, n: &str, job: &Job, body: Option<&Value>) -> Result<Response, Fail> {
    let Some(d) = find(s, n).await? else { return Ok(not_found()) };
    if let Some(b) = body {
        if !job.may("issues", Level::Write) && !job.may("pull-requests", Level::Write) {
            return Ok(forbidden("issues"));
        }
        let text = b.get("body").and_then(Value::as_str).unwrap_or_default();
        if text.trim().is_empty() {
            return Ok(invalid("body is missing"));
        }
        let made = match crate::ci::review::comment(s.hub, &d, &job_caller(job, &s.repo.name), &json!({ "body": text, "publish": true })).await {
            Ok(m) => m,
            Err((_, m)) => return Ok(invalid(&m)),
        };
        let id = rowid(s, made.get("id").and_then(Value::as_str).unwrap_or_default()).await;
        let rows = comment_rows(s, &d, false).await?;
        let Some(r) = rows.iter().find(|r| r.0 == id) else { return Ok(not_found()) };
        return Ok(srv::json(StatusCode::CREATED, &issue_comment_json(s, d.number, r)));
    }
    let rows = comment_rows(s, &d, false).await?;
    Ok(srv::json(StatusCode::OK, &rows.iter().map(|r| issue_comment_json(s, d.number, r)).collect::<Vec<_>>()))
}

/// `GET`/`PATCH`/`DELETE /issues/comments/{id}` and `/pulls/comments/{id}`. A job edits and
/// deletes the comments jobs wrote (each run is another job; all are `github-actions[bot]`).
pub async fn comment(s: &Site<'_>, id: &str, job: &Job, method: &Method, body: Option<&Value>, inline: bool) -> Result<Response, Fail> {
    let Ok(num) = id.parse::<i64>() else { return Ok(not_found()) };
    let repo = s.repo.id.clone();
    let found: Option<(String, String)> = s
        .hub
        .store
        .call(move |c| Ok(c.query_row("SELECT c.author, d.id FROM review_comments c JOIN review_threads t ON t.id = c.thread_id JOIN diffs d ON d.id = t.diff_id WHERE c.rowid = ?1 AND d.repo_id = ?2 AND c.published = 1", params![num, repo], |r| Ok((r.get(0)?, r.get(1)?))).optional()?))
        .await
        .map_err(internal)?;
    let Some((author, diff)) = found else { return Ok(not_found()) };
    let Some(d) = diffs::find(s.hub, &diff).await.map_err(internal)? else { return Ok(not_found()) };
    if *method != Method::GET {
        let scope = if inline { "pull-requests" } else { "issues" };
        if !job.may(scope, Level::Write) && !job.may("pull-requests", Level::Write) {
            return Ok(forbidden(scope));
        }
        if !author.starts_with("job:") {
            return Ok(srv::json(StatusCode::FORBIDDEN, &json!({ "message": "a job edits only the comments jobs wrote" })));
        }
        if *method == Method::DELETE {
            s.hub.store.call(move |c| Ok(c.execute("DELETE FROM review_comments WHERE rowid = ?1", [num]).map(|_| ())?)).await.map_err(internal)?;
            return Ok(srv::status(StatusCode::NO_CONTENT));
        }
        let text = body.and_then(|b| b.get("body")).and_then(Value::as_str).unwrap_or_default().to_owned();
        if text.trim().is_empty() {
            return Ok(invalid("body is missing"));
        }
        s.hub.store.call(move |c| Ok(c.execute("UPDATE review_comments SET body = ?2, updated_at = ?3 WHERE rowid = ?1", params![num, text, crate::util::now_unix()]).map(|_| ())?)).await.map_err(internal)?;
    }
    let vs = diffs::versions(s.hub, &d).await.map_err(internal)?;
    let rows = comment_rows(s, &d, inline).await?;
    let Some(r) = rows.iter().find(|r| r.0 == num) else { return Ok(not_found()) };
    Ok(srv::json(StatusCode::OK, &if inline { review_comment_json(s, d.number, &vs, r, rows.iter().find(|x| x.1 == r.1).map(|x| x.0)) } else { issue_comment_json(s, d.number, r) }))
}

/// `GET /issues/{n}`: a pull request as GitHub's issues API shows it.
pub async fn issue(s: &Site<'_>, n: &str) -> Result<Response, Fail> {
    let Some(d) = find(s, n).await? else { return Ok(not_found()) };
    let (comments, _) = counts(s, &d).await;
    let closed = matches!(d.state.as_str(), "landed" | "abandoned");
    let num = d.number;
    Ok(srv::json(
        StatusCode::OK,
        &json!({
            "id": num, "node_id": format!("kiln-issue-{}", d.id), "number": num, "url": format!("{}/issues/{num}", s.api), "html_url": format!("{}/diffs/{}", s.origin, d.name),
            "state": if closed { "closed" } else { "open" }, "title": d.title, "body": d.summary, "user": user(&d.author), "labels": [], "assignees": [], "comments": comments,
            "created_at": rfc3339(d.created_at), "updated_at": rfc3339(d.updated_at), "closed_at": if closed { Value::String(rfc3339(d.updated_at)) } else { Value::Null },
            "pull_request": { "url": format!("{}/pulls/{num}", s.api), "html_url": format!("{}/diffs/{}", s.origin, d.name) }, "author_association": "MEMBER",
        }),
    ))
}

fn review_state(action: &str) -> &'static str {
    match action {
        "accept" => "APPROVED",
        "request_changes" => "CHANGES_REQUESTED",
        "resign" => "DISMISSED",
        _ => "COMMENTED",
    }
}

/// `GET`/`POST /pulls/{n}/reviews`: `{ event: APPROVE | REQUEST_CHANGES | COMMENT, body, comments }`
/// (without `event`, a pending review: its comments are drafts).
pub async fn reviews(s: &Site<'_>, n: &str, job: &Job, body: Option<&Value>) -> Result<Response, Fail> {
    let Some(d) = find(s, n).await? else { return Ok(not_found()) };
    let vs = diffs::versions(s.hub, &d).await.map_err(internal)?;
    let pr_url = format!("{}/pulls/{}", s.api, d.number);
    if let Some(b) = body {
        if !job.may("pull-requests", Level::Write) {
            return Ok(forbidden("pull-requests"));
        }
        let caller = job_caller(job, &s.repo.name);
        for c in b.get("comments").and_then(Value::as_array).into_iter().flatten() {
            let mut ask = anchor(&vs, c);
            if let Some(o) = ask.as_object_mut() {
                o.insert("body".into(), c.get("body").cloned().unwrap_or(Value::Null));
            }
            if let Err((_, m)) = crate::ci::review::comment(s.hub, &d, &caller, &ask).await {
                return Ok(invalid(&m));
            }
        }
        let action = match b.get("event").and_then(Value::as_str) {
            Some("APPROVE") => Some("accept"),
            Some("REQUEST_CHANGES") => Some("request_changes"),
            Some("COMMENT") => Some("comment"),
            None | Some("") => None,
            Some(other) => return Ok(invalid(&format!("event `{other}`: APPROVE, REQUEST_CHANGES or COMMENT"))),
        };
        let Some(action) = action else {
            return Ok(srv::json(StatusCode::OK, &json!({ "id": 0, "state": "PENDING", "user": user(&format!("job:{}", job.leg)), "body": b.get("body"), "pull_request_url": pr_url })));
        };
        let text = b.get("body").and_then(Value::as_str).unwrap_or_default();
        match crate::ci::review::review(s.hub, &d, &caller, &json!({ "action": action, "body": text })).await {
            Ok(r) => {
                let commit = vs.last().map(|v| v.commit.clone());
                return Ok(srv::json(
                    StatusCode::OK,
                    &json!({ "id": crate::util::now_unix(), "user": user(&format!("job:{}", job.leg)), "body": text, "state": review_state(action), "commit_id": commit, "submitted_at": rfc3339(crate::util::now_unix()), "pull_request_url": pr_url, "kiln": r }),
                ));
            }
            Err((_, m)) => return Ok(invalid(&m)),
        }
    }
    let id = d.id.clone();
    let rows: Vec<(i64, String, u32, String, String, i64)> = s
        .hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT rowid, reviewer, version, action, body, created_at FROM reviews WHERE diff_id = ?1 ORDER BY created_at, rowid")?;
            let rows = st.query_map([&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(internal)?;
    let out: Vec<Value> = rows
        .iter()
        .map(|(rid, reviewer, version, action, body, at)| {
            let commit = vs.iter().find(|v| v.n == *version).map(|v| v.commit.clone());
            json!({ "id": rid, "node_id": format!("kiln-review-{rid}"), "user": user(reviewer), "body": body, "state": review_state(action), "commit_id": commit, "submitted_at": rfc3339(*at), "pull_request_url": pr_url, "html_url": format!("{}/diffs/{}#review-{rid}", s.origin, d.name), "author_association": "MEMBER" })
        })
        .collect();
    Ok(srv::json(StatusCode::OK, &out))
}
