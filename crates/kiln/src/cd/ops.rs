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
use kiln_cd::engine::{Input, OpPhase, Summary, SyncRequest, Trigger};
use kiln_cd::model::{Health, Sync, SyncOptions};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::state::{self, ClusterAuth, ClusterDef, Command, RepoDef, RepoKind};
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
        "project": {"type": "string", "description": "A project; several separated by commas."},
        "cluster": {"type": "string", "description": "A cluster; several separated by commas."},
        "namespace": {"type": "string", "description": "A destination namespace; several separated by commas."},
        "environment": {"type": "string"},
        "sync": {"type": "string", "description": "Synced, OutOfSync or Unknown; several separated by commas."},
        "health": {"type": "string", "description": "Healthy, Progressing, Degraded, Suspended, Missing or Unknown; several separated by commas."},
        "q": {"type": "string", "description": "A part of the app's id (any case)."},
        "labels": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Labels the app must have, all of them."}
    })
}

fn res_props() -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert("app".into(), json!({"type": "string", "description": APP}));
    m.insert("group".into(), json!({"type": "string", "description": "API group; empty for the core group."}));
    m.insert("kind".into(), json!({"type": "string"}));
    m.insert("namespace".into(), json!({"type": "string", "description": "Empty for cluster-scoped objects."}));
    m.insert("name".into(), json!({"type": "string"}));
    m
}

fn res_only() -> Value {
    obj_schema(Value::Object(res_props()), &["app", "kind", "name"])
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
                        m.insert("sort".into(), json!({"type": "string", "enum": ["name", "health", "sync", "deployed"], "default": "name",
                            "description": "By id; worst health first; out of sync first; most recently deployed first."}));
                    }
                    obj_schema(p, &[])
                }, run: list),
            op!("cd.apps.facets", "How many apps each filter value would show (sync, health, cluster, project, namespace), given the other filters chosen, and how many match all of them.", ro: true, destructive: false,
                input: || obj_schema(selector_props(), &[]), run: facets),
            op!("cd.apps.get", "One app: definition, status, the last sync, source commits (the revision's author, time and message), children.", ro: true, destructive: false,
                input: app_only, run: get),
            op!("cd.apps.tree", "An app's resource tree: its objects and every live object they own (ReplicaSets, Pods, Jobs, EndpointSlices…), each with uid, parents, health, sync (managed ones), age, ready counts, restarts, images and info. Read from the cluster, kept a few seconds.", ro: true, destructive: false,
                input: || obj_schema(json!({"app": {"type": "string", "description": APP}, "fresh": {"type": "boolean", "description": "Read the cluster now rather than within the last few seconds."}}), &["app"]),
                run: tree),
            op!("cd.apps.manifests", "The manifests an app renders now, as kiln applies them (Secrets masked).", ro: true, destructive: false,
                input: app_only, run: manifests),
            op!("cd.apps.resource", "One object of an app's tree in depth: its node, live manifest, desired manifest and diff (managed objects), labels, annotations, conditions and owners. Secrets are masked.", ro: true, destructive: false,
                input: res_only, run: resource),
            op!("cd.apps.resource_events", "An object's Kubernetes events, newest first.", ro: true, destructive: false,
                input: res_only, run: resource_events),
            op!("cd.apps.resource_logs", "The last lines of a pod's log, or of the pods of a workload (merged by time, each line with its pod). Follow a log over `GET /api/cd/apps/:app/resources/:key/logs/ws`.", ro: true, destructive: false,
                input: || {
                    let mut m = res_props();
                    m.insert("container".into(), json!({"type": "string", "description": "The container; the pod's first when not given."}));
                    m.insert("pod".into(), json!({"type": "string", "description": "Only this pod of the workload."}));
                    m.insert("tail_lines".into(), json!({"type": "integer", "minimum": 1, "maximum": 10000, "default": 500}));
                    m.insert("since_seconds".into(), json!({"type": "integer", "minimum": 1}));
                    m.insert("previous".into(), json!({"type": "boolean", "description": "The container's previous run (after a restart or crash)."}));
                    obj_schema(Value::Object(m), &["app", "kind", "name"])
                }, run: resource_logs),
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
            op!("cd.apps.rollback", "Roll an app back to an earlier deployment (`to`, its history id; by default the one before what runs now). kiln renders that deployment's commits again and syncs them; the app holds there until git changes it, then follows git again.", ro: false, destructive: true,
                input: || obj_schema(json!({
                    "app": {"type": "string", "description": APP},
                    "to": {"type": "integer", "minimum": 1, "description": "The deployment's id (cd.apps.history)."},
                    "prune": {"type": "boolean", "description": "Also delete what that deployment didn't have."}
                }), &["app"]), run: rollback),
            op!("cd.apps.follow", "Let a rolled-back app follow git again (an automatic app then deploys what git says).", ro: false, destructive: false,
                input: app_only, run: follow),
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

