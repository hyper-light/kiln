//! Comparing what git says with what runs.
//!
//! An object is in sync when every field kiln sets has its value in the live object, and kiln
//! owns no field it no longer sets. "Owns" is the API server's own record: the field set of
//! kiln's server-side apply in `metadata.managedFields`. For objects applied the client-side way
//! (adopted from Argo CD or `kubectl apply`), the fields of their `last-applied-configuration`
//! stand in for it. Fields other managers own (an HPA's `replicas`, a webhook's sidecar) are not
//! kiln's, and differ freely unless kiln sets them too.
//!
//! Values compare by meaning: `1` and `1.0`, `"500m"` and `"0.5"` in resource quantities, an
//! empty value and an absent one. List items are matched by their merge keys (containers by
//! name, ports by port and protocol) where Kubernetes has them, and by position where it doesn't.
//!
//! [`normalize_desired`] and [`LiveView`] prepare the two sides once; [`compare`] is then a walk
//! over the desired object, so its cost is the size of what kiln sets, not of what runs.

use base64::Engine as _;
use serde_json::{Map, Value};

use crate::model::IgnoreRule;
use crate::obj::{self, hash_value};

/// Where and how an object differs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// Live matches desired.
    Synced,
    /// A field differs.
    OutOfSync {
        /// The first difference, as a path (`.spec.template.spec.containers[name=web].image`).
        path: String,
        /// What kind of difference.
        why: Why,
    },
}

impl Verdict {
    /// Whether the object is in sync.
    pub fn synced(&self) -> bool {
        matches!(self, Verdict::Synced)
    }
}

/// What kind of difference.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    /// A field kiln sets is missing from the live object.
    Missing,
    /// A field kiln sets has another value live.
    Differs,
    /// kiln owns a field it no longer sets: the next apply removes it.
    Removed,
}

/// The annotation `kubectl apply` keeps the applied object in.
pub const LAST_APPLIED: &str = "kubectl.kubernetes.io/last-applied-configuration";

/// Fields the server owns, removed from both sides.
const SERVER_METADATA: &[&str] = &[
    "uid",
    "resourceVersion",
    "generation",
    "creationTimestamp",
    "managedFields",
    "selfLink",
    "deletionTimestamp",
    "deletionGracePeriodSeconds",
];

/// Prepares a rendered object for comparison and for applying: drops `status` and the fields the
/// server sets, drops nulls (an explicit null means "not set"), and folds a Secret's `stringData`
/// into `data`, as the API server does on write.
pub fn normalize_desired(obj: &mut Value) {
    if let Some(m) = obj.as_object_mut() {
        m.remove("status");
    }
    if let Some(meta) = obj.get_mut("metadata").and_then(Value::as_object_mut) {
        for k in SERVER_METADATA {
            meta.remove(*k);
        }
    }
    drop_nulls(obj);
    if obj::kind(obj) == "Secret" && obj::group(obj).is_empty() {
        fold_string_data(obj);
    }
}

fn drop_nulls(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.retain(|_, item| !item.is_null());
            for item in m.values_mut() {
                drop_nulls(item);
            }
        }
        Value::Array(items) => {
            for item in items {
                drop_nulls(item);
            }
        }
        _ => {}
    }
}

fn fold_string_data(obj: &mut Value) {
    let Some(m) = obj.as_object_mut() else { return };
    let Some(Value::Object(sd)) = m.remove("stringData") else { return };
    let data = m.entry("data").or_insert_with(|| Value::Object(Map::new()));
    if let Some(data) = data.as_object_mut() {
        for (k, v) in sd {
            let text = match v {
                Value::String(s) => s,
                other => other.to_string(),
            };
            data.insert(k, Value::String(base64::engine::general_purpose::STANDARD.encode(text)));
        }
    }
}

/// A live object prepared for comparison: what it says (without status and the server's
/// bookkeeping), the field set its manager owns, and its hash.
#[derive(Clone, Debug)]
pub struct LiveView {
    /// The object without `status` and server metadata; `managedFields` kept apart.
    pub object: Value,
    /// Every manager's entry from `metadata.managedFields`.
    pub managed: Vec<Managed>,
    /// The hash of what the object says plus the field set `manager` owns: it moves when
    /// anything kiln compares moves, and not when only status or other managers' timestamps do.
    pub hash: u64,
}

/// One entry of `metadata.managedFields`.
#[derive(Clone, Debug)]
pub struct Managed {
    /// The field manager.
    pub manager: String,
    /// `Apply` or `Update`.
    pub operation: String,
    /// The subresource it wrote (`status`), or empty.
    pub subresource: String,
    /// What it owns.
    pub fields: FieldSet,
}

