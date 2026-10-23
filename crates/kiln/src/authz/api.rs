//! The identity API (W10): users and their sign-in links, grants, custom roles, and personal
//! access and agent tokens. Tokens are shown once, stored as their SHA-256, and do at most what
//! both their own grant and their user allow.

use std::collections::HashMap;

use http::{Method, StatusCode};
use rusqlite::{OptionalExtension as _, params};
use serde::Deserialize;
use serde_json::{Value, json};

use super::gate::{AGENT_PREFIX, Caller, Kind, PAT_PREFIX, token_grants};
use super::{CAPABILITIES, Grant, ROLES, Scope, role};
use crate::ci::Fail;
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

const JSON_LIMIT: usize = 1 << 20;
/// A personal access token's longest life (W10-R03).
pub const PAT_MAX_DAYS: i64 = 366;
/// An agent token's life; rotating it starts another.
pub const AGENT_TTL: i64 = 3_600;

fn bad(m: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, m.into())
}

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn forbidden(m: impl Into<String>) -> Fail {
    (StatusCode::FORBIDDEN, m.into())
}

fn body<T: serde::de::DeserializeOwned>(v: &Value) -> Result<T, Fail> {
    T::deserialize(v).map_err(|e| bad(format!("{e}")))
}

/// The identity routes, or `None` for a path that isn't one.
pub async fn route(hub: &Hub, caller: &Caller, method: &Method, s: &[&str], q: &HashMap<String, String>, req: Request) -> Option<Result<Response, Fail>> {
    let v: Value = if matches!(*method, Method::POST | Method::PUT) {
        match srv::body_json(req.into_body(), JSON_LIMIT).await {
            Ok(v) => v,
            Err(e) => return Some(Err(bad(format!("{e:#}")))),
        }
    } else {
        Value::Null
    };
    call(hub, caller, method, s, q, &v).await
}

/// The same with the request's JSON already read: what kiln's operations call (`users.*`,
/// `grants.*`, `roles.*`, `tokens.*`; docs/ci.md W20-AC01).
pub async fn call(hub: &Hub, caller: &Caller, method: &Method, s: &[&str], q: &HashMap<String, String>, v: &Value) -> Option<Result<Response, Fail>> {
    Some(match (method, s) {
        (&Method::GET, ["api", "roles"]) => roles(hub).await,
        (&Method::PUT, ["api", "roles", name]) => put_role(hub, caller, name, v).await,
        (&Method::DELETE, ["api", "roles", name]) => delete_role(hub, caller, name).await,
        (&Method::GET, ["api", "users"]) => users(hub).await,
        (&Method::POST, ["api", "users"]) => add_user(hub, caller, v).await,
        (&Method::DELETE, ["api", "users", id]) => disable_user(hub, caller, id).await,
        (&Method::POST, ["api", "users", id, "link"]) => link(hub, caller, id).await,
        (&Method::POST, ["api", "users", id, "sign-out"]) => sign_out(hub, caller, id).await,
        (&Method::GET, ["api", "grants"]) => grants(hub, q).await,
        (&Method::POST, ["api", "grants"]) => add_grant(hub, caller, v).await,
        (&Method::DELETE, ["api", "grants", id]) => delete_grant(hub, caller, id).await,
        (&Method::GET, ["api", "tokens"]) => tokens(hub, caller).await,
        (&Method::POST, ["api", "tokens"]) => mint(hub, caller, v).await,
        (&Method::DELETE, ["api", "tokens", id]) => revoke(hub, caller, id).await,
        (&Method::POST, ["api", "tokens", id, "rotate"]) => rotate(hub, caller, id).await,
        _ => return None,
    })
}

// ------------------------------------------------------------------ roles

async fn custom(hub: &Hub) -> Result<HashMap<String, Vec<String>>, Fail> {
    hub.store
        .call(|c| {
            let mut st = c.prepare("SELECT name, capabilities FROM roles ORDER BY name")?;
            let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            Ok(rows.filter_map(Result::ok).map(|(n, caps)| (n, serde_json::from_str(&caps).unwrap_or_default())).collect())
        })
        .await
        .map_err(internal)
}

