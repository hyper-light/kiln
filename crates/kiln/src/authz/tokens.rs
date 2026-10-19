//! Tokens kiln makes for others to use (docs/deploy-api.md §7): a reader of one repository, as
//! Argo CD's read token for a release repository. It reads that repository over git
//! (`git.read@repo:<name>`, which only git hosting honors) and nothing else: no push, no API.

use rusqlite::params;
use serde_json::json;

use super::{Grant, Scope};
use crate::hub::Hub;

/// A reader of `repo` (`owner/name`): its id and the token, shown once. It lives `days` (none:
/// until it's revoked), and is listed, revoked and rotated with the owner's tokens.
pub async fn mint_reader(hub: &Hub, repo: &str, days: Option<i64>, note: &str) -> anyhow::Result<(String, String)> {
    let id = crate::util::new_id();
    let token = format!("{}{id}_{}", super::gate::PAT_PREFIX, crate::util::random_token());
    let hash = crate::util::sha256_hex(token.as_bytes());
    let grant = Grant { what: "git.read".into(), scope: Scope::Repo { repo: repo.to_owned() }, deny: false };
    let scopes = json!({ "grants": [grant], "note": note, "reader": true }).to_string();
    let now = crate::util::now_unix();
    let expires = days.filter(|d| *d > 0).map(|d| now + d * 86_400);
    let tid = id.clone();
    hub.store
        .call(move |c| {
            c.execute("INSERT INTO tokens (id, kind, principal, on_behalf_of, hash, scopes, created_at, expires_at) VALUES (?1, 'pat', 'owner', NULL, ?2, ?3, ?4, ?5)", params![tid, hash, scopes, now, expires])?;
            Ok(())
        })
        .await?;
    hub.store.audit("kiln".into(), "tokens.create".into(), format!("reader {id} of {repo} ({note})")).await;
    Ok((id, token))
}
