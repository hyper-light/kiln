//! Pipelines' drafts as kiln records them (docs/ci.md W29-R10, R11): each workflow or action
//! written and not yet published, by its repository, file and author, with the change it's saved
//! as: a draft diff of a repository kiln hosts, or a host's draft change on its own branch.

use rusqlite::{OptionalExtension as _, params};
use serde::Serialize;

use crate::hub::Hub;

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
