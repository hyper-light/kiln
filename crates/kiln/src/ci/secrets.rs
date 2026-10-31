//! Secrets and variables on the server (docs/ci.md W09).
//!
//! - **Encryption (W09-R03).** Every value is sealed with AES-256-GCM, with a fresh nonce, under
//!   a data key. The data key is wrapped by the master key file (`<data>/secrets/master.key`,
//!   32 random bytes, mode 0600, made at first use). A ciphertext is bound to its scope and name,
//!   so one can't be moved to another row. The database holds no plaintext, and nothing here
//!   returns a value except to a leg.
//! - **Resolution (W09-R01).** A name resolves to its most specific scope: environment, then
//!   workflow, then repository, then organization, then server. The `[secrets]` of the server's
//!   config count as server scope, below stored ones.
//! - **Uses (W09-R04).** Every delivery to a leg is recorded.
//! - **Variables (W09-R09).** The same scopes, in plain text.
//! - **U09.** Values, versions, read history and rotation use `secrets::kiln`; CI retains only
//!   scope/broker/delivery metadata. Legacy ciphertext is imported before its first use.
//! - **Rotation.** [`rotate`] re-seals CI and deployment values under one new data key.

use std::collections::BTreeMap;

use kiln_ci::secrets::{self as rules, ScopeKind, Used};
use crate::sql::{OptionalExtension, params};

use crate::hub::Hub;
use crate::store::Repo;
use crate::secrets::kiln as values;

/// A scope, as stored: its kind and id (`""`, an owner, a repository id, `<repo id>:<path>` or
/// `<repo id>:<environment>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub kind: ScopeKind,
    pub id: String,
}

impl Scope {
    pub(crate) fn key(&self) -> String {
        format!("ci/{}/{}", self.kind.as_str(), self.id)
    }

    #[cfg(test)]
    fn aad(&self, name: &str) -> Vec<u8> {
        format!("kiln secret\0{}\0{}\0{name}", self.kind.as_str(), self.id).into_bytes()
    }
}

/// A scope as people write it: `server`, `org:<owner>`, `repo:<owner/name or id>`,
/// `workflow:<repo>:<path>`, `env:<repo>:<environment>`.
pub async fn parse_scope(hub: &Hub, s: &str) -> Result<Scope, String> {
    let s = s.trim();
    if s == "server" || s.is_empty() {
        return Ok(Scope { kind: ScopeKind::Server, id: String::new() });
    }
    let (kind, rest) = s.split_once(':').ok_or_else(|| format!("`{s}` is not a scope: server, org:<owner>, repo:<owner/name>, workflow:<repo>:<path> or env:<repo>:<name>"))?;
    let repo = |name: &str| {
        let name = name.to_owned();
        async move {
            let repos = hub.store.repos().await.map_err(|e| format!("{e:#}"))?;
            repos.into_iter().find(|r| r.id == name || r.name.eq_ignore_ascii_case(&name)).ok_or_else(|| format!("no repository `{name}`"))
        }
    };
    match ScopeKind::parse(kind) {
        Some(ScopeKind::Organization) if !rest.is_empty() => Ok(Scope { kind: ScopeKind::Organization, id: rest.to_ascii_lowercase() }),
        Some(ScopeKind::Repository) => Ok(Scope { kind: ScopeKind::Repository, id: repo(rest).await?.id }),
        Some(k @ (ScopeKind::Workflow | ScopeKind::Environment)) => {
            let (r, tail) = rest.split_once(':').ok_or_else(|| format!("`{s}`: say {kind}:<repo>:<{}>", if k == ScopeKind::Workflow { "path" } else { "environment" }))?;
            if tail.is_empty() {
                return Err(format!("`{s}` names no {}", if k == ScopeKind::Workflow { "workflow" } else { "environment" }));
            }
            Ok(Scope { kind: k, id: format!("{}:{tail}", repo(r).await?.id) })
        }
        _ => Err(format!("`{s}` is not a scope: server, org:<owner>, repo:<owner/name>, workflow:<repo>:<path> or env:<repo>:<name>")),
    }
}

