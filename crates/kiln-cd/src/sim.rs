//! A simulated Kubernetes cluster, for tests and benchmarks.
//!
//! It keeps objects, answers server-side applies the way the API server does in the respects
//! the engine depends on (resource versions, generations, `managedFields` naming what the
//! applier owns, defaults the applier never set), sends watch events, and runs controllers that
//! roll Deployments, StatefulSets and DaemonSets out over simulated time and complete Jobs.
//! Faults are injectable: applies that fail, edits by someone else (drift).
//!
//! It is not an API server. It is the smallest cluster on which every rule the engine follows
//! (waves wait for health, hooks complete, self-heal undoes drift) can be watched happening, at
//! the speed of memory.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::{Map, Value, json};

use crate::obj::{self, GroupKind, Interner, ResKey, Scopes, Str};

/// What changed in the cluster, for its watchers.
#[derive(Clone, Debug)]
pub enum Event {
    /// An object was created or changed.
    Upsert(Value),
    /// An object was deleted.
    Delete(ResKey),
}

struct Object {
    /// Kept compressed: a benchmark's cluster holds millions of these.
    value: crate::obj::Blob,
    /// When its controller finishes rolling out the current generation.
    ready_at_ms: u64,
}

impl Object {
    fn value(&self) -> Value {
        self.value.decode().unwrap_or(Value::Null)
    }
}

/// A simulated cluster.
pub struct Cluster {
    /// Its name.
    pub name: Str,
    /// How long workloads take to roll out.
    pub rollout_ms: u64,
    /// How long Jobs take to complete.
    pub job_ms: u64,
    /// Object names whose applies fail.
    pub fail_names: FxHashSet<String>,
    /// Jobs whose names are here fail instead of completing.
    pub failing_jobs: FxHashSet<String>,
    /// Refuse objects in namespaces that don't exist, as the API server does.
    pub strict_namespaces: bool,
    objects: FxHashMap<ResKey, Object>,
    rv: u64,
    uid: u64,
    due: BinaryHeap<Reverse<(u64, u64, ResKey)>>,
    seq: u64,
    scopes: Scopes,
    interner: Interner,
    /// Requests answered.
    pub requests: u64,
}

