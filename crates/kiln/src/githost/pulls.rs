//! Pull requests on every host (docs/ci.md W23-R06), for kiln's review pages: GitHub's pull
//! requests, GitLab's merge requests and Bitbucket's pull requests in one shape, read with kiln's
//! credentials on the host, and acted on as the person when they signed in with that host (R08),
//! else with kiln's:
//! - a change: its title and description, who opened it, its branches and commits' ids, its page;
//! - the conversation, threads on lines, reviews, the files and their patches;
//! - commenting (on the change, on a line, in reply), reviewing (approve, request changes,
//!   comment), and merging by the host's methods.
//!
//! Every provider's checks of the head commit come from kiln's own tables
//! (`crate::ci::providers::checks`).

use std::collections::BTreeMap;

use anyhow::{anyhow, bail};
use serde_json::{Value, json};

use super::{bitbucket, github, gitlab};
use crate::http::client::Method;
use crate::hub::Hub;
use crate::store::Repo;

/// A comment's place on a line: the file, the line, and which side of the diff (`right`: the
/// change's version, `left`: the base's).
#[derive(Debug, Clone)]
pub struct Line {
    pub path: String,
    pub line: u64,
    pub left: bool,
}

/// What a review says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Approve,
    RequestChanges,
    Comment,
}

impl Verdict {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().replace('-', "_").as_str() {
            "approve" | "approved" => Some(Self::Approve),
            "request_changes" | "changes_requested" => Some(Self::RequestChanges),
            "comment" | "commented" => Some(Self::Comment),
            _ => None,
        }
    }
}

/// Which host a repository is on, for its changes.
enum On {
    Github(std::sync::Arc<github::App>, String),
    Gitlab(std::sync::Arc<gitlab::Gitlab>, String),
    Bitbucket(std::sync::Arc<bitbucket::Bitbucket>, String),
}

async fn on(hub: &Hub, repo: &Repo) -> anyhow::Result<On> {
    if let Some(app) = github::app(hub).await
        && let Some(n) = app.name_of(repo)
    {
        return Ok(On::Github(app, n));
    }
    if let Some((g, n)) = gitlab::for_repo(hub, repo).await {
        return Ok(On::Gitlab(g, n));
    }
    if let Some((b, n)) = bitbucket::for_repo(hub, repo).await {
        return Ok(On::Bitbucket(b, n));
    }
    bail!("{} isn't on a host kiln reaches (GitHub through its App, a GitLab, Bitbucket)", repo.name)
}

/// The SSO provider and web origin of a repository's host, for acting as the person there.
pub async fn identity_of(hub: &Hub, repo: &Repo) -> Option<(&'static str, String)> {
    Some(match on(hub, repo).await.ok()? {
        On::Github(a, _) => ("github", a.web.clone()),
        On::Gitlab(g, _) => ("gitlab", g.web.clone()),
        On::Bitbucket(b, _) => ("bitbucket", b.web.clone()),
    })
}

