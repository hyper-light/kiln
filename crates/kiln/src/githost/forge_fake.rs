//! A fake of GitHub's, GitLab's and Bitbucket's REST APIs over the forge seam's simulators
//! (docs/seams.md, "Forge"), for githost's forge: each request counted against the simulator's
//! rate limit as the host counts it (a read answered 304 free), the host's rate-limit headers on
//! every answer, then answered from the simulator in the host's own shape: GitHub's pulls,
//! stacks, asynchronous merges, pending reviews and GraphQL merge queue; GitLab's merge requests,
//! rebases, merge trains and draft notes; Bitbucket's pull requests and comments. Who asks is the
//! token: `Bearer person:<login>` is that person, any other kiln.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt as _;
use futures::future::BoxFuture;
use kiln_seams::forge::*;
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::forge::{Cred, HostForge, MemHeld, Watch};
use crate::http::server::{self as srv, Request, Response};

const REPO: &str = SimForgeFixture::REPO;

/// The fake: one host, its simulator.
pub struct FakeForge {
    pub sim: SimForge,
    st: Mutex<FakeSt>,
}

#[derive(Default)]
struct FakeSt {
    next: u64,
    /// Asynchronous merges that queued a change (GitHub answers them "enqueued").
    queued: BTreeMap<String, u64>,
    /// GitHub's pending reviews: id → (change, author, body); their line comments are the
    /// simulator's drafts.
    pending: BTreeMap<u64, (u64, String, String)>,
}

struct Ask {
    method: http::Method,
    path: String,
    query: BTreeMap<String, String>,
    who: String,
    etag: Option<String>,
    body: Value,
}

type Answer = (u16, Value, Option<String>);

/// A simulator's answer, which is always at once.
fn now<T>(f: BoxFuture<'_, Result<T, Refusal>>) -> Result<T, Refusal> {
    f.now_or_never().unwrap_or_else(|| Err(Refusal::Unavailable("the simulator didn't answer at once".into())))
}

fn num(s: &str) -> Result<u64, Refusal> {
    s.parse().map_err(|_| Refusal::NotFound { what: s.to_owned() })
}

