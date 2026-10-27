//! Review (docs/ci.md W14-R05) and policy from the trunk (R09): threads anchored to a version,
//! file, side and line range that follow the code to newer versions or become outdated; comments
//! drafted privately and published together with a review (accept, request changes, comment,
//! resign); each reviewer's state; files marked viewed until they change; the inbox of
//! conversations waiting for you and for others; and what still keeps a diff from landing, by
//! `.kiln/review.yaml` and `.kiln/owners.yaml` as the trunk has them now.

use std::collections::{BTreeMap, HashMap};

use http::StatusCode;
use kiln_ci::land::{self, Action, Facts, OwnerRule, Policy};
use kiln_ci::review::{Gate, Kind, State};
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
/// thread's lines as the latest version has them, or `outdated`; its kind (W26-R11); who resolved
/// it, when and why. A comment its author deleted says `deleted` (W26-R08).
pub async fn threads(hub: &Hub, d: &Diff, viewer: &str) -> Result<Vec<Value>, Fail> {
    let (id, me) = (d.id.clone(), viewer.to_owned());
    #[allow(clippy::type_complexity)]
    let rows: Vec<((String, u32, Option<String>, Option<String>, Option<u32>, Option<u32>, String, String, i64), (String, Option<String>, Option<i64>, Option<String>))> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT id, version, file, side, start_line, end_line, state, created_by, created_at, kind, resolved_by, resolved_at, resolution FROM review_threads WHERE diff_id = ?1 ORDER BY created_at, id",
            )?;
            let rows = st.query_map([&id], |r| Ok(((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?), (r.get(9)?, r.get(10)?, r.get(11)?, r.get(12)?))))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(fail)?;
    let (id, me2) = (d.id.clone(), me.clone());
    let comments: Vec<(String, String, String, String, Option<String>, i64, bool, bool)> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT c.id, c.thread_id, c.author, c.body, c.suggestion, c.created_at, c.published, c.deleted_at IS NOT NULL FROM review_comments c JOIN review_threads t ON t.id = c.thread_id
                 WHERE t.diff_id = ?1 AND (c.published = 1 OR c.author = ?2) ORDER BY c.created_at, c.id",
            )?;
            let rows = st.query_map(params![id, me2], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(fail)?;
    let versions = super::diffs::versions(hub, d).await.map_err(fail)?;
    let latest = versions.last().cloned();
    let path = super::diffs::home(hub, d).await.ok().map(|(_, p)| p);
    let mut by: HashMap<String, Vec<Value>> = HashMap::new();
    for (cid, tid, author, body, suggestion, at, published, deleted) in comments {
        by.entry(tid).or_default().push(json!({ "id": cid, "author": author, "body": body, "suggestion": suggestion, "created_at": at, "draft": !published, "deleted": deleted }));
    }
    let mut out = Vec::new();
    for ((tid, version, file, side, start, end, state, created_by, at), (kind, resolved_by, resolved_at, resolution)) in rows {
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
            "kind": Kind::read(&kind).as_str(), "resolved_by": resolved_by, "resolved_at": resolved_at, "resolution": resolution,
        }));
    }
    Ok(out)
}

/// A diff's threads as the gate reads them (W26-R11): each one's kind and reviewer, where it
/// is, whether it's resolved, and whether it's only a draft yet (none of its comments published).
async fn gate_threads(hub: &Hub, d: &Diff) -> Result<Vec<kiln_ci::review::Thread>, Fail> {
    let id = d.id.clone();
    hub.store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT t.kind, t.created_by, t.file, t.start_line, t.state = 'resolved', NOT EXISTS (SELECT 1 FROM review_comments c WHERE c.thread_id = t.id AND c.published = 1)
                 FROM review_threads t WHERE t.diff_id = ?1 ORDER BY t.created_at, t.id",
            )?;
            let rows = st.query_map([&id], |r| Ok(kiln_ci::review::Thread { kind: Kind::read(&r.get::<_, String>(0)?), by: r.get(1)?, file: r.get(2)?, line: r.get(3)?, resolved: r.get(4)?, draft: r.get(5)? }))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(fail)
}