impl Cluster {
    /// An empty cluster.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            rollout_ms: 3_000,
            job_ms: 2_000,
            fail_names: FxHashSet::default(),
            failing_jobs: FxHashSet::default(),
            strict_namespaces: false,
            objects: FxHashMap::default(),
            rv: 1,
            uid: 1,
            due: BinaryHeap::new(),
            seq: 0,
            scopes: Scopes::default(),
            interner: Interner::default(),
            requests: 0,
        }
    }

    /// How many objects it holds.
    pub fn len(&self) -> usize {
        self.objects.len()
    }

    /// Whether it holds nothing.
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    fn key(&mut self, v: &Value) -> Option<ResKey> {
        obj::key_of(v, &mut self.interner, "default", &self.scopes)
    }

    /// One object.
    pub fn get(&self, key: &ResKey) -> Option<Value> {
        self.objects.get(key).map(Object::value)
    }

    /// Every object of a kind.
    pub fn list(&self, kind: &GroupKind) -> Vec<Value> {
        self.objects
            .iter()
            .filter(|(k, _)| k.group == kind.group && k.kind == kind.kind)
            .map(|(_, o)| o.value())
            .collect()
    }

    /// Server-side apply of `desired` as `manager`. Returns the object as stored (or as it
    /// would be, for a dry run).
    pub fn apply(&mut self, now: u64, mut desired: Value, manager: &str, dry_run: bool) -> Result<(Value, Option<Event>), String> {
        self.requests += 1;
        let key = self.key(&desired).ok_or("object has no kind or name")?;
        if self.fail_names.contains(key.name.as_ref()) {
            return Err(format!("admission webhook denied {key}"));
        }
        if self.strict_namespaces && !key.namespace.is_empty() && !self.objects.contains_key(&ResKey::new("", "Namespace", "", &key.namespace)) {
            return Err(format!("HTTP 404 NotFound: namespaces \"{}\" not found", key.namespace));
        }
        if let Some(m) = desired.as_object_mut() {
            m.remove("status");
        }
        let fields = fields_of(&desired);
        let previous = self.objects.get(&key).map(Object::value);
        let mut next = match &previous {
            Some(prev) => merge_apply(prev, &desired),
            None => desired.clone(),
        };
        default_object(&mut next);
        // An apply that changes nothing is not a new version (the API server's rule too).
        if let Some(prev) = &previous
            && comparable(prev) == comparable(&next) {
                return Ok((prev.clone(), None));
            }
        let spec_changed = previous.as_ref().is_none_or(|p| p.get("spec") != next.get("spec"));
        let generation = previous.as_ref().and_then(|p| obj::i64_at(p, &["metadata", "generation"])).unwrap_or(0)
            + i64::from(spec_changed);
        let uid = previous
            .as_ref()
            .and_then(|p| obj::str_at(p, &["metadata", "uid"]).map(str::to_owned))
            .unwrap_or_else(|| format!("uid-{}", self.uid));
        if dry_run {
            return Ok((next, None));
        }
        if previous.is_none() {
            self.uid += 1;
        }
        self.rv += 1;
        if let Some(meta) = obj::ensure_map(&mut next, &["metadata"]) {
            meta.insert("uid".into(), Value::String(uid));
            meta.insert("resourceVersion".into(), Value::String(self.rv.to_string()));
            meta.insert("generation".into(), json!(generation.max(1)));
            meta.entry("creationTimestamp").or_insert_with(|| json!("2026-10-16T00:00:00Z"));
            let mut managed: Vec<Value> = previous
                .as_ref()
                .and_then(|p| obj::list_at(p, &["metadata", "managedFields"]).cloned())
                .unwrap_or_default()
                .into_iter()
                .filter(|e| e.get("manager").and_then(Value::as_str) != Some(manager))
                .collect();
            managed.push(json!({"manager": manager, "operation": "Apply", "apiVersion": obj::str_at(&desired, &["apiVersion"]).unwrap_or(""), "fieldsType": "FieldsV1", "fieldsV1": fields}));
            meta.insert("managedFields".into(), Value::Array(managed));
        }
        let ready_at = if spec_changed { now + self.rollout_ms_for(&key) } else { self.objects.get(&key).map(|o| o.ready_at_ms).unwrap_or(now) };
        start_status(&mut next, &key, previous.as_ref());
        if spec_changed && has_controller(&key.kind) {
            self.seq += 1;
            self.due.push(Reverse((ready_at, self.seq, key.clone())));
        }
        self.objects.insert(key, Object { value: crate::obj::Blob::encode(&next), ready_at_ms: ready_at });
        Ok((next.clone(), Some(Event::Upsert(next))))
    }

    fn rollout_ms_for(&self, key: &ResKey) -> u64 {
        match key.kind.as_ref() {
            "Deployment" | "StatefulSet" | "DaemonSet" => self.rollout_ms,
            "Job" => self.job_ms,
            _ => 0,
        }
    }

    /// Deletes an object.
    pub fn delete(&mut self, key: &ResKey) -> Result<Option<Event>, String> {
        self.requests += 1;
        match self.objects.remove(key) {
            Some(_) => {
                self.rv += 1;
                Ok(Some(Event::Delete(key.clone())))
            }
            None => Err(format!("{key} not found")),
        }
    }

    /// Someone else changes an object (drift): `f` edits it; the change is a new version.
    pub fn edit(&mut self, key: &ResKey, f: impl FnOnce(&mut Value)) -> Option<Event> {
        let o = self.objects.get_mut(key)?;
        let mut v = o.value();
        f(&mut v);
        self.rv += 1;
        if let Some(meta) = obj::ensure_map(&mut v, &["metadata"]) {
            meta.insert("resourceVersion".into(), Value::String(self.rv.to_string()));
        }
        o.value = crate::obj::Blob::encode(&v);
        Some(Event::Upsert(v))
    }

    /// Changes only an object's status, as controllers do all day (no new generation).
    pub fn touch_status(&mut self, key: &ResKey, f: impl FnOnce(&mut Map<String, Value>)) -> Option<Event> {
        let o = self.objects.get_mut(key)?;
        let mut v = o.value();
        if let Some(status) = obj::ensure_map(&mut v, &["status"]) {
            f(status);
        }
        self.rv += 1;
        if let Some(meta) = obj::ensure_map(&mut v, &["metadata"]) {
            meta.insert("resourceVersion".into(), Value::String(self.rv.to_string()));
        }
        o.value = crate::obj::Blob::encode(&v);
        Some(Event::Upsert(v))
    }

    /// The next time a controller has something to do.
    pub fn next_due(&self) -> Option<u64> {
        self.due.peek().map(|Reverse((t, _, _))| *t)
    }

    /// Runs the controllers up to `now`.
    pub fn advance(&mut self, now: u64, out: &mut Vec<Event>) {
        while let Some(Reverse((t, _, key))) = self.due.peek().cloned() {
            if t > now {
                break;
            }
            self.due.pop();
            let failing = self.failing_jobs.contains(key.name.as_ref());
            let Some(o) = self.objects.get_mut(&key) else { continue };
            if o.ready_at_ms > t {
                continue;
            }
            let mut v = o.value();
            finish_status(&mut v, &key, failing);
            self.rv += 1;
            if let Some(meta) = obj::ensure_map(&mut v, &["metadata"]) {
                meta.insert("resourceVersion".into(), Value::String(self.rv.to_string()));
            }
            o.value = crate::obj::Blob::encode(&v);
            out.push(Event::Upsert(v));
        }
    }
}

