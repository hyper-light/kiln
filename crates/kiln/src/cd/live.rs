//! What an app runs, read from its cluster when someone looks (docs/cd.md, "API"): its resource
//! tree, one object's live form, its events and its pods' logs.
//!
//! The engine keeps what it compares: the app's objects, without status or server metadata. The
//! tree needs more (uids, owners, status) and objects the engine never watches (ReplicaSets,
//! Pods, Jobs), so they are read here, bounded: one list per kind per namespace, the app's own
//! objects selected by its tracking label where it has one, children listed only where an
//! object that owns such children lives. Lists and trees are kept a few seconds and shared by
//! every app and viewer that asks meanwhile. Secrets are listed as metadata alone.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use http::StatusCode;
use kiln_cd::apps::AppDef;
use kiln_cd::model::Tracking;
use kiln_cd::obj::{self, GroupKind, ResKey};
use kiln_cd::tree::{self, Managed, Node, Tree};
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::Cd;
use super::kube::Cluster;
use super::ops::Fail;
use crate::http::ws::{self, Message};

/// How long a list or a tree is reused.
const TTL: Duration = Duration::from_secs(4);
/// Most namespaces an app's children are looked for in.
const MAX_NAMESPACES: usize = 16;
/// Most managed objects fetched one by one (those their lists didn't hold).
const MAX_GETS: usize = 32;
/// Most pods whose logs one request reads.
pub const MAX_LOG_PODS: usize = 8;
/// Most bytes of one pod's log one request reads.
const LOG_BYTES: usize = 2 << 20;
/// Longest log line kept; longer ones are cut.
const LINE_MAX: usize = 16 << 10;
/// Most requests one tree has in flight at once, so reading it never floods a cluster.
const IN_FLIGHT: usize = 8;

/// Runs `futs` [`IN_FLIGHT`] at a time, a batch after another; their results in order.
///
/// Batches of `join_all`, not `buffer_unordered`: hyper-rt's sockets know the task polling them by
/// its waker, and `FuturesUnordered` polls each future with a waker of its own, so a request that
/// had to dial a new connection failed with "not on a shard thread" (`join_all` of fewer than 30
/// futures polls them with the task's own).
async fn bounded<T, F: std::future::Future<Output = T>>(futs: Vec<F>) -> Vec<T> {
    let mut out = Vec::with_capacity(futs.len());
    let mut rest = futs.into_iter();
    loop {
        let batch: Vec<F> = rest.by_ref().take(IN_FLIGHT).collect();
        if batch.is_empty() {
            return out;
        }
        out.extend(futures::future::join_all(batch).await);
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ListKey {
    cluster: String,
    gk: GroupKind,
    namespace: String,
    selector: Option<String>,
    metadata: bool,
}

type Listed = Arc<Result<Vec<Value>, String>>;

/// The reads kept for a moment, and the commits already looked up.
#[derive(Default)]
pub struct Live {
    lists: Mutex<HashMap<ListKey, (Instant, Listed)>>,
    trees: Mutex<HashMap<String, (Instant, Arc<AppTree>)>>,
    commits: Mutex<HashMap<String, Option<Value>>>,
}

/// An app's tree as last read.
pub struct AppTree {
    /// The tree.
    pub tree: Tree,
    /// What couldn't be read (a list the cluster refused).
    pub errors: Vec<String>,
    /// When it was read (ms since the epoch).
    pub at_ms: u64,
}

impl AppTree {
    /// The node of an object, by its key.
    pub fn node(&self, k: &ResKey) -> Option<&Node> {
        self.tree.nodes.iter().find(|n| n.group == *k.group && n.kind == *k.kind && n.namespace == *k.namespace && n.name == *k.name)
    }

    fn by_uid(&self, uid: &str) -> Option<&Node> {
        self.tree.nodes.iter().find(|n| n.uid.as_deref() == Some(uid))
    }

    /// A node's owners, nearest first, up to the app's own object.
    pub fn owners<'a>(&'a self, n: &'a Node) -> Vec<&'a Node> {
        let mut out: Vec<&Node> = Vec::new();
        let mut cur = n;
        while let Some(p) = cur.parents.first().and_then(|u| self.by_uid(u)) {
            if out.iter().any(|x| x.id == p.id) || p.id == n.id {
                break;
            }
            out.push(p);
            cur = p;
        }
        out
    }

    /// The pods under a node (the node itself, if it is one), newest first.
    pub fn pods<'a>(&'a self, n: &'a Node) -> Vec<&'a Node> {
        if n.group.is_empty() && n.kind == "Pod" {
            return vec![n];
        }
        let Some(root) = n.uid.as_deref() else { return Vec::new() };
        let mut kids: HashMap<&str, Vec<&Node>> = HashMap::new();
        for x in &self.tree.nodes {
            for p in &x.parents {
                kids.entry(p.as_str()).or_default().push(x);
            }
        }
        let mut out: Vec<&Node> = Vec::new();
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut todo = vec![root];
        while let Some(u) = todo.pop() {
            for k in kids.get(u).into_iter().flatten() {
                let Some(ku) = k.uid.as_deref() else { continue };
                if !seen.insert(ku) {
                    continue;
                }
                if k.group.is_empty() && k.kind == "Pod" {
                    out.push(k);
                }
                todo.push(ku);
            }
        }
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| a.name.cmp(&b.name)));
        out
    }

    /// The tree as the API gives it.
    pub fn to_json(&self, def: &AppDef) -> Value {
        json!({
            "app": def.id(), "cluster": def.destination.cluster, "namespace": def.destination.namespace,
            "nodes": self.tree.nodes, "truncated": self.tree.truncated, "errors": self.errors, "at_ms": self.at_ms,
        })
    }
}