/// The facets apps are filtered and counted by.
const FACETS: [&str; 5] = ["sync", "health", "cluster", "project", "namespace"];

/// An app's value for each facet; an app not compared yet is Unknown.
fn facet_values<'a>(e: &'a AppEntry, s: Option<&Summary>) -> [&'a str; 5] {
    [
        s.map_or("Unknown", |s| s.sync.as_str()),
        s.map_or("Unknown", |s| s.health.as_str()),
        e.def.destination.cluster.as_str(),
        e.def.project.as_str(),
        e.def.destination.namespace.as_str(),
    ]
}

/// Whether a selector lets `value` through for `field`: no filter, or one of its values (a list,
/// or a string of comma-separated values).
fn allows(sel: &Value, field: &str, value: &str) -> bool {
    match sel.get(field) {
        Some(Value::String(s)) => s.is_empty() || s.split(',').any(|x| x.trim() == value),
        Some(Value::Array(a)) => a.is_empty() || a.iter().any(|x| x.as_str() == Some(value)),
        _ => true,
    }
}

/// Whether an app matches what a selector says besides the facets: its id, environment, labels.
fn selects_rest(sel: &Value, e: &AppEntry) -> bool {
    str_in(sel, "q").is_none_or(|needle| e.def.id().to_lowercase().contains(&needle.to_lowercase()))
        && str_in(sel, "environment").is_none_or(|v| e.def.environment.as_deref() == Some(v))
        && sel.get("labels").and_then(Value::as_object).is_none_or(|ls| ls.iter().all(|(k, v)| e.def.labels.get(k).map(String::as_str) == v.as_str()))
}

/// Whether an app matches a selector's fields.
fn selects(sel: &Value, e: &AppEntry, s: Option<&Summary>) -> bool {
    selects_rest(sel, e) && FACETS.iter().zip(facet_values(e, s)).all(|(f, v)| allows(sel, f, v))
}

/// How many apps each facet value would show given the other facets chosen (so a count says
/// what choosing it does), and how many match everything chosen.
async fn facets(cx: Cx, sel: Value) -> Out {
    let apps = cx.cd.apps.read();
    let statuses = cx.cd.statuses.read();
    let mut counts: [std::collections::BTreeMap<String, u64>; 5] = Default::default();
    let mut total = 0u64;
    for (id, e) in apps.iter() {
        if !selects_rest(&sel, e) {
            continue;
        }
        let values = facet_values(e, statuses.get(id.as_str()));
        let pass: Vec<bool> = FACETS.iter().zip(values).map(|(f, v)| allows(&sel, f, v)).collect();
        if pass.iter().all(|p| *p) {
            total += 1;
        }
        for (i, (count, v)) in counts.iter_mut().zip(values).enumerate() {
            if pass.iter().enumerate().all(|(j, p)| j == i || *p) {
                *count.entry(v.to_owned()).or_default() += 1;
            }
        }
    }
    let [sync, health, cluster, project, namespace] = counts;
    Ok(json!({"total": total, "facets": {"sync": sync, "health": health, "cluster": cluster, "project": project, "namespace": namespace}}))
}

fn selected(cd: &Cd, sel: &Value, max: usize) -> Vec<String> {
    let apps = cd.apps.read();
    let statuses = cd.statuses.read();
    apps.iter().filter(|(id, e)| selects(sel, e, statuses.get(id.as_str()))).map(|(id, _)| id.clone()).take(max).collect()
}

