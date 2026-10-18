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
    McpTool, ModeOption, ModelOption, Objective, Plugin, Question, QuestionOption, QueueItem, SendMode, Skill, SlashCommand, Status, StatusItem, Task, Todo, Tone, ToolCall, ToolKind, ToolStatus,
    ChoiceAnswer, OUTPUT_MAX, Route, account_id, choices_of, clip_json, clip_middle, mask_emails, plan_label, safe_url, sort_catalog,
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
    if allow_bypass && bypass_possible() {
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
    /// A request whose answer is shown to one client as `{title, text}` (what it was for).
    Info { client: String, what: &'static str },
    /// reload_plugins: its commands, plugins and servers.
    Plugins,
}

/// A run of the CLI kiln is waiting on.
#[derive(Debug, Clone)]
enum Run {
    McpAdd { name: String, client: Option<String> },
    McpRemove { name: String, client: Option<String> },
    /// The transcript's end, read for how the goal the turn pursued came out.
    GoalVerdict,
}

/// How often plan usage and status are asked for while a session runs (also after each turn,
/// and when the CLI reports a rate-limit change).
const USAGE_EVERY_MS: i64 = 180_000;

/// The effort levels Claude Code knows (its settings schema), offered until initialize says which
/// the model in use has, and for a model it does not list.
pub(super) const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// A permission request we are holding for a person: the input exactly as the CLI sent it (the
/// approval's copy is shortened for display; the answer must carry the original).
#[derive(Debug, Clone)]
struct Ask {
    input: Value,
    /// An MCP server's request for input (MCP elicitation), not a tool's.
    elicitation: bool,
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
    /// Bypass was chosen, which it was not started able to use: it starts again able to.
    bypass_next: bool,
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
    /// Commands bound to the terminal the CLI runs in (system/init; None before it).
    terminal_cmds: Option<Vec<String>>,
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
    /// This turn ended because Claude Code's cap on a Stop hook blocking it was reached (a goal
    /// it kept not meeting).
    goal_capped: bool,
    next_run: u64,
    /// The MCP configuration changed (what changed): Claude Code reads it when it starts, so it
    /// starts again, continuing the conversation, as soon as it is not in the middle of a turn.
    reload: Option<String>,
    /// That restart waits for tasks left running (said once).
    reload_waits: bool,
    /// Where a background command's output goes, by the tool call that started it, until its
    /// task is known: (task id, file).
    bg_files: HashMap<String, (String, String)>,
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
            bypass_next: false,
            entries: Vec::new(),
            efforts: HashMap::new(),
            output_styles: Vec::new(),
            config_dir: String::new(),
            commands: Vec::new(),
            skill_names: Vec::new(),
            skill_cmds: Vec::new(),
            terminal_cmds: None,
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
            goal_capped: false,
            next_run: 0,
            reload: None,
            reload_waits: false,
            bg_files: HashMap::new(),
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
        // Effort levels of the model in use (by its id, an alias in it, or the default model's),
        // else the ones the CLI knows.
        core.meta.efforts = self.effort_levels(&core.state.model).iter().map(|e| opt(e, &capitalize(e), "")).collect();
        core.touch_meta();
    }

    /// The effort levels of `model` as initialize listed them: by its id or alias, an alias it
    /// contains ("opus[1m]"), the default model's; the CLI's known levels when none matches.
    fn effort_levels(&self, model: &str) -> Vec<String> {
        let mut keys: Vec<&String> = self.efforts.keys().filter(|k| !k.is_empty() && model.contains(k.as_str())).collect();
        // The longest match is the most specific ("claude-opus-5-5" over "opus").
        keys.sort_by_key(|k| std::cmp::Reverse(k.len()));
        self.efforts
            .get(model)
            .or_else(|| keys.first().and_then(|k| self.efforts.get(*k)))
            .or_else(|| self.efforts.get("default"))
            .cloned()
            .unwrap_or_else(|| EFFORTS.iter().map(|e| (*e).to_owned()).collect())
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
            // Settings changed outside it (its own screen, beside it): start again to load them.
            "reload_config" => {
                let what = value.as_ref().and_then(|v| v.get("what")).and_then(Value::as_str).unwrap_or("Changed settings").to_owned();
                self.reload = Some(what);
                self.reload_when_idle(core);
            }
            "fork" => match core.state.harness_session.clone() {
                Some(session) => core.effects.push(Effect::Branch { resume: session, resume_at: None }),
                None => self.notice(core, Level::Info, "Nothing to branch yet: the conversation starts with the first message.", None),
            },
            // Requests the catalog runs: their answer is shown to the client that asked.
            "side_question" | "session_cost" | "export" | "plan" | "permission_rules" | "memory" | "workspace_diff" => {
                let client = value.as_ref().and_then(|v| v.get("client")).and_then(Value::as_str).unwrap_or("").to_owned();
                let (what, body): (&'static str, Value) = match id {
                    "side_question" => ("side_question", json!({"subtype": "side_question", "question": text()})),
                    "session_cost" => ("session_cost", json!({"subtype": "get_session_cost"})),
                    "export" => ("export", json!({"subtype": "export_conversation"})),
                    "plan" => ("plan", json!({"subtype": "get_plan"})),
                    "permission_rules" => ("permission_rules", json!({"subtype": "list_permission_rules"})),
                    "memory" => ("memory", json!({"subtype": "get_memory_dialog"})),
                    _ => ("workspace_diff", json!({"subtype": "get_workspace_diff"})),
                };
                if what == "side_question" && text().trim().is_empty() {
                    core.reply(client, json!({"error": "Ask something after /btw."}));
                    return;
                }
                self.request(core, Pending::Info { client, what }, body);
            }
            "reload_plugins" => self.request(core, Pending::Plugins, json!({"subtype": "reload_plugins"})),
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
        // Effort can be set from the start (initialize narrows the levels to the model's).
        core.meta.efforts = EFFORTS.iter().map(|e| ControlOption { id: (*e).to_owned(), label: capitalize(e), description: String::new(), dangerous: false }).collect();
        // kiln's commands until the CLI lists its own.
        if core.meta.commands.is_empty() {
            core.meta.commands = sort_catalog(claude_catalog());
        }
        core.meta.features = ["steer", "send_now", "queue", "queue_steer", "interrupt", "set_mode", "set_model", "set_effort", "stop_task", "file_suggestions", "resume", "attachments", "images", "switch_harness"]
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
        // What it ran ends with it.
        for t in core.state.tasks.iter_mut().filter(|t| t.live()) {
            t.status = "stopped".into();
            t.ended.get_or_insert_with(Core::now);
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
            // Written now, without priority: the CLI folds it into the running turn (or starts
            // one with it when none runs).
            Command::QueueSteer { id } => {
                if let Some(pos) = core.state.queue.iter().position(|q| q.id == id) {
                    let item = core.state.queue.remove(pos);
                    core.touch_state();
                    self.send(core, item.id, item.text, SendMode::Steer, item.attachments, item.overrides);
                }
            }
            Command::Approve { id, decision, message, always, answers, choices } => self.answer(core, &id, decision, message, always, choices_of(answers, choices)),
            Command::Interrupt => {
                if self.busy(core) {
                    self.request(core, Pending::Interrupt, json!({"subtype": "interrupt"}));
                }
            }
            Command::SetMode { mode } => {
                if mode == "bypassPermissions" && !self.allow_bypass {
                    // Claude Code refuses it unless started able to: it starts again, in it,
                    // continuing the conversation (once the turn under way ends).
                    self.bypass_next = true;
                    self.reload = Some("Bypass".to_owned());
                    if self.busy(core) {
                        self.notice(core, Level::Info, "Bypass comes on when this turn ends: Claude Code starts again with every permission check off (the conversation continues)".to_owned(), None);
                    }
                    self.reload_when_idle(core);
                } else {
                    self.request(core, Pending::SetMode(mode.clone()), json!({"subtype": "set_permission_mode", "mode": mode}));
                }
            }
            Command::SetModel { model, .. } => {
                self.request(core, Pending::SetModel(model.clone()), json!({"subtype": "set_model", "model": model}));
            }
            Command::StopTask { task } => {
                self.request(core, Pending::StopTask, json!({"subtype": "stop_task", "task_id": task}));
            }
            Command::StopTasks => {
                let live: Vec<String> = core.state.tasks.iter().filter(|t| t.live()).map(|t| t.id.clone()).collect();
                for task in live {
                    self.request(core, Pending::StopTask, json!({"subtype": "stop_task", "task_id": task}));
                }
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
            // The session itself does these (a move is not the harness's business).
            Command::Restart | Command::Snapshot | Command::MoveTo { .. } | Command::Moving { .. } => {}
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
            Run::GoalVerdict => {
                if code == Some(0) {
                    self.goal_verdict(core, output);
                }
                return;
            }
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

    /// /goal as Claude Code ran it, by what it said: "Goal set: …" (it works until a Stop hook
    /// finds the condition met) or "Goal cleared: …". Anything else (a bare /goal's answer, a
    /// refusal) changes nothing.
    fn goal_command(&mut self, core: &mut Core, args: &str, said: &str) {
        let said = strip_ansi(said);
        let said = said.trim();
        let next = if said.starts_with("Goal set") && !args.trim().is_empty() {
            self.goal_capped = false;
            Objective::new(args, "active", None)
        } else if let Some(rest) = said.strip_prefix("Goal cleared") {
            let text = rest.trim_start_matches(':').trim();
            let text = if text.is_empty() { core.state.objective.as_ref().map(|o| o.text.clone()).unwrap_or_default() } else { text.to_owned() };
            Objective::new(&text, "cleared", None)
        } else {
            return;
        };
        core.state.objective = Some(next);
        core.touch_state();
    }

    /// How the goal the turn pursued came out, from the verdicts Claude Code writes to its
    /// transcript (`goal_status`): met, can't be met, cleared; or still set (it gave up for now).
    fn goal_verdict(&mut self, core: &mut Core, tail: &str) {
        let Some(o) = core.state.objective.clone().filter(|o| o.status == "active") else { return };
        let flag = |a: &Value, k: &str| a.get(k).and_then(Value::as_bool) == Some(true);
        // This goal's records, and of them the ones since it was last set (a sentinel, not met).
        let records: Vec<Value> = tail
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|r| {
                let a = r.get("attachment").filter(|a| str_of(a, "type") == Some("goal_status"))?;
                str_of(a, "condition").is_some_and(|c| c.split_whitespace().eq(o.text.split_whitespace())).then(|| a.clone())
            })
            .collect();
        let set = records.iter().rposition(|a| flag(a, "sentinel") && !flag(a, "met")).map_or(0, |i| i + 1);
        let since = records.get(set..).unwrap_or(&[]);
        let reason = |a: &Value| str_of(a, "reason").map(str::to_owned);
        // Met, whatever came after (a met goal clears itself): met. Else the last word decides.
        let next = match since.iter().rev().find(|a| flag(a, "met") && !flag(a, "sentinel")) {
            Some(a) => Objective::new(&o.text, "met", reason(a)),
            None => match since.last() {
                Some(a) if flag(a, "met") && flag(a, "sentinel") => Objective::new(&o.text, "cleared", None),
                Some(a) if flag(a, "failed") => Objective::new(&o.text, "unmet", Some(reason(a).unwrap_or_else(|| "Claude Code found it can't be met.".to_owned()))),
                Some(a) if self.goal_capped => {
                    let why = reason(a).map(|r| format!(" The last check: {r}")).unwrap_or_default();
                    Objective::new(&o.text, "unmet", Some(format!("Claude Code stopped after its checks kept finding it unmet; the goal is still set.{why}")))
                }
                // Set and not yet decided: the turn was cut short; it carries on with the next.
                _ => return,
            },
        };
        self.goal_capped = false;
        core.state.objective = Some(next);
        core.touch_state();
    }

    /// Start again with the changed MCP configuration (resuming the conversation), unless a turn
    /// is under way: then when it ends.
    fn reload_when_idle(&mut self, core: &mut Core) {
        if self.reload.is_none() || self.busy(core) || core.state.status == Status::Starting {
            return;
        }
        // Starting again would end the shells and monitors it left running: it waits for them.
        let left = core.state.tasks.iter().filter(|t| t.left_running()).count();
        if left > 0 && core.state.status != Status::Exited {
            if !self.reload_waits {
                self.reload_waits = true;
                let what = self.reload.clone().unwrap_or_default();
                self.notice(core, Level::Info, format!("{what}: Claude Code loads it when it starts again, once the {left} task(s) it left running end (or restart it yourself)"), None);
            }
            return;
        }
        self.reload_waits = false;
        let Some(what) = self.reload.take() else { return };
        // Bypass: from the next start it may skip checks, and starts in that mode.
        let bypass = std::mem::take(&mut self.bypass_next);
        if bypass {
            core.effects.push(Effect::AllowBypass);
            core.state.mode = "bypassPermissions".to_owned();
            core.touch_state();
        }
        // Not running, or nothing to resume: its next start reads the configuration anyway.
        if core.state.status == Status::Exited || (core.state.harness_session.is_none() && !bypass) {
            return;
        }
        let why = if bypass { "Bypass: Claude Code starts again with every permission check off (the conversation continues)".to_owned() } else { format!("{what}: Claude Code starts again to load it (the conversation continues)") };
        self.notice(core, Level::Info, why, None);
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
        self.rebuild_skills(core);
        self.refresh_catalog(core);
    }

    fn refresh_catalog(&self, core: &mut Core) {
        let catalog = self.catalog(core);
        if core.meta.commands != catalog {
            core.meta.commands = catalog;
            core.touch_meta();
        }
    }

    /// The command catalog: the CLI's own commands (sent as they are, or done by kiln's panels and
    /// controls where kiln does them itself), its skills, plugins' and MCP servers' commands, and
    /// the commands of Claude Code's terminal interface kiln does itself (/btw, /cost, /export ...)
    /// or cannot do (with why).
    fn catalog(&self, core: &Core) -> Vec<SlashCommand> {
        let table = claude_catalog();
        // Bound to the CLI's terminal: init says which; until it comes (with the first turn), the
        // ones 2.1.296 names.
        let terminal: Vec<&str> = match &self.terminal_cmds {
            Some(t) => t.iter().map(String::as_str).collect(),
            None => TERMINAL_COMMANDS.to_vec(),
        };
        // Rows can share a name: /name runs the one marked builtin.
        let marked: std::collections::HashSet<&str> = self.commands.iter().filter(|c| c.get("builtin").and_then(Value::as_bool) == Some(true)).filter_map(|c| str_of(c, "name")).collect();
        let mut out = Vec::new();
        for c in &self.commands {
            let Some(name) = str_of(c, "name") else { continue };
            let builtin = c.get("builtin").and_then(Value::as_bool) == Some(true);
            if name.starts_with("__") || (!builtin && marked.contains(name)) {
                continue;
            }
            let (desc, source) = split_source(str_of(c, "description").unwrap_or(""));
            let desc = clip_middle(&desc, 300).0;
            let hint = str_of(c, "argumentHint").unwrap_or("");
            // Claude Code's own (marked so; or, from CLIs that do not mark them, not a user's,
            // project's or plugin's).
            let own = builtin || (source.is_none() && !name.contains(':') && !core.state.skills.iter().any(|s| s.name == name && s.source != "built-in"));
            let mut entry = if let Some(k) = table.iter().find(|k| k.name == name).filter(|_| own) {
                // kiln's way of running it, in the CLI's words.
                let mut k = k.clone();
                if !desc.is_empty() {
                    k.description.clone_from(&desc);
                }
                k
            } else if terminal.contains(&name) {
                SlashCommand::terminal(name, &desc, "Session")
            } else if name.ends_with(" (MCP)") {
                SlashCommand::prompt(name, &desc, hint, "Tools & MCP")
            } else if core.state.skills.iter().any(|s| s.name == name) {
                let group = if name.contains(':') && !name.starts_with("anthropic-skills:") { "Plugins" } else { "Skills" };
                SlashCommand::new(name, &desc, hint, group, Route::Prompt { text: None, skill: true })
            } else if name.contains(':') {
                SlashCommand::prompt(name, &desc, hint, "Plugins")
            } else if builtin {
                SlashCommand::prompt(name, &desc, hint, claude_group(name))
            } else {
                SlashCommand::prompt(name, &desc, hint, "Skills")
            };
            let aliases: Vec<&str> = c.get("aliases").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
            if !aliases.is_empty() {
                entry = entry.aka(&aliases);
            }
            out.push(entry);
        }
        // Those the CLI does not list (it lists what works without its terminal interface).
        out.extend(table);
        sort_catalog(out)
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
            // (Older CLIs do not mark their own commands: those kiln knows count as such.)
            let cli_own = builtin || (claude_catalog().iter().any(|k| k.name == name) && split_source(str_of(c, "description").unwrap_or("")).1.is_none());
            if cli_own && r.is_none() && !known(name) {
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
            self.refresh_catalog(core);
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
        if core.hold || self.busy(core) || core.state.queue.is_empty() || core.state.status == Status::Exited {
            return;
        }
        let item = core.state.queue.remove(0);
        core.touch_state();
        self.send(core, item.id, item.text, SendMode::Queue, item.attachments, item.overrides);
    }

    fn answer(&mut self, core: &mut Core, id: &str, decision: Decision, message: Option<String>, always: Option<usize>, choices: Option<Vec<ChoiceAnswer>>) {
        let Some(approval) = core.take_approval(id) else { return };
        let ask = self.asks.remove(id);
        let mut input = ask.as_ref().map(|a| a.input.clone()).unwrap_or_else(|| approval.input.clone());
        if ask.as_ref().is_some_and(|a| a.elicitation) {
            let (response, why) = super::elicit::answer(input.get("requested_schema"), decision != Decision::Deny, choices.as_deref().unwrap_or_default());
            if let Some(why) = why {
                self.notice(core, Level::Warn, format!("Declined it for you, as the answers didn't fit: {why}"), None);
            }
            core.write(&json!({"type": "control_response", "response": {"subtype": "success", "request_id": id, "response": response}}));
            self.settle(core);
            return;
        }
        let response = match decision {
            Decision::Allow | Decision::Always => {
                // Answers as AskUserQuestion takes them: question → label(s), several joined
                // with ", "; notes (and the chosen option's preview) as annotations.
                let answered = choices.map(|c| claude_answers(&c, &approval.questions));
                if let (Some((answers, annotations)), Some(o)) = (answered, input.as_object_mut()) {
                    o.insert("answers".into(), answers);
                    if !annotations.is_empty() {
                        o.insert("annotations".into(), Value::Object(annotations));
                    }
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
                Pending::Suggest(client) | Pending::Reply(client) | Pending::McpAuth { client, .. } | Pending::McpCallback { client, .. } | Pending::Info { client, .. } => core.reply(client, json!({"error": err})),
                Pending::Plugins => self.notice(core, Level::Warn, format!("Reloading plugins: {err}"), None),
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
                                efforts: m.get("supportedEffortLevels").and_then(Value::as_array).map(|l| l.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default(),
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
            Pending::Info { client, what } => core.reply(client, info_reply(what, &body)),
            Pending::Plugins => {
                if let Some(list) = body.get("mcpServers").and_then(Value::as_array) {
                    core.state.mcp = list.iter().filter_map(claude_server).collect();
                    core.touch_state();
                }
                if let Some(pl) = body.get("plugins").and_then(Value::as_array) {
                    self.plugins = plugins_of(pl);
                    self.plugins_from_init = true;
                }
                if let Some(cmds) = body.get("commands").and_then(Value::as_array) {
                    self.set_commands(core, cmds);
                }
                self.ask_mcp(core);
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
            "elicitation" => self.elicitation(core, id, &req),
            other => core.write(&json!({"type": "control_response", "response": {"subtype": "error", "request_id": id, "error": format!("kiln does not handle {other}")}})),
        }
    }

    /// An MCP server asks the person for input (MCP elicitation): asked as questions, like any.
    fn elicitation(&mut self, core: &mut Core, id: String, req: &Value) {
        let server = str_of(req, "display_name").or_else(|| str_of(req, "mcp_server_name")).unwrap_or("");
        let a = super::elicit::ask(server, str_of(req, "message").unwrap_or(""), str_of(req, "mode"), req.get("requested_schema"), str_of(req, "url"));
        let approval = Approval {
            id: id.clone(),
            tool_id: None,
            tool_name: "elicitation".to_owned(),
            tool: ToolKind::Question,
            title: a.title,
            detail: a.detail,
            input: req.clone(),
            command: None,
            cwd: core.state.cwd.clone(),
            diffs: Vec::new(),
            reason: None,
            always: Vec::new(),
            default_to_no: false,
            questions: a.questions,
            plan: None,
            agent: None,
            at: Core::now(),
        };
        core.state.doing = Some("Asking you a question".to_owned());
        core.touch_state();
        self.asks.insert(id, Ask { input: req.clone(), elicitation: true });
        core.add_approval(approval);
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
            self.queue_behind(core, tid);
        }
        core.state.doing = Some(match tool {
            ToolKind::Question => "Asking you a question".to_owned(),
            ToolKind::Plan => "Waiting for you to review a plan".to_owned(),
            _ => "Waiting for your approval".to_owned(),
        });
        self.asks.insert(id, Ask { input, elicitation: false });
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
                // (Absent when there are none, and from CLIs that predate it.)
                self.terminal_cmds = Some(v.get("terminal_slash_commands").and_then(Value::as_array).into_iter().flatten().filter_map(|x| x.as_str().map(str::to_owned)).collect());
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
                // Commands bound to the CLI's terminal are known now.
                self.refresh_catalog(core);
                core.touch_state();
                // The model in use is known now: its effort levels.
                self.refresh_controls(core);
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
                if str_of(v, "key") == Some("stop-hook-block-cap") {
                    self.goal_capped = true;
                }
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
                let tool_id = str_of(v, "tool_use_id").map(str::to_owned);
                let (tool_name, tool_input) = tool_id.as_deref().and_then(|t| self.tools.get(t).cloned()).unwrap_or_default();
                let class = task_class(str_of(v, "task_type"), &tool_name, str_of(v, "subagent_type").is_some());
                let output = tool_id.as_deref().and_then(|t| self.bg_files.remove(t)).map(|(_, f)| f);
                let mut task = Task {
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
                task.command = task_command(&tool_name, &tool_input);
                task.cwd = matches!(class.as_deref(), Some("shell" | "monitor")).then(|| core.state.cwd.clone());
                task.wakes = (class.as_deref() == Some("monitor")).then(|| task.parent_task.clone().unwrap_or_else(|| "agent".to_owned()));
                task.class = class;
                task.output = output;
                task.stoppable = true;
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
            // Every live background task (replace semantics): ones kiln missed (before a restart,
            // or a dropped line) join.
            "background_tasks_changed" => {
                for b in v.get("tasks").and_then(Value::as_array).into_iter().flatten() {
                    let Some(id) = str_of(b, "task_id") else { continue };
                    match core.state.tasks.iter_mut().find(|t| t.id == id) {
                        Some(t) => t.background = true,
                        None => {
                            let class = task_class(str_of(b, "task_type"), "", str_of(b, "subagent_type").is_some());
                            core.state.tasks.push(Task {
                                id: id.to_owned(),
                                description: str_of(b, "description").unwrap_or("").to_owned(),
                                kind: str_of(b, "subagent_type").or_else(|| str_of(b, "task_type")).unwrap_or("task").to_owned(),
                                status: "running".into(),
                                background: true,
                                parent_task: str_of(b, "parent_task_id").map(str::to_owned),
                                started: Core::now(),
                                wakes: (class.as_deref() == Some("monitor")).then(|| str_of(b, "parent_task_id").unwrap_or("agent").to_owned()),
                                class,
                                stoppable: true,
                                ..Task::default()
                            });
                        }
                    }
                }
                prune_tasks(core);
                core.touch_state();
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
                    if let Some(f) = str_of(v, "output_file").filter(|f| !f.is_empty()) {
                        t.output = Some(f.to_owned());
                    }
                    if let Some(s) = str_of(v, "summary") {
                        t.exit_code = t.exit_code.or_else(|| exit_code_in(s));
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
        if parent.is_none()
            && let Some(run) = v.get("local_command_run")
            && str_of(run, "command") == Some("goal")
        {
            let said = v.pointer("/message/content/0/text").and_then(Value::as_str).unwrap_or("");
            self.goal_command(core, str_of(run, "args").unwrap_or(""), said);
        }
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
                    // Asked about, or waiting behind a call that is (calls that change things run one
                    // at a time; read-only ones run alongside).
                    let asked = core.state.approvals.iter().any(|a| a.tool_id.as_deref() == Some(tid.as_str()));
                    let behind = !alongside(tool) && core.state.approvals.iter().any(|a| a.tool_id.is_some() && a.agent.is_none());
                    let status = if asked || behind { ToolStatus::Pending } else { ToolStatus::Running };
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
        // Context the CLI injects (reminders, command wrappers) is not something anyone said; a
        // monitor's event in it counts for its task.
        if v.get("isSynthetic").and_then(Value::as_bool) == Some(true) || (t.starts_with('<') && !t.starts_with("<task-notification")) {
            if t.contains("<task-notification>") {
                self.monitor_event(core, t);
            }
            return;
        }
        if t.starts_with("<task-notification") {
            self.monitor_event(core, t);
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

    /// A `<task-notification>` (as the CLI tells the model): for a monitor, one more event.
    fn monitor_event(&mut self, core: &mut Core, text: &str) {
        for part in text.split("<task-notification>").skip(1) {
            let Some(id) = between(part, "<task-id>", "</task-id>") else { continue };
            let Some(t) = core.state.tasks.iter_mut().find(|t| t.id == id.trim() && t.class.as_deref() == Some("monitor")) else { continue };
            let what = between(part, "<event>", "</event>").or_else(|| between(part, "<summary>", "</summary>")).unwrap_or("").trim();
            t.events += 1;
            t.last_event = Some(clip_middle(what.lines().last().unwrap_or(""), 300).0).filter(|e| !e.is_empty());
            core.touch_state();
        }
    }

    fn tool_result(&mut self, core: &mut Core, tid: &str, block: &Value, structured: &Value) {
        let is_error = block.get("is_error").and_then(Value::as_bool).unwrap_or(false);
        let mut text = content_text(block.get("content"));
        let (name, input) = self.tools.get(tid).cloned().unwrap_or_default();
        // A command now running in the background: its task's output file.
        if let Some((task, file)) = background_of(&text, structured) {
            match core.state.tasks.iter_mut().find(|t| t.id == task || t.tool_id.as_deref() == Some(tid)) {
                Some(t) => {
                    t.output = Some(file);
                    core.touch_state();
                }
                None => {
                    self.bg_files.insert(tid.to_owned(), (task, file));
                }
            }
        }
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
        // A command run on another runner (kiln reach): its output and exit code, not the tool's JSON.
        let reach = crate::reach::shown(&name, &Value::String(text.clone()));
        if let Some(r) = &reach {
            text.clone_from(&r.output);
            exit_code = r.exit_code.or(exit_code);
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
            if let Some(runner) = reach.and_then(|r| r.runner) {
                t.detail = Some(format!("on {runner}"));
            }
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
        let parent = core.get(tid).and_then(|e| e.parent.clone());
        self.next_in_line(core, parent.as_deref());
        if self.tools.is_empty() && self.turn && core.state.approvals.is_empty() {
            core.state.doing = Some("Thinking".to_owned());
            core.touch_state();
        }
    }

    /// A permission prompt for `asking`: the calls after it (in the same agent) that change things
    /// wait their turn (the CLI runs those one at a time), so they read as pending, not running.
    fn queue_behind(&self, core: &mut Core, asking: &str) {
        let parent = core.get(asking).and_then(|e| e.parent.clone());
        let mut after = false;
        let waiting: Vec<String> = core
            .events()
            .filter_map(|e| match &e.body {
                Body::Tool(t) if self.tools.contains_key(&e.id) && e.parent == parent => {
                    if e.id == asking {
                        after = true;
                        return None;
                    }
                    (after && t.status == ToolStatus::Running && !alongside(t.tool)).then(|| e.id.clone())
                }
                _ => None,
            })
            .collect();
        for id in waiting {
            set_tool_status(core, &id, ToolStatus::Pending);
        }
    }

    /// A call ended: in its agent, the first one waiting its turn (pending, not asked about) is
    /// next. If it needs permission, its prompt follows and it reads as pending again.
    fn next_in_line(&self, core: &mut Core, parent: Option<&str>) {
        let asked = |id: &str| core.state.approvals.iter().any(|a| a.tool_id.as_deref() == Some(id));
        let mut next = None;
        for e in core.events().filter(|e| e.parent.as_deref() == parent && self.tools.contains_key(&e.id)) {
            if let Body::Tool(t) = &e.body
                && t.status == ToolStatus::Pending
            {
                // Still asked about: the line has not moved.
                if asked(&e.id) {
                    return;
                }
                next = Some(e.id.clone());
                break;
            }
        }
        if let Some(id) = next {
            set_tool_status(core, &id, ToolStatus::Running);
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
        // A goal it pursued: how it came out is in the transcript.
        if core.state.objective.as_ref().is_some_and(|o| o.status == "active")
            && let Some(session) = core.state.harness_session.clone()
        {
            let job = format!("goal-verdict-{}", Core::now());
            self.runs.insert(job.clone(), Run::GoalVerdict);
            core.effects.push(Effect::TranscriptTail { job, config: self.config_dir.clone(), session, bytes: 512 * 1024 });
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

/// Claude Code's commands kiln runs itself (with a panel or a control) instead of sending them as
/// text, and those of its terminal interface kiln cannot run, with why. The CLI's list wins for
/// the rest: what it lists works when sent as text.
fn claude_catalog() -> Vec<SlashCommand> {
    let c = |name: &str, desc: &str, hint: &str, group: &str, command: Value| SlashCommand::control(name, desc, hint, group, command);
    // A request whose answer is shown to the client that asked.
    let ask = |name: &str, desc: &str, group: &str, id: &str| SlashCommand::control(name, desc, "", group, json!({"type": "control", "id": id, "value": {"client": "{client}"}}));
    let v = SlashCommand::view;
    let no = SlashCommand::unsupported;
    // Done in Claude Code's own screen, opened beside the agent in its folder.
    let t = SlashCommand::terminal;
    let cloud = "it hands this session to Anthropic's cloud or another app: use Claude Code in a terminal";
    vec![
        // Session.
        ask("cost", "What this session has cost so far", "Session", "session_cost"),
        ask("export", "The conversation as text", "Session", "export"),
        v("status", "Version, model, account, API connectivity and tool statuses", "Session", "status").aka(&["version"]),
        v("usage", "Session cost, plan usage and what counts against the limits", "Session", "usage").aka(&["stats"]),
        c("rename", "Rename the conversation", "<name>", "Session", json!({"type": "control", "id": "rename", "value": "{args}"})).aka(&["name"]),
        c("branch", "Continue this conversation in another agent", "", "Session", json!({"type": "control", "id": "fork"})),
        no("resume", "Resume a previous conversation", "Session", "continue another conversation from kiln's agents list").aka(&["continue"]),
        no("exit", "Quit", "Session", "close the agent in kiln").aka(&["quit"]),
        no("stop", "Stop this background session", "Session", "close the agent in kiln"),
        no("background", "Send this session to the background", "Session", "an agent in kiln runs on its own: closing the console leaves it running").aka(&["bg"]),
        v("login", "Sign Claude Code in on this machine", "Session", "login"),
        v("logout", "Sign Claude Code out on this machine", "Session", "logout"),
        no("feedback", "Send feedback or report a bug", "Session", "it sends the conversation to Anthropic: use the CLI for that").aka(&["bug"]),
        no("copy", "Copy the last response", "Session", "copy from the console"),
        no("cd", "Move this session to another folder", "Session", "an agent's folder is set when it starts"),
        no("fork", "A background agent that inherits the conversation", "Session", "Claude Code's in-session forks need its terminal interface: /branch continues the conversation in another agent"),
        t("upgrade", "Upgrade the plan", "Session"),
        t("usage-credits", "Usage credits", "Session"),
        t("privacy-settings", "Privacy settings", "Session"),
        t("setup-bedrock", "Amazon Bedrock setup", "Session"),
        t("setup-vertex", "Google Vertex AI setup", "Session"),
        t("install-github-app", "Set up Claude GitHub Actions", "Session"),
        t("install-slack-app", "Install the Claude Slack app", "Session"),
        t("import", "Import another agent's configuration", "Session"),
        no("teleport", "Send the session to the cloud, or resume one from claude.ai", "Session", cloud).aka(&["tp"]),
        no("remote-control", "Control this session from claude.ai", "Session", cloud).aka(&["rc"]),
        no("session", "The cloud session's address", "Session", cloud).aka(&["remote"]),
        no("desktop", "Continue in Claude Desktop", "Session", cloud).aka(&["app"]),
        t("mobile", "The Claude mobile app", "Session").aka(&["ios", "android"]),
        t("remote-env", "The cloud agents' environment", "Session"),
        t("web-setup", "Set up cloud sessions", "Session"),
        t("ultraplan", "Plan in the cloud", "Session"),
        t("ultrareview", "Review in the cloud", "Session"),
        t("autofix-pr", "Watch and fix the current pull request", "Session"),
        t("theme", "Change the theme", "Session"),
        t("color", "The prompt bar's color", "Session"),
        t("focus", "Focus view", "Session"),
        t("tui", "The terminal renderer", "Session"),
        t("scroll-speed", "Mouse wheel scroll speed", "Session"),
        t("terminal-setup", "Terminal key bindings", "Session"),
        no("voice", "Voice mode", "Session", "it listens to the microphone of the machine Claude Code runs on: use the console's microphone"),
        no("ide", "IDE integrations", "Context", "Claude Code's IDE integration needs its terminal interface"),
        // Model & mode.
        v("model", "The model and its effort", "Model & mode", "model"),
        v("effort", "The model's effort", "Model & mode", "effort"),
        v("output-style", "How it writes its replies", "Model & mode", "settings"),
        t("permissions", "Permission rules: allow, ask and deny, in Claude Code's own editor", "Model & mode").aka(&["allowed-tools"]),
        ask("permission-rules", "The permission rules in force, and where each comes from", "Model & mode", "permission_rules"),
        ask("plan", "Show the current plan", "Model & mode", "plan"),
        // Context.
        c("compact", "Summarize the conversation to free context", "[instructions]", "Context", json!({"type": "control", "id": "compact", "value": {"text": "{args}"}})),
        c("clear", "Start the conversation over (files stay as they are)", "", "Context", json!({"type": "control", "id": "clear"})),
        c("btw", "Ask a side question; the answer is not added to the conversation", "<question>", "Context", json!({"type": "control", "id": "side_question", "value": {"text": "{args}", "client": "{client}"}})),
        v("context", "How full the context is", "Context", "context"),
        ask("memory", "The memory files this session reads", "Context", "memory"),
        v("diff", "What this agent changed", "Context", "changes"),
        ask("git-diff", "The workspace's git diff, file by file", "Context", "workspace_diff"),
        // Tools & MCP.
        v("mcp", "MCP servers: status, tools, sign-in", "Tools & MCP", "mcp"),
        v("hooks", "Hooks that run on events", "Tools & MCP", "hooks"),
        v("tasks", "Subagents, shells and monitors", "Tools & MCP", "tasks").aka(&["bashes"]),
        v("workflows", "Running and finished workflows", "Tools & MCP", "tasks"),
        // Skills and plugins.
        v("skills", "Skills: view and run", "Skills", "skills"),
        SlashCommand::new("reload-skills", "Pick up skills added or changed on disk", "", "Skills", Route::Control { command: json!({"type": "skills_reload"}) }),
        v("plugin", "Installed plugins", "Plugins", "plugins").aka(&["plugins"]),
        c("reload-plugins", "Activate plugin changes in this session", "", "Plugins", json!({"type": "control", "id": "reload_plugins"})),
        // kiln's own.
        c("stop-tasks", "Stop every shell, monitor and task this agent left running", "", "Kiln", json!({"type": "stop_tasks"})),
        c("restart", "Start Claude Code again, continuing the conversation", "", "Kiln", json!({"type": "restart"})),
        v("terminal", "A shell in this agent's folder, on its machine", "Kiln", "terminal").aka(&["shell"]),
        v("screen", "Claude Code's own screen, in this agent's folder", "Kiln", "screen"),
        v("goals", "Goals one after another: each a /goal, the next once it's met", "Kiln", "goals"),
    ]
}

/// The commands Claude Code 2.1.296 binds to its terminal (init's terminal_slash_commands).
const TERMINAL_COMMANDS: &[&str] = &["doctor", "color", "focus", "reload-plugins"];

/// The group of a command of Claude Code's own that is sent as text.
fn claude_group(name: &str) -> &'static str {
    match name {
        "init" | "security-review" | "autocompact" => "Context",
        "fast" | "auto-mode-setup" => "Model & mode",
        "agents" | "list-agents" => "Tools & MCP",
        _ => "Session",
    }
}

/// The answer to an informational request, as `{title, text, format?, ...}`.
fn info_reply(what: &str, body: &Value) -> Value {
    match what {
        "side_question" => json!({"title": "Side question", "text": str_of(body, "response").unwrap_or(""), "format": "markdown"}),
        "session_cost" => json!({"title": "Cost", "text": strip_ansi(str_of(body, "text").unwrap_or(""))}),
        "export" => json!({"title": "Conversation", "text": str_of(body, "text").unwrap_or(""), "filename": str_of(body, "default_filename")}),
        "plan" => match body.get("exists").and_then(Value::as_bool) {
            Some(true) => json!({"title": "Plan", "text": str_of(body, "content").unwrap_or(""), "format": "markdown", "path": str_of(body, "path")}),
            _ => json!({"title": "Plan", "text": "No plan has been written in this session."}),
        },
        "permission_rules" => {
            let st = body.get("state").unwrap_or(body);
            let mut lines: Vec<String> = st
                .get("rules")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|r| format!("{:<5} {}  ({})", str_of(r, "behavior").unwrap_or(""), str_of(r, "rule").unwrap_or(""), str_of(r, "source").unwrap_or("")))
                .collect();
            if lines.is_empty() {
                lines.push("No permission rules: everything not allowed by the mode asks.".to_owned());
            }
            let dirs: Vec<&str> = st.get("workspaceDirectories").and_then(Value::as_array).into_iter().flatten().filter_map(|d| d.as_str().or_else(|| str_of(d, "path"))).collect();
            if !dirs.is_empty() {
                lines.push(format!("\nWorkspace directories: {}", dirs.join(", ")));
            }
            json!({"title": "Permission rules", "text": lines.join("\n")})
        }
        "memory" => {
            let row = |f: &Value| {
                let missing = if f.get("exists").and_then(Value::as_bool) == Some(false) { " (not created)" } else { "" };
                format!("{}  {}{missing}", str_of(f, "label").unwrap_or(""), str_of(f, "path").unwrap_or(""))
            };
            let mut lines: Vec<String> = body.get("files").and_then(Value::as_array).into_iter().flatten().map(row).collect();
            if lines.is_empty() {
                lines.push("No memory files.".to_owned());
            }
            let folders: Vec<String> = body.get("folders").and_then(Value::as_array).into_iter().flatten().map(row).collect();
            if !folders.is_empty() {
                lines.push(String::new());
                lines.extend(folders);
            }
            if let Some(on) = body.pointer("/auto_memory/enabled").and_then(Value::as_bool) {
                lines.push(format!("\nAuto-memory: {}", if on { "on" } else { "off" }));
            }
            json!({"title": "Memory", "text": lines.join("\n")})
        }
        _ => match body.get("diff").filter(|d| d.is_object()) {
            Some(d) => {
                let mut lines: Vec<String> = d
                    .get("perFileStats")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|f| format!("+{:<5} -{:<5} {}", f.get("added").and_then(Value::as_i64).unwrap_or(0), f.get("removed").and_then(Value::as_i64).unwrap_or(0), str_of(f, "path").unwrap_or("")))
                    .collect();
                if let Some(s) = d.get("stats") {
                    lines.insert(0, format!("{} files, +{} -{}", s.get("filesCount").and_then(Value::as_i64).unwrap_or(0), s.get("linesAdded").and_then(Value::as_i64).unwrap_or(0), s.get("linesRemoved").and_then(Value::as_i64).unwrap_or(0)));
                }
                json!({"title": "Git diff", "text": lines.join("\n")})
            }
            None => json!({"title": "Git diff", "text": "No changes (or not a git repository)."}),
        },
    }
}

/// What a task is, from Claude Code's `task_type` and the tool that started it.
fn task_class(task_type: Option<&str>, tool: &str, subagent: bool) -> Option<String> {
    let class = match task_type.unwrap_or("") {
        "local_bash" if tool == "Monitor" => "monitor",
        "local_bash" => "shell",
        t if t.starts_with("monitor") => "monitor",
        "local_agent" | "remote_agent" | "in_process_teammate" => "agent",
        "local_workflow" => "workflow",
        "mcp_task" => "mcp",
        _ if tool == "Monitor" => "monitor",
        _ if subagent || matches!(tool, "Task" | "Agent") => "agent",
        _ => return None,
    };
    Some(class.to_owned())
}

/// The command a shell or monitor task runs, from its tool call.
fn task_command(tool: &str, input: &Value) -> Option<String> {
    match tool {
        "Bash" | "Monitor" => str_of(input, "command").map(str::to_owned).or_else(|| input.pointer("/ws/url").and_then(Value::as_str).map(|u| format!("websocket {}", safe_url(u)))),
        _ => None,
    }
}

/// A background command's task id and output file, from its call's result ("… with ID: x.
/// Output is being written to: /tmp/…/tasks/x.output").
fn background_of(text: &str, structured: &Value) -> Option<(String, String)> {
    let file = text.split("Output is being written to: ").nth(1).and_then(|r| r.split_whitespace().next()).map(|f| f.trim_end_matches(['.', ',', ';']).to_owned())?;
    let id = structured
        .get("backgroundTaskId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| text.split("ID: ").nth(1).map(|r| r.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect::<String>()))
        .filter(|i| !i.is_empty())
        .or_else(|| std::path::Path::new(&file).file_stem().and_then(|s| s.to_str()).map(str::to_owned))?;
    Some((id, file))
}

/// The text between two markers.
fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = s.find(open)? + open.len();
    let end = s.get(start..)?.find(close)? + start;
    s.get(start..end)
}

/// "… (exit code 3)" → 3.
fn exit_code_in(s: &str) -> Option<i32> {
    let rest = s.split("exit code").nth(1)?;
    let digits: String = rest.trim_start_matches([' ', ':']).chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
    digits.parse().ok()
}

/// A call that runs alongside others (read-only), rather than one at a time.
fn alongside(tool: ToolKind) -> bool {
    matches!(tool, ToolKind::Read | ToolKind::Search | ToolKind::Web | ToolKind::Agent | ToolKind::Todo)
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

/// The permission modes Claude Code offers. Bypass is always one: a session not started able to
/// use it starts again in it (the conversation continues).
pub fn modes(allow_bypass: bool) -> Vec<ModeOption> {
    let bypass = if allow_bypass { "Skip every permission check" } else { "Skip every permission check (Claude Code starts again to turn it on; the conversation continues)" };
    vec![
        ModeOption { id: "default".into(), label: "Ask".into(), description: "Ask before edits and commands".into(), dangerous: false },
        ModeOption { id: "acceptEdits".into(), label: "Accept edits".into(), description: "Edit files without asking; ask before commands".into(), dangerous: false },
        ModeOption { id: "plan".into(), label: "Plan".into(), description: "Read and plan only; change nothing".into(), dangerous: false },
        ModeOption { id: "auto".into(), label: "Auto".into(), description: "A classifier approves safe actions; risky ones still ask".into(), dangerous: false },
        // Claude Code's dontAsk never asks and refuses whatever isn't already allowed: it is not
        // "go ahead without asking", which is Bypass.
        ModeOption { id: "dontAsk".into(), label: "Allowed only".into(), description: "Never asks: refuses anything not already allowed".into(), dangerous: false },
        ModeOption { id: "bypassPermissions".into(), label: "Bypass".into(), description: bypass.into(), dangerous: true },
    ]
}

/// Whether Claude Code would let this process skip its checks: as root it refuses to, unless
/// told it runs in a sandbox (`IS_SANDBOX`).
pub(super) fn bypass_possible() -> bool {
    !crate::os::is_root() || std::env::var_os("IS_SANDBOX").is_some()
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
            crate::reach::describe(&server, &tool, input).unwrap_or_else(|| (ToolKind::Mcp, format!("{server} · {tool}"), None))
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
                        // Claude Code always offers "Other".
                        other: true,
                        options: q
                            .get("options")
                            .and_then(Value::as_array)
                            .map(|o| {
                                o.iter()
                                    .filter_map(|x| {
                                        Some(QuestionOption {
                                            label: str_of(x, "label")?.to_owned(),
                                            description: str_of(x, "description").unwrap_or("").to_owned(),
                                            preview: str_of(x, "preview").unwrap_or("").to_owned(),
                                        })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                        ..Question::default()
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// AskUserQuestion's `answers` (question → the labels chosen and any free text, joined with
/// ", ") and `annotations` (question → {notes, preview of the option chosen}).
fn claude_answers(choices: &[ChoiceAnswer], questions: &[Question]) -> (Value, serde_json::Map<String, Value>) {
    let mut answers = serde_json::Map::new();
    let mut annotations = serde_json::Map::new();
    for c in choices {
        let mut said: Vec<&str> = c.selected.iter().map(String::as_str).collect();
        if let Some(o) = c.other.as_deref().map(str::trim).filter(|o| !o.is_empty()) {
            said.push(o);
        }
        answers.insert(c.question.clone(), json!(said.join(", ")));
        let mut note = serde_json::Map::new();
        if let Some(n) = c.note.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            note.insert("notes".into(), json!(n));
        }
        let preview = questions
            .iter()
            .find(|q| q.question == c.question)
            .and_then(|q| q.options.iter().find(|o| c.selected.contains(&o.label) && !o.preview.is_empty()))
            .map(|o| o.preview.clone());
        if let Some(p) = preview {
            note.insert("preview".into(), json!(p));
        }
        if !note.is_empty() {
            annotations.insert(c.question.clone(), Value::Object(note));
        }
    }
    (Value::Object(answers), annotations)
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
    fn a_goal_is_followed_to_the_verdict_in_the_transcript() {
        let mut c = Claude::new(false).with_config_dir("/cfg".into());
        let mut core = Core::new();
        c.line(&mut core, &json!({"type": "system", "subtype": "init", "cwd": "/w", "session_id": "s1", "model": "m", "permissionMode": "default"}).to_string());
        let run = |c: &mut Claude, core: &mut Core, args: &str, said: &str| {
            c.line(core, &json!({"type": "assistant", "uuid": "u", "local_command_run": {"command": "goal", "args": args}, "message": {"id": "lc", "role": "assistant", "content": [{"type": "text", "text": said}]}}).to_string());
        };
        let set = |c: &mut Claude, core: &mut Core, cond: &str| run(c, core, cond, &format!("Goal set: {cond}"));
        let end = |c: &mut Claude, core: &mut Core| -> Option<String> {
            c.line(core, &json!({"type": "result", "subtype": "success"}).to_string());
            core.effects.drain(..).find_map(|e| match e {
                Effect::TranscriptTail { job, config, session, .. } => (config == "/cfg" && session == "s1").then_some(job),
                _ => None,
            })
        };
        let rec = |a: Value| json!({"type": "attachment", "attachment": a}).to_string();
        let status = |core: &Core| core.state.objective.as_ref().map(|o| (o.status.clone(), o.reason.clone()));

        // Met after two checks.
        set(&mut c, &mut core, "tests pass");
        assert_eq!(status(&core), Some(("active".into(), None)));
        let job = end(&mut c, &mut core).unwrap_or_default();
        let tail = [rec(json!({"type": "goal_status", "met": false, "sentinel": true, "condition": "tests pass"})), rec(json!({"type": "goal_status", "met": false, "condition": "tests pass", "reason": "Not yet"})), rec(json!({"type": "goal_status", "met": true, "condition": "tests pass", "reason": "They pass", "iterations": 2}))].join("\n");
        c.run_done(&mut core, &job, Some(0), &tail);
        assert_eq!(status(&core), Some(("met".into(), Some("They pass".into()))));

        // Met, then cleared as a met goal clears itself (written however it's spaced): met.
        set(&mut c, &mut core, "docs  build");
        let job = end(&mut c, &mut core).unwrap_or_default();
        let tail = [
            rec(json!({"type": "goal_status", "met": false, "sentinel": true, "condition": "docs build"})),
            rec(json!({"type": "goal_status", "met": true, "condition": "docs build", "reason": "Built"})),
            rec(json!({"type": "goal_status", "met": true, "sentinel": true, "condition": "docs build"})),
        ]
        .join("\n");
        c.run_done(&mut core, &job, Some(0), &tail);
        assert_eq!(status(&core), Some(("met".into(), Some("Built".into()))));

        // A "met" from before the goal was set again doesn't count.
        set(&mut c, &mut core, "lint passes");
        let job = end(&mut c, &mut core).unwrap_or_default();
        let tail = [
            rec(json!({"type": "goal_status", "met": true, "condition": "lint passes", "reason": "Earlier"})),
            rec(json!({"type": "goal_status", "met": false, "sentinel": true, "condition": "lint passes"})),
            rec(json!({"type": "goal_status", "met": false, "condition": "lint passes", "reason": "Not yet"})),
        ]
        .join("\n");
        c.run_done(&mut core, &job, Some(0), &tail);
        assert_eq!(status(&core), Some(("active".into(), None)));

        // Can't be met.
        set(&mut c, &mut core, "fly");
        let job = end(&mut c, &mut core).unwrap_or_default();
        c.run_done(&mut core, &job, Some(0), &rec(json!({"type": "goal_status", "met": false, "failed": true, "condition": "fly", "reason": "No wings"})));
        assert_eq!(status(&core), Some(("unmet".into(), Some("No wings".into()))));

        // It gave up after its checks: still set, unmet.
        set(&mut c, &mut core, "never");
        c.line(&mut core, &json!({"type": "system", "subtype": "notification", "key": "stop-hook-block-cap", "text": "A hook blocked the turn from ending 9 consecutive times"}).to_string());
        let job = end(&mut c, &mut core).unwrap_or_default();
        c.run_done(&mut core, &job, Some(0), &rec(json!({"type": "goal_status", "met": false, "condition": "never", "reason": "Not yet (9)"})));
        assert!(matches!(status(&core), Some((s, Some(r))) if s == "unmet" && r.contains("Not yet (9)")));

        // Cut short (no verdict yet): still active; cleared by /goal clear.
        set(&mut c, &mut core, "later");
        let job = end(&mut c, &mut core).unwrap_or_default();
        c.run_done(&mut core, &job, Some(0), &rec(json!({"type": "goal_status", "met": false, "sentinel": true, "condition": "later"})));
        assert_eq!(status(&core), Some(("active".into(), None)));
        run(&mut c, &mut core, "clear", "Goal cleared: later");
        assert_eq!(status(&core), Some(("cleared".into(), None)));
        // One it gave up on, still set, cleared too; a bare /goal only asks.
        set(&mut c, &mut core, "again");
        run(&mut c, &mut core, "", "No goal set. Usage: `/goal <condition>`");
        assert_eq!(status(&core), Some(("active".into(), None)));
        core.state.objective = Some(Objective::new("again", "unmet", Some("still set".into())));
        run(&mut c, &mut core, "clear", "Goal cleared: again");
        assert_eq!(status(&core), Some(("cleared".into(), None)));
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
        // The CLI's /compact, done through kiln's control.
        let compact = core.meta.commands.iter().find(|c| c.name == "compact").cloned().unwrap_or_else(|| SlashCommand::prompt("", "", "", ""));
        assert!(matches!(&compact.route, Route::Control { command } if command.get("id") == Some(&json!("compact"))) && compact.group == "Context");
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
        c.command(&mut core, Command::Approve { id: "f7d9".into(), decision: Decision::Always, message: None, always: Some(0), answers: None, choices: None });
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
        c.command(&mut core, Command::Approve { id: "r9".into(), decision: Decision::Deny, message: Some("use b.txt".into()), always: None, answers: None, choices: None });
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
    fn an_mcp_servers_request_for_input_is_asked_and_answered() -> anyhow::Result<()> {
        let mut c = Claude::new(false);
        let mut core = Core::new();
        let req = json!({"subtype": "elicitation", "mcp_server_name": "linear", "message": "Your API token", "mode": "form", "requested_schema": {"type": "object", "properties": {"token": {"type": "string", "title": "API token"}}, "required": ["token"]}});
        c.line(&mut core, &json!({"type": "control_request", "request_id": "el1", "request": req}).to_string());
        let a = core.state.approvals.first().cloned().ok_or_else(|| anyhow::anyhow!("no approval"))?;
        assert_eq!((a.title.as_str(), a.questions.first().map(|q| q.other)), ("linear asks: Your API token", Some(true)));
        let choices = vec![ChoiceAnswer { id: Some("token".into()), other: Some("lin_1".into()), ..ChoiceAnswer::default() }];
        c.command(&mut core, Command::Approve { id: "el1".into(), decision: Decision::Allow, message: None, always: None, answers: None, choices: Some(choices) });
        let resp: Value = serde_json::from_str(core.take_writes().first().map(String::as_str).unwrap_or("{}"))?;
        assert_eq!(resp.pointer("/response/response").cloned(), Some(json!({"action": "accept", "content": {"token": "lin_1"}})));
        assert!(core.state.approvals.is_empty());
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
        c.command(&mut core, Command::Approve { id: "q1".into(), decision: Decision::Allow, message: None, always: None, answers: Some(answers), choices: None });
        let resp: Value = serde_json::from_str(core.take_writes().first().map(String::as_str).unwrap_or("{}"))?;
        assert_eq!(resp.pointer("/response/response/updatedInput/answers/Which database?").and_then(Value::as_str), Some("SQLite"));
        Ok(())
    }

    #[test]
    fn a_queued_message_is_steered_into_the_running_turn() -> anyhow::Result<()> {
        let (mut c, mut core, _) = started();
        assert!(core.meta.features.iter().any(|f| f == "queue_steer"));
        let send = |c: &mut Claude, core: &mut Core, id: &str, text: &str| c.command(core, Command::Send { id: id.into(), text: text.into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        send(&mut c, &mut core, "m1", "build it");
        send(&mut c, &mut core, "m2", "tests too, please");
        core.take_writes();
        assert_eq!(core.state.queue.len(), 1);
        // Written at once without priority (no interrupt): the CLI folds it into the turn.
        c.command(&mut core, Command::QueueSteer { id: "m2".into() });
        let w: Vec<Value> = core.take_writes().iter().filter_map(|l| serde_json::from_str(l).ok()).collect();
        let msg = w.iter().find(|v| str_of(v, "type") == Some("user")).cloned().unwrap_or_default();
        assert_eq!((msg.pointer("/message/content"), msg.get("uuid"), msg.get("priority")), (Some(&json!("tests too, please")), Some(&json!("m2")), None));
        assert!(core.state.queue.is_empty() && core.state.delivery.get("m2") == Some(&Delivery::Sent));
        assert!(!w.iter().any(|v| v.pointer("/request/subtype") == Some(&json!("interrupt"))));
        // Unknown: nothing happens.
        c.command(&mut core, Command::QueueSteer { id: "nope".into() });
        assert!(core.take_writes().is_empty());
        Ok(())
    }

    #[test]
    fn structured_answers_keep_commas_free_text_and_notes() -> anyhow::Result<()> {
        use super::super::model::ChoiceAnswer;
        let mut c = Claude::new(false);
        let mut core = Core::new();
        let ask = json!({"type": "control_request", "request_id": "q2", "request": {"subtype": "can_use_tool", "tool_name": "AskUserQuestion", "tool_use_id": "tq2", "input": {"questions": [
            {"question": "Which database?", "header": "DB", "multiSelect": false, "options": [{"label": "Postgres, with replicas", "description": "relational", "preview": "<pre>pg</pre>"}, {"label": "SQLite", "description": "embedded"}]},
            {"question": "Anything else?", "header": "More", "multiSelect": true, "options": [{"label": "Tests", "description": ""}, {"label": "Docs, too", "description": ""}]}
        ]}}});
        c.line(&mut core, &ask.to_string());
        let a = core.state.approvals.first().cloned().ok_or_else(|| anyhow::anyhow!("no approval"))?;
        let q = a.questions.first().cloned().unwrap_or_default();
        assert!(q.other && !q.secret && q.options.first().map(|o| o.preview.as_str()) == Some("<pre>pg</pre>"));
        let choices = vec![
            ChoiceAnswer { question: "Which database?".into(), selected: vec!["Postgres, with replicas".into()], note: Some("keep it cheap".into()), ..ChoiceAnswer::default() },
            ChoiceAnswer { question: "Anything else?".into(), selected: vec!["Docs, too".into()], other: Some("Benchmarks, maybe".into()), ..ChoiceAnswer::default() },
        ];
        c.command(&mut core, Command::Approve { id: "q2".into(), decision: Decision::Allow, message: None, always: None, answers: None, choices: Some(choices) });
        let resp: Value = serde_json::from_str(core.take_writes().first().map(String::as_str).unwrap_or("{}"))?;
        let input = resp.pointer("/response/response/updatedInput").cloned().unwrap_or_default();
        assert_eq!(input.get("answers"), Some(&json!({"Which database?": "Postgres, with replicas", "Anything else?": "Docs, too, Benchmarks, maybe"})));
        // Notes (and the chosen option's preview) as annotations, which 2.1.296's AskUserQuestion takes.
        assert_eq!(input.get("annotations"), Some(&json!({"Which database?": {"notes": "keep it cheap", "preview": "<pre>pg</pre>"}})));
        assert!(input.get("questions").is_some());
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
            allow_bypass: false,
            ..Default::default()
        };
        let a = args(&spec, None);
        assert!(a.windows(2).any(|w| w == ["--resume", "abc"]) && a.windows(2).any(|w| w == ["--permission-mode", "plan"]));
        assert!(a.windows(2).any(|w| w == ["--resume-session-at", "u9"]) && a.iter().any(|x| x == "--fork-session"));
        assert!(a.windows(2).any(|w| w == ["--effort", "high"]));
        assert!(!a.iter().any(|x| x == "--allow-dangerously-skip-permissions"));
        // Every session starts able to skip checks unless it says otherwise; none starts skipping them.
        let a = args(&super::super::session::Spec { kind: "claude".into(), ..Default::default() }, None);
        assert_eq!(a.iter().any(|x| x == "--allow-dangerously-skip-permissions"), bypass_possible());
        assert!(!a.iter().any(|x| x == "--dangerously-skip-permissions") && !a.iter().any(|x| x == "--permission-mode"));
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
    fn the_command_catalog_routes_each_command() -> anyhow::Result<()> {
        let mut c = Claude::new(false).with_config_dir("/home/u/.claude".into());
        let mut core = Core::new();
        c.start(&mut core);
        // Before the CLI lists its commands: kiln's.
        assert!(core.meta.commands.iter().any(|e| e.name == "btw"));
        let reqs = requests(&mut core);
        // Claude Code 2.1.296's list (abridged): its own commands and bundled skills marked
        // builtin, with aliases; a user's skill; an MCP prompt; a hidden one.
        let commands = json!([
            {"name": "kiln-test", "description": "A test skill. (user)", "argumentHint": "<word>"},
            {"name": "init", "description": "My own init. (user)", "argumentHint": ""},
            {"name": "deep-research", "description": "Deep research.", "argumentHint": "", "builtin": true},
            {"name": "doctor", "description": "Health-check the setup", "argumentHint": "", "aliases": ["checkup"], "builtin": true},
            {"name": "clear", "description": "Start a new session with empty context", "argumentHint": "[name]", "aliases": ["reset", "new"], "builtin": true},
            {"name": "compact", "description": "Free up context", "argumentHint": "<optional custom summarization instructions>", "builtin": true},
            {"name": "color", "description": "Set the prompt bar color", "argumentHint": "", "builtin": true},
            {"name": "init", "description": "Initialize a new CLAUDE.md", "argumentHint": "", "builtin": true},
            {"name": "config", "description": "Set a setting by key", "argumentHint": "key=value", "aliases": ["settings"], "builtin": true},
            {"name": "reload-plugins", "description": "Activate pending plugin changes", "argumentHint": "[--force]", "builtin": true},
            {"name": "usage", "description": "Show session cost and plan usage", "argumentHint": "", "aliases": ["cost", "stats"], "builtin": true},
            {"name": "__remote-workflow", "description": "Internal", "argumentHint": "", "builtin": true},
            {"name": "echo-srv:greet (MCP)", "description": "Greets", "argumentHint": "[name]"}
        ]);
        reply_to(&mut c, &mut core, &reqs, "initialize", json!({"commands": commands, "models": [], "claude_code_version": "2.1.296"}));
        // Before init (it comes with the first turn): the terminal's commands 2.1.296 names.
        assert!(core.meta.commands.iter().any(|e| e.name == "doctor" && matches!(e.route, Route::Terminal { .. })));
        c.line(
            &mut core,
            &json!({"type": "system", "subtype": "init", "cwd": "/w", "session_id": "s1", "model": "m", "permissionMode": "default",
                "skills": ["kiln-test", "deep-research"], "terminal_slash_commands": ["doctor", "color", "reload-plugins"]})
            .to_string(),
        );
        let cat = core.meta.commands.clone();
        let get = |n: &str| cat.iter().find(|e| e.name == n).cloned().unwrap_or_else(|| SlashCommand::prompt("", "", "", ""));
        let control = |n: &str| match get(n).route {
            Route::Control { command } => command.get("id").or_else(|| command.get("type")).and_then(Value::as_str).unwrap_or("").to_owned(),
            _ => String::new(),
        };
        // The CLI's own commands kiln does itself, in the CLI's words.
        assert_eq!((control("compact"), get("compact").group.as_str(), get("compact").description.as_str()), ("compact".to_owned(), "Context", "Free up context"));
        assert_eq!(control("clear"), "clear");
        assert_eq!(get("clear").aliases, vec!["reset", "new"]);
        assert_eq!(control("reload-plugins"), "reload_plugins");
        assert!(matches!(get("usage").route, Route::View { view } if view == "usage"));
        // kiln's /cost takes the name the CLI uses as an alias of /usage.
        assert_eq!((control("cost"), get("usage").aliases), ("session_cost".to_owned(), vec!["stats".to_owned()]));
        // What the CLI lists that kiln leaves to it: sent as text (/init runs the CLI's own, not
        // the user's row of that name).
        assert!(matches!(get("init").route, Route::Prompt { text: None, .. }) && get("init").description == "Initialize a new CLAUDE.md");
        assert!(matches!(get("config").route, Route::Prompt { .. }) && get("config").aliases == ["settings"]);
        // Skills (bundled ones too), MCP prompts.
        assert!(matches!(get("kiln-test").route, Route::Prompt { skill: true, .. }) && get("kiln-test").group == "Skills" && get("kiln-test").args == "required");
        assert!(matches!(get("deep-research").route, Route::Prompt { skill: true, .. }));
        assert_eq!(get("echo-srv:greet (MCP)").group, "Tools & MCP");
        // Bound to the CLI's terminal: done in Claude Code's own screen, opened beside the agent.
        assert!(matches!(get("doctor").route, Route::Terminal { command } if command == "/doctor"));
        assert!(matches!(get("color").route, Route::Terminal { .. }));
        assert!(matches!(get("permissions").route, Route::Terminal { command } if command == "/permissions") && get("permissions").aliases == ["allowed-tools"]);
        // Not runnable from kiln: why.
        assert!(matches!(get("teleport").route, Route::Unsupported { .. }));
        assert!(matches!(get("login").route, Route::View { view } if view == "login"));
        assert!(matches!(get("logout").route, Route::View { view } if view == "logout"));
        // kiln's own requests: the CLI does not list them.
        assert_eq!((control("btw"), get("btw").args.as_str()), ("side_question".to_owned(), "required"));
        assert_eq!((control("permission-rules"), control("plan"), control("memory"), control("git-diff"), control("export")), ("permission_rules".into(), "plan".into(), "memory".into(), "workspace_diff".into(), "export".into()));
        assert!(!cat.iter().any(|e| e.name.starts_with("__")));
        let mut names: Vec<&str> = cat.iter().flat_map(|e| std::iter::once(e.name.as_str()).chain(e.aliases.iter().map(String::as_str))).collect();
        let n = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), n, "a name or alias twice");
        Ok(())
    }

    #[test]
    fn the_catalogs_requests_answer_the_client_that_asked() -> anyhow::Result<()> {
        let (mut c, mut core, _) = started();
        let ask = |c: &mut Claude, core: &mut Core, id: &str, value: Value| {
            c.command(core, Command::Control { id: id.into(), value: Some(value) });
            requests(core)
        };
        // /btw: a side question, its answer to that client alone.
        let reqs = ask(&mut c, &mut core, "side_question", json!({"text": "what is 2+2?", "client": "k1"}));
        assert_eq!(reqs.first().map(|r| r.2.clone()), Some(json!({"subtype": "side_question", "question": "what is 2+2?"})));
        reply_to(&mut c, &mut core, &reqs, "side_question", json!({"response": "4", "synthetic": false}));
        assert_eq!(replies(&mut core), vec![("k1".to_owned(), json!({"title": "Side question", "text": "4", "format": "markdown"}))]);
        // Nothing asked: not sent.
        let reqs = ask(&mut c, &mut core, "side_question", json!({"text": " ", "client": "k2"}));
        assert!(reqs.is_empty() && replies(&mut core).iter().any(|(cl, v)| cl == "k2" && v.get("error").is_some()));
        // /cost without the terminal's colors.
        let reqs = ask(&mut c, &mut core, "session_cost", json!({"client": "k3"}));
        reply_to(&mut c, &mut core, &reqs, "get_session_cost", json!({"text": "\u{1b}[1mTotal cost:\u{1b}[22m $0.12"}));
        assert_eq!(replies(&mut core).first().and_then(|r| r.1.get("text").cloned()), Some(json!("Total cost: $0.12")));
        // /plan with none written; /git-diff outside a repository; /memory's files and folders.
        let reqs = ask(&mut c, &mut core, "plan", json!({"client": "k4"}));
        reply_to(&mut c, &mut core, &reqs, "get_plan", json!({"exists": false}));
        let reqs = ask(&mut c, &mut core, "workspace_diff", json!({"client": "k5"}));
        reply_to(&mut c, &mut core, &reqs, "get_workspace_diff", json!({"diff": null}));
        let reqs = ask(&mut c, &mut core, "memory", json!({"client": "k6"}));
        reply_to(
            &mut c,
            &mut core,
            &reqs,
            "get_memory_dialog",
            json!({"files": [{"kind": "user", "path": "/home/u/.claude/CLAUDE.md", "label": "User memory", "description": "", "exists": false}], "folders": [{"kind": "auto", "path": "/home/u/.claude/projects/w/memory", "label": "Auto-memory folder", "description": ""}], "auto_memory": {"enabled": true, "toggleable": true}}),
        );
        let r = replies(&mut core);
        let text = |cl: &str| r.iter().find(|x| x.0 == cl).and_then(|x| x.1.get("text").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default();
        assert!(text("k4").starts_with("No plan"), "{r:?}");
        assert!(text("k5").starts_with("No changes"), "{r:?}");
        assert!(text("k6").contains("User memory  /home/u/.claude/CLAUDE.md (not created)") && text("k6").contains("Auto-memory folder") && text("k6").ends_with("Auto-memory: on"), "{r:?}");
        // A request the CLI refuses answers with its error.
        let reqs = ask(&mut c, &mut core, "permission_rules", json!({"client": "k7"}));
        let id = reqs.first().map(|r| r.0.clone()).unwrap_or_default();
        c.line(&mut core, &json!({"type": "control_response", "response": {"subtype": "error", "request_id": id, "error": "nope"}}).to_string());
        assert_eq!(replies(&mut core), vec![("k7".to_owned(), json!({"error": "nope"}))]);
        // /branch: the whole conversation, continued in another agent.
        c.command(&mut core, Command::Control { id: "fork".into(), value: None });
        assert!(matches!(core.effects.pop(), Some(Effect::Branch { resume, resume_at: None }) if resume == "s1"));
        Ok(())
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
        let skills_in_catalog: Vec<&str> = core.meta.commands.iter().filter(|c| c.group == "Skills" && matches!(c.route, Route::Prompt { skill: true, .. })).map(|c| c.name.as_str()).collect();
        assert!(skills_in_catalog == ["kiln-test"] && core.state.skills.iter().any(|s| s.name == "kiln-test" && s.description == "A test skill."), "{skills_in_catalog:?}");
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
    fn bypass_without_its_flag_starts_claude_code_again_in_it() {
        let mut c = Claude::new(false);
        let mut core = Core::new();
        c.start(&mut core);
        assert!(core.meta.modes.iter().any(|m| m.id == "bypassPermissions" && m.dangerous));
        let reqs = requests(&mut core);
        reply_to(&mut c, &mut core, &reqs, "initialize", json!({"commands": []}));
        c.line(&mut core, &json!({"type": "system", "subtype": "init", "cwd": "/w", "session_id": "s1", "model": "m", "permissionMode": "default"}).to_string());
        c.line(&mut core, r#"{"type":"system","subtype":"session_state_changed","state":"idle","uuid":"u","session_id":"s1"}"#);
        let _ = requests(&mut core);
        core.effects.clear();
        c.command(&mut core, Command::SetMode { mode: "bypassPermissions".into() });
        // Not asked of the CLI (it would refuse): it starts again able to, continuing the conversation.
        assert!(!requests(&mut core).iter().any(|r| r.1 == "set_permission_mode"));
        assert_eq!(core.effects, vec![Effect::AllowBypass, Effect::Restart { resume_at: None }]);
        assert_eq!(core.state.mode, "bypassPermissions");
        // Started able to, it switches in place.
        let mut c = Claude::new(true);
        let mut core = Core::new();
        c.start(&mut core);
        let _ = requests(&mut core);
        c.command(&mut core, Command::SetMode { mode: "bypassPermissions".into() });
        assert!(requests(&mut core).iter().any(|r| r.1 == "set_permission_mode" && r.2.get("mode") == Some(&json!("bypassPermissions"))));
        assert!(!core.effects.contains(&Effect::AllowBypass));
    }

    #[test]
    fn a_mode_change_mid_session() {
        let mut c = Claude::new(false);
        let mut core = Core::new();
        c.start(&mut core);
        let ids: Vec<&str> = core.meta.modes.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"default") && ids.contains(&"plan") && ids.contains(&"acceptEdits") && ids.contains(&"bypassPermissions"), "{ids:?}");
        // dontAsk refuses what isn't allowed: it never reads as going ahead without asking.
        assert!(core.meta.modes.iter().any(|m| m.id == "dontAsk" && m.label == "Allowed only" && !m.dangerous));
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

    #[test]
    fn effort_levels_from_the_start_and_a_change_mid_session() {
        let mut c = Claude::new(false);
        let mut core = Core::new();
        c.start(&mut core);
        let ids = |core: &Core| core.meta.efforts.iter().map(|e| e.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&core), ["low", "medium", "high", "xhigh", "max"]);
        let reqs = requests(&mut core);
        reply_to(
            &mut c,
            &mut core,
            &reqs,
            "initialize",
            json!({"commands": [], "models": [
                {"value": "default", "resolvedModel": "claude-opus-5-5", "displayName": "Default", "description": "", "supportsEffort": true, "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"]},
                {"value": "sonnet", "resolvedModel": "claude-sonnet-5-5", "displayName": "Sonnet", "description": "", "supportsEffort": true, "supportedEffortLevels": ["low", "medium", "high"]},
                {"value": "haiku", "resolvedModel": "claude-haiku-5-5", "displayName": "Haiku", "description": ""}
            ]}),
        );
        // No model yet: the default model's levels.
        assert_eq!(ids(&core).len(), 5);
        // An alias with a context suffix finds its model; one without effort has none.
        c.line(&mut core, &json!({"type": "system", "subtype": "init", "cwd": "/w", "session_id": "s", "model": "sonnet[1m]", "permissionMode": "default"}).to_string());
        assert_eq!(ids(&core), ["low", "medium", "high"]);
        let _ = requests(&mut core);
        c.command(&mut core, Command::SetModel { model: "claude-haiku-5-5".into(), kind: None });
        let reqs = requests(&mut core);
        reply_to(&mut c, &mut core, &reqs, "set_model", json!({}));
        assert!(core.meta.efforts.is_empty());
        // Set mid-session the way the CLI takes it, shown at once and confirmed from its settings.
        c.command(&mut core, Command::SetEffort { effort: "max".into() });
        assert_eq!(core.state.effort, "max");
        let reqs = requests(&mut core);
        assert!(reqs.iter().any(|r| r.1 == "apply_flag_settings" && r.2.pointer("/settings/effortLevel") == Some(&json!("max"))));
        reply_to(&mut c, &mut core, &reqs, "apply_flag_settings", json!({}));
        let reqs = requests(&mut core);
        reply_to(&mut c, &mut core, &reqs, "get_settings", json!({"effective": {}, "applied": {"effort": "high"}}));
        assert_eq!(core.state.effort, "high");
        // A CLI that lists no models: the levels it knows.
        let mut c2 = Claude::new(false);
        let mut core2 = Core::new();
        c2.start(&mut core2);
        let reqs = requests(&mut core2);
        reply_to(&mut c2, &mut core2, &reqs, "initialize", json!({"commands": []}));
        assert_eq!(ids(&core2).len(), 5);
    }

    #[test]
    fn calls_waiting_behind_a_permission_prompt_read_as_pending() {
        let mut c = Claude::new(false);
        let mut core = Core::new();
        core.state.cwd = "/w".into();
        let write = |id: &str, n: u32| json!({"type": "tool_use", "id": id, "name": "Write", "input": {"file_path": format!("/w/f{n}.txt"), "content": "x"}});
        c.line(
            &mut core,
            &json!({"type": "assistant", "message": {"id": "m", "type": "message", "role": "assistant", "model": "x", "content": [write("t1", 1), write("t2", 2), write("t3", 3), {"type": "tool_use", "id": "t4", "name": "Read", "input": {"file_path": "/w/a.txt"}}]}, "parent_tool_use_id": null, "session_id": "s", "uuid": "a"})
                .to_string(),
        );
        let status = |core: &Core, id: &str| tool(core, id).map(|t| t.status);
        assert_eq!(status(&core, "t2"), Some(ToolStatus::Running));
        c.line(&mut core, &json!({"type": "control_request", "request_id": "p1", "request": {"subtype": "can_use_tool", "tool_name": "Write", "input": {"file_path": "/w/f1.txt", "content": "x"}, "tool_use_id": "t1"}}).to_string());
        assert_eq!(
            ["t1", "t2", "t3", "t4"].map(|t| status(&core, t)),
            [Some(ToolStatus::Pending), Some(ToolStatus::Pending), Some(ToolStatus::Pending), Some(ToolStatus::Running)]
        );
        c.command(&mut core, Command::Approve { id: "p1".into(), decision: Decision::Allow, message: None, always: None, answers: None, choices: None });
        assert_eq!(["t1", "t2", "t3"].map(|t| status(&core, t)), [Some(ToolStatus::Running), Some(ToolStatus::Pending), Some(ToolStatus::Pending)]);
        // The first is done: the next is under way (until it asks too).
        c.line(&mut core, &json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok", "is_error": false}]}, "parent_tool_use_id": null, "session_id": "s", "uuid": "r1"}).to_string());
        assert_eq!(["t1", "t2", "t3"].map(|t| status(&core, t)), [Some(ToolStatus::Done), Some(ToolStatus::Running), Some(ToolStatus::Pending)]);
        c.line(&mut core, &json!({"type": "control_request", "request_id": "p2", "request": {"subtype": "can_use_tool", "tool_name": "Write", "input": {"file_path": "/w/f2.txt", "content": "x"}, "tool_use_id": "t2"}}).to_string());
        assert_eq!(["t2", "t3"].map(|t| status(&core, t)), [Some(ToolStatus::Pending), Some(ToolStatus::Pending)]);
        assert_eq!(core.state.approvals.len(), 1);
    }

    #[test]
    fn shells_and_monitors_it_leaves_running() -> anyhow::Result<()> {
        let mut c = Claude::new(false);
        let mut core = Core::new();
        core.state.cwd = "/work".into();
        // A background command: its call, its task, its output file.
        c.line(&mut core, &json!({"type": "assistant", "message": {"id": "m1", "type": "message", "role": "assistant", "model": "x", "content": [
            {"type": "tool_use", "id": "tb", "name": "Bash", "input": {"command": "npm run dev", "description": "dev server", "run_in_background": true}},
            {"type": "tool_use", "id": "tm", "name": "Monitor", "input": {"command": "tail -f app.log | grep ERROR", "description": "errors in the log"}}
        ]}, "parent_tool_use_id": null, "session_id": "s", "uuid": "a"}).to_string());
        c.line(&mut core, &json!({"type": "system", "subtype": "task_started", "task_id": "b7", "tool_use_id": "tb", "description": "dev server", "task_type": "local_bash", "is_backgrounded": true, "uuid": "u", "session_id": "s"}).to_string());
        c.line(&mut core, &json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tb", "content": "Command running in background with ID: b7. Output is being written to: /tmp/claude-1000/-work/s/tasks/b7.output", "is_error": false}]}, "parent_tool_use_id": null, "session_id": "s", "uuid": "r"}).to_string());
        c.line(&mut core, &json!({"type": "system", "subtype": "task_started", "task_id": "m9", "tool_use_id": "tm", "description": "errors in the log", "task_type": "local_bash", "uuid": "u", "session_id": "s"}).to_string());
        let shell = core.state.tasks.iter().find(|t| t.id == "b7").cloned().ok_or_else(|| anyhow::anyhow!("no shell"))?;
        assert_eq!((shell.class.as_deref(), shell.command.as_deref(), shell.cwd.as_deref()), (Some("shell"), Some("npm run dev"), Some("/work")));
        assert_eq!(shell.output.as_deref(), Some("/tmp/claude-1000/-work/s/tasks/b7.output"));
        assert!(shell.stoppable && shell.background);
        let mon = core.state.tasks.iter().find(|t| t.id == "m9").cloned().ok_or_else(|| anyhow::anyhow!("no monitor"))?;
        assert_eq!((mon.class.as_deref(), mon.command.as_deref(), mon.wakes.as_deref()), (Some("monitor"), Some("tail -f app.log | grep ERROR"), Some("agent")));
        // Its events wake the agent: counted, the latest kept.
        c.line(&mut core, &json!({"type": "user", "message": {"role": "user", "content": "<system-reminder>\n<task-notification>\n<task-id>m9</task-id>\n<status>running</status>\n<summary>ERROR db down</summary>\n</task-notification>\n</system-reminder>"}, "parent_tool_use_id": null, "session_id": "s", "uuid": "e1"}).to_string());
        let mon = core.state.tasks.iter().find(|t| t.id == "m9").cloned().unwrap_or_default();
        assert_eq!((mon.events, mon.last_event.as_deref()), (1, Some("ERROR db down")));
        // The turn is over; both are left running.
        c.line(&mut core, r#"{"type":"system","subtype":"session_state_changed","state":"idle","uuid":"u","session_id":"s"}"#);
        core.set_status(Status::Idle);
        let _ = core.take_out();
        assert_eq!(core.state.count_left_behind(), 2);
        // Stopping them all.
        let _ = core.take_writes();
        c.command(&mut core, Command::StopTasks);
        let w = core.take_writes();
        assert!(w.iter().any(|l| l.contains("\"stop_task\"") && l.contains("b7")) && w.iter().any(|l| l.contains("m9")), "{w:?}");
        c.line(&mut core, &json!({"type": "system", "subtype": "task_notification", "task_id": "b7", "tool_use_id": "tb", "status": "stopped", "output_file": "/tmp/claude-1000/-work/s/tasks/b7.output", "summary": "Background command \"dev server\" was stopped (exit code 143)", "uuid": "u", "session_id": "s"}).to_string());
        let shell = core.state.tasks.iter().find(|t| t.id == "b7").cloned().unwrap_or_default();
        assert_eq!((shell.status.as_str(), shell.exit_code), ("stopped", Some(143)));
        assert_eq!(core.state.count_left_behind(), 1);
        // What a background_tasks_changed lists and kiln had missed joins.
        c.line(&mut core, &json!({"type": "system", "subtype": "background_tasks_changed", "tasks": [{"task_id": "w3", "task_type": "local_workflow", "description": "nightly"}], "uuid": "u", "session_id": "s"}).to_string());
        assert_eq!(core.state.tasks.iter().find(|t| t.id == "w3").and_then(|t| t.class.clone()).as_deref(), Some("workflow"));
        // The process ends: so do they.
        c.exited(&mut core, Some(0), None);
        assert!(core.state.tasks.iter().all(|t| !t.live()));
        Ok(())
    }
}
