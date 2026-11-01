//! Pipelines' drafts as kiln records them (docs/ci.md W29-R10, R11): each workflow or action
//! written and not yet published, by its repository, file and author, with the change it's saved
//! as: a draft diff of a repository kiln hosts, or a host's draft change on its own branch.

use std::sync::Arc;

use http::StatusCode;
use rusqlite::{OptionalExtension as _, params};
use serde::Serialize;
use serde_json::{Value, json};

use super::Fail;
use crate::authz::Caller;
use crate::hub::Hub;
use crate::store::Repo;

/// A draft (one row of `ci_pipeline_drafts`).
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Draft {
    pub id: String,
    pub repo: String,
    pub path: String,
    /// Who wrote it, as diffs name their authors (`user:ada`, `agent:…`).
    pub author: String,
    /// `diff` or `change`.
    pub kind: String,
    /// A host's draft branch (`kiln/draft/<stem>-<id8>`).
    pub branch: Option<String>,
    pub number: Option<u64>,
    /// `D12`, `#5`, `!5`.
    pub name: Option<String>,
    pub url: Option<String>,
    /// The commit kiln saved last, on a host's draft branch.
    pub head: Option<String>,
    /// A diff's `Kiln-Change`: a later save that names it is a new version of the diff.
    pub change: Option<String>,
    pub title: String,
    /// `draft`, `review`, `landed` or `discarded`.
    pub state: String,
    pub created_at: i64,
    pub updated_at: i64,
}

const COLUMNS: &str = "id, repo, path, author, kind, branch, number, name, url, head, change, title, state, created_at, updated_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Draft> {
    Ok(Draft {
        id: r.get(0)?,
        repo: r.get(1)?,
        path: r.get(2)?,
        author: r.get(3)?,
        kind: r.get(4)?,
        branch: r.get(5)?,
        number: r.get::<_, Option<i64>>(6)?.and_then(|n| u64::try_from(n).ok()),
        name: r.get(7)?,
        url: r.get(8)?,
        head: r.get(9)?,
        change: r.get(10)?,
        title: r.get(11)?,
        state: r.get(12)?,
        created_at: r.get(13)?,
        updated_at: r.get(14)?,
    })
}

/// `author`'s draft of `path` in `repo` that's still open (in draft or in review), if there is one:
/// what a later save adds to (W29-R11).
pub async fn open_for(hub: &Hub, repo: &str, path: &str, author: &str) -> anyhow::Result<Option<Draft>> {
    let (repo, path, author) = (repo.to_owned(), path.to_owned(), author.to_owned());
    hub.store
        .call(move |c| {
            let sql = format!("SELECT {COLUMNS} FROM ci_pipeline_drafts WHERE repo = ?1 AND path = ?2 AND author = ?3 AND state IN ('draft', 'review') ORDER BY created_at DESC LIMIT 1");
            Ok(c.query_row(&sql, params![repo, path, author], row).optional()?)
        })
        .await
}

/// The draft `id`, whatever its state.
pub async fn get(hub: &Hub, id: &str) -> anyhow::Result<Option<Draft>> {
    let id = id.to_owned();
    hub.store.call(move |c| Ok(c.query_row(&format!("SELECT {COLUMNS} FROM ci_pipeline_drafts WHERE id = ?1"), params![id], row).optional()?)).await
}

/// The commit a draft's file is at: a kiln diff's latest version, or the head kiln saved last on
/// a host's draft branch; in the server's mirror, fetched when it isn't there.
pub async fn head(hub: &Hub, repo: &Repo, d: &Draft) -> anyhow::Result<String> {
    let head = match d.kind.as_str() {
        "diff" => {
            let name = d.name.clone().ok_or_else(|| anyhow::anyhow!("the draft of {} has no diff", d.path))?;
            let diff = crate::ci::diffs::find(hub, &name).await?.ok_or_else(|| anyhow::anyhow!("no diff {name}"))?;
            crate::ci::diffs::versions(hub, &diff).await?.last().map(|v| v.commit.clone()).ok_or_else(|| anyhow::anyhow!("{name} has no version"))?
        }
        _ => d.head.clone().ok_or_else(|| anyhow::anyhow!("the draft of {} has no head", d.path))?,
    };
    crate::gitops::ensure_commit(hub, repo, &head).await?;
    Ok(head)
}

