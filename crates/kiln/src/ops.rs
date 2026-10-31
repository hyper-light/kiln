//! Every operation, once (docs/ci.md W20-R01): deployments' ([`crate::cd::ops`]), releases by
//! API ([`crate::deploy::ops`]), CI's ([`crate::ci::ops`]) and kiln's own: agents'
//! ([`crate::harness::ops`]), plans' ([`crate::plans::ops`]) and goal stacks'
//! ([`crate::goals::ops`]). Each has its name, summary, input schema and whether it changes
//! anything. The MCP tools and code mode's `kiln.*` are this list. Each operation is checked
//! against the route table as its HTTP route is, so a caller may do through them exactly what it
//! may do over HTTP.

mod routes;
pub mod local;
pub use routes::{Definition, Part, Payload, Reply, Routes};

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

/// U12: retain a typed refusal through the same authorization and audit boundary. Protocol
/// adapters render it after dispatch; they never validate or return before authorization.
pub(crate) enum CallError {
    Failed(Fail),
    Misfit(Misfit),
}

pub(crate) type CheckedOut = Result<Value, CallError>;

impl From<Fail> for CallError {
    fn from(failure: Fail) -> Self { Self::Failed(failure) }
}

impl CallError {
    fn status(&self) -> StatusCode {
        match self { Self::Failed((status, _)) => *status, Self::Misfit(_) => StatusCode::BAD_REQUEST }
    }

    fn into_fail(self, name: &str) -> Fail {
        match self { Self::Failed(failure) => failure, Self::Misfit(m) => (StatusCode::BAD_REQUEST, m.said(name)) }
    }
}
pub(crate) type Fut<T = Out> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Who calls.
#[derive(Clone)]
pub struct Cx {
    pub hub: Arc<Hub>,
    /// As the route table sees it; its name is who the audit trail says acted.
    pub caller: Caller,
}

/// U12: protocol adapters retain their context and route encoding, while every domain
/// uses this descriptor and the registry below for discovery and checked dispatch.
pub type Route = fn(&Value) -> Result<(Method, Vec<String>, std::collections::HashMap<String, String>), Fail>;

/// U12: ordinary HTTP aliases carry their parsing policy with their operation.
#[derive(Clone, Copy)]
pub enum Body { Ignore, Json, Text }

pub struct HttpAlias {
    pub method: &'static str,
    pub segments: &'static [&'static str],
    pub body: Body,
    pub input: fn(&[&str], &std::collections::HashMap<String, String>, &Value) -> Out,
}

impl HttpAlias {
    pub fn matches(&self, method: &Method, segments: &[&str]) -> bool {
        self.method == method.as_str() && self.segments.len() == segments.len()
            && self.segments.iter().zip(segments).all(|(pattern, segment)| *pattern == "*" || pattern == segment)
    }
}

pub struct HttpRoutes {
    pub canonical: Route,
    pub aliases: &'static [HttpAlias],
    pub authorization: crate::authz::Rule,
}

/// What an operation is, as lists and tools show it.
pub struct Info {
    pub name: &'static str,
    pub summary: &'static str,
    pub read_only: bool,
    pub destructive: bool,
    pub input: Input,
}

