//! Inbound webhooks (docs/ci.md W03-R02, R09): `POST /api/ci/hooks/:id`, signed with HMAC-SHA256
//! over its body by the hook's secret (`X-Kiln-Signature-256: sha256=<hex>`, or GitHub's
//! `X-Hub-Signature-256`), raises kiln's `webhook` in the hook's repository. Its default branch's
//! workflows that take it run, the JSON body as the event (with `kiln.hook`), filtered by their
//! `{ hook, filter }`. A hook's secret is derived from the server's master key and the hook's id,
//! never stored: a new secret is a new hook.

use http::StatusCode;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use super::Fail;
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;
use kiln_ci::trigger::{Event, Source};

/// The largest body a hook takes (GitHub sends at most 25 MB; kiln's hooks are for smaller calls).
const MAX_BODY: usize = 5 << 20;

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// A hook's secret.
fn secret(hub: &Hub, id: &str) -> anyhow::Result<String> {
    super::secrets::derived(hub, &format!("kiln ci inbound hook v1\n{id}"))
}

/// The URL a hook is called at.
fn url(hub: &Hub, id: &str) -> String {
    format!("{}/api/ci/hooks/{id}", hub.cfg.server.public_url.trim_end_matches('/'))
}

/// `POST /api/ci/repos/:repo/hooks` `{ name }`: a new hook, with its secret (shown this once).
pub async fn create(hub: &Hub, repo: &str, body: &Value, by: &str) -> Result<Response, Fail> {
    let r = hub.store.repos().await.map_err(internal)?.into_iter().find(|r| r.name == repo || r.id == repo).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no repository `{repo}`")))?;
    let name = body.get("name").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty()).unwrap_or("hook").to_owned();
    if name.chars().count() > 100 {
        return Err((StatusCode::BAD_REQUEST, "a hook's name is at most 100 characters".into()));
    }
    let id = crate::util::new_id();
    let (i, repo_id, n, b) = (id.clone(), r.id.clone(), name.clone(), by.to_owned());
    hub.store
        .call(move |c| Ok(c.execute("INSERT INTO ci_hooks (id, repo_id, name, created_by, created_at) VALUES (?1, ?2, ?3, ?4, ?5)", params![i, repo_id, n, b, crate::util::now_unix()])?))
        .await
        .map_err(internal)?;
    hub.store.audit(by.to_owned(), "ci.hooks.create".into(), format!("{} {name} ({id})", r.name)).await;
    let secret = secret(hub, &id).map_err(internal)?;
    Ok(srv::json(StatusCode::CREATED, &json!({ "id": id, "name": name, "repo": r.name, "url": url(hub, &id), "secret": secret, "signature": "X-Kiln-Signature-256: sha256=<HMAC-SHA256 of the body, hex>" })))
}

