//! Kubernetes objects as the engine holds them.
//!
//! An object is identified by its group, kind, namespace and name ([`ResKey`]). Its version is
//! not part of its identity: the API server serves one object at every version it knows. Objects
//! arrive as YAML or JSON and are worked on as [`serde_json::Value`]; between uses they are kept
//! as compressed JSON ([`Blob`]), because a fleet of a million apps keeps ten million of them and
//! JSON text shrinks several times over.
//!
//! [`hash_value`] is structural: a value hashes the same whatever order its object keys arrived
//! in, so equal hashes mean two objects say the same thing, however they were spelled. The engine
//! compares hashes first and objects only when a hash moved.

use std::borrow::Borrow;
use std::fmt;
use std::hash::Hash;
use std::sync::Arc;

use rustc_hash::FxHashSet;
use serde::Deserialize as _;
use serde_json::{Map, Value};
use xxhash_rust::xxh3::Xxh3;

/// A shared, immutable string. Keys repeat their group, kind and namespace millions of times, so
/// those are interned ([`Interner`]) and the clones are pointer copies.
pub type Str = Arc<str>;

/// An object's group and kind: what identifies its type across versions.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct GroupKind {
    /// API group; empty for the core group (`v1`).
    pub group: Str,
    /// Kind, as in the object's `kind` field.
    pub kind: Str,
}

impl GroupKind {
    /// A group and kind from string slices (no interning).
    pub fn new(group: &str, kind: &str) -> Self {
        Self { group: group.into(), kind: kind.into() }
    }
}

impl fmt::Display for GroupKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.group.is_empty() { f.write_str(&self.kind) } else { write!(f, "{}/{}", self.group, self.kind) }
    }
}

/// An object's identity in a cluster.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ResKey {
    /// API group; empty for the core group.
    pub group: Str,
    /// Kind.
    pub kind: Str,
    /// Namespace; empty for cluster-scoped objects.
    pub namespace: Str,
    /// Name.
    pub name: Str,
}

impl ResKey {
    /// A key from string slices (no interning); tests and one-off lookups.
    pub fn new(group: &str, kind: &str, namespace: &str, name: &str) -> Self {
        Self { group: group.into(), kind: kind.into(), namespace: namespace.into(), name: name.into() }
    }

    /// The key's group and kind.
    pub fn group_kind(&self) -> GroupKind {
        GroupKind { group: self.group.clone(), kind: self.kind.clone() }
    }

    /// The key in the form Argo CD's tracking id uses after the app name:
    /// `group/kind:namespace/name`.
    pub fn tracking(&self) -> String {
        format!("{}/{}:{}/{}", self.group, self.kind, self.namespace, self.name)
    }
}

impl fmt::Display for ResKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.tracking())
    }
}

/// Interns strings so that repeated groups, kinds and namespaces share one allocation.
#[derive(Default)]
pub struct Interner {
    set: FxHashSet<Str>,
}

impl Interner {
    /// The shared copy of `s`, made on first use.
    pub fn intern(&mut self, s: &str) -> Str {
        if let Some(found) = self.set.get(s) {
            return found.clone();
        }
        let owned: Str = s.into();
        self.set.insert(owned.clone());
        owned
    }

    /// How many distinct strings are held.
    pub fn len(&self) -> usize {
        self.set.len()
    }

    /// Whether nothing is held.
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }
}

/// Looks up `key` in a set of shared strings without allocating.
pub fn lookup<'a, T: Borrow<str> + Hash + Eq>(set: &'a FxHashSet<T>, key: &str) -> Option<&'a T> {
    set.get(key)
}

/// The group of an `apiVersion`: `apps` for `apps/v1`, empty for `v1`.
pub fn api_group(api_version: &str) -> &str {
    match api_version.rsplit_once('/') {
        Some((group, _)) => group,
        None => "",
    }
}

/// The value at `path` under `v`, following object keys only.
pub fn get<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cur = v;
    for key in path {
        cur = cur.as_object()?.get(*key)?;
    }
    Some(cur)
}

/// The string at `path`, if it is one.
pub fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    get(v, path)?.as_str()
}

/// The integer at `path`, if it is one (integral floats count).
pub fn i64_at(v: &Value, path: &[&str]) -> Option<i64> {
    as_i64(get(v, path)?)
}

/// An integer from a JSON number, accepting integral floats (YAML and Helm turn integers into
/// floats freely).
pub fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| {
            let f = n.as_f64()?;
            (f.fract() == 0.0 && f.abs() < 9.0e15).then_some(f as i64)
        }),
        _ => None,
    }
}

