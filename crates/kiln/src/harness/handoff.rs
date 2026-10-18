//! Carrying a conversation from one harness to another (Claude Code ⇄ Codex), so a person can
//! switch the model mid-conversation, across harnesses, and the new one picks up exactly where
//! the old one left off.
//!
//! The conversation is read from the source harness's own record (Claude Code's transcript,
//! Codex's rollout), never from kiln's timeline, which shortens long inputs and outputs. Only what
//! the model still sees is carried: from the last compaction on, without thinking (it is bound to
//! the provider that wrote it). It is written the way the target keeps its own history: a Claude
//! Code transcript to `--resume`, or Responses items for Codex's `thread/inject_items`. Tool calls
//! become the target's own calls where it has a faithful counterpart (commands, file changes,
//! reads); the rest become words saying what was run and what it returned.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::model::{Body, Event, Todo, ToolKind, ToolStatus, clip_middle};

/// A conversation as any harness can take it up: what was said and done, oldest first.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Conversation {
    /// The summary the source wrote when it last compacted (the model saw nothing before it).
    pub summary: Option<String>,
    pub entries: Vec<Entry>,
    /// The plan (todo list) as it stands.
    pub todos: Vec<Todo>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// What the person said (with any images they attached).
    User { text: String, images: Vec<Image> },
    /// What the agent said.
    Assistant(String),
    /// A tool the agent called, with what it returned.
    Call(Box<Call>),
}

/// An image, inline.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub mime: String,
    pub base64: String,
}

/// One tool call, whole: its input and its output as the model saw them.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub id: String,
    /// The source harness's own name for the tool.
    pub name: String,
    pub kind: ToolKind,
    pub input: Value,
    pub output: String,
    pub error: bool,
    pub exit_code: Option<i32>,
    /// A shell command's command line.
    pub command: Option<String>,
    /// A file change's effect, file by file.
    pub changes: Vec<Change>,
    /// A read: the file, and the lines read (first line, how many).
    pub read: Option<(String, Option<u64>, Option<u64>)>,
}

impl Call {
    fn new(id: &str, name: &str, kind: ToolKind, input: Value) -> Self {
        Self { id: id.to_owned(), name: name.to_owned(), kind, input, output: String::new(), error: false, exit_code: None, command: None, changes: Vec::new(), read: None }
    }
}

/// What a file change did to one file.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Add { path: String, content: String },
    /// Hunks of lines, each line with its ' ', '-' or '+'; `to`: it was also moved there.
    Update { path: String, hunks: Vec<Vec<String>>, to: Option<String> },
    Delete { path: String },
}

/// What a call left without a result says (the turn ended before it did).
const NO_RESULT: &str = "(no result: the turn ended before it finished)";

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

// ---------------------------------------------------------------------- Claude Code, read

/// The conversation in a Claude Code transcript (its lines): its own branch (the parent chain back
/// from the newest message), from its last compaction on.
pub fn from_claude(lines: &[String]) -> Conversation {
    let records: Vec<Value> = lines.iter().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let branch = super::transcript::branch(records, None, None);
    // The model sees the summary, the messages the compaction kept as they were, then what came
    // after.
    let boundary = branch.iter().rposition(|r| str_of(r, "type") == Some("system") && str_of(r, "subtype") == Some("compact_boundary"));
    let visible: Vec<&Value> = match boundary {
        None => branch.iter().collect(),
        Some(b) => {
            let meta = branch.get(b).and_then(|r| r.get("compactMetadata"));
            let kept: Vec<&str> = meta.and_then(|m| m.pointer("/preservedMessages/uuids")).and_then(Value::as_array).map(|u| u.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            let after: Vec<&Value> = branch.iter().skip(b + 1).collect();
            let (summary, rest): (Vec<&Value>, Vec<&Value>) = after.into_iter().partition(|r| r.get("isCompactSummary").and_then(Value::as_bool) == Some(true));
            let preserved = branch.iter().take(b).filter(|r| str_of(r, "uuid").is_some_and(|u| kept.contains(&u)));
            summary.into_iter().chain(preserved).chain(rest).collect()
        }
    };
    let mut c = Conversation::default();
    // Calls waiting for their results, by tool_use id.
    let mut open: HashMap<String, usize> = HashMap::new();
    for r in visible {
        if r.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        match str_of(r, "type").unwrap_or("") {
            "user" => {
                if r.get("isCompactSummary").and_then(Value::as_bool) == Some(true) {
                    let text = claude_text(r.pointer("/message/content"));
                    if !text.trim().is_empty() {
                        c.summary = Some(text);
                    }
                    continue;
                }
                if r.get("isMeta").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                claude_user(&mut c, &mut open, r);
            }
            "assistant" => {
                if r.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                claude_assistant(&mut c, &mut open, r);
            }
            // A message the person sent while it worked (steering), delivered with the turn.
            "attachment" if r.pointer("/attachment/type").and_then(Value::as_str) == Some("queued_command") => {
                if let Some(p) = r.pointer("/attachment/prompt").and_then(Value::as_str).filter(|p| !p.trim().is_empty()) {
                    c.entries.push(Entry::User { text: p.to_owned(), images: Vec::new() });
                }
            }
            _ => {}
        }
    }
    finish_open(&mut c, &open);
    c
}

/// Text blocks (or a string) as one text.
fn claude_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks.iter().filter(|b| str_of(b, "type") == Some("text")).filter_map(|b| str_of(b, "text")).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// A person's words as the model saw them, without what Claude Code wraps around them for itself
/// (reminders, a local command's output); a slash command reads as it was typed. None: nothing
/// anyone said.
fn claude_words(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() || t.starts_with("<system-reminder>") || t.starts_with("<local-command-") || t.starts_with("Caveat:") {
        return None;
    }
    if let Some(name) = between(t, "<command-name>", "</command-name>") {
        let args = between(t, "<command-args>", "</command-args>").map(str::trim).unwrap_or("");
        let name = name.trim();
        return Some(if args.is_empty() { name.to_owned() } else { format!("{name} {args}") });
    }
    Some(t.to_owned())
}

fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = s.find(open)? + open.len();
    let end = s.get(start..)?.find(close)? + start;
    s.get(start..end)
}

fn claude_user(c: &mut Conversation, open: &mut HashMap<String, usize>, r: &Value) {
    let structured = r.get("toolUseResult").cloned().unwrap_or(Value::Null);
    // The person said no: it never ran.
    let denied = r.get("toolDenialKind").is_some_and(|d| !d.is_null());
    match r.pointer("/message/content") {
        Some(Value::String(s)) => {
            if let Some(t) = claude_words(s) {
                c.entries.push(Entry::User { text: t, images: Vec::new() });
            }
        }
        Some(Value::Array(blocks)) => {
            let mut texts = Vec::new();
            let mut images = Vec::new();
            for b in blocks {
                match str_of(b, "type").unwrap_or("") {
                    "tool_result" => {
                        let Some(id) = str_of(b, "tool_use_id") else { continue };
                        let Some(call) = open.remove(id).and_then(|i| c.entries.get_mut(i)).and_then(|e| match e {
                            Entry::Call(call) => Some(call),
                            _ => None,
                        }) else {
                            continue;
                        };
                        claude_result(call, b, &structured);
                        if denied {
                            call.exit_code = None;
                            call.output = format!("The user declined to run it: {}", call.output);
                        }
                    }
                    "text" => texts.extend(str_of(b, "text").and_then(claude_words)),
                    "image" => {
                        if let (Some(mime), Some(data)) = (b.pointer("/source/media_type").and_then(Value::as_str), b.pointer("/source/data").and_then(Value::as_str)) {
                            images.push(Image { mime: mime.to_owned(), base64: data.to_owned() });
                        }
                    }
                    _ => {}
                }
            }
            if !texts.is_empty() || !images.is_empty() {
                c.entries.push(Entry::User { text: texts.join("\n"), images });
            }
        }
        _ => {}
    }
}

fn claude_assistant(c: &mut Conversation, open: &mut HashMap<String, usize>, r: &Value) {
    let Some(blocks) = r.pointer("/message/content").and_then(Value::as_array) else { return };
    let cwd = str_of(r, "cwd").unwrap_or("");
    for b in blocks {
        match str_of(b, "type").unwrap_or("") {
            "text" => {
                let t = str_of(b, "text").unwrap_or("");
                if t.trim().is_empty() {
                    continue;
                }
                // One message's text, written in pieces, reads as one.
                match c.entries.last_mut() {
                    Some(Entry::Assistant(prev)) => {
                        prev.push_str("\n\n");
                        prev.push_str(t);
                    }
                    _ => c.entries.push(Entry::Assistant(t.to_owned())),
                }
            }
            "tool_use" | "server_tool_use" => {
                let id = str_of(b, "id").unwrap_or("");
                let name = str_of(b, "name").unwrap_or("tool");
                let input = b.get("input").cloned().unwrap_or(Value::Null);
                let (kind, _, _) = super::claude::describe(name, &input, cwd);
                if kind == ToolKind::Todo {
                    c.todos = claude_todos(&input);
                    // Its result says nothing more than the plan does.
                    continue;
                }
                let mut call = Call::new(id, name, kind, input.clone());
                match name {
                    "Bash" | "PowerShell" => call.command = str_of(&input, "command").map(str::to_owned),
                    "Read" => {
                        call.read = str_of(&input, "file_path").map(|p| (p.to_owned(), input.get("offset").and_then(Value::as_u64), input.get("limit").and_then(Value::as_u64)));
                    }
                    _ => {}
                }
                c.entries.push(Entry::Call(Box::new(call)));
                if !id.is_empty() {
                    open.insert(id.to_owned(), c.entries.len() - 1);
                }
            }
            // Thinking is bound to the provider that wrote it; a server tool's result is in its
            // text.
            _ => {}
        }
    }
}

fn claude_todos(input: &Value) -> Vec<Todo> {
    input
        .get("todos")
        .and_then(Value::as_array)
        .map(|t| {
            t.iter()
                .filter_map(|x| Some(Todo { text: str_of(x, "content").or_else(|| str_of(x, "subject")).or_else(|| str_of(x, "text"))?.to_owned(), status: str_of(x, "status").unwrap_or("pending").to_owned() }))
                .collect()
        })
        .unwrap_or_default()
}

