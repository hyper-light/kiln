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
        Ty::StrMap => json!({"type": "object", "additionalProperties": {"type": ["string", "number", "boolean"]}}),
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

fn wrap(mut v: Value, title: &str) -> Value {
    if let Value::Object(o) = &mut v {
        o.insert("$schema".into(), Value::String("https://json-schema.org/draft/2020-12/schema".into()));
        o.insert("title".into(), Value::String(title.to_owned()));
    }
    v
}
