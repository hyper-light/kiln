//! JSON-RPC 2.0 as kiln's MCP servers read it (docs/wire.md, "What can't be read"): the server's
//! `/api/mcp`, and a runner's own (`kiln reach mcp`, `crate::reach::mcp`). Each message is read into
//! a [`Request`], or answered with the error JSON-RPC 2.0 gives what it is: -32700 for what isn't
//! JSON, -32600 for what isn't a request (under a null id when its id can't be read), -32601 for a
//! method the server doesn't have, -32602 for params that don't fit it (and, as MCP 2025-11-25
//! has it, for a tool it doesn't have). A valid notification is never answered, though one no client
//! sends is noted. In a batch (the MCP versions before 2025-06-18 send them), each message is
//! answered as its own.

use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::wire::unread::Code;

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// The notifications a client sends a server in the MCP versions kiln speaks (2024-11-05 to
/// 2025-11-25; `tasks/status` from a client that takes tasks). Any other is noted `unknown`
/// ([`Request::unknown_notification`]), though never answered.
pub const CLIENT_NOTIFICATIONS: [&str; 5] = ["notifications/initialized", "notifications/cancelled", "notifications/progress", "notifications/roots/list_changed", "notifications/tasks/status"];

/// A list's params (`tools/list`, `resources/list`, …): where to go on from. kiln's lists come
/// whole, so it only checks one is a cursor.
#[derive(Debug, serde::Deserialize)]
pub struct Paginated {
    #[serde(default)]
    pub cursor: Option<String>,
}

/// The most a message (an HTTP request's body, a line a runner's server reads) may be: one past it
/// is refused whole, never read ([`too_large`]).
pub const BODY_MOST: usize = 16 << 20;

/// `initialize`'s params, as far as kiln reads them (a client says more).
#[derive(Debug, serde::Deserialize)]
pub struct Initialize {
    #[serde(default, rename = "protocolVersion")]
    pub protocol_version: Option<String>,
}

/// `tools/call`'s params.
#[derive(Debug, serde::Deserialize)]
pub struct ToolsCall {
    pub name: String,
    #[serde(default)]
    pub arguments: Option<serde_json::Map<String, Value>>,
}

/// `resources/read`'s params.
#[derive(Debug, serde::Deserialize)]
pub struct ResourcesRead {
    pub uri: String,
}

/// `skills/get`'s params: a skill by its uri, or its name.
#[derive(Debug, serde::Deserialize)]
pub struct SkillsGet {
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

/// One message read: a request (it has an id) or a notification (it hasn't).
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// Its id, a string or a number; none for a notification.
    pub id: Option<Value>,
    pub method: String,
    /// Its params, an object or an array; null when it has none.
    pub params: Value,
}

impl Request {
    /// The id an answer to it goes under: null for a notification.
    pub fn answer_id(&self) -> Value {
        self.id.clone().unwrap_or(Value::Null)
    }

    /// Its params read as a `T`: -32602 when they don't fit, saying where.
    pub fn params<T: DeserializeOwned>(&self) -> Result<T, Box<Refused>> {
        let p = if self.params.is_null() { json!({}) } else { self.params.clone() };
        serde_json::from_value(p).map_err(|e| Box::new(Refused::new(self.answer_id(), INVALID_PARAMS, format!("Invalid params: {e}")).about(&self.method)))
    }

    /// Refused: a method the server doesn't have.
    pub fn unknown_method(&self) -> Refused {
        Refused { unknown: true, ..Refused::new(self.answer_id(), METHOD_NOT_FOUND, format!("Method not found: {}", self.method)) }.about(&self.method)
    }

    /// Refused: a tool the server doesn't have (a protocol error, MCP 2025-11-25).
    pub fn unknown_tool(&self, tool: &str) -> Refused {
        Refused { unknown: true, ..Refused::new(self.answer_id(), INVALID_PARAMS, format!("Unknown tool: {tool}")) }.about(&format!("{} {tool}", self.method))
    }

