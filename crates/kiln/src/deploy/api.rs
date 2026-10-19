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
//! | `GET /apps/{app}/environments/{env}/resources`, `/manifests`, `/diff` (`?version` on Argo CD), `/logs?kind&name` | what runs, as the provider sees it |
//! | `GET`, `POST /releases`; `GET /releases/{id}` | releases (filters: `app`, `environment`, `state`, `limit`) |
//! | `POST /releases/{id}/cancel`, `/releases/{id}/rollback` | |
//! | `POST /releases/{id}/approve`, `/releases/{id}/reject` | a person's answer to a release waiting for approval (`{note}`) |
//!
//! A release, rollback or cancel asked for with an `Idempotency-Key` header (or
//! `idempotency_key`) is the same one when asked for again.

use std::collections::HashMap;
use std::sync::Arc;

use http::{Method, StatusCode};
use hyper::body::Incoming;
use serde_json::{Value, json};

use super::flow::{self, Ask, Fail, RollbackAsk, Who};
use super::model::{App, Environment, State, Template};
use super::{Deploy, argocd, kilncd, reads, store};
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

const LIMIT: usize = 4 << 20;

fn bad(msg: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, msg.into())
}

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

async fn body<T: serde::de::DeserializeOwned>(body: Incoming) -> Result<T, Fail> {
    let bytes = srv::body_bytes(body, LIMIT).await.map_err(|e| bad(format!("{e:#}")))?;
    let bytes = if bytes.is_empty() { &b"{}"[..] } else { &bytes[..] };
    serde_json::from_slice(bytes).map_err(|e| bad(format!("the body doesn't read: {e}")))
}

fn ok(v: &impl serde::Serialize) -> Result<Response, Fail> {
    Ok(srv::json(StatusCode::OK, v))
}

fn created(v: &impl serde::Serialize) -> Result<Response, Fail> {
    Ok(srv::json(StatusCode::CREATED, v))
}

/// What an approval or a rejection says.
#[derive(Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Note {
    #[serde(default)]
    note: Option<String>,
}

fn release_id(s: &str) -> Result<i64, Fail> {
    s.parse().map_err(|_| bad(format!("{s:?} isn't a release number")))
}

