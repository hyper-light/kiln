//! The reaper's policies, stored and served (docs/ci.md W24-R31): one per scope (the system, a
//! runner, a repository, a session) in `work_reap_policies`, and the policy in force for a runner,
//! a repository and a session together, with the scope each value came from
//! (`kiln_ci::work::reap::resolve`).
//!
//! - `GET /api/ci/work/reap/policies?runner=&repo=&session=&volume=`: the policies those scopes
//!   set, and the effective one on a volume of `volume` bytes (1 TiB if left out).
//! - `PUT /api/ci/work/reap/policies?scope=server|runner:<name>|repo:<owner/name>|session:<id>`
//!   with a policy as people write it (`{ "keep_last": 3, "max_age": "14d" }`; `{}` or `null`
//!   removes it): needs `caches.write` at that scope (a session's: the server's); every change is
//!   audited.

use std::collections::HashMap;

use http::StatusCode;
use kiln_ci::work::reap::{self, Policy, Scope};
use rusqlite::params;
use serde_json::{Value, json};

use super::Fail;
use crate::authz::Caller;
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;

/// The volume an effective policy is shown on when the request names none: 1 TiB.
const VOLUME: u64 = 1 << 40;

fn bad(m: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, m.into())
}

fn internal(e: impl std::fmt::Display) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn scope_name(s: Scope) -> &'static str {
    match s {
        Scope::System => "system",
        Scope::Runner => "runner",
        Scope::Repo => "repo",
        Scope::Session => "session",
    }
}

/// `server`, `runner:<name>`, `repo:<owner/name>` or `session:<id>`.
fn parse_scope(s: &str) -> Result<(Scope, String), Fail> {
    let s = s.trim();
    if s == "server" || s == "system" {
        return Ok((Scope::System, String::new()));
    }
    let (kind, target) = s.split_once(':').ok_or_else(|| bad(format!("`{s}` isn't a scope: server, runner:<name>, repo:<owner/name> or session:<id>")))?;
    let target = target.trim();
    if target.is_empty() {
        return Err(bad(format!("`{s}` names no {kind}")));
    }
    match kind {
        "runner" => Ok((Scope::Runner, target.to_owned())),
        "repo" if target.contains('/') => Ok((Scope::Repo, target.to_ascii_lowercase())),
        "repo" => Err(bad("a repository's scope is `repo:<owner>/<name>`")),
        "session" | "agent" | "harness" => Ok((Scope::Session, target.to_owned())),
        _ => Err(bad(format!("`{s}` isn't a scope: server, runner:<name>, repo:<owner/name> or session:<id>"))),
    }
}

/// Whether `caller` may change the policy at this scope.
fn may(caller: Option<&Caller>, scope: Scope, target: &str) -> bool {
    let Some(c) = caller else { return true };
    let at = match scope {
        Scope::System => crate::authz::Scope::Server,
        Scope::Runner => crate::authz::Scope::Runner { runner: target.to_owned() },
        Scope::Repo => crate::authz::Scope::Repo { repo: target.to_owned() },
        Scope::Session => crate::authz::Scope::Server,
    };
    c.can("caches.write", &at)
}

/// The stored policy of each (scope, target) asked for, with who changed it and when.
async fn load(hub: &Hub, wanted: Vec<(Scope, String)>) -> Result<Vec<(Scope, String, Policy, i64, String)>, Fail> {
    hub.store
        .call(move |c| {
            let mut out = Vec::new();
            for (scope, target) in wanted {
                let row: Option<(String, i64, String)> = c
                    .query_row("SELECT policy, updated_at, updated_by FROM work_reap_policies WHERE scope = ?1 AND target = ?2", params![scope_name(scope), target], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                    .ok();
                if let Some((text, at, by)) = row {
                    let policy: Policy = serde_json::from_str(&text).unwrap_or_default();
                    out.push((scope, target, policy, at, by));
                }
            }
            Ok(out)
        })
        .await
        .map_err(internal)
}

/// The system's policy and each repository's, as stored (for the server's reaper, W24-R31).
pub fn stored(c: &rusqlite::Connection) -> rusqlite::Result<(Policy, std::collections::BTreeMap<String, Policy>)> {
    let mut system = Policy::default();
    let mut repos = std::collections::BTreeMap::new();
    let mut st = c.prepare("SELECT scope, target, policy FROM work_reap_policies WHERE scope IN ('system', 'repo')")?;
    for row in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))? {
        let (scope, target, text) = row?;
        let p: Policy = serde_json::from_str(&text).unwrap_or_default();
        if scope == "system" {
            system = p;
        } else {
            repos.insert(target, p);
        }
    }
    Ok((system, repos))
}