fn unavailable(msg: impl Into<String>) -> Fail {
    (StatusCode::SERVICE_UNAVAILABLE, msg.into())
}

fn missing(msg: impl Into<String>) -> Fail {
    (StatusCode::NOT_FOUND, msg.into())
}

/// The label selector that finds an app's own objects, where its tracking has a label.
fn selector(def: &AppDef) -> Option<String> {
    match def.tracking {
        Tracking::Kiln => Some(format!("{}={}", kiln_cd::engine::APP_LABEL, kiln_cd::engine::app_label(&def.id()))),
        Tracking::ArgoLabel | Tracking::ArgoAnnotationAndLabel => Some(format!("{}={}", kiln_cd::engine::ARGO_INSTANCE, def.name)),
        Tracking::ArgoAnnotation => None,
    }
}

/// What a custom resource may own: the children the tree lists, and the workloads and Services an
/// operator makes for it (a Prometheus' StatefulSet, a database's Service), whose own children are
/// in the same lists.
const OPERATED: &[(&str, &str)] = &[
    ("", "Pod"),
    ("apps", "ReplicaSet"),
    ("apps", "ControllerRevision"),
    ("batch", "Job"),
    ("discovery.k8s.io", "EndpointSlice"),
    ("apps", "StatefulSet"),
    ("apps", "Deployment"),
    ("apps", "DaemonSet"),
    ("", "Service"),
];

/// The kinds of children an object of a kind can own, among those the tree lists. Kinds known to
/// own none list nothing; kinds kiln doesn't know (custom resources) may own any of [`OPERATED`].
fn child_kinds(group: &str, kind: &str) -> &'static [(&'static str, &'static str)] {
    match (group, kind) {
        ("apps", "Deployment") => &[("apps", "ReplicaSet"), ("", "Pod")],
        ("apps", "ReplicaSet") | ("batch", "Job") => &[("", "Pod")],
        ("apps", "StatefulSet" | "DaemonSet") => &[("", "Pod"), ("apps", "ControllerRevision")],
        ("batch", "CronJob") => &[("batch", "Job"), ("", "Pod")],
        ("", "Service") => &[("discovery.k8s.io", "EndpointSlice")],
        (
            "",
            "Pod" | "ConfigMap" | "Secret" | "ServiceAccount" | "PersistentVolumeClaim" | "PersistentVolume" | "Namespace" | "Endpoints"
            | "LimitRange" | "ResourceQuota" | "Event",
        )
        | (
            "rbac.authorization.k8s.io"
            | "networking.k8s.io"
            | "policy"
            | "scheduling.k8s.io"
            | "storage.k8s.io"
            | "apiextensions.k8s.io"
            | "admissionregistration.k8s.io"
            | "autoscaling"
            | "coordination.k8s.io"
            | "certificates.k8s.io"
            | "node.k8s.io"
            | "apiregistration.k8s.io"
            | "discovery.k8s.io"
            | "events.k8s.io"
            | "flowcontrol.apiserver.k8s.io",
            _,
        ) => &[],
        _ => OPERATED,
    }
}

fn now_ms() -> u64 {
    super::now_ms()
}