fn st(v: &Value, p: &str) -> String {
    v.pointer(p).and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn at(v: &Value, p: &str) -> Option<i64> {
    crate::ci::providers::unix(v.pointer(p))
}

/// Every page of a GitHub list (100 at a time), up to 1,000 items.
async fn gh_all(app: &github::App, token: &str, path: &str) -> anyhow::Result<Vec<Value>> {
    let mut out = Vec::new();
    let join = if path.contains('?') { '&' } else { '?' };
    for page in 1..=10 {
        let (code, v) = app.call(Method::Get, &format!("{path}{join}per_page=100&page={page}"), github::Auth::Token(token), None).await?;
        anyhow::ensure!(code == 200, "GitHub ({code}): {}", github::said(&v));
        let items = v.as_array().cloned().unwrap_or_default();
        let n = items.len();
        out.extend(items);
        if n < 100 {
            break;
        }
    }
    Ok(out)
}

async fn gh_token(app: &github::App, name: &str, write: bool) -> anyhow::Result<String> {
    let level = if write { "write" } else { "read" };
    let p: BTreeMap<String, String> = [("pull_requests", level), ("issues", level), ("contents", level), ("checks", "read"), ("metadata", "read")].iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
    app.token(name, &p).await
}

/// A GitHub pull request in kiln's shape.
fn gh_pull(p: &Value) -> Value {
    let state = if p.get("merged_at").is_some_and(|m| !m.is_null()) { "merged" } else { p.get("state").and_then(Value::as_str).unwrap_or("open") };
    json!({
        "host": "github", "number": p.get("number"), "title": st(p, "/title"), "body": st(p, "/body"), "author": st(p, "/user/login"), "state": state,
        "draft": p.get("draft").and_then(Value::as_bool).unwrap_or(false), "head": st(p, "/head/ref"), "sha": st(p, "/head/sha"), "base": st(p, "/base/ref"), "base_sha": st(p, "/base/sha"),
        "url": st(p, "/html_url"), "updated_at": at(p, "/updated_at"), "mergeable": p.get("mergeable"),
    })
}

/// A GitLab merge request in kiln's shape.
fn gl_pull(m: &Value) -> Value {
    let state = match m.get("state").and_then(Value::as_str).unwrap_or("opened") {
        "opened" | "locked" => "open",
        other => other,
    };
    json!({
        "host": "gitlab", "number": m.get("iid"), "title": st(m, "/title"), "body": st(m, "/description"), "author": st(m, "/author/username"), "state": state,
        "draft": m.get("draft").or_else(|| m.get("work_in_progress")).and_then(Value::as_bool).unwrap_or(false), "head": st(m, "/source_branch"), "sha": st(m, "/sha"), "base": st(m, "/target_branch"),
        "base_sha": st(m, "/diff_refs/base_sha"), "url": st(m, "/web_url"), "updated_at": at(m, "/updated_at"),
        "mergeable": m.get("detailed_merge_status").and_then(Value::as_str).map(|s| s == "mergeable"),
    })
}

/// A Bitbucket pull request in kiln's shape.
fn bb_pull(p: &Value) -> Value {
    let state = match p.get("state").and_then(Value::as_str).unwrap_or("OPEN") {
        "MERGED" => "merged",
        "DECLINED" | "SUPERSEDED" => "closed",
        _ => "open",
    };
    json!({
        "host": "bitbucket", "number": p.get("id"), "title": st(p, "/title"), "body": st(p, "/description"), "author": p.pointer("/author/nickname").or_else(|| p.pointer("/author/display_name")).and_then(Value::as_str).unwrap_or_default(),
        "state": state, "draft": p.get("draft").and_then(Value::as_bool).unwrap_or(false), "head": st(p, "/source/branch/name"), "sha": st(p, "/source/commit/hash"), "base": st(p, "/destination/branch/name"),
        "base_sha": st(p, "/destination/commit/hash"), "url": st(p, "/links/html/href"), "updated_at": at(p, "/updated_on"), "mergeable": Value::Null,
    })
}

/// The repository's open changes on its host.
pub async fn list(hub: &Hub, repo: &Repo) -> anyhow::Result<Vec<Value>> {
    Ok(match on(hub, repo).await? {
        On::Github(app, name) => {
            let t = gh_token(&app, &name, false).await?;
            gh_all(&app, &t, &format!("/repos/{name}/pulls?state=open")).await?.iter().map(gh_pull).collect()
        }
        On::Gitlab(g, name) => {
            let (code, v) = g.call(Method::Get, &format!("/projects/{}/merge_requests?state=opened&per_page=100", gitlab::id(&name)), None).await?;
            anyhow::ensure!(code == 200, "GitLab's merge requests ({code}): {}", gitlab::said(&v));
            v.as_array().into_iter().flatten().map(gl_pull).collect()
        }
        On::Bitbucket(b, name) => {
            let (code, v) = b.call(Method::Get, &format!("/repositories/{name}/pullrequests?state=OPEN&pagelen=50"), None).await?;
            anyhow::ensure!(code == 200, "Bitbucket's pull requests ({code}): {}", bitbucket::said(&v));
            v.get("values").and_then(Value::as_array).into_iter().flatten().map(bb_pull).collect()
        }
    })
}

/// Threads from comments with a place and a parent: `[{ id, path, line, side, resolved, comments }]`.
fn threads(comments: Vec<(String, Option<String>, Option<Line>, Option<bool>, Value)>) -> Vec<Value> {
    let mut roots: Vec<(String, Option<Line>, Option<bool>, Vec<Value>)> = Vec::new();
    for (id, parent, place, resolved, c) in comments {
        match parent.and_then(|p| roots.iter_mut().find(|r| r.0 == p || r.3.iter().any(|x| x.get("id").and_then(Value::as_str) == Some(p.as_str())))) {
            Some(root) => root.3.push(c),
            None => roots.push((id, place, resolved, vec![c])),
        }
    }
    roots
        .into_iter()
        .map(|(id, place, resolved, comments)| {
            json!({ "id": id, "path": place.as_ref().map(|p| p.path.clone()), "line": place.as_ref().map(|p| p.line), "side": place.as_ref().map(|p| if p.left { "left" } else { "right" }), "resolved": resolved, "comments": comments })
        })
        .collect()
}

fn comment_json(id: &str, author: &str, body: &str, when: Option<i64>) -> Value {
    json!({ "id": id, "author": author, "body": body, "at": when })
}

/// Files of a unified diff (Bitbucket's): `[{ path, old_path, status, additions, deletions, patch }]`.
fn files_of_diff(diff: &str) -> Vec<Value> {
    let mut out: Vec<(String, String, String, Vec<&str>)> = Vec::new();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let (a, b) = rest.split_once(" b/").unwrap_or((rest, rest));
            out.push((b.to_owned(), a.trim_start_matches("a/").to_owned(), "modified".into(), Vec::new()));
            continue;
        }
        let Some(f) = out.last_mut() else { continue };
        if line.starts_with("new file mode") {
            f.2 = "added".into();
        } else if line.starts_with("deleted file mode") {
            f.2 = "removed".into();
        } else if line.starts_with("rename from") {
            f.2 = "renamed".into();
        }
        if line.starts_with("@@") || !f.3.is_empty() {
            f.3.push(line);
        }
    }
    out.into_iter()
        .map(|(path, old, status, hunk)| {
            let adds = hunk.iter().filter(|l| l.starts_with('+') && !l.starts_with("+++")).count();
            let dels = hunk.iter().filter(|l| l.starts_with('-') && !l.starts_with("---")).count();
            json!({ "path": path, "old_path": (old != path).then_some(old), "status": status, "additions": adds, "deletions": dels, "patch": hunk.join("\n") })
        })
        .collect()
}