/// Where an app sorts, for sorts other than by id: a rank that compares as text.
fn rank(sort: &str, s: Option<&Summary>, deployed: Option<u64>) -> String {
    match sort {
        "health" => {
            let order = [Health::Degraded, Health::Missing, Health::Progressing, Health::Suspended, Health::Unknown, Health::Healthy];
            let h = s.map_or(Health::Unknown, |s| s.health);
            order.iter().position(|x| *x == h).unwrap_or(order.len()).to_string()
        }
        "sync" => match s.map_or(Sync::Unknown, |s| s.sync) {
            Sync::OutOfSync => "0".into(),
            Sync::Unknown => "1".into(),
            Sync::Synced => "2".into(),
        },
        // Newest first; never deployed last.
        _ => deployed.map_or_else(|| "~".to_owned(), |at| format!("{:020}", u64::MAX - at)),
    }
}

async fn list(cx: Cx, q: Value) -> Out {
    let sort = str_in(&q, "sort").unwrap_or("name").to_owned();
    if sort != "name" {
        if !["health", "sync", "deployed"].contains(&sort.as_str()) {
            return Err(bad(format!("unknown sort {sort}: name, health, sync or deployed")));
        }
        return list_sorted(cx, q, sort).await;
    }
    let pins = cx.cd.store.read(|d| d.pins.clone());
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
        items.push(app_row(e, statuses.get(id.as_str()), pins.get(id.as_str())));
    }
    Ok(json!({"apps": items, "next": next}))
}

/// A page of apps in another order than by id. The cursor is the last row's rank and id, so
/// pages follow on whatever changed meanwhile.
async fn list_sorted(cx: Cx, q: Value, sort: String) -> Out {
    let pins = cx.cd.store.read(|d| d.pins.clone());
    let deployed: std::collections::HashMap<String, u64> =
        if sort == "deployed" { cx.cd.store.read(|d| d.history.iter().filter_map(|(id, h)| Some((id.clone(), h.back()?.at_ms))).collect()) } else { Default::default() };
    let limit = q.get("limit").and_then(Value::as_u64).unwrap_or(200).clamp(1, 1000) as usize;
    let apps = cx.cd.apps.read();
    let statuses = cx.cd.statuses.read();
    let mut rows: Vec<(String, &String, &AppEntry)> = apps
        .iter()
        .filter(|(id, e)| selects(&q, e, statuses.get(id.as_str())))
        .map(|(id, e)| (rank(&sort, statuses.get(id.as_str()), deployed.get(id).copied()), id, e))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    let after = str_in(&q, "cursor").and_then(|c| c.split_once('\t')).map(|(r, id)| (r.to_owned(), id.to_owned()));
    let start = after.map_or(0, |(r, id)| rows.partition_point(|x| (x.0.as_str(), x.1.as_str()) <= (r.as_str(), id.as_str())));
    let page: Vec<Value> = rows.iter().skip(start).take(limit).map(|(_, id, e)| app_row(e, statuses.get(id.as_str()), pins.get(id.as_str()))).collect();
    let next = (rows.len() > start + limit).then(|| rows.get(start + limit - 1).map(|(r, id, _)| format!("{r}\t{id}"))).flatten();
    Ok(json!({"apps": page, "next": next}))
}

/// An app as lists show it.
pub fn app_row(e: &AppEntry, s: Option<&Summary>, pin: Option<&state::Pin>) -> Value {
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
        m.insert("pinned".into(), pin.map_or(Value::Null, |p| json!({"to": p.to, "revision": p.revision, "by": p.by, "at_ms": p.at_ms})));
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
    let pin = cx.cd.store.read(|d| d.pins.get(&id).cloned());
    let revision = match source_commit(&entry, status.as_ref().and_then(|s| s.revision.as_deref())) {
        Some((url, sha)) => cx.cd.live.commit(&cx.cd, &url, &sha).await,
        None => None,
    };
    let mut out = app_row(&entry, status.as_ref(), pin.as_ref());
    if let Some(m) = out.as_object_mut() {
        m.insert("definition".into(), serde_json::to_value(&entry.def).unwrap_or(Value::Null));
        m.insert("commits".into(), json!(entry.commits));
        m.insert("children".into(), json!(entry.children));
        m.insert("revision_info".into(), revision.unwrap_or(Value::Null));
    }
    Ok(out)
}

