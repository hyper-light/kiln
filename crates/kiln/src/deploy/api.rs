//! `/api/deploy` (docs/deploy-api.md §12.1): environments, apps, registrations and releases over
//! HTTP, for the UI, `kiln` and scripts. Errors are `{error}` with the status that says what went
//! wrong; every change is audited with its caller.
//!
//! | Route | |
//! | --- | --- |
//! | `GET /board` | every registration, what runs and what's in flight, and what waits for approval |
//! | `GET /metrics?app&environment&days` | delivery metrics: DORA's four keys, from the releases |
//! | `GET /apps/{app}/environments/{env}/versions` | the versions it has run |
//! | `GET`, `POST /environments`; `GET`, `PUT`, `DELETE /environments/{env}` | environments |
//! | `GET`, `POST /templates`; `GET`, `PUT`, `DELETE /templates/{name}` | what apps are made from |
//! | `GET`, `POST /apps`; `GET`, `PUT`, `PATCH`, `DELETE /apps/{app}` (`?everywhere&keep`) | apps |
//! | `GET /apps/{app}/catalog-info.yaml`, `GET /app-names/{name}` | Backstage's view; whether a name is free |
//! | `POST`, `GET`, `DELETE /apps/{app}/environments/{env}` | register, show, deregister (`?keep=true`) |
//! | `POST /apps/{app}/environments/{env}/pause`, `…/resume` | the provider's automation |
//! | `GET /apps/{app}/environments/{env}/candidates` | releases a rollback can go to |
//! | `GET /link`; `GET /apps/{app}/environments/{env}/statement?audience` | for linked servers (§11): where this one is and the issuer it signs with; what runs of an app there and what's on its way, as a JWT signed for `audience` |
//! | `GET /apps/{app}/environments/{env}/resources`, `/manifests`, `/diff` (`?version` on Argo CD), `/logs?kind&name` | what runs, as the provider sees it |
//! | `GET /apps/{app}/environments/{env}/actions?kind&name&group&namespace`; `POST …/actions` (`{kind, name, action, params}`) | what can be done to an object it runs (restart, scale, suspend, delete a pod …), and doing it |
//! | `POST`, `GET`, `DELETE /environments/{env}/move` | moving it to another provider (`{to, handover, force}`): compared in shadow, then handed over with nothing made again; cancelled before the handover |
//! | `GET`, `POST /releases`; `GET /releases/{id}` | releases (filters: `app`, `environment`, `state`, `limit`) |
//! | `POST /releases/{id}/cancel`, `/releases/{id}/rollback` | |
//! | `POST /releases/{id}/approve`, `/releases/{id}/reject` | a person's answer to a release waiting for approval (`{note}`) |
//! | `POST /fleets`, `GET /fleets/{id}` | a fleet change: one version, or values set, across many apps of an environment; or (`source`) a kiln-base version or values set in each app's source repository, one pull request per repository |
//! | `POST /fleets/{id}/merge` (`{repo}`), `DELETE /fleets/{id}` | a source change's proposals on kiln's own git server merged; its open proposals withdrawn |
//! | `GET /apps/{app}/environments/{env}/secrets`; `PUT`, `DELETE …/secrets/{name}` (`?release=false`) | an app's secrets there (§3.5): names and versions; a value is set (`{value}` or `{base64}`), never read back; a change is released |
//! | `GET …/secrets/{name}/versions`, `GET …/secret-uses?limit` | a secret's versions; each read of the secrets for a delivery |
//! | `POST /secret-keys/rotate` | every app's secrets re-sealed under a new data key |
//!
//! A release, rollback or cancel asked for with an `Idempotency-Key` header (or
//! `idempotency_key`) is the same one when asked for again.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use http::StatusCode;
use serde_json::{Value, json};

use super::flow::{self, Ask, Fail, Who};
use super::model::{Release, State};
use super::{Deploy, argocd, kilncd, link, store};
use crate::http::server::{Request, Response};
use crate::hub::Hub;
use crate::secrets::{self, Scope};