/// The hash [`LiveView::new`] gives an object, computed on the object as it arrived, without
/// copying it: what it says, without `status` and the server's bookkeeping, plus the field set
/// `manager` owns by apply. Equal hashes mean nothing kiln compares has moved.
pub fn live_hash(obj: &Value, manager: &str) -> u64 {
    let Some(m) = obj.as_object() else { return hash_value(obj) };
    let mut hash = obj::hash_object_without(m, &["status"], ("metadata", SERVER_METADATA));
    let own = obj::list_at(obj, &["metadata", "managedFields"]).and_then(|entries| {
        entries.iter().find(|e| {
            e.get("manager").and_then(Value::as_str) == Some(manager) && e.get("operation").and_then(Value::as_str) == Some("Apply")
        })
    });
    if let Some(fields) = own.and_then(|e| e.get("fieldsV1")) {
        hash ^= hash_value(fields).rotate_left(17);
    }
    hash
}

impl LiveView {
    /// Prepares a live object; `manager` is the field manager kiln applies as.
    pub fn new(obj: Value, manager: &str) -> LiveView {
        Self::keeping(obj, manager, |_| true)
    }

    /// Prepares a live object, parsing only the managers `keep` names (beside `manager`'s apply).
    pub fn keeping(mut obj: Value, manager: &str, keep: impl Fn(&str) -> bool) -> LiveView {
        let hash = live_hash(&obj, manager);
        let raw_managed = obj.get_mut("metadata").and_then(Value::as_object_mut).and_then(|m| m.remove("managedFields"));
        let mut managed = Vec::new();
        if let Some(Value::Array(entries)) = raw_managed {
            for e in entries {
                let get = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
                let name = e.get("manager").and_then(Value::as_str).unwrap_or("");
                let op = e.get("operation").and_then(Value::as_str).unwrap_or("");
                if !((name == manager && op == "Apply") || keep(name)) {
                    continue;
                }
                managed.push(Managed {
                    manager: get("manager"),
                    operation: get("operation"),
                    subresource: get("subresource"),
                    fields: e.get("fieldsV1").map(FieldSet::parse).unwrap_or_default(),
                });
            }
        }
        if let Some(m) = obj.as_object_mut() {
            m.remove("status");
        }
        if let Some(meta) = obj.get_mut("metadata").and_then(Value::as_object_mut) {
            for k in SERVER_METADATA {
                meta.remove(*k);
            }
        }
        LiveView { object: obj, managed, hash }
    }

    /// The field set `manager` owns by server-side apply, if it applied this object.
    pub fn applied_by(&self, manager: &str) -> Option<&FieldSet> {
        self.managed.iter().find(|m| m.manager == manager && m.operation == "Apply").map(|m| &m.fields)
    }

    /// The fields of the object's `last-applied-configuration`, if it has one.
    pub fn last_applied(&self) -> Option<FieldSet> {
        let text = obj::annotation(&self.object, LAST_APPLIED)?;
        let v: Value = serde_json::from_str(text).ok()?;
        Some(FieldSet::of_value(&v))
    }

    /// The fields kiln is responsible for: its server-side apply field set if there is one, else
    /// the last applied configuration's (client-side apply and adoption).
    pub fn owned(&self, manager: &str) -> Option<FieldSet> {
        self.applied_by(manager).cloned().or_else(|| self.last_applied())
    }
}

/// One step of a field path, as `managedFields` spells it (`f:`, `k:`, `v:`, `i:`).
#[derive(Clone, PartialEq, Debug)]
pub enum PathElem {
    /// An object field.
    Field(String),
    /// A list item, by the values of its merge keys.
    Key(Map<String, Value>),
    /// A list item of a set of scalars, by value.
    Value(Value),
    /// A list item, by position.
    Index(usize),
}

/// A set of field paths: `managedFields`' `fieldsV1`, or the fields of an object.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct FieldSet {
    /// The set owns this node itself (the `"."` entry, or a leaf).
    pub own: bool,
    /// Children.
    pub members: Vec<(PathElem, FieldSet)>,
}