/// The gate on `d` (W26-R11): for `asking`, whoever approves (their drafts count, since approving
/// publishes them), or for no one, as merging and any automatic merge ask it. Conflicts join it as
/// W26-R12 records them.
pub async fn gate_of(hub: &Hub, d: &Diff, asking: Option<&str>) -> Result<Gate, Fail> {
    Ok(kiln_ci::review::gate(&gate_threads(hub, d).await?, &[], asking))
}

/// A new comment: on a thread (`thread`), or a new thread on a version's `file`, `side` and
/// lines (or on the diff as a whole). A draft until a review publishes it, unless `publish`.
pub async fn comment(hub: &std::sync::Arc<Hub>, d: &Diff, caller: &Caller, body: &Value) -> Result<Value, Fail> {
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
    // A new thread's kind (W26-R11): a question unless it says otherwise.
    let kind = match body.get("kind").and_then(Value::as_str) {
        None => Kind::Question,
        Some(k) => Kind::parse(k).ok_or_else(|| bad("`kind` is `question`, `nit`, `issue` or `blocking`"))?,
    }
    .as_str();
    let (diff_id, me2, said, file_c, side_c) = (d.id.clone(), me.clone(), text.clone(), file.clone(), side.clone());
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
                        "INSERT INTO review_threads (id, diff_id, version, file, side, start_line, end_line, state, created_by, created_at, kind) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'open', ?8, ?9, ?10)",
                        params![id, diff_id, version, file, side, start, end.or(start), me2, now, kind],
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
        // For whoever follows a diff's conversation (an agent's queue, the inbox).
        hub.emit(&json!({
            "type": "comment", "repo": d.repo, "diff": d.name, "version": version, "thread": tid, "comment": cid, "author": me, "body": said,
            "file": file_c, "side": side_c, "start_line": start, "end_line": end.or(start), "diff_author": d.author, "diff_author_for": d.author_for,
        }));
        tell(hub, d, &me, vec![(tid.clone(), said)]);
    }
    Ok(json!({ "id": cid, "thread": tid, "draft": !publish }))
}

/// Who a published comment is news to (W19-R06), for each `(thread, body)`: the people in its
/// thread and the diff's author (`comment.reply`), and whom it mentions (`mention`), but whoever
/// wrote it.
fn tell(hub: &std::sync::Arc<Hub>, d: &Diff, me: &str, said: Vec<(String, String)>) {
    if said.is_empty() {
        return;
    }
    let (hub, d, me) = (std::sync::Arc::clone(hub), d.clone(), me.to_owned());
    crate::rt::spawn(async move {
        for (thread, body) in said {
            let t = thread.clone();
            let mut people: Vec<String> = hub
                .store
                .call(move |c| {
                    let mut st = c.prepare("SELECT DISTINCT author FROM review_comments WHERE thread_id = ?1 AND published = 1")?;
                    let rows = st.query_map([&t], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
                    Ok(rows)
                })
                .await
                .unwrap_or_default();
            people.push(d.author.clone());
            super::notify::diff_event(
                std::sync::Arc::clone(&hub),
                d.repo.clone(),
                "comment.reply",
                d.name.clone(),
                d.title.clone(),
                d.trunk.clone(),
                me.clone(),
                body.clone(),
                people,
                Some(thread.clone()),
            )
            .await;
            let named = kiln_ci::trigger::context::mentions(&body);
            super::notify::diff_event(std::sync::Arc::clone(&hub), d.repo.clone(), "mention", d.name.clone(), d.title.clone(), d.trunk.clone(), me.clone(), body, named, Some(thread)).await;
        }
    });
}

/// A comment as editing and deleting it read it: its thread, its author, whether it was deleted;
/// its thread's author, kind, state and place; its diff's name and repository.
struct Found {
    thread: String,
    author: String,
    deleted: bool,
    opened_by: String,
    kind: Kind,
    resolved: bool,
    at: String,
    diff: String,
    repo: String,
}

async fn found(hub: &Hub, id: &str) -> Result<Found, Fail> {
    let key = id.to_owned();
    hub.store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT c.thread_id, c.author, c.deleted_at IS NOT NULL, t.created_by, t.kind, t.state = 'resolved', t.file, t.start_line, CASE d.kind WHEN 'diff' THEN 'D' ELSE 'CR' END || d.number, p.name
                 FROM review_comments c JOIN review_threads t ON t.id = c.thread_id JOIN diffs d ON d.id = t.diff_id JOIN repos p ON p.id = d.repo_id WHERE c.id = ?1",
                [&key],
                |r| {
                    let (file, line): (Option<String>, Option<u32>) = (r.get(6)?, r.get(7)?);
                    Ok(Found {
                        thread: r.get(0)?,
                        author: r.get(1)?,
                        deleted: r.get(2)?,
                        opened_by: r.get(3)?,
                        kind: Kind::read(&r.get::<_, String>(4)?),
                        resolved: r.get(5)?,
                        at: kiln_ci::review::place(file.as_deref(), line),
                        diff: r.get(8)?,
                        repo: r.get(9)?,
                    })
                },
            )
            .optional()?)
        })
        .await
        .map_err(fail)?
        .ok_or((StatusCode::NOT_FOUND, "no such comment".into()))
}

