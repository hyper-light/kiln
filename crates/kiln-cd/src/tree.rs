//! An app's resource tree, as Argo CD draws it: the objects the app manages, and every live
//! object they own, found through `metadata.ownerReferences`, transitively (a Deployment's
//! ReplicaSets and their Pods, a CronJob's Jobs and theirs, a Service's EndpointSlices).
//!
//! [`build`] joins what the owner read from the cluster on owner uids. It is a pure function:
//! objects owned by nothing in the app stay out, an owner that isn't there leaves its objects out
//! too, and ownership cycles (which the API server doesn't forbid) are cut, so the result is a
//! forest every node of which is reached once. Each node carries what people read off it: health
//! and sync, when it was made, ready counts, restarts, images and a few `info` pairs per kind.

use std::collections::VecDeque;

use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;
use serde_json::Value;

use crate::health::{self, BuiltIn};
use crate::model::Health;
use crate::obj::{self, ResKey, bool_at, i64_at, list_at, str_at};

/// Most nodes a tree holds; past it, children are left out and the tree says so.
pub const MAX_NODES: usize = 5000;

/// The kinds the owner lists to find an app's children: `(group, kind)`.
pub const CHILD_KINDS: &[(&str, &str)] =
    &[("", "Pod"), ("apps", "ReplicaSet"), ("apps", "ControllerRevision"), ("batch", "Job"), ("discovery.k8s.io", "EndpointSlice")];

/// One object an app manages, as the engine sees it, with its live form when the owner read it.
#[derive(Clone, Debug)]
pub struct Managed {
    /// Its key.
    pub key: ResKey,
    /// Its API version, for objects not in the cluster (from the rendered object).
    pub api_version: Option<String>,
    /// `Synced`, `OutOfSync`, `Unknown`, or `Hook` for hooks.
    pub sync: String,
    /// Health, as the engine assesses it.
    pub health: Option<Health>,
    /// Why it has that health.
    pub message: Option<String>,
    /// A sync hook.
    pub hook: bool,
    /// In git, not in the cluster.
    pub missing: bool,
    /// In the cluster, no longer in git.
    pub requires_pruning: bool,
    /// Where it differs.
    pub why: Option<String>,
    /// The app whose tracking the live object carries, when that isn't this one.
    pub tracked_by: Option<String>,
    /// Other apps that deploy it too.
    pub shared_with: Vec<String>,
    /// It differs only by tracking marks.
    pub marks_only: bool,
    /// The live object as the cluster serves it (status and server metadata included).
    pub live: Option<Value>,
}

impl Managed {
    /// A synced object with nothing else known of it yet.
    pub fn new(key: ResKey) -> Managed {
        Managed {
            key,
            api_version: None,
            sync: "Synced".into(),
            health: None,
            message: None,
            hook: false,
            missing: false,
            requires_pruning: false,
            why: None,
            tracked_by: None,
            shared_with: Vec::new(),
            marks_only: false,
            live: None,
        }
    }
}

/// A label and value people read on a node (`Ready` · `2/2`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Info {
    /// What it is.
    pub name: String,
    /// Its value.
    pub value: String,
}

/// One container of a pod, or of a workload's pod template.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Container {
    /// Its name.
    pub name: String,
    /// Its image.
    pub image: String,
    /// An init container.
    pub init: bool,
    /// Whether it is ready (pods only).
    pub ready: Option<bool>,
    /// How often it restarted (pods only).
    pub restarts: Option<i64>,
    /// `running`, `waiting` or `terminated` (pods only).
    pub state: Option<String>,
    /// Why it waits or ended (`CrashLoopBackOff`, `Completed`, `OOMKilled`).
    pub reason: Option<String>,
    /// Why its previous run ended, when it restarted.
    pub last: Option<String>,
}

/// One node of the tree.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Node {
    /// Unique in the tree: the object's uid, or its key when it isn't in the cluster.
    pub id: String,
    /// The object's uid, when it is live.
    pub uid: Option<String>,
    /// API group; empty for the core group.
    pub group: String,
    /// API version.
    pub version: String,
    /// Kind.
    pub kind: String,
    /// Namespace; empty for cluster-scoped objects.
    pub namespace: String,
    /// Name.
    pub name: String,
    /// The uids of its owners in the tree (none for the app's top-level objects).
    pub parents: Vec<String>,
    /// The app manages it (it is in git, or tracked as applied by the app).
    pub managed: bool,
    /// Health; for children, assessed here by the built-in rules.
    pub health: Option<String>,
    /// Why it has that health.
    pub message: Option<String>,
    /// Sync, for managed objects.
    pub sync: Option<String>,
    /// A sync hook.
    pub hook: bool,
    /// In git, not in the cluster.
    pub missing: bool,
    /// In the cluster, no longer in git.
    pub requires_pruning: bool,
    /// Where it differs from git.
    pub why: Option<String>,
    /// The app whose tracking the live object carries, when that isn't this one: a sync of
    /// this one takes it over.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracked_by: Option<String>,
    /// Other apps that deploy it too.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub shared_with: Vec<String>,
    /// It differs only by tracking marks: a sync changes nothing else in it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub marks_only: bool,
    /// `metadata.creationTimestamp`.
    pub created_at: Option<String>,
    /// A few pairs per kind, as Argo CD shows them.
    pub info: Vec<Info>,
    /// A pod's state in `kubectl get pods` words (`Running`, `CrashLoopBackOff`, `Completed`).
    pub status: Option<String>,
    /// Ready out of wanted: containers of a pod, replicas of a workload (`2/2`).
    pub ready: Option<String>,
    /// A pod's container restarts.
    pub restarts: Option<i64>,
    /// The images it runs.
    pub images: Vec<String>,
    /// Its containers (pods, and workloads' templates).
    pub containers: Vec<Container>,
    /// The node a pod runs on.
    pub node: Option<String>,
}

