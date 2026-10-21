//! The facade's GraphQL (docs/ci.md W14-R11), at `/api/graphql` for a leg's `GITHUB_TOKEN`: the
//! queries and mutations `gh` and the actions that ask GitHub's GraphQL API send, answered over
//! what the REST half serves, in the shapes of GitHub's schema:
//! - `viewer`, `rateLimit`, and `node(id:)` and `nodes(ids:)` by the REST half's `node_id`s;
//! - `repository(owner:, name:)`, the token's own: its default branch, `ref`, `refs`, `object`,
//!   `labels`, `pullRequest(number:)`, `issueOrPullRequest(number:)` and `pullRequests` (by
//!   state, head, base and labels, paged);
//! - a pull request's fields, with its `labels`, `comments`, `commits`, `files`, `reviews`
//!   (`latestReviews`, `latestOpinionatedReviews`), `reviewRequests` and `statusCheckRollup`
//!   (its head commit's check runs and statuses);
//! - mutations: `addComment`, `updateIssueComment`, `deleteIssueComment`, `createPullRequest`,
//!   `updatePullRequest`, `closePullRequest`, `reopenPullRequest`, `addLabelsToLabelable`,
//!   `removeLabelsFromLabelable` and `addPullRequestReview`, through the REST half's handlers (so
//!   the token's permissions hold as they do there);
//! - `__type(name:)`'s fields, which `gh` asks an Enterprise Server for before deciding what to ask.
//!
//! It's lenient where GitHub is strict: a field kiln doesn't keep answers null (a connection,
//! empty), so a client asking for more than kiln keeps still gets what it does keep. An unknown
//! root field or mutation is an error naming it, and the leg gets a warning, as the REST half's
//! 501s give one.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use base64::Engine as _;
use http::{Method, StatusCode};
use http_body_util::BodyExt as _;
use kiln_ci::graphql::{self as gql, Document, Field, Kind, Selection};
use serde_json::{Map, Value, json};

use super::repos::Site;
use super::{pulls, records, repos};
use crate::ci::Fail;
use crate::ci::token::Job;
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

type Fut<'a> = Pin<Box<dyn Future<Output = Value> + Send + 'a>>;

/// One request's execution.
struct Cx<'a> {
    site: Site<'a>,
    claims: &'a Job,
    doc: &'a Document,
    vars: Map<String, Value>,
    /// The repository, as its object.
    repo: Value,
    errors: parking_lot::Mutex<Vec<Value>>,
    /// Routes asked that kiln doesn't answer, for the leg's warning.
    missing: parking_lot::Mutex<Vec<String>>,
}

impl Cx<'_> {
    fn error(&self, path: &[Value], kind: Option<&str>, message: &str) {
        let mut e = json!({ "path": path, "locations": [], "message": message });
        if let (Some(k), Some(o)) = (kind, e.as_object_mut()) {
            o.insert("type".into(), json!(k));
        }
        self.errors.lock().push(e);
    }

    fn missing(&self, path: &[Value], what: &str) {
        self.error(path, Some("NOT_IMPLEMENTED"), &format!("not implemented by kiln: {what}"));
        self.missing.lock().push(what.to_owned());
    }
}

/// Whether an object of type `ty` takes a fragment on `cond`: its own type, or an interface or
/// union it is in.
fn is_a(ty: &str, cond: &str) -> bool {
    ty == cond
        || match cond {
            "Node" => true,
            "Actor" => matches!(ty, "User" | "Bot" | "Organization"),
            "RepositoryOwner" | "ProfileOwner" => matches!(ty, "User" | "Organization"),
            "Comment" | "Deletable" | "Updatable" | "UpdatableComment" | "Reactable" | "Minimizable" => matches!(ty, "IssueComment" | "PullRequest" | "PullRequestReview"),
            "Labelable" | "Assignable" | "Closable" | "Lockable" | "Subscribable" | "UniformResourceLocatable" | "IssueOrPullRequest" | "ProjectV2Owner" => matches!(ty, "PullRequest" | "Repository"),
            "GitObject" => ty == "Commit",
            "StatusCheckRollupContext" => matches!(ty, "CheckRun" | "StatusContext"),
            "RequestedReviewer" => matches!(ty, "User" | "Bot" | "Team"),
            _ => false,
        }
}

/// Objects' fields together (one `json!` holds only so many).
fn joined(parts: Vec<Value>) -> Value {
    let mut out = Map::new();
    for p in parts {
        if let Value::Object(m) = p {
            out.extend(m);
        }
    }
    Value::Object(out)
}

