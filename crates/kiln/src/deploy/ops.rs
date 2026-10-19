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
use super::model::{App, Environment, State, Template};
use super::{Deploy, api, reads, store};
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
            op!("deploy.templates.list", "The templates apps are made from (txst's app types): each one's package, namespace pattern, checks, properties and environments.", ro: true, destructive: false,
                input: || schema(json!({}), &[]),
                route: |_| Ok((Method::GET, path(&["templates"]), HashMap::new())),
                run: |_cx: Cx, _v: Value| async move { answer(json!({ "templates": store::templates(&service()?.hub.store).await.map_err(internal)? })) }),
            op!("deploy.templates.put", "Create or replace a template: the package its apps start with (`{app}` is the app's name), their namespace pattern (`apps-{app}`), the checks their releases pass, the properties they may have, the environments they may go to, and the source repository kiln makes for each (`repository: {name: \"acme/{app}\", starter: <git URL>, starter_ref?, branch?}`: its first commit the starter's files, `{{kiln.app}}` filled in). A template that makes repositories is put by someone who may add them (settings.manage).", ro: false, destructive: false,
                input: || schema(json!({"template": {"type": "object", "description": "The template, as deploy.templates.list shows one."}}), &["template"]),
                route: |v| {
                    let name = v.pointer("/template/name").and_then(Value::as_str).ok_or_else(|| bad("`template.name` is required"))?;
                    Ok((Method::PUT, path(&["templates", name]), HashMap::new()))
                },
                run: |cx: Cx, v: Value| async move {
                    let t: Template = serde_json::from_value(v.get("template").cloned().unwrap_or(Value::Null)).map_err(|e| bad(format!("the template doesn't read: {e}")))?;
                    answer(flow::put_template(&*service()?, &cx.caller.name, t, cx.caller.can("settings.manage", &crate::authz::Scope::Server)).await?)
                }),
            op!("deploy.apps.create", "Create an app: its name, owner teams, and how it is deployed, or a template that says how (its package, namespace and checks follow, and the source repository it makes, from its starter). `source` names an existing kiln repository instead (`\"\"`: none). Then register it in environments.", ro: false, destructive: false,
                input: || schema(json!({"app": {"type": "object", "description": "{name, project?, owners?, template?, package?, namespace?, properties?, contacts?, source?}"}}), &["app"]),
                route: |_| Ok((Method::POST, path(&["apps"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move {
                    let ask: flow::AppAsk = serde_json::from_value(v.get("app").cloned().unwrap_or(Value::Null)).map_err(|e| bad(format!("the app doesn't read: {e}")))?;
                    answer(flow::create_app(&*service()?, &cx.caller.name, ask).await?)
                }),
            op!("deploy.apps.patch", "Change some of an app: owners, properties (null removes one), contacts (on call, channels, links), template, package, namespace, source (a kiln repository, `\"\"` for none), archived. A package's change reaches each environment with its next release.", ro: false, destructive: false,
                input: || schema(json!({"app": {"type": "string", "description": APP}, "patch": {"type": "object"}}), &["app", "patch"]),
                route: |v| Ok((Method::PATCH, path(&["apps", &text(v, "app")?]), HashMap::new())),
                run: |cx: Cx, v: Value| async move {
                    let p: flow::AppPatch = serde_json::from_value(v.get("patch").cloned().unwrap_or(Value::Null)).map_err(|e| bad(format!("the patch doesn't read: {e}")))?;
                    answer(flow::patch_app(&*service()?, &cx.caller.name, &text(&v, "app")?, p).await?)
                }),
            op!("deploy.apps.delete", "Delete an app. With `everywhere`, it is deregistered from every environment first, what it runs deleted (or kept, with `keep`); otherwise an app still registered somewhere is refused. Its source repository is kept.", ro: false, destructive: true,
                input: || schema(json!({"app": {"type": "string", "description": APP}, "everywhere": {"type": "boolean"}, "keep": {"type": "boolean"}}), &["app"]),
                route: |v| {
                    let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false).to_string();
                    Ok((Method::DELETE, path(&["apps", &text(v, "app")?]), HashMap::from([("everywhere".to_owned(), flag("everywhere")), ("keep".to_owned(), flag("keep"))])))
                },
                run: |cx: Cx, v: Value| async move {
                    let app = text(&v, "app")?;
                    let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
                    flow::delete_app(&*service()?, &Who::of(&cx.caller), &app, flag("everywhere"), flag("keep")).await?;
                    Ok(json!({ "deleted": app }))
                }),
            op!("deploy.apps.catalog_info", "An app as Backstage's catalog reads it: its catalog-info.yaml.", ro: true, destructive: false,
                input: || schema(json!({"app": {"type": "string", "description": APP}}), &["app"]),
                route: |v| Ok((Method::GET, path(&["apps", &text(v, "app")?, "catalog-info.yaml"]), HashMap::new())),
                run: |_cx: Cx, v: Value| async move {
                    let name = text(&v, "app")?;
                    let a = store::app(&service()?.hub.store, &name).await.map_err(internal)?.ok_or_else(|| (StatusCode::NOT_FOUND, format!("no app {name}")))?;
                    Ok(json!({ "catalog_info": flow::catalog_info(&a) }))
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
            op!("deploy.registrations.resources", "What an app runs in an environment, as its provider (KilnCD or Argo CD) sees it: each object with its sync, health and why.", ro: true, destructive: false,
                input: || schema(json!({"app": {"type": "string", "description": APP}, "environment": {"type": "string", "description": ENV}}), &["app", "environment"]),
                route: |v| Ok((Method::GET, registration_path(v, &["resources"])?, HashMap::new())),
                run: |_cx: Cx, v: Value| async move { registration_read(&v, |_| Ok(reads::Read::Resources)).await }),
            op!("deploy.registrations.manifests", "The manifests an app renders in an environment (Secrets masked); on Argo CD, those another `version` would render.", ro: true, destructive: false,
                input: || schema(json!({"app": {"type": "string", "description": APP}, "environment": {"type": "string", "description": ENV}, "version": {"type": "string"}}), &["app", "environment"]),
                route: |v| Ok((Method::GET, registration_path(v, &["manifests"])?, HashMap::new())),
                run: |_cx: Cx, v: Value| async move { registration_read(&v, |v| Ok(reads::Read::Manifests { version: opt_text(v, "version") })).await }),
            op!("deploy.registrations.diff", "What a sync would change for an app in an environment, per object (live, predicted, unified hunks); on Argo CD, what deploying another `version` would change.", ro: true, destructive: false,
                input: || schema(json!({"app": {"type": "string", "description": APP}, "environment": {"type": "string", "description": ENV}, "version": {"type": "string"}}), &["app", "environment"]),
                route: |v| Ok((Method::GET, registration_path(v, &["diff"])?, HashMap::new())),
                run: |_cx: Cx, v: Value| async move { registration_read(&v, |v| Ok(reads::Read::Diff { version: opt_text(v, "version") })).await }),
            op!("deploy.registrations.logs", "The last lines of a pod's log, or of a workload's pods (`kind`, `name`), for an app in an environment.", ro: true, destructive: false,
                input: || schema(json!({
                    "app": {"type": "string", "description": APP}, "environment": {"type": "string", "description": ENV},
                    "kind": {"type": "string", "description": "Deployment, StatefulSet, Pod, …"}, "name": {"type": "string"}, "group": {"type": "string"},
                    "namespace": {"type": "string"}, "container": {"type": "string"}, "tail": {"type": "integer", "minimum": 1, "maximum": 10000}
                }), &["app", "environment", "kind", "name"]),
                route: |v| Ok((Method::GET, registration_path(v, &["logs"])?, HashMap::new())),
                run: |_cx: Cx, v: Value| async move {
                    registration_read(&v, |v| {
                        Ok(reads::Read::Logs {
                            group: opt_text(v, "group"),
                            kind: text(v, "kind")?,
                            name: text(v, "name")?,
                            namespace: opt_text(v, "namespace"),
                            container: opt_text(v, "container"),
                            tail: v.get("tail").and_then(Value::as_u64).and_then(|t| u32::try_from(t).ok()).unwrap_or(200).clamp(1, 10_000),
                        })
                    })
                    .await
                }),
            op!("deploy.registrations.candidates", "The releases a rollback can go to: the last ten that succeeded, the one running and any rolled back from left out.", ro: true, destructive: false,
                input: || schema(json!({"app": {"type": "string", "description": APP}, "environment": {"type": "string", "description": ENV}}), &["app", "environment"]),
                route: |v| Ok((Method::GET, registration_path(v, &["candidates"])?, HashMap::new())),
                run: |_cx: Cx, v: Value| async move { answer(json!({ "candidates": flow::candidates(&*service()?, &text(&v, "app")?, &text(&v, "environment")?).await? })) }),
            op!("deploy.releases.list", "Releases, newest first: of an app, in an environment, in a state (waiting, committing, delivering, succeeded, failed, cancelled, rolled_back, superseded).", ro: true, destructive: false,
                input: || schema(json!({
                    "app": {"type": "string", "description": APP}, "environment": {"type": "string", "description": ENV},
                    "state": {"type": "string"}, "version": {"type": "string", "description": "Those of one version."}, "limit": {"type": "integer", "minimum": 1, "maximum": 1000}
                }), &[]),
                route: |v| {
                    let mut q = HashMap::new();
                    for k in ["app", "environment", "state", "version"] {
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
                    let q = store::ReleaseQuery { app: opt_text(&v, "app"), environment: opt_text(&v, "environment"), state, version: opt_text(&v, "version"), limit };
                    answer(json!({ "releases": store::releases(&service()?.hub.store, q).await.map_err(internal)? }))
                }),
            op!("deploy.metrics", "Delivery metrics from the releases (DORA's four keys): how often changes land, lead time from asked to landed, change failure rate (failed or rolled back), time to restore; for an app, an environment, or all, over `days` (30).", ro: true, destructive: false,
                input: || schema(json!({"app": {"type": "string", "description": APP}, "environment": {"type": "string", "description": ENV}, "days": {"type": "integer", "minimum": 1, "maximum": 365}}), &[]),
                route: |v| {
                    let mut q = HashMap::new();
                    for k in ["app", "environment"] {
                        if let Some(x) = opt_text(v, k) {
                            q.insert(k.to_owned(), x);
                        }
                    }
                    Ok((Method::GET, path(&["metrics"]), q))
                },
                run: |_cx: Cx, v: Value| async move {
                    let days = v.get("days").and_then(Value::as_i64).unwrap_or(30).clamp(1, 365);
                    let list = store::releases(&service()?.hub.store, store::ReleaseQuery { app: opt_text(&v, "app"), environment: opt_text(&v, "environment"), limit: 1000, ..store::ReleaseQuery::default() })
                        .await
                        .map_err(internal)?;
                    Ok(super::metrics::compute(&list, crate::util::now_unix(), days))
                }),
            op!("deploy.registrations.versions", "The versions an app has run in an environment, newest first: when each first and last landed, how often, and whether it runs now.", ro: true, destructive: false,
                input: || schema(json!({"app": {"type": "string", "description": APP}, "environment": {"type": "string", "description": ENV}}), &["app", "environment"]),
                route: |v| Ok((Method::GET, registration_path(v, &["versions"])?, HashMap::new())),
                run: |_cx: Cx, v: Value| async move { answer(json!({ "versions": api::versions(&*service()?, &text(&v, "app")?, &text(&v, "environment")?).await? })) }),
            op!("deploy.secrets.list", "An app's secrets in an environment: each one's name, version, size, who set it and when. Never their values: no one reads them back, and agents neither set nor delete secrets (a person does, with `kiln app secret set`).", ro: true, destructive: false,
                input: || schema(json!({"app": {"type": "string", "description": APP}, "environment": {"type": "string", "description": ENV}}), &["app", "environment"]),
                route: |v| Ok((Method::GET, registration_path(v, &["secrets"])?, HashMap::new())),
                run: |cx: Cx, v: Value| async move { api::secrets_list(&*service()?, &Who::of(&cx.caller), &text(&v, "app")?, &text(&v, "environment")?).await }),
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
            op!("deploy.fleets.create", "A fleet change: one `version`, or values to `set` (by path: `image.pullPolicy`), released to many apps of an environment at once (the `apps` given, or those of a `template`, `project` or `owner`; every app registered there when none is), each through the environment's gates as any release. The fleet's id, and each app's release or why it has none.", ro: false, destructive: false,
                input: || schema(json!({
                    "environment": {"type": "string", "description": ENV},
                    "apps": {"type": "array", "items": {"type": "string"}}, "template": {"type": "string"}, "project": {"type": "string"}, "owner": {"type": "string"},
                    "version": {"type": "string", "description": "A deploy of this version to each."},
                    "set": {"type": "object", "description": "Values set in each app's own, by path: `{\"replicaCount\": \"2\"}`.", "additionalProperties": {"type": "string"}},
                    "idempotency_key": {"type": "string", "description": KEY}
                }), &["environment"]),
                route: |_| Ok((Method::POST, path(&["fleets"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move {
                    let f: flow::FleetAsk = serde_json::from_value(v).map_err(|e| bad(format!("the request doesn't read: {e}")))?;
                    answer(flow::fleet(&*service()?, &Who::of(&cx.caller), f).await?)
                }),
            op!("deploy.fleets.get", "A fleet change as it stands: each of its releases, how many are in each state, and whether all have finished.", ro: true, destructive: false,
                input: || schema(json!({"id": {"type": "string", "description": "The fleet change's id."}}), &["id"]),
                route: |v| Ok((Method::GET, path(&["fleets", &text(v, "id")?]), HashMap::new())),
                run: |_cx: Cx, v: Value| async move { answer(flow::fleet_status(&*service()?, &text(&v, "id")?).await?) }),
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

/// A registration's read from its provider: what `what` makes of the input.
async fn registration_read(v: &Value, what: impl FnOnce(&Value) -> Result<reads::Read, Fail>) -> Out {
    let d = service()?;
    let (app, env) = (text(v, "app")?, text(v, "environment")?);
    let a = store::app(&d.hub.store, &app).await.map_err(internal)?.ok_or_else(|| (StatusCode::NOT_FOUND, format!("no app {app}")))?;
    let e = store::environment(&d.hub.store, &env).await.map_err(internal)?.ok_or_else(|| (StatusCode::NOT_FOUND, format!("no environment {env}")))?;
    store::registration(&d.hub.store, &app, &env).await.map_err(internal)?.ok_or_else(|| (StatusCode::NOT_FOUND, format!("{app} isn't registered in {env}")))?;
    let mut read = what(v)?;
    if let reads::Read::Logs { namespace: ns @ None, .. } = &mut read {
        *ns = Some(a.namespace().to_owned());
    }
    reads::read(&d, &a, &e, read).await
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
                "deploy.templates.put" => json!({ "template": { "name": "service" } }),
                "deploy.fleets.get" => json!({ "id": "f1" }),
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
