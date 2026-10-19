//! Who a request is (W10-R01) and whether the route table lets them through (W10-R02, R05).
//!
//! The owner is recognized first, exactly as before (the admin token, the owner's session).
//! Everyone else is a token (`kiln_pat_…`, `kiln_agt_…`, `kiln_job_…`) or a user's session, and
//! each request is checked against [`super::ROUTES`]: the rule its route matches names a
//! capability and where its scope comes from.

use std::collections::HashMap;

use http::{Method, StatusCode};
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::Serialize;

use super::{At, Grant, SIGNED_IN, Scope};
use crate::hub::Hub;
use kiln_ci::plan::perms::Level;

/// The prefix of a personal access token.
pub const PAT_PREFIX: &str = "kiln_pat_";
/// The prefix of an agent token.
pub const AGENT_PREFIX: &str = "kiln_agt_";

/// What a principal may do: its grants, and the custom roles they name.
#[derive(Debug, Clone, Default)]
pub struct Rights {
    pub grants: Vec<Grant>,
    pub custom: HashMap<String, Vec<String>>,
}

impl Rights {
    pub fn allows(&self, capability: &str, target: &Scope) -> bool {
        super::allows(&self.grants, &self.custom, capability, target)
    }

    /// Whether some grant gives `capability`, anywhere.
    fn somewhere(&self, capability: &str) -> bool {
        self.grants.iter().any(|g| !g.deny && g.gives(capability, &self.custom))
    }
}

/// How a caller signed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Owner,
    User,
    Pat,
    Agent,
    Job,
}

/// A request's caller.
#[derive(Debug, Clone)]
pub struct Caller {
    pub kind: Kind,
    /// As the audit names it: `admin` (the owner, as it always has), `user:<name>`,
    /// `job:<leg>`; a token as `pat:<id>` or `agent:<label>`, then whom it acts for.
    pub name: String,
    /// The user it is or acts for, by id (`None`: the owner, or a job).
    pub user: Option<String>,
    /// The token it signed in with (`tokens.id`).
    pub token: Option<String>,
    /// The agent session it is, when it's one kiln runs: set from how it reached kiln (its MCP
    /// token, its runner vouching for it, the machine's socket seeing its process), never from
    /// what it asks. An agent it starts is that agent's child (`harness::lineage`).
    pub agent: Option<String>,
    /// What the principal may do; `None`: everything.
    rights: Option<Rights>,
    /// A token's own grant: the caller does at most what both allow.
    limit: Option<Rights>,
}

impl Caller {
    pub fn owner() -> Caller {
        Caller { kind: Kind::Owner, name: "admin".into(), user: None, token: None, agent: None, rights: None, limit: None }
    }

    /// A caller with these rights, for tests and for checks made on someone's behalf.
    pub fn with(kind: Kind, name: &str, rights: Option<Rights>, limit: Option<Rights>) -> Caller {
        Caller { kind, name: name.to_owned(), user: None, token: None, agent: None, rights, limit }
    }

    /// Whether it may do everything: the owner, or an owner's token without a grant of its own.
    pub fn is_owner(&self) -> bool {
        self.rights.is_none() && self.limit.is_none()
    }

    /// Whom an agent session it starts acts for: `owner`, or `user:<id>`. `None` for a job, or a
    /// token whose own grant is narrower than its owner (its agents would do more than it may).
    pub fn opener(&self) -> Option<String> {
        match (&self.kind, &self.user) {
            (Kind::Job, _) => None,
            (Kind::Owner, _) => Some("owner".to_owned()),
            (_, Some(u)) if self.limit.is_none() => Some(format!("user:{u}")),
            (_, None) if self.is_owner() => Some("owner".to_owned()),
            _ => None,
        }
    }

    /// Whether it may use `capability` in `target`.
    pub fn can(&self, capability: &str, target: &Scope) -> bool {
        capability == SIGNED_IN || [&self.rights, &self.limit].into_iter().all(|r| r.as_ref().is_none_or(|r| r.allows(capability, target)))
    }

    /// Whether it has `capability` anywhere (a route that narrows by itself).
    pub fn has_somewhere(&self, capability: &str) -> bool {
        capability == SIGNED_IN || [&self.rights, &self.limit].into_iter().all(|r| r.as_ref().is_none_or(|r| r.somewhere(capability)))
    }

