//! Claude Code, driven the way its Agent SDK drives it: `claude -p` with stream-json in both
//! directions and the control protocol on top. kiln is the whole front end: it sends prompts (with
//! a uuid each, so it can follow their fate), answers permission requests (`can_use_tool`),
//! interrupts, switches modes and models, stops subagent tasks and asks for file suggestions, and
//! renders everything the CLI streams back: replies as they are written, thinking, tool calls with
//! their results and real diffs, subagents with their own messages, usage.
//!
//! Message shapes were taken from Claude Code 2.1.296 (`system/init`, `command_lifecycle`,
//! `task_*`, `control_request`/`control_response`/`control_cancel_request`, `stream_event`,
//! `result`). Unknown messages and fields are ignored, so newer CLIs keep working.

use std::collections::{BTreeSet, HashMap};

use serde_json::{Value, json};

use super::core::{Core, Effect, PreviewJob};
use super::model::{
    AlwaysOption, Approval, Body, Command, Control, ControlKind, ControlOption, ControlScope, Decision, Delivery, ExitInfo, FileDiff, HookInfo, Hunk, Level, ModeOption, ModelOption,
    Question, QuestionOption, QueueItem, SendMode, SlashCommand, Status, Task, Todo, ToolCall, ToolKind, ToolStatus, OUTPUT_MAX,
    clip_json, clip_middle,
};

/// Environment variables a Claude Code session sets for its children; a harness kiln starts must
/// not inherit them (it would think it runs inside another session).
pub const SCRUB_ENV: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
    "AI_AGENT",
];

/// How kiln starts a Claude Code session.
pub fn args(spec: &super::session::Spec, session_id: Option<&str>) -> Vec<String> {
    let (model, mode, resume, allow_bypass) = (spec.model.as_deref(), spec.mode.as_deref(), spec.resume.as_deref(), spec.allow_bypass);
    let mut a: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--replay-user-messages",
        "--forward-subagent-text",
        "--permission-prompt-tool",
        "stdio",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    if let Some(m) = model.filter(|m| !m.is_empty()) {
        a.extend(["--model".to_owned(), m.to_owned()]);
    }
    if let Some(m) = mode.filter(|m| !m.is_empty()) {
        a.extend(["--permission-mode".to_owned(), m.to_owned()]);
    }
    if allow_bypass {
        a.push("--allow-dangerously-skip-permissions".to_owned());
    }
    if let Some(e) = spec.effort.as_deref().filter(|e| !e.is_empty()) {
        a.extend(["--effort".to_owned(), e.to_owned()]);
    }
    match (resume, session_id) {
        (Some(r), _) => {
            a.extend(["--resume".to_owned(), r.to_owned()]);
            // Only up to a point of that conversation (a rewind or a branch).
            if let Some(at) = spec.resume_at.as_deref() {
                a.extend(["--resume-session-at".to_owned(), at.to_owned()]);
            }
            if spec.fork {
                a.push("--fork-session".to_owned());
            }
        }
        (None, Some(id)) => a.extend(["--session-id".to_owned(), id.to_owned()]),
        (None, None) => {}
    }
    a
}

/// What one of our control requests was for.
#[derive(Debug, Clone)]
enum Pending {
    Initialize,
    SetMode(String),
    SetModel(String),
    Interrupt,
    StopTask,
    Suggest(String),
    ContextUsage,
    /// apply_flag_settings: the settings applied (read back with get_settings).
    ApplySettings,
    Settings,
    McpStatus,
    Hooks,
    /// A request whose answer goes to one client (task output, ...).
    Reply(String),
    /// A request that just needs to succeed (its failure is shown).
    Plain(String),
}

/// A permission request we are holding for a person: the input exactly as the CLI sent it (the
/// approval's copy is shortened for display; the answer must carry the original).
#[derive(Debug, Clone)]
struct Ask {
    input: Value,
}

/// The streaming state of one API message: which event each content block index became.
#[derive(Debug, Default)]
struct Streamed {
    /// Event id per block index (text/thinking blocks).
    blocks: HashMap<u64, String>,
    /// Final (non-partial) blocks seen so far for this message, in order.
    finals: u64,
}

pub struct Claude {
    next_req: u64,
    pending: HashMap<String, Pending>,
    asks: HashMap<String, Ask>,
    /// tool_use id → (tool name, input), to interpret results.
    tools: HashMap<String, (String, Value)>,
    streams: HashMap<String, Streamed>,
    /// Messages we sent that have not reached a terminal lifecycle state.
    inflight: BTreeSet<String>,
    /// A turn is under way (between a start signal and its result).
    turn: bool,
    /// Tool calls we denied (their error result reads as a denial, not a failure).
    denied: BTreeSet<String>,
    notice_seq: u64,
    allow_bypass: bool,
    /// Every conversation entry, in order: its uuid, and the prompt's event id when it is one
    /// (rewinds and branches name an entry).
    entries: Vec<(String, Option<String>)>,
    /// Effort levels per model, as initialize lists them.
    efforts: HashMap<String, Vec<String>>,
    output_styles: Vec<String>,
    /// MCP servers: (name, status).
    mcp: Vec<(String, String)>,
}

impl Claude {
    pub fn new(allow_bypass: bool) -> Self {
        Self {
            next_req: 0,
            pending: HashMap::new(),
            asks: HashMap::new(),
            tools: HashMap::new(),
            streams: HashMap::new(),
            inflight: BTreeSet::new(),
            turn: false,
            denied: BTreeSet::new(),
            notice_seq: 0,
            allow_bypass,
            entries: Vec::new(),
            efforts: HashMap::new(),
            output_styles: Vec::new(),
            mcp: Vec::new(),
        }
    }

    /// Note a conversation entry (`prompt`: the event id when it is a person's prompt).
    fn entry(&mut self, uuid: &str, prompt: Option<&str>) {
        if uuid.is_empty() || self.entries.last().is_some_and(|(u, _)| u == uuid) {
            return;
        }
        self.entries.push((uuid.to_owned(), prompt.map(str::to_owned)));
        if self.entries.len() > 20_000 {
            self.entries.drain(..5_000);
        }
    }

    /// For a prompt's event: (its entry index, the entry before it, the last entry of its turn).
    fn turn_of(&self, event: &str) -> Option<(usize, Option<String>, String)> {
        let i = self.entries.iter().position(|(_, p)| p.as_deref() == Some(event))?;
        let before = i.checked_sub(1).and_then(|j| self.entries.get(j)).map(|(u, _)| u.clone());
        let end = self.entries.iter().skip(i + 1).position(|(_, p)| p.is_some()).map_or(self.entries.len(), |n| i + 1 + n);
        let last = self.entries.get(end.saturating_sub(1)).map(|(u, _)| u.clone())?;
        Some((i, before, last))
    }

    /// The controls this session offers, from what the CLI said it has.
    fn refresh_controls(&mut self, core: &mut Core) {
        let opt = |id: &str, label: &str, description: &str| ControlOption { id: id.to_owned(), label: label.to_owned(), description: description.to_owned(), dangerous: false };
        let ctl = |id: &str, group: &str, label: &str, description: &str, kind: ControlKind| Control {
            id: id.to_owned(),
            group: group.to_owned(),
            label: label.to_owned(),
            description: description.to_owned(),
            kind,
            options: Vec::new(),
            value: None,
            dangerous: false,
            per_turn: false,
            requires: None,
            scope: ControlScope::Session,
            input: None,
        };
        let mut c = Vec::new();
        // Thinking.
        let mut t = ctl("thinking_display", "Thinking", "Thinking shown", "How much of the model's thinking is shown", ControlKind::Select);
        t.options = vec![opt("summarized", "Summaries", "A summary of each stretch of thinking"), opt("highlights", "Highlights", "One short title per stretch"), opt("omitted", "Hidden", "Only that it thought")];
        c.push(t);
        let mut t = ctl("thinking_budget", "Thinking", "Thinking budget", "Most tokens it may think for per step", ControlKind::Select);
        t.options = vec![opt("default", "Default", "The model's own"), opt("4000", "4k", ""), opt("16000", "16k", ""), opt("32000", "32k", ""), opt("0", "Off", "No extended thinking")];
        c.push(t);
        // Output.
        if !self.output_styles.is_empty() {
            let mut t = ctl("output_style", "Output", "Output style", "How it writes its replies", ControlKind::Select);
            t.options = self.output_styles.iter().map(|s| opt(s, s, "")).collect();
            c.push(t);
        }
        // Context.
        let mut t = ctl("compact", "Context", "Compact", "Summarize the conversation to free context", ControlKind::Action);
        t.input = Some("What to keep (optional)".into());
        c.push(t);
        c.push(ctl("context_usage", "Context", "Context usage", "Measure how full the context is", ControlKind::Action));
        let mut t = ctl("clear", "Context", "Clear conversation", "Start the conversation over (files stay as they are)", ControlKind::Action);
        t.dangerous = true;
        c.push(t);
        // Session.
        c.push(ctl("rename", "Session", "Name", "The session's name", ControlKind::Text));
        let mut t = ctl("rewind", "Session", "Rewind to here", "Undo this prompt and everything after it: the conversation and the file changes", ControlKind::Action);
        t.scope = ControlScope::Turn;
        t.dangerous = true;
        c.push(t);
        let mut t = ctl("rewind_files", "Session", "Restore files to here", "Undo the file changes made since this prompt; keep the conversation", ControlKind::Action);
        t.scope = ControlScope::Turn;
        t.dangerous = true;
        c.push(t);
        let mut t = ctl("branch", "Session", "Branch from here", "A new session with the conversation up to the end of this turn", ControlKind::Action);
        t.scope = ControlScope::Turn;
        c.push(t);
        // Subagents.
        if !core.meta.agents.is_empty() {
            let mut t = ctl("spawn_agent", "Subagents", "Start a subagent", "Ask it to hand a task to a subagent of this type", ControlKind::Action);
            t.options = core.meta.agents.iter().map(|a| opt(a, a, "")).collect();
            t.input = Some("The task".into());
            c.push(t);
        }
        c.push(ctl("background", "Subagents", "Move to background", "Let running commands and subagents continue in the background", ControlKind::Action));
        // MCP.
        for (name, status) in &self.mcp {
            let mut t = ctl(&format!("mcp:{name}"), "MCP", name, &format!("MCP server ({status})"), ControlKind::Toggle);
            t.value = Some(json!(status != "disabled"));
            c.push(t);
            if matches!(status.as_str(), "failed" | "needs-auth" | "pending") {
                c.push(ctl(&format!("mcp_reconnect:{name}"), "MCP", &format!("Reconnect {name}"), "", ControlKind::Action));
            }
        }
        c.push(ctl("mcp_refresh", "MCP", "Refresh servers", "Ask for every server's status again", ControlKind::Action));
        core.meta.controls = c;
        // Effort levels of the model in use.
        let model = core.state.model.clone();
        let levels = self.efforts.get(&model).or_else(|| self.efforts.iter().find(|(k, _)| model.contains(k.as_str())).map(|(_, v)| v)).or_else(|| self.efforts.get("default"));
        core.meta.efforts = levels.map(|l| l.iter().map(|e| opt(e, &capitalize(e), "")).collect()).unwrap_or_default();
        core.touch_meta();
    }

