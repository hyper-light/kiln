//! Codex, driven through `codex app-server`: the JSON-RPC protocol (v2) its IDE extensions use.
//! kiln starts or resumes a thread, runs turns (`turn/start`), steers a running one
//! (`turn/steer`), interrupts it, answers the approval requests Codex sends (commands, file
//! changes, questions) and renders the thread's items: messages as they stream, reasoning
//! summaries, commands with their output and exit codes, file changes as real diffs, MCP calls,
//! web searches, subagents, the plan.
//!
//! Shapes follow the schema `codex app-server generate-json-schema` prints for codex-cli 0.162.1.
//! Unknown methods and fields are ignored.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::core::{Baseline, Core, Effect};
use super::model::{
    AlwaysOption, Approval, Body, Command, Control, ControlKind, ControlOption, ControlScope, Decision, Delivery, ExitInfo, FileDiff, HookInfo, Known, Level, Limit, McpServer,
    McpStatus, McpTool, ModeOption, ModelOption, Objective, Plugin, Question, QuestionOption, QueueItem, SendMode, Skill, SlashCommand, Status, StatusItem, Task, Todo, Tone, ToolCall,
    ChoiceAnswer, ToolKind, ToolStatus, OUTPUT_MAX, Route, account_id, choices_of, clip_json, clip_middle, mask_emails, plan_label, safe_url, sort_catalog, window_words,
};
use super::claude::{McpAdd, mcp_status_words, safe_command, valid_server_name};

/// How kiln starts Codex.
pub fn args() -> Vec<String> {
    vec!["app-server".to_owned()]
}

/// Codex's approval presets, as kiln offers them.
pub fn modes(allow_bypass: bool) -> Vec<ModeOption> {
    let mut m = vec![
        ModeOption { id: "auto".into(), label: "Auto".into(), description: "Works in its folder; asks before going beyond it".into(), dangerous: false },
        ModeOption { id: "read-only".into(), label: "Read only".into(), description: "Reads and plans; asks before any change".into(), dangerous: false },
        ModeOption { id: "untrusted".into(), label: "Ask".into(), description: "Asks before running anything it does not trust".into(), dangerous: false },
    ];
    if allow_bypass {
        m.push(ModeOption { id: "full-access".into(), label: "Full access".into(), description: "No sandbox, never asks".into(), dangerous: true });
    }
    m
}

/// (approval policy, sandbox mode for threads, sandbox policy for turns) of a kiln mode.
fn policy(mode: &str) -> (&'static str, &'static str, Value) {
    match mode {
        "read-only" => ("on-request", "read-only", json!({"type": "readOnly"})),
        "untrusted" => ("untrusted", "workspace-write", json!({"type": "workspaceWrite"})),
        "full-access" => ("never", "danger-full-access", json!({"type": "dangerFullAccess"})),
        _ => ("on-request", "workspace-write", json!({"type": "workspaceWrite"})),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum Pending {
    Initialize,
    Thread,
    /// thread/turns/list: a resumed thread's history, page by page.
    History,
    Turn(String),
    /// turn/steer for a message; the message itself (to send it as the next turn should the
    /// running one end first), unless kiln wrote it (feedback).
    Steer(String, Option<QueueItem>),
    Interrupt,
    Models,
    Suggest(String),
    Skills,
    Hooks,
    Mcp,
    /// thread/revert, cutting the timeline from this event.
    Revert(String),
    /// A request that just needs to succeed (its failure is shown).
    Plain(String),
    Account,
    RateLimits,
    /// config/read: the MCP servers' configuration.
    Config,
    Plugins,
    /// A change to the MCP configuration (what, for a failure): the servers are reloaded after.
    McpConfig(String),
    /// config/mcpServer/reload: the servers are asked for again after.
    McpReload,
    McpAuth { client: String, name: String },
    /// An http server written into the configuration (answered to `client`).
    McpAdded { name: String, client: Option<String> },
    /// thread/goal/get or set: the goal, shown to `client`.
    Goal(String),
    /// thread/inject_items: a conversation carried over from another harness, into the new thread.
    Inject,
}

/// A run of the CLI (or a fetch) kiln is waiting on.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum Run {
    McpAdd { name: String, client: Option<String> },
    McpRemove { name: String, client: Option<String> },
    /// Forgetting an http server's sign-in before removing it (as Claude Code does).
    McpLogout { name: String, client: Option<String> },
    /// The last page of an MCP sign-in, fetched on Codex's machine.
    Callback { name: String, client: String },
}

/// How often the plan's rate limits are read while a session runs (Codex also pushes them
/// during turns).
const USAGE_EVERY_MS: i64 = 180_000;
/// How often kiln opens a thread Codex cannot open yet (once a tick, two minutes in all): it
/// refuses to fork a thread whose history it is still writing out, and to resume one its
/// app-server daemon still holds for a terminal that just quit (for about a minute).
const OPEN_TRIES: u32 = 24;
/// What the console shows meanwhile.
const CATCHING_UP: &str = "Waiting for Codex to catch up with the conversation";
const LETTING_GO: &str = "Waiting for Codex to let go of the conversation (it still holds it for the terminal it ran in)";

/// Turns of a resumed thread's history loaded (the latest).
const HISTORY_TURNS: u32 = 100;

/// Whether a turn's error is the account's usage limit: `codexErrorInfo` says so
/// (`usageLimitExceeded`, as a word or an object's key), or the message does.
fn usage_limited(err: &Value) -> bool {
    let info = err.get("codexErrorInfo").or_else(|| err.get("codex_error_info"));
    let said = match info {
        Some(Value::String(s)) => Some(s.as_str()),
        Some(Value::Object(o)) => o.keys().next().map(String::as_str),
        _ => None,
    };
    said.is_some_and(|s| s.eq_ignore_ascii_case("usageLimitExceeded") || s.eq_ignore_ascii_case("usage_limit_exceeded"))
        || err.get("message").and_then(Value::as_str).is_some_and(super::pause::says_limit)
}

/// Why Codex cannot open a thread yet, when that passes.
fn not_yet(msg: &str) -> Option<&'static str> {
    if msg.contains("behind durable rollout") {
        Some(CATCHING_UP)
    } else if msg.contains("already has an active writer") {
        Some(LETTING_GO)
    } else {
        None
    }
}

/// Reasoning efforts every Codex reasoning model takes, offered for a model model/list doesn't
/// list.
pub(super) const EFFORTS: &[&str] = &["low", "medium", "high"];

/// The efforts a new Codex session can start at, before its model/list answers: its default
/// model's, as Codex on this machine last listed them, else the newest model kiln knows of
/// (`kinds::models_of`).
pub(super) fn default_efforts() -> Vec<String> {
    let models = super::kinds::models_of("codex");
    models.iter().find(|m| m.default).or_else(|| models.first()).map(|m| m.efforts.clone()).filter(|e| !e.is_empty()).unwrap_or_else(|| EFFORTS.iter().map(|e| (*e).to_owned()).collect())
}

/// An approval request Codex is waiting on.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Ask {
    /// The JSON-RPC id to answer under.
    rpc: Value,
    kind: AskKind,
    /// Question ids by question text (user-input requests).
    questions: Vec<(String, String)>,
    /// "decline" is on offer (the turn goes on with the refusal); otherwise only "cancel".
    can_decline: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum AskKind {
    Command,
    FileChange,
    UserInput,
    Permissions,
    LegacyExec,
    LegacyPatch,
    /// An MCP server asks the person for input (MCP elicitation).
    Elicitation,
}

/// Its whole state is plain data, written out when this process re-executes itself (an in-place
/// upgrade) and read back by the next image, which goes on with the same process.
#[derive(Serialize, Deserialize)]
pub struct Codex {
    next_id: u64,
    pending: HashMap<u64, Pending>,
    asks: HashMap<String, Ask>,
    thread: Option<String>,
    /// The turn under way.
    turn: Option<String>,
    /// Runtime status pushed by Codex, including work started outside kiln.
    #[serde(default)]
    thread_status: Option<Status>,
    /// The message that started (or was steered into) the turn under way.
    turn_messages: Vec<String>,
    /// Messages to send once the thread exists or the turn under way has been interrupted.
    waiting: Vec<QueueItem>,
    /// The reasoning effort for turns (None: the model's default).
    effort: Option<String>,
    resume: Option<String>,
    model: Option<String>,
    allow_bypass: bool,
    notice_seq: u64,
    mode: String,
    /// Reasoning summaries in turns (auto, concise, detailed, none).
    summary: Option<String>,
    /// Approval policy and sandbox chosen directly (over the mode's).
    approval: Option<String>,
    sandbox: Option<String>,
    /// Effort levels per model, from model/list.
    efforts: HashMap<String, Vec<(String, String)>>,
    /// The turn each prompt started (rewinds and branches name a prompt).
    turns: HashMap<String, String>,
    /// Skills (custom commands): name → path.
    skills: HashMap<String, String>,
    /// A new thread branched off `resume` at this turn (instead of resuming it).
    fork_at: Option<Option<String>>,
    /// The MCP servers as Codex reports them (mcpServerStatus/list).
    mcp_rows: Vec<Value>,
    /// Their configuration (config/read), by name.
    mcp_config: serde_json::Map<String, Value>,
    /// Their start-up state as Codex pushes it: name → (state, error).
    mcp_startup: HashMap<String, (String, Option<String>)>,
    /// The account's rate-limit snapshots by limit id (rolling updates merged in).
    rate_limits: std::collections::BTreeMap<String, Value>,
    /// How the session is signed in ("chatgpt", "apiKey", ...) and as whom (masked).
    account_kind: Option<String>,
    account_email: Option<String>,
    /// The model provider the thread runs on.
    provider: Option<String>,
    /// Installed plugins as plugin/installed lists them (marketplace name, plugin).
    plugins_raw: Vec<(String, Value)>,
    runs: HashMap<String, Run>,
    next_run: u64,
    usage_at: i64,
    mcp_at: i64,
    /// The MCP configuration and the servers' status have each been read once.
    mcp_config_seen: bool,
    mcp_status_seen: bool,
    /// The servers were asked for again while an answer was on its way (it may be from before a
    /// reload): ask once more when it comes.
    mcp_again: bool,
    /// Terminals kiln is stopping: their end reads as stopped, not failed.
    stopping: std::collections::HashSet<String>,
    /// Queued messages to steer into the turn about to start, once it has (it has no id yet).
    steer_on_start: Vec<String>,
    /// The thread's rollout file.
    rollout: Option<String>,
    /// The thread open kiln sent (method, params), how often, and whether to send it again at
    /// the next tick.
    opening: Option<(String, Value)>,
    open_tries: u32,
    reopen: bool,
    /// Settings changed outside it (its own screen): it starts again to load them once idle.
    reload: Option<String>,
    /// A conversation carried over from another harness (Responses items), put into the new
    /// thread before its first turn; whether it is in, and while it goes in.
    seed: Option<Vec<Value>>,
    seeded: bool,
    injecting: bool,
}

impl Codex {
    pub fn from_spec(spec: &super::session::Spec) -> Self {
        let mut c = Self::new(spec.resume.clone(), spec.model.clone(), spec.mode.clone(), spec.allow_bypass);
        c.effort = spec.effort.clone().filter(|e| !e.is_empty());
        if spec.fork {
            c.fork_at = Some(spec.resume_at.clone());
        }
        c
    }

    pub fn new(resume: Option<String>, model: Option<String>, mode: Option<String>, allow_bypass: bool) -> Self {
        let mode = mode.filter(|m| modes(allow_bypass).iter().any(|x| &x.id == m)).unwrap_or_else(|| "auto".to_owned());
        Self {
            next_id: 0,
            pending: HashMap::new(),
            asks: HashMap::new(),
            thread: None,
            turn: None,
            thread_status: None,
            turn_messages: Vec::new(),
            waiting: Vec::new(),
            effort: None,
            resume,
            model: model.filter(|m| !m.is_empty()),
            allow_bypass,
            notice_seq: 0,
            mode,
            summary: None,
            approval: None,
            sandbox: None,
            efforts: HashMap::new(),
            turns: HashMap::new(),
            skills: HashMap::new(),
            fork_at: None,
            mcp_rows: Vec::new(),
            mcp_config: serde_json::Map::new(),
            mcp_startup: HashMap::new(),
            rate_limits: std::collections::BTreeMap::new(),
            account_kind: None,
            account_email: None,
            provider: None,
            plugins_raw: Vec::new(),
            runs: HashMap::new(),
            next_run: 0,
            usage_at: 0,
            mcp_at: 0,
            mcp_config_seen: false,
            mcp_status_seen: false,
            mcp_again: false,
            stopping: std::collections::HashSet::new(),
            steer_on_start: Vec::new(),
            rollout: None,
            opening: None,
            open_tries: 0,
            reopen: false,
            reload: None,
            seed: None,
            seeded: false,
            injecting: false,
        }
    }

    /// Start the new thread with this conversation (one carried over from another harness).
    pub fn seed(&mut self, items: Vec<Value>) {
        self.seeded = items.is_empty();
        self.seed = (!items.is_empty()).then_some(items);
    }

    /// The conversation it was given is in its thread (or it was given none).
    pub fn seeded(&self) -> bool {
        self.seeded || (self.seed.is_none() && !self.injecting)
    }

    /// The approval policy, sandbox (thread) and sandbox policy (turn) in force.
    fn policies(&self, core: &Core) -> (String, String, Value) {
        let (a, s, p) = policy(&core.state.mode);
        let approval = self.approval.clone().unwrap_or_else(|| a.to_owned());
        let sandbox = self.sandbox.clone().unwrap_or_else(|| s.to_owned());
        let turn = match sandbox.as_str() {
            "read-only" => json!({"type": "readOnly"}),
            "danger-full-access" => json!({"type": "dangerFullAccess"}),
            "workspace-write" => json!({"type": "workspaceWrite"}),
            _ => p,
        };
        (approval, sandbox, turn)
    }

    fn refresh_controls(&mut self, core: &mut Core) {
        let opt = |id: &str, label: &str, description: &str, dangerous: bool| ControlOption { id: id.to_owned(), label: label.to_owned(), description: description.to_owned(), dangerous };
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
        let mut t = ctl("reasoning_summary", "Thinking", "Reasoning summaries", "How much of its reasoning Codex summarizes", ControlKind::Select);
        t.options = vec![opt("auto", "Auto", "", false), opt("concise", "Concise", "", false), opt("detailed", "Detailed", "", false), opt("none", "None", "", false)];
        c.push(t);
        let mut t = ctl("approval_policy", "Permissions", "Approvals", "When Codex asks before acting", ControlKind::Select);
        t.options = vec![
            opt("untrusted", "Untrusted", "Asks before anything not known to be safe", false),
            opt("on-request", "On request", "Asks when it needs more than its sandbox allows", false),
            opt("never", "Never", "Never asks", true),
        ];
        c.push(t);
        let mut t = ctl("sandbox", "Permissions", "Sandbox", "What commands may touch", ControlKind::Select);
        t.options = vec![
            opt("read-only", "Read only", "No writes anywhere", false),
            opt("workspace-write", "Workspace", "Writes inside the working folder", false),
            opt("danger-full-access", "Full access", "No sandbox at all", true),
        ];
        if !self.allow_bypass {
            t.options.retain(|o| !o.dangerous);
            if let Some(a) = c.iter_mut().find(|c| c.id == "approval_policy") {
                a.options.retain(|o| !o.dangerous);
            }
        }
        c.push(t);
        c.push(ctl("compact", "Context", "Compact", "Summarize the conversation to free context", ControlKind::Action));
        let mut t = ctl("review", "Context", "Review changes", "Have Codex review the uncommitted changes (or follow your instructions)", ControlKind::Action);
        t.input = Some("Review instructions (optional)".into());
        c.push(t);
        c.push(ctl("rename", "Session", "Name", "The conversation's name", ControlKind::Text));
        let mut t = ctl("rewind", "Session", "Rewind to here", "Undo this prompt and everything after it", ControlKind::Action);
        t.scope = ControlScope::Turn;
        t.dangerous = true;
        c.push(t);
        let mut t = ctl("branch", "Session", "Branch from here", "A new session with the conversation up to the end of this turn", ControlKind::Action);
        t.scope = ControlScope::Turn;
        c.push(t);
        // MCP servers and skills have their own state and commands (State::mcp, State::skills).
        core.meta.controls = c;
        let model = core.state.model.clone();
        core.meta.efforts = match self.efforts.get(&model).or_else(|| self.efforts.get("")) {
            Some(l) => l.iter().map(|(e, d)| opt(e, &super::model::effort_label(e), d, false)).collect(),
            None => default_efforts().iter().map(|e| opt(e, &super::model::effort_label(e), "", false)).collect(),
        };
        core.touch_meta();
    }

    fn control(&mut self, core: &mut Core, id: &str, value: Option<Value>) {
        let text = || value.as_ref().and_then(|v| v.as_str().map(str::to_owned).or_else(|| v.get("text").and_then(Value::as_str).map(str::to_owned))).unwrap_or_default();
        let event = || value.as_ref().and_then(|v| v.get("event")).and_then(Value::as_str).map(str::to_owned);
        let client = value.as_ref().and_then(|v| v.get("client")).and_then(Value::as_str).unwrap_or("").to_owned();
        // Signed in on this machine (kiln's /login): what account it runs on now.
        if id == "account" {
            self.request(core, Pending::Account, "account/read", json!({}));
            return;
        }
        // Answered from what kiln knows, thread or not.
        if matches!(id, "export" | "rollout" | "pwd") {
            let reply = match id {
                "export" => json!({"title": "Conversation", "text": core.export(), "format": "markdown", "filename": "conversation.md"}),
                "rollout" => json!({"title": "Rollout file", "text": self.rollout.clone().unwrap_or_else(|| "Not known yet.".to_owned())}),
                _ => json!({"title": "Working directory", "text": core.state.cwd.clone()}),
            };
            core.reply(client, reply);
            return;
        }
        let Some(thread) = self.thread.clone() else {
            if id == "goal" {
                core.reply(client, json!({"error": "Codex has not started the conversation yet."}));
            }
            return;
        };
        match id {
            "reasoning_summary" => {
                self.summary = Some(text());
                core.set_control(id, json!(text()));
            }
            "approval_policy" | "sandbox" => {
                let v = text();
                let dangerous = matches!(v.as_str(), "never" | "danger-full-access");
                if dangerous && !self.allow_bypass {
                    self.notice(core, Level::Warn, "That needs the session to be started with bypass allowed.");
                    return;
                }
                if id == "sandbox" {
                    self.sandbox = Some(v.clone());
                } else {
                    self.approval = Some(v.clone());
                }
                core.set_control(id, json!(v));
            }
            "compact" => self.request(core, Pending::Plain("compact".into()), "thread/compact/start", json!({"threadId": thread})),
            "review" => {
                let t = text();
                let target = if t.trim().is_empty() { json!({"type": "uncommittedChanges"}) } else { json!({"type": "custom", "instructions": t}) };
                self.request(core, Pending::Plain("review".into()), "review/start", json!({"threadId": thread, "target": target}));
            }
            "rename" => {
                let t = text();
                self.request(core, Pending::Plain("rename".into()), "thread/name/set", json!({"threadId": thread, "name": t}));
                core.state.title.clone_from(&t);
                core.set_control(id, json!(t));
            }
            "rewind" => {
                let Some(ev) = event() else { return };
                let Some(turn) = self.turns.get(&ev).cloned() else {
                    self.notice(core, Level::Warn, "That prompt's turn is not known.");
                    return;
                };
                self.request(core, Pending::Revert(ev), "thread/revert", json!({"threadId": thread, "beforeTurnId": turn}));
            }
            "branch" => {
                let Some(ev) = event() else { return };
                let Some(turn) = self.turns.get(&ev).cloned() else {
                    self.notice(core, Level::Warn, "That prompt's turn is not known.");
                    return;
                };
                core.effects.push(Effect::Branch { resume: thread, resume_at: Some(turn) });
            }
            // Run by the command catalog.
            // As Codex's own: /goal [<objective>|clear|pause|resume].
            "goal" => {
                let t = text();
                let (method, params) = match t.trim() {
                    "" => ("thread/goal/get", json!({"threadId": thread})),
                    "clear" => ("thread/goal/clear", json!({"threadId": thread, "origin": "user"})),
                    "pause" => ("thread/goal/set", json!({"threadId": thread, "status": "paused", "origin": "user"})),
                    "resume" => ("thread/goal/set", json!({"threadId": thread, "status": "active", "origin": "user"})),
                    objective => ("thread/goal/set", json!({"threadId": thread, "objective": objective, "origin": "user"})),
                };
                self.request(core, Pending::Goal(client), method, params);
            }
            "fork" => core.effects.push(Effect::Branch { resume: thread, resume_at: None }),
            "undo" => {
                // The last prompt's turn, undone with everything after it.
                let last = core.events().rfind(|e| matches!(&e.body, Body::User { origin, .. } if origin.as_deref() != Some("system")) && self.turns.contains_key(&e.id)).map(|e| e.id.clone());
                match last {
                    Some(ev) => self.control(core, "rewind", Some(json!({"event": ev}))),
                    None => self.notice(core, Level::Info, "Nothing to undo"),
                }
            }
            // Older consoles' MCP controls (now commands of their own).
            "mcp_refresh" => self.command(core, Command::McpRefresh),
            "mcp_reload" => self.mcp_reload(core),
            // Settings changed outside it (its own screen, beside it): start again to load them.
            "reload_config" => {
                self.reload = Some(value.as_ref().and_then(|v| v.get("what")).and_then(Value::as_str).unwrap_or("Changed settings").to_owned());
                self.reload_when_idle(core);
            }
            _ => {}
        }
        self.compose_status(core);
    }

    fn request(&mut self, core: &mut Core, kind: Pending, method: &str, params: Value) {
        self.next_id += 1;
        self.pending.insert(self.next_id, kind);
        core.write(&json!({"id": self.next_id, "method": method, "params": params}));
    }

    fn notice(&mut self, core: &mut Core, level: Level, text: impl Into<String>) {
        self.notice_seq += 1;
        core.upsert(&format!("notice-{}", self.notice_seq), None, Body::Notice { level, text: text.into() });
    }

    fn busy(&self, core: &Core) -> bool {
        self.injecting || self.turn.is_some() || matches!(self.thread_status, Some(Status::Running | Status::Waiting)) || !core.state.approvals.is_empty() || self.pending.values().any(|p| matches!(p, Pending::Turn(_)))
    }

    /// Start again with the changed settings (resuming the thread), unless a turn is under way or
    /// it left background terminals running: then once it's idle and they've ended.
    fn reload_when_idle(&mut self, core: &mut Core) {
        if self.reload.is_none() || self.busy(core) || core.state.status == Status::Starting || core.state.tasks.iter().any(|t| t.left_running()) {
            return;
        }
        let Some(what) = self.reload.take() else { return };
        // Not running, or nothing to resume: its next start reads the settings anyway.
        if core.state.status == Status::Exited || core.state.harness_session.is_none() {
            return;
        }
        self.notice(core, Level::Info, format!("{what}: Codex starts again to load it (the conversation continues)"));
        core.effects.push(Effect::Restart { resume_at: None });
    }

    fn settle(&self, core: &mut Core) {
        let running = self.turn.is_some() || self.pending.values().any(|p| matches!(p, Pending::Turn(_))) || matches!(self.thread_status, Some(Status::Running | Status::Waiting));
        if let Some(status @ (Status::Waiting | Status::Starting | Status::Exited)) = self.thread_status {
            core.set_status(status);
        } else {
            core.settle_status(running);
        }
    }