/// The repository and commit an app's revision names: its first git source's.
fn source_commit(e: &AppEntry, revision: Option<&str>) -> Option<(String, String)> {
    let (i, src) = e.def.sources.iter().enumerate().find(|(_, s)| s.chart.is_none() && s.as_ref.is_none())?;
    let sha = revision.filter(|r| r.len() >= 7 && r.bytes().all(|b| b.is_ascii_hexdigit())).map(str::to_owned).or_else(|| e.commits.get(i).cloned().flatten())?;
    Some((src.repo.clone(), sha))
}

/// The object an op's input names.
fn res_key(v: &Value) -> Result<ResKey, Fail> {
    let f = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("");
    if f("kind").is_empty() || f("name").is_empty() {
        return Err(bad("`kind` and `name` are required"));
    }
    Ok(ResKey::new(f("group"), f("kind"), f("namespace"), f("name")))
}

fn cluster_of(cd: &Cd, id: &str) -> Result<Arc<super::kube::Cluster>, Fail> {
    let name = cd.apps.read().get(id).map(|e| e.def.destination.cluster.clone()).ok_or_else(|| missing(format!("no app {id}")))?;
    cd.clusters.read().get(&name).cloned().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, format!("cluster {name} is not connected")))
}

async fn tree(cx: Cx, v: Value) -> Out {
    let id = app_id(&v)?;
    let def = cx.cd.apps.read().get(&id).map(|e| e.def.clone()).ok_or_else(|| missing(format!("no app {id}")))?;
    let t = cx.cd.live.tree(&cx.cd, &id, v.get("fresh").and_then(Value::as_bool).unwrap_or(false)).await?;
    Ok(t.to_json(&def))
}

async fn manifests(cx: Cx, v: Value) -> Out {
    let id = app_id(&v)?;
    exists(&cx.cd, &id)?;
    let key = id.clone();
    let objects = cx
        .cd
        .query(move |e| e.resources(&key).into_iter().filter_map(|r| Some((r.key.clone(), e.object(&key, &r.key)?.0?))).collect::<Vec<_>>())
        .await
        .unwrap_or_default();
    let out: Vec<Value> = objects
        .into_iter()
        .map(|(k, mut o)| {
            mask_secret(&mut o);
            json!({"key": key_json(&k), "yaml": yaml(Some(&o))})
        })
        .collect();
    Ok(json!({"manifests": out}))
}

/// What a sync changes in one object, as `cd.apps.diff` shows it.
fn change_json(k: &ResKey, why: Option<String>, c: kiln_cd::engine::Change) -> Value {
    let (mut before, mut after) = (c.before.map(display), c.after.map(display));
    mask_secrets(&mut before, &mut after);
    let (live, predicted) = (yaml(before.as_ref()), yaml(after.as_ref()));
    let hunks = crate::harness::diff::hunks(&live, &predicted, 3);
    json!({"key": key_json(k), "why": why, "live": live, "predicted": predicted, "hunks": hunks})
}

/// A map of strings, at most `max` bytes per value (a last-applied configuration can be long).
fn strings(v: Option<&Value>, max: usize) -> Value {
    let Some(m) = v.and_then(Value::as_object) else { return json!({}) };
    Value::Object(
        m.iter()
            .map(|(k, x)| {
                let s = x.as_str().map(str::to_owned).unwrap_or_else(|| x.to_string());
                let s = if s.len() > max {
                    let mut cut = max;
                    while !s.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    format!("{}… ({} bytes)", &s[..cut], s.len())
                } else {
                    s
                };
                (k.clone(), Value::String(s))
            })
            .collect(),
    )
}