fn st(v: &Value, p: &str) -> String {
    v.pointer(p).and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn line_at(path: Option<&str>, line: Option<u64>) -> Option<Line> {
    Some(Line { path: path?.to_owned(), line: u32::try_from(line?).ok()? })
}

fn changed<T>(f: Fresh<T>) -> Result<(T, String), Refusal> {
    f.changed().ok_or_else(|| Refusal::Unavailable("an unconditional read answered NotModified".into()))
}

fn gh_pr(c: &Change) -> Value {
    let (state, merged_at, commit) = match &c.state {
        State::Open => ("open", Value::Null, Value::Null),
        State::Merged { commit } => ("closed", json!("2027-01-15T00:00:00Z"), json!(commit)),
        State::Closed => ("closed", Value::Null, Value::Null),
    };
    json!({
        "number": c.number, "node_id": format!("PR_{}", c.number), "title": c.title, "user": { "login": c.author },
        "head": { "ref": c.head_ref, "sha": c.head_sha }, "base": { "ref": c.base_ref },
        "state": state, "merged_at": merged_at, "merge_commit_sha": commit,
        "stack": c.stack.map(|p| json!({ "id": p.stack, "number": p.stack, "position": p.position, "size": p.size })),
    })
}

fn gh_stack(s: &Stack) -> Value {
    json!({ "id": s.number, "number": s.number, "base": { "ref": s.base }, "open": true,
            "pull_requests": s.changes.iter().map(|n| json!({ "number": n, "state": "open", "merged_at": null, "draft": false })).collect::<Vec<_>>() })
}

fn gl_mr(c: &Change) -> Value {
    let (state, commit) = match &c.state {
        State::Open => ("opened", Value::Null),
        State::Merged { commit } => ("merged", json!(commit)),
        State::Closed => ("closed", Value::Null),
    };
    json!({ "iid": c.number, "title": c.title, "author": { "username": c.author }, "source_branch": c.head_ref, "target_branch": c.base_ref,
            "sha": c.head_sha, "state": state, "merge_commit_sha": commit, "rebase_in_progress": c.updating,
            "diff_refs": { "base_sha": "", "start_sha": "", "head_sha": c.head_sha } })
}

fn bb_pr(c: &Change) -> Value {
    let (state, commit) = match &c.state {
        State::Open => ("OPEN", Value::Null),
        State::Merged { commit } => ("MERGED", json!({ "hash": commit })),
        State::Closed => ("DECLINED", Value::Null),
    };
    json!({ "id": c.number, "title": c.title, "author": { "nickname": c.author },
            "source": { "branch": { "name": c.head_ref }, "commit": { "hash": c.head_sha } },
            "destination": { "branch": { "name": c.base_ref } }, "state": state, "merge_commit": commit })
}

fn gl_position(l: &Option<Line>) -> Value {
    l.as_ref().map_or(Value::Null, |l| json!({ "position_type": "text", "new_path": l.path, "new_line": l.line }))
}

fn verdict(event: &str) -> Verdict {
    match event {
        "APPROVE" => Verdict::Approve,
        "REQUEST_CHANGES" => Verdict::RequestChanges,
        _ => Verdict::Comment,
    }
}

impl FakeForge {
    pub fn new(sim: SimForge) -> Self {
        Self { sim, st: Mutex::new(FakeSt::default()) }
    }

    fn host(&self) -> Host {
        self.sim.host()
    }

    fn next(&self) -> u64 {
        let mut st = self.st.lock();
        st.next = st.next.saturating_add(1);
        st.next.saturating_add(1_000_000)
    }

    /// What a refusal is in the host's words and statuses.
    fn refused(&self, e: &Refusal) -> Answer {
        let msg = e.to_string();
        let status = match (self.host(), e) {
            (_, Refusal::NotFound { .. }) => 404,
            (Host::Github, Refusal::RateLimited { .. }) => 403,
            (_, Refusal::RateLimited { .. }) => 429,
            (Host::Github, Refusal::Closed { .. }) => 400,
            (Host::Github | Host::Gitlab, Refusal::Conflict { .. }) => 409,
            (Host::Github, _) => 422,
            (Host::Gitlab, Refusal::Moved { .. }) => 409,
            (Host::Gitlab, Refusal::Closed { .. }) => 405,
            (Host::Gitlab, Refusal::NotLinear { .. }) => 406,
            (Host::Gitlab, Refusal::Unsupported { .. }) => 403,
            (Host::Gitlab | Host::Bitbucket, _) => 400,
        };
        let body = match self.host() {
            Host::Bitbucket => json!({ "type": "error", "error": { "message": msg } }),
            _ => json!({ "message": msg }),
        };
        (status, body, None)
    }

    fn respond(&self, (status, body, etag): Answer) -> Response {
        let code = http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR);
        let mut r = if body.is_null() { srv::status(code) } else { srv::json(code, &body) };
        if let Some(t) = etag {
            r = srv::with_header(r, http::header::ETAG, &t);
        }
        if let Ok(rate) = now(self.sim.rate(REPO)) {
            let prefix = if self.host() == Host::Gitlab { "ratelimit-" } else { "x-ratelimit-" };
            for (k, v) in [("limit", u64::from(rate.limit)), ("remaining", u64::from(rate.remaining)), ("reset", rate.resets_at)] {
                if let Ok(name) = http::header::HeaderName::try_from(format!("{prefix}{k}")) {
                    r = srv::with_header(r, name, &v.to_string());
                }
            }
        }
        r
    }

    /// Whether a conditional read finds what its ETag names unchanged (answered free).
    fn unchanged(&self, a: &Ask, tag: &str) -> bool {
        if a.method != http::Method::GET {
            return false;
        }
        let seg: Vec<&str> = a.path.split('/').collect();
        let (list, one) = match (self.host(), seg.as_slice()) {
            (Host::Github, ["", "repos", _, _, "pulls"]) | (Host::Gitlab, ["", "projects", _, "merge_requests"]) | (Host::Bitbucket, ["", "repositories", _, _, "pullrequests"]) => (true, None),
            (Host::Github, ["", "repos", _, _, "pulls", n]) | (Host::Gitlab, ["", "projects", _, "merge_requests", n]) | (Host::Bitbucket, ["", "repositories", _, _, "pullrequests", n]) => (false, n.parse::<u64>().ok()),
            _ => return false,
        };
        self.sim.uncounted(|s| match (list, one) {
            (true, _) => matches!(now(s.changes(REPO, Some(tag))), Ok(Fresh::NotModified)),
            (false, Some(n)) => matches!(now(s.change(REPO, n, Some(tag))), Ok(Fresh::NotModified)),
            _ => false,
        })
    }

    fn answer(&self, a: &Ask) -> Answer {
        if self.host() == Host::Github && a.path == "/rate_limit" {
            return match now(self.sim.rate(REPO)) {
                Ok(r) => (200, json!({ "resources": { "core": { "limit": r.limit, "remaining": r.remaining, "reset": r.resets_at } } }), None),
                Err(e) => self.refused(&e),
            };
        }
        if let Some(tag) = &a.etag
            && self.unchanged(a, tag)
        {
            return (304, Value::Null, Some(tag.clone()));
        }
        if let Err(e) = self.sim.charge() {
            return self.refused(&e);
        }
        match self.sim.uncounted(|s| self.route(s, a)) {
            Ok(out) => out,
            Err(e) => self.refused(&e),
        }
    }

    fn route(&self, s: &SimForge, a: &Ask) -> Result<Answer, Refusal> {
        let seg: Vec<&str> = a.path.split('/').collect();
        match (self.host(), seg.as_slice()) {
            (Host::Github, ["", "graphql"]) => self.graphql(s, a),
            (Host::Github, ["", "repos", o, n, rest @ ..]) => self.github(s, a, &format!("{o}/{n}"), rest),
            (Host::Gitlab, ["", "projects", p, rest @ ..]) => self.gitlab(s, a, &srv::decode(p), rest),
            (Host::Bitbucket, ["", "repositories", w, r, rest @ ..]) => self.bitbucket(s, a, &format!("{w}/{r}"), rest),
            _ => Err(Refusal::NotFound { what: format!("{} {}", a.method, a.path) }),
        }
    }

    fn head(s: &SimForge, repo: &str, n: u64) -> Result<String, Refusal> {
        Ok(changed(now(s.change(repo, n, None))?)?.0.head_sha)
    }

    fn github(&self, s: &SimForge, a: &Ask, repo: &str, rest: &[&str]) -> Result<Answer, Refusal> {
        use http::Method as M;
        let b = &a.body;
        Ok(match (&a.method, rest) {
            (&M::GET, []) => (200, json!({ "full_name": repo, "default_branch": TRUNK, "allow_merge_commit": true, "allow_squash_merge": true, "allow_rebase_merge": true }), None),
            (&M::GET, ["rules", "branches", _]) => {
                let queue = now(s.abilities(repo))?.queue.is_some();
                (200, if queue { json!([{ "type": "merge_queue" }]) } else { json!([]) }, None)
            }
            (&M::POST, ["pulls"]) => {
                let c = now(s.open(repo, Open { head: st(b, "/head"), base: st(b, "/base"), title: st(b, "/title"), by: a.who.clone() }))?;
                (201, gh_pr(&c), None)
            }
            (&M::GET, ["pulls"]) => {
                let (list, tag) = changed(now(s.changes(repo, None))?)?;
                (200, json!(list.iter().map(gh_pr).collect::<Vec<_>>()), Some(tag))
            }
            (&M::GET, ["pulls", n]) => {
                let (c, tag) = changed(now(s.change(repo, num(n)?, None))?)?;
                (200, gh_pr(&c), Some(tag))
            }
            (&M::PUT, ["pulls", n, "update-branch"]) => {
                let n = num(n)?;
                let expected = b.get("expected_head_sha").and_then(Value::as_str).map_or_else(|| Self::head(s, repo, n), |e| Ok(e.to_owned()))?;
                now(s.update(repo, n, Update { expected, replayed: None }))?;
                (202, json!({ "message": "Updating pull request branch." }), None)
            }
            (&M::PUT, ["pulls", n, "merge-async"]) => {
                let n = num(n)?;
                let sha = b.get("sha").and_then(Value::as_str).map_or_else(|| Self::head(s, repo, n), |e| Ok(e.to_owned()))?;
                if st(b, "/merge_action") == "merge_queue" {
                    now(s.enqueue(repo, n, &sha))?;
                    let id = format!("queue-{}", self.next());
                    self.st.lock().queued.insert(id.clone(), n);
                    (202, json!({ "uuid": id, "status": "pending" }), None)
                } else {
                    let method = match st(b, "/merge_method").as_str() {
                        "squash" => Method::Squash,
                        "rebase" => Method::Rebase,
                        _ => Method::Merge,
                    };
                    match now(s.merge(repo, n, Merge { method, expected: sha }))? {
                        Merging::Started { id } => (202, json!({ "uuid": id, "status": "pending" }), None),
                        Merging::Merged { commit, .. } => (200, json!({ "status": "merged", "merge_commit_oid": commit }), None),
                    }
                }
            }
            (&M::GET, ["pulls", n, "merge-async", id]) => {
                if self.st.lock().queued.contains_key(*id) {
                    return Ok((200, json!({ "uuid": id, "status": "enqueued" }), None));
                }
                match now(s.merge_result(repo, num(n)?, id))? {
                    MergeResult::Pending => (200, json!({ "uuid": id, "status": "pending" }), None),
                    MergeResult::Merged { commit, .. } => (200, json!({ "uuid": id, "status": "merged", "merge_commit_oid": commit }), None),
                    MergeResult::Failed { why } => (200, json!({ "uuid": id, "status": "failed", "message": why }), None),
                }
            }
            (&M::GET, ["pulls", n, "reviews"]) => {
                let n = num(n)?;
                let mut out: Vec<Value> = Vec::new();
                let mut by_review: BTreeMap<u64, (String, Vec<String>)> = BTreeMap::new();
                for c in now(s.comments(repo, n))? {
                    let e = by_review.entry(c.review.unwrap_or_default()).or_insert_with(|| (c.author.clone(), Vec::new()));
                    if c.line.is_none() {
                        e.1.push(c.body);
                    }
                }
                for (id, (author, bodies)) in by_review {
                    out.push(json!({ "id": id, "node_id": format!("PRR_{id}"), "user": { "login": author }, "state": "COMMENTED", "body": bodies.join("\n\n") }));
                }
                for (id, (pn, author, body)) in &self.st.lock().pending {
                    if *pn == n && *author == a.who {
                        out.push(json!({ "id": id, "node_id": format!("PRR_{id}"), "user": { "login": author }, "state": "PENDING", "body": body }));
                    }
                }
                (200, json!(out), None)
            }
            (&M::POST, ["pulls", n, "reviews"]) => {
                let n = num(n)?;
                for c in b.get("comments").and_then(Value::as_array).into_iter().flatten() {
                    let line = line_at(c.get("path").and_then(Value::as_str), c.get("line").and_then(Value::as_u64));
                    now(s.draft(repo, n, &a.who, Draft { body: st(c, "/body"), line }))?;
                }
                let body = st(b, "/body");
                match b.get("event").and_then(Value::as_str) {
                    Some(event) => {
                        if !body.trim().is_empty() {
                            now(s.draft(repo, n, &a.who, Draft { body, line: None }))?;
                        }
                        let r = now(s.publish(repo, n, &a.who, verdict(event)))?;
                        (200, json!({ "id": r.id, "state": event }), None)
                    }
                    None => {
                        let mut stt = self.st.lock();
                        if stt.pending.values().any(|(pn, au, _)| *pn == n && *au == a.who) {
                            return Err(Refusal::Invalid { what: "a review", why: "you have a pending review already".into() });
                        }
                        stt.next = stt.next.saturating_add(1);
                        let id = stt.next.saturating_add(1_000_000);
                        stt.pending.insert(id, (n, a.who.clone(), body.clone()));
                        (200, json!({ "id": id, "node_id": format!("PRR_{id}"), "state": "PENDING", "user": { "login": a.who }, "body": body }), None)
                    }
                }
            }
            (&M::PUT, ["pulls", _, "reviews", id]) => {
                let id = num(id)?;
                let mut stt = self.st.lock();
                let p = stt.pending.get_mut(&id).ok_or_else(|| Refusal::NotFound { what: format!("review {id}") })?;
                p.2 = st(b, "/body");
                (200, json!({ "id": id, "state": "PENDING", "body": p.2 }), None)
            }
            (&M::GET, ["pulls", n, "reviews", id, "comments"]) => {
                let (n, id) = (num(n)?, num(id)?);
                let pending = self.st.lock().pending.get(&id).cloned();
                let comments = match pending {
                    Some((_, author, _)) => now(s.drafts(repo, n, &author))?,
                    None => now(s.comments(repo, n))?.into_iter().filter(|c| c.review == Some(id)).collect(),
                };
                let out: Vec<Value> = comments.iter().filter_map(|c| c.line.as_ref().map(|l| json!({ "id": c.id, "path": l.path, "line": l.line, "body": c.body, "user": { "login": c.author } }))).collect();
                (200, json!(out), None)
            }
            (&M::POST, ["pulls", n, "reviews", id, "events"]) => {
                let (n, id) = (num(n)?, num(id)?);
                let (_, author, body) = self.st.lock().pending.remove(&id).ok_or_else(|| Refusal::NotFound { what: format!("review {id}") })?;
                if !body.trim().is_empty() {
                    now(s.draft(repo, n, &author, Draft { body, line: None }))?;
                }
                let r = now(s.publish(repo, n, &author, verdict(&st(b, "/event"))))?;
                (200, json!({ "id": r.id, "state": st(b, "/event") }), None)
            }
            (&M::GET, ["pulls", n, "comments"]) => {
                let out: Vec<Value> = now(s.comments(repo, num(n)?))?
                    .iter()
                    .filter_map(|c| c.line.as_ref().map(|l| json!({ "id": c.id, "pull_request_review_id": c.review, "path": l.path, "line": l.line, "body": c.body, "user": { "login": c.author } })))
                    .collect();
                (200, json!(out), None)
            }
            (&M::POST, ["stacks"]) => {
                let prs: Vec<u64> = b.get("pull_requests").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_u64).collect();
                (201, gh_stack(&now(s.stack(repo, prs))?), None)
            }
            (&M::GET, ["stacks"]) => {
                let n = num(a.query.get("pull_request").map(String::as_str).unwrap_or_default())?;
                (200, json!(now(s.stack_of(repo, n))?.iter().map(gh_stack).collect::<Vec<_>>()), None)
            }
            (&M::POST, ["stacks", sn, "add"]) => {
                let prs: Vec<u64> = b.get("pull_requests").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_u64).collect();
                (200, gh_stack(&now(s.stack_add(repo, num(sn)?, prs))?), None)
            }
            (&M::POST, ["stacks", sn, "unstack"]) => match now(s.unstack(repo, num(sn)?))? {
                Some(left) => (200, gh_stack(&left), None),
                None => (204, Value::Null, None),
            },
            _ => return Err(Refusal::NotFound { what: format!("{} {}", a.method, a.path) }),
        })
    }

    fn graphql(&self, s: &SimForge, a: &Ask) -> Result<Answer, Refusal> {
        let (q, vars) = (st(&a.body, "/query"), a.body.get("variables").cloned().unwrap_or(Value::Null));
        if q.contains("dequeuePullRequest") {
            let n = num(st(&vars, "/id").trim_start_matches("PR_"))?;
            now(s.dequeue(REPO, n))?;
            return Ok((200, json!({ "data": { "dequeuePullRequest": { "mergeQueueEntry": null } } }), None));
        }
        if q.contains("addPullRequestReviewThread") {
            let id = num(st(&vars, "/review").trim_start_matches("PRR_"))?;
            let (n, author, _) = self.st.lock().pending.get(&id).cloned().ok_or_else(|| Refusal::NotFound { what: format!("review {id}") })?;
            let line = line_at(vars.get("path").and_then(Value::as_str), vars.get("line").and_then(Value::as_u64));
            let c = now(s.draft(REPO, n, &author, Draft { body: st(&vars, "/body"), line }))?;
            return Ok((200, json!({ "data": { "addPullRequestReviewThread": { "thread": { "comments": { "nodes": [{ "databaseId": c }] } } } } }), None));
        }
        if q.contains("mergeQueue(") {
            let repo = format!("{}/{}", st(&vars, "/owner"), st(&vars, "/name"));
            let mq = match now(s.queued(&repo)) {
                Ok(list) => json!({ "entries": { "nodes": list.iter().enumerate().map(|(i, n)| json!({ "position": i.saturating_add(1), "pullRequest": { "number": n } })).collect::<Vec<_>>() } }),
                Err(Refusal::Unsupported { .. }) => Value::Null,
                Err(e) => return Err(e),
            };
            return Ok((200, json!({ "data": { "repository": { "mergeQueue": mq } } }), None));
        }
        Ok((200, json!({ "errors": [{ "message": "the fake doesn't know this query" }] }), None))
    }

    fn gitlab(&self, s: &SimForge, a: &Ask, repo: &str, rest: &[&str]) -> Result<Answer, Refusal> {
        use http::Method as M;
        let b = &a.body;
        Ok(match (&a.method, rest) {
            (&M::GET, []) => {
                let queue = now(s.abilities(repo))?.queue.is_some();
                (200, json!({ "path_with_namespace": repo, "default_branch": TRUNK, "merge_trains_enabled": queue, "merge_method": "merge" }), None)
            }
            (&M::POST, ["merge_requests"]) => {
                let c = now(s.open(repo, Open { head: st(b, "/source_branch"), base: st(b, "/target_branch"), title: st(b, "/title"), by: a.who.clone() }))?;
                (201, gl_mr(&c), None)
            }
            (&M::GET, ["merge_requests"]) => {
                let (list, tag) = changed(now(s.changes(repo, None))?)?;
                (200, json!(list.iter().map(gl_mr).collect::<Vec<_>>()), Some(tag))
            }
            (&M::GET, ["merge_requests", n]) => {
                let (c, tag) = changed(now(s.change(repo, num(n)?, None))?)?;
                (200, gl_mr(&c), Some(tag))
            }
            (&M::PUT, ["merge_requests", n, "rebase"]) => {
                let n = num(n)?;
                let expected = Self::head(s, repo, n)?;
                now(s.update(repo, n, Update { expected, replayed: None }))?;
                (202, json!({ "rebase_in_progress": true }), None)
            }
            (&M::PUT, ["merge_requests", n, "merge"]) => {
                let n = num(n)?;
                let expected = b.get("sha").and_then(Value::as_str).map_or_else(|| Self::head(s, repo, n), |e| Ok(e.to_owned()))?;
                let method = if b.get("squash").and_then(Value::as_bool) == Some(true) { Method::Squash } else { Method::Merge };
                now(s.merge(repo, n, Merge { method, expected }))?;
                (200, gl_mr(&changed(now(s.change(repo, n, None))?)?.0), None)
            }
            (&M::POST, ["merge_requests", n, "cancel_auto_merge"]) => {
                let n = num(n)?;
                now(s.dequeue(repo, n))?;
                (201, gl_mr(&changed(now(s.change(repo, n, None))?)?.0), None)
            }
            (&M::POST, ["merge_requests", n, "draft_notes"]) => {
                let line = line_at(b.pointer("/position/new_path").and_then(Value::as_str), b.pointer("/position/new_line").and_then(Value::as_u64));
                let id = now(s.draft(repo, num(n)?, &a.who, Draft { body: st(b, "/note"), line: line.clone() }))?;
                (201, json!({ "id": id, "note": st(b, "/note"), "position": gl_position(&line) }), None)
            }
            (&M::GET, ["merge_requests", n, "draft_notes"]) => {
                let out: Vec<Value> = now(s.drafts(repo, num(n)?, &a.who))?.iter().map(|d| json!({ "id": d.id, "note": d.body, "position": gl_position(&d.line) })).collect();
                (200, json!(out), None)
            }
            (&M::POST, ["merge_requests", n, "draft_notes", "bulk_publish"]) => {
                now(s.publish(repo, num(n)?, &a.who, Verdict::Comment))?;
                (204, Value::Null, None)
            }
            (&M::POST, ["merge_requests", n, "approve"]) => {
                num(n)?;
                (201, json!({}), None)
            }
            (&M::GET, ["merge_requests", n, "notes"]) => {
                let out: Vec<Value> =
                    now(s.comments(repo, num(n)?))?.iter().map(|c| json!({ "id": c.id, "body": c.body, "author": { "username": c.author }, "system": false, "position": gl_position(&c.line) })).collect();
                (200, json!(out), None)
            }
            (&M::POST, ["merge_trains", "merge_requests", n]) => {
                let n = num(n)?;
                let sha = b.get("sha").and_then(Value::as_str).map_or_else(|| Self::head(s, repo, n), |e| Ok(e.to_owned()))?;
                let list = now(s.enqueue(repo, n, &sha))?;
                (201, json!(list.iter().map(|n| json!({ "merge_request": { "iid": n } })).collect::<Vec<_>>()), None)
            }
            (&M::GET, ["merge_trains", _]) => {
                let list = now(s.queued(repo))?;
                (200, json!(list.iter().map(|n| json!({ "merge_request": { "iid": n } })).collect::<Vec<_>>()), None)
            }
            _ => return Err(Refusal::NotFound { what: format!("{} {}", a.method, a.path) }),
        })
    }

    fn bitbucket(&self, s: &SimForge, a: &Ask, repo: &str, rest: &[&str]) -> Result<Answer, Refusal> {
        use http::Method as M;
        let b = &a.body;
        Ok(match (&a.method, rest) {
            (&M::GET, []) => {
                now(s.abilities(repo))?;
                (200, json!({ "full_name": repo, "mainbranch": { "name": TRUNK } }), None)
            }
            (&M::POST, ["pullrequests"]) => {
                let c = now(s.open(repo, Open { head: st(b, "/source/branch/name"), base: st(b, "/destination/branch/name"), title: st(b, "/title"), by: a.who.clone() }))?;
                (201, bb_pr(&c), None)
            }
            (&M::GET, ["pullrequests"]) => {
                let (list, tag) = changed(now(s.changes(repo, None))?)?;
                (200, json!({ "values": list.iter().map(bb_pr).collect::<Vec<_>>(), "next": null }), Some(tag))
            }
            (&M::GET, ["pullrequests", n]) => {
                let (c, tag) = changed(now(s.change(repo, num(n)?, None))?)?;
                (200, bb_pr(&c), Some(tag))
            }
            (&M::POST, ["pullrequests", n, "merge"]) => {
                let n = num(n)?;
                let method = match st(b, "/merge_strategy").as_str() {
                    "squash" => Method::Squash,
                    "fast_forward" => Method::FastForward,
                    _ => Method::Merge,
                };
                let expected = Self::head(s, repo, n)?;
                now(s.merge(repo, n, Merge { method, expected }))?;
                (200, bb_pr(&changed(now(s.change(repo, n, None))?)?.0), None)
            }
            (&M::POST, ["pullrequests", n, "comments"]) => {
                let n = num(n)?;
                let line = line_at(b.pointer("/inline/path").and_then(Value::as_str), b.pointer("/inline/to").and_then(Value::as_u64));
                now(s.draft(repo, n, &a.who, Draft { body: st(b, "/content/raw"), line }))?;
                let r = now(s.publish(repo, n, &a.who, Verdict::Comment))?;
                let c = r.comments.last().cloned().ok_or_else(|| Refusal::Unavailable("no comment published".into()))?;
                (201, json!({ "id": c.id, "content": { "raw": c.body }, "user": { "nickname": c.author } }), None)
            }
            (&M::GET, ["pullrequests", n, "comments"]) => {
                let out: Vec<Value> = now(s.comments(repo, num(n)?))?
                    .iter()
                    .map(|c| json!({ "id": c.id, "content": { "raw": c.body }, "user": { "nickname": c.author }, "deleted": false, "inline": c.line.as_ref().map(|l| json!({ "path": l.path, "to": l.line })) }))
                    .collect();
                (200, json!({ "values": out, "next": null }), None)
            }
            (&M::POST, ["pullrequests", n, "approve" | "request-changes"]) => {
                num(n)?;
                (200, json!({}), None)
            }
            _ => return Err(Refusal::NotFound { what: format!("{} {}", a.method, a.path) }),
        })
    }
}