impl FieldSet {
    /// Parses `fieldsV1`.
    pub fn parse(v: &Value) -> FieldSet {
        let Some(m) = v.as_object() else { return FieldSet::default() };
        let mut set = FieldSet { own: m.is_empty(), members: Vec::new() };
        for (k, child) in m {
            if k == "." {
                set.own = true;
                continue;
            }
            let elem = if let Some(f) = k.strip_prefix("f:") {
                PathElem::Field(f.to_owned())
            } else if let Some(j) = k.strip_prefix("k:") {
                match serde_json::from_str::<Value>(j) {
                    Ok(Value::Object(keys)) => PathElem::Key(keys),
                    _ => continue,
                }
            } else if let Some(j) = k.strip_prefix("v:") {
                match serde_json::from_str::<Value>(j) {
                    Ok(v) => PathElem::Value(v),
                    Err(_) => continue,
                }
            } else if let Some(i) = k.strip_prefix("i:") {
                match i.parse() {
                    Ok(i) => PathElem::Index(i),
                    Err(_) => continue,
                }
            } else {
                continue;
            };
            set.members.push((elem, FieldSet::parse(child)));
        }
        set
    }

    /// The fields an object sets, leaves owned; lists count as one atomic field (what
    /// `kubectl apply` replaces).
    pub fn of_value(v: &Value) -> FieldSet {
        match v {
            Value::Object(m) if !m.is_empty() => FieldSet {
                own: false,
                members: m
                    .iter()
                    .filter(|(_, child)| !child.is_null())
                    .map(|(k, child)| (PathElem::Field(k.clone()), FieldSet::of_value(child)))
                    .collect(),
            },
            _ => FieldSet { own: true, members: Vec::new() },
        }
    }

    /// The child for one field name.
    pub fn field(&self, name: &str) -> Option<&FieldSet> {
        self.members.iter().find_map(|(e, c)| matches!(e, PathElem::Field(f) if f == name).then_some(c))
    }

    /// A hash of the set (order-insensitive).
    pub fn hash(&self) -> u64 {
        let mut acc: u64 = u64::from(self.own);
        for (e, c) in &self.members {
            let tag = match e {
                PathElem::Field(f) => obj::hash_bytes(f.as_bytes()),
                PathElem::Key(m) => hash_value(&Value::Object(m.clone())).rotate_left(1),
                PathElem::Value(v) => hash_value(v).rotate_left(2),
                PathElem::Index(i) => (*i as u64).rotate_left(3) ^ 0x9e37_79b9_7f4a_7c15,
            };
            acc = acc.wrapping_add(tag ^ c.hash().rotate_left(7));
        }
        acc
    }
}

/// Merge keys of the lists Kubernetes merges by key, from the field's name and its parent's.
/// Each key may have a default (a port's protocol is TCP when unset).
fn merge_keys(field: &str, parent: &str) -> Option<&'static [(&'static str, Option<&'static str>)]> {
    Some(match field {
        "containers" | "initContainers" | "ephemeralContainers" | "env" | "volumes" | "imagePullSecrets" | "resourceClaims"
        | "webhooks" | "secrets" => &[("name", None)],
        "volumeMounts" => &[("mountPath", None)],
        "volumeDevices" => &[("devicePath", None)],
        "hostAliases" => &[("ip", None)],
        "topologySpreadConstraints" => &[("topologyKey", None), ("whenUnsatisfiable", None)],
        "ports" if parent == "spec" => &[("port", None), ("protocol", Some("TCP"))],
        "ports" => &[("containerPort", None), ("protocol", Some("TCP"))],
        "conditions" => &[("type", None)],
        _ => return None,
    })
}

/// Lists of scalars Kubernetes treats as sets.
fn is_set(field: &str) -> bool {
    matches!(field, "finalizers")
}

fn key_of_item(item: &Value, keys: &[(&str, Option<&str>)]) -> Option<Vec<Value>> {
    let m = item.as_object()?;
    keys.iter()
        .map(|(k, default)| match m.get(*k) {
            Some(v) => Some(v.clone()),
            None => default.map(|d| Value::String(d.to_owned())),
        })
        .collect()
}

/// Path segments under which strings are resource quantities.
const QUANTITY_CONTEXT: &[&str] = &["resources", "limits", "requests", "hard", "capacity", "overhead", "storage", "sizeLimit"];

struct Walk {
    path: Vec<String>,
}

impl Walk {
    fn render(&self) -> String {
        if self.path.is_empty() {
            return ".".into();
        }
        let mut out = String::new();
        for seg in &self.path {
            if seg.starts_with('[') {
                out.push_str(seg);
            } else {
                out.push('.');
                out.push_str(seg);
            }
        }
        out
    }

    fn out(&self, why: Why) -> Verdict {
        Verdict::OutOfSync { path: self.render(), why }
    }