/// The scopes a leg of `repo`'s `workflow` reads, bound to `environment`, from the most
/// specific.
fn scopes_for(repo: &Repo, workflow: &str, environment: Option<&str>) -> Vec<Scope> {
    let owner = repo.name.split_once('/').map_or(repo.name.as_str(), |(o, _)| o).to_ascii_lowercase();
    let mut v = Vec::new();
    if let Some(e) = environment.filter(|e| !e.is_empty()) {
        v.push(Scope { kind: ScopeKind::Environment, id: format!("{}:{e}", repo.id) });
    }
    v.push(Scope { kind: ScopeKind::Workflow, id: format!("{}:{workflow}", repo.id) });
    v.push(Scope { kind: ScopeKind::Repository, id: repo.id.clone() });
    v.push(Scope { kind: ScopeKind::Organization, id: owner });
    v.push(Scope { kind: ScopeKind::Server, id: String::new() });
    v
}

// ------------------------------------------------------------------ keys

pub(crate) use crate::secrets::crypto::{derived, open_master, seal_master};
use crate::secrets::crypto::master;
#[cfg(test)]
use crate::secrets::crypto::{open, seal};
#[cfg(test)]
use crate::secrets::crypto::random;

// ------------------------------------------------------------------ secrets

/// A secret as reads describe it: never its value.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Meta {
    pub name: String,
    pub size: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_used_at: Option<i64>,
    /// A brokered secret's source (W09-R08); its value is made at leg start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

/// Sets a secret (W09-R02, W09-R03).
pub async fn set(hub: &Hub, scope: &Scope, name: &str, value: &str, by: &str) -> Result<(), String> {
    store_row(hub, scope, name, value, None, by).await
}