impl srv::Handler for FakeForge {
    async fn handle(self: Arc<Self>, req: Request) -> Response {
        let (method, path) = (req.method().clone(), req.uri().path().to_owned());
        let query = req.uri().query().unwrap_or_default().split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), srv::decode(v))).collect();
        let auth = srv::header_str(&req, &http::header::AUTHORIZATION).unwrap_or_default().to_owned();
        let etag = srv::header_str(&req, &http::header::IF_NONE_MATCH).map(str::to_owned);
        let raw = srv::body_bytes(req.into_body(), 16 << 20).await.unwrap_or_default();
        let body = serde_json::from_slice(&raw).unwrap_or(Value::Null);
        let token = auth.strip_prefix("Bearer ").unwrap_or(&auth);
        let who = token.strip_prefix("person:").unwrap_or(token).to_owned();
        let ask = Ask { method, path, query, who, etag, body };
        let out = self.answer(&ask);
        self.respond(out)
    }
}

/// Serves a fake of `sim`'s host on a free port: its base URL.
pub async fn start(sim: SimForge) -> anyhow::Result<String> {
    let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 64).map_err(|e| anyhow::anyhow!("{e}"))?;
    let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
    let fake = Arc::new(FakeForge::new(sim));
    crate::rt::spawn(async move {
        while let Ok(s) = tcp.accept().await {
            let (h, r) = (Arc::clone(&fake), s.peer_addr().ok());
            crate::rt::spawn(srv::serve_connection(crate::http::io::RtIo::new(s), h, r));
        }
    });
    Ok(format!("http://{addr}"))
}