    fn in_quantity_context(&self) -> bool {
        self.path.iter().any(|s| QUANTITY_CONTEXT.contains(&s.as_str()))
    }

    fn field(&self) -> &str {
        self.path.last().map(String::as_str).unwrap_or("")
    }

    fn parent(&self) -> &str {
        let n = self.path.len();
        if n < 2 { "" } else { self.path.get(n - 2).map(String::as_str).unwrap_or("") }
    }
}

/// Compares a normalized desired object with a live one. `owned` is the field set kiln is
/// responsible for (see [`LiveView::owned`]); without one only the desired fields are checked.
pub fn compare(desired: &Value, live: &Value, owned: Option<&FieldSet>) -> Verdict {
    let mut w = Walk { path: Vec::new() };
    if let Some(v) = covers(desired, Some(live), &mut w) {
        return v;
    }
    if let Some(owned) = owned {
        let mut w = Walk { path: Vec::new() };
        if let Some(v) = removed(owned, Some(desired), &mut w) {
            return v;
        }
    }
    Verdict::Synced
}

fn is_empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(b) => !*b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(m) => m.is_empty(),
    }
}

fn covers(d: &Value, l: Option<&Value>, w: &mut Walk) -> Option<Verdict> {
    let l = l.filter(|l| !l.is_null());
    match d {
        Value::Null => None,
        Value::Object(dm) => {
            if dm.is_empty() {
                return None;
            }
            let lm = match l {
                Some(Value::Object(lm)) => lm,
                None => return Some(w.out(Why::Missing)),
                Some(_) => return Some(w.out(Why::Differs)),
            };
            for (k, dv) in dm {
                let lv = lm.get(k);
                if lv.is_none() && is_empty(dv) {
                    continue;
                }
                w.path.push(k.clone());
                let r = covers(dv, lv, w);
                w.path.pop();
                if r.is_some() {
                    return r;
                }
            }
            None
        }
        Value::Array(da) => {
            let la = match l {
                Some(Value::Array(la)) => la,
                None if da.is_empty() => return None,
                None => return Some(w.out(Why::Missing)),
                Some(_) => return Some(w.out(Why::Differs)),
            };
            let field = w.field().to_owned();
            let keyed = merge_keys(&field, w.parent()).filter(|keys| da.iter().all(|di| key_of_item(di, keys).is_some()));
            if let Some(keys) = keyed {
                for di in da {
                    let Some(dk) = key_of_item(di, keys) else {
                        return Some(w.out(Why::Differs));
                    };
                    let found = la.iter().find(|li| key_of_item(li, keys).as_ref() == Some(&dk));
                    let label = keys
                        .iter()
                        .zip(&dk)
                        .map(|((k, _), v)| format!("{k}={}", v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())))
                        .collect::<Vec<_>>()
                        .join(",");
                    w.path.push(format!("[{label}]"));
                    let r = match found {
                        Some(li) => covers(di, Some(li), w),
                        None => Some(w.out(Why::Missing)),
                    };
                    w.path.pop();
                    if r.is_some() {
                        return r;
                    }
                }
                return None;
            }
            if is_set(&field) {
                for di in da {
                    if !la.iter().any(|li| scalar_eq(di, li, false)) {
                        return Some(w.out(Why::Missing));
                    }
                }
                return None;
            }
            if da.len() != la.len() {
                return Some(w.out(Why::Differs));
            }
            for (i, (di, li)) in da.iter().zip(la).enumerate() {
                w.path.push(format!("[{i}]"));
                let r = covers(di, Some(li), w);
                w.path.pop();
                if r.is_some() {
                    return r;
                }
            }
            None
        }
        scalar => match l {
            None if is_empty(scalar) => None,
            None => Some(w.out(Why::Missing)),
            Some(lv) => (!scalar_eq(scalar, lv, w.in_quantity_context())).then(|| w.out(Why::Differs)),
        },
    }
}

fn scalar_eq(a: &Value, b: &Value, quantities: bool) -> bool {
    if a == b {
        return true;
    }
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ if quantities => match (quantity(a), quantity(b)) {
            (Some(x), Some(y)) => (x - y).abs() <= f64::EPSILON * x.abs().max(y.abs()).max(1.0) * 4.0,
            _ => false,
        },
        _ => false,
    }
}

