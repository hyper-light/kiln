//! JSON Schemas (draft 2020-12) of the GHA dialect, the kiln dialect, overlays and action
//! metadata (docs/ci.md W02-R09), generated from [`super::keys`], the same tables the readers
//! check against, so the editor and the validator never disagree.

use serde_json::{Map, Value, json};

use super::keys::{self, Key, Ty};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaKind {
    Gha,
    Kiln,
    Overlay,
    Action,
}

impl SchemaKind {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "gha" => Self::Gha,
            "kiln" => Self::Kiln,
            "overlay" => Self::Overlay,
            "action" => Self::Action,
            _ => return None,
        })
    }
}

fn expr_string() -> Value {
    json!({"type": "string", "pattern": "\\$\\{\\{"})
}

fn ty(t: Ty, kind: SchemaKind) -> Value {
    match t {
        Ty::Str | Ty::Tpl => json!({"type": "string"}),
        Ty::Bool => json!({"type": "boolean"}),
        Ty::Num => json!({"type": "number"}),
        Ty::BoolOrExpr => json!({"anyOf": [{"type": "boolean"}, expr_string()]}),
        Ty::NumOrExpr => json!({"anyOf": [{"type": "number"}, expr_string()]}),
        Ty::StrList => json!({"anyOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}}]}),
        Ty::StrMap => json!({"type": "object", "additionalProperties": {"type": ["string", "number", "boolean", "null"]}}),
        Ty::Map(keys) => object(keys, kind),
        Ty::StrOrMap(keys) => json!({"anyOf": [{"type": "string"}, object(keys, kind)]}),
        Ty::Any => json!({}),
    }
}

fn allowed(k: &Key, kind: SchemaKind) -> bool {
    match kind {
        SchemaKind::Gha => !k.kiln,
        SchemaKind::Kiln | SchemaKind::Action => true,
        SchemaKind::Overlay => k.overlay || k.name == "runs-on" || k.name == "jobs" || k.name == "steps",
    }
}

fn object(keys: &[Key], kind: SchemaKind) -> Value {
    let mut props = Map::new();
    for k in keys.iter().filter(|k| allowed(k, kind)) {
        let mut s = ty(k.ty, kind);
        if let Value::Object(o) = &mut s {
            o.insert("description".into(), Value::String(k.doc.to_owned()));
        }
        props.insert(k.name.to_owned(), s);
    }
    json!({"type": "object", "properties": props, "additionalProperties": false})
}

fn with_prop(mut obj: Value, name: &str, schema: Value) -> Value {
    if let Some(Value::Object(props)) = obj.get_mut("properties") {
        let doc = props.get(name).and_then(|p| p.get("description")).cloned();
        let mut s = schema;
        if let (Some(d), Value::Object(o)) = (doc, &mut s) {
            o.insert("description".into(), d);
        }
        props.insert(name.to_owned(), s);
    }
    obj
}

const ID: &str = "^[A-Za-z_][A-Za-z0-9_-]*$";

/// The schema of `kind`.
pub fn schema(kind: SchemaKind) -> Value {
    if kind == SchemaKind::Action {
        let mut a = object(keys::ACTION, kind);
        a = with_prop(a, "inputs", json!({"type": "object", "additionalProperties": object(keys::ACTION_INPUT, kind)}));
        a = with_prop(a, "outputs", json!({"type": "object", "additionalProperties": object(keys::ACTION_OUTPUT, kind)}));
        a = with_prop(a, "runs", object(keys::ACTION_RUNS, kind));
        return wrap(a, "kiln action metadata (action.yml)");
    }
    let step = object(keys::STEP, kind);
    let mut job = object(keys::JOB, kind);
    job = with_prop(job, "steps", if kind == SchemaKind::Overlay { json!({"type": "object", "additionalProperties": step}) } else { json!({"type": "array", "items": step}) });
    let mut strategy = object(keys::STRATEGY, kind);
    strategy = with_prop(strategy, "matrix", json!({"anyOf": [expr_string(), {"type": "object", "properties": reserved(kind), "additionalProperties": {"anyOf": [{"type": "array"}, expr_string()]}}]}));
    job = with_prop(job, "strategy", strategy);
    job = with_prop(job, "runs-on", json!({"anyOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}}, {"type": "object", "properties": {"group": {"type": "string"}, "labels": {"anyOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}}]}}, "additionalProperties": false}]}));
    job = with_prop(job, "needs", json!({"anyOf": [{"type": "string"}, {"type": "array"}]}));
    job = with_prop(job, "services", json!({"type": "object", "additionalProperties": ty(Ty::StrOrMap(keys::CONTAINER), kind)}));
    let mut wf = object(keys::WORKFLOW, kind);
    wf = with_prop(wf, "jobs", json!({"type": "object", "patternProperties": {ID: job}, "additionalProperties": false}));
    wf = with_prop(wf, "on", on(kind));
    let title = match kind {
        SchemaKind::Gha => "GitHub Actions workflow (.github/workflows), as kiln reads it",
        SchemaKind::Kiln => "kiln workflow (.kiln/*.yaml): GitHub's syntax plus kiln's keys",
        _ => "kiln overlay (.kiln/overlays/<workflow>.yaml): kiln keys for a GitHub workflow",
    };
    wrap(wf, title)
}