/// `PATCH /api/comments/:id`: a comment's author edits it (`body`, `suggestion`); its thread's
/// author changes the thread's kind (`kind`, W26-R11); anyone who may review resolves or reopens
/// its thread (`state`), but a blocking thread is resolved only by its author, or by an owner who
/// says why (`reason`, kept with the thread: a reviewer who left can't strand a change). Kinds and
/// resolutions are audited, and who resolved a thread, when and why, is kept.
pub async fn edit(hub: &Hub, id: &str, caller: &Caller, body: &Value) -> Result<Value, Fail> {
    let (me, _) = who(caller);
    let f = found(hub, id).await?;
    if !caller.can("diffs.review", &Scope::Repo { repo: f.repo.clone() }) {
        return Err((StatusCode::FORBIDDEN, "commenting needs `diffs.review`".into()));
    }
    let text = body.get("body").and_then(Value::as_str).map(str::to_owned);
    let suggestion = body.get("suggestion").and_then(Value::as_str).map(str::to_owned);
    if text.is_some() || suggestion.is_some() {
        if f.deleted {
            return Err((StatusCode::CONFLICT, "the comment was deleted".into()));
        }
        if !f.author.eq_ignore_ascii_case(&me) && !caller.is_owner() {
            return Err((StatusCode::FORBIDDEN, "only its author edits a comment".into()));
        }
    }
    let state = match body.get("state").and_then(Value::as_str) {
        None => None,
        Some(s @ ("open" | "resolved")) => Some(s.to_owned()),
        Some(other) => return Err(bad(format!("`state` is `open` or `resolved`, not `{other}`"))),
    };
    let kind = match body.get("kind").and_then(Value::as_str) {
        None => None,
        Some(k) => Some(Kind::parse(k).ok_or_else(|| bad(format!("`kind` is `question`, `nit`, `issue` or `blocking`, not `{k}`")))?),
    }
    .filter(|k| *k != f.kind);
    let reason = body.get("reason").and_then(Value::as_str).map(str::trim).filter(|r| !r.is_empty()).map(str::to_owned);
    let theirs = f.opened_by.eq_ignore_ascii_case(&me);
    let whose = kiln_ci::review::named(&f.opened_by);
    // A thread's kind is its author's to change (W26-R11).
    if kind.is_some() && !theirs {
        return Err((StatusCode::FORBIDDEN, format!("{}: only {whose} changes this thread's kind", f.at)));
    }
    // A blocking thread holds the change until its author resolves it; an owner may, saying why.
    let resolving = state.as_deref() == Some("resolved") && !f.resolved;
    let reopening = state.as_deref() == Some("open") && f.resolved;
    if resolving && f.kind == Kind::Blocking && !theirs {
        if !caller.is_owner() {
            return Err((StatusCode::FORBIDDEN, format!("{}: only {whose} resolves this blocking comment (or an owner, saying why)", f.at)));
        }
        if reason.is_none() {
            return Err(bad(format!("{}: an owner resolving {whose}'s blocking comment says why (`reason`)", f.at)));
        }
    }
    let (cid, tid, me2) = (id.to_owned(), f.thread.clone(), me.clone());
    let (text_c, sug_c, reason_c) = (text.clone(), suggestion.clone(), reason.clone());
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let now = crate::util::now_unix();
            if text_c.is_some() || sug_c.is_some() {
                tx.execute("UPDATE review_comments SET body = COALESCE(?2, body), suggestion = COALESCE(?3, suggestion), updated_at = ?4 WHERE id = ?1", params![cid, text_c, sug_c, now])?;
            }
            if let Some(k) = kind {
                tx.execute("UPDATE review_threads SET kind = ?2 WHERE id = ?1", params![tid, k.as_str()])?;
            }
            if resolving {
                tx.execute("UPDATE review_threads SET state = 'resolved', resolved_by = ?2, resolved_at = ?3, resolution = ?4 WHERE id = ?1", params![tid, me2, now, reason_c])?;
            } else if reopening {
                tx.execute("UPDATE review_threads SET state = 'open', resolved_by = NULL, resolved_at = NULL, resolution = NULL WHERE id = ?1", [&tid])?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
        .map_err(fail)?;
    let (repo, thread) = (&f.repo, &f.thread);
    if let Some(k) = kind {
        hub.store.audit(caller.name.clone(), "diffs.thread.kind".into(), format!("{repo} {} thread {thread} ({}): {} to {}", f.diff, f.at, f.kind.as_str(), k.as_str())).await;
    }
    if let Some(s) = &state {
        let why = reason.as_deref().filter(|_| resolving).map(|r| format!(" ({whose}'s {}): {r}", f.kind.as_str())).unwrap_or_default();
        hub.store.audit(caller.name.clone(), format!("diffs.thread.{s}"), format!("{repo} thread {thread}{why}")).await;
    }
    if kind.is_some() || resolving || reopening {
        // What waits on the gate to land asks it again: an automatic merge goes once it clears.
        super::land::kick();
    }
    Ok(json!({ "id": id, "thread": thread, "state": state, "kind": kind.unwrap_or(f.kind).as_str() }))
}

/// `DELETE /api/comments/:id` (W26-R08): its author takes a comment back. A thread's first comment
/// stays where it was, saying `deleted`, while replies remain under it; a thread left with no
/// comment goes with it, and with it whatever it held. Audited.
pub async fn delete(hub: &Hub, id: &str, caller: &Caller) -> Result<Value, Fail> {
    let (me, _) = who(caller);
    let f = found(hub, id).await?;
    if f.deleted {
        return Err((StatusCode::NOT_FOUND, "no such comment: it was deleted".into()));
    }
    if !caller.can("diffs.review", &Scope::Repo { repo: f.repo.clone() }) {
        return Err((StatusCode::FORBIDDEN, "commenting needs `diffs.review`".into()));
    }
    if !f.author.eq_ignore_ascii_case(&me) {
        return Err((StatusCode::FORBIDDEN, format!("only its author, {}, deletes a comment", kiln_ci::review::named(&f.author))));
    }
    let (cid, tid) = (id.to_owned(), f.thread.clone());
    let gone = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let first: Option<String> = tx.query_row("SELECT id FROM review_comments WHERE thread_id = ?1 ORDER BY created_at, rowid LIMIT 1", [&tid], |r| r.get(0)).optional()?;
            let others: i64 = tx.query_row("SELECT COUNT(*) FROM review_comments WHERE thread_id = ?1 AND id <> ?2 AND deleted_at IS NULL", params![tid, cid], |r| r.get(0))?;
            let gone = others == 0;
            if gone {
                tx.execute("DELETE FROM review_comments WHERE thread_id = ?1", [&tid])?;
                tx.execute("DELETE FROM review_threads WHERE id = ?1", [&tid])?;
            } else if first.as_deref() == Some(cid.as_str()) {
                tx.execute("UPDATE review_comments SET body = 'deleted', suggestion = NULL, deleted_at = ?2, updated_at = ?2 WHERE id = ?1", params![cid, crate::util::now_unix()])?;
            } else {
                tx.execute("DELETE FROM review_comments WHERE id = ?1", [&cid])?;
            }
            tx.commit()?;
            Ok(gone)
        })
        .await
        .map_err(fail)?;
    hub.store.audit(caller.name.clone(), "diffs.comment.delete".into(), format!("{} {} thread {} ({}) comment {id}{}", f.repo, f.diff, f.thread, f.at, if gone { ", and the thread with it" } else { "" })).await;
    if gone {
        super::land::kick();
    }
    Ok(json!({ "id": id, "thread": f.thread, "deleted": true, "thread_deleted": gone }))
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
            // Threads on code (as GitHub's conversation resolution counts them; a comment on the
            // diff as a whole, a bot's report say, doesn't hold a land up).
            let open: i64 = c.query_row("SELECT COUNT(*) FROM review_threads t WHERE t.diff_id = ?1 AND t.state = 'open' AND t.file IS NOT NULL AND EXISTS (SELECT 1 FROM review_comments c WHERE c.thread_id = t.id AND c.published = 1)", [&id], |r| r.get(0))?;
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