/// A repository's open drafts (in draft or in review), oldest first.
pub async fn open_in(hub: &Hub, repo: &str) -> anyhow::Result<Vec<Draft>> {
    let repo = repo.to_owned();
    hub.store
        .call(move |c| {
            let sql = format!("SELECT {COLUMNS} FROM ci_pipeline_drafts WHERE repo = ?1 AND state IN ('draft', 'review') ORDER BY created_at, id");
            let mut st = c.prepare(&sql)?;
            let out = st.query_map(params![repo], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(out)
        })
        .await
}

/// Records `d` as it is now: a new draft, or what an existing one has become.
pub async fn put(hub: &Hub, d: &Draft) -> anyhow::Result<()> {
    let d = d.clone();
    hub.store
        .call(move |c| {
            let number = d.number.and_then(|n| i64::try_from(n).ok());
            c.execute(
                &format!(
                    "INSERT INTO ci_pipeline_drafts ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
                     ON CONFLICT(id) DO UPDATE SET branch = excluded.branch, number = excluded.number, name = excluded.name, url = excluded.url, head = excluded.head,
                     change = excluded.change, title = excluded.title, state = excluded.state, updated_at = excluded.updated_at"
                ),
                params![d.id, d.repo, d.path, d.author, d.kind, d.branch, number, d.name, d.url, d.head, d.change, d.title, d.state, d.created_at, d.updated_at],
            )?;
            Ok(())
        })
        .await
}

fn internal(e: &anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// The diff a draft of a repository kiln hosts is.
async fn diff_of(hub: &Hub, d: &Draft) -> Result<crate::ci::diffs::Diff, Fail> {
    let name = d.name.clone().ok_or_else(|| (StatusCode::CONFLICT, format!("the draft of {} has no diff", d.path)))?;
    crate::ci::diffs::find(hub, &name).await.map_err(|e| internal(&e))?.ok_or_else(|| (StatusCode::NOT_FOUND, format!("no diff {name}")))
}

/// A host's draft change: its host, branch, head and number.
async fn on_host(hub: &Hub, repo: &Repo, d: &Draft) -> Result<(crate::githost::GitHost, String, String, u64), Fail> {
    let host = crate::githost::for_repo(hub, repo).await.ok_or_else(|| (StatusCode::CONFLICT, format!("kiln can't reach {}'s host: no App or credentials of it are set up here", repo.name)))?;
    match (d.branch.clone(), d.head.clone(), d.number) {
        (Some(branch), Some(head), Some(number)) => Ok((host, branch, head, number)),
        _ => Err((StatusCode::CONFLICT, format!("the draft of {} has no change on its host", d.path))),
    }
}

/// Records `d`'s new `state`, and audits what `caller` did to it.
async fn moved(hub: &Hub, caller: &Caller, d: &mut Draft, state: &str, what: &str) -> Result<Value, Fail> {
    d.state = state.to_owned();
    d.updated_at = crate::util::now_unix();
    put(hub, d).await.map_err(|e| internal(&e))?;
    hub.store.audit(caller.name.clone(), format!("ci.workflow.draft.{what}"), format!("{} {} {}", d.repo, d.path, d.name.as_deref().unwrap_or("draft"))).await;
    Ok(json!({ "draft": d }))
}

/// Pipelines' Ready (docs/ci.md W29-R12): `caller`'s draft `d` asks for review. kiln's draft diff
/// leaves draft as `ci.diffs.ready` takes it out. On a host, the draft's head is committed again,
/// the same tree, asking to skip nothing, so the host's checks and kiln's run on it; then the
/// change is marked ready. The record says `review`.
pub async fn ready(hub: &Arc<Hub>, caller: &Caller, repo: &Repo, d: &mut Draft) -> Result<Value, Fail> {
    if d.state != "draft" {
        return Err((StatusCode::CONFLICT, format!("{} is in review already (W29-R12)", d.name.as_deref().unwrap_or(&d.path))));
    }
    if d.kind == "diff" {
        let diff = diff_of(hub, d).await?;
        crate::ci::diffs::ready_as(hub, &diff, caller).await?;
    } else {
        let (host, branch, head, number) = on_host(hub, repo, d).await?;
        let message = kiln_ci::drafts::ready_message(&d.title);
        let author = super::workflows::commit_author(caller);
        let sha = host.recommit(hub, repo, &branch, &head, &message, &author, d.path.starts_with(".github/workflows/")).await.map_err(|e| match e {
            crate::githost::CommitError::Moved => (StatusCode::CONFLICT, format!("{branch} moved on {}'s host since kiln saved it: save the draft again", repo.name)),
            crate::githost::CommitError::Other(e) => (StatusCode::BAD_GATEWAY, format!("{}'s host refused the draft's ready commit: {e:#}", repo.name)),
        })?;
        d.head = Some(sha);
        host.ready_change(hub, repo, number).await.map_err(|e| (StatusCode::BAD_GATEWAY, format!("the draft's head is ready, but its change isn't marked so: {e:#}")))?;
    }
    moved(hub, caller, d, "review", "ready").await
}

/// Pipelines' Discard (docs/ci.md W29-R13): `caller`'s draft `d` is gone. kiln's draft diff is
/// abandoned as `ci.diffs.abandon` does; a host's draft change is closed, and its branch deleted.
/// The record says `discarded`, and a save after starts a new draft.
pub async fn discard(hub: &Arc<Hub>, caller: &Caller, repo: &Repo, d: &mut Draft) -> Result<Value, Fail> {
    if d.kind == "diff" {
        let diff = diff_of(hub, d).await?;
        crate::ci::diffs::abandon_as(hub, &diff, caller).await?;
    } else {
        let (host, branch, _, number) = on_host(hub, repo, d).await?;
        let why = format!("Discarded in kiln's Pipelines by {} (docs/ci.md W29-R13).", caller.name);
        host.close_change(hub, repo, number, &why).await.map_err(|e| (StatusCode::BAD_GATEWAY, format!("{}'s host didn't close the draft's change: {e:#}", repo.name)))?;
        host.delete_branch(hub, repo, &branch).await.map_err(|e| (StatusCode::BAD_GATEWAY, format!("the draft's change is closed, but its branch {branch} isn't deleted: {e:#}")))?;
    }
    moved(hub, caller, d, "discarded", "discard").await
}
