//! W29-R25/R26: what the editor and the Pipelines pane say of a workflow's locks. Each reads the
//! locks at the commit already shown, the trunk's trusted policy, and the tag moves this repository
//! was seen to meet; nothing here fetches.

use rusqlite::params;

use super::*;

/// Each written use its locks hold at `commit` (else the trunk's tip), with the commit held, and
/// whether any lock has an entry for the workflow. Locks that don't read hold nothing.
pub(crate) async fn editor_facts(hub: &Hub, repo: &Repo, commit: Option<&str>, path: &str, w: &Workflow) -> (BTreeMap<String, String>, bool) {
    let commit = match commit.filter(|c| kiln_seams::dependencies::commit_valid(c)) {
        Some(c) => c.to_owned(),
        None => match super::super::load::resolve(hub, repo, "HEAD").await {
            Ok(Some(c)) => c,
            _ => return (BTreeMap::new(), false),
        },
    };
    let gh = super::super::load::blob(hub, repo, &commit, github::PATH).await.ok().and_then(|t| github::parse(&t).ok());
    let kl = super::super::load::blob(hub, repo, &commit, kiln_ci::lock::kiln::PATH).await.ok().and_then(|t| kiln_ci::lock::kiln::parse(&t).ok());
    let onboarded = gh.as_ref().is_some_and(|f| f.workflows.contains_key(path)) || kl.as_ref().is_some_and(|f| f.workflows.contains_key(path));
    let mut locked = BTreeMap::new();
    for (u, _, _) in kiln_ci::lock::editor::written(w) {
        let native = kl.as_ref().and_then(|f| kiln_ci::lock::verify_kiln::pin_for(f, path, &u)).map(|(_, g)| g.commit.clone());
        let hosted = gh.as_ref().and_then(|f| github::action_pin(&u).and_then(|pin| verify::pin_for(f, path, &pin).map(|(_, a)| a.commit.clone())));
        if let Some(sha) = native.or(hosted).as_deref().and_then(github::digest) {
            locked.insert(u, sha.to_ascii_lowercase());
        }
    }
    (locked, onboarded)
}

/// The pane's lock facts for Settings: how many of its lockable uses its locks hold at `commit`,
/// the first that none does, the trunk's policy, and the full versions it met moved.
pub(crate) async fn pane_facts(hub: &Hub, repo: &Repo, commit: &str, path: &str, w: &Workflow) -> kiln_ci::pipelines::LockSeen {
    let (locked, onboarded) = editor_facts(hub, repo, Some(commit), path, w).await;
    let lockable: BTreeSet<String> = kiln_ci::lock::editor::written(w).into_iter().map(|(u, _, _)| u).filter(|u| kiln_ci::lock::editor::lockable(u)).collect();
    let held = onboarded.then(|| (lockable.iter().filter(|u| locked.contains_key(*u)).count(), lockable.len()));
    let missing = if onboarded { lockable.iter().find(|u| !locked.contains_key(*u)).cloned() } else { None };
    let policy = super::super::repo_config::lock_policy(hub, repo).await.unwrap_or_default();
    let moved = moved_for(hub, repo, path).await.unwrap_or_else(|e| {
        tracing::warn!(repo = %repo.name, %path, error = %format!("{e:#}"), "its moved tags couldn't be read");
        Vec::new()
    });
    kiln_ci::pipelines::LockSeen { held, missing, require: policy.require, moved_tags: policy.moved_tags, moved }
}

/// The full versions `path` of `consumer` met moved, newest first, less those whose new commit
/// this repository has accepted since (a person's word, landed): `acme/tools@v1.2.3`.
async fn moved_for(hub: &Hub, consumer: &Repo, path: &str) -> anyhow::Result<Vec<String>> {
    let (repo, path) = (consumer.id.clone(), path.to_owned());
    hub.store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT CASE WHEN s.native THEN 'kiln://' ELSE '' END || s.publisher, m.git_ref FROM ci_lock_moves m JOIN ci_lock_sources s ON s.source = m.source
                 WHERE m.consumer_repo_id = ?1 AND m.workflow = ?2
                   AND NOT EXISTS (SELECT 1 FROM ci_lock_acceptances x JOIN ci_lock_approved_pins p ON p.approval_id = x.approval_id AND p.uses_key = x.uses_key
                                   WHERE x.consumer_repo_id = m.consumer_repo_id AND p.git_ref = m.git_ref AND p.commit_sha = m.now_commit)
                 GROUP BY s.source, m.git_ref ORDER BY MAX(m.id) DESC LIMIT 20",
            )?;
            let rows = st.query_map(params![repo, path], |r| Ok(format!("{}@{}", r.get::<_, String>(0)?, r.get::<_, String>(1)?.trim_start_matches("refs/tags/"))))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
}