/// An app's tree.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Tree {
    /// Managed objects first (in key order), then their children (owners before what they own).
    pub nodes: Vec<Node>,
    /// Children were left out past [`MAX_NODES`].
    pub truncated: bool,
}

fn uid(v: &Value) -> Option<&str> {
    str_at(v, &["metadata", "uid"]).filter(|u| !u.is_empty())
}

fn owner_uids(v: &Value) -> impl Iterator<Item = &str> {
    list_at(v, &["metadata", "ownerReferences"]).into_iter().flatten().filter_map(|r| str_at(r, &["uid"])).filter(|u| !u.is_empty())
}

fn key_of(v: &Value) -> ResKey {
    ResKey::new(obj::group(v), obj::kind(v), obj::namespace(v), obj::name(v))
}

fn version_of(api_version: &str) -> String {
    api_version.rsplit('/').next().unwrap_or("").to_owned()
}

/// Joins an app's managed objects and the live objects the owner listed (any kinds, any number
/// of them; those the app doesn't own are left out) into its tree.
pub fn build(managed: Vec<Managed>, listed: Vec<Value>) -> Tree {
    let mut nodes: Vec<Node> = Vec::with_capacity(managed.len());
    let mut objects: Vec<Option<Value>> = Vec::with_capacity(managed.len());
    // Every uid in the tree, and where its node is.
    let mut at: FxHashMap<String, usize> = FxHashMap::default();
    for m in managed {
        let live = m.live;
        let node = managed_node(&m.key, m.api_version.as_deref(), live.as_ref());
        let node = Node {
            sync: Some(m.sync),
            health: m.health.map(|h| h.as_str().to_owned()),
            message: m.message.filter(|s| !s.is_empty()),
            hook: m.hook,
            missing: m.missing,
            requires_pruning: m.requires_pruning,
            why: m.why,
            tracked_by: m.tracked_by,
            shared_with: m.shared_with,
            marks_only: m.marks_only,
            ..node
        };
        // Two managed objects can't share a uid; if a list served one twice, the first stands.
        if let Some(u) = &node.uid {
            if at.contains_key(u) {
                continue;
            }
            at.insert(u.clone(), nodes.len());
        }
        nodes.push(node);
        objects.push(live);
    }

    // What each uid owns, among the listed objects not already in the tree.
    let mut owns: FxHashMap<&str, Vec<usize>> = FxHashMap::default();
    for (i, o) in listed.iter().enumerate() {
        let Some(u) = uid(o) else { continue };
        if at.contains_key(u) {
            continue;
        }
        let mut seen_owner: FxHashSet<&str> = FxHashSet::default();
        for owner in owner_uids(o) {
            if owner != u && seen_owner.insert(owner) {
                owns.entry(owner).or_default().push(i);
            }
        }
    }
    let order = |i: &usize| listed.get(*i).map(|o| (obj::kind(o), obj::name(o), uid(o)));
    for kids in owns.values_mut() {
        kids.sort_by(|a, b| order(a).cmp(&order(b)));
    }

    // Breadth first from the managed objects: each child is reached once, however many owners
    // or cycles lead to it.
    let mut truncated = false;
    let mut queue: VecDeque<String> = nodes.iter().filter_map(|n| n.uid.clone()).collect();
    let mut taken: Vec<usize> = Vec::new();
    let mut taken_set: FxHashSet<usize> = FxHashSet::default();
    'walk: while let Some(u) = queue.pop_front() {
        let Some(kids) = owns.get(u.as_str()) else { continue };
        for &i in kids {
            let Some(child_uid) = listed.get(i).and_then(uid) else { continue };
            if at.contains_key(child_uid) || !taken_set.insert(i) {
                continue;
            }
            if nodes.len() + taken.len() >= MAX_NODES {
                truncated = true;
                break 'walk;
            }
            at.insert(child_uid.to_owned(), usize::MAX);
            taken.push(i);
            queue.push_back(child_uid.to_owned());
        }
    }
    let mut listed: Vec<Option<Value>> = listed.into_iter().map(Some).collect();
    for i in taken {
        let Some(o) = listed.get_mut(i).and_then(Option::take) else { continue };
        let n = child_node(&o);
        if let Some(u) = &n.uid {
            at.insert(u.clone(), nodes.len());
        }
        nodes.push(n);
        objects.push(Some(o));
    }

    // Owners in the tree, then cut whatever closes a cycle.
    for (n, o) in nodes.iter_mut().zip(&objects) {
        let Some(o) = o else { continue };
        let own = n.uid.clone();
        let mut parents: Vec<String> = Vec::new();
        for p in owner_uids(o) {
            if Some(p) != own.as_deref() && at.contains_key(p) && !parents.iter().any(|x| x == p) {
                parents.push(p.to_owned());
            }
        }
        n.parents = parents;
    }
    cut_cycles(&mut nodes, &at);
    Tree { nodes, truncated }
}