/// One change with its conversation, threads, reviews, files and the host's merge methods.
pub async fn get(hub: &Hub, repo: &Repo, number: u64) -> anyhow::Result<Value> {
    let mut out = match on(hub, repo).await? {
        On::Github(app, name) => {
            let t = gh_token(&app, &name, false).await?;
            let (code, p) = app.call(Method::Get, &format!("/repos/{name}/pulls/{number}"), github::Auth::Token(&t), None).await?;
            anyhow::ensure!(code == 200, "pull request #{number} ({code}): {}", github::said(&p));
            let mut out = gh_pull(&p);
            let files: Vec<Value> = gh_all(&app, &t, &format!("/repos/{name}/pulls/{number}/files"))
                .await?
                .iter()
                .map(|f| json!({ "path": st(f, "/filename"), "old_path": f.get("previous_filename"), "status": st(f, "/status"), "additions": f.get("additions"), "deletions": f.get("deletions"), "patch": f.get("patch") }))
                .collect();
            let conversation: Vec<Value> = gh_all(&app, &t, &format!("/repos/{name}/issues/{number}/comments"))
                .await?
                .iter()
                .map(|c| comment_json(&c.get("id").map(Value::to_string).unwrap_or_default(), &st(c, "/user/login"), &st(c, "/body"), at(c, "/created_at")))
                .collect();
            let inline = gh_all(&app, &t, &format!("/repos/{name}/pulls/{number}/comments")).await?;
            let threads = threads(
                inline
                    .iter()
                    .map(|c| {
                        let id = c.get("id").map(Value::to_string).unwrap_or_default();
                        let place = c.get("line").or_else(|| c.get("original_line")).and_then(Value::as_u64).map(|line| Line { path: st(c, "/path"), line, left: st(c, "/side") == "LEFT" });
                        (id.clone(), c.get("in_reply_to_id").filter(|r| !r.is_null()).map(Value::to_string), place, None, comment_json(&id, &st(c, "/user/login"), &st(c, "/body"), at(c, "/created_at")))
                    })
                    .collect(),
            );
            let reviews: Vec<Value> = gh_all(&app, &t, &format!("/repos/{name}/pulls/{number}/reviews"))
                .await?
                .iter()
                .filter(|r| st(r, "/state") != "PENDING")
                .map(|r| json!({ "author": st(r, "/user/login"), "state": st(r, "/state").to_ascii_lowercase(), "body": st(r, "/body"), "at": at(r, "/submitted_at") }))
                .collect();
            let (_, info) = app.call(Method::Get, &format!("/repos/{name}"), github::Auth::Token(&t), None).await?;
            let methods: Vec<&str> = [("allow_merge_commit", "merge"), ("allow_squash_merge", "squash"), ("allow_rebase_merge", "rebase")].iter().filter(|(k, _)| info.get(*k).and_then(Value::as_bool).unwrap_or(true)).map(|(_, m)| *m).collect();
            if let Some(o) = out.as_object_mut() {
                o.insert("files".into(), json!(files));
                o.insert("conversation".into(), json!(conversation));
                o.insert("threads".into(), json!(threads));
                o.insert("reviews".into(), json!(reviews));
                o.insert("merge_methods".into(), json!(methods));
            }
            out
        }
        On::Gitlab(g, name) => {
            let pid = gitlab::id(&name);
            let (code, m) = g.call(Method::Get, &format!("/projects/{pid}/merge_requests/{number}"), None).await?;
            anyhow::ensure!(code == 200, "merge request !{number} ({code}): {}", gitlab::said(&m));
            let mut out = gl_pull(&m);
            let (_, diffs) = g.call(Method::Get, &format!("/projects/{pid}/merge_requests/{number}/diffs?per_page=100"), None).await?;
            let files: Vec<Value> = diffs
                .as_array()
                .into_iter()
                .flatten()
                .map(|d| {
                    let status = if d.get("new_file").and_then(Value::as_bool) == Some(true) {
                        "added"
                    } else if d.get("deleted_file").and_then(Value::as_bool) == Some(true) {
                        "removed"
                    } else if d.get("renamed_file").and_then(Value::as_bool) == Some(true) {
                        "renamed"
                    } else {
                        "modified"
                    };
                    let patch = st(d, "/diff");
                    let adds = patch.lines().filter(|l| l.starts_with('+')).count();
                    let dels = patch.lines().filter(|l| l.starts_with('-')).count();
                    json!({ "path": st(d, "/new_path"), "old_path": (st(d, "/old_path") != st(d, "/new_path")).then(|| st(d, "/old_path")), "status": status, "additions": adds, "deletions": dels, "patch": patch })
                })
                .collect();
            let (_, discussions) = g.call(Method::Get, &format!("/projects/{pid}/merge_requests/{number}/discussions?per_page=100"), None).await?;
            let (mut conversation, mut inline) = (Vec::new(), Vec::new());
            for d in discussions.as_array().into_iter().flatten() {
                let did = st(d, "/id");
                for (i, n) in d.get("notes").and_then(Value::as_array).into_iter().flatten().enumerate() {
                    if n.get("system").and_then(Value::as_bool) == Some(true) {
                        continue;
                    }
                    let id = n.get("id").map(Value::to_string).unwrap_or_default();
                    let c = comment_json(&id, &st(n, "/author/username"), &st(n, "/body"), at(n, "/created_at"));
                    let place = n.pointer("/position/new_line").and_then(Value::as_u64).map(|line| Line { path: st(n, "/position/new_path"), line, left: false }).or_else(|| {
                        n.pointer("/position/old_line").and_then(Value::as_u64).map(|line| Line { path: st(n, "/position/old_path"), line, left: true })
                    });
                    if place.is_none() && d.get("individual_note").and_then(Value::as_bool) == Some(true) {
                        conversation.push(c);
                    } else {
                        // A discussion is a thread: its id, and its first note's place.
                        let parent = (i > 0).then(|| did.clone());
                        inline.push((did.clone(), parent, place, n.get("resolved").and_then(Value::as_bool), c));
                    }
                }
            }
            let (_, approvals) = g.call(Method::Get, &format!("/projects/{pid}/merge_requests/{number}/approvals"), None).await?;
            let reviews: Vec<Value> = approvals.get("approved_by").and_then(Value::as_array).into_iter().flatten().map(|a| json!({ "author": st(a, "/user/username"), "state": "approved", "body": "", "at": Value::Null })).collect();
            let (_, project) = g.call(Method::Get, &format!("/projects/{pid}"), None).await?;
            let methods: Vec<&str> = match project.get("merge_method").and_then(Value::as_str).unwrap_or("merge") {
                "ff" => vec!["fast-forward", "rebase"],
                "rebase_merge" => vec!["merge", "rebase"],
                _ => vec!["merge"],
            };
            let mut methods = methods;
            if project.get("squash_option").and_then(Value::as_str) != Some("never") {
                methods.push("squash");
            }
            if let Some(o) = out.as_object_mut() {
                o.insert("files".into(), json!(files));
                o.insert("conversation".into(), json!(conversation));
                o.insert("threads".into(), json!(threads(inline)));
                o.insert("reviews".into(), json!(reviews));
                o.insert("merge_methods".into(), json!(methods));
            }
            out
        }
        On::Bitbucket(b, name) => {
            let (code, p) = b.call(Method::Get, &format!("/repositories/{name}/pullrequests/{number}"), None).await?;
            anyhow::ensure!(code == 200, "pull request #{number} ({code}): {}", bitbucket::said(&p));
            let mut out = bb_pull(&p);
            let (_, diff) = b.bytes(&format!("/repositories/{name}/pullrequests/{number}/diff"), 16 << 20).await?;
            let files = files_of_diff(&String::from_utf8_lossy(&diff));
            let (_, comments) = b.call(Method::Get, &format!("/repositories/{name}/pullrequests/{number}/comments?pagelen=100"), None).await?;
            let (mut conversation, mut inline) = (Vec::new(), Vec::new());
            for c in comments.get("values").and_then(Value::as_array).into_iter().flatten() {
                if c.get("deleted").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let id = c.get("id").map(Value::to_string).unwrap_or_default();
                let author = c.pointer("/user/nickname").or_else(|| c.pointer("/user/display_name")).and_then(Value::as_str).unwrap_or_default().to_owned();
                let j = comment_json(&id, &author, &st(c, "/content/raw"), at(c, "/created_on"));
                let parent = c.pointer("/parent/id").map(Value::to_string);
                let place = c.get("inline").filter(|i| !i.is_null()).and_then(|i| {
                    let path = st(i, "/path");
                    i.get("to").and_then(Value::as_u64).map(|line| Line { path: path.clone(), line, left: false }).or_else(|| i.get("from").and_then(Value::as_u64).map(|line| Line { path, line, left: true }))
                });
                if place.is_none() && parent.is_none() {
                    conversation.push(j);
                } else {
                    inline.push((id, parent, place, c.get("resolution").map(|r| !r.is_null()), j));
                }
            }
            let reviews: Vec<Value> = p
                .get("participants")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|x| {
                    let state = match x.get("state").and_then(Value::as_str) {
                        Some("approved") => "approved",
                        Some("changes_requested") => "changes_requested",
                        _ if x.get("approved").and_then(Value::as_bool) == Some(true) => "approved",
                        _ => return None,
                    };
                    Some(json!({ "author": x.pointer("/user/nickname").or_else(|| x.pointer("/user/display_name")).and_then(Value::as_str).unwrap_or_default(), "state": state, "body": "", "at": at(x, "/participated_on") }))
                })
                .collect();
            if let Some(o) = out.as_object_mut() {
                o.insert("files".into(), json!(files));
                o.insert("conversation".into(), json!(conversation));
                o.insert("threads".into(), json!(threads(inline)));
                o.insert("reviews".into(), json!(reviews));
                o.insert("merge_methods".into(), json!(["merge", "squash", "fast-forward"]));
            }
            out
        }
    };
    if let Some(o) = out.as_object_mut() {
        let sha = o.get("sha").and_then(Value::as_str).unwrap_or_default().to_owned();
        let checks = crate::ci::providers::checks(hub, &repo.id, &sha).await.unwrap_or_else(|(_, e)| json!({ "error": e }));
        o.insert("checks".into(), checks);
    }
    Ok(out)
}