/// U12: one operation descriptor. Context, route and output types adapt domain handlers
/// and protocol encodings, including local commands; schema, effects and handler stay together.
pub struct Op<C = Cx, R = Routes, O = Out> {
    /// Its name, dotted (`agents.start`).
    pub name: &'static str,
    pub summary: &'static str,
    pub read_only: bool,
    pub destructive: bool,
    pub input: Input,
    /// Its transport's routes, or a local-only marker for a platform command.
    pub route: R,
    pub run: fn(C, Value) -> Fut<O>,
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
    pub const fn of<T: DeserializeOwned + JsonSchema>() -> Input {
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
    find_in(registry(), name).ok().map(|operation| operation.info().input)
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
            .map(|mut operation| { operation.route.name(operation.name); operation })
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
    registry().iter().filter(|o| cd || !o.needs_cd()).map(Entry::info).collect()
}

/// U12: one catalog is the source for discovery and dispatch. Domain contexts and
/// legacy route encodings are adapters, never independent name dispatchers.
pub enum Entry {
    Own(&'static Op),
    Routed(&'static Op),
    Cd(&'static crate::cd::ops::Op),
}

impl Entry {
    pub fn info(&self) -> Info {
        macro_rules! info { ($o:expr) => { Info { name: $o.name, summary: $o.summary, read_only: $o.read_only, destructive: $o.destructive, input: $o.input } }; }
        match self { Self::Own(o) => info!(o), Self::Routed(o) => info!(o), Self::Cd(o) => info!(o) }
    }

    fn needs_cd(&self) -> bool {
        matches!(self, Self::Cd(_)) || self.info().name.starts_with("deploy.")
    }

    pub fn route(&self, input: &Value) -> Result<(Method, Vec<String>, std::collections::HashMap<String, String>), Fail> {
        match self {
            Self::Own(o) => o.route.build(input),
            Self::Routed(o) => o.route.build(input),
            Self::Cd(o) => (o.route.canonical)(input),
        }
    }

    pub fn definitions(&self) -> &[Definition] {
        match self { Self::Own(op) | Self::Routed(op) => &op.route.0, Self::Cd(_) => &[] }
    }

    pub fn policy(&self, input: &Value) -> Result<&crate::authz::Rule, Fail> {
        match self {
            Self::Own(op) | Self::Routed(op) => Ok(&op.route.selected(input)?.authorization),
            Self::Cd(op) => Ok(&op.route.authorization),
        }
    }

    async fn run(&self, cx: Cx, input: Value) -> Out {
        match self {
            Self::Own(o) => (o.run)(cx, input).await,
            Self::Routed(o) => (o.run)(cx, input).await,
            Self::Cd(o) => {
                let cd = crate::cd::get().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "deployments aren't on here, or are starting ([cd] enabled)".to_owned()))?;
                (o.run)(crate::cd::ops::Cx { hub: cx.hub, cd, actor: cx.caller.name }, input).await
            }
        }
    }
}

pub fn registry() -> &'static [Entry] {
    static REGISTRY: std::sync::OnceLock<Vec<Entry>> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| {
        crate::cd::ops::ops().iter().map(Entry::Cd)
            .chain(crate::deploy::ops::ops().iter().map(Entry::Routed))
            .chain(crate::ci::ops::ops().iter().map(Entry::Routed))
            .chain(own().iter().map(Entry::Own)).collect()
    })
}

/// Refuse an ambiguous catalog rather than silently dispatching the first declaration.
fn find_in<'a>(entries: &'a [Entry], name: &str) -> Result<&'a Entry, Fail> {
    let mut found = entries.iter().filter(|o| o.info().name == name);
    let op = found.next().ok_or_else(|| (StatusCode::NOT_FOUND, format!("no operation {name}")))?;
    if found.next().is_some() {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("duplicate operation {name}")));
    }
    Ok(op)
}

/// U12: a selected HTTP spelling of an operation, with decoded path bindings.
pub struct HttpMatch {
    pub operation: &'static Entry,
    pub definition: &'static Definition,
    pub bindings: Vec<(&'static str, String)>,
}

pub fn match_http(method: &Method, path: &[&str]) -> Result<Option<HttpMatch>, Fail> {
    let mut selected: Option<HttpMatch> = None;
    for operation in registry() {
        for definition in operation.definitions().iter().filter(|d| d.primary) {
            let Some(bindings) = definition.bindings(method, path) else { continue };
            if let Some(previous) = &selected {
                match definition.specificity().cmp(&previous.definition.specificity()) {
                    std::cmp::Ordering::Less => continue,
                    std::cmp::Ordering::Equal => return Err((StatusCode::INTERNAL_SERVER_ERROR, "ambiguous operation HTTP route".to_owned())),
                    std::cmp::Ordering::Greater => {}
                }
            }
            selected = Some(HttpMatch { operation, definition, bindings });
        }
    }
    Ok(selected)
}

pub fn http_policy(method: &Method, path: &[&str]) -> Option<&'static crate::authz::Rule> {
    match_http(method, path).ok().flatten().map(|route| &route.definition.authorization)
}

