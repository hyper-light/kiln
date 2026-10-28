//! Every operation, once (docs/ci.md W20-R01): deployments' ([`crate::cd::ops`]), releases by
//! API ([`crate::deploy::ops`]), CI's ([`crate::ci::ops`]) and kiln's own: agents'
//! ([`crate::harness::ops`]), plans' ([`crate::plans::ops`]) and goal stacks'
//! ([`crate::goals::ops`]). Each has its name, summary, input schema and whether it changes
//! anything. The MCP tools and code mode's `kiln.*` are this list. Each operation is checked
//! against the route table as its HTTP route is, so a caller may do through them exactly what it
//! may do over HTTP.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use http::{Method, StatusCode};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::authz::Caller;
use crate::hub::Hub;

/// A failure: an HTTP status and why.
pub type Fail = (StatusCode, String);
/// An operation's answer.
pub type Out = Result<Value, Fail>;
pub(crate) type Fut = Pin<Box<dyn Future<Output = Out> + Send>>;

/// Who calls.
#[derive(Clone)]
pub struct Cx {
    pub hub: Arc<Hub>,
    /// As the route table sees it; its name is who the audit trail says acted.
    pub caller: Caller,
}

/// What an operation is, as lists and tools show it.
pub struct Info {
    pub name: &'static str,
    pub summary: &'static str,
    pub read_only: bool,
    pub destructive: bool,
    pub input: Input,
}

/// One of kiln's own operations (agents, plans, goal stacks): what it is, the HTTP route it is
/// (method and path, for the route table and the audit trail), and what it does.
pub struct Op {
    /// Its name, dotted (`agents.start`).
    pub name: &'static str,
    pub summary: &'static str,
    pub read_only: bool,
    pub destructive: bool,
    pub input: Input,
    /// Its route, from the input: method and path segments (`["api", "harness", "agents"]`).
    pub route: fn(&Value) -> Result<(Method, Vec<String>), Fail>,
    pub run: fn(Cx, Value) -> Fut,
}

/// What an operation takes: a Rust type, which reads each input it's given before it runs
/// ([`call`]) and from which its JSON Schema is generated (MCP's `inputSchema`, `GET /api/ops`'
/// `input`), so the two can't drift apart. Made only by [`Input::of`].
#[derive(Clone, Copy)]
pub struct Input {
    schema: fn() -> Value,
    fits: fn(&Value) -> Result<(), Misfit>,
}

impl Input {
    /// `T`'s: an input is read as a `T` (a field `T` doesn't have is refused where `T` says
    /// `deny_unknown_fields`, its schema's `additionalProperties: false`).
    pub fn of<T: DeserializeOwned + JsonSchema>() -> Input {
        Input { schema: schema_of::<T>, fits: fits::<T> }
    }

    /// Its JSON Schema.
    pub fn schema(&self) -> Value {
        (self.schema)()
    }

    /// Whether `input` fits it: where and why not.
    pub fn fits(&self, input: &Value) -> Result<(), Misfit> {
        (self.fits)(input)
    }
}

/// `T`'s JSON Schema as an operation publishes it: whole (no `$ref`), without `$schema` or a
/// title, each description one line (a doc comment's lines joined), `properties` even when it
/// takes none, and a field that may be left out said so only by not being `required`
/// ([`optional_not_null`]).
pub fn schema_of<T: JsonSchema>() -> Value {
    fn one_line(v: &mut Value) {
        match v {
            Value::Object(o) => {
                if let Some(Value::String(d)) = o.get_mut("description") {
                    *d = d.lines().map(str::trim).collect::<Vec<_>>().join(" ");
                }
                o.values_mut().for_each(one_line);
            }
            Value::Array(a) => a.iter_mut().for_each(one_line),
            _ => {}
        }
    }
    let settings = schemars::generate::SchemaSettings::draft2020_12().with(|s| {
        s.meta_schema = None;
        s.inline_subschemas = true;
    });
    let mut v = settings.into_generator().into_root_schema_for::<T>().to_value();
    if let Some(o) = v.as_object_mut() {
        o.remove("title");
        o.remove("description");
        // An input that takes nothing says so: some clients want `properties` on every object.
        o.entry("properties").or_insert_with(|| json!({}));
    }
    optional_not_null(&mut v);
    one_line(&mut v);
    v
}