/// A tool_result into its call: the output as the model saw it (a command's whole stdout and
/// stderr where the transcript keeps them), and what a file change did.
fn claude_result(call: &mut Call, block: &Value, structured: &Value) {
    call.error = block.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    let mut text = match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts.iter().filter_map(|p| if str_of(p, "type") == Some("text") { str_of(p, "text").map(str::to_owned) } else { Some("[image]".to_owned()) }).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    };
    if let Some(o) = structured.as_object() {
        if let Some(out) = o.get("stdout").and_then(Value::as_str) {
            let err = o.get("stderr").and_then(Value::as_str).unwrap_or("");
            text = match (out.is_empty(), err.is_empty()) {
                (_, true) => out.to_owned(),
                (true, false) => err.to_owned(),
                (false, false) => format!("{out}\n{err}"),
            };
        }
        if let Some(code) = o.get("exitCode").or_else(|| o.get("exit_code")).and_then(Value::as_i64) {
            call.exit_code = i32::try_from(code).ok();
        }
        if matches!(call.name.as_str(), "Edit" | "MultiEdit" | "Write") && !call.error {
            call.changes = claude_changes(&call.name, &call.input, o);
        }
    }
    // A failed command says its exit code first ("Exit code 2").
    if call.exit_code.is_none() && call.command.is_some() {
        call.exit_code = if call.error { text.strip_prefix("Exit code ").and_then(|r| r.split_whitespace().next()).and_then(|n| n.parse().ok()) } else { Some(0) };
    }
    call.output = text;
}

/// What an Edit, MultiEdit or Write did, from its structured result (real hunks, with context).
fn claude_changes(name: &str, input: &Value, o: &serde_json::Map<String, Value>) -> Vec<Change> {
    let Some(path) = o.get("filePath").and_then(Value::as_str).or_else(|| str_of(input, "file_path")) else { return Vec::new() };
    let hunks: Vec<Vec<String>> = o
        .get("structuredPatch")
        .and_then(Value::as_array)
        .map(|hs| hs.iter().filter_map(|h| Some(h.get("lines")?.as_array()?.iter().filter_map(|l| l.as_str().map(str::to_owned)).collect::<Vec<String>>())).filter(|h| !h.is_empty()).collect())
        .unwrap_or_default();
    if name == "Write" && (o.get("type").and_then(Value::as_str) == Some("create") || (hunks.is_empty() && !o.get("originalFile").is_some_and(Value::is_string))) {
        let content = str_of(input, "content").or_else(|| o.get("content").and_then(Value::as_str)).unwrap_or("");
        return vec![Change::Add { path: path.to_owned(), content: content.to_owned() }];
    }
    if !hunks.is_empty() {
        return vec![Change::Update { path: path.to_owned(), hunks, to: None }];
    }
    // No patch kept: the edit's own strings.
    let pairs: Vec<(String, String)> = match name {
        "MultiEdit" => input.get("edits").and_then(Value::as_array).map(|es| es.iter().map(|e| (str_of(e, "old_string").unwrap_or("").to_owned(), str_of(e, "new_string").unwrap_or("").to_owned())).collect()).unwrap_or_default(),
        "Write" => vec![(o.get("originalFile").and_then(Value::as_str).unwrap_or("").to_owned(), str_of(input, "content").unwrap_or("").to_owned())],
        _ => vec![(str_of(input, "old_string").unwrap_or("").to_owned(), str_of(input, "new_string").unwrap_or("").to_owned())],
    };
    let hunks: Vec<Vec<String>> = pairs.iter().map(|(old, new)| old.lines().map(|l| format!("-{l}")).chain(new.lines().map(|l| format!("+{l}"))).collect::<Vec<_>>()).filter(|h| !h.is_empty()).collect();
    if hunks.is_empty() {
        return Vec::new();
    }
    vec![Change::Update { path: path.to_owned(), hunks, to: None }]
}

/// Calls whose result never came: the turn ended before they did.
fn finish_open(c: &mut Conversation, open: &HashMap<String, usize>) {
    for i in open.values() {
        if let Some(Entry::Call(call)) = c.entries.get_mut(*i) {
            call.output = NO_RESULT.to_owned();
            call.error = true;
        }
    }
}

// ---------------------------------------------------------------------- Codex, read

/// The records of a Codex thread as Codex itself sees its history, oldest first, from its rollout
/// segments (`(first ordinal, history base, records)` each, any order): a rewind starts a new
/// segment that continues the thread's history up to an ordinal of an earlier one.
pub fn codex_history(segments: Vec<(Option<u64>, Option<u64>, Vec<Value>)>) -> Vec<Value> {
    let mut segs = segments;
    // Newest last (a segment starts at its first ordinal, then by when it was written).
    segs.sort_by_key(|(first, _, recs)| (first.unwrap_or(0), recs.first().and_then(|r| str_of(r, "timestamp").map(str::to_owned)).unwrap_or_default()));
    let Some((_, base, newest)) = segs.pop() else { return Vec::new() };
    let mut out = newest;
    let mut cut = base;
    // Back through the segments each one continues.
    while let Some(end) = cut {
        let Some(pos) = segs.iter().rposition(|(first, _, _)| first.unwrap_or(0) < end) else { break };
        let (_, base, recs) = segs.remove(pos);
        let mut head: Vec<Value> = recs.into_iter().filter(|r| r.get("ordinal").and_then(Value::as_u64).is_none_or(|o| o < end)).collect();
        head.append(&mut out);
        out = head;
        cut = base;
    }
    out
}

/// The conversation in a Codex thread's history (its rollout records, as [`codex_history`] puts
/// them together), from its last compaction on.
pub fn from_codex(records: &[Value]) -> Conversation {
    let mut items: Vec<&Value> = Vec::new();
    for r in records {
        let p = r.get("payload").unwrap_or(r);
        match str_of(r, "type").unwrap_or("") {
            // What the model sees from here on: the compaction's history, then what follows.
            "compacted" => {
                items.clear();
                items.extend(p.get("replacement_history").and_then(Value::as_array).into_iter().flatten());
            }
            "response_item" => items.push(p),
            // Older rollouts: bare items.
            "" if p.get("type").is_some() && p.get("payload").is_none() => items.push(p),
            _ => {}
        }
    }
    let mut c = Conversation::default();
    let mut open: HashMap<String, usize> = HashMap::new();
    for it in items {
        codex_item(&mut c, &mut open, it);
    }
    finish_open(&mut c, &open);
    c
}

fn codex_item(c: &mut Conversation, open: &mut HashMap<String, usize>, it: &Value) {
    let call_id = str_of(it, "call_id").unwrap_or("").to_owned();
    match str_of(it, "type").unwrap_or("") {
        "message" => codex_message(c, it),
        "function_call" => {
            let name = match (str_of(it, "namespace"), str_of(it, "name").unwrap_or("tool")) {
                (Some(ns), n) if !ns.is_empty() && ns != "functions" => format!("{ns}.{n}"),
                (_, n) => n.to_owned(),
            };
            let raw = str_of(it, "arguments").unwrap_or("");
            let args: Value = serde_json::from_str(raw).unwrap_or_else(|_| json!({"arguments": raw}));
            if name == "update_plan" {
                c.todos = codex_plan(&args);
                return;
            }
            let call = codex_call(&call_id, &name, args);
            c.entries.push(Entry::Call(Box::new(call)));
            open.insert(call_id, c.entries.len() - 1);
        }
        "custom_tool_call" => {
            let name = str_of(it, "name").unwrap_or("tool");
            let input = str_of(it, "input").unwrap_or("");
            let mut call = Call::new(&call_id, name, ToolKind::Other, json!({"input": input}));
            if name == "apply_patch" {
                call.kind = ToolKind::Edit;
                call.changes = parse_patch(input);
            }
            c.entries.push(Entry::Call(Box::new(call)));
            open.insert(call_id, c.entries.len() - 1);
        }
        "local_shell_call" => {
            let action = it.get("action").cloned().unwrap_or(Value::Null);
            let words: Vec<String> = action.get("command").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
            let mut call = Call::new(&call_id, "local_shell", ToolKind::Shell, action);
            call.command = Some(shell_line(&words));
            c.entries.push(Entry::Call(Box::new(call)));
            open.insert(call_id, c.entries.len() - 1);
        }
        "function_call_output" | "custom_tool_call_output" | "local_shell_call_output" => {
            let Some(call) = open.remove(&call_id).and_then(|i| c.entries.get_mut(i)).and_then(|e| match e {
                Entry::Call(call) => Some(call),
                _ => None,
            }) else {
                return;
            };
            let out = match it.get("output") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(parts)) => parts.iter().filter_map(|p| str_of(p, "text").map(str::to_owned).or_else(|| p.get("image_url").map(|_| "[image]".to_owned()))).collect::<Vec<_>>().join("\n"),
                Some(Value::Object(o)) => o.get("content").and_then(Value::as_str).map_or_else(|| Value::Object(o.clone()).to_string(), str::to_owned),
                _ => String::new(),
            };
            codex_result(call, &out);
        }
        "web_search_call" => {
            let action = it.get("action").cloned().unwrap_or(Value::Null);
            let mut call = Call::new(&call_id, "web_search", ToolKind::Web, action);
            call.output = "(searched)".to_owned();
            c.entries.push(Entry::Call(Box::new(call)));
        }
        // Reasoning is bound to the provider that wrote it.
        _ => {}
    }
}