fn text<'v>(v: &'v Value, key: &str) -> &'v str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn arg<'v>(args: &'v Map<String, Value>, key: &str) -> &'v str {
    args.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// A list argument's strings: one value given where a list belongs is a list of it, as GraphQL
/// coerces it (`$state: [PullRequestState!] = OPEN`).
fn list_arg<'v>(args: &'v Map<String, Value>, key: &str) -> Vec<&'v str> {
    match args.get(key) {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

/// A REST handler's answer: its status and its JSON.
async fn rest(r: Result<Response, Fail>) -> (u16, Value) {
    match r {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let bytes = resp.into_body().collect().await.map(|c| c.to_bytes()).unwrap_or_default();
            (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
        }
        Err((status, message)) => (status.as_u16(), json!({ "message": message })),
    }
}

/// An answer that went wrong, said at `path`; null.
fn refused(cx: &Cx<'_>, path: &[Value], (status, v): (u16, Value)) -> Value {
    let kind = match status {
        404 => "NOT_FOUND",
        403 => "FORBIDDEN",
        422 => "UNPROCESSABLE",
        _ => "INTERNAL",
    };
    cx.error(path, Some(kind), v.get("message").and_then(Value::as_str).unwrap_or("it went wrong"));
    Value::Null
}

/// A page's cursor.
fn cursor(i: usize) -> String {
    base64::engine::general_purpose::STANDARD.encode(format!("cursor:{i}"))
}

fn uncursor(c: &str) -> Option<usize> {
    let raw = base64::engine::general_purpose::STANDARD.decode(c).ok()?;
    String::from_utf8(raw).ok()?.strip_prefix("cursor:")?.parse().ok()
}

/// `items` as a connection of `of`, paged as `first`/`last`/`after`/`before` ask.
fn connection(items: Vec<Value>, args: &Map<String, Value>, of: &str) -> Value {
    let total = items.len();
    let mut start = args.get("after").and_then(Value::as_str).and_then(uncursor).map_or(0, |a| a + 1).min(total);
    let mut end = args.get("before").and_then(Value::as_str).and_then(uncursor).unwrap_or(total).min(total);
    start = start.min(end);
    if let Some(n) = args.get("first").and_then(Value::as_u64) {
        end = end.min(start.saturating_add(usize::try_from(n).unwrap_or(usize::MAX)));
    }
    if let Some(n) = args.get("last").and_then(Value::as_u64) {
        start = start.max(end.saturating_sub(usize::try_from(n).unwrap_or(usize::MAX)));
    }
    let page: Vec<Value> = items.into_iter().skip(start).take(end - start).collect();
    let edges: Vec<Value> = page.iter().enumerate().map(|(i, n)| json!({ "__typename": format!("{of}Edge"), "node": n, "cursor": cursor(start + i) })).collect();
    let (first, last) = (page.first().map(|_| cursor(start)), page.last().map(|_| cursor(end.saturating_sub(1))));
    json!({
        "__typename": format!("{of}Connection"), "totalCount": total, "nodes": page, "edges": edges,
        "pageInfo": { "__typename": "PageInfo", "hasNextPage": end < total, "hasPreviousPage": start > 0, "startCursor": first, "endCursor": last },
    })
}

/// Whether a selection asks for a connection's parts.
fn wants_connection(sub: &[Selection]) -> bool {
    sub.iter().any(|s| match s {
        Selection::Field(f) => matches!(f.name.as_str(), "nodes" | "edges" | "totalCount" | "pageInfo"),
        Selection::Inline { selection, .. } => wants_connection(selection),
        Selection::Spread { .. } => false,
    })
}

/// A REST user as GraphQL's actor: a bot's login without `[bot]`, as GitHub's GraphQL says it.
fn actor(u: &Value) -> Value {
    let login = text(u, "login");
    if login.is_empty() {
        return Value::Null;
    }
    let bot = text(u, "type") == "Bot";
    let name = if bot { login.trim_end_matches("[bot]") } else { login };
    json!({ "__typename": if bot { "Bot" } else { "User" }, "id": format!("kiln-actor-{login}"), "login": name, "name": name, "url": "", "avatarUrl": "", "resourcePath": format!("/{name}") })
}

fn viewer() -> Value {
    json!({ "__typename": "Bot", "id": "MDM6Qm90NDE4OTgyODI=", "databaseId": 41898282, "login": "github-actions", "name": "github-actions", "url": "", "avatarUrl": "" })
}

/// The repository as GraphQL's object, from the REST half's.
async fn repository(s: &Site<'_>, claims: &Job) -> Value {
    let (_, r) = rest(Ok(repos::repository(s))).await;
    let owner = json!({ "__typename": "Organization", "id": format!("kiln-owner-{}", s.owner()), "login": s.owner(), "name": s.owner(), "url": format!("{}/{}", s.origin, s.owner()) });
    let head = s.resolve(&s.default_branch).await;
    let default = json!({ "__typename": "Ref", "id": format!("kiln-ref-refs/heads/{}", s.default_branch), "name": s.default_branch, "prefix": "refs/heads/", "_oid": head });
    let permission = if claims.may("contents", kiln_ci::plan::perms::Level::Write) { "WRITE" } else { "READ" };
    joined(vec![
        json!({
            "__typename": "Repository", "id": text(&r, "node_id"), "databaseId": 0, "name": s.name(), "nameWithOwner": s.repo.name, "owner": owner,
            "url": text(&r, "html_url"), "resourcePath": format!("/{}", s.repo.name), "sshUrl": Value::Null, "homepageUrl": Value::Null, "description": Value::Null,
            "isPrivate": true, "visibility": "PRIVATE", "isFork": false, "isArchived": false, "isDisabled": false, "isEmpty": head.is_none(), "isMirror": false, "isTemplate": false, "isLocked": false,
            "hasIssuesEnabled": false, "hasWikiEnabled": false, "hasProjectsEnabled": false, "hasDiscussionsEnabled": false,
        }),
        json!({
            "mergeCommitAllowed": true, "squashMergeAllowed": true, "rebaseMergeAllowed": true, "deleteBranchOnMerge": false, "autoMergeAllowed": false,
            "viewerPermission": permission, "viewerCanAdminister": false, "viewerCanCreateProjects": false, "viewerSubscription": "UNSUBSCRIBED",
            "createdAt": text(&r, "created_at"), "updatedAt": text(&r, "updated_at"), "pushedAt": text(&r, "pushed_at"),
            "defaultBranchRef": default, "parent": Value::Null, "templateRepository": Value::Null, "stargazerCount": 0, "forkCount": 0,
        }),
    ])
}

/// A commit, from the REST half's.
fn commit_obj(c: &Value) -> Value {
    let sha = text(c, "sha");
    let message = c.pointer("/commit/message").and_then(Value::as_str).unwrap_or_default();
    let (headline, body) = message.split_once('\n').map_or((message, ""), |(h, b)| (h, b.trim_start_matches('\n')));
    let person = |who: &str| {
        let p = c.pointer(&format!("/commit/{who}")).cloned().unwrap_or_default();
        json!({ "__typename": "GitActor", "name": text(&p, "name"), "email": text(&p, "email"), "date": text(&p, "date"), "user": Value::Null })
    };
    json!({
        "__typename": "Commit", "id": format!("kiln-commit-{sha}"), "oid": sha, "abbreviatedOid": sha.get(..7).unwrap_or(sha), "url": text(c, "html_url"),
        "message": message, "messageHeadline": headline, "messageBody": body, "committedDate": c.pointer("/commit/committer/date"), "authoredDate": c.pointer("/commit/author/date"),
        "author": person("author"), "committer": person("committer"), "authors": { "__typename": "GitActorConnection", "totalCount": 1, "nodes": [person("author")] },
    })
}

/// A label, from the REST half's.
fn label_obj(l: &Value) -> Value {
    json!({ "__typename": "Label", "id": text(l, "node_id"), "name": text(l, "name"), "color": text(l, "color"), "description": l.get("description"), "url": text(l, "url"), "isDefault": false })
}

/// An issue comment, from the REST half's.
fn comment_obj(c: &Value) -> Value {
    let mine = c.pointer("/user/login").and_then(Value::as_str) == Some("github-actions[bot]");
    json!({
        "__typename": "IssueComment", "id": text(c, "node_id"), "databaseId": c.get("id"), "body": text(c, "body"), "bodyText": text(c, "body"),
        "author": actor(c.get("user").unwrap_or(&Value::Null)), "authorAssociation": "MEMBER", "createdAt": text(c, "created_at"), "updatedAt": text(c, "updated_at"),
        "url": text(c, "html_url"), "isMinimized": false, "minimizedReason": Value::Null, "includesCreatedEdit": false, "reactionGroups": [],
        "viewerDidAuthor": mine, "viewerCanUpdate": mine, "viewerCanDelete": mine, "viewerCanMinimize": false,
    })
}

/// A review, from the REST half's.
fn review_obj(r: &Value) -> Value {
    json!({
        "__typename": "PullRequestReview", "id": text(r, "node_id"), "databaseId": r.get("id"), "author": actor(r.get("user").unwrap_or(&Value::Null)), "authorAssociation": "MEMBER",
        "body": text(r, "body"), "state": text(r, "state"), "submittedAt": r.get("submitted_at"), "createdAt": r.get("submitted_at"), "url": text(r, "html_url"),
        "commit": r.get("commit_id").and_then(Value::as_str).map(|sha| json!({ "__typename": "Commit", "id": format!("kiln-commit-{sha}"), "oid": sha })),
    })
}

/// A pull request, from the REST half's.
fn pr_obj(cx_repo: &Value, p: &Value) -> Value {
    let merged = p.get("merged").and_then(Value::as_bool).unwrap_or(false);
    let state = if merged {
        "MERGED"
    } else if text(p, "state") == "closed" {
        "CLOSED"
    } else {
        "OPEN"
    };
    let n = p.get("number").cloned().unwrap_or_default();
    let labels: Vec<Value> = p.get("labels").and_then(Value::as_array).into_iter().flatten().map(label_obj).collect();
    let requests: Vec<Value> =
        p.get("requested_reviewers").and_then(Value::as_array).into_iter().flatten().map(|u| json!({ "__typename": "ReviewRequest", "requestedReviewer": actor(u), "asCodeOwner": false })).collect();
    let kiln_state = p.pointer("/kiln/state").and_then(Value::as_str).unwrap_or_default();
    let open = state == "OPEN";
    joined(vec![
        json!({
            "__typename": "PullRequest", "id": text(p, "node_id"), "databaseId": p.get("id"), "number": n, "title": text(p, "title"), "body": text(p, "body"), "bodyText": text(p, "body"),
            "url": text(p, "html_url"), "permalink": text(p, "html_url"), "resourcePath": format!("/{}/pull/{n}", cx_repo.get("nameWithOwner").and_then(Value::as_str).unwrap_or_default()),
            "state": state, "isDraft": p.get("draft"), "closed": !open, "merged": merged, "mergedAt": p.get("merged_at"), "closedAt": p.get("closed_at"),
            "createdAt": p.get("created_at"), "updatedAt": p.get("updated_at"), "publishedAt": p.get("created_at"), "lastEditedAt": Value::Null, "locked": false, "activeLockReason": Value::Null,
            "headRefName": p.pointer("/head/ref"), "headRefOid": p.pointer("/head/sha"), "baseRefName": p.pointer("/base/ref"), "baseRefOid": p.pointer("/base/sha"),
            "headRef": { "__typename": "Ref", "id": format!("kiln-ref-refs/heads/{}", p.pointer("/head/ref").and_then(Value::as_str).unwrap_or_default()), "name": p.pointer("/head/ref"), "prefix": "refs/heads/", "_oid": p.pointer("/head/sha") },
            "baseRef": { "__typename": "Ref", "id": format!("kiln-ref-refs/heads/{}", p.pointer("/base/ref").and_then(Value::as_str).unwrap_or_default()), "name": p.pointer("/base/ref"), "prefix": "refs/heads/", "_oid": p.pointer("/base/sha") },
            "isCrossRepository": false, "maintainerCanModify": false, "headRepository": cx_repo, "headRepositoryOwner": cx_repo.get("owner"), "baseRepository": cx_repo, "repository": cx_repo,
        }),
        json!({
            "author": actor(p.get("user").unwrap_or(&Value::Null)), "editor": Value::Null, "authorAssociation": "MEMBER",
            "additions": p.get("additions"), "deletions": p.get("deletions"), "changedFiles": p.get("changed_files"),
            "mergeable": if open { "MERGEABLE" } else { "UNKNOWN" }, "mergeStateStatus": if kiln_state == "accepted" { "CLEAN" } else { "BLOCKED" },
            "reviewDecision": if kiln_state == "accepted" { "APPROVED" } else { "REVIEW_REQUIRED" },
            "mergeCommit": p.get("merge_commit_sha").and_then(Value::as_str).map(|sha| json!({ "__typename": "Commit", "id": format!("kiln-commit-{sha}"), "oid": sha })),
            "milestone": Value::Null, "autoMergeRequest": Value::Null, "isInMergeQueue": false, "isMergeQueueEnabled": false,
            "viewerDidAuthor": p.pointer("/user/login").and_then(Value::as_str) == Some("github-actions[bot]"), "viewerCanUpdate": true, "viewerCanClose": open, "viewerCanReopen": !open,
            "_labels": labels, "_requests": requests,
        }),
    ])
}

/// The pull request numbered `n`, or why not (said at `path`).
async fn pr_by_number(cx: &Cx<'_>, n: &str, path: &[Value]) -> Value {
    match rest(pulls::get(&cx.site, n).await).await {
        (200, p) => pr_obj(&cx.repo, &p),
        (404, _) => {
            cx.error(path, Some("NOT_FOUND"), &format!("Could not resolve to a PullRequest with the number of {n}."));
            Value::Null
        }
        other => refused(cx, path, other),
    }
}

/// A pull request's number from its id (`kiln-pr-…`, or its issue's `kiln-issue-…`).
async fn pr_number(cx: &Cx<'_>, id: &str) -> Option<String> {
    let key = id.strip_prefix("kiln-pr-").or_else(|| id.strip_prefix("kiln-issue-"))?;
    let d = crate::ci::diffs::find(cx.site.hub, key).await.ok()??;
    d.repo.eq_ignore_ascii_case(&cx.site.repo.name).then(|| d.number.to_string())
}

/// The labels this repository's pull requests have had, by name.
async fn repo_labels(cx: &Cx<'_>) -> Vec<Value> {
    let repo = cx.site.repo.id.clone();
    let names: Vec<String> = cx
        .site
        .hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT DISTINCT l.name FROM diff_labels l JOIN diffs d ON d.id = l.diff_id WHERE d.repo_id = ?1 ORDER BY l.name")?;
            let rows = st.query_map([&repo], |r| r.get::<_, String>(0))?;
            Ok(rows.filter_map(Result::ok).collect())
        })
        .await
        .unwrap_or_default();
    names.iter().map(|n| label_obj(&pulls::label_json(&cx.site, n))).collect()
}

