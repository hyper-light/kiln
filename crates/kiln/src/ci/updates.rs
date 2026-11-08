//! Update drafts for consumers (docs/ci.md W29-R22), as the server makes them. A release of a
//! repository (kiln's `ci.releases.tag`, or a new full version a mirror fetches) is due to each
//! repository whose runs used an older one of it and whose `.kiln/config.yaml` asks for updates,
//! once its cooldown has passed (`ci_pending_updates`). A sweep, every minute and after a
//! release, opens each due one as an update draft: its files' `uses:` moved to the release,
//! through `ci.workflows.edit { draft: true }`, in the draft its `group` names.

use std::sync::Arc;

use rusqlite::params;
use serde_json::{Value, json};

use crate::authz::Caller;
use crate::hub::Hub;
use crate::store::Repo;

/// A release of `publisher` (`owner/repo`) at `tag`, a full version: due to each repository that
/// used an older release of it and asked for updates. Where the consumer's Dependabot updates
/// GitHub Actions, only what kiln hosts is kiln's to update. How many came due.
pub async fn released(hub: &Hub, publisher: &str, tag: &str) -> anyhow::Result<usize> {
    // A pre-release is no one's update.
    if !kiln_ci::pipelines::full_version(tag).is_some_and(|(.., pre)| pre.is_none()) {
        return Ok(0);
    }
    let p = publisher.to_ascii_lowercase();
    let used: Vec<(String, String)> = {
        let p = p.clone();
        hub.store
            .call(move |c| {
                let mut st = c.prepare("SELECT DISTINCT repo_id, git_ref FROM ci_uses WHERE publisher = ?1")?;
                let rows = st.query_map([&p], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await?
    };
    let older = |r: &str| kiln_ci::pipelines::full_version(r).is_some() && r != tag && kiln_ci::pipelines::newest_release([r, tag]) == Some(tag);
    let mut consumers: Vec<String> = used.into_iter().filter(|(_, r)| older(r)).map(|(id, _)| id).collect();
    consumers.sort();
    consumers.dedup();
    let hosted = super::hosting::path_of(hub, publisher).is_some_and(|d| d.join("HEAD").exists());
    let repos = hub.store.repos().await?;
    let now = crate::util::now_unix();
    let mut due = 0;
    for id in consumers {
        let Some(repo) = repos.iter().find(|r| r.id == id) else { continue };
        let config = super::repo_config::load(hub, repo).await;
        let updates = config.updates();
        if !updates.on {
            continue;
        }
        let dependabot = super::repo_config::trunk_file(hub, repo, &[".github/dependabot.yml", ".github/dependabot.yaml"]).await.is_some_and(|(_, t)| kiln_ci::updates::dependabot_covers_actions(&String::from_utf8_lossy(&t)));
        let (state, note) = if dependabot && !hosted { ("skipped", Some("Dependabot updates GitHub Actions here")) } else { ("pending", None) };
        let (rid, p, t, at) = (repo.id.clone(), p.clone(), tag.to_owned(), now + updates.cooldown_s);
        let made = hub
            .store
            .call(move |c| Ok(c.execute("INSERT OR IGNORE INTO ci_pending_updates (repo_id, publisher, tag, due_at, state, note, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)", params![rid, p, t, at, state, note, now])?))
            .await?;
        due += usize::from(made > 0 && state == "pending");
    }
    Ok(due)
}

/// Opens each update whose time has come: every minute, and after a release. A failure is
/// logged, and the update is tried again next time.
pub async fn sweep(hub: &Arc<Hub>) {
    let now = crate::util::now_unix();
    let due: Vec<(String, String, String, i64)> = match hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT repo_id, publisher, tag, due_at FROM ci_pending_updates WHERE state = 'pending' AND due_at <= ?1 ORDER BY due_at LIMIT 50")?;
            let rows = st.query_map([now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "the updates due couldn't be read");
            return;
        }
    };
    let Ok(repos) = hub.store.repos().await else { return };
    for (id, publisher, tag, due_at) in due {
        let Some(repo) = repos.iter().find(|r| r.id == id) else { continue };
        let (state, note) = match open(hub, repo, &publisher, &tag, due_at).await {
            Ok(drafts) if drafts.is_empty() => ("skipped", "nothing of it is used on the default branch now".to_owned()),
            Ok(drafts) => ("opened", drafts.join(", ")),
            Err(e) => {
                tracing::warn!(repo = %repo.name, %publisher, %tag, error = %format!("{e:#}"), "an update draft didn't open; it's tried again");
                continue;
            }
        };
        let (rid, p, t) = (id.clone(), publisher.clone(), tag.clone());
        if let Err(e) = hub.store.call(move |c| Ok(c.execute("UPDATE ci_pending_updates SET state = ?4, note = ?5 WHERE repo_id = ?1 AND publisher = ?2 AND tag = ?3", params![rid, p, t, state, note])?)).await {
            tracing::warn!(repo = %repo.name, error = %format!("{e:#}"), "an update's state wasn't kept");
        }
    }
}

/// One update opened in `repo`: each of its workflows and actions on its default branch whose
/// `uses:` of `publisher` it moves to `tag`, saved as a draft of kiln's updates, in the draft
/// the consumer's group names. The drafts it went in.
async fn open(hub: &Arc<Hub>, repo: &Repo, publisher: &str, tag: &str, due_at: i64) -> anyhow::Result<Vec<String>> {
    let updates = super::repo_config::load(hub, repo).await.updates();
    let bucket = kiln_ci::updates::bucket(updates.group, publisher, due_at);
    let updater = Caller::with(crate::authz::gate::Kind::Owner, &format!("{}{bucket}", kiln_ci::updates::AUTHOR), None, None);
    let branch = repo.default_branch.clone().or_else(|| repo.branches.first().cloned()).unwrap_or_else(|| "main".to_owned());
    let Some(sha) = super::load::resolve(hub, repo, &branch).await? else { return Ok(Vec::new()) };
    let commit = super::load::load(hub, repo, &sha).await?;
    let paths: Vec<String> = commit.workflows.iter().map(|l| l.path.clone()).chain(commit.actions.iter().cloned()).collect();
    let mut drafts = Vec::new();
    for path in paths {
        // The draft's own text, where kiln's updates have one open: it gathers them.
        let at = match super::pipeline_drafts::open_for(hub, &repo.name, &path, &updater.name).await? {
            Some(d) => super::pipeline_drafts::head(hub, repo, &d).await?,
            None => sha.clone(),
        };
        let text = super::load::blob(hub, repo, &at, &path).await.unwrap_or_default();
        let (moved, from) = kiln_ci::updates::moved_uses(&text, publisher, tag);
        if from.is_empty() {
            continue;
        }
        let title = format!("Update {publisher} to {tag}");
        let ask = json!({ "content": moved, "draft": true, "title": title });
        let saved: Value = super::workflows::edit(hub, &updater, &repo.name, &path, &ask).await.map_err(|(_, e)| anyhow::anyhow!("{path}: {e}"))?;
        drafts.push(saved.pointer("/draft/id").and_then(Value::as_str).unwrap_or(&path).to_owned());
    }
    Ok(drafts)
}