impl Live {
    /// A list, as read within the last few seconds or read now.
    async fn list(&self, c: &Arc<Cluster>, gk: GroupKind, namespace: &str, selector: Option<&str>, metadata: bool) -> Listed {
        let key = ListKey { cluster: c.name.clone(), gk: gk.clone(), namespace: namespace.to_owned(), selector: selector.map(str::to_owned), metadata };
        if let Some((at, l)) = self.lists.lock().get(&key)
            && at.elapsed() < TTL
        {
            return l.clone();
        }
        let ns = (!namespace.is_empty()).then_some(namespace);
        let got = Arc::new(c.list_in(&gk, ns, selector, metadata).await.map_err(|e| format!("{e:#}")));
        let mut lists = self.lists.lock();
        lists.retain(|_, (at, _)| at.elapsed() < TTL * 4);
        lists.insert(key, (Instant::now(), got.clone()));
        got
    }

    /// An app's tree, as read within the last few seconds (unless `fresh`) or read now.
    pub async fn tree(&self, cd: &Arc<Cd>, id: &str, fresh: bool) -> Result<Arc<AppTree>, Fail> {
        if !fresh
            && let Some((at, t)) = self.trees.lock().get(id)
            && at.elapsed() < TTL
        {
            return Ok(t.clone());
        }
        let def = cd.apps.read().get(id).map(|e| e.def.clone()).ok_or_else(|| missing(format!("no app {id}")))?;
        let cluster = cd
            .clusters
            .read()
            .get(&def.destination.cluster)
            .cloned()
            .ok_or_else(|| unavailable(format!("cluster {} is not connected", def.destination.cluster)))?;
        let key = id.to_owned();
        let views = cd
            .query(move |e| {
                e.resources(&key)
                    .into_iter()
                    .map(|r| {
                        // An object not in the cluster says its version through its rendered form.
                        let api = if r.missing {
                            e.object(&key, &r.key).and_then(|(d, _)| d).and_then(|d| obj::str_at(&d, &["apiVersion"]).map(str::to_owned))
                        } else {
                            None
                        };
                        (r, api)
                    })
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
        let sel = selector(&def);
        let mut errors: Vec<String> = Vec::new();

        // The app's own objects: one list per kind and namespace.
        let mut groups: BTreeMap<(GroupKind, String), Vec<ResKey>> = BTreeMap::new();
        for (r, _) in views.iter().filter(|(r, _)| !r.missing) {
            groups.entry((r.key.group_kind(), r.key.namespace.to_string())).or_default().push(r.key.clone());
        }
        let own = groups.keys().filter(|(gk, _)| cluster.serves(&gk.group, &gk.kind)).map(|(gk, ns)| {
            let metadata = gk.group.is_empty() && &*gk.kind == "Secret";
            let (c, sel) = (&cluster, sel.as_deref());
            async move { ((gk.clone(), ns.clone()), self.list(c, gk.clone(), ns, sel, metadata).await) }
        });
        let own: Vec<((GroupKind, String), Listed)> = bounded(own.collect()).await;
        let mut found: HashMap<ResKey, Value> = HashMap::new();
        for ((gk, ns), listed) in &own {
            match listed.as_ref() {
                Ok(items) => {
                    let wanted: BTreeSet<&str> = groups.get(&(gk.clone(), ns.clone())).into_iter().flatten().map(|k| &*k.name).collect();
                    for o in items.iter().filter(|o| wanted.contains(obj::name(o))) {
                        found.insert(ResKey::new(&gk.group, &gk.kind, obj::namespace(o), obj::name(o)), o.clone());
                    }
                }
                Err(e) => errors.push(e.clone()),
            }
        }
        // What the lists didn't hold (an object without the label yet), one by one.
        let stray: Vec<ResKey> = views
            .iter()
            .filter(|(r, _)| !r.missing && !found.contains_key(&r.key) && cluster.serves(&r.key.group, &r.key.kind))
            .map(|(r, _)| r.key.clone())
            .take(MAX_GETS)
            .collect();
        let gets = bounded(
            stray
                .iter()
                .map(|k| {
                    let c = &cluster;
                    async move { (k.clone(), c.get(k).await) }
                })
                .collect(),
        )
        .await;
        for (k, got) in gets {
            match got {
                Ok(Some(mut o)) => {
                    // A Secret's values never sit in what is kept.
                    if k.group.is_empty() && &*k.kind == "Secret"
                        && let Some(m) = o.as_object_mut()
                    {
                        m.remove("data");
                        m.remove("stringData");
                    }
                    found.insert(k, o);
                }
                Ok(None) => {}
                Err(e) => errors.push(format!("reading {k}: {e:#}")),
            }
        }

        // Children: the kinds the app's objects can own, in the namespaces they live in.
        let mut scans: BTreeSet<(&'static str, &'static str, String)> = BTreeSet::new();
        let mut namespaces: BTreeSet<String> = BTreeSet::new();
        for (r, _) in views.iter().filter(|(r, _)| !r.missing && !r.key.namespace.is_empty()) {
            let kinds = child_kinds(&r.key.group, &r.key.kind);
            if kinds.is_empty() {
                continue;
            }
            if !namespaces.contains(&*r.key.namespace) {
                if namespaces.len() >= MAX_NAMESPACES {
                    errors.push(format!("children are looked for in {MAX_NAMESPACES} namespaces at most; not in {}", r.key.namespace));
                    continue;
                }
                namespaces.insert(r.key.namespace.to_string());
            }
            for (g, k) in kinds {
                scans.insert((g, k, r.key.namespace.to_string()));
            }
        }
        let lists = scans.iter().filter(|(g, k, _)| cluster.serves(g, k)).map(|(g, k, ns)| {
            let c = &cluster;
            async move { self.list(c, GroupKind::new(g, k), ns, None, false).await }
        });
        let mut listed: Vec<Value> = Vec::new();
        for l in bounded(lists.collect()).await {
            match l.as_ref() {
                Ok(items) => listed.extend(items.iter().cloned()),
                Err(e) => errors.push(e.clone()),
            }
        }

        let managed: Vec<Managed> = views
            .into_iter()
            .map(|(r, api)| Managed {
                live: found.remove(&r.key),
                api_version: api,
                sync: if r.hook { "Hook".to_owned() } else { r.sync.as_str().to_owned() },
                health: r.health,
                message: r.message,
                hook: r.hook,
                missing: r.missing,
                requires_pruning: r.requires_pruning,
                why: r.why,
                ..Managed::new(r.key)
            })
            .collect();
        let tree = crate::rt::blocking(move || tree::build(managed, listed)).await.map_err(|e| unavailable(format!("{e:#}")))?;
        errors.sort();
        errors.dedup();
        let t = Arc::new(AppTree { tree, errors, at_ms: now_ms() });
        let mut trees = self.trees.lock();
        trees.retain(|_, (at, _)| at.elapsed() < TTL * 4);
        trees.insert(id.to_owned(), (Instant::now(), t.clone()));
        Ok(t)
    }

    /// A commit's author, time and message, looked up once.
    pub async fn commit(&self, cd: &Arc<Cd>, url: &str, sha: &str) -> Option<Value> {
        let key = format!("{url}\n{sha}");
        if let Some(c) = self.commits.lock().get(&key) {
            return c.clone();
        }
        let (dir, auth, url2, sha2) = (cd.mirror_dir(url), cd.git_auth(url), url.to_owned(), sha.to_owned());
        let got = crate::rt::blocking(move || -> anyhow::Result<kiln_cd::git::Commit> {
            kiln_cd::git::Mirror::open(&dir, &url2, auth)?.commit(&sha2)
        })
        .await
        .ok()
        .and_then(Result::ok)
        .map(|c| json!({"sha": c.sha, "author": c.author, "email": c.email, "at_ms": c.at * 1000, "subject": c.subject, "body": c.body}));
        let mut commits = self.commits.lock();
        if commits.len() > 512 {
            commits.clear();
        }
        commits.insert(key, got.clone());
        got
    }
}

/// An object's events (core/v1 Events whose `involvedObject` is it), newest first.
pub async fn events(c: &Cluster, k: &ResKey, uid: Option<&str>) -> anyhow::Result<Vec<Value>> {
    let field = match uid {
        Some(u) => format!("involvedObject.uid={u}"),
        None => format!("involvedObject.kind={},involvedObject.name={}", k.kind, k.name),
    };
    let base = if k.namespace.is_empty() { "/api/v1/events".to_owned() } else { format!("/api/v1/namespaces/{}/events", kiln_cd::kube::enc(&k.namespace)) };
    let doc = c.get_path(&format!("{base}?limit=500&fieldSelector={}", kiln_cd::kube::enc(&field))).await?;
    let when = |e: &Value| -> String {
        ["series.lastObservedTime", "lastTimestamp", "eventTime", "firstTimestamp", "metadata.creationTimestamp"]
            .iter()
            .find_map(|p| obj::str_at(e, &p.split('.').collect::<Vec<_>>()).filter(|s| !s.is_empty()).map(str::to_owned))
            .unwrap_or_default()
    };
    let mut out: Vec<(String, Value)> = obj::list_at(&doc, &["items"])
        .into_iter()
        .flatten()
        .map(|e| {
            let last = when(e);
            let first = obj::str_at(e, &["firstTimestamp"]).or_else(|| obj::str_at(e, &["eventTime"])).unwrap_or(&last).to_owned();
            let source = obj::str_at(e, &["source", "component"]).or_else(|| obj::str_at(e, &["reportingComponent"])).unwrap_or("");
            let count = obj::i64_at(e, &["count"]).or_else(|| obj::i64_at(e, &["series", "count"])).unwrap_or(1);
            (
                last.clone(),
                json!({
                    "type": obj::str_at(e, &["type"]).unwrap_or("Normal"),
                    "reason": obj::str_at(e, &["reason"]).unwrap_or(""),
                    "message": obj::str_at(e, &["message"]).unwrap_or("").trim(),
                    "count": count, "first": first, "last": last, "source": source,
                    "object": {"kind": obj::str_at(e, &["involvedObject", "kind"]), "name": obj::str_at(e, &["involvedObject", "name"])},
                }),
            )
        })
        .collect();
    out.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(out.into_iter().map(|(_, e)| e).collect())
}

/// How a log is read: the log endpoint's query, from what the API takes.
#[derive(Clone, Debug, Default)]
pub struct LogQuery {
    /// The container (the pod's first when none).
    pub container: Option<String>,
    /// Lines from the end.
    pub tail: Option<u64>,
    /// Only lines this recent.
    pub since: Option<u64>,
    /// The previous run of the container (after a restart).
    pub previous: bool,
}

impl LogQuery {
    /// From an op's input, or the query of the WebSocket route.
    pub fn from_json(v: &Value) -> LogQuery {
        let num = |k: &str| v.get(k).and_then(|x| x.as_u64().or_else(|| x.as_str().and_then(|s| s.parse().ok())));
        LogQuery {
            container: v.get("container").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned),
            tail: num("tail_lines"),
            since: num("since_seconds"),
            previous: v.get("previous").is_some_and(|p| p.as_bool() == Some(true) || p.as_str() == Some("true")),
        }
    }

    fn query(&self, container: &str, follow: bool) -> String {
        let mut q = format!("timestamps=true&container={}", kiln_cd::kube::enc(container));
        let tail = self.tail.unwrap_or(if follow { 200 } else { 500 }).clamp(1, 10_000);
        q.push_str(&format!("&tailLines={tail}"));
        if let Some(s) = self.since.filter(|s| *s > 0) {
            q.push_str(&format!("&sinceSeconds={s}"));
        }
        if self.previous && !follow {
            q.push_str("&previous=true");
        }
        if !follow {
            q.push_str(&format!("&limitBytes={LOG_BYTES}"));
        }
        q
    }
}

/// The container a log reads in a pod: the one asked for, else its first (not an init one).
pub fn container_of(pod: &Node, asked: Option<&str>) -> Option<String> {
    match asked {
        Some(c) => Some(c.to_owned()),
        None => pod.containers.iter().find(|c| !c.init).or(pod.containers.first()).map(|c| c.name.clone()),
    }
}

/// One log line: its pod, its time, its text.
fn line(pod: &str, raw: &str) -> Value {
    let raw = raw.strip_suffix('\r').unwrap_or(raw);
    let (ts, text) = match raw.split_once(' ') {
        Some((t, rest)) if t.len() >= 20 && t.as_bytes().get(4) == Some(&b'-') && t.ends_with('Z') => (t, rest),
        _ => ("", raw),
    };
    let text = if text.len() > LINE_MAX {
        let mut cut = LINE_MAX;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…", &text[..cut])
    } else {
        text.to_owned()
    };
    json!({"pod": pod, "ts": ts, "text": text})
}

/// The last lines of some pods' logs, merged by time.
pub async fn logs(c: &Cluster, pods: &[&Node], q: &LogQuery) -> (Vec<Value>, Vec<String>) {
    let reads = pods.iter().map(|p| async move {
        let Some(container) = container_of(p, q.container.as_deref()) else {
            return (p.name.clone(), Err(anyhow::anyhow!("it has no containers")));
        };
        (p.name.clone(), c.pod_log(&p.namespace, &p.name, &q.query(&container, false)).await)
    });
    let mut lines: Vec<Value> = Vec::new();
    let mut errors = Vec::new();
    for (pod, got) in futures::future::join_all(reads).await {
        match got {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                lines.extend(text.lines().filter(|l| !l.is_empty()).map(|l| line(&pod, l)));
            }
            Err(e) => errors.push(format!("{pod}: {e:#}")),
        }
    }
    // Stable, so each pod's own lines keep their order where times tie.
    lines.sort_by(|a, b| a["ts"].as_str().cmp(&b["ts"].as_str()));
    (lines, errors)
}

