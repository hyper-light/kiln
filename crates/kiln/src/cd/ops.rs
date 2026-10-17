//! Every deployments operation, once: name, input schema, whether it changes anything, and what
//! it does. The HTTP routes (`/api/cd/...`), the MCP tools and code mode's `kiln.cd.*` are all
//! this table, so they can't drift apart (docs/access.md, "one route table").
//!
//! An operation takes JSON and gives JSON. Lists page by cursor and filter on the server, and
//! the `*_many` operations act on every app a selector matches, so an agent working on a fleet
//! of 100,000 apps makes one call, not 100,000.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use http::StatusCode;
use kiln_cd::apps::{self, AppDef, Origin};
use kiln_cd::engine::{Input, SyncRequest, Summary};
use kiln_cd::model::{Health, Sync, SyncOptions};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::state::{ClusterAuth, ClusterDef, Command, RepoDef, RepoKind};
use serde_json::{Value, json};

use super::{AppEntry, Cd};
use crate::hub::Hub;

/// A failure: an HTTP status and why.
pub type Fail = (StatusCode, String);
/// An operation's answer.
pub type Out = Result<Value, Fail>;
type Fut = Pin<Box<dyn Future<Output = Out> + Send>>;

/// Who calls, and the services an operation works with.
#[derive(Clone)]
pub struct Cx {
    /// The hub (audit, events).
    pub hub: Arc<Hub>,
    /// Deployments.
    pub cd: Arc<Cd>,
    /// Who acts, for the audit trail (`admin`, `agent:<session>`).
    pub actor: String,
}

/// One operation.
pub struct Op {
    /// Its name, dotted (`cd.apps.list`).
    pub name: &'static str,
    /// What it does, in a sentence.
    pub summary: &'static str,
    /// It changes nothing.
    pub read_only: bool,
    /// It can delete or overwrite something.
    pub destructive: bool,
    /// Its input's JSON Schema.
    pub input: fn() -> Value,
    run: fn(Cx, Value) -> Fut,
}

fn bad(msg: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, msg.into())
}

fn missing(msg: impl Into<String>) -> Fail {
    (StatusCode::NOT_FOUND, msg.into())
}

fn obj_schema(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
}

const APP: &str = "The app's id, `project/name` (a bare name means the default project).";

fn app_only() -> Value {
    obj_schema(json!({"app": {"type": "string", "description": APP}}), &["app"])
}

fn selector_props() -> Value {
    json!({
        "project": {"type": "string"},
        "cluster": {"type": "string"},
        "environment": {"type": "string"},
        "sync": {"type": "string", "enum": ["Synced", "OutOfSync", "Unknown"]},
        "health": {"type": "string", "enum": ["Healthy", "Progressing", "Degraded", "Suspended", "Missing", "Unknown"]},
        "q": {"type": "string", "description": "A substring of the app's id."},
        "labels": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Labels the app must have, all of them."}
    })
}

macro_rules! op {
    ($name:literal, $summary:literal, ro: $ro:expr, destructive: $d:expr, input: $input:expr, run: $f:path) => {
        Op { name: $name, summary: $summary, read_only: $ro, destructive: $d, input: $input, run: |cx, v| Box::pin($f(cx, v)) }
    };
}