/// The check runs and statuses on `sha`, as GitHub rolls them up.
async fn rollup(cx: &Cx<'_>, sha: &str) -> Value {
    if sha.is_empty() {
        return Value::Null;
    }
    let (_, checks) = rest(records::checks(&cx.site, None, Some(sha)).await).await;
    let (_, statuses) = rest(records::statuses(&cx.site, sha, true).await).await;
    let up = |s: &str| s.to_ascii_uppercase();
    let mut contexts: Vec<Value> = checks
        .get("check_runs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|c| {
            json!({
                "__typename": "CheckRun", "id": text(c, "node_id"), "databaseId": c.get("id"), "name": text(c, "name"), "status": up(text(c, "status")),
                "conclusion": c.get("conclusion").and_then(Value::as_str).map(up), "startedAt": c.get("started_at"), "completedAt": c.get("completed_at"),
                "detailsUrl": c.get("details_url").filter(|u| !u.is_null()).or_else(|| c.get("html_url")), "isRequired": false, "checkSuite": { "__typename": "CheckSuite", "workflowRun": Value::Null },
            })
        })
        .collect();
    contexts.extend(statuses.get("statuses").and_then(Value::as_array).into_iter().flatten().map(|s| {
        json!({ "__typename": "StatusContext", "id": text(s, "node_id"), "context": text(s, "context"), "state": up(text(s, "state")), "targetUrl": s.get("target_url"), "description": s.get("description"), "createdAt": s.get("created_at"), "isRequired": false })
    }));
    if contexts.is_empty() {
        return Value::Null;
    }
    let states: Vec<String> = contexts
        .iter()
        .map(|c| if text(c, "__typename") == "CheckRun" { if text(c, "status") == "COMPLETED" { text(c, "conclusion").to_owned() } else { "PENDING".to_owned() } } else { text(c, "state").to_owned() })
        .collect();
    let state = if states.iter().any(|s| matches!(s.as_str(), "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE")) {
        "FAILURE"
    } else if states.iter().any(|s| matches!(s.as_str(), "PENDING" | "EXPECTED" | "")) {
        "PENDING"
    } else {
        "SUCCESS"
    };
    json!({ "__typename": "StatusCheckRollup", "id": format!("kiln-rollup-{sha}"), "state": state, "_contexts": contexts })
}

