//! Groups (docs/deploy-api.md §2, §3.1, §15): teams of kiln's users, by name, and the groups a
//! person's identity provider says they're in, recorded at each sign-in (GitHub's organizations and
//! `org/team`s, GitLab's groups, Bitbucket's workspaces). An app's owner teams are groups: `shop`,
//! or one of a provider's, `github:acme/platform`.
//!
//! | Route | |
//! | --- | --- |
//! | `GET`, `POST /api/groups` | kiln's groups and their members; a new one (`{name, description, members}`) |
//! | `GET`, `PATCH`, `DELETE /api/groups/{name}` | one; its description (`{description}`); gone (refused while it owns apps) |
//! | `PUT`, `DELETE /api/groups/{name}/members/{user}` | a member added, or taken out |

use http::{Method, StatusCode};
use rusqlite::{OptionalExtension as _, params};
use serde::Deserialize;
use serde_json::{Value, json};

use super::Scope;
use super::gate::Caller;
use crate::ci::Fail;
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

/// The identity providers people sign in with, whose groups an owner team may name.
pub const PROVIDERS: &[&str] = &["github", "gitlab", "bitbucket"];

fn bad(m: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, m.into())
}

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// A group's name: letters, digits, `-`, `_` and `.`, at most 64.
pub fn name_ok(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// An owner team as an app names it: one of kiln's groups (`shop`), or an identity provider's
/// (`github:acme/platform`, `gitlab:acme/platform`, `bitbucket:acme`).
pub fn ref_ok(r: &str) -> Result<(), String> {
    let path_ok = |g: &str| !g.is_empty() && g.len() <= 255 && !g.starts_with('/') && !g.ends_with('/') && !g.contains("//") && g.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'));
    match r.split_once(':') {
        None if name_ok(r) => Ok(()),
        Some((p, g)) if PROVIDERS.contains(&p) && path_ok(g) => Ok(()),
        _ => Err(format!("`{r}` isn't a group: one of kiln's (`shop`), or an identity provider's (`github:acme/platform`, `gitlab:acme/platform`, `bitbucket:acme`)")),
    }
}

/// Whether the caller administers people (it may make anyone a member of anything): the owner, or
/// `users.manage` on the server.
pub fn admin(caller: &Caller) -> bool {
    caller.is_owner() || caller.can("users.manage", &Scope::Server)
}

fn member_in(c: &rusqlite::Connection, user: &str, r: &str) -> rusqlite::Result<bool> {
    match r.split_once(':') {
        None => c.query_row("SELECT EXISTS(SELECT 1 FROM user_group_members m JOIN user_groups g ON g.name = m.group_name WHERE m.group_name = ?1 AND m.user_id = ?2)", params![r, user], |row| row.get(0)),
        // The provider's groups as they were when the person last signed in with it.
        Some((provider, group)) => {
            let mut st = c.prepare("SELECT groups FROM user_identities WHERE provider = ?1 AND user_id = ?2")?;
            let lists = st.query_map(params![provider, user], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            Ok(lists.iter().any(|l| serde_json::from_str::<Vec<String>>(l).unwrap_or_default().iter().any(|g| g.eq_ignore_ascii_case(group))))
        }
    }
}

/// Whether the user (by id) is in any of the groups `refs` name.
pub async fn member_of_any(hub: &Hub, user: &str, refs: &[String]) -> anyhow::Result<bool> {
    let (user, refs) = (user.to_owned(), refs.to_vec());
    hub.store
        .call(move |c| {
            for r in &refs {
                if member_in(c, &user, r)? {
                    return Ok(true);
                }
            }
            Ok(false)
        })
        .await
}

/// The groups a user is in: kiln's, then each identity provider's as an owner team names it
/// (`github:acme/platform`).
pub async fn of_user(hub: &Hub, user: &str) -> anyhow::Result<Vec<String>> {
    let user = user.to_owned();
    hub.store
        .call(move |c| {
            let mut st = c.prepare("SELECT g.name FROM user_group_members m JOIN user_groups g ON g.name = m.group_name WHERE m.user_id = ?1 ORDER BY g.name")?;
            let mut out = st.query_map([&user], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            let mut st = c.prepare("SELECT provider, groups FROM user_identities WHERE user_id = ?1 ORDER BY provider")?;
            for (p, l) in st.query_map([&user], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<Result<Vec<_>, _>>()? {
                out.extend(serde_json::from_str::<Vec<String>>(&l).unwrap_or_default().into_iter().map(|g| format!("{p}:{g}")));
            }
            Ok(out)
        })
        .await
}

/// The group routes, or `None` for a path that isn't one.
pub async fn route(hub: &Hub, caller: &Caller, method: &Method, s: &[&str], req: Request) -> Option<Result<Response, Fail>> {
    Some(match (method, s) {
        (&Method::GET, ["api", "groups"]) => list(hub, None).await.map(|l| srv::json(StatusCode::OK, &json!({ "groups": l }))),
        (&Method::POST, ["api", "groups"]) => create(hub, caller, req).await,
        (&Method::GET, ["api", "groups", name]) => match list(hub, Some(name)).await {
            Ok(mut l) if !l.is_empty() => Ok(srv::json(StatusCode::OK, &l.remove(0))),
            Ok(_) => Err((StatusCode::NOT_FOUND, format!("no group `{name}`"))),
            Err(e) => Err(e),
        },
        (&Method::PATCH, ["api", "groups", name]) => describe(hub, caller, name, req).await,
        (&Method::DELETE, ["api", "groups", name]) => delete(hub, caller, name).await,
        (&Method::PUT, ["api", "groups", name, "members", user]) => add(hub, caller, name, user).await,
        (&Method::DELETE, ["api", "groups", name, "members", user]) => remove(hub, caller, name, user).await,
        _ => return None,
    })
}

/// kiln's groups (or the one named), each with its members by name.
async fn list(hub: &Hub, only: Option<&str>) -> Result<Vec<Value>, Fail> {
    let only = only.map(str::to_owned);
    hub.store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT g.name, g.description, g.created_at, g.created_by,
                        (SELECT json_group_array(n) FROM (SELECT u.name AS n FROM user_group_members m JOIN users u ON u.id = m.user_id
                                                          WHERE m.group_name = g.name AND u.disabled_at IS NULL ORDER BY u.name))
                 FROM user_groups g WHERE ?1 IS NULL OR g.name = ?1 ORDER BY g.name",
            )?;
            let rows = st.query_map([only], |r| {
                let members: String = r.get(4)?;
                Ok(json!({
                    "name": r.get::<_, String>(0)?, "description": r.get::<_, String>(1)?, "created_at": r.get::<_, i64>(2)?,
                    "created_by": r.get::<_, String>(3)?, "members": serde_json::from_str::<Value>(&members).unwrap_or(json!([])),
                }))
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(internal)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewGroup {
    name: String,
    #[serde(default)]
    description: String,
    /// Users by name (or id).
    #[serde(default)]
    members: Vec<String>,
}

async fn create(hub: &Hub, caller: &Caller, req: Request) -> Result<Response, Fail> {
    let b: NewGroup = srv::body_json(req.into_body(), 1 << 20).await.map_err(|e| bad(format!("{e:#}")))?;
    let name = b.name.trim().to_owned();
    if !name_ok(&name) {
        return Err(bad("a group's name is up to 64 letters, digits, `-`, `_` and `.`"));
    }
    let mut members = Vec::new();
    for m in &b.members {
        members.push(super::api::find_user(hub, m).await?);
    }
    let (n, description, by) = (name.clone(), b.description.trim().to_owned(), caller.name.clone());
    let ids: Vec<String> = members.iter().map(|(id, _)| id.clone()).collect();
    let made: bool = hub
        .store
        .call(move |c| {
            let tx = c.transaction()?;
            let now = crate::util::now_unix();
            if tx.execute("INSERT OR IGNORE INTO user_groups (name, description, created_at, created_by) VALUES (?1, ?2, ?3, ?4)", params![n, description, now, by])? == 0 {
                return Ok(false);
            }
            for id in &ids {
                tx.execute("INSERT OR IGNORE INTO user_group_members (group_name, user_id, added_at, added_by) VALUES (?1, ?2, ?3, ?4)", params![n, id, now, by])?;
            }
            tx.commit()?;
            Ok(true)
        })
        .await
        .map_err(internal)?;
    if !made {
        return Err((StatusCode::CONFLICT, format!("a group `{name}` exists")));
    }
    let names: Vec<String> = members.into_iter().map(|(_, n)| n).collect();
    hub.store.audit(caller.name.clone(), "groups.create".into(), format!("{name}: {}", names.join(", "))).await;
    let mut made = list(hub, Some(&name)).await?;
    Ok(srv::json(StatusCode::CREATED, &made.pop().unwrap_or(Value::Null)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Describe {
    description: String,
}

async fn describe(hub: &Hub, caller: &Caller, name: &str, req: Request) -> Result<Response, Fail> {
    let b: Describe = srv::body_json(req.into_body(), 1 << 20).await.map_err(|e| bad(format!("{e:#}")))?;
    let (n, d) = (name.to_owned(), b.description.trim().to_owned());
    let changed = hub.store.call(move |c| Ok(c.execute("UPDATE user_groups SET description = ?2 WHERE name = ?1", params![n, d])?)).await.map_err(internal)?;
    if changed == 0 {
        return Err((StatusCode::NOT_FOUND, format!("no group `{name}`")));
    }
    hub.store.audit(caller.name.clone(), "groups.update".into(), name.to_owned()).await;
    let mut l = list(hub, Some(name)).await?;
    Ok(srv::json(StatusCode::OK, &l.pop().unwrap_or(Value::Null)))
}

async fn delete(hub: &Hub, caller: &Caller, name: &str) -> Result<Response, Fail> {
    // An app is never left with an owner team that isn't there.
    let owned: Vec<String> = crate::deploy::store::apps(&hub.store).await.map_err(internal)?.into_iter().filter(|a| a.owners.iter().any(|o| o.eq_ignore_ascii_case(name))).map(|a| a.name).collect();
    if !owned.is_empty() {
        return Err((StatusCode::CONFLICT, format!("`{name}` owns {}: give them another owner team first", owned.join(", "))));
    }
    let n = name.to_owned();
    let gone = hub
        .store
        .call(move |c| {
            let tx = c.transaction()?;
            let gone = tx.execute("DELETE FROM user_groups WHERE name = ?1", [&n])?;
            tx.execute("DELETE FROM user_group_members WHERE group_name = ?1", [&n])?;
            tx.commit()?;
            Ok(gone)
        })
        .await
        .map_err(internal)?;
    if gone == 0 {
        return Err((StatusCode::NOT_FOUND, format!("no group `{name}`")));
    }
    hub.store.audit(caller.name.clone(), "groups.delete".into(), name.to_owned()).await;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

async fn there(hub: &Hub, name: &str) -> Result<String, Fail> {
    let n = name.to_owned();
    hub.store
        .call(move |c| Ok(c.query_row("SELECT name FROM user_groups WHERE name = ?1", [&n], |r| r.get::<_, String>(0)).optional()?))
        .await
        .map_err(internal)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("no group `{name}`")))
}

async fn add(hub: &Hub, caller: &Caller, name: &str, user: &str) -> Result<Response, Fail> {
    let group = there(hub, name).await?;
    let (id, uname) = super::api::find_user(hub, user).await?;
    let (g, by) = (group.clone(), caller.name.clone());
    hub.store
        .call(move |c| Ok(c.execute("INSERT OR IGNORE INTO user_group_members (group_name, user_id, added_at, added_by) VALUES (?1, ?2, ?3, ?4)", params![g, id, crate::util::now_unix(), by])?))
        .await
        .map_err(internal)?;
    hub.store.audit(caller.name.clone(), "groups.add".into(), format!("{uname} to {group}")).await;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

async fn remove(hub: &Hub, caller: &Caller, name: &str, user: &str) -> Result<Response, Fail> {
    let group = there(hub, name).await?;
    let (id, uname) = super::api::find_user(hub, user).await?;
    let g = group.clone();
    let gone = hub.store.call(move |c| Ok(c.execute("DELETE FROM user_group_members WHERE group_name = ?1 AND user_id = ?2", params![g, id])?)).await.map_err(internal)?;
    if gone == 0 {
        return Err((StatusCode::NOT_FOUND, format!("{uname} isn't in {group}")));
    }
    hub.store.audit(caller.name.clone(), "groups.remove".into(), format!("{uname} from {group}")).await;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_teams_are_groups() {
        for good in ["shop", "platform-team", "a.b_c", "github:acme", "github:acme/platform", "gitlab:acme/platform/sre", "bitbucket:acme"] {
            assert!(ref_ok(good).is_ok(), "{good}");
        }
        for bad in ["", "shop team", "slack:acme", "github:", "github:/acme", "github:acme/", "github:acme//x", "a:b:c", &"x".repeat(65)] {
            assert!(ref_ok(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn membership_in_kiln_s_groups_and_a_provider_s() -> anyhow::Result<()> {
        let c = rusqlite::Connection::open_in_memory()?;
        c.execute_batch(
            "CREATE TABLE user_groups (name TEXT PRIMARY KEY COLLATE NOCASE, description TEXT NOT NULL DEFAULT '', created_at INTEGER NOT NULL, created_by TEXT NOT NULL);
             CREATE TABLE user_group_members (group_name TEXT NOT NULL COLLATE NOCASE, user_id TEXT NOT NULL, added_at INTEGER NOT NULL, added_by TEXT NOT NULL, PRIMARY KEY (group_name, user_id));
             CREATE TABLE user_identities (provider TEXT NOT NULL, subject TEXT NOT NULL, user_id TEXT NOT NULL, groups TEXT NOT NULL DEFAULT '[]');
             INSERT INTO user_groups VALUES ('Shop', '', 0, 'admin');
             INSERT INTO user_group_members VALUES ('shop', 'u1', 0, 'admin');
             INSERT INTO user_group_members VALUES ('gone', 'u1', 0, 'admin');
             INSERT INTO user_identities VALUES ('github', '42', 'u2', '[\"acme\",\"acme/platform\"]');",
        )?;
        assert!(member_in(&c, "u1", "shop")? && member_in(&c, "u1", "SHOP")?, "a group's name in any case");
        assert!(!member_in(&c, "u2", "shop")?);
        assert!(!member_in(&c, "u1", "gone")?, "a membership of a group that isn't there any more");
        assert!(member_in(&c, "u2", "github:acme/platform")? && member_in(&c, "u2", "github:ACME")?);
        assert!(!member_in(&c, "u2", "github:acme/other")? && !member_in(&c, "u2", "gitlab:acme")?, "another team, another provider");
        Ok(())
    }
}