fn has_controller(kind: &str) -> bool {
    matches!(kind, "Deployment" | "StatefulSet" | "DaemonSet" | "Job")
}

/// An object without what the server keeps for itself.
fn comparable(v: &Value) -> Value {
    let mut c = v.clone();
    if let Some(m) = c.as_object_mut() {
        m.remove("status");
    }
    if let Some(meta) = c.get_mut("metadata").and_then(Value::as_object_mut) {
        for k in ["resourceVersion", "generation", "uid", "creationTimestamp", "managedFields"] {
            meta.remove(k);
        }
    }
    c
}

/// The fields an apply owns, as `fieldsV1`: lists of named objects by key, other lists whole.
fn fields_of(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, child) in m {
                if child.is_null() {
                    continue;
                }
                let inner = match child {
                    Value::Array(items) if k == "ports" && items.iter().all(|i| i.get("containerPort").is_some()) => {
                        let mut lm = Map::new();
                        for item in items {
                            let key = json!({"containerPort": item.get("containerPort"), "protocol": item.get("protocol").and_then(Value::as_str).unwrap_or("TCP")});
                            let mut f = fields_of(item);
                            if let Some(fm) = f.as_object_mut() {
                                fm.insert(".".into(), json!({}));
                            }
                            lm.insert(format!("k:{key}"), f);
                        }
                        Value::Object(lm)
                    }
                    Value::Array(items) if !items.is_empty() && items.iter().all(|i| i.get("name").and_then(Value::as_str).is_some()) => {
                        let mut lm = Map::new();
                        for item in items {
                            let key = json!({"name": item.get("name")});
                            let mut f = fields_of(item);
                            if let Some(fm) = f.as_object_mut() {
                                fm.insert(".".into(), json!({}));
                            }
                            lm.insert(format!("k:{key}"), f);
                        }
                        Value::Object(lm)
                    }
                    Value::Object(cm) if !cm.is_empty() => fields_of(child),
                    _ => json!({}),
                };
                out.insert(format!("f:{k}"), inner);
            }
            Value::Object(out)
        }
        _ => json!({}),
    }
}

/// An apply over an existing object: what the applier sets replaces what it set before; what
/// others set stays. (The real rule is per field; per top-level section is enough here.)
fn merge_apply(prev: &Value, desired: &Value) -> Value {
    let mut next = prev.clone();
    let (Some(n), Some(d)) = (next.as_object_mut(), desired.as_object()) else { return desired.clone() };
    for (k, v) in d {
        if k == "metadata" {
            let Some(meta) = n.entry("metadata").or_insert_with(|| json!({})).as_object_mut() else { continue };
            if let Some(dm) = v.as_object() {
                for (mk, mv) in dm {
                    if mk == "labels" || mk == "annotations" {
                        let slot = meta.entry(mk.clone()).or_insert_with(|| json!({}));
                        if let (Some(slot), Some(src)) = (slot.as_object_mut(), mv.as_object()) {
                            for (lk, lv) in src {
                                slot.insert(lk.clone(), lv.clone());
                            }
                        }
                    } else {
                        meta.insert(mk.clone(), mv.clone());
                    }
                }
            }
        } else if k != "status" {
            n.insert(k.clone(), v.clone());
        }
    }
    next
}

