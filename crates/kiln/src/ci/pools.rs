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
use kiln_ci::sched::reserve::Reservation;
use rusqlite::params;
use serde_json::{Value, json};

use super::Fail;
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// The pools in order (the configuration's, then the stored ones), and the reservations
/// (`[[ci.reservations]]`, then the pools' own). One that doesn't check out is left out, with a
/// warning.
pub async fn sources(hub: &Hub) -> (Vec<Pool>, Vec<Reservation>) {
    let specs = specs(hub).await;
    let mut reservations: Vec<Reservation> = Vec::new();
    for r in hub.cfg.ci.reservations.iter().map(|s| (s.name.clone(), s.reservation())).chain(specs.iter().filter_map(|p| p.reservation().map(|r| (p.name.clone(), r)))) {
        match r {
            (_, Ok(r)) => reservations.push(r),
            (n, Err(e)) => tracing::warn!(reservation = %n, error = %e, "a reservation is left out"),
        }
    }
    let pools = specs
        .iter()
        .filter_map(|s| match s.pool() {
            // Its hosts' runners start executors when asked.
            Ok(p) => Some(Pool { caps: u64::from(crate::proto::CAP_POOLS), ..p }),
            Err(e) => {
                tracing::warn!(pool = %s.name, error = %e, "a pool is left out");
                None
            }
        })
        .collect();
    (pools, reservations)
}

