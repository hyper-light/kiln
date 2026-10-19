//! Pools (docs/ci.md W13-R09): from the server's configuration (`[[ci.pools]]`) and the Pools page
//! (`ci_pools`, through `/api/ci/pools`), the configuration's first; a stored pool can't take a
//! configured one's name.
//!
//! - `GET /api/ci/pools`: each pool, its settings and its executors now (starting, idle, busy, on
//!   which runner), the legs waiting for it, and its last failure to start one.
//! - `GET /api/ci/pools/:name`: one.
//! - `PUT /api/ci/pools/:name`: makes or changes a stored pool (`pools.manage`); checked as the
//!   configuration's are.
//! - `DELETE /api/ci/pools/:name`: removes one; its idle executors stop, busy ones end with their
//!   legs.

use std::collections::HashMap;

use http::StatusCode;
use kiln_ci::sched::pools::{Pool, PoolSpec};
use rusqlite::params;
use serde_json::{Value, json};

use super::Fail;
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// The pools in order: the configuration's, then the stored ones. One that doesn't check out is
/// left out, with a warning.
pub async fn sources(hub: &Hub) -> Vec<Pool> {
    let mut specs: Vec<PoolSpec> = hub.cfg.ci.pools.clone();
    let stored: Vec<String> = hub.store.call(|c| {
        let mut s = c.prepare("SELECT spec FROM ci_pools ORDER BY created_at, name")?;
        let rows = s.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<Vec<String>, _>>()?)
    })
    .await
    .unwrap_or_default();
    for text in stored {
        match serde_json::from_str::<PoolSpec>(&text) {
            Ok(p) if !specs.iter().any(|x| x.name == p.name) => specs.push(p),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "a stored pool can't be read"),
        }
    }
    specs
        .iter()
        .filter_map(|s| match s.pool() {
            // Its hosts' runners start executors when asked.
            Ok(p) => Some(Pool { caps: u64::from(crate::proto::CAP_POOLS), ..p }),
            Err(e) => {
                tracing::warn!(pool = %s.name, error = %e, "a pool is left out");
                None
            }
        })
        .collect()
}

/// `GET /api/ci/pools[/:name]`.
pub async fn list(hub: &Hub, name: Option<&str>) -> Result<Response, Fail> {
    let (tx, rx) = async_channel::bounded(1);
    hub.ci.send(super::dispatch::Msg::Pools { reply: tx });
    let live: Value = rx.recv().await.unwrap_or_else(|_| json!([]));
    let configured: HashMap<String, bool> = hub.cfg.ci.pools.iter().map(|p| (p.name.clone(), true)).collect();
    let mut pools: Vec<Value> = live.as_array().cloned().unwrap_or_default();
    for p in &mut pools {
        if let Some(o) = p.as_object_mut() {
            let n = o.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
            o.insert("source".into(), json!(if configured.contains_key(&n) { "config" } else { "stored" }));
        }
    }
    match name {
        Some(n) => pools.into_iter().find(|p| p.get("name").and_then(Value::as_str) == Some(n)).map(|p| srv::json(StatusCode::OK, &p)).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no pool `{n}`"))),
        None => Ok(srv::json(StatusCode::OK, &json!({ "pools": pools }))),
    }
}

/// `PUT /api/ci/pools/:name`.
pub async fn put(hub: &Hub, name: &str, body: &Value) -> Result<Response, Fail> {
    if hub.cfg.ci.pools.iter().any(|p| p.name == name) {
        return Err((StatusCode::CONFLICT, format!("pool `{name}` is the server configuration's; change it there")));
    }
    let mut spec: PoolSpec = serde_json::from_value(body.clone()).map_err(|e| (StatusCode::BAD_REQUEST, format!("{e}")))?;
    spec.name = name.to_owned();
    let pool = spec.pool().map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let text = serde_json::to_string(&spec).map_err(|e| internal(e.into()))?;
    let n = name.to_owned();
    hub.store
        .call(move |c| {
            c.execute(
                "INSERT INTO ci_pools (name, spec, created_at, updated_at) VALUES (?1, ?2, ?3, ?3) ON CONFLICT (name) DO UPDATE SET spec = excluded.spec, updated_at = excluded.updated_at",
                params![n, text, crate::util::now_unix()],
            )?;
            Ok(())
        })
        .await
        .map_err(internal)?;
    hub.ci.send(super::dispatch::Msg::PoolsChanged);
    Ok(srv::json(StatusCode::OK, &json!({ "name": pool.name, "executor": pool.executor, "labels": pool.labels, "min_idle": pool.min_idle, "max": pool.max })))
}

/// `DELETE /api/ci/pools/:name`.
pub async fn delete(hub: &Hub, name: &str) -> Result<Response, Fail> {
    if hub.cfg.ci.pools.iter().any(|p| p.name == name) {
        return Err((StatusCode::CONFLICT, format!("pool `{name}` is the server configuration's; remove it there")));
    }
    let n = name.to_owned();
    let gone = hub.store.call(move |c| Ok(c.execute("DELETE FROM ci_pools WHERE name = ?1", [&n])?)).await.map_err(internal)?;
    if gone == 0 {
        return Err((StatusCode::NOT_FOUND, format!("no stored pool `{name}`")));
    }
    hub.ci.send(super::dispatch::Msg::PoolsChanged);
    Ok(srv::status(StatusCode::NO_CONTENT))
}

/// What a runner needs to start one of `pool`'s executors: its image pulled from kiln's registry
/// with a short-lived token for that image alone, when it's there (W12-R01).
pub fn start_ask(hub: &Hub, pool: &str, executor: &str, kind: &str, image: Option<&str>) -> Value {
    let local = image.and_then(|i| crate::ci::registry::local_name(hub, i));
    let token = local.map(|name| {
        let access = crate::ci::registry::auth::Access { sub: format!("pool:{pool}"), grants: vec![(name, vec![crate::ci::registry::auth::Action::Pull])], exp: crate::util::now_unix() + crate::ci::registry::auth::TOKEN_SECS };
        crate::ci::registry::auth::token(hub, &access)
    });
    json!({ "op": "start", "executor": executor, "pool": pool, "kind": kind, "image": image, "registry": token.as_ref().map(|_| hub.cfg.server.public_url.clone()), "pull_token": token })
}

