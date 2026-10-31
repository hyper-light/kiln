//! U12: bidirectional operation routes. A binding names one input field in both
//! directions; authorization and HTTP adaptation belong to that same declaration.
use std::collections::HashMap;

use http::{Method, StatusCode};
use serde_json::{Value, json};

use super::{Fail, Out};
use crate::authz::{At, Rule};

#[derive(Clone, Copy)]
pub enum Part {
    Literal(&'static str),
    Field(&'static str),
    Default(&'static str, &'static str),
    Number(&'static str),
    Unsigned(&'static str),
    Nested(&'static str),
    Tail(&'static str),
}

impl Part {
    pub fn key(&self) -> Option<&'static str> {
        match self {
            Self::Literal(_) => None,
            Self::Field(k) | Self::Default(k, _) | Self::Number(k) | Self::Unsigned(k) | Self::Nested(k) | Self::Tail(k) => Some(k),
        }
    }

    fn value(&self, input: &Value) -> Result<String, Fail> {
        let bad = |k| (StatusCode::BAD_REQUEST, format!("`{k}` is required"));
        match self {
            Self::Literal(s) => Ok((*s).to_owned()),
            Self::Field(k) | Self::Tail(k) => input.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned).ok_or_else(|| bad(k)),
            Self::Default(k, default) => Ok(input.get(k).and_then(Value::as_str).unwrap_or(default).to_owned()),
            Self::Nested(k) => input.get(k).and_then(|v| v.get("name")).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| bad(k)),
            Self::Number(k) => input.get(k).and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).map(|n| n.to_string()).ok_or_else(|| bad(k)),
            Self::Unsigned(k) => input.get(k).and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).map(|n| n.to_string()).ok_or_else(|| bad(k)),
        }
    }
}

#[derive(Clone, Copy)]
pub enum Payload {
    Object,
    /// Legacy JSON routes that accept an absent or whitespace-only body as an empty object.
    EmptyObject,
    Ignored,
    Wrapped(&'static str),
    Text(&'static str),
}

#[derive(Clone, Copy)]
pub enum Reply {
    Json(u16),
    /// U12: keep a legacy HTTP answer while named calls retain the whole domain result.
    JsonField(u16, &'static str),
    Empty,
    Yaml(&'static str),
    /// Binary downloads, streaming and other protocol-owned responses stay adapters.
    Protocol,
}

pub struct Definition {
    pub method: &'static str,
    pub parts: &'static [Part],
    pub query: &'static [&'static str],
    pub authorization: Rule,
    pub payload: Payload,
    pub limit: usize,
    pub invalid_path: StatusCode,
    pub reply: Reply,
    /// A specialized named operation can share the general operation's URL.
    pub primary: bool,
    /// Select the canonical spelling according to whether this input field is present.
    pub when: Option<(&'static str, bool)>,
}

impl Definition {
    pub fn new(method: &'static str, parts: &'static [Part], query: &'static [&'static str], capability: &'static str, at: At) -> Self {
        Self { method, parts, query, authorization: Rule { method: Some(method), pattern: "/api/ops/*", capability, at }, payload: Payload::Object, limit: 4 << 20, invalid_path: StatusCode::BAD_REQUEST, reply: Reply::Json(200), primary: true, when: None }
    }

    pub fn path_error(mut self, status: StatusCode) -> Self { self.invalid_path = status; self }
    pub fn limit(mut self, limit: usize) -> Self { self.limit = limit; self }
    pub fn payload(mut self, payload: Payload) -> Self { self.payload = payload; self }
    pub fn reply(mut self, reply: Reply) -> Self { self.reply = reply; self }
    pub fn named_only(mut self) -> Self { self.primary = false; self }
    pub fn when(mut self, field: &'static str, present: bool) -> Self { self.when = Some((field, present)); self }

    fn accepts(&self, input: &Value) -> bool {
        self.when.is_none_or(|(key, present)| input.get(key).is_some_and(|v| !v.is_null()) == present)
    }

    pub fn build(&self, input: &Value) -> Result<(Method, Vec<String>, HashMap<String, String>), Fail> {
        let method = Method::from_bytes(self.method.as_bytes()).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let path = self.parts.iter().map(|part| part.value(input)).collect::<Result<Vec<_>, _>>()?;
        let query = self.query.iter().filter_map(|key| input.get(key).and_then(scalar).map(|value| ((*key).to_owned(), value))).collect();
        Ok((method, path, query))
    }

    /// Decoded path bindings, including a workflow path spanning several segments.
    pub fn bindings(&self, method: &Method, path: &[&str]) -> Option<Vec<(&'static str, String)>> {
        if self.method != method.as_str() { return None; }
        let tail = self.parts.iter().position(|p| matches!(p, Part::Tail(_)));
        if tail.is_none() && path.len() != self.parts.len() || tail.is_some() && path.len() < self.parts.len() { return None; }
        let mut bindings = Vec::new();
        for (i, part) in self.parts.iter().enumerate() {
            let index = if tail.is_some_and(|tail| i > tail) { path.len().checked_sub(self.parts.len().checked_sub(i)?)? } else { i };
            let value = match part {
                Part::Tail(_) => {
                    let end = path.len().checked_sub(self.parts.len().checked_sub(i + 1)?)?;
                    path.get(i..end)?.join("/")
                }
                _ => (*path.get(index)?).to_owned(),
            };
            match part {
                Part::Literal(expected) if *expected != value => return None,
                Part::Literal(_) => {}
                _ => bindings.push((part.key()?, value)),
            }
        }
        Some(bindings)
    }

    pub fn specificity(&self) -> usize { self.parts.iter().filter(|p| matches!(p, Part::Literal(_))).count() }

    pub fn input(&self, bindings: &[(&str, String)], query: &HashMap<String, String>, body: Value, schema: &Value) -> Out {
        let mut input = match self.payload {
            Payload::Object | Payload::Ignored => if body.is_null() { json!({}) } else { body },
            Payload::EmptyObject => body,
            Payload::Wrapped(key) => json!({key: body}),
            Payload::Text(key) => json!({key: body}),
        };
        let object = input.as_object_mut().ok_or_else(|| (StatusCode::BAD_REQUEST, "the request is a JSON object".to_owned()))?;
        // Query parameters belong to the protocol adapter; path bindings always win.
        for (key, value) in query {
            if self.method != "GET" && !self.query.contains(&key.as_str()) { continue; }
            let kind = schema.get("properties").and_then(|p| p.get(key)).and_then(|p| p.get("type")).and_then(Value::as_str);
            let value = match kind {
                Some("integer") => value.parse::<i64>().map(Value::from).unwrap_or_else(|_| json!(value)),
                Some("boolean") => json!(matches!(value.as_str(), "true" | "1")),
                _ => json!(value),
            };
            object.insert(key.clone(), value);
        }
        for (key, value) in bindings {
            let part = self.parts.iter().find(|p| p.key() == Some(*key));
            if matches!(part, Some(Part::Nested(_))) {
                let name = object.get(*key).and_then(|v| v.get("name")).and_then(Value::as_str);
                if name != Some(value.as_str()) { return Err((StatusCode::BAD_REQUEST, format!("the body's {key}.name must match the path {value}"))); }
            } else {
                let value = match part {
                    Some(Part::Number(_)) => value.parse::<i64>().map(Value::from).map_err(|_| (self.invalid_path, format!("`{key}` must be an integer")))?,
                    Some(Part::Unsigned(_)) => value.parse::<u64>().map(Value::from).map_err(|_| (self.invalid_path, format!("`{key}` must be a non-negative integer")))?,
                    _ => json!(value),
                };
                object.insert((*key).to_owned(), value);
            }
        }
        Ok(input)
    }
}

fn scalar(value: &Value) -> Option<String> {
    match value { Value::String(s) => Some(s.clone()), Value::Number(n) => Some(n.to_string()), Value::Bool(b) => Some(b.to_string()), _ => None }
}

pub struct Routes(pub Vec<Definition>);

impl Routes {
    pub fn limit(&mut self, limit: usize) {
        for route in &mut self.0 { if route.limit == 4 << 20 { route.limit = limit; } }
    }

    pub fn name(&mut self, name: &'static str) {
        for route in &mut self.0 { route.authorization.pattern = name; }
    }

    pub fn selected(&self, input: &Value) -> Result<&Definition, Fail> {
        self.0.iter().find(|route| route.accepts(input)).ok_or_else(|| (StatusCode::BAD_REQUEST, "no route for this operation input".to_owned()))
    }

    pub fn build(&self, input: &Value) -> Result<(Method, Vec<String>, HashMap<String, String>), Fail> { self.selected(input)?.build(input) }
}