/// Stores a secret's row: a value sealed, or a brokered one's source (its value empty).
async fn store_row(hub: &Hub, scope: &Scope, name: &str, value: &str, from: Option<String>, by: &str) -> Result<(), String> {
    let name = rules::check_name(name)?;
    if value.len() > rules::MAX_SECRET {
        return Err(format!("a secret is at most {} KiB", rules::MAX_SECRET >> 10));
    }
    let master = master(hub).map_err(|e| format!("{e:#}"))?;
    let value = crate::secrets::Value::new(value.as_bytes().to_vec());
    let (s, by, size) = (scope.clone(), by.to_owned(), i64::try_from(value.len()).unwrap_or(0));
    hub.store
        .call(move |c| {
            values::import_ci(&master, c)?;
            let tx = crate::store::tx(c)?;
            let n: i64 = tx.query_row("SELECT COUNT(*) FROM ci_secrets WHERE scope_kind = ?1 AND scope_id = ?2 AND name != ?3", params![s.kind.as_str(), s.id, name], |r| r.get(0))?;
            if usize::try_from(n).unwrap_or(usize::MAX) >= rules::MAX_PER_SCOPE {
                anyhow::bail!("a scope holds at most {} secrets", rules::MAX_PER_SCOPE);
            }
            let now = crate::util::now_unix();
            values::put_value(&master, &tx, &s.key(), &name, value, &by, from.as_deref())?;
            tx.execute(
                "INSERT INTO ci_secrets (id, scope_kind, scope_id, name, size, created_by, created_at, updated_at, broker) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8)
                 ON CONFLICT (scope_kind, scope_id, name) DO UPDATE SET size = excluded.size, updated_at = excluded.updated_at, broker = excluded.broker",
                params![crate::util::new_id(), s.kind.as_str(), s.id, name, size, by, now, from],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Defines a secret as brokered (W09-R08): `from` says where its value comes from at leg start.
pub async fn set_brokered(hub: &Hub, scope: &Scope, name: &str, from: &str, by: &str) -> Result<(), String> {
    super::broker::parse(from)?;
    store_row(hub, scope, name, "", Some(from.trim().to_owned()), by).await
}

pub async fn delete(hub: &Hub, scope: &Scope, name: &str) -> anyhow::Result<bool> {
    let (s, name, master) = (scope.clone(), name.to_ascii_uppercase(), master(hub)?);
    hub.store.call(move |c| {
        values::import_ci(&master, c)?;
        let tx = crate::store::tx(c)?;
        let changed = tx.execute("DELETE FROM ci_secrets WHERE scope_kind = ?1 AND scope_id = ?2 AND name = ?3", params![s.kind.as_str(), s.id, name])? > 0;
        if changed {
            values::delete_value(&tx, &s.key(), &name, "admin")?;
        }
        tx.commit()?;
        Ok(changed)
    }).await
}

pub async fn list(hub: &Hub, scope: &Scope) -> anyhow::Result<Vec<Meta>> {
    let s = scope.clone();
    hub.store
        .call(move |c| {
            let mut q = c.prepare("SELECT name, size, created_at, updated_at, last_used_at, broker FROM ci_secrets WHERE scope_kind = ?1 AND scope_id = ?2 ORDER BY name")?;
            let rows = q.query_map(params![s.kind.as_str(), s.id], |r| Ok(Meta { name: r.get(0)?, size: r.get(1)?, created_at: r.get(2)?, updated_at: r.get(3)?, last_used_at: r.get(4)?, from: r.get(5)? }))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

/// One secret's value, decrypted, for the server's own use: an integration's credential (W11),
/// which lives in [`ScopeKind::Integration`] and is never resolved for a leg.
pub(crate) async fn reveal(hub: &Hub, scope: &Scope, name: &str) -> anyhow::Result<Option<String>> {
    let (s, name, master) = (scope.clone(), name.to_ascii_uppercase(), master(hub)?);
    hub.store.call(move |c| {
        values::import_ci(&master, c)?;
        let exists: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM ci_secrets WHERE scope_kind = ?1 AND scope_id = ?2 AND name = ?3)", params![s.kind.as_str(), s.id, name], |r| r.get(0))?;
        if !exists { return Ok(None) }
        let version = values::latest(c, &s.key(), &name)?.ok_or_else(|| anyhow::anyhow!("secret version is missing"))?;
        let plain = values::read_value(&master, c, &s.key(), &name, version)?;
        values::record_read(c, &s.key(), "integration", "credential broker", &format!("{name}@{version}"))?;
        Ok(Some(String::from_utf8_lossy(plain.bytes()).into_owned()))
    }).await
}

/// A resolved secret: its value, and its row (`None` for the config's).
#[derive(Clone)]
pub struct Resolved {
    pub value: String,
    pub id: Option<String>,
    pub scope: ScopeKind,
}

impl std::fmt::Debug for Resolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolved").field("value", &"<redacted>").field("id", &self.id).field("scope", &self.scope).finish()
    }
}

/// The secrets a leg of `repo`'s `workflow` (bound to `environment`) may draw on, resolved by
/// scope and decrypted: the names in `wanted`, or all of them (W09-R01, W09-R04). Brokered ones
/// (W09-R08) are made now, for the leg acting as `w`; those that couldn't be, with why, come
/// second.
pub async fn for_leg(hub: &Hub, repo: &Repo, workflow: &str, environment: Option<&str>, wanted: &Used, w: &kiln_ci::oidc::Workload<'_>) -> anyhow::Result<(BTreeMap<String, Resolved>, Vec<(String, String)>)> {
    let scopes = scopes_for(repo, workflow, environment);
    let wanted = wanted.clone();
    let master = master(hub)?;
    let (all, names) = (wanted.all, wanted.names.clone());
    let (by, purpose) = (w.actor.to_owned(), format!("CI resolution {} {workflow}", repo.name));
    let (mut stored, brokered): (BTreeMap<String, Resolved>, Vec<(String, String, String, ScopeKind)>) = hub
        .store
        .call(move |c| {
            values::import_ci(&master, c)?;
            let mut out: BTreeMap<String, Resolved> = BTreeMap::new();
            let mut brokered: Vec<(String, String, String, ScopeKind)> = Vec::new();
            // From the least specific up, so the most specific wins.
            for s in scopes.iter().rev() {
                let mut q = c.prepare("SELECT id, name, broker FROM ci_secrets WHERE scope_kind = ?1 AND scope_id = ?2")?;
                let rows = q.query_map(params![s.kind.as_str(), s.id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?)))?;
                for row in rows {
                    let (id, name, broker) = row?;
                    if !all && !names.contains(&name) {
                        continue;
                    }
                    if let Some(from) = broker {
                        // A more specific scope's wins here too.
                        out.remove(&name);
                        brokered.retain(|b| b.0 != name);
                        brokered.push((name, from, id, s.kind));
                        continue;
                    }
                    brokered.retain(|b| b.0 != name);
                    let version = values::latest(c, &s.key(), &name)?.ok_or_else(|| anyhow::anyhow!("secret version is missing"))?;
                    let plain = values::read_value(&master, c, &s.key(), &name, version)?;
                    values::record_read(c, &s.key(), &by, &purpose, &format!("{name}@{version}"))?;
                    out.insert(name, Resolved { value: String::from_utf8_lossy(plain.bytes()).into_owned(), id: Some(id), scope: s.kind });
                }
            }
            Ok((out, brokered))
        })
        .await?;
    let mut failed = Vec::new();
    for (name, from, id, scope) in brokered {
        let made = match super::broker::parse(&from) {
            Ok(src) => super::broker::value(hub, repo, w, &src).await.map_err(|e| format!("{e:#}")),
            Err(e) => Err(e),
        };
        match made {
            Ok(value) => {
                stored.insert(name, Resolved { value, id: Some(id), scope });
            }
            Err(e) => failed.push((name, format!("from {from}: {e}"))),
        }
    }
    // The config's `[secrets]`: server scope, beneath stored ones.
    let mut out: BTreeMap<String, Resolved> = BTreeMap::new();
    for (k, v) in hub.ci_secrets() {
        let name = k.to_ascii_uppercase();
        if wanted.all || wanted.names.contains(&name) {
            out.insert(name, Resolved { value: v, id: None, scope: ScopeKind::Server });
        }
    }
    for (name, r) in stored {
        let keep = out.get(&name).is_none_or(|have| r.scope >= have.scope);
        if keep {
            out.insert(name, r);
        }
    }
    Ok((out, failed))
}

/// The names a leg of `repo`'s `workflow` bound to `environment` could resolve, without
/// decrypting anything (W09-R06's check).
pub async fn names_for(hub: &Hub, repo: &Repo, workflow: &str, environment: Option<&str>) -> anyhow::Result<std::collections::BTreeSet<String>> {
    let scopes = scopes_for(repo, workflow, environment);
    let mut names: std::collections::BTreeSet<String> = hub
        .store
        .call(move |c| {
            let mut out = std::collections::BTreeSet::new();
            for s in &scopes {
                let mut q = c.prepare("SELECT name FROM ci_secrets WHERE scope_kind = ?1 AND scope_id = ?2")?;
                let rows = q.query_map(params![s.kind.as_str(), s.id], |r| r.get::<_, String>(0))?;
                for r in rows {
                    out.insert(r?);
                }
            }
            Ok(out)
        })
        .await?;
    names.extend(hub.ci_secrets().into_iter().map(|(k, _)| k.to_ascii_uppercase()));
    Ok(names)
}

/// The scopes searched for a leg, as people read them.
pub fn searched(repo: &Repo, workflow: &str, environment: Option<&str>) -> String {
    let owner = repo.name.split_once('/').map_or(repo.name.as_str(), |(o, _)| o);
    let mut v = Vec::new();
    if let Some(e) = environment {
        v.push(format!("environment `{e}`"));
    }
    v.push(format!("workflow `{workflow}`"));
    v.push(format!("repository {}", repo.name));
    v.push(format!("organization {owner}"));
    v.push("server".into());
    v.join(", ")
}

/// Records that `leg` was given these secrets (W09-R04).
pub async fn record_uses(hub: &Hub, leg: &str, ids: Vec<String>) -> anyhow::Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let leg = leg.to_owned();
    let l = leg.clone();
    let names: Vec<String> = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let now = crate::util::now_unix();
            let mut names = Vec::new();
            for id in &ids {
                tx.execute("INSERT OR REPLACE INTO ci_secret_uses (secret_id, leg_id, ts) VALUES (?1, ?2, ?3)", params![id, l, now])?;
                tx.execute("UPDATE ci_secrets SET last_used_at = ?2 WHERE id = ?1", params![id, now])?;
                if let Some(n) = tx.query_row("SELECT name FROM ci_secrets WHERE id = ?1", [id], |r| r.get::<_, String>(0)).optional()? {
                    names.push(n);
                }
            }
            tx.commit()?;
            Ok(names)
        })
        .await?;
    // A secret's use is audited (W19-R05): which leg was given which, by name.
    hub.store.audit(format!("job:{leg}"), "ci.secrets.use".into(), names.join(", ")).await;
    Ok(())
}

