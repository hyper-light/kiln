//! Review (docs/ci.md W14-R05) and policy from the trunk (R09): threads anchored to a version,
//! file, side and line range that follow the code to newer versions or become outdated; comments
//! drafted privately and published together with a review (accept, request changes, comment,
//! resign); each reviewer's state; files marked viewed until they change; the inbox of
//! conversations waiting for you and for others; and what still keeps a diff from landing, by
//! `.kiln/review.yaml` and `.kiln/owners.yaml` as the trunk has them now.

use std::collections::{BTreeMap, HashMap};

use http::StatusCode;
use kiln_ci::land::{self, Action, Facts, OwnerRule, Policy};
use kiln_ci::review::State;
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use super::Fail;
use super::diffs::{Diff, Version};
use crate::authz::{Caller, Scope};
use crate::gitops::{self, LS_REMOTE_TIMEOUT};
use crate::hub::Hub;

fn fail(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn bad(m: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, m.into())
}

/// Who a caller is in reviews: `user:ada`, `agent:review`, `admin`; and whether an agent.
pub fn who(caller: &Caller) -> (String, bool) {
    let (name, kind) = super::diffs::author_of(&caller.name, Some(caller.kind));
    (name, kind == "agent")
}

/// `.kiln/review.yaml` and `.kiln/owners.yaml` as the trunk has them now (a file that doesn't
/// read is the defaults, and says so in the log).
pub async fn policy(hub: &Hub, path: &str, trunk: &str) -> (Policy, Vec<OwnerRule>) {
    let read = |file: &'static str| {
        let spec = format!("refs/heads/{trunk}:{file}");
        async move { gitops::run_git(hub, &["-C", path, "show", &spec], LS_REMOTE_TIMEOUT).await.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default() }
    };
    let policy = Policy::parse(&read(".kiln/review.yaml").await).unwrap_or_else(|e| {
        tracing::warn!(%trunk, error = %e, "the trunk's review policy doesn't read; the defaults apply");
        Policy::default()
    });
    let owners = land::parse_owners(&read(".kiln/owners.yaml").await).unwrap_or_else(|e| {
        tracing::warn!(%trunk, error = %e, "the trunk's owners don't read; none apply");
        Vec::new()
    });
    (policy, owners)
}

// ------------------------------------------------------------------ threads and comments

/// The lines `lines` of `file` on side `side` of version `from`, as the later version `to` shows them.
async fn follow(hub: &Hub, path: &str, file: &str, side: &str, (from, to): (&Version, &Version), (start, end): (u32, u32)) -> Option<(u32, u32)> {
    let (a, b) = if side == "old" { (&from.parent, &to.parent) } else { (&from.commit, &to.commit) };
    if a == b {
        return Some((start, end));
    }
    let d = gitops::run_git(hub, &["-C", path, "diff", "-U0", "--no-color", "-M", a, b, "--", file], LS_REMOTE_TIMEOUT).await.ok()?;
    kiln_ci::review::follow(&kiln_ci::review::hunks(&String::from_utf8_lossy(&d)), start, end)
}