/// The null serde takes for a field that may be left out (an `Option`), taken out of its schema:
/// such a field says so by not being `required`, as the hand-written schemas had it and as
/// clients that take a narrow JSON Schema (Gemini's) want it. A field marked `x-kiln-nullable`,
/// where null means something (`programs.assign`'s unassign), keeps it.
fn optional_not_null(v: &mut Value) {
    fn without_null(p: &mut Value) {
        let Some(o) = p.as_object_mut() else { return };
        if o.remove("x-kiln-nullable").is_some() {
            return;
        }
        let single = match o.get_mut("type") {
            Some(Value::Array(ts)) => {
                ts.retain(|t| t.as_str() != Some("null"));
                if ts.len() == 1 { ts.first().cloned() } else { None }
            }
            _ => None,
        };
        if let Some(t) = single {
            o.insert("type".into(), t);
        }
        if let Some(Value::Array(e)) = o.get_mut("enum") {
            e.retain(|x| !x.is_null());
        }
        let only = match o.get_mut("anyOf") {
            Some(Value::Array(any)) => {
                any.retain(|x| x.get("type").and_then(Value::as_str) != Some("null"));
                if any.len() == 1 { any.pop() } else { None }
            }
            _ => None,
        };
        if let Some(Value::Object(only)) = only {
            o.remove("anyOf");
            for (k, x) in only {
                o.entry(k).or_insert(x);
            }
        }
    }
    match v {
        Value::Object(o) => {
            let required: Vec<String> = o.get("required").and_then(Value::as_array).map(|r| r.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
            if let Some(Value::Object(props)) = o.get_mut("properties") {
                for (_, p) in props.iter_mut().filter(|(k, _)| !required.contains(k)) {
                    without_null(p);
                }
            }
            o.values_mut().for_each(optional_not_null);
        }
        Value::Array(a) => a.iter_mut().for_each(optional_not_null),
        _ => {}
    }
}

/// A schema as kiln's hand-written schemas spelled it, to hold a generated one ([`schema_of`]) to
/// the one it replaces: without a number's `format` (schemars says `f64` is a `double`), and
/// without `additionalProperties` as a boolean (`true` on a free-form object; `false`, which is
/// what reading an input as its type adds, `deny_unknown_fields`).
#[cfg(test)]
pub(crate) fn as_written_by_hand(schema: &Value) -> Value {
    fn walk(v: &mut Value) {
        let Some(o) = v.as_object_mut() else { return };
        o.remove("format");
        if o.get("additionalProperties").is_some_and(Value::is_boolean) {
            o.remove("additionalProperties");
        }
        for (k, sub) in o.iter_mut() {
            match k.as_str() {
                "properties" => sub.as_object_mut().into_iter().flat_map(|p| p.values_mut()).for_each(walk),
                "items" | "additionalProperties" => walk(sub),
                "anyOf" | "oneOf" | "allOf" => sub.as_array_mut().into_iter().flatten().for_each(walk),
                _ => {}
            }
        }
    }
    let mut v = schema.clone();
    walk(&mut v);
    v
}

/// `input` read as a `T`: where and why it doesn't fit.
fn fits<T: DeserializeOwned>(input: &Value) -> Result<(), Misfit> {
    serde_path_to_error::deserialize::<_, T>(input).map(|_| ()).map_err(|e| Misfit::of(e.path(), &e.inner().to_string()))
}

// A number or a string: a goal by its place (from 1) or its id; a time as unix ms or RFC 3339.
// (Not a doc comment: it would be the description of each field of this type.)
#[derive(Debug, serde::Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum NumberOrText {
    Number(i64),
    Text(String),
}

/// An input that doesn't fit its operation's type: where (a JSON Pointer, `/goals/0/needs`; empty
/// for the whole) and why, as serde says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Misfit {
    pub path: String,
    pub why: String,
}

impl Misfit {
    fn of(path: &serde_path_to_error::Path, why: &str) -> Misfit {
        use serde_path_to_error::Segment;
        let mut at = String::new();
        for s in path.iter() {
            let part = match s {
                Segment::Seq { index } => index.to_string(),
                Segment::Map { key } => key.replace('~', "~0").replace('/', "~1"),
                Segment::Enum { variant } => variant.clone(),
                Segment::Unknown => continue,
            };
            at.push('/');
            at.push_str(&part);
        }
        Misfit { path: at, why: why.to_owned() }
    }