    /// The grants it holds (a token's: its own), for `/api/me`.
    pub fn grants(&self) -> Option<Vec<String>> {
        let g = |r: &Rights| r.grants.iter().map(Grant::text).collect::<Vec<_>>();
        match (&self.rights, &self.limit) {
            (None, None) => None,
            (Some(r), None) | (None, Some(r)) => Some(g(r)),
            (Some(r), Some(l)) => Some(g(r).into_iter().chain(g(l).into_iter().map(|t| format!("{t} (token)"))).collect()),
        }
    }
}

// ------------------------------------------------------------------ who

/// The caller of a request the owner didn't make: a token, or a user's session. `None`: nobody
/// kiln knows (or a token that is revoked or expired).
pub async fn caller(hub: &Hub, bearer: Option<&str>, session: Option<&str>) -> Option<Caller> {
    if let Some(t) = bearer.map(str::trim) {
        if t.starts_with(PAT_PREFIX) || t.starts_with(AGENT_PREFIX) {
            return token(hub, t).await;
        }
        if t.starts_with(crate::ci::token::JOB_PREFIX) {
            return job(hub, t).await;
        }
    }
    user_session(hub, session?).await
}

fn store_err(e: anyhow::Error) -> Option<Caller> {
    tracing::warn!(error = %e, "reading a caller's rights failed");
    None
}

/// An agent session acting for whoever started it (`opener`: `owner`, or `user:<id>`), as kiln
/// answers the operations its MCP server asks for through its runner (`crate::mcp::runner_ask`).
/// Audited as `<name> for <who>`. `None`: that user is gone or disabled.
pub async fn agent_for(hub: &Hub, name: &str, opener: &str) -> Option<Caller> {
    if opener == "owner" {
        return Some(Caller { kind: Kind::Agent, name: format!("{name} for admin"), user: None, token: None, agent: None, rights: None, limit: None });
    }
    let uid = opener.strip_prefix("user:")?.to_owned();
    let name = name.to_owned();
    hub.store
        .call(move |c| {
            let Some((who, rights)) = user_rights(c, &uid)? else { return Ok(None) };
            Ok(Some(Caller { kind: Kind::Agent, name: format!("{name} for user:{who}"), user: Some(uid), token: None, agent: None, rights: Some(rights), limit: None }))
        })
        .await
        .unwrap_or_else(store_err)
}

/// Whether anyone but the owner can sign in here.
pub async fn has_users(hub: &Hub) -> bool {
    hub.store.call(|c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM users WHERE disabled_at IS NULL)", [], |r| r.get::<_, bool>(0))?)).await.unwrap_or(true)
}

/// Custom roles, by name.
fn custom_roles(c: &Connection) -> rusqlite::Result<HashMap<String, Vec<String>>> {
    let mut st = c.prepare("SELECT name, capabilities FROM roles")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    Ok(rows.filter_map(Result::ok).map(|(n, caps)| (n, serde_json::from_str(&caps).unwrap_or_default())).collect())
}

/// A live user's name and grants (`None`: no such user, or disabled).
pub(super) fn user_rights(c: &Connection, id: &str) -> rusqlite::Result<Option<(String, Rights)>> {
    let Some(name) = c.query_row("SELECT name FROM users WHERE id = ?1 AND disabled_at IS NULL", [id], |r| r.get::<_, String>(0)).optional()? else {
        return Ok(None);
    };
    let mut st = c.prepare("SELECT what, scope, deny FROM grants WHERE principal = ?1 ORDER BY id")?;
    let grants = st
        .query_map([format!("user:{id}")], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, bool>(2)?)))?
        .filter_map(Result::ok)
        .filter_map(|(what, scope, deny)| Some(Grant { what, scope: Scope::parse(&scope).ok()?, deny }))
        .collect();
    Ok(Some((name, Rights { grants, custom: custom_roles(c)? })))
}

/// A token's own grant, from its `scopes`.
pub(super) fn token_grants(scopes: &str) -> Vec<Grant> {
    serde_json::from_str::<serde_json::Value>(scopes)
        .ok()
        .and_then(|v| v.get("grants").cloned())
        .and_then(|g| serde_json::from_value(g).ok())
        .unwrap_or_default()
}

