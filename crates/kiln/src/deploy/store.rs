//! Deploying by API's records (docs/deploy-api.md §14.1), on kiln's store until KilnDB takes them
//! over: everything else reaches them through here, so only this module changes then.
//!
//! Each record is kept as its JSON, with the columns its queries need beside it. The work table
//! stands in for Focal's bus (§14.2): one row per release being followed, leased by the server
//! that follows it, taken again once the lease lapses.

use anyhow::Context as _;
use rusqlite::{OptionalExtension, params};

use super::model::{App, Environment, Registration, Release, State};
use crate::store::Store;

fn to_json<T: serde::Serialize>(v: &T) -> anyhow::Result<String> {
    serde_json::to_string(v).context("encode a deploy record")
}

fn from_json<T: serde::de::DeserializeOwned>(s: &str) -> anyhow::Result<T> {
    serde_json::from_str(s).context("read a deploy record")
}

// ------------------------------------------------------------------ environments

pub async fn put_environment(store: &Store, e: &Environment) -> anyhow::Result<()> {
    let (name, spec) = (e.name.clone(), to_json(e)?);
    store
        .call(move |c| {
            c.execute(
                "INSERT INTO deploy_environments (name, spec, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(name) DO UPDATE SET spec = excluded.spec, updated_at = excluded.updated_at",
                params![name, spec, crate::util::now_unix()],
            )?;
            Ok(())
        })
        .await
}

pub async fn environment(store: &Store, name: &str) -> anyhow::Result<Option<Environment>> {
    let name = name.to_owned();
    let spec: Option<String> = store.call(move |c| Ok(c.query_row("SELECT spec FROM deploy_environments WHERE name = ?1", [name], |r| r.get(0)).optional()?)).await?;
    spec.as_deref().map(from_json).transpose()
}