/// Drops the owner edges that close cycles, keeping every node reachable: a depth-first walk
/// from each node in order (managed objects first) drops an edge back into the walk's own path.
fn cut_cycles(nodes: &mut [Node], at: &FxHashMap<String, usize>) {
    let mut kids: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for (c, node) in nodes.iter().enumerate() {
        for p in &node.parents {
            if let Some(list) = at.get(p).and_then(|pi| kids.get_mut(*pi)) {
                list.push(c);
            }
        }
    }
    // 0: not seen, 1: on the path, 2: done.
    let mut state = vec![0u8; nodes.len()];
    let mut cut: Vec<(usize, usize)> = Vec::new();
    // Roots first, so a cycle is cut where it closes back, not where it is entered.
    let roots = nodes.iter().enumerate().filter(|(_, x)| x.parents.is_empty()).map(|(i, _)| i);
    let rest = nodes.iter().enumerate().filter(|(_, x)| !x.parents.is_empty()).map(|(i, _)| i);
    let order: Vec<usize> = roots.chain(rest).collect();
    for start in order {
        match state.get_mut(start) {
            Some(s) if *s == 0 => *s = 1,
            _ => continue,
        }
        let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
        while let Some(top) = stack.last_mut() {
            let (node, next) = *top;
            match kids.get(node).and_then(|k| k.get(next)).copied() {
                Some(c) => {
                    top.1 += 1;
                    match state.get_mut(c) {
                        Some(s) if *s == 0 => {
                            *s = 1;
                            stack.push((c, 0));
                        }
                        Some(s) if *s == 1 => cut.push((node, c)),
                        _ => {}
                    }
                }
                None => {
                    if let Some(s) = state.get_mut(node) {
                        *s = 2;
                    }
                    stack.pop();
                }
            }
        }
    }
    for (p, c) in cut {
        let Some(pu) = nodes.get(p).and_then(|x| x.uid.clone()) else { continue };
        if let Some(child) = nodes.get_mut(c) {
            child.parents.retain(|x| *x != pu);
        }
    }
}

fn base(key: &ResKey, version: String, live: Option<&Value>) -> Node {
    let uid = live.and_then(uid).map(str::to_owned);
    Node {
        id: uid.clone().unwrap_or_else(|| key.tracking()),
        uid,
        group: key.group.to_string(),
        version,
        kind: key.kind.to_string(),
        namespace: key.namespace.to_string(),
        name: key.name.to_string(),
        parents: Vec::new(),
        managed: false,
        health: None,
        message: None,
        sync: None,
        hook: false,
        missing: false,
        requires_pruning: false,
        why: None,
        tracked_by: None,
        shared_with: Vec::new(),
        marks_only: false,
        created_at: None,
        info: Vec::new(),
        status: None,
        ready: None,
        restarts: None,
        images: Vec::new(),
        containers: Vec::new(),
        node: None,
    }
}

fn managed_node(key: &ResKey, api_version: Option<&str>, live: Option<&Value>) -> Node {
    let version = live.and_then(|l| str_at(l, &["apiVersion"])).or(api_version).map(version_of).unwrap_or_default();
    let mut n = base(key, version, live);
    n.managed = true;
    if let Some(l) = live {
        describe(&mut n, l);
    }
    n
}

fn child_node(o: &Value) -> Node {
    let mut n = base(&key_of(o), str_at(o, &["apiVersion"]).map(version_of).unwrap_or_default(), Some(o));
    if let Some(s) = health::assess(o, &BuiltIn) {
        n.health = Some(s.health.as_str().to_owned());
        n.message = Some(s.message).filter(|m| !m.is_empty());
    }
    describe(&mut n, o);
    n
}

fn info(n: &mut Node, name: &str, value: impl Into<String>) {
    let value = value.into();
    if !value.is_empty() {
        n.info.push(Info { name: name.to_owned(), value });
    }
}

fn int(o: &Value, path: &[&str]) -> i64 {
    i64_at(o, path).unwrap_or(0)
}

