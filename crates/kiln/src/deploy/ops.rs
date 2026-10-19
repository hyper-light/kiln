//! Deploying by API's operations (docs/deploy-api.md §12.3): what agents reach through MCP, as
//! tools one by one (`deploy_releases_create`) and as code mode's `kiln.deploy.*`. Each is one of
//! `/api/deploy`'s routes: the route table's rule for it decides who may, and it runs what the
//! route runs, so an agent does through an operation exactly what its caller can over HTTP. The
//! flow decides the rest the same way for both: agents release only where an environment lets
//! them, and an idempotency key makes a retried release the same one.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use http::{Method, StatusCode};
use serde_json::{Value, json};

use super::flow::{self, Ask, RollbackAsk, Who};
use super::model::{App, Environment, State};
use super::{Deploy, api, store};
use crate::ops::{Cx, Fail, Out};

type Fut = Pin<Box<dyn Future<Output = Out> + Send>>;

/// One operation.
pub struct Op {
    /// Its name, dotted (`deploy.releases.create`).
    pub name: &'static str,
    pub summary: &'static str,
    pub read_only: bool,
    pub destructive: bool,
    pub input: fn() -> Value,
    /// The route it is: method, path under `/api/deploy`, query.
    route: fn(&Value) -> Result<(Method, Vec<String>, HashMap<String, String>), Fail>,
    run: fn(Cx, Value) -> Fut,
}

/// The longest a wait waits.
const WAIT_MAX_SECS: u64 = 600;

const APP: &str = "The app's name.";
const ENV: &str = "The environment's name.";
const RELEASE: &str = "The release's number.";
const KEY: &str = "Any string: asked for again with the same key, it is the same release (a retry after a timeout releases nothing twice).";

fn bad(msg: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, msg.into())
}

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn text(v: &Value, k: &str) -> Result<String, Fail> {
    v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned).ok_or_else(|| bad(format!("`{k}` is required")))
}

fn opt_text(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned)
}

fn id(v: &Value) -> Result<i64, Fail> {
    v.get("id").and_then(|x| x.as_i64().or_else(|| x.as_str().and_then(|s| s.parse().ok()))).ok_or_else(|| bad("`id` is required: a release's number"))
}

fn schema(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
}

fn path(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_owned()).collect()
}

fn registration_path(v: &Value, tail: &[&str]) -> Result<Vec<String>, Fail> {
    let mut p = vec!["apps".to_owned(), text(v, "app")?, "environments".to_owned(), text(v, "environment")?];
    p.extend(tail.iter().map(|s| (*s).to_owned()));
    Ok(p)
}

/// The running service.
fn service() -> Result<Arc<Deploy>, Fail> {
    super::get().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "deploying by API isn't running here (it runs with [cd] enabled)".to_owned()))
}

fn answer(v: impl serde::Serialize) -> Out {
    serde_json::to_value(v).map_err(|e| internal(e.into()))
}

macro_rules! op {
    ($name:literal, $summary:literal, ro: $ro:expr, destructive: $d:expr, input: $input:expr, route: $route:expr, run: $run:expr) => {
        Op { name: $name, summary: $summary, read_only: $ro, destructive: $d, input: $input, route: $route, run: |cx, v| Box::pin($run(cx, v)) }
    };
}