/// Protocol-only routes continue through the existing binary/streaming adapters.
pub fn owns_http(method: &Method, path: &[&str]) -> bool {
    if let ["api", "cd", rest @ ..] = path {
        if let ["ops", name] = rest { return crate::cd::ops::ops().iter().any(|op| op.name == *name) && method == Method::POST; }
        return crate::cd::ops::http_alias(method, rest).is_ok();
    }
    match match_http(method, path) {
        Ok(Some(route)) => !matches!(route.definition.reply, Reply::Protocol),
        Ok(None) => false,
        Err(_) => true,
    }
}

async fn checked(cx: Cx, op: &Entry, input: Value) -> CheckedOut {
    let (method, segs, q) = op.route(&input)?;
    let decoded: Vec<&str> = segs.iter().map(String::as_str).collect();
    crate::authz::gate::check_in(&cx.hub, &cx.caller, &method, &decoded, &q, op.policy(&input)?).await?;
    run_reached(cx, op, input, &decoded).await
}

async fn run_reached(cx: Cx, op: &Entry, input: Value, decoded: &[&str]) -> CheckedOut {
    if let ["api", "harness", "sessions", agent, ..] = decoded {
        if op.info().read_only { crate::harness::lineage::may_see(&cx.hub, &cx.caller, agent).await?; }
        else { crate::harness::lineage::may_steer(&cx.hub, &cx.caller, agent).await?; }
    }
    // U12: every transport validates the same typed descriptor after authorization and
    // before the domain may accept durable work or perform its effect.
    let info = op.info();
    info.input.fits(&input).map_err(CallError::Misfit)?;
    op.run(cx, input).await.map_err(CallError::Failed)
}

/// The shared HTTP adapter: parse inside the operation's audit boundary, check the
/// same route as MCP/code mode, and preserve explicitly declared response adapters.
pub async fn http(cx: Cx, method: Method, path: &[&str], q: &std::collections::HashMap<String, String>, req: crate::http::server::Request) -> Result<crate::http::server::Response, Fail> {
    use crate::http::server as srv;
    let matched = match_http(&method, path)?;
    let cd = if let ["api", "cd", rest @ ..] = path { Some(rest) } else { None };
    let (op, definition, bindings, cd_alias) = if let Some(route) = matched {
        (route.operation, Some(route.definition), route.bindings, None)
    } else if let Some(rest) = cd {
        if let ["ops", name] = rest { (find_in(registry(), name)?, None, Vec::new(), None) }
        else {
            let (op, alias) = crate::cd::ops::http_alias(&method, rest)?;
            (find_in(registry(), op.name)?, None, Vec::new(), Some(alias))
        }
    } else { return Err((StatusCode::NOT_FOUND, "no operation HTTP route".to_owned())); };
    let info = op.info();
    let policy = match definition { Some(definition) => &definition.authorization, None => op.policy(&Value::Null)? };
    let key = req.headers().get("idempotency-key").and_then(|v| v.to_str().ok()).map(str::to_owned);
    let ip = srv::remote_ip(&req);
    let target = req.uri().path().to_owned();
    let run = async {
        // Refusals happen before parsing a caller's body and remain inside the
        // operation's one audit boundary, exactly as for the legacy HTTP API.
        crate::authz::gate::check_in(&cx.hub, &cx.caller, &method, path, q, policy).await?;
        let schema = info.input.schema();
        let text_body = matches!(definition.map(|d| d.payload), Some(Payload::Text(_))) || cd_alias.is_some_and(|a| matches!(a.body, Body::Text));
        let ignore_body = matches!(method, Method::GET | Method::HEAD | Method::DELETE) || definition.is_some_and(|d| matches!(d.payload, Payload::Ignored)) || cd_alias.is_some_and(|a| matches!(a.body, Body::Ignore));
        let body = if ignore_body { json!({}) } else {
            let bytes = srv::body_bytes(req.into_body(), definition.map_or(8 << 20, |d| d.limit)).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            if text_body { json!(String::from_utf8_lossy(&bytes)) }
            else if bytes.is_empty() || definition.is_some_and(|d| matches!(d.payload, Payload::EmptyObject)) && bytes.iter().all(u8::is_ascii_whitespace) { json!({}) }
            else { serde_json::from_slice(&bytes).map_err(|e| (StatusCode::BAD_REQUEST, format!("the body is not JSON: {e}")))? }
        };
        let mut input = if let Some(definition) = definition { definition.input(&bindings, q, body, &schema)? }
        else if let Some(alias) = cd_alias { (alias.input)(cd.unwrap_or_default(), q, &body)? }
        else { body };
        if let (Some(key), Some(object)) = (key, input.as_object_mut()) {
            for field in ["key", "idempotency_key"] {
                if schema.get("properties").is_some_and(|p| p.get(field).is_some()) && !object.contains_key(field) { object.insert(field.to_owned(), json!(key)); }
            }
        }
        let input = yours(&cx.caller, info.name, input);
        let (_, reached, _) = op.route(&input)?;
        let reached: Vec<&str> = reached.iter().map(String::as_str).collect();
        let value = run_reached(cx.clone(), op, input, &reached).await.map_err(|error| error.into_fail(info.name))?;
        match definition.map(|d| d.reply).unwrap_or(Reply::Json(200)) {
            Reply::Empty => Ok(srv::status(StatusCode::NO_CONTENT)),
            Reply::Yaml(field) => {
                let text = value.get(field).and_then(Value::as_str).ok_or_else(|| (StatusCode::INTERNAL_SERVER_ERROR, format!("{} did not return {field}", info.name)))?;
                Ok(srv::response(StatusCode::OK, "application/yaml", srv::full(text.to_owned())))
            }
            Reply::JsonField(code, field) => {
                let status = StatusCode::from_u16(code).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
                let answer = value.get(field).ok_or_else(|| (StatusCode::INTERNAL_SERVER_ERROR, format!("{} did not return {field}", info.name)))?;
                Ok(srv::json(status, answer))
            }
            Reply::Json(code) => {
                let status = StatusCode::from_u16(code).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
                Ok(if value.is_null() { srv::status(status) } else { srv::json(status, &value) })
            }
            Reply::Protocol => Err((StatusCode::INTERNAL_SERVER_ERROR, "a protocol adapter must handle this operation".to_owned())),
        }
    };
    if info.read_only { return run.await; }
    crate::audit::request_in(&cx.hub, crate::audit::Req { caller: &cx.caller, method: &method, path: &target, q, ip }, policy, path, run, |out: &Result<crate::http::server::Response, Fail>| {
        match out { Ok(response) => response.status().as_u16(), Err((status, _)) => status.as_u16() }
    }).await
}