fn reserved(kind: SchemaKind) -> Value {
    let mut m = Map::new();
    for k in keys::MATRIX_RESERVED.iter().filter(|k| allowed(k, kind) || (kind == SchemaKind::Gha && !k.kiln)) {
        m.insert(k.name.to_owned(), json!({"description": k.doc}));
    }
    Value::Object(m)
}

fn on(kind: SchemaKind) -> Value {
    let mut events = Map::new();
    let kiln_events = if matches!(kind, SchemaKind::Kiln | SchemaKind::Overlay) { keys::KILN_EVENTS } else { &[] };
    for e in keys::GHA_EVENTS.iter().chain(kiln_events) {
        let filters: Vec<&Key> = keys::FILTERS.iter().filter(|f| keys::event_keys(e).contains(&f.name)).collect();
        let mut props = Map::new();
        for f in filters {
            let mut s = ty(f.ty, kind);
            if f.name == "types"
                && let Some(types) = keys::event_types(e)
            {
                s = json!({"anyOf": [{"type": "string", "enum": types}, {"type": "array", "items": {"type": "string", "enum": types}}]});
            }
            if let Value::Object(o) = &mut s {
                o.insert("description".into(), Value::String(f.doc.to_owned()));
            }
            props.insert(f.name.to_owned(), s);
        }
        let ev = if *e == "schedule" { json!({"type": "array", "items": {"type": "object", "properties": {"cron": {"type": "string"}}, "required": ["cron"]}}) } else { json!({"anyOf": [{"type": "null"}, {"type": "object", "properties": props, "additionalProperties": false}]}) };
        events.insert((*e).to_owned(), ev);
    }
    let names: Vec<&str> = keys::GHA_EVENTS.iter().chain(kiln_events).copied().collect();
    json!({"anyOf": [
        {"type": "string", "enum": names},
        {"type": "array", "items": {"type": "string", "enum": names}},
        {"type": "object", "properties": events, "additionalProperties": false}
    ]})
}

/// Checks `v` against `schema`: the subset of JSON Schema these schemas use (type, properties,
/// additionalProperties, patternProperties, items, anyOf, enum, pattern, required). Returns the
/// problems as `path: why`; empty when it conforms.
pub fn check(schema: &Value, v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    check_at(schema, v, "", &mut out);
    out
}

fn type_ok(t: &str, v: &Value) -> bool {
    match t {
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "number" => v.is_number(),
        "object" => v.is_object(),
        "array" => v.is_array(),
        "null" => v.is_null(),
        _ => true,
    }
}

fn check_at(schema: &Value, v: &Value, path: &str, out: &mut Vec<String>) {
    let Some(s) = schema.as_object() else { return };
    if let Some(any) = s.get("anyOf").and_then(Value::as_array) {
        if !any.iter().any(|alt| check(alt, v).is_empty()) {
            out.push(format!("{path}: matches none of its allowed forms"));
        }
        return;
    }
    match s.get("type") {
        Some(Value::String(t)) if !type_ok(t, v) => {
            out.push(format!("{path}: expected {t}"));
            return;
        }
        Some(Value::Array(ts)) if !ts.iter().filter_map(Value::as_str).any(|t| type_ok(t, v)) => {
            out.push(format!("{path}: wrong type"));
            return;
        }
        _ => {}
    }
    if let Some(e) = s.get("enum").and_then(Value::as_array)
        && !e.contains(v)
    {
        out.push(format!("{path}: not one of the allowed values"));
    }
    if let (Some(p), Some(text)) = (s.get("pattern").and_then(Value::as_str), v.as_str())
        && regex::Regex::new(p).is_ok_and(|re| !re.is_match(text))
    {
        out.push(format!("{path}: doesn't match {p}"));
    }
    if let (Some(items), Some(arr)) = (s.get("items"), v.as_array()) {
        for (i, x) in arr.iter().enumerate() {
            check_at(items, x, &format!("{path}[{i}]"), out);
        }
    }
    if let Some(obj) = v.as_object() {
        let props = s.get("properties").and_then(Value::as_object);
        let pattern_props = s.get("patternProperties").and_then(Value::as_object);
        for (k, x) in obj {
            let at = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
            if let Some(ps) = props.and_then(|p| p.get(k)) {
                check_at(ps, x, &at, out);
                continue;
            }
            let pat = pattern_props.and_then(|pp| pp.iter().find(|(re, _)| regex::Regex::new(re).is_ok_and(|r| r.is_match(k))).map(|(_, ps)| ps));
            if let Some(ps) = pat {
                check_at(ps, x, &at, out);
                continue;
            }
            match s.get("additionalProperties") {
                Some(Value::Bool(false)) => out.push(format!("{at}: not allowed here")),
                Some(extra @ Value::Object(_)) => check_at(extra, x, &at, out),
                _ => {}
            }
        }
        if let Some(req) = s.get("required").and_then(Value::as_array) {
            for r in req.iter().filter_map(Value::as_str) {
                if !obj.contains_key(r) {
                    out.push(format!("{path}: needs `{r}`"));
                }
            }
        }
    }
}

fn wrap(mut v: Value, title: &str) -> Value {
    if let Value::Object(o) = &mut v {
        o.insert("$schema".into(), Value::String("https://json-schema.org/draft/2020-12/schema".into()));
        o.insert("title".into(), Value::String(title.to_owned()));
    }
    v
}