/// A ref by name (`main`, `refs/heads/main`, `refs/tags/v1`).
async fn ref_obj(cx: &Cx<'_>, name: &str) -> Value {
    let full = if name.starts_with("refs/") { name.to_owned() } else { format!("refs/heads/{name}") };
    let Some(oid) = cx.site.resolve(&full).await else { return Value::Null };
    let (prefix, short) = full.match_indices('/').nth(1).map_or(("refs/", full.trim_start_matches("refs/")), |(i, _)| (full.get(..=i).unwrap_or("refs/"), full.get(i + 1..).unwrap_or_default()));
    json!({ "__typename": "Ref", "id": format!("kiln-ref-{full}"), "name": short, "prefix": prefix, "_oid": oid })
}

/// The object `id` names, of this repository.
async fn node(cx: &Cx<'_>, id: &str, path: &[Value]) -> Value {
    if id.starts_with("kiln-pr-") || id.starts_with("kiln-issue-") {
        return match pr_number(cx, id).await {
            Some(n) => pr_by_number(cx, &n, path).await,
            None => Value::Null,
        };
    }
    if id == text(&cx.repo, "id") {
        return cx.repo.clone();
    }
    if let Some(n) = id.strip_prefix("kiln-comment-") {
        return match rest(pulls::comment(&cx.site, n, cx.claims, &Method::GET, None, false).await).await {
            (200, c) => comment_obj(&c),
            _ => Value::Null,
        };
    }
    if id.starts_with("kiln-label-") {
        return repo_labels(cx).await.into_iter().find(|l| text(l, "id") == id).unwrap_or(Value::Null);
    }
    if let Some(sha) = id.strip_prefix("kiln-commit-") {
        return repos::commit_json(&cx.site, sha, false).await.map_or(Value::Null, |c| commit_obj(&c));
    }
    Value::Null
}

/// Turns a value's objects into what `sel` asks of them, at `path`.
fn complete<'a>(cx: &'a Cx<'a>, v: Value, sel: Vec<Selection>, path: Vec<Value>) -> Fut<'a> {
    Box::pin(async move {
        match v {
            Value::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for (i, item) in items.into_iter().enumerate() {
                    let mut p = path.clone();
                    p.push(json!(i));
                    out.push(complete(cx, item, sel.clone(), p).await);
                }
                Value::Array(out)
            }
            Value::Object(_) => {
                let ty = text(&v, "__typename").to_owned();
                let mut out = Map::new();
                for (key, fields) in gql::collect(cx.doc, &sel, &ty, &is_a, &cx.vars) {
                    let Some(f) = fields.first().copied() else { continue };
                    let mut p = path.clone();
                    p.push(json!(key));
                    let value = if f.name == "__typename" {
                        json!(ty)
                    } else {
                        let args = gql::args(f, &cx.vars);
                        let sub = gql::sub(&fields);
                        let raw = resolve(cx, &ty, &v, f, &args, &sub, &p).await;
                        if sub.is_empty() || raw.is_null() { raw } else { complete(cx, raw, sub, p).await }
                    };
                    out.insert(key, value);
                }
                Value::Object(out)
            }
            other => other,
        }
    })
}