/// U12: HTTP documentation is another projection of the dispatch catalog.
pub fn document_http(paths: &mut serde_json::Map<String, Value>, listed: &[Info]) {
    for operation in registry() {
        let info = operation.info();
        if !listed.iter().any(|o| o.name == info.name) { continue; }
        for route in operation.definitions().iter().filter(|route| route.primary) {
            let segments: Vec<String> = route.parts.iter().map(|part| match part {
                Part::Literal(value) => (*value).to_owned(),
                _ => format!("{{{}}}", part.key().unwrap_or("id")),
            }).collect();
            let path = format!("/{}", segments.join("/"));
            let mut parameters = Vec::new();
            for part in route.parts {
                if let Some(key) = part.key() {
                    parameters.push(json!({"name": key, "in": "path", "required": true, "schema": {"type": "string"}, "x-kiln-path-tail": matches!(part, Part::Tail(_))}));
                }
            }
            let mut input = info.input.schema();
            let bound: Vec<&str> = route.parts.iter().filter_map(Part::key).chain(route.query.iter().copied()).collect();
            if let Some(properties) = input.get_mut("properties").and_then(Value::as_object_mut) {
                for key in &bound { properties.remove(*key); }
            }
            if let Some(required) = input.get_mut("required").and_then(Value::as_array_mut) {
                required.retain(|key| key.as_str().is_none_or(|key| !bound.contains(&key)));
            }
            let input = match route.payload {
                Payload::Wrapped(key) => info.input.schema().get("properties").and_then(|p| p.get(key)).cloned().unwrap_or_else(|| json!({})),
                Payload::Text(_) => json!({"type": "string"}),
                Payload::Object | Payload::EmptyObject | Payload::Ignored => input,
            };
            let code = match route.reply { Reply::Json(code) | Reply::JsonField(code, _) => code, Reply::Empty => 204, _ => 200 };
            let mut response = json!({
                "summary": info.summary,
                "x-kiln-operation": info.name,
                "x-kiln-canonical-route": format!("/api/ops/{}", info.name),
                "x-kiln-read-only": info.read_only,
                "x-kiln-destructive": info.destructive,
                "x-kiln-capability": route.authorization.capability,
                "x-kiln-scope": format!("{:?}", route.authorization.at),
                "parameters": parameters,
                "security": [{"bearer": []}, {"session": []}],
                "responses": {code.to_string(): {"description": "its answer"}, "403": {"description": "the operation's capability is missing"}}
            });
            if !matches!(route.method, "GET" | "HEAD" | "DELETE") {
                let content = if matches!(route.payload, Payload::Text(_)) { "text/plain" } else { "application/json" };
                if let Some(object) = response.as_object_mut() {
                    object.insert("requestBody".to_owned(), json!({"required": false, "content": {content: {"schema": input}}}));
                }
            }
            if let Some(methods) = paths.entry(path).or_insert_with(|| json!({})).as_object_mut() {
                methods.insert(route.method.to_ascii_lowercase(), response);
            }
        }
    }
}