/// Re-seals every secret under a new data key and retires the old ones: how many moved.
pub async fn rotate(hub: &Hub) -> anyhow::Result<usize> {
    let master = master(hub)?;
    hub.store.call(move |c| values::rotate_values(&master, c)).await
}

// ------------------------------------------------------------------ variables

#[derive(Debug, Clone, serde::Serialize)]
pub struct Variable {
    pub name: String,
    pub value: String,
    pub updated_at: i64,
}

pub async fn set_var(hub: &Hub, scope: &Scope, name: &str, value: &str, by: &str) -> Result<(), String> {
    let name = rules::check_name(name)?;
    if value.len() > rules::MAX_VARIABLE {
        return Err(format!("a variable is at most {} KiB", rules::MAX_VARIABLE >> 10));
    }
    let (s, value, by) = (scope.clone(), value.to_owned(), by.to_owned());
    hub.store
        .call(move |c| {
            let n: i64 = c.query_row("SELECT COUNT(*) FROM ci_variables WHERE scope_kind = ?1 AND scope_id = ?2 AND name != ?3", params![s.kind.as_str(), s.id, name], |r| r.get(0))?;
            if usize::try_from(n).unwrap_or(usize::MAX) >= rules::MAX_PER_SCOPE {
                anyhow::bail!("a scope holds at most {} variables", rules::MAX_PER_SCOPE);
            }
            c.execute(
                "INSERT INTO ci_variables (scope_kind, scope_id, name, value, updated_by, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (scope_kind, scope_id, name) DO UPDATE SET value = excluded.value, updated_by = excluded.updated_by, updated_at = excluded.updated_at",
                params![s.kind.as_str(), s.id, name, value, by, crate::util::now_unix()],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| format!("{e:#}"))
}

pub async fn delete_var(hub: &Hub, scope: &Scope, name: &str) -> anyhow::Result<bool> {
    let (s, name) = (scope.clone(), name.to_ascii_uppercase());
    hub.store.call(move |c| Ok(c.execute("DELETE FROM ci_variables WHERE scope_kind = ?1 AND scope_id = ?2 AND name = ?3", params![s.kind.as_str(), s.id, name])? > 0)).await
}

pub async fn list_vars(hub: &Hub, scope: &Scope) -> anyhow::Result<Vec<Variable>> {
    let s = scope.clone();
    hub.store
        .call(move |c| {
            let mut q = c.prepare("SELECT name, value, updated_at FROM ci_variables WHERE scope_kind = ?1 AND scope_id = ?2 ORDER BY name")?;
            let rows = q.query_map(params![s.kind.as_str(), s.id], |r| Ok(Variable { name: r.get(0)?, value: r.get(1)?, updated_at: r.get(2)? }))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

/// The `vars` context of `repo`'s `workflow` bound to `environment`: every scope's, the most
/// specific winning (W09-R09).
pub async fn vars_for(hub: &Hub, repo: &Repo, workflow: &str, environment: Option<&str>) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    let scopes = scopes_for(repo, workflow, environment);
    hub.store
        .call(move |c| {
            let mut out = serde_json::Map::new();
            for s in scopes.iter().rev() {
                let mut q = c.prepare("SELECT name, value FROM ci_variables WHERE scope_kind = ?1 AND scope_id = ?2")?;
                let rows = q.query_map(params![s.kind.as_str(), s.id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
                for row in rows {
                    let (n, v) = row?;
                    out.insert(n, serde_json::Value::String(v));
                }
            }
            Ok(out)
        })
        .await
}

// ------------------------------------------------------------------ HTTP (W09-R11)

#[derive(serde::Deserialize)]
struct Value {
    value: String,
}

/// A secret as set: its `value`, or where it comes `from` (W09-R08).
#[derive(serde::Deserialize)]
struct SecretBody {
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    from: Option<String>,
}

fn fail(status: http::StatusCode, m: impl Into<String>) -> super::Fail {
    (status, m.into())
}

async fn scope_of(hub: &Hub, q: &std::collections::HashMap<String, String>) -> Result<(Scope, String), super::Fail> {
    let text = q.get("scope").cloned().unwrap_or_else(|| "server".into());
    let scope = parse_scope(hub, &text).await.map_err(|e| fail(http::StatusCode::BAD_REQUEST, e))?;
    Ok((scope, text))
}

/// `/api/ci/secrets…` and `/api/ci/variables…`, behind the console's auth. Reads never carry a
/// secret's value.
pub async fn route(hub: &Hub, method: &http::Method, rest: &[&str], q: &std::collections::HashMap<String, String>, body: kiln_http::Incoming) -> Result<crate::http::server::Response, super::Fail> {
    use crate::http::server as srv;
    use http::{Method, StatusCode};
    let internal = |e: anyhow::Error| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    match (method, rest) {
        (&Method::GET, ["secrets"]) => {
            let (scope, text) = scope_of(hub, q).await?;
            let list = list(hub, &scope).await.map_err(internal)?;
            Ok(srv::json(StatusCode::OK, &serde_json::json!({ "scope": text, "secrets": list })))
        }
        (&Method::POST, ["secrets", "rotate"]) => {
            let n = rotate(hub).await.map_err(internal)?;
            hub.store.audit("admin".into(), "ci.secrets.rotate".into(), format!("re-sealed {n} secrets under a new data key")).await;
            Ok(srv::json(StatusCode::OK, &serde_json::json!({ "resealed": n })))
        }
        (&Method::PUT, ["secrets", name]) => {
            let (scope, text) = scope_of(hub, q).await?;
            let v: SecretBody = srv::body_json(body, rules::MAX_SECRET * 2 + 1024).await.map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            let detail = match (&v.value, &v.from) {
                (Some(value), None) => {
                    set(hub, &scope, name, value, "admin").await.map_err(|e| fail(StatusCode::UNPROCESSABLE_ENTITY, e))?;
                    format!("{} in {text}", name.to_ascii_uppercase())
                }
                // W09-R08: brokered, its value made at leg start.
                (None, Some(from)) => {
                    set_brokered(hub, &scope, name, from, "admin").await.map_err(|e| fail(StatusCode::UNPROCESSABLE_ENTITY, e))?;
                    format!("{} in {text}, from {}", name.to_ascii_uppercase(), from.trim())
                }
                _ => return Err(fail(StatusCode::BAD_REQUEST, "a secret has a `value`, or says where it comes `from`")),
            };
            hub.store.audit("admin".into(), "ci.secret.set".into(), detail).await;
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        (&Method::DELETE, ["secrets", name]) => {
            let (scope, text) = scope_of(hub, q).await?;
            if !delete(hub, &scope, name).await.map_err(internal)? {
                return Err(fail(StatusCode::NOT_FOUND, format!("no secret {} in {text}", name.to_ascii_uppercase())));
            }
            hub.store.audit("admin".into(), "ci.secret.delete".into(), format!("{} in {text}", name.to_ascii_uppercase())).await;
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        (&Method::GET, ["variables"]) => {
            let (scope, text) = scope_of(hub, q).await?;
            let list = list_vars(hub, &scope).await.map_err(internal)?;
            Ok(srv::json(StatusCode::OK, &serde_json::json!({ "scope": text, "variables": list })))
        }
        (&Method::PUT, ["variables", name]) => {
            let (scope, text) = scope_of(hub, q).await?;
            let v: Value = srv::body_json(body, rules::MAX_VARIABLE * 2 + 1024).await.map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            set_var(hub, &scope, name, &v.value, "admin").await.map_err(|e| fail(StatusCode::UNPROCESSABLE_ENTITY, e))?;
            hub.store.audit("admin".into(), "ci.variable.set".into(), format!("{} in {text}", name.to_ascii_uppercase())).await;
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        (&Method::DELETE, ["variables", name]) => {
            let (scope, text) = scope_of(hub, q).await?;
            if !delete_var(hub, &scope, name).await.map_err(internal)? {
                return Err(fail(StatusCode::NOT_FOUND, format!("no variable {} in {text}", name.to_ascii_uppercase())));
            }
            hub.store.audit("admin".into(), "ci.variable.delete".into(), format!("{} in {text}", name.to_ascii_uppercase())).await;
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        _ => Err(fail(StatusCode::NOT_FOUND, "no such route")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// U09: imported CI/integration ciphertext and app pins share one rotation owner.
    #[test]
    fn u09_import_scope_versions_rotation_and_redaction() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let scope = Scope { kind: ScopeKind::Integration, id: "broker-one".into() };
            let master = master(&hub)?;
            let old: [u8; 32] = random()?;
            let (wrapped, key_nonce) = seal(&master, &old, b"kiln data key\0legacy")?;
            let (sealed, nonce) = seal(&old, b"legacy-credential", b"kiln secret\0integration\0broker-one\0TOKEN")?;
            hub.store.call(move |c| {
                c.execute("INSERT INTO ci_keys (id, wrapped, nonce, created_at) VALUES ('legacy', ?1, ?2, 1)", params![wrapped, key_nonce])?;
                c.execute("INSERT INTO ci_secrets (id, scope_kind, scope_id, name, ciphertext, nonce, key_id, size, created_by, created_at, updated_at) VALUES ('old', 'integration', 'broker-one', 'TOKEN', ?1, ?2, 'legacy', 17, 'ada', 1, 1)", params![sealed, vec![0u8]])?;
                Ok(())
            }).await?;
            anyhow::ensure!(reveal(&hub, &scope, "TOKEN").await.is_err(), "corrupt import is refused");
            hub.store.call(move |c| {
                let copied: i64 = c.query_row("SELECT COUNT(*) FROM secret_versions", [], |r| r.get(0))?;
                anyhow::ensure!(copied == 0, "failed import keeps no partial replacement");
                c.execute("UPDATE ci_secrets SET nonce = ?1 WHERE id = 'old'", [nonce])?;
                Ok(())
            }).await?;
            anyhow::ensure!(reveal(&hub, &scope, "TOKEN").await?.as_deref() == Some("legacy-credential"));
            // Repeated reads do not import another version, and scopes cannot exchange values.
            anyhow::ensure!(reveal(&hub, &scope, "TOKEN").await?.is_some());
            let other = Scope { kind: ScopeKind::Integration, id: "other-broker".into() };
            anyhow::ensure!(reveal(&hub, &other, "TOKEN").await?.is_none());
            set(&hub, &scope, "TOKEN", "replacement-credential", "ada").await.map_err(anyhow::Error::msg)?;
            let app = crate::secrets::Scope::of("web", "staging");
            let service = crate::secrets::of(&hub);
            let ada = crate::secrets::Caller { name: "ada".into(), agent: false };
            service.put(&app, "TOKEN", crate::secrets::Value::new(b"app-credential".to_vec()), &ada).await?;
            anyhow::ensure!(rotate(&hub).await? == 3, "both CI versions and the pinned app version rotate");
            let scope_key = scope.key();
            hub.store.call(move |c| {
                let count: i64 = c.query_row("SELECT COUNT(*) FROM secret_versions WHERE scope = ?1", [&scope_key], |r| r.get(0))?;
                anyhow::ensure!(count == 2, "one import, one update");
                let keys: i64 = c.query_row("SELECT COUNT(*) FROM secret_keys", [], |r| r.get(0))?;
                let legacy: i64 = c.query_row("SELECT COUNT(*) FROM ci_keys", [], |r| r.get(0))?;
                let copies: i64 = c.query_row("SELECT COUNT(*) FROM ci_secrets WHERE ciphertext IS NOT NULL", [], |r| r.get(0))?;
                anyhow::ensure!(keys == 1 && legacy == 0 && copies == 0, "only canonical encryption remains");
                anyhow::ensure!(values::read_value(&master, c, &scope_key, "TOKEN", 1)?.bytes() == b"legacy-credential");
                Ok(())
            }).await?;
            let why = crate::secrets::Use { by: "kilncd".into(), purpose: "pinned release".into() };
            let pins = [("TOKEN".to_owned(), 1)];
            let bundle = service.resolve(&app, &pins, &why).await?;
            anyhow::ensure!(bundle.items.first().is_some_and(|(_, _, v)| v.bytes() == b"app-credential"));
            anyhow::ensure!(!format!("{bundle:?}").contains("app-credential"));
            let resolved = Resolved { value: "replacement-credential".into(), id: None, scope: ScopeKind::Server };
            anyhow::ensure!(!format!("{resolved:?}").contains("replacement-credential"));
            anyhow::ensure!(!serde_json::to_string(&list(&hub, &scope).await?)?.contains("credential"));
            let repo = Repo { id: "repo-one".into(), name: "ada/web".into(), clone_url: String::new(), branches: vec![], poll_seconds: 0, enabled: true, created_at: 1, last_polled_at: None, last_error: None, mode: "native".into(), default_branch: None, mirror: None };
            let w = kiln_ci::oidc::Workload { repository: "ada/web", environment: Some("prod"), git_ref: None, sha: None, actor: "ada", source: "job", app: None, deployment: None, trigger: None };
            let server = Scope { kind: ScopeKind::Server, id: String::new() };
            let env = Scope { kind: ScopeKind::Environment, id: "repo-one:prod".into() };
            set(&hub, &server, "CHOICE", "server-value", "ada").await.map_err(anyhow::Error::msg)?;
            set(&hub, &server, "UNREQUESTED", "never-delivered", "ada").await.map_err(anyhow::Error::msg)?;
            set(&hub, &env, "CHOICE", "environment-value", "ada").await.map_err(anyhow::Error::msg)?;
            let wanted = Used { names: ["CHOICE".to_owned()].into(), all: false };
            let (delivered, failed) = for_leg(&hub, &repo, "build.yml", Some("prod"), &wanted, &w).await?;
            anyhow::ensure!(failed.is_empty() && delivered.len() == 1 && delivered.get("CHOICE").is_some_and(|r| r.value == "environment-value"));
            // A broker refusal never returns a lower scope's literal in its place.
            set_brokered(&hub, &env, "CHOICE", "integration:not-granted", "ada").await.map_err(anyhow::Error::msg)?;
            let (delivered, failed) = for_leg(&hub, &repo, "build.yml", Some("prod"), &wanted, &w).await?;
            anyhow::ensure!(delivered.is_empty() && failed.len() == 1);
            let (delivered, _) = for_leg(&hub, &repo, "build.yml", Some("prod"), &Used { all: true, names: Default::default() }, &w).await?;
            anyhow::ensure!(!delivered.contains_key("TOKEN"), "integration credentials never enter job scopes");
            anyhow::ensure!(delete(&hub, &scope, "TOKEN").await?);
            anyhow::ensure!(reveal(&hub, &scope, "TOKEN").await?.is_none());
            Ok(())
        })
    }

    #[test]
    fn w09_r03_sealed_values_open_only_where_they_were_sealed() -> anyhow::Result<()> {
        let k: [u8; 32] = random()?;
        let s = Scope { kind: ScopeKind::Repository, id: "r1".into() };
        let (sealed, nonce) = seal(&k, b"hunter2", &s.aad("PASS"))?;
        assert!(!sealed.windows(7).any(|w| w == b"hunter2"));
        assert_eq!(open(&k, &sealed, &nonce, &s.aad("PASS"))?, b"hunter2");
        // Another name, another scope, another key, or a flipped bit: refused.
        assert!(open(&k, &sealed, &nonce, &s.aad("OTHER")).is_err());
        assert!(open(&k, &sealed, &nonce, &Scope { kind: ScopeKind::Server, id: String::new() }.aad("PASS")).is_err());
        let other: [u8; 32] = random()?;
        assert!(open(&other, &sealed, &nonce, &s.aad("PASS")).is_err());
        let mut bad = sealed.clone();
        if let Some(b) = bad.first_mut() {
            *b ^= 1;
        }
        assert!(open(&k, &bad, &nonce, &s.aad("PASS")).is_err());
        // Fresh nonces: the same value seals differently each time.
        let (again, n2) = seal(&k, b"hunter2", &s.aad("PASS"))?;
        assert!(again != sealed && n2 != nonce);
        Ok(())
    }
}