/// The operations.
pub fn ops() -> &'static [Op] {
    static OPS: std::sync::OnceLock<Vec<Op>> = std::sync::OnceLock::new();
    OPS.get_or_init(|| {
        vec![
            op!("cd.summary", "Counts of apps by sync and health, and of clusters.", ro: true, destructive: false,
                input: || obj_schema(json!({}), &[]), run: summary),
            op!("cd.apps.list", "Apps matching filters, a page at a time (cursor), each with sync, health, resources and what differs.", ro: true, destructive: false,
                input: || {
                    let mut p = selector_props();
                    if let Some(m) = p.as_object_mut() {
                        m.insert("limit".into(), json!({"type": "integer", "minimum": 1, "maximum": 1000, "default": 200}));
                        m.insert("cursor".into(), json!({"type": "string", "description": "The `next` of the previous page."}));
                    }
                    obj_schema(p, &[])
                }, run: list),
            op!("cd.apps.get", "One app: definition, status, the last sync, source commits, children.", ro: true, destructive: false,
                input: app_only, run: get),
            op!("cd.apps.resources", "An app's resources, each with sync, health, and why it differs when it does.", ro: true, destructive: false,
                input: || obj_schema(json!({"app": {"type": "string", "description": APP}, "out_of_sync": {"type": "boolean", "description": "Only those that differ."}}), &["app"]),
                run: resources),
            op!("cd.apps.diff", "What a sync would change in an app: per resource that differs, the live YAML, what applying git makes of it (`predicted`), and the unified hunks between them. Fields the cluster fills in itself don't show.", ro: true, destructive: false,
                input: app_only, run: diff),
            op!("cd.apps.history", "An app's deployments, newest first.", ro: true, destructive: false,
                input: app_only, run: history),
            op!("cd.apps.sync", "Start a sync of an app (optionally only some resources, with pruning, as a dry run, with Argo CD sync options).", ro: false, destructive: true,
                input: || obj_schema(json!({
                    "app": {"type": "string", "description": APP},
                    "prune": {"type": "boolean"},
                    "dry_run": {"type": "boolean"},
                    "resources": {"type": "array", "items": {"type": "object", "properties": {"group": {"type": "string"}, "kind": {"type": "string"}, "namespace": {"type": "string"}, "name": {"type": "string"}}, "required": ["kind", "name"]}},
                    "options": {"type": "array", "items": {"type": "string"}, "description": "Argo CD sync options, e.g. `ServerSideApply=true`."}
                }), &["app"]), run: sync),
            op!("cd.apps.sync_many", "Start a sync of every app a selector matches (at most `max`).", ro: false, destructive: true,
                input: || {
                    let mut p = selector_props();
                    if let Some(m) = p.as_object_mut() {
                        m.insert("prune".into(), json!({"type": "boolean"}));
                        m.insert("dry_run".into(), json!({"type": "boolean"}));
                        m.insert("max".into(), json!({"type": "integer", "minimum": 1, "maximum": 100000, "default": 1000}));
                    }
                    obj_schema(p, &[])
                }, run: sync_many),
            op!("cd.apps.refresh", "Compare an app again; `hard` renders it again too.", ro: false, destructive: false,
                input: || obj_schema(json!({"app": {"type": "string", "description": APP}, "hard": {"type": "boolean"}}), &["app"]), run: refresh),
            op!("cd.apps.refresh_many", "Compare again every app a selector matches.", ro: false, destructive: false,
                input: || {
                    let mut p = selector_props();
                    if let Some(m) = p.as_object_mut() {
                        m.insert("hard".into(), json!({"type": "boolean"}));
                    }
                    obj_schema(p, &[])
                }, run: refresh_many),
            op!("cd.apps.terminate", "Stop an app's running sync.", ro: false, destructive: false, input: app_only, run: terminate),
            op!("cd.apps.take_over", "Stop shadowing an app adopted from Argo CD: kiln syncs it from now on.", ro: false, destructive: false,
                input: app_only, run: take_over),
            op!("cd.apps.create", "Create or replace an app kept in kiln (an AppDef).", ro: false, destructive: true,
                input: || obj_schema(json!({"definition": {"type": "object", "description": "The app: name, project, sources, destination, sync, ignore, tracking."}}), &["definition"]),
                run: create),
            op!("cd.apps.delete", "Delete an app; its objects too unless `cascade` is `orphan`.", ro: false, destructive: true,
                input: || obj_schema(json!({"app": {"type": "string", "description": APP}, "cascade": {"type": "string", "enum": ["foreground", "background", "orphan"]}}), &["app"]),
                run: delete),
            op!("cd.import", "Import Argo CD manifests (Applications), which start in shadow mode.", ro: false, destructive: false,
                input: || obj_schema(json!({"manifests": {"type": "string", "description": "YAML or JSON, any number of documents."}}), &["manifests"]),
                run: import),
            op!("cd.clusters.list", "Clusters: connection, version, kinds served, labels.", ro: true, destructive: false,
                input: || obj_schema(json!({}), &[]), run: clusters),
            op!("cd.clusters.add", "Add a cluster from a kubeconfig context on the server.", ro: false, destructive: false,
                input: || obj_schema(json!({"name": {"type": "string"}, "context": {"type": "string"}, "kubeconfig": {"type": "string"},
                    "labels": {"type": "object", "additionalProperties": {"type": "string"}}}), &["name", "context"]),
                run: add_cluster),
            op!("cd.clusters.remove", "Remove a cluster no app deploys to.", ro: false, destructive: true,
                input: || obj_schema(json!({"name": {"type": "string"}}), &["name"]), run: remove_cluster),
            op!("cd.repos.list", "Repositories with credentials (secrets never shown).", ro: true, destructive: false,
                input: || obj_schema(json!({}), &[]), run: repos),
            op!("cd.repos.add", "Add credentials for a repository or URL prefix.", ro: false, destructive: false,
                input: || obj_schema(json!({"url": {"type": "string"}, "username": {"type": "string"}, "password": {"type": "string"}, "insecure": {"type": "boolean"}}), &["url"]),
                run: add_repo),
            op!("cd.repos.remove", "Remove a repository's credentials.", ro: false, destructive: true,
                input: || obj_schema(json!({"url": {"type": "string"}}), &["url"]), run: remove_repo),
        ]
    })
}