    /// A notification no client sends ([`CLIENT_NOTIFICATIONS`]), as it's noted (`unknown`). It's
    /// never answered all the same: JSON-RPC 2.0 answers no notification.
    pub fn unknown_notification(&self) -> Option<Refused> {
        if self.id.is_some() || CLIENT_NOTIFICATIONS.contains(&self.method.as_str()) {
            return None;
        }
        Some(Refused { unknown: true, ..Refused::new(Value::Null, METHOD_NOT_FOUND, format!("Notification not found: {}", self.method)) }.about(&self.method))
    }
}

/// An error JSON-RPC 2.0 answers with: its code, its words, and the id it's for (null when a
/// message's couldn't be read).
#[derive(Debug, Clone, PartialEq)]
pub struct Refused {
    pub id: Value,
    pub code: i64,
    pub message: String,
    /// What it was, as `crate::wire::unread` says it: a method or a tool, never what it said.
    pub what: String,
    /// A method or a tool the server doesn't have (`unknown`), rather than one it couldn't read.
    pub unknown: bool,
}

impl Refused {
    pub fn new(id: Value, code: i64, message: impl Into<String>) -> Self {
        Refused { id, code, message: message.into(), what: "a message".to_owned(), unknown: false }
    }

    fn about(self, what: &str) -> Self {
        Refused { what: what.chars().take(crate::wire::unread::WHAT_MOST).collect(), ..self }
    }

    /// It, as the answer to send.
    pub fn answer(&self) -> Value {
        json!({"jsonrpc": "2.0", "id": self.id, "error": {"code": self.code, "message": self.message}})
    }

    /// Whether it says a message couldn't be read (JSON-RPC 2.0's own errors), and so is noted
    /// (`crate::wire::unread`); a resource that isn't there, say, isn't.
    pub fn unreadable(&self) -> bool {
        matches!(self.code, PARSE_ERROR | INVALID_REQUEST | METHOD_NOT_FOUND | INVALID_PARAMS)
    }

    /// Its code as `crate::wire::unread` notes it.
    pub fn code(&self) -> Code {
        if self.unknown { Code::Unknown } else { Code::Malformed }
    }

    /// Its words as they're noted (`crate::wire::unread`): without the values that came
    /// (`Invalid params: invalid type: string, expected a boolean`).
    pub fn noted(&self) -> String {
        match self.message.split_once(": ") {
            Some((kind, rest)) => format!("{kind}: {}", crate::wire::payload::without_values(rest)),
            None => self.message.clone(),
        }
    }
}

/// `msg` read as one request, or why it isn't one.
pub fn read(msg: &Value) -> Result<Request, Box<Refused>> {
    let refused = |id: Value, why: &str| Box::new(Refused::new(id, INVALID_REQUEST, why));
    let Some(o) = msg.as_object() else { return Err(refused(Value::Null, "Invalid Request")) };
    let id = match o.get("id") {
        None => None,
        Some(v @ (Value::String(_) | Value::Number(_))) => Some(v.clone()),
        // MCP: an id is never null (JSON-RPC 2.0 only discourages it).
        Some(_) => return Err(refused(Value::Null, "Invalid Request: an id is a string or a number")),
    };
    let answer_id = id.clone().unwrap_or(Value::Null);
    if o.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(refused(answer_id, "Invalid Request: `jsonrpc` is \"2.0\""));
    }
    let Some(method) = o.get("method").and_then(Value::as_str) else {
        return Err(refused(answer_id, "Invalid Request: no method"));
    };
    let params = match o.get("params") {
        None | Some(Value::Null) => Value::Null,
        Some(p @ (Value::Object(_) | Value::Array(_))) => p.clone(),
        Some(_) => return Err(Box::new(Refused::new(answer_id, INVALID_REQUEST, "Invalid Request: params are an object or an array").about(method))),
    };
    Ok(Request { id, method: method.to_owned(), params })
}

/// What a body (an HTTP request's, or a line a runner's server read) held: its messages, each read
/// or refused, and whether they came as a batch.
pub struct Body {
    pub batch: bool,
    pub messages: Vec<Result<Request, Box<Refused>>>,
}