/// Who acts: the person's own token on the host, or kiln's credentials there.
pub struct As<'a>(pub Option<&'a str>);

impl As<'_> {
    fn who(&self) -> &'static str {
        if self.0.is_some() { "person" } else { "kiln" }
    }
}

/// A comment on the change, on a line of it (`line`), or in reply to a thread (`reply_to`).
pub async fn comment(hub: &Hub, repo: &Repo, number: u64, body: &str, line: Option<&Line>, reply_to: Option<&str>, by: &As<'_>) -> anyhow::Result<Value> {
    let made = match on(hub, repo).await? {
        On::Github(app, name) => {
            let t = match by.0 {
                Some(t) => t.to_owned(),
                None => gh_token(&app, &name, true).await?,
            };
            let (path, ask) = match (reply_to, line) {
                (Some(r), _) => (format!("/repos/{name}/pulls/{number}/comments/{r}/replies"), json!({ "body": body })),
                (None, Some(l)) => {
                    let (_, p) = app.call(Method::Get, &format!("/repos/{name}/pulls/{number}"), github::Auth::Token(&t), None).await?;
                    (format!("/repos/{name}/pulls/{number}/comments"), json!({ "body": body, "commit_id": st(&p, "/head/sha"), "path": l.path, "line": l.line, "side": if l.left { "LEFT" } else { "RIGHT" } }))
                }
                (None, None) => (format!("/repos/{name}/issues/{number}/comments"), json!({ "body": body })),
            };
            let (code, v) = app.call(Method::Post, &path, github::Auth::Token(&t), Some(&ask)).await?;
            anyhow::ensure!(code == 201, "GitHub took no comment ({code}): {}", github::said(&v));
            v.get("id").map(Value::to_string)
        }
        On::Gitlab(g, name) => {
            let pid = gitlab::id(&name);
            let (path, ask) = match (reply_to, line) {
                (Some(r), _) => (format!("/projects/{pid}/merge_requests/{number}/discussions/{r}/notes"), json!({ "body": body })),
                (None, Some(l)) => {
                    let (_, m) = g.call(Method::Get, &format!("/projects/{pid}/merge_requests/{number}"), None).await?;
                    let mut position = json!({ "position_type": "text", "base_sha": st(&m, "/diff_refs/base_sha"), "start_sha": st(&m, "/diff_refs/start_sha"), "head_sha": st(&m, "/diff_refs/head_sha"), "new_path": l.path, "old_path": l.path });
                    if let Some(o) = position.as_object_mut() {
                        o.insert(if l.left { "old_line".into() } else { "new_line".into() }, json!(l.line));
                    }
                    (format!("/projects/{pid}/merge_requests/{number}/discussions"), json!({ "body": body, "position": position }))
                }
                (None, None) => (format!("/projects/{pid}/merge_requests/{number}/notes"), json!({ "body": body })),
            };
            let (code, v) = match by.0 {
                Some(t) => g.call_as(t, Method::Post, &path, Some(&ask)).await?,
                None => g.call(Method::Post, &path, Some(&ask)).await?,
            };
            anyhow::ensure!(code == 201, "GitLab took no comment ({code}): {}", gitlab::said(&v));
            v.get("id").map(|i| i.as_str().map_or_else(|| i.to_string(), str::to_owned))
        }
        On::Bitbucket(b, name) => {
            let mut ask = json!({ "content": { "raw": body } });
            if let Some(o) = ask.as_object_mut() {
                if let Some(r) = reply_to {
                    o.insert("parent".into(), json!({ "id": r.parse::<u64>().map_err(|_| anyhow!("`reply_to`: a comment's id"))? }));
                }
                if let Some(l) = line {
                    o.insert("inline".into(), if l.left { json!({ "path": l.path, "from": l.line }) } else { json!({ "path": l.path, "to": l.line }) });
                }
            }
            let path = format!("/repositories/{name}/pullrequests/{number}/comments");
            let (code, v) = match by.0 {
                Some(t) => b.call_as(t, Method::Post, &path, Some(&ask)).await?,
                None => b.call(Method::Post, &path, Some(&ask)).await?,
            };
            anyhow::ensure!(code == 201, "Bitbucket took no comment ({code}): {}", bitbucket::said(&v));
            v.get("id").map(Value::to_string)
        }
    };
    Ok(json!({ "comment": made, "as": by.who() }))
}