async fn resource(cx: Cx, v: Value) -> Out {
    let id = app_id(&v)?;
    exists(&cx.cd, &id)?;
    let k = res_key(&v)?;
    let (app, key) = (id.clone(), k.clone());
    let (view, desired, change) = cx
        .cd
        .query(move |e| {
            let view = e.resources(&app).into_iter().find(|r| r.key == key);
            let desired = view.as_ref().and_then(|_| e.object(&app, &key)).and_then(|(d, _)| d);
            let change = view.as_ref().and_then(|_| e.change(&app, &key));
            (view, desired, change)
        })
        .await
        .unwrap_or((None, None, None));
    let tree = cx.cd.live.tree(&cx.cd, &id, false).await;
    let (node, owners, tree_error) = match &tree {
        Ok(t) => {
            let n = t.node(&k);
            let owners: Vec<Value> = n.map(|n| t.owners(n)).unwrap_or_default().iter().map(|o| json!({"id": o.id, "uid": o.uid, "group": o.group, "kind": o.kind, "namespace": o.namespace, "name": o.name})).collect();
            (n.cloned(), owners, None)
        }
        Err((_, e)) => (None, Vec::new(), Some(e.clone())),
    };
    if view.is_none() && node.is_none() {
        return Err(match tree_error {
            Some(e) => (StatusCode::SERVICE_UNAVAILABLE, e),
            None => missing(format!("{k} is not part of {id}")),
        });
    }
    // The object as the cluster serves it now.
    let mut errors: Vec<String> = tree_error.into_iter().collect();
    let live = if node.as_ref().is_some_and(|n| n.missing) || view.as_ref().is_some_and(|r| r.missing) {
        None
    } else {
        match cluster_of(&cx.cd, &id) {
            Ok(c) => match c.get(&k).await {
                Ok(o) => o,
                Err(e) => {
                    errors.push(format!("reading {k}: {e:#}"));
                    None
                }
            },
            Err((_, e)) => {
                errors.push(e);
                None
            }
        }
    };
    let live = live.map(|mut o| {
        if let Some(meta) = o.get_mut("metadata").and_then(Value::as_object_mut) {
            meta.remove("managedFields");
        }
        mask_secret(&mut o);
        o
    });
    let desired = desired.map(|mut o| {
        mask_secret(&mut o);
        o
    });
    let shown = live.as_ref().or(desired.as_ref());
    let conditions: Vec<Value> = shown
        .and_then(|o| obj::list_at(o, &["status", "conditions"]))
        .into_iter()
        .flatten()
        .map(|c| {
            let s = |f: &str| obj::str_at(c, &[f]).unwrap_or("");
            json!({"type": s("type"), "status": s("status"), "reason": s("reason"), "message": s("message"), "last": obj::str_at(c, &["lastTransitionTime"]).or_else(|| obj::str_at(c, &["lastUpdateTime"]))})
        })
        .collect();
    let finalizers: Vec<&str> = shown.and_then(|o| obj::list_at(o, &["metadata", "finalizers"])).into_iter().flatten().filter_map(Value::as_str).collect();
    let diff = match (&view, change) {
        (Some(r), Some(c)) if !r.hook => Some(change_json(&k, r.why.clone(), c)),
        _ => None,
    };
    Ok(json!({
        "key": key_json(&k),
        "managed": view.is_some(),
        "node": node,
        "live": live.as_ref().map(|o| yaml(Some(o))),
        "desired": desired.as_ref().map(|o| yaml(Some(o))),
        "diff": diff,
        "labels": strings(shown.and_then(|o| obj::get(o, &["metadata", "labels"])), 4096),
        "annotations": strings(shown.and_then(|o| obj::get(o, &["metadata", "annotations"])), 2048),
        "conditions": conditions,
        "finalizers": finalizers,
        "owners": owners,
        "errors": errors,
    }))
}

async fn resource_events(cx: Cx, v: Value) -> Out {
    let id = app_id(&v)?;
    exists(&cx.cd, &id)?;
    let k = res_key(&v)?;
    let t = cx.cd.live.tree(&cx.cd, &id, false).await?;
    let node = t.node(&k).ok_or_else(|| missing(format!("{k} is not part of {id}")))?;
    if node.missing {
        return Ok(json!({"events": []}));
    }
    let c = cluster_of(&cx.cd, &id)?;
    let list = super::live::events(&c, &k, node.uid.as_deref()).await.map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    Ok(json!({"events": list}))
}

/// The pods a log request reads: the object's own (a pod, or a workload's), at most a few.
pub(super) async fn log_pods(cd: &Arc<Cd>, id: &str, k: &ResKey, pod: Option<&str>) -> Result<(Arc<super::live::AppTree>, Vec<String>, usize), Fail> {
    let t = cd.live.tree(cd, id, false).await?;
    let node = t.node(k).ok_or_else(|| missing(format!("{k} is not part of {id}")))?;
    let mut pods: Vec<String> = t.pods(node).iter().map(|p| p.id.clone()).collect();
    if let Some(p) = pod {
        pods.retain(|x| t.tree.nodes.iter().any(|n| n.id == *x && n.name == p));
    }
    if pods.is_empty() {
        return Err(bad(format!("{} {} runs no pods{}", k.kind, k.name, pod.map(|p| format!(" named {p}")).unwrap_or_default())));
    }
    let all = pods.len();
    pods.truncate(super::live::MAX_LOG_PODS);
    Ok((t, pods, all))
}