/// Runs an operation by name.
pub async fn call(cx: Cx, name: &str, input: Value) -> Out {
    let op = ops().iter().find(|o| o.name == name).ok_or_else(|| missing(format!("no operation {name}")))?;
    let input = if input.is_null() { json!({}) } else { input };
    (op.run)(cx, input).await
}

fn str_in<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn app_id(v: &Value) -> Result<String, Fail> {
    let a = str_in(v, "app").ok_or_else(|| bad("`app` is required"))?;
    Ok(if a.contains('/') { a.to_owned() } else { format!("default/{a}") })
}

fn exists(cd: &Cd, id: &str) -> Result<(), Fail> {
    if cd.apps.read().contains_key(id) { Ok(()) } else { Err(missing(format!("no app {id}"))) }
}

async fn propose(cd: &Cd, cmd: Command) -> Result<(), Fail> {
    match cd.store.submit(cmd).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(bad(e)),
        Err(_) => Err((StatusCode::SERVICE_UNAVAILABLE, "the deployments store stopped".into())),
    }
}

async fn audit(cx: &Cx, action: &str, detail: String) {
    cx.hub.store.audit(cx.actor.clone(), action.into(), detail).await;
}

async fn summary(cx: Cx, _: Value) -> Out {
    let statuses = cx.cd.statuses.read();
    let mut sync = std::collections::BTreeMap::<&str, u64>::new();
    let mut health = std::collections::BTreeMap::<&str, u64>::new();
    for s in statuses.values() {
        *sync.entry(s.sync.as_str()).or_default() += 1;
        *health.entry(s.health.as_str()).or_default() += 1;
    }
    Ok(json!({"apps": cx.cd.apps.read().len(), "sync": sync, "health": health, "clusters": cx.cd.clusters.read().len()}))
}