/// A review: approve, request changes, or comment, with a body.
pub async fn review(hub: &Hub, repo: &Repo, number: u64, verdict: Verdict, body: &str, by: &As<'_>) -> anyhow::Result<Value> {
    match on(hub, repo).await? {
        On::Github(app, name) => {
            let t = match by.0 {
                Some(t) => t.to_owned(),
                None => gh_token(&app, &name, true).await?,
            };
            let event = match verdict {
                Verdict::Approve => "APPROVE",
                Verdict::RequestChanges => "REQUEST_CHANGES",
                Verdict::Comment => "COMMENT",
            };
            let (code, v) = app.call(Method::Post, &format!("/repos/{name}/pulls/{number}/reviews"), github::Auth::Token(&t), Some(&json!({ "event": event, "body": body }))).await?;
            anyhow::ensure!(code == 200, "GitHub took no review ({code}): {}", github::said(&v));
        }
        On::Gitlab(g, name) => {
            let pid = gitlab::id(&name);
            let call = |path: String, ask: Option<Value>| {
                let g = g.clone();
                async move {
                    match by.0 {
                        Some(t) => g.call_as(t, Method::Post, &path, ask.as_ref()).await,
                        None => g.call(Method::Post, &path, ask.as_ref()).await,
                    }
                }
            };
            match verdict {
                Verdict::Approve => {
                    let (code, v) = call(format!("/projects/{pid}/merge_requests/{number}/approve"), None).await?;
                    anyhow::ensure!(code == 201 || code == 200, "GitLab took no approval ({code}): {}", gitlab::said(&v));
                }
                // GitLab's API has no "request changes": the approval taken back, and the note.
                Verdict::RequestChanges => {
                    let _ = call(format!("/projects/{pid}/merge_requests/{number}/unapprove"), None).await;
                }
                Verdict::Comment => {}
            }
            if !body.is_empty() || verdict == Verdict::RequestChanges {
                let text = if verdict == Verdict::RequestChanges && body.is_empty() { "Changes requested." } else { body };
                let (code, v) = call(format!("/projects/{pid}/merge_requests/{number}/notes"), Some(json!({ "body": text }))).await?;
                anyhow::ensure!(code == 201, "GitLab took no note ({code}): {}", gitlab::said(&v));
            }
        }
        On::Bitbucket(b, name) => {
            let path = match verdict {
                Verdict::Approve => Some(format!("/repositories/{name}/pullrequests/{number}/approve")),
                Verdict::RequestChanges => Some(format!("/repositories/{name}/pullrequests/{number}/request-changes")),
                Verdict::Comment => None,
            };
            if let Some(path) = path {
                let (code, v) = match by.0 {
                    Some(t) => b.call_as(t, Method::Post, &path, None).await?,
                    None => b.call(Method::Post, &path, None).await?,
                };
                anyhow::ensure!(code == 200 || code == 201, "Bitbucket took no review ({code}): {}", bitbucket::said(&v));
            }
            if !body.is_empty() {
                comment(hub, repo, number, body, None, None, by).await?;
            }
        }
    }
    Ok(json!({ "review": format!("{verdict:?}").to_ascii_lowercase(), "as": by.who() }))
}

