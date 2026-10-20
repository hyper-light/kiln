//! Change requests (docs/ci.md W14-R04): a branch reviewed against a trunk as a whole, many
//! commits at once, as a GitHub pull request is. They share the diffs' table (`kind =
//! 'change_request'`, `CR4`, numbered with the diffs so no two share a number), review states,
//! threads, checks and the land queue; a change request's versions are its branch's pushes.
//! Opened by pushing `refs/for/<trunk>/branch/<name>` (`kiln cr open <branch>`) or by
//! `POST /api/repos/:repo/change-requests` for a branch already there; every push of the branch
//! after that is a new version (rebase-only when its combined patch is the same on a new base).

use std::collections::HashMap;
use std::sync::Arc;

use http::StatusCode;
use kiln_ci::review::{self, State};
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use super::Fail;
use super::diffs::{self, Outcome};
use crate::authz::{Caller, Scope};
use crate::gitops::{self, LS_REMOTE_TIMEOUT};
use crate::hub::Hub;
use crate::store::Repo;

/// A branch name a change request may have: a git branch name that isn't a trunk.
pub fn branch_ok(name: &str, trunks: &[String]) -> Result<(), String> {
    let bad = name.is_empty() || name.starts_with(['-', '/', '.']) || name.ends_with(['/', '.']) || name.ends_with(".lock") || name.contains("..") || name.contains("//") || name.contains("@{") || name.chars().any(|c| c.is_control() || " ~^:?*[\\".contains(c));
    if bad {
        return Err(format!("`{name}` isn't a branch name"));
    }
    if trunks.iter().any(|t| t.eq_ignore_ascii_case(name)) {
        return Err(format!("{name} is a trunk: a change request reviews another branch"));
    }
    Ok(())
}

async fn rev(hub: &Hub, path: &str, spec: &str) -> Option<String> {
    gitops::run_git(hub, &["-C", path, "rev-parse", "--verify", "--quiet", spec], LS_REMOTE_TIMEOUT).await.ok().map(|o| String::from_utf8_lossy(&o).trim().to_owned()).filter(|s| !s.is_empty())
}

/// What a change request's version is: its head, its base (where the branch leaves the trunk),
/// its combined patch's id, and the commits it brings (oldest first: subject lines).
struct Shape {
    head: String,
    base: String,
    patch_id: String,
    subjects: Vec<String>,
}

async fn shape(hub: &Hub, path: &str, trunk: &str, head: &str) -> Result<Shape, String> {
    let tip = rev(hub, path, &format!("refs/heads/{trunk}^{{commit}}")).await.ok_or_else(|| format!("{trunk} has no commits yet: push it first"))?;
    let base = gitops::run_git(hub, &["-C", path, "merge-base", &tip, head], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).map_err(|_| format!("the branch shares no history with {trunk}"))?;
    let subjects: Vec<String> = gitops::run_git(hub, &["-C", path, "log", "--reverse", "--format=%s", &format!("{base}..{head}")], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).lines().map(str::to_owned).collect()).unwrap_or_default();
    if subjects.is_empty() {
        return Err(format!("nothing to review: the branch is already on {trunk}"));
    }
    let patch = gitops::run_git(hub, &["-C", path, "diff", "--no-color", &base, head], LS_REMOTE_TIMEOUT).await.map_err(|e| format!("{e:#}"))?;
    let patch_id = diffs::git_with_input(hub, &["-C", path, "patch-id", "--stable"], patch).await.ok().and_then(|o| String::from_utf8_lossy(&o).split_whitespace().next().map(str::to_owned)).unwrap_or_default();
    Ok(Shape { head: head.to_owned(), base, patch_id, subjects })
}