/// Whether an app matches a selector's fields.
fn selects(sel: &Value, e: &AppEntry, s: Option<&Summary>) -> bool {
    let id = e.def.id();
    str_in(sel, "project").is_none_or(|p| p == e.def.project)
        && str_in(sel, "cluster").is_none_or(|c| c == e.def.destination.cluster)
        && str_in(sel, "environment").is_none_or(|v| e.def.environment.as_deref() == Some(v))
        && str_in(sel, "sync").is_none_or(|v| s.is_some_and(|s| s.sync.as_str() == v))
        && str_in(sel, "health").is_none_or(|v| s.is_some_and(|s| s.health.as_str() == v))
        && str_in(sel, "q").is_none_or(|needle| id.contains(needle))
        && sel.get("labels").and_then(Value::as_object).is_none_or(|ls| ls.iter().all(|(k, v)| e.def.labels.get(k).map(String::as_str) == v.as_str()))
}

fn selected(cd: &Cd, sel: &Value, max: usize) -> Vec<String> {
    let apps = cd.apps.read();
    let statuses = cd.statuses.read();
    apps.iter().filter(|(id, e)| selects(sel, e, statuses.get(id.as_str()))).map(|(id, _)| id.clone()).take(max).collect()
}

async fn list(cx: Cx, q: Value) -> Out {
    let limit = q.get("limit").and_then(Value::as_u64).unwrap_or(200).clamp(1, 1000) as usize;
    let apps = cx.cd.apps.read();
    let statuses = cx.cd.statuses.read();
    let start = str_in(&q, "cursor").map(str::to_owned);
    let mut items = Vec::new();
    let mut next = None;
    for (id, e) in apps.range(start.clone().unwrap_or_default()..) {
        if start.as_deref() == Some(id.as_str()) || !selects(&q, e, statuses.get(id.as_str())) {
            continue;
        }
        if items.len() == limit {
            next = Some(id.clone());
            break;
        }
        items.push(app_row(e, statuses.get(id.as_str())));
    }
    Ok(json!({"apps": items, "next": next}))
}

/// An app as lists show it.
pub fn app_row(e: &AppEntry, s: Option<&Summary>) -> Value {
    let unknown = || json!({"sync": Sync::Unknown.as_str(), "health": Health::Unknown.as_str()});
    let mut row = s.map(super::summary_json).unwrap_or_else(unknown);
    if let Some(m) = row.as_object_mut() {
        m.insert("app".into(), json!(e.def.id()));
        m.insert("name".into(), json!(e.def.name));
        m.insert("project".into(), json!(e.def.project));
        m.insert("cluster".into(), json!(e.def.destination.cluster));
        m.insert("namespace".into(), json!(e.def.destination.namespace));
        m.insert("environment".into(), json!(e.def.environment));
        m.insert("auto".into(), json!(e.def.sync.auto));
        m.insert("labels".into(), json!(e.def.labels));
        m.insert("origin".into(), origin(&e.def.origin));
        m.insert("shadow".into(), json!(super::is_shadow(&e.def)));
        if let Some(err) = &e.error {
            m.insert("source_error".into(), json!(err));
        }
    }
    row
}

fn origin(o: &Origin) -> Value {
    match o {
        Origin::Deploy { repo } => json!({"kind": "deploy", "repo": repo}),
        Origin::Argo { parent } => json!({"kind": "argo", "parent": parent}),
        Origin::Api => json!({"kind": "api"}),
    }
}

async fn get(cx: Cx, v: Value) -> Out {
    let id = app_id(&v)?;
    let entry = cx.cd.apps.read().get(&id).cloned().ok_or_else(|| missing(format!("no app {id}")))?;
    let status = cx.cd.statuses.read().get(&id).cloned();
    let mut out = app_row(&entry, status.as_ref());
    if let Some(m) = out.as_object_mut() {
        m.insert("definition".into(), serde_json::to_value(&entry.def).unwrap_or(Value::Null));
        m.insert("commits".into(), json!(entry.commits));
        m.insert("children".into(), json!(entry.children));
    }
    Ok(out)
}