    /// What kind of misfit it is, for the model to correct it: `unknown_field`, `missing_field`,
    /// `invalid_type`, `invalid_value` or `invalid_input`.
    pub fn code(&self) -> &'static str {
        let w = self.why.as_str();
        if w.starts_with("unknown field") {
            "unknown_field"
        } else if w.starts_with("missing field") {
            "missing_field"
        } else if w.starts_with("invalid type") {
            "invalid_type"
        } else if w.starts_with("invalid value") || w.starts_with("unknown variant") || w.starts_with("invalid length") {
            "invalid_value"
        } else {
            "invalid_input"
        }
    }

    /// It in words, for operation `name`: `goals.add doesn't take this input: unknown field
    /// `golas`, expected … (at /golas)`.
    pub fn said(&self, name: &str) -> String {
        if self.path.is_empty() { format!("{name} doesn't take this input: {}", self.why) } else { format!("{name} doesn't take this input: {} (at {})", self.why, self.path) }
    }

    /// Its why as the refusal is noted (`crate::wire::unread`): without the values that came.
    pub fn noted(&self) -> String {
        let why = crate::wire::payload::without_values(&self.why);
        if self.path.is_empty() { why } else { format!("{why} (at {})", self.path) }
    }

    /// A tool's structured answer for it: what went wrong, its code, where and why.
    pub fn answer(&self, name: &str) -> Value {
        json!({"error": self.said(name), "code": self.code(), "path": self.path, "why": self.why})
    }
}

/// What operation `name` takes, whichever's it is: kiln's own, CI's, releases' or deployments'.
pub fn input_of(name: &str) -> Option<Input> {
    own()
        .iter()
        .find(|o| o.name == name)
        .map(|o| o.input)
        .or_else(|| crate::ci::ops::ops().iter().find(|o| o.name == name).map(|o| o.input))
        .or_else(|| crate::deploy::ops::ops().iter().find(|o| o.name == name).map(|o| o.input))
        .or_else(|| crate::cd::ops::ops().iter().find(|o| o.name == name).map(|o| o.input))
}

/// Why `input` doesn't fit operation `name` as `caller` gives it (an agent's own id where it left
/// `agent` out, [`YOURS`]): each operation's input is read as its type.
pub fn misfit(caller: &Caller, name: &str, input: &Value) -> Option<Misfit> {
    let wants = input_of(name)?;
    let input = yours(caller, name, if input.is_null() { json!({}) } else { input.clone() });
    wants.fits(&input).err()
}

/// A field that must be given, null among what it may be (`"on": null` clears): serde would take a
/// missing `Option` for null.
pub fn present<'de, D: serde::Deserializer<'de>, T: serde::Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    <Option<T> as serde::Deserialize>::deserialize(d)
}

/// A string field of an input, required.
pub fn text(v: &Value, k: &str) -> Result<String, Fail> {
    v.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).ok_or_else(|| (StatusCode::BAD_REQUEST, format!("`{k}` is required")))
}

/// A route's path, as segments.
pub fn path(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_owned()).collect()
}

/// kiln's own operations: agents', plans', goal stacks', runners', attachments', and access and
/// the audit trail's, in the order they're listed.
pub fn own() -> &'static [Op] {
    static OPS: std::sync::OnceLock<Vec<Op>> = std::sync::OnceLock::new();
    OPS.get_or_init(|| {
        crate::harness::ops::list_ops()
            .into_iter()
            .chain(crate::plans::ops::list_ops())
            .chain(crate::goals::ops::list_ops())
            .chain(crate::programs::ops::list_ops())
            .chain(crate::research::ops::list_ops())
            .chain(crate::doctor::list_ops())
            .chain(crate::ci::attach::list_ops())
            .chain(crate::authz::ops::list_ops())
            .collect()
    })
}

/// Every operation this server has: CI's, deployments' when they're on, and kiln's own.
pub fn all(hub: &Hub) -> Vec<Info> {
    listed(hub.cfg.cd.enabled)
}

/// The operations as `GET /api/ops` lists them (W20-R01): each one's name, summary, whether it
/// changes anything, and its input's JSON Schema.
pub fn catalog(hub: &Hub) -> Value {
    let ops: Vec<Value> = all(hub).iter().map(|o| json!({ "name": o.name, "summary": o.summary, "read_only": o.read_only, "destructive": o.destructive, "input": o.input.schema() })).collect();
    json!({ "operations": ops })
}

/// CI's operations, deployments' and releases' with `cd`, and kiln's own.
pub fn listed(cd: bool) -> Vec<Info> {
    let mut out: Vec<Info> = Vec::new();
    if cd {
        out.extend(crate::cd::ops::ops().iter().map(|o| Info { name: o.name, summary: o.summary, read_only: o.read_only, destructive: o.destructive, input: o.input }));
        out.extend(crate::deploy::ops::ops().iter().map(|o| Info { name: o.name, summary: o.summary, read_only: o.read_only, destructive: o.destructive, input: o.input }));
    }
    out.extend(crate::ci::ops::ops().iter().map(|o| Info { name: o.name, summary: o.summary, read_only: o.read_only, destructive: o.destructive, input: o.input }));
    out.extend(own().iter().map(|o| Info { name: o.name, summary: o.summary, read_only: o.read_only, destructive: o.destructive, input: o.input }));
    out
}