/// Fills in what people read off a live object.
fn describe(n: &mut Node, o: &Value) {
    n.created_at = str_at(o, &["metadata", "creationTimestamp"]).map(str::to_owned);
    match (n.group.as_str(), n.kind.as_str()) {
        ("", "Pod") => pod(n, o),
        ("apps", "Deployment") => {
            let want = i64_at(o, &["spec", "replicas"]).unwrap_or(1);
            n.ready = Some(format!("{}/{want}", int(o, &["status", "readyReplicas"])));
            info(n, "Ready", n.ready.clone().unwrap_or_default());
            info(n, "Up to date", int(o, &["status", "updatedReplicas"]).to_string());
            info(n, "Available", int(o, &["status", "availableReplicas"]).to_string());
            info(n, "Strategy", str_at(o, &["spec", "strategy", "type"]).unwrap_or("RollingUpdate"));
            template(n, o);
        }
        ("apps", "StatefulSet") => {
            let want = i64_at(o, &["spec", "replicas"]).unwrap_or(1);
            n.ready = Some(format!("{}/{want}", int(o, &["status", "readyReplicas"])));
            info(n, "Ready", n.ready.clone().unwrap_or_default());
            info(n, "Revision", str_at(o, &["status", "currentRevision"]).unwrap_or(""));
            template(n, o);
        }
        ("apps", "DaemonSet") => {
            n.ready = Some(format!("{}/{}", int(o, &["status", "numberReady"]), int(o, &["status", "desiredNumberScheduled"])));
            info(n, "Ready", n.ready.clone().unwrap_or_default());
            info(n, "Up to date", int(o, &["status", "updatedNumberScheduled"]).to_string());
            template(n, o);
        }
        ("apps", "ReplicaSet") => {
            let want = i64_at(o, &["spec", "replicas"]).unwrap_or(1);
            n.ready = Some(format!("{}/{want}", int(o, &["status", "readyReplicas"])));
            if let Some(r) = obj::annotation(o, "deployment.kubernetes.io/revision") {
                info(n, "Revision", r);
            }
            info(n, "Ready", n.ready.clone().unwrap_or_default());
            template(n, o);
        }
        ("apps", "ControllerRevision") => info(n, "Revision", i64_at(o, &["revision"]).map(|r| r.to_string()).unwrap_or_default()),
        ("batch", "Job") => {
            let want = i64_at(o, &["spec", "completions"]).unwrap_or(1);
            n.ready = Some(format!("{}/{want}", int(o, &["status", "succeeded"])));
            info(n, "Completions", n.ready.clone().unwrap_or_default());
            if int(o, &["status", "active"]) > 0 {
                info(n, "Active", int(o, &["status", "active"]).to_string());
            }
            if int(o, &["status", "failed"]) > 0 {
                info(n, "Failed", int(o, &["status", "failed"]).to_string());
            }
            if let (Some(s), Some(e)) = (str_at(o, &["status", "startTime"]), str_at(o, &["status", "completionTime"])) {
                info(n, "Duration", duration(s, e).unwrap_or_default());
            }
            template(n, o);
        }
        ("batch", "CronJob") => {
            info(n, "Schedule", str_at(o, &["spec", "schedule"]).unwrap_or(""));
            if bool_at(o, &["spec", "suspend"]).unwrap_or(false) {
                info(n, "Suspended", "yes");
            }
            info(n, "Last run", str_at(o, &["status", "lastScheduleTime"]).unwrap_or(""));
            let active = list_at(o, &["status", "active"]).map_or(0, Vec::len);
            if active > 0 {
                info(n, "Active", active.to_string());
            }
            if let Some(spec) = obj::get(o, &["spec", "jobTemplate"]) {
                template(n, spec);
            }
        }
        ("", "Service") => {
            info(n, "Type", str_at(o, &["spec", "type"]).unwrap_or("ClusterIP"));
            info(n, "Cluster IP", str_at(o, &["spec", "clusterIP"]).unwrap_or(""));
            let ports: Vec<String> = list_at(o, &["spec", "ports"])
                .into_iter()
                .flatten()
                .map(|p| {
                    let port = obj::get(p, &["port"]).map(plain).unwrap_or_default();
                    let proto = str_at(p, &["protocol"]).unwrap_or("TCP");
                    match obj::get(p, &["targetPort"]).map(plain) {
                        Some(t) if t != port => format!("{port}→{t}/{proto}"),
                        _ => format!("{port}/{proto}"),
                    }
                })
                .collect();
            info(n, "Ports", ports.join(", "));
            let ingress: Vec<String> = list_at(o, &["status", "loadBalancer", "ingress"])
                .into_iter()
                .flatten()
                .filter_map(|i| str_at(i, &["ip"]).or_else(|| str_at(i, &["hostname"])).map(str::to_owned))
                .chain(list_at(o, &["spec", "externalIPs"]).into_iter().flatten().filter_map(|v| v.as_str().map(str::to_owned)))
                .collect();
            info(n, "External", ingress.join(", "));
        }
        ("networking.k8s.io" | "extensions", "Ingress") => {
            let hosts: Vec<&str> = list_at(o, &["spec", "rules"]).into_iter().flatten().filter_map(|r| str_at(r, &["host"])).collect();
            info(n, "Hosts", hosts.join(", "));
            let addr: Vec<&str> = list_at(o, &["status", "loadBalancer", "ingress"])
                .into_iter()
                .flatten()
                .filter_map(|i| str_at(i, &["ip"]).or_else(|| str_at(i, &["hostname"])))
                .collect();
            info(n, "Address", addr.join(", "));
        }
        ("discovery.k8s.io", "EndpointSlice") => {
            let eps = list_at(o, &["endpoints"]).map(Vec::as_slice).unwrap_or_default();
            let ready = eps.iter().filter(|e| bool_at(e, &["conditions", "ready"]).unwrap_or(true)).count();
            n.ready = Some(format!("{ready}/{}", eps.len()));
            info(n, "Ready", n.ready.clone().unwrap_or_default());
            let ports: Vec<String> = list_at(o, &["ports"])
                .into_iter()
                .flatten()
                .map(|p| format!("{}/{}", obj::get(p, &["port"]).map(plain).unwrap_or_default(), str_at(p, &["protocol"]).unwrap_or("TCP")))
                .collect();
            info(n, "Ports", ports.join(", "));
            info(n, "Addresses", str_at(o, &["addressType"]).unwrap_or(""));
        }
        ("", "PersistentVolumeClaim") => {
            info(n, "Phase", str_at(o, &["status", "phase"]).unwrap_or(""));
            info(n, "Capacity", str_at(o, &["status", "capacity", "storage"]).or_else(|| str_at(o, &["spec", "resources", "requests", "storage"])).unwrap_or(""));
            info(n, "Class", str_at(o, &["spec", "storageClassName"]).unwrap_or(""));
            let modes: Vec<&str> = list_at(o, &["spec", "accessModes"]).into_iter().flatten().filter_map(Value::as_str).map(access_mode).collect();
            info(n, "Access", modes.join(", "));
        }
        ("", "ConfigMap") => {
            let keys = obj::map_at(o, &["data"]).map_or(0, |m| m.len()) + obj::map_at(o, &["binaryData"]).map_or(0, |m| m.len());
            info(n, "Keys", keys.to_string());
        }
        // Secrets are read as metadata alone: their type and keys are said only when known.
        ("", "Secret") => {
            info(n, "Type", str_at(o, &["type"]).unwrap_or(""));
            if let Some(d) = obj::map_at(o, &["data"]) {
                info(n, "Keys", d.len().to_string());
            }
        }
        ("autoscaling", "HorizontalPodAutoscaler") => {
            let (cur, want) = (int(o, &["status", "currentReplicas"]), int(o, &["status", "desiredReplicas"]));
            n.ready = Some(format!("{cur}/{want}"));
            info(n, "Replicas", format!("{cur} (min {}, max {})", i64_at(o, &["spec", "minReplicas"]).unwrap_or(1), int(o, &["spec", "maxReplicas"])));
        }
        ("", "Namespace") => info(n, "Phase", str_at(o, &["status", "phase"]).unwrap_or("")),
        _ => {
            if let Some(c) = list_at(o, &["status", "conditions"]).into_iter().flatten().find(|c| str_at(c, &["type"]) == Some("Ready")) {
                info(n, "Ready", str_at(c, &["status"]).unwrap_or(""));
            }
        }
    }
}

fn access_mode(m: &str) -> &str {
    match m {
        "ReadWriteOnce" => "RWO",
        "ReadOnlyMany" => "ROX",
        "ReadWriteMany" => "RWX",
        "ReadWriteOncePod" => "RWOP",
        other => other,
    }
}