/// The host's name and the merge methods it has (whether a repository allows each is the host's to
/// say when asked to merge).
pub async fn methods(hub: &Hub, repo: &Repo) -> (&'static str, &'static [&'static str]) {
    match on(hub, repo).await {
        Ok(On::Github(..)) => ("GitHub", &["merge", "squash", "rebase"]),
        Ok(On::Gitlab(..)) => ("GitLab", &["merge", "squash", "fast-forward", "rebase"]),
        Ok(On::Bitbucket(..)) => ("Bitbucket", &["merge", "squash", "fast-forward"]),
        Err(_) => ("the host", &[]),
    }
}

/// Merges the change by `method` (`merge`, `squash`, `rebase`, `fast-forward`), as the host allows.
pub async fn merge(hub: &Hub, repo: &Repo, number: u64, method: &str, by: &As<'_>) -> anyhow::Result<Value> {
    let merged = match on(hub, repo).await? {
        On::Github(app, name) => {
            let m = match method {
                "merge" | "squash" | "rebase" => method,
                other => bail!("GitHub merges by `merge`, `squash` or `rebase`, not `{other}`"),
            };
            let t = match by.0 {
                Some(t) => t.to_owned(),
                None => gh_token(&app, &name, true).await?,
            };
            let (code, v) = app.call(Method::Put, &format!("/repos/{name}/pulls/{number}/merge"), github::Auth::Token(&t), Some(&json!({ "merge_method": m }))).await?;
            anyhow::ensure!(code == 200, "GitHub didn't merge it ({code}): {}", github::said(&v));
            st(&v, "/sha")
        }
        On::Gitlab(g, name) => {
            let pid = gitlab::id(&name);
            let (path, ask) = match method {
                // Rebases the source branch on the target; merging it is a step of its own after.
                "rebase" => (format!("/projects/{pid}/merge_requests/{number}/rebase"), None),
                "squash" => (format!("/projects/{pid}/merge_requests/{number}/merge"), Some(json!({ "squash": true }))),
                "merge" | "fast-forward" => (format!("/projects/{pid}/merge_requests/{number}/merge"), Some(json!({ "squash": false }))),
                other => bail!("GitLab merges by `merge`, `squash`, `fast-forward` or `rebase`, not `{other}`"),
            };
            let (code, v) = match by.0 {
                Some(t) => g.call_as(t, Method::Put, &path, ask.as_ref()).await?,
                None => g.call(Method::Put, &path, ask.as_ref()).await?,
            };
            anyhow::ensure!(code == 200 || code == 202, "GitLab didn't {method} it ({code}): {}", gitlab::said(&v));
            let sha = v.get("merge_commit_sha").and_then(Value::as_str).or_else(|| v.get("squash_commit_sha").and_then(Value::as_str)).unwrap_or_default();
            if sha.is_empty() && v.get("state").and_then(Value::as_str) == Some("merged") { st(&v, "/sha") } else { sha.to_owned() }
        }
        On::Bitbucket(b, name) => {
            let strategy = match method {
                "merge" => "merge_commit",
                "squash" => "squash",
                "fast-forward" => "fast_forward",
                other => bail!("Bitbucket merges by `merge`, `squash` or `fast-forward`, not `{other}`"),
            };
            let path = format!("/repositories/{name}/pullrequests/{number}/merge");
            let ask = json!({ "type": "pullrequest_merge_parameters", "merge_strategy": strategy });
            let (code, v) = match by.0 {
                Some(t) => b.call_as(t, Method::Post, &path, Some(&ask)).await?,
                None => b.call(Method::Post, &path, Some(&ask)).await?,
            };
            anyhow::ensure!(code == 200 || code == 202, "Bitbucket didn't merge it ({code}): {}", bitbucket::said(&v));
            st(&v, "/merge_commit/hash")
        }
    };
    // GitLab's rebase moves the source branch onto the target; merging it is a call of its own.
    if method == "rebase" && merged.is_empty() {
        return Ok(json!({ "merged": false, "rebased": true, "method": method, "as": by.who() }));
    }
    Ok(json!({ "merged": !merged.is_empty(), "commit": (!merged.is_empty()).then_some(merged), "method": method, "as": by.who() }))
}

/// `/api/ci/repos/:repo/pulls…`: `GET` the open ones, `GET :n` one with all it has, `POST
/// :n/comments { body, path?, line?, side?, reply_to? }`, `POST :n/reviews { verdict, body }`,
/// `POST :n/merge { method }`; what's done is the person's on the host when they signed in with it.
pub async fn route(hub: &Hub, method: &http::Method, repo_key: &str, tail: &[&str], body: Value, caller: Option<&crate::authz::Caller>) -> Result<crate::http::server::Response, crate::ci::Fail> {
    use crate::http::server as srv;
    use http::StatusCode;
    let repo = hub.store.repos().await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?.into_iter().find(|r| r.name == repo_key || r.id == repo_key).ok_or((StatusCode::NOT_FOUND, "no such repository".to_owned()))?;
    let upstream = |e: anyhow::Error| (StatusCode::BAD_GATEWAY, format!("{e:#}"));
    let number = |n: &str| n.parse::<u64>().map_err(|_| (StatusCode::NOT_FOUND, format!("`{n}` isn't a pull request's number")));
    // The person's own token on the host, when they signed in with it (W23-R08).
    let token = match (caller.and_then(|c| c.user.as_deref()), identity_of(hub, &repo).await) {
        (Some(user), Some((provider, web))) if *method == http::Method::POST => crate::authz::sso::person_token(hub, provider, user, &web).await,
        _ => None,
    };
    let by = As(token.as_deref());
    let text = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let by_name = caller.map_or_else(|| "admin".to_owned(), |c| c.name.clone());
    match (method, tail) {
        (&http::Method::GET, []) => Ok(srv::json(StatusCode::OK, &json!({ "repo": repo.name, "pulls": list(hub, &repo).await.map_err(upstream)? }))),
        (&http::Method::GET, [n]) => Ok(srv::json(StatusCode::OK, &get(hub, &repo, number(n)?).await.map_err(upstream)?)),
        (&http::Method::POST, [n, "comments"]) => {
            let said = text("body");
            if said.trim().is_empty() {
                return Err((StatusCode::BAD_REQUEST, "`body`: what the comment says".into()));
            }
            let line = body.get("line").and_then(Value::as_u64).map(|l| Line { path: text("path"), line: l, left: text("side").eq_ignore_ascii_case("left") });
            if line.as_ref().is_some_and(|l| l.path.is_empty()) {
                return Err((StatusCode::BAD_REQUEST, "a comment on a line names its `path`".into()));
            }
            let reply = body.get("reply_to").map(|r| r.as_str().map_or_else(|| r.to_string(), str::to_owned));
            let v = comment(hub, &repo, number(n)?, &said, line.as_ref(), reply.as_deref(), &by).await.map_err(upstream)?;
            hub.store.audit(by_name, "host.pulls.comment".into(), format!("{} #{n}", repo.name)).await;
            Ok(srv::json(StatusCode::CREATED, &v))
        }
        (&http::Method::POST, [n, "reviews"]) => {
            let asked = body.get("verdict").or_else(|| body.get("event")).and_then(Value::as_str).unwrap_or("comment");
            let verdict = Verdict::parse(asked).ok_or_else(|| (StatusCode::BAD_REQUEST, format!("`{asked}`: a review is `approve`, `request_changes` or `comment`")))?;
            let v = review(hub, &repo, number(n)?, verdict, &text("body"), &by).await.map_err(upstream)?;
            hub.store.audit(by_name, "host.pulls.review".into(), format!("{} #{n}: {asked}", repo.name)).await;
            Ok(srv::json(StatusCode::OK, &v))
        }
        (&http::Method::POST, [n, "merge"]) => {
            let how = body.get("method").and_then(Value::as_str).unwrap_or("merge");
            let (host, ways) = methods(hub, &repo).await;
            if !ways.contains(&how) {
                return Err((StatusCode::UNPROCESSABLE_ENTITY, format!("{host} merges by {}, not `{how}`", ways.iter().map(|w| format!("`{w}`")).collect::<Vec<_>>().join(", "))));
            }
            let v = merge(hub, &repo, number(n)?, how, &by).await.map_err(upstream)?;
            hub.store.audit(by_name, "host.pulls.merge".into(), format!("{} #{n} by {how}", repo.name)).await;
            Ok(srv::json(StatusCode::OK, &v))
        }
        _ => Err((StatusCode::NOT_FOUND, "no such route".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w23_r06_threads_and_diffs() {
        let c = |id: &str, parent: Option<&str>, line: Option<u64>| (id.to_owned(), parent.map(str::to_owned), line.map(|l| Line { path: "a.rs".into(), line: l, left: false }), None, json!({ "id": id }));
        let t = threads(vec![c("1", None, Some(3)), c("2", Some("1"), None), c("3", Some("2"), None), c("4", None, Some(9))]);
        assert_eq!(t.len(), 2);
        assert_eq!(t.first().and_then(|x| x.get("comments")).and_then(Value::as_array).map(Vec::len), Some(3), "replies to replies stay in the thread");
        assert_eq!(t.get(1).and_then(|x| x.get("line")), Some(&json!(9)));
        let diff = "diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n ctx\ndiff --git a/n.txt b/n.txt\nnew file mode 100644\n--- /dev/null\n+++ b/n.txt\n@@ -0,0 +1 @@\n+hi\n";
        let f = files_of_diff(diff);
        assert_eq!(f.iter().map(|x| (x.get("path").and_then(Value::as_str).unwrap_or_default(), x.get("status").and_then(Value::as_str).unwrap_or_default())).collect::<Vec<_>>(), [("src/a.rs", "modified"), ("n.txt", "added")]);
        assert_eq!(f.first().map(|x| (x.get("additions").cloned(), x.get("deletions").cloned())), Some((Some(json!(1)), Some(json!(1)))));
        assert_eq!(Verdict::parse("request-changes"), Some(Verdict::RequestChanges));
    }
}