/// A Kubernetes resource quantity as a number: `500m`, `1.5Gi`, `2e3`, `128974848`.
pub fn quantity(v: &Value) -> Option<f64> {
    let text = match v {
        Value::Number(n) => return n.as_f64(),
        Value::String(s) => s.trim(),
        _ => return None,
    };
    let split = text.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '+' || c == '-')).unwrap_or(text.len());
    let (num, suffix) = text.split_at(split);
    let base: f64 = num.parse().ok()?;
    let mult = match suffix {
        "" => 1.0,
        "n" => 1e-9,
        "u" => 1e-6,
        "m" => 1e-3,
        "k" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        "P" => 1e15,
        "E" => 1e18,
        "Ki" => 1024.0,
        "Mi" => 1024.0 * 1024.0,
        "Gi" => 1024.0 * 1024.0 * 1024.0,
        "Ti" => 1024f64.powi(4),
        "Pi" => 1024f64.powi(5),
        "Ei" => 1024f64.powi(6),
        e if e.starts_with(['e', 'E']) => 10f64.powi(e.get(1..)?.parse().ok()?),
        _ => return None,
    };
    Some(base * mult)
}

fn removed(f: &FieldSet, d: Option<&Value>, w: &mut Walk) -> Option<Verdict> {
    let d = d.filter(|d| !d.is_null());
    for (elem, child) in &f.members {
        match elem {
            PathElem::Field(name) => {
                let dv = d.and_then(Value::as_object).and_then(|m| m.get(name));
                w.path.push(name.clone());
                let r = match dv {
                    None => (!is_status_or_server(&w.path)).then(|| w.out(Why::Removed)),
                    Some(dv) => removed(child, Some(dv), w),
                };
                w.path.pop();
                if r.is_some() {
                    return r;
                }
            }
            PathElem::Key(keys) => {
                // A key the desired item leaves out matches its default (a port's protocol).
                let defaults = merge_keys(w.field(), w.parent()).unwrap_or(&[]);
                let found = d.and_then(Value::as_array).and_then(|items| {
                    items.iter().find(|item| {
                        keys.iter().all(|(k, kv)| match item.get(k) {
                            Some(iv) => scalar_eq(iv, kv, false),
                            None => defaults
                                .iter()
                                .any(|(dk, dv)| dk == k && dv.is_some_and(|dv| kv.as_str() == Some(dv))),
                        })
                    })
                });
                let label = keys.iter().map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or(""))).collect::<Vec<_>>().join(",");
                w.path.push(format!("[{label}]"));
                let r = match found {
                    None => Some(w.out(Why::Removed)),
                    Some(item) => removed(child, Some(item), w),
                };
                w.path.pop();
                if r.is_some() {
                    return r;
                }
            }
            PathElem::Value(v) => {
                let present = d.and_then(Value::as_array).is_some_and(|items| items.iter().any(|i| scalar_eq(i, v, false)));
                if !present {
                    return Some(w.out(Why::Removed));
                }
            }
            PathElem::Index(i) => {
                let item = d.and_then(Value::as_array).and_then(|items| items.get(*i));
                w.path.push(format!("[{i}]"));
                let r = match item {
                    None => Some(w.out(Why::Removed)),
                    Some(item) => removed(child, Some(item), w),
                };
                w.path.pop();
                if r.is_some() {
                    return r;
                }
            }
        }
    }
    None
}

/// Field paths no apply removes (kiln never compares them).
fn is_status_or_server(path: &[String]) -> bool {
    match path {
        [first] => first == "status",
        [meta, field] => meta == "metadata" && SERVER_METADATA.contains(&field.as_str()),
        _ => false,
    }
}

/// Ignore rules for one object, compiled.
#[derive(Clone, Debug, Default)]
pub struct Ignores {
    paths: Vec<Vec<Seg>>,
    managers: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
enum Seg {
    Key(String),
    Index(usize),
    All,
    Select(String, Value),
}

/// A rule kiln could not read, kept so the owner can report it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadPath(pub String);

impl Ignores {
    /// Compiles the rules that apply to one object.
    pub fn for_object<'a>(rules: impl IntoIterator<Item = &'a IgnoreRule>, obj: &Value) -> (Ignores, Vec<BadPath>) {
        let (group, kind, ns, name) = (obj::group(obj), obj::kind(obj), obj::namespace(obj), obj::name(obj));
        let mut out = Ignores::default();
        let mut bad = Vec::new();
        for r in rules.into_iter().filter(|r| r.matches(group, kind, ns, name)) {
            for p in &r.json_pointers {
                match pointer(p) {
                    Some(segs) => out.paths.push(segs),
                    None => bad.push(BadPath(p.clone())),
                }
            }
            for p in &r.jq_paths {
                match jq(p) {
                    Some(segs) => out.paths.push(segs),
                    None => bad.push(BadPath(p.clone())),
                }
            }
            out.managers.extend(r.managers.iter().cloned());
        }
        (out, bad)
    }