fn bad(msg: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, msg.into())
}

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// A future made and boxed in a frame of its own, so none of it lands on the stack of the function
/// awaiting it. A debug build gives each temporary of a function its own stack slot: a router
/// awaiting many large futures inline had a frame of all of them, and with an outbound TLS
/// handshake beneath it, a shard's 2 MiB stack overflowed.
fn boxed<'a, F: Future + Send + 'a>(make: impl FnOnce() -> F) -> Pin<Box<dyn Future<Output = F::Output> + Send + 'a>> {
    Box::pin(make())
}

/// Routes `/api/deploy/<rest>`, as a boxed future (see [`boxed`]).
pub fn route<'a>(hub: &'a Arc<Hub>, rest: &'a [&'a str], q: &'a HashMap<String, String>, req: Request) -> Pin<Box<dyn Future<Output = Result<Response, Fail>> + Send + 'a>> {
    boxed(move || routed(hub, rest, q, req))
}

async fn routed(hub: &Arc<Hub>, rest: &[&str], q: &HashMap<String, String>, req: Request) -> Result<Response, Fail> {
    // U12: all deploy URLs are aliases declared on their operation descriptors.
    let caller = req.extensions().get::<crate::authz::Caller>().cloned().unwrap_or_else(crate::authz::Caller::owner);
    let method = req.method().clone();
    let mut path = vec!["api", "deploy"];
    path.extend_from_slice(rest);
    crate::ops::http(crate::ops::Cx { hub: Arc::clone(hub), caller }, method, &path, q, req).await
}

/// What runs of `app` in `env` and what's on its way, signed for the linked server whose issuer is
/// `audience` (§11).
pub(super) async fn statement(d: &Deploy, app: &str, env: &str, audience: Option<&str>) -> Result<Value, Fail> {
    let audience = audience.map(str::trim).filter(|a| !a.is_empty() && a.len() <= 512).ok_or_else(|| bad("`audience`: the issuer of the server asking"))?;
    match link::statement(d, app, env, audience).await.map_err(internal)? {
        Some(statement) => Ok(json!({ "statement": statement })),
        None => Err((StatusCode::NOT_FOUND, format!("{app} isn't registered in {env}"))),
    }
}

/// Every app's secrets sealed again under a new data key (§3.5): how many were.
pub(super) async fn rotate_keys(d: &Deploy, who: &Who) -> Result<Value, Fail> {
    let n = secrets::of(&d.hub).rotate(&caller(who)).await.map_err(refused)?;
    Ok(json!({ "resealed": n }))
}

/// The git hosts release repositories may be on (docs/deploy-api.md §7).
pub(super) async fn git_hosts(hub: &Hub) -> Vec<Value> {
    use super::model::{BITBUCKET_HOST, GITHUB_HOST, GITLAB_HOST, KILN_HOST};
    let mut out = vec![json!({ "host": KILN_HOST, "name": "kiln's own git server" })];
    if let Some(app) = crate::githost::github::app(hub).await {
        out.push(json!({ "host": GITHUB_HOST, "name": "GitHub", "web": app.web }));
    }
    for g in crate::githost::gitlab::hosts(hub).await {
        out.push(json!({ "host": GITLAB_HOST, "name": "GitLab", "web": g.web, "groups": g.groups() }));
    }
    if let Some(b) = crate::githost::bitbucket::host(hub).await {
        out.push(json!({ "host": BITBUCKET_HOST, "name": "Bitbucket", "web": b.web, "groups": b.workspaces() }));
    }
    out
}