/// Operations on one agent that an agent calling them without `agent` means for itself: its own
/// goal stack, and putting its own work up for review.
const YOURS: &[&str] = &["goals.get", "goals.add", "goals.set", "goals.control", "goals.done", "goals.update", "goals.order", "goals.split", "goals.plan", "agents.change"];

/// `input` to operation `name` as `caller` means it: an agent's own id where it left out `agent`
/// on one of [`YOURS`].
fn yours(caller: &Caller, name: &str, mut input: Value) -> Value {
    if YOURS.contains(&name)
        && let (Some(me), Some(o)) = (caller.agent.as_deref(), input.as_object_mut())
        && o.get("agent").and_then(Value::as_str).is_none_or(|a| a.trim().is_empty())
    {
        o.insert("agent".into(), json!(me));
    }
    input
}

/// Runs operation `name` for `cx`'s caller, when the route table lets it.
pub async fn call(cx: Cx, name: &str, input: Value) -> Out {
    let input = yours(&cx.caller, name, if input.is_null() { json!({}) } else { input });
    // CI's, and the hosts' pull requests (W23-R06) and experiments (W20-R11) beside them.
    if name.starts_with("ci.") || name.starts_with("host.") || name.starts_with("experiments.") {
        return crate::ci::ops::call(cx, name, input).await;
    }
    if name.starts_with("deploy.") {
        return crate::deploy::ops::call(cx, name, input).await;
    }
    if let Some(op) = own().iter().find(|o| o.name == name) {
        let (method, segs) = (op.route)(&input)?;
        let path = format!("/{}", segs.join("/"));
        let run = async {
            let s: Vec<&str> = segs.iter().map(String::as_str).collect();
            crate::authz::gate::check(&cx.hub, &cx.caller, &method, &s, &Default::default()).await?;
            // Read as its type, once its caller may run it: what doesn't fit is refused, saying
            // where and why.
            op.input.fits(&input).map_err(|m| (StatusCode::BAD_REQUEST, m.said(name)))?;
            (op.run)(cx.clone(), input).await
        };
        if op.read_only {
            return run.await;
        }
        // A change is audited as its route's would be (docs/ci.md W19-R05): one row.
        return crate::audit::request(&cx.hub, crate::audit::Req { caller: &cx.caller, method: &method, path: &path, q: &Default::default(), ip: None }, run, |r: &Out| match r {
            Ok(_) => 200,
            Err((s, _)) => s.as_u16(),
        })
        .await;
    }
    let op = crate::cd::ops::ops().iter().find(|o| o.name == name).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no operation {name}")))?;
    // Deployments' routes: reading needs runs.read, the rest environments.deploy.
    let method = if op.read_only { Method::GET } else { Method::POST };
    let run = async {
        crate::authz::gate::check(&cx.hub, &cx.caller, &method, &["api", "cd", "ops", name], &Default::default()).await?;
        op.input.fits(&input).map_err(|m| (StatusCode::BAD_REQUEST, m.said(name)))?;
        let cd = crate::cd::get().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "deployments aren't on here, or are starting ([cd] enabled)".to_owned()))?;
        crate::cd::ops::call(crate::cd::ops::Cx { hub: Arc::clone(&cx.hub), cd, actor: cx.caller.name.clone() }, name, input).await
    };
    if op.read_only {
        return run.await;
    }
    // A change is audited as its route's would be (docs/ci.md W19-R05): one row.
    let path = format!("/api/cd/ops/{name}");
    crate::audit::request(&cx.hub, crate::audit::Req { caller: &cx.caller, method: &method, path: &path, q: &Default::default(), ip: None }, run, |r: &Out| match r {
        Ok(_) => 200,
        Err((s, _)) => s.as_u16(),
    })
    .await
}