/// The `bench-gate` results on commits of a repository (W16-R09), by commit: what each leg's
/// latest attempt was told, failing (`error`) or warning, with its run and job. Shown on the diff
/// and in the land queue.
pub async fn gates_on(hub: &Hub, repo: &str, shas: Vec<String>) -> anyhow::Result<HashMap<String, Vec<Value>>> {
    if shas.is_empty() {
        return Ok(HashMap::new());
    }
    let (repo, shas) = (repo.to_owned(), serde_json::to_string(&shas)?);
    hub.store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT r.sha, r.id, j.id, COALESCE(j.job_id, j.key), a.level, a.message FROM runs r JOIN repos p ON p.id = r.repo_id JOIN jobs j ON j.run_id = r.id JOIN ci_annotations a ON a.leg_id = j.id
                 WHERE (p.name = ?1 OR p.id = ?1) AND r.sha IN (SELECT value FROM json_each(?2)) AND a.title = 'bench-gate'
                   AND j.run_attempt = (SELECT MAX(k.run_attempt) FROM jobs k WHERE k.run_id = j.run_id AND k.key = j.key)
                 ORDER BY a.id",
            )?;
            let mut out: HashMap<String, Vec<Value>> = HashMap::new();
            let rows = st.query_map(params![repo, shas], |r| {
                Ok((r.get::<_, String>(0)?, json!({ "run": r.get::<_, String>(1)?, "leg": r.get::<_, String>(2)?, "job": r.get::<_, String>(3)?, "level": r.get::<_, String>(4)?, "message": r.get::<_, String>(5)? })))
            })?;
            for row in rows {
                let (sha, g) = row?;
                out.entry(sha).or_default().push(g);
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
    // A blocking thread holds landing, and any automatic land, until its author resolves it
    // (W26-R11): the gate's hold, as accepting and Ship read it. The change stays out of the queue
    // until its review clears (W26-R02).
    blockers.extend(gate_of(hub, d, None).await?.reasons());
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
pub async fn review(hub: &std::sync::Arc<Hub>, d: &Diff, caller: &Caller, body: &Value) -> Result<Value, Fail> {
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
    // Accepting goes through the gate (W26-R11): a blocking thread or a conflict holds it, saying
    // why; open issues ask to go ahead anyway (`force`); what's still open is said.
    let note = match action {
        Action::Accept => gate_of(hub, d, Some(&me)).await?.approve(body.get("force").and_then(Value::as_bool).unwrap_or(false)).map_err(|why| (StatusCode::CONFLICT, why))?,
        _ => None,
    };
    let versions = super::diffs::versions(hub, d).await.map_err(fail)?;
    let latest = versions.last().map_or(1, |v| v.n);
    let (id, me2, text2) = (d.id.clone(), me.clone(), text.clone());
    let (published, drafts) = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let now = crate::util::now_unix();
            // What the review publishes, for whom it's news (W19-R06).
            let drafts: Vec<(String, String)> = {
                let mut st = tx.prepare("SELECT c.thread_id, c.body FROM review_comments c JOIN review_threads t ON t.id = c.thread_id WHERE t.diff_id = ?1 AND c.author = ?2 AND c.published = 0 ORDER BY c.created_at")?;
                st.query_map(params![id, me2], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?
            };
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
            Ok((published, drafts))
        })
        .await
        .map_err(fail)?;
    tell(hub, d, &me, drafts);
    if !text.is_empty() {
        let named = kiln_ci::trigger::context::mentions(&text);
        crate::rt::spawn(super::notify::diff_event(std::sync::Arc::clone(hub), d.repo.clone(), "mention", d.name.clone(), d.title.clone(), d.trunk.clone(), me.clone(), text.clone(), named, None));
    }
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
    hub.emit(&json!({ "type": "review", "repo": d.repo, "diff": d.name, "version": latest, "reviewer": me, "action": action.as_str(), "body": text, "published": published, "state": shown.as_str(), "diff_author": d.author, "diff_author_for": d.author_for }));
    Ok(json!({ "diff": d.name, "action": action.as_str(), "version": latest, "published": published, "state": shown.as_str(), "note": note }))
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

/// `POST /api/comments/:id/apply` (W14-R05): a comment's suggestion, the lines it replaces taken
/// to the latest version. A diff gets a new version, its commit amended (same parent, same
/// message, same author); a change request's branch gets a commit. The thread is resolved, saying
/// where the suggestion went.
pub async fn apply_suggestion(hub: &std::sync::Arc<Hub>, id: &str, caller: &Caller) -> Result<Value, Fail> {
    let key = id.to_owned();
    #[allow(clippy::type_complexity)]
    let found: Option<(Option<String>, String, String, String, u32, Option<String>, Option<String>, Option<u32>, Option<u32>)> = hub
        .store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT c.suggestion, c.author, c.thread_id, t.diff_id, t.version, t.file, t.side, t.start_line, t.end_line FROM review_comments c JOIN review_threads t ON t.id = c.thread_id WHERE c.id = ?1 OR CAST(c.rowid AS TEXT) = ?1",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)),
            )
            .optional()?)
        })
        .await
        .map_err(fail)?;
    let Some((suggestion, suggested_by, thread, diff, version, file, side, start, end)) = found else { return Err((StatusCode::NOT_FOUND, "no such comment".into())) };
    let Some(suggestion) = suggestion else { return Err(bad("the comment suggests nothing")) };
    let (Some(file), Some(start)) = (file, start) else { return Err(bad("a suggestion is on a file's lines")) };
    if side.as_deref() == Some("old") {
        return Err(bad("a suggestion replaces lines of the new side"));
    }
    let d = super::diffs::find(hub, &diff).await.map_err(fail)?.ok_or((StatusCode::NOT_FOUND, "the diff is gone".to_owned()))?;
    if !caller.can("diffs.submit", &Scope::Repo { repo: d.repo.clone() }) {
        return Err((StatusCode::FORBIDDEN, "applying a suggestion needs `diffs.submit`".into()));
    }
    if !State::parse(&d.state).is_some_and(|s| s.is_open() && s != State::Landing) {
        return Err((StatusCode::CONFLICT, format!("{} is {}", d.name, d.state)));
    }
    let versions = super::diffs::versions(hub, &d).await.map_err(fail)?;
    let (Some(from), Some(latest)) = (versions.iter().find(|v| v.n == version).cloned(), versions.last().cloned()) else { return Err(bad("the comment's version is gone")) };
    let (repo, path) = super::diffs::home(hub, &d).await?;
    let (s, e) = follow(hub, &path, &file, "new", (&from, &latest), (start, end.unwrap_or(start))).await.ok_or((StatusCode::CONFLICT, "the lines it replaces changed since: it's outdated".to_owned()))?;
    // The file with the lines replaced.
    let old = gitops::run_git(hub, &["-C", &path, "show", &format!("{}:{file}", latest.commit)], LS_REMOTE_TIMEOUT).await.map_err(|_| bad(format!("{file} isn't in the latest version")))?;
    let text = String::from_utf8_lossy(&old).into_owned();
    let ends_nl = text.ends_with('\n');
    let mut lines: Vec<&str> = text.lines().collect();
    let (lo, hi) = (usize::try_from(s).unwrap_or(1).max(1) - 1, usize::try_from(e).unwrap_or(1));
    if hi > lines.len() || lo >= hi {
        return Err((StatusCode::CONFLICT, "the lines it replaces aren't there".into()));
    }
    let replacement: Vec<&str> = suggestion.lines().collect();
    lines.splice(lo..hi, replacement);
    let mut new_text = lines.join("\n");
    if ends_nl {
        new_text.push('\n');
    }
    let listing = gitops::run_git(hub, &["-C", &path, "ls-tree", &latest.commit, "--", &file], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).into_owned()).unwrap_or_default();
    let mode = listing.split_whitespace().next().unwrap_or("100644").to_owned();
    let tree = super::diffs::tree_with(hub, &path, &latest.commit, super::diffs::Edit::Put { file: &file, mode: &mode, content: new_text.into_bytes() }).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let (me, _) = who(caller);
    let applier = me.strip_prefix("user:").unwrap_or(&me).to_owned();
    let n = match &d.branch {
        // A change request: a commit on its branch, a version from it.
        Some(branch) => {
            let msg = format!("Apply a suggestion from {suggested_by}\n\nOn {file}, lines {s}-{e}.\n");
            let c = super::diffs::make_commit(hub, &path, &tree, &latest.commit, (&applier, None), &applier, &msg).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
            gitops::run_git(hub, &["-C", &path, "update-ref", &format!("refs/heads/{branch}"), &c, &latest.commit], LS_REMOTE_TIMEOUT).await.map_err(|_| (StatusCode::CONFLICT, format!("{branch} moved: push or pull, then try again")))?;
            let by = super::cr::By { pushed_by: caller.name.clone(), author: super::diffs::author_of(&caller.name, Some(caller.kind)), started_by: caller.started_by() };
            super::cr::submit(hub, &repo, &path, &d.trunk, branch, &c, &by, &kiln_ci::review::Options::default()).await.map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?.version
        }
        // A diff: its commit amended.
        None => {
            let message = gitops::run_git(hub, &["-C", &path, "log", "-1", "--format=%B", &latest.commit], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).into_owned()).unwrap_or_default();
            let who_wrote = gitops::run_git(hub, &["-C", &path, "log", "-1", "--format=%an <%ae>%x00%ad", "--date=raw", &latest.commit], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).unwrap_or_default();
            let (author, date) = who_wrote.split_once('\0').unwrap_or((&who_wrote, ""));
            let c = super::diffs::make_commit(hub, &path, &tree, &latest.parent, (author, Some(date).filter(|d| !d.is_empty())), &applier, &message).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
            let patch = super::diffs::git_run(hub, &["-C", &path, "show", "--no-color", &c], &[], Vec::new()).await.ok().map(|r| r.out).unwrap_or_default();
            let patch_id = super::diffs::git_with_input(hub, &["-C", &path, "patch-id", "--stable"], patch).await.ok().and_then(|o| String::from_utf8_lossy(&o).split_whitespace().next().map(str::to_owned)).unwrap_or_default();
            let id = d.id.clone();
            let below: Option<String> = hub.store.call(move |c| Ok(c.query_row("SELECT parent_diff FROM diffs WHERE id = ?1", [&id], |r| r.get(0)).optional()?.flatten())).await.map_err(fail)?;
            let nv = super::diffs::NewVersion { commit: c, parent: latest.parent.clone(), base: latest.base.clone(), patch_id, message, note: Some(format!("applied a suggestion from {suggested_by}")), rebase_only: false, pushed_by: caller.name.clone(), parent_diff: below };
            super::diffs::add_version(hub, &repo, &path, &d, nv).await.map_err(fail)?
        }
    };
    // The thread says where it went, and is done; a blocking one stays open until its author
    // resolves it (W26-R11), unless they applied it themselves.
    let (t, me2, note) = (thread.clone(), me.clone(), format!("Applied in v{n}."));
    let resolved = hub
        .store
        .call(move |c| {
            let now = crate::util::now_unix();
            c.execute("INSERT INTO review_comments (id, thread_id, author, body, created_at, updated_at, published) VALUES (?1, ?2, ?3, ?4, ?5, ?5, 1)", params![crate::util::new_id(), t, me2, note, now])?;
            Ok(c.execute("UPDATE review_threads SET state = 'resolved', resolved_by = ?2, resolved_at = ?3 WHERE id = ?1 AND (kind <> 'blocking' OR lower(created_by) = lower(?2))", params![t, me2, now])? > 0)
        })
        .await
        .map_err(fail)?;
    hub.store.audit(caller.name.clone(), "diffs.suggestion.apply".into(), format!("{} {} v{n}", d.repo, d.name)).await;
    Ok(json!({ "diff": d.name, "version": n, "thread": thread, "state": if resolved { "resolved" } else { "open" } }))
}