/// Every registration with what runs and what's in flight, and what waits for people: the
/// environments board, in one answer.
pub(super) async fn board(d: &Deploy) -> Result<Value, Fail> {
    let st = &*d.records;
    let environments = store::environments(st).await.map_err(internal)?;
    let apps = store::apps(st).await.map_err(internal)?;
    let mut cells = Vec::new();
    for reg in store::registrations(st, None, None).await.map_err(internal)? {
        let running = match reg.running {
            Some(id) => store::release(st, id).await.map_err(internal)?,
            None => None,
        };
        let in_flight = store::in_flight(st, &reg.app, &reg.environment).await.map_err(internal)?;
        cells.push(json!({ "registration": reg, "running": running, "in_flight": in_flight }));
    }
    let waiting = store::releases(st, store::ReleaseQuery { state: Some(State::WaitingApproval), limit: 200, ..store::ReleaseQuery::default() }).await.map_err(internal)?;
    Ok(json!({ "environments": environments, "apps": apps, "cells": cells, "waiting_approval": waiting }))
}

/// The versions a registration has run, newest first: when each first and last landed, how often,
/// and whether it runs now.
pub(super) async fn versions(d: &Deploy, app: &str, env: &str) -> Result<Vec<Value>, Fail> {
    let st = &*d.records;
    let reg = store::registration(st, app, env).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("{app} isn't registered in {env}")))?;
    let list = store::releases(st, store::ReleaseQuery { app: Some(app.into()), environment: Some(env.into()), limit: 1000, ..store::ReleaseQuery::default() }).await.map_err(internal)?;
    let running = reg.running.and_then(|id| list.iter().find(|r| r.id == id)).and_then(|r| r.version.clone());
    let mut seen: Vec<(String, Vec<&super::model::Release>)> = Vec::new();
    for r in list.iter().filter(|r| matches!(r.state, State::Succeeded | State::Superseded | State::RolledBack)) {
        let Some(v) = &r.version else { continue };
        match seen.iter_mut().find(|(x, _)| x == v) {
            Some((_, rs)) => rs.push(r),
            None => seen.push((v.clone(), vec![r])),
        }
    }
    Ok(seen
        .into_iter()
        .map(|(v, rs)| {
            let ended: Vec<i64> = rs.iter().filter_map(|r| r.ended_at).collect();
            json!({
                "version": v,
                "runs_now": running.as_deref() == Some(v.as_str()),
                "releases": rs.len(),
                "last_release": rs.first().map(|r| r.id),
                "first_landed": ended.iter().min(),
                "last_landed": ended.iter().max(),
            })
        })
        .collect())
}

/// An app, with each environment it's registered in and what runs there.
pub(super) async fn app_json(d: &Deploy, name: &str) -> Result<Value, Fail> {
    let a = store::app(&*d.records, name).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no app {name}")))?;
    let regs = store::registrations(&*d.records, Some(name), None).await.map_err(internal)?;
    let mut envs = Vec::new();
    for reg in regs {
        let running = match reg.running {
            Some(id) => store::release(&*d.records, id).await.map_err(internal)?,
            None => None,
        };
        envs.push(json!({ "registration": reg, "running": running }));
    }
    Ok(json!({ "app": a, "environments": envs }))
}

/// A registration: what runs, what's in flight, the latest releases, and what its provider says now.
pub(super) async fn registration_json(d: &Deploy, app: &str, env: &str) -> Result<Value, Fail> {
    let a = store::app(&*d.records, app).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no app {app}")))?;
    let environment = store::environment(&*d.records, env).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no environment {env}")))?;
    let reg = store::registration(&*d.records, app, env).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("{app} isn't registered in {env}")))?;
    let recent = store::releases(&*d.records, store::ReleaseQuery { app: Some(app.into()), environment: Some(env.into()), state: None, version: None, limit: 20 }).await.map_err(internal)?;
    let in_flight = recent.iter().find(|r| r.state.in_flight()).cloned();
    let running = recent.iter().find(|r| Some(r.id) == reg.running).cloned();
    let provider = match environment.provider.strip_prefix("argocd/") {
        Some(name) => {
            let application = argocd::application_name(&a, env);
            let status = match argocd::Argo::of(&d.hub, name).await {
                Ok(argo) => match argo.application(&application, None).await {
                    Ok(Some(app)) => argocd::status(&app),
                    Ok(None) => Value::Null,
                    Err(e) => json!({ "error": e }),
                },
                Err(e) => json!({ "error": e }),
            };
            json!({ "kind": "argocd", "integration": name, "app": application, "status": status })
        }
        None => {
            let id = kilncd::app_id(&a, env);
            let status = crate::cd::get().and_then(|cd| cd.statuses.read().get(&id).map(crate::cd::summary_json)).unwrap_or(Value::Null);
            json!({ "kind": "kilncd", "app": id, "status": status })
        }
    };
    Ok(json!({ "registration": reg, "running": running, "in_flight": in_flight, "releases": recent, "provider": provider }))
}