/// The boolean at `path`, if it is one.
pub fn bool_at(v: &Value, path: &[&str]) -> Option<bool> {
    get(v, path)?.as_bool()
}

/// The object at `path`, if it is one.
pub fn map_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Map<String, Value>> {
    get(v, path)?.as_object()
}

/// The list at `path`, if it is one.
pub fn list_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Vec<Value>> {
    get(v, path)?.as_array()
}

/// `metadata.name`, or empty.
pub fn name(v: &Value) -> &str {
    str_at(v, &["metadata", "name"]).unwrap_or("")
}

/// `metadata.namespace`, or empty.
pub fn namespace(v: &Value) -> &str {
    str_at(v, &["metadata", "namespace"]).unwrap_or("")
}

/// `kind`, or empty.
pub fn kind(v: &Value) -> &str {
    str_at(v, &["kind"]).unwrap_or("")
}

/// The group of the object's `apiVersion`.
pub fn group(v: &Value) -> &str {
    api_group(str_at(v, &["apiVersion"]).unwrap_or(""))
}

/// One annotation's value.
pub fn annotation<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    map_at(v, &["metadata", "annotations"])?.get(key)?.as_str()
}

/// One label's value.
pub fn label<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    map_at(v, &["metadata", "labels"])?.get(key)?.as_str()
}

/// The mutable object at `path`, made (as empty objects) where it is missing. `None` when
/// something on the way is not an object.
pub fn ensure_map<'a>(v: &'a mut Value, path: &[&str]) -> Option<&'a mut Map<String, Value>> {
    let mut cur = v;
    for key in path {
        let map = cur.as_object_mut()?;
        cur = map.entry((*key).to_owned()).or_insert_with(|| Value::Object(Map::new()));
    }
    cur.as_object_mut()
}

/// Removes the value at `path`, returning it.
pub fn remove(v: &mut Value, path: &[&str]) -> Option<Value> {
    let (last, parents) = path.split_last()?;
    let mut cur = v;
    for key in parents {
        cur = cur.as_object_mut()?.get_mut(*key)?;
    }
    cur.as_object_mut()?.remove(*last)
}

/// Kinds that are cluster-scoped in every Kubernetes release kiln supports. Custom resources'
/// scope comes from discovery ([`Scopes`]).
const CLUSTER_SCOPED: &[(&str, &str)] = &[
    ("", "Namespace"),
    ("", "Node"),
    ("", "PersistentVolume"),
    ("", "ComponentStatus"),
    ("rbac.authorization.k8s.io", "ClusterRole"),
    ("rbac.authorization.k8s.io", "ClusterRoleBinding"),
    ("apiextensions.k8s.io", "CustomResourceDefinition"),
    ("apiregistration.k8s.io", "APIService"),
    ("storage.k8s.io", "StorageClass"),
    ("storage.k8s.io", "CSIDriver"),
    ("storage.k8s.io", "CSINode"),
    ("storage.k8s.io", "VolumeAttachment"),
    ("scheduling.k8s.io", "PriorityClass"),
    ("node.k8s.io", "RuntimeClass"),
    ("networking.k8s.io", "IngressClass"),
    ("admissionregistration.k8s.io", "ValidatingWebhookConfiguration"),
    ("admissionregistration.k8s.io", "MutatingWebhookConfiguration"),
    ("admissionregistration.k8s.io", "ValidatingAdmissionPolicy"),
    ("admissionregistration.k8s.io", "ValidatingAdmissionPolicyBinding"),
    ("certificates.k8s.io", "CertificateSigningRequest"),
    ("flowcontrol.apiserver.k8s.io", "FlowSchema"),
    ("flowcontrol.apiserver.k8s.io", "PriorityLevelConfiguration"),
    ("policy", "PodSecurityPolicy"),
];

/// Which kinds a cluster serves cluster-scoped, from discovery. Kinds discovery hasn't reported
/// fall back to the built-in list.
#[derive(Default, Clone)]
pub struct Scopes {
    cluster: FxHashSet<GroupKind>,
    namespaced: FxHashSet<GroupKind>,
}

impl Scopes {
    /// Records what discovery said of one kind.
    pub fn set(&mut self, gk: GroupKind, namespaced: bool) {
        if namespaced {
            self.cluster.remove(&gk);
            self.namespaced.insert(gk);
        } else {
            self.namespaced.remove(&gk);
            self.cluster.insert(gk);
        }
    }