/// A user or assistant message; the harness's own context (instructions, the environment, the
/// handoff's own note) is left out: Codex marks what each part is, older rollouts don't (their
/// context is tagged text).
fn codex_message(c: &mut Conversation, it: &Value) {
    let role = str_of(it, "role").unwrap_or("");
    let parts = it.get("content").and_then(Value::as_array).cloned().unwrap_or_default();
    let kinds: Vec<String> = it.pointer("/internal_chat_message_metadata_passthrough/content_item_kinds").and_then(Value::as_array).map(|k| k.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
    match role {
        "assistant" => {
            let text = parts.iter().filter_map(|p| str_of(p, "text")).collect::<Vec<_>>().join("\n");
            if !text.trim().is_empty() {
                match c.entries.last_mut() {
                    Some(Entry::Assistant(prev)) => {
                        prev.push_str("\n\n");
                        prev.push_str(&text);
                    }
                    _ => c.entries.push(Entry::Assistant(text)),
                }
            }
        }
        "user" => {
            let mut texts = Vec::new();
            let mut images = Vec::new();
            for (i, p) in parts.iter().enumerate() {
                let kind = kinds.get(i).map(String::as_str);
                if kind == Some("compaction.summary") {
                    c.summary = str_of(p, "text").map(str::to_owned);
                    continue;
                }
                // What Codex tells the model of an interrupted turn, in the words Claude Code uses.
                if kind == Some("generic.turn_aborted") {
                    texts.push("[Request interrupted by user]".to_owned());
                    continue;
                }
                // The person's own (or an item kiln injected, which Codex can't tell apart).
                let theirs = match kind {
                    Some(k) => k.starts_with("user.") || k == "unknown",
                    None => true,
                };
                if !theirs {
                    continue;
                }
                match str_of(p, "type").unwrap_or("") {
                    "input_text" | "text" => {
                        let t = str_of(p, "text").unwrap_or("");
                        let tt = t.trim_start();
                        // Context without its kind marked; an image's wrapper.
                        if tt.is_empty() || (tt.starts_with('<') && (kind.is_none() || tt.starts_with("<image") || tt.starts_with("</image") || tt.starts_with("<kiln_handoff>"))) {
                            continue;
                        }
                        texts.push(t.to_owned());
                    }
                    "input_image" => {
                        if let Some((mime, data)) = str_of(p, "image_url").and_then(data_url) {
                            images.push(Image { mime, base64: data });
                        }
                    }
                    _ => {}
                }
            }
            if !texts.is_empty() || !images.is_empty() {
                c.entries.push(Entry::User { text: texts.join("\n"), images });
            }
        }
        _ => {}
    }
}

/// `data:<mime>;base64,<data>` → (mime, data).
fn data_url(u: &str) -> Option<(String, String)> {
    let rest = u.strip_prefix("data:")?;
    let (mime, data) = rest.split_once(";base64,")?;
    Some((mime.to_owned(), data.to_owned()))
}

fn codex_plan(args: &Value) -> Vec<Todo> {
    args.get("plan")
        .and_then(Value::as_array)
        .map(|steps| {
            steps
                .iter()
                .filter_map(|s| {
                    let status = match str_of(s, "status").unwrap_or("pending") {
                        "completed" => "completed",
                        "in_progress" | "inProgress" => "in_progress",
                        _ => "pending",
                    };
                    Some(Todo { text: str_of(s, "step")?.to_owned(), status: status.to_owned() })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A Codex function call: commands (and the patches some are), by what it is.
fn codex_call(id: &str, name: &str, args: Value) -> Call {
    let command = match name {
        "exec_command" => str_of(&args, "cmd").map(str::to_owned),
        "shell_command" => str_of(&args, "command").map(str::to_owned),
        "shell" | "container.exec" => args.get("command").and_then(Value::as_array).map(|a| shell_line(&a.iter().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>())),
        _ => None,
    };
    let mut call = Call::new(id, name, ToolKind::Other, args.clone());
    if let Some(cmd) = command {
        // apply_patch, run as a command.
        if let Some(patch) = between_inclusive(&cmd, "*** Begin Patch", "*** End Patch") {
            call.kind = ToolKind::Edit;
            call.changes = parse_patch(patch);
            return call;
        }
        call.kind = ToolKind::Shell;
        let dir = str_of(&args, "workdir").filter(|d| !d.is_empty());
        call.command = Some(match dir {
            Some(d) => format!("cd {} && {cmd}", quote(d)),
            None => cmd,
        });
    }
    call
}

fn between_inclusive<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = s.find(open)?;
    let end = s.get(start..)?.find(close)? + start + close.len();
    s.get(start..end)
}

/// A command given as words: `bash -lc '<cmd>'` is `<cmd>`; otherwise the words, quoted where
/// they need it.
fn shell_line(words: &[String]) -> String {
    if let [sh, flag, cmd] = words
        && matches!(sh.rsplit('/').next(), Some("bash" | "sh" | "zsh" | "dash"))
        && matches!(flag.as_str(), "-c" | "-lc")
    {
        return cmd.clone();
    }
    words.iter().map(|w| if w.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c)) && !w.is_empty() { w.clone() } else { quote(w) }).collect::<Vec<_>>().join(" ")
}

/// Single-quoted for a POSIX shell.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A Codex call's output: a command's exit code and output from the header Codex puts on it
/// ("Process exited with code 0", "Output:").
fn codex_result(call: &mut Call, out: &str) {
    if call.kind == ToolKind::Shell || call.kind == ToolKind::Edit {
        let (code, body) = exec_output(out);
        call.exit_code = code;
        call.error = code.is_some_and(|c| c != 0);
        call.output = body;
        if call.kind == ToolKind::Edit && (call.error || call.output.contains("rror")) && !call.output.contains("Success") {
            call.error = true;
        }
    } else {
        call.output = out.to_owned();
    }
}

/// (exit code, output) of an `exec_command` result.
fn exec_output(out: &str) -> (Option<i32>, String) {
    let mut code = None;
    let mut body = None;
    for (i, line) in out.lines().enumerate() {
        if let Some(n) = line.strip_prefix("Process exited with code ").or_else(|| line.strip_prefix("Exit code: ")) {
            code = code.or_else(|| n.trim().parse().ok());
        }
        if line == "Output:" {
            body = Some(out.lines().skip(i + 1).collect::<Vec<_>>().join("\n"));
            break;
        }
    }
    // An apply_patch run as a command nests one result in another.
    let body = body.unwrap_or_else(|| out.to_owned());
    if body.starts_with("Exit code: ") || body.starts_with("Wall time:") {
        let (inner, rest) = exec_output(&body);
        return (code.or(inner), rest);
    }
    (code, body)
}

/// apply_patch's format, as file changes.
pub fn parse_patch(text: &str) -> Vec<Change> {
    let mut out: Vec<Change> = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if let Some(p) = line.strip_prefix("*** Add File: ") {
            let mut content = String::new();
            while let Some(l) = lines.peek().filter(|l| !l.starts_with("*** ")) {
                content.push_str(l.strip_prefix('+').unwrap_or(l));
                content.push('\n');
                lines.next();
            }
            out.push(Change::Add { path: p.trim().to_owned(), content });
        } else if let Some(p) = line.strip_prefix("*** Delete File: ") {
            out.push(Change::Delete { path: p.trim().to_owned() });
        } else if let Some(p) = line.strip_prefix("*** Update File: ") {
            let mut to = None;
            let mut hunks: Vec<Vec<String>> = Vec::new();
            while let Some(l) = lines.peek().copied() {
                if let Some(t) = l.strip_prefix("*** Move to: ") {
                    to = Some(t.trim().to_owned());
                } else if l == "*** End of File" {
                } else if l.starts_with("*** ") {
                    break;
                } else if l.starts_with("@@") {
                    hunks.push(Vec::new());
                } else {
                    if hunks.is_empty() {
                        hunks.push(Vec::new());
                    }
                    if let Some(h) = hunks.last_mut() {
                        h.push(if l.is_empty() { " ".to_owned() } else { l.to_owned() });
                    }
                }
                lines.next();
            }
            hunks.retain(|h| !h.is_empty());
            out.push(Change::Update { path: p.trim().to_owned(), hunks, to });
        }
    }
    out
}

/// File changes in apply_patch's format.
pub fn patch_text(changes: &[Change]) -> String {
    let mut s = String::from("*** Begin Patch\n");
    for c in changes {
        match c {
            Change::Add { path, content } => {
                s.push_str(&format!("*** Add File: {path}\n"));
                for l in content.lines() {
                    s.push_str(&format!("+{l}\n"));
                }
            }
            Change::Delete { path } => s.push_str(&format!("*** Delete File: {path}\n")),
            Change::Update { path, hunks, to } => {
                s.push_str(&format!("*** Update File: {path}\n"));
                if let Some(t) = to {
                    s.push_str(&format!("*** Move to: {t}\n"));
                }
                for h in hunks {
                    s.push_str("@@\n");
                    for l in h {
                        s.push_str(l);
                        s.push('\n');
                    }
                }
            }
        }
    }
    s.push_str("*** End Patch");
    s
}

// ---------------------------------------------------------------------- the size it may take

/// The bytes a conversation takes, about (what the model reads of it).
pub fn size(c: &Conversation) -> usize {
    c.summary.as_ref().map_or(0, String::len)
        + c.entries
            .iter()
            .map(|e| match e {
                Entry::User { text, images } => text.len() + images.iter().map(|i| i.base64.len()).sum::<usize>(),
                Entry::Assistant(t) => t.len(),
                Entry::Call(call) => call.output.len() + call.input.to_string().len(),
            })
            .sum::<usize>()
}

/// Longest a call's output stays when the conversation has to be made smaller.
const SHORT_OUTPUT: usize = 8 * 1024;
/// Images kept when the conversation has to be made smaller (the newest).
const KEEP_IMAGES: usize = 4;

/// Make the conversation fit `budget` bytes, losing as little as it can: long tool outputs are
/// shortened (head and tail kept), oldest first; then older images go; then the oldest messages
/// (the first thing asked stays). Returns how many entries were left out.
pub fn fit(c: &mut Conversation, budget: usize) -> usize {
    if size(c) <= budget {
        return 0;
    }
    for e in &mut c.entries {
        if let Entry::Call(call) = e
            && call.output.len() > SHORT_OUTPUT
        {
            call.output = clip_middle(&call.output, SHORT_OUTPUT).0;
        }
    }
    let mut c_size = size(c);
    if c_size > budget {
        let mut seen = 0;
        for e in c.entries.iter_mut().rev() {
            if let Entry::User { text, images } = e
                && !images.is_empty()
            {
                seen += images.len();
                if seen > KEEP_IMAGES {
                    text.push_str(&format!("\n[{} image(s) attached here, left out]", images.len()));
                    images.clear();
                }
            }
        }
        c_size = size(c);
    }
    let mut dropped = 0;
    if c_size > budget {
        // The first thing asked stays: it says what all this is for.
        let first = c.entries.iter().position(|e| matches!(e, Entry::User { .. }));
        let i = first.map_or(0, |f| f + 1);
        while c_size > budget && i < c.entries.len() {
            c.entries.remove(i);
            dropped += 1;
            c_size = size(c);
            // A call's result never goes without the call: nothing is split.
            while matches!(c.entries.get(i), Some(Entry::Assistant(_))) && c_size > budget {
                c.entries.remove(i);
                dropped += 1;
                c_size = size(c);
            }
        }
        if dropped > 0 {
            let note = format!("[{dropped} earlier message(s) and tool call(s) are left out here to fit the context window.]");
            c.entries.insert(i.min(c.entries.len()), Entry::User { text: note, images: Vec::new() });
        }
    }
    dropped
}

// ---------------------------------------------------------------------- written as words

/// The most a conversation in words takes ([`as_words`]): about 50k tokens.
pub const WORDS_BUDGET: usize = 200 * 1024;

/// The conversation from kiln's own timeline, for a harness whose own record kiln doesn't keep
/// (ACP agents, Antigravity): what was said, and each tool call with what kiln kept of its output
/// (kiln shortens long ones). A subagent's events and kiln's own notes to the agent (task
/// notifications, reminders) are left out; the person's attachments go by name.
pub fn from_events(events: &[Event]) -> Conversation {
    let mut c = Conversation::default();
    for e in events.iter().filter(|e| e.parent.is_none()) {
        match &e.body {
            Body::User { text, origin, attachments, .. } if origin.as_deref() != Some("system") && (!text.trim().is_empty() || !attachments.is_empty()) => {
                let mut text = text.clone();
                if !attachments.is_empty() {
                    text.push_str(&format!("\n[attached: {}]", attachments.join(", ")));
                }
                c.entries.push(Entry::User { text, images: Vec::new() });
            }
            Body::Assistant { text, .. } if !text.trim().is_empty() => c.entries.push(Entry::Assistant(text.clone())),
            Body::Tool(t) => {
                let mut call = Call::new(&t.tool_id, &t.name, t.tool, t.input.clone());
                call.output = t.output.clone().unwrap_or_default();
                call.exit_code = t.exit_code;
                call.error = matches!(t.status, ToolStatus::Error | ToolStatus::Denied | ToolStatus::Cancelled);
                if t.tool == ToolKind::Shell && !t.title.trim().is_empty() {
                    call.command = Some(t.title.clone());
                }
                c.entries.push(Entry::Call(Box::new(call)));
            }
            _ => {}
        }
    }
    c
}

/// `c` in words, for a harness that takes a conversation only as text (its first message's
/// context): `note` first, then each entry, oldest first, all in one `<kiln_handoff>` block. Made
/// to fit `budget` bytes first ([`fit`]: long outputs shortened, then the oldest left out).
pub fn as_words(mut c: Conversation, note: &str, budget: usize) -> String {
    fit(&mut c, budget);
    let mut s = format!("<kiln_handoff>\n{note}\nThis is the conversation so far, oldest first, as kiln kept it; go on from where it ends.\n\n");
    if let Some(sum) = &c.summary {
        s.push_str(&format!("[What came before, in short]\n{sum}\n\n"));
    }
    for e in &c.entries {
        match e {
            Entry::User { text, .. } => s.push_str(&format!("The person said:\n{text}\n\n")),
            Entry::Assistant(t) => s.push_str(&format!("You said:\n{t}\n\n")),
            Entry::Call(call) => {
                let out = if call.output.trim().is_empty() { "(nothing)".to_owned() } else { clip_middle(call.output.trim_end(), 16 * 1024).0 };
                let how = if call.error { "it failed" } else { "it returned" };
                match call.command.as_deref().filter(|c| !c.trim().is_empty()) {
                    Some(cmd) => {
                        let exit = call.exit_code.map(|c| format!(" (exit code {c})")).unwrap_or_default();
                        s.push_str(&format!("You ran `{cmd}`{exit}; {how}:\n{out}\n\n"));
                    }
                    None => {
                        let input = clip_middle(&call.input.to_string(), 4 * 1024).0;
                        s.push_str(&format!("You used the {} tool with {input}; {how}:\n{out}\n\n", call.name));
                    }
                }
            }
        }
    }
    s.push_str("</kiln_handoff>");
    s
}

// ---------------------------------------------------------------------- written for Codex

/// How a call goes to the target: as its own tool's call, or in words.
enum Native {
    /// A shell command (the command line), with what it printed and its exit code.
    Command(String),
    /// File changes, as one patch.
    Patch,
    Words,
}

fn native(call: &Call) -> Native {
    if !call.changes.is_empty() && !call.error {
        return Native::Patch;
    }
    if let Some(cmd) = call.command.as_ref().filter(|c| !c.trim().is_empty()) {
        return Native::Command(cmd.clone());
    }
    if let Some((path, offset, limit)) = &call.read {
        // A read as the command that prints the same (numbered lines).
        let cmd = match (offset, limit) {
            (None, None) => format!("cat -n {}", quote(path)),
            (o, l) => {
                let first = o.unwrap_or(1).max(1);
                let last = l.map_or_else(|| "$".to_owned(), |l| (first + l.saturating_sub(1)).to_string());
                format!("cat -n {} | sed -n '{first},{last}p'", quote(path))
            }
        };
        return Native::Command(cmd);
    }
    Native::Words
}

/// A call no target tool matches, in words: what was run and what it returned.
fn in_words(call: &Call) -> String {
    let input = clip_middle(&call.input.to_string(), 4 * 1024).0;
    let output = if call.output.trim().is_empty() { "(nothing)".to_owned() } else { clip_middle(call.output.trim_end(), 16 * 1024).0 };
    let how = if call.error { "It failed" } else { "It returned" };
    format!("[I used the {} tool with {input}. {how}:]\n{output}", call.name)
}

/// A Codex command result, as Codex writes one.
fn exec_text(call: &Call) -> String {
    match call.exit_code {
        Some(c) => format!("Process exited with code {c}\nOutput:\n{}", call.output),
        // It may never have run (declined, cut off): just what it said.
        None if call.error => call.output.clone(),
        None => format!("Output:\n{}", call.output),
    }
}

/// The conversation as Responses items for Codex's `thread/inject_items`: messages, and calls as
/// the `exec_command` calls (commands, reads, patches) Codex makes itself, or in words. `note`,
/// for the model, goes last.
pub fn to_codex(c: &Conversation, note: Option<&str>) -> Vec<Value> {
    let mut out = Vec::new();
    let msg = |role: &str, kind: &str, text: &str| json!({"type": "message", "role": role, "content": [{"type": kind, "text": text}]});
    if let Some(s) = &c.summary {
        out.push(msg("user", "input_text", s));
    }
    for (n, e) in c.entries.iter().enumerate() {
        match e {
            Entry::User { text, images } => {
                let mut content: Vec<Value> = Vec::new();
                if !text.is_empty() {
                    content.push(json!({"type": "input_text", "text": text}));
                }
                for i in images {
                    content.push(json!({"type": "input_image", "image_url": format!("data:{};base64,{}", i.mime, i.base64)}));
                }
                if !content.is_empty() {
                    out.push(json!({"type": "message", "role": "user", "content": content}));
                }
            }
            Entry::Assistant(t) => out.push(msg("assistant", "output_text", t)),
            Entry::Call(call) => {
                let id = if call.id.is_empty() { format!("call_kiln_{n}") } else { call.id.clone() };
                match native(call) {
                    Native::Command(cmd) => {
                        out.push(json!({"type": "function_call", "name": "exec_command", "arguments": json!({"cmd": cmd}).to_string(), "call_id": id}));
                        out.push(json!({"type": "function_call_output", "call_id": id, "output": exec_text(call)}));
                    }
                    Native::Patch => {
                        let cmd = format!("apply_patch <<'EOF'\n{}\nEOF\n", patch_text(&call.changes));
                        let listed: Vec<String> = call
                            .changes
                            .iter()
                            .map(|ch| match ch {
                                Change::Add { path, .. } => format!("A {path}"),
                                Change::Delete { path } => format!("D {path}"),
                                Change::Update { path, .. } => format!("M {path}"),
                            })
                            .collect();
                        out.push(json!({"type": "function_call", "name": "exec_command", "arguments": json!({"cmd": cmd}).to_string(), "call_id": id}));
                        out.push(json!({"type": "function_call_output", "call_id": id, "output": format!("Process exited with code 0\nOutput:\nSuccess. Updated the following files:\n{}\n", listed.join("\n"))}));
                    }
                    Native::Words => out.push(msg("assistant", "output_text", &in_words(call))),
                }
            }
        }
    }
    if let Some(n) = note {
        out.push(msg("developer", "input_text", n));
    }
    out
}

// ---------------------------------------------------------------------- written for Claude Code

/// A tool_use id the Messages API takes (letters, digits, `_`, `-`), unique in the transcript.
fn tool_id(id: &str, n: usize, used: &mut std::collections::HashSet<String>) -> String {
    let clean: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    let id = if clean.is_empty() || used.contains(&clean) { format!("toolu_kiln_{n}") } else { clean };
    used.insert(id.clone());
    id
}

/// One tool_use and the tool_result that answers it.
fn claude_pairs(call: &Call, cwd: &str, n: usize, used: &mut std::collections::HashSet<String>) -> Option<Vec<(Value, Value, Value)>> {
    let abs = |p: &str| if Path::new(p).is_absolute() || cwd.is_empty() { p.to_owned() } else { Path::new(cwd).join(p).to_string_lossy().into_owned() };
    let result = |id: &str, text: String, error: bool| json!({"type": "tool_result", "tool_use_id": id, "content": text, "is_error": error});
    match native(call) {
        Native::Command(cmd) => {
            let id = tool_id(&call.id, n, used);
            let failed = call.error || call.exit_code.is_some_and(|c| c != 0);
            let text = match call.exit_code {
                Some(c) if c != 0 => format!("Exit code {c}\n{}", call.output),
                _ => call.output.clone(),
            };
            let structured = json!({"stdout": call.output, "stderr": "", "interrupted": false, "isImage": false});
            Some(vec![(json!({"type": "tool_use", "id": id, "name": "Bash", "input": {"command": cmd}}), result(&id, text, failed), structured)])
        }
        Native::Patch => {
            let mut out = Vec::new();
            for (i, ch) in call.changes.iter().enumerate() {
                match ch {
                    Change::Add { path, content } => {
                        let id = tool_id(&format!("{}_{i}", call.id), n * 100 + i, used);
                        let p = abs(path);
                        let structured = json!({"type": "create", "filePath": p, "content": content, "structuredPatch": [], "originalFile": null});
                        out.push((json!({"type": "tool_use", "id": id, "name": "Write", "input": {"file_path": p, "content": content}}), result(&id, format!("File created successfully at: {p}"), false), structured));
                    }
                    Change::Update { path, hunks, to: None } => {
                        for (j, h) in hunks.iter().enumerate() {
                            let old: Vec<&str> = h.iter().filter(|l| !l.starts_with('+')).map(|l| l.get(1..).unwrap_or("")).collect();
                            let new: Vec<&str> = h.iter().filter(|l| !l.starts_with('-')).map(|l| l.get(1..).unwrap_or("")).collect();
                            if old == new {
                                continue;
                            }
                            let id = tool_id(&format!("{}_{i}_{j}", call.id), n * 100 + i * 10 + j, used);
                            let p = abs(path);
                            let structured = json!({"filePath": p, "oldString": old.join("\n"), "newString": new.join("\n"), "structuredPatch": [{"oldStart": 1, "oldLines": old.len(), "newStart": 1, "newLines": new.len(), "lines": h}]});
                            out.push((
                                json!({"type": "tool_use", "id": id, "name": "Edit", "input": {"file_path": p, "old_string": old.join("\n"), "new_string": new.join("\n")}}),
                                result(&id, format!("The file {p} has been updated successfully."), false),
                                structured,
                            ));
                        }
                    }
                    // Moves and deletions: no faithful Edit or Write for them.
                    _ => return None,
                }
            }
            (!out.is_empty()).then_some(out)
        }
        Native::Words => None,
    }
}

/// Unix ms as RFC 3339.
fn rfc3339(ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok().and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()).unwrap_or_default()
}

/// The conversation as a Claude Code transcript (one record a line) for a new session `session`
/// in `cwd`, to start with `--resume <session>`: messages on one parent chain; calls as the
/// Bash, Edit and Write calls Claude Code makes itself (each answered in the next message, as
/// the API requires), or in words. `note`, for the model, goes first (as context, not as
/// something anyone said: a note left last would read as a prompt never answered). The last
/// reply carries the conversation's size, so Claude Code compacts it when it is too large for
/// the model.
pub fn to_claude(c: &Conversation, note: Option<&str>, session: &str, cwd: &str, model: &str) -> Vec<Value> {
    let mut recs: Vec<Value> = Vec::new();
    let mut parent: Option<String> = None;
    let t0 = crate::util::now_ms() - 1000;
    let mut used = std::collections::HashSet::new();
    let mut msg_n = 0usize;
    let mut push = |recs: &mut Vec<Value>, kind: &str, message: Value, extra: Value| -> String {
        let uuid = uuid::Uuid::now_v7().to_string();
        let mut r = json!({
            "parentUuid": parent,
            "isSidechain": false,
            "type": kind,
            "message": message,
            "uuid": uuid,
            "timestamp": rfc3339(t0 + i64::try_from(recs.len()).unwrap_or(0)),
            "userType": "external",
            "entrypoint": "sdk-cli",
            "cwd": cwd,
            "sessionId": session,
        });
        if let (Some(o), Some(x)) = (r.as_object_mut(), extra.as_object()) {
            for (k, v) in x {
                o.insert(k.clone(), v.clone());
            }
        }
        recs.push(r);
        parent = Some(uuid.clone());
        uuid
    };
    let mut assistant = |blocks: Vec<Value>, stop: &str| -> Value {
        msg_n += 1;
        json!({"id": format!("msg_kiln_{msg_n}"), "type": "message", "role": "assistant", "model": model, "content": blocks, "stop_reason": stop, "stop_sequence": null, "usage": {"input_tokens": 0, "output_tokens": 0}})
    };
    if let Some(n) = note {
        push(&mut recs, "user", json!({"role": "user", "content": [{"type": "text", "text": n}]}), json!({"isMeta": true}));
    }
    // The API's first message is the person's.
    let first_user = note.is_some() || c.summary.is_some() || matches!(c.entries.first(), Some(Entry::User { .. }));
    if let Some(s) = &c.summary {
        push(&mut recs, "user", json!({"role": "user", "content": s}), json!({}));
    } else if !first_user && !c.entries.is_empty() {
        push(&mut recs, "user", json!({"role": "user", "content": "(The conversation continues from here.)"}), json!({}));
    }
    // Words of the reply under way, waiting for its next block or its end.
    let mut pending: Vec<Value> = Vec::new();
    for (n, e) in c.entries.iter().enumerate() {
        match e {
            Entry::User { text, images } => {
                if !pending.is_empty() {
                    let m = assistant(std::mem::take(&mut pending), "end_turn");
                    push(&mut recs, "assistant", m, json!({}));
                }
                let content = if images.is_empty() {
                    json!(text)
                } else {
                    let mut blocks: Vec<Value> = images.iter().map(|i| json!({"type": "image", "source": {"type": "base64", "media_type": i.mime, "data": i.base64}})).collect();
                    if !text.is_empty() {
                        blocks.push(json!({"type": "text", "text": text}));
                    }
                    json!(blocks)
                };
                push(&mut recs, "user", json!({"role": "user", "content": content}), json!({}));
            }
            Entry::Assistant(t) => pending.push(json!({"type": "text", "text": t})),
            Entry::Call(call) => match claude_pairs(call, cwd, n, &mut used) {
                Some(pairs) => {
                    let mut blocks = std::mem::take(&mut pending);
                    blocks.extend(pairs.iter().map(|(u, _, _)| u.clone()));
                    let m = assistant(blocks, "tool_use");
                    let from = push(&mut recs, "assistant", m, json!({}));
                    let results: Vec<Value> = pairs.iter().map(|(_, r, _)| r.clone()).collect();
                    // (One structured result per record, as Claude Code keeps them.)
                    let structured = match pairs.as_slice() {
                        [(_, _, s)] => s.clone(),
                        _ => Value::Null,
                    };
                    let extra = if structured.is_null() { json!({"sourceToolAssistantUUID": from}) } else { json!({"sourceToolAssistantUUID": from, "toolUseResult": structured}) };
                    push(&mut recs, "user", json!({"role": "user", "content": results}), extra);
                }
                None => pending.push(json!({"type": "text", "text": in_words(call)})),
            },
        }
    }
    if !pending.is_empty() {
        let m = assistant(pending, "end_turn");
        push(&mut recs, "assistant", m, json!({}));
    }
    // How full the context is, for Claude Code's own accounting (about four bytes a token).
    let tokens = size(c) / 4;
    if let Some(last) = recs.iter_mut().rev().find(|r| str_of(r, "type") == Some("assistant"))
        && let Some(u) = last.pointer_mut("/message/usage")
    {
        *u = json!({"input_tokens": tokens, "output_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0});
    }
    recs
}

// ---------------------------------------------------------------------- the note to the model

/// What the target model is told about the switch: where the conversation came from, how its
/// calls are shown, and the plan as it stands.
pub fn note(from: &str, from_model: &str, to: &str, todos: &[Todo], dropped: usize) -> String {
    let model = if from_model.is_empty() { String::new() } else { format!(" (model {from_model})") };
    let mut s = format!(
        "This conversation began in {from}{model} and now continues here in {to}, with the person's messages and the tool calls carried over as they ran: \
         calls {to} has a counterpart for are shown as its own, the rest are described in words. Pick up exactly where it left off, as the same agent; don't start over or repeat work that is done."
    );
    if dropped > 0 {
        s.push_str(&format!(" {dropped} older messages were left out to fit the context window."));
    }
    if !todos.is_empty() {
        s.push_str("\n\nThe plan as it stands:\n");
        for t in todos {
            let mark = match t.status.as_str() {
                "completed" => "[x]",
                "in_progress" => "[~]",
                _ => "[ ]",
            };
            s.push_str(&format!("- {mark} {}\n", t.text));
        }
    }
    s
}

// ---------------------------------------------------------------------- settings carried over

/// How much a permission mode lets the agent do by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Latitude {
    ReadOnly,
    Ask,
    Edits,
    Auto,
    SkipAll,
}

fn latitude(kind: &str, mode: &str) -> Option<Latitude> {
    Some(match (kind, mode) {
        // Never asks, refuses what isn't allowed: no more than read-only (never escalated).
        ("claude", "plan" | "dontAsk") | ("codex", "read-only") => Latitude::ReadOnly,
        ("claude", "default") | ("codex", "untrusted") => Latitude::Ask,
        ("claude", "acceptEdits") | ("codex", "auto") => Latitude::Edits,
        ("claude", "auto") => Latitude::Auto,
        ("claude", "bypassPermissions") | ("codex", "full-access") => Latitude::SkipAll,
        _ => return None,
    })
}

fn mode_at(kind: &str, l: Latitude) -> Option<&'static str> {
    Some(match (kind, l) {
        ("claude", Latitude::ReadOnly) => "plan",
        ("claude", Latitude::Ask) => "default",
        ("claude", Latitude::Edits) => "acceptEdits",
        ("claude", Latitude::Auto) => "auto",
        ("claude", Latitude::SkipAll) => "bypassPermissions",
        ("codex", Latitude::ReadOnly) => "read-only",
        ("codex", Latitude::Ask) => "untrusted",
        ("codex", Latitude::Edits | Latitude::Auto) => "auto",
        ("codex", Latitude::SkipAll) => "full-access",
        _ => return None,
    })
}

/// The target's permission mode for the source's: the same latitude (plan → read-only, ask →
/// ask, accept edits ≈ Codex's auto), never more. Skipping every check carries over only from a
/// mode that already did, and only if the target offers it (`offered`); else the next one down.
/// A mode kiln doesn't know asks.
pub fn mode_for(from_kind: &str, mode: &str, to_kind: &str, offered: &[String]) -> Option<String> {
    let mut l = latitude(from_kind, mode).unwrap_or(Latitude::Ask);
    loop {
        if let Some(m) = mode_at(to_kind, l).filter(|m| offered.is_empty() || offered.iter().any(|o| o == m)) {
            return Some(m.to_owned());
        }
        l = match l {
            Latitude::SkipAll => Latitude::Auto,
            Latitude::Auto => Latitude::Edits,
            Latitude::Edits => Latitude::Ask,
            Latitude::Ask | Latitude::ReadOnly => return None,
        };
    }
}

/// Where an effort level sits, lowest first.
fn effort_rank(e: &str) -> Option<u8> {
    Some(match e {
        "none" => 0,
        "minimal" => 1,
        "low" => 2,
        "medium" => 3,
        "high" => 4,
        "xhigh" => 5,
        "max" => 6,
        "ultra" => 7,
        _ => return None,
    })
}

/// The target's effort level for the source's: the same one if it takes it, else the nearest by
/// rank (the lower of two as near). None: no effort (the model's own default).
pub fn effort_for(effort: &str, levels: &[String]) -> Option<String> {
    if effort.is_empty() {
        return None;
    }
    if levels.iter().any(|l| l == effort) {
        return Some(effort.to_owned());
    }
    let want = effort_rank(effort)?;
    levels.iter().filter_map(|l| Some((effort_rank(l)?, l))).min_by_key(|(r, _)| (r.abs_diff(want), *r)).map(|(_, l)| l.clone())
}

// ---------------------------------------------------------------------- the files

/// Largest transcript or rollout read for a handoff (its end).
const READ_MAX: u64 = 64 << 20;

/// The conversation of Claude Code session `session` (blocking: reads its transcript).
pub fn read_claude(env: &[(String, String)], session: &str) -> Option<Conversation> {
    let file = super::transcript::find(&super::transcript::config_dir(env), session)?;
    let lines = super::transcript::tail_lines(&file, READ_MAX)?;
    Some(from_claude(&lines))
}

/// Codex's home: `CODEX_HOME` from `env` (the session's) or this process's, else `~/.codex`.
pub fn codex_home(env: &[(String, String)]) -> PathBuf {
    env.iter()
        .rev()
        .find(|(k, _)| k == "CODEX_HOME")
        .map(|(_, v)| PathBuf::from(v))
        .or_else(|| std::env::var_os("CODEX_HOME").map(PathBuf::from))
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| crate::os::home_dir().join(".codex"))
}

