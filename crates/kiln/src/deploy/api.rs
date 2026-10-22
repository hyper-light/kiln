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
//! | `POST /fleets`, `GET /fleets/{id}` | a fleet change: one version, or values set, across many apps of an environment |
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

use http::{Method, StatusCode};
use hyper::body::Incoming;
use serde_json::{Value, json};

use super::flow::{self, Ask, Fail, RollbackAsk, Who};
use super::model::{App, Environment, Release, State, Template};
use super::{Deploy, actions, argocd, kilncd, link, moves, reads, store};
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;
use crate::secrets::{self, Scope};

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

/// An app and an environment it's registered in.
async fn registered(d: &Deploy, app: &str, env: &str) -> Result<(App, Environment), Fail> {
    let a = store::app(&d.hub.store, app).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no app {app}")))?;
    let environment = store::environment(&d.hub.store, env).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no environment {env}")))?;
    store::registration(&d.hub.store, app, env).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("{app} isn't registered in {env}")))?;
    Ok((a, environment))
}

fn release_id(s: &str) -> Result<i64, Fail> {
    s.parse().map_err(|_| bad(format!("{s:?} isn't a release number")))
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

async fn routed(_hub: &Arc<Hub>, rest: &[&str], q: &HashMap<String, String>, req: Request) -> Result<Response, Fail> {
    let d = super::get().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "deploying by API isn't running here (it runs with [cd] enabled)".to_owned()))?;
    let who = req.extensions().get::<crate::authz::Caller>().map_or_else(|| Who::kiln("admin"), Who::of);
    let actor = who.name.clone();
    // A template that makes repositories is put by someone who may add them.
    let adds_repos = req.extensions().get::<crate::authz::Caller>().is_none_or(|c| c.can("settings.manage", &crate::authz::Scope::Server));
    let key = req.headers().get("idempotency-key").and_then(|v| v.to_str().ok()).map(str::to_owned);
    let method = req.method().clone();
    let store = &d.hub.store;
    match (&method, rest) {
        (&Method::GET, ["environments"]) => ok(&json!({ "environments": store::environments(store).await.map_err(internal)? })),
        (&Method::POST, ["environments"]) => {
            let e: Environment = body(req.into_body()).await?;
            created(&boxed(|| flow::put_environment(&d, &who, e)).await?)
        }
        (&Method::GET, ["environments", name]) => ok(&store::environment(store, name).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no environment {name}")))?),
        (&Method::PUT, ["environments", name]) => {
            let e: Environment = body(req.into_body()).await?;
            if e.name != *name {
                return Err(bad(format!("the body is environment {}, the path {name}", e.name)));
            }
            ok(&boxed(|| flow::put_environment(&d, &who, e)).await?)
        }
        (&Method::DELETE, ["environments", name]) => {
            boxed(|| flow::delete_environment(&d, &actor, name)).await?;
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        // Moving an environment between providers (§8.4).
        (&Method::GET, ["environments", name, "move"]) => match moves::get(&d, name).await.map_err(internal)? {
            Some(m) => ok(&m),
            None => Err((StatusCode::NOT_FOUND, format!("{name} hasn't moved between providers"))),
        },
        (&Method::POST, ["environments", name, "move"]) => {
            let ask: moves::Ask = body(req.into_body()).await?;
            Ok(srv::json(StatusCode::ACCEPTED, &boxed(|| moves::start(&d, &who, name, ask)).await?))
        }
        (&Method::DELETE, ["environments", name, "move"]) => ok(&boxed(|| moves::cancel(&d, &who, name)).await?),

        (&Method::GET, ["board"]) => ok(&boxed(|| board(&d)).await?),
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
        (&Method::POST, ["templates"]) => {
            let t: Template = body(req.into_body()).await?;
            created(&boxed(|| flow::put_template(&d, &actor, t, adds_repos)).await?)
        }
        (&Method::GET, ["templates", name]) => ok(&store::template(store, name).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no template {name}")))?),
        (&Method::PUT, ["templates", name]) => {
            let t: Template = body(req.into_body()).await?;
            if t.name != *name {
                return Err(bad(format!("the body is template {}, the path {name}", t.name)));
            }
            ok(&boxed(|| flow::put_template(&d, &actor, t, adds_repos)).await?)
        }
        (&Method::DELETE, ["templates", name]) => {
            boxed(|| flow::delete_template(&d, &actor, name)).await?;
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        (&Method::GET, ["app-names", name]) => ok(&boxed(|| flow::app_name_free(&d, name)).await?),

        (&Method::GET, ["apps"]) => ok(&json!({ "apps": store::apps(store).await.map_err(internal)? })),
        (&Method::POST, ["apps"]) => {
            let ask: flow::AppAsk = body(req.into_body()).await?;
            created(&boxed(|| flow::create_app(&d, &who, ask)).await?)
        }
        (&Method::PATCH, ["apps", name]) => {
            let patch: flow::AppPatch = body(req.into_body()).await?;
            ok(&boxed(|| flow::patch_app(&d, &who, name, patch)).await?)
        }
        (&Method::GET, ["apps", name, "catalog-info.yaml"]) => {
            let a = store::app(store, name).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no app {name}")))?;
            Ok(srv::response(StatusCode::OK, "application/yaml", srv::full(flow::catalog_info(&a))))
        }
        (&Method::GET, ["apps", name]) => ok(&boxed(|| app_json(&d, name)).await?),
        (&Method::PUT, ["apps", name]) => {
            let a: App = body(req.into_body()).await?;
            if a.name != *name {
                return Err(bad(format!("the body is app {}, the path {name}", a.name)));
            }
            ok(&boxed(|| flow::put_app(&d, &who, a)).await?)
        }
        (&Method::DELETE, ["apps", name]) => {
            let yes = |k: &str| q.get(k).is_some_and(|v| v == "true" || v == "1");
            boxed(|| flow::delete_app(&d, &who, name, yes("everywhere"), yes("keep"))).await?;
            Ok(srv::status(StatusCode::NO_CONTENT))
        }

        (&Method::POST, ["apps", app, "environments", env]) => created(&boxed(|| flow::register(&d, &actor, app, env)).await?),
        (&Method::GET, ["apps", app, "environments", env]) => ok(&boxed(|| registration_json(&d, app, env)).await?),
        (&Method::DELETE, ["apps", app, "environments", env]) => {
            let keep = q.get("keep").is_some_and(|v| v == "true" || v == "1");
            boxed(|| flow::deregister(&d, &who, app, env, keep)).await?;
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        (&Method::POST, ["apps", app, "environments", env, "pause"]) => ok(&boxed(|| flow::pause(&d, &actor, app, env, true)).await?),
        (&Method::POST, ["apps", app, "environments", env, "resume"]) => ok(&boxed(|| flow::pause(&d, &actor, app, env, false)).await?),
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
            ok(&boxed(|| reads::read(&d, &a, &environment, read)).await?)
        }
        // What can be done to an object a registration runs, and doing it (§8.1).
        (&Method::GET, ["apps", app, "environments", env, "actions"]) => {
            let (a, environment) = registered(&d, app, env).await?;
            let target = actions::Target {
                group: q.get("group").cloned(),
                kind: q.get("kind").cloned().ok_or_else(|| bad("actions on what? ?kind=Deployment&name=web"))?,
                namespace: q.get("namespace").cloned().filter(|n| !n.is_empty()),
                name: q.get("name").cloned().ok_or_else(|| bad("actions on what? ?kind=Deployment&name=web"))?,
            };
            ok(&json!({ "actions": boxed(|| actions::list(&d, &a, &environment, &target)).await? }))
        }
        (&Method::POST, ["apps", app, "environments", env, "actions"]) => {
            let ask: actions::Ask = body(req.into_body()).await?;
            let (a, environment) = registered(&d, app, env).await?;
            boxed(|| flow::may_release(&d, &who, &a, &environment, "act on what runs of")).await?;
            ok(&boxed(|| actions::run(&d, &who, &a, &environment, &ask)).await?)
        }
        (&Method::GET, ["apps", app, "environments", env, "versions"]) => ok(&json!({ "versions": boxed(|| versions(&d, app, env)).await? })),
        (&Method::GET, ["apps", app, "environments", env, "secrets"]) => ok(&boxed(|| secrets_list(&d, &who, app, env)).await?),
        (&Method::PUT, ["apps", app, "environments", env, "secrets", name]) => {
            let value = secret_value(req.into_body()).await?;
            let how = Change::of(q);
            ok(&boxed(|| secret_put(&d, &who, app, env, name, value, how)).await?)
        }
        (&Method::DELETE, ["apps", app, "environments", env, "secrets", name]) => {
            let how = Change::of(q);
            ok(&boxed(|| secret_delete(&d, &who, app, env, name, how)).await?)
        }
        (&Method::GET, ["apps", app, "environments", env, "secrets", name, "versions"]) => ok(&boxed(|| secret_versions(&d, &who, app, env, name)).await?),
        (&Method::POST, ["secret-keys", "rotate"]) => {
            let n = secrets::of(&d.hub).rotate(&caller(&who)).await.map_err(refused)?;
            ok(&json!({ "resealed": n }))
        }
        (&Method::GET, ["apps", app, "environments", env, "secret-uses"]) => {
            let limit = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(100);
            ok(&boxed(|| secret_uses(&d, &who, app, env, limit)).await?)
        }
        (&Method::GET, ["apps", app, "environments", env, "candidates"]) => ok(&json!({ "candidates": boxed(|| flow::candidates(&d, app, env)).await? })),
        // For linked servers (§11): where this one is and what it signs with, and what runs of an
        // app in one of its environments, signed for the server asking (`audience`, its issuer).
        (&Method::GET, ["link"]) => ok(&link::about(&d.hub)),
        // Where release repositories may be (§7): kiln's own git server, GitHub through kiln's App,
        // each GitLab with the groups its token covers, Bitbucket with its workspaces.
        (&Method::GET, ["git-hosts"]) => ok(&json!({ "hosts": boxed(|| git_hosts(&d.hub)).await })),
        (&Method::GET, ["apps", app, "environments", env, "statement"]) => {
            let audience = q.get("audience").map(|a| a.trim()).filter(|a| !a.is_empty() && a.len() <= 512).ok_or_else(|| bad("`audience`: the issuer of the server asking"))?;
            match boxed(|| link::statement(&d, app, env, audience)).await.map_err(internal)? {
                Some(statement) => ok(&json!({ "statement": statement })),
                None => Err((StatusCode::NOT_FOUND, format!("{app} isn't registered in {env}"))),
            }
        }

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
            created(&boxed(|| flow::release(&d, &who, ask)).await?)
        }
        (&Method::POST, ["fleets"]) => {
            let mut f: flow::FleetAsk = body(req.into_body()).await?;
            f.idempotency_key = f.idempotency_key.or(key);
            created(&boxed(|| flow::fleet(&d, &who, f)).await?)
        }
        (&Method::GET, ["fleets", id]) => ok(&boxed(|| flow::fleet_status(&d, id)).await?),
        (&Method::GET, ["releases", id]) => ok(&store::release(store, release_id(id)?).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no release {id}")))?),
        (&Method::POST, ["releases", id, verdict @ ("approve" | "reject")]) => {
            let (id, note) = (release_id(id)?, body::<Note>(req.into_body()).await?.note);
            ok(&boxed(|| flow::answer(&d, &who, id, *verdict == "approve", note)).await?)
        }
        (&Method::POST, ["releases", id, "cancel"]) => {
            let id = release_id(id)?;
            ok(&boxed(|| flow::cancel(&d, &who, id, key.is_some())).await?)
        }
        (&Method::POST, ["releases", id, "rollback"]) => {
            let of = store::release(store, release_id(id)?).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no release {id}")))?;
            let to: RollbackAsk = body(req.into_body()).await?;
            let ask = Ask { app: of.app, environment: of.environment, rollback: Some(to), idempotency_key: key, ..Ask::default() };
            created(&boxed(|| flow::release(&d, &who, ask)).await?)
        }
        _ => Err((StatusCode::NOT_FOUND, "not found".into())),
    }
}

/// The git hosts release repositories may be on (docs/deploy-api.md §7).
async fn git_hosts(hub: &Hub) -> Vec<Value> {
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

/// An app's secrets in an environment (§3.5) belong to an app and an environment that are there.
async fn secret_scope(d: &Deploy, app: &str, env: &str) -> Result<Scope, Fail> {
    store::app(&d.hub.store, app).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no app {app}")))?;
    store::environment(&d.hub.store, env).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no environment {env}")))?;
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

/// A secret's value as sent: `{value}` (text) or `{base64}` (bytes). What doesn't read is refused
/// without saying what it was, which may be the value.
async fn secret_value(body: Incoming) -> Result<secrets::Value, Fail> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Put {
        #[serde(default)]
        value: Option<String>,
        #[serde(default)]
        base64: Option<String>,
    }
    let bytes = srv::body_bytes(body, LIMIT).await.map_err(|e| bad(format!("{e:#}")))?;
    let put: Put = serde_json::from_slice(&bytes).map_err(|_| bad("a secret is set with `{\"value\": \"…\"}`, or `{\"base64\": \"…\"}` for bytes"))?;
    match (put.value, put.base64) {
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
async fn secret_put(d: &Deploy, who: &Who, app: &str, env: &str, name: &str, value: secrets::Value, how: Change) -> Result<Value, Fail> {
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

async fn secret_delete(d: &Deploy, who: &Who, app: &str, env: &str, name: &str, how: Change) -> Result<Value, Fail> {
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
struct Change {
    release: bool,
    revert: bool,
}

impl Change {
    fn of(q: &HashMap<String, String>) -> Change {
        let yes = |k: &str| q.get(k).is_none_or(|v| v != "false" && v != "0");
        Change { release: yes("release"), revert: yes("revert") }
    }
}

async fn released(d: &Deploy, who: &Who, app: &str, env: &str, what: String, how: Change) -> (Option<Release>, String) {
    let st = &d.hub.store;
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

async fn secret_versions(d: &Deploy, who: &Who, app: &str, env: &str, name: &str) -> Result<Value, Fail> {
    let at = secret_scope(d, app, env).await?;
    let versions = secrets::of(&d.hub).versions(&at, name, &caller(who)).await.map_err(refused)?;
    Ok(json!({ "name": name, "versions": versions }))
}

async fn secret_uses(d: &Deploy, who: &Who, app: &str, env: &str, limit: usize) -> Result<Value, Fail> {
    let at = secret_scope(d, app, env).await?;
    let reads = secrets::of(&d.hub).reads(&at, limit, &caller(who)).await.map_err(refused)?;
    Ok(json!({ "reads": reads }))
}

#[cfg(test)]
mod tests {
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
            let d = Deploy { hosted: super::super::host::Hosted::new(root.path()), owner: "test".into(), wake: crate::rt::Notify::new(), changes: crate::rt::Broadcast::new(16), argo: Default::default(), remote: Default::default(), hub: Arc::clone(&hub) };
            let st = &hub.store;
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
            anyhow::ensure!(r.kind == Kind::Config && r.values.is_none() && r.secrets.as_deref() == Some("DB_PASSWORD v2") && r.state == State::Committing, "{r:?}");

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