async fn roles(hub: &Hub) -> Result<Response, Fail> {
    let mut out: Vec<Value> = ROLES.iter().map(|n| json!({ "name": n, "builtin": true, "capabilities": role(n).unwrap_or_default() })).collect();
    let mut custom: Vec<(String, Vec<String>)> = custom(hub).await?.into_iter().collect();
    custom.sort();
    out.extend(custom.into_iter().map(|(n, caps)| json!({ "name": n, "builtin": false, "capabilities": caps })));
    Ok(srv::json(StatusCode::OK, &json!({ "roles": out, "capabilities": CAPABILITIES })))
}

#[derive(Deserialize)]
struct RoleBody {
    capabilities: Vec<String>,
}

async fn put_role(hub: &Hub, caller: &Caller, name: &str, v: &Value) -> Result<Response, Fail> {
    if role(name).is_some() || CAPABILITIES.contains(&name) {
        return Err(bad(format!("`{name}` is built in")));
    }
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(bad("a role's name is letters, digits, `-` and `_`"));
    }
    let b: RoleBody = body(v)?;
    if let Some(unknown) = b.capabilities.iter().find(|c| !super::capability(c)) {
        return Err(bad(format!("`{unknown}` is not a capability")));
    }
    let (n, caps) = (name.to_owned(), serde_json::to_string(&b.capabilities).unwrap_or_default());
    hub.store
        .call(move |c| {
            c.execute("INSERT INTO roles (name, capabilities, created_at) VALUES (?1, ?2, ?3) ON CONFLICT(name) DO UPDATE SET capabilities = ?2", params![n, caps, crate::util::now_unix()])?;
            Ok(())
        })
        .await
        .map_err(internal)?;
    hub.store.audit(caller.name.clone(), "roles.put".into(), format!("{name}: {}", b.capabilities.join(", "))).await;
    Ok(srv::json(StatusCode::OK, &json!({ "name": name, "capabilities": b.capabilities })))
}

async fn delete_role(hub: &Hub, caller: &Caller, name: &str) -> Result<Response, Fail> {
    let n = name.to_owned();
    let gone = hub.store.call(move |c| Ok(c.execute("DELETE FROM roles WHERE name = ?1", [&n])?)).await.map_err(internal)?;
    if gone == 0 {
        return Err((StatusCode::NOT_FOUND, format!("no custom role `{name}`")));
    }
    hub.store.audit(caller.name.clone(), "roles.delete".into(), name.to_owned()).await;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

// ------------------------------------------------------------------ users

/// A user by id or name: `(id, name)`.
async fn find_user(hub: &Hub, key: &str) -> Result<(String, String), Fail> {
    let k = key.trim_start_matches("user:").to_owned();
    hub.store
        .call(move |c| Ok(c.query_row("SELECT id, name FROM users WHERE (id = ?1 OR name = ?1 COLLATE NOCASE) AND disabled_at IS NULL", [&k], |r| Ok((r.get(0)?, r.get(1)?))).optional()?))
        .await
        .map_err(internal)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("no user `{key}`")))
}