/// The operations.
pub fn ops() -> &'static [Op] {
    static OPS: std::sync::OnceLock<Vec<Op>> = std::sync::OnceLock::new();
    OPS.get_or_init(|| {
        vec![
            op!("deploy.environments.list", "The environments apps are released to: each one's CD provider (KilnCD, or an Argo CD), release repository, targets, sync and watch settings, and what agents may do there.", ro: true, destructive: false,
                input: || schema(json!({}), &[]),
                route: |_| Ok((Method::GET, path(&["environments"]), HashMap::new())),
                run: |_cx: Cx, _v: Value| async move { answer(json!({ "environments": store::environments(&service()?.hub.store).await.map_err(internal)? })) }),
            op!("deploy.environments.get", "One environment.", ro: true, destructive: false,
                input: || schema(json!({"environment": {"type": "string", "description": ENV}}), &["environment"]),
                route: |v| Ok((Method::GET, path(&["environments", &text(v, "environment")?]), HashMap::new())),
                run: |_cx: Cx, v: Value| async move {
                    let name = text(&v, "environment")?;
                    answer(store::environment(&service()?.hub.store, &name).await.map_err(internal)?.ok_or_else(|| (StatusCode::NOT_FOUND, format!("no environment {name}")))?)
                }),
            op!("deploy.environments.put", "Create or replace an environment: its provider (`kilncd`, or `argocd/<integration>`), release repository, targets, sync and watch settings. What agents may do there (`agents`) only a person changes.", ro: false, destructive: false,
                input: || schema(json!({"environment": {"type": "object", "description": "The environment, as deploy.environments.get shows one."}}), &["environment"]),
                route: |v| {
                    let name = v.pointer("/environment/name").and_then(Value::as_str).ok_or_else(|| bad("`environment.name` is required"))?;
                    Ok((Method::PUT, path(&["environments", name]), HashMap::new()))
                },
                run: |cx: Cx, v: Value| async move {
                    let e: Environment = serde_json::from_value(v.get("environment").cloned().unwrap_or(Value::Null)).map_err(|e| bad(format!("the environment doesn't read: {e}")))?;
                    answer(flow::put_environment(&*service()?, &Who::of(&cx.caller), e).await?)
                }),
            op!("deploy.apps.list", "Every app kiln releases: its project, owner teams and package.", ro: true, destructive: false,
                input: || schema(json!({}), &[]),
                route: |_| Ok((Method::GET, path(&["apps"]), HashMap::new())),
                run: |_cx: Cx, _v: Value| async move { answer(json!({ "apps": store::apps(&service()?.hub.store).await.map_err(internal)? })) }),
            op!("deploy.apps.get", "An app, and each environment it's registered in with the release that runs there.", ro: true, destructive: false,
                input: || schema(json!({"app": {"type": "string", "description": APP}}), &["app"]),
                route: |v| Ok((Method::GET, path(&["apps", &text(v, "app")?]), HashMap::new())),
                run: |_cx: Cx, v: Value| async move { api::app_json(&*service()?, &text(&v, "app")?).await }),
            op!("deploy.apps.put", "Create or replace an app: its project, owner teams, package (a Helm chart, or a path in a git repository) and the values a release sets.", ro: false, destructive: false,
                input: || schema(json!({"app": {"type": "object", "description": "The app, as deploy.apps.get shows one."}}), &["app"]),
                route: |v| {
                    let name = v.pointer("/app/name").and_then(Value::as_str).ok_or_else(|| bad("`app.name` is required"))?;
                    Ok((Method::PUT, path(&["apps", name]), HashMap::new()))
                },
                run: |cx: Cx, v: Value| async move {
                    let a: App = serde_json::from_value(v.get("app").cloned().unwrap_or(Value::Null)).map_err(|e| bad(format!("the app doesn't read: {e}")))?;
                    answer(flow::put_app(&*service()?, &cx.caller.name, a).await?)
                }),
            op!("deploy.registrations.get", "What runs for an app in an environment, the release in flight, the latest releases, and what its provider (KilnCD or Argo CD) says now.", ro: true, destructive: false,
                input: || schema(json!({"app": {"type": "string", "description": APP}, "environment": {"type": "string", "description": ENV}}), &["app", "environment"]),
                route: |v| Ok((Method::GET, registration_path(v, &[])?, HashMap::new())),
                run: |_cx: Cx, v: Value| async move { api::registration_json(&*service()?, &text(&v, "app")?, &text(&v, "environment")?).await }),
            op!("deploy.registrations.create", "Register an app in an environment, ready to release there.", ro: false, destructive: false,
                input: || schema(json!({"app": {"type": "string", "description": APP}, "environment": {"type": "string", "description": ENV}}), &["app", "environment"]),
                route: |v| Ok((Method::POST, registration_path(v, &[])?, HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(flow::register(&*service()?, &cx.caller.name, &text(&v, "app")?, &text(&v, "environment")?).await?) }),
            op!("deploy.registrations.delete", "Remove an app from an environment: what it runs there goes with it, unless `keep`.", ro: false, destructive: true,
                input: || schema(json!({"app": {"type": "string", "description": APP}, "environment": {"type": "string", "description": ENV}, "keep": {"type": "boolean", "description": "Leave what it runs in place."}}), &["app", "environment"]),
                route: |v| {
                    let keep = v.get("keep").and_then(Value::as_bool).unwrap_or(false);
                    Ok((Method::DELETE, registration_path(v, &[])?, HashMap::from([("keep".to_owned(), keep.to_string())])))
                },
                run: |cx: Cx, v: Value| async move {
                    let (app, env) = (text(&v, "app")?, text(&v, "environment")?);
                    flow::deregister(&*service()?, &Who::of(&cx.caller), &app, &env, v.get("keep").and_then(Value::as_bool).unwrap_or(false)).await?;
                    Ok(json!({ "deregistered": { "app": app, "environment": env } }))
                }),
            op!("deploy.registrations.pause", "Turn the provider's automation off for an app in an environment, so hand edits survive; its next release turns it back on.", ro: false, destructive: false,
                input: || schema(json!({"app": {"type": "string", "description": APP}, "environment": {"type": "string", "description": ENV}}), &["app", "environment"]),
                route: |v| Ok((Method::POST, registration_path(v, &["pause"])?, HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(flow::pause(&*service()?, &cx.caller.name, &text(&v, "app")?, &text(&v, "environment")?, true).await?) }),
            op!("deploy.registrations.resume", "Turn the provider's automation back on for an app in an environment.", ro: false, destructive: false,
                input: || schema(json!({"app": {"type": "string", "description": APP}, "environment": {"type": "string", "description": ENV}}), &["app", "environment"]),
                route: |v| Ok((Method::POST, registration_path(v, &["resume"])?, HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(flow::pause(&*service()?, &cx.caller.name, &text(&v, "app")?, &text(&v, "environment")?, false).await?) }),
            op!("deploy.registrations.candidates", "The releases a rollback can go to: the last ten that succeeded, the one running and any rolled back from left out.", ro: true, destructive: false,
                input: || schema(json!({"app": {"type": "string", "description": APP}, "environment": {"type": "string", "description": ENV}}), &["app", "environment"]),
                route: |v| Ok((Method::GET, registration_path(v, &["candidates"])?, HashMap::new())),
                run: |_cx: Cx, v: Value| async move { answer(json!({ "candidates": flow::candidates(&*service()?, &text(&v, "app")?, &text(&v, "environment")?).await? })) }),
            op!("deploy.releases.list", "Releases, newest first: of an app, in an environment, in a state (waiting, committing, delivering, succeeded, failed, cancelled, rolled_back, superseded).", ro: true, destructive: false,
                input: || schema(json!({
                    "app": {"type": "string", "description": APP}, "environment": {"type": "string", "description": ENV},
                    "state": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 1000}
                }), &[]),
                route: |v| {
                    let mut q = HashMap::new();
                    for k in ["app", "environment", "state"] {
                        if let Some(x) = opt_text(v, k) {
                            q.insert(k.to_owned(), x);
                        }
                    }
                    Ok((Method::GET, path(&["releases"]), q))
                },
                run: |_cx: Cx, v: Value| async move {
                    let state = match opt_text(&v, "state") {
                        Some(s) => Some(State::parse(&s).ok_or_else(|| bad(format!("{s:?} isn't a release state")))?),
                        None => None,
                    };
                    let limit = v.get("limit").and_then(Value::as_u64).and_then(|l| usize::try_from(l).ok()).unwrap_or(50);
                    let q = store::ReleaseQuery { app: opt_text(&v, "app"), environment: opt_text(&v, "environment"), state, limit };
                    answer(json!({ "releases": store::releases(&service()?.hub.store, q).await.map_err(internal)? }))
                }),
            op!("deploy.releases.get", "One release: its kind, state, version, artifacts, commit, who asked, its message, and what its provider last said.", ro: true, destructive: false,
                input: || schema(json!({"id": {"type": "integer", "description": RELEASE}}), &["id"]),
                route: |v| Ok((Method::GET, path(&["releases", &id(v)?.to_string()]), HashMap::new())),
                run: |_cx: Cx, v: Value| async move {
                    let n = id(&v)?;
                    answer(store::release(&service()?.hub.store, n).await.map_err(internal)?.ok_or_else(|| (StatusCode::NOT_FOUND, format!("no release {n}")))?)
                }),
            op!("deploy.releases.create", "Release an app in an environment: a new `version` (with its `artifacts`), new `values` (a config release: the environment's values.yaml), `enabled` true or false, or `promote_from` another environment. Exactly one of them. The environment's gates come first (upstream environments, soak, windows, checks, approvals): one it can't pass is refused (409), one that holds it leaves it `waiting`, or `waiting_approval` for people (deploy.releases.approve). It answers at once, the release `committing` or `waiting`: follow it with deploy.releases.wait. A version that isn't in its registry is refused (422), and agents release only where the environment lets them (403).", ro: false, destructive: true,
                input: || schema(json!({
                    "app": {"type": "string", "description": APP}, "environment": {"type": "string", "description": ENV},
                    "version": {"type": "string", "description": "The version: a chart version, or a git package's branch, tag or commit."},
                    "artifacts": {"type": "object", "description": "Build outputs it deploys, by name: `{repository, tag, digest}`.", "additionalProperties": {"type": "object"}},
                    "values": {"type": "string", "description": "A config release: YAML that becomes the environment's values.yaml."},
                    "enabled": {"type": "boolean", "description": "Run the app here again (true), or stop running it (false)."},
                    "promote_from": {"type": "string", "description": "A promotion: exactly what this environment runs, the same version and artifacts."},
                    "keep": {"type": "boolean", "description": "With enabled false: leave what it runs in place."},
                    "idempotency_key": {"type": "string", "description": KEY}
                }), &["app", "environment"]),
                route: |_| Ok((Method::POST, path(&["releases"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move {
                    let ask: Ask = serde_json::from_value(v).map_err(|e| bad(format!("the request doesn't read: {e}")))?;
                    answer(flow::release(&*service()?, &Who::of(&cx.caller), ask).await?)
                }),
            op!("deploy.releases.rollback", "Go back to an earlier release of an app in an environment, as a new release: its version, artifacts and values. By default the newest that succeeded before what runs (see deploy.registrations.candidates).", ro: false, destructive: true,
                input: || schema(json!({
                    "app": {"type": "string", "description": APP}, "environment": {"type": "string", "description": ENV},
                    "to": {"type": "integer", "description": "The release to go back to."}, "idempotency_key": {"type": "string", "description": KEY}
                }), &["app", "environment"]),
                route: |_| Ok((Method::POST, path(&["releases"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move {
                    let ask = Ask {
                        app: text(&v, "app")?,
                        environment: text(&v, "environment")?,
                        rollback: Some(RollbackAsk { to: v.get("to").and_then(Value::as_i64) }),
                        idempotency_key: opt_text(&v, "idempotency_key"),
                        ..Ask::default()
                    };
                    answer(flow::release(&*service()?, &Who::of(&cx.caller), ask).await?)
                }),
            op!("deploy.releases.cancel", "Stop a release that is waiting, or being delivered: one being delivered is reverted in git, and its provider goes back to what ran before.", ro: false, destructive: false,
                input: || schema(json!({"id": {"type": "integer", "description": RELEASE}, "idempotency_key": {"type": "string", "description": "With one, a release cancelled already is no error."}}), &["id"]),
                route: |v| Ok((Method::POST, path(&["releases", &id(v)?.to_string(), "cancel"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(flow::cancel(&*service()?, &Who::of(&cx.caller), id(&v)?, opt_text(&v, "idempotency_key").is_some()).await?) }),
            op!("deploy.releases.approve", "Approve a release waiting for approval (`waiting_approval`): the approval counts toward its environment's, and the release goes once there are enough. Agents answer only where the environment lets them (agents: { approve: true }), and whoever asked for a release doesn't approve it unless the environment says so.", ro: false, destructive: false,
                input: || schema(json!({"id": {"type": "integer", "description": RELEASE}, "note": {"type": "string", "description": "Why, for the record."}}), &["id"]),
                route: |v| Ok((Method::POST, path(&["releases", &id(v)?.to_string(), "approve"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(flow::answer(&*service()?, &Who::of(&cx.caller), id(&v)?, true, opt_text(&v, "note")).await?) }),
            op!("deploy.releases.reject", "Reject a release waiting for approval: it is cancelled, and says who rejected it and why.", ro: false, destructive: false,
                input: || schema(json!({"id": {"type": "integer", "description": RELEASE}, "note": {"type": "string", "description": "Why."}}), &["id"]),
                route: |v| Ok((Method::POST, path(&["releases", &id(v)?.to_string(), "reject"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(flow::answer(&*service()?, &Who::of(&cx.caller), id(&v)?, false, opt_text(&v, "note")).await?) }),
            op!("deploy.releases.wait", "Wait for a release to finish (`until: done`, the default) or to move at all (`until: any`), for at most `timeout_s` seconds (600 at most): the release as it is then, whether it is done, and whether the time ran out.", ro: true, destructive: false,
                input: || schema(json!({
                    "id": {"type": "integer", "description": RELEASE}, "until": {"type": "string", "enum": ["done", "any"]},
                    "timeout_s": {"type": "integer", "minimum": 1, "maximum": WAIT_MAX_SECS}
                }), &["id"]),
                route: |v| Ok((Method::GET, path(&["releases", &id(v)?.to_string()]), HashMap::new())),
                run: |_cx: Cx, v: Value| async move { wait(&*service()?, id(&v)?, v.get("until").and_then(Value::as_str) != Some("any"), v.get("timeout_s").and_then(Value::as_u64).unwrap_or(WAIT_MAX_SECS)).await }),
        ]
    })
}

/// Waits on a release: until it is done (or, `done` false, until it moves at all), or the time is
/// up. Woken as it moves on this server; looked at every few seconds besides, since another server
/// may move it.
async fn wait(d: &Deploy, id: i64, done: bool, secs: u64) -> Out {
    let st = &d.hub.store;
    let mut changes = d.changes.subscribe();
    let first = store::release(st, id).await.map_err(internal)?.ok_or_else(|| (StatusCode::NOT_FOUND, format!("no release {id}")))?;
    let seen = (first.state, first.message.clone());
    let finished = |r: &super::model::Release| !r.state.in_flight();
    // Done already: it won't move again.
    if finished(&first) {
        return Ok(json!({ "release": first, "done": true, "timed_out": false }));
    }
    let deadline = Instant::now() + Duration::from_secs(secs.clamp(1, WAIT_MAX_SECS));
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let _ = crate::rt::timeout(left.min(Duration::from_secs(5)), async {
            loop {
                match changes.recv().await {
                    Ok(moved) if moved != id => {}
                    _ => return,
                }
            }
        })
        .await;
        let r = store::release(st, id).await.map_err(internal)?.ok_or_else(|| (StatusCode::NOT_FOUND, format!("no release {id}")))?;
        if finished(&r) || (!done && (r.state, r.message.clone()) != seen) {
            let is_done = finished(&r);
            return Ok(json!({ "release": r, "done": is_done, "timed_out": false }));
        }
    }
    let r = store::release(st, id).await.map_err(internal)?.ok_or_else(|| (StatusCode::NOT_FOUND, format!("no release {id}")))?;
    let is_done = finished(&r);
    Ok(json!({ "release": r, "done": is_done, "timed_out": true }))
}

/// Runs operation `name` for `cx`'s caller, when the route table lets it.
pub async fn call(cx: Cx, name: &str, input: Value) -> Out {
    let op = ops().iter().find(|o| o.name == name).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no operation {name}")))?;
    let (method, rest, q) = (op.route)(&input)?;
    let path = format!("/api/deploy/{}", rest.join("/"));
    let run = async {
        let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
        crate::authz::gate::check(&cx.hub, &cx.caller, &method, &segs, &q).await?;
        (op.run)(cx.clone(), input).await
    };
    if op.read_only {
        return run.await;
    }
    // A change is audited as its route's would be (docs/ci.md W19-R05): one row.
    crate::audit::request(&cx.hub, crate::audit::Req { caller: &cx.caller, method: &method, path: &path, q: &q, ip: None }, run, |r: &Out| match r {
        Ok(_) => 200,
        Err((s, _)) => s.as_u16(),
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_operation_is_a_route_of_its_own_kind() -> Result<(), Fail> {
        let input = json!({ "app": "web", "environment": "staging", "id": 7 });
        for op in ops() {
            let v = match op.name {
                "deploy.environments.put" => json!({ "environment": { "name": "staging" } }),
                "deploy.apps.put" => json!({ "app": { "name": "web" } }),
                _ => input.clone(),
            };
            let (method, rest, _) = (op.route)(&v)?;
            assert!(!rest.is_empty(), "{} names a route", op.name);
            assert_eq!(method == Method::GET, op.read_only, "{}: a read is a GET, a change isn't", op.name);
            assert!(op.name.starts_with("deploy."), "{}", op.name);
            let schema = (op.input)();
            assert_eq!(schema.get("type").and_then(Value::as_str), Some("object"), "{}", op.name);
        }
        let release = ops().iter().find(|o| o.name == "deploy.releases.create").ok_or_else(|| bad("deploy.releases.create"))?;
        assert!(release.destructive, "a release changes what runs");
        Ok(())
    }
}
