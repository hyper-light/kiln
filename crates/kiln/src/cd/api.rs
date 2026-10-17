//! `/api/cd`: deployments over HTTP, for the UI, the CLI, scripts and agents (docs/cd.md).
//!
//! Apps are addressed by their id's two parts, `/api/cd/apps/<project>/<name>`. Errors are
//! `{error, code}`; mutations are audited like the rest of kiln's API.

use std::sync::Arc;

use http::{Method, StatusCode};
use hyper::body::Incoming;
use kiln_cd::apps::{self, AppDef, Origin};
use kiln_cd::engine::{Input, SyncRequest};
use kiln_cd::model::{Health, Sync};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::state::{ClusterAuth, ClusterDef, Command, RepoDef};
use serde_json::{Value, json};

use super::Cd;
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;

/// An API failure: status and message.
pub type Fail = (StatusCode, String);
type R = Result<Response, Fail>;

const LIMIT: usize = 8 << 20;

fn bad(msg: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, msg.into())
}

fn missing(msg: impl Into<String>) -> Fail {
    (StatusCode::NOT_FOUND, msg.into())
}

fn ok(v: Value) -> R {
    Ok(srv::json(StatusCode::OK, &v))
}

/// Routes `/api/cd/...` (`rest` is the path after `cd`).
pub async fn route(hub: &Arc<Hub>, method: &Method, rest: &[&str], query: &std::collections::HashMap<String, String>, body: Incoming) -> R {
    let Some(cd) = super::get() else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "deployments are starting".into()));
    };
    match (method, rest) {
        (&Method::GET, ["summary"]) => ok(summary(&cd)),
        (&Method::GET, ["apps"]) => ok(list(&cd, query)),
        (&Method::POST, ["apps"]) => create(hub, &cd, body).await,
        (&Method::POST, ["import"]) => import(hub, &cd, body).await,
        (&Method::GET, ["apps", p, n]) => detail(&cd, &format!("{p}/{n}")).await,
        (&Method::DELETE, ["apps", p, n]) => delete(hub, &cd, &format!("{p}/{n}"), query).await,
        (&Method::GET, ["apps", p, n, "resources"]) => resources(&cd, &format!("{p}/{n}")).await,
        (&Method::GET, ["apps", p, n, "diff"]) => diff(&cd, &format!("{p}/{n}")).await,
        (&Method::GET, ["apps", p, n, "history"]) => history(&cd, &format!("{p}/{n}")),
        (&Method::POST, ["apps", p, n, "sync"]) => sync(hub, &cd, &format!("{p}/{n}"), body).await,
        (&Method::POST, ["apps", p, n, "refresh"]) => {
            let id = format!("{p}/{n}");
            exists(&cd, &id)?;
            cd.input(Input::Refresh { app: id.as_str().into(), hard: query.get("hard").is_some_and(|v| v == "true" || v == "1") });
            cd.kick();
            ok(json!({"refreshing": id}))
        }
        (&Method::POST, ["apps", p, n, "terminate"]) => {
            let id = format!("{p}/{n}");
            exists(&cd, &id)?;
            cd.input(Input::Terminate { app: id.as_str().into() });
            hub.store.audit("admin".into(), "cd.terminate".into(), id.clone()).await;
            ok(json!({"terminating": id}))
        }
        (&Method::POST, ["apps", p, n, "take-over"]) => take_over(hub, &cd, &format!("{p}/{n}")).await,
        (&Method::GET, ["clusters"]) => ok(clusters(&cd)),
        (&Method::POST, ["clusters"]) => add_cluster(hub, &cd, body).await,
        (&Method::DELETE, ["clusters", name]) => {
            propose(&cd, Command::DeleteCluster((*name).to_owned())).await?;
            hub.store.audit("admin".into(), "cd.cluster.delete".into(), (*name).to_owned()).await;
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        (&Method::GET, ["repos"]) => ok(json!({"repos": cd.store.read(|d| d.repos.values().map(|r| json!({
            "url": r.url, "kind": format!("{:?}", r.kind), "username": r.username, "insecure": r.insecure, "project": r.project,
            "has_password": r.password.is_some(), "has_ssh_key": r.ssh_key.is_some(),
        })).collect::<Vec<_>>())})),
        (&Method::POST, ["repos"]) => {
            let r: RepoDef = srv::body_json(body, LIMIT).await.map_err(|e| bad(format!("{e:#}")))?;
            let url = r.url.clone();
            propose(&cd, Command::PutRepo(Box::new(r))).await?;
            hub.store.audit("admin".into(), "cd.repo.put".into(), url).await;
            cd.kick();
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        (&Method::DELETE, ["repos"]) => {
            let url = query.get("url").cloned().ok_or_else(|| bad("?url= is required"))?;
            propose(&cd, Command::DeleteRepo(url.clone())).await?;
            hub.store.audit("admin".into(), "cd.repo.delete".into(), url).await;
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        _ => Err(missing("no such deployments route")),
    }
}

async fn propose(cd: &Cd, cmd: Command) -> Result<(), Fail> {
    match cd.store.submit(cmd).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(bad(e)),
        Err(_) => Err((StatusCode::SERVICE_UNAVAILABLE, "the deployments store stopped".into())),
    }
}

fn exists(cd: &Cd, id: &str) -> Result<(), Fail> {
    if cd.apps.read().contains_key(id) { Ok(()) } else { Err(missing(format!("no app {id}"))) }
}

fn summary(cd: &Cd) -> Value {
    let statuses = cd.statuses.read();
    let mut sync = std::collections::BTreeMap::<&str, u64>::new();
    let mut health = std::collections::BTreeMap::<&str, u64>::new();
    for s in statuses.values() {
        *sync.entry(s.sync.as_str()).or_default() += 1;
        *health.entry(s.health.as_str()).or_default() += 1;
    }
    json!({"apps": cd.apps.read().len(), "sync": sync, "health": health, "clusters": cd.clusters.read().len()})
}

/// Apps, filtered by `project`, `cluster`, `sync`, `health` and `q` (a substring of the id),
/// a page of `limit` (≤ 1000) after `cursor` (an id).
fn list(cd: &Cd, q: &std::collections::HashMap<String, String>) -> Value {
    let limit = q.get("limit").and_then(|l| l.parse::<usize>().ok()).unwrap_or(200).clamp(1, 1000);
    let apps = cd.apps.read();
    let statuses = cd.statuses.read();
    let wanted = |e: &super::AppEntry| -> bool {
        let id = e.def.id();
        let s = statuses.get(id.as_str());
        q.get("project").is_none_or(|p| *p == e.def.project)
            && q.get("cluster").is_none_or(|c| *c == e.def.destination.cluster)
            && q.get("sync").is_none_or(|v| s.is_some_and(|s| s.sync.as_str() == v))
            && q.get("health").is_none_or(|v| s.is_some_and(|s| s.health.as_str() == v))
            && q.get("q").is_none_or(|needle| id.contains(needle.as_str()))
    };
    let start = q.get("cursor").cloned();
    let mut items = Vec::new();
    let mut next = None;
    for (id, e) in apps.range(start.clone().unwrap_or_default()..) {
        if start.as_deref() == Some(id.as_str()) || !wanted(e) {
            continue;
        }
        if items.len() == limit {
            next = Some(id.clone());
            break;
        }
        items.push(app_row(e, statuses.get(id.as_str())));
    }
    json!({"apps": items, "next": next})
}

fn app_row(e: &super::AppEntry, s: Option<&kiln_cd::engine::Summary>) -> Value {
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
        m.insert("origin".into(), json!(origin(&e.def.origin)));
        m.insert("shadow".into(), json!(matches!(e.def.origin, Origin::Argo { .. }) && e.def.labels.get("kiln.dev/take-over").map(String::as_str) != Some("true")));
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

async fn detail(cd: &Cd, id: &str) -> R {
    let entry = cd.apps.read().get(id).cloned().ok_or_else(|| missing(format!("no app {id}")))?;
    let status = cd.statuses.read().get(id).cloned();
    let mut v = app_row(&entry, status.as_ref());
    if let Some(m) = v.as_object_mut() {
        m.insert("definition".into(), serde_json::to_value(&entry.def).unwrap_or(Value::Null));
        m.insert("commits".into(), json!(entry.commits));
        m.insert("children".into(), json!(entry.children));
    }
    ok(v)
}

async fn resources(cd: &Cd, id: &str) -> R {
    exists(cd, id)?;
    let key = id.to_owned();
    let list = cd.query(move |e| e.resources(&key)).await.unwrap_or_default();
    let items: Vec<Value> = list
        .iter()
        .map(|r| {
            json!({
                "group": r.key.group.as_ref(), "kind": r.key.kind.as_ref(), "namespace": r.key.namespace.as_ref(), "name": r.key.name.as_ref(),
                "sync": r.sync.as_str(), "missing": r.missing, "requires_pruning": r.requires_pruning,
                "health": r.health.map(|h| h.as_str()), "message": r.message, "hook": r.hook, "why": r.why,
            })
        })
        .collect();
    ok(json!({"resources": items}))
}

/// What differs, per resource: desired and live as YAML (status and the server's bookkeeping
/// left out, secrets' data masked) and the unified hunks between them.
async fn diff(cd: &Cd, id: &str) -> R {
    exists(cd, id)?;
    let key = id.to_owned();
    let pairs = cd
        .query(move |e| {
            e.resources(&key)
                .into_iter()
                .filter(|r| r.sync != Sync::Synced && !r.hook)
                .filter_map(|r| e.object(&key, &r.key).map(|(d, l)| (r.key, r.why, d, l)))
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
    let mut out = Vec::new();
    for (k, why, desired, live) in pairs {
        let (d, l) = (desired.map(display), live.map(display));
        let (dy, ly) = (yaml(d.as_ref()), yaml(l.as_ref()));
        let hunks = crate::harness::diff::hunks(&ly, &dy, 3);
        out.push(json!({"key": key_json(&k), "why": why, "desired": dy, "live": ly, "hunks": hunks}));
    }
    ok(json!({"diffs": out}))
}

fn key_json(k: &ResKey) -> Value {
    json!({"group": k.group.as_ref(), "kind": k.kind.as_ref(), "namespace": k.namespace.as_ref(), "name": k.name.as_ref()})
}

fn yaml(v: Option<&Value>) -> String {
    v.map(|v| serde_yaml::to_string(&sorted(v)).unwrap_or_default()).unwrap_or_default()
}

/// The value with every object's keys in order (apiVersion, kind and metadata first), so both
/// sides of a diff line up whatever order they arrived in.
fn sorted(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            let rank = |k: &str| match k {
                "apiVersion" => 0,
                "kind" => 1,
                "metadata" => 2,
                _ => 3,
            };
            keys.sort_by(|a, b| rank(a).cmp(&rank(b)).then(a.cmp(b)));
            let mut out = serde_json::Map::new();
            for k in keys {
                if let Some(x) = m.get(k) {
                    out.insert(k.clone(), sorted(x));
                }
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// An object as people read it: no status, no server bookkeeping, secrets masked.
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
    if obj::kind(&v) == "Secret" && obj::group(&v).is_empty() {
        for field in ["data", "stringData"] {
            if let Some(d) = v.get_mut(field).and_then(Value::as_object_mut) {
                for (i, val) in d.values_mut().enumerate() {
                    *val = Value::String(format!("++++++++ ({})", i + 1));
                }
            }
        }
    }
    v
}

fn history(cd: &Cd, id: &str) -> R {
    exists(cd, id)?;
    let list = cd.store.read(|d| d.history.get(id).cloned().unwrap_or_default());
    ok(json!({"history": list.iter().rev().map(|h| serde_json::to_value(h).unwrap_or(Value::Null)).collect::<Vec<_>>()}))
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct SyncBody {
    prune: Option<bool>,
    dry_run: bool,
    resources: Option<Vec<ResRef>>,
    options: Vec<String>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct ResRef {
    group: String,
    kind: String,
    namespace: String,
    name: String,
}

async fn sync(hub: &Arc<Hub>, cd: &Cd, id: &str, body: Incoming) -> R {
    exists(cd, id)?;
    let b: SyncBody = srv::body_json(body, LIMIT).await.map_err(|e| bad(format!("{e:#}")))?;
    let (options, unknown) = kiln_cd::model::SyncOptions::parse(b.options.iter().map(String::as_str));
    if let Some(u) = unknown.first() {
        return Err(bad(format!("unknown sync option {}", u.0)));
    }
    let request = SyncRequest {
        prune: b.prune,
        dry_run: b.dry_run,
        resources: b.resources.map(|rs| rs.iter().map(|r| ResKey::new(&r.group, &r.kind, &r.namespace, &r.name)).collect()),
        options,
        by: Some("admin".into()),
        trigger: None,
    };
    cd.input(Input::Sync { app: id.into(), request });
    hub.store.audit("admin".into(), "cd.sync".into(), id.to_owned()).await;
    ok(json!({"syncing": id}))
}

async fn take_over(hub: &Arc<Hub>, cd: &Cd, id: &str) -> R {
    let mut def = cd.apps.read().get(id).map(|e| e.def.clone()).ok_or_else(|| missing(format!("no app {id}")))?;
    def.labels.insert("kiln.dev/take-over".into(), "true".into());
    // Kept in kiln from now on, so the take-over survives restarts and the parent's re-renders.
    def.origin = Origin::Api;
    propose(cd, Command::PutApp(Box::new(def))).await?;
    hub.store.audit("admin".into(), "cd.take_over".into(), id.to_owned()).await;
    ok(json!({"taken_over": id}))
}

async fn create(hub: &Arc<Hub>, cd: &Cd, body: Incoming) -> R {
    let mut def: AppDef = srv::body_json(body, LIMIT).await.map_err(|e| bad(format!("{e:#}")))?;
    def.origin = Origin::Api;
    if def.sources.is_empty() || def.name.is_empty() {
        return Err(bad("an app needs a name and a source"));
    }
    let id = def.id();
    propose(cd, Command::PutApp(Box::new(def))).await?;
    hub.store.audit("admin".into(), "cd.app.put".into(), id.clone()).await;
    ok(json!({"app": id}))
}

/// Imports Argo CD manifests: every `Application` becomes an app kept in kiln, in shadow mode
/// until it is taken over; `AppProject`s and `ApplicationSet`s are reported as not imported yet.
async fn import(hub: &Arc<Hub>, cd: &Cd, body: Incoming) -> R {
    let bytes = srv::body_bytes(body, LIMIT).await.map_err(|e| bad(format!("{e:#}")))?;
    let text = String::from_utf8_lossy(&bytes);
    let docs = obj::parse_documents(&text).map_err(|e| bad(format!("{e:#}")))?;
    let clusters = cd.store.read(|d| d.clusters.clone());
    let by_url = |url: &str| clusters.values().find(|c| c.server.trim_end_matches('/') == url.trim_end_matches('/')).map(|c| c.name.clone());
    let mut imported = Vec::new();
    let mut skipped = Vec::new();
    for d in docs {
        match (obj::group(&d), obj::kind(&d)) {
            ("argoproj.io", "Application") => match apps::from_argo(&d, None, &by_url) {
                Ok(def) => {
                    let id = def.id();
                    propose(cd, Command::PutApp(Box::new(def))).await?;
                    imported.push(id);
                }
                Err(e) => skipped.push(json!({"name": obj::name(&d), "why": e.0})),
            },
            (g, k) => skipped.push(json!({"name": obj::name(&d), "why": format!("{g}/{k} is not imported yet")})),
        }
    }
    hub.store.audit("admin".into(), "cd.import".into(), imported.join(",")).await;
    ok(json!({"imported": imported, "skipped": skipped}))
}

async fn delete(hub: &Arc<Hub>, cd: &Cd, id: &str, q: &std::collections::HashMap<String, String>) -> R {
    let entry = cd.apps.read().get(id).cloned().ok_or_else(|| missing(format!("no app {id}")))?;
    let cascade = q.get("cascade").map(|c| c != "false" && c != "orphan").unwrap_or(entry.def.cascade);
    if matches!(entry.def.origin, Origin::Api | Origin::Argo { parent: None }) {
        propose(cd, Command::DeleteApp(id.to_owned())).await?;
    }
    cd.unregister(id, cascade);
    hub.store.audit("admin".into(), "cd.app.delete".into(), format!("{id} cascade={cascade}")).await;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

fn clusters(cd: &Cd) -> Value {
    let connected = cd.clusters.read();
    let defs = cd.cluster_defs();
    json!({"clusters": defs.iter().map(|c| {
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
    }).collect::<Vec<_>>()})
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum ClusterBody {
    /// A kubeconfig context on the server.
    Context {
        name: String,
        context: String,
        #[serde(default)]
        kubeconfig: String,
        #[serde(default)]
        labels: std::collections::BTreeMap<String, String>,
    },
    /// A full definition.
    Full(Box<ClusterDef>),
}

async fn add_cluster(hub: &Arc<Hub>, cd: &Cd, body: Incoming) -> R {
    let b: ClusterBody = srv::body_json(body, LIMIT).await.map_err(|e| bad(format!("{e:#}")))?;
    let def = match b {
        ClusterBody::Context { name, context, kubeconfig, labels } => {
            let ep = super::kubeconfig_endpoint(&kubeconfig, &context).map_err(|e| bad(format!("{e:#}")))?;
            ClusterDef { name, server: ep.server, auth: ClusterAuth::Kubeconfig { path: kubeconfig, context }, labels, ..ClusterDef::default() }
        }
        ClusterBody::Full(def) => *def,
    };
    let name = def.name.clone();
    propose(cd, Command::PutCluster(Box::new(def))).await?;
    hub.store.audit("admin".into(), "cd.cluster.put".into(), name.clone()).await;
    ok(json!({"cluster": name}))
}
