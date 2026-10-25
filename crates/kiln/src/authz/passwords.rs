//! People's own passwords (docs/ci.md W10): beside sign-in links, SSO and passkeys, a person may
//! keep a password (Argon2id, `users.password_hash`) and sign in with their name and it. The
//! owner's password is the server's own (`crate::password`).
//!
//! Wrong guesses are throttled per person, as the owner's are for the server; names kiln doesn't
//! know share one throttle, and are checked against a stand-in hash, so how long an answer takes
//! doesn't say whether a name is someone's.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use http::StatusCode;
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use super::Caller;
use super::gate::Kind;
use crate::hub::Hub;

type Fail = (StatusCode, String);

/// Each person's throttle (by name, lowercased; `""` for names kiln doesn't know).
static THROTTLES: LazyLock<parking_lot::Mutex<HashMap<String, crate::password::Throttle>>> = LazyLock::new(Default::default);

/// What an unknown name's guess is checked against, so it takes as long as a known one's.
static STAND_IN: LazyLock<String> = LazyLock::new(|| crate::password::hash(&crate::util::random_token()).unwrap_or_default());

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// The person the caller is, signed in themselves (not through a token or an agent).
fn person(caller: &Caller) -> Result<String, Fail> {
    match (&caller.kind, &caller.user) {
        (Kind::User, Some(id)) if caller.token.is_none() && caller.agent.is_none() => Ok(id.clone()),
        (Kind::Owner, _) => Err((StatusCode::CONFLICT, "the owner's password is the server's: Settings → Sign-in sets it".into())),
        _ => Err((StatusCode::FORBIDDEN, "a password is a person's own: sign in as yourself to set it".into())),
    }
}

/// `GET /api/me/password`: whether the caller keeps a password.
pub async fn get(hub: &Hub, caller: &Caller) -> Result<Value, Fail> {
    let id = person(caller)?;
    let set: bool = hub.store.call(move |c| Ok(c.query_row("SELECT password_hash IS NOT NULL FROM users WHERE id = ?1", [&id], |r| r.get(0)).optional()?.unwrap_or(false))).await.map_err(internal)?;
    Ok(json!({ "set": set }))
}

/// `PUT /api/me/password` `{password, current}`: the caller's own password, set; or, `password`
/// empty, removed. Changing or removing one takes the current one.
pub async fn set(hub: &Hub, caller: &Caller, body: &Value) -> Result<Value, Fail> {
    let id = person(caller)?;
    let new = body.get("password").and_then(Value::as_str).unwrap_or_default().to_owned();
    let current = body.get("current").and_then(Value::as_str).unwrap_or_default().to_owned();
    if !new.is_empty() {
        crate::password::validate(&new).map_err(|why| (StatusCode::BAD_REQUEST, format!("Password not accepted: {why}.")))?;
    }
    let i = id.clone();
    let existing: Option<String> = hub.store.call(move |c| Ok(c.query_row("SELECT password_hash FROM users WHERE id = ?1", [&i], |r| r.get::<_, Option<String>>(0)).optional()?.flatten())).await.map_err(internal)?;
    if let Some(phc) = existing {
        let ok = crate::rt::blocking(move || crate::password::verify(&current, &phc)).await.unwrap_or(false);
        if !ok {
            crate::rt::sleep(Duration::from_millis(300)).await;
            return Err((StatusCode::FORBIDDEN, "Your current password is wrong.".into()));
        }
    } else if new.is_empty() {
        return Ok(json!({ "set": false }));
    }
    let hash = if new.is_empty() { None } else { Some(crate::rt::blocking(move || crate::password::hash(&new)).await.map_err(internal)?.map_err(internal)?) };
    let (i, removed) = (id, hash.is_none());
    hub.store.call(move |c| Ok(c.execute("UPDATE users SET password_hash = ?2 WHERE id = ?1", params![i, hash]).map(|_| ())?)).await.map_err(internal)?;
    hub.store.audit(caller.name.clone(), "password".into(), if removed { "removed".into() } else { "set".into() }).await;
    Ok(json!({ "set": !removed }))
}

/// A person signing in with their `name` and `password`: their id, name and session epoch.
pub async fn sign_in(hub: &Hub, name: &str, password: &str) -> Result<(String, String, i64), Fail> {
    let n = name.trim().to_owned();
    let row: Option<(String, String, i64, Option<String>)> = hub
        .store
        .call(move |c| Ok(c.query_row("SELECT id, name, session_epoch, password_hash FROM users WHERE name = ?1 AND disabled_at IS NULL", [&n], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?))
        .await
        .map_err(internal)?;
    let slot = row.as_ref().map(|(_, name, _, _)| name.to_lowercase()).unwrap_or_default();
    if let Err(wait) = THROTTLES.lock().entry(slot.clone()).or_default().check() {
        return Err((StatusCode::TOO_MANY_REQUESTS, format!("Too many wrong passwords. Try again in {}s.", wait.as_secs().max(1))));
    }
    let phc = row.as_ref().and_then(|(_, _, _, phc)| phc.clone());
    let (found, pw) = (phc.is_some(), password.to_owned());
    // On a blocking thread, the stand-in too: hashing it the first time takes as long as a check.
    let ok = crate::rt::blocking(move || crate::password::verify(&pw, phc.as_deref().unwrap_or(STAND_IN.as_str()))).await.unwrap_or(false) && found;
    let throttled = |f: fn(&crate::password::Throttle)| f(THROTTLES.lock().entry(slot.clone()).or_default());
    match row.filter(|_| ok) {
        Some((id, name, epoch, _)) => {
            throttled(crate::password::Throttle::succeeded);
            hub.store.audit(format!("user:{name}"), "login".into(), "password".into()).await;
            Ok((id, name, epoch))
        }
        None => {
            throttled(crate::password::Throttle::failed);
            crate::rt::sleep(Duration::from_millis(300)).await;
            Err((StatusCode::UNAUTHORIZED, "Wrong name or password.".into()))
        }
    }
}

/// Whether any person keeps a password (the sign-in page asks for a name then).
pub async fn any(hub: &Hub) -> bool {
    hub.store.call(|c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM users WHERE password_hash IS NOT NULL AND disabled_at IS NULL)", [], |r| r.get::<_, bool>(0))?)).await.unwrap_or(false)
}