    /// Whether there is nothing to ignore.
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.managers.is_empty()
    }

    /// Removes the ignored fields from a value.
    pub fn strip(&self, v: &mut Value, live: Option<&LiveView>) {
        for p in &self.paths {
            strip_path(v, p);
        }
        if let Some(live) = live {
            for m in live.managed.iter().filter(|m| self.managers.contains(&m.manager)) {
                strip_fields(v, &m.fields);
            }
        }
    }
}

fn pointer(p: &str) -> Option<Vec<Seg>> {
    if p.is_empty() {
        return None;
    }
    let rest = p.strip_prefix('/')?;
    Some(
        rest.split('/')
            .map(|s| {
                let s = s.replace("~1", "/").replace("~0", "~");
                match s.parse::<usize>() {
                    Ok(i) => Seg::Index(i),
                    Err(_) => Seg::Key(s),
                }
            })
            .collect(),
    )
}

/// The jq subset Argo CD's `jqPathExpressions` are written in: `.a.b`, `.a["k.e.y"]`, `.a[]`,
/// `.a[0]`, and `| select(.field == "value")` after a `[]`.
fn jq(p: &str) -> Option<Vec<Seg>> {
    let mut segs = Vec::new();
    for (n, part) in p.split('|').map(str::trim).enumerate() {
        if let Some(inner) = part.strip_prefix("select(").and_then(|r| r.strip_suffix(')')) {
            if segs.last() != Some(&Seg::All) {
                return None;
            }
            let (lhs, rhs) = inner.split_once("==")?;
            let field = lhs.trim().strip_prefix('.')?.to_owned();
            let value: Value = serde_json::from_str(rhs.trim()).ok()?;
            segs.pop();
            segs.push(Seg::Select(field, value));
            continue;
        }
        if n > 0 && part == "." {
            continue;
        }
        let mut rest = part.strip_prefix('.')?;
        while !rest.is_empty() {
            if let Some(r) = rest.strip_prefix("[]") {
                segs.push(Seg::All);
                rest = r;
            } else if let Some(r) = rest.strip_prefix('[') {
                let end = r.find(']')?;
                let inner = r.get(..end)?;
                rest = r.get(end + 1..)?;
                if let Ok(i) = inner.parse::<usize>() {
                    segs.push(Seg::Index(i));
                } else {
                    segs.push(Seg::Key(serde_json::from_str::<String>(inner).ok()?));
                }
            } else if let Some(r) = rest.strip_prefix('.') {
                rest = r;
            } else if let Some(r) = rest.strip_prefix('"') {
                let end = r.find('"')?;
                segs.push(Seg::Key(r.get(..end)?.to_owned()));
                rest = r.get(end + 1..)?;
            } else {
                let end = rest.find(['.', '[']).unwrap_or(rest.len());
                segs.push(Seg::Key(rest.get(..end)?.to_owned()));
                rest = rest.get(end..)?;
            }
        }
    }
    (!segs.is_empty()).then_some(segs)
}

fn strip_path(v: &mut Value, segs: &[Seg]) {
    let Some((first, rest)) = segs.split_first() else { return };
    match first {
        Seg::Key(k) => {
            if let Some(m) = v.as_object_mut() {
                if rest.is_empty() {
                    m.remove(k);
                } else if let Some(child) = m.get_mut(k) {
                    strip_path(child, rest);
                }
            }
        }
        Seg::Index(i) => {
            if let Some(items) = v.as_array_mut() {
                if rest.is_empty() {
                    if *i < items.len() {
                        items.remove(*i);
                    }
                } else if let Some(child) = items.get_mut(*i) {
                    strip_path(child, rest);
                }
            }
        }
        Seg::All => {
            if let Some(items) = v.as_array_mut() {
                if rest.is_empty() {
                    items.clear();
                } else {
                    for child in items {
                        strip_path(child, rest);
                    }
                }
            }
        }
        Seg::Select(field, want) => {
            if let Some(items) = v.as_array_mut() {
                if rest.is_empty() {
                    items.retain(|i| i.get(field) != Some(want));
                } else {
                    for child in items.iter_mut().filter(|i| i.get(field) == Some(want)) {
                        strip_path(child, rest);
                    }
                }
            }
        }
    }
}