/// Streams some pods' logs as they are written to a WebSocket, as JSON text messages:
/// `{type: "lines", lines: [{pod, ts, text}]}`, `{type: "error", pod, message}`, and
/// `{type: "end"}` once every log has ended. Closing the socket stops it.
pub async fn follow(c: Arc<Cluster>, pods: Vec<(String, String, String)>, q: LogQuery, socket: ws::WebSocket) -> anyhow::Result<()> {
    use futures::FutureExt as _;
    enum Ev {
        Data(usize, bytes::Bytes),
        End(usize),
        Fail(usize, String),
    }
    let (mut tx, mut rx) = socket.split();
    let mut streams = Vec::new();
    for (i, (ns, pod, container)) in pods.iter().enumerate() {
        match c.follow_log(ns, pod, &q.query(container, true)).await {
            Ok(resp) if resp.status().is_success() => {
                let body = resp.into_body();
                streams.push(
                    futures::stream::unfold(Some(body), move |state| async move {
                        let mut body = state?;
                        match super::kube::next_chunk(&mut body).await {
                            Ok(Some(b)) => Some((Ev::Data(i, b), Some(body))),
                            Ok(None) => Some((Ev::End(i), None)),
                            Err(e) => Some((Ev::Fail(i, format!("{e:#}")), None)),
                        }
                    })
                    .boxed(),
                );
            }
            Ok(resp) => {
                let status = resp.status().as_u16();
                let body = http_body_util::BodyExt::collect(resp.into_body()).await.map(|b| b.to_bytes()).unwrap_or_default();
                let msg = kiln_cd::kube::status_message(status, &body);
                tx.send(ws::text(json!({"type": "error", "pod": pod, "message": msg}).to_string())).await?;
            }
            Err(e) => tx.send(ws::text(json!({"type": "error", "pod": pod, "message": format!("{e:#}")}).to_string())).await?,
        }
    }
    let mut open = streams.len();
    let mut all = futures::stream::select_all(streams);
    let mut partial: Vec<String> = vec![String::new(); pods.len()];
    while open > 0 {
        futures::select_biased! {
            msg = rx.next().fuse() => match msg {
                Some(Ok(Message::Close(_))) => {
                    tx.close(None).await.ok();
                    return Ok(());
                }
                None | Some(Err(_)) => return Ok(()),
                _ => {}
            },
            ev = all.next().fuse() => match ev {
                Some(Ev::Data(i, b)) => {
                    let Some(buf) = partial.get_mut(i) else { continue };
                    buf.push_str(&String::from_utf8_lossy(&b));
                    let Some(end) = buf.rfind('\n') else { continue };
                    let done: String = buf.drain(..=end).collect();
                    let pod = pods.get(i).map(|p| p.1.as_str()).unwrap_or("");
                    let lines: Vec<Value> = done.lines().filter(|l| !l.is_empty()).map(|l| line(pod, l)).collect();
                    if !lines.is_empty() {
                        tx.send(ws::text(json!({"type": "lines", "lines": lines}).to_string())).await?;
                    }
                }
                Some(Ev::End(i)) => {
                    open -= 1;
                    let pod = pods.get(i).map(|p| p.1.as_str()).unwrap_or("");
                    if let Some(rest) = partial.get_mut(i).map(std::mem::take).filter(|r| !r.is_empty()) {
                        tx.send(ws::text(json!({"type": "lines", "lines": [line(pod, &rest)]}).to_string())).await?;
                    }
                }
                Some(Ev::Fail(i, e)) => {
                    open -= 1;
                    let pod = pods.get(i).map(|p| p.1.as_str()).unwrap_or("");
                    tx.send(ws::text(json!({"type": "error", "pod": pod, "message": e}).to_string())).await?;
                }
                None => break,
            },
        }
    }
    tx.send(ws::text(json!({"type": "end"}).to_string())).await?;
    tx.close(None).await.ok();
    Ok(())
}