/// A diff's threads, with their comments: the published ones, and `viewer`'s drafts. Each
/// thread's lines as the latest version has them, or `outdated`.
pub async fn threads(hub: &Hub, d: &Diff, viewer: &str) -> Result<Vec<Value>, Fail> {
    let (id, me) = (d.id.clone(), viewer.to_owned());
    #[allow(clippy::type_complexity)]
    let rows: Vec<(String, u32, Option<String>, Option<String>, Option<u32>, Option<u32>, String, String, i64)> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT id, version, file, side, start_line, end_line, state, created_by, created_at FROM review_threads WHERE diff_id = ?1 ORDER BY created_at, id")?;
            let rows = st.query_map([&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(fail)?;
    let (id, me2) = (d.id.clone(), me.clone());
    let comments: Vec<(String, String, String, String, Option<String>, i64, bool)> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT c.id, c.thread_id, c.author, c.body, c.suggestion, c.created_at, c.published FROM review_comments c JOIN review_threads t ON t.id = c.thread_id
                 WHERE t.diff_id = ?1 AND (c.published = 1 OR c.author = ?2) ORDER BY c.created_at, c.id",
            )?;
            let rows = st.query_map(params![id, me2], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(fail)?;
    let versions = super::diffs::versions(hub, d).await.map_err(fail)?;
    let latest = versions.last().cloned();
    let path = super::diffs::home(hub, d).await.ok().map(|(_, p)| p);
    let mut by: HashMap<String, Vec<Value>> = HashMap::new();
    for (cid, tid, author, body, suggestion, at, published) in comments {
        by.entry(tid).or_default().push(json!({ "id": cid, "author": author, "body": body, "suggestion": suggestion, "created_at": at, "draft": !published }));
    }
    let mut out = Vec::new();
    for (tid, version, file, side, start, end, state, created_by, at) in rows {
        let Some(list) = by.remove(&tid) else { continue };
        let side = side.unwrap_or_else(|| "new".into());
        // Where it is now.
        let now = match (&file, start, &latest, &path, versions.iter().find(|v| v.n == version)) {
            (Some(f), Some(s), Some(l), Some(p), Some(v)) => follow(hub, p, f, &side, (v, l), (s, end.unwrap_or(s))).await.map(|(s, e)| json!({ "version": l.n, "start_line": s, "end_line": e })),
            (None, ..) => Some(Value::Null),
            _ => None,
        };
        out.push(json!({
            "id": tid, "version": version, "file": file, "side": side, "start_line": start, "end_line": end, "state": state, "created_by": created_by, "created_at": at,
            "outdated": now.is_none(), "now": now.unwrap_or(Value::Null), "comments": list,
        }));
    }
    Ok(out)
}

/// A new comment: on a thread (`thread`), or a new thread on a version's `file`, `side` and
/// lines (or on the diff as a whole). A draft until a review publishes it, unless `publish`.
pub async fn comment(hub: &Hub, d: &Diff, caller: &Caller, body: &Value) -> Result<Value, Fail> {
    let (me, _) = who(caller);
    let text = body.get("body").and_then(Value::as_str).unwrap_or_default().trim().to_owned();
    let suggestion = body.get("suggestion").and_then(Value::as_str).map(str::to_owned);
    if text.is_empty() && suggestion.is_none() {
        return Err(bad("a comment says something: `body`"));
    }
    let publish = body.get("publish").and_then(Value::as_bool).unwrap_or(false);
    let versions = super::diffs::versions(hub, d).await.map_err(fail)?;
    let latest = versions.last().map_or(1, |v| v.n);
    let thread = body.get("thread").and_then(Value::as_str).map(str::to_owned);
    let version = body.get("version").and_then(Value::as_u64).and_then(|v| u32::try_from(v).ok()).unwrap_or(latest);
    if !versions.iter().any(|v| v.n == version) {
        return Err(bad(format!("{} has no version {version}", d.name)));
    }
    let file = body.get("file").and_then(Value::as_str).map(str::to_owned);
    let side = body.get("side").and_then(Value::as_str).unwrap_or("new").to_owned();
    if !matches!(side.as_str(), "old" | "new") {
        return Err(bad("`side` is `old` or `new`"));
    }
    let line = |k: &str| body.get(k).and_then(Value::as_u64).and_then(|v| u32::try_from(v).ok());
    let (start, end) = (line("start_line").or_else(|| line("line")), line("end_line"));
    if file.is_some() != start.is_some() {
        return Err(bad("a comment on code names its `file` and `start_line`"));
    }
    let (diff_id, me2) = (d.id.clone(), me.clone());
    let (tid, cid) = hub
        .store
        .call(move |c| {
            let now = crate::util::now_unix();
            let tid = match thread {
                Some(t) => {
                    let ok: Option<String> = c.query_row("SELECT id FROM review_threads WHERE id = ?1 AND diff_id = ?2", params![t, diff_id], |r| r.get(0)).optional()?;
                    ok.ok_or_else(|| anyhow::anyhow!("no such thread on this diff"))?
                }
                None => {
                    let id = crate::util::new_id();
                    c.execute(
                        "INSERT INTO review_threads (id, diff_id, version, file, side, start_line, end_line, state, created_by, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'open', ?8, ?9)",
                        params![id, diff_id, version, file, side, start, end.or(start), me2, now],
                    )?;
                    id
                }
            };
            let cid = crate::util::new_id();
            c.execute(
                "INSERT INTO review_comments (id, thread_id, author, body, suggestion, created_at, updated_at, published) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)",
                params![cid, tid, me2, text, suggestion, now, publish],
            )?;
            Ok((tid, cid))
        })
        .await
        .map_err(|e| bad(format!("{e:#}")))?;
    if publish {
        hub.store.audit(caller.name.clone(), "diffs.comment".into(), format!("{} {} thread {tid}", d.repo, d.name)).await;
    }
    Ok(json!({ "id": cid, "thread": tid, "draft": !publish }))
}

/// `PATCH /api/comments/:id`: a comment's author edits it (`body`, `suggestion`); anyone who may
/// review resolves or reopens its thread (`state`).
pub async fn edit(hub: &Hub, id: &str, caller: &Caller, body: &Value) -> Result<Value, Fail> {
    let (me, _) = who(caller);
    let key = id.to_owned();
    let found: Option<(String, String, String, String)> = hub
        .store
        .call(move |c| Ok(c.query_row("SELECT c.thread_id, c.author, t.diff_id, p.name FROM review_comments c JOIN review_threads t ON t.id = c.thread_id JOIN diffs d ON d.id = t.diff_id JOIN repos p ON p.id = d.repo_id WHERE c.id = ?1", [&key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?))
        .await
        .map_err(fail)?;
    let Some((thread, author, _diff, repo)) = found else { return Err((StatusCode::NOT_FOUND, "no such comment".into())) };
    if !caller.can("diffs.review", &Scope::Repo { repo: repo.clone() }) {
        return Err((StatusCode::FORBIDDEN, "commenting needs `diffs.review`".into()));
    }
    let text = body.get("body").and_then(Value::as_str).map(str::to_owned);
    let suggestion = body.get("suggestion").and_then(Value::as_str).map(str::to_owned);
    if (text.is_some() || suggestion.is_some()) && !author.eq_ignore_ascii_case(&me) && !caller.is_owner() {
        return Err((StatusCode::FORBIDDEN, "only its author edits a comment".into()));
    }
    let state = match body.get("state").and_then(Value::as_str) {
        None => None,
        Some(s @ ("open" | "resolved")) => Some(s.to_owned()),
        Some(other) => return Err(bad(format!("`state` is `open` or `resolved`, not `{other}`"))),
    };
    let (cid, tid) = (id.to_owned(), thread.clone());
    let (text_c, sug_c, state_c) = (text.clone(), suggestion.clone(), state.clone());
    hub.store
        .call(move |c| {
            let now = crate::util::now_unix();
            if text_c.is_some() || sug_c.is_some() {
                c.execute("UPDATE review_comments SET body = COALESCE(?2, body), suggestion = COALESCE(?3, suggestion), updated_at = ?4 WHERE id = ?1", params![cid, text_c, sug_c, now])?;
            }
            if let Some(s) = state_c {
                c.execute("UPDATE review_threads SET state = ?2 WHERE id = ?1", params![tid, s])?;
            }
            Ok(())
        })
        .await
        .map_err(fail)?;
    if let Some(s) = &state {
        hub.store.audit(caller.name.clone(), format!("diffs.thread.{s}"), format!("{repo} thread {thread}")).await;
    }
    Ok(json!({ "id": id, "thread": thread, "state": state }))
}

// ------------------------------------------------------------------ reviews and policy

/// What a diff's land readiness depends on, now: its versions, its reviews, its open threads,
/// its latest version's checks and files.
pub async fn facts(hub: &Hub, d: &Diff, versions: &[Version]) -> Result<Facts, Fail> {
    let id = d.id.clone();
    let (reviews, open) = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT reviewer, reviewer_kind, version, action FROM reviews WHERE diff_id = ?1 ORDER BY created_at, rowid")?;
            let rows = st.query_map([&id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, u32>(2)?, r.get::<_, String>(3)?)))?;
            let reviews: Vec<land::Review> = rows.filter_map(Result::ok).filter_map(|(reviewer, kind, version, action)| Action::parse(&action).map(|action| land::Review { reviewer, agent: kind == "agent", version, action })).collect();
            let open: i64 = c.query_row("SELECT COUNT(*) FROM review_threads t WHERE t.diff_id = ?1 AND t.state = 'open' AND EXISTS (SELECT 1 FROM review_comments c WHERE c.thread_id = t.id AND c.published = 1)", [&id], |r| r.get(0))?;
            let open = usize::try_from(open).unwrap_or(0);
            Ok((reviews, open))
        })
        .await
        .map_err(fail)?;
    let latest = versions.last();
    let checks = match latest {
        Some(v) => checks_on(hub, &d.repo, &v.commit).await.map_err(fail)?,
        None => BTreeMap::new(),
    };
    let paths = match (latest, super::diffs::home(hub, d).await) {
        (Some(v), Ok((_, p))) => gitops::run_git(hub, &["-C", &p, "diff", "--name-only", &v.parent, &v.commit], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).lines().map(str::to_owned).collect()).unwrap_or_default(),
        _ => Vec::new(),
    };
    Ok(Facts { author: d.author.clone(), rebase_only: versions.iter().map(|v| v.rebase_only).collect(), reviews, open_threads: open, checks, paths })
}