/// Who opens or pushes: their name as rows keep it, and whether an agent.
pub struct By {
    pub pushed_by: String,
    pub author: (String, &'static str),
    /// What its versions' runs are held to ([`crate::authz::Caller::started_by`]).
    pub started_by: Option<serde_json::Value>,
}

/// The open change request of `branch` for `trunk`, if there is one: its id and number.
pub(crate) async fn open_of(hub: &Hub, repo: &str, trunk: &str, branch: &str) -> anyhow::Result<Option<(String, u32)>> {
    let (repo, trunk, branch) = (repo.to_owned(), trunk.to_owned(), branch.to_owned());
    hub.store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT id, number FROM diffs WHERE repo_id = ?1 AND trunk = ?2 AND branch = ?3 AND kind = 'change_request' AND state NOT IN ('landed', 'abandoned') ORDER BY number DESC LIMIT 1",
                params![repo, trunk, branch],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
        })
        .await
}

/// Opens a change request for `branch` (at `head`) against `trunk`, or adds a version to the one
/// that is open: what it did.
#[allow(clippy::too_many_arguments)]
pub async fn submit(hub: &Arc<Hub>, repo: &Repo, path: &str, trunk: &str, branch: &str, head: &str, by: &By, options: &review::Options) -> Result<Outcome, String> {
    let s = shape(hub, path, trunk, head).await?;
    let found = open_of(hub, &repo.id, trunk, branch).await.map_err(|e| format!("{e:#}"))?;
    let title = options.title.clone().unwrap_or_else(|| if s.subjects.len() == 1 { s.subjects.first().cloned().unwrap_or_default() } else { branch.to_owned() });
    let summary = s.subjects.iter().map(|l| format!("- {l}")).collect::<Vec<_>>().join("\n");
    let base_url = hub.cfg.server.public_url.trim_end_matches('/').to_owned();
    let (repo_id, trunk_s, branch_s, pushed_by, author) = (repo.id.clone(), trunk.to_owned(), branch.to_owned(), by.pushed_by.clone(), by.author.clone());
    let (draft, ready, note, topic) = (options.draft, options.ready, options.message.clone(), options.topic.clone());
    let reviewers = options.reviewers.clone();
    let (head_s, base_s, patch_s) = (s.head.clone(), s.base.clone(), s.patch_id.clone());
    let (title_c, summary_c) = (title.clone(), summary.clone());
    let by_text = by.started_by.as_ref().map(serde_json::Value::to_string);
    let made: (String, u32, u32, &'static str, bool, Vec<String>) = hub
        .store
        .call(move |c| {
            let tx = c.transaction()?;
            let now = crate::util::now_unix();
            let (id, number, n, kind, rebase_only) = match found {
                Some((id, number)) => {
                    let last: Option<(u32, String, String, String)> = tx.query_row("SELECT n, commit_sha, base, patch_id FROM diff_versions WHERE diff_id = ?1 ORDER BY n DESC LIMIT 1", [&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?;
                    if let Some((n, commit, _, _)) = &last
                        && *commit == head_s
                    {
                        tx.commit()?;
                        return Ok((id, number, *n, "unchanged", false, Vec::new()));
                    }
                    let rebase_only = last.as_ref().is_some_and(|(_, _, base, patch)| !patch.is_empty() && *patch == patch_s && *base != base_s);
                    let n = last.map_or(1, |(n, ..)| n + 1);
                    tx.execute(
                        "UPDATE diffs SET updated_at = ?2, topic = COALESCE(?3, topic), state = CASE WHEN state = 'changes-requested' OR (state = 'draft' AND ?4) THEN 'needs-review' ELSE state END WHERE id = ?1",
                        params![id, now, topic, ready],
                    )?;
                    (id, number, n, if rebase_only { "rebased" } else { "updated" }, rebase_only)
                }
                None => {
                    let number: u32 = tx.query_row("SELECT COALESCE(MAX(number), 0) + 1 FROM diffs", [], |r| r.get(0))?;
                    let id = crate::util::new_id();
                    let state = if draft { State::Draft } else { State::NeedsReview };
                    tx.execute(
                        "INSERT INTO diffs (id, repo_id, number, kind, change_id, title, summary, author, author_kind, state, trunk, branch, topic, created_at, updated_at, author_for)
                         VALUES (?1, ?2, ?3, 'change_request', NULL, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12, ?13)",
                        params![id, repo_id, number, title_c, summary_c, author.0, author.1, state.as_str(), trunk_s, branch_s, topic, now, diffs::acting_for(&pushed_by)],
                    )?;
                    (id, number, 1, "new", false)
                }
            };
            tx.execute(
                "INSERT INTO diff_versions (diff_id, n, commit_sha, parent, base, patch_id, message, note, rebase_only, pushed_by, created_at, started_by) VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![id, n, head_s, base_s, patch_s, summary_c, note, rebase_only, pushed_by, now, by_text],
            )?;
            // Reviewers asked for the first time: they hear of it (W19-R06).
            let mut asked = Vec::new();
            for r in &reviewers {
                if tx.execute("INSERT OR IGNORE INTO diff_reviewers (diff_id, reviewer, state, added_at) VALUES (?1, ?2, 'not-looked', ?3)", params![id, r, now])? > 0 {
                    asked.push(r.clone());
                }
            }
            tx.commit()?;
            Ok((id, number, n, kind, rebase_only, asked))
        })
        .await
        .map_err(|e| format!("{e:#}"))?;
    let (id, number, n, kind, rebase_only, asked) = made;
    if !asked.is_empty() {
        crate::rt::spawn(super::notify::diff_event(Arc::clone(hub), repo.name.clone(), "review.requested", format!("CR{number}"), title.clone(), trunk.to_owned(), by.author.0.clone(), String::new(), asked));
    }
    if kind != "unchanged" {
        let _ = gitops::run_git(hub, &["-C", path, "update-ref", &format!("refs/kiln/diffs/CR{number}/v{n}"), &s.head], LS_REMOTE_TIMEOUT).await;
        let fresh = diffs::Fresh { number, n, sha: s.head.clone(), parent: s.base.clone(), title: title.clone(), summary: summary.clone(), message: summary.clone(), draft, rebase_only, branch: Some(branch.to_owned()), labels: diffs::labels(hub, &id).await, started_by: by.started_by.clone() };
        let (hub2, repo2, trunk2, author2, base2) = (hub.clone(), repo.clone(), trunk.to_owned(), by.author.clone(), s.base.clone());
        // A job's token (its push, or its `POST /pulls`) starts no run, as on GitHub.
        if !diffs::by_job(&by.pushed_by) {
            crate::rt::spawn(async move {
                if let Err(e) = diffs::checks(&hub2, &repo2, &trunk2, &base2, &author2, &fresh).await {
                    tracing::warn!(cr = fresh.number, version = fresh.n, error = %format!("{e:#}"), "a change request's workflows didn't start");
                }
            });
        }
    }
    hub.store.audit(by.pushed_by.clone(), "change_requests.submit".into(), format!("{} CR{number} v{n} {kind} ({branch} into {trunk})", repo.name)).await;
    let _ = id;
    Ok(Outcome { name: format!("CR{number}"), number, version: n, kind, title, url: format!("{base_url}/diffs/CR{number}") })
}

/// After a push to branches of `repo`: each open change request of a pushed branch gets the new
/// head as a version. What each did.
pub async fn pushed(hub: &Arc<Hub>, repo: &Repo, path: &str, updates: &[(String, String)], by: &By) -> Vec<String> {
    let mut said = Vec::new();
    for (branch, new) in updates {
        let (repo_id, b) = (repo.id.clone(), branch.clone());
        let trunks: Vec<String> = hub
            .store
            .call(move |c| {
                let mut st = c.prepare("SELECT DISTINCT trunk FROM diffs WHERE repo_id = ?1 AND branch = ?2 AND kind = 'change_request' AND state NOT IN ('landed', 'abandoned')")?;
                let rows = st.query_map(params![repo_id, b], |r| r.get::<_, String>(0))?;
                Ok(rows.filter_map(Result::ok).collect())
            })
            .await
            .unwrap_or_default();
        for trunk in trunks {
            match submit(hub, repo, path, &trunk, branch, new, by, &review::Options::default()).await {
                Ok(o) if o.kind != "unchanged" => said.push(format!("CR{} v{} ({})  {}", o.number, o.version, o.kind, o.url)),
                Ok(_) => {}
                Err(e) => said.push(format!("CR for {branch}: {e}")),
            }
        }
    }
    said
}

/// `POST /api/repos/:repo/change-requests` `{ branch, trunk?, title?, draft?, reviewers? }`: a
/// change request for a branch already pushed.
pub async fn create(hub: &Arc<Hub>, repo_key: &str, caller: &Caller, body: &Value) -> Result<Value, Fail> {
    let repos = hub.store.repos().await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let repo = repos.into_iter().find(|r| r.name == repo_key || r.id == repo_key).ok_or((StatusCode::NOT_FOUND, "no such repository".to_owned()))?;
    if !caller.can("diffs.submit", &Scope::Repo { repo: repo.name.clone() }) {
        return Err((StatusCode::FORBIDDEN, "opening a change request needs `diffs.submit`".into()));
    }
    let path = super::hosting::native_path(hub, &repo).ok_or((StatusCode::CONFLICT, "change requests are for repositories kiln hosts".to_owned()))?;
    let p = path.to_string_lossy().into_owned();
    let branch = body.get("branch").and_then(Value::as_str).ok_or((StatusCode::BAD_REQUEST, "`branch`".to_owned()))?.trim_start_matches("refs/heads/").to_owned();
    let head_trunk = gitops::run_git(hub, &["-C", &p, "symbolic-ref", "--short", "HEAD"], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).unwrap_or_else(|_| "main".into());
    let (policy, _) = super::review::policy(hub, &p, &head_trunk).await;
    let trunk = body.get("trunk").and_then(Value::as_str).map_or(head_trunk.clone(), str::to_owned);
    let mut trunks = policy.trunks.clone();
    trunks.push(head_trunk);
    if !trunks.contains(&trunk) {
        return Err((StatusCode::BAD_REQUEST, format!("{trunk} isn't a trunk")));
    }
    branch_ok(&branch, &trunks).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let head = rev(hub, &p, &format!("refs/heads/{branch}^{{commit}}")).await.ok_or((StatusCode::NOT_FOUND, format!("no branch {branch}: push it first")))?;
    let mut options = review::Options { draft: body.get("draft").and_then(Value::as_bool).unwrap_or(false), title: body.get("title").and_then(Value::as_str).map(str::to_owned), ..review::Options::default() };
    options.reviewers = body.get("reviewers").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
    let (name, kind) = diffs::author_of(&caller.name, Some(caller.kind));
    let by = By { pushed_by: caller.name.clone(), author: (name, kind), started_by: caller.started_by() };
    let o = submit(hub, &repo, &p, &trunk, &branch, &head, &by, &options).await.map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    Ok(json!({ "change_request": format!("CR{}", o.number), "version": o.version, "kind": o.kind, "title": o.title, "url": o.url }))
}

/// What a change request's versions bring: the subjects of its latest version's commits.
pub async fn commits(hub: &Hub, path: &str, base: &str, head: &str) -> Vec<HashMap<&'static str, String>> {
    let out = gitops::run_git(hub, &["-C", path, "log", "--reverse", "--format=%H%x1f%an%x1f%s", &format!("{base}..{head}")], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).into_owned()).unwrap_or_default();
    out.lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, '\x1f');
            Some(HashMap::from([("sha", f.next()?.to_owned()), ("author", f.next()?.to_owned()), ("subject", f.next().unwrap_or_default().to_owned())]))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w14_r04_branch_names() {
        let trunks = vec!["main".to_owned()];
        for ok in ["topic", "ada/outputs", "fix-1.2"] {
            assert!(branch_ok(ok, &trunks).is_ok(), "{ok}");
        }
        for no in ["main", "MAIN", "", "-x", "a..b", "a b", "x.lock", "a:b", "/a", "a/"] {
            assert!(branch_ok(no, &trunks).is_err(), "{no}");
        }
    }
}