/// The pools' settings: the configuration's, then the stored ones.
pub(crate) async fn specs(hub: &Hub) -> Vec<PoolSpec> {
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

/// How far back a pool's scaling history goes (W21-R08).
const KEEP_SAMPLES: i64 = 7 * 24 * 3600;

/// Each pool's size (starting, idle, busy) and the legs waiting for it, a minute apart from the
/// server's start, for its scaling history (W21-R08), kept a week.
pub async fn sample_loop(hub: std::sync::Arc<Hub>) -> anyhow::Result<()> {
    let mut first = true;
    loop {
        if !std::mem::take(&mut first) && !hub.shutdown.sleep(std::time::Duration::from_secs(60)).await {
            return Ok(());
        }
        if !hub.ci.enabled() {
            continue;
        }
        let (tx, rx) = async_channel::bounded(1);
        hub.ci.send(super::dispatch::Msg::Pools { reply: tx });
        let Ok(Ok(live)) = crate::rt::timeout(std::time::Duration::from_secs(10), rx.recv()).await else { continue };
        let n = |p: &Value, k: &str| p.get(k).and_then(Value::as_i64).unwrap_or(0);
        let rows: Vec<(String, [i64; 4])> = live
            .as_array()
            .into_iter()
            .flatten()
            .map(|p| (p.get("name").and_then(Value::as_str).unwrap_or_default().to_owned(), [n(p, "starting"), n(p, "idle"), n(p, "busy"), n(p, "waiting")]))
            .collect();
        if rows.is_empty() {
            continue;
        }
        let now = crate::util::now_unix();
        let kept = hub
            .store
            .call(move |c| {
                let tx = c.transaction()?;
                for (pool, [starting, idle, busy, waiting]) in rows {
                    tx.execute("INSERT OR REPLACE INTO ci_pool_samples (pool, ts, starting, idle, busy, waiting) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![pool, now, starting, idle, busy, waiting])?;
                }
                tx.execute("DELETE FROM ci_pool_samples WHERE ts < ?1", [now - KEEP_SAMPLES])?;
                tx.commit()?;
                Ok(())
            })
            .await;
        if let Err(e) = kept {
            tracing::warn!(error = %format!("{e:#}"), "the pools' sizes weren't kept");
        }
    }
}

/// `GET /api/ci/pools/:name/history?hours=`: the pool's size and the legs waiting for it, a minute
/// apart, oldest first, over the last `hours` (24 by default, a week at most).
pub async fn history(hub: &Hub, name: &str, q: &HashMap<String, String>) -> Result<Response, Fail> {
    let hours: i64 = q.get("hours").and_then(|h| h.parse().ok()).unwrap_or(24).clamp(1, 7 * 24);
    let since = crate::util::now_unix() - hours * 3600;
    let pool = name.to_owned();
    let samples: Vec<Value> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT ts, starting, idle, busy, waiting FROM ci_pool_samples WHERE pool = ?1 AND ts >= ?2 ORDER BY ts")?;
            let rows = st.query_map(params![pool, since], |r| {
                Ok(json!({ "ts": r.get::<_, i64>(0)?, "starting": r.get::<_, i64>(1)?, "idle": r.get::<_, i64>(2)?, "busy": r.get::<_, i64>(3)?, "waiting": r.get::<_, i64>(4)? }))
            })?;
            Ok(rows.collect::<Result<Vec<Value>, _>>()?)
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(srv::json(StatusCode::OK, &json!({ "pool": name, "hours": hours, "samples": samples })))
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
    let (diagnostics, preview) = effect(hub, &pool).await;
    Ok(srv::json(StatusCode::OK, &json!({ "name": pool.name, "executor": pool.executor, "labels": pool.labels, "min_idle": pool.min_idle, "max": pool.max, "diagnostics": diagnostics, "preview": preview })))
}

/// What a pool's change does now (W20-R03): warnings when no runner connected now announces its
/// executor (none of its executors could start) or a runner carries all its labels itself (a leg
/// asking for them may go there instead); and the pool as the dispatcher has it now (it has the
/// change before it answers): its executors, the size it's heading for, the legs waiting.
async fn effect(hub: &Hub, pool: &Pool) -> (Vec<Value>, Value) {
    let mut diagnostics = Vec::new();
    let (hosts, carriers): (Vec<String>, Vec<String>) = {
        let agents = hub.agents.lock();
        let live: Vec<_> = agents.values().filter(|a| a.connected).collect();
        (
            live.iter().filter(|a| a.facts.as_ref().is_some_and(|f| f.executors.contains_key(&pool.executor))).map(|a| a.name.clone()).collect(),
            live.iter().filter(|a| !pool.labels.is_empty() && pool.labels.iter().all(|l| a.labels.contains(l))).map(|a| a.name.clone()).collect(),
        )
    };
    if hosts.is_empty() {
        diagnostics.push(json!({ "level": "warning", "message": format!("no runner connected now announces the {} executor: none of {}'s executors can start", pool.executor, pool.name) }));
    }
    for r in carriers {
        diagnostics.push(json!({ "level": "warning", "message": format!("runner {r} carries all of {} itself: a leg asking for them may run there instead of in the pool", pool.labels.join(", ")) }));
    }
    let (tx, rx) = async_channel::bounded(1);
    hub.ci.send(super::dispatch::Msg::Pools { reply: tx });
    let live: Value = crate::rt::timeout(std::time::Duration::from_secs(5), rx.recv()).await.ok().and_then(Result::ok).unwrap_or_else(|| json!([]));
    let now = live.as_array().into_iter().flatten().find(|p| p.get("name").and_then(Value::as_str) == Some(pool.name.as_str())).cloned().unwrap_or(Value::Null);
    let n = |k: &str| now.get(k).and_then(Value::as_u64).and_then(|v| usize::try_from(v).ok()).unwrap_or(0);
    let (starting, idle, busy, waiting) = (n("starting"), n("idle"), n("busy"), n("waiting"));
    let heading = starting + idle + busy + kiln_ci::sched::pools::wanted(pool, starting, idle, busy, waiting);
    (diagnostics, json!({ "hosts": hosts, "now": starting + idle + busy, "heading_for": heading, "waiting": waiting }))
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


/// Tells the dispatcher a pool's executor didn't start, and why.
fn failed(hub: &Hub, executor: &str, error: String) {
    let said = crate::runner::pool::Said { executor: executor.to_owned(), state: "failed".into(), error: Some(error) };
    if let Ok(json) = serde_json::to_string(&said) {
        hub.ci.send(super::dispatch::Msg::Executor { agent: "kubernetes".into(), json });
    }
}

/// What a pod made for a leg holds (docs/ci.md W13-R07), evaluated: the job container's image
/// (else the pool's) and environment, and the services beside it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LegPod {
    pub image: Option<String>,
    pub env: Vec<(String, String)>,
    pub services: Vec<PodService>,
}

/// A service, as a pod's native sidecar.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PodService {
    pub id: String,
    pub image: String,
    pub env: Vec<(String, String)>,
    /// The ports it listens on.
    pub ports: Vec<u16>,
}

/// Starts a `k8s` pool's executor: its pod (docs/ci.md W13-R07), made for a leg when `pod` says
/// what it holds, whose own runner says it's ready by connecting. Why one couldn't start fails the
/// executor, as a host's failure does.
pub async fn start_pod(hub: &std::sync::Arc<Hub>, pool: &str, executor: &str, kind: &str, pod: Option<LegPod>) {
    if kind != "k8s" {
        return failed(hub, executor, format!("a `{kind}` pool's executors are started by their hosts, not this server"));
    }
    let Some(k) = hub.k8s.get().and_then(Option::as_ref).cloned() else {
        return failed(hub, executor, "this server starts no pods: its `[kubernetes]` isn't enabled".into());
    };
    let Some(spec) = specs(hub).await.into_iter().find(|p| p.name == pool) else {
        return failed(hub, executor, format!("no pool `{pool}`"));
    };
    match k.launch_executor(hub, executor, &spec, pod.as_ref()).await {
        Ok(pod) => tracing::info!(pool = %pool, executor = %executor, pod = %pod, "executor pod started"),
        Err(e) => failed(hub, executor, format!("{e:#}")),
    }
}

/// Stops a `k8s` pool's executor: its pod goes (its runner goes with it).
pub async fn stop_pod(hub: &Hub, executor: &str, runner: &str) {
    let Some(k) = hub.k8s.get().and_then(Option::as_ref).cloned() else { return };
    let mut names: Vec<String> = Vec::new();
    if !executor.is_empty() {
        names.push(crate::k8s::executor_pod(executor));
    }
    if runner.starts_with("kiln-x-") && !names.iter().any(|n| n == runner) {
        names.push(runner.to_owned());
    }
    for n in names {
        hub.pod_executors.lock().remove(&n);
        if let Err(e) = k.delete_pod(&n).await {
            tracing::debug!(pod = %n, error = %format!("{e:#}"), "an executor pod couldn't be deleted");
        }
    }
}

/// The executors the scheduler has now.
async fn live_executors(hub: &Hub) -> Option<std::collections::HashSet<String>> {
    let (tx, rx) = async_channel::bounded(1);
    hub.ci.send(super::dispatch::Msg::Pools { reply: tx });
    let pools = rx.recv().await.ok()?;
    Some(
        pools
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|p| p.get("executors").and_then(Value::as_array).cloned().unwrap_or_default())
            .filter_map(|e| e.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect(),
    )
}

/// Every 15 s, the executor pods (docs/ci.md W13-R07): one that ended goes; one that can't start
/// (its image can't be pulled, its container made) for two minutes goes, its executor failed with
/// why; one whose executor the scheduler hasn't got (stopped, or from before a restart) goes.
pub async fn reap_pods(hub: std::sync::Arc<Hub>) -> anyhow::Result<()> {
    let mut stuck_since: HashMap<String, std::time::Instant> = HashMap::new();
    loop {
        if !hub.shutdown.sleep(std::time::Duration::from_secs(15)).await {
            return Ok(());
        }
        let Some(k) = hub.k8s.get().and_then(Option::as_ref).cloned() else { continue };
        let pods = match k.executor_pods().await {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(error = %format!("{e:#}"), "executor pods couldn't be listed");
                continue;
            }
        };
        let Some(live) = live_executors(&hub).await else { continue };
        stuck_since.retain(|n, _| pods.iter().any(|p| &p.name == n));
        for p in pods.into_iter().filter(|p| !p.deleting) {
            let ended = matches!(p.phase.as_str(), "Succeeded" | "Failed");
            let orphan = !live.contains(&p.executor);
            let stuck = p.stuck.as_ref().filter(|_| stuck_since.entry(p.name.clone()).or_insert_with(std::time::Instant::now).elapsed() > std::time::Duration::from_secs(120));
            if let Some(why) = stuck {
                tracing::warn!(pod = %p.name, reason = %why, "an executor pod can't start");
                failed(&hub, &p.executor, format!("pod {}: {why}", p.name));
            }
            if ended || orphan || stuck.is_some() {
                hub.pod_executors.lock().remove(&p.name);
                if let Err(e) = k.delete_pod(&p.name).await {
                    tracing::debug!(pod = %p.name, error = %format!("{e:#}"), "an executor pod couldn't be deleted");
                }
            }
        }
    }
}