/// An app's secrets in an environment (§3.5) belong to an app and an environment that are there.
async fn secret_scope(d: &Deploy, app: &str, env: &str) -> Result<Scope, Fail> {
    store::app(&*d.records, app).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no app {app}")))?;
    store::environment(&*d.records, env).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no environment {env}")))?;
    Ok(Scope::of(app, env))
}

fn caller(who: &Who) -> secrets::Caller {
    secrets::Caller { name: who.name.clone(), agent: who.agent }
}

fn refused(r: secrets::Refused) -> Fail {
    match r {
        secrets::Refused::Bad(m) => (StatusCode::BAD_REQUEST, m),
        secrets::Refused::Forbidden(m) => (StatusCode::FORBIDDEN, m),
        secrets::Refused::Missing(m) => (StatusCode::NOT_FOUND, m),
        secrets::Refused::Failed(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
    }
}

/// A secret's value from its text (`value`) or its bytes (`base64`): one of them. What doesn't
/// read is refused without saying what it was.
pub(super) fn secret_value_of(value: Option<String>, base64: Option<String>) -> Result<secrets::Value, Fail> {
    match (value, base64) {
        (Some(v), None) => Ok(secrets::Value::new(v.into_bytes())),
        (None, Some(b)) => {
            use base64::Engine as _;
            let v = base64::engine::general_purpose::STANDARD.decode(b.trim()).map_err(|_| bad("`base64` isn't base64"))?;
            Ok(secrets::Value::new(v))
        }
        _ => Err(bad("a secret is set with `value` or `base64`: one of them")),
    }
}

/// The secrets of an app in an environment: names, versions, who set each and when; never values.
pub(super) async fn secrets_list(d: &Deploy, who: &Who, app: &str, env: &str) -> Result<Value, Fail> {
    let at = secret_scope(d, app, env).await?;
    let service = secrets::of(&d.hub);
    let list = service.list(&at, &caller(who)).await.map_err(refused)?;
    Ok(json!({ "app": app, "environment": env, "backend": service.backend_name(), "secrets": list }))
}

/// Sets a secret's next version, and releases it unless `release` is false: the version, and the
/// release that delivers it (made, or joined) with what happens next.
pub(super) async fn secret_put(d: &Deploy, who: &Who, app: &str, env: &str, name: &str, value: secrets::Value, how: Change) -> Result<Value, Fail> {
    let at = secret_scope(d, app, env).await?;
    let meta = secrets::of(&d.hub).put(&at, name, value, &caller(who)).await.map_err(refused)?;
    let (release, next) = released(d, who, app, env, format!("{name} v{}", meta.version), how).await;
    let mut v = serde_json::to_value(&meta).map_err(|e| internal(e.into()))?;
    if let Some(o) = v.as_object_mut() {
        o.insert("release".into(), json!(release));
        o.insert("next".into(), json!(next));
    }
    Ok(v)
}

pub(super) async fn secret_delete(d: &Deploy, who: &Who, app: &str, env: &str, name: &str, how: Change) -> Result<Value, Fail> {
    let at = secret_scope(d, app, env).await?;
    secrets::of(&d.hub).delete(&at, name, &caller(who)).await.map_err(refused)?;
    let (release, next) = released(d, who, app, env, format!("{name} deleted"), how).await;
    Ok(json!({ "name": name, "deleted": true, "release": release, "next": next }))
}

/// A secret's change on its way to what runs (§3.5): a secrets release, when something runs there
/// on KilnCD to take it; a release still waiting takes it instead, as it commits (and says so, for
/// whoever approves it). The release, and what happens next in words.
/// How a secret's change goes out: `?release=false` keeps it for a later release, `?revert=false`
/// asks that the release delivering it isn't reverted if it fails (a rotation after a leak: a
/// revert would deliver the old value again).
#[derive(Debug, Clone, Copy)]
pub(super) struct Change {
    pub(super) release: bool,
    pub(super) revert: bool,
}


async fn released(d: &Deploy, who: &Who, app: &str, env: &str, what: String, how: Change) -> (Option<Release>, String) {
    let st = &*d.records;
    let later = format!("release it with `kiln deploy {app} --to {env} --secrets`");
    if !how.release {
        return (None, format!("not released: {later}"));
    }
    if let Err(e) = store::environment(st, env).await {
        return (None, format!("not released ({e:#}): {later}"));
    }
    match store::registration(st, app, env).await {
        Ok(Some(reg)) if reg.running.is_some() => {}
        Ok(_) => return (None, format!("nothing of {app} runs in {env} yet: its first release there delivers it")),
        Err(e) => return (None, format!("not released ({e:#}): {later}")),
    }
    if let Ok(Some(mut r)) = store::newest_in_flight(st, app, env).await
        && matches!(r.state, State::Waiting | State::WaitingApproval)
    {
        let was = r.state;
        r.secrets = Some(r.secrets.take().map_or_else(|| what.clone(), |s| format!("{s}, {what}")));
        // A change that mustn't come back makes the release delivering it one not reverted.
        r.no_revert |= !how.revert;
        if store::update_release(st, &r, was).await.unwrap_or(false) {
            flow::emit_release(d, &r);
            let id = r.id;
            return (Some(r), format!("release {id}, waiting, delivers it as it goes"));
        }
    }
    let ask = Ask { app: app.to_owned(), environment: env.to_owned(), secrets: Some(what), no_revert: !how.revert, ..Ask::default() };
    match flow::release(d, who, ask).await {
        Ok(r) => {
            let id = r.id;
            (Some(r), format!("release {id} delivers it"))
        }
        Err((_, why)) => (None, format!("not released ({why}): {later}")),
    }
}

pub(super) async fn secret_versions(d: &Deploy, who: &Who, app: &str, env: &str, name: &str) -> Result<Value, Fail> {
    let at = secret_scope(d, app, env).await?;
    let versions = secrets::of(&d.hub).versions(&at, name, &caller(who)).await.map_err(refused)?;
    Ok(json!({ "name": name, "versions": versions }))
}

pub(super) async fn secret_uses(d: &Deploy, who: &Who, app: &str, env: &str, limit: usize) -> Result<Value, Fail> {
    let at = secret_scope(d, app, env).await?;
    let reads = secrets::of(&d.hub).reads(&at, limit, &caller(who)).await.map_err(refused)?;
    Ok(json!({ "reads": reads }))
}

#[cfg(test)]
mod tests {
    use super::super::model::{App, Environment};
    use super::*;
    use crate::deploy::model::{AgentPolicy, Contacts, KILNCD, Kind, Package, Registration};

    fn env(name: &str, provider: &str) -> Environment {
        Environment {
            name: name.into(),
            provider: provider.into(),
            release_repo: Default::default(),
            targets: Default::default(),
            gates: Default::default(),
            sync: Default::default(),
            watch: Default::default(),
            concurrency: Default::default(),
            agents: AgentPolicy::default(),
            argocd: Default::default(),
            owners: Default::default(),
        }
    }

    /// Sets a secret as a person would over the API: what the answer says.
    async fn set(d: &Deploy, env: &str, name: &str, release: bool) -> anyhow::Result<Value> {
        let ada = Who::kiln("ada");
        secret_put(d, &ada, "web", env, name, secrets::Value::new(b"value".to_vec()), Change { release, revert: true }).await.map_err(|(code, m)| anyhow::anyhow!("{code}: {m}"))
    }

    fn next(v: &Value) -> &str {
        v.get("next").and_then(Value::as_str).unwrap_or_default()
    }

    #[test]
    fn a_secrets_change_is_released_where_something_runs_or_joins_a_release_still_waiting() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let root = tempfile::tempdir()?;
            let d = Deploy { records: Arc::clone(&hub.records), stand_in: None, hosted: super::super::host::Hosted::new(root.path()), owner: "test".into(), wake: crate::rt::Notify::new(), changes: crate::rt::Broadcast::new(16), argo: Default::default(), remote: Default::default(), hub: Arc::clone(&hub) };
            let st = &*d.records;
            store::put_environment(st, &env("staging", KILNCD)).await?;
            store::put_environment(st, &env("argo", "argocd/prod")).await?;
            let package: Package = serde_json::from_value(json!({ "kind": "helm", "repo": "oci://ghcr.io/acme/charts", "chart": "web" }))?;
            let web = App {
                name: "web".into(),
                project: "shop".into(),
                owners: vec!["payments".into()],
                package,
                namespace: None,
                properties: Default::default(),
                template: None,
                contacts: Contacts::default(),
                source: None,
                archived: false,
                registries: None,
            };
            store::put_app(st, &web).await?;

            // Nothing runs there yet: kept, for the first release to deliver.
            let v = set(&d, "staging", "DB_PASSWORD", true).await?;
            anyhow::ensure!(v.get("version") == Some(&json!(1)) && v.get("release") == Some(&Value::Null) && next(&v).contains("first release"), "{v}");

            // Something runs: a secrets release, through the environment's gates.
            store::put_registration(st, &Registration { app: "web".into(), environment: "staging".into(), enabled: true, paused: false, running: Some(1), created_at: 0, degraded: None }).await?;
            let v = set(&d, "staging", "DB_PASSWORD", true).await?;
            let r: Release = serde_json::from_value(v.get("release").cloned().unwrap_or_default())?;
            anyhow::ensure!(r.kind == Kind::Config && r.values.is_none() && r.secrets.as_deref() == Some("DB_PASSWORD v2") && r.state == State::Waiting, "{r:?}");

            // U10: numbered releases wait for canonical admission; establish the committing
            // boundary explicitly before checking that a change cannot join it.
            let mut committing = r.clone();
            committing.state = State::Committing;
            anyhow::ensure!(store::update_release(st, &committing, State::Waiting).await?);
            // While it commits, another change can't join it, and the environment takes one release at a time.
            let v = set(&d, "staging", "API_KEY", true).await?;
            anyhow::ensure!(v.get("release") == Some(&Value::Null) && next(&v).contains("in flight") && next(&v).contains("--secrets"), "{v}");

            // One still waiting takes changes on, and says so.
            let mut waiting = r.clone();
            waiting.state = State::Waiting;
            anyhow::ensure!(store::update_release(st, &waiting, State::Committing).await?);
            let v = set(&d, "staging", "API_KEY", true).await?;
            anyhow::ensure!(v.pointer("/release/id") == Some(&json!(r.id)) && v.pointer("/release/secrets") == Some(&json!("DB_PASSWORD v2, API_KEY v2")), "{v}");

            // Kept without releasing, when asked; and where Argo CD deploys as anywhere: kept for its first release.
            anyhow::ensure!(next(&set(&d, "staging", "OTHER", false).await?).starts_with("not released"));
            let v = set(&d, "argo", "DB_PASSWORD", true).await?;
            anyhow::ensure!(v.get("release") == Some(&Value::Null) && next(&v).contains("first release"), "{v}");
            Ok(())
        })
    }
}