/// githost's forge on the fake at `base`: kiln's token `kiln`, a person's `person:<login>`, its
/// leases pushed on the simulator, its clock the simulator's, its held drafts in memory.
pub fn forge_on(base: &str, sim: &SimForge) -> HostForge {
    let (p, c) = (sim.clone(), sim.clone());
    HostForge::new(
        sim.host(),
        base,
        Cred::Header("Bearer kiln".into()),
        Box::new(|by| Some(format!("Bearer person:{by}"))),
        Box::new(move |repo, branch, sha, expected| {
            let r = p.push_lease(&repo, &branch, &sha, &expected);
            Box::pin(async move { r })
        }),
        Box::new(move || c.now()),
        Arc::new(MemHeld::default()),
    )
}

/// What the suite runs against: githost's forge on a fake of the host over a fresh simulator.
pub struct GithostFixture {
    forge: HostForge,
    sim: SimForge,
}

impl ForgeFixture for GithostFixture {
    fn forge(&self) -> &dyn Forge {
        &self.forge
    }
    fn host(&self) -> Host {
        self.sim.host()
    }
    fn repo(&self) -> &str {
        REPO
    }
    fn git(&self) -> &SimForge {
        &self.sim
    }
}

fn suite(host: Host) -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let cases = kiln_seams::forge::cases::<GithostFixture>();
        let make = || async move {
            let sim = SimForge::of(host);
            let base = start(sim.clone()).await.unwrap_or_else(|e| format!("http://127.0.0.1:1/{e}"));
            GithostFixture { forge: forge_on(&base, &sim), sim }
        };
        let clock = crate::rt::clock();
        let report = kiln_seams::conformance::run_within("Forge (githost)", &cases, make, || clock.sleep(Duration::from_secs(120))).await;
        anyhow::ensure!(report.failed.is_empty() && report.passed.len() == cases.len(), "{}: failed {:?}", host.name(), report.failed);
        Ok(())
    })
}