/// What `f` of `obj` (of type `ty`) is.
async fn resolve(cx: &Cx<'_>, ty: &str, obj: &Value, f: &Field, args: &Map<String, Value>, sub: &[Selection], path: &[Value]) -> Value {
    match (ty, f.name.as_str()) {
        ("Query", "viewer") => viewer(),
        ("Query", "rateLimit") => json!({ "__typename": "RateLimit", "limit": 5000, "cost": 1, "remaining": 5000, "used": 0, "nodeCount": 0, "resetAt": "2099-01-01T00:00:00Z" }),
        ("Query", "repository") => {
            let full = format!("{}/{}", arg(args, "owner"), arg(args, "name"));
            if full.eq_ignore_ascii_case(&cx.site.repo.name) {
                cx.repo.clone()
            } else {
                cx.error(path, Some("NOT_FOUND"), &format!("Could not resolve to a Repository with the name '{full}'."));
                Value::Null
            }
        }
        ("Query", "node") => node(cx, arg(args, "id"), path).await,
        ("Query", "nodes") => {
            let mut out = Vec::new();
            for id in list_arg(args, "ids") {
                out.push(node(cx, id, path).await);
            }
            Value::Array(out)
        }
        // W14-R11: the job's repository's pull requests, as `gh pr list --search` and `gh search
        // prs` ask for them (issue search's qualifiers: `search.rs`).
        ("Query", "search") => {
            let query = super::search::parse(arg(args, "query"));
            let issues = matches!(arg(args, "type"), "ISSUE" | "ISSUE_ADVANCED" | "");
            let hits = if issues && query.of(&cx.site.repo.name) {
                match super::pulls::all(&cx.site).await {
                    Ok(all) => query.select(all, None),
                    Err((_, why)) => {
                        cx.error(path, None, &why);
                        return Value::Null;
                    }
                }
            } else {
                Vec::new()
            };
            let total = hits.len();
            let mut found = connection(hits.iter().map(|p| pr_obj(&cx.repo, p)).collect(), args, "SearchResultItem");
            if let Some(o) = found.as_object_mut() {
                for (k, n) in [("issueCount", total), ("discussionCount", 0), ("repositoryCount", 0), ("userCount", 0), ("wikiCount", 0), ("codeCount", 0)] {
                    o.insert(k.into(), json!(n));
                }
            }
            found
        }
        ("Query", "__type") => introspect(arg(args, "name")),
        ("Query", "__schema") => {
            json!({ "__typename": "__Schema", "queryType": { "__typename": "__Type", "name": "Query", "kind": "OBJECT" }, "mutationType": { "__typename": "__Type", "name": "Mutation", "kind": "OBJECT" }, "subscriptionType": Value::Null, "types": [], "directives": [] })
        }
        ("Query", other) => {
            cx.missing(path, &format!("query field `{other}`"));
            Value::Null
        }
        ("Mutation", name) => mutate(cx, name, args.get("input").cloned().unwrap_or_default(), path).await,
        ("Repository", "pullRequest" | "issueOrPullRequest") => {
            let n = args.get("number").map(|n| n.to_string()).unwrap_or_default();
            pr_by_number(cx, &n, path).await
        }
        ("Repository", "issue") => {
            let n = args.get("number").map(|n| n.to_string()).unwrap_or_default();
            cx.error(path, Some("NOT_FOUND"), &format!("Could not resolve to an Issue with the number of {n}."));
            Value::Null
        }
        ("Repository", "pullRequests") => {
            let mut q: HashMap<String, String> = HashMap::from([("state".to_owned(), "all".to_owned()), ("per_page".to_owned(), "100".to_owned())]);
            if let Some(h) = args.get("headRefName").and_then(Value::as_str) {
                q.insert("head".into(), h.to_owned());
            }
            if let Some(b) = args.get("baseRefName").and_then(Value::as_str) {
                q.insert("base".into(), b.to_owned());
            }
            let (status, list) = rest(pulls::list(&cx.site, &q).await).await;
            if status != 200 {
                return refused(cx, path, (status, list));
            }
            let (states, labels) = (list_arg(args, "states"), list_arg(args, "labels"));
            let mut prs: Vec<Value> = list
                .as_array()
                .into_iter()
                .flatten()
                .map(|p| pr_obj(&cx.repo, p))
                .filter(|p| states.is_empty() || states.contains(&text(p, "state")))
                .filter(|p| labels.is_empty() || p.get("_labels").and_then(Value::as_array).is_some_and(|l| l.iter().any(|x| labels.contains(&text(x, "name")))))
                .collect();
            // GitHub's order is by creation, oldest first, unless `orderBy` says otherwise.
            let desc = args.get("orderBy").and_then(|o| o.get("direction")).and_then(Value::as_str) == Some("DESC");
            prs.sort_by_key(|p| p.get("number").and_then(Value::as_i64).unwrap_or(0));
            if desc {
                prs.reverse();
            }
            connection(prs, args, "PullRequest")
        }
        ("Repository", "labels") => {
            let query = arg(args, "query").to_ascii_lowercase();
            let all: Vec<Value> = repo_labels(cx).await.into_iter().filter(|l| query.is_empty() || text(l, "name").to_ascii_lowercase().contains(&query)).collect();
            connection(all, args, "Label")
        }
        ("Repository", "label") => {
            let name = arg(args, "name");
            repo_labels(cx).await.into_iter().find(|l| text(l, "name").eq_ignore_ascii_case(name)).unwrap_or(Value::Null)
        }
        ("Repository", "ref") => ref_obj(cx, arg(args, "qualifiedName")).await,
        ("Repository", "refs") => {
            let prefix = args.get("refPrefix").and_then(Value::as_str).unwrap_or("refs/heads/");
            let (_, list) = rest(Ok(repos::git_refs(&cx.site, prefix.trim_end_matches('/'), false).await)).await;
            let query = arg(args, "query");
            let refs: Vec<Value> = list
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|r| {
                    let full = text(r, "ref");
                    let short = full.strip_prefix(prefix)?;
                    (query.is_empty() || short.contains(query))
                        .then(|| json!({ "__typename": "Ref", "id": format!("kiln-ref-{full}"), "name": short, "prefix": prefix, "_oid": r.pointer("/object/sha") }))
                })
                .collect();
            connection(refs, args, "Ref")
        }
        ("Repository", "object") => {
            let expr = args.get("expression").or_else(|| args.get("oid")).and_then(Value::as_str).unwrap_or_default();
            if expr.contains(':') {
                return Value::Null;
            }
            match cx.site.resolve(expr).await {
                Some(sha) => repos::commit_json(&cx.site, &sha, false).await.map_or(Value::Null, |c| commit_obj(&c)),
                None => Value::Null,
            }
        }
        ("Ref", "target") => match obj.get("_oid").and_then(Value::as_str) {
            Some(sha) => repos::commit_json(&cx.site, sha, false).await.map_or(Value::Null, |c| commit_obj(&c)),
            None => Value::Null,
        },
        ("PullRequest", "labels") => connection(obj.get("_labels").and_then(Value::as_array).cloned().unwrap_or_default(), args, "Label"),
        ("PullRequest", "reviewRequests") => connection(obj.get("_requests").and_then(Value::as_array).cloned().unwrap_or_default(), args, "ReviewRequest"),
        ("PullRequest", "comments") => {
            let n = obj.get("number").map(|n| n.to_string()).unwrap_or_default();
            match rest(pulls::issue_comments(&cx.site, &n, cx.claims, None).await).await {
                (200, list) => connection(list.as_array().into_iter().flatten().map(comment_obj).collect(), args, "IssueComment"),
                other => refused(cx, path, other),
            }
        }
        ("PullRequest", "commits") => {
            let n = obj.get("number").map(|n| n.to_string()).unwrap_or_default();
            match rest(pulls::commits(&cx.site, &n).await).await {
                (200, list) => {
                    let nodes = list
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|c| json!({ "__typename": "PullRequestCommit", "id": format!("kiln-prc-{}", text(c, "sha")), "url": text(c, "html_url"), "commit": commit_obj(c) }))
                        .collect();
                    connection(nodes, args, "PullRequestCommit")
                }
                other => refused(cx, path, other),
            }
        }
        ("PullRequest", "files") => {
            let n = obj.get("number").map(|n| n.to_string()).unwrap_or_default();
            match rest(pulls::files(&cx.site, &n).await).await {
                (200, list) => {
                    let nodes = list
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|x| {
                            let kind = match text(x, "status") {
                                "added" => "ADDED",
                                "removed" => "DELETED",
                                "renamed" => "RENAMED",
                                "copied" => "COPIED",
                                _ => "MODIFIED",
                            };
                            json!({ "__typename": "PullRequestChangedFile", "path": text(x, "filename"), "additions": x.get("additions"), "deletions": x.get("deletions"), "changeType": kind, "viewerViewedState": "UNVIEWED" })
                        })
                        .collect();
                    connection(nodes, args, "PullRequestChangedFile")
                }
                other => refused(cx, path, other),
            }
        }
        ("PullRequest", "reviews" | "latestReviews" | "latestOpinionatedReviews") => {
            let n = obj.get("number").map(|n| n.to_string()).unwrap_or_default();
            match rest(pulls::reviews(&cx.site, &n, cx.claims, None).await).await {
                (200, list) => {
                    let all: Vec<Value> = list.as_array().into_iter().flatten().map(review_obj).collect();
                    let nodes = if f.name == "reviews" {
                        all
                    } else {
                        // Each author's last, or last that approved or asked for changes.
                        let opinionated = f.name == "latestOpinionatedReviews";
                        let mut last: Vec<Value> = Vec::new();
                        for r in all.into_iter().filter(|r| !opinionated || matches!(text(r, "state"), "APPROVED" | "CHANGES_REQUESTED")) {
                            let who = r.pointer("/author/login").cloned();
                            last.retain(|x| x.pointer("/author/login").cloned() != who);
                            last.push(r);
                        }
                        last
                    };
                    connection(nodes, args, "PullRequestReview")
                }
                other => refused(cx, path, other),
            }
        }
        ("PullRequest", "statusCheckRollup") => rollup(cx, obj.get("headRefOid").and_then(Value::as_str).unwrap_or_default()).await,
        ("Commit", "statusCheckRollup") => rollup(cx, text(obj, "oid")).await,
        ("StatusCheckRollup", "contexts") => connection(obj.get("_contexts").and_then(Value::as_array).cloned().unwrap_or_default(), args, "StatusCheckRollupContext"),
        ("__Type", "fields") => obj.get("_fields").cloned().unwrap_or(Value::Null),
        _ => match obj.get(&f.name) {
            Some(v) => v.clone(),
            None if wants_connection(sub) => connection(Vec::new(), args, "Unknown"),
            None => Value::Null,
        },
    }
}

