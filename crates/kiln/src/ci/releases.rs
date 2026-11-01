//! Releases by tag (docs/ci.md W29-R21): Release (`ci.releases.tag`), on a repository kiln hosts
//! and on GitHub, and every full version a repository kiln hosts has had, by push or by Release
//! (`ci_released_tags`), so the push policy never lets its name be used again.

use std::collections::HashSet;
use std::sync::Arc;

use http::StatusCode;
use rusqlite::params;
use serde_json::{Value, json};

use super::Fail;
use crate::authz::Caller;
use crate::githost::GitHost;
use crate::hub::Hub;
use crate::store::Repo;

/// `POST /api/ci/repos/:repo/releases { sha, tag, major?, confirm?, notes? }` (`ci.releases.tag`)
/// as `caller`, who may land on it: an annotated tag `tag`, a full version, at the commit `sha`
/// names, and the floating tags it's the newest release under moved there (`major`, true unless
/// it's a pre-release). At a commit one of whose own checks didn't pass, it asks again, naming
/// them, unless `confirm`. On GitHub, a release of it too.
pub async fn tag(hub: &Arc<Hub>, caller: &Caller, name: &str, body: &Value) -> Result<Value, Fail> {
    let internal = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    let repo = hub.store.repos().await.map_err(internal)?.into_iter().find(|r| r.name.eq_ignore_ascii_case(name)).ok_or_else(|| (StatusCode::NOT_FOUND, format!("No repository {name}")))?;
    let tag = body.get("tag").and_then(Value::as_str).map(str::trim).unwrap_or_default().to_owned();
    if kiln_ci::releases::kind_of(&tag) != kiln_ci::releases::TagKind::Full {
        return Err((StatusCode::BAD_REQUEST, format!("`{tag}` isn't a full version: a release is vMAJOR.MINOR.PATCH, a pre-release after `-` (W29-R21)")));
    }
    let asked = body.get("sha").and_then(Value::as_str).unwrap_or_default();
    let sha = super::load::resolve(hub, &repo, asked).await.map_err(internal)?.ok_or_else(|| (StatusCode::NOT_FOUND, format!("{} has no commit `{asked}`", repo.name)))?;
    let failed = failed_checks(hub, &repo.id, &sha).await.map_err(internal)?;
    if !failed.is_empty() && body.get("confirm").and_then(Value::as_bool) != Some(true) {
        let short = sha.get(..12).unwrap_or(&sha);
        return Err((StatusCode::CONFLICT, format!("{short}'s own checks didn't all pass ({}): release it with `confirm` to tag it anyway", failed.join(", "))));
    }
    let pre = kiln_ci::pipelines::full_version(&tag).is_some_and(|(.., p)| p.is_some());
    let notes = body.get("notes").and_then(Value::as_str).unwrap_or_default().to_owned();
    let message = if notes.trim().is_empty() { format!("{tag}\n") } else { format!("{tag}\n\n{}\n", notes.trim_end()) };
    let host = crate::githost::for_repo(hub, &repo).await;
    let tags = match &host {
        Some(GitHost::Kiln) => tags_in(hub, &super::hosting::native_path(hub, &repo).unwrap_or_default().to_string_lossy()).await,
        _ => tags_in(hub, &crate::gitops::mirror_path(hub, &repo.name).to_string_lossy()).await,
    };
    if tags.contains(&tag) || released(hub, &repo.id).await.map_err(internal)?.contains(&tag) {
        return Err((StatusCode::CONFLICT, format!("`{tag}` is a release of {} already: a release's name isn't used again", repo.name)));
    }
    let others: Vec<&str> = tags.iter().map(String::as_str).collect();
    let moved = if body.get("major").and_then(Value::as_bool) == Some(false) { Vec::new() } else { kiln_ci::releases::floating_moved(&tag, &others) };
    let url = match host {
        Some(GitHost::Kiln) => on_kiln(hub, &repo, caller, &tag, &sha, &moved, &message, pre).await?,
        Some(GitHost::Github(app)) => Some(app.release(&repo, &tag, &sha, &moved, &message, pre).await.map_err(|e| (StatusCode::BAD_GATEWAY, format!("GitHub didn't make the release: {e:#}")))?),
        _ => return Err((StatusCode::BAD_REQUEST, format!("{} isn't one kiln releases: Release is for repositories kiln hosts and GitHub's (W29-R21)", repo.name))),
    };
    record(hub, &repo.id, &[(tag.clone(), sha.clone())], &caller.name).await.map_err(internal)?;
    // Each consumer that asked for updates gets one (W29-R22), opened as its cooldown says.
    match super::updates::released(hub, &repo.name, &tag).await {
        Ok(0) => {}
        Ok(_) => {
            let hub = Arc::clone(hub);
            crate::rt::spawn(async move { super::updates::sweep(&hub).await });
        }
        Err(e) => tracing::warn!(repo = %repo.name, %tag, error = %format!("{e:#}"), "the release's updates weren't made due"),
    }
    hub.store.audit(caller.name.clone(), "ci.releases.tag".into(), format!("{} {tag} at {sha}, moving {}", repo.name, if moved.is_empty() { "nothing".to_owned() } else { moved.join(", ") })).await;
    Ok(json!({ "repo": repo.name, "tag": tag, "sha": sha, "moved": moved, "url": url, "confirmed": !failed.is_empty() }))
}