async fn resource_logs(cx: Cx, v: Value) -> Out {
    let id = app_id(&v)?;
    exists(&cx.cd, &id)?;
    let k = res_key(&v)?;
    let (t, ids, all) = log_pods(&cx.cd, &id, &k, str_in(&v, "pod")).await?;
    let pods: Vec<&kiln_cd::tree::Node> = ids.iter().filter_map(|i| t.tree.nodes.iter().find(|n| n.id == *i)).collect();
    let q = super::live::LogQuery::from_json(&v);
    let c = cluster_of(&cx.cd, &id)?;
    let (lines, errors) = super::live::logs(&c, &pods, &q).await;
    let mut containers: Vec<&str> = Vec::new();
    for p in &pods {
        for cn in p.containers.iter().filter(|c| !c.init).chain(p.containers.iter().filter(|c| c.init)) {
            if !containers.contains(&cn.name.as_str()) {
                containers.push(&cn.name);
            }
        }
    }
    let container = pods.first().and_then(|p| super::live::container_of(p, q.container.as_deref()));
    Ok(json!({
        "pods": pods.iter().map(|p| &p.name).collect::<Vec<_>>(), "more_pods": all - pods.len(),
        "container": container, "containers": containers, "lines": lines, "errors": errors,
    }))
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
    let out: Vec<Value> = changes.into_iter().map(|(k, why, c)| change_json(&k, why, c)).collect();
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

/// Masks a Secret's values in one object, as [`mask_secrets`] masks a diff's two sides, its
/// last-applied configuration (which holds them in the clear) included.
fn mask_secret(v: &mut Value) {
    if !(obj::kind(v) == "Secret" && obj::group(v).is_empty()) {
        return;
    }
    let mut one = Some(std::mem::take(v));
    mask_secrets(&mut one, &mut None);
    *v = one.unwrap_or_default();
    if let Some(Value::String(text)) = v.pointer_mut("/metadata/annotations").and_then(Value::as_object_mut).and_then(|a| a.get_mut(kiln_cd::diff::LAST_APPLIED)) {
        *text = match serde_json::from_str::<Value>(text) {
            Ok(inner) => {
                let mut inner = Some(inner);
                mask_secrets(&mut inner, &mut None);
                inner.map(|i| i.to_string()).unwrap_or_default()
            }
            Err(_) => "+".repeat(8),
        };
    }
}

async fn history(cx: Cx, v: Value) -> Out {
    let id = app_id(&v)?;
    let entry = cx.cd.apps.read().get(&id).cloned().ok_or_else(|| missing(format!("no app {id}")))?;
    let list = cx.cd.store.read(|d| d.history.get(&id).cloned().unwrap_or_default());
    let mut out = Vec::new();
    for h in list.iter().rev() {
        let mut row = serde_json::to_value(h).unwrap_or(Value::Null);
        // What the deployed commit says, as the header says it of the revision.
        if let Some((url, sha)) = source_commit(&entry, h.revision.as_deref())
            && let Some(c) = cx.cd.live.commit(&cx.cd, &url, &sha).await
            && let Some(m) = row.as_object_mut()
        {
            m.insert("commit".into(), c);
        }
        out.push(row);
    }
    Ok(json!({"history": out}))
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

/// Whether an app can start a sync now: none is running, and its render didn't fail.
fn ready_to_sync(cd: &Cd, id: &str) -> Result<(), Fail> {
    let statuses = cd.statuses.read();
    let Some(s) = statuses.get(id) else { return Ok(()) };
    if s.operation.as_ref().is_some_and(|o| matches!(o.phase, OpPhase::DryRun | OpPhase::Running | OpPhase::Failing | OpPhase::Terminating)) {
        return Err((StatusCode::CONFLICT, format!("{id} is already syncing; stop that sync first")));
    }
    if let Some(m) = s.message.as_deref().filter(|m| m.starts_with("render failed")) {
        return Err(bad(format!("{id} can't sync: its {m}")));
    }
    Ok(())
}

async fn sync(cx: Cx, v: Value) -> Out {
    let id = app_id(&v)?;
    exists(&cx.cd, &id)?;
    ready_to_sync(&cx.cd, &id)?;
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

async fn rollback(cx: Cx, v: Value) -> Out {
    let id = app_id(&v)?;
    let def = cx.cd.apps.read().get(&id).map(|e| e.def.clone()).ok_or_else(|| missing(format!("no app {id}")))?;
    if super::is_shadow(&def) {
        return Err(bad(format!("kiln doesn't deploy {id} (it shadows Argo CD's): take it over first")));
    }
    ready_to_sync(&cx.cd, &id)?;
    let history = cx.cd.store.read(|d| d.history.get(&id).cloned().unwrap_or_default());
    let current = history.iter().rev().find(|d| d.succeeded).and_then(|d| d.key);
    let target = match v.get("to").and_then(Value::as_u64) {
        Some(n) => history.iter().find(|d| d.id == n).ok_or_else(|| missing(format!("{id} has no deployment {n}")))?,
        None => history
            .iter()
            .rev()
            .find(|d| d.succeeded && d.key != current)
            .ok_or_else(|| bad(format!("{id} has no earlier deployment to roll back to")))?,
    };
    if !target.succeeded {
        return Err(bad(format!("deployment {} of {id} failed; roll back to one that succeeded", target.id)));
    }
    // Each source's commit; entries kept before kiln recorded them carry the revision only.
    let commits = if !target.commits.is_empty() {
        target.commits.clone()
    } else if def.sources.len() == 1 && target.revision.is_some() {
        vec![target.revision.clone()]
    } else {
        return Err(bad(format!("deployment {} of {id} was kept without its sources' commits, so it can't be rendered again", target.id)));
    };
    let Some(cluster) = cx.cd.clusters.read().get(&def.destination.cluster).cloned() else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, format!("cluster {} is not connected", def.destination.cluster)));
    };
    let (kube_version, api_versions) = (cluster.version.read().clone(), cluster.discovery.read().api_versions.clone());
    let (cd, d2, at) = (cx.cd.clone(), def.clone(), commits.clone());
    let (head, back) = crate::rt::blocking(move || -> anyhow::Result<_> {
        let head = cd.inputs_of(&d2, kube_version.as_deref(), &api_versions, None)?;
        let back = cd.inputs_of(&d2, kube_version.as_deref(), &api_versions, Some(&at))?;
        Ok((head, back))
    })
    .await
    .and_then(|r| r)
    .map_err(|e| bad(format!("{e:#}")))?;
    let pin = state::Pin { to: target.id, revision: target.revision.clone(), commits, head: head.content, by: Some(cx.actor.clone()), at_ms: super::now_ms() };
    propose(&cx.cd, Command::Pin { app: id.clone(), pin: Some(Box::new(pin)) }).await?;
    // The held inputs and their sync as one step: nothing syncs the app in between.
    let request = SyncRequest { prune: v.get("prune").and_then(Value::as_bool), by: Some(cx.actor.clone()), trigger: Some(Trigger::Rollback), ..SyncRequest::default() };
    {
        let _gate = cx.cd.inputs_gate.lock();
        cx.cd.keep_inputs(&id, &back);
        cx.cd.input(Input::SyncTo { app: id.as_str().into(), revision: back.revision.as_str().into(), key: back.key, content: back.content, request });
    }
    audit(&cx, "cd.rollback", format!("{id} to deployment {}", target.id)).await;
    Ok(json!({"rolling_back": id, "to": target.id, "revision": target.revision}))
}

async fn follow(cx: Cx, v: Value) -> Out {
    let id = app_id(&v)?;
    exists(&cx.cd, &id)?;
    if cx.cd.store.read(|d| d.pins.contains_key(&id)) {
        propose(&cx.cd, Command::Pin { app: id.clone(), pin: None }).await?;
        cx.cd.kick();
        audit(&cx, "cd.follow", id.clone()).await;
    }
    Ok(json!({"following": id}))
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