/// The checks on a commit: each workflow (by its name and its path) and each of its jobs
/// (`workflow/job`) → its conclusion, `None` while it runs. The latest attempt of each.
pub async fn checks_on(hub: &Hub, repo: &str, sha: &str) -> anyhow::Result<BTreeMap<String, Option<String>>> {
    let (repo, sha) = (repo.to_owned(), sha.to_owned());
    hub.store
        .call(move |c| {
            let mut out: BTreeMap<String, Option<String>> = BTreeMap::new();
            let mut st = c.prepare(
                "SELECT r.id, r.pipeline, COALESCE(r.run_name, ''), r.status, r.conclusion FROM runs r JOIN repos p ON p.id = r.repo_id
                 WHERE p.name = ?1 AND r.sha = ?2 ORDER BY r.created_at, r.number",
            )?;
            let runs: Vec<(String, String, String, String, Option<String>)> = st.query_map(params![repo, sha], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.filter_map(Result::ok).collect();
            let mut jobs = c.prepare("SELECT COALESCE(job_id, key), status, conclusion FROM jobs WHERE run_id = ?1")?;
            for (id, path, name, status, conclusion) in runs {
                let done = |status: &str, conclusion: Option<String>| -> Option<String> {
                    match status {
                        "queued" | "running" | "waiting" | "pending" | "blocked" => None,
                        "succeeded" => Some(conclusion.unwrap_or_else(|| "success".into())),
                        _ => Some(conclusion.unwrap_or_else(|| status.to_owned())),
                    }
                };
                let state = done(&status, conclusion);
                let stem = path.rsplit('/').next().unwrap_or(&path).trim_end_matches(".yml").trim_end_matches(".yaml").to_owned();
                for key in [path.clone(), stem.clone(), name.clone()].into_iter().filter(|k| !k.is_empty()) {
                    out.insert(key, state.clone());
                }
                let rows: Vec<(String, String, Option<String>)> = jobs.query_map([&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.filter_map(Result::ok).collect();
                for (job, status, conclusion) in rows {
                    let state = done(&status, conclusion);
                    for w in [&stem, &name].into_iter().filter(|k| !k.is_empty()) {
                        out.insert(format!("{w}/{job}"), state.clone());
                    }
                }
            }
            Ok(out)
        })
        .await
}

/// What keeps `d` from landing now, by its trunk's policy: each a sentence.
pub async fn readiness(hub: &Hub, d: &Diff) -> Result<Value, Fail> {
    let versions = super::diffs::versions(hub, d).await.map_err(fail)?;
    let f = facts(hub, d, &versions).await?;
    let (_, path) = super::diffs::home(hub, d).await?;
    let (policy, owners) = policy(hub, &path, &d.trunk).await;
    let mut blockers = land::blockers(&policy.land, &owners, &f);
    match State::parse(&d.state) {
        Some(State::Draft) => blockers.insert(0, "it's a draft".into()),
        Some(State::Landed) => blockers = vec!["it landed".into()],
        Some(State::Abandoned) => blockers = vec!["it's abandoned".into()],
        Some(State::Landing) => blockers = vec!["it's in the land queue".into()],
        _ => {}
    }
    Ok(json!({ "blockers": blockers, "accepting": f.accepting(&policy.land), "requesting": f.requesting(), "checks": f.checks, "required": policy.land.checks, "strategies": policy.land.strategies, "default": policy.strategy(None, d.kind != "diff").ok() }))
}

/// `POST /api/diffs/:id/review`: `{ action: accept | request_changes | comment | resign, body }`.
/// Publishes the reviewer's drafts on the diff with it, and moves the diff's review state.
pub async fn review(hub: &Hub, d: &Diff, caller: &Caller, body: &Value) -> Result<Value, Fail> {
    let (me, agent) = who(caller);
    let action = body.get("action").and_then(Value::as_str).and_then(Action::parse).ok_or_else(|| bad("`action`: accept, request_changes, comment or resign"))?;
    let text = body.get("body").and_then(Value::as_str).unwrap_or_default().trim().to_owned();
    let state = State::parse(&d.state).unwrap_or(State::NeedsReview);
    if !state.is_open() {
        return Err((StatusCode::CONFLICT, format!("{} is {}", d.name, d.state)));
    }
    if action == Action::Accept && d.author.eq_ignore_ascii_case(&me) {
        return Err((StatusCode::FORBIDDEN, format!("{} is yours: someone else accepts it", d.name)));
    }
    let versions = super::diffs::versions(hub, d).await.map_err(fail)?;
    let latest = versions.last().map_or(1, |v| v.n);
    let (id, me2, text2) = (d.id.clone(), me.clone(), text.clone());
    let published = hub
        .store
        .call(move |c| {
            let tx = c.transaction()?;
            let now = crate::util::now_unix();
            let published = tx.execute("UPDATE review_comments SET published = 1, updated_at = ?3 WHERE author = ?2 AND published = 0 AND thread_id IN (SELECT id FROM review_threads WHERE diff_id = ?1)", params![id, me2, now])?;
            tx.execute(
                "INSERT INTO reviews (id, diff_id, version, reviewer, reviewer_kind, action, body, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![crate::util::new_id(), id, latest, me2, if agent { "agent" } else { "human" }, action.as_str(), text2, now],
            )?;
            match action {
                Action::Resign => {
                    tx.execute("DELETE FROM diff_reviewers WHERE diff_id = ?1 AND reviewer = ?2", params![id, me2])?;
                }
                _ => {
                    let st = match action {
                        Action::Accept => "accepted",
                        Action::RequestChanges => "asked-for-changes",
                        _ => "reviewing",
                    };
                    tx.execute(
                        "INSERT INTO diff_reviewers (diff_id, reviewer, state, added_at) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(diff_id, reviewer) DO UPDATE SET state = excluded.state",
                        params![id, me2, st, now],
                    )?;
                }
            }
            tx.commit()?;
            Ok(published)
        })
        .await
        .map_err(fail)?;
    // The review state the reviews give it now (a draft stays a draft, landing stays landing).
    let (_, path) = super::diffs::home(hub, d).await?;
    let (policy, _) = policy(hub, &path, &d.trunk).await;
    let f = facts(hub, d, &versions).await?;
    let now = land::review_state(&policy.land, &f);
    if matches!(state, State::NeedsReview | State::ChangesRequested | State::Accepted) && now != state {
        let (id, to) = (d.id.clone(), now.as_str());
        hub.store.call(move |c| Ok(c.execute("UPDATE diffs SET state = ?2, updated_at = ?3 WHERE id = ?1", params![id, to, crate::util::now_unix()]).map(|_| ())?)).await.map_err(fail)?;
    }
    let shown = if matches!(state, State::NeedsReview | State::ChangesRequested | State::Accepted) { now } else { state };
    hub.store.audit(caller.name.clone(), format!("diffs.review.{}", action.as_str()), format!("{} {} v{latest}", d.repo, d.name)).await;
    Ok(json!({ "diff": d.name, "action": action.as_str(), "version": latest, "published": published, "state": shown.as_str() }))
}

/// `POST /api/diffs/:id/viewed`: `{ file, viewed }` for the caller, at the latest version (it
/// stays viewed until the file changes).
pub async fn viewed(hub: &Hub, d: &Diff, caller: &Caller, body: &Value) -> Result<Value, Fail> {
    let (me, _) = who(caller);
    let file = body.get("file").and_then(Value::as_str).ok_or_else(|| bad("`file`"))?.to_owned();
    let on = body.get("viewed").and_then(Value::as_bool).unwrap_or(true);
    let versions = super::diffs::versions(hub, d).await.map_err(fail)?;
    let latest = versions.last().ok_or_else(|| bad("no versions"))?.clone();
    let (_, path) = super::diffs::home(hub, d).await?;
    let blob = gitops::run_git(hub, &["-C", &path, "rev-parse", &format!("{}:{file}", latest.commit)], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).unwrap_or_default();
    let (id, me2, f2, n) = (d.id.clone(), me, file.clone(), latest.n);
    hub.store
        .call(move |c| {
            if on {
                c.execute("INSERT OR REPLACE INTO viewed_files (diff_id, user, file, version, blob) VALUES (?1, ?2, ?3, ?4, ?5)", params![id, me2, f2, n, blob])?;
            } else {
                c.execute("DELETE FROM viewed_files WHERE diff_id = ?1 AND user = ?2 AND file = ?3", params![id, me2, f2])?;
            }
            Ok(())
        })
        .await
        .map_err(fail)?;
    Ok(json!({ "file": file, "viewed": on, "version": latest.n }))
}

/// The files `viewer` marked viewed that haven't changed since: file → the version marked.
pub async fn viewed_files(hub: &Hub, d: &Diff, viewer: &str, latest: &Version) -> HashMap<String, u32> {
    let (id, me) = (d.id.clone(), viewer.to_owned());
    let rows: Vec<(String, u32, String)> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT file, version, blob FROM viewed_files WHERE diff_id = ?1 AND user = ?2")?;
            let rows = st.query_map(params![id, me], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            Ok(rows.filter_map(Result::ok).collect())
        })
        .await
        .unwrap_or_default();
    let Ok((_, path)) = super::diffs::home(hub, d).await else { return HashMap::new() };
    let mut out = HashMap::new();
    for (file, version, blob) in rows {
        let now = gitops::run_git(hub, &["-C", &path, "rev-parse", &format!("{}:{file}", latest.commit)], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).unwrap_or_default();
        if now == blob {
            out.insert(file, version);
        }
    }
    out
}

// ------------------------------------------------------------------ the inbox

/// `GET /api/review/inbox`: the caller's conversations (G-08) in the repositories they may read:
/// threads waiting for their reply (on their diffs, ones they took part in, ones that mention
/// them), threads where they wait for others, diffs that ask for their review, and their own open
/// diffs.
pub async fn inbox(hub: &Hub, caller: &Caller) -> Result<Value, Fail> {
    let (me, _) = who(caller);
    let mention = format!("@{}", me.strip_prefix("user:").unwrap_or(&me));
    let me2 = me.clone();
    #[allow(clippy::type_complexity)]
    let (threads, asked, mine): (Vec<(String, String, String, String, u32, Option<String>, Option<u32>, String, String, bool, bool)>, Vec<(String, String, String, String, String)>, Vec<(String, String, String, String)>) = hub
        .store
        .call(move |c| {
            // Each open thread with a published comment, on an open diff: its last published
            // comment, whether the caller wrote the diff, took part, or is mentioned.
            let mut st = c.prepare(
                "SELECT t.id, p.name, 'D' || d.number, d.title, t.version, t.file, t.start_line,
                        (SELECT author FROM review_comments WHERE thread_id = t.id AND published = 1 ORDER BY created_at DESC, id DESC LIMIT 1),
                        (SELECT body FROM review_comments WHERE thread_id = t.id AND published = 1 ORDER BY created_at DESC, id DESC LIMIT 1),
                        d.author = ?1 OR EXISTS (SELECT 1 FROM review_comments WHERE thread_id = t.id AND author = ?1),
                        EXISTS (SELECT 1 FROM review_comments WHERE thread_id = t.id AND published = 1 AND instr(body, ?2) > 0)
                 FROM review_threads t JOIN diffs d ON d.id = t.diff_id JOIN repos p ON p.id = d.repo_id
                 WHERE t.state = 'open' AND d.state NOT IN ('landed', 'abandoned') AND EXISTS (SELECT 1 FROM review_comments WHERE thread_id = t.id AND published = 1)
                 ORDER BY t.created_at DESC LIMIT 500",
            )?;
            let threads = st
                .query_map(params![me2, mention], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?)))?
                .filter_map(Result::ok)
                .collect();
            let mut st = c.prepare(
                "SELECT p.name, 'D' || d.number, d.title, d.author, r.state FROM diff_reviewers r JOIN diffs d ON d.id = r.diff_id JOIN repos p ON p.id = d.repo_id
                 WHERE r.reviewer = ?1 AND d.state IN ('needs-review', 'changes-requested') AND r.state IN ('not-looked', 'reviewing') ORDER BY d.updated_at DESC LIMIT 200",
            )?;
            let asked = st.query_map([&me2], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.filter_map(Result::ok).collect();
            let mut st = c.prepare("SELECT p.name, 'D' || d.number, d.title, d.state FROM diffs d JOIN repos p ON p.id = d.repo_id WHERE d.author = ?1 AND d.state NOT IN ('landed', 'abandoned') ORDER BY d.updated_at DESC LIMIT 200")?;
            let mine = st.query_map([&me2], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.filter_map(Result::ok).collect();
            Ok((threads, asked, mine))
        })
        .await
        .map_err(fail)?;
    let may = |repo: &str| caller.can("diffs.read", &Scope::Repo { repo: repo.to_owned() });
    let (mut yours, mut theirs) = (Vec::new(), Vec::new());
    for (id, repo, diff, title, version, file, line, last, body, involved, mentioned) in threads {
        if !may(&repo) || !(involved || mentioned) {
            continue;
        }
        let row = json!({ "thread": id, "repo": repo, "diff": diff, "title": title, "version": version, "file": file, "line": line, "last_author": last, "last": body.chars().take(280).collect::<String>() });
        if last.eq_ignore_ascii_case(&me) { theirs.push(row) } else { yours.push(row) }
    }
    let asked: Vec<Value> = asked.into_iter().filter(|(repo, ..)| may(repo)).map(|(repo, diff, title, author, state)| json!({ "repo": repo, "diff": diff, "title": title, "author": author, "your_state": state })).collect();
    let mine: Vec<Value> = mine.into_iter().filter(|(repo, ..)| may(repo)).map(|(repo, diff, title, state)| json!({ "repo": repo, "diff": diff, "title": title, "state": state })).collect();
    Ok(json!({ "you": me, "waiting_for_you": yours, "waiting_for_others": theirs, "review_requested": asked, "your_diffs": mine }))
}