/// W26-AC09: githost's forge passes the forge seam's suite against GitHub's simulator, over
/// GitHub's REST and GraphQL shapes.
#[test]
fn w26_ac09_githost_passes_the_forge_suite_on_github() -> anyhow::Result<()> {
    suite(Host::Github)
}

/// W26-AC09: githost's forge passes the forge seam's suite against GitLab's simulator.
#[test]
fn w26_ac09_githost_passes_the_forge_suite_on_gitlab() -> anyhow::Result<()> {
    suite(Host::Gitlab)
}

/// W26-AC09: githost's forge passes the forge seam's suite against Bitbucket's simulator.
#[test]
fn w26_ac09_githost_passes_the_forge_suite_on_bitbucket() -> anyhow::Result<()> {
    suite(Host::Bitbucket)
}

/// W26-AC09: an installation with 50 open pull requests, followed for an hour by webhooks and
/// ETag polling every minute while ten of them are pushed to every ten minutes, makes at most
/// 600 GitHub API requests (a poll that finds nothing new answered 304, free).
#[test]
fn w26_ac09_fifty_open_pull_requests_cost_at_most_600_github_requests_an_hour() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let sim = SimForge::github();
        let forge = forge_on(&start(sim.clone()).await?, &sim);
        for i in 0..50 {
            let branch = format!("pr-{i}");
            sim.push(REPO, &branch, TRUNK, &branch)?;
            forge.open(REPO, Open { head: branch.clone(), base: TRUNK.into(), title: branch, by: "alice".into() }).await?;
        }
        let mut watch = Watch::default();
        watch.poll(&forge, REPO).await?;
        let (counted, free) = (sim.counted(), sim.free());
        let mut pushes = 0u64;
        for minute in 0..60u64 {
            if minute % 10 == 5 {
                for k in 0..10u64 {
                    let n = (minute / 10 * 10 + k) % 50 + 1;
                    sim.push(REPO, &format!("pr-{}", n - 1), &format!("pr-{}", n - 1), &format!("push {minute}"))?;
                    pushes += 1;
                    assert!(watch.hook(&forge, REPO, n).await?, "the webhook for {n} found it changed");
                }
            }
            watch.poll(&forge, REPO).await?;
            sim.advance(60);
        }
        let spent = sim.counted() - counted;
        assert!(spent <= 600, "{spent} requests in the hour");
        assert!(sim.free() - free >= 50, "polls that found nothing new: {}", sim.free() - free);
        for (n, c) in &watch.changes {
            assert_eq!(Some(c.head_sha.clone()), sim.tip(REPO, &c.head_ref), "change {n} as watched");
        }
        assert_eq!((watch.changes.len(), pushes), (50, 60));
        Ok(())
    })
}