    /// Set a control or run an action.
    fn control(&mut self, core: &mut Core, id: &str, value: Option<Value>) {
        let text = || value.as_ref().and_then(|v| v.as_str().map(str::to_owned).or_else(|| v.get("text").and_then(Value::as_str).map(str::to_owned))).unwrap_or_default();
        let event = || value.as_ref().and_then(|v| v.get("event")).and_then(Value::as_str).map(str::to_owned);
        match id {
            "thinking_display" => {
                let d = text();
                self.request(core, Pending::Plain("thinking shown".into()), json!({"subtype": "set_max_thinking_tokens", "thinking_display": d}));
                core.set_control(id, json!(d));
            }
            "thinking_budget" => {
                let v = text();
                let budget = if v == "default" { Value::Null } else { v.parse::<u64>().map(|n| json!(n)).unwrap_or(Value::Null) };
                self.request(core, Pending::Plain("thinking budget".into()), json!({"subtype": "set_max_thinking_tokens", "max_thinking_tokens": budget}));
                core.set_control(id, json!(v));
            }
            "output_style" => {
                let s = text();
                self.request(core, Pending::ApplySettings, json!({"subtype": "apply_flag_settings", "settings": {"outputStyle": s}}));
                core.set_control(id, json!(s));
            }
            "compact" => {
                let t = text();
                self.slash(core, &format!("/compact {t}"));
            }
            "clear" => self.slash(core, "/clear"),
            "context_usage" => self.request(core, Pending::ContextUsage, json!({"subtype": "get_context_usage"})),
            "rename" => {
                let t = text();
                self.request(core, Pending::Plain("rename".into()), json!({"subtype": "rename_session", "title": t, "source": "host"}));
                core.state.title.clone_from(&t);
                core.set_control(id, json!(t));
            }
            "spawn_agent" => {
                let kind = value.as_ref().and_then(|v| v.get("option")).and_then(Value::as_str).unwrap_or("general-purpose").to_owned();
                let t = text();
                self.slash(core, &format!("Use the {kind} subagent for this task: {t}"));
            }
            "background" => self.request(core, Pending::Plain("background".into()), json!({"subtype": "background_tasks"})),
            "task_output" => {
                let task = value.as_ref().and_then(|v| v.get("task")).and_then(Value::as_str).unwrap_or("").to_owned();
                let client = value.as_ref().and_then(|v| v.get("client")).and_then(Value::as_str).unwrap_or("task_output").to_owned();
                self.request(core, Pending::Reply(client), json!({"subtype": "get_task_output", "task_id": task}));
            }
            "mcp_refresh" => self.request(core, Pending::McpStatus, json!({"subtype": "mcp_status"})),
            "rewind" | "rewind_files" => {
                let Some(ev) = event() else { return };
                let Some((i, before, _)) = self.turn_of(&ev) else {
                    self.notice(core, Level::Warn, "That prompt is not in this session's history.", None);
                    return;
                };
                let Some((uuid, _)) = self.entries.get(i).cloned() else { return };
                self.request(core, Pending::Plain("restore files".into()), json!({"subtype": "rewind_files", "user_message_id": uuid}));
                if id == "rewind" {
                    // The conversation continues from just before that prompt.
                    self.entries.truncate(i);
                    core.truncate_from(&ev);
                    core.effects.push(Effect::Restart { resume_at: before });
                }
            }
            "branch" => {
                let Some(ev) = event() else { return };
                let (Some((_, _, last)), Some(session)) = (self.turn_of(&ev), core.state.harness_session.clone()) else {
                    self.notice(core, Level::Warn, "That prompt is not in this session's history.", None);
                    return;
                };
                core.effects.push(Effect::Branch { resume: session, resume_at: Some(last) });
            }
            other => {
                if let Some(name) = other.strip_prefix("mcp_reconnect:") {
                    self.request(core, Pending::Plain(format!("reconnect {name}")), json!({"subtype": "mcp_reconnect", "serverName": name}));
                } else if let Some(name) = other.strip_prefix("mcp:") {
                    let on = value.as_ref().and_then(Value::as_bool).unwrap_or(true);
                    self.request(core, Pending::Plain(format!("MCP server {name}")), json!({"subtype": "mcp_toggle", "serverName": name, "enabled": on}));
                    core.set_control(other, json!(on));
                    self.request(core, Pending::McpStatus, json!({"subtype": "mcp_status"}));
                }
            }
        }
    }

    /// A slash command (or any text) sent as a message.
    fn slash(&mut self, core: &mut Core, text: &str) {
        let id = crate::util::new_id();
        self.send(core, id, text.trim().to_owned(), SendMode::Queue, Vec::new(), None);
    }

    fn request(&mut self, core: &mut Core, kind: Pending, body: Value) {
        self.next_req += 1;
        let id = format!("kiln_{}", self.next_req);
        self.pending.insert(id.clone(), kind);
        core.write(&json!({"type": "control_request", "request_id": id, "request": body}));
    }

    fn notice(&mut self, core: &mut Core, level: Level, text: impl Into<String>, parent: Option<String>) {
        self.notice_seq += 1;
        core.upsert(&format!("notice-{}", self.notice_seq), parent, Body::Notice { level, text: text.into() });
    }

    /// Something is being worked on: note when the turn began.
    fn begin(&mut self, core: &mut Core) {
        if !self.turn {
            self.turn = true;
            if core.state.turn_started.is_none() {
                core.state.turn_started = Some(Core::now());
                core.touch_state();
            }
        }
    }

    fn busy(&self, core: &Core) -> bool {
        self.turn || !self.inflight.is_empty() || !core.state.approvals.is_empty()
    }

    fn settle(&self, core: &mut Core) {
        let running = self.turn || !self.inflight.is_empty();
        core.settle_status(running);
    }

    /// Right after the process starts.
    pub fn start(&mut self, core: &mut Core) {
        core.meta.modes = modes(self.allow_bypass);
        core.meta.features = ["steer", "send_now", "queue", "interrupt", "set_mode", "set_model", "set_effort", "stop_task", "file_suggestions", "resume", "attachments", "images"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        core.touch_meta();
        self.request(core, Pending::Initialize, json!({"subtype": "initialize"}));
    }

    /// The process ended: nothing in flight survives it.
    pub fn exited(&mut self, core: &mut Core, code: Option<i32>, error: Option<String>) {
        self.turn = false;
        self.inflight.clear();
        self.pending.clear();
        self.asks.clear();
        core.state.approvals.clear();
        for id in self.tools.keys().cloned().collect::<Vec<_>>() {
            finish_tool(core, &id, ToolStatus::Cancelled, None);
        }
        core.state.exit = Some(ExitInfo { code, error });
        core.state.activity = None;
        core.state.turn_started = None;
        core.state.doing = None;
        core.set_status(Status::Exited);
        core.touch_state();
    }

    // ------------------------------------------------------------------ commands

    pub fn command(&mut self, core: &mut Core, cmd: Command) {
        match cmd {
            Command::Send { id, text, mode, attachments, overrides } => self.send(core, id, text, mode, attachments, overrides),
            Command::QueueEdit { id, text } => {
                if let Some(q) = core.state.queue.iter_mut().find(|q| q.id == id) {
                    q.text = text;
                    core.touch_state();
                }
            }
            Command::QueueRemove { id } => {
                core.state.queue.retain(|q| q.id != id);
                core.touch_state();
            }
            Command::QueueMove { id, index } => {
                if let Some(pos) = core.state.queue.iter().position(|q| q.id == id) {
                    let item = core.state.queue.remove(pos);
                    let index = index.min(core.state.queue.len());
                    core.state.queue.insert(index, item);
                    core.touch_state();
                }
            }
            Command::QueueSendNow { id } => {
                if let Some(pos) = core.state.queue.iter().position(|q| q.id == id) {
                    let item = core.state.queue.remove(pos);
                    core.touch_state();
                    self.send(core, item.id, item.text, SendMode::Now, item.attachments, item.overrides);
                }
            }
            Command::Approve { id, decision, message, always, answers } => self.answer(core, &id, decision, message, always, answers),
            Command::Interrupt => {
                if self.busy(core) {
                    self.request(core, Pending::Interrupt, json!({"subtype": "interrupt"}));
                }
            }
            Command::SetMode { mode } => {
                self.request(core, Pending::SetMode(mode.clone()), json!({"subtype": "set_permission_mode", "mode": mode}));
            }
            Command::SetModel { model } => {
                self.request(core, Pending::SetModel(model.clone()), json!({"subtype": "set_model", "model": model}));
            }
            Command::StopTask { task } => {
                self.request(core, Pending::StopTask, json!({"subtype": "stop_task", "task_id": task}));
            }
            Command::SetEffort { effort } => {
                self.request(core, Pending::ApplySettings, json!({"subtype": "apply_flag_settings", "settings": {"effortLevel": effort}}));
                core.state.effort = effort;
                core.touch_state();
            }
            Command::Control { id, value } => self.control(core, &id, value),
            Command::Suggest { client, query } => {
                self.request(core, Pending::Suggest(client), json!({"subtype": "file_suggestions", "query": query}));
            }
            // Restarts and snapshots are the session's (it owns the process and the sink).
            Command::Restart | Command::Snapshot => {}
        }
    }

    fn send(&mut self, core: &mut Core, id: String, text: String, mode: SendMode, attachments: Vec<super::model::Attachment>, overrides: Option<super::model::Overrides>) {
        let text = text.trim_end().to_owned();
        if text.is_empty() && attachments.is_empty() {
            return;
        }
        let busy = self.busy(core);
        if busy && mode == SendMode::Queue {
            core.state.queue.push(QueueItem { id, text, at: Core::now(), attachments, overrides });
            core.touch_state();
            return;
        }
        // Images as image blocks; other files by path (the agent reads them with its tools).
        let content = if attachments.is_empty() {
            json!(text)
        } else {
            let mut blocks: Vec<Value> = attachments
                .iter()
                .filter(|a| a.is_image() && !a.data_base64.is_empty())
                .map(|a| json!({"type": "image", "source": {"type": "base64", "media_type": a.mime, "data": a.data_base64}}))
                .collect();
            let mut words = text.clone();
            let listed: Vec<String> = attachments
                .iter()
                .filter(|a| !(a.is_image() && !a.data_base64.is_empty()))
                .filter_map(|a| a.path.as_ref().map(|p| format!("- {} ({}): {p}", a.name, a.mime)))
                .collect();
            if !listed.is_empty() {
                words.push_str(&format!("\n\nAttached files:\n{}", listed.join("\n")));
            }
            if !words.trim().is_empty() {
                blocks.push(json!({"type": "text", "text": words}));
            }
            json!(blocks)
        };
        let mut msg = json!({
            "type": "user",
            "message": {"role": "user", "content": content},
            "parent_tool_use_id": null,
            "session_id": core.state.harness_session.clone().unwrap_or_default(),
            "uuid": id,
        });
        if busy && mode == SendMode::Now
            && let Some(o) = msg.as_object_mut()
        {
            o.insert("priority".into(), json!("now"));
        }
        core.write(&msg);
        if core.state.goal.is_empty() {
            core.state.goal = clip_middle(&text, 600).0;
        }
        self.inflight.insert(id.clone());
        core.state.delivery.insert(id.clone(), Delivery::Sent);
        prune_delivery(core);
        let (images, names) = super::model::attachment_summary(&attachments);
        core.upsert(&id, None, Body::User { text, images, client_id: Some(id.clone()), origin: Some("human".into()), attachments: names });
        core.touch_state();
        self.settle(core);
    }

    /// The turn is over: the next message waiting in kiln's queue goes out.
    fn drain_queue(&mut self, core: &mut Core) {
        if self.busy(core) || core.state.queue.is_empty() || core.state.status == Status::Exited {
            return;
        }
        let item = core.state.queue.remove(0);
        core.touch_state();
        self.send(core, item.id, item.text, SendMode::Queue, item.attachments, item.overrides);
    }

    fn answer(&mut self, core: &mut Core, id: &str, decision: Decision, message: Option<String>, always: Option<usize>, answers: Option<std::collections::BTreeMap<String, String>>) {
        let Some(approval) = core.take_approval(id) else { return };
        let ask = self.asks.remove(id);
        let mut input = ask.as_ref().map(|a| a.input.clone()).unwrap_or_else(|| approval.input.clone());
        let response = match decision {
            Decision::Allow | Decision::Always => {
                if let (Some(answers), Some(o)) = (answers, input.as_object_mut()) {
                    o.insert("answers".into(), json!(answers));
                    if let Some(m) = message.as_ref().filter(|m| !m.trim().is_empty()) {
                        o.insert("response".into(), json!(m));
                    }
                }
                let mut r = json!({"behavior": "allow", "updatedInput": input});
                let permanent = decision == Decision::Always;
                if permanent
                    && let Some(opt) = always.and_then(|i| approval.always.get(i)).or_else(|| approval.always.first())
                    && let Some(o) = r.as_object_mut()
                {
                    o.insert("updatedPermissions".into(), json!([opt.value.clone()]));
                }
                if let Some(o) = r.as_object_mut() {
                    o.insert("decisionClassification".into(), json!(if permanent { "user_permanent" } else { "user_temporary" }));
                }
                if let Some(tid) = approval.tool_id.as_deref() {
                    set_tool_status(core, tid, ToolStatus::Running);
                    if let Some(Body::Tool(t)) = core.get(tid).map(|e| &e.body) {
                        core.state.doing = Some(doing(t));
                    }
                }
                r
            }
            Decision::Deny => {
                let feedback = message.filter(|m| !m.trim().is_empty());
                if let Some(tid) = approval.tool_id.clone() {
                    self.denied.insert(tid.clone());
                    set_tool_status(core, &tid, ToolStatus::Denied);
                }
                json!({
                    "behavior": "deny",
                    "message": feedback.clone().unwrap_or_else(|| "The user declined this action.".to_owned()),
                    // No feedback: stop the turn and wait, as declining does in the terminal.
                    "interrupt": feedback.is_none(),
                    "decisionClassification": "user_reject",
                })
            }
        };
        core.write(&json!({"type": "control_response", "response": {"subtype": "success", "request_id": id, "response": response}}));
        self.settle(core);
    }

    // ------------------------------------------------------------------ stdout

    pub fn line(&mut self, core: &mut Core, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            self.notice(core, Level::Warn, clip_middle(line, 2000).0, None);
            return;
        };
        let parent = str_of(&v, "parent_tool_use_id").map(str::to_owned);
        match str_of(&v, "type").unwrap_or("") {
            "control_response" => self.control_response(core, &v),
            "control_request" => self.control_request(core, &v),
            "control_cancel_request" => {
                if let Some(id) = str_of(&v, "request_id") {
                    self.asks.remove(id);
                    if let Some(a) = core.take_approval(id)
                        && let Some(tid) = a.tool_id
                    {
                        set_tool_status(core, &tid, ToolStatus::Running);
                    }
                    self.settle(core);
                }
            }
            "system" => self.system(core, &v, parent),
            "stream_event" => self.stream_event(core, &v, parent),
            "assistant" => self.assistant(core, &v, parent),
            "user" => self.user(core, &v, parent),
            "result" => self.result(core, &v),
            "command_lifecycle" => self.lifecycle(core, &v),
            _ => {}
        }
    }