fn plain(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Seconds between two RFC 3339 times, as `1m 12s`.
fn duration(start: &str, end: &str) -> Option<String> {
    let secs = rfc3339_secs(end)? - rfc3339_secs(start)?;
    if secs < 0 {
        return None;
    }
    Some(match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {}s", s / 60, s % 60),
        s => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    })
}

/// Seconds since the epoch of `YYYY-MM-DDTHH:MM:SSZ` (what the API server writes).
pub fn rfc3339_secs(t: &str) -> Option<i64> {
    let b = t.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| t.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, s) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?, num(17..19)?);
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + s)
}

/// A workload's images and containers, from its pod template.
fn template(n: &mut Node, o: &Value) {
    let spec = ["spec", "template", "spec"];
    let mut containers = Vec::new();
    for (init, field) in [(true, "initContainers"), (false, "containers")] {
        let mut path = spec.to_vec();
        path.push(field);
        for c in list_at(o, &path).into_iter().flatten() {
            containers.push(Container {
                name: str_at(c, &["name"]).unwrap_or("").to_owned(),
                image: str_at(c, &["image"]).unwrap_or("").to_owned(),
                init,
                ready: None,
                restarts: None,
                state: None,
                reason: None,
                last: None,
            });
        }
    }
    n.images = images(&containers);
    n.containers = containers;
}

fn images(containers: &[Container]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in containers.iter().filter(|c| !c.init) {
        if !c.image.is_empty() && !out.contains(&c.image) {
            out.push(c.image.clone());
        }
    }
    out
}

fn pod(n: &mut Node, o: &Value) {
    let statuses = |field: &str| -> FxHashMap<String, Value> {
        list_at(o, &["status", field])
            .into_iter()
            .flatten()
            .filter_map(|s| Some((str_at(s, &["name"])?.to_owned(), s.clone())))
            .collect()
    };
    let (init_st, main_st) = (statuses("initContainerStatuses"), statuses("containerStatuses"));
    let mut containers = Vec::new();
    for (init, field, st) in [(true, "initContainers", &init_st), (false, "containers", &main_st)] {
        for c in list_at(o, &["spec", field]).into_iter().flatten() {
            let name = str_at(c, &["name"]).unwrap_or("").to_owned();
            let s = st.get(&name);
            let (state, reason) = match s.and_then(|s| s.get("state")) {
                Some(state) if state.get("running").is_some() => (Some("running"), None),
                Some(state) if state.get("waiting").is_some() => (Some("waiting"), str_at(state, &["waiting", "reason"])),
                Some(state) if state.get("terminated").is_some() => (Some("terminated"), str_at(state, &["terminated", "reason"])),
                _ => (None, None),
            };
            containers.push(Container {
                image: s.and_then(|s| str_at(s, &["image"])).or_else(|| str_at(c, &["image"])).unwrap_or("").to_owned(),
                name,
                init,
                ready: s.and_then(|s| bool_at(s, &["ready"])),
                restarts: s.and_then(|s| i64_at(s, &["restartCount"])),
                state: state.map(str::to_owned),
                reason: reason.map(str::to_owned),
                last: s.and_then(|s| str_at(s, &["lastState", "terminated", "reason"])).map(str::to_owned),
            });
        }
    }
    let main: Vec<&Container> = containers.iter().filter(|c| !c.init).collect();
    let ready = main.iter().filter(|c| c.ready == Some(true)).count();
    n.ready = Some(format!("{ready}/{}", main.len()));
    n.restarts = Some(main.iter().filter_map(|c| c.restarts).sum());
    n.status = Some(pod_status(o));
    n.node = str_at(o, &["spec", "nodeName"]).filter(|s| !s.is_empty()).map(str::to_owned);
    n.images = images(&containers);
    info(n, "Status", n.status.clone().unwrap_or_default());
    info(n, "Ready", n.ready.clone().unwrap_or_default());
    info(n, "Restarts", n.restarts.unwrap_or(0).to_string());
    info(n, "Node", n.node.clone().unwrap_or_default());
    info(n, "IP", str_at(o, &["status", "podIP"]).unwrap_or(""));
    n.containers = containers;
}