/// All registered policies, including named specializations, for conformance checks.
pub fn policies() -> Vec<&'static crate::authz::Rule> {
    registry().iter().flat_map(|operation| match operation {
        Entry::Cd(op) => vec![&op.route.authorization],
        _ => operation.definitions().iter().map(|route| &route.authorization).collect(),
    }).collect()
}

pub fn authorization_metadata(name: &str) -> Value {
    match find_in(registry(), name) {
        Ok(Entry::Cd(op)) => json!([{"capability": op.route.authorization.capability, "scope": op.route.authorization.at}]),
        Ok(operation) => json!(operation.definitions().iter().map(|route| json!({"capability": route.authorization.capability, "scope": route.authorization.at})).collect::<Vec<_>>()),
        Err(_) => Value::Null,
    }
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
    call_checked(cx, name, input).await.map_err(|error| error.into_fail(name))
}

/// U12: the same dispatch as call, preserving typed refusal details for protocol adapters.
pub(crate) async fn call_checked(cx: Cx, name: &str, input: Value) -> CheckedOut {
    let input = yours(&cx.caller, name, if input.is_null() { json!({}) } else { input });
    let op = find_in(registry(), name)?;
    let policy = op.policy(&input)?;
    let (method, segs, q) = op.route(&input)?;
    let decoded: Vec<&str> = segs.iter().map(String::as_str).collect();
    let path = format!("/{}", segs.iter().map(|s| s.replace('%', "%25").replace('/', "%2F")).collect::<Vec<_>>().join("/"));
    let run = checked(cx.clone(), op, input);
    if op.info().read_only {
        return run.await;
    }
    crate::audit::request_in(&cx.hub, crate::audit::Req { caller: &cx.caller, method: &method, path: &path, q: &q, ip: None }, policy, &decoded, run, |r: &CheckedOut| match r {
        Ok(_) => 200,
        Err(error) => error.status().as_u16(),
    }).await
}

