//! W29-R25: a fresh-action request makes a draft with complete upstream code evidence. It
//! does not modify the old run, schedule work, or accept any new source resolution.

use http::StatusCode;
use rusqlite::params;
use serde_json::{Value, json};

use super::*;
use crate::authz::{Caller, Scope};
use crate::ci::{Fail, pipeline_drafts::Draft};

fn refused(why: impl Into<String>) -> Fail { (StatusCode::UNPROCESSABLE_ENTITY, why.into()) }

pub(crate) async fn proposed_files(hub: &Arc<Hub>, caller: &Repo, commit: &str, changed: &[crate::githost::Change], path: &str) -> Result<super::generate::Proposal, String> {
    crate::gitops::ensure_commit(hub, caller, commit).await.map_err(|_| "KCI4051 the draft's exact base is unavailable".to_owned())?;
    let source = crate::ci::dependency_sources::Sources::new(Arc::clone(hub), caller.name.clone());
    let edits = changed.iter().map(|f| (f.path.clone(), f.content.clone())).collect();
    super::generate::propose(hub, &source, caller, commit, edits, path).await
}

/// Evidence is immutable and attached to the exact saved revision. A subsequent editor save
/// has no review evidence until it regenerates that evidence for its own revision.
pub(crate) async fn keep_proposal(hub: &Hub, repo: &Repo, draft: &Draft, base: &str, head: &str, differences: &BTreeMap<String, kiln_seams::dependencies::Difference>) -> Result<Value, Fail> {
    if draft.repo != repo.name || !kiln_seams::dependencies::commit_valid(base) || !kiln_seams::dependencies::commit_valid(head) { return Err(refused("KCI4051 invalid lock proposal revision")); }
    for difference in differences.values() { difference.validate().map_err(|e| refused(format!("KCI4051 {e}")))?; }
    let evidence = serde_json::to_string(differences).map_err(|_| refused("KCI1022 code evidence cannot be encoded"))?;
    if evidence.len() > 24 << 20 { return Err(refused("KCI1022 code evidence exceeds 24 MiB")); }
    let hash = kiln_seams::sha256_hex(evidence.as_bytes());
    let read_head = head.to_owned();
    let pins: Vec<_> = differences.iter().map(|(key, difference)| serde_json::to_string(&difference.after.identity).map(|identity| (key.clone(), identity, difference.after.commit.clone()))).collect::<Result<_, _>>().map_err(|_| refused("KCI1022 source identity cannot be encoded"))?;
    let (id, repo_id, draft_id, base, head, author) = (crate::util::new_id(), repo.id.clone(), draft.id.clone(), base.to_owned(), head.to_owned(), draft.author.clone());
    hub.store.call(move |c| {
        let tx = crate::store::tx(c)?;
        tx.execute("INSERT INTO ci_lock_reviews (id, consumer_repo_id, draft_id, base_sha, commit_sha, evidence, evidence_sha256, proposed_by, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) ON CONFLICT(draft_id, commit_sha) DO NOTHING", params![id, repo_id, draft_id, base, head, evidence, hash, author, crate::util::now_unix()])?;
        let id: String = tx.query_row("SELECT id FROM ci_lock_reviews WHERE draft_id = ?1 AND commit_sha = ?2", params![draft_id, head], |r| r.get(0))?;
        for (key, identity, commit) in pins { tx.execute("INSERT OR IGNORE INTO ci_lock_review_pins (review_id, consumer_repo_id, uses_key, source_identity, commit_sha) VALUES (?1, ?2, ?3, ?4, ?5)", params![id, repo_id, key, identity, commit])?; }
        tx.commit()?;
        Ok(())
    }).await.map_err(|_| refused("the lock draft is saved, but its code evidence could not be recorded"))?;
    proposal_for(hub, repo, &draft.id, &read_head).await?.ok_or_else(|| refused("the saved lock proposal could not be read"))
}