/// The fields kiln answers of each type it knows, for `__type`.
fn introspect(name: &str) -> Value {
    let fields: &[&str] = match name {
        "PullRequest" => &[
            "id",
            "databaseId",
            "number",
            "title",
            "body",
            "bodyText",
            "url",
            "permalink",
            "resourcePath",
            "state",
            "isDraft",
            "closed",
            "merged",
            "mergedAt",
            "closedAt",
            "createdAt",
            "updatedAt",
            "publishedAt",
            "lastEditedAt",
            "locked",
            "activeLockReason",
            "headRefName",
            "headRefOid",
            "baseRefName",
            "baseRefOid",
            "headRef",
            "baseRef",
            "isCrossRepository",
            "maintainerCanModify",
            "headRepository",
            "headRepositoryOwner",
            "baseRepository",
            "repository",
            "author",
            "editor",
            "authorAssociation",
            "additions",
            "deletions",
            "changedFiles",
            "mergeable",
            "mergeStateStatus",
            "reviewDecision",
            "mergeCommit",
            "milestone",
            "autoMergeRequest",
            "viewerDidAuthor",
            "viewerCanUpdate",
            "viewerCanClose",
            "viewerCanReopen",
            "labels",
            "reviewRequests",
            "comments",
            "commits",
            "files",
            "reviews",
            "latestReviews",
            "latestOpinionatedReviews",
            "statusCheckRollup",
            "assignees",
            "participants",
            "timelineItems",
            "closingIssuesReferences",
        ],
        "IssueComment" => &[
            "id",
            "databaseId",
            "body",
            "bodyText",
            "author",
            "authorAssociation",
            "createdAt",
            "updatedAt",
            "url",
            "isMinimized",
            "minimizedReason",
            "includesCreatedEdit",
            "reactionGroups",
            "viewerDidAuthor",
            "viewerCanUpdate",
            "viewerCanDelete",
            "viewerCanMinimize",
        ],
        "Repository" => &[
            "id",
            "databaseId",
            "name",
            "nameWithOwner",
            "owner",
            "url",
            "resourcePath",
            "sshUrl",
            "homepageUrl",
            "description",
            "isPrivate",
            "visibility",
            "isFork",
            "isArchived",
            "isDisabled",
            "isEmpty",
            "isMirror",
            "isTemplate",
            "isLocked",
            "hasIssuesEnabled",
            "hasWikiEnabled",
            "hasProjectsEnabled",
            "hasDiscussionsEnabled",
            "mergeCommitAllowed",
            "squashMergeAllowed",
            "rebaseMergeAllowed",
            "deleteBranchOnMerge",
            "autoMergeAllowed",
            "viewerPermission",
            "viewerCanAdminister",
            "viewerCanCreateProjects",
            "viewerSubscription",
            "createdAt",
            "updatedAt",
            "pushedAt",
            "defaultBranchRef",
            "parent",
            "templateRepository",
            "stargazerCount",
            "forkCount",
            "pullRequest",
            "pullRequests",
            "issueOrPullRequest",
            "labels",
            "label",
            "ref",
            "refs",
            "object",
        ],
        "Commit" => &["id", "oid", "abbreviatedOid", "url", "message", "messageHeadline", "messageBody", "committedDate", "authoredDate", "author", "committer", "authors", "statusCheckRollup"],
        "StatusCheckRollup" => &["id", "state", "contexts"],
        "StatusCheckRollupContextConnection" | "LabelConnection" | "PullRequestConnection" | "IssueCommentConnection" => &["totalCount", "nodes", "edges", "pageInfo"],
        "CheckRun" => &["id", "databaseId", "name", "status", "conclusion", "startedAt", "completedAt", "detailsUrl", "isRequired", "checkSuite"],
        "StatusContext" => &["id", "context", "state", "targetUrl", "description", "createdAt", "isRequired"],
        "Label" => &["id", "name", "color", "description", "url", "isDefault"],
        "Ref" => &["id", "name", "prefix", "target"],
        _ => return Value::Null,
    };
    let list: Vec<Value> = fields.iter().map(|f| json!({ "__typename": "__Field", "name": f, "description": Value::Null, "isDeprecated": false, "args": [] })).collect();
    json!({ "__typename": "__Type", "name": name, "kind": "OBJECT", "description": Value::Null, "_fields": list })
}