pub async fn environments(store: &Store) -> anyhow::Result<Vec<Environment>> {
    let specs: Vec<String> = store
        .call(|c| {
            let mut s = c.prepare("SELECT spec FROM deploy_environments ORDER BY name")?;
            let rows = s.query_map([], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            Ok(rows)
        })
        .await?;
    specs.iter().map(|s| from_json(s)).collect()
}

pub async fn delete_environment(store: &Store, name: &str) -> anyhow::Result<bool> {
    let name = name.to_owned();
    store.call(move |c| Ok(c.execute("DELETE FROM deploy_environments WHERE name = ?1", [name])? > 0)).await
}

// ------------------------------------------------------------------ apps

pub async fn put_app(store: &Store, a: &App) -> anyhow::Result<()> {
    let (name, spec) = (a.name.clone(), to_json(a)?);
    store
        .call(move |c| {
            c.execute(
                "INSERT INTO deploy_apps (name, spec, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(name) DO UPDATE SET spec = excluded.spec, updated_at = excluded.updated_at",
                params![name, spec, crate::util::now_unix()],
            )?;
            Ok(())
        })
        .await
}

pub async fn app(store: &Store, name: &str) -> anyhow::Result<Option<App>> {
    let name = name.to_owned();
    let spec: Option<String> = store.call(move |c| Ok(c.query_row("SELECT spec FROM deploy_apps WHERE name = ?1", [name], |r| r.get(0)).optional()?)).await?;
    spec.as_deref().map(from_json).transpose()
}

pub async fn apps(store: &Store) -> anyhow::Result<Vec<App>> {
    let specs: Vec<String> = store
        .call(|c| {
            let mut s = c.prepare("SELECT spec FROM deploy_apps ORDER BY name")?;
            let rows = s.query_map([], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            Ok(rows)
        })
        .await?;
    specs.iter().map(|s| from_json(s)).collect()
}

pub async fn delete_app(store: &Store, name: &str) -> anyhow::Result<bool> {
    let name = name.to_owned();
    store.call(move |c| Ok(c.execute("DELETE FROM deploy_apps WHERE name = ?1", [name])? > 0)).await
}

// ------------------------------------------------------------------ templates

pub async fn put_template(store: &Store, t: &super::model::Template) -> anyhow::Result<()> {
    let (name, spec) = (t.name.clone(), to_json(t)?);
    store
        .call(move |c| {
            c.execute(
                "INSERT INTO deploy_templates (name, spec, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(name) DO UPDATE SET spec = excluded.spec, updated_at = excluded.updated_at",
                params![name, spec, crate::util::now_unix()],
            )?;
            Ok(())
        })
        .await
}

pub async fn template(store: &Store, name: &str) -> anyhow::Result<Option<super::model::Template>> {
    let name = name.to_owned();
    let spec: Option<String> = store.call(move |c| Ok(c.query_row("SELECT spec FROM deploy_templates WHERE name = ?1", [name], |r| r.get(0)).optional()?)).await?;
    spec.as_deref().map(from_json).transpose()
}

pub async fn templates(store: &Store) -> anyhow::Result<Vec<super::model::Template>> {
    let specs: Vec<String> = store
        .call(|c| {
            let mut s = c.prepare("SELECT spec FROM deploy_templates ORDER BY name")?;
            let rows = s.query_map([], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            Ok(rows)
        })
        .await?;
    specs.iter().map(|s| from_json(s)).collect()
}

pub async fn delete_template(store: &Store, name: &str) -> anyhow::Result<bool> {
    let name = name.to_owned();
    store.call(move |c| Ok(c.execute("DELETE FROM deploy_templates WHERE name = ?1", [name])? > 0)).await
}

// ------------------------------------------------------------------ registrations

pub async fn put_registration(store: &Store, r: &Registration) -> anyhow::Result<()> {
    let (app, env, spec) = (r.app.clone(), r.environment.clone(), to_json(r)?);
    store
        .call(move |c| {
            c.execute(
                "INSERT INTO deploy_registrations (app, environment, spec) VALUES (?1, ?2, ?3)
                 ON CONFLICT(app, environment) DO UPDATE SET spec = excluded.spec",
                params![app, env, spec],
            )?;
            Ok(())
        })
        .await
}

pub async fn registration(store: &Store, app: &str, env: &str) -> anyhow::Result<Option<Registration>> {
    let (app, env) = (app.to_owned(), env.to_owned());
    let spec: Option<String> = store
        .call(move |c| Ok(c.query_row("SELECT spec FROM deploy_registrations WHERE app = ?1 AND environment = ?2", params![app, env], |r| r.get(0)).optional()?))
        .await?;
    spec.as_deref().map(from_json).transpose()
}

/// Registrations of an app, of an environment, or every one.
pub async fn registrations(store: &Store, app: Option<&str>, env: Option<&str>) -> anyhow::Result<Vec<Registration>> {
    let (app, env) = (app.map(str::to_owned), env.map(str::to_owned));
    let specs: Vec<String> = store
        .call(move |c| {
            let mut s = c.prepare(
                "SELECT spec FROM deploy_registrations WHERE (?1 IS NULL OR app = ?1) AND (?2 IS NULL OR environment = ?2) ORDER BY app, environment",
            )?;
            let rows = s.query_map(params![app, env], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            Ok(rows)
        })
        .await?;
    specs.iter().map(|s| from_json(s)).collect()
}

pub async fn delete_registration(store: &Store, app: &str, env: &str) -> anyhow::Result<bool> {
    let (app, env) = (app.to_owned(), env.to_owned());
    store.call(move |c| Ok(c.execute("DELETE FROM deploy_registrations WHERE app = ?1 AND environment = ?2", params![app, env])? > 0)).await
}

// ------------------------------------------------------------------ releases

/// Records a new release, numbering it; returns it with its id, and `true`. One asked for with an
/// idempotency key its registration has used already isn't recorded again: that release is
/// returned, with `false`.
pub async fn insert_release(store: &Store, r: &Release) -> anyhow::Result<(Release, bool)> {
    let mut r = r.clone();
    let (app, env, state, created, key) = (r.app.clone(), r.environment.clone(), r.state.as_str(), r.created_at, r.idempotency_key.clone());
    let draft = to_json(&r)?;
    let id: Result<i64, i64> = store
        .call(move |c| {
            let tx = c.transaction()?;
            if let Some(k) = &key {
                let used: Option<i64> =
                    tx.query_row("SELECT id FROM deploy_releases WHERE app = ?1 AND environment = ?2 AND idempotency_key = ?3", params![app, env, k], |r| r.get(0)).optional()?;
                if let Some(id) = used {
                    return Ok(Err(id));
                }
            }
            tx.execute(
                "INSERT INTO deploy_releases (app, environment, state, spec, created_at, idempotency_key) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![app, env, state, draft, created, key],
            )?;
            let id = tx.last_insert_rowid();
            tx.commit()?;
            Ok(Ok(id))
        })
        .await?;
    let id = match id {
        Ok(id) => id,
        Err(used) => return release(store, used).await?.map(|r| (r, false)).ok_or_else(|| anyhow::anyhow!("release {used} vanished")),
    };
    r.id = id;
    let spec = to_json(&r)?;
    store
        .call(move |c| {
            c.execute("UPDATE deploy_releases SET spec = ?2 WHERE id = ?1", params![id, spec])?;
            Ok(())
        })
        .await?;
    Ok((r, true))
}

/// Writes a release back if its state is still `expected` (another server, or a cancel, may have
/// moved it meanwhile). Whether it did.
pub async fn update_release(store: &Store, r: &Release, expected: State) -> anyhow::Result<bool> {
    let (id, state, spec, expected) = (r.id, r.state.as_str(), to_json(r)?, expected.as_str());
    store.call(move |c| Ok(c.execute("UPDATE deploy_releases SET state = ?2, spec = ?3 WHERE id = ?1 AND state = ?4", params![id, state, spec, expected])? > 0)).await
}

/// The release a registration was asked for with `key`, if it was.
pub async fn release_by_key(store: &Store, app: &str, env: &str, key: &str) -> anyhow::Result<Option<Release>> {
    let (app, env, key) = (app.to_owned(), env.to_owned(), key.to_owned());
    let spec: Option<String> = store
        .call(move |c| Ok(c.query_row("SELECT spec FROM deploy_releases WHERE app = ?1 AND environment = ?2 AND idempotency_key = ?3", params![app, env, key], |r| r.get(0)).optional()?))
        .await?;
    spec.as_deref().map(from_json).transpose()
}

/// The releases asked for under one idempotency key's prefix (a fleet's: `fleet:<id>:`), oldest
/// first.
pub async fn releases_keyed(store: &Store, prefix: &str) -> anyhow::Result<Vec<Release>> {
    let like = format!("{}%", prefix.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
    let specs: Vec<String> = store
        .call(move |c| {
            let mut s = c.prepare("SELECT spec FROM deploy_releases WHERE idempotency_key LIKE ?1 ESCAPE '\\' ORDER BY id")?;
            let rows = s.query_map([like], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            Ok(rows)
        })
        .await?;
    specs.iter().map(|s| from_json(s)).collect()
}

pub async fn release(store: &Store, id: i64) -> anyhow::Result<Option<Release>> {
    let spec: Option<String> = store.call(move |c| Ok(c.query_row("SELECT spec FROM deploy_releases WHERE id = ?1", [id], |r| r.get(0)).optional()?)).await?;
    spec.as_deref().map(from_json).transpose()
}

/// Releases, newest first, filtered.
#[derive(Debug, Default, Clone)]
pub struct ReleaseQuery {
    pub app: Option<String>,
    pub environment: Option<String>,
    pub state: Option<State>,
    /// Those of one version.
    pub version: Option<String>,
    pub limit: usize,
}

pub async fn releases(store: &Store, q: ReleaseQuery) -> anyhow::Result<Vec<Release>> {
    let limit = i64::try_from(if q.limit == 0 { 100 } else { q.limit.min(1000) }).unwrap_or(100);
    let state = q.state.map(State::as_str);
    let specs: Vec<String> = store
        .call(move |c| {
            let mut s = c.prepare(
                "SELECT spec FROM deploy_releases WHERE (?1 IS NULL OR app = ?1) AND (?2 IS NULL OR environment = ?2) AND (?3 IS NULL OR state = ?3)
                 AND (?5 IS NULL OR json_extract(spec, '$.version') = ?5) ORDER BY id DESC LIMIT ?4",
            )?;
            let rows = s.query_map(params![q.app, q.environment, state, limit, q.version], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            Ok(rows)
        })
        .await?;
    specs.iter().map(|s| from_json(s)).collect()
}

/// A registration's release in flight, if any.
pub async fn in_flight(store: &Store, app: &str, env: &str) -> anyhow::Result<Option<Release>> {
    let (app, env) = (app.to_owned(), env.to_owned());
    let spec: Option<String> = store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT spec FROM deploy_releases WHERE app = ?1 AND environment = ?2 AND state IN ('waiting', 'waiting_approval', 'committing', 'delivering') ORDER BY id LIMIT 1",
                params![app, env],
                |r| r.get(0),
            )
            .optional()?)
        })
        .await?;
    spec.as_deref().map(from_json).transpose()
}

// ------------------------------------------------------------------ the work queue

/// Follow a release from `due_at` (unix seconds) on.
pub async fn schedule(store: &Store, release: i64, due_at: i64) -> anyhow::Result<()> {
    store
        .call(move |c| {
            c.execute(
                "INSERT INTO deploy_work (release, due_at) VALUES (?1, ?2) ON CONFLICT(release) DO UPDATE SET due_at = excluded.due_at",
                params![release, due_at],
            )?;
            Ok(())
        })
        .await
}

/// Takes the releases due now that no other server holds, for `lease_secs`.
pub async fn claim(store: &Store, owner: &str, now: i64, lease_secs: i64, max: usize) -> anyhow::Result<Vec<i64>> {
    let owner = owner.to_owned();
    let max = i64::try_from(max).unwrap_or(16);
    store
        .call(move |c| {
            let tx = c.transaction()?;
            let ids: Vec<i64> = {
                let mut s = tx.prepare("SELECT release FROM deploy_work WHERE due_at <= ?1 AND (lease_until <= ?1 OR lease_owner = ?2) ORDER BY due_at LIMIT ?3")?;
                s.query_map(params![now, owner, max], |r| r.get(0))?.collect::<Result<Vec<i64>, _>>()?
            };
            for id in &ids {
                tx.execute("UPDATE deploy_work SET lease_owner = ?2, lease_until = ?3 WHERE release = ?1", params![id, owner, now + lease_secs])?;
            }
            tx.commit()?;
            Ok(ids)
        })
        .await
}

/// Done following a release.
pub async fn done(store: &Store, release: i64) -> anyhow::Result<()> {
    store
        .call(move |c| {
            c.execute("DELETE FROM deploy_work WHERE release = ?1", [release])?;
            Ok(())
        })
        .await
}