async fn resources(cx: Cx, v: Value) -> Out {
    let id = app_id(&v)?;
    exists(&cx.cd, &id)?;
    let only = v.get("out_of_sync").and_then(Value::as_bool).unwrap_or(false);
    let key = id.clone();
    let list = cx.cd.query(move |e| e.resources(&key)).await.unwrap_or_default();
    let items: Vec<Value> = list
        .iter()
        .filter(|r| !only || (r.sync != Sync::Synced && !r.hook))
        .map(|r| {
            json!({
                "group": r.key.group.as_ref(), "kind": r.key.kind.as_ref(), "namespace": r.key.namespace.as_ref(), "name": r.key.name.as_ref(),
                "sync": if r.hook { "Hook" } else { r.sync.as_str() }, "missing": r.missing, "requires_pruning": r.requires_pruning,
                "health": r.health.map(|h| h.as_str()), "message": r.message, "hook": r.hook, "why": r.why,
            })
        })
        .collect();
    Ok(json!({"resources": items}))
}

async fn diff(cx: Cx, v: Value) -> Out {
    let id = app_id(&v)?;
    exists(&cx.cd, &id)?;
    let key = id.clone();
    let changes = cx
        .cd
        .query(move |e| {
            e.resources(&key)
                .into_iter()
                .filter(|r| r.sync != Sync::Synced && !r.hook)
                .filter_map(|r| e.change(&key, &r.key).map(|c| (r.key, r.why, c)))
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
    let mut out = Vec::new();
    for (k, why, c) in changes {
        let (mut before, mut after) = (c.before.map(display), c.after.map(display));
        mask_secrets(&mut before, &mut after);
        let (live, predicted) = (yaml(before.as_ref()), yaml(after.as_ref()));
        let hunks = crate::harness::diff::hunks(&live, &predicted, 3);
        out.push(json!({"key": key_json(&k), "why": why, "live": live, "predicted": predicted, "hunks": hunks}));
    }
    Ok(json!({"diffs": out}))
}

fn key_json(k: &ResKey) -> Value {
    json!({"group": k.group.as_ref(), "kind": k.kind.as_ref(), "namespace": k.namespace.as_ref(), "name": k.name.as_ref()})
}

fn yaml(v: Option<&Value>) -> String {
    v.map(|v| serde_yaml::to_string(&sorted(v)).unwrap_or_default()).unwrap_or_default()
}

/// The value with every object's keys in order (apiVersion, kind and metadata first), so both
/// sides of a diff line up whatever order they arrived in. A walk with an explicit stack, not
/// recursion, so a deep object can't exhaust the thread's stack.
fn sorted(v: &Value) -> Value {
    let rank = |k: &str| match k {
        "apiVersion" => 0,
        "kind" => 1,
        "metadata" => 2,
        _ => 3,
    };
    let mut out = v.clone();
    sort_in_place(&mut out, &rank);
    out
}

fn sort_in_place(v: &mut Value, rank: &dyn Fn(&str) -> u8) {
    let mut todo: Vec<&mut Value> = vec![v];
    while let Some(cur) = todo.pop() {
        match cur {
            Value::Object(m) => {
                let mut entries: Vec<(String, Value)> = std::mem::take(m).into_iter().collect();
                entries.sort_by(|a, b| rank(&a.0).cmp(&rank(&b.0)).then(a.0.cmp(&b.0)));
                m.extend(entries);
                todo.extend(m.values_mut());
            }
            Value::Array(a) => todo.extend(a.iter_mut()),
            _ => {}
        }
    }
}

/// An object as people read it: no status, no server bookkeeping.
fn display(mut v: Value) -> Value {
    if let Some(m) = v.as_object_mut() {
        m.remove("status");
    }
    if let Some(meta) = v.get_mut("metadata").and_then(Value::as_object_mut) {
        for k in ["managedFields", "resourceVersion", "uid", "generation", "creationTimestamp", "selfLink"] {
            meta.remove(k);
        }
        if let Some(a) = meta.get_mut("annotations").and_then(Value::as_object_mut) {
            a.remove(kiln_cd::diff::LAST_APPLIED);
        }
    }
    v
}

/// Masks a Secret's values on both sides of a diff alike: equal values get equal masks and
/// different ones different masks, so a changed value still shows as changed and no value shows.
fn mask_secrets(before: &mut Option<Value>, after: &mut Option<Value>) {
    let secret = |v: &Value| obj::kind(v) == "Secret" && obj::group(v).is_empty();
    if !before.as_ref().or(after.as_ref()).is_some_and(secret) {
        return;
    }
    let mut seen: Vec<Value> = Vec::new();
    for side in [before.as_mut(), after.as_mut()].into_iter().flatten() {
        for field in ["data", "stringData"] {
            let Some(d) = side.get_mut(field).and_then(Value::as_object_mut) else { continue };
            for val in d.values_mut() {
                let n = seen.iter().position(|s| s == val).unwrap_or_else(|| {
                    seen.push(val.clone());
                    seen.len() - 1
                });
                *val = Value::String("+".repeat(8 + n));
            }
        }
    }
}

async fn history(cx: Cx, v: Value) -> Out {
    let id = app_id(&v)?;
    exists(&cx.cd, &id)?;
    let list = cx.cd.store.read(|d| d.history.get(&id).cloned().unwrap_or_default());
    Ok(json!({"history": list.iter().rev().map(|h| serde_json::to_value(h).unwrap_or(Value::Null)).collect::<Vec<_>>()}))
}

fn sync_request(v: &Value, by: &str) -> Result<SyncRequest, Fail> {
    let opts: Vec<&str> = v.get("options").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    let (options, unknown) = SyncOptions::parse(opts);
    if let Some(u) = unknown.first() {
        return Err(bad(format!("unknown sync option {}", u.0)));
    }
    let resources = v.get("resources").and_then(Value::as_array).map(|rs| {
        rs.iter()
            .map(|r| {
                let f = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("");
                ResKey::new(f("group"), f("kind"), f("namespace"), f("name"))
            })
            .collect()
    });
    Ok(SyncRequest {
        prune: v.get("prune").and_then(Value::as_bool),
        dry_run: v.get("dry_run").and_then(Value::as_bool).unwrap_or(false),
        resources,
        options,
        by: Some(by.to_owned()),
        trigger: None,
    })
}

async fn sync(cx: Cx, v: Value) -> Out {
    let id = app_id(&v)?;
    exists(&cx.cd, &id)?;
    let request = sync_request(&v, &cx.actor)?;
    cx.cd.input(Input::Sync { app: id.as_str().into(), request });
    audit(&cx, "cd.sync", id.clone()).await;
    Ok(json!({"syncing": id}))
}

async fn sync_many(cx: Cx, v: Value) -> Out {
    let max = v.get("max").and_then(Value::as_u64).unwrap_or(1000).clamp(1, 100_000) as usize;
    let ids = selected(&cx.cd, &v, max);
    let request = sync_request(&v, &cx.actor)?;
    for id in &ids {
        cx.cd.input(Input::Sync { app: id.as_str().into(), request: request.clone() });
    }
    audit(&cx, "cd.sync_many", format!("{} apps", ids.len())).await;
    Ok(json!({"started": ids.len(), "apps": ids.iter().take(100).collect::<Vec<_>>()}))
}

async fn refresh(cx: Cx, v: Value) -> Out {
    let id = app_id(&v)?;
    exists(&cx.cd, &id)?;
    cx.cd.input(Input::Refresh { app: id.as_str().into(), hard: v.get("hard").and_then(Value::as_bool).unwrap_or(false) });
    cx.cd.kick();
    Ok(json!({"refreshing": id}))
}

async fn refresh_many(cx: Cx, v: Value) -> Out {
    let ids = selected(&cx.cd, &v, 1_000_000);
    let hard = v.get("hard").and_then(Value::as_bool).unwrap_or(false);
    for id in &ids {
        cx.cd.input(Input::Refresh { app: id.as_str().into(), hard });
    }
    cx.cd.kick();
    Ok(json!({"refreshing": ids.len()}))
}

async fn terminate(cx: Cx, v: Value) -> Out {
    let id = app_id(&v)?;
    exists(&cx.cd, &id)?;
    cx.cd.input(Input::Terminate { app: id.as_str().into() });
    audit(&cx, "cd.terminate", id.clone()).await;
    Ok(json!({"terminating": id}))
}

async fn take_over(cx: Cx, v: Value) -> Out {
    let id = app_id(&v)?;
    let mut def = cx.cd.apps.read().get(&id).map(|e| e.def.clone()).ok_or_else(|| missing(format!("no app {id}")))?;
    def.labels.insert("kiln.dev/take-over".into(), "true".into());
    // Kept in kiln from now on, so the take-over survives restarts and the parent's re-renders.
    def.origin = Origin::Api;
    propose(&cx.cd, Command::PutApp(Box::new(def))).await?;
    audit(&cx, "cd.take_over", id.clone()).await;
    Ok(json!({"taken_over": id}))
}

async fn create(cx: Cx, v: Value) -> Out {
    let raw = v.get("definition").cloned().ok_or_else(|| bad("`definition` is required"))?;
    let mut def: AppDef = serde_json::from_value(raw).map_err(|e| bad(format!("the definition doesn't read: {e}")))?;
    def.origin = Origin::Api;
    if def.sources.is_empty() || def.name.is_empty() {
        return Err(bad("an app needs a name and a source"));
    }
    let id = def.id();
    propose(&cx.cd, Command::PutApp(Box::new(def))).await?;
    audit(&cx, "cd.app.put", id.clone()).await;
    Ok(json!({"app": id}))
}

async fn import(cx: Cx, v: Value) -> Out {
    let text = str_in(&v, "manifests").ok_or_else(|| bad("`manifests` is required"))?;
    let docs = obj::parse_documents(text).map_err(|e| bad(format!("{e:#}")))?;
    let clusters = cx.cd.store.read(|d| d.clusters.clone());
    let by_url = |url: &str| clusters.values().find(|c| c.server.trim_end_matches('/') == url.trim_end_matches('/')).map(|c| c.name.clone());
    let mut imported = Vec::new();
    let mut skipped = Vec::new();
    for d in docs {
        match (obj::group(&d), obj::kind(&d)) {
            ("argoproj.io", "Application") => match apps::from_argo(&d, None, &by_url) {
                Ok(def) => {
                    let id = def.id();
                    propose(&cx.cd, Command::PutApp(Box::new(def))).await?;
                    imported.push(id);
                }
                Err(e) => skipped.push(json!({"name": obj::name(&d), "why": e.0})),
            },
            (g, k) => skipped.push(json!({"name": obj::name(&d), "why": format!("{g}/{k} is not imported yet")})),
        }
    }
    audit(&cx, "cd.import", imported.join(",")).await;
    Ok(json!({"imported": imported, "skipped": skipped}))
}

async fn delete(cx: Cx, v: Value) -> Out {
    let id = app_id(&v)?;
    let entry = cx.cd.apps.read().get(&id).cloned().ok_or_else(|| missing(format!("no app {id}")))?;
    let cascade = str_in(&v, "cascade").map(|c| c != "orphan").unwrap_or(entry.def.cascade);
    if matches!(entry.def.origin, Origin::Api | Origin::Argo { parent: None }) {
        propose(&cx.cd, Command::DeleteApp(id.clone())).await?;
    }
    cx.cd.unregister(&id, cascade);
    audit(&cx, "cd.app.delete", format!("{id} cascade={cascade}")).await;
    Ok(json!({"deleted": id, "cascade": cascade}))
}

async fn clusters(cx: Cx, _: Value) -> Out {
    let connected = cx.cd.clusters.read();
    let defs = cx.cd.cluster_defs();
    Ok(json!({"clusters": defs.iter().map(|c| {
        let live = connected.get(&c.name);
        json!({
            "name": c.name, "server": c.server, "labels": c.labels, "mode": format!("{:?}", c.mode),
            "auth": match &c.auth {
                ClusterAuth::InCluster => "in-cluster".to_owned(),
                ClusterAuth::Token { .. } => "token".into(),
                ClusterAuth::Certificate { .. } => "certificate".into(),
                ClusterAuth::Exec { command, .. } => format!("exec ({command})"),
                ClusterAuth::Kubeconfig { context, .. } => format!("kubeconfig ({context})"),
            },
            "connected": live.is_some(),
            "version": live.and_then(|l| l.version.read().clone()),
            "kinds": live.map(|l| l.discovery.read().kinds.len()),
        })
    }).collect::<Vec<_>>()}))
}

async fn add_cluster(cx: Cx, v: Value) -> Out {
    let name = str_in(&v, "name").ok_or_else(|| bad("`name` is required"))?.to_owned();
    let context = str_in(&v, "context").ok_or_else(|| bad("`context` is required"))?.to_owned();
    let kubeconfig = str_in(&v, "kubeconfig").unwrap_or("").to_owned();
    let labels = v
        .get("labels")
        .and_then(Value::as_object)
        .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned()))).collect())
        .unwrap_or_default();
    let ep = super::kubeconfig_endpoint(&kubeconfig, &context).map_err(|e| bad(format!("{e:#}")))?;
    let def = ClusterDef { name: name.clone(), server: ep.server, auth: ClusterAuth::Kubeconfig { path: kubeconfig, context }, labels, ..ClusterDef::default() };
    propose(&cx.cd, Command::PutCluster(Box::new(def))).await?;
    audit(&cx, "cd.cluster.put", name.clone()).await;
    Ok(json!({"cluster": name}))
}