/// W29-R25: marking a draft Ready cannot make its proposed code an executable pin. This
/// guard is scoped to the consumer; the old run and other consumers keep their accepted pins.
pub(super) async fn check_pin(hub: &Hub, repo: &Repo, root: &str, revision: &Revision, commit: &str) -> Result<(), String> {
    let identity = serde_json::to_string(&revision.identity).map_err(|_| "KCI4051 source identity could not be encoded")?;
    let (repo_id, wanted) = (repo.id.clone(), commit.to_ascii_lowercase());
    let pending: bool = hub.store.call(move |c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM ci_lock_review_pins WHERE consumer_repo_id = ?1 AND source_identity = ?2 AND commit_sha = ?3)", params![repo_id, identity, wanted], |r| r.get(0))?)).await.map_err(|_| "KCI4051 lock review state could not be read")?;
    if pending && !super::approvals::allowed(hub, repo, root, &revision.identity, &revision.reference, commit).await? { Err("KCI4055 this dependency pin is proposed in a lock draft; its exact code change needs a person's review".into()) } else { Ok(()) }
}

/// Panes read saved facts only. Opening one never fetches a source or establishes trust.
pub async fn proposal_for(hub: &Hub, repo: &Repo, draft: &str, head: &str) -> Result<Option<Value>, Fail> {
    let (repo, draft, head) = (repo.id.clone(), draft.to_owned(), head.to_owned());
    let row: Option<(String, String, String, String)> = hub.store.call(move |c| Ok(c.query_row("SELECT id, base_sha, evidence, evidence_sha256 FROM ci_lock_reviews WHERE consumer_repo_id = ?1 AND draft_id = ?2 AND commit_sha = ?3", params![repo, draft, head], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?)).await.map_err(|_| refused("lock review evidence could not be read"))?;
    row.map(|(id, base, evidence, hash)| {
        if evidence.len() > 24 << 20 || kiln_seams::sha256_hex(evidence.as_bytes()) != hash { return Err(refused("KCI4051 saved lock review evidence failed its integrity check")); }
        let differences: BTreeMap<String, kiln_seams::dependencies::Difference> = serde_json::from_str(&evidence).map_err(|_| refused("KCI1022 saved lock review evidence does not read"))?;
        for difference in differences.values() { difference.validate().map_err(|e| refused(format!("KCI4051 {e}")))?; }
        Ok(json!({"id": id, "base": base, "evidence_sha256": hash, "differences": differences}))
    }).transpose()
}

pub async fn fresh_draft(hub: &Arc<Hub>, caller: Option<&Caller>, run: &str) -> Result<Option<Value>, Fail> {
    let id = run.to_owned();
    let saved: Option<(String, String, String, String, String)> = hub.store.call(move |c| Ok(c.query_row("SELECT r.repo_id, r.sha, r.status, p.workflow, p.input FROM runs r JOIN ci_plans p ON p.run_id = r.id WHERE r.id = ?1", [&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()?)).await.map_err(|_| refused("the original run could not be read"))?;
    let (repo_id, commit, status, model, input) = saved.ok_or_else(|| (StatusCode::NOT_FOUND, "no such run of the CI engine".to_owned()))?;
    if !crate::store::Status::parse(&status).is_terminal() { return Err((StatusCode::CONFLICT, "only a finished run can be run again".into())); }
    let repo = hub.store.repos().await.map_err(|_| refused("the original repository could not be read"))?.into_iter().find(|r| r.id == repo_id).ok_or_else(|| refused("the original repository is gone"))?;
    let workflow: Workflow = serde_json::from_str(&model).map_err(|_| refused("the original workflow model does not read"))?;
    let input = serde_json::from_str(&input).map_err(|_| refused("the original run input does not read"))?;
    let snapshot = Frozen::saved(&input, &repo.name, &workflow.path, &commit).map_err(refused)?;
    if !super::is_locked(hub, &repo, &commit, &workflow.path, snapshot.as_ref()).await.map_err(refused)? { return Ok(None); }
    let caller = caller.ok_or_else(|| (StatusCode::FORBIDDEN, "a fresh-action lock draft needs an authenticated caller".to_owned()))?;
    let scope = Scope::Repo { repo: repo.name.clone() };
    if !caller.can("workflows.edit", &scope) || !caller.can("diffs.submit", &scope) { return Err((StatusCode::FORBIDDEN, "fresh actions open a lock draft and need `workflows.edit` and `diffs.submit`".into())); }
    let mut out = crate::ci::workflows::fresh_lock_draft(hub, caller, &repo, &commit, &workflow.path).await?;
    if let Some(out) = out.as_object_mut() { out.insert("run".into(), json!(run)); out.insert("proposal".into(), json!("dependency_lock")); }
    Ok(Some(out))
}
