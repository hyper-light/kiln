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
    AlwaysOption, Approval, Body, Command, Control, ControlKind, ControlOption, ControlScope, Decision, Delivery, ExitInfo, FileDiff, HookInfo, Hunk, Known, Level, Limit, McpServer, McpStatus,
    McpTool, ModeOption, ModelOption, Plugin, Question, QuestionOption, QueueItem, SendMode, Skill, SlashCommand, Status, StatusItem, Task, Todo, Tone, ToolCall, ToolKind, ToolStatus,
    OUTPUT_MAX, account_id, clip_json, clip_middle, mask_emails, plan_label, safe_url,
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
    /// get_usage: the plan limits.
    Usage,
    /// get_status: the /status rows.
    Status,
    /// get_skills_dialog: each skill's source and state.
    SkillsDialog,
    ReloadSkills,
    /// mcp_toggle / mcp_reconnect (what it did, for a failure); the servers are asked for again.
    McpChange(String),
    McpAuth { client: String, name: String },
    McpCallback { client: String, name: String },
}

/// A run of the CLI kiln is waiting on.
#[derive(Debug, Clone)]
enum Run {
    McpAdd { name: String, client: Option<String> },
    McpRemove { name: String, client: Option<String> },
}

/// How often plan usage and status are asked for while a session runs (also after each turn,
/// and when the CLI reports a rate-limit change).
const USAGE_EVERY_MS: i64 = 180_000;

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
    /// Its configuration directory (user skills and commands live under it).
    config_dir: String,
    /// The slash commands as the CLI last listed them (initialize, commands_changed, reloads).
    commands: Vec<Value>,
    /// The skills the model may use (system/init, which comes with the first turn).
    skill_names: Vec<String>,
    /// The skill commands (reload_skills, asked for at start: before the first turn).
    skill_cmds: Vec<String>,
    /// The plugins came from init (else from the settings' enabled plugins).
    plugins_from_init: bool,
    /// When the process started (ms), and how many times it has listed its MCP servers: servers
    /// that come late (claude.ai connectors are fetched) are polled for a while.
    started_at: i64,
    mcp_answers: u32,
    /// A sign-in is under way (until then, ms): the servers are polled, since one finished in a
    /// browser on this machine reaches the CLI directly.
    signing_in_until: i64,
    /// The servers were asked for again while an answer was on its way: ask once more after it.
    mcp_again: bool,
    /// Its /skills menu: each skill's source and state, when it answers.
    skill_rows: Vec<Value>,
    /// Its plugins: (name, path, source, version).
    plugins: Vec<(String, String, String, Option<String>)>,
    /// When plan usage was last asked for (ms).
    usage_at: i64,
    /// When the MCP servers were last asked for (ms).
    mcp_at: i64,
    /// CLI runs under way, by job.
    runs: HashMap<String, Run>,
    next_run: u64,
    /// The MCP configuration changed (what changed): Claude Code reads it when it starts, so it
    /// starts again, continuing the conversation, as soon as it is not in the middle of a turn.
    reload: Option<String>,
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
            config_dir: String::new(),
            commands: Vec::new(),
            skill_names: Vec::new(),
            skill_cmds: Vec::new(),
            plugins_from_init: false,
            started_at: 0,
            mcp_answers: 0,
            signing_in_until: 0,
            mcp_again: false,
            skill_rows: Vec::new(),
            plugins: Vec::new(),
            usage_at: 0,
            mcp_at: 0,
            runs: HashMap::new(),
            next_run: 0,
            reload: None,
        }
    }

    /// Where its configuration lives (`CLAUDE_CONFIG_DIR`, else `~/.claude`).
    #[must_use]
    pub fn with_config_dir(mut self, dir: String) -> Self {
        self.config_dir = dir;
        self
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
        // MCP servers and skills have their own state and commands (State::mcp, State::skills).
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
            "mcp_refresh" => self.ask_mcp(core),
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
            // Older consoles' MCP controls (now commands of their own).
            other => {
                if let Some(name) = other.strip_prefix("mcp_reconnect:") {
                    self.command(core, Command::McpReconnect { name: name.to_owned() });
                } else if let Some(name) = other.strip_prefix("mcp:") {
                    let enabled = value.as_ref().and_then(Value::as_bool).unwrap_or(true);
                    self.command(core, Command::McpEnable { name: name.to_owned(), enabled });
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
        self.started_at = Core::now();
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
            Command::McpEnable { name, enabled } => {
                let what = format!("{} {name}", if enabled { "Enable" } else { "Disable" });
                self.request(core, Pending::McpChange(what), json!({"subtype": "mcp_toggle", "serverName": name, "enabled": enabled}));
                // Shown at once; the servers' status settles it.
                if let Some(m) = core.state.mcp.iter_mut().find(|m| m.name == name) {
                    m.enabled = enabled;
                    m.status = if enabled { McpStatus::Pending } else { McpStatus::Disabled };
                    core.touch_state();
                }
            }
            Command::McpReconnect { name } => {
                self.request(core, Pending::McpChange(format!("Reconnect {name}")), json!({"subtype": "mcp_reconnect", "serverName": name}));
                if let Some(m) = core.state.mcp.iter_mut().find(|m| m.name == name && m.enabled) {
                    m.status = McpStatus::Pending;
                    m.error = None;
                    core.touch_state();
                }
            }
            Command::McpRefresh => self.ask_mcp(core),
            Command::McpAuth { name, client, redirect } => match redirect.filter(|r| !r.trim().is_empty()) {
                // Signing in: the CLI answers with the page to open, and waits for its redirect.
                None => self.request(core, Pending::McpAuth { client, name: name.clone() }, json!({"subtype": "mcp_authenticate", "serverName": name})),
                // The page the sign-in ended on (it could not reach the CLI on its own).
                Some(url) => self.request(core, Pending::McpCallback { client, name: name.clone() }, json!({"subtype": "mcp_oauth_callback_url", "serverName": name, "callbackUrl": url.trim()})),
            },
            Command::McpAdd { scope, name, transport, command, args, env, url, headers, client } => {
                let add = McpAdd { scope, name, transport, command, args, env, url, headers };
                match add.claude_args() {
                    Ok(a) => self.run(core, Run::McpAdd { name: add.name.clone(), client }, a),
                    Err(e) => answer(core, client, json!({"ok": false, "error": e})),
                }
            }
            Command::McpRemove { scope, name, client } => {
                if core.state.mcp.iter().any(|m| m.name == name && m.scope == "claudeai") {
                    answer(core, client, json!({"ok": false, "error": format!("{name} is a claude.ai connector: it is managed on claude.ai, not here.")}));
                    return;
                }
                if !valid_server_name(&name) {
                    answer(core, client, json!({"ok": false, "error": "That is not a server name."}));
                    return;
                }
                let mut a = vec!["mcp".to_owned(), "remove".to_owned()];
                if let Some(sc) = scope.filter(|s| matches!(s.as_str(), "local" | "user" | "project")) {
                    a.extend(["-s".to_owned(), sc]);
                }
                a.extend(["--".to_owned(), name.clone()]);
                self.run(core, Run::McpRemove { name, client }, a);
            }
            Command::SkillView { name, client } => self.skill_view(core, &name, client),
            Command::SkillRun { name, args, id, mode } => {
                let name = name.trim().trim_start_matches('/').to_owned();
                if name.is_empty() {
                    return;
                }
                let text = match args.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
                    Some(a) => format!("/{name} {a}"),
                    None => format!("/{name}"),
                };
                self.send(core, id.unwrap_or_else(crate::util::new_id), text, mode, Vec::new(), None);
            }
            Command::SkillsReload => self.request(core, Pending::ReloadSkills, json!({"subtype": "reload_skills"})),
        }
    }

    /// Run the CLI for `what` (its end comes back to `run_done`).
    fn run(&mut self, core: &mut Core, what: Run, args: Vec<String>) {
        self.next_run += 1;
        let job = format!("run-{}", self.next_run);
        self.runs.insert(job.clone(), what);
        core.effects.push(Effect::Run { job, args });
    }

    /// A run of the CLI ended.
    pub fn run_done(&mut self, core: &mut Core, job: &str, code: Option<i32>, output: &str) {
        let Some(run) = self.runs.remove(job) else { return };
        let (name, client, added) = match run {
            Run::McpAdd { name, client } => (name, client, true),
            Run::McpRemove { name, client } => (name, client, false),
        };
        let out = strip_ansi(output).trim().to_owned();
        if code == Some(0) {
            let what = format!("{} MCP server {name}", if added { "Added" } else { "Removed" });
            answer(core, client, json!({"ok": true, "message": if out.is_empty() { what.clone() } else { out }}));
            self.reload = Some(what);
            self.reload_when_idle(core);
        } else {
            let err = if out.is_empty() { format!("claude mcp {} failed", if added { "add" } else { "remove" }) } else { out };
            if client.is_none() {
                self.notice(core, Level::Warn, err.clone(), None);
            }
            answer(core, client, json!({"ok": false, "error": err}));
        }
    }

    /// Start again with the changed MCP configuration (resuming the conversation), unless a turn
    /// is under way: then when it ends.
    fn reload_when_idle(&mut self, core: &mut Core) {
        if self.reload.is_none() || self.busy(core) || core.state.status == Status::Starting {
            return;
        }
        let Some(what) = self.reload.take() else { return };
        // Not running, or nothing to resume: its next start reads the configuration anyway.
        if core.state.status == Status::Exited || core.state.harness_session.is_none() {
            return;
        }
        self.notice(core, Level::Info, format!("{what}: Claude Code starts again to load it (the conversation continues)"), None);
        core.effects.push(Effect::Restart { resume_at: None });
    }

    /// Every few seconds: plan usage now and then; servers that are still connecting.
    pub fn tick(&mut self, core: &mut Core) {
        self.reload_when_idle(core);
        if matches!(core.state.status, Status::Starting | Status::Exited) {
            return;
        }
        let now = Core::now();
        if now - self.usage_at > USAGE_EVERY_MS {
            self.ask_usage(core);
        }
        // Servers still connecting; and for the first minute, servers that come late (claude.ai
        // connectors are fetched after start).
        let young = now - self.started_at < 60_000;
        let watching = now < self.signing_in_until && core.state.mcp.iter().any(|m| m.status == McpStatus::NeedsAuth);
        if (young || watching || core.state.mcp.iter().any(|m| m.status == McpStatus::Pending)) && now - self.mcp_at > 4_000 {
            self.ask_mcp(core);
        }
    }

    /// Each MCP server's prompts (they are commands: "server:prompt (MCP)").
    fn count_prompts(&self, core: &mut Core) {
        let mut changed = false;
        for m in &mut core.state.mcp {
            let prefix = format!("{}:", m.name);
            let n = self.commands.iter().filter_map(|c| str_of(c, "name")).filter(|n| n.starts_with(&prefix) && n.ends_with(" (MCP)")).count();
            let n = u32::try_from(n).ok().filter(|n| *n > 0);
            if m.prompts != n {
                m.prompts = n;
                changed = true;
            }
        }
        if changed {
            core.touch_state();
        }
    }

    fn ask_usage(&mut self, core: &mut Core) {
        if self.pending.values().any(|p| matches!(p, Pending::Usage)) {
            return;
        }
        self.usage_at = Core::now();
        self.request(core, Pending::Usage, json!({"subtype": "get_usage", "skip_behaviors": true}));
        self.request(core, Pending::Status, json!({"subtype": "get_status"}));
    }

    fn ask_mcp(&mut self, core: &mut Core) {
        if self.pending.values().any(|p| matches!(p, Pending::McpStatus)) {
            self.mcp_again = true;
            return;
        }
        self.mcp_at = Core::now();
        self.request(core, Pending::McpStatus, json!({"subtype": "mcp_status"}));
    }

    /// The account, from initialize: its plan in words, and an id that names it without saying
    /// who it is.
    fn account(core: &mut Core, a: &Value) {
        let plan = match (str_of(a, "subscriptionType").filter(|s| !s.is_empty()), str_of(a, "apiProvider").unwrap_or("firstParty")) {
            (Some(sub), _) => plan_label(sub),
            (None, "bedrock") => "Amazon Bedrock".to_owned(),
            (None, "vertex") => "Google Vertex AI".to_owned(),
            (None, "foundry") => "Microsoft Foundry".to_owned(),
            (None, _) if str_of(a, "apiKeySource").is_some_and(|k| k != "none") => "API key".to_owned(),
            (None, _) if str_of(a, "tokenSource").is_some_and(|k| k != "none") => "Claude account".to_owned(),
            _ => return,
        };
        let who = str_of(a, "email").or_else(|| str_of(a, "organization"));
        core.state.account = Some(account_id("claude", &plan, who));
        core.state.plan = Some(plan);
        core.touch_state();
    }

    /// get_usage's answer: the plan's windows, as the CLI's /usage shows them.
    fn usage(core: &mut Core, body: &Value) {
        if core.state.plan.is_none()
            && let Some(sub) = str_of(body, "subscription_type")
        {
            core.state.plan = Some(plan_label(sub));
        }
        let account = core.state.account.clone();
        let mut limits = Vec::new();
        match body.get("rate_limits").filter(|r| r.is_object()) {
            Some(r) => {
                for key in ["five_hour", "seven_day", "seven_day_opus", "seven_day_sonnet", "seven_day_oauth_apps"] {
                    if let Some(w) = r.get(key).filter(|w| w.is_object()) {
                        let (label, window) = claude_window(key);
                        limits.push(Limit {
                            id: key.to_owned(),
                            label: label.to_owned(),
                            used_percent: w.get("utilization").and_then(Value::as_f64),
                            resets_at: str_of(w, "resets_at").and_then(iso_ms),
                            window: Some(window.to_owned()),
                            account: account.clone(),
                            ..Limit::default()
                        });
                    }
                }
                for m in r.get("model_scoped").and_then(Value::as_array).into_iter().flatten() {
                    let Some(name) = str_of(m, "display_name") else { continue };
                    limits.push(Limit {
                        id: format!("model:{}", name.to_lowercase().replace(' ', "-")),
                        label: format!("Weekly · {name}"),
                        used_percent: m.get("utilization").and_then(Value::as_f64),
                        resets_at: str_of(m, "resets_at").and_then(iso_ms),
                        window: Some("7d".to_owned()),
                        account: account.clone(),
                        ..Limit::default()
                    });
                }
                if let Some(x) = r.get("extra_usage").filter(|x| x.get("is_enabled").and_then(Value::as_bool) == Some(true)) {
                    limits.push(Limit {
                        id: "extra_usage".to_owned(),
                        label: "Extra usage".to_owned(),
                        used_percent: x.get("utilization").and_then(Value::as_f64),
                        used: x.get("used_credits").and_then(Value::as_f64),
                        limit: x.get("monthly_limit").and_then(Value::as_f64),
                        unit: Some(str_of(x, "currency").unwrap_or("credits").to_owned()),
                        window: Some("month".to_owned()),
                        account: account.clone(),
                        ..Limit::default()
                    });
                }
            }
            // No plan limits for this account (API key, cloud providers).
            None if body.get("rate_limits_available").and_then(Value::as_bool) == Some(false) => {}
            // Not known this time: what the rate-limit events said stands.
            None => return,
        }
        // What the rate-limit events said about each window (allowed, warning, reached) carries over.
        for l in &mut limits {
            if let Some(old) = core.state.limits.iter().find(|o| o.id == l.id) {
                l.status.clone_from(&old.status);
            }
        }
        if core.state.limits != limits {
            core.state.limits = limits;
            core.touch_state();
        }
    }

    /// rate_limit_event: the windows as the API's headers report them (for subscribers).
    fn rate_limit(&mut self, core: &mut Core, v: &Value) {
        let Some(info) = v.get("rate_limit_info") else { return };
        let account = core.state.account.clone();
        let upsert = |core: &mut Core, key: &str, frac: Option<f64>, reset_s: Option<i64>, status: Option<&str>| {
            let (label, window) = claude_window(key);
            let id = if key == "overage" { "extra_usage" } else { key };
            let i = match core.state.limits.iter().position(|l| l.id == id) {
                Some(i) => i,
                None => {
                    let window = (!window.is_empty()).then(|| window.to_owned());
                    core.state.limits.push(Limit { id: id.to_owned(), label: label.to_owned(), window, account: account.clone(), ..Limit::default() });
                    core.state.limits.len() - 1
                }
            };
            if let Some(l) = core.state.limits.get_mut(i) {
                if let Some(f) = frac {
                    l.used_percent = Some((f * 1000.0).round() / 10.0);
                }
                if let Some(r) = reset_s {
                    l.resets_at = Some(r.saturating_mul(1000));
                }
                if let Some(st) = status {
                    l.status = Some(if st == "allowed_warning" { "warning".to_owned() } else { st.to_owned() });
                }
            }
        };
        if let Some(w) = info.get("unifiedWindows").and_then(Value::as_object) {
            for (key, x) in w {
                upsert(core, key, x.get("utilization").and_then(Value::as_f64), x.get("resetsAt").and_then(Value::as_i64), None);
            }
        }
        if let Some(t) = str_of(info, "rateLimitType") {
            upsert(core, t, info.get("utilization").and_then(Value::as_f64), info.get("resetsAt").and_then(Value::as_i64), str_of(info, "status"));
        }
        core.touch_state();
        // The whole picture again, now and then.
        if Core::now() - self.usage_at > 30_000 {
            self.ask_usage(core);
        }
    }

    /// The slash commands, for the console's menu and the skills.
    fn set_commands(&mut self, core: &mut Core, cmds: &[Value]) {
        self.commands = cmds.to_vec();
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
        core.touch_meta();
        self.rebuild_skills(core);
    }

    /// State::skills (and the plugins' skill lists) from the commands, init's skills and the
    /// /skills menu: Claude Code's own commands (/compact ...) are not skills.
    fn rebuild_skills(&self, core: &mut Core) {
        let row = |name: &str| self.skill_rows.iter().find(|r| str_of(r, "name") == Some(name));
        let mut out: Vec<Skill> = Vec::new();
        let known = |name: &str| self.skill_names.iter().chain(&self.skill_cmds).any(|n| n == name);
        for c in &self.commands {
            let Some(name) = str_of(c, "name") else { continue };
            let builtin = c.get("builtin").and_then(Value::as_bool) == Some(true);
            let r = row(name);
            // Claude Code's own commands (/compact ...) are not skills; its bundled skills are.
            if builtin && r.is_none() && !known(name) {
                continue;
            }
            let (desc, hint) = split_source(str_of(c, "description").unwrap_or(""));
            let source = r
                .and_then(|r| str_of(r, "source"))
                .map(str::to_owned)
                .or_else(|| builtin.then(|| "built-in".to_owned()))
                .or_else(|| skill_source(name))
                .or(hint)
                .unwrap_or_else(|| "user".to_owned());
            out.push(Skill {
                name: name.to_owned(),
                description: r.and_then(|r| str_of(r, "description")).map(str::to_owned).unwrap_or(desc),
                source,
                path: None,
                args_hint: str_of(c, "argumentHint").filter(|h| !h.is_empty()).map(str::to_owned),
                user_invocable: r.and_then(|r| r.get("advertised")).and_then(Value::as_bool).unwrap_or(true),
                enabled: r.and_then(|r| str_of(r, "state")) != Some("off"),
            });
        }
        // Skills only the model uses, and ones the menu lists that are not commands (switched off).
        let extra: Vec<String> = self.skill_names.iter().chain(&self.skill_cmds).cloned().chain(self.skill_rows.iter().filter_map(|r| str_of(r, "name").map(str::to_owned))).collect();
        for name in extra {
            if out.iter().any(|s| s.name == name) {
                continue;
            }
            let r = row(&name);
            out.push(Skill {
                description: r.and_then(|r| str_of(r, "description")).unwrap_or("").to_owned(),
                source: r.and_then(|r| str_of(r, "source")).map(str::to_owned).or_else(|| skill_source(&name)).unwrap_or_else(|| "user".to_owned()),
                path: None,
                args_hint: None,
                user_invocable: r.and_then(|r| r.get("advertised")).and_then(Value::as_bool).unwrap_or(false),
                enabled: r.and_then(|r| str_of(r, "state")) != Some("off"),
                name,
            });
        }
        let enabled_of = |name: &str| self.plugins_from_init || core.state.plugins.iter().find(|p| p.name == name).is_none_or(|p| p.enabled);
        let plugins: Vec<Plugin> = self
            .plugins
            .iter()
            .map(|(name, _, source, version)| Plugin {
                name: name.clone(),
                source: source.split_once('@').map_or(source.as_str(), |(_, m)| m).to_owned(),
                enabled: enabled_of(name),
                skills: out.iter().filter(|s| s.name.strip_prefix(name.as_str()).is_some_and(|r| r.starts_with(':'))).map(|s| s.name.clone()).collect(),
                commands: Vec::new(),
                version: version.clone(),
            })
            .collect();
        if core.state.skills != out || core.state.plugins != plugins {
            core.state.skills = out;
            core.state.plugins = plugins;
            core.touch_state();
        }
    }

    /// Where a skill's (or command's) file can be: the project's .claude (and its parents'), the
    /// user's configuration, a plugin's folder.
    fn skill_paths(&self, name: &str, cwd: &str) -> Vec<String> {
        let parts: Vec<&str> = name.split(':').collect();
        if parts.iter().any(|p| p.is_empty() || *p == "." || *p == ".." || p.contains(['/', '\\'])) {
            return Vec::new();
        }
        let mut roots: Vec<std::path::PathBuf> = std::path::Path::new(cwd).ancestors().take(8).map(|d| d.join(".claude")).collect();
        if !self.config_dir.is_empty() {
            roots.push(std::path::PathBuf::from(&self.config_dir));
        }
        let mut out = Vec::new();
        let rest = parts.join("/");
        let cfg = std::path::PathBuf::from(&self.config_dir);
        // Skills synced from claude.ai: "anthropic-skills:<name>", under skills/synced/<account>/
        // (the most recently synced account's first; `*` is matched when the file is read).
        if let (Some(&"anthropic-skills"), Some(last)) = (parts.first(), parts.last()) {
            out.push(cfg.join("skills").join("synced").join("*").join(last).join("SKILL.md"));
        }
        if let [single] = parts.as_slice() {
            for r in &roots {
                out.push(r.join("skills").join(single).join("SKILL.md"));
                out.push(r.join("commands").join(format!("{single}.md")));
            }
            out.push(cfg.join("skills").join("synced").join("*").join(single).join("SKILL.md"));
        } else {
            if let (Some(plugin), Some(tail)) = (parts.first(), parts.get(1..))
                && let Some((_, path, _, _)) = self.plugins.iter().find(|p| p.0 == *plugin && p.1 != "builtin")
            {
                let tail = tail.join("/");
                out.push(std::path::Path::new(path).join("skills").join(&tail).join("SKILL.md"));
                out.push(std::path::Path::new(path).join("commands").join(format!("{tail}.md")));
            }
            // An installed plugin's (its folder is known once init lists it; until then, its cache).
            if let (Some(plugin), Some(tail)) = (parts.first(), parts.get(1..)) {
                let tail = tail.join("/");
                let cache = cfg.join("plugins").join("cache").join("*").join(plugin).join("*");
                out.push(cache.join("skills").join(&tail).join("SKILL.md"));
                out.push(cache.join("commands").join(format!("{tail}.md")));
            }
            for r in &roots {
                out.push(r.join("commands").join(format!("{rest}.md")));
            }
        }
        out.into_iter().map(|p| p.to_string_lossy().into_owned()).collect()
    }

    fn skill_view(&mut self, core: &mut Core, name: &str, client: String) {
        let Some(skill) = core.state.skills.iter().find(|s| s.name == name).cloned() else {
            core.reply(client, json!({"error": format!("This session has no skill named {name}.")}));
            return;
        };
        // Built into Claude Code, or an MCP server's prompt: there is no file to show.
        if skill.source == "built-in" || skill.source == "mcp" {
            core.reply(client, json!({"content": skill.description, "builtin": true, "source": skill.source}));
            return;
        }
        let paths = self.skill_paths(name, &core.state.cwd);
        core.effects.push(Effect::Read { client, paths });
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
            "rate_limit_event" => self.rate_limit(core, &v),
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
                Pending::Suggest(client) | Pending::Reply(client) | Pending::McpAuth { client, .. } | Pending::McpCallback { client, .. } => core.reply(client, json!({"error": err})),
                Pending::Initialize | Pending::ContextUsage | Pending::Settings | Pending::McpStatus | Pending::Hooks | Pending::SkillsDialog => {}
                Pending::Usage => {
                    core.state.inventory.limits = Known::Unsupported;
                    core.touch_state();
                }
                // The commands initialize listed stand for the skills.
                Pending::ReloadSkills => {
                    core.state.inventory.skills = Known::Ready;
                    self.request(core, Pending::SkillsDialog, json!({"subtype": "get_skills_dialog"}));
                    self.rebuild_skills(core);
                }
                // No /status rows from this CLI: kiln's own summary.
                Pending::Status => self.fallback_status(core),
                Pending::McpChange(what) => {
                    self.notice(core, Level::Warn, format!("{what}: {err}"), None);
                    self.ask_mcp(core);
                }
                Pending::Plain(what) => self.notice(core, Level::Warn, format!("{what}: {err}"), None),
                _ => self.notice(core, Level::Warn, err, None),
            }
            return;
        }
        match kind {
            Pending::Initialize => {
                if let Some(cmds) = body.get("commands").and_then(Value::as_array) {
                    self.set_commands(core, cmds);
                }
                if let Some(a) = body.get("account") {
                    Self::account(core, a);
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
                self.ask_mcp(core);
                self.request(core, Pending::Hooks, json!({"subtype": "get_hooks_listing"}));
                // The skill commands, now: init (which lists the skills) only comes with the first
                // turn. Its answer asks for the /skills menu.
                self.request(core, Pending::ReloadSkills, json!({"subtype": "reload_skills"}));
                self.ask_usage(core);
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
                // The plugins the settings switch on or off, until init says which loaded.
                if !self.plugins_from_init {
                    let plugins: Vec<Plugin> = body
                        .pointer("/effective/enabledPlugins")
                        .and_then(Value::as_object)
                        .map(|m| {
                            m.iter()
                                .map(|(id, on)| {
                                    let (name, market) = id.split_once('@').unwrap_or((id.as_str(), ""));
                                    Plugin { name: name.to_owned(), source: market.to_owned(), enabled: on.as_bool().unwrap_or(false), skills: Vec::new(), commands: Vec::new(), version: None }
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let mut plugins = plugins;
                    plugins.sort_by(|a, b| a.name.cmp(&b.name));
                    self.plugins = plugins.iter().map(|p| (p.name.clone(), String::new(), format!("{}@{}", p.name, p.source), None)).collect();
                    core.state.plugins = plugins;
                    core.state.inventory.plugins = Known::Ready;
                    self.rebuild_skills(core);
                    core.touch_state();
                }
            }
            Pending::McpStatus => {
                let servers: Vec<McpServer> = body.get("mcpServers").and_then(Value::as_array).map(|l| l.iter().filter_map(claude_server).collect()).unwrap_or_default();
                self.mcp_answers += 1;
                // None yet may only mean not yet (claude.ai connectors are fetched at start): an empty
                // list counts once it has stayed empty for a few seconds.
                if core.state.inventory.mcp != Known::Ready && (!servers.is_empty() || (self.mcp_answers >= 2 && Core::now() - self.started_at >= 5_000)) {
                    core.state.inventory.mcp = Known::Ready;
                    core.touch_state();
                }
                if core.state.mcp != servers {
                    core.state.mcp = servers;
                    core.touch_state();
                }
                self.count_prompts(core);
                if std::mem::take(&mut self.mcp_again) {
                    self.ask_mcp(core);
                }
            }
            Pending::McpChange(_) => self.ask_mcp(core),
            Pending::McpAuth { client, name } => match str_of(&body, "authUrl") {
                Some(url) => {
                    self.signing_in_until = Core::now() + 300_000;
                    core.reply(
                    client,
                    json!({
                        "url": url,
                        // The CLI waits for the sign-in's redirect on its own machine's loopback; from
                        // anywhere else, the address the sign-in ends on is sent back (McpAuth.redirect).
                        "callback": body.get("callbackExpected").and_then(Value::as_bool).unwrap_or(false),
                        "redirect": str_of(&body, "redirectScheme"),
                    }),
                    );
                }
                None => {
                    core.reply(client, json!({"ok": true, "message": format!("{name} needs no sign-in.")}));
                    self.ask_mcp(core);
                }
            },
            Pending::McpCallback { client, name } => {
                core.reply(client, json!({"ok": true, "message": format!("Signed in to {name}.")}));
                // Signed in through kiln, the CLI does not connect by itself: it is asked to.
                self.request(core, Pending::McpChange(format!("Reconnect {name}")), json!({"subtype": "mcp_reconnect", "serverName": name}));
            }
            Pending::Usage => {
                core.state.inventory.limits = Known::Ready;
                Self::usage(core, &body);
                core.touch_state();
            }
            Pending::Status => {
                let rows: Vec<StatusItem> = body
                    .get("sections")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .flat_map(|sec| {
                        let group = str_of(sec, "title").unwrap_or("").to_owned();
                        sec.get("rows").and_then(Value::as_array).into_iter().flatten().filter_map(move |r| {
                            let value = strip_ansi(str_of(r, "value")?);
                            Some(StatusItem { group: group.clone(), label: str_of(r, "label").unwrap_or("").to_owned(), tone: tone_of(&value), value: mask_emails(value.trim()) })
                        })
                    })
                    .collect();
                if rows.is_empty() {
                    self.fallback_status(core);
                } else if core.state.harness_status != rows {
                    core.state.harness_status = rows;
                    core.touch_state();
                }
            }
            Pending::SkillsDialog => {
                self.skill_rows = body.get("skills").and_then(Value::as_array).cloned().unwrap_or_default();
                self.rebuild_skills(core);
            }
            Pending::ReloadSkills => {
                // The skills again; the commands that are not skills stay.
                self.skill_cmds = body.get("skills").and_then(Value::as_array).into_iter().flatten().filter_map(|s| str_of(s, "name").map(str::to_owned)).collect();
                core.state.inventory.skills = Known::Ready;
                for sk in body.get("skills").and_then(Value::as_array).into_iter().flatten() {
                    let name = str_of(sk, "name");
                    match self.commands.iter_mut().find(|c| str_of(c, "name") == name) {
                        Some(c) => *c = sk.clone(),
                        None => self.commands.push(sk.clone()),
                    }
                }
                let cmds = self.commands.clone();
                self.set_commands(core, &cmds);
                self.request(core, Pending::SkillsDialog, json!({"subtype": "get_skills_dialog"}));
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
                if let Some(sk) = v.get("skills").and_then(Value::as_array) {
                    self.skill_names = sk.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect();
                    core.state.inventory.skills = Known::Ready;
                }
                if let Some(pl) = v.get("plugins").and_then(Value::as_array) {
                    self.plugins = plugins_of(pl);
                    self.plugins_from_init = true;
                    core.state.inventory.plugins = Known::Ready;
                }
                // Init lists every server the session has (claude.ai connectors too): any mcp_status
                // has not shown yet join, by name, until it says more.
                if let Some(ms) = v.get("mcp_servers").and_then(Value::as_array) {
                    let mut missing = false;
                    for m in ms.iter().filter_map(claude_server) {
                        if !core.state.mcp.iter().any(|x| x.name == m.name) {
                            core.state.mcp.push(m);
                            missing = true;
                        }
                    }
                    core.state.inventory.mcp = Known::Ready;
                    if missing {
                        self.ask_mcp(core);
                    }
                    self.count_prompts(core);
                }
                self.rebuild_skills(core);
                core.touch_state();
            }
            "commands_changed" => {
                if let Some(cmds) = v.get("commands").and_then(Value::as_array) {
                    self.set_commands(core, cmds);
                }
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
                    self.reload_when_idle(core);
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
                        server: None,
                        tool_short: None,
                        skill: None,
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
                    server: None,
                    tool_short: None,
                    skill: None,
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
        if Core::now() - self.usage_at > 60_000 {
            self.ask_usage(core);
        }
        self.drain_queue(core);
        self.reload_when_idle(core);
    }

    /// Status lines kiln puts together itself, for a CLI that has no /status rows to give.
    fn fallback_status(&self, core: &mut Core) {
        let st = &core.state;
        let mut rows = Vec::new();
        let mut push = |group: &str, label: &str, value: String, tone: Option<Tone>| {
            if !value.is_empty() {
                rows.push(StatusItem { group: group.to_owned(), label: label.to_owned(), value, tone });
            }
        };
        push("Session", "Version", core.meta.version.clone(), None);
        push("Session", "Session ID", st.harness_session.clone().unwrap_or_default(), None);
        push("Session", "cwd", st.cwd.clone(), None);
        push("Model", "Model", st.model.clone(), None);
        push("Model", "Effort", st.effort.clone(), None);
        push("Model", "Permission mode", st.mode.clone(), None);
        push("Account", "Plan", st.plan.clone().unwrap_or_default(), None);
        for m in &st.mcp {
            let tone = match m.status {
                McpStatus::Connected => Some(Tone::Ok),
                McpStatus::Failed => Some(Tone::Error),
                McpStatus::NeedsAuth | McpStatus::Pending => Some(Tone::Warn),
                McpStatus::Disabled => None,
            };
            push("MCP servers", &m.name, mcp_status_words(m.status).to_owned(), tone);
        }
        if core.state.harness_status != rows {
            core.state.harness_status = rows;
            core.touch_state();
        }
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

/// Answer the client that asked, or say it in the timeline when no one is waiting.
fn answer(core: &mut Core, client: Option<String>, value: Value) {
    match client.filter(|c| !c.is_empty()) {
        Some(c) => core.reply(c, value),
        None => {
            let ok = value.get("ok").and_then(Value::as_bool) == Some(true);
            let text = str_of(&value, "message").or_else(|| str_of(&value, "error")).unwrap_or(if ok { "Done." } else { "That did not work." }).to_owned();
            let n = core.events().count();
            core.upsert(&format!("notice-run-{n}-{}", Core::now()), None, Body::Notice { level: if ok { Level::Info } else { Level::Warn }, text });
        }
    }
}

/// A plan-limit window's words: (label, length).
fn claude_window(key: &str) -> (&'static str, &'static str) {
    match key {
        "five_hour" => ("Session", "5h"),
        "seven_day" => ("Weekly", "7d"),
        "seven_day_opus" => ("Weekly · Opus", "7d"),
        "seven_day_sonnet" => ("Weekly · Sonnet", "7d"),
        "seven_day_oauth_apps" => ("Weekly · OAuth apps", "7d"),
        "seven_day_overage_included" => ("Weekly · included models", "7d"),
        "overage" | "extra_usage" => ("Extra usage", "month"),
        _ => ("Limit", ""),
    }
}

/// An RFC 3339 time as Unix ms.
pub fn iso_ms(s: &str) -> Option<i64> {
    let t = time::OffsetDateTime::parse(s.trim(), &time::format_description::well_known::Rfc3339).ok()?;
    i64::try_from(t.unix_timestamp_nanos() / 1_000_000).ok()
}

/// A command's description without the source the CLI appends to it ("… (user)").
fn split_source(desc: &str) -> (String, Option<String>) {
    let d = desc.trim_end();
    if let Some(open) = d.rfind(" (")
        && d.ends_with(')')
        && let Some(src) = d.get(open + 2..d.len() - 1)
        && matches!(src, "user" | "project" | "local" | "managed" | "plugin" | "claude.ai" | "org")
    {
        return (d.get(..open).unwrap_or(d).to_owned(), Some(src.to_owned()));
    }
    (d.to_owned(), None)
}

/// Where a skill comes from, by its name: an MCP server's prompt ("server:prompt (MCP)"), a skill
/// synced from claude.ai ("anthropic-skills:…"), a plugin's ("plugin:…").
fn skill_source(name: &str) -> Option<String> {
    if name.ends_with(" (MCP)") {
        return Some("mcp".to_owned());
    }
    let (prefix, _) = name.split_once(':')?;
    // Any other "prefix:" is a plugin's (one not listed yet, before init, too).
    Some(if prefix == "anthropic-skills" { "claude.ai" } else { "plugin" }.to_owned())
}

fn plugins_of(list: &[Value]) -> Vec<(String, String, String, Option<String>)> {
    list.iter()
        .filter_map(|p| Some((str_of(p, "name")?.to_owned(), str_of(p, "path").unwrap_or("").to_owned(), str_of(p, "source").unwrap_or("").to_owned(), str_of(p, "version").map(str::to_owned))))
        .collect()
}

pub fn mcp_status_words(s: McpStatus) -> &'static str {
    match s {
        McpStatus::Connected => "connected",
        McpStatus::Pending => "connecting",
        McpStatus::Failed => "failed",
        McpStatus::NeedsAuth => "needs sign-in",
        McpStatus::Disabled => "disabled",
    }
}

/// A status row's tone, from the marks the CLI's /status puts on it.
fn tone_of(value: &str) -> Option<Tone> {
    let v = value.trim_start();
    if v.starts_with(['✔', '✓']) {
        Some(Tone::Ok)
    } else if v.starts_with(['✘', '✗', '×']) {
        Some(Tone::Error)
    } else if v.starts_with(['⚠', '△', '!']) {
        Some(Tone::Warn)
    } else {
        None
    }
}

/// An MCP server as mcp_status (or init, or a reload) reports it.
fn claude_server(s: &Value) -> Option<McpServer> {
    let name = str_of(s, "name")?.to_owned();
    let cfg = s.get("config");
    let transport = cfg
        .and_then(|c| str_of(c, "type"))
        .map(str::to_owned)
        .or_else(|| cfg.and_then(|c| c.get("command")).map(|_| "stdio".to_owned()))
        .unwrap_or_default();
    let target = cfg.and_then(|c| match (str_of(c, "url"), str_of(c, "command")) {
        (Some(u), _) => Some(safe_url(u)),
        (None, Some(cmd)) => Some(safe_command(cmd, c.get("args").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default())),
        _ => None,
    });
    let status = match str_of(s, "status").unwrap_or("") {
        "connected" => McpStatus::Connected,
        "failed" => McpStatus::Failed,
        "needs-auth" => McpStatus::NeedsAuth,
        "disabled" => McpStatus::Disabled,
        _ => McpStatus::Pending,
    };
    Some(McpServer {
        scope: str_of(s, "scope").or_else(|| str_of(s, "source")).unwrap_or("").to_owned(),
        transport,
        error: str_of(s, "error").map(|e| clip_middle(&strip_ansi(e), 2000).0),
        enabled: status != McpStatus::Disabled,
        tools: s
            .get("tools")
            .and_then(Value::as_array)
            .map(|l| l.iter().filter_map(|t| Some(McpTool { name: str_of(t, "name")?.to_owned(), description: str_of(t, "description").map(|d| clip_middle(d, 600).0) })).collect())
            .unwrap_or_default(),
        resources: None,
        prompts: None,
        target,
        server: s.get("serverInfo").and_then(|i| Some(format!("{} {}", str_of(i, "name")?, str_of(i, "version").unwrap_or("")).trim().to_owned())),
        auth: (status == McpStatus::NeedsAuth).then(|| "signed out".to_owned()),
        status,
        name,
    })
}

/// A server's command line to show: values of arguments that look like secrets are hidden
/// (`--api-key=…`, `--token …`).
pub fn safe_command(cmd: &str, args: Vec<&str>) -> String {
    let secretish = |s: &str| {
        let l = s.to_ascii_lowercase();
        ["token", "secret", "password", "passwd", "apikey", "api-key", "api_key", "auth", "credential", "bearer"].iter().any(|k| l.contains(k))
    };
    let mut out = vec![cmd.to_owned()];
    let mut hide_next = false;
    for a in args {
        if hide_next {
            out.push("…".to_owned());
            hide_next = false;
            continue;
        }
        match a.split_once('=') {
            Some((k, _)) if secretish(k) => out.push(format!("{k}=…")),
            _ if a.starts_with('-') && secretish(a) => {
                out.push(a.to_owned());
                hide_next = true;
            }
            _ => out.push(if a.contains(' ') { format!("'{a}'") } else { a.to_owned() }),
        }
    }
    out.join(" ")
}

/// A server name both CLIs accept (and that is safe in a config key path).
pub fn valid_server_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')) && !name.starts_with('-')
}

/// An MCP server to add, as the console sent it.
pub struct McpAdd {
    pub scope: String,
    pub name: String,
    pub transport: String,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
    pub url: Option<String>,
    pub headers: std::collections::BTreeMap<String, String>,
}

impl McpAdd {
    /// Its transport: as given, else http for a URL and stdio for a command.
    pub fn transport(&self) -> String {
        match self.transport.trim() {
            "" if self.url.as_deref().is_some_and(|u| !u.trim().is_empty()) => "http".to_owned(),
            "" => "stdio".to_owned(),
            t => t.to_ascii_lowercase(),
        }
    }

    pub fn check(&self) -> Result<(), String> {
        if !valid_server_name(&self.name) {
            return Err("A server name is letters, digits, '-' and '_' (up to 64).".to_owned());
        }
        if self.env.keys().any(|k| k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')) {
            return Err("An environment variable name is letters, digits and '_'.".to_owned());
        }
        if self.headers.keys().any(|k| k.is_empty() || k.contains([':', '\r', '\n'])) || self.headers.values().any(|v| v.contains(['\r', '\n'])) {
            return Err("A header is a name and a value on one line.".to_owned());
        }
        match self.transport().as_str() {
            "stdio" if self.command.as_deref().is_some_and(|c| !c.trim().is_empty()) => Ok(()),
            "stdio" => Err("A stdio server needs the command that starts it.".to_owned()),
            "http" | "sse" if self.url.as_deref().is_some_and(|u| u.trim().starts_with("http://") || u.trim().starts_with("https://")) => Ok(()),
            "http" | "sse" => Err("An http server needs its http:// or https:// address.".to_owned()),
            other => Err(format!("kiln cannot add a {other} server.")),
        }
    }

    /// `claude mcp add` arguments.
    pub fn claude_args(&self) -> Result<Vec<String>, String> {
        self.check()?;
        let t = self.transport();
        let mut a: Vec<String> = vec!["mcp".into(), "add".into()];
        if matches!(self.scope.as_str(), "local" | "user" | "project") {
            a.extend(["-s".into(), self.scope.clone()]);
        }
        a.extend(["-t".into(), t.clone()]);
        for (k, v) in &self.env {
            a.extend(["-e".into(), format!("{k}={v}")]);
        }
        if t != "stdio" {
            for (k, v) in &self.headers {
                a.extend(["-H".into(), format!("{k}: {v}")]);
            }
        }
        a.extend(["--".into(), self.name.clone()]);
        if t == "stdio" {
            a.push(self.command.clone().unwrap_or_default().trim().to_owned());
            a.extend(self.args.iter().cloned());
        } else {
            a.push(self.url.clone().unwrap_or_default().trim().to_owned());
        }
        Ok(a)
    }
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

    /// The control requests kiln wrote since the last call: (request id, subtype, request).
    fn requests(core: &mut Core) -> Vec<(String, String, Value)> {
        core.take_writes()
            .iter()
            .filter_map(|w| serde_json::from_str::<Value>(w).ok())
            .filter(|v| str_of(v, "type") == Some("control_request"))
            .map(|v| (str_of(&v, "request_id").unwrap_or("").to_owned(), v.pointer("/request/subtype").and_then(Value::as_str).unwrap_or("").to_owned(), v.get("request").cloned().unwrap_or(Value::Null)))
            .collect()
    }

    fn reply_to(c: &mut Claude, core: &mut Core, reqs: &[(String, String, Value)], subtype: &str, response: Value) {
        let id = reqs.iter().find(|r| r.1 == subtype).map(|r| r.0.clone()).unwrap_or_default();
        c.line(core, &json!({"type": "control_response", "response": {"subtype": "success", "request_id": id, "response": response}}).to_string());
    }

    fn replies(core: &mut Core) -> Vec<(String, Value)> {
        core.take_out()
            .into_iter()
            .filter_map(|u| match u {
                Update::Reply { client, value } => Some((client, value)),
                _ => None,
            })
            .collect()
    }

    /// A session that has answered initialize (as a Max subscriber) and init.
    fn started() -> (Claude, Core, Vec<(String, String, Value)>) {
        let mut c = Claude::new(false).with_config_dir("/home/u/.claude".into());
        let mut core = Core::new();
        core.state.cwd = "/work/app".into();
        c.start(&mut core);
        let reqs = requests(&mut core);
        let commands = json!([
            {"name": "kiln-test", "description": "A test skill. (user)", "argumentHint": "<word>"},
            {"name": "proj-skill", "description": "A project skill. (project)", "argumentHint": ""},
            {"name": "dataviz", "description": "Charts.", "argumentHint": "", "builtin": true},
            {"name": "compact", "description": "Compact the conversation", "argumentHint": "<instructions>", "builtin": true},
            {"name": "tools:lint", "description": "Lint with the plugin", "argumentHint": ""}
        ]);
        reply_to(
            &mut c,
            &mut core,
            &reqs,
            "initialize",
            json!({"commands": commands, "models": [], "account": {"email": "someone@example.com", "subscriptionType": "max", "tokenSource": "claude.ai"}, "claude_code_version": "2.1.296"}),
        );
        c.line(
            &mut core,
            &json!({"type": "system", "subtype": "init", "cwd": "/work/app", "session_id": "s1", "model": "claude-opus-5-5", "permissionMode": "default", "claude_code_version": "2.1.296",
                "skills": ["kiln-test", "proj-skill", "dataviz", "tools:lint", "model-only"],
                "plugins": [{"name": "tools", "path": "/plugins/tools", "source": "tools@market", "version": "1.2.0"}],
                "mcp_servers": [{"name": "echo", "status": "pending", "source": "user"}]})
            .to_string(),
        );
        let mut reqs = requests(&mut core);
        // The skill commands asked for at start; the /skills menu after them.
        reply_to(
            &mut c,
            &mut core,
            &reqs,
            "reload_skills",
            json!({"skills": [
                {"name": "kiln-test", "description": "A test skill. (user)", "argumentHint": "<word>"},
                {"name": "proj-skill", "description": "A project skill. (project)", "argumentHint": ""},
                {"name": "dataviz", "description": "Charts.", "argumentHint": "", "builtin": true}
            ]}),
        );
        reqs.extend(requests(&mut core));
        (c, core, reqs)
    }

    #[test]
    fn plan_limits_account_and_status_as_the_cli_reports_them() -> anyhow::Result<()> {
        let (mut c, mut core, reqs) = started();
        let subtypes: Vec<&str> = reqs.iter().map(|r| r.1.as_str()).collect();
        for want in ["get_usage", "get_status", "reload_skills", "get_skills_dialog", "mcp_status"] {
            assert!(subtypes.contains(&want), "{want} not asked for: {subtypes:?}");
        }
        assert!(reqs.iter().any(|r| r.1 == "get_usage" && r.2.get("skip_behaviors") == Some(&json!(true))));
        assert_eq!(core.state.plan.as_deref(), Some("Max"));
        let account = core.state.account.clone().unwrap_or_default();
        assert!(account.starts_with("claude:max:") && !account.contains("someone"), "{account}");
        reply_to(
            &mut c,
            &mut core,
            &reqs,
            "get_usage",
            json!({"session": {"total_cost_usd": 0.1}, "subscription_type": "max", "rate_limits_available": true, "rate_limits": {
                "five_hour": {"utilization": 42.5, "resets_at": "2026-10-17T05:00:00Z"},
                "seven_day": {"utilization": 13.0, "resets_at": "2026-10-20T00:00:00.000Z"},
                "seven_day_opus": null,
                "model_scoped": [{"display_name": "Fable", "utilization": 7.0, "resets_at": null}],
                "extra_usage": {"is_enabled": true, "monthly_limit": 50.0, "used_credits": 12.5, "utilization": 25.0, "currency": "USD"}
            }, "behaviors": null}),
        );
        let ids: Vec<&str> = core.state.limits.iter().map(|l| l.id.as_str()).collect();
        assert_eq!(ids, vec!["five_hour", "seven_day", "model:fable", "extra_usage"]);
        let five = core.state.limits.first().cloned().unwrap_or_default();
        assert_eq!((five.label.as_str(), five.used_percent, five.window.as_deref()), ("Session", Some(42.5), Some("5h")));
        assert_eq!(five.resets_at, Some(1_792_213_200_000));
        assert_eq!(five.account.as_deref(), Some(account.as_str()));
        let extra = core.state.limits.iter().find(|l| l.id == "extra_usage").cloned().unwrap_or_default();
        assert_eq!((extra.used, extra.limit, extra.unit.as_deref()), (Some(12.5), Some(50.0), Some("USD")));
        // A rate-limit event: the window it names moves, and says it is close.
        c.line(
            &mut core,
            &json!({"type": "rate_limit_event", "rate_limit_info": {"status": "allowed_warning", "rateLimitType": "five_hour", "utilization": 0.91, "resetsAt": 1_792_213_200,
                "unifiedWindows": {"seven_day": {"utilization": 0.2, "resetsAt": 1_792_454_400}}}, "uuid": "u", "session_id": "s1"})
            .to_string(),
        );
        let five = core.state.limits.iter().find(|l| l.id == "five_hour").cloned().unwrap_or_default();
        assert_eq!((five.used_percent, five.status.as_deref()), (Some(91.0), Some("warning")));
        assert_eq!(core.state.limits.iter().find(|l| l.id == "seven_day").and_then(|l| l.used_percent), Some(20.0));
        // The /status rows, with addresses shortened.
        reply_to(
            &mut c,
            &mut core,
            &reqs,
            "get_status",
            json!({"sections": [{"title": "Session", "rows": [{"label": "Version", "value": "2.1.296"}]}, {"title": "Account", "rows": [{"label": "Email", "value": "someone@example.com"}, {"value": "✔ Signed in"}]}]}),
        );
        let st = &core.state.harness_status;
        assert_eq!(st.len(), 3);
        assert!(st.iter().any(|r| r.group == "Account" && r.value == "s…@example.com"));
        assert!(st.iter().any(|r| r.label.is_empty() && r.tone == Some(Tone::Ok)));
        assert!(!serde_json::to_string(&core.state).unwrap_or_default().contains("someone@"));
        // Info carries the limits for lists.
        let mut info: super::super::session::Info = serde_json::from_value(json!({"id": "a", "kind": "claude", "kind_name": "Claude Code", "name": "x", "cwd": "/", "created_at": 0, "status": "idle", "last_activity": 0}))?;
        info.apply(&core.state);
        assert_eq!((info.limits.len(), info.plan.as_deref()), (4, Some("Max")));
        Ok(())
    }

    #[test]
    fn an_api_key_session_has_no_plan_limits_and_a_status_without_rows_is_put_together() {
        let mut c = Claude::new(false);
        let mut core = Core::new();
        c.start(&mut core);
        let reqs = requests(&mut core);
        reply_to(&mut c, &mut core, &reqs, "initialize", json!({"commands": [], "account": {"tokenSource": "none", "apiKeySource": "ANTHROPIC_API_KEY", "apiProvider": "firstParty"}}));
        assert_eq!((core.state.plan.as_deref(), core.state.account.as_deref()), (Some("API key"), Some("claude:api-key")));
        let reqs = requests(&mut core);
        reply_to(&mut c, &mut core, &reqs, "get_usage", json!({"subscription_type": null, "rate_limits_available": false, "rate_limits": null, "behaviors": null}));
        assert!(core.state.limits.is_empty());
        let id = reqs.iter().find(|r| r.1 == "get_status").map(|r| r.0.clone()).unwrap_or_default();
        c.line(&mut core, &json!({"type": "control_response", "response": {"subtype": "error", "request_id": id, "error": "get_status is not available on this connection"}}).to_string());
        assert!(core.state.harness_status.iter().any(|r| r.label == "Plan" && r.value == "API key"));
    }

    #[test]
    fn mcp_servers_their_commands_and_the_cli() -> anyhow::Result<()> {
        let (mut c, mut core, reqs) = started();
        assert_eq!(core.state.mcp.first().map(|m| (m.name.as_str(), m.status)), Some(("echo", McpStatus::Pending)));
        reply_to(
            &mut c,
            &mut core,
            &reqs,
            "mcp_status",
            json!({"mcpServers": [
                {"name": "echo", "status": "connected", "serverInfo": {"name": "kiln-echo", "version": "1.0.0"}, "config": {"type": "stdio", "command": "python3", "args": ["srv.py", "--api-key", "sk-123", "TOKEN=abc"]}, "scope": "user", "tools": [{"name": "echo", "description": "Echo it"}]},
                {"name": "docs", "status": "needs-auth", "config": {"type": "http", "url": "https://user:pw@mcp.example.com/v1?key=1"}, "scope": "project"},
                {"name": "old", "status": "disabled", "config": {"type": "sse", "url": "https://sse.example.com/sse"}, "scope": "local"}
            ]}),
        );
        let m = &core.state.mcp;
        assert_eq!(m.len(), 3);
        let echo = m.first().cloned().ok_or_else(|| anyhow::anyhow!("missing"))?;
        assert_eq!((echo.status, echo.transport.as_str(), echo.scope.as_str(), echo.server.as_deref()), (McpStatus::Connected, "stdio", "user", Some("kiln-echo 1.0.0")));
        assert_eq!(echo.target.as_deref(), Some("python3 srv.py --api-key … TOKEN=…"));
        assert_eq!(echo.tools.first().map(|t| t.name.as_str()), Some("echo"));
        let docs = m.get(1).cloned().ok_or_else(|| anyhow::anyhow!("missing"))?;
        assert_eq!((docs.status, docs.target.as_deref(), docs.auth.as_deref()), (McpStatus::NeedsAuth, Some("https://mcp.example.com/v1"), Some("signed out")));
        assert!(!m.get(2).is_some_and(|o| o.enabled));
        // Switching one off: shown at once, sent as mcp_toggle, then asked for again.
        c.command(&mut core, Command::McpEnable { name: "echo".into(), enabled: false });
        assert_eq!(core.state.mcp.first().map(|m| m.status), Some(McpStatus::Disabled));
        let reqs = requests(&mut core);
        assert!(reqs.iter().any(|r| r.1 == "mcp_toggle" && r.2.get("serverName") == Some(&json!("echo")) && r.2.get("enabled") == Some(&json!(false))));
        reply_to(&mut c, &mut core, &reqs, "mcp_toggle", json!({}));
        // Asked for again (or already on its way, and asked once more after it).
        let asked = requests(&mut core).iter().any(|r| r.1 == "mcp_status");
        assert!(asked || (c.pending.values().any(|p| matches!(p, Pending::McpStatus)) && c.mcp_again));
        // Signing in: the page to open goes back to the client that asked; then its redirect.
        c.command(&mut core, Command::McpAuth { name: "docs".into(), client: "c1".into(), redirect: None });
        let reqs = requests(&mut core);
        reply_to(&mut c, &mut core, &reqs, "mcp_authenticate", json!({"authUrl": "https://auth.example.com/authorize?x=1", "requiresUserAction": true, "callbackExpected": true, "redirectScheme": "localhost", "callbackPort": 41234}));
        let r = replies(&mut core);
        assert_eq!(r.first().map(|(c, v)| (c.as_str(), v.get("url").and_then(Value::as_str), v.get("callback"))), Some(("c1", Some("https://auth.example.com/authorize?x=1"), Some(&json!(true)))));
        c.command(&mut core, Command::McpAuth { name: "docs".into(), client: "c2".into(), redirect: Some("http://localhost:41234/callback?code=abc&state=s".into()) });
        let reqs = requests(&mut core);
        assert!(reqs.iter().any(|r| r.1 == "mcp_oauth_callback_url" && r.2.get("callbackUrl") == Some(&json!("http://localhost:41234/callback?code=abc&state=s"))));
        reply_to(&mut c, &mut core, &reqs, "mcp_oauth_callback_url", json!({}));
        assert!(replies(&mut core).iter().any(|(c, v)| c == "c2" && v.get("ok") == Some(&json!(true))));
        assert!(requests(&mut core).iter().any(|r| r.1 == "mcp_reconnect" && r.2.get("serverName") == Some(&json!("docs"))));
        // Adding one goes through the CLI, then the CLI's configuration is reloaded.
        let mut env = std::collections::BTreeMap::new();
        env.insert("API_KEY".to_owned(), "x".to_owned());
        c.command(
            &mut core,
            Command::McpAdd { scope: "user".into(), name: "files".into(), transport: String::new(), command: Some("npx".into()), args: vec!["-y".into(), "srv".into()], env, url: None, headers: Default::default(), client: Some("c3".into()) },
        );
        let Some(Effect::Run { job, args }) = core.effects.pop() else { anyhow::bail!("no run") };
        assert_eq!(args, ["mcp", "add", "-s", "user", "-t", "stdio", "-e", "API_KEY=x", "--", "files", "npx", "-y", "srv"]);
        // In the middle of a turn: Claude Code starts again (to read its configuration) once it ends.
        c.line(&mut core, r#"{"type":"system","subtype":"session_state_changed","state":"running","uuid":"u","session_id":"s1"}"#);
        c.run_done(&mut core, &job, Some(0), "Added stdio MCP server files to user config");
        assert!(replies(&mut core).iter().any(|(c, v)| c == "c3" && v.get("ok") == Some(&json!(true))));
        assert!(core.effects.is_empty());
        c.line(&mut core, r#"{"type":"system","subtype":"session_state_changed","state":"idle","uuid":"u","session_id":"s1"}"#);
        assert_eq!(core.effects.pop(), Some(Effect::Restart { resume_at: None }));
        // A name the CLI would read as a flag never gets that far.
        c.command(&mut core, Command::McpRemove { scope: None, name: "-rf".into(), client: Some("c4".into()) });
        assert!(core.effects.is_empty());
        assert!(replies(&mut core).iter().any(|(c, v)| c == "c4" && v.get("ok") == Some(&json!(false))));
        c.command(&mut core, Command::McpRemove { scope: Some("user".into()), name: "files".into(), client: None });
        assert!(matches!(core.effects.pop(), Some(Effect::Run { args, .. }) if args == ["mcp", "remove", "-s", "user", "--", "files"]));
        // An http server with headers.
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("Authorization".to_owned(), "Bearer t".to_owned());
        c.command(
            &mut core,
            Command::McpAdd { scope: String::new(), name: "web".into(), transport: "http".into(), command: None, args: vec![], env: Default::default(), url: Some("https://mcp.example.com/mcp".into()), headers, client: None },
        );
        assert!(matches!(core.effects.pop(), Some(Effect::Run { args, .. }) if args == ["mcp", "add", "-t", "http", "-H", "Authorization: Bearer t", "--", "web", "https://mcp.example.com/mcp"]));
        Ok(())
    }

    #[test]
    fn skills_from_the_commands_init_and_the_skills_menu() -> anyhow::Result<()> {
        let (mut c, mut core, reqs) = started();
        reply_to(
            &mut c,
            &mut core,
            &reqs,
            "get_skills_dialog",
            json!({"skills": [
                {"name": "proj-skill", "display_name": "proj-skill", "description": "A project skill.", "source": "project", "tokens": 15, "state": "on", "advertised": true, "handles": {}},
                {"name": "kiln-test", "display_name": "kiln-test", "description": "A test skill.", "source": "user", "tokens": 26, "state": "on", "advertised": true, "handles": {}},
                {"name": "quiet", "display_name": "quiet", "description": "Switched off.", "source": "user", "tokens": 5, "state": "off", "advertised": false, "handles": {}}
            ]}),
        );
        let names: Vec<(&str, &str, bool, bool)> = core.state.skills.iter().map(|s| (s.name.as_str(), s.source.as_str(), s.user_invocable, s.enabled)).collect();
        assert_eq!(
            names,
            vec![
                ("kiln-test", "user", true, true),
                ("proj-skill", "project", true, true),
                ("dataviz", "built-in", true, true),
                ("tools:lint", "plugin", true, true),
                ("model-only", "user", false, true),
                ("quiet", "user", false, false),
            ]
        );
        assert_eq!(core.state.skills.first().and_then(|s| s.args_hint.as_deref()), Some("<word>"));
        assert_eq!(core.state.skills.first().map(|s| s.description.as_str()), Some("A test skill."));
        assert_eq!(core.state.plugins.first().map(|p| (p.name.as_str(), p.source.as_str(), p.skills.clone())), Some(("tools", "market", vec!["tools:lint".to_owned()])));
        // Viewing one reads it where it can be; a built-in one has no file.
        c.command(&mut core, Command::SkillView { name: "kiln-test".into(), client: "v1".into() });
        let Some(Effect::Read { client, paths }) = core.effects.pop() else { anyhow::bail!("no read") };
        assert_eq!(client, "v1");
        assert!(paths.iter().any(|p| p == "/work/app/.claude/skills/kiln-test/SKILL.md"));
        assert!(paths.iter().any(|p| p == "/work/.claude/skills/kiln-test/SKILL.md"));
        assert!(paths.iter().any(|p| p == "/home/u/.claude/skills/kiln-test/SKILL.md"));
        assert!(paths.iter().any(|p| p == "/home/u/.claude/commands/kiln-test.md"));
        c.command(&mut core, Command::SkillView { name: "tools:lint".into(), client: "v2".into() });
        let Some(Effect::Read { paths, .. }) = core.effects.pop() else { anyhow::bail!("no read") };
        assert_eq!(paths.first().map(String::as_str), Some("/plugins/tools/skills/lint/SKILL.md"));
        c.command(&mut core, Command::SkillView { name: "dataviz".into(), client: "v3".into() });
        c.command(&mut core, Command::SkillView { name: "../../etc/passwd".into(), client: "v4".into() });
        assert!(core.effects.is_empty());
        let r = replies(&mut core);
        assert!(r.iter().any(|(c, v)| c == "v3" && v.get("builtin") == Some(&json!(true))));
        assert!(r.iter().any(|(c, v)| c == "v4" && v.get("error").is_some()));
        // Running one is a message.
        c.command(&mut core, Command::SkillRun { name: "kiln-test".into(), args: Some("hello".into()), id: Some("m9".into()), mode: SendMode::Queue });
        let sent: Vec<Value> = core.take_writes().iter().filter_map(|w| serde_json::from_str(w).ok()).collect();
        assert!(sent.iter().any(|v| str_of(v, "type") == Some("user") && v.pointer("/message/content").and_then(Value::as_str) == Some("/kiln-test hello") && str_of(v, "uuid") == Some("m9")), "{sent:?}");
        // Reloading: the skills again, then the menu.
        c.command(&mut core, Command::SkillsReload);
        let reqs = requests(&mut core);
        reply_to(&mut c, &mut core, &reqs, "reload_skills", json!({"skills": [{"name": "fresh", "description": "New on disk. (user)", "argumentHint": ""}]}));
        assert!(core.state.skills.iter().any(|s| s.name == "fresh" && s.source == "user" && s.description == "New on disk."));
        assert!(requests(&mut core).iter().any(|r| r.1 == "get_skills_dialog"));
        // A pushed command list replaces the old one.
        c.line(&mut core, &json!({"type": "system", "subtype": "commands_changed", "commands": [{"name": "kiln-test", "description": "Changed. (user)", "argumentHint": ""}], "uuid": "u", "session_id": "s1"}).to_string());
        assert!(core.meta.commands.len() == 1 && core.state.skills.iter().any(|s| s.name == "kiln-test" && s.description == "A test skill."));
        Ok(())
    }

    #[test]
    fn mcp_and_skill_tool_calls_name_their_server_and_skill() -> anyhow::Result<()> {
        let mut c = Claude::new(false);
        let mut core = Core::new();
        c.line(&mut core, &json!({"type": "assistant", "message": {"id": "m", "type": "message", "role": "assistant", "model": "x", "content": [
            {"type": "tool_use", "id": "t1", "name": "mcp__echo-srv__shout", "input": {"text": "hi"}},
            {"type": "tool_use", "id": "t2", "name": "Skill", "input": {"skill": "kiln-test", "args": "x"}}
        ]}, "parent_tool_use_id": null, "session_id": "s", "uuid": "a"}).to_string());
        let t1 = tool(&core, "t1").cloned().ok_or_else(|| anyhow::anyhow!("missing"))?;
        assert_eq!((t1.server.as_deref(), t1.tool_short.as_deref(), t1.tool), (Some("echo-srv"), Some("shout"), ToolKind::Mcp));
        assert_eq!(tool(&core, "t2").and_then(|t| t.skill.clone()).as_deref(), Some("kiln-test"));
        Ok(())
    }

    #[test]
    fn server_names_and_command_lines_are_safe() {
        assert!(valid_server_name("echo-srv_2") && !valid_server_name("-x") && !valid_server_name("a.b") && !valid_server_name("") && !valid_server_name("a b"));
        assert_eq!(safe_command("srv", vec!["--token", "abc", "--port", "1", "PASSWORD=x", "with space"]), "srv --token … --port 1 PASSWORD=… 'with space'");
        assert_eq!(iso_ms("1970-01-01T00:00:01Z"), Some(1000));
        assert_eq!(iso_ms("not a time"), None);
        assert_eq!(split_source("Does a thing. (project)"), ("Does a thing.".to_owned(), Some("project".to_owned())));
        assert_eq!(split_source("Research (dynamic workflow)"), ("Research (dynamic workflow)".to_owned(), None));
        let bad = McpAdd { scope: String::new(), name: "x".into(), transport: "stdio".into(), command: None, args: vec![], env: Default::default(), url: None, headers: Default::default() };
        assert!(bad.claude_args().is_err());
        let ws = McpAdd { transport: "ws".into(), command: Some("x".into()), ..bad };
        assert!(ws.claude_args().is_err());
    }

    #[test]
    fn the_inventory_is_known_before_the_first_turn() -> anyhow::Result<()> {
        let mut c = Claude::new(false).with_config_dir("/home/u/.claude".into());
        let mut core = Core::new();
        core.state.cwd = "/work".into();
        c.start(&mut core);
        assert_eq!(core.state.inventory, super::super::model::Inventory::default());
        let reqs = requests(&mut core);
        reply_to(
            &mut c,
            &mut core,
            &reqs,
            "initialize",
            json!({"commands": [
                {"name": "compact", "description": "Compact the conversation", "argumentHint": "", "builtin": true},
                {"name": "dataviz", "description": "Charts.", "argumentHint": "", "builtin": true},
                {"name": "anthropic-skills:docs", "description": "Docs. (claude.ai)", "argumentHint": ""},
                {"name": "claude.ai Claude Docs:outline (MCP)", "description": "Outline a doc", "argumentHint": ""}
            ], "account": {"subscriptionType": "max", "email": "a@example.com"}}),
        );
        let reqs = requests(&mut core);
        // No init yet (it comes with the first turn): the skill commands say which are skills.
        reply_to(&mut c, &mut core, &reqs, "reload_skills", json!({"skills": [{"name": "dataviz", "description": "Charts.", "argumentHint": "", "builtin": true}, {"name": "anthropic-skills:docs", "description": "Docs. (claude.ai)", "argumentHint": ""}]}));
        let names: Vec<(&str, &str)> = core.state.skills.iter().map(|s| (s.name.as_str(), s.source.as_str())).collect();
        assert_eq!(names, vec![("dataviz", "built-in"), ("anthropic-skills:docs", "claude.ai"), ("claude.ai Claude Docs:outline (MCP)", "mcp")]);
        assert_eq!(core.state.inventory.skills, Known::Ready);
        // Plugins from the settings until init lists the ones that loaded.
        reply_to(&mut c, &mut core, &reqs, "get_settings", json!({"effective": {"enabledPlugins": {"rust-analyzer-lsp@claude-plugins-official": true, "old@market": false}}, "applied": {}}));
        let pl: Vec<(&str, &str, bool)> = core.state.plugins.iter().map(|p| (p.name.as_str(), p.source.as_str(), p.enabled)).collect();
        assert_eq!(pl, vec![("old", "market", false), ("rust-analyzer-lsp", "claude-plugins-official", true)]);
        assert_eq!(core.state.inventory.plugins, Known::Ready);
        // No servers at first may only mean not yet: known once it stays so.
        reply_to(&mut c, &mut core, &reqs, "mcp_status", json!({"mcpServers": []}));
        assert_eq!(core.state.inventory.mcp, Known::Loading);
        c.started_at -= 10_000;
        c.mcp_at -= 10_000;
        c.tick(&mut core);
        let reqs = requests(&mut core);
        reply_to(
            &mut c,
            &mut core,
            &reqs,
            "mcp_status",
            json!({"mcpServers": [{"name": "claude.ai Claude Docs", "status": "connected", "config": {"type": "claudeai-proxy", "url": "https://api.anthropic.com/v1/pages/mcp", "id": "x"}, "scope": "claudeai", "tools": [{"name": "guide"}]}]}),
        );
        assert_eq!(core.state.inventory.mcp, Known::Ready);
        let docs = core.state.mcp.first().cloned().ok_or_else(|| anyhow::anyhow!("missing"))?;
        assert_eq!((docs.transport.as_str(), docs.scope.as_str(), docs.target.as_deref(), docs.prompts), ("claudeai-proxy", "claudeai", Some("https://api.anthropic.com/v1/pages/mcp"), Some(1)));
        // A synced skill is read where claude.ai synced it; an MCP prompt has no file.
        c.command(&mut core, Command::SkillView { name: "anthropic-skills:docs".into(), client: "v".into() });
        let Some(Effect::Read { paths, .. }) = core.effects.pop() else { anyhow::bail!("no read") };
        assert_eq!(paths.first().map(String::as_str), Some("/home/u/.claude/skills/synced/*/docs/SKILL.md"));
        c.command(&mut core, Command::SkillView { name: "claude.ai Claude Docs:outline (MCP)".into(), client: "m".into() });
        assert!(core.effects.is_empty());
        // A claude.ai connector is not the CLI's to remove.
        c.command(&mut core, Command::McpRemove { scope: None, name: "claude.ai Claude Docs".into(), client: Some("r".into()) });
        assert!(core.effects.is_empty());
        let r = replies(&mut core);
        assert!(r.iter().any(|(c, v)| c == "m" && v.get("source") == Some(&json!("mcp"))));
        assert!(r.iter().any(|(c, v)| c == "r" && v.get("error").and_then(Value::as_str).is_some_and(|e| e.contains("claude.ai"))));
        // Init (the first turn) adds a server mcp_status had not shown yet, and replaces the plugins.
        c.line(
            &mut core,
            &json!({"type": "system", "subtype": "init", "cwd": "/work", "session_id": "s1", "model": "m", "permissionMode": "default",
                "skills": ["dataviz", "anthropic-skills:docs"], "plugins": [{"name": "rust-analyzer-lsp", "path": "/p", "source": "rust-analyzer-lsp@claude-plugins-official"}],
                "mcp_servers": [{"name": "claude.ai Claude Docs", "status": "connected", "source": "claudeai"}, {"name": "late", "status": "pending", "source": "user"}]})
            .to_string(),
        );
        assert_eq!(core.state.mcp.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), vec!["claude.ai Claude Docs", "late"]);
        assert!(requests(&mut core).iter().any(|r| r.1 == "mcp_status"));
        assert_eq!(core.state.plugins.iter().map(|p| (p.name.as_str(), p.enabled)).collect::<Vec<_>>(), vec![("rust-analyzer-lsp", true)]);
        // Modes are there from the start (the console's picker needs them).
        assert!(core.meta.modes.iter().any(|m| m.id == "plan"));
        Ok(())
    }

    #[test]
    fn a_mode_change_mid_session() {
        let mut c = Claude::new(false);
        let mut core = Core::new();
        c.start(&mut core);
        let ids: Vec<&str> = core.meta.modes.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"default") && ids.contains(&"plan") && ids.contains(&"acceptEdits") && !ids.contains(&"bypassPermissions"), "{ids:?}");
        let _ = requests(&mut core);
        c.command(&mut core, Command::SetMode { mode: "plan".into() });
        let reqs = requests(&mut core);
        assert!(reqs.iter().any(|r| r.1 == "set_permission_mode" && r.2.get("mode") == Some(&json!("plan"))));
        reply_to(&mut c, &mut core, &reqs, "set_permission_mode", json!({}));
        assert_eq!(core.state.mode, "plan");
        // The CLI's own word wins (it may refuse or settle on another).
        c.command(&mut core, Command::SetMode { mode: "acceptEdits".into() });
        let reqs = requests(&mut core);
        reply_to(&mut c, &mut core, &reqs, "set_permission_mode", json!({"mode": "default"}));
        assert_eq!(core.state.mode, "default");
    }
}