/// A repository's tags, as its git has them.
async fn tags_in(hub: &Hub, dir: &str) -> Vec<String> {
    crate::gitops::run_git(hub, &["-C", dir, "for-each-ref", "--format=%(refname:short)", "refs/tags"], crate::gitops::LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).lines().map(str::to_owned).collect()).unwrap_or_default()
}

/// The workflows whose newest run at `sha` didn't pass: its own checks that failed.
async fn failed_checks(hub: &Hub, repo_id: &str, sha: &str) -> anyhow::Result<Vec<String>> {
    let (id, sha) = (repo_id.to_owned(), sha.to_owned());
    hub.store
        .call(move |c| {
            let mut st = c.prepare("SELECT workflow_id, conclusion FROM runs WHERE repo_id = ?1 AND sha = ?2 AND workflow_id IS NOT NULL AND finished_at IS NOT NULL ORDER BY created_at DESC")?;
            let rows = st.query_map(params![id, sha], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?;
            let mut seen = HashSet::new();
            let mut out = Vec::new();
            for row in rows {
                let (workflow, conclusion) = row?;
                if seen.insert(workflow.clone()) && !matches!(conclusion.as_deref(), Some("success" | "skipped" | "neutral")) {
                    out.push(workflow);
                }
            }
            Ok(out)
        })
        .await
}

/// A release in a repository kiln hosts: the annotated tag made in its own git as Release, its
/// floating tags moved, and W14's record of it, as GitHub's facade lists releases.
#[allow(clippy::too_many_arguments)]
async fn on_kiln(hub: &Hub, repo: &Repo, caller: &Caller, tag: &str, sha: &str, moved: &[String], message: &str, pre: bool) -> Result<Option<String>, Fail> {
    let internal = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    let dir = super::hosting::native_path(hub, repo).ok_or_else(|| (StatusCode::NOT_FOUND, format!("{} isn't hosted here", repo.name)))?.to_string_lossy().into_owned();
    let who = super::workflows::commit_author(caller);
    let (name, email) = (format!("user.name={}", who.name), format!("user.email={}", who.email));
    crate::gitops::run_git(hub, &["-c", &name, "-c", &email, "-C", &dir, "tag", "-a", tag, "-m", message, sha], crate::gitops::LS_REMOTE_TIMEOUT).await.map_err(internal)?;
    for f in moved {
        crate::gitops::run_git(hub, &["-C", &dir, "update-ref", &format!("refs/tags/{f}"), sha], crate::gitops::LS_REMOTE_TIMEOUT).await.map_err(internal)?;
    }
    let (id, t, s, by, now) = (repo.id.clone(), tag.to_owned(), sha.to_owned(), caller.name.clone(), crate::util::now_unix());
    hub.store
        .call(move |c| {
            c.execute("INSERT OR IGNORE INTO ci_releases (repo_id, tag_name, target_commitish, name, body, draft, prerelease, author, created_at, published_at) VALUES (?1, ?2, ?3, ?2, '', 0, ?4, ?5, ?6, ?6)", params![id, t, s, pre, by, now])?;
            Ok(())
        })
        .await
        .map_err(internal)?;
    Ok(None)
}

/// The full versions repository `repo_id` has had.
pub async fn released(hub: &Hub, repo_id: &str) -> anyhow::Result<HashSet<String>> {
    let id = repo_id.to_owned();
    hub.store
        .call(move |c| {
            let mut st = c.prepare("SELECT tag FROM ci_released_tags WHERE repo_id = ?1")?;
            let tags = st.query_map([&id], |r| r.get::<_, String>(0))?.collect::<Result<HashSet<_>, _>>()?;
            Ok(tags)
        })
        .await
}

/// Keeps `tags` (each a full version and what it names) as releases of `repo_id`, made by `by`;
/// one kept already stays as it was.
pub async fn record(hub: &Hub, repo_id: &str, tags: &[(String, String)], by: &str) -> anyhow::Result<()> {
    let (id, tags, by, at) = (repo_id.to_owned(), tags.to_vec(), by.to_owned(), crate::util::now_unix());
    hub.store
        .call(move |c| {
            let mut st = c.prepare("INSERT OR IGNORE INTO ci_released_tags (repo_id, tag, sha, at, by) VALUES (?1, ?2, ?3, ?4, ?5)")?;
            for (tag, sha) in &tags {
                st.execute(params![id, tag, sha, at, by])?;
            }
            Ok(())
        })
        .await
}