/// Says when a run on a diff's or change request's version finishes (`diff_run`): which version,
/// how it ended, its failed legs (with where their failure excerpts are), and whose it is, for
/// whoever follows the diff (an agent that wrote it, its person). And raises the workflow events
/// of a diff's conversation (its comments and reviews). For as long as the server runs.
pub async fn runs_loop(hub: std::sync::Arc<Hub>) -> anyhow::Result<()> {
    use futures::FutureExt as _;
    let mut rx = hub.events.subscribe();
    loop {
        futures::select_biased! {
            () = hub.shutdown.cancelled().fuse() => return Ok(()),
            m = rx.recv().fuse() => match m {
                Ok(text) => {
                    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    // A diff's conversation raises its workflow events (W03-R09).
                    if matches!(v.get("type").and_then(Value::as_str), Some("comment" | "review"))
                        && let Err(e) = super::diffs::conversation(&hub, &v).await
                    {
                        tracing::warn!(diff = %v.get("diff").and_then(serde_json::Value::as_str).unwrap_or_default(), error = %format!("{e:#}"), "a diff's conversation started no workflows");
                    }
                    // CD's apps moving raise kiln's `deploy` (W03-R09, W15-R06).
                    if v.get("type").and_then(Value::as_str) == Some("cd_apps") {
                        super::cd_events::observe(&hub, &v).await;
                    }
                    let finished = v.get("type").and_then(Value::as_str) == Some("run")
                        && v.pointer("/run/status").and_then(Value::as_str).is_some_and(|s| matches!(s, "succeeded" | "failed" | "cancelled" | "timed_out" | "skipped"));
                    if finished
                        && let Some(id) = v.pointer("/run/id").and_then(Value::as_str)
                        && let Err(e) = diff_run(&hub, id).await
                    {
                        tracing::debug!(run = %id, error = %format!("{e:#}"), "a diff's run event");
                    }
                }
                Err(async_broadcast::RecvError::Overflowed(_)) => {}
                Err(async_broadcast::RecvError::Closed) => return Ok(()),
            },
        }
    }
}

