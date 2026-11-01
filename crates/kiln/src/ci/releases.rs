//! Releases by tag, as the server keeps them (docs/ci.md W29-R21): every full version a
//! repository kiln hosts has had, by push or by Release (`ci_released_tags`), so the push policy never
//! lets its name be used again.

use std::collections::HashSet;

use rusqlite::params;

use crate::hub::Hub;

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
