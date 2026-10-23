//! The `inbox` channel (docs/ci.md W19-R06): kiln's own list of notifications for each person,
//! shown on the review inbox page. A rule's `inbox` puts an item there for the people its event
//! is about (a run's actor, a review's requested reviewers, the people in a thread a reply joins,
//! whom a comment mentions); `inbox:ada` puts one there for that person. An item is its event,
//! title and line, where it points, its repository, when, and whether it was read.

use std::sync::Arc;

use http::StatusCode;
use rusqlite::params;
use serde_json::{Value, json};

use super::Fail;
use crate::authz::{Caller, Scope};
use crate::hub::Hub;

/// How many items a person keeps: the oldest go past it.
const KEEP: i64 = 500;

/// A person as the inbox keys them: `user:ada`, or `admin` (the owner).
pub fn person(name: &str) -> String {
    let n = name.trim();
    if n.is_empty() || n.contains(':') || n == "admin" { n.to_owned() } else { format!("user:{n}") }
}

/// Puts an item in each of `people`'s inboxes; how many went in.
pub async fn put(hub: &Hub, people: &[String], event: &str, title: &str, line: &str, url: &str, repo: Option<&str>) -> anyhow::Result<usize> {
    let mut people: Vec<String> = people.iter().map(|p| person(p)).filter(|p| !p.is_empty()).collect();
    people.sort();
    people.dedup();
    if people.is_empty() {
        return Ok(0);
    }
    let (event, title, line, url, repo) = (event.to_owned(), title.to_owned(), line.to_owned(), url.to_owned(), repo.map(str::to_owned));
    let n = people.len();
    let told = people.clone();
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let now = crate::util::now_unix();
            for p in &people {
                tx.execute(
                    "INSERT INTO ci_inbox (id, person, event, title, line, url, repo, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![crate::util::new_id(), p, event, title, line, url, repo, now],
                )?;
                tx.execute("DELETE FROM ci_inbox WHERE person = ?1 AND id NOT IN (SELECT id FROM ci_inbox WHERE person = ?1 ORDER BY created_at DESC, id DESC LIMIT ?2)", params![p, KEEP])?;
            }
            tx.commit()?;
            Ok(())
        })
        .await?;
    for p in &told {
        hub.emit(&json!({ "type": "inbox", "person": p }));
    }
    Ok(n)
}

fn me(caller: &Caller) -> String {
    person(&super::review::who(caller).0)
}

/// `GET /api/inbox?unread=1&limit=`: the caller's items, newest first, of repositories they may
/// read, and how many are unread.
pub async fn list(hub: &Hub, caller: &Caller, q: &std::collections::HashMap<String, String>) -> Result<Value, Fail> {
    let who = me(caller);
    let unread_only = q.get("unread").is_some_and(|v| v == "1" || v == "true");
    let limit: i64 = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(50).clamp(1, 200);
    #[allow(clippy::type_complexity)]
    let rows: Vec<(String, String, String, String, String, Option<String>, i64, Option<i64>)> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT id, event, title, line, url, repo, created_at, read_at FROM ci_inbox WHERE person = ?1 AND (?2 = 0 OR read_at IS NULL) ORDER BY created_at DESC, id DESC LIMIT ?3",
            )?;
            let rows = st.query_map(params![who, i64::from(unread_only), limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let may = |repo: &Option<String>| repo.as_ref().is_none_or(|r| caller.can("runs.read", &Scope::Repo { repo: r.clone() }) || caller.can("diffs.read", &Scope::Repo { repo: r.clone() }));
    let items: Vec<Value> = rows
        .into_iter()
        .filter(|r| may(&r.5))
        .map(|(id, event, title, line, url, repo, at, read)| json!({ "id": id, "event": event, "title": title, "line": line, "url": url, "repo": repo, "at": at, "read": read.is_some() }))
        .collect();
    let who = me(caller);
    let unread: i64 = hub.store.call(move |c| Ok(c.query_row("SELECT COUNT(*) FROM ci_inbox WHERE person = ?1 AND read_at IS NULL", [&who], |r| r.get(0))?)).await.unwrap_or(0);
    Ok(json!({ "items": items, "unread": unread }))
}

/// `POST /api/inbox/read`: `{ "ids": [...] }` or `{ "all": true }`, the caller's own.
pub async fn read(hub: &Arc<Hub>, caller: &Caller, body: &Value) -> Result<Value, Fail> {
    let who = me(caller);
    let all = body.get("all").and_then(Value::as_bool).unwrap_or(false);
    let ids: Vec<String> = body.get("ids").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
    if !all && ids.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "`ids`, or `all: true`".into()));
    }
    let who2 = who.clone();
    let n = hub
        .store
        .call(move |c| {
            let now = crate::util::now_unix();
            if all {
                return Ok(c.execute("UPDATE ci_inbox SET read_at = ?2 WHERE person = ?1 AND read_at IS NULL", params![who2, now])?);
            }
            let mut n = 0;
            for id in &ids {
                n += c.execute("UPDATE ci_inbox SET read_at = ?3 WHERE person = ?1 AND id = ?2 AND read_at IS NULL", params![who2, id, now])?;
            }
            Ok(n)
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    hub.emit(&json!({ "type": "inbox", "person": who }));
    Ok(json!({ "read": n }))
}