/// The rollout segments of Codex thread `thread` under `home` (its sessions, archived ones too).
pub(crate) fn codex_files(home: &Path, thread: &str) -> Vec<PathBuf> {
    if thread.is_empty() || thread.contains(['/', '\\']) || thread.contains("..") {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut dirs = vec![(home.join("sessions"), 0u8), (home.join("archived_sessions"), 0u8)];
    while let Some((d, depth)) = dirs.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                if depth < 4 {
                    dirs.push((p, depth + 1));
                }
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("rollout-") && (name.ends_with(&format!("-{thread}.jsonl")) || name.contains(&format!("-{thread}_"))) {
                out.push(p);
            }
        }
    }
    out
}

/// The conversation of Codex thread `thread` (blocking: reads its rollout).
pub fn read_codex(env: &[(String, String)], thread: &str) -> Option<Conversation> {
    let home = codex_home(env);
    let files = codex_files(&home, thread);
    if files.is_empty() {
        return None;
    }
    let mut segments = Vec::new();
    for f in files {
        segments.push(super::transcript::tail_lines(&f, READ_MAX)?);
    }
    codex_of_lines(&segments, thread)
}

/// The conversation of Codex thread `thread` from the lines of its rollout files (a copy of them:
/// the conversation store's).
pub fn codex_of_lines(files: &[Vec<String>], thread: &str) -> Option<Conversation> {
    if files.is_empty() {
        return None;
    }
    let mut segments = Vec::new();
    for lines in files {
        let recs: Vec<Value> = lines.iter().filter_map(|l| serde_json::from_str(l).ok()).collect();
        let meta = recs.iter().find(|r| str_of(r, "type") == Some("session_meta"));
        // A segment of this thread (a branch's own is another thread's file).
        if meta.and_then(|m| m.pointer("/payload/id")).and_then(Value::as_str).is_some_and(|id| id != thread) {
            continue;
        }
        let first = meta.and_then(|m| m.get("ordinal")).and_then(Value::as_u64);
        let base = meta.and_then(|m| m.pointer("/payload/history_base/end_ordinal_exclusive")).and_then(Value::as_u64);
        segments.push((first, base, recs));
    }
    Some(from_codex(&codex_history(segments)))
}