/// A mutation, through the REST half's handlers; its payload.
async fn mutate(cx: &Cx<'_>, name: &str, input: Value, path: &[Value]) -> Value {
    let client = input.get("clientMutationId").cloned().unwrap_or(Value::Null);
    let payload = |ty: &str, more: Value| -> Value {
        let mut p = json!({ "__typename": format!("{ty}Payload"), "clientMutationId": client });
        if let (Some(o), Value::Object(m)) = (p.as_object_mut(), more) {
            o.extend(m);
        }
        p
    };
    let pr_of = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
    let number = |id: String| async move { pr_number(cx, &id).await };
    let not_pr = |what: &str| {
        cx.error(path, Some("NOT_FOUND"), &format!("Could not resolve to a node with the global id of '{what}'"));
        Value::Null
    };
    match name {
        "addComment" => {
            let id = pr_of("subjectId");
            let Some(n) = number(id.clone()).await else { return not_pr(&id) };
            match rest(pulls::issue_comments(&cx.site, &n, cx.claims, Some(&json!({ "body": input.get("body") }))).await).await {
                (200 | 201, c) => {
                    let node = comment_obj(&c);
                    let subject = pr_by_number(cx, &n, path).await;
                    payload(
                        "AddComment",
                        json!({ "commentEdge": { "__typename": "IssueCommentEdge", "node": node, "cursor": cursor(0) }, "subject": subject, "timelineEdge": { "__typename": "IssueTimelineItemEdge", "node": node, "cursor": cursor(0) } }),
                    )
                }
                other => refused(cx, path, other),
            }
        }
        "updateIssueComment" | "deleteIssueComment" => {
            let id = pr_of("id");
            let Some(n) = id.strip_prefix("kiln-comment-") else { return not_pr(&id) };
            let (method, body) = if name == "updateIssueComment" { (Method::PATCH, Some(json!({ "body": input.get("body") }))) } else { (Method::DELETE, None) };
            match rest(pulls::comment(&cx.site, n, cx.claims, &method, body.as_ref(), false).await).await {
                (200, c) => payload("UpdateIssueComment", json!({ "issueComment": comment_obj(&c) })),
                (204, _) => payload("DeleteIssueComment", json!({})),
                other => refused(cx, path, other),
            }
        }
        "createPullRequest" => {
            let body = json!({ "title": input.get("title"), "body": input.get("body"), "head": input.get("headRefName"), "base": input.get("baseRefName"), "draft": input.get("draft") });
            match rest(pulls::create(cx.site.hub, &cx.site, cx.claims, &body).await).await {
                (200 | 201, p) => payload("CreatePullRequest", json!({ "pullRequest": pr_obj(&cx.repo, &p) })),
                other => refused(cx, path, other),
            }
        }
        "updatePullRequest" | "closePullRequest" | "reopenPullRequest" => {
            let id = pr_of("pullRequestId");
            let Some(n) = number(id.clone()).await else { return not_pr(&id) };
            let body = match name {
                "closePullRequest" => json!({ "state": "closed" }),
                "reopenPullRequest" => json!({ "state": "open" }),
                _ => json!({ "title": input.get("title"), "body": input.get("body"), "state": input.get("state").and_then(Value::as_str).map(str::to_ascii_lowercase) }),
            };
            let ty = format!("{}{}", name.get(..1).unwrap_or_default().to_ascii_uppercase(), name.get(1..).unwrap_or_default());
            match rest(pulls::update(&cx.site, &n, cx.claims, &body).await).await {
                (200, p) => payload(&ty, json!({ "pullRequest": pr_obj(&cx.repo, &p) })),
                other => refused(cx, path, other),
            }
        }
        "addLabelsToLabelable" | "removeLabelsFromLabelable" => {
            let id = pr_of("labelableId");
            let Some(n) = number(id.clone()).await else { return not_pr(&id) };
            let known = repo_labels(cx).await;
            let ids: Vec<&str> = input.get("labelIds").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
            let mut names = Vec::new();
            for l in &ids {
                match known.iter().find(|k| text(k, "id") == *l) {
                    Some(k) => names.push(text(k, "name").to_owned()),
                    None => return not_pr(l),
                }
            }
            if name == "addLabelsToLabelable" {
                let r = rest(pulls::labels(&cx.site, &n, cx.claims, &Method::POST, Some(&json!({ "labels": names }))).await).await;
                if r.0 != 200 {
                    return refused(cx, path, r);
                }
            } else {
                for label in &names {
                    let r = rest(pulls::unlabel(&cx.site, &n, label, cx.claims).await).await;
                    if r.0 != 200 && r.0 != 404 {
                        return refused(cx, path, r);
                    }
                }
            }
            let ty = if name == "addLabelsToLabelable" { "AddLabelsToLabelable" } else { "RemoveLabelsFromLabelable" };
            payload(ty, json!({ "labelable": pr_by_number(cx, &n, path).await }))
        }
        "addPullRequestReview" => {
            let id = pr_of("pullRequestId");
            let Some(n) = number(id.clone()).await else { return not_pr(&id) };
            match rest(pulls::reviews(&cx.site, &n, cx.claims, Some(&json!({ "event": input.get("event"), "body": input.get("body") }))).await).await {
                (200 | 201, r) => payload("AddPullRequestReview", json!({ "pullRequestReview": review_obj(&r) })),
                other => refused(cx, path, other),
            }
        }
        other => {
            cx.missing(path, &format!("mutation `{other}`"));
            Value::Null
        }
    }
}