/// Defaults the API server adds that no applier set.
fn default_object(v: &mut Value) {
    let kind = obj::kind(v).to_owned();
    if kind == "Deployment" {
        if let Some(spec) = obj::ensure_map(v, &["spec"]) {
            spec.entry("progressDeadlineSeconds").or_insert(json!(600));
            spec.entry("revisionHistoryLimit").or_insert(json!(10));
            spec.entry("replicas").or_insert(json!(1));
            spec.entry("strategy").or_insert(json!({"type": "RollingUpdate", "rollingUpdate": {"maxSurge": "25%", "maxUnavailable": "25%"}}));
        }
        if let Some(containers) = v.pointer_mut("/spec/template/spec/containers").and_then(Value::as_array_mut) {
            for c in containers {
                if let Some(c) = c.as_object_mut() {
                    c.entry("imagePullPolicy").or_insert(json!("IfNotPresent"));
                    c.entry("terminationMessagePath").or_insert(json!("/dev/termination-log"));
                    if let Some(ports) = c.get_mut("ports").and_then(Value::as_array_mut) {
                        for p in ports {
                            if let Some(p) = p.as_object_mut() {
                                p.entry("protocol").or_insert(json!("TCP"));
                            }
                        }
                    }
                }
            }
        }
    }
    if kind == "Service" {
        if let Some(spec) = obj::ensure_map(v, &["spec"]) {
            spec.entry("type").or_insert(json!("ClusterIP"));
            spec.entry("clusterIP").or_insert(json!("10.0.0.1"));
            spec.entry("sessionAffinity").or_insert(json!("None"));
        }
        if let Some(ports) = v.pointer_mut("/spec/ports").and_then(Value::as_array_mut) {
            for p in ports {
                if let Some(p) = p.as_object_mut() {
                    p.entry("protocol").or_insert(json!("TCP"));
                    if let Some(port) = p.get("port").cloned() {
                        p.entry("targetPort").or_insert(port);
                    }
                }
            }
        }
    }
}

fn replicas(v: &Value) -> i64 {
    obj::i64_at(v, &["spec", "replicas"]).unwrap_or(1)
}

/// The status right after an apply: a workload whose spec moved is rolling out.
fn start_status(v: &mut Value, key: &ResKey, previous: Option<&Value>) {
    let n = replicas(v);
    let generation = obj::i64_at(v, &["metadata", "generation"]).unwrap_or(1);
    let kept = previous.and_then(|p| p.get("status").cloned());
    let status = match key.kind.as_ref() {
        "Deployment" => {
            let mut s = kept.unwrap_or_else(|| json!({}));
            if let Some(m) = s.as_object_mut() {
                m.insert("replicas".into(), json!(n));
                m.insert("updatedReplicas".into(), json!(0));
                m.insert("availableReplicas".into(), json!(m.get("availableReplicas").and_then(Value::as_i64).unwrap_or(0)));
                m.insert("observedGeneration".into(), json!(generation - 1));
            }
            s
        }
        "StatefulSet" => json!({"observedGeneration": generation - 1, "replicas": n, "readyReplicas": 0}),
        "DaemonSet" => json!({"observedGeneration": generation - 1, "desiredNumberScheduled": 3, "updatedNumberScheduled": 0, "numberAvailable": 0}),
        "Job" => json!({"active": 1}),
        "PersistentVolumeClaim" => json!({"phase": "Bound"}),
        _ => return,
    };
    if let Some(m) = v.as_object_mut() {
        m.insert("status".into(), status);
    }
}

/// The status once the controller is done.
fn finish_status(v: &mut Value, key: &ResKey, failing: bool) {
    let n = replicas(v);
    let generation = obj::i64_at(v, &["metadata", "generation"]).unwrap_or(1);
    let status = match key.kind.as_ref() {
        "Deployment" => json!({"observedGeneration": generation, "replicas": n, "updatedReplicas": n, "readyReplicas": n, "availableReplicas": n,
            "conditions": [{"type": "Available", "status": "True"}, {"type": "Progressing", "status": "True", "reason": "NewReplicaSetAvailable"}]}),
        "StatefulSet" => json!({"observedGeneration": generation, "replicas": n, "readyReplicas": n, "currentReplicas": n, "updatedReplicas": n,
            "currentRevision": "r1", "updateRevision": "r1"}),
        "DaemonSet" => json!({"observedGeneration": generation, "desiredNumberScheduled": 3, "updatedNumberScheduled": 3, "numberAvailable": 3}),
        "Job" if failing => json!({"failed": 1, "conditions": [{"type": "Failed", "status": "True", "message": "BackoffLimitExceeded"}]}),
        "Job" => json!({"succeeded": 1, "conditions": [{"type": "Complete", "status": "True"}]}),
        _ => return,
    };
    if let Some(m) = v.as_object_mut() {
        m.insert("status".into(), status);
    }
}