/// `text` read as JSON-RPC messages: one, or a batch of them. What isn't JSON, and an empty batch,
/// are one message refused.
pub fn body(text: &[u8]) -> Body {
    match serde_json::from_slice::<Value>(text) {
        Err(e) => Body { batch: false, messages: vec![Err(Box::new(Refused::new(Value::Null, PARSE_ERROR, format!("Parse error: {e}"))))] },
        Ok(Value::Array(each)) if each.is_empty() => Body { batch: false, messages: vec![Err(Box::new(Refused::new(Value::Null, INVALID_REQUEST, "Invalid Request: an empty batch")))] },
        Ok(Value::Array(each)) => Body { batch: true, messages: each.iter().map(read).collect() },
        Ok(one) => Body { batch: false, messages: vec![read(&one)] },
    }
}

/// The messages a stdio transport reads, a line each, as bytes: a line that isn't UTF-8 is read
/// and refused -32700 ([`body`]) like any other that isn't JSON, rather than ending what reads it.
/// Blank lines are let go; it ends with its input, or an error reading it.
pub fn lines(input: impl std::io::BufRead) -> impl Iterator<Item = Vec<u8>> {
    input.split(b'\n').map_while(Result::ok).filter(|l| !l.trim_ascii().is_empty())
}

/// A message past [`BODY_MOST`], refused as one that isn't a request (its id unread, so null).
pub fn too_large() -> Refused {
    Refused::new(Value::Null, INVALID_REQUEST, format!("Invalid Request: more than {} MiB", BODY_MOST >> 20))
}