    pub fn start(&mut self, core: &mut Core) {
        // Codex's commands (skills join when they are listed).
        if core.meta.commands.is_empty() {
            core.meta.commands = sort_catalog(codex_catalog());
        }
        // Effort can be set from the start (model/list narrows the levels to the model's).
        if core.meta.efforts.is_empty() {
            core.meta.efforts = default_efforts().iter().map(|e| ControlOption { id: e.clone(), label: super::model::effort_label(e), description: String::new(), dangerous: false }).collect();
        }
        if !modes(self.allow_bypass).iter().any(|m| m.id == core.state.mode) {
            core.state.mode = self.mode.clone();
            core.touch_state();
        }
        // Full access is always a choice; choosing it allows it.
        core.meta.modes = modes(true);
        core.meta.features = ["steer", "send_now", "queue", "queue_steer", "interrupt", "set_mode", "set_model", "set_effort", "file_suggestions", "resume", "attachments", "images", "overrides", "switch_harness"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        core.touch_meta();
        self.request(
            core,
            Pending::Initialize,
            "initialize",
            json!({"clientInfo": {"name": "kiln", "title": "kiln", "version": env!("CARGO_PKG_VERSION")}, "capabilities": {"experimentalApi": true}}),
        );
    }

    pub fn exited(&mut self, core: &mut Core, code: Option<i32>, error: Option<String>) {
        // What kiln took back over runs apart from it (`survivors`).
        for t in core.state.tasks.iter_mut().filter(|t| t.live() && !t.id.starts_with(super::survivors::LEFT)) {
            t.status = "stopped".into();
            t.ended.get_or_insert_with(Core::now);
        }
        self.turn = None;
        self.thread_status = None;
        self.pending.clear();
        self.asks.clear();
        core.state.approvals.clear();
        core.state.exit = Some(ExitInfo { code, error });
        core.state.activity = None;
        core.state.doing = None;
        core.state.turn_started = None;
        core.set_status(Status::Exited);
        core.touch_state();
    }

    // ------------------------------------------------------------------ commands

    pub fn command(&mut self, core: &mut Core, cmd: Command) {
        match cmd {
            Command::Send { id, text, mode, attachments, overrides } => self.send(core, QueueItem { id, text, at: Core::now(), attachments, overrides }, mode),
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
                    self.send(core, item, SendMode::Now);
                }
            }
            Command::QueueSteer { id } => {
                let Some(pos) = core.state.queue.iter().position(|q| q.id == id) else { return };
                // A turn about to start has no id to steer into yet: once it has.
                if self.busy(core) && self.turn.is_none() {
                    if !self.steer_on_start.contains(&id) {
                        self.steer_on_start.push(id);
                    }
                    return;
                }
                let item = core.state.queue.remove(pos);
                core.touch_state();
                self.send(core, item, SendMode::Steer);
            }
            Command::Approve { id, decision, message, always, answers, choices } => self.answer(core, &id, decision, message, always, choices_of(answers, choices)),
            Command::Interrupt => self.interrupt(core),
            Command::SetMode { mode } => {
                // Takes effect from the next turn (Codex sets policy per turn): the mode's approval
                // policy and sandbox, over any chosen one by one before.
                if modes(true).iter().any(|m| m.id == mode) {
                    if mode == "full-access" && !self.allow_bypass {
                        // Chosen, so allowed: its approval and sandbox settings come with it.
                        self.allow_bypass = true;
                        self.refresh_controls(core);
                    }
                    self.approval = None;
                    self.sandbox = None;
                    let (a, s, _) = policy(&mode);
                    core.set_control("approval_policy", json!(a));
                    core.set_control("sandbox", json!(s));
                    core.state.mode = mode;
                    core.touch_state();
                } else {
                    self.notice(core, Level::Warn, format!("{mode}: not a mode this session offers"));
                }
            }
            Command::SetModel { model, .. } => {
                self.model = Some(model.clone()).filter(|m| !m.is_empty());
                core.state.model = model;
                core.touch_state();
            }
            Command::Suggest { client, query } => {
                let root = core.state.cwd.clone();
                self.request(core, Pending::Suggest(client), "fuzzyFileSearch", json!({"query": query, "roots": [root]}));
            }
            Command::SetEffort { effort } => {
                // Codex takes effort per turn: from the next one.
                self.effort = Some(effort.clone()).filter(|e| !e.is_empty());
                core.state.effort = effort;
                core.touch_state();
            }
            Command::Control { id, value } => self.control(core, &id, value),
            // The session itself does these (a move is not the harness's business).
            Command::Restart | Command::ForceStop | Command::Snapshot | Command::MoveTo { .. } | Command::Moving { .. } => {}
            // A terminal Codex left running: Codex has no way to stop it from outside, so kiln stops
            // the process itself (the newest one under Codex running that command).
            Command::StopTask { task } => self.stop_terminal(core, &task),
            Command::StopTasks => {
                let live: Vec<String> = core.state.tasks.iter().filter(|t| t.live() && t.process.is_some()).map(|t| t.id.clone()).collect();
                for task in live {
                    self.stop_terminal(core, &task);
                }
            }
            // A pause's commands are the session's (`harness::pause`).
            Command::Resume | Command::AutoResume { .. } | Command::PauseAt { .. } | Command::MoveOnLimit { .. } | Command::LimitHold { .. } => {}
            Command::McpEnable { name, enabled } => {
                if !valid_server_name(&name) {
                    self.notice(core, Level::Warn, format!("{name}: not a server kiln can change"));
                    return;
                }
                let what = format!("{} {name}", if enabled { "Enable" } else { "Disable" });
                self.request(core, Pending::McpConfig(what), "config/value/write", json!({"keyPath": format!("mcp_servers.{name}.enabled"), "value": enabled, "mergeStrategy": "replace"}));
                // Shown at once; the reloaded servers settle it.
                if let Some(cfg) = self.mcp_config.get_mut(&name).and_then(Value::as_object_mut) {
                    cfg.insert("enabled".into(), json!(enabled));
                }
                self.mcp_startup.remove(&name);
                self.compose_mcp(core);
            }
            // Codex reloads its servers all at once.
            Command::McpReconnect { name } => {
                self.mcp_startup.insert(name, ("starting".to_owned(), None));
                self.compose_mcp(core);
                self.mcp_reload(core);
            }
            Command::McpRefresh => {
                self.request(core, Pending::Config, "config/read", json!({"cwd": core.state.cwd.clone()}));
                self.ask_mcp(core);
            }
            Command::McpAuth { name, client, redirect } => match redirect.filter(|r| !r.trim().is_empty()) {
                None => {
                    let mut params = json!({"name": name});
                    if let (Some(t), Some(o)) = (&self.thread, params.as_object_mut()) {
                        o.insert("threadId".into(), json!(t));
                    }
                    self.request(core, Pending::McpAuth { client, name }, "mcpServer/oauth/login", params);
                }
                // The sign-in's last page could not reach Codex (another device): open it on its machine.
                Some(url) => {
                    self.next_run += 1;
                    let job = format!("fetch-{}", self.next_run);
                    self.runs.insert(job.clone(), Run::Callback { name, client });
                    core.effects.push(Effect::Fetch { job, url: url.trim().to_owned() });
                }
            },
            Command::McpAdd { scope, name, transport, command, args, env, url, headers, client } => {
                let add = McpAdd { scope, name, transport, command, args, env, url, headers };
                if self.mcp_config.contains_key(&add.name) {
                    answer(core, client, json!({"ok": false, "error": format!("There is already a server named {}.", add.name)}));
                    return;
                }
                match (codex_add_args(&add), add.transport().as_str()) {
                    (Err(e), _) => answer(core, client, json!({"ok": false, "error": e})),
                    // `codex mcp add --url` signs in at once (opening a browser on Codex's machine and
                    // waiting for it): the server goes into Codex's configuration through its own
                    // API instead, and signing in is kiln's (McpAuth), from wherever the person is.
                    (Ok(_), "http") => {
                        let mut server = json!({"url": add.url.clone().unwrap_or_default().trim()});
                        if !add.headers.is_empty()
                            && let Some(o) = server.as_object_mut()
                        {
                            o.insert("http_headers".into(), json!(add.headers));
                        }
                        let key = format!("mcp_servers.{}", add.name);
                        self.request(core, Pending::McpAdded { name: add.name, client }, "config/value/write", json!({"keyPath": key, "value": server, "mergeStrategy": "replace"}));
                    }
                    (Ok(a), _) => self.run(core, Run::McpAdd { name: add.name.clone(), client }, a),
                }
            }
            Command::McpRemove { name, client, .. } => {
                if !valid_server_name(&name) {
                    answer(core, client, json!({"ok": false, "error": "That is not a server name."}));
                    return;
                }
                // An http server's sign-in is forgotten first (`codex mcp remove` keeps it).
                if self.mcp_config.get(&name).is_some_and(|c| c.get("url").is_some()) {
                    self.run(core, Run::McpLogout { name: name.clone(), client }, vec!["mcp".into(), "logout".into(), "--".into(), name]);
                } else {
                    self.run(core, Run::McpRemove { name: name.clone(), client }, vec!["mcp".into(), "remove".into(), "--".into(), name]);
                }
            }
            Command::SkillView { name, client } => match core.state.skills.iter().find(|s| s.name == name).and_then(|s| s.path.clone()) {
                Some(path) => core.effects.push(Effect::Read { client, paths: vec![path] }),
                None => core.reply(client, json!({"error": format!("This session has no skill named {name}.")})),
            },
            Command::SkillRun { name, args, id, mode } => {
                let name = name.trim().trim_start_matches(['/', '$']).to_owned();
                if name.is_empty() {
                    return;
                }
                let text = match args.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
                    Some(a) => format!("/{name} {a}"),
                    None => format!("/{name}"),
                };
                self.send(core, QueueItem { id: id.unwrap_or_else(crate::util::new_id), text, at: Core::now(), attachments: Vec::new(), overrides: None }, mode);
            }
            Command::SkillsReload => {
                let cwd = core.state.cwd.clone();
                self.request(core, Pending::Skills, "skills/list", json!({"cwds": [cwd], "forceReload": true}));
            }
        }
        self.compose_status(core);
    }

    fn stop_terminal(&mut self, core: &mut Core, task: &str) {
        let Some(t) = core.state.tasks.iter().find(|t| t.id == task && t.live()) else { return };
        match t.command.clone().filter(|_| t.process.is_some()) {
            Some(command) => {
                self.stopping.insert(task.to_owned());
                core.effects.push(Effect::Kill { job: format!("stop:{task}"), command });
            }
            None => self.notice(core, Level::Warn, format!("{}: Codex has no way to stop it", t.description)),
        }
    }

    fn run(&mut self, core: &mut Core, what: Run, args: Vec<String>) {
        self.next_run += 1;
        let job = format!("run-{}", self.next_run);
        self.runs.insert(job.clone(), what);
        core.effects.push(Effect::Run { job, args });
    }

    /// A run of the CLI (or a fetch) ended.
    pub fn run_done(&mut self, core: &mut Core, job: &str, code: Option<i32>, output: &str) {
        // A terminal kiln stopped.
        if let Some(task) = job.strip_prefix("stop:") {
            self.stopping.remove(task);
            let stopped = code == Some(0);
            if let Some(t) = core.state.tasks.iter_mut().find(|t| t.id == task && t.live()) {
                t.status = "stopped".into();
                t.ended = Some(Core::now());
                if !stopped {
                    t.summary = Some(format!("Not running any more ({output})"));
                }
                core.touch_state();
            }
            set_status(core, task, ToolStatus::Cancelled);
            return;
        }
        let Some(run) = self.runs.remove(job) else { return };
        let out = super::claude::strip_ansi(output).trim().to_owned();
        match run {
            Run::McpAdd { name, client } => {
                if code != Some(0) {
                    answer(core, client, json!({"ok": false, "error": if out.is_empty() { "codex mcp add failed".to_owned() } else { out }}));
                    return;
                }
                answer(core, client, json!({"ok": true, "message": if out.is_empty() { format!("Added MCP server {name}") } else { out }}));
                self.mcp_reload(core);
            }
            // Signed out or never signed in: either way, on to removing it.
            Run::McpLogout { name, client } => self.run(core, Run::McpRemove { name: name.clone(), client }, vec!["mcp".into(), "remove".into(), "--".into(), name]),
            Run::McpRemove { name, client } => {
                if code == Some(0) {
                    answer(core, client, json!({"ok": true, "message": if out.is_empty() { format!("Removed MCP server {name}") } else { out }}));
                    self.mcp_config.remove(&name);
                    self.mcp_reload(core);
                } else {
                    answer(core, client, json!({"ok": false, "error": if out.is_empty() { "codex mcp remove failed".to_owned() } else { out }}));
                }
            }
            Run::Callback { name, client } => match code {
                Some(c) if (200..400).contains(&c) => {
                    core.reply(client, json!({"ok": true, "message": format!("Finishing the sign-in to {name}.")}));
                    self.ask_mcp(core);
                }
                Some(c) => core.reply(client, json!({"ok": false, "error": format!("Codex answered {c}: {out}")})),
                None => core.reply(client, json!({"ok": false, "error": out})),
            },
        }
    }

    /// Every few seconds: a thread open to send again; servers still starting; the plan's rate
    /// limits now and then.
    pub fn tick(&mut self, core: &mut Core) {
        self.reload_when_idle(core);
        if std::mem::take(&mut self.reopen)
            && let Some((method, params)) = self.opening.clone()
        {
            self.open_thread(core, &method, params);
            return;
        }
        if matches!(core.state.status, Status::Starting | Status::Exited) {
            return;
        }
        let now = Core::now();
        if core.state.mcp.iter().any(|m| m.status == McpStatus::Pending) && now - self.mcp_at > 4_000 {
            self.ask_mcp(core);
        }
        if self.account_kind.as_deref() == Some("chatgpt") && now - self.usage_at > USAGE_EVERY_MS && !self.pending.values().any(|p| matches!(p, Pending::RateLimits)) {
            self.usage_at = now;
            self.request(core, Pending::RateLimits, "account/rateLimits/read", json!({}));
        }
    }

    /// thread/start, thread/resume or thread/fork, kept to be sent again.
    fn open_thread(&mut self, core: &mut Core, method: &str, params: Value) {
        self.open_tries += 1;
        self.opening = Some((method.to_owned(), params.clone()));
        self.request(core, Pending::Thread, method, params);
    }

    fn ask_mcp(&mut self, core: &mut Core) {
        if self.pending.values().any(|p| matches!(p, Pending::Mcp)) {
            self.mcp_again = true;
            return;
        }
        self.mcp_at = Core::now();
        let params = match &self.thread {
            Some(t) => json!({"threadId": t}),
            None => json!({}),
        };
        self.request(core, Pending::Mcp, "mcpServerStatus/list", params);
    }

    /// Codex starts its MCP servers again from its configuration.
    fn mcp_reload(&mut self, core: &mut Core) {
        self.request(core, Pending::McpReload, "config/mcpServer/reload", json!({}));
    }

    /// State::mcp from Codex's status rows, its start-up pushes and the configuration.
    fn compose_mcp(&mut self, core: &mut Core) {
        let mut names: Vec<String> = self.mcp_rows.iter().filter_map(|r| r.get("name").and_then(Value::as_str).map(str::to_owned)).collect();
        for n in self.mcp_config.keys() {
            if !names.contains(n) {
                names.push(n.clone());
            }
        }
        let servers: Vec<McpServer> = names
            .into_iter()
            .map(|name| {
                let row = self.mcp_rows.iter().find(|r| r.get("name").and_then(Value::as_str) == Some(name.as_str()));
                let cfg = self.mcp_config.get(&name);
                let startup = self.mcp_startup.get(&name);
                codex_server(&name, row, cfg, startup)
            })
            .collect();
        if core.state.mcp != servers {
            core.state.mcp = servers;
            core.touch_state();
        }
        // Known once both the configuration and the servers' status have been read.
        if self.mcp_config_seen && self.mcp_status_seen && core.state.inventory.mcp != Known::Ready {
            core.state.inventory.mcp = Known::Ready;
            core.touch_state();
        }
        self.compose_status(core);
    }

    /// State::limits from the rate-limit snapshots.
    fn apply_limits(&mut self, core: &mut Core) {
        let account = core.state.account.clone();
        let mut limits = Vec::new();
        for (id, snap) in &self.rate_limits {
            let suffix = match snap.get("limitName").and_then(Value::as_str).filter(|n| !n.is_empty()) {
                Some(n) if id != "codex" => format!(" · {n}"),
                _ => String::new(),
            };
            let reached = snap.get("rateLimitReachedType").is_some_and(|r| !r.is_null());
            for (slot, fallback) in [("primary", "Session"), ("secondary", "Weekly")] {
                let Some(w) = snap.get(slot).filter(|w| w.is_object()) else { continue };
                let mins = w.get("windowDurationMins").and_then(Value::as_i64);
                let used = w.get("usedPercent").and_then(Value::as_f64);
                limits.push(Limit {
                    id: format!("{id}.{slot}"),
                    label: format!("{}{suffix}", window_label(mins, fallback)),
                    used_percent: used,
                    resets_at: w.get("resetsAt").and_then(Value::as_i64).map(|s| s.saturating_mul(1000)),
                    window: mins.map(window_words),
                    account: account.clone(),
                    status: (reached && used.is_some_and(|u| u >= 100.0)).then(|| "rejected".to_owned()),
                    ..Limit::default()
                });
            }
            if let Some(l) = snap.get("individualLimit").filter(|l| l.is_object()) {
                let num = |k: &str| l.get(k).and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().trim_start_matches('$').parse().ok())));
                limits.push(Limit {
                    id: format!("{id}.spend"),
                    label: format!("Spend limit{suffix}"),
                    used_percent: l.get("remainingPercent").and_then(Value::as_f64).map(|r| (100.0 - r).max(0.0)),
                    used: num("used"),
                    limit: num("limit"),
                    resets_at: l.get("resetsAt").and_then(Value::as_i64).map(|s| s.saturating_mul(1000)),
                    account: account.clone(),
                    status: (snap.get("spendControlReached").and_then(Value::as_bool) == Some(true)).then(|| "rejected".to_owned()),
                    ..Limit::default()
                });
            }
            if core.state.plan.is_none()
                && let Some(p) = snap.get("planType").and_then(Value::as_str)
            {
                core.state.plan = Some(plan_label(p));
            }
        }
        if core.state.limits != limits {
            core.state.limits = limits;
            core.touch_state();
        }
        self.compose_status(core);
    }

    /// Codex has no /status of its own to read: kiln puts its lines together.
    fn compose_status(&self, core: &mut Core) {
        let (approval, sandbox, _) = self.policies(core);
        let st = &core.state;
        let mut rows = Vec::new();
        let mut push = |group: &str, label: &str, value: String, tone: Option<Tone>| {
            if !value.is_empty() {
                rows.push(StatusItem { group: group.to_owned(), label: label.to_owned(), value, tone });
            }
        };
        push("Codex", "Version", core.meta.version.clone(), None);
        push("Codex", "Thread", st.harness_session.clone().unwrap_or_default(), None);
        push("Codex", "Directory", st.cwd.clone(), None);
        push("Model", "Provider", self.provider.clone().unwrap_or_default(), None);
        push("Model", "Model", st.model.clone(), None);
        push("Model", "Reasoning effort", st.effort.clone(), None);
        push("Model", "Reasoning summaries", self.summary.clone().unwrap_or_default(), None);
        push("Permissions", "Approvals", approval, None);
        push("Permissions", "Sandbox", sandbox, None);
        let how = match self.account_kind.as_deref() {
            Some("chatgpt") => "ChatGPT".to_owned(),
            Some("apiKey") => "API key".to_owned(),
            Some("amazonBedrock") => "Amazon Bedrock".to_owned(),
            Some(other) => other.to_owned(),
            None => self.provider.clone().filter(|p| p != "openai").map(|p| format!("Model provider {p}")).unwrap_or_default(),
        };
        push("Account", "Signed in with", how, None);
        push("Account", "Email", self.account_email.clone().unwrap_or_default(), None);
        push("Account", "Plan", st.plan.clone().unwrap_or_default(), None);
        for (id, snap) in &self.rate_limits {
            let Some(c) = snap.get("credits").filter(|c| c.is_object()) else { continue };
            let label = if id == "codex" { "Credits".to_owned() } else { format!("Credits · {id}") };
            let v = if c.get("unlimited").and_then(Value::as_bool) == Some(true) {
                "Unlimited".to_owned()
            } else if c.get("hasCredits").and_then(Value::as_bool) == Some(true) {
                c.get("balance").and_then(Value::as_str).map_or_else(|| "Yes".to_owned(), str::to_owned)
            } else {
                "None".to_owned()
            };
            push("Account", &label, v, None);
        }
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

    fn interrupt(&mut self, core: &mut Core) {
        if let (Some(thread), Some(turn)) = (self.thread.clone(), self.turn.clone()) {
            self.request(core, Pending::Interrupt, "turn/interrupt", json!({"threadId": thread, "turnId": turn}));
        }
    }

    fn send(&mut self, core: &mut Core, mut m: QueueItem, mode: SendMode) {
        m.text = m.text.trim_end().to_owned();
        if m.text.is_empty() && m.attachments.is_empty() {
            return;
        }
        if core.state.goal.is_empty() {
            core.state.goal = clip_middle(&m.text, 600).0;
        }
        let Some(thread) = self.thread.clone() else {
            // Not ready yet: goes out once the thread exists.
            self.show_sent(core, &m, Delivery::Queued);
            self.waiting.push(m);
            return;
        };
        let busy = self.busy(core);
        match (busy, mode, self.turn.clone()) {
            (true, SendMode::Steer, Some(turn)) => {
                let items = self.items(&m);
                self.request(core, Pending::Steer(m.id.clone(), Some(m.clone())), "turn/steer", json!({"threadId": thread, "expectedTurnId": turn, "input": items, "clientUserMessageId": m.id}));
                self.turn_messages.push(m.id.clone());
                self.show_sent(core, &m, Delivery::Sent);
            }
            (true, SendMode::Now, Some(_)) => {
                // Stop the turn; this message starts the next one.
                self.show_sent(core, &m, Delivery::Queued);
                self.waiting.insert(0, m);
                self.interrupt(core);
            }
            (true, _, _) => {
                core.state.queue.push(m);
                core.touch_state();
            }
            (false, _, _) => self.start_turn(core, &thread, m),
        }
    }

    /// A message's input items: "/skill words" (or "$skill words") is the skill, then the words.
    fn items(&self, m: &QueueItem) -> Value {
        let mut items = input(&m.text, &m.attachments);
        if let Some(rest) = m.text.strip_prefix(['/', '$'])
            && let Some(name) = rest.split_whitespace().next()
            && let Some(path) = self.skills.get(name).cloned()
            && let Some(list) = items.as_array_mut()
        {
            let words = rest.get(name.len()..).unwrap_or("").trim();
            list.retain(|i| i.get("type").and_then(Value::as_str) != Some("text"));
            list.insert(0, json!({"type": "skill", "name": name, "path": path}));
            if !words.is_empty() {
                list.insert(1, json!({"type": "text", "text": words}));
            }
        }
        items
    }

    fn start_turn(&mut self, core: &mut Core, thread: &str, m: QueueItem) {
        self.thread_status = None;
        let (approval, _, sandbox) = self.policies(core);
        let items = self.items(&m);
        let mut params = json!({
            "threadId": thread,
            "input": items,
            "clientUserMessageId": m.id,
            "approvalPolicy": approval,
            "sandboxPolicy": sandbox,
        });
        // The session's model and effort, or this message's own (Codex sets both per turn).
        let o = m.overrides.clone().unwrap_or_default();
        if let Some(obj) = params.as_object_mut() {
            if let Some(model) = o.model.or_else(|| self.model.clone()) {
                obj.insert("model".into(), json!(model));
            }
            if let Some(effort) = o.effort.or_else(|| self.effort.clone()) {
                obj.insert("effort".into(), json!(effort));
            }
            if let Some(summary) = self.summary.clone() {
                obj.insert("summary".into(), json!(summary));
            }
        }
        self.request(core, Pending::Turn(m.id.clone()), "turn/start", params);
        self.turn_messages = vec![m.id.clone()];
        self.show_sent(core, &m, Delivery::Sent);
        if core.state.turn_started.is_none() {
            core.state.turn_started = Some(Core::now());
        }
        self.settle(core);
    }

    fn show_sent(&mut self, core: &mut Core, m: &QueueItem, d: Delivery) {
        core.state.delivery.insert(m.id.clone(), d);
        let (images, names) = super::model::attachment_summary(&m.attachments);
        core.note_sent(&m.id);
        core.upsert(&m.id, None, Body::User { text: m.text.clone(), images, client_id: Some(m.id.clone()), origin: Some(super::core::origin_of(&m.id).into()), attachments: names });
        core.touch_state();
    }

    /// Shown for a message kiln sends on its own (feedback after a refusal).
    fn note(&mut self, core: &mut Core, id: &str, text: &str, d: Delivery) {
        let m = QueueItem { id: id.to_owned(), text: text.to_owned(), at: Core::now(), attachments: Vec::new(), overrides: None };
        self.show_sent(core, &m, d);
    }

    /// Turns of a thread's history, as items.
    fn replay(&mut self, core: &mut Core, turns: &[Value]) {
        for turn in turns {
            let tid = turn.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
            for item in turn.get("items").and_then(Value::as_array).into_iter().flatten() {
                if item.get("type").and_then(Value::as_str) == Some("userMessage") {
                    let key = item.get("clientId").or_else(|| item.get("id")).and_then(Value::as_str).unwrap_or("").to_owned();
                    self.turns.insert(key, tid.clone());
                }
                self.item(core, item, true);
            }
        }
    }

    /// The thread is open (with its history): ready for messages.
    fn ready(&mut self, core: &mut Core) {
        self.refresh_controls(core);
        core.touch_state();
        core.set_status(self.thread_status.unwrap_or(Status::Idle));
        // The thread's own MCP connections (their runtime state).
        self.ask_mcp(core);
        self.compose_status(core);
        self.next(core);
    }

    /// The turn has its id: the queued messages to steer into it go in.
    fn steer_waiting(&mut self, core: &mut Core) {
        if self.turn.is_none() {
            return;
        }
        for id in std::mem::take(&mut self.steer_on_start) {
            if let Some(pos) = core.state.queue.iter().position(|q| q.id == id) {
                let item = core.state.queue.remove(pos);
                core.touch_state();
                self.send(core, item, SendMode::Steer);
            }
        }
    }

    /// Between turns: the next waiting message, then kiln's queue.
    fn next(&mut self, core: &mut Core) {
        if self.busy(core) || core.state.status == Status::Exited {
            return;
        }
        // A pause (or a switch) holds what waits, and the stop point may start one here.
        if (!self.waiting.is_empty() || !core.state.queue.is_empty()) && !super::pause::open(core) {
            return;
        }
        // No turn to steer into: what was to be steered goes out in order.
        self.steer_on_start.clear();
        let Some(thread) = self.thread.clone() else { return };
        if !self.waiting.is_empty() {
            let m = self.waiting.remove(0);
            self.start_turn(core, &thread, m);
            return;
        }
        if !core.state.queue.is_empty() {
            let item = core.state.queue.remove(0);
            core.touch_state();
            let id = item.id.clone();
            self.start_turn(core, &thread, item);
            super::pause::sent_from_queue(core, &id);
        }
    }

    /// The account reached its usage limit (`usageLimitExceeded`): the session pauses until it
    /// resets, its turn noted as cut off. Said once.
    fn limit_reached(&mut self, core: &mut Core, said: &str) {
        let now = Core::now();
        let hit = super::pause::Hit::from_limits(&core.state.limits, said, None, now);
        let cut = super::pause::cut_now(core);
        super::pause::hold(core, super::pause::LIMIT, "harness", hit, Some(cut));
    }

    /// A pause's limit should have reset: ask for the rate limits now (the answer goes to
    /// `pause::checked`).
    pub fn check_limits(&mut self, core: &mut Core) {
        if core.state.status == Status::Exited || self.pending.values().any(|p| matches!(p, Pending::RateLimits)) {
            return;
        }
        self.usage_at = Core::now();
        self.request(core, Pending::RateLimits, "account/rateLimits/read", json!({}));
    }

    /// A pause ended: the turn it cut off goes on first, then kiln's queue.
    pub fn go_on(&mut self, core: &mut Core, p: super::model::Pause) {
        if p.cut.is_some() && !self.busy(core) && core.state.status != Status::Exited {
            let id = crate::util::new_id();
            self.send(core, QueueItem { id: id.clone(), text: super::pause::CONTINUE.to_owned(), at: Core::now(), attachments: Vec::new(), overrides: None }, SendMode::Queue);
            super::pause::from_kiln(core, &id);
            return;
        }
        if p.cut.is_some() {
            super::pause::continue_later(core, super::pause::CONTINUE);
        }
        self.next(core);
    }

    fn answer(&mut self, core: &mut Core, id: &str, decision: Decision, message: Option<String>, always: Option<usize>, choices: Option<Vec<ChoiceAnswer>>) {
        let Some(approval) = core.take_approval(id) else { return };
        let Some(ask) = self.asks.remove(id) else { return };
        let feedback = message.filter(|m| !m.trim().is_empty());
        let result = match ask.kind {
            AskKind::Elicitation => {
                let (r, why) = super::elicit::answer(approval.input.get("requestedSchema"), decision != Decision::Deny, choices.as_deref().unwrap_or_default());
                if let Some(why) = why {
                    self.notice(core, Level::Warn, format!("Declined it for you, as the answers didn't fit: {why}"));
                }
                r
            }
            AskKind::UserInput => {
                let mut out = serde_json::Map::new();
                if decision != Decision::Deny {
                    // Each option chosen and any free text one answer, commas and all; a note as
                    // Codex's own TUI sends it.
                    for (qid, text) in &ask.questions {
                        let Some(c) = choices.as_ref().and_then(|cs| cs.iter().find(|c| c.id.as_deref() == Some(qid.as_str())).or_else(|| cs.iter().find(|c| c.question == *text))) else {
                            continue;
                        };
                        let mut list: Vec<String> = c.selected.clone();
                        if let Some(o) = c.other.as_deref().map(str::trim).filter(|o| !o.is_empty()) {
                            list.push(o.to_owned());
                        }
                        if let Some(n) = c.note.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                            list.push(format!("user_note: {n}"));
                        }
                        out.insert(qid.clone(), json!({"answers": list}));
                    }
                }
                json!({"answers": out})
            }
            AskKind::Permissions => match decision {
                // Granting exactly what was asked for.
                Decision::Allow | Decision::Always => json!({"permissions": approval.input.get("permissions").cloned().unwrap_or(json!({})), "scope": if decision == Decision::Always { "session" } else { "turn" }}),
                Decision::Deny => json!({"permissions": {}}),
            },
            AskKind::LegacyExec | AskKind::LegacyPatch => json!({"decision": match decision {
                Decision::Allow => json!("approved"),
                Decision::Always => json!("approved_for_session"),
                Decision::Deny => match &feedback {
                    Some(f) => json!({"denied": {"rejection": f}}),
                    None => json!("abort"),
                },
            }}),
            AskKind::Command | AskKind::FileChange => json!({"decision": match decision {
                Decision::Allow => json!("accept"),
                Decision::Always => always.and_then(|i| approval.always.get(i)).or_else(|| approval.always.first()).map_or(json!("accept"), |o| o.value.clone()),
                // With feedback the turn goes on (the feedback is steered in); without, it stops.
                Decision::Deny => if feedback.is_some() && ask.can_decline { json!("decline") } else { json!("cancel") },
            }}),
        };
        core.write(&json!({"id": ask.rpc, "result": result}));
        if let Some(tid) = approval.tool_id.as_deref() {
            set_status(core, tid, if decision == Decision::Deny { ToolStatus::Denied } else { ToolStatus::Running });
        }
        if decision == Decision::Deny
            && let Some(f) = feedback
        {
            let mid = crate::util::new_id();
            match (ask.can_decline, self.thread.clone(), self.turn.clone()) {
                // The turn goes on: the feedback joins it.
                (true, Some(thread), Some(turn)) => {
                    self.request(core, Pending::Steer(mid.clone(), None), "turn/steer", json!({"threadId": thread, "expectedTurnId": turn, "input": [{"type": "text", "text": f}], "clientUserMessageId": mid}));
                    self.turn_messages.push(mid.clone());
                    self.note(core, &mid, &f, Delivery::Sent);
                }
                // Cancelled: the feedback starts the next turn.
                _ => {
                    self.note(core, &mid, &f, Delivery::Queued);
                    self.waiting.insert(0, QueueItem { id: mid, text: f, at: Core::now(), attachments: Vec::new(), overrides: None });
                }
            }
        }
        if core.state.approvals.is_empty() && self.turn.is_some() {
            core.state.doing = Some("Working".to_owned());
        }
        self.settle(core);
    }

    // ------------------------------------------------------------------ stdout

    pub fn line(&mut self, core: &mut Core, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            self.notice(core, Level::Warn, clip_middle(line, 2000).0);
            return;
        };
        let method = v.get("method").and_then(Value::as_str).map(str::to_owned);
        match (method, v.get("id").cloned()) {
            (Some(m), Some(id)) => self.server_request(core, &m, id, v.get("params").cloned().unwrap_or(Value::Null)),
            (Some(m), None) => self.notification(core, &m, v.get("params").cloned().unwrap_or(Value::Null)),
            (None, Some(id)) => self.response(core, &id, &v),
            (None, None) => {}
        }
    }

    fn response(&mut self, core: &mut Core, id: &Value, v: &Value) {
        let Some(kind) = id.as_u64().and_then(|n| self.pending.remove(&n)) else { return };
        let result = v.get("result").cloned().unwrap_or(Value::Null);
        if let Some(err) = v.get("error") {
            let msg = err.get("message").and_then(Value::as_str).unwrap_or("request failed").to_owned();
            let wait = not_yet(&msg).filter(|_| self.open_tries < OPEN_TRIES);
            match kind {
                Pending::Suggest(client) => core.reply(client, json!({"error": msg})),
                // The turn ended before the steer reached it: the message starts the next one.
                Pending::Steer(id, Some(m)) if self.turn.is_none() => {
                    self.turn_messages.retain(|x| *x != id);
                    self.show_sent(core, &m, Delivery::Queued);
                    self.waiting.insert(0, m);
                    self.next(core);
                }
                Pending::Turn(id) | Pending::Steer(id, _) => {
                    core.state.delivery.insert(id, Delivery::Cancelled);
                    self.notice(core, Level::Error, format!("Codex refused the message: {msg}"));
                    core.touch_state();
                }
                // Not yet: tried again at the next tick.
                Pending::Thread if wait.is_some() => {
                    self.reopen = true;
                    core.state.doing = wait.map(str::to_owned);
                    core.touch_state();
                }
                Pending::Thread => {
                    self.notice(core, Level::Error, format!("Codex could not open the conversation: {msg}"));
                    core.set_status(Status::Idle);
                }
                Pending::History => {
                    self.notice(core, Level::Warn, format!("Codex could not list the conversation's history: {msg}"));
                    self.ready(core);
                }
                Pending::RateLimits => {
                    // A pause's check that can't be answered: go on, and let Codex say.
                    if core.state.pause.as_ref().is_some_and(|p| p.checking.is_some())
                        && let Some(p) = super::pause::release(core, "The limit's reset time came; going on.")
                    {
                        self.go_on(core, p);
                    }
                }
                Pending::Models | Pending::Interrupt | Pending::Hooks | Pending::Account => {}
                Pending::Skills => {
                    core.state.inventory.skills = Known::Ready;
                    core.touch_state();
                }
                Pending::Plugins => {
                    core.state.inventory.plugins = Known::Unsupported;
                    core.touch_state();
                }
                Pending::Mcp | Pending::Config => {
                    if matches!(kind, Pending::Mcp) {
                        self.mcp_status_seen = true;
                    } else {
                        self.mcp_config_seen = true;
                    }
                    self.compose_mcp(core);
                }
                Pending::McpAuth { client, .. } | Pending::Goal(client) => core.reply(client, json!({"error": msg})),
                Pending::McpAdded { client, .. } => answer(core, client, json!({"ok": false, "error": msg})),
                Pending::McpConfig(what) => {
                    self.notice(core, Level::Warn, format!("{what}: {msg}"));
                    self.request(core, Pending::Config, "config/read", json!({"cwd": core.state.cwd.clone()}));
                }
                Pending::McpReload => {
                    self.notice(core, Level::Warn, format!("Reloading the MCP servers: {msg}"));
                    self.ask_mcp(core);
                }
                Pending::Revert(_) => self.notice(core, Level::Warn, format!("Could not rewind: {msg}")),
                Pending::Inject => {
                    self.injecting = false;
                    self.notice(core, Level::Error, format!("Codex did not take the conversation carried over: {msg}"));
                    self.ready(core);
                }
                Pending::Plain(what) => self.notice(core, Level::Warn, format!("{what}: {msg}")),
                Pending::Initialize => self.notice(core, Level::Error, format!("Codex did not start: {msg}")),
            }
            self.settle(core);
            return;
        }
        match kind {
            Pending::Initialize => {
                core.write(&json!({"method": "initialized"}));
                let (approval, sandbox, _) = self.policies(core);
                let cwd = core.state.cwd.clone();
                let mut params = json!({"cwd": cwd, "approvalPolicy": approval, "sandbox": sandbox});
                if let (Some(m), Some(o)) = (&self.model, params.as_object_mut()) {
                    o.insert("model".into(), json!(m));
                }
                match (self.resume.clone(), self.fork_at.take()) {
                    // A branch: a new thread with that one's turns up to `at`.
                    // (Its history comes after, page by page: thread/turns/list.)
                    (Some(t), Some(at)) => {
                        if let Some(o) = params.as_object_mut() {
                            o.insert("threadId".into(), json!(t));
                            o.insert("excludeTurns".into(), json!(true));
                            if let Some(at) = at {
                                o.insert("lastTurnId".into(), json!(at));
                            }
                        }
                        self.open_thread(core, "thread/fork", params);
                    }
                    (Some(t), None) => {
                        if let Some(o) = params.as_object_mut() {
                            o.insert("threadId".into(), json!(t));
                            o.insert("excludeTurns".into(), json!(true));
                        }
                        self.open_thread(core, "thread/resume", params);
                    }
                    (None, _) => self.open_thread(core, "thread/start", params),
                }
                self.request(core, Pending::Models, "model/list", json!({}));
                self.request(core, Pending::Skills, "skills/list", json!({"cwds": [cwd.clone()]}));
                self.request(core, Pending::Hooks, "hooks/list", json!({"cwds": [cwd.clone()]}));
                self.request(core, Pending::Config, "config/read", json!({"cwd": cwd.clone()}));
                self.request(core, Pending::Account, "account/read", json!({}));
                self.request(core, Pending::Plugins, "plugin/installed", json!({"cwds": [cwd]}));
                if let Some(ua) = result.get("userAgent").and_then(Value::as_str) {
                    core.meta.version = ua.split_whitespace().next().unwrap_or(ua).to_owned();
                    core.touch_meta();
                }
            }
            Pending::Thread => {
                if core.state.doing.as_deref().is_some_and(|d| d == CATCHING_UP || d == LETTING_GO) {
                    core.state.doing = None;
                }
                let thread = result.get("thread").cloned().unwrap_or(Value::Null);
                if let Some(id) = thread.get("id").and_then(Value::as_str) {
                    self.thread = Some(id.to_owned());
                    core.state.harness_session = Some(id.to_owned());
                }
                if let Some(status) = thread.get("status") {
                    self.thread_status(core, status);
                }
                if let Some(m) = result.get("model").and_then(Value::as_str) {
                    core.state.model = m.to_owned();
                }
                if let Some(c) = result.get("cwd").and_then(Value::as_str) {
                    core.state.cwd = c.to_owned();
                }
                if let Some(p) = result.get("modelProvider").and_then(Value::as_str) {
                    self.provider = Some(p.to_owned());
                }
                if let Some(p) = thread.get("path").and_then(Value::as_str) {
                    self.rollout = Some(p.to_owned());
                }
                if let Some(v) = thread.get("cliVersion").and_then(Value::as_str) {
                    core.meta.version = v.to_owned();
                    core.touch_meta();
                }
                if let Some(n) = thread.get("name").and_then(Value::as_str) {
                    core.state.title = n.to_owned();
                }
                // A new thread that goes on with a conversation from another harness: it goes in
                // first (the session is ready once it is).
                if let (Some(items), Some(id)) = (self.seed.take(), thread.get("id").and_then(Value::as_str)) {
                    self.injecting = true;
                    let id = id.to_owned();
                    self.request(core, Pending::Inject, "thread/inject_items", json!({"threadId": id, "items": items}));
                    self.settle(core);
                    return;
                }
                // A resumed thread's history: inline from a Codex that still sends it so, else
                // its latest turns, listed (the session is ready once they are in).
                let turns = thread.get("turns").and_then(Value::as_array).cloned().unwrap_or_default();
                match (self.resume.is_some() && turns.is_empty(), thread.get("id").and_then(Value::as_str)) {
                    (true, Some(id)) => {
                        let id = id.to_owned();
                        self.request(core, Pending::History, "thread/turns/list", json!({"threadId": id, "itemsView": "full", "sortDirection": "desc", "limit": HISTORY_TURNS}));
                    }
                    _ => {
                        self.replay(core, &turns);
                        self.ready(core);
                    }
                }
            }
            Pending::History => {
                let mut turns = result.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
                turns.reverse();
                if result.get("nextCursor").is_some_and(|c| !c.is_null()) {
                    self.notice(core, Level::Info, format!("Earlier turns are not shown: the last {HISTORY_TURNS}."));
                }
                self.replay(core, &turns);
                self.ready(core);
            }
            Pending::Turn(id) => {
                if let Some(t) = result.get("turn").and_then(|t| t.get("id")).and_then(Value::as_str) {
                    self.turn = Some(t.to_owned());
                }
                core.state.delivery.insert(id, Delivery::Started);
                core.touch_state();
                self.steer_waiting(core);
            }
            Pending::Steer(id, _) => {
                core.state.delivery.insert(id, Delivery::Started);
                core.touch_state();
            }
            Pending::Models => {
                for m in result.get("data").and_then(Value::as_array).cloned().unwrap_or_default() {
                    let levels: Vec<(String, String)> = m
                        .get("supportedReasoningEfforts")
                        .and_then(Value::as_array)
                        .map(|l| {
                            l.iter()
                                .filter_map(|e| {
                                    let id = e.get("reasoningEffort").and_then(Value::as_str).or_else(|| e.as_str())?.to_owned();
                                    Some((id, e.get("description").and_then(Value::as_str).unwrap_or("").to_owned()))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    for key in [m.get("model"), m.get("id")].into_iter().flatten().filter_map(Value::as_str) {
                        self.efforts.insert(key.to_owned(), levels.clone());
                    }
                    if m.get("isDefault").and_then(Value::as_bool) == Some(true) {
                        self.efforts.insert(String::new(), levels.clone());
                    }
                    if m.get("isDefault").and_then(Value::as_bool) == Some(true)
                        && core.state.effort.is_empty()
                        && let Some(d) = m.get("defaultReasoningEffort").and_then(Value::as_str)
                    {
                        core.state.effort = self.effort.clone().unwrap_or_else(|| d.to_owned());
                    }
                }
                if let Some(e) = &self.effort {
                    core.state.effort.clone_from(e);
                }
                core.meta.models = result
                    .get("data")
                    .and_then(Value::as_array)
                    .map(|ms| {
                        ms.iter()
                            .filter(|m| m.get("hidden").and_then(Value::as_bool) != Some(true))
                            .filter_map(|m| {
                                Some(ModelOption {
                                    id: m.get("model").or_else(|| m.get("id")).and_then(Value::as_str)?.to_owned(),
                                    label: m.get("displayName").and_then(Value::as_str).unwrap_or("").to_owned(),
                                    description: m.get("description").and_then(Value::as_str).unwrap_or("").to_owned(),
                                    efforts: m
                                        .get("supportedReasoningEfforts")
                                        .and_then(Value::as_array)
                                        .map(|l| l.iter().filter_map(|e| e.get("reasoningEffort").and_then(Value::as_str).or_else(|| e.as_str()).map(str::to_owned)).collect())
                                        .unwrap_or_default(),
                                    default: m.get("isDefault").and_then(Value::as_bool).unwrap_or(false),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                core.touch_meta();
                self.refresh_controls(core);
                core.touch_state();
            }
            Pending::Skills => {
                let mut cmds = Vec::new();
                let mut all = Vec::new();
                self.skills.clear();
                for entry in result.get("data").and_then(Value::as_array).cloned().unwrap_or_default() {
                    for sk in entry.get("skills").and_then(Value::as_array).cloned().unwrap_or_default() {
                        let (Some(name), Some(path)) = (sk.get("name").and_then(Value::as_str), sk.get("path").and_then(Value::as_str)) else { continue };
                        if all.iter().any(|s: &Skill| s.name == name) {
                            continue;
                        }
                        let enabled = sk.get("enabled").and_then(Value::as_bool) != Some(false);
                        let short = sk
                            .get("shortDescription")
                            .or_else(|| sk.pointer("/interface/shortDescription"))
                            .and_then(Value::as_str)
                            .filter(|d| !d.is_empty())
                            .or_else(|| sk.get("description").and_then(Value::as_str))
                            .unwrap_or("")
                            .to_owned();
                        let source = match (sk.get("pluginId").and_then(Value::as_str), sk.get("scope").and_then(Value::as_str)) {
                            (Some(_), _) => "plugin",
                            (None, Some("repo")) => "project",
                            (None, Some(s)) => s,
                            (None, None) => "user",
                        };
                        all.push(Skill {
                            name: name.to_owned(),
                            description: clip_middle(sk.get("description").and_then(Value::as_str).unwrap_or(&short), 1200).0,
                            source: source.to_owned(),
                            path: Some(path.to_owned()),
                            args_hint: None,
                            user_invocable: true,
                            enabled,
                        });
                        if !enabled {
                            continue;
                        }
                        self.skills.insert(name.to_owned(), path.to_owned());
                        cmds.push(SlashCommand::new(name, &clip_middle(&short, 300).0, "", "Skills", Route::Prompt { text: None, skill: true }));
                    }
                }
                // Codex's own commands first: a skill of the same name is run from the skills
                // panel (Codex names skills with "$", its commands with "/").
                let mut catalog = codex_catalog();
                catalog.extend(cmds);
                core.meta.commands = sort_catalog(catalog);
                core.touch_meta();
                core.state.skills = all;
                core.state.plugins = self.plugins();
                core.state.inventory.skills = Known::Ready;
                core.touch_state();
            }
            Pending::Account => {
                let acct = result.get("account").filter(|a| a.is_object());
                let kind = acct.and_then(|a| a.get("type")).and_then(Value::as_str).map(str::to_owned);
                let email = acct.and_then(|a| a.get("email")).and_then(Value::as_str).map(str::to_owned);
                let plan = match kind.as_deref() {
                    Some("chatgpt") => Some(acct.and_then(|a| a.get("planType")).and_then(Value::as_str).filter(|p| *p != "unknown").map_or_else(|| "ChatGPT".to_owned(), plan_label)),
                    Some("apiKey") => Some("API key".to_owned()),
                    Some("amazonBedrock") => Some("Amazon Bedrock".to_owned()),
                    Some(other) => Some(plan_label(other)),
                    None if result.get("requiresOpenaiAuth").and_then(Value::as_bool) == Some(true) => Some("Signed out".to_owned()),
                    // A model provider of its own (no OpenAI account).
                    None => None,
                };
                let was = core.state.account.clone();
                core.state.account = plan.as_deref().filter(|p| *p != "Signed out").map(|p| account_id("codex", p, email.as_deref()));
                // Signed in as someone else (here, or on the machine it moved to for room): the
                // old account's limits go, and a pause for one ends.
                if was.is_some() && core.state.account != was {
                    core.state.limits.clear();
                    if core.state.pause.is_some()
                        && let Some(p) = super::pause::release(core, "Signed in to another account: going on.")
                    {
                        self.go_on(core, p);
                    }
                }
                core.state.plan = plan;
                self.account_email = email.as_deref().map(mask_emails);
                if kind.as_deref() == Some("chatgpt") {
                    self.usage_at = Core::now();
                    self.request(core, Pending::RateLimits, "account/rateLimits/read", json!({}));
                }
                self.account_kind = kind;
                core.state.inventory.limits = Known::Ready;
                core.touch_state();
                self.apply_limits(core);
            }
            Pending::RateLimits => {
                if let Some(by) = result.get("rateLimitsByLimitId").and_then(Value::as_object) {
                    for (k, snap) in by {
                        self.rate_limits.insert(k.clone(), snap.clone());
                    }
                } else if let Some(snap) = result.get("rateLimits").filter(|r| r.is_object()) {
                    let id = snap.get("limitId").and_then(Value::as_str).unwrap_or("codex").to_owned();
                    self.rate_limits.insert(id, snap.clone());
                }
                self.apply_limits(core);
                if let Some(p) = super::pause::checked(core, Core::now()) {
                    self.go_on(core, p);
                }
            }
            Pending::Config => {
                self.mcp_config = result.pointer("/config/mcp_servers").and_then(Value::as_object).cloned().unwrap_or_default();
                self.mcp_config_seen = true;
                self.compose_mcp(core);
            }
            Pending::Plugins => {
                self.plugins_raw = result
                    .get("marketplaces")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .flat_map(|m| {
                        let market = m.get("name").and_then(Value::as_str).unwrap_or("").to_owned();
                        m.get("plugins").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().map(move |p| (market.clone(), p))
                    })
                    .filter(|(_, p)| p.get("installed").and_then(Value::as_bool) == Some(true))
                    .collect();
                core.state.plugins = self.plugins();
                core.state.inventory.plugins = Known::Ready;
                core.touch_state();
            }
            Pending::McpConfig(_) => self.mcp_reload(core),
            Pending::Goal(client) => {
                let goal = result.get("goal").filter(|g| g.is_object());
                let text = match goal {
                    Some(g) => {
                        let status = g.get("status").and_then(Value::as_str).unwrap_or("active");
                        let next = if status == "active" { "Codex works toward it until it is done: /goal pause, /goal clear." } else { "/goal resume, /goal clear." };
                        format!("{}\n\nStatus: {status} · {} tokens used\n{next}", g.get("objective").and_then(Value::as_str).unwrap_or(""), g.get("tokensUsed").and_then(Value::as_i64).unwrap_or(0))
                    }
                    None if result.get("cleared").and_then(Value::as_bool) == Some(true) => "Goal cleared.".to_owned(),
                    None => "No goal is set. Give one after /goal.".to_owned(),
                };
                core.reply(client, json!({"title": "Goal", "text": text}));
            }
            Pending::McpAdded { name, client } => {
                answer(core, client, json!({"ok": true, "message": format!("Added MCP server {name} to Codex's configuration")}));
                self.mcp_reload(core);
            }
            Pending::McpReload => {
                self.request(core, Pending::Config, "config/read", json!({"cwd": core.state.cwd.clone()}));
                self.ask_mcp(core);
            }
            Pending::McpAuth { client, name } => match result.get("authorizationUrl").and_then(Value::as_str) {
                // Codex waits for the sign-in's redirect on its own machine's loopback.
                Some(url) => core.reply(client, json!({"url": url, "callback": true, "redirect": "localhost"})),
                None => {
                    core.reply(client, json!({"ok": true, "message": format!("{name} needs no sign-in.")}));
                    self.ask_mcp(core);
                }
            },
            Pending::Hooks => {
                core.meta.hooks = result
                    .get("data")
                    .and_then(Value::as_array)
                    .map(|d| {
                        d.iter()
                            .flat_map(|e| e.get("hooks").and_then(Value::as_array).cloned().unwrap_or_default())
                            .filter_map(|h| {
                                Some(HookInfo {
                                    event: h.get("eventName").and_then(Value::as_str)?.to_owned(),
                                    matcher: h.get("matcher").and_then(Value::as_str).unwrap_or("").to_owned(),
                                    command: h.get("statusMessage").or_else(|| h.get("key")).and_then(Value::as_str).unwrap_or("").to_owned(),
                                    source: h.get("source").and_then(Value::as_str).unwrap_or("").to_owned(),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                core.touch_meta();
            }
            Pending::Mcp => {
                self.mcp_rows = result.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
                self.mcp_status_seen = true;
                if std::mem::take(&mut self.mcp_again) {
                    self.ask_mcp(core);
                }
                // What the list says about a server's runtime replaces what start-up pushed.
                for r in &self.mcp_rows {
                    if r.get("runtimeStatus").is_some_and(|s| !s.is_null())
                        && let Some(n) = r.get("name").and_then(Value::as_str)
                    {
                        self.mcp_startup.remove(n);
                    }
                }
                self.compose_mcp(core);
            }
            Pending::Inject => {
                self.injecting = false;
                self.seeded = true;
                self.ready(core);
            }
            Pending::Revert(ev) => {
                // Those turns are gone: so is their part of the timeline.
                core.truncate_from(&ev);
                self.turns.retain(|k, _| core.get(k).is_some());
            }
            Pending::Plain(_) => {}
            Pending::Suggest(client) => {
                let cwd = core.state.cwd.clone();
                let files: Vec<Value> = result
                    .get("files")
                    .and_then(Value::as_array)
                    .map(|f| f.iter().filter_map(|x| x.get("path").and_then(Value::as_str)).map(|p| json!({"path": p})).collect())
                    .unwrap_or_default();
                core.reply(client, json!({"suggestions": files, "cwd": cwd}));
            }
            Pending::Interrupt => {}
        }
        self.settle(core);
    }

    fn server_request(&mut self, core: &mut Core, method: &str, rpc: Value, p: Value) {
        let id = match &rpc {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let item_id = p.get("itemId").and_then(Value::as_str).map(str::to_owned);
        let cwd = p.get("cwd").and_then(Value::as_str).map_or_else(|| core.state.cwd.clone(), str::to_owned);
        let reason = p.get("reason").and_then(Value::as_str).map(str::to_owned).filter(|r| !r.trim().is_empty());
        // What Codex offers for this request (newer servers list it; older ones take all four).
        let offered: Option<Vec<Value>> = p.get("availableDecisions").and_then(Value::as_array).cloned();
        let offers = |name: &str| offered.as_ref().is_none_or(|d| d.iter().any(|x| x.as_str() == Some(name) || x.get(name).is_some()));
        let can_decline = offers("decline");
        // What an MCP server's request for input shows beyond its questions (a page to open).
        let mut shown: Option<String> = None;
        let (kind, tool, title, command, mut always, questions, diffs) = match method {
            "item/commandExecution/requestApproval" | "execCommandApproval" => {
                let full = p.get("command").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| joined(p.get("command")));
                // The command as written, not the shell wrapping it ("bash -lc '…'").
                let actions = p.get("commandActions").and_then(Value::as_array).cloned().unwrap_or_default();
                let cmd = match actions.as_slice() {
                    [one] => one.get("command").and_then(Value::as_str).map_or_else(|| full.clone(), str::to_owned),
                    _ => full.clone(),
                };
                let mut always = Vec::new();
                if offers("acceptForSession") {
                    always.push(AlwaysOption { label: "Allow commands like this for the session".into(), scope: "this session".into(), value: json!("acceptForSession") });
                }
                if let Some(prefix) = p.get("proposedExecpolicyAmendment").and_then(Value::as_array).filter(|a| !a.is_empty())
                    && offers("acceptWithExecpolicyAmendment")
                {
                    let words: Vec<&str> = prefix.iter().filter_map(Value::as_str).collect();
                    always.push(AlwaysOption {
                        label: format!("Always allow `{}`", words.join(" ")),
                        scope: "Codex rules".into(),
                        value: json!({"acceptWithExecpolicyAmendment": {"execpolicy_amendment": prefix}}),
                    });
                }
                for d in offered.iter().flatten() {
                    if let Some(net) = d.get("applyNetworkPolicyAmendment") {
                        let host = net.pointer("/network_policy_amendment/host").and_then(Value::as_str).unwrap_or("this host");
                        always.push(AlwaysOption { label: format!("Allow network access to {host}"), scope: "Codex rules".into(), value: d.clone() });
                    }
                }
                let kind = if method == "execCommandApproval" { AskKind::LegacyExec } else { AskKind::Command };
                (kind, ToolKind::Shell, cmd.clone(), Some(cmd), always, Vec::new(), Vec::new())
            }
            "item/fileChange/requestApproval" | "applyPatchApproval" => {
                // The change itself arrived as the item (item/started); show its diffs.
                let diffs = item_id.as_deref().and_then(|i| core.get(i)).and_then(|e| match &e.body {
                    Body::Tool(t) => Some(t.diffs.clone()),
                    _ => None,
                });
                let diffs = diffs.unwrap_or_else(|| file_changes(p.get("changes").or_else(|| p.get("fileChanges")), &core.state.cwd));
                let title = diffs.iter().map(|d| d.path.clone()).collect::<Vec<_>>().join(", ");
                let always = if offers("acceptForSession") {
                    vec![AlwaysOption { label: "Allow edits for the session".into(), scope: "this session".into(), value: json!("acceptForSession") }]
                } else {
                    Vec::new()
                };
                let kind = if method == "applyPatchApproval" { AskKind::LegacyPatch } else { AskKind::FileChange };
                (kind, ToolKind::Edit, if title.is_empty() { "File changes".into() } else { title }, None, always, Vec::new(), diffs)
            }
            "item/tool/requestUserInput" => {
                let qs: Vec<Question> = p
                    .get("questions")
                    .and_then(Value::as_array)
                    .map(|qs| {
                        qs.iter()
                            .filter_map(|q| {
                                Some(Question {
                                    id: q.get("id").and_then(Value::as_str).unwrap_or("").to_owned(),
                                    question: q.get("question").and_then(Value::as_str)?.to_owned(),
                                    header: q.get("header").and_then(Value::as_str).unwrap_or("").to_owned(),
                                    multi: false,
                                    other: q.get("isOther").and_then(Value::as_bool).unwrap_or(false),
                                    secret: q.get("isSecret").and_then(Value::as_bool).unwrap_or(false),
                                    options: q
                                        .get("options")
                                        .and_then(Value::as_array)
                                        .map(|o| {
                                            o.iter()
                                                .filter_map(|x| {
                                                    Some(QuestionOption {
                                                        label: x.get("label").and_then(Value::as_str)?.to_owned(),
                                                        description: x.get("description").and_then(Value::as_str).unwrap_or("").to_owned(),
                                                        preview: String::new(),
                                                    })
                                                })
                                                .collect()
                                        })
                                        .unwrap_or_default(),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let title = qs.first().map_or_else(|| "Question".to_owned(), |q| q.question.clone());
                (AskKind::UserInput, ToolKind::Question, title, None, Vec::new(), qs, Vec::new())
            }
            "item/permissions/requestApproval" => (AskKind::Permissions, ToolKind::Other, "More access".to_owned(), None, Vec::new(), Vec::new(), Vec::new()),
            "mcpServer/elicitation/request" => {
                let s = |k: &str| p.get(k).and_then(Value::as_str);
                let a = super::elicit::ask(s("serverName").unwrap_or(""), s("message").unwrap_or(""), s("mode"), p.get("requestedSchema"), s("url"));
                shown = a.detail;
                (AskKind::Elicitation, ToolKind::Question, a.title, None, Vec::new(), a.questions, Vec::new())
            }
            other => {
                core.write(&json!({"id": rpc, "error": {"code": -32601, "message": format!("kiln does not handle {other}")}}));
                return;
            }
        };
        let question_ids: Vec<(String, String)> = if kind == AskKind::Elicitation {
            questions.iter().map(|q| (q.id.clone(), q.question.clone())).collect()
        } else {
            p.get("questions")
                .and_then(Value::as_array)
                .map(|qs| qs.iter().filter_map(|q| Some((q.get("id")?.as_str()?.to_owned(), q.get("question")?.as_str()?.to_owned()))).collect())
                .unwrap_or_default()
        };
        if kind == AskKind::LegacyExec || kind == AskKind::LegacyPatch {
            always.truncate(1);
        }
        let approval = Approval {
            id: id.clone(),
            tool_id: item_id.clone(),
            tool_name: method.to_owned(),
            tool,
            title,
            detail: shown,
            input: clip_json(&p, 64 * 1024),
            command,
            cwd,
            diffs,
            reason,
            always,
            default_to_no: false,
            questions,
            plan: None,
            agent: None,
            at: Core::now(),
        };
        if let Some(tid) = item_id.as_deref() {
            set_status(core, tid, ToolStatus::Pending);
        }
        core.state.doing = Some(match tool {
            ToolKind::Question => "Asking you a question".to_owned(),
            _ => "Waiting for your approval".to_owned(),
        });
        self.asks.insert(id, Ask { rpc, kind, questions: question_ids, can_decline });
        core.add_approval(approval);
        self.settle(core);
    }

    fn thread_status(&mut self, core: &mut Core, status: &Value) {
        let next = match status.get("type").and_then(Value::as_str) {
            Some("active") if status.get("activeFlags").and_then(Value::as_array).is_some_and(|f| f.iter().any(|v| matches!(v.as_str(), Some("waitingOnApproval" | "waitingOnUserInput")))) => Status::Waiting,
            Some("active") => Status::Running,
            Some("idle") => Status::Idle,
            Some("notLoaded") => Status::Starting,
            Some("systemError") => Status::Exited,
            _ => return,
        };
        self.thread_status = Some(next);
        if !matches!(next, Status::Running | Status::Waiting) {
            self.turn = None;
            core.state.doing = None;
            core.set_status(next);
        }
        core.state.exit = (next == Status::Exited).then(|| ExitInfo { code: None, error: Some("Codex reported a thread error".into()) });
        core.touch_state();
        self.settle(core);
    }

    fn notification(&mut self, core: &mut Core, method: &str, p: Value) {
        // One app-server can report child threads too. Their turns must not end the parent's.
        if method != "serverRequest/resolved"
            && let (Some(own), Some(thread)) = (self.thread.as_deref(), p.get("threadId").and_then(Value::as_str))
            && own != thread
        {
            return;
        }
        match method {
            "thread/status/changed" => self.thread_status(core, p.get("status").unwrap_or(&Value::Null)),
            "turn/started" => {
                self.thread_status = Some(Status::Running);
                if let Some(t) = p.get("turn").and_then(|t| t.get("id")).and_then(Value::as_str) {
                    self.turn = Some(t.to_owned());
                }
                self.steer_waiting(core);
                if core.state.turn_started.is_none() {
                    core.state.turn_started = Some(Core::now());
                }
                core.state.doing = Some("Thinking".to_owned());
                core.touch_state();
                self.settle(core);
            }
            "turn/completed" => {
                let turn = p.get("turn").cloned().unwrap_or(Value::Null);
                if let (Some(current), Some(completed)) = (self.turn.as_deref(), turn.get("id").and_then(Value::as_str))
                    && current != completed
                {
                    return;
                }
                let status = turn.get("status").and_then(Value::as_str).unwrap_or("completed");
                match status {
                    "interrupted" => self.notice(core, Level::Info, "Interrupted"),
                    "failed" => {
                        let err = turn.get("error").cloned().unwrap_or(Value::Null);
                        let msg = err.get("message").and_then(Value::as_str).unwrap_or("The turn failed.").to_owned();
                        if usage_limited(&err) {
                            self.limit_reached(core, &msg);
                        } else {
                            self.notice(core, Level::Error, msg);
                        }
                    }
                    _ => {}
                }
                let done = if status == "completed" { Delivery::Completed } else { Delivery::Cancelled };
                for id in std::mem::take(&mut self.turn_messages) {
                    if core.state.delivery.contains_key(&id) {
                        core.state.delivery.insert(id, done);
                    }
                }
                // Calls the turn left open were cut off with it; terminals still running were left
                // behind (they go on after the turn).
                let terminals: Vec<String> = core.state.tasks.iter().filter(|t| t.live() && t.process.is_some()).filter_map(|t| t.tool_id.clone()).collect();
                for t in core.state.tasks.iter_mut().filter(|t| t.live() && t.process.is_some()) {
                    t.background = true;
                }
                let open: Vec<String> = core
                    .events()
                    .filter_map(|e| match &e.body {
                        Body::Tool(t) if matches!(t.status, ToolStatus::Running | ToolStatus::Pending) && !terminals.contains(&e.id) => Some(e.id.clone()),
                        _ => None,
                    })
                    .collect();
                for id in open {
                    set_status(core, &id, ToolStatus::Cancelled);
                }
                self.turn = None;
                if !matches!(self.thread_status, Some(Status::Starting | Status::Exited)) {
                    self.thread_status = Some(Status::Idle);
                }
                // How it ended, for kiln's keep-going.
                let how = match status {
                    "completed" => "done",
                    "interrupted" => "stopped",
                    _ => "cut",
                };
                core.end_turn(core.state.turn_started, how, Some(status));
                core.state.turn_started = None;
                core.state.doing = None;
                core.state.usage.turns += 1;
                core.touch_state();
                self.settle(core);
                self.next(core);
            }
            "item/started" | "item/completed" => {
                if let Some(item) = p.get("item") {
                    if item.get("type").and_then(Value::as_str) == Some("userMessage")
                        && let Some(turn) = p.get("turnId").and_then(Value::as_str)
                    {
                        let key = item.get("clientId").or_else(|| item.get("id")).and_then(Value::as_str).unwrap_or("").to_owned();
                        self.turns.insert(key, turn.to_owned());
                    }
                    self.item(core, item, method == "item/completed");
                }
            }
            "item/agentMessage/delta" | "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" | "item/plan/delta" => {
                let (Some(id), Some(d)) = (p.get("itemId").and_then(Value::as_str), p.get("delta").and_then(Value::as_str)) else { return };
                if core.get(id).is_none() {
                    let body = if matches!(method, "item/agentMessage/delta" | "item/plan/delta") {
                        Body::Assistant { text: String::new(), streaming: true }
                    } else {
                        Body::Thinking { text: String::new(), streaming: true, redacted: false }
                    };
                    core.upsert(id, None, body);
                }
                core.append(id, d);
                if method == "item/agentMessage/delta" && core.state.doing.as_deref() != Some("Writing") {
                    core.state.doing = Some("Writing".to_owned());
                    core.touch_state();
                }
            }
            "item/reasoning/summaryPartAdded" => {
                let Some(id) = p.get("itemId").and_then(Value::as_str) else { return };
                if core.get(id).is_none() {
                    core.upsert(id, None, Body::Thinking { text: String::new(), streaming: true, redacted: false });
                }
                if p.get("summaryIndex").and_then(Value::as_u64).is_some_and(|i| i > 0) {
                    core.append(id, "\n\n");
                }
            }
            "item/commandExecution/outputDelta" | "item/fileChange/outputDelta" => {
                let (Some(id), Some(d)) = (p.get("itemId").and_then(Value::as_str), p.get("delta").and_then(Value::as_str)) else { return };
                if d.is_empty() {
                    return;
                }
                // A terminal's whole output is kept (the call shows only its start).
                if let Some(task) = core.state.tasks.iter().find(|t| t.tool_id.as_deref() == Some(id) && t.process.is_some()).map(|t| t.id.clone()) {
                    core.task_output.entry(task).or_default().append(d.as_bytes());
                }
                if core.get(id).is_some_and(|e| matches!(&e.body, Body::Tool(t) if t.truncated && t.output.as_ref().is_some_and(|out| out.len() >= OUTPUT_MAX * 2))) {
                    return;
                }
                if let Some(e) = core.get_mut(id)
                    && let Body::Tool(t) = &mut e.body
                {
                    let out = t.output.get_or_insert_with(String::new);
                    if out.len() < OUTPUT_MAX * 2 {
                        out.push_str(d);
                    } else {
                        t.truncated = true;
                    }
                }
            }
            "turn/plan/updated" => {
                core.state.todos = p
                    .get("plan")
                    .and_then(Value::as_array)
                    .map(|steps| {
                        steps
                            .iter()
                            .filter_map(|s| {
                                Some(Todo {
                                    text: s.get("step").and_then(Value::as_str)?.to_owned(),
                                    status: match s.get("status").and_then(Value::as_str).unwrap_or("pending") {
                                        "completed" => "completed",
                                        "inProgress" => "in_progress",
                                        _ => "pending",
                                    }
                                    .to_owned(),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                core.touch_state();
            }
            "thread/tokenUsage/updated" => {
                let u = p.get("tokenUsage").cloned().unwrap_or(Value::Null);
                let total = u.get("total").cloned().unwrap_or(Value::Null);
                let n = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
                core.state.usage.input_tokens = n(&total, "inputTokens");
                core.state.usage.output_tokens = n(&total, "outputTokens");
                core.state.usage.cache_read_tokens = n(&total, "cachedInputTokens");
                if let Some(last) = u.get("last") {
                    core.state.usage.context_tokens = Some(n(last, "inputTokens") + n(last, "outputTokens"));
                }
                core.state.usage.context_window = u.get("modelContextWindow").and_then(Value::as_u64);
                core.touch_state();
            }
            // The thread's goal (/goal): Codex works toward it until it says it's complete.
            "thread/goal/updated" => {
                let Some(g) = p.get("goal").filter(|g| g.is_object()) else { return };
                let text = g.get("objective").and_then(Value::as_str).unwrap_or("");
                let (status, reason) = match g.get("status").and_then(Value::as_str).unwrap_or("active") {
                    "complete" => ("met", None),
                    "paused" => ("paused", None),
                    "blocked" => ("unmet", Some("Codex is blocked on it.")),
                    "usageLimited" => ("unmet", Some("Codex reached its usage limit.")),
                    "budgetLimited" => ("unmet", Some("Codex used the goal's token budget.")),
                    _ => ("active", None),
                };
                core.state.objective = Some(Objective::new(text, status, reason.map(str::to_owned)));
                core.touch_state();
            }
            "thread/goal/cleared" => {
                if let Some(o) = core.state.objective.as_ref().filter(|o| o.status != "cleared") {
                    core.state.objective = Some(Objective::new(&o.text, "cleared", None));
                    core.touch_state();
                }
            }
            "thread/name/updated" => {
                if let Some(n) = p.get("name").or_else(|| p.get("threadName")).and_then(Value::as_str) {
                    core.state.title = n.to_owned();
                    core.touch_state();
                }
            }
            "serverRequest/resolved" => {
                let id = match p.get("requestId") {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => return,
                };
                self.asks.remove(&id);
                if core.take_approval(&id).is_some() {
                    self.settle(core);
                }
            }
            "error" => {
                let err = p.get("error").cloned().unwrap_or(Value::Null);
                let msg = err.get("message").and_then(Value::as_str).unwrap_or("Codex reported an error").to_owned();
                let retry = p.get("willRetry").and_then(Value::as_bool).unwrap_or(false);
                if !retry && usage_limited(&err) {
                    self.limit_reached(core, &msg);
                } else {
                    self.notice(core, if retry { Level::Warn } else { Level::Error }, if retry { format!("{msg} (retrying)") } else { msg });
                }
            }
            "warning" | "deprecationNotice" | "configWarning" | "guardianWarning" | "model/rerouted" => {
                let msg = p.get("message").or_else(|| p.get("summary")).and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| method.to_owned());
                self.notice(core, Level::Warn, msg);
            }
            "thread/compacted" => self.notice(core, Level::Info, "Conversation compacted"),
            "account/rateLimits/updated" => {
                // Sparse: what a rolling update leaves out (or sends as null) stays as it was.
                let Some(snap) = p.get("rateLimits").and_then(Value::as_object) else { return };
                let id = snap.get("limitId").and_then(Value::as_str).unwrap_or("codex").to_owned();
                let entry = self.rate_limits.entry(id).or_insert_with(|| json!({}));
                if let Some(e) = entry.as_object_mut() {
                    for (k, v) in snap {
                        if !v.is_null() {
                            e.insert(k.clone(), v.clone());
                        }
                    }
                }
                self.apply_limits(core);
            }
            "account/updated" => self.request(core, Pending::Account, "account/read", json!({})),
            "mcpServer/startupStatus/updated" => {
                let Some(name) = p.get("name").and_then(Value::as_str) else { return };
                let mut state = p.get("status").and_then(Value::as_str).unwrap_or("starting").to_owned();
                if p.get("failureReason").and_then(Value::as_str) == Some("reauthenticationRequired") {
                    state = "authenticationRequired".to_owned();
                }
                self.mcp_startup.insert(name.to_owned(), (state, p.get("error").and_then(Value::as_str).map(str::to_owned)));
                self.compose_mcp(core);
            }
            "mcpServer/oauthLogin/completed" => {
                let name = p.get("name").and_then(Value::as_str).unwrap_or("the server").to_owned();
                if p.get("success").and_then(Value::as_bool) == Some(true) {
                    self.notice(core, Level::Info, format!("Signed in to {name}"));
                    self.mcp_startup.remove(&name);
                } else {
                    let err = p.get("error").and_then(Value::as_str).unwrap_or("it did not finish");
                    self.notice(core, Level::Warn, format!("Signing in to {name} failed: {err}"));
                }
                self.mcp_reload(core);
            }
            "skills/changed" => {
                let cwd = core.state.cwd.clone();
                self.request(core, Pending::Skills, "skills/list", json!({"cwds": [cwd], "forceReload": true}));
            }
            _ => {}
        }
    }

    /// A terminal Codex runs (its process outlives the call that started it) as a task: running
    /// until its item completes.
    #[allow(clippy::too_many_arguments)]
    fn terminal(&mut self, core: &mut Core, item_id: &str, process: &str, command: &str, item: &Value, done: bool, exit: Option<i32>) {
        let task_id = format!("terminal-{item_id}");
        let status = match (done, item.get("status").and_then(Value::as_str)) {
            (false, _) => "running",
            (true, _) if self.stopping.remove(&task_id) => "stopped",
            (true, Some("failed")) => "failed",
            (true, _) if exit.unwrap_or(0) != 0 => "failed",
            (true, _) => "completed",
        };
        match core.state.tasks.iter_mut().find(|t| t.id == task_id) {
            Some(t) => {
                if t.live() {
                    t.status = status.to_owned();
                }
                t.exit_code = exit.or(t.exit_code);
                if done {
                    t.ended.get_or_insert_with(Core::now);
                }
            }
            None => core.state.tasks.push(Task {
                id: task_id,
                tool_id: Some(item_id.to_owned()),
                description: clip_middle(command, 300).0,
                kind: "terminal".into(),
                status: status.to_owned(),
                class: Some("shell".into()),
                command: Some(command.to_owned()),
                cwd: item.get("cwd").and_then(Value::as_str).map(str::to_owned),
                process: Some(process.to_owned()),
                exit_code: exit,
                stoppable: cfg!(unix),
                started: Core::now(),
                ended: done.then(Core::now),
                ..Task::default()
            }),
        }
        core.touch_state();
    }

    /// The installed plugins, with the skills each brings.
    fn plugins(&self) -> Vec<Plugin> {
        self.plugins_raw
            .iter()
            .map(|(market, p)| {
                let id = p.get("id").and_then(Value::as_str).unwrap_or("");
                let name = p.pointer("/interface/displayName").and_then(Value::as_str).or_else(|| p.get("name").and_then(Value::as_str)).unwrap_or(id).to_owned();
                Plugin {
                    name,
                    source: market.clone(),
                    enabled: p.get("enabled").and_then(Value::as_bool).unwrap_or(true),
                    skills: Vec::new(),
                    commands: Vec::new(),
                    version: p.get("localVersion").and_then(Value::as_str).map(str::to_owned),
                }
            })
            .collect()
    }

    /// One thread item, started or completed (`done`), into the timeline.
    fn item(&mut self, core: &mut Core, item: &Value, done: bool) {
        let Some(id) = item.get("id").and_then(Value::as_str).map(str::to_owned) else { return };
        let ty = item.get("type").and_then(Value::as_str).unwrap_or("");
        let cwd = core.state.cwd.clone();
        match ty {
            "userMessage" => {
                let text = item
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|c| c.iter().filter_map(|x| x.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"))
                    .unwrap_or_default();
                let text = text.split("\n\nAttached files:\n").next().unwrap_or(&text).to_owned();
                let client = item.get("clientId").and_then(Value::as_str).map(str::to_owned);
                let key = client.clone().unwrap_or(id);
                if let Some(c) = &client
                    && core.state.delivery.contains_key(c)
                {
                    core.state.delivery.insert(c.clone(), if done { Delivery::Completed } else { Delivery::Started });
                    core.touch_state();
                }
                if core.state.goal.is_empty() && !text.trim().is_empty() {
                    core.state.goal = clip_middle(text.trim(), 600).0;
                }
                // What it carried: as kiln showed it when sending, else from its input items.
                let content = item.get("content").and_then(Value::as_array).cloned().unwrap_or_default();
                let counted = u32::try_from(content.iter().filter(|x| matches!(x.get("type").and_then(Value::as_str), Some("image" | "localImage"))).count()).unwrap_or(0);
                let named: Vec<String> = content
                    .iter()
                    .filter(|x| matches!(x.get("type").and_then(Value::as_str), Some("localImage" | "mention")))
                    .filter_map(|x| x.get("name").or_else(|| x.get("path")).and_then(Value::as_str))
                    .map(|p| p.rsplit('/').next().unwrap_or(p).to_owned())
                    .collect();
                let (images, attachments) = match core.get(&key).map(|e| &e.body) {
                    Some(Body::User { images, attachments, .. }) if !attachments.is_empty() => ((*images).max(counted), attachments.clone()),
                    _ => (counted, named),
                };
                core.upsert(&key, None, Body::User { text, images, client_id: client, origin: Some("human".into()), attachments });
            }
            "agentMessage" => {
                let text = item.get("text").and_then(Value::as_str).unwrap_or("").to_owned();
                if done && !text.trim().is_empty() {
                    core.state.summary = clip_middle(text.trim(), 600).0;
                    core.touch_state();
                }
                if done || !text.is_empty() {
                    core.upsert(&id, None, Body::Assistant { text, streaming: !done });
                } else if core.get(&id).is_none() {
                    core.upsert(&id, None, Body::Assistant { text: String::new(), streaming: true });
                }
            }
            "reasoning" => {
                let parts = |k: &str| -> String {
                    item.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().or_else(|| x.get("text").and_then(Value::as_str))).collect::<Vec<_>>().join("\n\n")).unwrap_or_default()
                };
                let text = if parts("summary").is_empty() { parts("content") } else { parts("summary") };
                if done || core.get(&id).is_none() {
                    let redacted = done && text.is_empty();
                    core.upsert(&id, None, Body::Thinking { text, streaming: !done, redacted });
                }
            }
            "plan" => {
                if done || core.get(&id).is_none() {
                    let text = item.get("text").and_then(Value::as_str).unwrap_or("").to_owned();
                    core.upsert(&id, None, Body::Assistant { text, streaming: !done });
                }
            }
            "commandExecution" => {
                let actions = item.get("commandActions").and_then(Value::as_array).cloned().unwrap_or_default();
                let full = item.get("command").and_then(Value::as_str).unwrap_or("");
                // The command as written: the single action's, else unwrapped from `bash -lc '…'`.
                let command = match actions.as_slice() {
                    [one] => one.get("command").and_then(Value::as_str).map_or_else(|| unwrap_shell(full), str::to_owned),
                    _ => unwrap_shell(full),
                };
                let (tool, title) = match actions.first().and_then(|a| a.get("type")).and_then(Value::as_str) {
                    Some("read") if actions.len() == 1 => (ToolKind::Read, actions.first().and_then(|a| a.get("path")).and_then(Value::as_str).map_or_else(|| command.clone(), |p| super::claude::rel(p, &cwd))),
                    Some("search") | Some("listFiles") if actions.len() == 1 => (ToolKind::Search, command.clone()),
                    _ => (ToolKind::Shell, command.clone()),
                };
                let exit = item.get("exitCode").and_then(Value::as_i64).and_then(|c| i32::try_from(c).ok());
                let status = match item.get("status").and_then(Value::as_str).unwrap_or("inProgress") {
                    "completed" if exit.unwrap_or(0) == 0 => ToolStatus::Done,
                    "completed" | "failed" => ToolStatus::Error,
                    "declined" => ToolStatus::Denied,
                    _ => ToolStatus::Running,
                };
                let streamed = core.get(&id).and_then(|e| match &e.body {
                    Body::Tool(t) => t.output.clone(),
                    _ => None,
                });
                let (output, truncated) = match item.get("aggregatedOutput").and_then(Value::as_str) {
                    Some(o) => clip_middle(o, OUTPUT_MAX),
                    None => streamed.map(|o| clip_middle(&o, OUTPUT_MAX)).unwrap_or_default(),
                };
                let call = ToolCall {
                    tool_id: id.clone(),
                    name: "shell".into(),
                    tool,
                    title,
                    detail: item.get("cwd").and_then(Value::as_str).filter(|c| *c != cwd).map(|c| format!("in {c}")),
                    input: json!({"command": command}),
                    status,
                    output: (!output.is_empty()).then_some(output),
                    truncated,
                    diffs: Vec::new(),
                    todos: Vec::new(),
                    task: None,
                    exit_code: exit,
                    started: started_of(core, &id),
                    ended: done.then(Core::now),
                    server: None,
                    tool_short: None,
                    skill: None,
                };
                if !done {
                    core.state.doing = Some(super::claude::doing(&call));
                    core.touch_state();
                }
                // A terminal (unified exec): it can go on after its call and the turn.
                if let Some(process) = item.get("processId").and_then(Value::as_str) {
                    self.terminal(core, &id, process, &command, item, done, exit);
                }
                core.upsert(&id, None, Body::Tool(Box::new(call)));
            }
            "fileChange" => {
                let changes = item.get("changes").and_then(Value::as_array).cloned().unwrap_or_default();
                let diffs = file_changes(item.get("changes"), &cwd);
                let status = match item.get("status").and_then(Value::as_str).unwrap_or("inProgress") {
                    "completed" => ToolStatus::Done,
                    "failed" => ToolStatus::Error,
                    "declined" => ToolStatus::Denied,
                    _ => ToolStatus::Running,
                };
                if done && status == ToolStatus::Done {
                    for c in &changes {
                        let Some(path) = c.get("path").and_then(Value::as_str) else { continue };
                        let abs = if std::path::Path::new(path).is_absolute() { path.to_owned() } else { format!("{}/{path}", cwd.trim_end_matches('/')) };
                        let kind = c.get("kind").and_then(|k| k.get("type")).and_then(Value::as_str).unwrap_or("update");
                        let diff = c.get("diff").and_then(Value::as_str).unwrap_or("");
                        let baseline = match kind {
                            "add" => Baseline::Absent,
                            _ => Baseline::Reverse(super::diff::parse_unified(diff)),
                        };
                        core.note_baseline(&abs, baseline);
                        core.want_change(&abs, &super::claude::rel(path, &cwd));
                    }
                }
                let title = diffs.iter().map(|d| d.path.clone()).collect::<Vec<_>>().join(", ");
                let call = ToolCall {
                    tool_id: id.clone(),
                    name: "apply_patch".into(),
                    tool: ToolKind::Edit,
                    title: if title.is_empty() { "File changes".into() } else { title },
                    detail: (diffs.len() > 1).then(|| format!("{} files", diffs.len())),
                    input: json!({}),
                    status,
                    output: None,
                    truncated: false,
                    diffs,
                    todos: Vec::new(),
                    task: None,
                    exit_code: None,
                    started: started_of(core, &id),
                    ended: done.then(Core::now),
                    server: None,
                    tool_short: None,
                    skill: None,
                };
                if !done {
                    core.state.doing = Some(super::claude::doing(&call));
                    core.touch_state();
                }
                core.upsert(&id, None, Body::Tool(Box::new(call)));
            }
            "mcpToolCall" | "dynamicToolCall" => {
                let server = item.get("server").or_else(|| item.get("namespace")).and_then(Value::as_str).unwrap_or("");
                let tool_name = item.get("tool").and_then(Value::as_str).unwrap_or("tool");
                let status = match item.get("status").and_then(Value::as_str).unwrap_or("inProgress") {
                    "completed" if item.get("success").and_then(Value::as_bool) != Some(false) => ToolStatus::Done,
                    "completed" | "failed" => ToolStatus::Error,
                    _ => ToolStatus::Running,
                };
                let output = item
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| item.get("result").map(|r| clip_middle(&r.to_string(), OUTPUT_MAX).0));
                let reach = crate::reach::describe(server, tool_name, item.get("arguments").unwrap_or(&Value::Null));
                // A command run on another runner (kiln reach): its output and exit code.
                let shown = item.get("result").and_then(|r| crate::reach::shown(&format!("mcp__{server}__{tool_name}"), r));
                let output = match &shown {
                    Some(s) => Some(clip_middle(&s.output, OUTPUT_MAX).0),
                    None => output,
                };
                let call = ToolCall {
                    tool_id: id.clone(),
                    name: format!("mcp__{server}__{tool_name}"),
                    tool: reach.as_ref().map(|r| r.0).unwrap_or(if ty == "mcpToolCall" { ToolKind::Mcp } else { ToolKind::Other }),
                    title: reach.as_ref().map(|r| r.1.clone()).unwrap_or_else(|| if server.is_empty() { tool_name.to_owned() } else { format!("{server} · {tool_name}") }),
                    detail: shown.as_ref().and_then(|s| s.runner.as_ref()).map(|r| format!("on {r}")).or_else(|| reach.and_then(|r| r.2)),
                    input: clip_json(item.get("arguments").unwrap_or(&Value::Null), 16 * 1024),
                    status,
                    output,
                    truncated: false,
                    diffs: Vec::new(),
                    todos: Vec::new(),
                    task: None,
                    exit_code: shown.as_ref().and_then(|s| s.exit_code),
                    started: started_of(core, &id),
                    ended: done.then(Core::now),
                    server: None,
                    tool_short: None,
                    skill: None,
                };
                core.upsert(&id, None, Body::Tool(Box::new(call)));
            }
            "webSearch" => {
                let q = item.get("query").and_then(Value::as_str).unwrap_or("").to_owned();
                let call = ToolCall {
                    tool_id: id.clone(),
                    name: "WebSearch".into(),
                    tool: ToolKind::Web,
                    title: q,
                    detail: None,
                    input: json!({}),
                    status: if done { ToolStatus::Done } else { ToolStatus::Running },
                    output: item.get("results").map(|r| clip_middle(&r.to_string(), OUTPUT_MAX).0),
                    truncated: false,
                    diffs: Vec::new(),
                    todos: Vec::new(),
                    task: None,
                    exit_code: None,
                    started: started_of(core, &id),
                    ended: done.then(Core::now),
                    server: None,
                    tool_short: None,
                    skill: None,
                };
                core.upsert(&id, None, Body::Tool(Box::new(call)));
            }
            "collabAgentToolCall" | "subAgentActivity" => {
                let desc = item.get("prompt").or_else(|| item.get("agentPath")).and_then(Value::as_str).unwrap_or("subagent").to_owned();
                let mut task = None;
                let status = if ty == "collabAgentToolCall" {
                    let status = match item.get("status").and_then(Value::as_str) {
                        Some("completed") => ToolStatus::Done,
                        Some("failed") => ToolStatus::Error,
                        Some("interrupted") => ToolStatus::Cancelled,
                        _ => ToolStatus::Running,
                    };
                    let spawned = item.get("tool").and_then(Value::as_str) == Some("spawnAgent");
                    let states = item.get("agentsStates").and_then(Value::as_object);
                    let mut targets: Vec<&str> = item.get("receiverThreadIds").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
                    for target in states.into_iter().flat_map(|s| s.keys().map(String::as_str)) {
                        if !targets.contains(&target) {
                            targets.push(target);
                        }
                    }
                    for target in targets {
                        let child = match states.and_then(|s| s.get(target)).and_then(|s| s.get("status")).and_then(Value::as_str) {
                            Some("pendingInit") => Some("pending"),
                            Some("running") => Some("running"),
                            Some("completed") => Some("completed"),
                            Some("errored") => Some("failed"),
                            Some("interrupted" | "shutdown" | "notFound") => Some("stopped"),
                            _ if spawned && status == ToolStatus::Done && !core.state.tasks.iter().any(|t| t.id == target) => Some("pending"),
                            _ => None,
                        };
                        if let Some(child) = child {
                            subagent(core, target, &id, &desc, child, spawned);
                        }
                        task.get_or_insert_with(|| target.to_owned());
                    }
                    status
                } else {
                    let Some(target) = item.get("agentThreadId").and_then(Value::as_str) else { return };
                    let child = match item.get("kind").and_then(Value::as_str) {
                        Some("started" | "interacted") => "running",
                        Some("interrupted") => "stopped",
                        Some("completed") => "completed",
                        _ => return,
                    };
                    subagent(core, target, &id, &desc, child, false);
                    task = Some(target.to_owned());
                    if done { ToolStatus::Done } else { ToolStatus::Running }
                };
                let call = ToolCall {
                    tool_id: id.clone(),
                    name: ty.to_owned(),
                    tool: ToolKind::Agent,
                    title: clip_middle(&desc, 200).0,
                    detail: item.get("model").and_then(Value::as_str).map(str::to_owned),
                    input: json!({}),
                    status,
                    output: None,
                    truncated: false,
                    diffs: Vec::new(),
                    todos: Vec::new(),
                    task,
                    exit_code: None,
                    started: started_of(core, &id),
                    ended: done.then(Core::now),
                    server: None,
                    tool_short: None,
                    skill: None,
                };
                core.upsert(&id, None, Body::Tool(Box::new(call)));
            }
            "contextCompaction" if done => self.notice(core, Level::Info, "Conversation compacted"),
            "enteredReviewMode" if done => self.notice(core, Level::Info, "Reviewing"),
            "exitedReviewMode" if done => self.notice(core, Level::Info, "Review finished"),
            _ => {}
        }
        self.settle(core);
    }
}

/// The first lines of Codex's own /init prompt.
const INIT_PROMPT: &str = "Generate a file named AGENTS.md that serves as a contributor guide for this repository. Before writing, check whether AGENTS.md already exists in the current working directory. If it does, do not overwrite or modify it. Produce a clear, concise (200-400 words), well-structured document titled \"Repository Guidelines\", with sections as they apply: project structure and module organization; build, test and development commands; coding style and naming conventions; testing guidelines; commit and pull request guidelines.";

/// Codex's own commands (its TUI's), and what each does from kiln: Codex's app-server runs no
/// slash commands, so each is a kiln panel or control, a prompt, or unavailable.
fn codex_catalog() -> Vec<SlashCommand> {
    let c = |name: &str, desc: &str, hint: &str, group: &str, command: Value| SlashCommand::control(name, desc, hint, group, command);
    let v = SlashCommand::view;
    let no = SlashCommand::unsupported;
    // Done in Codex's own screen, opened beside the agent in its folder.
    let t = SlashCommand::terminal;
    vec![
        v("model", "Choose the model and reasoning effort", "Model & mode", "model"),
        v("approvals", "Choose what Codex is allowed to do", "Model & mode", "permissions"),
        v("permissions", "Choose what Codex is allowed to do", "Model & mode", "permissions"),
        c("review", "Review the current changes and find issues", "[instructions]", "Context", json!({"type": "control", "id": "review", "value": {"text": "{args}"}})),
        c("compact", "Summarize the conversation to free context", "", "Context", json!({"type": "control", "id": "compact"})),
        v("diff", "The changes (git diff, untracked files too)", "Context", "changes"),
        v("mention", "Mention a file", "Context", "files"),
        SlashCommand::new("init", "Create an AGENTS.md file with instructions for Codex", "", "Context", Route::Prompt { text: Some(INIT_PROMPT.to_owned()), skill: false }),
        v("status", "Session configuration and token usage", "Session", "status"),
        v("usage", "Account usage and limits", "Session", "usage"),
        c("rename", "Rename the conversation", "<name>", "Session", json!({"type": "control", "id": "rename", "value": "{args}"})),
        c("goal", "Set, show, pause, resume or clear the goal for a long-running task", "[<objective>|clear|pause|resume]", "Session", json!({"type": "control", "id": "goal", "value": {"text": "{args}", "client": "{client}"}})),
        c("fork", "Fork the conversation into another agent", "", "Session", json!({"type": "control", "id": "fork"})),
        c("undo", "Undo the last turn", "", "Session", json!({"type": "control", "id": "undo"})),
        c("export", "The conversation as Markdown", "", "Session", json!({"type": "control", "id": "export", "value": {"client": "{client}"}})),
        c("rollout", "The rollout file's path", "", "Session", json!({"type": "control", "id": "rollout", "value": {"client": "{client}"}})),
        c("pwd", "The working directory", "", "Session", json!({"type": "control", "id": "pwd", "value": {"client": "{client}"}})),
        v("mcp", "MCP servers: status, tools, sign-in", "Tools & MCP", "mcp"),
        v("ps", "Background terminals", "Tools & MCP", "tasks"),
        v("agent", "This session's subagents", "Tools & MCP", "tasks"),
        v("hooks", "Lifecycle hooks", "Tools & MCP", "hooks"),
        v("skills", "Skills: view and run", "Skills", "skills"),
        v("plugins", "Installed plugins", "Plugins", "plugins"),
        c("stop-tasks", "Stop every background terminal and task", "", "Kiln", json!({"type": "stop_tasks"})),
        c("restart", "Start Codex again, continuing the conversation", "", "Kiln", json!({"type": "restart"})),
        v("new", "Start a new conversation (another agent)", "Kiln", "new_agent"),
        v("terminal", "A shell in this agent's folder, on its machine", "Kiln", "terminal").aka(&["shell"]),
        v("screen", "Codex's own screen, in this agent's folder", "Kiln", "screen"),
        v("goals", "Goals one after another: each a /goal, the next once it's complete", "Kiln", "goals"),
        v("clear", "Start a new conversation (another agent)", "Kiln", "new_agent"),
        no("resume", "Resume a saved chat", "Session", "continue another conversation from kiln's agents list"),
        no("plan", "Switch to Plan mode", "Model & mode", "Codex's Plan mode is not offered through its app-server"),
        no("side", "A side conversation in an ephemeral fork", "Context", "Codex's side conversations are not offered through its app-server"),
        no("copy", "Copy the last response", "Session", "copy from the console"),
        no("cd", "Change the working directory", "Session", "an agent's folder is set when it starts"),
        no("worktree", "Continue in a new worktree", "Session", "start an agent in the worktree's folder"),
        no("archive", "Archive this session", "Session", "manage saved conversations with the Codex CLI"),
        no("delete", "Delete this session", "Session", "manage saved conversations with the Codex CLI"),
        v("login", "Sign Codex in on this machine", "Session", "login"),
        v("logout", "Sign Codex out on this machine", "Session", "logout"),
        no("feedback", "Send logs to the maintainers", "Session", "it sends logs to OpenAI: use the Codex CLI"),
        no("quit", "Exit Codex", "Session", "close the agent in kiln"),
        no("exit", "Exit Codex", "Session", "close the agent in kiln"),
        t("memory", "Configure memory", "Context"),
        t("apps", "Manage apps", "Tools & MCP"),
        t("experimental", "Toggle experimental features", "Session"),
        t("debug-config", "Config layers and requirement sources", "Session"),
        t("theme", "Syntax highlighting theme", "Session"),
        t("statusline", "Status line items", "Session"),
        t("title", "Terminal title items", "Session"),
        t("raw", "Raw scrollback mode", "Session"),
        t("vim", "Vim keys in the composer", "Session"),
        no("voice", "Voice input", "Session", "it listens to the microphone of the machine Codex runs on: use the console's microphone"),
        no("ide", "Context from your IDE", "Context", "Codex's IDE integration is not offered through its app-server"),
    ]
}

/// Answer the client that asked, or say it in the timeline when no one is waiting.
fn answer(core: &mut Core, client: Option<String>, value: Value) {
    match client.filter(|c| !c.is_empty()) {
        Some(c) => core.reply(c, value),
        None => {
            let ok = value.get("ok").and_then(Value::as_bool) == Some(true);
            let text = value.get("message").or_else(|| value.get("error")).and_then(Value::as_str).unwrap_or(if ok { "Done." } else { "That did not work." }).to_owned();
            let n = core.events().count();
            core.upsert(&format!("notice-run-{n}-{}", Core::now()), None, Body::Notice { level: if ok { Level::Info } else { Level::Warn }, text });
        }
    }
}

/// `codex mcp add` arguments (Codex speaks stdio and streamable HTTP; an http server is written to
/// the configuration instead, see `Command::McpAdd`).
fn codex_add_args(add: &McpAdd) -> Result<Vec<String>, String> {
    add.check()?;
    let mut a: Vec<String> = vec!["mcp".into(), "add".into(), add.name.clone()];
    match add.transport().as_str() {
        "stdio" => {
            for (k, v) in &add.env {
                a.extend(["--env".into(), format!("{k}={v}")]);
            }
            a.push("--".into());
            a.push(add.command.clone().unwrap_or_default().trim().to_owned());
            a.extend(add.args.iter().cloned());
        }
        "http" => a.extend(["--url".into(), add.url.clone().unwrap_or_default().trim().to_owned()]),
        other => return Err(format!("Codex connects to MCP servers over stdio or streamable HTTP, not {other}.")),
    }
    Ok(a)
}

/// A limit window's name from its length.
fn window_label(mins: Option<i64>, fallback: &str) -> String {
    match mins {
        Some(m) if m > 0 && m <= 1440 => "Session".to_owned(),
        Some(m) if (10_000..=10_200).contains(&m) => "Weekly".to_owned(),
        Some(m) if m >= 40_000 => "Monthly".to_owned(),
        Some(m) if m > 0 => format!("{} window", window_words(m)),
        _ => fallback.to_owned(),
    }
}

/// One MCP server from what Codex says about it: its status row, its start-up state and its
/// configuration.
fn codex_server(name: &str, row: Option<&Value>, cfg: Option<&Value>, startup: Option<&(String, Option<String>)>) -> McpServer {
    let enabled = cfg.and_then(|c| c.get("enabled")).and_then(Value::as_bool) != Some(false);
    let auth_status = row.and_then(|r| r.get("authStatus")).and_then(Value::as_str);
    let runtime = row.and_then(|r| r.get("runtimeStatus")).and_then(Value::as_str);
    let tools: Vec<McpTool> = {
        let mut t: Vec<McpTool> = row
            .and_then(|r| r.get("tools"))
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .map(|(k, v)| McpTool {
                        name: v.get("name").and_then(Value::as_str).unwrap_or(k).to_owned(),
                        description: v.get("description").and_then(Value::as_str).map(|d| clip_middle(d, 600).0),
                    })
                    .collect()
            })
            .unwrap_or_default();
        t.sort_by(|a, b| a.name.cmp(&b.name));
        t
    };
    let state = runtime.map(str::to_owned).or_else(|| startup.map(|s| s.0.clone()));
    let status = if !enabled {
        McpStatus::Disabled
    } else {
        match state.as_deref() {
            Some("connected" | "ready") => McpStatus::Connected,
            Some("starting" | "notStarted") => McpStatus::Pending,
            Some("authenticationRequired") => McpStatus::NeedsAuth,
            Some("failed" | "cancelled") => McpStatus::Failed,
            Some("disabled") => McpStatus::Disabled,
            _ if auth_status == Some("notLoggedIn") => McpStatus::NeedsAuth,
            _ if row.is_some_and(|r| r.get("toolsError").is_some_and(|e| !e.is_null())) => McpStatus::Failed,
            _ if !tools.is_empty() => McpStatus::Connected,
            _ => McpStatus::Pending,
        }
    };
    let error = row
        .and_then(|r| r.get("toolsError"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| startup.and_then(|s| s.1.clone()))
        .filter(|_| matches!(status, McpStatus::Failed | McpStatus::NeedsAuth));
    let transport = match cfg {
        Some(c) if c.get("url").is_some() => "http",
        Some(c) if c.get("command").is_some() => "stdio",
        _ if row.and_then(|r| r.get("httpOrigin")).is_some_and(|o| !o.is_null()) => "http",
        _ => "",
    };
    let target = cfg
        .and_then(|c| match (c.get("url").and_then(Value::as_str), c.get("command").and_then(Value::as_str)) {
            (Some(u), _) => Some(safe_url(u)),
            (None, Some(cmd)) => Some(safe_command(cmd, c.get("args").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default())),
            _ => None,
        })
        .or_else(|| row.and_then(|r| r.get("httpOrigin")).and_then(Value::as_str).map(safe_url));
    let count = |k: &str| row.and_then(|r| r.get(k)).and_then(Value::as_array).map_or(0, Vec::len);
    McpServer {
        name: name.to_owned(),
        scope: if row.and_then(|r| r.get("pluginId")).is_some_and(|p| !p.is_null()) { "plugin".to_owned() } else { "user".to_owned() },
        transport: transport.to_owned(),
        status,
        error,
        enabled,
        tools,
        resources: row.map(|_| u32::try_from(count("resources") + count("resourceTemplates")).unwrap_or(u32::MAX)),
        prompts: None,
        target,
        server: row.and_then(|r| r.get("serverInfo")).and_then(|i| Some(format!("{} {}", i.get("name")?.as_str()?, i.get("version").and_then(Value::as_str).unwrap_or("")).trim().to_owned())),
        auth: match auth_status {
            Some("oAuth") => Some("oauth".to_owned()),
            Some("bearerToken") => Some("bearer".to_owned()),
            Some("notLoggedIn") => Some("signed out".to_owned()),
            _ => None,
        },
    }
}

fn started_of(core: &Core, id: &str) -> i64 {
    match core.get(id).map(|e| &e.body) {
        Some(Body::Tool(t)) => t.started,
        _ => Core::now(),
    }
}

/// A collab call can finish while its child keeps working; only the child's state ends its task.
fn subagent(core: &mut Core, id: &str, tool: &str, desc: &str, status: &str, spawned: bool) {
    match core.state.tasks.iter_mut().find(|t| t.id == id) {
        Some(t) => {
            t.status = status.to_owned();
            if spawned {
                t.tool_id = Some(tool.to_owned());
                t.description = clip_middle(desc, 300).0;
            }
            if t.live() {
                t.ended = None;
            } else {
                t.ended.get_or_insert_with(Core::now);
            }
        }
        None => core.state.tasks.push(Task {
            id: id.to_owned(),
            tool_id: Some(tool.to_owned()),
            description: clip_middle(desc, 300).0,
            kind: "subagent".into(),
            class: Some("agent".into()),
            status: status.to_owned(),
            depth: 1,
            started: Core::now(),
            ended: (!matches!(status, "running" | "pending" | "paused")).then(Core::now),
            ..Task::default()
        }),
    }
    core.touch_state();
}

fn set_status(core: &mut Core, id: &str, status: ToolStatus) {
    if let Some(ev) = core.get_mut(id)
        && let Body::Tool(t) = &mut ev.body
        && matches!(t.status, ToolStatus::Running | ToolStatus::Pending)
    {
        t.status = status;
        if matches!(status, ToolStatus::Cancelled | ToolStatus::Denied) {
            t.ended = Some(Core::now());
        }
    }
}

/// `/usr/bin/bash -lc 'cmd'` (or sh/zsh/dash, -c/-lc) → `cmd`; anything else as it is.
fn unwrap_shell(full: &str) -> String {
    let mut parts = full.splitn(3, ' ');
    let (Some(sh), Some(flag), Some(rest)) = (parts.next(), parts.next(), parts.next()) else { return full.to_owned() };
    let shell = sh.rsplit('/').next().unwrap_or(sh);
    if !matches!(shell, "bash" | "sh" | "zsh" | "dash") || !matches!(flag, "-c" | "-lc") {
        return full.to_owned();
    }
    let rest = rest.trim();
    if let Some(inner) = rest.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')) {
        // Inside shell single quotes a ' is written '\''.
        return inner.replace("'\\''", "'");
    }
    if let Some(inner) = rest.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        return inner.replace("\\\"", "\"");
    }
    rest.to_owned()
}


/// A message as Codex input items: its text (with other files listed by path, for Codex to
/// read with its tools), then images by path.
fn input(text: &str, attachments: &[super::model::Attachment]) -> Value {
    let mut words = text.to_owned();
    let listed: Vec<String> = attachments.iter().filter(|a| !a.is_image()).filter_map(|a| a.path.as_ref().map(|p| format!("- {} ({}): {p}", a.name, a.mime))).collect();
    if !listed.is_empty() {
        words.push_str(&format!("\n\nAttached files:\n{}", listed.join("\n")));
    }
    let mut items = Vec::new();
    if !words.is_empty() {
        items.push(json!({"type": "text", "text": words}));
    }
    for a in attachments.iter().filter(|a| a.is_image()) {
        if let Some(p) = &a.path {
            items.push(json!({"type": "localImage", "path": p}));
        }
    }
    json!(items)
}

/// A command given as an array of words.
fn joined(v: Option<&Value>) -> String {
    v.and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")).unwrap_or_default()
}

/// Codex's file changes (path, kind, unified diff) as kiln diffs.
fn file_changes(v: Option<&Value>, cwd: &str) -> Vec<FileDiff> {
    let Some(list) = v.and_then(Value::as_array) else { return Vec::new() };
    list.iter()
        .filter_map(|c| {
            let path = super::claude::rel(c.get("path")?.as_str()?, cwd);
            let kind = c.get("kind").and_then(|k| k.get("type")).and_then(Value::as_str).unwrap_or("update");
            let diff = c.get("diff").and_then(Value::as_str).unwrap_or("");
            if kind == "add" && !diff.contains("@@") {
                return Some(super::diff::created(&path, diff));
            }
            Some(FileDiff { path, created: kind == "add", hunks: super::diff::parse_unified(diff) })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(c: &mut Codex, core: &mut Core, lines: &[Value]) {
        for l in lines {
            c.line(core, &l.to_string());
        }
    }

    fn written(core: &mut Core) -> Vec<Value> {
        core.take_writes().iter().filter_map(|w| serde_json::from_str(w).ok()).collect()
    }

    #[test]
    fn a_usage_limit_pauses_the_thread_and_it_goes_on_at_the_reset() -> anyhow::Result<()> {
        let mut core = Core::new();
        core.state.cwd = "/work".into();
        let mut c = Codex::new(None, None, None, false);
        c.start(&mut core);
        feed(&mut c, &mut core, &[json!({"id": 1, "result": {"userAgent": "codex_cli_rs/0.162.1"}})]);
        feed(&mut c, &mut core, &[json!({"id": 2, "result": {"thread": {"id": "th1", "cliVersion": "0.162.1", "turns": []}, "model": "gpt-5", "cwd": "/work"}})]);
        let _ = written(&mut core);
        c.command(&mut core, Command::Send { id: "m1".into(), text: "port the parser".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        let start = written(&mut core).into_iter().find(|v| v.get("method").and_then(Value::as_str) == Some("turn/start")).unwrap_or_default();
        feed(
            &mut c,
            &mut core,
            &[
                json!({"id": start.get("id").cloned().unwrap_or_default(), "result": {"turn": {"id": "t1", "status": "inProgress", "items": []}}}),
                json!({"method": "turn/started", "params": {"threadId": "th1", "turn": {"id": "t1", "status": "inProgress", "items": []}}}),
            ],
        );
        c.command(&mut core, Command::Send { id: "m2".into(), text: "then the tests".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        assert_eq!(core.state.queue.len(), 1);
        let said = "You've hit your usage limit. Upgrade to Pro or try again in 2 hours 13 minutes.";
        let before = Core::now();
        feed(&mut c, &mut core, &[json!({"method": "turn/completed", "params": {"threadId": "th1", "turn": {"id": "t1", "status": "failed", "items": [], "error": {"message": said, "codexErrorInfo": "usageLimitExceeded"}}}})]);
        // A failed turn is cut short (the pause, not keep-going, takes it from here).
        assert_eq!(core.state.last_turn.as_ref().map(|t| (t.how.as_str(), t.why.as_deref())), Some(("cut", Some("failed"))));
        let p = core.state.pause.clone().ok_or_else(|| anyhow::anyhow!("not paused"))?;
        let wait = 2 * 3_600_000 + 13 * 60_000;
        assert!(p.until.is_some_and(|u| u >= before + wait && u <= Core::now() + wait), "{p:?}");
        assert_eq!(p.cut.as_ref().map(|c| c.text.as_str()), Some("port the parser"));
        // Held: the queued message waits, and the limit is said once.
        assert_eq!(core.state.queue.len(), 1);
        assert!(!written(&mut core).iter().any(|v| v.get("method").and_then(Value::as_str) == Some("turn/start")));
        assert_eq!(core.events().filter(|e| matches!(e.body, Body::Notice { .. })).count(), 1);
        // At the reset kiln reads the rate limits; the window started over, so the thread goes on.
        let due = p.until.unwrap_or(0) + 10 * 60_000;
        assert_eq!(crate::harness::pause::step(&mut core, due, "th1"), crate::harness::pause::Step::Check);
        c.check_limits(&mut core);
        let read = written(&mut core).into_iter().find(|v| v.get("method").and_then(Value::as_str) == Some("account/rateLimits/read")).ok_or_else(|| anyhow::anyhow!("no check"))?;
        let resets = Core::now() / 1000 + 5 * 3_600;
        feed(&mut c, &mut core, &[json!({"id": read.get("id").cloned().unwrap_or_default(), "result": {"rateLimits": {"limitId": "codex", "primary": {"usedPercent": 4.0, "windowDurationMins": 300, "resetsAt": resets}}}})]);
        assert!(core.state.pause.is_none());
        let go_on = written(&mut core).into_iter().find(|v| v.get("method").and_then(Value::as_str) == Some("turn/start")).ok_or_else(|| anyhow::anyhow!("nothing sent"))?;
        assert_eq!(go_on.pointer("/params/input/0/text").and_then(Value::as_str), Some(crate::harness::pause::CONTINUE));
        assert_eq!(core.state.queue.len(), 1);
        Ok(())
    }

    #[test]
    fn a_thread_a_turn_an_approval_and_the_result() -> anyhow::Result<()> {
        let mut core = Core::new();
        core.state.cwd = "/work".into();
        let mut c = Codex::new(None, None, None, false);
        c.start(&mut core);
        let w = written(&mut core);
        assert_eq!(w.first().and_then(|v| v.get("method")).and_then(Value::as_str), Some("initialize"));
        // A message before the thread exists waits for it.
        c.command(&mut core, Command::Send { id: "m1".into(), text: "list files".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        assert!(core.take_writes().is_empty());
        feed(&mut c, &mut core, &[json!({"id": 1, "result": {"userAgent": "codex_cli_rs/0.162.1"}})]);
        let w = written(&mut core);
        let methods: Vec<&str> = w.iter().filter_map(|v| v.get("method").and_then(Value::as_str)).collect();
        assert_eq!(methods, vec!["initialized", "thread/start", "model/list", "skills/list", "hooks/list", "config/read", "account/read", "plugin/installed"]);
        assert_eq!(w.get(1).and_then(|v| v.pointer("/params/sandbox")).and_then(Value::as_str), Some("workspace-write"));
        feed(&mut c, &mut core, &[json!({"id": 2, "result": {"thread": {"id": "th1", "cliVersion": "0.162.1", "turns": []}, "model": "gpt-5", "cwd": "/work"}})]);
        let w = written(&mut core);
        let start = w.iter().find(|v| v.get("method").and_then(Value::as_str) == Some("turn/start")).cloned().unwrap_or_default();
        assert_eq!(start.pointer("/params/clientUserMessageId").and_then(Value::as_str), Some("m1"));
        assert_eq!(start.pointer("/params/input/0/text").and_then(Value::as_str), Some("list files"));
        assert_eq!(core.state.harness_session.as_deref(), Some("th1"));
        let turn_req = start.get("id").cloned().unwrap_or_default();
        feed(
            &mut c,
            &mut core,
            &[
                json!({"id": turn_req, "result": {"turn": {"id": "t1", "status": "inProgress", "items": []}}}),
                json!({"method": "turn/started", "params": {"threadId": "th1", "turn": {"id": "t1", "status": "inProgress", "items": []}}}),
                json!({"method": "item/started", "params": {"threadId": "th1", "turnId": "t1", "item": {"type": "userMessage", "id": "u1", "clientId": "m1", "content": [{"type": "text", "text": "list files"}]}}}),
                json!({"method": "item/started", "params": {"threadId": "th1", "turnId": "t1", "item": {"type": "commandExecution", "id": "c1", "command": "rm -rf build", "commandActions": [{"type": "unknown", "command": "rm -rf build"}], "cwd": "/work", "status": "inProgress"}}}),
                json!({"id": "srv-7", "method": "item/commandExecution/requestApproval", "params": {"threadId": "th1", "turnId": "t1", "itemId": "c1", "command": "rm -rf build", "cwd": "/work", "reason": "outside the sandbox", "proposedExecpolicyAmendment": ["rm", "-rf"], "startedAtMs": 1}}),
            ],
        );
        assert_eq!(core.state.status, Status::Waiting);
        let a = core.state.approvals.first().cloned().ok_or_else(|| anyhow::anyhow!("no approval"))?;
        assert_eq!((a.command.as_deref(), a.reason.as_deref()), (Some("rm -rf build"), Some("outside the sandbox")));
        assert_eq!(a.always.len(), 2);
        // A steer while the turn runs goes in as turn/steer.
        c.command(&mut core, Command::Send { id: "m2".into(), text: "only build/".into(), mode: SendMode::Steer, attachments: vec![], overrides: None });
        let w = written(&mut core);
        assert_eq!(w.first().and_then(|v| v.get("method")).and_then(Value::as_str), Some("turn/steer"));
        assert_eq!(w.first().and_then(|v| v.pointer("/params/expectedTurnId")).and_then(Value::as_str), Some("t1"));
        // Always allow with the exec-policy amendment.
        c.command(&mut core, Command::Approve { id: "srv-7".into(), decision: Decision::Always, message: None, always: Some(1), answers: None, choices: None });
        let w = written(&mut core);
        assert_eq!(w.first().and_then(|v| v.get("id")).and_then(Value::as_str), Some("srv-7"));
        assert!(w.first().and_then(|v| v.pointer("/result/decision/acceptWithExecpolicyAmendment")).is_some());
        feed(
            &mut c,
            &mut core,
            &[
                json!({"method": "item/commandExecution/outputDelta", "params": {"threadId": "th1", "turnId": "t1", "itemId": "c1", "delta": "removed\n"}}),
                json!({"method": "item/completed", "params": {"threadId": "th1", "turnId": "t1", "item": {"type": "commandExecution", "id": "c1", "command": "rm -rf build", "commandActions": [], "cwd": "/work", "status": "completed", "exitCode": 0, "aggregatedOutput": "removed\n", "durationMs": 12}}}),
                json!({"method": "item/agentMessage/delta", "params": {"threadId": "th1", "turnId": "t1", "itemId": "a1", "delta": "Removed "}}),
                json!({"method": "item/agentMessage/delta", "params": {"threadId": "th1", "turnId": "t1", "itemId": "a1", "delta": "build/."}}),
                json!({"method": "item/completed", "params": {"threadId": "th1", "turnId": "t1", "item": {"type": "agentMessage", "id": "a1", "text": "Removed build/."}}}),
                json!({"method": "turn/plan/updated", "params": {"threadId": "th1", "turnId": "t1", "plan": [{"step": "clean", "status": "completed"}, {"step": "rebuild", "status": "pending"}]}}),
                json!({"method": "thread/tokenUsage/updated", "params": {"threadId": "th1", "turnId": "t1", "tokenUsage": {"total": {"inputTokens": 100, "outputTokens": 20, "cachedInputTokens": 0, "reasoningOutputTokens": 0, "totalTokens": 120}, "last": {"inputTokens": 100, "outputTokens": 20, "cachedInputTokens": 0, "reasoningOutputTokens": 0, "totalTokens": 120}, "modelContextWindow": 1000}}}),
                json!({"method": "turn/completed", "params": {"threadId": "th1", "turn": {"id": "t1", "status": "completed", "items": []}}}),
            ],
        );
        let tool = core.get("c1").and_then(|e| match &e.body {
            Body::Tool(t) => Some(t.as_ref().clone()),
            _ => None,
        });
        assert_eq!(tool.map(|t| (t.status, t.exit_code, t.output)), Some((ToolStatus::Done, Some(0), Some("removed\n".to_owned()))));
        assert!(matches!(core.get("a1").map(|e| e.body.clone()), Some(Body::Assistant { text, streaming: false }) if text == "Removed build/."));
        assert_eq!(core.state.todos.len(), 2);
        assert_eq!(core.state.status, Status::Idle);
        assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Completed));
        assert_eq!(core.state.usage.context_window, Some(1000));
        // How the turn ended, for kiln's keep-going: done, with its command among its tools.
        assert_eq!(core.state.last_turn.as_ref().map(|t| (t.how.as_str(), t.why.as_deref(), t.tools)), Some(("done", Some("completed"), 1)));
        assert_eq!(core.state.summary, "Removed build/.");
        Ok(())
    }

    #[test]
    fn shell_wrappers_unwrap() {
        assert_eq!(unwrap_shell("/usr/bin/bash -lc 'mkdir -p x && echo ok'"), "mkdir -p x && echo ok");
        assert_eq!(unwrap_shell(r"bash -c 'echo '\''hi'\'''"), "echo 'hi'");
        assert_eq!(unwrap_shell(r#"/bin/zsh -lc "ls -la""#), "ls -la");
        assert_eq!(unwrap_shell("cargo test"), "cargo test");
        assert_eq!(unwrap_shell("python -c 'x'"), "python -c 'x'");
    }

    #[test]
    fn file_changes_questions_queue_and_send_now() -> anyhow::Result<()> {
        let mut core = Core::new();
        core.state.cwd = "/work".into();
        let mut c = Codex::new(Some("th9".into()), Some("gpt-5".into()), Some("read-only".into()), false);
        c.start(&mut core);
        core.take_writes();
        feed(&mut c, &mut core, &[json!({"id": 1, "result": {}})]);
        let w = written(&mut core);
        let resume = w.iter().find(|v| v.get("method").and_then(Value::as_str) == Some("thread/resume")).cloned().unwrap_or_default();
        assert_eq!(resume.pointer("/params/threadId").and_then(Value::as_str), Some("th9"));
        assert_eq!(resume.pointer("/params/sandbox").and_then(Value::as_str), Some("read-only"));
        // The resumed thread's history becomes the timeline.
        feed(&mut c, &mut core, &[json!({"id": 2, "result": {"thread": {"id": "th9", "turns": [{"id": "t0", "status": "completed", "items": [
            {"type": "userMessage", "id": "u0", "content": [{"type": "text", "text": "earlier"}]},
            {"type": "agentMessage", "id": "a0", "text": "earlier reply"}
        ]}]}, "model": "gpt-5"}})]);
        assert!(core.get("u0").is_some() && core.get("a0").is_some());
        core.take_writes();
        c.command(&mut core, Command::Send { id: "m1".into(), text: "edit it".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        let w = written(&mut core);
        assert_eq!(w.first().and_then(|v| v.pointer("/params/sandboxPolicy/type")).and_then(Value::as_str), Some("readOnly"));
        let rid = w.first().and_then(|v| v.get("id")).cloned().unwrap_or_default();
        feed(
            &mut c,
            &mut core,
            &[
                json!({"id": rid, "result": {"turn": {"id": "t1", "status": "inProgress", "items": []}}}),
                json!({"method": "item/started", "params": {"item": {"type": "fileChange", "id": "f1", "status": "inProgress", "changes": [{"path": "/work/a.rs", "kind": {"type": "update"}, "diff": "@@ -1,1 +1,1 @@\n-old\n+new\n"}]}}}),
                json!({"id": 44, "method": "item/fileChange/requestApproval", "params": {"threadId": "th9", "turnId": "t1", "itemId": "f1", "startedAtMs": 1}}),
            ],
        );
        let a = core.state.approvals.first().cloned().ok_or_else(|| anyhow::anyhow!("no approval"))?;
        assert_eq!(a.title, "a.rs");
        assert_eq!(a.diffs.first().and_then(|d| d.hunks.first()).map(|h| h.lines.clone()), Some(vec!["-old".to_owned(), "+new".to_owned()]));
        // Queue while busy; deny with feedback declines and steers the feedback in.
        c.command(&mut core, Command::Send { id: "m2".into(), text: "then test".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        assert_eq!(core.state.queue.len(), 1);
        c.command(&mut core, Command::Approve { id: "44".into(), decision: Decision::Deny, message: Some("keep old".into()), always: None, answers: None, choices: None });
        let w = written(&mut core);
        assert_eq!(w.first().and_then(|v| v.pointer("/result/decision")).and_then(Value::as_str), Some("decline"));
        assert!(w.iter().any(|v| v.get("method").and_then(Value::as_str) == Some("turn/steer") && v.pointer("/params/input/0/text").and_then(Value::as_str) == Some("keep old")));
        // Send now: interrupt, and the message starts the next turn.
        c.command(&mut core, Command::Send { id: "m3".into(), text: "stop and do X".into(), mode: SendMode::Now, attachments: vec![], overrides: None });
        let w = written(&mut core);
        assert_eq!(w.first().and_then(|v| v.get("method")).and_then(Value::as_str), Some("turn/interrupt"));
        feed(&mut c, &mut core, &[json!({"method": "turn/completed", "params": {"turn": {"id": "t1", "status": "interrupted", "items": []}}})]);
        let w = written(&mut core);
        let next = w.iter().find(|v| v.get("method").and_then(Value::as_str) == Some("turn/start")).cloned().unwrap_or_default();
        assert_eq!(next.pointer("/params/clientUserMessageId").and_then(Value::as_str), Some("m3"));
        // A question.
        feed(&mut c, &mut core, &[json!({"id": "q1", "method": "item/tool/requestUserInput", "params": {"threadId": "th9", "turnId": "t2", "itemId": "i9", "isBlocking": true, "questions": [{"id": "db", "header": "DB", "question": "Which database?", "options": [{"label": "Postgres", "description": ""}, {"label": "SQLite", "description": ""}]}]}})]);
        let mut answers = std::collections::BTreeMap::new();
        answers.insert("Which database?".to_owned(), "SQLite".to_owned());
        c.command(&mut core, Command::Approve { id: "q1".into(), decision: Decision::Allow, message: None, always: None, answers: Some(answers), choices: None });
        let w = written(&mut core);
        assert_eq!(w.first().and_then(|v| v.pointer("/result/answers/db/answers/0")).and_then(Value::as_str), Some("SQLite"));
        Ok(())
    }

    #[test]
    fn an_mcp_servers_request_for_input_is_asked_and_answered() -> anyhow::Result<()> {
        let mut c = Codex::from_spec(&super::super::session::Spec { kind: "codex".into(), ..Default::default() });
        let mut core = Core::new();
        let schema = json!({"type": "object", "properties": {"team": {"type": "string", "enum": ["eng", "ops"], "enumNames": ["Engineering", "Operations"]}}, "required": ["team"]});
        feed(&mut c, &mut core, &[json!({"id": "el1", "method": "mcpServer/elicitation/request", "params": {"threadId": "th1", "turnId": "t1", "serverName": "linear", "mode": "form", "message": "Pick a team", "requestedSchema": schema}})]);
        let a = core.state.approvals.first().cloned().ok_or_else(|| anyhow::anyhow!("no approval"))?;
        assert_eq!((a.title.as_str(), a.tool, a.questions.first().map(|q| q.options.len())), ("linear asks: Pick a team", ToolKind::Question, Some(2)));
        let choices = vec![ChoiceAnswer { id: Some("team".into()), selected: vec!["Operations".into()], ..ChoiceAnswer::default() }];
        c.command(&mut core, Command::Approve { id: "el1".into(), decision: Decision::Allow, message: None, always: None, answers: None, choices: Some(choices) });
        assert_eq!(written(&mut core).first().and_then(|v| v.get("result")).cloned(), Some(json!({"action": "accept", "content": {"team": "ops"}})));
        // A page to open, declined.
        feed(&mut c, &mut core, &[json!({"id": "el2", "method": "mcpServer/elicitation/request", "params": {"serverName": "gh", "mode": "url", "message": "Sign in", "url": "https://github.com/login/device", "elicitationId": "e2"}})]);
        let a = core.state.approvals.first().cloned().ok_or_else(|| anyhow::anyhow!("no approval"))?;
        assert!(a.detail.as_deref().is_some_and(|d| d.contains("https://github.com/login/device")));
        c.command(&mut core, Command::Approve { id: "el2".into(), decision: Decision::Deny, message: None, always: None, answers: None, choices: None });
        assert_eq!(written(&mut core).first().and_then(|v| v.get("result")).cloned(), Some(json!({"action": "decline"})));
        Ok(())
    }

    /// The response to the request kiln wrote for `method` (the latest one).
    fn respond(c: &mut Codex, core: &mut Core, w: &[Value], method: &str, result: Value) {
        let id = w.iter().rev().find(|v| v.get("method").and_then(Value::as_str) == Some(method)).and_then(|v| v.get("id")).cloned().unwrap_or_default();
        feed(c, core, &[json!({"id": id, "result": result})]);
    }

    fn replies(core: &mut Core) -> Vec<(String, Value)> {
        core.take_out()
            .into_iter()
            .filter_map(|u| match u {
                super::super::core::Update::Reply { client, value } => Some((client, value)),
                _ => None,
            })
            .collect()
    }

    /// A Codex session with its thread open.
    fn opened() -> (Codex, Core, Vec<Value>) {
        let mut core = Core::new();
        core.state.cwd = "/work".into();
        let mut c = Codex::new(None, None, None, false);
        c.start(&mut core);
        let w = written(&mut core);
        respond(&mut c, &mut core, &w, "initialize", json!({"userAgent": "kiln/0.162.1 (Linux)"}));
        let w = written(&mut core);
        respond(&mut c, &mut core, &w, "thread/start", json!({"thread": {"id": "th1", "cliVersion": "0.162.1", "turns": []}, "model": "gpt-5", "modelProvider": "openai", "cwd": "/work"}));
        let mut all = w;
        all.extend(written(&mut core));
        (c, core, all)
    }

    #[test]
    fn account_plan_limits_and_status() -> anyhow::Result<()> {
        let (mut c, mut core, w) = opened();
        respond(&mut c, &mut core, &w, "account/read", json!({"account": {"type": "chatgpt", "email": "someone@example.com", "planType": "pro"}, "requiresOpenaiAuth": true}));
        assert_eq!(core.state.plan.as_deref(), Some("Pro"));
        let account = core.state.account.clone().unwrap_or_default();
        assert!(account.starts_with("codex:pro:") && !account.contains("someone"), "{account}");
        let w = written(&mut core);
        assert!(w.iter().any(|v| v.get("method").and_then(Value::as_str) == Some("account/rateLimits/read")));
        respond(
            &mut c,
            &mut core,
            &w,
            "account/rateLimits/read",
            json!({"rateLimits": {"limitId": "codex", "primary": {"usedPercent": 37, "windowDurationMins": 300, "resetsAt": 1_792_220_725}, "secondary": {"usedPercent": 12, "windowDurationMins": 10080, "resetsAt": 1_792_807_525}, "credits": {"hasCredits": true, "unlimited": false, "balance": "120"}, "planType": "pro"},
                "rateLimitsByLimitId": {"codex": {"limitId": "codex", "primary": {"usedPercent": 37, "windowDurationMins": 300, "resetsAt": 1_792_220_725}, "secondary": {"usedPercent": 12, "windowDurationMins": 10080, "resetsAt": 1_792_807_525}, "credits": {"hasCredits": true, "unlimited": false, "balance": "120"}},
                    "codex_spark": {"limitId": "codex_spark", "limitName": "Spark", "primary": {"usedPercent": 5, "windowDurationMins": 300}, "secondary": null}}}),
        );
        let l: Vec<(&str, &str, Option<f64>, Option<&str>)> = core.state.limits.iter().map(|l| (l.id.as_str(), l.label.as_str(), l.used_percent, l.window.as_deref())).collect();
        assert_eq!(
            l,
            vec![
                ("codex.primary", "Session", Some(37.0), Some("5h")),
                ("codex.secondary", "Weekly", Some(12.0), Some("7d")),
                ("codex_spark.primary", "Session · Spark", Some(5.0), Some("5h"))
            ]
        );
        assert_eq!(core.state.limits.first().and_then(|l| l.resets_at), Some(1_792_220_725_000));
        assert!(core.state.limits.iter().all(|l| l.account.as_deref() == Some(account.as_str())));
        // A rolling update is sparse: what it leaves out stays.
        feed(&mut c, &mut core, &[json!({"method": "account/rateLimits/updated", "params": {"rateLimits": {"limitId": "codex", "primary": {"usedPercent": 40, "windowDurationMins": 300, "resetsAt": 1_792_220_725}, "secondary": null, "credits": null, "planType": null}}})]);
        let used: Vec<Option<f64>> = core.state.limits.iter().take(2).map(|l| l.used_percent).collect();
        assert_eq!(used, vec![Some(40.0), Some(12.0)]);
        let st = &core.state.harness_status;
        assert!(st.iter().any(|r| r.label == "Email" && r.value == "s…@example.com"));
        assert!(st.iter().any(|r| r.label == "Credits" && r.value == "120"));
        assert!(st.iter().any(|r| r.label == "Model" && r.value == "gpt-5"));
        assert!(!serde_json::to_string(&core.state).unwrap_or_default().contains("someone@"));
        Ok(())
    }

    #[test]
    fn a_custom_provider_has_no_account_and_rate_limit_pushes_still_count() -> anyhow::Result<()> {
        let (mut c, mut core, w) = opened();
        respond(&mut c, &mut core, &w, "account/read", json!({"account": null, "requiresOpenaiAuth": false}));
        assert_eq!((core.state.plan.as_deref(), core.state.account.as_deref()), (None, None));
        assert!(!written(&mut core).iter().any(|v| v.get("method").and_then(Value::as_str) == Some("account/rateLimits/read")));
        feed(&mut c, &mut core, &[json!({"method": "account/rateLimits/updated", "params": {"rateLimits": {"limitId": "codex", "limitName": null, "primary": {"usedPercent": 37, "windowDurationMins": 300, "resetsAt": 1_792_220_725}, "secondary": {"usedPercent": 12, "windowDurationMins": 10080, "resetsAt": 1_792_807_525}, "credits": null}}})]);
        assert_eq!(core.state.limits.len(), 2);
        Ok(())
    }

    #[test]
    fn mcp_servers_from_status_startup_and_configuration_and_their_commands() -> anyhow::Result<()> {
        let (mut c, mut core, w) = opened();
        assert!(w.iter().any(|v| v.get("method").and_then(Value::as_str) == Some("mcpServerStatus/list") && v.pointer("/params/threadId").and_then(Value::as_str) == Some("th1")));
        respond(
            &mut c,
            &mut core,
            &w,
            "config/read",
            json!({"config": {"mcp_servers": {"echo": {"command": "python3", "args": ["srv.py", "--token", "abc"], "env": {"SECRET": "x"}}, "web": {"url": "https://u:p@mcp.example.com/mcp?k=1", "enabled": false}}}, "origins": {}}),
        );
        respond(
            &mut c,
            &mut core,
            &w,
            "mcpServerStatus/list",
            json!({"data": [{"name": "echo", "authStatus": "unsupported", "tools": {"echo": {"name": "echo", "description": "Echo it", "inputSchema": {}}, "shout": {"name": "shout", "inputSchema": {}}}, "resources": [{"name": "r", "uri": "x://r"}], "resourceTemplates": [], "runtimeStatus": "connected", "serverInfo": {"name": "kiln-echo", "version": "1.0.0"}}], "nextCursor": null}),
        );
        let m = core.state.mcp.clone();
        let echo = m.iter().find(|s| s.name == "echo").cloned().ok_or_else(|| anyhow::anyhow!("missing"))?;
        assert_eq!((echo.status, echo.transport.as_str(), echo.tools.len(), echo.resources, echo.server.as_deref()), (McpStatus::Connected, "stdio", 2, Some(1), Some("kiln-echo 1.0.0")));
        assert_eq!(echo.target.as_deref(), Some("python3 srv.py --token …"));
        let web = m.iter().find(|s| s.name == "web").cloned().ok_or_else(|| anyhow::anyhow!("missing"))?;
        assert_eq!((web.status, web.enabled, web.target.as_deref()), (McpStatus::Disabled, false, Some("https://mcp.example.com/mcp")));
        assert!(!serde_json::to_string(&core.state).unwrap_or_default().contains("SECRET"));
        // Start-up pushes.
        feed(&mut c, &mut core, &[json!({"method": "mcpServer/startupStatus/updated", "params": {"name": "web", "status": "failed", "error": "401", "failureReason": "reauthenticationRequired"}})]);
        // (still off in the configuration)
        assert_eq!(core.state.mcp.iter().find(|s| s.name == "web").map(|s| s.status), Some(McpStatus::Disabled));
        // Switching one on: written to the configuration, then the servers are reloaded and listed.
        c.command(&mut core, Command::McpEnable { name: "web".into(), enabled: true });
        let w = written(&mut core);
        let wr = w.iter().find(|v| v.get("method").and_then(Value::as_str) == Some("config/value/write")).cloned().unwrap_or_default();
        assert_eq!((wr.pointer("/params/keyPath").and_then(Value::as_str), wr.pointer("/params/value")), (Some("mcp_servers.web.enabled"), Some(&json!(true))));
        respond(&mut c, &mut core, &w, "config/value/write", json!({"status": "ok"}));
        let w = written(&mut core);
        assert!(w.iter().any(|v| v.get("method").and_then(Value::as_str) == Some("config/mcpServer/reload")));
        respond(&mut c, &mut core, &w, "config/mcpServer/reload", json!({}));
        let methods: Vec<String> = written(&mut core).iter().filter_map(|v| v.get("method").and_then(Value::as_str).map(str::to_owned)).collect();
        assert!(methods.contains(&"config/read".to_owned()) && methods.contains(&"mcpServerStatus/list".to_owned()), "{methods:?}");
        // Signing in: the page to open, then the redirect is fetched on Codex's machine.
        c.command(&mut core, Command::McpAuth { name: "web".into(), client: "c1".into(), redirect: None });
        let w = written(&mut core);
        assert!(w.iter().any(|v| v.get("method").and_then(Value::as_str) == Some("mcpServer/oauth/login") && v.pointer("/params/threadId").and_then(Value::as_str) == Some("th1")));
        respond(&mut c, &mut core, &w, "mcpServer/oauth/login", json!({"authorizationUrl": "https://auth.example.com/a?x=1", "loginId": "l1"}));
        assert!(replies(&mut core).iter().any(|(c, v)| c == "c1" && v.get("url").and_then(Value::as_str) == Some("https://auth.example.com/a?x=1")));
        c.command(&mut core, Command::McpAuth { name: "web".into(), client: "c2".into(), redirect: Some(" http://127.0.0.1:5555/callback?code=x ".into()) });
        let Some(Effect::Fetch { job, url }) = core.effects.pop() else { anyhow::bail!("no fetch") };
        assert_eq!(url, "http://127.0.0.1:5555/callback?code=x");
        c.run_done(&mut core, &job, Some(200), "ok");
        assert!(replies(&mut core).iter().any(|(c, v)| c == "c2" && v.get("ok") == Some(&json!(true))));
        feed(&mut c, &mut core, &[json!({"method": "mcpServer/oauthLogin/completed", "params": {"name": "web", "success": true}})]);
        assert!(written(&mut core).iter().any(|v| v.get("method").and_then(Value::as_str) == Some("config/mcpServer/reload")));
        // Adding: the CLI, then its headers into the configuration, then a reload.
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("X-Team".to_owned(), "a".to_owned());
        c.command(
            &mut core,
            Command::McpAdd { scope: String::new(), name: "docs".into(), transport: String::new(), command: None, args: vec![], env: Default::default(), url: Some("https://docs.example.com/mcp".into()), headers, client: Some("c3".into()) },
        );
        // (written to the configuration: the CLI would start signing in on its machine)
        assert!(core.effects.is_empty());
        let w = written(&mut core);
        let wr = w.iter().find(|v| v.get("method").and_then(Value::as_str) == Some("config/value/write")).cloned().unwrap_or_default();
        assert_eq!(
            (wr.pointer("/params/keyPath").and_then(Value::as_str), wr.pointer("/params/value/url"), wr.pointer("/params/value/http_headers/X-Team")),
            (Some("mcp_servers.docs"), Some(&json!("https://docs.example.com/mcp")), Some(&json!("a")))
        );
        respond(&mut c, &mut core, &w, "config/value/write", json!({"status": "ok"}));
        assert!(replies(&mut core).iter().any(|(c, v)| c == "c3" && v.get("ok") == Some(&json!(true))));
        assert!(written(&mut core).iter().any(|v| v.get("method").and_then(Value::as_str) == Some("config/mcpServer/reload")));
        c.command(&mut core, Command::McpAdd { scope: String::new(), name: "echo".into(), transport: "stdio".into(), command: Some("x".into()), args: vec![], env: Default::default(), url: None, headers: Default::default(), client: Some("c6".into()) });
        assert!(replies(&mut core).iter().any(|(c, v)| c == "c6" && v.get("ok") == Some(&json!(false))));
        let mut env = std::collections::BTreeMap::new();
        env.insert("K".to_owned(), "v".to_owned());
        c.command(&mut core, Command::McpAdd { scope: String::new(), name: "local".into(), transport: "stdio".into(), command: Some("node".into()), args: vec!["s.js".into()], env, url: None, headers: Default::default(), client: None });
        assert!(matches!(core.effects.pop(), Some(Effect::Run { args, .. }) if args == ["mcp", "add", "local", "--env", "K=v", "--", "node", "s.js"]));
        c.command(&mut core, Command::McpAdd { scope: String::new(), name: "old".into(), transport: "sse".into(), command: None, args: vec![], env: Default::default(), url: Some("https://x.example.com/sse".into()), headers: Default::default(), client: Some("c5".into()) });
        assert!(core.effects.is_empty());
        assert!(replies(&mut core).iter().any(|(c, v)| c == "c5" && v.get("ok") == Some(&json!(false))));
        c.command(&mut core, Command::McpRemove { scope: None, name: "echo".into(), client: None });
        assert!(matches!(core.effects.pop(), Some(Effect::Run { args, .. }) if args == ["mcp", "remove", "--", "echo"]));
        // An http server: its sign-in is forgotten, then it is removed.
        c.command(&mut core, Command::McpRemove { scope: None, name: "web".into(), client: Some("c7".into()) });
        let Some(Effect::Run { job, args }) = core.effects.pop() else { anyhow::bail!("no logout") };
        assert_eq!(args, ["mcp", "logout", "--", "web"]);
        c.run_done(&mut core, &job, Some(1), "not signed in");
        assert!(matches!(core.effects.pop(), Some(Effect::Run { args, .. }) if args == ["mcp", "remove", "--", "web"]));
        Ok(())
    }

    #[test]
    fn a_resumed_thread_loads_its_latest_turns() -> anyhow::Result<()> {
        let spec = super::super::session::Spec { kind: "codex".into(), resume: Some("th9".into()), ..Default::default() };
        let mut c = Codex::from_spec(&spec);
        let mut core = Core::new();
        core.state.cwd = "/work".into();
        c.start(&mut core);
        let w = written(&mut core);
        respond(&mut c, &mut core, &w, "initialize", json!({"userAgent": "kiln/0.162.1 (Linux)"}));
        let w = written(&mut core);
        // Resumed without its history inline (Codex deprecates that for paginated threads).
        let resume = w.iter().find(|v| v.get("method") == Some(&json!("thread/resume"))).cloned().unwrap_or_default();
        assert_eq!(resume.pointer("/params/excludeTurns"), Some(&json!(true)));
        respond(&mut c, &mut core, &w, "thread/resume", json!({"thread": {"id": "th9", "turns": []}, "model": "gpt-5", "cwd": "/work"}));
        // Not ready until its history is in.
        assert_eq!(core.state.status, Status::Starting);
        let w = written(&mut core);
        let list = w.iter().find(|v| v.get("method") == Some(&json!("thread/turns/list"))).cloned().unwrap_or_default();
        assert_eq!(list.get("params"), Some(&json!({"threadId": "th9", "itemsView": "full", "sortDirection": "desc", "limit": 100})));
        let turn = |t: &str, said: &str, reply: &str| json!({"id": t, "status": "completed", "items": [
            {"type": "userMessage", "id": format!("u-{t}"), "content": [{"type": "text", "text": said}]},
            {"type": "agentMessage", "id": format!("a-{t}"), "text": reply}
        ]});
        // Newest first, and more before them.
        respond(&mut c, &mut core, &w, "thread/turns/list", json!({"data": [turn("t2", "second", "two"), turn("t1", "first", "one")], "nextCursor": "c1"}));
        let said: Vec<String> = core
            .events()
            .filter_map(|e| match &e.body {
                Body::User { text, .. } | Body::Assistant { text, .. } => Some(text.clone()),
                Body::Notice { text, .. } => Some(format!("notice: {text}")),
                _ => None,
            })
            .collect();
        assert_eq!(said, vec!["notice: Earlier turns are not shown: the last 100.", "first", "one", "second", "two"]);
        assert_eq!(core.state.status, Status::Idle);
        // A rewind knows the listed turns.
        assert_eq!(c.turns.get("u-t1").map(String::as_str), Some("t1"));
        // A Codex that cannot list them: the session goes on without.
        let mut c = Codex::from_spec(&spec);
        let mut core = Core::new();
        c.start(&mut core);
        let w = written(&mut core);
        respond(&mut c, &mut core, &w, "initialize", json!({"userAgent": "kiln/0.162.1 (Linux)"}));
        let w = written(&mut core);
        respond(&mut c, &mut core, &w, "thread/resume", json!({"thread": {"id": "th9", "turns": []}, "model": "gpt-5", "cwd": "/work"}));
        let w = written(&mut core);
        let id = w.iter().find(|v| v.get("method") == Some(&json!("thread/turns/list"))).and_then(|v| v.get("id")).cloned().unwrap_or_default();
        feed(&mut c, &mut core, &[json!({"id": id, "error": {"code": -32601, "message": "unknown method"}})]);
        assert_eq!(core.state.status, Status::Idle);
        Ok(())
    }

    #[test]
    fn a_queued_message_is_steered_into_the_running_turn() -> anyhow::Result<()> {
        let (mut c, mut core, _) = opened();
        assert!(core.meta.features.iter().any(|f| f == "queue_steer"));
        let send = |c: &mut Codex, core: &mut Core, id: &str, text: &str| c.command(core, Command::Send { id: id.into(), text: text.into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        // A turn is starting (no id yet); a message waits behind it.
        send(&mut c, &mut core, "m1", "build it");
        let w = written(&mut core);
        send(&mut c, &mut core, "m2", "tests too, please");
        assert_eq!(core.state.queue.len(), 1);
        // Steered before the turn has an id: it goes in once it has, without interrupting.
        c.command(&mut core, Command::QueueSteer { id: "m2".into() });
        assert!(written(&mut core).is_empty() && core.state.queue.len() == 1);
        respond(&mut c, &mut core, &w, "turn/start", json!({"turn": {"id": "t1", "status": "inProgress", "items": []}}));
        let w = written(&mut core);
        let steer = w.iter().find(|v| v.get("method") == Some(&json!("turn/steer"))).cloned().unwrap_or_default();
        assert_eq!((steer.pointer("/params/expectedTurnId"), steer.pointer("/params/input/0/text")), (Some(&json!("t1")), Some(&json!("tests too, please"))));
        assert!(core.state.queue.is_empty() && !w.iter().any(|v| v.get("method") == Some(&json!("turn/interrupt"))));
        // Another, steered at once, but the turn ends before Codex takes it: it starts the next.
        send(&mut c, &mut core, "m3", "and docs, too");
        c.command(&mut core, Command::QueueSteer { id: "m3".into() });
        let w = written(&mut core);
        let sid = w.iter().find(|v| v.get("method") == Some(&json!("turn/steer"))).and_then(|v| v.get("id")).cloned().unwrap_or_default();
        feed(&mut c, &mut core, &[json!({"method": "turn/completed", "params": {"threadId": "th1", "turn": {"id": "t1", "status": "completed", "items": []}}})]);
        feed(&mut c, &mut core, &[json!({"id": sid, "error": {"code": -32600, "message": "no active turn to steer"}})]);
        let w = written(&mut core);
        let start = w.iter().find(|v| v.get("method") == Some(&json!("turn/start"))).cloned().unwrap_or_default();
        assert_eq!(start.pointer("/params/input/0/text"), Some(&json!("and docs, too")));
        assert_ne!(core.state.delivery.get("m3"), Some(&Delivery::Cancelled));
        Ok(())
    }

    #[test]
    fn structured_answers_keep_commas_free_text_secrets_and_notes() -> anyhow::Result<()> {
        use super::super::model::ChoiceAnswer;
        let (mut c, mut core, _) = opened();
        feed(&mut c, &mut core, &[json!({"id": "q7", "method": "item/tool/requestUserInput", "params": {"threadId": "th1", "turnId": "t1", "itemId": "i7", "isBlocking": true, "questions": [
            {"id": "db", "header": "DB", "question": "Which database?", "isOther": true, "options": [{"label": "Postgres, with replicas", "description": ""}, {"label": "SQLite", "description": ""}]},
            {"id": "token", "header": "Token", "question": "API token?", "isOther": true, "isSecret": true, "options": null},
            {"id": "name", "header": "Name", "question": "Name it?", "isOther": true, "options": []}
        ]}})]);
        let a = core.state.approvals.first().cloned().ok_or_else(|| anyhow::anyhow!("no approval"))?;
        let qs: Vec<(&str, bool, bool)> = a.questions.iter().map(|q| (q.id.as_str(), q.other, q.secret)).collect();
        assert_eq!(qs, vec![("db", true, false), ("token", true, true), ("name", true, false)]);
        let choices = vec![
            ChoiceAnswer { question: "Which database?".into(), id: Some("db".into()), selected: vec!["Postgres, with replicas".into()], note: Some("keep it cheap".into()), ..ChoiceAnswer::default() },
            ChoiceAnswer { question: "API token?".into(), id: Some("token".into()), other: Some("sk-123".into()), ..ChoiceAnswer::default() },
            // Matched by its text when no id comes with it.
            ChoiceAnswer { question: "Name it?".into(), other: Some("alpha, beta".into()), ..ChoiceAnswer::default() },
        ];
        c.command(&mut core, Command::Approve { id: "q7".into(), decision: Decision::Allow, message: None, always: None, answers: None, choices: Some(choices) });
        let w = written(&mut core);
        assert_eq!(
            w.first().and_then(|v| v.pointer("/result/answers")),
            Some(&json!({"db": {"answers": ["Postgres, with replicas", "user_note: keep it cheap"]}, "token": {"answers": ["sk-123"]}, "name": {"answers": ["alpha, beta"]}}))
        );
        // The older map: its text is one answer, commas and all.
        feed(&mut c, &mut core, &[json!({"id": "q8", "method": "item/tool/requestUserInput", "params": {"threadId": "th1", "turnId": "t1", "itemId": "i8", "isBlocking": true, "questions": [{"id": "db", "header": "DB", "question": "Which database?", "options": [{"label": "Postgres, with replicas", "description": ""}]}]}})]);
        let mut answers = std::collections::BTreeMap::new();
        answers.insert("Which database?".to_owned(), "Postgres, with replicas".to_owned());
        c.command(&mut core, Command::Approve { id: "q8".into(), decision: Decision::Allow, message: None, always: None, answers: Some(answers), choices: None });
        assert_eq!(written(&mut core).first().and_then(|v| v.pointer("/result/answers/db/answers")), Some(&json!(["Postgres, with replicas"])));
        Ok(())
    }

    #[test]
    fn a_fork_codex_is_not_ready_for_is_tried_again() -> anyhow::Result<()> {
        let mut core = Core::new();
        core.state.cwd = "/work".into();
        let spec = super::super::session::Spec { kind: "codex".into(), resume: Some("th1".into()), fork: true, ..Default::default() };
        let mut c = Codex::from_spec(&spec);
        c.start(&mut core);
        let w = written(&mut core);
        respond(&mut c, &mut core, &w, "initialize", json!({"userAgent": "kiln/0.162.1 (Linux)"}));
        let w = written(&mut core);
        let fork = w.iter().find(|v| v.get("method") == Some(&json!("thread/fork"))).cloned().unwrap_or_default();
        let id = fork.get("id").cloned().unwrap_or_default();
        feed(&mut c, &mut core, &[json!({"id": id, "error": {"code": -32603, "message": "failed to prepare paginated fork: thread-store internal error: thread history projection for th1 is behind durable rollout"}})]);
        assert!(core.state.doing.as_deref() == Some(CATCHING_UP) && core.state.status == Status::Starting);
        assert!(!core.events().any(|e| matches!(&e.body, Body::Notice { .. })));
        c.tick(&mut core);
        let w = written(&mut core);
        let again = w.iter().find(|v| v.get("method") == Some(&json!("thread/fork"))).cloned().unwrap_or_default();
        assert_eq!(again.get("params"), fork.get("params"));
        respond(&mut c, &mut core, &w, "thread/fork", json!({"thread": {"id": "th2", "turns": []}, "model": "gpt-5", "cwd": "/work"}));
        assert!(core.state.doing.is_none() && core.state.harness_session.as_deref() == Some("th2"));
        // A resume the daemon still holds for a terminal waits too.
        let mut core = Core::new();
        let held = super::super::session::Spec { kind: "codex".into(), resume: Some("th1".into()), ..Default::default() };
        let mut c = Codex::from_spec(&held);
        c.start(&mut core);
        let w = written(&mut core);
        respond(&mut c, &mut core, &w, "initialize", json!({"userAgent": "kiln/0.162.1 (Linux)"}));
        let w = written(&mut core);
        let id = w.iter().find(|v| v.get("method") == Some(&json!("thread/resume"))).and_then(|v| v.get("id")).cloned().unwrap_or_default();
        feed(&mut c, &mut core, &[json!({"id": id, "error": {"code": -32600, "message": "thread th1 already has an active writer"}})]);
        assert_eq!(core.state.doing.as_deref(), Some(LETTING_GO));
        c.tick(&mut core);
        assert!(written(&mut core).iter().any(|v| v.get("method") == Some(&json!("thread/resume"))));
        // Other refusals are said at once.
        let mut core = Core::new();
        let mut c = Codex::from_spec(&spec);
        c.start(&mut core);
        let w = written(&mut core);
        respond(&mut c, &mut core, &w, "initialize", json!({"userAgent": "kiln/0.162.1 (Linux)"}));
        let w = written(&mut core);
        let id = w.iter().find(|v| v.get("method") == Some(&json!("thread/fork"))).and_then(|v| v.get("id")).cloned().unwrap_or_default();
        feed(&mut c, &mut core, &[json!({"id": id, "error": {"code": -32600, "message": "no such thread"}})]);
        assert!(core.events().any(|e| matches!(&e.body, Body::Notice { text, .. } if text.contains("no such thread"))) && core.state.status == Status::Idle);
        Ok(())
    }

    #[test]
    fn the_command_catalog_and_what_its_controls_answer() -> anyhow::Result<()> {
        let mut core = Core::new();
        core.state.cwd = "/work".into();
        let mut c = Codex::new(None, None, None, false);
        c.start(&mut core);
        // Codex's commands from the start, each with a route; names unique.
        let cat = core.meta.commands.clone();
        let route = |n: &str| cat.iter().find(|e| e.name == n).map(|e| e.route.clone());
        assert!(matches!(route("model"), Some(Route::View { view }) if view == "model"));
        assert!(matches!(route("ps"), Some(Route::View { view }) if view == "tasks"));
        assert!(matches!(route("init"), Some(Route::Prompt { text: Some(t), .. }) if t.contains("AGENTS.md")));
        assert!(matches!(route("logout"), Some(Route::View { view }) if view == "logout"));
        assert!(matches!(route("login"), Some(Route::View { view }) if view == "login"));
        assert!(matches!(route("goal"), Some(Route::Control { command }) if command.pointer("/value/client") == Some(&json!("{client}"))));
        let mut names: Vec<&str> = cat.iter().map(|e| e.name.as_str()).collect();
        names.dedup();
        assert_eq!(names.len(), cat.len());
        // Answered before the thread is open: the export and the folder; a goal says it must wait.
        c.command(&mut core, Command::Control { id: "pwd".into(), value: Some(json!({"client": "k1"})) });
        c.command(&mut core, Command::Control { id: "goal".into(), value: Some(json!({"client": "k2"})) });
        let r = replies(&mut core);
        assert!(r.iter().any(|(cl, v)| cl == "k1" && v.get("text") == Some(&json!("/work"))), "{r:?}");
        assert!(r.iter().any(|(cl, v)| cl == "k2" && v.get("error").is_some()), "{r:?}");
        let (mut c, mut core, _) = opened();
        // /goal with words sets it; without, shows it.
        c.command(&mut core, Command::Control { id: "goal".into(), value: Some(json!({"text": " ship it ", "client": "k3"})) });
        let w = written(&mut core);
        let set = w.iter().find(|v| v.get("method") == Some(&json!("thread/goal/set"))).cloned().unwrap_or_default();
        assert_eq!(set.get("params"), Some(&json!({"threadId": "th1", "objective": "ship it", "origin": "user"})));
        respond(&mut c, &mut core, &w, "thread/goal/set", json!({"goal": {"objective": "ship it", "status": "active", "tokensUsed": 12}}));
        let r = replies(&mut core);
        assert!(r.iter().any(|(cl, v)| cl == "k3" && v.get("title") == Some(&json!("Goal")) && v.get("text").and_then(Value::as_str).is_some_and(|t| t.contains("ship it") && t.contains("12 tokens"))), "{r:?}");
        c.command(&mut core, Command::Control { id: "goal".into(), value: Some(json!({"text": "pause", "client": "k4"})) });
        let w = written(&mut core);
        assert!(w.iter().any(|v| v.get("method") == Some(&json!("thread/goal/set")) && v.get("params") == Some(&json!({"threadId": "th1", "status": "paused", "origin": "user"}))));
        c.command(&mut core, Command::Control { id: "goal".into(), value: Some(json!({"text": "clear", "client": "k4"})) });
        let w = written(&mut core);
        respond(&mut c, &mut core, &w, "thread/goal/clear", json!({"cleared": true}));
        assert!(replies(&mut core).iter().any(|(cl, v)| cl == "k4" && v.get("text") == Some(&json!("Goal cleared."))));
        c.command(&mut core, Command::Control { id: "goal".into(), value: Some(json!({"client": "k4"})) });
        let w = written(&mut core);
        respond(&mut c, &mut core, &w, "thread/goal/get", json!({"goal": null}));
        assert!(replies(&mut core).iter().any(|(cl, v)| cl == "k4" && v.get("text").and_then(Value::as_str).is_some_and(|t| t.starts_with("No goal"))));
        // /fork: the whole thread in another agent; /undo with no prompt yet says so.
        c.command(&mut core, Command::Control { id: "fork".into(), value: None });
        assert!(matches!(core.effects.pop(), Some(Effect::Branch { resume, resume_at: None }) if resume == "th1"));
        c.command(&mut core, Command::Control { id: "undo".into(), value: None });
        assert!(written(&mut core).iter().all(|v| v.get("method") != Some(&json!("thread/revert"))));
        // /export: the conversation from the timeline.
        c.command(&mut core, Command::Control { id: "export".into(), value: Some(json!({"client": "k5"})) });
        assert!(replies(&mut core).iter().any(|(cl, v)| cl == "k5" && v.get("format") == Some(&json!("markdown"))));
        Ok(())
    }

    #[test]
    fn the_threads_goal_is_followed_to_complete() {
        let mut c = Codex::new(None, None, None, false);
        let mut core = Core::new();
        let goal = |status: &str| json!({"threadId": "th1", "goal": {"threadId": "th1", "objective": "ship it", "status": status, "tokensUsed": 5, "timeUsedSeconds": 1, "createdAt": 1, "updatedAt": 2}});
        let status = |core: &Core| core.state.objective.as_ref().map(|o| (o.text.clone(), o.status.clone(), o.reason.clone()));
        c.notification(&mut core, "thread/goal/updated", goal("active"));
        assert_eq!(status(&core), Some(("ship it".into(), "active".into(), None)));
        c.notification(&mut core, "thread/goal/updated", goal("complete"));
        assert_eq!(status(&core), Some(("ship it".into(), "met".into(), None)));
        c.notification(&mut core, "thread/goal/updated", goal("budgetLimited"));
        assert!(matches!(status(&core), Some((_, s, Some(r))) if s == "unmet" && r.contains("budget")));
        c.notification(&mut core, "thread/goal/cleared", json!({"threadId": "th1"}));
        assert_eq!(status(&core), Some(("ship it".into(), "cleared".into(), None)));
    }

    #[test]
    fn skills_and_plugins_are_listed_viewed_and_run() -> anyhow::Result<()> {
        let (mut c, mut core, w) = opened();
        respond(
            &mut c,
            &mut core,
            &w,
            "skills/list",
            json!({"data": [{"cwd": "/work", "errors": [], "skills": [
                {"name": "imagegen", "description": "Generate images.", "shortDescription": null, "interface": {"shortDescription": "Images"}, "path": "/codex/skills/.system/imagegen/SKILL.md", "scope": "system", "enabled": true, "pluginId": null},
                {"name": "review", "description": "Review code.", "path": "/work/.codex/skills/review/SKILL.md", "scope": "repo", "enabled": true},
                {"name": "deploy", "description": "Deploy it.", "path": "/p/deploy/SKILL.md", "scope": "user", "enabled": true, "pluginId": "ops@market"},
                {"name": "off", "description": "Off.", "path": "/u/off/SKILL.md", "scope": "user", "enabled": false}
            ]}]}),
        );
        let sk: Vec<(&str, &str, bool)> = core.state.skills.iter().map(|s| (s.name.as_str(), s.source.as_str(), s.enabled)).collect();
        assert_eq!(sk, vec![("imagegen", "system", true), ("review", "project", true), ("deploy", "plugin", true), ("off", "user", false)]);
        // Skills join the catalog; Codex's own /review keeps its name (the skill runs from the panel).
        let skills: Vec<&str> = core.meta.commands.iter().filter(|c| matches!(c.route, Route::Prompt { skill: true, .. })).map(|c| c.name.as_str()).collect();
        assert_eq!(skills, vec!["deploy", "imagegen"]);
        assert!(core.meta.commands.iter().any(|c| c.name == "review" && matches!(&c.route, Route::Control { command } if command.get("id") == Some(&json!("review")))));
        respond(&mut c, &mut core, &w, "plugin/installed", json!({"marketplaces": [{"name": "market", "plugins": [{"id": "ops@market", "name": "ops", "installed": true, "enabled": true, "localVersion": "2.0.0", "interface": {"displayName": "Ops"}}, {"id": "x@market", "name": "x", "installed": false, "enabled": false}]}], "marketplaceLoadErrors": []}));
        assert_eq!(core.state.plugins.iter().map(|p| (p.name.as_str(), p.source.as_str(), p.version.as_deref())).collect::<Vec<_>>(), vec![("Ops", "market", Some("2.0.0"))]);
        c.command(&mut core, Command::SkillView { name: "review".into(), client: "v1".into() });
        assert!(matches!(core.effects.pop(), Some(Effect::Read { client, paths }) if client == "v1" && paths == ["/work/.codex/skills/review/SKILL.md"]));
        c.command(&mut core, Command::SkillView { name: "nope".into(), client: "v2".into() });
        assert!(replies(&mut core).iter().any(|(c, v)| c == "v2" && v.get("error").is_some()));
        c.command(&mut core, Command::SkillRun { name: "review".into(), args: Some("the diff".into()), id: Some("m1".into()), mode: SendMode::Queue });
        let w = written(&mut core);
        let start = w.iter().find(|v| v.get("method").and_then(Value::as_str) == Some("turn/start")).cloned().unwrap_or_default();
        assert_eq!(start.pointer("/params/input/0"), Some(&json!({"type": "skill", "name": "review", "path": "/work/.codex/skills/review/SKILL.md"})));
        assert_eq!(start.pointer("/params/input/1/text").and_then(Value::as_str), Some("the diff"));
        assert_eq!(start.pointer("/params/clientUserMessageId").and_then(Value::as_str), Some("m1"));
        feed(&mut c, &mut core, &[json!({"method": "skills/changed", "params": {}})]);
        assert!(written(&mut core).iter().any(|v| v.get("method").and_then(Value::as_str) == Some("skills/list") && v.pointer("/params/forceReload") == Some(&json!(true))));
        Ok(())
    }

    #[test]
    fn a_mode_change_applies_from_the_next_turn() -> anyhow::Result<()> {
        let (mut c, mut core, _) = opened();
        assert!(core.meta.modes.iter().any(|m| m.id == "read-only"));
        // An approval policy chosen on its own, then a mode: the mode's policies win.
        c.command(&mut core, Command::Control { id: "approval_policy".into(), value: Some(json!("untrusted")) });
        c.command(&mut core, Command::SetMode { mode: "read-only".into() });
        assert_eq!(core.state.mode, "read-only");
        assert_eq!(core.state.control_values.get("approval_policy"), Some(&json!("on-request")));
        let _ = written(&mut core);
        c.command(&mut core, Command::Send { id: "m1".into(), text: "hi".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        let w = written(&mut core);
        let start = w.iter().find(|v| v.get("method").and_then(Value::as_str) == Some("turn/start")).cloned().ok_or_else(|| anyhow::anyhow!("no turn"))?;
        assert_eq!(start.pointer("/params/approvalPolicy").and_then(Value::as_str), Some("on-request"));
        assert_eq!(start.pointer("/params/sandboxPolicy/type").and_then(Value::as_str), Some("readOnly"));
        // Full access is always a choice: chosen, it is allowed, its sandbox with it.
        c.command(&mut core, Command::SetMode { mode: "full-access".into() });
        assert_eq!(core.state.mode, "full-access");
        assert_eq!(core.state.control_values.get("sandbox"), Some(&json!("danger-full-access")));
        // Not a mode at all: refused.
        c.command(&mut core, Command::SetMode { mode: "nope".into() });
        assert_eq!(core.state.mode, "full-access");
        Ok(())
    }

    #[test]
    fn effort_levels_and_per_turn_overrides() -> anyhow::Result<()> {
        let mut core = Core::new();
        core.state.cwd = "/work".into();
        let mut c = Codex::new(None, None, None, false);
        c.start(&mut core);
        let ids = |core: &Core| core.meta.efforts.iter().map(|e| e.id.clone()).collect::<Vec<_>>();
        // Before model/list answers: its default model's levels (as Codex here last listed them,
        // else the newest model kiln knows of).
        assert_eq!(ids(&core), default_efforts());
        let w = written(&mut core);
        respond(&mut c, &mut core, &w, "initialize", json!({"userAgent": "kiln/0.162.1"}));
        let w = written(&mut core);
        respond(
            &mut c,
            &mut core,
            &w,
            "model/list",
            json!({"data": [
                {"id": "gpt-5", "model": "gpt-5", "displayName": "GPT-5", "description": "", "isDefault": true, "defaultReasoningEffort": "medium", "supportedReasoningEfforts": [{"reasoningEffort": "minimal", "description": ""}, {"reasoningEffort": "low", "description": ""}, {"reasoningEffort": "medium", "description": ""}, {"reasoningEffort": "high", "description": ""}]},
                {"id": "o-mini", "model": "o-mini", "displayName": "o mini", "description": "", "supportedReasoningEfforts": [{"reasoningEffort": "low", "description": ""}, {"reasoningEffort": "high", "description": ""}]}
            ]}),
        );
        respond(&mut c, &mut core, &w, "thread/start", json!({"thread": {"id": "th1", "turns": []}, "model": "gpt-5", "cwd": "/work"}));
        assert_eq!(ids(&core), ["minimal", "low", "medium", "high"]);
        assert_eq!(core.state.effort, "medium");
        // A model model/list does not name: the default model's levels.
        c.command(&mut core, Command::SetModel { model: "custom-model".into(), kind: None });
        c.command(&mut core, Command::Control { id: "reasoning_summary".into(), value: Some(json!("auto")) });
        assert_eq!(ids(&core).len(), 4);
        // Effort and model for the session, and for one message.
        c.command(&mut core, Command::SetEffort { effort: "high".into() });
        assert_eq!(core.state.effort, "high");
        let _ = written(&mut core);
        c.command(&mut core, Command::Send { id: "m1".into(), text: "one".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        let w = written(&mut core);
        let start = w.iter().find(|v| v.get("method").and_then(Value::as_str) == Some("turn/start")).cloned().ok_or_else(|| anyhow::anyhow!("no turn"))?;
        assert_eq!((start.pointer("/params/effort").and_then(Value::as_str), start.pointer("/params/model").and_then(Value::as_str)), (Some("high"), Some("custom-model")));
        respond(&mut c, &mut core, &w, "turn/start", json!({"turn": {"id": "t1", "status": "inProgress", "items": []}}));
        feed(&mut c, &mut core, &[json!({"method": "turn/completed", "params": {"turn": {"id": "t1", "status": "completed", "items": []}}})]);
        let _ = written(&mut core);
        let o = super::super::model::Overrides { model: Some("o-mini".into()), effort: Some("low".into()) };
        c.command(&mut core, Command::Send { id: "m2".into(), text: "two".into(), mode: SendMode::Queue, attachments: vec![], overrides: Some(o) });
        let w = written(&mut core);
        let start = w.iter().find(|v| v.get("method").and_then(Value::as_str) == Some("turn/start")).cloned().ok_or_else(|| anyhow::anyhow!("no turn"))?;
        assert_eq!((start.pointer("/params/effort").and_then(Value::as_str), start.pointer("/params/model").and_then(Value::as_str)), (Some("low"), Some("o-mini")));
        Ok(())
    }

    #[test]
    fn streamed_plans_and_reasoning_parts_keep_their_final_shape() {
        let (mut c, mut core, _) = opened();
        c.item(&mut core, &json!({"type": "plan", "id": "plan", "text": "The "}), false);
        feed(&mut c, &mut core, &[json!({"method": "item/plan/delta", "params": {"threadId": "th1", "turnId": "turn", "itemId": "plan", "delta": "plan"}})]);
        assert!(core.get("plan").is_some_and(|e| matches!(&e.body, Body::Assistant { text, streaming: true } if text == "The plan")));
        c.item(&mut core, &json!({"type": "plan", "id": "plan", "text": "The plan"}), true);
        assert!(core.get("plan").is_some_and(|e| matches!(&e.body, Body::Assistant { text, streaming: false } if text == "The plan")));
        feed(&mut c, &mut core, &[json!({"method": "item/plan/delta", "params": {"threadId": "th1", "turnId": "turn", "itemId": "delta-plan", "delta": "Another plan"}})]);
        assert!(core.get("delta-plan").is_some_and(|e| matches!(&e.body, Body::Assistant { streaming: true, .. })));
        for (index, text) in [(0, "First"), (1, "Second")] {
            feed(&mut c, &mut core, &[
                json!({"method": "item/reasoning/summaryPartAdded", "params": {"threadId": "th1", "turnId": "turn", "itemId": "thinking", "summaryIndex": index}}),
                json!({"method": "item/reasoning/summaryTextDelta", "params": {"threadId": "th1", "turnId": "turn", "itemId": "thinking", "summaryIndex": index, "delta": text}}),
            ]);
        }
        assert!(core.get("thinking").is_some_and(|e| matches!(&e.body, Body::Thinking { text, streaming: true, .. } if text == "First\n\nSecond")));
        c.item(&mut core, &json!({"type": "reasoning", "id": "thinking", "summary": ["First", "Second"], "content": []}), true);
        assert!(core.get("thinking").is_some_and(|e| matches!(&e.body, Body::Thinking { text, streaming: false, .. } if text == "First\n\nSecond")));
    }

    #[test]
    fn capped_tool_previews_stop_updates_without_losing_terminal_output() -> anyhow::Result<()> {
        let (mut c, mut core, _) = opened();
        c.item(&mut core, &json!({"type": "commandExecution", "id": "command", "command": "build", "cwd": "/work", "processId": "123", "status": "inProgress", "commandActions": []}), false);
        let _ = core.take_out();
        let output = |delta: &str| json!({"method": "item/commandExecution/outputDelta", "params": {"threadId": "th1", "turnId": "turn", "itemId": "command", "delta": delta}});
        feed(&mut c, &mut core, &[output("")]);
        assert!(core.take_out().is_empty());
        let first = "x".repeat(OUTPUT_MAX * 2);
        feed(&mut c, &mut core, &[output(&first), output("tail")]);
        assert!(core.get("command").is_some_and(|e| matches!(&e.body, Body::Tool(t) if t.truncated)));
        let _ = core.take_out();
        feed(&mut c, &mut core, &[output("more")]);
        assert!(core.take_out().is_empty());
        let (bytes, offset, _) = core.task_output.get("terminal-command").ok_or_else(|| anyhow::anyhow!("no terminal output"))?.since(0);
        assert!(bytes.ends_with(b"tailmore"));
        assert_eq!(offset, first.len() as u64 + 8);
        Ok(())
    }

    #[test]
    fn child_notifications_cannot_end_or_replace_the_parent_turn() {
        let (mut c, mut core, _) = opened();
        feed(&mut c, &mut core, &[json!({"method": "turn/started", "params": {"threadId": "th1", "turn": {"id": "parent-turn", "status": "inProgress", "items": []}}})]);
        assert_eq!(core.state.status, Status::Running);
        let started = core.state.turn_started;
        feed(&mut c, &mut core, &[
            json!({"method": "turn/started", "params": {"threadId": "child", "turn": {"id": "child-turn", "status": "inProgress", "items": []}}}),
            json!({"method": "item/agentMessage/delta", "params": {"threadId": "child", "turnId": "child-turn", "itemId": "child-message", "delta": "Child text"}}),
            json!({"method": "turn/completed", "params": {"threadId": "child", "turn": {"id": "child-turn", "status": "completed", "items": []}}}),
            json!({"method": "thread/status/changed", "params": {"threadId": "child", "status": {"type": "idle"}}}),
            json!({"method": "turn/completed", "params": {"threadId": "th1", "turn": {"id": "old-parent-turn", "status": "completed", "items": []}}}),
        ]);
        assert_eq!((core.state.status, c.turn.as_deref(), core.state.turn_started, core.state.usage.turns), (Status::Running, Some("parent-turn"), started, 0));
        assert!(core.get("child-message").is_none());
        feed(&mut c, &mut core, &[json!({"method": "turn/completed", "params": {"threadId": "th1", "turn": {"id": "parent-turn", "status": "completed", "items": []}}})]);
        assert_eq!(core.state.status, Status::Idle);
    }

    #[test]
    fn opening_a_thread_keeps_its_runtime_status_before_draining_the_queue() {
        for (status, expected) in [
            (json!({"type": "active", "activeFlags": []}), Status::Running),
            (json!({"type": "active", "activeFlags": ["waitingOnUserInput"]}), Status::Waiting),
            (json!({"type": "systemError"}), Status::Exited),
        ] {
            let (mut c, mut core, _) = opened();
            c.waiting.push(QueueItem { id: "queued".into(), text: "Next task".into(), at: Core::now(), attachments: Vec::new(), overrides: None });
            c.open_thread(&mut core, "thread/resume", json!({"threadId": "th1"}));
            let requests = written(&mut core);
            respond(&mut c, &mut core, &requests, "thread/resume", json!({"thread": {"id": "th1", "turns": [], "status": status}, "model": "gpt-5", "cwd": "/work"}));
            assert_eq!(core.state.status, expected);
            assert_eq!(c.waiting.len(), 1);
            assert!(!written(&mut core).iter().any(|v| v.get("method").and_then(Value::as_str) == Some("turn/start")));
        }
    }

    #[test]
    fn thread_runtime_status_survives_item_updates_and_keeps_approvals_waiting() {
        let (mut c, mut core, _) = opened();
        let status = |state: Value| json!({"method": "thread/status/changed", "params": {"threadId": "th1", "status": state}});
        feed(&mut c, &mut core, &[status(json!({"type": "active", "activeFlags": []}))]);
        c.item(&mut core, &json!({"type": "agentMessage", "id": "reply", "text": "Still working"}), true);
        assert_eq!(core.state.status, Status::Running);
        assert!(c.busy(&core));
        for flag in ["waitingOnApproval", "waitingOnUserInput"] {
            feed(&mut c, &mut core, &[status(json!({"type": "active", "activeFlags": [flag]}))]);
            c.settle(&mut core);
            assert_eq!(core.state.status, Status::Waiting);
        }
        feed(&mut c, &mut core, &[
            status(json!({"type": "active", "activeFlags": []})),
            json!({"id": "approval", "method": "item/commandExecution/requestApproval", "params": {"threadId": "th1", "turnId": "turn", "itemId": "command", "command": "build", "cwd": "/work"}}),
            status(json!({"type": "idle"})),
        ]);
        assert_eq!(core.state.status, Status::Waiting);
        // An approval's resolution can come from a child thread whose request was shown here.
        feed(&mut c, &mut core, &[json!({"method": "serverRequest/resolved", "params": {"threadId": "child", "requestId": "approval"}})]);
        assert_eq!(core.state.status, Status::Idle);
        for (state, expected) in [("notLoaded", Status::Starting), ("systemError", Status::Exited), ("idle", Status::Idle)] {
            feed(&mut c, &mut core, &[status(json!({"type": state}))]);
            feed(&mut c, &mut core, &[json!({"method": "turn/completed", "params": {"threadId": "th1", "turn": {"id": "turn", "status": "completed", "items": []}}})]);
            c.settle(&mut core);
            assert_eq!(core.state.status, expected);
            assert_eq!(core.state.exit.is_some(), expected == Status::Exited);
        }
    }

    #[test]
    fn collab_calls_keep_the_receiving_agents_lifetimes() -> anyhow::Result<()> {
        let (mut c, mut core, _) = opened();
        // codex-cli 0.162.1: the call completed, but its receiving agent is still running.
        let call = |id: &str, tool: &str, states: Value| json!({"type": "collabAgentToolCall", "id": id, "tool": tool, "senderThreadId": "th1", "receiverThreadIds": ["child"], "status": "completed", "agentsStates": states, "prompt": "Audit the changes"});
        c.item(&mut core, &call("spawn", "spawnAgent", json!({"child": {"status": "running"}})), true);
        let child = core.state.tasks.first().ok_or_else(|| anyhow::anyhow!("no child task"))?;
        assert_eq!((core.state.tasks.len(), child.id.as_str(), child.tool_id.as_deref(), child.status.as_str(), child.ended), (1, "child", Some("spawn"), "running", None));
        assert!(core.get("spawn").is_some_and(|e| matches!(&e.body, Body::Tool(t) if t.status == ToolStatus::Done && t.task.as_deref() == Some("child"))));
        c.item(&mut core, &call("spawn", "spawnAgent", json!({})), true);
        assert_eq!(core.state.tasks.first().map(|t| t.status.as_str()), Some("running"));
        c.item(&mut core, &call("wait", "wait", json!({"child": {"status": "completed"}, "other": {"status": "errored"}})), true);
        assert_eq!(core.state.tasks.len(), 2);
        let child = core.state.tasks.first().ok_or_else(|| anyhow::anyhow!("no child task"))?;
        assert_eq!((child.status.as_str(), child.tool_id.as_deref()), ("completed", Some("spawn")));
        assert!(child.ended.is_some());
        assert_eq!(core.state.tasks.iter().find(|t| t.id == "other").map(|t| t.status.as_str()), Some("failed"));
        c.item(&mut core, &call("resume", "resumeAgent", json!({"child": {"status": "pendingInit"}})), true);
        assert_eq!(core.state.tasks.first().map(|t| (t.status.as_str(), t.ended)), Some(("pending", None)));
        // A collab operation without any receiving agent is not itself a new task.
        c.item(&mut core, &json!({"type": "collabAgentToolCall", "id": "list", "tool": "listAgents", "senderThreadId": "th1", "receiverThreadIds": [], "status": "completed", "agentsStates": {}}), true);
        assert_eq!(core.state.tasks.len(), 2);
        Ok(())
    }

    #[test]
    fn subagent_activity_uses_its_kind_even_when_the_item_completed() -> anyhow::Result<()> {
        let (mut c, mut core, _) = opened();
        // Activity items have no status field. Completing a "started" marker does not end its agent.
        let activity = |id: &str, kind: &str| json!({"type": "subAgentActivity", "id": id, "agentThreadId": "child", "agentPath": "/root/audit", "kind": kind});
        for (kind, expected) in [("started", "running"), ("completed", "completed"), ("interacted", "running"), ("interrupted", "stopped")] {
            c.item(&mut core, &activity(kind, kind), true);
            let child = core.state.tasks.first().ok_or_else(|| anyhow::anyhow!("no child task"))?;
            assert_eq!((core.state.tasks.len(), child.id.as_str(), child.tool_id.as_deref(), child.status.as_str()), (1, "child", Some("started"), expected));
            assert_eq!(child.ended.is_none(), child.live());
        }
        Ok(())
    }

    #[test]
    fn terminals_codex_leaves_running() -> anyhow::Result<()> {
        let (mut c, mut core, _) = opened();
        let item = |status: &str, exit: Value| json!({"type": "commandExecution", "id": "call_4", "command": "/usr/bin/bash -lc 'for i in $(seq 1 120); do echo tick $i; sleep 1; done'", "cwd": "/work", "processId": "91122", "source": "unifiedExecStartup", "status": status, "exitCode": exit, "commandActions": []});
        feed(&mut c, &mut core, &[json!({"method": "turn/started", "params": {"turn": {"id": "t1"}}}), json!({"method": "item/started", "params": {"turnId": "t1", "item": item("inProgress", Value::Null)}})]);
        let t = core.state.tasks.iter().find(|t| t.process.as_deref() == Some("91122")).cloned().ok_or_else(|| anyhow::anyhow!("no terminal"))?;
        assert_eq!((t.class.as_deref(), t.command.as_deref(), t.cwd.as_deref(), t.status.as_str()), (Some("shell"), Some("for i in $(seq 1 120); do echo tick $i; sleep 1; done"), Some("/work"), "running"));
        feed(&mut c, &mut core, &[json!({"method": "item/commandExecution/outputDelta", "params": {"itemId": "call_4", "delta": "tick 1\n"}}), json!({"method": "item/commandExecution/outputDelta", "params": {"itemId": "call_4", "delta": "tick 2\n"}})]);
        let (bytes, offset, _) = core.task_output.get(&t.id).map(|o| o.since(0)).unwrap_or_default();
        assert_eq!((String::from_utf8_lossy(&bytes).as_ref(), offset), ("tick 1\ntick 2\n", 14));
        // The turn ends; the terminal goes on, left behind (not cut off).
        feed(&mut c, &mut core, &[json!({"method": "turn/completed", "params": {"turn": {"id": "t1", "status": "completed", "items": []}}})]);
        core.set_status(Status::Idle);
        assert!(core.state.tasks.iter().any(|x| x.id == t.id && x.live() && x.background));
        assert_eq!(core.state.count_left_behind(), 1);
        assert!(core.get("call_4").is_some_and(|e| matches!(&e.body, Body::Tool(x) if x.status == ToolStatus::Running)));
        // Stopping it: kiln stops the process (Codex cannot).
        c.command(&mut core, Command::StopTask { task: t.id.clone() });
        let Some(Effect::Kill { job, command }) = core.effects.pop() else { anyhow::bail!("no kill") };
        assert_eq!(command, "for i in $(seq 1 120); do echo tick $i; sleep 1; done");
        c.run_done(&mut core, &job, Some(0), "");
        assert_eq!(core.state.tasks.iter().find(|x| x.id == t.id).map(|x| x.status.clone()).as_deref(), Some("stopped"));
        // One that ends on its own: its exit code.
        let (mut c2, mut core2, _) = opened();
        feed(&mut c2, &mut core2, &[json!({"method": "item/started", "params": {"item": item("inProgress", Value::Null)}}), json!({"method": "item/completed", "params": {"item": item("completed", json!(3))}})]);
        let done = core2.state.tasks.first().cloned().unwrap_or_default();
        assert_eq!((done.status.as_str(), done.exit_code), ("failed", Some(3)));
        Ok(())
    }
}