fn strip_fields(v: &mut Value, f: &FieldSet) {
    for (elem, child) in &f.members {
        match elem {
            PathElem::Field(name) => {
                let Some(m) = v.as_object_mut() else { continue };
                if child.own && child.members.is_empty() {
                    m.remove(name);
                } else if let Some(cv) = m.get_mut(name) {
                    strip_fields(cv, child);
                }
            }
            PathElem::Key(keys) => {
                let Some(items) = v.as_array_mut() else { continue };
                let matches = |item: &Value| keys.iter().all(|(k, kv)| item.get(k) == Some(kv));
                if child.own && child.members.is_empty() {
                    items.retain(|i| !matches(i));
                } else {
                    for item in items.iter_mut().filter(|i| matches(i)) {
                        strip_fields(item, child);
                    }
                }
            }
            PathElem::Value(val) => {
                if let Some(items) = v.as_array_mut() {
                    items.retain(|i| i != val);
                }
            }
            PathElem::Index(i) => {
                if let Some(item) = v.as_array_mut().and_then(|items| items.get_mut(*i)) {
                    strip_fields(item, child);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn deployment(image: &str, replicas: i64) -> Value {
        json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web", "namespace": "w", "labels": {"app": "web"}},
            "spec": {"replicas": replicas, "template": {"metadata": {"creationTimestamp": null}, "spec": {"containers": [
                {"name": "web", "image": image, "ports": [{"containerPort": 8080}], "resources": {"limits": {"cpu": "500m", "memory": "1Gi"}}}
            ]}}}})
    }

    fn live_of(desired: &Value, manager_fields: Value) -> Value {
        let mut l = desired.clone();
        if let Some(m) = l.get_mut("metadata").and_then(Value::as_object_mut) {
            m.insert("uid".into(), json!("u-1"));
            m.insert("resourceVersion".into(), json!("42"));
            m.insert("managedFields".into(), json!([{"manager": "kiln", "operation": "Apply", "fieldsV1": manager_fields},
                {"manager": "kube-controller-manager", "operation": "Update", "subresource": "status", "fieldsV1": {"f:status": {}}}]));
        }
        l
    }

    fn kiln_fields() -> Value {
        json!({"f:metadata": {"f:labels": {"f:app": {}}}, "f:spec": {"f:replicas": {}, "f:template": {"f:spec": {"f:containers": {
            "k:{\"name\":\"web\"}": {".": {}, "f:image": {}, "f:name": {}, "f:ports": {"k:{\"containerPort\":8080,\"protocol\":\"TCP\"}": {".": {}, "f:containerPort": {}}},
            "f:resources": {"f:limits": {"f:cpu": {}, "f:memory": {}}}}}}}}})
    }

    #[test]
    fn defaults_and_spellings_are_not_drift() {
        let mut d = deployment("web:1", 3);
        normalize_desired(&mut d);
        let mut l = live_of(&d, kiln_fields());
        // The server's defaults and its own spelling of quantities.
        if let Some(c) = l.pointer_mut("/spec/template/spec/containers/0").and_then(Value::as_object_mut) {
            c.insert("imagePullPolicy".into(), json!("IfNotPresent"));
            c.insert("terminationMessagePath".into(), json!("/dev/termination-log"));
            c.insert("ports".into(), json!([{"containerPort": 8080, "protocol": "TCP"}]));
            c.insert("resources".into(), json!({"limits": {"cpu": "0.5", "memory": "1073741824"}}));
        }
        if let Some(s) = l.pointer_mut("/spec").and_then(Value::as_object_mut) {
            s.insert("progressDeadlineSeconds".into(), json!(600));
        }
        let view = LiveView::new(l, "kiln");
        assert_eq!(compare(&d, &view.object, view.owned("kiln").as_ref()), Verdict::Synced);
    }

    #[test]
    fn changed_and_removed_fields_are_drift() {
        let mut d = deployment("web:1", 3);
        normalize_desired(&mut d);
        let mut l = live_of(&d, kiln_fields());
        if let Some(c) = l.pointer_mut("/spec/template/spec/containers/0").and_then(Value::as_object_mut) {
            c.insert("image".into(), json!("web:2"));
        }
        let view = LiveView::new(l, "kiln");
        assert_eq!(
            compare(&d, &view.object, view.owned("kiln").as_ref()),
            Verdict::OutOfSync { path: ".spec.template.spec.containers[name=web].image".into(), why: Why::Differs }
        );
        // Desired stops setting replicas while kiln still owns them: the next apply removes them.
        let mut d2 = d.clone();
        obj::remove(&mut d2, &["spec", "replicas"]);
        let view = LiveView::new(live_of(&d, kiln_fields()), "kiln");
        assert_eq!(
            compare(&d2, &view.object, view.owned("kiln").as_ref()),
            Verdict::OutOfSync { path: ".spec.replicas".into(), why: Why::Removed }
        );
    }

    #[test]
    fn other_managers_fields_are_not_drift_unless_kiln_sets_them() {
        let mut d = deployment("web:1", 3);
        normalize_desired(&mut d);
        obj::remove(&mut d, &["spec", "replicas"]);
        let mut fields = kiln_fields();
        if let Some(spec) = fields.pointer_mut("/f:spec").and_then(Value::as_object_mut) {
            spec.remove("f:replicas");
        }
        let mut l = live_of(&d, fields);
        if let Some(s) = l.pointer_mut("/spec").and_then(Value::as_object_mut) {
            s.insert("replicas".into(), json!(9)); // the HPA's
        }
        let view = LiveView::new(l, "kiln");
        assert!(compare(&d, &view.object, view.owned("kiln").as_ref()).synced());
    }

    #[test]
    fn three_way_uses_last_applied_without_managed_fields() {
        let mut d = deployment("web:1", 3);
        normalize_desired(&mut d);
        let mut previous = d.clone();
        if let Some(m) = previous.pointer_mut("/metadata/labels").and_then(Value::as_object_mut) {
            m.insert("tier".into(), json!("front"));
        }
        let mut l = previous.clone();
        if let Some(a) = obj::ensure_map(&mut l, &["metadata", "annotations"]) {
            a.insert(LAST_APPLIED.into(), Value::String(previous.to_string()));
        }
        let view = LiveView::new(l, "kiln");
        assert_eq!(
            compare(&d, &view.object, view.owned("kiln").as_ref()),
            Verdict::OutOfSync { path: ".metadata.labels.tier".into(), why: Why::Removed }
        );
    }

    #[test]
    fn ignore_rules_take_pointers_jq_and_managers() {
        let mut d = deployment("web:1", 3);
        normalize_desired(&mut d);
        let rules = vec![IgnoreRule {
            group: "apps".into(),
            kind: "Deployment".into(),
            json_pointers: vec!["/spec/replicas".into()],
            jq_paths: vec![".spec.template.spec.containers[] | select(.name == \"web\") | .image".into()],
            ..IgnoreRule::default()
        }];
        let (ig, bad) = Ignores::for_object(&rules, &d);
        assert!(bad.is_empty());
        let mut l = live_of(&d, kiln_fields());
        if let Some(c) = l.pointer_mut("/spec/template/spec/containers/0").and_then(Value::as_object_mut) {
            c.insert("image".into(), json!("web:2"));
        }
        if let Some(s) = l.pointer_mut("/spec").and_then(Value::as_object_mut) {
            s.insert("replicas".into(), json!(1));
        }
        let view = LiveView::new(l, "kiln");
        let (mut d2, mut l2) = (d.clone(), view.object.clone());
        ig.strip(&mut d2, Some(&view));
        ig.strip(&mut l2, Some(&view));
        assert!(compare(&d2, &l2, None).synced());
    }

    #[test]
    fn secrets_fold_string_data() {
        let mut s = json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "s"}, "stringData": {"a": "hi"}, "data": {"b": "eA=="}});
        normalize_desired(&mut s);
        assert_eq!(s.pointer("/data/a"), Some(&json!("aGk=")));
        assert_eq!(s.pointer("/data/b"), Some(&json!("eA==")));
        assert!(s.get("stringData").is_none());
    }

    #[test]
    fn quantities_parse() {
        assert_eq!(quantity(&json!("500m")), Some(0.5));
        assert_eq!(quantity(&json!("1Gi")), Some(1073741824.0));
        assert_eq!(quantity(&json!("2e3")), Some(2000.0));
        assert_eq!(quantity(&json!("x")), None);
    }

    #[test]
    fn live_hash_ignores_status_and_other_managers() {
        let mut d = deployment("web:1", 3);
        normalize_desired(&mut d);
        let mut a = live_of(&d, kiln_fields());
        let mut b = a.clone();
        if let Some(m) = a.as_object_mut() {
            m.insert("status".into(), json!({"replicas": 1}));
        }
        if let Some(m) = b.as_object_mut() {
            m.insert("status".into(), json!({"replicas": 3}));
        }
        assert_eq!(LiveView::new(a, "kiln").hash, LiveView::new(b, "kiln").hash);
    }
}