/// A personal access token's or an agent token's caller.
async fn token(hub: &Hub, t: &str) -> Option<Caller> {
    let hash = crate::util::sha256_hex(t.as_bytes());
    hub.store
        .call(move |c| {
            let now = crate::util::now_unix();
            let row: Option<(String, String, String, Option<String>, String)> = c
                .query_row(
                    "SELECT id, kind, principal, on_behalf_of, scopes FROM tokens WHERE hash = ?1 AND kind IN ('pat', 'agent') AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > ?2)",
                    params![hash, now],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()?;
            let Some((id, kind, principal, behalf, scopes)) = row else { return Ok(None) };
            c.execute("UPDATE tokens SET last_used_at = ?2 WHERE id = ?1", params![id, now])?;
            let agent = kind == "agent";
            // Whom it acts for: a personal token's owner; the user an agent acts for.
            let whose = if agent { behalf.unwrap_or_default() } else { principal.clone() };
            let (user, who, rights) = match whose.strip_prefix("user:") {
                Some(uid) => {
                    let Some((name, rights)) = user_rights(c, uid)? else { return Ok(None) };
                    (Some(uid.to_owned()), format!("user:{name}"), Some(rights))
                }
                None if whose == "owner" => (None, "admin".to_owned(), None),
                None => return Ok(None),
            };
            let limit = Rights { grants: token_grants(&scopes), custom: custom_roles(c)? };
            let short: String = id.chars().take(12).collect();
            let name = if agent { format!("{principal} for {who}") } else { format!("pat:{short} of {who}") };
            Ok(Some(Caller { kind: if agent { Kind::Agent } else { Kind::Pat }, name, user, token: Some(id), agent: None, rights, limit: Some(limit) }))
        })
        .await
        .unwrap_or_else(store_err)
}

/// A job token's caller: what its job's `permissions` give (W10-R04), in its repository.
async fn job(hub: &Hub, t: &str) -> Option<Caller> {
    let j = crate::ci::token::job_of(hub, t).await?;
    let id = j.repo.clone();
    let repo = hub.store.call(move |c| Ok(c.query_row("SELECT name FROM repos WHERE id = ?1", [&id], |r| r.get::<_, String>(0)).optional()?)).await.ok().flatten()?;
    let here = Scope::Repo { repo };
    let mut grants = Vec::new();
    let mut give = |caps: &[&str], scope: &Scope| grants.extend(caps.iter().map(|c| Grant { what: (*c).to_owned(), scope: scope.clone(), deny: false }));
    if j.may("actions", Level::Read) {
        give(&["runs.read", "logs.read", "artifacts.read"], &here);
    }
    if j.may("actions", Level::Write) {
        give(&["runs.trigger", "runs.cancel", "runs.rerun"], &here);
    }
    if j.may("contents", Level::Write) {
        give(&["runs.trigger"], &here);
    }
    if j.may("environments", Level::Write) {
        give(&["environments.deploy"], &here);
    }
    if j.may("binaries", Level::Write) {
        give(&["binaries.publish"], &Scope::Server);
    }
    Some(Caller { kind: Kind::Job, name: format!("job:{}", j.leg), user: None, token: None, agent: None, rights: Some(Rights { grants, custom: HashMap::new() }), limit: None })
}

// ------------------------------------------------------------------ users' sessions

/// How long a user's session lasts (the owner's: as long).
pub const SESSION_TTL: i64 = 30 * 86_400;
/// How long a user's sign-in link works.
pub const LINK_TTL: i64 = 86_400;

fn sign(hub: &Hub, msg: &str) -> String {
    crate::util::hmac_hex(hub.admin_token.as_bytes(), msg.as_bytes())
}

/// A user's session cookie value: `u.<id>.<epoch>.<expiry>.<signature>`. Raising the user's
/// `session_epoch` ends every session signed before.
pub fn session_value(hub: &Hub, id: &str, epoch: i64) -> String {
    let exp = crate::util::now_unix() + SESSION_TTL;
    format!("u.{id}.{epoch}.{exp}.{}", sign(hub, &format!("user-session:{id}:{epoch}:{exp}")))
}

async fn user_session(hub: &Hub, value: &str) -> Option<Caller> {
    let mut parts = value.strip_prefix("u.")?.splitn(4, '.');
    let (id, epoch, exp, sig) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    let (e, x): (i64, i64) = (epoch.parse().ok()?, exp.parse().ok()?);
    if x <= crate::util::now_unix() || !crate::util::ct_eq(sig.as_bytes(), sign(hub, &format!("user-session:{id}:{epoch}:{exp}")).as_bytes()) {
        return None;
    }
    let id = id.to_owned();
    hub.store
        .call(move |c| {
            let current: Option<i64> = c.query_row("SELECT session_epoch FROM users WHERE id = ?1 AND disabled_at IS NULL", [&id], |r| r.get(0)).optional()?;
            if current != Some(e) {
                return Ok(None);
            }
            let Some((name, rights)) = user_rights(c, &id)? else { return Ok(None) };
            Ok(Some(Caller { kind: Kind::User, name: format!("user:{name}"), user: Some(id), token: None, agent: None, rights: Some(rights), limit: None }))
        })
        .await
        .unwrap_or_else(store_err)
}

/// A user's sign-in link: `u.<id>.<secret>`; only the newest works, once, for [`LINK_TTL`].
pub async fn new_link(hub: &Hub, id: &str) -> anyhow::Result<Option<String>> {
    let secret = crate::util::random_token();
    let hash = crate::util::sha256_hex(secret.as_bytes());
    let uid = id.to_owned();
    let n = hub
        .store
        .call(move |c| {
            Ok(c.execute("UPDATE users SET link_hash = ?2, link_expires_at = ?3 WHERE id = ?1 AND disabled_at IS NULL", params![uid, hash, crate::util::now_unix() + LINK_TTL])?)
        })
        .await?;
    Ok((n > 0).then(|| format!("u.{id}.{secret}")))
}

/// The session a sign-in link opens (and the link is used up), if it is the user's live link.
pub async fn redeem_link(hub: &Hub, code: &str) -> Option<(String, String)> {
    let (id, secret) = code.strip_prefix("u.")?.split_once('.')?;
    let (id, hash) = (id.to_owned(), crate::util::sha256_hex(secret.as_bytes()));
    let found = hub
        .store
        .call(move |c| {
            let now = crate::util::now_unix();
            let row: Option<(String, i64, String)> = c
                .query_row(
                    "SELECT name, session_epoch, link_hash FROM users WHERE id = ?1 AND disabled_at IS NULL AND link_expires_at > ?2",
                    params![id, now],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some((name, epoch, stored)) = row.filter(|(_, _, h)| crate::util::ct_eq(h.as_bytes(), hash.as_bytes())) else { return Ok(None) };
            c.execute("UPDATE users SET link_hash = NULL, link_expires_at = NULL WHERE id = ?1 AND link_hash = ?2", params![id, stored])?;
            Ok(Some((id, name, epoch)))
        })
        .await
        .ok()
        .flatten()?;
    let (id, name, epoch) = found;
    Some((session_value(hub, &id, epoch), name))
}

// ------------------------------------------------------------------ may they

/// The scope a request's rule is checked in.
async fn target(hub: &Hub, at: At, segs: &[&str], q: &HashMap<String, String>) -> Scope {
    let seg = |i: usize| segs.get(i).copied().unwrap_or_default().to_owned();
    let one = |sql: &'static str, id: String| async move { hub.store.call(move |c| Ok(c.query_row(sql, [&id], |r| r.get::<_, String>(0)).optional()?)).await.ok().flatten() };
    let repo_of = |name: Option<String>| name.map_or(Scope::Server, |repo| Scope::Repo { repo });
    match at {
        At::Repo(i) => repo_scope(hub, &seg(i)).await,
        At::Run(i) => repo_of(one("SELECT p.name FROM runs r JOIN repos p ON p.id = r.repo_id WHERE r.id = ?1", seg(i)).await),
        At::Job(i) => repo_of(one("SELECT p.name FROM jobs j JOIN runs r ON r.id = j.run_id JOIN repos p ON p.id = r.repo_id WHERE j.id = ?1", seg(i)).await),
        At::Runner(i) => Scope::Runner { runner: seg(i) },
        At::Diff(i) => {
            let key = seg(i);
            let (kind, number) = crate::ci::diffs::key_of(&key);
            let name = hub
                .store
                .call(move |c| Ok(c.query_row("SELECT p.name FROM diffs d JOIN repos p ON p.id = d.repo_id WHERE d.id = ?1 OR (d.number = ?2 AND (?3 IS NULL OR d.kind = ?3))", rusqlite::params![key, number, kind], |r| r.get::<_, String>(0)).optional()?))
                .await
                .ok()
                .flatten();
            repo_of(name)
        }
        At::Stack(i) => repo_of(one("SELECT p.name FROM stacks s JOIN repos p ON p.id = s.repo_id WHERE s.id = ?1", seg(i)).await),
        At::Comment(i) => repo_of(one("SELECT p.name FROM review_comments c JOIN review_threads t ON t.id = c.thread_id JOIN diffs d ON d.id = t.diff_id JOIN repos p ON p.id = d.repo_id WHERE c.id = ?1", seg(i)).await),
        At::Entry(i) => repo_of(one("SELECT p.name FROM land_queue q JOIN repos p ON p.id = q.repo_id WHERE q.id = ?1", seg(i)).await),
        At::ScopeQuery => match q.get("scope").map(|s| Scope::parse(s)) {
            Some(Ok(s)) => match s.repo().map(str::to_owned) {
                Some(r) => {
                    let name = match repo_scope(hub, &r).await {
                        Scope::Repo { repo } => repo,
                        _ => r,
                    };
                    s.with_repo(name)
                }
                None => s,
            },
            _ => Scope::Server,
        },
        At::RepoQuery => match q.get("repo").filter(|r| !r.is_empty()) {
            Some(r) => repo_scope(hub, r).await,
            None => Scope::Server,
        },
        At::Server | At::Anywhere | At::Own => Scope::Server,
    }
}

/// The scope `rule` is checked in for this request (W19-R05's audit rows name it).
pub async fn scope_of(hub: &Hub, rule: &super::Rule, segs: &[&str], q: &HashMap<String, String>) -> Scope {
    target(hub, rule.at, segs, q).await
}

/// A repository named by id or `owner/repo`, as a scope with its name.
pub async fn repo_scope(hub: &Hub, s: &str) -> Scope {
    let key = s.to_owned();
    let name = hub
        .store
        .call(move |c| Ok(c.query_row("SELECT name FROM repos WHERE id = ?1 OR name = ?1 COLLATE NOCASE", [&key], |r| r.get::<_, String>(0)).optional()?))
        .await
        .ok()
        .flatten();
    Scope::Repo { repo: name.unwrap_or_else(|| s.to_owned()) }
}

/// Whether the rule `rule` lets `caller` through in `target` (`None`: the rule's scope is
/// anywhere, or the caller's own).
pub fn decide(caller: &Caller, rule: &super::Rule, target: Option<&Scope>) -> bool {
    match (rule.at, target) {
        (At::Anywhere, _) => caller.has_somewhere(rule.capability),
        (At::Own, _) => caller.kind != Kind::Job,
        (_, Some(t)) => caller.can(rule.capability, t),
        (_, None) => caller.can(rule.capability, &Scope::Server),
    }
}

/// Whether `caller` may make this request (the owner always may); `Err` says what it lacks.
pub async fn check(hub: &Hub, caller: &Caller, method: &Method, segs: &[&str], q: &HashMap<String, String>) -> Result<(), (StatusCode, String)> {
    if caller.is_owner() {
        return Ok(());
    }
    let rule = super::rule(method, segs);
    let scope = match rule.at {
        At::Anywhere | At::Own => None,
        at => Some(target(hub, at, segs, q).await),
    };
    if decide(caller, rule, scope.as_ref()) {
        return Ok(());
    }
    let on = scope.map(|s| format!(" on {}", s.text())).unwrap_or_default();
    Err((StatusCode::FORBIDDEN, format!("{} may not {method} /{}: that needs {}{on}", caller.name, segs.join("/"), rule.capability)))
}

#[cfg(test)]
mod tests {
    use super::super::{CAPABILITIES, ROLES, ROUTES, role};
    use super::*;

    fn rights(grants: &[&str]) -> Rights {
        Rights { grants: grants.iter().filter_map(|g| Grant::parse(g).ok()).collect(), custom: HashMap::new() }
    }

    /// W10-AC02, generated from the table: for every route, each built-in role (held on the
    /// server, and on the repository the route is about) and an agent whose token carries a
    /// reduced grant are let through exactly when the route's capability is theirs in that scope.
    #[test]
    fn w10_ac02_every_route_for_every_role_and_a_reduced_agent() {
        let app = Scope::Repo { repo: "acme/app".into() };
        let other = Scope::Repo { repo: "acme/other".into() };
        let mut checked = 0;
        for rule in ROUTES {
            for name in ROLES {
                let caps = role(name).unwrap_or_default();
                let has = |cap: &str| cap == SIGNED_IN || caps.contains(&cap);
                // On the server: everything the role has, everywhere.
                let server = Caller::with(Kind::User, name, Some(rights(&[name])), None);
                for t in [&Scope::Server, &app] {
                    assert_eq!(decide(&server, rule, Some(t)), has(rule.capability) || rule.at == At::Own, "{name}@server: {:?} {} in {}", rule.method, rule.pattern, t.text());
                }
                // On one repository: that repository only (and the routes that narrow themselves).
                let scoped = Caller::with(Kind::User, name, Some(rights(&[&format!("{name}@repo:acme/app")])), None);
                let narrows = matches!(rule.at, At::Anywhere | At::Own);
                assert_eq!(decide(&scoped, rule, Some(&app)), has(rule.capability) || rule.at == At::Own, "{name}@repo: {} on its repository", rule.pattern);
                assert_eq!(decide(&scoped, rule, Some(&other)), rule.capability == SIGNED_IN || rule.at == At::Own || (narrows && has(rule.capability)), "{name}@repo: {} elsewhere", rule.pattern);
                assert_eq!(decide(&scoped, rule, Some(&Scope::Server)), rule.capability == SIGNED_IN || rule.at == At::Own || (narrows && has(rule.capability)), "{name}@repo: {} on the server", rule.pattern);
                checked += 1;
            }
            // An agent for a maintainer, its token reduced to reading: reads, and nothing more.
            let agent = Caller::with(Kind::Agent, "agent:fix", Some(rights(&["maintainer"])), Some(rights(&["viewer@repo:acme/app"])));
            let reads = role("viewer").unwrap_or_default().contains(&rule.capability) || rule.capability == SIGNED_IN;
            assert_eq!(decide(&agent, rule, Some(&app)), reads || rule.at == At::Own, "agent: {} on its repository", rule.pattern);
            let narrows = matches!(rule.at, At::Anywhere | At::Own);
            assert_eq!(decide(&agent, rule, Some(&other)), rule.capability == SIGNED_IN || rule.at == At::Own || (narrows && reads), "agent: {} elsewhere", rule.pattern);
            // A deny wins over the role.
            let denied = Caller::with(Kind::User, "d", Some(rights(&["admin", &format!("!{}@repo:acme/app", rule.capability)])), None);
            if rule.capability != SIGNED_IN && !narrows {
                assert!(!decide(&denied, rule, Some(&app)) && decide(&denied, rule, Some(&other)), "deny: {}", rule.pattern);
            }
        }
        assert_eq!(checked, ROUTES.len() * ROLES.len());
        // An owner's token without a grant is the owner; with one, it is held to it.
        assert!(Caller::with(Kind::Pat, "pat", None, None).is_owner());
        let reduced = Caller::with(Kind::Pat, "pat", None, Some(rights(&["runs.read@repo:acme/app"])));
        assert!(reduced.can("runs.read", &app) && !reduced.can("runs.read", &other) && !reduced.can("runs.cancel", &app));
        assert!(CAPABILITIES.iter().all(|c| Caller::owner().can(c, &Scope::Server)));
    }

    /// W10-R03: the runner masks every prefix kiln mints.
    #[test]
    fn w10_r03_the_masker_knows_every_prefix() {
        for p in [PAT_PREFIX, AGENT_PREFIX, crate::ci::token::JOB_PREFIX] {
            assert!(kiln_ci::runner::mask::TOKEN_PREFIXES.contains(&p), "{p}");
        }
    }
}