async fn diff_run(hub: &Hub, run: &str) -> anyhow::Result<()> {
    let key = run.to_owned();
    #[allow(clippy::type_complexity)]
    let found: Option<(String, String, u32, String, Option<String>, String, String, Option<String>, String)> = hub
        .store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT d.id, CASE d.kind WHEN 'diff' THEN 'D' ELSE 'CR' END || d.number, v.n, d.author, d.author_for, p.name, r.status, r.conclusion, COALESCE(r.run_name, r.pipeline)
                 FROM runs r JOIN repos p ON p.id = r.repo_id JOIN diff_versions v ON v.commit_sha = r.sha JOIN diffs d ON d.id = v.diff_id AND d.repo_id = r.repo_id
                 WHERE r.id = ?1 ORDER BY v.n DESC LIMIT 1",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)),
            )
            .optional()?)
        })
        .await?;
    let Some((_, name, version, author, author_for, repo, status, conclusion, workflow)) = found else { return Ok(()) };
    let key = run.to_owned();
    let failed: Vec<Value> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT id, COALESCE(job_id, key), status FROM jobs WHERE run_id = ?1 AND status IN ('failed', 'timed_out')")?;
            let rows = st.query_map([&key], |r| {
                let id: String = r.get(0)?;
                Ok(json!({ "leg": id, "name": r.get::<_, String>(1)?, "status": r.get::<_, String>(2)?, "failure": format!("/api/ci/legs/{id}/logs/failure") }))
            })?;
            Ok(rows.filter_map(Result::ok).collect())
        })
        .await?;
    hub.emit(&json!({ "type": "diff_run", "repo": repo, "diff": name, "version": version, "run": run, "workflow": workflow, "status": status, "conclusion": conclusion, "failed": failed, "diff_author": author, "diff_author_for": author_for }));
    Ok(())
}