/// Routes `/api/deploy/<rest>`.
pub async fn route(_hub: &Arc<Hub>, rest: &[&str], q: &HashMap<String, String>, req: Request) -> Result<Response, Fail> {
    let d = super::get().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "deploying by API isn't running here (it runs with [cd] enabled)".to_owned()))?;
    let who = req.extensions().get::<crate::authz::Caller>().map_or_else(|| Who { name: "admin".to_owned(), agent: false }, Who::of);
    let actor = who.name.clone();
    // A template that makes repositories is put by someone who may add them.
    let adds_repos = req.extensions().get::<crate::authz::Caller>().is_none_or(|c| c.can("settings.manage", &crate::authz::Scope::Server));
    let key = req.headers().get("idempotency-key").and_then(|v| v.to_str().ok()).map(str::to_owned);
    let method = req.method().clone();
    let store = &d.hub.store;
    match (&method, rest) {
        (&Method::GET, ["environments"]) => ok(&json!({ "environments": store::environments(store).await.map_err(internal)? })),
        (&Method::POST, ["environments"]) => created(&flow::put_environment(&d, &who, body::<Environment>(req.into_body()).await?).await?),
        (&Method::GET, ["environments", name]) => ok(&store::environment(store, name).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no environment {name}")))?),
        (&Method::PUT, ["environments", name]) => {
            let e: Environment = body(req.into_body()).await?;
            if e.name != *name {
                return Err(bad(format!("the body is environment {}, the path {name}", e.name)));
            }
            ok(&flow::put_environment(&d, &who, e).await?)
        }
        (&Method::DELETE, ["environments", name]) => {
            flow::delete_environment(&d, &actor, name).await?;
            Ok(srv::status(StatusCode::NO_CONTENT))
        }

        (&Method::GET, ["board"]) => ok(&board(&d).await?),
        (&Method::GET, ["metrics"]) => {
            let days = q.get("days").and_then(|x| x.parse().ok()).unwrap_or(30i64).clamp(1, 365);
            let list = store::releases(store, store::ReleaseQuery { app: q.get("app").cloned(), environment: q.get("environment").cloned(), limit: 1000, ..store::ReleaseQuery::default() })
                .await
                .map_err(internal)?;
            let mut m = super::metrics::compute(&list, crate::util::now_unix(), days);
            if let Some(o) = m.as_object_mut() {
                o.insert("app".into(), json!(q.get("app")));
                o.insert("environment".into(), json!(q.get("environment")));
            }
            ok(&m)
        }
        (&Method::GET, ["templates"]) => ok(&json!({ "templates": store::templates(store).await.map_err(internal)? })),
        (&Method::POST, ["templates"]) => created(&flow::put_template(&d, &actor, body::<Template>(req.into_body()).await?, adds_repos).await?),
        (&Method::GET, ["templates", name]) => ok(&store::template(store, name).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no template {name}")))?),
        (&Method::PUT, ["templates", name]) => {
            let t: Template = body(req.into_body()).await?;
            if t.name != *name {
                return Err(bad(format!("the body is template {}, the path {name}", t.name)));
            }
            ok(&flow::put_template(&d, &actor, t, adds_repos).await?)
        }
        (&Method::DELETE, ["templates", name]) => {
            flow::delete_template(&d, &actor, name).await?;
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        (&Method::GET, ["app-names", name]) => ok(&flow::app_name_free(&d, name).await?),

        (&Method::GET, ["apps"]) => ok(&json!({ "apps": store::apps(store).await.map_err(internal)? })),
        (&Method::POST, ["apps"]) => created(&flow::create_app(&d, &actor, body::<flow::AppAsk>(req.into_body()).await?).await?),
        (&Method::PATCH, ["apps", name]) => ok(&flow::patch_app(&d, &actor, name, body::<flow::AppPatch>(req.into_body()).await?).await?),
        (&Method::GET, ["apps", name, "catalog-info.yaml"]) => {
            let a = store::app(store, name).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no app {name}")))?;
            Ok(srv::response(StatusCode::OK, "application/yaml", srv::full(flow::catalog_info(&a))))
        }
        (&Method::GET, ["apps", name]) => ok(&app_json(&d, name).await?),
        (&Method::PUT, ["apps", name]) => {
            let a: App = body(req.into_body()).await?;
            if a.name != *name {
                return Err(bad(format!("the body is app {}, the path {name}", a.name)));
            }
            ok(&flow::put_app(&d, &actor, a).await?)
        }
        (&Method::DELETE, ["apps", name]) => {
            let yes = |k: &str| q.get(k).is_some_and(|v| v == "true" || v == "1");
            flow::delete_app(&d, &who, name, yes("everywhere"), yes("keep")).await?;
            Ok(srv::status(StatusCode::NO_CONTENT))
        }

        (&Method::POST, ["apps", app, "environments", env]) => created(&flow::register(&d, &actor, app, env).await?),
        (&Method::GET, ["apps", app, "environments", env]) => ok(&registration_json(&d, app, env).await?),
        (&Method::DELETE, ["apps", app, "environments", env]) => {
            flow::deregister(&d, &who, app, env, q.get("keep").is_some_and(|v| v == "true" || v == "1")).await?;
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        (&Method::POST, ["apps", app, "environments", env, "pause"]) => ok(&flow::pause(&d, &actor, app, env, true).await?),
        (&Method::POST, ["apps", app, "environments", env, "resume"]) => ok(&flow::pause(&d, &actor, app, env, false).await?),
        (&Method::GET, ["apps", app, "environments", env, what @ ("resources" | "manifests" | "diff" | "logs")]) => {
            let a = store::app(store, app).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no app {app}")))?;
            let environment = store::environment(store, env).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no environment {env}")))?;
            store::registration(store, app, env).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("{app} isn't registered in {env}")))?;
            let version = q.get("version").filter(|v| !v.is_empty()).cloned();
            let read = match *what {
                "resources" => reads::Read::Resources,
                "manifests" => reads::Read::Manifests { version },
                "diff" => reads::Read::Diff { version },
                _ => reads::Read::Logs {
                    group: q.get("group").cloned(),
                    kind: q.get("kind").cloned().ok_or_else(|| bad("logs of what? ?kind=Deployment&name=web"))?,
                    name: q.get("name").cloned().ok_or_else(|| bad("logs of what? ?kind=Deployment&name=web"))?,
                    namespace: q.get("namespace").cloned().or_else(|| Some(a.namespace().to_owned())),
                    container: q.get("container").cloned(),
                    tail: q.get("tail").and_then(|t| t.parse().ok()).unwrap_or(200).clamp(1, 10_000),
                },
            };
            ok(&reads::read(&d, &a, &environment, read).await?)
        }
        (&Method::GET, ["apps", app, "environments", env, "versions"]) => ok(&json!({ "versions": versions(&d, app, env).await? })),
        (&Method::GET, ["apps", app, "environments", env, "candidates"]) => ok(&json!({ "candidates": flow::candidates(&d, app, env).await? })),

        (&Method::GET, ["releases"]) => {
            let state = match q.get("state") {
                Some(s) => Some(State::parse(s).ok_or_else(|| bad(format!("{s:?} isn't a release state")))?),
                None => None,
            };
            let query = store::ReleaseQuery {
                app: q.get("app").cloned(),
                environment: q.get("environment").cloned(),
                state,
                version: q.get("version").filter(|v| !v.is_empty()).cloned(),
                limit: q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(50),
            };
            ok(&json!({ "releases": store::releases(store, query).await.map_err(internal)? }))
        }
        (&Method::POST, ["releases"]) => {
            let mut ask: Ask = body(req.into_body()).await?;
            ask.idempotency_key = ask.idempotency_key.or(key);
            created(&flow::release(&d, &who, ask).await?)
        }
        (&Method::GET, ["releases", id]) => ok(&store::release(store, release_id(id)?).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no release {id}")))?),
        (&Method::POST, ["releases", id, "approve"]) => ok(&flow::answer(&d, &who, release_id(id)?, true, body::<Note>(req.into_body()).await?.note).await?),
        (&Method::POST, ["releases", id, "reject"]) => ok(&flow::answer(&d, &who, release_id(id)?, false, body::<Note>(req.into_body()).await?.note).await?),
        (&Method::POST, ["releases", id, "cancel"]) => ok(&flow::cancel(&d, &who, release_id(id)?, key.is_some()).await?),
        (&Method::POST, ["releases", id, "rollback"]) => {
            let of = store::release(store, release_id(id)?).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no release {id}")))?;
            let to: RollbackAsk = body(req.into_body()).await?;
            created(&flow::release(&d, &who, Ask { app: of.app, environment: of.environment, rollback: Some(to), idempotency_key: key, ..Ask::default() }).await?)
        }
        _ => Err((StatusCode::NOT_FOUND, "not found".into())),
    }
}

/// Every registration with what runs and what's in flight, and what waits for people: the
/// environments board, in one answer.
async fn board(d: &Deploy) -> Result<Value, Fail> {
    let st = &d.hub.store;
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
    let st = &d.hub.store;
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
    let a = store::app(&d.hub.store, name).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no app {name}")))?;
    let regs = store::registrations(&d.hub.store, Some(name), None).await.map_err(internal)?;
    let mut envs = Vec::new();
    for reg in regs {
        let running = match reg.running {
            Some(id) => store::release(&d.hub.store, id).await.map_err(internal)?,
            None => None,
        };
        envs.push(json!({ "registration": reg, "running": running }));
    }
    Ok(json!({ "app": a, "environments": envs }))
}

/// A registration: what runs, what's in flight, the latest releases, and what its provider says now.
pub(super) async fn registration_json(d: &Deploy, app: &str, env: &str) -> Result<Value, Fail> {
    let a = store::app(&d.hub.store, app).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no app {app}")))?;
    let environment = store::environment(&d.hub.store, env).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no environment {env}")))?;
    let reg = store::registration(&d.hub.store, app, env).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("{app} isn't registered in {env}")))?;
    let recent = store::releases(&d.hub.store, store::ReleaseQuery { app: Some(app.into()), environment: Some(env.into()), state: None, version: None, limit: 20 }).await.map_err(internal)?;
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