/// `/api/graphql`: `{query, variables, operationName}` (POST, or GET's query string).
pub async fn serve(hub: &Arc<Hub>, req: Request) -> Result<Response, Fail> {
    let Some(claims) = super::token::job_of_request(hub, req.headers()).await else { return Ok(super::message(StatusCode::UNAUTHORIZED, "Bad credentials")) };
    let origin = super::blob::origin(&req);
    let ask: Value = match *req.method() {
        Method::POST => srv::body_json(req.into_body(), 4 << 20).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?,
        Method::GET => {
            let q = srv::query(&req);
            json!({ "query": q.get("query"), "operationName": q.get("operationName"), "variables": q.get("variables").and_then(|v| serde_json::from_str::<Value>(v).ok()) })
        }
        _ => return Ok(super::message(StatusCode::METHOD_NOT_ALLOWED, "GraphQL is asked with POST")),
    };
    let fail = |message: String| Ok(srv::json(StatusCode::OK, &json!({ "errors": [{ "message": message, "locations": [] }] })));
    let doc = match gql::parse(ask.get("query").and_then(Value::as_str).unwrap_or_default()) {
        Ok(d) => d,
        Err(e) => return fail(format!("Parse error: {e}")),
    };
    let op = match doc.operation(ask.get("operationName").and_then(Value::as_str)) {
        Ok(o) => o.clone(),
        Err(e) => return fail(e),
    };
    let root = match op.kind {
        Kind::Query => "Query",
        Kind::Mutation => "Mutation",
        Kind::Subscription => return fail("subscriptions aren't served over HTTP".to_owned()),
    };
    let repos = hub.store.repos().await.map_err(super::internal)?;
    let Some(r) = repos.into_iter().find(|r| r.id == claims.repo) else { return Ok(super::message(StatusCode::NOT_FOUND, "Not Found")) };
    let site = Site::new(hub, r, &origin).await;
    let repo = repository(&site, &claims).await;
    let vars = op.variables(ask.get("variables").and_then(Value::as_object).unwrap_or(&Map::new()));
    let cx = Cx { site, claims: &claims, doc: &doc, vars, repo, errors: parking_lot::Mutex::new(Vec::new()), missing: parking_lot::Mutex::new(Vec::new()) };
    let data = complete(&cx, json!({ "__typename": root }), op.selection.clone(), Vec::new()).await;
    let missing: Vec<String> = std::mem::take(&mut *cx.missing.lock());
    for what in missing {
        tracing::info!(leg = %claims.leg, what = %what, "facade GraphQL not implemented");
        let _ = super::runs::annotation(
            hub,
            &claims.leg,
            None,
            "warning",
            Some("GitHub API".into()),
            &format!("This job asked GitHub's GraphQL API for something kiln doesn't answer yet ({what}); it got an error."),
            None,
            None,
            None,
            None,
            None,
        )
        .await;
    }
    let errors: Vec<Value> = std::mem::take(&mut *cx.errors.lock());
    let mut out = json!({ "data": data });
    if let (false, Some(o)) = (errors.is_empty(), out.as_object_mut()) {
        o.insert("errors".into(), Value::Array(errors));
    }
    Ok(srv::json(StatusCode::OK, &out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w14_r11_connections_page_as_github_pages_them() {
        let items: Vec<Value> = (0..5).map(|i| json!({ "__typename": "Label", "name": format!("l{i}") })).collect();
        let args = |v: Value| v.as_object().cloned().unwrap_or_default();
        let names = |c: &Value| -> Vec<String> { c.get("nodes").and_then(Value::as_array).into_iter().flatten().map(|n| text(n, "name").to_owned()).collect() };
        let first = connection(items.clone(), &args(json!({ "first": 2 })), "Label");
        assert_eq!(names(&first), ["l0", "l1"]);
        assert_eq!((first.get("totalCount"), first.pointer("/pageInfo/hasNextPage"), first.pointer("/pageInfo/hasPreviousPage")), (Some(&json!(5)), Some(&json!(true)), Some(&json!(false))));
        let after = first.pointer("/pageInfo/endCursor").and_then(Value::as_str).unwrap_or_default().to_owned();
        let next = connection(items.clone(), &args(json!({ "first": 2, "after": after })), "Label");
        assert_eq!(names(&next), ["l2", "l3"]);
        let last = connection(items.clone(), &args(json!({ "last": 2 })), "Label");
        assert_eq!((names(&last), last.pointer("/pageInfo/hasPreviousPage")), (vec!["l3".to_owned(), "l4".to_owned()], Some(&json!(true))));
        let all = connection(items, &Map::new(), "Label");
        assert_eq!(all.get("edges").and_then(Value::as_array).map(Vec::len), Some(5));
        assert_eq!(all.get("__typename"), Some(&json!("LabelConnection")));
        // One value where a list belongs is a list of it.
        let a = args(json!({ "states": "OPEN", "labels": ["a", "b"] }));
        assert_eq!((list_arg(&a, "states"), list_arg(&a, "labels"), list_arg(&a, "none")), (vec!["OPEN"], vec!["a", "b"], vec![]));
    }

    #[test]
    fn w14_r11_pull_requests_in_graphql_s_shape() {
        let repo = json!({ "__typename": "Repository", "nameWithOwner": "acme/app", "owner": { "login": "acme" } });
        let rest_pr = json!({
            "node_id": "kiln-pr-d1", "id": 3, "number": 3, "title": "t", "body": "b", "html_url": "https://k/diffs/D3", "state": "closed", "merged": true, "draft": false,
            "head": { "ref": "feature", "sha": "aaaa" }, "base": { "ref": "main", "sha": "bbbb" }, "user": { "login": "github-actions[bot]", "type": "Bot" },
            "labels": [{ "node_id": "kiln-label-1", "name": "bug", "color": "ededed" }], "requested_reviewers": [{ "login": "ada", "type": "User" }], "kiln": { "state": "landed" },
        });
        let p = pr_obj(&repo, &rest_pr);
        assert_eq!((text(&p, "state"), text(&p, "headRefName"), text(&p, "baseRefOid"), text(&p, "resourcePath")), ("MERGED", "feature", "bbbb", "/acme/app/pull/3"));
        assert_eq!(p.pointer("/author/login"), Some(&json!("github-actions")));
        assert_eq!(p.pointer("/author/__typename"), Some(&json!("Bot")));
        assert_eq!(p.pointer("/_labels/0/name"), Some(&json!("bug")));
        assert_eq!(p.pointer("/_requests/0/requestedReviewer/login"), Some(&json!("ada")));
        // Fragments on interfaces apply to the types in them.
        assert!(is_a("PullRequest", "Labelable") && is_a("Bot", "Actor") && is_a("CheckRun", "StatusCheckRollupContext") && !is_a("User", "Labelable"));
        assert_eq!(introspect("PullRequest").pointer("/_fields/0/name"), Some(&json!("id")));
        assert_eq!(introspect("Discussion"), Value::Null);
    }
}