async fn remove_cluster(cx: Cx, v: Value) -> Out {
    let name = str_in(&v, "name").ok_or_else(|| bad("`name` is required"))?.to_owned();
    propose(&cx.cd, Command::DeleteCluster(name.clone())).await?;
    audit(&cx, "cd.cluster.delete", name.clone()).await;
    Ok(json!({"removed": name}))
}

async fn repos(cx: Cx, _: Value) -> Out {
    Ok(json!({"repos": cx.cd.store.read(|d| d.repos.values().map(|r| json!({
        "url": r.url, "kind": format!("{:?}", r.kind), "username": r.username, "insecure": r.insecure, "project": r.project,
        "has_password": r.password.is_some(), "has_ssh_key": r.ssh_key.is_some(),
    })).collect::<Vec<_>>())}))
}

async fn add_repo(cx: Cx, v: Value) -> Out {
    let url = str_in(&v, "url").ok_or_else(|| bad("`url` is required"))?.to_owned();
    let r = RepoDef {
        url: url.clone(),
        kind: RepoKind::Git,
        username: str_in(&v, "username").map(str::to_owned),
        password: str_in(&v, "password").map(str::to_owned),
        ssh_key: None,
        insecure: v.get("insecure").and_then(Value::as_bool).unwrap_or(false),
        project: None,
    };
    propose(&cx.cd, Command::PutRepo(Box::new(r))).await?;
    audit(&cx, "cd.repo.put", url.clone()).await;
    cx.cd.kick();
    Ok(json!({"repo": url}))
}

async fn remove_repo(cx: Cx, v: Value) -> Out {
    let url = str_in(&v, "url").ok_or_else(|| bad("`url` is required"))?.to_owned();
    propose(&cx.cd, Command::DeleteRepo(url.clone())).await?;
    audit(&cx, "cd.repo.delete", url.clone()).await;
    Ok(json!({"removed": url}))
}