    /// Whether objects of this kind live in namespaces.
    pub fn namespaced(&self, group: &str, kind: &str) -> bool {
        let gk = GroupKind::new(group, kind);
        if self.cluster.contains(&gk) {
            return false;
        }
        if self.namespaced.contains(&gk) {
            return true;
        }
        !CLUSTER_SCOPED.iter().any(|(g, k)| *g == group && *k == kind)
    }
}

/// The object's key, interned. Namespaced objects without a namespace get `default_ns` (the app's
/// destination namespace); cluster-scoped objects never have one.
pub fn key_of(v: &Value, interner: &mut Interner, default_ns: &str, scopes: &Scopes) -> Option<ResKey> {
    let kind = kind(v);
    let name = name(v);
    if kind.is_empty() || name.is_empty() {
        return None;
    }
    let group = group(v);
    let ns = if scopes.namespaced(group, kind) {
        let ns = namespace(v);
        if ns.is_empty() { default_ns } else { ns }
    } else {
        ""
    };
    Some(ResKey {
        group: interner.intern(group),
        kind: interner.intern(kind),
        namespace: interner.intern(ns),
        name: name.into(),
    })
}

/// Parses YAML or JSON text holding any number of documents. Empty documents are skipped, and
/// `List` objects are flattened into their items, as `kubectl apply` and Argo CD do.
pub fn parse_documents(text: &str) -> anyhow::Result<Vec<Value>> {
    let mut out = Vec::new();
    for doc in serde_yaml::Deserializer::from_str(text) {
        let v = Value::deserialize(doc)?;
        flatten_into(v, &mut out);
    }
    Ok(out)
}

fn flatten_into(v: Value, out: &mut Vec<Value>) {
    match v {
        Value::Null => {}
        Value::Object(mut m) => {
            let is_list = m.get("kind").and_then(Value::as_str).is_some_and(|k| k.ends_with("List"))
                && m.get("items").is_some_and(Value::is_array);
            if is_list {
                if let Some(Value::Array(items)) = m.remove("items") {
                    for item in items {
                        flatten_into(item, out);
                    }
                }
            } else {
                out.push(Value::Object(m));
            }
        }
        other => out.push(other),
    }
}

/// A structural hash of a value: object keys may come in any order.
pub fn hash_value(v: &Value) -> u64 {
    let mut h = Xxh3::new();
    feed(&mut h, v);
    h.digest()
}

fn feed(h: &mut Xxh3, v: &Value) {
    match v {
        Value::Null => h.update(&[0]),
        Value::Bool(b) => h.update(&[1, u8::from(*b)]),
        Value::Number(n) => {
            if let Some(i) = as_i64(v) {
                h.update(&[2]);
                h.update(&i.to_le_bytes());
            } else if let Some(u) = n.as_u64() {
                h.update(&[2]);
                h.update(&u.to_le_bytes());
            } else {
                h.update(&[6]);
                h.update(&n.as_f64().unwrap_or(0.0).to_bits().to_le_bytes());
            }
        }
        Value::String(s) => {
            h.update(&[3]);
            h.update(&(s.len() as u64).to_le_bytes());
            h.update(s.as_bytes());
        }
        Value::Array(items) => {
            h.update(&[4]);
            h.update(&(items.len() as u64).to_le_bytes());
            for item in items {
                feed(h, item);
            }
        }
        Value::Object(m) => {
            let (acc, n) = m.iter().fold((0u64, 0u64), |(acc, n), (k, item)| (acc.wrapping_add(entry_hash(k, hash_value(item))), n + 1));
            finish_object(h, acc, n);
        }
    }
}

/// One object entry's share of its object's hash. Entries combine by wrapping sum, so their
/// order doesn't matter, and a hash that leaves some entries out ([`hash_object_without`]) is
/// the hash of the object without them.
pub fn entry_hash(key: &str, value_hash: u64) -> u64 {
    let mut e = Xxh3::new();
    e.update(key.as_bytes());
    e.update(&[0xff]);
    e.update(&value_hash.to_le_bytes());
    e.digest()
}

fn finish_object(h: &mut Xxh3, acc: u64, n: u64) {
    h.update(&[5]);
    h.update(&n.to_le_bytes());
    h.update(&acc.to_le_bytes());
}

