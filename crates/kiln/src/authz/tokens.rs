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

/// The readers made with `note` that are neither revoked nor expiring within `margin` seconds:
/// each one's id and when it expires.
pub async fn readers(hub: &Hub, note: &str, margin: i64) -> anyhow::Result<Vec<(String, Option<i64>)>> {
    let (note, after) = (note.to_owned(), crate::util::now_unix() + margin);
    hub.store
        .call(move |c| {
            let mut q = c.prepare(
                "SELECT id, expires_at FROM tokens WHERE kind = 'pat' AND revoked_at IS NULL AND json_extract(scopes, '$.reader') = 1 AND json_extract(scopes, '$.note') = ?1 AND (expires_at IS NULL OR expires_at > ?2)",
            )?;
            let rows = q.query_map(params![note, after], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

/// Revokes one reader, made in vain.
pub async fn revoke_reader(hub: &Hub, id: &str) -> anyhow::Result<()> {
    let (id, now) = (id.to_owned(), crate::util::now_unix());
    hub.store
        .call(move |c| {
            c.execute("UPDATE tokens SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL", params![id, now])?;
            Ok(())
        })
        .await
}

/// Revokes the readers made with `note`, but `keep`: those a new one replaced.
pub async fn revoke_readers(hub: &Hub, note: &str, keep: &str) -> anyhow::Result<usize> {
    let (n, k, now) = (note.to_owned(), keep.to_owned(), crate::util::now_unix());
    let revoked = hub
        .store
        .call(move |c| {
            Ok(c.execute(
                "UPDATE tokens SET revoked_at = ?3 WHERE kind = 'pat' AND revoked_at IS NULL AND json_extract(scopes, '$.reader') = 1 AND json_extract(scopes, '$.note') = ?1 AND id != ?2",
                params![n, k, now],
            )?)
        })
        .await?;
    if revoked > 0 {
        hub.store.audit("kiln".into(), "tokens.revoke".into(), format!("{revoked} reader(s) replaced: {note}")).await;
    }
    Ok(revoked)
}

/// An agent token kiln makes for one of its own agent sessions (`agent:<label>`), acting for the
/// user `user` (their id, or `owner` on a kiln nobody else signs in to) with `grants`
/// (`developer@repo:acme/app`; never more than the user may):
/// what such an agent pushes (`/git/`, `refs/for/…`), comments and reviews is the agent's, for its
/// person. It lives `days` (default 30): its id and the token, shown once.
pub async fn mint_agent(hub: &Hub, label: &str, user: &str, grants: &[&str], days: Option<i64>, note: &str) -> anyhow::Result<(String, String)> {
    let label: String = label.trim_start_matches("agent:").chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')).take(64).collect();
    anyhow::ensure!(!label.is_empty(), "an agent token names its agent");
    let parsed: Vec<Grant> = grants.iter().map(|g| Grant::parse(g).map_err(anyhow::Error::msg)).collect::<anyhow::Result<_>>()?;
    anyhow::ensure!(!parsed.is_empty(), "an agent token says what it may do");
    let id = crate::util::new_id();
    let token = format!("{}{id}_{}", super::gate::AGENT_PREFIX, crate::util::random_token());
    let hash = crate::util::sha256_hex(token.as_bytes());
    let scopes = json!({ "grants": parsed, "note": note }).to_string();
    let now = crate::util::now_unix();
    let expires = now + days.filter(|d| *d > 0).unwrap_or(30) * 86_400;
    let behalf = if user == "owner" { "owner".to_owned() } else { format!("user:{}", user.trim_start_matches("user:")) };
    let (tid, principal, whose) = (id.clone(), format!("agent:{label}"), behalf.clone());
    hub.store
        .call(move |c| {
            c.execute("INSERT INTO tokens (id, kind, principal, on_behalf_of, hash, scopes, created_at, expires_at) VALUES (?1, 'agent', ?2, ?3, ?4, ?5, ?6, ?7)", params![tid, principal, behalf, hash, scopes, now, expires])?;
            Ok(())
        })
        .await?;
    hub.store.audit("kiln".into(), "tokens.create".into(), format!("agent {id} agent:{label} for {whose} ({})", grants.join(" "))).await;
    Ok((id, token))
}