/// The scopes a request names: the system's always, then a runner's, a repository's and a
/// session's when given.
fn chain(q: &HashMap<String, String>) -> Vec<(Scope, String)> {
    let mut out = vec![(Scope::System, String::new())];
    for (key, scope) in [("runner", Scope::Runner), ("repo", Scope::Repo), ("session", Scope::Session)] {
        if let Some(t) = q.get(key).map(|t| t.trim()).filter(|t| !t.is_empty()) {
            let t = if scope == Scope::Repo { t.to_ascii_lowercase() } else { t.to_owned() };
            out.push((scope, t));
        }
    }
    out
}

/// What [`get`] and [`put`] answer: the policies set and the effective one.
async fn view(hub: &Hub, wanted: Vec<(Scope, String)>, volume: u64) -> Result<Value, Fail> {
    let rows = load(hub, wanted).await?;
    let layers: Vec<(Scope, &Policy)> = rows.iter().map(|(s, _, p, _, _)| (*s, p)).collect();
    let r = reap::resolve(volume, &layers);
    let policies: Vec<Value> = rows.iter().map(|(s, t, p, at, by)| json!({ "scope": scope_name(*s), "target": t, "policy": reap::policy_text(p), "updated_at": at, "updated_by": by })).collect();
    Ok(json!({ "policies": policies, "effective": reap::effective_text(&r, volume) }))
}

fn volume(q: &HashMap<String, String>) -> Result<u64, Fail> {
    q.get("volume").map_or(Ok(VOLUME), |v| v.trim().parse::<u64>().ok().filter(|v| *v > 0).ok_or_else(|| bad(format!("volume: `{v}` isn't a size in bytes"))))
}

/// `GET /api/ci/work/reap/policies?runner=&repo=&session=&volume=`.
pub async fn get(hub: &Hub, q: &HashMap<String, String>) -> Result<Response, Fail> {
    let v = view(hub, chain(q), volume(q)?).await?;
    Ok(srv::json(StatusCode::OK, &v))
}

/// `PUT /api/ci/work/reap/policies?scope=` with a policy as people write it.
pub async fn put(hub: &Hub, q: &HashMap<String, String>, body: Value, caller: Option<&Caller>) -> Result<Response, Fail> {
    let (scope, target) = parse_scope(q.get("scope").map(String::as_str).unwrap_or("server"))?;
    if !may(caller, scope, &target) {
        return Err((StatusCode::FORBIDDEN, format!("changing the reaper's policy for {} {target} needs caches.write there", scope_name(scope))));
    }
    let body = body.get("policy").cloned().unwrap_or(body);
    let policy = reap::parse_policy(&body).map_err(bad)?;
    reap::check_scope(&policy, scope).map_err(bad)?;
    let by = caller.map_or_else(|| "admin".to_owned(), |c| c.name.clone());
    let (name, t, text, by_, empty) = (scope_name(scope), target.clone(), serde_json::to_string(&policy).unwrap_or_default(), by.clone(), policy == Policy::default());
    hub.store
        .call(move |c| {
            if empty {
                c.execute("DELETE FROM work_reap_policies WHERE scope = ?1 AND target = ?2", params![name, t])?;
            } else {
                c.execute(
                    "INSERT INTO work_reap_policies (scope, target, policy, updated_at, updated_by) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(scope, target) DO UPDATE SET policy = ?3, updated_at = ?4, updated_by = ?5",
                    params![name, t, text, crate::util::now_unix(), by_],
                )?;
            }
            Ok(())
        })
        .await
        .map_err(internal)?;
    let shown = if target.is_empty() { name.to_owned() } else { format!("{name}:{target}") };
    hub.store.audit(by, "ci.work.reap.set".into(), format!("{shown}: {}", reap::policy_text(&policy))).await;
    let mut wanted = vec![(Scope::System, String::new())];
    if scope != Scope::System {
        wanted.push((scope, target));
    }
    let v = view(hub, wanted, volume(q)?).await?;
    Ok(srv::json(StatusCode::OK, &json!({ "scope": shown, "policy": reap::policy_text(&policy), "effective": v.get("effective").cloned().unwrap_or(Value::Null) })))
}