/// A pod's state as `kubectl get pods` words it.
pub fn pod_status(o: &Value) -> String {
    let mut reason = str_at(o, &["status", "reason"]).or_else(|| str_at(o, &["status", "phase"])).unwrap_or("Unknown").to_owned();
    let inits = list_at(o, &["status", "initContainerStatuses"]).map(Vec::as_slice).unwrap_or_default();
    let init_count = list_at(o, &["spec", "initContainers"]).map_or(0, Vec::len);
    let mut initializing = false;
    for (i, c) in inits.iter().enumerate() {
        let st = c.get("state");
        let terminated = st.and_then(|s| s.get("terminated"));
        let waiting = st.and_then(|s| s.get("waiting"));
        if let Some(t) = terminated {
            if int(t, &["exitCode"]) == 0 {
                continue;
            }
            reason = match str_at(t, &["reason"]) {
                Some(r) if !r.is_empty() => format!("Init:{r}"),
                _ if int(t, &["signal"]) != 0 => format!("Init:Signal:{}", int(t, &["signal"])),
                _ => format!("Init:ExitCode:{}", int(t, &["exitCode"])),
            };
        } else if let Some(r) = waiting.and_then(|w| str_at(w, &["reason"])).filter(|r| !r.is_empty() && *r != "PodInitializing") {
            reason = format!("Init:{r}");
        } else {
            reason = format!("Init:{i}/{}", init_count.max(inits.len()));
        }
        initializing = true;
        break;
    }
    if !initializing {
        let mut running = false;
        for c in list_at(o, &["status", "containerStatuses"]).into_iter().flatten().rev() {
            let st = c.get("state");
            if let Some(r) = st.and_then(|s| str_at(s, &["waiting", "reason"])).filter(|r| !r.is_empty()) {
                reason = r.to_owned();
            } else if let Some(t) = st.and_then(|s| s.get("terminated")) {
                reason = match str_at(t, &["reason"]) {
                    Some(r) if !r.is_empty() => r.to_owned(),
                    _ if int(t, &["signal"]) != 0 => format!("Signal:{}", int(t, &["signal"])),
                    _ => format!("ExitCode:{}", int(t, &["exitCode"])),
                };
            } else if st.is_some_and(|s| s.get("running").is_some()) && bool_at(c, &["ready"]).unwrap_or(false) {
                running = true;
            }
        }
        if reason == "Completed" && running {
            let ready = list_at(o, &["status", "conditions"])
                .into_iter()
                .flatten()
                .any(|c| str_at(c, &["type"]) == Some("Ready") && str_at(c, &["status"]) == Some("True"));
            reason = if ready { "Running".into() } else { "NotReady".into() };
        }
    }
    if obj::get(o, &["metadata", "deletionTimestamp"]).is_some_and(|t| !t.is_null()) {
        reason = if str_at(o, &["status", "reason"]) == Some("NodeLost") { "Unknown".into() } else { "Terminating".into() };
    }
    reason
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn managed(group: &str, kind: &str, ns: &str, name: &str, live: Option<Value>) -> Managed {
        Managed { health: Some(Health::Healthy), live, ..Managed::new(ResKey::new(group, kind, ns, name)) }
    }

    fn object(api: &str, kind: &str, name: &str, uid: &str, owners: &[&str]) -> Value {
        let refs: Vec<Value> = owners.iter().map(|o| json!({"apiVersion": "v1", "kind": "X", "name": "x", "uid": o})).collect();
        json!({"apiVersion": api, "kind": kind, "metadata": {"name": name, "namespace": "web", "uid": uid, "ownerReferences": refs,
            "creationTimestamp": "2026-10-17T10:00:00Z"}})
    }

    fn pod(name: &str, uid: &str, owner: &str, ready: bool, restarts: i64, waiting: Option<&str>) -> Value {
        let state = match waiting {
            Some(r) => json!({"waiting": {"reason": r}}),
            None => json!({"running": {"startedAt": "2026-10-17T10:00:00Z"}}),
        };
        json!({"apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": name, "namespace": "web", "uid": uid, "ownerReferences": [{"apiVersion": "apps/v1", "kind": "ReplicaSet", "name": "rs", "uid": owner}]},
            "spec": {"nodeName": "node-1", "restartPolicy": "Always", "containers": [{"name": "app", "image": "nginx:1.27"}, {"name": "side", "image": "envoy:1"}]},
            "status": {"phase": "Running", "podIP": "10.0.0.7", "conditions": [{"type": "Ready", "status": if ready { "True" } else { "False" }}],
                "containerStatuses": [
                    {"name": "app", "image": "nginx:1.27", "ready": ready, "restartCount": restarts, "state": state},
                    {"name": "side", "image": "envoy:1", "ready": true, "restartCount": 0, "state": {"running": {}}}]}})
    }

    fn names(t: &Tree) -> Vec<&str> {
        t.nodes.iter().map(|n| n.name.as_str()).collect()
    }

    fn node<'a>(t: &'a Tree, name: &str) -> Option<&'a Node> {
        t.nodes.iter().find(|n| n.name == name)
    }

    fn parents_of<'a>(t: &'a Tree, name: &str) -> Vec<&'a str> {
        let Some(n) = node(t, name) else { return vec!["(no such node)"] };
        n.parents.iter().filter_map(|p| t.nodes.iter().find(|x| x.uid.as_deref() == Some(p.as_str()))).map(|x| x.name.as_str()).collect()
    }

    #[test]
    fn a_deployment_its_replica_set_and_pods() {
        let d = object("apps/v1", "Deployment", "api", "d1", &[]);
        let rs = object("apps/v1", "ReplicaSet", "api-7d9f", "rs1", &["d1"]);
        let tree = build(
            vec![managed("apps", "Deployment", "web", "api", Some(d))],
            vec![pod("api-7d9f-b", "p2", "rs1", true, 0, None), rs, pod("api-7d9f-a", "p1", "rs1", false, 3, Some("CrashLoopBackOff"))],
        );
        assert_eq!(names(&tree), ["api", "api-7d9f", "api-7d9f-a", "api-7d9f-b"]);
        assert_eq!(node(&tree, "api").map(|n| n.managed), Some(true));
        assert_eq!(node(&tree, "api-7d9f").map(|n| n.managed), Some(false));
        assert_eq!(parents_of(&tree, "api-7d9f"), ["api"]);
        assert_eq!(parents_of(&tree, "api-7d9f-a"), ["api-7d9f"]);
        let crash = node(&tree, "api-7d9f-a");
        assert_eq!(crash.and_then(|n| n.status.as_deref()), Some("CrashLoopBackOff"));
        assert_eq!(crash.and_then(|n| n.ready.as_deref()), Some("1/2"));
        assert_eq!(crash.and_then(|n| n.restarts), Some(3));
        assert_eq!(crash.and_then(|n| n.health.as_deref()), Some("Degraded"));
        assert_eq!(crash.map(|n| n.images.clone()), Some(vec!["nginx:1.27".to_owned(), "envoy:1".to_owned()]));
        assert_eq!(crash.and_then(|n| n.node.as_deref()), Some("node-1"));
        let fine = node(&tree, "api-7d9f-b");
        assert_eq!(fine.and_then(|n| n.status.as_deref()), Some("Running"));
        assert_eq!(fine.and_then(|n| n.health.as_deref()), Some("Healthy"));
        assert_eq!(node(&tree, "api").and_then(|n| n.created_at.as_deref()), Some("2026-10-17T10:00:00Z"));
        assert!(!tree.truncated);
    }

    #[test]
    fn chains_through_every_kind_listed() {
        // CronJob → Job → Pod, StatefulSet → Pod and ControllerRevision, Service → EndpointSlice.
        let cj = object("batch/v1", "CronJob", "nightly", "cj", &[]);
        let sts = object("apps/v1", "StatefulSet", "db", "sts", &[]);
        let svc = object("v1", "Service", "api", "svc", &[]);
        let tree = build(
            vec![
                managed("batch", "CronJob", "web", "nightly", Some(cj)),
                managed("apps", "StatefulSet", "web", "db", Some(sts)),
                managed("", "Service", "web", "api", Some(svc)),
            ],
            vec![
                object("v1", "Pod", "nightly-1-x", "jp", &["job"]),
                object("batch/v1", "Job", "nightly-1", "job", &["cj"]),
                object("v1", "Pod", "db-0", "sp", &["sts"]),
                object("apps/v1", "ControllerRevision", "db-5d4", "cr", &["sts"]),
                object("discovery.k8s.io/v1", "EndpointSlice", "api-abcde", "es", &["svc"]),
            ],
        );
        assert_eq!(parents_of(&tree, "nightly-1"), ["nightly"]);
        assert_eq!(parents_of(&tree, "nightly-1-x"), ["nightly-1"]);
        assert_eq!(parents_of(&tree, "db-0"), ["db"]);
        assert_eq!(parents_of(&tree, "db-5d4"), ["db"]);
        assert_eq!(parents_of(&tree, "api-abcde"), ["api"]);
        assert_eq!(tree.nodes.len(), 8);
        // Owners come before what they own.
        let pos = |n: &str| tree.nodes.iter().position(|x| x.name == n);
        assert!(pos("nightly-1") < pos("nightly-1-x"));
        assert_eq!(node(&tree, "api-abcde").map(|n| n.version.as_str()), Some("v1"));
    }

    #[test]
    fn many_children_each_once_in_order() {
        let d = object("apps/v1", "Deployment", "api", "d1", &[]);
        let mut listed = vec![object("apps/v1", "ReplicaSet", "api-1", "rs1", &["d1"])];
        for i in (0..300).rev() {
            listed.push(pod(&format!("api-1-{i:03}"), &format!("p{i}"), "rs1", true, 0, None));
        }
        // The same pod listed twice (two lists overlapped) counts once.
        listed.push(pod("api-1-000", "p0", "rs1", true, 0, None));
        let tree = build(vec![managed("apps", "Deployment", "web", "api", Some(d))], listed);
        assert_eq!(tree.nodes.len(), 302);
        assert_eq!(tree.nodes.get(2).map(|n| n.name.as_str()), Some("api-1-000"));
        assert_eq!(tree.nodes.last().map(|n| n.name.as_str()), Some("api-1-299"));
        assert!(tree.nodes.iter().skip(2).all(|n| n.parents == ["rs1"]));
    }

    #[test]
    fn a_missing_owner_leaves_its_objects_out() {
        // The ReplicaSet these pods name was deleted (or not listed): they don't belong to the app.
        let d = object("apps/v1", "Deployment", "api", "d1", &[]);
        let tree = build(
            vec![managed("apps", "Deployment", "web", "api", Some(d)), managed("", "ConfigMap", "web", "cfg", None)],
            vec![object("v1", "Pod", "orphan", "p1", &["gone"]), object("v1", "Pod", "loose", "p2", &[])],
        );
        assert_eq!(names(&tree), ["api", "cfg"]);
        // A managed object not read from the cluster is still a node, keyed by its key.
        assert_eq!(node(&tree, "cfg").map(|n| (n.uid.clone(), n.id.as_str())), Some((None, "/ConfigMap:web/cfg")));
    }

    #[test]
    fn objects_owned_outside_the_app_stay_out() {
        let d = object("apps/v1", "Deployment", "api", "d1", &[]);
        let tree = build(
            vec![managed("apps", "Deployment", "web", "api", Some(d))],
            vec![
                object("apps/v1", "ReplicaSet", "other-1", "rs-other", &["d-other"]),
                object("v1", "Pod", "other-1-a", "po", &["rs-other"]),
                // Owned by the app and by something else: in, with only the owner in the tree.
                object("v1", "ConfigMap", "shared", "cm", &["d1", "d-other"]),
            ],
        );
        assert_eq!(names(&tree), ["api", "shared"]);
        assert_eq!(parents_of(&tree, "shared"), ["api"]);
    }

    #[test]
    fn cycles_end_and_every_node_keeps_a_way_in() {
        let d = object("apps/v1", "Deployment", "api", "d1", &[]);
        let tree = build(
            vec![managed("apps", "Deployment", "web", "api", Some(d))],
            vec![
                // a ↔ b, reached from the app; c owns itself; x ↔ y, reached from nothing.
                object("v1", "Pod", "a", "a", &["d1", "b"]),
                object("v1", "Pod", "b", "b", &["a"]),
                object("v1", "Pod", "c", "c", &["c", "b"]),
                object("v1", "Pod", "x", "x", &["y"]),
                object("v1", "Pod", "y", "y", &["x"]),
            ],
        );
        assert_eq!(names(&tree), ["api", "a", "b", "c"]);
        assert_eq!(parents_of(&tree, "a"), ["api"]);
        assert_eq!(parents_of(&tree, "b"), ["a"]);
        assert_eq!(parents_of(&tree, "c"), ["b"]);
        // No node is its own ancestor.
        for n in &tree.nodes {
            let mut seen = FxHashSet::default();
            let mut todo: Vec<&str> = n.parents.iter().map(String::as_str).collect();
            while let Some(p) = todo.pop() {
                assert_ne!(Some(p), n.uid.as_deref(), "{} is its own ancestor", n.name);
                if seen.insert(p)
                    && let Some(up) = tree.nodes.iter().find(|x| x.uid.as_deref() == Some(p))
                {
                    todo.extend(up.parents.iter().map(String::as_str));
                }
            }
        }
    }

    #[test]
    fn managed_objects_owning_each_other_keep_one_root() {
        let a = object("apps/v1", "Deployment", "a", "ua", &["ub"]);
        let b = object("v1", "ConfigMap", "b", "ub", &["ua"]);
        let tree = build(vec![managed("apps", "Deployment", "web", "a", Some(a)), managed("", "ConfigMap", "web", "b", Some(b))], vec![]);
        let roots: Vec<&str> = tree.nodes.iter().filter(|n| n.parents.is_empty()).map(|n| n.name.as_str()).collect();
        assert_eq!(roots, ["a"]);
        assert_eq!(parents_of(&tree, "b"), ["a"]);
    }

    #[test]
    fn past_the_limit_children_are_cut() {
        let d = object("apps/v1", "Deployment", "api", "d1", &[]);
        let listed: Vec<Value> = (0..MAX_NODES + 10).map(|i| object("v1", "Pod", &format!("p{i:05}"), &format!("u{i}"), &["d1"])).collect();
        let tree = build(vec![managed("apps", "Deployment", "web", "api", Some(d))], listed);
        assert_eq!(tree.nodes.len(), MAX_NODES);
        assert!(tree.truncated);
    }

    #[test]
    fn pod_states_read_as_kubectl_reads_them() {
        let p = |status: Value| json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "p"}, "spec": {"initContainers": [{"name": "i"}], "containers": [{"name": "c"}]}, "status": status});
        assert_eq!(pod_status(&p(json!({"phase": "Pending", "initContainerStatuses": [{"name": "i", "state": {"waiting": {"reason": "PodInitializing"}}}]}))), "Init:0/1");
        assert_eq!(pod_status(&p(json!({"phase": "Pending", "initContainerStatuses": [{"name": "i", "state": {"terminated": {"exitCode": 1, "reason": "Error"}}}]}))), "Init:Error");
        assert_eq!(pod_status(&p(json!({"phase": "Succeeded", "containerStatuses": [{"name": "c", "state": {"terminated": {"exitCode": 0, "reason": "Completed"}}}]}))), "Completed");
        assert_eq!(pod_status(&p(json!({"phase": "Running", "containerStatuses": [{"name": "c", "state": {"terminated": {"exitCode": 137, "reason": "OOMKilled"}}}]}))), "OOMKilled");
        assert_eq!(pod_status(&p(json!({"phase": "Pending", "containerStatuses": [{"name": "c", "state": {"waiting": {"reason": "ImagePullBackOff"}}}]}))), "ImagePullBackOff");
        let mut gone = p(json!({"phase": "Running"}));
        if let Some(m) = gone.pointer_mut("/metadata").and_then(Value::as_object_mut) {
            m.insert("deletionTimestamp".into(), json!("2026-10-17T10:00:00Z"));
        }
        assert_eq!(pod_status(&gone), "Terminating");
    }

    #[test]
    fn workloads_say_what_they_run() {
        let d = json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "api", "namespace": "web", "uid": "d"},
            "spec": {"replicas": 3, "template": {"spec": {"initContainers": [{"name": "migrate", "image": "tool:1"}], "containers": [{"name": "app", "image": "api:2"}]}}},
            "status": {"readyReplicas": 2, "updatedReplicas": 3, "availableReplicas": 2}});
        let svc = json!({"apiVersion": "v1", "kind": "Service", "metadata": {"name": "api", "namespace": "web", "uid": "s"},
            "spec": {"type": "ClusterIP", "clusterIP": "10.96.0.9", "ports": [{"port": 80, "targetPort": 8080, "protocol": "TCP"}]}});
        let tree = build(vec![managed("apps", "Deployment", "web", "api", Some(d)), managed("", "Service", "web", "api", Some(svc))], vec![]);
        let d = tree.nodes.first();
        assert_eq!(d.and_then(|n| n.ready.as_deref()), Some("2/3"));
        assert_eq!(d.map(|n| n.images.clone()), Some(vec!["api:2".to_owned()]));
        assert_eq!(d.map(|n| n.containers.iter().map(|c| (c.name.as_str(), c.init)).collect::<Vec<_>>()), Some(vec![("migrate", true), ("app", false)]));
        let ports = tree.nodes.get(1).and_then(|n| n.info.iter().find(|i| i.name == "Ports")).map(|i| i.value.as_str());
        assert_eq!(ports, Some("80→8080/TCP"));
    }

    #[test]
    fn a_secret_read_as_metadata_claims_no_type() {
        let meta = json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "tls", "namespace": "web", "uid": "s"}});
        let full = json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "cfg", "namespace": "web", "uid": "t"},
            "type": "kubernetes.io/tls", "data": {"tls.crt": "eA==", "tls.key": "eQ=="}});
        let tree = build(vec![managed("", "Secret", "web", "tls", Some(meta)), managed("", "Secret", "web", "cfg", Some(full))], vec![]);
        assert_eq!(node(&tree, "tls").map(|n| n.info.len()), Some(0));
        let said: Vec<(String, String)> = node(&tree, "cfg").map(|n| n.info.iter().map(|i| (i.name.clone(), i.value.clone())).collect()).unwrap_or_default();
        assert_eq!(said, [("Type".to_owned(), "kubernetes.io/tls".to_owned()), ("Keys".to_owned(), "2".to_owned())]);
    }

    #[test]
    fn times() {
        assert_eq!(rfc3339_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_secs("2026-10-17T10:00:00Z"), Some(1_792_231_200));
        assert_eq!(duration("2026-10-17T10:00:00Z", "2026-10-17T10:01:12Z").as_deref(), Some("1m 12s"));
    }
}