/// What to send back for a body whose messages were answered `answers` (a notification's
/// answer is none): one answer, a batch's, or nothing at all when every message was a notification.
pub fn reply(batch: bool, answers: Vec<Value>) -> Option<Value> {
    match (batch, answers.len()) {
        (_, 0) => None,
        (true, _) => Some(Value::Array(answers)),
        (false, _) => answers.into_iter().next(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(text: &str) -> Vec<Result<String, (Value, i64)>> {
        body(text.as_bytes()).messages.into_iter().map(|m| m.map(|r| r.method).map_err(|e| (e.id, e.code))).collect()
    }

    /// Each message is read into a request or refused with JSON-RPC 2.0's code, under its id when
    /// that could be read.
    #[test]
    fn a_message_is_a_request_or_refused_as_json_rpc_says() {
        assert_eq!(codes(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#), vec![Ok("ping".to_owned())]);
        assert_eq!(codes("{"), vec![Err((Value::Null, PARSE_ERROR))]);
        assert_eq!(codes("[]"), vec![Err((Value::Null, INVALID_REQUEST))]);
        assert_eq!(codes("[1,2,3]"), vec![Err((Value::Null, INVALID_REQUEST)); 3]);
        assert_eq!(codes("{}"), vec![Err((Value::Null, INVALID_REQUEST))]);
        assert_eq!(codes(r#"{"jsonrpc":"2.0","id":"a"}"#), vec![Err((json!("a"), INVALID_REQUEST))], "no method: refused under its id");
        assert_eq!(codes(r#"{"jsonrpc":"1.0","id":7,"method":"ping"}"#), vec![Err((json!(7), INVALID_REQUEST))]);
        assert_eq!(codes(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#), vec![Err((Value::Null, INVALID_REQUEST))], "MCP: an id is never null");
        assert_eq!(codes(r#"{"jsonrpc":"2.0","id":2,"method":"ping","params":"x"}"#), vec![Err((json!(2), INVALID_REQUEST))]);
        assert!(body(br#"[{"jsonrpc":"2.0","method":"notifications/initialized"},{"jsonrpc":"2.0","id":3,"method":"ping"}]"#).batch);
        // What a batch of nothing but notifications is answered: nothing; one message, alone.
        assert_eq!(reply(true, Vec::new()), None);
        assert_eq!(reply(false, vec![json!(1)]), Some(json!(1)));
        assert_eq!(reply(true, vec![json!(1)]), Some(json!([1])));
    }

    /// Params that don't fit a method are -32602, saying where; a method or a tool the server
    /// doesn't have is noted `unknown`, the rest `malformed`.
    #[test]
    fn params_that_dont_fit_are_invalid_and_unknowns_are_said_so() {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        #[allow(dead_code)]
        struct Call {
            name: String,
        }
        #[derive(Debug, serde::Deserialize)]
        #[allow(dead_code)]
        struct Flag {
            name: bool,
        }
        let r = read(&json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {"nam": "x"}})).ok();
        let e = r.as_ref().and_then(|r| r.params::<Call>().err());
        assert!(e.as_ref().is_some_and(|e| e.code == INVALID_PARAMS && e.id == json!(9) && e.message.contains("nam") && e.code() == Code::Malformed), "{e:?}");
        // Noted without the values that came.
        let secret = read(&json!({"jsonrpc": "2.0", "id": 10, "method": "tools/call", "params": {"name": "hunter2"}})).ok().and_then(|r| r.params::<Flag>().err());
        assert!(secret.as_ref().is_some_and(|e| e.message.contains("hunter2") && !e.noted().contains("hunter2") && e.noted().starts_with("Invalid params: invalid type: string, expected")), "{secret:?}");
        let e = r.as_ref().map(|r| r.unknown_tool("teleport"));
        assert!(e.as_ref().is_some_and(|e| e.code == INVALID_PARAMS && e.message == "Unknown tool: teleport" && e.code() == Code::Unknown && e.what == "tools/call teleport"), "{e:?}");
        let e = r.as_ref().map(Request::unknown_method);
        assert!(e.is_some_and(|e| e.code == METHOD_NOT_FOUND && e.code() == Code::Unknown));
        // A list's cursor is a string.
        let list = read(&json!({"jsonrpc": "2.0", "id": 11, "method": "tools/list", "params": {"cursor": 5}})).ok().and_then(|r| r.params::<Paginated>().err());
        assert!(list.as_ref().is_some_and(|e| e.code == INVALID_PARAMS && e.id == json!(11) && e.code() == Code::Malformed), "{list:?}");
        assert!(read(&json!({"jsonrpc": "2.0", "id": 12, "method": "tools/list", "params": {"cursor": "c1"}})).ok().is_some_and(|r| r.params::<Paginated>().is_ok()));
    }

    /// No notification is answered: one a client sends is taken as it is, and one no client sends
    /// is noted `unknown`.
    #[test]
    fn a_notification_no_client_sends_is_noted_unknown() {
        let noted = |m: &Value| read(m).ok().and_then(|r| r.unknown_notification());
        for method in CLIENT_NOTIFICATIONS {
            assert_eq!(noted(&json!({"jsonrpc": "2.0", "method": method})), None, "{method}");
        }
        let n = noted(&json!({"jsonrpc": "2.0", "method": "notifications/teleport", "params": {"to": "mars"}}));
        assert!(n.as_ref().is_some_and(|n| n.unreadable() && n.code() == Code::Unknown && n.what == "notifications/teleport" && n.id == Value::Null), "{n:?}");
        // A request of that method is answered -32601 instead (`unknown_method`).
        assert_eq!(noted(&json!({"jsonrpc": "2.0", "id": 1, "method": "notifications/teleport"})), None);
    }

    /// A stdio line that isn't UTF-8 is a message like any other that isn't JSON, refused -32700,
    /// and the lines after it are read all the same.
    #[test]
    fn a_line_that_isnt_utf8_is_refused_and_the_next_is_read() {
        let input: &[u8] = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"p\xffng\"}\n\n  \n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\r\n";
        let read: Vec<Vec<u8>> = lines(input).collect();
        assert_eq!(read.len(), 2, "blank lines let go");
        let first = read.first().map(|l| body(l).messages.into_iter().map(|m| m.map(|r| r.method).map_err(|e| (e.id, e.code))).collect::<Vec<_>>());
        assert_eq!(first, Some(vec![Err((Value::Null, PARSE_ERROR))]));
        let next = read.get(1).map(|l| body(l).messages.into_iter().map(|m| m.map(|r| r.method).map_err(|e| (e.id, e.code))).collect::<Vec<_>>());
        assert_eq!(next, Some(vec![Ok("ping".to_owned())]));
    }
}