/// Runs one of kiln's own operations for its HTTP route, which the route table has let through
/// and the audit trail records already.
pub async fn run_own(cx: Cx, name: &str, input: Value) -> Out {
    let op = own().iter().find(|o| o.name == name).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no operation {name}")))?;
    op.input.fits(&input).map_err(|m| (StatusCode::BAD_REQUEST, m.said(name)))?;
    (op.run)(cx, input).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u12_registry_is_complete_and_refuses_duplicate_dispatch() -> Result<(), Fail> {
        let entries = registry();
        assert_eq!(entries.len(), own().len() + crate::ci::ops::ops().len() + crate::deploy::ops::ops().len() + crate::cd::ops::ops().len());
        for entry in entries {
            let name = entry.info().name;
            assert_eq!(find_in(entries, name)?.info().name, name);
        }
        let first = own().first().ok_or_else(|| (StatusCode::INTERNAL_SERVER_ERROR, "empty own catalog".to_owned()))?;
        let duplicate = [Entry::Own(first), Entry::Own(first)];
        assert!(matches!(find_in(&duplicate, first.name), Err((StatusCode::INTERNAL_SERVER_ERROR, _))));
        assert!(matches!(find_in(entries, "absent.operation"), Err((StatusCode::NOT_FOUND, _))));
        assert_eq!(listed(false).len(), own().len() + crate::ci::ops::ops().len());
        assert_eq!(listed(true).len(), entries.len());
        Ok(())
    }

    #[test]
    fn u12_bidirectional_routes_are_unambiguous_and_preserve_path_scope() -> Result<(), Fail> {
        for operation in registry() {
            for definition in operation.definitions().iter().filter(|d| d.primary) {
                let path: Vec<&str> = definition.parts.iter().map(|part| match part { Part::Literal(value) => *value, _ => "sample" }).collect();
                let method = Method::from_bytes(definition.method.as_bytes()).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
                let matched = match_http(&method, &path)?.ok_or_else(|| (StatusCode::INTERNAL_SERVER_ERROR, operation.info().name.to_owned()))?;
                assert_eq!(matched.operation.info().name, operation.info().name, "{} {}", definition.method, path.join("/"));
            }
        }
        let path = ["api", "ci", "repos", "acme/app", "workflows", ".github", "workflows", "ci.yml"];
        let matched = match_http(&Method::GET, &path)?.ok_or_else(|| (StatusCode::NOT_FOUND, "workflow route".to_owned()))?;
        let input = matched.definition.input(&matched.bindings, &Default::default(), Value::Null, &matched.operation.info().input.schema())?;
        assert_eq!(input.get("repo"), Some(&json!("acme/app")));
        assert_eq!(input.get("path"), Some(&json!(".github/workflows/ci.yml")));
        assert_eq!(matched.definition.authorization.at, crate::authz::At::Repo(3));
        let (_, built, _) = matched.operation.route(&input)?;
        assert_eq!(built.get(3).map(String::as_str), Some("acme/app"));
        Ok(())
    }

    #[test]
    fn u12_http_body_and_stream_adapters_preserve_legacy_shapes() -> Result<(), Fail> {
        let apps = match_http(&Method::POST, &["api", "deploy", "apps"])?.ok_or_else(|| (StatusCode::NOT_FOUND, "app create".to_owned()))?;
        let body = json!({"name": "web", "project": "shop"});
        assert_eq!(apps.definition.input(&apps.bindings, &Default::default(), body.clone(), &apps.operation.info().input.schema())?, json!({"app": body}));
        assert!(matches!(apps.definition.reply, Reply::Json(201)));
        let reaper = match_http(&Method::PUT, &["api", "ci", "work", "reap", "policies"])?.ok_or_else(|| (StatusCode::NOT_FOUND, "reaper policy".to_owned()))?;
        let query = std::collections::HashMap::from([("scope".to_owned(), "server".to_owned())]);
        assert_eq!(reaper.definition.input(&reaper.bindings, &query, json!({"keep_last": 3}), &reaper.operation.info().input.schema())?, json!({"policy": {"keep_last": 3}, "scope": "server"}));
        for path in [vec!["api", "ci", "legs", "leg", "logs"], vec!["api", "diffs", "D1", "versions", "1", "blob"], vec!["api", "ci", "runs", "run", "artifacts", "7", "download"]] {
            assert!(!owns_http(&Method::GET, &path), "the original protocol adapter owns {}", path.join("/"));
        }
        Ok(())
    }

    #[test]
    fn u12_artifact_download_keeps_http_redirect_and_named_link_with_matching_refusals() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            use crate::http::server::Handler as _;
            use http_body_util::BodyExt as _;
            let hub = crate::hub::tests::scratch_hub().await?;
            let repo = hub.store.upsert_repo(crate::store::Repo {
                id: "artifact-repo".into(), name: "acme/app".into(), clone_url: "https://example.com/acme/app.git".into(),
                branches: Vec::new(), poll_seconds: 60, enabled: true, created_at: 0, last_polled_at: None, last_error: None,
                mode: "upstream".into(), default_branch: None, mirror: None,
            }).await?;
            hub.store.call(move |c| {
                // U12: runs_api serves workflow-backed runs; legacy rows are filtered out.
                c.execute("INSERT INTO ci_workflows (id, repo_id, path, content_key, dialect, name, created_at) VALUES ('artifact-workflow', ?1, '.github/workflows/ci.yml', 'u12-fixture', 'github-actions', 'ci', 0)", crate::sql::params![repo.id.clone()])?;
                c.execute("INSERT INTO runs (id, repo_id, workflow_id, number, pipeline, event, branch, sha, status, created_at) VALUES ('artifact-run', ?1, 'artifact-workflow', 1, 'ci', 'push', 'main', 'abc', 'completed', 0)", crate::sql::params![repo.id])?;
                c.execute("INSERT INTO ci_artifacts (id, run_id, name, object, created_at, finalized) VALUES (7, 'artifact-run', 'live', 'fixture-object', 0, 1), (8, 'artifact-run', 'expired', NULL, 0, 1)", [])?;
                Ok(())
            }).await?;
            let api = Arc::new(crate::api::Api { hub: Arc::clone(&hub) });
            for (label, grant, allowed) in [
                ("artifact-reader", "artifacts.read@repo:acme/app", true),
                ("run-reader", "runs.read@repo:acme/app", false),
                ("other-reader", "artifacts.read@repo:acme/other", false),
            ] {
                let (_, token) = crate::authz::tokens::mint_agent(&hub, label, "owner", &[grant], None, "U12 download parity").await?;
                let caller = crate::authz::gate::caller(&hub, Some(&token), None).await.ok_or_else(|| anyhow::anyhow!("artifact caller"))?;
                for (artifact, expected) in [("7", StatusCode::FOUND), ("8", StatusCode::GONE), ("missing", StatusCode::NOT_FOUND)] {
                    let path = ["api", "ci", "runs", "artifact-run", "artifacts", artifact, "download"];
                    let policy = crate::authz::rule(&Method::GET, &path);
                    assert_eq!((policy.capability, policy.at), ("artifacts.read", crate::authz::At::Run(3)));
                    let mut request = http::Request::builder().uri(format!("/{}", path.join("/")))
                        .header(http::header::AUTHORIZATION, format!("Bearer {token}")).body(kiln_http::Incoming::empty())?;
                    request.extensions_mut().insert(crate::http::server::Secure);
                    let response = Arc::clone(&api).handle(request).await;
                    let status = response.status();
                    let location = response.headers().get(http::header::LOCATION).map(|value| value.to_str().map(str::to_owned)).transpose()?;
                    let bytes = response.into_body().collect().await?.to_bytes();
                    let named = call(Cx { hub: Arc::clone(&hub), caller: caller.clone() }, "ci.runs.artifact", json!({"run": "artifact-run", "artifact": artifact})).await;
                    assert_eq!(status, if allowed { expected } else { StatusCode::FORBIDDEN }, "{label} artifact {artifact}: HTTP body {}; named result {named:?}", String::from_utf8_lossy(&bytes));
                    if status == StatusCode::FOUND {
                        let value = named.map_err(|(_, why)| anyhow::anyhow!(why))?;
                        let prefix = format!("{}/_kiln/blob/artifact-7?", hub.cfg.server.public_url.trim_end_matches('/'));
                        assert!(location.as_deref().is_some_and(|url| url.starts_with(&prefix)));
                        assert!(value.get("url").and_then(Value::as_str).is_some_and(|url| url.starts_with(&prefix)));
                        assert_eq!(value.get("expires_in"), Some(&json!(60)));
                        assert!(bytes.is_empty(), "HTTP keeps the redirect instead of returning named-operation JSON");
                    } else {
                        let (named_status, why) = named.err().ok_or_else(|| anyhow::anyhow!("expected an artifact refusal"))?;
                        assert_eq!(status, named_status);
                        assert_eq!(serde_json::from_slice::<Value>(&bytes)?.get("error"), Some(&json!(why)));
                    }
                }
            }
            Ok(())
        })
    }

    #[test]
    fn u12_http_and_named_operations_share_results_denials_and_one_audit_row() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            use http_body_util::BodyExt as _;
            let hub = crate::hub::tests::scratch_hub().await?;
            let baseline: i64 = hub.store.call(|c| Ok(c.query_row("SELECT count(*) FROM audit2", [], |row| row.get(0))?)).await?;
            let owner = Cx { hub: Arc::clone(&hub), caller: Caller::owner() };
            let read = call(owner.clone(), "plans.list", json!({})).await.map_err(|(_, why)| anyhow::anyhow!(why))?;
            let request = http::Request::builder().uri("/api/plans").body(kiln_http::Incoming::empty())?;
            let response = super::http(owner.clone(), Method::GET, &["api", "plans"], &Default::default(), request).await.map_err(|(_, why)| anyhow::anyhow!(why))?;
            let body = response.into_body().collect().await?.to_bytes();
            assert_eq!(serde_json::from_slice::<Value>(&body)?, read);
            // U12: a typed refusal reaches no domain mutation, even for an authorized caller.
            assert!(matches!(call_checked(owner.clone(), "plans.create", json!({"title": "must not be accepted", "unexpected": true})).await, Err(CallError::Misfit(m)) if m.code() == "unknown_field" && m.path == "/unexpected"));
            let unchanged = call(owner.clone(), "plans.list", json!({})).await.map_err(|(_, why)| anyhow::anyhow!(why))?;
            assert_eq!(unchanged, read);
            let created = call(owner.clone(), "plans.create", json!({"title": "U12 test", "body": "1. Verify unified dispatch"})).await.map_err(|(_, why)| anyhow::anyhow!(why))?;
            let id = created.get("id").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("plan id"))?;
            let request = http::Request::builder().method(Method::DELETE).uri(format!("/api/plans/{id}")).body(kiln_http::Incoming::empty())?;
            let response = super::http(owner, Method::DELETE, &["api", "plans", id], &Default::default(), request).await.map_err(|(_, why)| anyhow::anyhow!(why))?;
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
            let reader = Caller::with(crate::authz::gate::Kind::User, "reader", Some(crate::authz::Rights {
                grants: vec![crate::authz::Grant::parse("runs.read").map_err(anyhow::Error::msg)?], custom: Default::default(),
            }), None);
            let denied = Cx { hub: Arc::clone(&hub), caller: reader };
            assert!(matches!(call_checked(denied.clone(), "plans.create", json!({"title": "denied", "unexpected": true})).await, Err(CallError::Failed((StatusCode::FORBIDDEN, _)))));
            let request = http::Request::builder().method(Method::POST).uri("/api/plans").body(kiln_http::Incoming::empty())?;
            assert!(matches!(super::http(denied, Method::POST, &["api", "plans"], &Default::default(), request).await, Err((StatusCode::FORBIDDEN, _))));
            let count: i64 = hub.store.call(|c| Ok(c.query_row("SELECT count(*) FROM audit2", [], |row| row.get(0))?)).await?;
            assert_eq!(count - baseline, 5, "two successful changes, two denied changes and one typed refusal, with no read or nested handler rows");
            Ok(())
        })
    }

    /// Each of kiln's own operations is a route the table names (none falls through to the
    /// administrators' `settings.manage`), needs what its route needs (agents' `agents.run`,
    /// runners' `runners.read` or `runners.manage`), and reads exactly when its route is a GET.
    #[test]
    fn kiln_s_own_operations_are_routes_of_the_table() {
        let probe = json!({"agent": "a1", "id": "p1", "run": "r1", "step": 1, "action": "start", "text": "x", "goals": ["g"], "runner": "mac", "policy": "auto", "attachment": "ci-doctor", "incident": "i1", "user": "ada", "grant": "g1", "name": "auditor", "token": "t1", "program": "p1", "track": "T1"});
        assert!(own().iter().any(|o| o.name == "agents.start") && own().iter().any(|o| o.name == "plans.run") && own().iter().any(|o| o.name == "goals.add"));
        for op in own() {
            let route = op.route.build(&probe);
            assert!(route.is_ok(), "{}", op.name);
            let Ok((method, segs, _)) = route else { continue };
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