    fn control_response(&mut self, core: &mut Core, v: &Value) {
        let Some(resp) = v.get("response") else { return };
        let Some(id) = str_of(resp, "request_id") else { return };
        let Some(kind) = self.pending.remove(id) else { return };
        let ok = str_of(resp, "subtype") == Some("success");
        let body = resp.get("response").cloned().unwrap_or(Value::Null);
        if !ok {
            let err = str_of(resp, "error").unwrap_or("request failed").to_owned();
            match kind {
                Pending::Suggest(client) | Pending::Reply(client) => core.reply(client, json!({"error": err})),
                Pending::Initialize | Pending::ContextUsage | Pending::Settings | Pending::McpStatus | Pending::Hooks => {}
                Pending::Plain(what) => self.notice(core, Level::Warn, format!("{what}: {err}"), None),
                _ => self.notice(core, Level::Warn, err, None),
            }
            return;
        }
        match kind {
            Pending::Initialize => {
                if let Some(cmds) = body.get("commands").and_then(Value::as_array) {
                    core.meta.commands = cmds
                        .iter()
                        .filter_map(|c| {
                            Some(SlashCommand {
                                name: str_of(c, "name")?.to_owned(),
                                description: str_of(c, "description").unwrap_or("").chars().take(300).collect(),
                                hint: str_of(c, "argumentHint").unwrap_or("").to_owned(),
                            })
                        })
                        .collect();
                }
                if let Some(models) = body.get("models").and_then(Value::as_array) {
                    for m in models {
                        let levels: Vec<String> = m.get("supportedEffortLevels").and_then(Value::as_array).map(|l| l.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default();
                        for key in [str_of(m, "value"), str_of(m, "resolvedModel")].into_iter().flatten() {
                            self.efforts.insert(key.to_owned(), levels.clone());
                        }
                    }
                    core.meta.models = models
                        .iter()
                        .filter_map(|m| {
                            Some(ModelOption {
                                id: str_of(m, "value")?.to_owned(),
                                label: str_of(m, "displayName").unwrap_or("").to_owned(),
                                description: str_of(m, "description").unwrap_or("").to_owned(),
                            })
                        })
                        .collect();
                }
                if let Some(agents) = body.get("agents").and_then(Value::as_array) {
                    core.meta.agents = agents.iter().filter_map(|a| str_of(a, "name").map(str::to_owned)).collect();
                }
                if let Some(m) = str_of(&body, "current_permission_mode") {
                    core.state.mode = m.to_owned();
                    core.touch_state();
                }
                if let Some(ver) = str_of(&body, "claude_code_version") {
                    core.meta.version = ver.to_owned();
                }
                self.output_styles = body.get("available_output_styles").and_then(Value::as_array).map(|s| s.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default();
                if let Some(s) = str_of(&body, "output_style") {
                    core.set_control("output_style", json!(s));
                }
                core.touch_meta();
                self.refresh_controls(core);
                if core.state.status == Status::Starting {
                    core.set_status(Status::Idle);
                }
                self.request(core, Pending::Settings, json!({"subtype": "get_settings"}));
                self.request(core, Pending::McpStatus, json!({"subtype": "mcp_status"}));
                self.request(core, Pending::Hooks, json!({"subtype": "get_hooks_listing"}));
                self.drain_queue(core);
            }
            Pending::SetMode(mode) => {
                core.state.mode = str_of(&body, "mode").unwrap_or(&mode).to_owned();
                core.touch_state();
            }
            Pending::SetModel(model) => {
                core.state.model = model;
                core.touch_state();
                // Effort levels follow the model.
                self.refresh_controls(core);
                self.request(core, Pending::Settings, json!({"subtype": "get_settings"}));
            }
            Pending::ApplySettings => self.request(core, Pending::Settings, json!({"subtype": "get_settings"})),
            Pending::Settings => {
                if let Some(e) = body.pointer("/applied/effort").and_then(Value::as_str) {
                    core.state.effort = e.to_owned();
                    core.touch_state();
                }
                if let Some(s) = body.pointer("/effective/outputStyle").and_then(Value::as_str) {
                    core.set_control("output_style", json!(s));
                }
            }
            Pending::McpStatus => {
                self.mcp = body
                    .get("mcpServers")
                    .and_then(Value::as_array)
                    .map(|l| l.iter().filter_map(|s| Some((str_of(s, "name")?.to_owned(), str_of(s, "status").unwrap_or("").to_owned()))).collect())
                    .unwrap_or_default();
                for (name, status) in self.mcp.clone() {
                    core.set_control(&format!("mcp:{name}"), json!(status != "disabled"));
                }
                self.refresh_controls(core);
            }
            Pending::Hooks => {
                core.meta.hooks = body
                    .get("hooks")
                    .and_then(Value::as_array)
                    .map(|l| {
                        l.iter()
                            .filter_map(|h| {
                                Some(HookInfo {
                                    event: str_of(h, "event")?.to_owned(),
                                    matcher: str_of(h, "matcher").unwrap_or("").to_owned(),
                                    command: str_of(h, "commandText").or_else(|| str_of(h, "displayText")).unwrap_or("").to_owned(),
                                    source: str_of(h, "sourceLabel").or_else(|| str_of(h, "source")).unwrap_or("").to_owned(),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                core.touch_meta();
            }
            Pending::Reply(client) => core.reply(client, body),
            Pending::Plain(_) => {}
            Pending::Suggest(client) => core.reply(client, body),
            Pending::ContextUsage => {
                let total = body.get("totalTokens").and_then(Value::as_u64);
                let max = body.get("maxTokens").and_then(Value::as_u64);
                if total.is_some() || max.is_some() {
                    core.state.usage.context_tokens = total;
                    core.state.usage.context_window = max;
                    core.touch_state();
                }
            }
            Pending::Interrupt | Pending::StopTask => {}
        }
    }

    fn control_request(&mut self, core: &mut Core, v: &Value) {
        let Some(id) = str_of(v, "request_id").map(str::to_owned) else { return };
        let req = v.get("request").cloned().unwrap_or(Value::Null);
        match str_of(&req, "subtype").unwrap_or("") {
            "can_use_tool" => self.can_use_tool(core, id, &req),
            // kiln registers no hooks or in-process MCP servers; answer so nothing hangs.
            "hook_callback" => core.write(&json!({"type": "control_response", "response": {"subtype": "success", "request_id": id, "response": {}}})),
            "elicitation" => core.write(&json!({"type": "control_response", "response": {"subtype": "success", "request_id": id, "response": {"action": "decline"}}})),
            other => core.write(&json!({"type": "control_response", "response": {"subtype": "error", "request_id": id, "error": format!("kiln does not handle {other}")}})),
        }
    }

    fn can_use_tool(&mut self, core: &mut Core, id: String, req: &Value) {
        let tool_name = str_of(req, "tool_name").unwrap_or("tool").to_owned();
        let input = req.get("input").cloned().unwrap_or(Value::Null);
        let tool_id = str_of(req, "tool_use_id").map(str::to_owned);
        let cwd = core.state.cwd.clone();
        let (tool, title, detail) = describe(&tool_name, &input, &cwd);
        let always = if req.get("suppress_always_allow_rule").and_then(Value::as_bool) == Some(true) {
            Vec::new()
        } else {
            req.get("permission_suggestions").and_then(Value::as_array).map(|s| s.iter().filter_map(always_option).collect()).unwrap_or_default()
        };
        let questions = if tool == ToolKind::Question { questions(&input) } else { Vec::new() };
        let plan = if tool == ToolKind::Plan { str_of(&input, "plan").map(str::to_owned) } else { None };
        let reason = str_of(req, "decision_reason").map(strip_ansi).filter(|r| !r.trim().is_empty());
        let approval = Approval {
            id: id.clone(),
            tool_id: tool_id.clone(),
            tool_name: tool_name.clone(),
            tool,
            title,
            detail: str_of(req, "description").map(str::to_owned).or(detail),
            input: clip_json(&input, 64 * 1024),
            command: if tool == ToolKind::Shell { str_of(&input, "command").map(str::to_owned) } else { None },
            cwd: cwd.clone(),
            diffs: Vec::new(),
            reason,
            always,
            default_to_no: req.get("default_to_no").and_then(Value::as_bool).unwrap_or(false),
            questions,
            plan,
            agent: str_of(req, "agent_id").map(str::to_owned),
            at: Core::now(),
        };
        if tool == ToolKind::Edit {
            core.previews.push(PreviewJob { approval: id.clone(), cwd, tool: tool_name.clone(), input: input.clone() });
        }
        if let Some(tid) = tool_id.as_deref() {
            set_tool_status(core, tid, ToolStatus::Pending);
        }
        core.state.doing = Some(match tool {
            ToolKind::Question => "Asking you a question".to_owned(),
            ToolKind::Plan => "Waiting for you to review a plan".to_owned(),
            _ => "Waiting for your approval".to_owned(),
        });
        self.asks.insert(id, Ask { input });
        core.add_approval(approval);
        self.settle(core);
    }

    fn system(&mut self, core: &mut Core, v: &Value, parent: Option<String>) {
        match str_of(v, "subtype").unwrap_or("") {
            "init" => {
                if let Some(s) = str_of(v, "session_id") {
                    core.state.harness_session = Some(s.to_owned());
                }
                if let Some(c) = str_of(v, "cwd") {
                    core.state.cwd = c.to_owned();
                }
                if let Some(m) = str_of(v, "model") {
                    core.state.model = m.to_owned();
                }
                if let Some(m) = str_of(v, "permissionMode") {
                    core.state.mode = m.to_owned();
                }
                if let Some(ver) = str_of(v, "claude_code_version") {
                    core.meta.version = ver.to_owned();
                    core.touch_meta();
                }
                if core.meta.agents.is_empty()
                    && let Some(a) = v.get("agents").and_then(Value::as_array)
                {
                    core.meta.agents = a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect();
                    core.touch_meta();
                }
                core.touch_state();
            }
            "status" => {
                if let Some(m) = str_of(v, "permissionMode") {
                    core.state.mode = m.to_owned();
                }
                let activity = str_of(v, "status").map(str::to_owned);
                if activity.as_deref() == Some("requesting") {
                    self.begin(core);
                }
                core.state.activity = activity;
                core.touch_state();
                self.settle(core);
            }
            "session_state_changed" => match str_of(v, "state") {
                Some("running") => {
                    self.begin(core);
                    self.settle(core);
                }
                Some("idle") => {
                    self.turn = false;
                    core.state.turn_started = None;
                    core.touch_state();
                    self.settle(core);
                    self.drain_queue(core);
                }
                _ => {}
            },
            "session_title_changed" => {
                if let Some(t) = str_of(v, "title") {
                    core.state.title = t.to_owned();
                    core.touch_state();
                }
            }
            "compact_boundary" => {
                let meta = v.get("compact_metadata").or_else(|| v.get("compactMetadata"));
                let pre = meta.and_then(|m| m.get("pre_tokens").or_else(|| m.get("preTokens"))).and_then(Value::as_u64);
                let text = match pre {
                    Some(p) => format!("Conversation compacted (was {} tokens)", fmt_tokens(p)),
                    None => "Conversation compacted".to_owned(),
                };
                self.notice(core, Level::Info, text, parent);
            }
            "api_retry" => {
                let attempt = v.get("attempt").and_then(Value::as_u64).unwrap_or(0);
                let err = str_of(v, "error").or_else(|| str_of(v, "error_status")).unwrap_or("API error");
                self.notice(core, Level::Warn, format!("Retrying the API (attempt {attempt}): {err}"), parent);
            }
            "api_error" | "model_refusal_no_fallback" => {
                let text = str_of(v, "message").or_else(|| str_of(v, "error")).unwrap_or("The API returned an error.");
                self.notice(core, Level::Error, strip_ansi(text), parent);
            }
            "model_fallback" | "model_refusal_fallback" | "model_consent_fallback" | "instruction_size_warning" => {
                let text = str_of(v, "message").or_else(|| str_of(v, "content")).unwrap_or("The model changed.");
                self.notice(core, Level::Warn, strip_ansi(text), parent);
            }
            "informational" | "notification" | "local_command_output" => {
                let text = str_of(v, "content").or_else(|| str_of(v, "text")).or_else(|| str_of(v, "output")).unwrap_or("");
                if !text.trim().is_empty() {
                    self.notice(core, Level::Info, strip_ansi(text), parent);
                }
            }
            "permission_denied" => {
                if let Some(tid) = str_of(v, "tool_use_id") {
                    self.denied.insert(tid.to_owned());
                    set_tool_status(core, tid, ToolStatus::Denied);
                }
                let text = str_of(v, "message").or_else(|| str_of(v, "decision_reason")).unwrap_or("Permission denied.");
                self.notice(core, Level::Warn, strip_ansi(text), parent);
            }
            "task_started" => {
                let Some(id) = str_of(v, "task_id") else { return };
                let task = Task {
                    id: id.to_owned(),
                    tool_id: str_of(v, "tool_use_id").map(str::to_owned),
                    description: str_of(v, "description").unwrap_or("").to_owned(),
                    kind: str_of(v, "subagent_type").or_else(|| str_of(v, "task_type")).unwrap_or("task").to_owned(),
                    status: "running".into(),
                    background: v.get("is_backgrounded").and_then(Value::as_bool).unwrap_or(false),
                    depth: v.get("spawn_depth").and_then(Value::as_u64).and_then(|d| u32::try_from(d).ok()).unwrap_or(1),
                    parent_task: str_of(v, "parent_task_id").map(str::to_owned),
                    prompt: str_of(v, "prompt").map(|p| clip_middle(p, 4000).0),
                    started: Core::now(),
                    ..Task::default()
                };
                if let Some(tid) = task.tool_id.clone()
                    && let Some(ev) = core.get_mut(&tid)
                    && let Body::Tool(t) = &mut ev.body
                {
                    t.task = Some(task.id.clone());
                }
                core.state.tasks.retain(|t| t.id != task.id);
                core.state.tasks.push(task);
                prune_tasks(core);
                core.touch_state();
            }
            "task_progress" => {
                let Some(id) = str_of(v, "task_id") else { return };
                if let Some(t) = core.state.tasks.iter_mut().find(|t| t.id == id) {
                    if let Some(u) = v.get("usage") {
                        t.tokens = u.get("total_tokens").and_then(Value::as_u64).unwrap_or(t.tokens);
                        t.tool_uses = u.get("tool_uses").and_then(Value::as_u64).unwrap_or(t.tool_uses);
                        t.duration_ms = u.get("duration_ms").and_then(Value::as_u64).unwrap_or(t.duration_ms);
                    }
                    if let Some(s) = str_of(v, "summary") {
                        t.summary = Some(s.to_owned());
                    }
                    if let Some(s) = str_of(v, "last_tool_name") {
                        t.last_tool = Some(s.to_owned());
                    }
                    core.touch_state();
                }
            }
            "task_updated" => {
                let Some(id) = str_of(v, "task_id") else { return };
                let patch = v.get("patch").cloned().unwrap_or(Value::Null);
                if let Some(t) = core.state.tasks.iter_mut().find(|t| t.id == id) {
                    if let Some(s) = str_of(&patch, "status") {
                        t.status = s.to_owned();
                        if s != "running" && s != "pending" && s != "paused" {
                            t.ended = Some(patch.get("end_time").and_then(Value::as_i64).unwrap_or_else(Core::now));
                        }
                    }
                    if let Some(b) = patch.get("is_backgrounded").and_then(Value::as_bool) {
                        t.background = b;
                    }
                    if let Some(d) = str_of(&patch, "description") {
                        t.description = d.to_owned();
                    }
                    if let Some(e) = str_of(&patch, "error") {
                        t.summary = Some(e.to_owned());
                    }
                    core.touch_state();
                }
            }
            "task_notification" => {
                let Some(id) = str_of(v, "task_id") else { return };
                if let Some(t) = core.state.tasks.iter_mut().find(|t| t.id == id) {
                    t.status = str_of(v, "status").unwrap_or("completed").to_owned();
                    t.ended.get_or_insert_with(Core::now);
                    if let Some(s) = str_of(v, "summary") {
                        t.summary = Some(clip_middle(s, 8000).0);
                    }
                    if let Some(u) = v.get("usage") {
                        t.tokens = u.get("total_tokens").and_then(Value::as_u64).unwrap_or(t.tokens);
                        t.tool_uses = u.get("tool_uses").and_then(Value::as_u64).unwrap_or(t.tool_uses);
                        t.duration_ms = u.get("duration_ms").and_then(Value::as_u64).unwrap_or(t.duration_ms);
                    }
                    core.touch_state();
                }
            }
            _ => {}
        }
    }

    fn stream_event(&mut self, core: &mut Core, v: &Value, parent: Option<String>) {
        let Some(ev) = v.get("event") else { return };
        let msg_id = str_of(v, "api_message_id")
            .or_else(|| ev.get("message").and_then(|m| str_of(m, "id")))
            .unwrap_or("")
            .to_owned();
        self.begin(core);
        match str_of(ev, "type").unwrap_or("") {
            "message_start" => {
                self.streams.entry(msg_id).or_default();
                self.settle(core);
            }
            "content_block_start" => {
                let index = ev.get("index").and_then(Value::as_u64).unwrap_or(0);
                let block = ev.get("content_block").cloned().unwrap_or(Value::Null);
                let id = format!("{msg_id}:{index}");
                if parent.is_none() {
                    let now = match str_of(&block, "type") {
                        Some("text") => Some("Writing"),
                        Some("thinking") => Some("Thinking"),
                        _ => None,
                    };
                    if let Some(d) = now
                        && self.tools.is_empty()
                    {
                        core.state.doing = Some(d.to_owned());
                        core.touch_state();
                    }
                }
                match str_of(&block, "type") {
                    Some("text") => {
                        core.upsert(&id, parent, Body::Assistant { text: str_of(&block, "text").unwrap_or("").to_owned(), streaming: true });
                        self.streams.entry(msg_id).or_default().blocks.insert(index, id);
                    }
                    Some("thinking") => {
                        core.upsert(&id, parent, Body::Thinking { text: str_of(&block, "thinking").unwrap_or("").to_owned(), streaming: true, redacted: false });
                        self.streams.entry(msg_id).or_default().blocks.insert(index, id);
                    }
                    Some("tool_use") => {
                        // Shown when the complete call arrives (its input streams as partial JSON).
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let index = ev.get("index").and_then(Value::as_u64).unwrap_or(0);
                let Some(id) = self.streams.get(&msg_id).and_then(|s| s.blocks.get(&index)).cloned() else { return };
                let delta = ev.get("delta").cloned().unwrap_or(Value::Null);
                let text = str_of(&delta, "text").or_else(|| str_of(&delta, "thinking")).unwrap_or("");
                core.append(&id, text);
            }
            _ => {}
        }
    }

    fn assistant(&mut self, core: &mut Core, v: &Value, parent: Option<String>) {
        self.begin(core);
        let Some(msg) = v.get("message") else { return };
        if parent.is_none() {
            self.entry(str_of(v, "uuid").unwrap_or(""), None);
        }
        let msg_id = str_of(msg, "id").unwrap_or("").to_owned();
        if let Some(m) = str_of(msg, "model").filter(|_| parent.is_none())
            && core.state.model != m
            && !m.starts_with("<synthetic")
        {
            core.state.model = m.to_owned();
            core.touch_state();
        }
        let error = v.get("error").is_some() || v.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true);
        for block in msg.get("content").and_then(Value::as_array).cloned().unwrap_or_default() {
            let index = {
                let s = self.streams.entry(msg_id.clone()).or_default();
                let i = s.finals;
                s.finals += 1;
                i
            };
            let id = if msg_id.is_empty() { str_of(v, "uuid").unwrap_or("").to_owned() } else { format!("{msg_id}:{index}") };
            match str_of(&block, "type").unwrap_or("") {
                "text" => {
                    let text = str_of(&block, "text").unwrap_or("").to_owned();
                    if error {
                        self.notice(core, Level::Error, text, parent.clone());
                    } else {
                        if parent.is_none() && !text.trim().is_empty() {
                            core.state.summary = clip_middle(text.trim(), 600).0;
                            core.touch_state();
                        }
                        core.upsert(&id, parent.clone(), Body::Assistant { text: clip_middle(&text, super::model::TEXT_MAX).0, streaming: false });
                    }
                }
                "thinking" | "redacted_thinking" => {
                    let text = str_of(&block, "thinking").unwrap_or("").to_owned();
                    let redacted = text.is_empty();
                    core.upsert(&id, parent.clone(), Body::Thinking { text, streaming: false, redacted });
                }
                "tool_use" | "server_tool_use" => {
                    let tid = str_of(&block, "id").unwrap_or(&id).to_owned();
                    let name = str_of(&block, "name").unwrap_or("tool").to_owned();
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    let (tool, title, detail) = describe(&name, &input, &core.state.cwd);
                    let todos = if tool == ToolKind::Todo { todos(&input) } else { Vec::new() };
                    if !todos.is_empty() && parent.is_none() {
                        core.state.todos = todos.clone();
                        core.touch_state();
                    }
                    let status = if core.state.approvals.iter().any(|a| a.tool_id.as_deref() == Some(tid.as_str())) { ToolStatus::Pending } else { ToolStatus::Running };
                    let call = ToolCall {
                        tool_id: tid.clone(),
                        name: name.clone(),
                        tool,
                        title,
                        detail,
                        input: clip_json(&input, 16 * 1024),
                        status,
                        output: None,
                        truncated: false,
                        diffs: Vec::new(),
                        todos,
                        task: core.state.tasks.iter().find(|t| t.tool_id.as_deref() == Some(tid.as_str())).map(|t| t.id.clone()),
                        exit_code: None,
                        started: Core::now(),
                        ended: None,
                    };
                    if parent.is_none() {
                        core.state.doing = Some(doing(&call));
                        core.touch_state();
                    }
                    self.tools.insert(tid.clone(), (name, input));
                    core.upsert(&tid, parent.clone(), Body::Tool(Box::new(call)));
                }
                _ => {}
            }
        }
        self.settle(core);
    }

    fn user(&mut self, core: &mut Core, v: &Value, parent: Option<String>) {
        let Some(msg) = v.get("message") else { return };
        let uuid = str_of(v, "uuid").unwrap_or("").to_owned();
        if parent.is_none() {
            // A person's prompt (our own, replayed, or one from the transcript) names its turn.
            let human = v.get("isReplay").and_then(Value::as_bool) == Some(true);
            self.entry(&uuid, human.then_some(uuid.as_str()));
        }
        let content = msg.get("content").cloned().unwrap_or(Value::Null);
        let structured = v.get("tool_use_result").cloned().unwrap_or(Value::Null);
        match &content {
            Value::String(text) => self.user_text(core, &uuid, text, v, parent),
            Value::Array(blocks) => {
                let mut texts = Vec::new();
                let mut images = 0u32;
                for b in blocks {
                    match str_of(b, "type").unwrap_or("") {
                        "tool_result" => {
                            let Some(tid) = str_of(b, "tool_use_id") else { continue };
                            self.tool_result(core, tid, b, &structured);
                        }
                        "text" => texts.push(str_of(b, "text").unwrap_or("").to_owned()),
                        "image" => images += 1,
                        _ => {}
                    }
                }
                let text = texts.join("\n");
                if !text.trim().is_empty() || images > 0 {
                    self.user_text(core, &uuid, &text, v, parent);
                    if images > 0
                        && let Some(e) = core.get_mut(&uuid)
                        && let Body::User { images: n, .. } = &mut e.body
                    {
                        *n = (*n).max(images);
                    }
                }
            }
            _ => {}
        }
    }

    fn user_text(&mut self, core: &mut Core, uuid: &str, text: &str, v: &Value, parent: Option<String>) {
        let t = text.trim();
        if t.starts_with("[Request interrupted by user") {
            self.notice(core, Level::Info, "Interrupted", parent);
            return;
        }
        // Context the CLI injects (reminders, command wrappers) is not something anyone said.
        if v.get("isSynthetic").and_then(Value::as_bool) == Some(true) || (t.starts_with('<') && !t.starts_with("<task-notification")) {
            return;
        }
        if t.starts_with("<task-notification") {
            return; // shown through the task's own state
        }
        let id = if uuid.is_empty() { format!("user-{}", Core::now()) } else { uuid.to_owned() };
        let ours = core.state.delivery.contains_key(&id);
        let human = ours || v.get("isReplay").and_then(Value::as_bool) == Some(true);
        if human && core.state.goal.is_empty() {
            core.state.goal = clip_middle(t, 600).0;
            core.touch_state();
        }
        let (keep_images, keep_names) = match core.get(&id).map(|e| &e.body) {
            Some(Body::User { images, attachments, .. }) => (*images, attachments.clone()),
            _ => (0, Vec::new()),
        };
        // The replay of our own message: its words without the attached-files list we added.
        let t = t.split("\n\nAttached files:\n").next().unwrap_or(t);
        core.upsert(
            &id,
            parent,
            Body::User {
                text: clip_middle(t, super::model::TEXT_MAX).0,
                images: keep_images,
                attachments: keep_names,
                client_id: ours.then(|| id.clone()),
                origin: Some(if human { "human".into() } else { "system".into() }),
            },
        );
    }

    fn tool_result(&mut self, core: &mut Core, tid: &str, block: &Value, structured: &Value) {
        let is_error = block.get("is_error").and_then(Value::as_bool).unwrap_or(false);
        let mut text = content_text(block.get("content"));
        let (name, input) = self.tools.get(tid).cloned().unwrap_or_default();
        let mut diffs = Vec::new();
        let mut exit_code = None;
        if let Some(o) = structured.as_object() {
            // Shell commands: stdout and stderr as the command printed them.
            if let Some(out) = o.get("stdout").and_then(Value::as_str) {
                let err = o.get("stderr").and_then(Value::as_str).unwrap_or("");
                text = if err.is_empty() { out.to_owned() } else if out.is_empty() { err.to_owned() } else { format!("{out}\n{err}") };
                if o.get("interrupted").and_then(Value::as_bool) == Some(true) {
                    text.push_str("\n[interrupted]");
                }
            }
            if let Some(code) = o.get("exitCode").or_else(|| o.get("exit_code")).and_then(Value::as_i64) {
                exit_code = i32::try_from(code).ok();
            }
            diffs = patch_diffs(o, &core.state.cwd);
        }
        let interrupted = text.contains("[Request interrupted by user");
        let status = if self.denied.remove(tid) {
            ToolStatus::Denied
        } else if interrupted {
            ToolStatus::Cancelled
        } else if is_error {
            ToolStatus::Error
        } else {
            ToolStatus::Done
        };
        if status == ToolStatus::Done {
            track_changes(core, structured);
        }
        let (output, truncated) = clip_middle(&text, OUTPUT_MAX);
        if let Some(ev) = core.get_mut(tid)
            && let Body::Tool(t) = &mut ev.body
        {
            t.status = status;
            t.output = (!output.is_empty()).then_some(output);
            t.truncated = truncated;
            if !diffs.is_empty() {
                t.diffs = diffs;
            }
            t.exit_code = exit_code.or(t.exit_code);
            t.ended = Some(Core::now());
        } else if !name.is_empty() {
            // A result whose call we never saw (it started before we attached).
            let (tool, title, detail) = describe(&name, &input, &core.state.cwd);
            core.upsert(
                tid,
                None,
                Body::Tool(Box::new(ToolCall {
                    tool_id: tid.to_owned(),
                    name,
                    tool,
                    title,
                    detail,
                    input: clip_json(&input, 16 * 1024),
                    status,
                    output: (!output.is_empty()).then_some(output),
                    truncated,
                    diffs,
                    todos: Vec::new(),
                    task: None,
                    exit_code,
                    started: Core::now(),
                    ended: Some(Core::now()),
                })),
            );
        }
        self.tools.remove(tid);
        if self.tools.is_empty() && self.turn && core.state.approvals.is_empty() {
            core.state.doing = Some("Thinking".to_owned());
            core.touch_state();
        }
    }

    fn result(&mut self, core: &mut Core, v: &Value) {
        self.turn = false;
        core.state.turn_started = None;
        core.state.doing = None;
        let u = &mut core.state.usage;
        if let Some(c) = v.get("total_cost_usd").and_then(Value::as_f64) {
            u.cost_usd = u.cost_usd.max(c);
        }
        if let Some(models) = v.get("modelUsage").and_then(Value::as_object) {
            let (mut i, mut o, mut cr) = (0u64, 0u64, 0u64);
            for m in models.values() {
                i += m.get("inputTokens").and_then(Value::as_u64).unwrap_or(0);
                o += m.get("outputTokens").and_then(Value::as_u64).unwrap_or(0);
                cr += m.get("cacheReadInputTokens").and_then(Value::as_u64).unwrap_or(0);
                if let Some(w) = m.get("contextWindow").and_then(Value::as_u64) {
                    u.context_window = Some(w);
                }
            }
            u.input_tokens = u.input_tokens.max(i);
            u.output_tokens = u.output_tokens.max(o);
            u.cache_read_tokens = u.cache_read_tokens.max(cr);
        }
        u.turns += 1;
        core.state.activity = None;
        // Calls left without a result were cut off with the turn (a background task's call
        // returns at once, so it is not among them).
        let open: Vec<String> = self.tools.keys().cloned().collect();
        for id in open {
            if core.state.approvals.iter().all(|a| a.tool_id.as_deref() != Some(id.as_str())) {
                finish_tool(core, &id, ToolStatus::Cancelled, None);
                self.tools.remove(&id);
            }
        }
        let sub = str_of(v, "subtype").unwrap_or("success");
        if sub != "success" && str_of(v, "terminal_reason") != Some("aborted_streaming") && str_of(v, "terminal_reason") != Some("aborted_tools") {
            let msg = match sub {
                "error_max_turns" => "Stopped: the turn limit was reached.".to_owned(),
                "error_max_budget_usd" => "Stopped: the budget was reached.".to_owned(),
                "error_during_execution" => str_of(v, "result").map(str::to_owned).unwrap_or_else(|| "The turn ended with an error.".to_owned()),
                other => format!("The turn ended: {other}"),
            };
            if str_of(v, "stop_reason").is_some() || sub != "error_during_execution" {
                self.notice(core, Level::Error, msg, None);
            }
        }
        core.touch_state();
        self.settle(core);
        self.request(core, Pending::ContextUsage, json!({"subtype": "get_context_usage"}));
        self.drain_queue(core);
    }

    fn lifecycle(&mut self, core: &mut Core, v: &Value) {
        let Some(id) = str_of(v, "command_uuid") else { return };
        let state = match str_of(v, "state").unwrap_or("") {
            "queued" => Delivery::Queued,
            "started" => Delivery::Started,
            "completed" => Delivery::Completed,
            "cancelled" | "discarded" | "refused" => Delivery::Cancelled,
            _ => return,
        };
        if matches!(state, Delivery::Completed | Delivery::Cancelled) {
            self.inflight.remove(id);
        }
        if state == Delivery::Started {
            self.begin(core);
        }
        if core.state.delivery.contains_key(id) {
            core.state.delivery.insert(id.to_owned(), state);
            core.touch_state();
        }
        self.settle(core);
        if matches!(state, Delivery::Completed | Delivery::Cancelled) {
            self.drain_queue(core);
        }
    }
}

// ---------------------------------------------------------------------- helpers

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn set_tool_status(core: &mut Core, id: &str, status: ToolStatus) {
    if let Some(ev) = core.get_mut(id)
        && let Body::Tool(t) = &mut ev.body
        && !matches!(t.status, ToolStatus::Done | ToolStatus::Error)
    {
        t.status = status;
    }
}

fn finish_tool(core: &mut Core, id: &str, status: ToolStatus, output: Option<String>) {
    if let Some(ev) = core.get_mut(id)
        && let Body::Tool(t) = &mut ev.body
        && matches!(t.status, ToolStatus::Running | ToolStatus::Pending)
    {
        t.status = status;
        if output.is_some() {
            t.output = output;
        }
        t.ended = Some(Core::now());
    }
}

/// What a running tool call is doing, in plain words.
pub fn doing(t: &ToolCall) -> String {
    let short = |s: &str, n: usize| -> String {
        let one = s.lines().next().unwrap_or("").trim();
        if one.chars().count() > n { format!("{}…", one.chars().take(n).collect::<String>()) } else { one.to_owned() }
    };
    match t.tool {
        ToolKind::Shell => format!("Running {}", short(&t.title, 60)),
        ToolKind::Edit => format!("Editing {}", short(&t.title, 60)),
        ToolKind::Read => format!("Reading {}", short(&t.title, 60)),
        ToolKind::Search => format!("Searching for {}", short(&t.title, 50)),
        ToolKind::Web if t.name == "WebSearch" => format!("Searching the web for {}", short(&t.title, 50)),
        ToolKind::Web => format!("Fetching {}", short(&t.title, 60)),
        ToolKind::Agent => format!("Delegating: {}", short(&t.title, 60)),
        ToolKind::Skill => format!("Using the {} skill", short(&t.title, 40)),
        ToolKind::Todo => "Updating the plan".to_owned(),
        ToolKind::Question => "Asking you a question".to_owned(),
        ToolKind::Plan => "Presenting a plan".to_owned(),
        ToolKind::Mcp => format!("Calling {}", short(&t.title, 60)),
        ToolKind::Other => format!("Using {}", t.name),
    }
}

/// Note the files a finished edit changed, for the session's changeset.
fn track_changes(core: &mut Core, structured: &Value) {
    use super::core::{BASELINE_MAX, Baseline};
    let Some(o) = structured.as_object() else { return };
    let cwd = core.state.cwd.clone();
    let abs = |p: &str| -> String {
        if std::path::Path::new(p).is_absolute() || cwd.is_empty() { p.to_owned() } else { format!("{}/{p}", cwd.trim_end_matches('/')) }
    };
    if let Some(fp) = o.get("filePath").and_then(Value::as_str) {
        let a = abs(fp);
        if o.contains_key("originalFile") {
            let before = match o.get("originalFile") {
                Some(Value::String(t)) if t.len() > BASELINE_MAX => Baseline::TooLarge,
                Some(Value::String(t)) => Baseline::Text(t.clone()),
                _ => Baseline::Absent,
            };
            core.note_baseline(&a, before);
        }
        core.want_change(&a, &rel(fp, &cwd));
    }
    // Files a shell command changed that the agent had read or edited before.
    if let Some(files) = o.get("bashEditDiff").and_then(|b| b.get("files")).and_then(Value::as_array) {
        for f in files {
            if let Some(fp) = f.get("filePath").and_then(Value::as_str) {
                core.want_change(&abs(fp), &rel(fp, &cwd));
            }
        }
    }
}

fn prune_delivery(core: &mut Core) {
    const KEEP: usize = 300;
    if core.state.delivery.len() <= KEEP {
        return;
    }
    let done: Vec<String> = core
        .state
        .delivery
        .iter()
        .filter(|(_, d)| matches!(d, Delivery::Completed | Delivery::Cancelled))
        .map(|(k, _)| k.clone())
        .take(core.state.delivery.len() - KEEP)
        .collect();
    for k in done {
        core.state.delivery.remove(&k);
    }
}

fn prune_tasks(core: &mut Core) {
    const KEEP: usize = 200;
    while core.state.tasks.len() > KEEP {
        match core.state.tasks.iter().position(|t| t.status != "running" && t.status != "pending") {
            Some(i) => {
                core.state.tasks.remove(i);
            }
            None => break,
        }
    }
}

fn content_text(c: Option<&Value>) -> String {
    match c {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| match str_of(p, "type") {
                Some("text") => str_of(p, "text").map(str::to_owned),
                Some("image") => Some("[image]".to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Strip ANSI escape sequences (decision reasons may carry them).
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for d in chars.by_ref() {
                    if ('@'..='~').contains(&d) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.0}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// `path` relative to `cwd` when inside it.
pub fn rel(path: &str, cwd: &str) -> String {
    if !cwd.is_empty()
        && let Some(rest) = path.strip_prefix(cwd)
        && let Some(rest) = rest.strip_prefix('/')
    {
        return rest.to_owned();
    }
    path.to_owned()
}

/// The permission modes Claude Code offers.
pub fn modes(allow_bypass: bool) -> Vec<ModeOption> {
    let mut m = vec![
        ModeOption { id: "default".into(), label: "Ask".into(), description: "Ask before edits and commands".into(), dangerous: false },
        ModeOption { id: "acceptEdits".into(), label: "Accept edits".into(), description: "Edit files without asking; ask before commands".into(), dangerous: false },
        ModeOption { id: "plan".into(), label: "Plan".into(), description: "Read and plan only; change nothing".into(), dangerous: false },
        ModeOption { id: "auto".into(), label: "Auto".into(), description: "A classifier approves safe actions; risky ones still ask".into(), dangerous: false },
        ModeOption { id: "dontAsk".into(), label: "Don't ask".into(), description: "Deny anything not already allowed".into(), dangerous: false },
    ];
    if allow_bypass {
        m.push(ModeOption { id: "bypassPermissions".into(), label: "Bypass".into(), description: "Skip every permission check".into(), dangerous: true });
    }
    m
}

/// Kind, one-line title and detail for a tool call.
pub fn describe(name: &str, input: &Value, cwd: &str) -> (ToolKind, String, Option<String>) {
    let s = |k: &str| str_of(input, k).map(str::to_owned);
    let path = || s("file_path").or_else(|| s("notebook_path")).or_else(|| s("path")).map(|p| rel(&p, cwd)).unwrap_or_default();
    match name {
        "Bash" | "PowerShell" | "BashOutput" | "KillShell" | "KillBash" => {
            let title = s("command").or_else(|| s("bash_id").map(|b| format!("output of {b}"))).or_else(|| s("shell_id")).unwrap_or_default();
            (ToolKind::Shell, title, s("description"))
        }
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => {
            let detail = match name {
                "Write" => Some("write file".to_owned()),
                "MultiEdit" => input.get("edits").and_then(Value::as_array).map(|e| format!("{} edits", e.len())),
                "NotebookEdit" => s("edit_mode").map(|m| format!("notebook {m}")),
                _ => (input.get("replace_all").and_then(Value::as_bool) == Some(true)).then(|| "replace all".to_owned()),
            };
            (ToolKind::Edit, path(), detail)
        }
        "Read" => {
            let detail = match (input.get("offset").and_then(Value::as_u64), input.get("limit").and_then(Value::as_u64)) {
                (Some(o), Some(l)) => Some(format!("lines {}–{}", o, o + l)),
                (Some(o), None) => Some(format!("from line {o}")),
                (None, Some(l)) => Some(format!("first {l} lines")),
                _ => None,
            };
            (ToolKind::Read, path(), detail)
        }
        "Grep" => (ToolKind::Search, s("pattern").unwrap_or_default(), s("path").map(|p| rel(&p, cwd)).or_else(|| s("glob"))),
        "Glob" => (ToolKind::Search, s("pattern").unwrap_or_default(), s("path").map(|p| rel(&p, cwd))),
        "LS" => (ToolKind::Search, path(), None),
        "WebFetch" => (ToolKind::Web, s("url").unwrap_or_default(), s("prompt").map(|p| clip_middle(&p, 200).0)),
        "WebSearch" => (ToolKind::Web, s("query").unwrap_or_default(), None),
        "Agent" | "Task" => (ToolKind::Agent, s("description").unwrap_or_else(|| "subagent".into()), s("subagent_type")),
        "Skill" => (ToolKind::Skill, s("skill").or_else(|| s("command")).or_else(|| s("name")).unwrap_or_default(), s("args")),
        "TodoWrite" | "TaskCreate" | "TaskUpdate" => (ToolKind::Todo, "Update the task list".into(), None),
        "AskUserQuestion" => {
            let q = input.get("questions").and_then(Value::as_array).and_then(|q| q.first()).and_then(|q| str_of(q, "question")).unwrap_or("Question").to_owned();
            (ToolKind::Question, q, None)
        }
        "ExitPlanMode" => (ToolKind::Plan, "Plan ready for review".into(), None),
        n if n.starts_with("mcp__") => {
            let mut parts = n.splitn(3, "__").skip(1);
            let server = parts.next().unwrap_or("").to_owned();
            let tool = parts.next().unwrap_or("").to_owned();
            (ToolKind::Mcp, format!("{server} · {tool}"), None)
        }
        _ => {
            let title = ["description", "command", "file_path", "path", "query", "url", "name", "prompt"]
                .iter()
                .find_map(|k| s(k))
                .map(|t| clip_middle(&t, 200).0)
                .unwrap_or_default();
            (ToolKind::Other, title, None)
        }
    }
}

fn todos(input: &Value) -> Vec<Todo> {
    input
        .get("todos")
        .and_then(Value::as_array)
        .map(|t| {
            t.iter()
                .filter_map(|x| {
                    Some(Todo {
                        text: str_of(x, "content").or_else(|| str_of(x, "subject")).or_else(|| str_of(x, "text"))?.to_owned(),
                        status: str_of(x, "status").unwrap_or("pending").to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn questions(input: &Value) -> Vec<Question> {
    input
        .get("questions")
        .and_then(Value::as_array)
        .map(|qs| {
            qs.iter()
                .filter_map(|q| {
                    Some(Question {
                        question: str_of(q, "question")?.to_owned(),
                        header: str_of(q, "header").unwrap_or("").to_owned(),
                        multi: q.get("multiSelect").and_then(Value::as_bool).unwrap_or(false),
                        options: q
                            .get("options")
                            .and_then(Value::as_array)
                            .map(|o| {
                                o.iter()
                                    .filter_map(|x| Some(QuestionOption { label: str_of(x, "label")?.to_owned(), description: str_of(x, "description").unwrap_or("").to_owned() }))
                                    .collect()
                            })
                            .unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A permission suggestion as an "always allow" choice.
fn always_option(s: &Value) -> Option<AlwaysOption> {
    let scope = match str_of(s, "destination").unwrap_or("session") {
        "session" | "cliArg" => "this session",
        "localSettings" => "this project, just you",
        "projectSettings" => "this project, everyone",
        "userSettings" => "all your projects",
        _ => "this session",
    };
    let label = match str_of(s, "type")? {
        "addRules" | "replaceRules" => {
            let behavior = str_of(s, "behavior").unwrap_or("allow");
            if behavior != "allow" {
                return None;
            }
            let rules: Vec<String> = s
                .get("rules")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(|r| {
                    let tool = str_of(r, "toolName")?;
                    Some(match str_of(r, "ruleContent") {
                        Some(c) if !c.is_empty() => format!("{tool}({c})"),
                        _ => tool.to_owned(),
                    })
                })
                .collect();
            format!("Always allow {}", rules.join(", "))
        }
        "setMode" => match str_of(s, "mode")? {
            "acceptEdits" => "Accept all edits".to_owned(),
            "bypassPermissions" => "Bypass all permissions".to_owned(),
            "plan" => "Switch to plan mode".to_owned(),
            other => format!("Switch to {other} mode"),
        },
        "addDirectories" => {
            let dirs: Vec<&str> = s.get("directories").and_then(Value::as_array)?.iter().filter_map(Value::as_str).collect();
            format!("Allow access to {}", dirs.join(", "))
        }
        _ => return None,
    };
    Some(AlwaysOption { label, scope: scope.to_owned(), value: s.clone() })
}

/// Diffs from a structured Edit/Write result (`structuredPatch`, or a created file's content).
fn patch_diffs(o: &serde_json::Map<String, Value>, cwd: &str) -> Vec<FileDiff> {
    let Some(path) = o.get("filePath").and_then(Value::as_str) else { return Vec::new() };
    let path = rel(path, cwd);
    if o.get("type").and_then(Value::as_str) == Some("create")
        && let Some(content) = o.get("content").and_then(Value::as_str)
    {
        return vec![super::diff::created(&path, content)];
    }
    let hunks: Vec<Hunk> = o
        .get("structuredPatch")
        .and_then(Value::as_array)
        .map(|hs| {
            hs.iter()
                .filter_map(|h| {
                    let n = |k: &str| h.get(k).and_then(Value::as_u64).and_then(|x| u32::try_from(x).ok());
                    Some(Hunk {
                        old_start: n("oldStart")?,
                        old_lines: n("oldLines")?,
                        new_start: n("newStart")?,
                        new_lines: n("newLines")?,
                        lines: h.get("lines")?.as_array()?.iter().filter_map(|l| l.as_str().map(str::to_owned)).collect(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    if hunks.is_empty() {
        return Vec::new();
    }
    vec![FileDiff { path, created: false, hunks }]
}

/// What an edit tool would change, against the file as it is now (runs off the event loop).
pub fn preview(tool: &str, input: &Value, cwd: &str) -> Vec<FileDiff> {
    let Some(path) = str_of(input, "file_path").or_else(|| str_of(input, "notebook_path")) else { return Vec::new() };
    let full = if std::path::Path::new(path).is_absolute() { std::path::PathBuf::from(path) } else { std::path::Path::new(cwd).join(path) };
    let current = std::fs::metadata(&full)
        .ok()
        .filter(|m| m.is_file() && m.len() <= super::diff::MAX_FILE)
        .and_then(|_| std::fs::read_to_string(&full).ok());
    let shown = rel(path, cwd);
    let replace = |text: &str, old: &str, new: &str, all: bool| -> Option<String> {
        if old.is_empty() || !text.contains(old) {
            return None;
        }
        Some(if all { text.replace(old, new) } else { text.replacen(old, new, 1) })
    };
    let next = match tool {
        "Write" => {
            let content = str_of(input, "content").unwrap_or("");
            match &current {
                None => return vec![super::diff::created(&shown, content)],
                Some(_) => Some(content.to_owned()),
            }
        }
        "Edit" => current.as_deref().and_then(|c| {
            replace(c, str_of(input, "old_string").unwrap_or(""), str_of(input, "new_string").unwrap_or(""), input.get("replace_all").and_then(Value::as_bool).unwrap_or(false))
        }),
        "MultiEdit" => current.as_deref().and_then(|c| {
            let mut text = c.to_owned();
            for e in input.get("edits").and_then(Value::as_array)? {
                text = replace(&text, str_of(e, "old_string").unwrap_or(""), str_of(e, "new_string").unwrap_or(""), e.get("replace_all").and_then(Value::as_bool).unwrap_or(false))?;
            }
            Some(text)
        }),
        _ => None,
    };
    match (current, next) {
        (Some(old), Some(new)) => {
            let hunks = super::diff::hunks(&old, &new, super::diff::CONTEXT);
            if hunks.is_empty() { Vec::new() } else { vec![FileDiff { path: shown, created: false, hunks }] }
        }
        _ => {
            // The file could not be read or the text is not in it (yet): show the edit itself.
            let old = str_of(input, "old_string").unwrap_or("");
            let new = str_of(input, "new_string").or_else(|| str_of(input, "new_source")).or_else(|| str_of(input, "content")).unwrap_or("");
            let hunks = super::diff::hunks(old, new, super::diff::CONTEXT);
            if hunks.is_empty() { Vec::new() } else { vec![FileDiff { path: shown, created: false, hunks }] }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::core::Update;

    /// Lines Claude Code 2.1.296 printed in a session against a scripted API (paths shortened).
    const BASH_SESSION: &[&str] = &[
        r#"{"type":"control_response","response":{"subtype":"success","request_id":"kiln_1","response":{"commands":[{"name":"compact","description":"Compact the conversation","argumentHint":"<instructions>"}],"models":[{"value":"default","displayName":"Default (recommended)","description":"Opus 5.5"}],"agents":[{"name":"general-purpose","description":"General"}],"current_permission_mode":"default","claude_code_version":"2.1.296"}}}"#,
        r#"{"type":"system","subtype":"init","cwd":"/work","session_id":"9e27860a-3c18-4752-86c6-d427ca280b36","tools":["Bash","Edit"],"mcp_servers":[],"model":"claude-opus-5-5","permissionMode":"default","claude_code_version":"2.1.296","agents":["general-purpose"]}"#,
        r#"{"type":"command_lifecycle","command_uuid":"m1","state":"queued","uuid":"x","session_id":"s"}"#,
        r#"{"type":"command_lifecycle","command_uuid":"m1","state":"started","uuid":"x","session_id":"s"}"#,
        r#"{"type":"system","subtype":"status","status":"requesting","session_id":"s","uuid":"u1"}"#,
        r#"{"type":"user","message":{"role":"user","content":"please RUN_BASH"},"session_id":"s","parent_tool_use_id":null,"uuid":"m1","isReplay":true}"#,
        r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[]}},"parent_tool_use_id":null,"uuid":"s1"}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}},"parent_tool_use_id":null,"uuid":"s2","api_message_id":"msg_1"}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Running "}},"parent_tool_use_id":null,"uuid":"s3","api_message_id":"msg_1"}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"a command."}},"parent_tool_use_id":null,"uuid":"s4","api_message_id":"msg_1"}"#,
        r#"{"type":"assistant","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[{"type":"text","text":"Running a command."}]},"parent_tool_use_id":null,"session_id":"s","uuid":"a1"}"#,
        r#"{"type":"assistant","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[{"type":"tool_use","id":"toolu_2","name":"Bash","input":{"command":"mkdir -p kiln-mock-dir && touch kiln-mock-dir/marker && echo kiln-mock-ok","description":"Create a marker file"}}]},"parent_tool_use_id":null,"session_id":"s","uuid":"a2"}"#,
        r#"{"type":"control_request","request_id":"f7d9","request":{"subtype":"can_use_tool","tool_name":"Bash","display_name":"Bash","input":{"command":"mkdir -p kiln-mock-dir && touch kiln-mock-dir/marker && echo kiln-mock-ok","description":"Create a marker file"},"description":"Create a marker file","permission_suggestions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"mkdir -p kiln-mock-dir"},{"toolName":"Bash","ruleContent":"touch kiln-mock-dir/marker"}],"behavior":"allow","destination":"localSettings"}],"decision_reason_type":"subcommandResults","tool_use_id":"toolu_2"}}"#,
    ];

    const BASH_AFTER_ALLOW: &[&str] = &[
        r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_2","type":"tool_result","content":"kiln-mock-ok","is_error":false}]},"parent_tool_use_id":null,"session_id":"s","uuid":"r1","tool_use_result":{"stdout":"kiln-mock-ok","stderr":"","interrupted":false,"isImage":false,"noOutputExpected":false}}"#,
        r#"{"type":"assistant","message":{"id":"msg_3","type":"message","role":"assistant","model":"claude-opus-5-5","content":[{"type":"text","text":"Done."}]},"parent_tool_use_id":null,"session_id":"s","uuid":"a3"}"#,
        r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":35,"num_turns":2,"result":"Done.","stop_reason":"end_turn","session_id":"s","total_cost_usd":0.000456,"modelUsage":{"claude-opus-5-5":{"inputTokens":24,"outputTokens":18,"cacheReadInputTokens":0,"contextWindow":1000000}}}"#,
        r#"{"type":"command_lifecycle","command_uuid":"m1","state":"completed","uuid":"x","session_id":"s"}"#,
    ];

    fn feed(c: &mut Claude, core: &mut Core, lines: &[&str]) {
        for l in lines {
            c.line(core, l);
        }
    }

    fn tool<'a>(core: &'a Core, id: &str) -> Option<&'a ToolCall> {
        match &core.get(id)?.body {
            Body::Tool(t) => Some(t),
            _ => None,
        }
    }

    #[test]
    fn a_turn_with_an_approval_from_send_to_result() -> anyhow::Result<()> {
        let mut c = Claude::new(false);
        let mut core = Core::new();
        c.start(&mut core);
        let init: Value = serde_json::from_str(core.take_writes().first().map(String::as_str).unwrap_or("{}"))?;
        assert_eq!(init.pointer("/request/subtype").and_then(Value::as_str), Some("initialize"));

        c.command(&mut core, Command::Send { id: "m1".into(), text: "please RUN_BASH".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        let sent: Value = serde_json::from_str(core.take_writes().first().map(String::as_str).unwrap_or("{}"))?;
        assert_eq!(sent.get("uuid").and_then(Value::as_str), Some("m1"));
        assert!(sent.get("priority").is_none());
        feed(&mut c, &mut core, BASH_SESSION);

        // Meta from initialize, state from init.
        assert_eq!(core.meta.version, "2.1.296");
        assert_eq!(core.meta.commands.first().map(|c| c.name.as_str()), Some("compact"));
        assert_eq!(core.state.harness_session.as_deref(), Some("9e27860a-3c18-4752-86c6-d427ca280b36"));
        assert_eq!(core.state.cwd, "/work");
        // The prompt (our id), the streamed reply and the call, in order.
        let kinds: Vec<String> = core.events().map(|e| format!("{}:{}", e.id, serde_json::to_value(&e.body).ok().and_then(|v| v.get("kind").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default())).collect();
        assert_eq!(kinds, vec!["m1:user", "msg_1:0:assistant", "toolu_2:tool"]);
        assert!(matches!(&core.get("msg_1:0").map(|e| e.body.clone()), Some(Body::Assistant { text, streaming: false }) if text == "Running a command."));
        // Waiting on the approval, which carries the exact command and an always-allow option.
        assert_eq!(core.state.status, Status::Waiting);
        let a = core.state.approvals.first().cloned().ok_or_else(|| anyhow::anyhow!("no approval"))?;
        assert_eq!(a.command.as_deref(), Some("mkdir -p kiln-mock-dir && touch kiln-mock-dir/marker && echo kiln-mock-ok"));
        assert_eq!(a.always.first().map(|o| (o.label.as_str(), o.scope.as_str())), Some(("Always allow Bash(mkdir -p kiln-mock-dir), Bash(touch kiln-mock-dir/marker)", "this project, just you")));
        assert_eq!(tool(&core, "toolu_2").map(|t| t.status), Some(ToolStatus::Pending));

        // After initialize it asks for settings, MCP servers and hooks.
        let w = core.take_writes();
        assert!(["get_settings", "mcp_status", "get_hooks_listing"].iter().all(|k| w.iter().any(|l| l.contains(k))), "{w:?}");
        // A message typed meanwhile waits in kiln's queue.
        c.command(&mut core, Command::Send { id: "m2".into(), text: "next please".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        assert_eq!(core.state.queue.len(), 1);
        assert!(core.take_writes().is_empty());

        // Always allow: the CLI gets allow + the suggested rule.
        c.command(&mut core, Command::Approve { id: "f7d9".into(), decision: Decision::Always, message: None, always: Some(0), answers: None });
        let resp: Value = serde_json::from_str(core.take_writes().first().map(String::as_str).unwrap_or("{}"))?;
        assert_eq!(resp.pointer("/response/request_id").and_then(Value::as_str), Some("f7d9"));
        assert_eq!(resp.pointer("/response/response/behavior").and_then(Value::as_str), Some("allow"));
        assert_eq!(resp.pointer("/response/response/updatedPermissions/0/destination").and_then(Value::as_str), Some("localSettings"));
        assert_eq!(core.state.status, Status::Running);

        feed(&mut c, &mut core, BASH_AFTER_ALLOW);
        let t = tool(&core, "toolu_2").cloned().ok_or_else(|| anyhow::anyhow!("no tool"))?;
        assert_eq!((t.status, t.output.as_deref()), (ToolStatus::Done, Some("kiln-mock-ok")));
        assert!((core.state.usage.cost_usd - 0.000456).abs() < 1e-9);
        assert_eq!(core.state.usage.context_window, Some(1_000_000));
        assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Completed));
        // The turn ended, so the queued message went out by itself.
        assert!(core.state.queue.is_empty());
        let writes = core.take_writes();
        assert!(writes.iter().any(|w| w.contains("\"uuid\":\"m2\"")), "{writes:?}");
        assert!(writes.iter().any(|w| w.contains("get_context_usage")));
        Ok(())
    }

    #[test]
    fn steer_and_send_now_and_deny_with_feedback() -> anyhow::Result<()> {
        let mut c = Claude::new(false);
        let mut core = Core::new();
        c.start(&mut core);
        core.take_writes();
        c.command(&mut core, Command::Send { id: "m1".into(), text: "SLOW story".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        c.line(&mut core, r#"{"type":"command_lifecycle","command_uuid":"m1","state":"started"}"#);
        core.take_writes();
        // Steer goes straight to the CLI (it folds it into the running turn), without priority.
        c.command(&mut core, Command::Send { id: "m2".into(), text: "also the moon".into(), mode: SendMode::Steer, attachments: vec![], overrides: None });
        let w = core.take_writes();
        assert!(w.first().is_some_and(|l| l.contains("also the moon") && !l.contains("priority")));
        // Send now carries priority "now".
        c.command(&mut core, Command::Send { id: "m3".into(), text: "urgent".into(), mode: SendMode::Now, attachments: vec![], overrides: None });
        let w = core.take_writes();
        assert!(w.first().is_some_and(|l| l.contains("\"priority\":\"now\"")));
        // A denial with feedback lets the model continue; without feedback it stops the turn.
        c.line(&mut core, r#"{"type":"control_request","request_id":"r9","request":{"subtype":"can_use_tool","tool_name":"Write","input":{"file_path":"/work/a.txt","content":"x\n"},"tool_use_id":"t9"}}"#);
        c.command(&mut core, Command::Approve { id: "r9".into(), decision: Decision::Deny, message: Some("use b.txt".into()), always: None, answers: None });
        let resp: Value = serde_json::from_str(core.take_writes().first().map(String::as_str).unwrap_or("{}"))?;
        assert_eq!(resp.pointer("/response/response/behavior").and_then(Value::as_str), Some("deny"));
        assert_eq!(resp.pointer("/response/response/interrupt").and_then(Value::as_bool), Some(false));
        assert_eq!(resp.pointer("/response/response/message").and_then(Value::as_str), Some("use b.txt"));
        // A withdrawn request disappears.
        c.line(&mut core, r#"{"type":"control_request","request_id":"r10","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"rm -rf x"},"tool_use_id":"t10"}}"#);
        assert_eq!(core.state.approvals.len(), 1);
        c.line(&mut core, r#"{"type":"control_cancel_request","request_id":"r10"}"#);
        assert!(core.state.approvals.is_empty());
        // Interrupt is a control request.
        c.command(&mut core, Command::Interrupt);
        assert!(core.take_writes().first().is_some_and(|l| l.contains("\"subtype\":\"interrupt\"")));
        c.line(&mut core, r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]},"parent_tool_use_id":null}"#);
        assert!(core.events().any(|e| matches!(&e.body, Body::Notice { text, .. } if text == "Interrupted")));
        Ok(())
    }

    #[test]
    fn subagents_are_tasks_with_their_own_messages() {
        let mut c = Claude::new(false);
        let mut core = Core::new();
        for l in [
            r#"{"type":"assistant","message":{"id":"msg_4","model":"claude-opus-5-5","content":[{"type":"tool_use","id":"toolu_5","name":"Agent","input":{"description":"Mock subtask","prompt":"SUBTASK say hi","subagent_type":"general-purpose"}}]},"parent_tool_use_id":null}"#,
            r#"{"type":"system","subtype":"task_started","task_id":"a575","tool_use_id":"toolu_5","description":"Mock subtask","subagent_type":"general-purpose","is_backgrounded":true,"spawn_depth":1,"task_type":"local_agent","prompt":"SUBTASK say hi"}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_5","type":"tool_result","content":[{"type":"text","text":"Async agent launched successfully."}]}]},"parent_tool_use_id":null}"#,
            r#"{"type":"assistant","message":{"id":"msg_6","model":"claude-opus-5-5","content":[{"type":"text","text":"Subagent says hi."}]},"parent_tool_use_id":"toolu_5","agent_id":"a575"}"#,
            r#"{"type":"system","subtype":"task_updated","task_id":"a575","patch":{"status":"completed","end_time":1792194748074}}"#,
            r#"{"type":"system","subtype":"task_notification","task_id":"a575","tool_use_id":"toolu_5","status":"completed","output_file":"/tmp/x","summary":"Subagent says hi.","usage":{"total_tokens":21,"tool_uses":0,"duration_ms":627}}"#,
        ] {
            c.line(&mut core, l);
        }
        let t = core.state.tasks.first().cloned().unwrap_or_default();
        assert_eq!((t.id.as_str(), t.status.as_str(), t.tokens, t.background), ("a575", "completed", 21, true));
        assert_eq!(t.summary.as_deref(), Some("Subagent says hi."));
        assert_eq!(tool(&core, "toolu_5").and_then(|x| x.task.clone()).as_deref(), Some("a575"));
        let sub = core.get("msg_6:0").cloned();
        assert_eq!(sub.and_then(|e| e.parent), Some("toolu_5".to_owned()));
        c.command(&mut core, Command::StopTask { task: "a575".into() });
        assert!(core.take_writes().iter().any(|w| w.contains("\"stop_task\"") && w.contains("a575")));
    }

    #[test]
    fn edit_results_and_previews_are_real_diffs() -> anyhow::Result<()> {
        let mut c = Claude::new(false);
        let mut core = Core::new();
        core.state.cwd = "/work".into();
        c.line(&mut core, r#"{"type":"assistant","message":{"id":"m","content":[{"type":"tool_use","id":"te","name":"Edit","input":{"file_path":"/work/notes.txt","old_string":"alpha","new_string":"beta","replace_all":false}}]},"parent_tool_use_id":null}"#);
        c.line(&mut core, r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"te","type":"tool_result","content":"ok"}]},"tool_use_result":{"filePath":"/work/notes.txt","oldString":"alpha","newString":"beta","structuredPatch":[{"oldStart":1,"oldLines":2,"newStart":1,"newLines":2,"lines":["-alpha","+beta"," line two"]}],"userModified":false,"replaceAll":false}}"#);
        let t = tool(&core, "te").cloned().ok_or_else(|| anyhow::anyhow!("no tool"))?;
        assert_eq!(t.title, "notes.txt");
        assert_eq!(t.diffs.first().map(|d| (d.path.as_str(), d.hunks.len())), Some(("notes.txt", 1)));

        let dir = tempfile::tempdir()?;
        let f = dir.path().join("a.rs");
        std::fs::write(&f, "fn a() {}\nfn b() {}\nfn c() {}\n")?;
        let cwd = dir.path().to_string_lossy().into_owned();
        let d = preview("Edit", &json!({"file_path": f.to_string_lossy(), "old_string": "fn b() {}", "new_string": "fn b() { 1 }"}), &cwd);
        let lines = d.first().and_then(|d| d.hunks.first()).map(|h| h.lines.clone()).unwrap_or_default();
        assert_eq!(lines, vec![" fn a() {}", "-fn b() {}", "+fn b() { 1 }", " fn c() {}"]);
        assert_eq!(d.first().map(|d| d.path.as_str()), Some("a.rs"));
        let w = preview("Write", &json!({"file_path": "new.txt", "content": "one\ntwo\n"}), &cwd);
        assert!(w.first().is_some_and(|d| d.created && d.hunks.first().is_some_and(|h| h.new_lines == 2)));
        Ok(())
    }

    #[test]
    fn questions_are_answered_through_updated_input() -> anyhow::Result<()> {
        let mut c = Claude::new(false);
        let mut core = Core::new();
        c.line(&mut core, r#"{"type":"control_request","request_id":"q1","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"question":"Which database?","header":"DB","multiSelect":false,"options":[{"label":"Postgres","description":"relational"},{"label":"SQLite","description":"embedded"}]}]},"tool_use_id":"tq"}}"#);
        let a = core.state.approvals.first().cloned().ok_or_else(|| anyhow::anyhow!("no approval"))?;
        assert_eq!(a.tool, ToolKind::Question);
        assert_eq!(a.questions.first().map(|q| q.options.len()), Some(2));
        let mut answers = std::collections::BTreeMap::new();
        answers.insert("Which database?".to_owned(), "SQLite".to_owned());
        c.command(&mut core, Command::Approve { id: "q1".into(), decision: Decision::Allow, message: None, always: None, answers: Some(answers) });
        let resp: Value = serde_json::from_str(core.take_writes().first().map(String::as_str).unwrap_or("{}"))?;
        assert_eq!(resp.pointer("/response/response/updatedInput/answers/Which database?").and_then(Value::as_str), Some("SQLite"));
        Ok(())
    }

    #[test]
    fn flushing_yields_ordered_updates() {
        let mut c = Claude::new(false);
        let mut core = Core::new();
        c.start(&mut core);
        let out = core.take_out();
        assert!(out.iter().any(|u| matches!(u, Update::Meta { .. })));
        assert!(out.iter().any(|u| matches!(u, Update::State { .. })));
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m text"), "red text");
        assert_eq!(rel("/work/src/a.rs", "/work"), "src/a.rs");
        assert_eq!(rel("/other/a.rs", "/work"), "/other/a.rs");
        let spec = super::super::session::Spec {
            kind: "claude".into(),
            model: Some("opus".into()),
            mode: Some("plan".into()),
            resume: Some("abc".into()),
            resume_at: Some("u9".into()),
            fork: true,
            effort: Some("high".into()),
            ..Default::default()
        };
        let a = args(&spec, None);
        assert!(a.windows(2).any(|w| w == ["--resume", "abc"]) && a.windows(2).any(|w| w == ["--permission-mode", "plan"]));
        assert!(a.windows(2).any(|w| w == ["--resume-session-at", "u9"]) && a.iter().any(|x| x == "--fork-session"));
        assert!(a.windows(2).any(|w| w == ["--effort", "high"]));
        assert!(!a.iter().any(|x| x == "--allow-dangerously-skip-permissions"));
    }
}