/// The hash `hash_value` would give the object with the entries `skip` names removed, and with
/// `inner` (an entry's key and the keys to drop inside it) removed one level down; computed
/// without building that object.
pub fn hash_object_without(m: &Map<String, Value>, skip: &[&str], inner: (&str, &[&str])) -> u64 {
    let (inner_key, inner_skip) = inner;
    let mut acc = 0u64;
    let mut n = 0u64;
    for (k, item) in m {
        if skip.contains(&k.as_str()) {
            continue;
        }
        let vh = match item {
            Value::Object(im) if k == inner_key => {
                let mut h = Xxh3::new();
                let (iacc, inn) = im
                    .iter()
                    .filter(|(ik, _)| !inner_skip.contains(&ik.as_str()))
                    .fold((0u64, 0u64), |(acc, n), (ik, iv)| (acc.wrapping_add(entry_hash(ik, hash_value(iv))), n + 1));
                finish_object(&mut h, iacc, inn);
                h.digest()
            }
            other => hash_value(other),
        };
        acc = acc.wrapping_add(entry_hash(k, vh));
        n += 1;
    }
    let mut h = Xxh3::new();
    finish_object(&mut h, acc, n);
    h.digest()
}

/// A 64-bit hash of bytes (render keys, ids).
pub fn hash_bytes(bytes: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(bytes)
}

/// An object kept compressed: LZ4 over its JSON text.
#[derive(Clone)]
pub struct Blob(Arc<[u8]>);

impl Blob {
    /// Compresses a value.
    pub fn encode(v: &Value) -> Self {
        let json = serde_json::to_vec(v).unwrap_or_default();
        Self(lz4_flex::compress_prepend_size(&json).into())
    }

    /// The value back.
    pub fn decode(&self) -> anyhow::Result<Value> {
        let raw = lz4_flex::decompress_size_prepended(&self.0)?;
        Ok(serde_json::from_slice(&raw)?)
    }

    /// Compressed size in bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the blob holds nothing.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Blob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Blob({} bytes)", self.0.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hashes_ignore_key_order_but_not_values() {
        let a = json!({"a": 1, "b": {"c": [1, 2], "d": "x"}});
        let b: Value = serde_json::from_str(r#"{"b": {"d": "x", "c": [1, 2]}, "a": 1}"#).unwrap_or_default();
        assert_eq!(hash_value(&a), hash_value(&b));
        assert_ne!(hash_value(&a), hash_value(&json!({"a": 1, "b": {"c": [2, 1], "d": "x"}})));
        assert_ne!(hash_value(&json!({"a": "1"})), hash_value(&json!({"a": 1})));
        assert_eq!(hash_value(&json!({"a": 1.0})), hash_value(&json!({"a": 1})));
    }

    #[test]
    fn documents_skip_empties_and_flatten_lists() -> anyhow::Result<()> {
        let docs = parse_documents(
            "---\napiVersion: v1\nkind: ConfigMap\nmetadata: {name: a}\n---\n---\napiVersion: v1\nkind: List\nitems:\n- {apiVersion: v1, kind: Secret, metadata: {name: b}}\n- {apiVersion: v1, kind: Service, metadata: {name: c}}\n",
        )?;
        let names: Vec<&str> = docs.iter().map(name).collect();
        assert_eq!(names, ["a", "b", "c"]);
        Ok(())
    }

    #[test]
    fn keys_take_the_default_namespace_only_when_namespaced() {
        let mut i = Interner::default();
        let s = Scopes::default();
        let cm = json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "a"}});
        let ns = json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "web"}});
        let crb = json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRoleBinding", "metadata": {"name": "x", "namespace": "ignored"}});
        assert_eq!(key_of(&cm, &mut i, "web", &s).map(|k| k.tracking()), Some("/ConfigMap:web/a".to_owned()));
        assert_eq!(key_of(&ns, &mut i, "web", &s).map(|k| k.tracking()), Some("/Namespace:/web".to_owned()));
        assert_eq!(
            key_of(&crb, &mut i, "web", &s).map(|k| k.tracking()),
            Some("rbac.authorization.k8s.io/ClusterRoleBinding:/x".to_owned())
        );
    }

    #[test]
    fn blobs_round_trip_and_shrink() -> anyhow::Result<()> {
        let v = json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web", "labels": {"app": "web", "tier": "frontend"}},
            "spec": {"replicas": 3, "template": {"spec": {"containers": [{"name": "web", "image": "ghcr.io/hyper-light/web:1.2.3", "ports": [{"containerPort": 8080}]}]}}}});
        let b = Blob::encode(&v);
        assert_eq!(b.decode()?, v);
        Ok(())
    }
}