/// Runs one of kiln's own operations for its HTTP route, which the route table has let through
/// and the audit trail records already.
pub async fn run_own(cx: Cx, name: &str, input: Value) -> Out {
    let op = own().iter().find(|o| o.name == name).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no operation {name}")))?;
    (op.run)(cx, input).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each of kiln's own operations is a route the table names (none falls through to the
    /// administrators' `settings.manage`), needs what its route needs (agents' `agents.run`,
    /// runners' `runners.read` or `runners.manage`), and reads exactly when its route is a GET.
    #[test]
    fn kiln_s_own_operations_are_routes_of_the_table() {
        let probe = json!({"agent": "a1", "id": "p1", "run": "r1", "step": 1, "action": "start", "text": "x", "goals": ["g"], "runner": "mac", "policy": "auto", "attachment": "ci-doctor", "incident": "i1", "user": "ada", "grant": "g1", "name": "auditor", "token": "t1", "program": "p1", "track": "T1"});
        assert!(own().iter().any(|o| o.name == "agents.start") && own().iter().any(|o| o.name == "plans.run") && own().iter().any(|o| o.name == "goals.add"));
        for op in own() {
            let route = (op.route)(&probe);
            assert!(route.is_ok(), "{}", op.name);
            let Ok((method, segs)) = route else { continue };
            let s: Vec<&str> = segs.iter().map(String::as_str).collect();
            let rule = crate::authz::rule(&method, &s);
            assert_ne!(rule.pattern, crate::authz::UNNAMED.pattern, "{} ({method} /{}) has a rule of its own", op.name, s.join("/"));
            let needs = match op.name {
                "runners.list" | "runners.diagnose" | "updates.list" => "runners.read",
                "runners.repair" | "updates.retry" | "updates.policy" => "runners.manage",
                // Attachments and incidents: reading them is reading runs (each narrowed to what it
                // looks after); attaching is `agents.attach`; reporting, the incident's own check.
                "attachments.list" | "attachments.get" | "incidents.list" | "incidents.get" => "runs.read",
                "attachments.save" | "attachments.remove" | "attachments.pause" => "agents.attach",
                "incidents.report" => crate::authz::SIGNED_IN,
                // Access: people, grants and custom roles are `users.manage`'s; tokens are each
                // caller's own (anyone's with `tokens.manage`); the audit trail is `audit.read`'s.
                "roles.list" => crate::authz::SIGNED_IN,
                n if n.starts_with("users.") || n.starts_with("grants.") || n.starts_with("roles.") => "users.manage",
                n if n.starts_with("tokens.") => "tokens.manage",
                "audit.list" => "audit.read",
                _ => "agents.run",
            };
            assert_eq!(rule.capability, needs, "{}", op.name);
            assert_eq!(op.read_only, method == Method::GET, "{}: read-only exactly when its route reads", op.name);
            assert!(op.input.schema().get("type").is_some(), "{}", op.name);
        }
        // Listed whether deployments are on or not.
        assert!(listed(false).iter().any(|o| o.name == "agents.wait"));
        assert!(listed(false).iter().any(|o| o.name == "plans.create"));
    }

    /// CP-T3.2 (the twelfth adversarial check's): an operation that takes an idempotency key (or,
    /// a step tried again, the try it replaces) says what a call without it is, and tells its caller
    /// to pass it: an agent that asks again after a lost answer without it is a second prompt, or a
    /// second agent. Every one that ends in a prompt takes one.
    #[test]
    fn a_keyed_operation_tells_its_caller_to_pass_its_key() {
        let mut keyed = Vec::new();
        for op in own() {
            let input = op.input.schema();
            for field in ["key", "try"] {
                let Some(said) = input.pointer(&format!("/properties/{field}/description")).and_then(Value::as_str) else { continue };
                keyed.push(op.name);
                assert!(said.contains("Without it") && said.contains("pass it"), "{}'s `{field}` doesn't tell its caller to pass it: {said}", op.name);
            }
        }
        for name in ["agents.send", "goals.add", "goals.control", "goals.done", "plans.run", "plans.runs.retry"] {
            assert!(keyed.contains(&name), "{name} takes no key");
        }
    }

    /// An agent's own goal stack and change request need no id: left out, `agent` is the agent
    /// calling. One it names stays; a person's input, and other operations', are as they came.
    #[test]
    fn an_agent_s_own_goals_and_change_need_no_id() {
        let mut agent = Caller::with(crate::authz::gate::Kind::Agent, "agent:01a1", None, None);
        agent.agent = Some("01a1".into());
        assert_eq!(yours(&agent, "goals.add", json!({"goals": ["g"]})), json!({"goals": ["g"], "agent": "01a1"}));
        assert_eq!(yours(&agent, "goals.control", json!({"agent": " ", "action": "start"})), json!({"agent": "01a1", "action": "start"}));
        assert_eq!(yours(&agent, "agents.change", json!({})), json!({"agent": "01a1"}));
        assert_eq!(yours(&agent, "goals.get", json!({"agent": "child"})), json!({"agent": "child"}));
        assert_eq!(yours(&agent, "agents.stop", json!({})), json!({}), "ending an agent always names it");
        assert_eq!(yours(&Caller::owner(), "goals.add", json!({"goals": ["g"]})), json!({"goals": ["g"]}));
    }
}