/// `GET /api/ci/repos/:repo/hooks`: its hooks (never their secrets).
pub async fn list(hub: &Hub, repo: &str) -> Result<Response, Fail> {
    let r = hub.store.repos().await.map_err(internal)?.into_iter().find(|r| r.name == repo || r.id == repo).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no repository `{repo}`")))?;
    let repo_id = r.id.clone();
    let rows: Vec<(String, String, String, i64, Option<i64>, i64)> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT id, name, created_by, created_at, last_called_at, calls FROM ci_hooks WHERE repo_id = ?1 ORDER BY created_at")?;
            let rows = st.query_map([&repo_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(internal)?;
    let hooks: Vec<Value> = rows.into_iter().map(|(id, name, by, at, last, calls)| json!({ "id": id, "name": name, "url": url(hub, &id), "created_by": by, "created_at": at, "last_called_at": last, "calls": calls })).collect();
    Ok(srv::json(StatusCode::OK, &json!({ "hooks": hooks })))
}

/// `DELETE /api/ci/repos/:repo/hooks/:id`.
pub async fn delete(hub: &Hub, repo: &str, id: &str, by: &str) -> Result<Response, Fail> {
    let r = hub.store.repos().await.map_err(internal)?.into_iter().find(|r| r.name == repo || r.id == repo).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no repository `{repo}`")))?;
    let (i, repo_id) = (id.to_owned(), r.id.clone());
    let n = hub.store.call(move |c| Ok(c.execute("DELETE FROM ci_hooks WHERE id = ?1 AND repo_id = ?2", params![i, repo_id])?)).await.map_err(internal)?;
    if n == 0 {
        return Err((StatusCode::NOT_FOUND, "no such hook".into()));
    }
    hub.store.audit(by.to_owned(), "ci.hooks.delete".into(), id.to_owned()).await;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

/// Whether `header` (`sha256=<hex>`) signs `body` with `secret`.
fn signed(secret: &str, body: &[u8], header: Option<&str>) -> bool {
    let Some(given) = header.and_then(|h| h.trim().strip_prefix("sha256=")) else { return false };
    let want = crate::util::hmac_hex(secret.as_bytes(), body);
    crate::util::ct_eq(given.to_ascii_lowercase().as_bytes(), want.as_bytes())
}

/// `POST /api/ci/hooks/:id`: a call, checked, as `webhook` (202 `{ runs }`).
pub async fn call(hub: &Hub, id: &str, req: Request) -> Result<Response, Fail> {
    let i = id.to_owned();
    let found: Option<(String, String)> = hub.store.call(move |c| Ok(c.query_row("SELECT repo_id, name FROM ci_hooks WHERE id = ?1", [&i], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)).await.map_err(internal)?;
    // An unknown hook and a bad signature answer alike.
    let refused = || (StatusCode::UNAUTHORIZED, "the call isn't signed by this hook's secret (X-Kiln-Signature-256: sha256=<HMAC-SHA256 of the body>)".to_owned());
    let Some((repo_id, name)) = found else { return Err(refused()) };
    let header = req.headers().get("x-kiln-signature-256").or_else(|| req.headers().get("x-hub-signature-256")).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let body = srv::body_bytes(req.into_body(), MAX_BODY).await.map_err(|e| (StatusCode::PAYLOAD_TOO_LARGE, format!("{e:#}")))?;
    if !signed(&secret(hub, id).map_err(internal)?, &body, header.as_deref()) {
        return Err(refused());
    }
    let mut payload: Value = if body.is_empty() { json!({}) } else { serde_json::from_slice(&body).map_err(|e| (StatusCode::BAD_REQUEST, format!("the body isn't JSON: {e}")))? };
    if !payload.is_object() {
        payload = json!({ "body": payload });
    }
    let (i, now) = (id.to_owned(), crate::util::now_unix());
    let _ = hub.store.call(move |c| Ok(c.execute("UPDATE ci_hooks SET last_called_at = ?2, calls = calls + 1 WHERE id = ?1", params![i, now])?)).await;
    let Some(repo) = hub.store.repos().await.map_err(internal)?.into_iter().find(|r| r.id == repo_id) else { return Err(refused()) };
    if !hub.ci.enabled() {
        return Err((StatusCode::NOT_FOUND, "The CI engine is off ([ci] enabled).".into()));
    }
    let f = super::events::facts(hub, &repo).await;
    let (full_ref, sha) = super::events::named_ref(hub, &repo, &f.default_branch).await.map_err(|r| (r.status, r.message))?;
    let commit = super::load::load(hub, &repo, &sha).await.map_err(internal)?;
    let (message, _) = crate::gitops::commit_info(hub, &repo, &sha).await;
    let action = payload.get("action").and_then(Value::as_str).unwrap_or_default().to_owned();
    if let Some(o) = payload.as_object_mut() {
        o.insert("kiln".into(), json!({ "hook": { "id": id, "name": name } }));
        o.entry("repository").or_insert_with(|| f.repo.payload());
    }
    let actor = format!("hook:{name}");
    let e = Event { name: "webhook".into(), action, repo: repo.name.clone(), git_ref: full_ref, sha, actor: actor.clone(), trusted: true, source: Source::Hook, payload, message: Some(message.clone()), ..Event::default() };
    let mut runs = 0;
    for l in commit.workflows.iter().filter(|l| l.runs && !l.has_errors() && l.workflow.as_ref().is_some_and(|w| w.on.iter().any(|t| t.event == "webhook"))) {
        if super::events::start(hub, &repo, &e, l, &message, &actor, Value::Null).await.map_err(internal)? {
            runs += 1;
        }
    }
    Ok(srv::json(StatusCode::ACCEPTED, &json!({ "runs": runs })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w03_r02_inbound_signatures() {
        let body = br#"{"action":"ping"}"#;
        let good = format!("sha256={}", crate::util::hmac_hex(b"s3cret", body));
        assert!(signed("s3cret", body, Some(&good)));
        assert!(signed("s3cret", body, Some(&good.to_ascii_uppercase().replace("SHA256=", "sha256="))), "hex in either case");
        assert!(!signed("other", body, Some(&good)));
        assert!(!signed("s3cret", b"{}", Some(&good)));
        assert!(!signed("s3cret", body, None));
        assert!(!signed("s3cret", body, Some("sha1=abc")));
    }
}