/// Claude Code's folder for a working directory's sessions: the path with every character that
/// isn't a letter or digit made `-`. Claude Code shortens a long one and adds a hash kiln can't
/// work out: for those, the folder Claude Code already made for it (its newest).
pub fn claude_project_dir(config: &Path, cwd: &str) -> PathBuf {
    let name: String = cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let projects = config.join("projects");
    if name.len() <= 200 {
        return projects.join(name);
    }
    let prefix = format!("{}-", name.get(..200).unwrap_or(&name));
    std::fs::read_dir(&projects)
        .ok()
        .and_then(|rd| {
            rd.flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
                .max_by_key(|e| e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH))
                .map(|e| e.path())
        })
        .unwrap_or_else(|| projects.join(name))
}

/// Write a Claude Code transcript for `session` (blocking): where `claude --resume <session>`
/// run in `cwd` finds it.
pub fn write_claude(env: &[(String, String)], cwd: &str, session: &str, records: &[Value]) -> anyhow::Result<PathBuf> {
    use std::io::Write as _;
    let dir = claude_project_dir(&super::transcript::config_dir(env), cwd);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{session}.jsonl"));
    let mut f = std::fs::OpenOptions::new().create_new(true).write(true).open(&path)?;
    let mut buf = Vec::new();
    for r in records {
        serde_json::to_writer(&mut buf, r)?;
        buf.push(b'\n');
    }
    f.write_all(&buf)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kilns_timeline_in_words_for_a_harness_whose_record_kiln_doesnt_keep() -> anyhow::Result<()> {
        use crate::harness::model::{Event, ToolCall};
        let ev = |id: &str, parent: Option<&str>, body: Body| Event { id: id.into(), seq: 0, at: 0, parent: parent.map(str::to_owned), by: None, body };
        let user = |text: &str, origin: Option<&str>| Body::User { text: text.into(), images: 0, client_id: None, origin: origin.map(str::to_owned), attachments: vec![] };
        let tool: ToolCall = serde_json::from_value(json!({"tool_id": "t1", "name": "run_shell_command", "tool": "shell", "title": "cargo test", "input": {"command": "cargo test"}, "status": "error", "output": "1 failed", "exit_code": 101, "started": 0}))?;
        let events = vec![
            ev("u1", None, user("fix the build", Some("human"))),
            ev("a1", None, Body::Assistant { text: "Running the tests.".into(), streaming: false }),
            ev("t1", None, Body::Tool(Box::new(tool))),
            ev("s1", None, user("a reminder kiln sent", Some("system"))),
            ev("x1", Some("t9"), Body::Assistant { text: "a subagent's words".into(), streaming: false }),
            ev("n1", None, Body::Notice { level: crate::harness::model::Level::Info, text: "compacted".into() }),
        ];
        let c = from_events(&events);
        assert_eq!(c.entries.len(), 3, "{:?}", c.entries);
        let words = as_words(c, "It went on from mac, which is gone.", WORDS_BUDGET);
        assert!(words.starts_with("<kiln_handoff>\nIt went on from mac, which is gone."));
        assert!(words.ends_with("</kiln_handoff>"));
        let (asked, said, ran) = (words.find("The person said:\nfix the build"), words.find("You said:\nRunning the tests."), words.find("You ran `cargo test` (exit code 101); it failed:\n1 failed"));
        assert!(asked.is_some() && said.is_some() && ran.is_some(), "{words}");
        assert!(asked < said && said < ran, "oldest first");
        assert!(!words.contains("a reminder kiln sent") && !words.contains("a subagent's words") && !words.contains("compacted"));
        // Too long for its budget: the oldest go (the first thing asked stays), and it says so.
        let many: Vec<Event> = (0..400).map(|i| ev(&format!("a{i}"), None, Body::Assistant { text: format!("{i} {}", "x".repeat(1000)), streaming: false })).collect();
        let mut all = vec![ev("u0", None, user("the goal", Some("human")))];
        all.extend(many);
        let words = as_words(from_events(&all), "", 50 * 1024);
        assert!(words.len() < 60 * 1024 && words.contains("the goal") && words.contains("left out") && words.contains("399 "));
        Ok(())
    }

    fn lines(v: &[Value]) -> Vec<String> {
        v.iter().map(Value::to_string).collect()
    }

    fn rec(kind: &str, uuid: &str, parent: Option<&str>, message: Value) -> Value {
        json!({"type": kind, "uuid": uuid, "parentUuid": parent, "isSidechain": false, "message": message, "cwd": "/w"})
    }

    /// A Claude Code session: a prompt, a command, an edit, a todo list, thinking, a compaction
    /// kept apart, and a message steered in.
    fn claude_session() -> Vec<Value> {
        vec![
            rec("user", "u0", None, json!({"role": "user", "content": "old, before the compaction"})),
            json!({"type": "system", "subtype": "compact_boundary", "uuid": "b1", "parentUuid": null, "logicalParentUuid": "u0", "compactMetadata": {"preservedMessages": {"uuids": []}}}),
            json!({"type": "user", "uuid": "s1", "parentUuid": "b1", "isCompactSummary": true, "message": {"role": "user", "content": "Summary: we set things up."}}),
            rec("user", "u1", Some("s1"), json!({"role": "user", "content": "fix the bug in a.txt"})),
            json!({"type": "attachment", "uuid": "at1", "parentUuid": "u1", "attachment": {"type": "date", "date": "x"}}),
            rec("assistant", "a1", Some("at1"), json!({"id": "m1", "role": "assistant", "content": [{"type": "thinking", "thinking": "hmm", "signature": "sig"}]})),
            rec("assistant", "a2", Some("a1"), json!({"id": "m1", "role": "assistant", "content": [{"type": "text", "text": "Looking."}]})),
            rec("assistant", "a3", Some("a2"), json!({"id": "m1", "role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "cat a.txt", "description": "read it"}}]})),
            json!({"type": "user", "uuid": "r1", "parentUuid": "a3", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "short", "is_error": false}]}, "toolUseResult": {"stdout": "alpha\nline two", "stderr": "", "interrupted": false}}),
            rec("assistant", "a4", Some("r1"), json!({"id": "m2", "role": "assistant", "content": [{"type": "tool_use", "id": "t2", "name": "Edit", "input": {"file_path": "/w/a.txt", "old_string": "alpha", "new_string": "beta"}}]})),
            json!({"type": "user", "uuid": "r2", "parentUuid": "a4", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t2", "content": "The file /w/a.txt has been updated successfully."}]}, "toolUseResult": {"filePath": "/w/a.txt", "structuredPatch": [{"oldStart": 1, "oldLines": 2, "newStart": 1, "newLines": 2, "lines": ["-alpha", "+beta", " line two"]}]}}),
            json!({"type": "attachment", "uuid": "q1", "parentUuid": "r2", "attachment": {"type": "queued_command", "prompt": "also add a test"}}),
            rec("assistant", "a5", Some("q1"), json!({"id": "m3", "role": "assistant", "content": [{"type": "tool_use", "id": "t3", "name": "TodoWrite", "input": {"todos": [{"content": "fix", "status": "completed"}, {"content": "test", "status": "in_progress"}]}}]})),
            json!({"type": "user", "uuid": "r3", "parentUuid": "a5", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t3", "content": "ok"}]}}),
            rec("assistant", "a6", Some("r3"), json!({"id": "m4", "role": "assistant", "content": [{"type": "tool_use", "id": "t4", "name": "WebFetch", "input": {"url": "https://example.com", "prompt": "what"}}]})),
            json!({"type": "user", "uuid": "r4", "parentUuid": "a6", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t4", "content": "a page"}]}}),
            json!({"type": "user", "uuid": "m5", "parentUuid": "r4", "isMeta": true, "message": {"role": "user", "content": "<local-command-caveat>x</local-command-caveat>"}}),
            rec("assistant", "a7", Some("m5"), json!({"id": "m5", "role": "assistant", "content": [{"type": "text", "text": "Fixed it."}]})),
            rec("user", "u9", Some("a7"), json!({"role": "user", "content": [{"type": "text", "text": "<system-reminder>\nignore\n</system-reminder>"}]})),
        ]
    }

    #[test]
    fn a_claude_transcript_reads_from_its_last_compaction() {
        let c = from_claude(&lines(&claude_session()));
        assert_eq!(c.summary.as_deref(), Some("Summary: we set things up."));
        assert_eq!(c.todos.iter().map(|t| (t.text.as_str(), t.status.as_str())).collect::<Vec<_>>(), vec![("fix", "completed"), ("test", "in_progress")]);
        let shape: Vec<String> = c
            .entries
            .iter()
            .map(|e| match e {
                Entry::User { text, .. } => format!("user:{text}"),
                Entry::Assistant(t) => format!("assistant:{t}"),
                Entry::Call(call) => format!("call:{}:{:?}:{}", call.name, call.exit_code, call.output.lines().next().unwrap_or("")),
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                "user:fix the bug in a.txt",
                "assistant:Looking.",
                // The whole stdout, not the tool_result's short form.
                "call:Bash:Some(0):alpha",
                "call:Edit:None:The file /w/a.txt has been updated successfully.",
                "user:also add a test",
                "call:WebFetch:None:a page",
                "assistant:Fixed it.",
            ]
        );
        let hunks = vec![vec!["-alpha".to_owned(), "+beta".to_owned(), " line two".to_owned()]];
        assert!(matches!(c.entries.get(3), Some(Entry::Call(edit)) if edit.changes == vec![Change::Update { path: "/w/a.txt".into(), hunks, to: None }]));
    }

    #[test]
    fn codex_takes_claude_calls_as_its_own() {
        let c = from_claude(&lines(&claude_session()));
        let items = to_codex(&c, Some("<kiln_handoff>note</kiln_handoff>"));
        let kinds: Vec<String> = items.iter().map(|i| format!("{}:{}", str_of(i, "type").unwrap_or(""), str_of(i, "role").or_else(|| str_of(i, "name")).unwrap_or(""))).collect();
        assert_eq!(
            kinds,
            vec![
                "message:user",
                "message:user",
                "message:assistant",
                "function_call:exec_command",
                "function_call_output:",
                "function_call:exec_command",
                "function_call_output:",
                "message:user",
                "message:assistant",
                "message:assistant",
                "message:developer",
            ]
        );
        let args = |i: usize| items.get(i).and_then(|x| str_of(x, "arguments")).and_then(|a| serde_json::from_str::<Value>(a).ok()).and_then(|a| a.get("cmd").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default();
        assert_eq!(args(3), "cat a.txt");
        assert_eq!(items.get(4).and_then(|x| str_of(x, "output")), Some("Process exited with code 0\nOutput:\nalpha\nline two"));
        assert!(args(5).starts_with("apply_patch <<'EOF'\n*** Begin Patch\n*** Update File: /w/a.txt\n@@\n-alpha\n+beta\n line two\n*** End Patch\nEOF"));
        // A call Codex has no counterpart for is said in words: what ran and what it returned.
        let words = items.get(8).and_then(|x| x.pointer("/content/0/text")).and_then(Value::as_str).unwrap_or("");
        assert!(words.contains("WebFetch") && words.contains("https://example.com") && words.contains("a page"), "{words}");
        // Each call has its output, under its id.
        let calls: Vec<&str> = items.iter().filter(|i| str_of(i, "type") == Some("function_call")).filter_map(|i| str_of(i, "call_id")).collect();
        let outs: Vec<&str> = items.iter().filter(|i| str_of(i, "type") == Some("function_call_output")).filter_map(|i| str_of(i, "call_id")).collect();
        assert_eq!(calls, outs);
    }

    /// A Codex rollout: context, a command, a patch run as a command, a freeform patch, a plan,
    /// reasoning, code mode's own tool.
    fn codex_rollout() -> Vec<Value> {
        let item = |p: Value| json!({"type": "response_item", "payload": p});
        vec![
            json!({"type": "session_meta", "ordinal": 0, "payload": {"id": "T", "cwd": "/w"}}),
            item(json!({"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "<permissions instructions>"}]})),
            item(json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "<environment_context>x</environment_context>"}], "internal_chat_message_metadata_passthrough": {"content_item_kinds": ["environments.environment_context"]}})),
            item(json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "look at this"}, {"type": "input_text", "text": "<image name=[Image #1] path=\"/tmp/p.png\">"}, {"type": "input_image", "image_url": "data:image/png;base64,AAAA"}, {"type": "input_text", "text": "</image>"}], "internal_chat_message_metadata_passthrough": {"content_item_kinds": ["user.text", "user.text", "user.image", "user.text"]}})),
            item(json!({"type": "reasoning", "summary": [], "encrypted_content": "zzz"})),
            item(json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Running it."}]})),
            item(json!({"type": "function_call", "name": "exec_command", "arguments": "{\"cmd\":\"make test\",\"workdir\":\"/w/sub\"}", "call_id": "c1"})),
            item(json!({"type": "function_call_output", "call_id": "c1", "output": "Chunk ID: 1\nWall time: 0.1 seconds\nProcess exited with code 2\nOriginal token count: 3\nOutput:\nFAIL one\n"})),
            item(json!({"type": "function_call", "name": "exec_command", "arguments": "{\"cmd\":\"apply_patch <<'EOF'\\n*** Begin Patch\\n*** Add File: new.txt\\n+hello\\n*** End Patch\\nEOF\\n\"}", "call_id": "c2"})),
            item(json!({"type": "function_call_output", "call_id": "c2", "output": "Wall time: 0 seconds\nOutput:\nExit code: 0\nWall time: 0 seconds\nOutput:\nSuccess. Updated the following files:\nA new.txt\n"})),
            item(json!({"type": "custom_tool_call", "name": "apply_patch", "input": "*** Begin Patch\n*** Update File: a.txt\n@@ fn x\n-alpha\n+beta\n*** End Patch", "call_id": "c3"})),
            item(json!({"type": "custom_tool_call_output", "call_id": "c3", "output": "Success. Updated the following files:\nM a.txt"})),
            item(json!({"type": "function_call", "name": "update_plan", "arguments": "{\"plan\":[{\"step\":\"fix\",\"status\":\"completed\"},{\"step\":\"ship\",\"status\":\"in_progress\"}]}", "call_id": "c4"})),
            item(json!({"type": "function_call_output", "call_id": "c4", "output": "Plan updated"})),
            item(json!({"type": "custom_tool_call", "name": "exec", "input": "const r = await tools.exec_command({cmd: 'ls'}); text(r.output);", "call_id": "c5"})),
            item(json!({"type": "custom_tool_call_output", "call_id": "c5", "output": [{"type": "input_text", "text": "a.txt\nnew.txt"}]})),
            item(json!({"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "<kiln_handoff>old note</kiln_handoff>"}]})),
            item(json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Done."}]})),
        ]
    }

    #[test]
    fn a_codex_rollout_reads_as_the_model_saw_it() {
        let c = from_codex(&codex_rollout());
        assert_eq!(c.todos.iter().map(|t| t.status.as_str()).collect::<Vec<_>>(), vec!["completed", "in_progress"]);
        // The person's words and image, without the image's wrapper.
        assert!(matches!(c.entries.first(), Some(Entry::User { text, images }) if text == "look at this" && images.len() == 1));
        let calls: Vec<&Call> = c.entries.iter().filter_map(|e| if let Entry::Call(call) = e { Some(&**call) } else { None }).collect();
        let summary: Vec<(String, Option<String>, Option<i32>, bool, usize)> = calls.iter().map(|c| (c.name.clone(), c.command.clone(), c.exit_code, c.error, c.changes.len())).collect();
        assert_eq!(
            summary,
            vec![
                ("exec_command".into(), Some("cd '/w/sub' && make test".into()), Some(2), true, 0),
                ("exec_command".into(), None, Some(0), false, 1),
                ("apply_patch".into(), None, None, false, 1),
                ("exec".into(), None, None, false, 0),
            ]
        );
        assert_eq!(calls.first().map(|c| c.output.as_str()), Some("FAIL one"));
        assert_eq!(calls.get(1).map(|c| c.changes.clone()), Some(vec![Change::Add { path: "new.txt".into(), content: "hello\n".into() }]));
        assert_eq!(calls.get(3).map(|c| c.output.as_str()), Some("a.txt\nnew.txt"));
        assert!(matches!(c.entries.last(), Some(Entry::Assistant(t)) if t == "Done."));
    }

    #[test]
    fn claude_code_takes_codex_calls_as_its_own() {
        let c = from_codex(&codex_rollout());
        let recs = to_claude(&c, Some("<system-reminder>note</system-reminder>"), "S", "/w", "claude-opus-5-5");
        // One parent chain, oldest first.
        let mut parent: Option<String> = None;
        for r in &recs {
            assert_eq!(r.get("parentUuid").and_then(Value::as_str).map(str::to_owned), parent);
            parent = str_of(r, "uuid").map(str::to_owned);
            assert_eq!(str_of(r, "sessionId"), Some("S"));
        }
        // The person (or the note) speaks first; every tool_use is answered in the very next
        // message.
        assert_eq!(recs.first().and_then(|r| str_of(r, "type")), Some("user"));
        for (i, r) in recs.iter().enumerate() {
            let uses: Vec<&str> = r.pointer("/message/content").and_then(Value::as_array).into_iter().flatten().filter(|b| str_of(b, "type") == Some("tool_use")).filter_map(|b| str_of(b, "id")).collect();
            if uses.is_empty() {
                continue;
            }
            let next = recs.get(i + 1);
            let answered: Vec<&str> = next.and_then(|n| n.pointer("/message/content")).and_then(Value::as_array).into_iter().flatten().filter_map(|b| str_of(b, "tool_use_id")).collect();
            assert_eq!(uses, answered);
        }
        let tools: Vec<(String, Value)> = recs
            .iter()
            .flat_map(|r| r.pointer("/message/content").and_then(Value::as_array).cloned().unwrap_or_default())
            .filter(|b| str_of(b, "type") == Some("tool_use"))
            .map(|b| (str_of(&b, "name").unwrap_or("").to_owned(), b.get("input").cloned().unwrap_or(Value::Null)))
            .collect();
        assert_eq!(
            tools,
            vec![
                ("Bash".into(), json!({"command": "cd '/w/sub' && make test"})),
                ("Write".into(), json!({"file_path": "/w/new.txt", "content": "hello\n"})),
                ("Edit".into(), json!({"file_path": "/w/a.txt", "old_string": "alpha", "new_string": "beta"})),
            ]
        );
        let failed = recs.iter().flat_map(|r| r.pointer("/message/content").and_then(Value::as_array).cloned().unwrap_or_default()).find(|b| b.get("is_error") == Some(&json!(true)));
        assert_eq!(failed.as_ref().and_then(|b| str_of(b, "content")), Some("Exit code 2\nFAIL one"));
        // Code mode's script has no counterpart: in words. The note is context, first.
        let all = serde_json::to_string(&recs).unwrap_or_default();
        assert!(all.contains("I used the exec tool"));
        assert_eq!(recs.first().and_then(|r| r.get("isMeta")), Some(&json!(true)));
        assert_eq!(recs.get(1).and_then(|r| r.pointer("/message/content/1/text")).and_then(Value::as_str), Some("look at this"));
        // Back again: Claude Code's own record reads as the same conversation.
        let back = from_claude(&lines(&recs));
        assert!(matches!(back.entries.first(), Some(Entry::User { images, .. }) if images.len() == 1));
        let again: Vec<(String, Option<String>, Vec<Change>)> = back.entries.iter().filter_map(|e| if let Entry::Call(c) = e { Some((c.name.clone(), c.command.clone(), c.changes.clone())) } else { None }).collect();
        assert_eq!(
            again,
            vec![
                ("Bash".into(), Some("cd '/w/sub' && make test".into()), vec![]),
                ("Write".into(), None, vec![Change::Add { path: "/w/new.txt".into(), content: "hello\n".into() }]),
                ("Edit".into(), None, vec![Change::Update { path: "/w/a.txt".into(), hunks: vec![vec!["-alpha".into(), "+beta".into()]], to: None }]),
            ]
        );
    }

    #[test]
    fn a_rewound_codex_thread_reads_its_kept_history() {
        let r = |o: u64, t: &str| json!({"type": "response_item", "ordinal": o, "timestamp": "2026-10-17T00:00:00Z", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": t}]}});
        let base = vec![json!({"type": "session_meta", "ordinal": 0, "timestamp": "2026-10-17T00:00:00Z", "payload": {"id": "T"}}), r(1, "one"), r(2, "two (rewound)")];
        let part = vec![json!({"type": "session_meta", "ordinal": 2, "timestamp": "2026-10-17T00:01:00Z", "payload": {"id": "T", "history_base": {"end_ordinal_exclusive": 2}}}), r(3, "three")];
        let part2 = vec![json!({"type": "session_meta", "ordinal": 4, "timestamp": "2026-10-17T00:02:00Z", "payload": {"id": "T", "history_base": {"end_ordinal_exclusive": 4}}}), r(5, "five")];
        let all = codex_history(vec![(Some(4), Some(4), part2), (Some(0), None, base), (Some(2), Some(2), part)]);
        let c = from_codex(&all);
        let texts: Vec<&str> = c.entries.iter().filter_map(|e| if let Entry::User { text, .. } = e { Some(text.as_str()) } else { None }).collect();
        assert_eq!(texts, vec!["one", "three", "five"]);
        // A compaction: what the model sees starts with its history.
        let compacted = json!({"type": "compacted", "payload": {"message": "s", "replacement_history": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "kept"}], "internal_chat_message_metadata_passthrough": {"content_item_kinds": ["user.text"]}},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "the summary"}], "internal_chat_message_metadata_passthrough": {"content_item_kinds": ["compaction.summary"]}}
        ]}});
        let c = from_codex(&[r(1, "gone"), compacted, r(2, "after")]);
        assert_eq!(c.summary.as_deref(), Some("the summary"));
        let texts: Vec<&str> = c.entries.iter().filter_map(|e| if let Entry::User { text, .. } = e { Some(text.as_str()) } else { None }).collect();
        assert_eq!(texts, vec!["kept", "after"]);
    }

    #[test]
    fn patches_read_and_write_the_same() {
        let text = "*** Begin Patch\n*** Add File: a.txt\n+one\n+two\n*** Update File: b.txt\n*** Move to: c.txt\n@@ fn main\n ctx\n-old\n+new\n@@\n-x\n*** End of File\n*** Delete File: d.txt\n*** End Patch";
        let changes = parse_patch(text);
        assert_eq!(
            changes,
            vec![
                Change::Add { path: "a.txt".into(), content: "one\ntwo\n".into() },
                Change::Update { path: "b.txt".into(), hunks: vec![vec![" ctx".into(), "-old".into(), "+new".into()], vec!["-x".into()]], to: Some("c.txt".into()) },
                Change::Delete { path: "d.txt".into() },
            ]
        );
        assert_eq!(parse_patch(&patch_text(&changes)), changes);
        assert_eq!(exec_output("Chunk ID: 9\nProcess exited with code 0\nOutput:\nok\n"), (Some(0), "ok".to_owned()));
        assert_eq!(exec_output("plain"), (None, "plain".to_owned()));
    }

    #[test]
    fn settings_carry_over_without_escalating() {
        let claude: Vec<String> = ["default", "acceptEdits", "plan", "auto", "dontAsk", "bypassPermissions"].iter().map(|s| (*s).to_owned()).collect();
        let codex: Vec<String> = ["auto", "read-only", "untrusted", "full-access"].iter().map(|s| (*s).to_owned()).collect();
        let to_codex = |m: &str| mode_for("claude", m, "codex", &codex);
        let to_claude = |m: &str| mode_for("codex", m, "claude", &claude);
        assert_eq!(
            ["plan", "dontAsk", "default", "acceptEdits", "auto", "bypassPermissions", "weird"].map(|m| to_codex(m).unwrap_or_default()),
            ["read-only", "read-only", "untrusted", "auto", "auto", "full-access", "untrusted"].map(str::to_owned)
        );
        assert_eq!(["read-only", "untrusted", "auto", "full-access"].map(|m| to_claude(m).unwrap_or_default()), ["plan", "default", "acceptEdits", "bypassPermissions"].map(str::to_owned));
        // Skipping every check only where it's offered: else the next one down.
        let safe: Vec<String> = codex.iter().filter(|m| *m != "full-access").cloned().collect();
        assert_eq!(mode_for("claude", "bypassPermissions", "codex", &safe).as_deref(), Some("auto"));
        let levels = |l: &[&str]| l.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(effort_for("max", &levels(&["low", "medium", "high", "xhigh", "max", "ultra"])).as_deref(), Some("max"));
        assert_eq!(effort_for("max", &levels(&["low", "medium", "high", "xhigh"])).as_deref(), Some("xhigh"));
        assert_eq!(effort_for("ultra", &levels(&["low", "medium", "high", "xhigh", "max"])).as_deref(), Some("max"));
        assert_eq!(effort_for("minimal", &levels(&["low", "high"])).as_deref(), Some("low"));
        assert_eq!(effort_for("", &levels(&["low"])), None);
    }

    #[test]
    fn a_large_conversation_is_made_to_fit() {
        let mut c = Conversation::default();
        c.entries.push(Entry::User { text: "the goal".into(), images: Vec::new() });
        for i in 0..40 {
            let mut call = Call::new(&format!("c{i}"), "Bash", ToolKind::Shell, json!({"command": "x"}));
            call.command = Some("x".into());
            call.output = "o".repeat(20_000);
            c.entries.push(Entry::Call(Box::new(call)));
            c.entries.push(Entry::Assistant(format!("step {i}")));
        }
        // Outputs are shortened first: every message stays.
        let mut a = c.clone();
        assert_eq!(fit(&mut a, 400_000), 0);
        assert_eq!(a.entries.len(), c.entries.len());
        assert!(size(&a) <= 400_000);
        // Then the oldest go; the goal stays, and a note says what's left out.
        let mut b = c.clone();
        let dropped = fit(&mut b, 100_000);
        assert!(dropped > 0 && size(&b) <= 100_000 + 200);
        assert!(matches!(b.entries.first(), Some(Entry::User { text, .. }) if text == "the goal"));
        assert!(matches!(b.entries.get(1), Some(Entry::User { text, .. }) if text.contains("left out")));
        assert!(matches!(b.entries.last(), Some(Entry::Assistant(t)) if t == "step 39"));
    }

    #[test]
    fn transcripts_go_where_claude_code_looks() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        assert_eq!(claude_project_dir(dir.path(), "/home/u/my app"), dir.path().join("projects").join("-home-u-my-app"));
        let long = format!("/{}", "d".repeat(260));
        let made = dir.path().join("projects").join(format!("{}-abc123", "-".chars().chain("d".repeat(199).chars()).collect::<String>()));
        std::fs::create_dir_all(&made)?;
        assert_eq!(claude_project_dir(dir.path(), &long), made);
        let env = vec![("CLAUDE_CONFIG_DIR".to_owned(), dir.path().to_string_lossy().into_owned())];
        let recs = to_claude(&from_claude(&lines(&claude_session())), None, "S1", "/w", "m");
        let p = write_claude(&env, "/w", "S1", &recs)?;
        assert_eq!(p, dir.path().join("projects").join("-w").join("S1.jsonl"));
        let read = read_claude(&env, "S1").ok_or_else(|| anyhow::anyhow!("not found"))?;
        assert_eq!(read.entries.len(), from_claude(&lines(&claude_session())).entries.len());
        // Never over another session's transcript.
        assert!(write_claude(&env, "/w", "S1", &recs).is_err());
        Ok(())
    }
}

#[cfg(test)]
mod probe_tmp {
    use super::*;
    #[test]
    #[ignore]
    fn probe() -> anyhow::Result<()> {
        let out = std::env::var("PROBE_OUT")?;
        if let Ok(f) = std::env::var("PROBE_CLAUDE") {
            let lines = super::super::transcript::tail_lines(Path::new(&f), READ_MAX).unwrap_or_default();
            let c = from_claude(&lines);
            std::fs::write(format!("{out}/claude-ir.txt"), format!("{c:#?}"))?;
            let items = to_codex(&c, Some("<kiln_handoff>note</kiln_handoff>"));
            std::fs::write(format!("{out}/claude-to-codex.json"), serde_json::to_string_pretty(&items)?)?;
        }
        if let Ok(f) = std::env::var("PROBE_CODEX") {
            let (home, thread) = f.split_once('#').unwrap_or_default();
            let env = vec![("CODEX_HOME".to_owned(), home.to_owned())];
            let c = read_codex(&env, thread).unwrap_or_default();
            std::fs::write(format!("{out}/codex-ir.txt"), format!("{c:#?}"))?;
            let recs = to_claude(&c, Some("<system-reminder>note</system-reminder>"), "S", "/w", "claude-opus-5-5");
            std::fs::write(format!("{out}/codex-to-claude.jsonl"), recs.iter().map(|r| r.to_string()).collect::<Vec<_>>().join("\n"))?;
        }
        Ok(())
    }
}