async fn users(hub: &Hub) -> Result<Response, Fail> {
    let rows = hub
        .store
        .call(|c| {
            let mut st = c.prepare(
                "SELECT u.id, u.name, u.created_at, u.created_by, u.link_expires_at,
                        (SELECT json_group_array(CASE WHEN g.deny THEN '!' ELSE '' END || g.what || '@' || g.scope) FROM grants g WHERE g.principal = 'user:' || u.id)
                 FROM users u WHERE u.disabled_at IS NULL ORDER BY u.name",
            )?;
            let rows = st.query_map([], |r| {
                let grants: String = r.get(5)?;
                Ok(json!({
                    "id": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?, "created_at": r.get::<_, i64>(2)?, "created_by": r.get::<_, String>(3)?,
                    "link_expires_at": r.get::<_, Option<i64>>(4)?, "grants": serde_json::from_str::<Value>(&grants).unwrap_or(json!([])),
                }))
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(internal)?;
    Ok(srv::json(StatusCode::OK, &json!({ "users": rows })))
}

#[derive(Deserialize)]
struct NewUser {
    name: String,
    /// Grants to start with (`developer@org:acme`, …).
    #[serde(default)]
    grants: Vec<String>,
    /// The email SSO matches them by (a host's verified one, docs/ci.md W23-R08).
    #[serde(default)]
    email: Option<String>,
}

fn parse_grants(texts: &[String], custom: &HashMap<String, Vec<String>>) -> Result<Vec<Grant>, Fail> {
    texts
        .iter()
        .map(|t| {
            let g = Grant::parse(t).map_err(bad)?;
            if super::known(&g.what, custom) { Ok(g) } else { Err(bad(format!("`{}` is neither a role nor a capability", g.what))) }
        })
        .collect()
}

async fn add_user(hub: &Hub, caller: &Caller, v: &Value) -> Result<Response, Fail> {
    let b: NewUser = body(v)?;
    let name = b.name.trim().to_owned();
    if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.@".contains(c)) || name == "admin" || name == "owner" {
        return Err(bad("a user's name is up to 64 letters, digits, `-`, `_`, `.` and `@` (and not admin or owner)"));
    }
    let grants = parse_grants(&b.grants, &custom(hub).await?)?;
    let (id, by, n, email) = (crate::util::new_id(), caller.name.clone(), name.clone(), b.email.clone().map(|e| e.trim().to_owned()).filter(|e| e.contains('@')));
    let uid = id.clone();
    let texts: Vec<(String, String, bool)> = grants.iter().map(|g| (g.what.clone(), g.scope.text(), g.deny)).collect();
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let now = crate::util::now_unix();
            let taken: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM users WHERE name = ?1 COLLATE NOCASE)", [&n], |r| r.get(0))?;
            if taken {
                anyhow::bail!("conflict: a user `{n}` exists (or did: names are not reused)");
            }
            tx.execute("INSERT INTO users (id, name, created_at, created_by, email) VALUES (?1, ?2, ?3, ?4, ?5)", params![uid, n, now, by, email])?;
            for (what, scope, deny) in texts {
                tx.execute("INSERT INTO grants (principal, what, scope, deny, created_at, created_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![format!("user:{uid}"), what, scope, deny, now, by])?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
        .map_err(|e| if format!("{e}").starts_with("conflict: ") { (StatusCode::CONFLICT, format!("{e}").replacen("conflict: ", "", 1)) } else { internal(e) })?;
    hub.store.audit(caller.name.clone(), "users.add".into(), format!("{name} {}", b.grants.join(" "))).await;
    Ok(srv::json(StatusCode::CREATED, &json!({ "id": id, "name": name, "grants": grants.iter().map(Grant::text).collect::<Vec<_>>() })))
}

/// Disables a user: their sessions end, their tokens are revoked, their grants stay for the
/// record. Names are not reused.
async fn disable_user(hub: &Hub, caller: &Caller, key: &str) -> Result<Response, Fail> {
    let (id, name) = find_user(hub, key).await?;
    let uid = id.clone();
    hub.store
        .call(move |c| {
            let now = crate::util::now_unix();
            let p = format!("user:{uid}");
            c.execute("UPDATE users SET disabled_at = ?2, session_epoch = session_epoch + 1, link_hash = NULL WHERE id = ?1", params![uid, now])?;
            c.execute("UPDATE tokens SET revoked_at = ?2 WHERE (principal = ?1 OR on_behalf_of = ?1) AND revoked_at IS NULL", params![p, now])?;
            Ok(())
        })
        .await
        .map_err(internal)?;
    hub.store.audit(caller.name.clone(), "users.disable".into(), name).await;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

/// A one-time sign-in link for a user (only the newest works), to hand them.
async fn link(hub: &Hub, caller: &Caller, key: &str) -> Result<Response, Fail> {
    let (id, name) = find_user(hub, key).await?;
    let code = super::gate::new_link(hub, &id).await.map_err(internal)?.ok_or_else(|| (StatusCode::NOT_FOUND, format!("no user `{key}`")))?;
    let url = format!("{}/api/auth/otp?code={code}", hub.cfg.server.public_url.trim_end_matches('/'));
    hub.store.audit(caller.name.clone(), "users.link".into(), name.clone()).await;
    Ok(srv::json(StatusCode::OK, &json!({ "user": name, "url": url, "expires_at": crate::util::now_unix() + super::gate::LINK_TTL })))
}

async fn sign_out(hub: &Hub, caller: &Caller, key: &str) -> Result<Response, Fail> {
    let (id, name) = find_user(hub, key).await?;
    hub.store.call(move |c| Ok(c.execute("UPDATE users SET session_epoch = session_epoch + 1 WHERE id = ?1", [&id])?)).await.map_err(internal)?;
    hub.store.audit(caller.name.clone(), "users.sign_out".into(), name).await;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

// ------------------------------------------------------------------ grants

async fn grants(hub: &Hub, q: &HashMap<String, String>) -> Result<Response, Fail> {
    let user = match q.get("user").filter(|u| !u.is_empty()) {
        Some(u) => Some(find_user(hub, u).await?.0),
        None => None,
    };
    let rows = hub
        .store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT g.id, g.principal, u.name, g.what, g.scope, g.deny, g.created_at, g.created_by FROM grants g JOIN users u ON g.principal = 'user:' || u.id
                 WHERE u.disabled_at IS NULL AND (?1 IS NULL OR u.id = ?1) ORDER BY u.name, g.id",
            )?;
            let rows = st.query_map([user], |r| {
                Ok(json!({
                    "id": r.get::<_, i64>(0)?, "principal": r.get::<_, String>(1)?, "user": r.get::<_, String>(2)?, "what": r.get::<_, String>(3)?,
                    "scope": r.get::<_, String>(4)?, "deny": r.get::<_, bool>(5)?, "created_at": r.get::<_, i64>(6)?, "created_by": r.get::<_, String>(7)?,
                }))
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(internal)?;
    Ok(srv::json(StatusCode::OK, &json!({ "grants": rows })))
}

#[derive(Deserialize)]
struct NewGrant {
    user: String,
    /// `developer@org:acme`, `!runs.cancel@repo:acme/app`, …
    grant: String,
}

async fn add_grant(hub: &Hub, caller: &Caller, v: &Value) -> Result<Response, Fail> {
    let b: NewGrant = body(v)?;
    let (uid, name) = find_user(hub, &b.user).await?;
    let g = parse_grants(std::slice::from_ref(&b.grant), &custom(hub).await?)?.into_iter().next().ok_or_else(|| bad("no grant"))?;
    let (what, scope, deny, by) = (g.what.clone(), g.scope.text(), g.deny, caller.name.clone());
    let id = hub
        .store
        .call(move |c| {
            c.execute(
                "INSERT INTO grants (principal, what, scope, deny, created_at, created_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![format!("user:{uid}"), what, scope, deny, crate::util::now_unix(), by],
            )?;
            Ok(c.last_insert_rowid())
        })
        .await
        .map_err(internal)?;
    hub.store.audit(caller.name.clone(), "grants.add".into(), format!("{name} {}", g.text())).await;
    Ok(srv::json(StatusCode::CREATED, &json!({ "id": id, "user": name, "grant": g.text() })))
}

async fn delete_grant(hub: &Hub, caller: &Caller, id: &str) -> Result<Response, Fail> {
    let id: i64 = id.parse().map_err(|_| bad("a grant's id is a number"))?;
    let gone = hub.store.call(move |c| Ok(c.execute("DELETE FROM grants WHERE id = ?1", [id])?)).await.map_err(internal)?;
    if gone == 0 {
        return Err((StatusCode::NOT_FOUND, "no such grant".into()));
    }
    hub.store.audit(caller.name.clone(), "grants.delete".into(), id.to_string()).await;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

// ------------------------------------------------------------------ tokens

/// Whose tokens a caller acts for: `user:<id>`, or `owner`.
fn whose(caller: &Caller) -> Option<String> {
    match caller.kind {
        Kind::Owner => Some("owner".into()),
        Kind::Job => None,
        _ => Some(caller.user.as_ref().map_or_else(|| "owner".to_owned(), |u| format!("user:{u}"))),
    }
}

fn manages(caller: &Caller) -> bool {
    caller.can("tokens.manage", &Scope::Server)
}

fn token_json(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let (id, kind, principal, behalf, scopes): (String, String, String, Option<String>, String) = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?);
    let note = serde_json::from_str::<Value>(&scopes).ok().and_then(|v| v.get("note").and_then(Value::as_str).map(str::to_owned));
    let prefix = if kind == "agent" { AGENT_PREFIX } else { PAT_PREFIX };
    Ok(json!({
        "id": id, "kind": kind, "principal": principal, "on_behalf_of": behalf, "note": note,
        "grants": token_grants(&scopes).iter().map(Grant::text).collect::<Vec<_>>(),
        // What a log or a scanner shows of it: the prefix and id, never the secret.
        "masked": format!("{prefix}{id}_…"),
        "created_at": r.get::<_, i64>(5)?, "expires_at": r.get::<_, Option<i64>>(6)?, "last_used_at": r.get::<_, Option<i64>>(7)?, "revoked_at": r.get::<_, Option<i64>>(8)?,
    }))
}

const TOKEN_COLUMNS: &str = "id, kind, principal, on_behalf_of, scopes, created_at, expires_at, last_used_at, revoked_at";

async fn tokens(hub: &Hub, caller: &Caller) -> Result<Response, Fail> {
    let session = matches!(caller.kind, Kind::Owner | Kind::User);
    let mine = if manages(caller) { None } else { Some(whose(caller).ok_or_else(|| forbidden("a job lists no tokens"))?) };
    // A token without tokens.manage sees only itself.
    let only = (!session && mine.is_some()).then(|| caller.token.clone().unwrap_or_default());
    let rows = hub
        .store
        .call(move |c| {
            let mut st = c.prepare(&format!(
                "SELECT {TOKEN_COLUMNS} FROM tokens WHERE kind IN ('pat', 'agent') AND (?1 IS NULL OR principal = ?1 OR on_behalf_of = ?1) AND (?2 IS NULL OR id = ?2)
                 ORDER BY created_at DESC LIMIT 500"
            ))?;
            let rows = st.query_map(params![mine, only], token_json)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(internal)?;
    Ok(srv::json(StatusCode::OK, &json!({ "tokens": rows })))
}

#[derive(Deserialize)]
struct NewToken {
    /// `pat` (default) or `agent`.
    #[serde(default)]
    kind: Option<String>,
    /// What it is for, shown in lists.
    #[serde(default)]
    note: Option<String>,
    /// An agent token's agent (`agent:<label>`).
    #[serde(default)]
    agent: Option<String>,
    /// Whom it is for, when not the caller (needs `tokens.manage`): a user's name or id, or `owner`.
    #[serde(default)]
    user: Option<String>,
    /// What it may do (at most what its user may): `developer@repo:acme/app`, …
    grants: Vec<String>,
    /// A personal access token's life in days (default 90, at most 366).
    #[serde(default)]
    expires_in_days: Option<i64>,
}

async fn mint(hub: &Hub, caller: &Caller, v: &Value) -> Result<Response, Fail> {
    let b: NewToken = body(v)?;
    let agent = match b.kind.as_deref().unwrap_or("pat") {
        "pat" => false,
        "agent" => true,
        k => return Err(bad(format!("`{k}`: a token is a `pat` or an `agent`"))),
    };
    // Minting from a session, or with tokens.manage: a token doesn't mint itself more tokens.
    let session = matches!(caller.kind, Kind::Owner | Kind::User);
    if !session && !manages(caller) {
        return Err(forbidden("tokens are minted from a signed-in session, or with tokens.manage"));
    }
    let principal = match b.user.as_deref().filter(|u| !u.is_empty()) {
        None => whose(caller).ok_or_else(|| forbidden("a job mints no tokens"))?,
        Some(u) => {
            if !manages(caller) {
                return Err(forbidden("minting for someone else needs tokens.manage"));
            }
            if u == "owner" { "owner".to_owned() } else { format!("user:{}", find_user(hub, u).await?.0) }
        }
    };
    if b.grants.is_empty() {
        return Err(bad("say what the token may do: grants such as `developer@repo:acme/app`, `viewer`, or `owner` for everything its user may"));
    }
    let grants = parse_grants(&b.grants, &custom(hub).await?)?;
    let now = crate::util::now_unix();
    let expires_at = if agent {
        now + AGENT_TTL
    } else {
        let days = b.expires_in_days.unwrap_or(90);
        if !(1..=PAT_MAX_DAYS).contains(&days) {
            return Err(bad(format!("a personal access token lives 1 to {PAT_MAX_DAYS} days")));
        }
        now + days * 86_400
    };
    let (owner_col, behalf) = if agent {
        let label = b.agent.as_deref().map(|a| a.trim_start_matches("agent:").to_owned()).filter(|a| !a.is_empty()).ok_or_else(|| bad("an agent token names its agent (`agent`)"))?;
        (format!("agent:{label}"), Some(principal.clone()))
    } else {
        (principal.clone(), None)
    };
    let id = crate::util::new_id();
    let token = format!("{}{id}_{}", if agent { AGENT_PREFIX } else { PAT_PREFIX }, crate::util::random_token());
    let hash = crate::util::sha256_hex(token.as_bytes());
    let scopes = json!({ "grants": grants, "note": b.note }).to_string();
    let (tid, kind, oc, bh) = (id.clone(), if agent { "agent" } else { "pat" }, owner_col.clone(), behalf.clone());
    hub.store
        .call(move |c| {
            c.execute(
                "INSERT INTO tokens (id, kind, principal, on_behalf_of, hash, scopes, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![tid, kind, oc, bh, hash, scopes, now, expires_at],
            )?;
            Ok(())
        })
        .await
        .map_err(internal)?;
    let shown: Vec<String> = grants.iter().map(Grant::text).collect();
    hub.store.audit(caller.name.clone(), "tokens.create".into(), format!("{kind} {id} for {} ({})", behalf.as_deref().unwrap_or(&owner_col), shown.join(" "))).await;
    Ok(srv::json(StatusCode::CREATED, &json!({ "id": id, "kind": kind, "token": token, "principal": owner_col, "on_behalf_of": behalf, "grants": shown, "expires_at": expires_at })))
}

/// A token's owner columns, if the caller may act on it.
async fn may_touch(hub: &Hub, caller: &Caller, id: &str) -> Result<(String, Option<String>), Fail> {
    let tid = id.to_owned();
    let row: Option<(String, String, Option<String>)> = hub
        .store
        .call(move |c| Ok(c.query_row("SELECT kind, principal, on_behalf_of FROM tokens WHERE id = ?1 AND kind IN ('pat', 'agent')", [&tid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?))
        .await
        .map_err(internal)?;
    let Some((_, principal, behalf)) = row else { return Err((StatusCode::NOT_FOUND, "no such token".into())) };
    // A token acts on itself; its user's other tokens are theirs to manage from a session.
    let session = matches!(caller.kind, Kind::Owner | Kind::User);
    let own = session && whose(caller).is_some_and(|w| w == principal || behalf.as_deref() == Some(&w));
    if own || manages(caller) || caller.token.as_deref() == Some(id) {
        Ok((principal, behalf))
    } else {
        Err((StatusCode::NOT_FOUND, "no such token".into()))
    }
}

async fn revoke(hub: &Hub, caller: &Caller, id: &str) -> Result<Response, Fail> {
    may_touch(hub, caller, id).await?;
    let tid = id.to_owned();
    hub.store.call(move |c| Ok(c.execute("UPDATE tokens SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL", params![tid, crate::util::now_unix()])?)).await.map_err(internal)?;
    hub.store.audit(caller.name.clone(), "tokens.revoke".into(), id.to_owned()).await;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

/// A new secret for a live agent token, good for another hour (W10-R03: agents' tokens rotate
/// hourly while they live); the old secret stops working at once.
async fn rotate(hub: &Hub, caller: &Caller, id: &str) -> Result<Response, Fail> {
    may_touch(hub, caller, id).await?;
    let token = format!("{AGENT_PREFIX}{id}_{}", crate::util::random_token());
    let (tid, hash) = (id.to_owned(), crate::util::sha256_hex(token.as_bytes()));
    let expires_at = crate::util::now_unix() + AGENT_TTL;
    let n = hub
        .store
        .call(move |c| {
            Ok(c.execute(
                "UPDATE tokens SET hash = ?2, expires_at = ?3 WHERE id = ?1 AND kind = 'agent' AND revoked_at IS NULL AND expires_at > ?4",
                params![tid, hash, expires_at, crate::util::now_unix()],
            )?)
        })
        .await
        .map_err(internal)?;
    if n == 0 {
        return Err((StatusCode::CONFLICT, "only a live agent token rotates; mint a new one".into()));
    }
    hub.store.audit(caller.name.clone(), "tokens.rotate".into(), id.to_owned()).await;
    Ok(srv::json(StatusCode::OK, &json!({ "id": id, "token": token, "expires_at": expires_at })))
}
