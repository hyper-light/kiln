//! Harnesses that speak the Agent Client Protocol (ACP: JSON-RPC 2.0 over stdio, as Zed and
//! other editors drive agents): Gemini CLI (`gemini --acp`), opencode (`opencode acp`), Goose
//! (`goose acp`), Amp through its ACP adapter, and any other ACP agent. kiln is the client: it
//! opens (or loads) a session, sends prompts, streams the agent's messages, thoughts, tool calls
//! (with diffs) and plan, answers its permission requests, switches modes and cancels turns.
//!
//! kiln advertises no file-system or terminal capabilities, so agents use their own tools.
//! Steering waits for the turn unless Devin advertises its mid-turn prompt extension.
//!
//! Devin CLI (`devin acp`) uses this adapter too, including ACP's session config options.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::core::{Baseline, Core, Effect};
use super::model::{
    AlwaysOption, Approval, Body, Command, Control, ControlKind, ControlOption, Decision, Delivery, ExitInfo, FileDiff, Level, ModeOption, ModelOption, QueueItem, SendMode,
    Inventory, Known, SlashCommand, Status, StatusItem, Todo, ToolCall, ToolKind, ToolStatus, OUTPUT_MAX, clip_json, clip_middle, sort_catalog,
};

mod catalog;

#[cfg(not(test))]
pub(super) fn warm_models() {
    catalog::warm_models();
}

#[cfg(test)]
mod config_tests;
#[cfg(test)]
mod live_tests;

/// kiln's own commands for an ACP agent.
fn acp_extras(account: bool) -> Vec<SlashCommand> {
    let mut commands = vec![
        SlashCommand::control("export", "The conversation as Markdown", "", "Session", json!({"type": "control", "id": "export", "value": {"client": "{client}"}})),
        SlashCommand::view("status", "The agent and what it can do", "Session", "status"),
        SlashCommand::view("model", "The model and mode", "Model & mode", "model"),
        SlashCommand::view("diff", "What this agent changed", "Context", "changes"),
        SlashCommand::control("restart", "Start the agent again", "", "Kiln", json!({"type": "restart"})),
        SlashCommand::view("new", "Start another agent", "Kiln", "new_agent"),
        SlashCommand::view("terminal", "A shell in this agent's folder, on its machine", "Kiln", "terminal").aka(&["shell"]),
        SlashCommand::view("screen", "The agent's own terminal interface, in its folder", "Kiln", "screen"),
        SlashCommand::view("goals", "Goals one after another: each sent once the one before is done", "Kiln", "goals"),
    ];
    if account {
        commands.push(SlashCommand::view("login", "Sign in to the harness on this machine", "Session", "login"));
        commands.push(SlashCommand::view("logout", "Sign out of the harness on this machine", "Session", "logout"));
    }
    commands
}

/// What an ACP agent cannot do from kiln.
const UNSUPPORTED: &str = "This agent's protocol (ACP) has no way to manage its MCP servers or skills from kiln.";

/// The agent's own status, from what initialize says: who it is, its protocol and what it can do.
fn status_rows(init: &Value) -> Vec<StatusItem> {
    let mut rows = Vec::new();
    let mut push = |group: &str, label: &str, value: String| {
        if !value.is_empty() {
            rows.push(StatusItem { group: group.to_owned(), label: label.to_owned(), value, tone: None });
        }
    };
    let s = |p: &str| init.pointer(p).and_then(Value::as_str).unwrap_or("").to_owned();
    let yes = |p: &str| if init.pointer(p).and_then(Value::as_bool) == Some(true) { "yes".to_owned() } else { "no".to_owned() };
    push("Agent", "Name", if s("/agentInfo/title").is_empty() { s("/agentInfo/name") } else { s("/agentInfo/title") });
    push("Agent", "Version", s("/agentInfo/version"));
    push("Agent", "Protocol", init.get("protocolVersion").map(|v| format!("ACP {v}")).unwrap_or_default());
    push("Capabilities", "Resume sessions", yes("/agentCapabilities/loadSession"));
    push("Capabilities", "Images", yes("/agentCapabilities/promptCapabilities/image"));
    push("Capabilities", "Audio", yes("/agentCapabilities/promptCapabilities/audio"));
    push("Capabilities", "MCP over HTTP", yes("/agentCapabilities/mcpCapabilities/http"));
    let auth: Vec<String> = init.get("authMethods").and_then(Value::as_array).into_iter().flatten().filter_map(|m| m.get("name").or_else(|| m.get("id")).and_then(Value::as_str).map(str::to_owned)).collect();
    push("Account", "Sign-in methods", auth.join(", "));
    rows
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum Pending {
    Initialize,
    Capabilities,
    Session,
    Refresh,
    RefreshNew,
    Prompt(String),
    SetMode(String),
    SetModel(String),
    SetConfig(Option<String>, bool),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Ask {
    rpc: Value,
    /// (option id, kind) as the agent offered them.
    options: Vec<(String, String)>,
}

/// Its whole state is plain data, written out when this process re-executes itself (an in-place
/// upgrade) and read back by the next image, which goes on with the same process.
#[derive(Serialize, Deserialize)]
pub struct Acp {
    next_id: u64,
    pending: HashMap<u64, Pending>,
    asks: HashMap<String, Ask>,
    session: Option<String>,
    resume: Option<String>,
    /// The prompt in flight (its client id).
    prompting: Option<String>,
    /// Devin's injected prompts share the original turn's result and usage.
    #[serde(default)]
    prompt_result: Option<Value>,
    #[serde(default)]
    steering: Option<bool>,
    /// Messages to send once the session exists or the turn has been cancelled.
    waiting: Vec<QueueItem>,
    can_load: bool,
    /// The agent takes images in prompts.
    images: bool,
    /// The event being streamed into, per kind of chunk.
    cur_agent: Option<String>,
    cur_thought: Option<String>,
    cur_user: Option<String>,
    /// This run's tag, in the ids of the events it makes ([`super::core::run_tag`]).
    #[serde(default = "super::core::run_tag")]
    run: String,
    seq: u64,
    notice_seq: u64,
    /// The files each tool call changes, with their text before it (from its diff content).
    befores: HashMap<String, Vec<(String, Option<String>)>>,
    /// kiln's Bypass: every request to go ahead is answered yes by kiln, at once.
    bypass: bool,
    /// The MCP servers its session is given (kiln's own, `crate::reach::mcp`).
    #[serde(default)]
    mcp: Vec<Value>,
    /// The conversation so far, in words, given with the first message it sends (a session that
    /// went on here from another machine, `super::moving`).
    #[serde(default)]
    context: Option<String>,
    /// Config options replace the older model and mode setters when the agent offers them.
    #[serde(default)]
    config: Vec<Value>,
    #[serde(default)]
    has_config: bool,
    /// Requested settings, applied one by one before the first prompt (model before effort).
    #[serde(default)]
    settings: Vec<(String, String)>,
    /// Later startup setters can reset the mode; no prompt may escape its requested permission.
    #[serde(default)]
    startup_mode: Option<String>,
    /// Settings received together wait for each reply, since one can change the next's choices.
    #[serde(default)]
    changes: Vec<(String, Value)>,
    #[serde(default)]
    configuring: Option<u64>,
    /// A rejected startup value is retried after signing in, when its choices can change.
    #[serde(default)]
    applying: Option<(String, String)>,
    #[serde(default)]
    failed_setting: Option<String>,
    /// Older images handed over sessions that were already open.
    #[serde(default = "ready")]
    ready: bool,
    #[serde(default)]
    replaying: bool,
    /// Its account can be managed through kiln's existing account card.
    #[serde(default)]
    account: bool,
    /// A sign-in reloads credentials once the current turn and its requests have finished.
    #[serde(default)]
    refresh: bool,
    /// Only a new conversation with no prompt or history may be replaced after signing in.
    #[serde(default)]
    empty: bool,
    /// The speed the session had (`Spec::speed`: a restart, a move), set once it's ready where
    /// the model in use has a `speed` option.
    #[serde(default)]
    speed_start: Option<String>,
    /// A `/compact` it was sent: its turn is a compaction (Devin says nothing else of one).
    #[serde(default)]
    compact_prompt: Option<String>,
    /// The compaction it says is under way (`compaction_update`), by its id.
    #[serde(default)]
    compaction: Option<String>,
}

fn ready() -> bool {
    true
}

/// kiln's own permission modes, for agents whose own all keep their checks: kiln asks you, or
/// kiln answers every request to go ahead itself.
pub const KILN_ASK: &str = "kiln-ask";
pub const KILN_BYPASS: &str = "kiln-bypass";

/// The config option an agent offers its speeds as (Devin's Standard and Fast, on the models that
/// go faster): kiln's speed (`Meta::speeds`), not a control of its own.
const SPEED: &str = "speed";

fn kiln_ask() -> ModeOption {
    ModeOption { id: KILN_ASK.into(), label: "Ask".into(), description: "Ask before anything the agent asks to do".into(), dangerous: false }
}

fn kiln_bypass() -> ModeOption {
    ModeOption { id: KILN_BYPASS.into(), label: "Bypass".into(), description: "kiln approves everything the agent asks to do".into(), dangerous: true }
}

impl Acp {
    pub fn new(resume: Option<String>) -> Self {
        let empty = resume.is_none();
        Self {
            next_id: 0,
            pending: HashMap::new(),
            asks: HashMap::new(),
            session: None,
            resume,
            prompting: None,
            prompt_result: None,
            steering: None,
            waiting: Vec::new(),
            can_load: false,
            images: false,
            cur_agent: None,
            cur_thought: None,
            cur_user: None,
            run: super::core::run_tag(),
            seq: 0,
            notice_seq: 0,
            befores: HashMap::new(),
            bypass: false,
            mcp: Vec::new(),
            context: None,
            config: Vec::new(),
            has_config: false,
            settings: Vec::new(),
            startup_mode: None,
            changes: Vec::new(),
            configuring: None,
            applying: None,
            failed_setting: None,
            ready: false,
            replaying: false,
            account: false,
            refresh: false,
            empty,
            speed_start: None,
            compact_prompt: None,
            compaction: None,
        }
    }

    pub fn from_spec(spec: &super::session::Spec) -> Self {
        let mut a = Self::new(spec.resume.clone()).with_mode(spec.mode.as_deref());
        a.account = spec.kind == "devin";
        for (category, value) in [("model", &spec.model), ("thought_level", &spec.effort)] {
            if let Some(v) = value.as_ref().filter(|v| !v.is_empty()) {
                a.settings.push((category.into(), v.clone()));
            }
        }
        a.speed_start.clone_from(&spec.speed);
        a
    }

    /// The conversation so far, in words, to give with the first message it sends.
    pub fn offer_context(&mut self, text: Option<String>) {
        self.context = text.filter(|t| !t.is_empty());
    }

    /// Its session gets these MCP servers (ACP's `mcpServers`, stdio).
    pub fn offer_mcp(&mut self, servers: Vec<Value>) {
        self.mcp = servers;
    }

    /// Starts in this mode: kiln's Bypass goes on at once (the agent's own modes it is told of).
    pub fn with_mode(mut self, mode: Option<&str>) -> Self {
        self.bypass = mode == Some(KILN_BYPASS);
        if let Some(m) = mode.filter(|m| !m.is_empty() && *m != KILN_BYPASS && *m != KILN_ASK) {
            self.startup_mode = Some(m.into());
            self.settings.push(("mode".into(), m.into()));
        }
        self
    }

    fn request(&mut self, core: &mut Core, kind: Pending, method: &str, params: Value) {
        self.next_id += 1;
        self.pending.insert(self.next_id, kind);
        core.write(&json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}));
    }

    fn notify(core: &mut Core, method: &str, params: Value) {
        core.write(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn notice(&mut self, core: &mut Core, level: Level, text: impl Into<String>) {
        self.notice_seq += 1;
        core.upsert(&format!("notice-{}-{}", self.run, self.notice_seq), None, Body::Notice { level, text: text.into() });
    }

    fn busy(&self, core: &Core) -> bool {
        self.prompting.is_some() || self.pending.values().any(|p| matches!(p, Pending::Initialize | Pending::Session | Pending::Refresh | Pending::RefreshNew | Pending::SetMode(_) | Pending::SetModel(_) | Pending::SetConfig(_, _))) || !core.state.approvals.is_empty()
    }

    fn settle(&self, core: &mut Core) {
        core.settle_status(self.prompting.is_some());
    }

    /// The streamed message or thought stops growing (a tool call or the turn's end came).
    fn close_streams(&mut self, core: &mut Core) {
        for id in [self.cur_agent.take(), self.cur_thought.take(), self.cur_user.take()].into_iter().flatten() {
            if let Some(e) = core.get_mut(&id) {
                match &mut e.body {
                    Body::Assistant { streaming, .. } | Body::Thinking { streaming, .. } => *streaming = false,
                    _ => {}
                }
            }
        }
    }

    pub fn start(&mut self, core: &mut Core) {
        self.replaying = self.resume.is_some() && core.events().any(|e| matches!(e.body, Body::User { .. } | Body::Assistant { .. }));
        core.meta.features = ["send_now", "queue", "interrupt", "set_mode", "resume", "attachments"].iter().map(|s| (*s).to_owned()).collect();
        core.touch_meta();
        // kiln's commands until the agent lists its own.
        if core.meta.commands.is_empty() {
            core.meta.commands = sort_catalog(acp_extras(self.account));
        }
        // ACP has no way to report MCP servers, skills, plugins or plan limits.
        core.state.inventory = Inventory { mcp: Known::Unsupported, skills: Known::Unsupported, plugins: Known::Unsupported, limits: Known::Unsupported };
        core.touch_state();
        self.initialize(core, Pending::Initialize);
    }

    fn initialize(&mut self, core: &mut Core, kind: Pending) {
        self.request(
            core,
            kind,
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false, "session": {"configOptions": {"boolean": {}}, "compaction": {}}},
                "clientInfo": {"name": "kiln", "version": env!("CARGO_PKG_VERSION")},
            }),
        );
    }

    fn capabilities(&mut self, core: &mut Core, result: &Value) {
        let steering = self.account && result.pointer("/agentCapabilities/_meta/cognition.ai~1midTurnPrompts").and_then(Value::as_bool) == Some(true);
        self.steering = Some(steering);
        core.meta.features.retain(|f| f != "steer" && f != "queue_steer");
        if steering {
            core.meta.features.extend(["steer".into(), "queue_steer".into()]);
        }
        core.touch_meta();
    }

    pub fn tick(&mut self, core: &mut Core) {
        if self.account && self.steering.is_none() && self.session.is_some() && !self.pending.values().any(|p| matches!(p, Pending::Initialize)) && core.state.status != Status::Exited {
            // Older images did not retain this capability; initialize leaves Devin's session intact.
            self.steering = Some(false);
            self.initialize(core, Pending::Capabilities);
        }
    }

    pub fn exited(&mut self, core: &mut Core, code: Option<i32>, error: Option<String>) {
        self.close_streams(core);
        self.prompting = None;
        self.prompt_result = None;
        self.pending.clear();
        self.asks.clear();
        self.compact_prompt = None;
        self.compaction = None;
        core.state.approvals.clear();
        core.state.exit = Some(ExitInfo::now(code, error));
        core.state.doing = None;
        core.state.turn_started = None;
        core.set_status(Status::Exited);
        core.touch_state();
    }

    // ------------------------------------------------------------------ commands

    pub fn command(&mut self, core: &mut Core, cmd: Command) {
        match cmd {
            Command::Send { id, text, mode, attachments, overrides, reply_to, .. } => {
                // Its go-back is kiln's (`harness::goback`): a checkpoint anchors the prompt's
                // moment so "restore files to here" reaches back to it.
                core.effects.push(Effect::Checkpoint { at: Core::now() });
                self.send(core, QueueItem { id, text, at: Core::now(), attachments, overrides, reply_to }, mode);
            }
            Command::QueueEdit { id, text, attachments } => {
                if let Some(q) = core.state.queue.iter_mut().find(|q| q.id == id) {
                    q.text = text;
                    if let Some(a) = attachments { q.attachments = a; }
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
                if self.steering == Some(true)
                    && let Some(pos) = core.state.queue.iter().position(|q| q.id == id)
                {
                    let item = core.state.queue.remove(pos);
                    core.touch_state();
                    self.send(core, item, SendMode::Steer);
                }
            }
            Command::Approve { id, decision, message, always, .. } => self.answer(core, &id, decision, message, always),
            Command::Interrupt => self.cancel(core),
            Command::SetMode { mode } => self.change(core, "mode".into(), json!(mode)),
            Command::SetModel { model, .. } => self.change(core, "model".into(), json!(model)),
            Command::SetEffort { effort } => self.change(core, "thought_level".into(), json!(effort)),
            Command::SetSpeed { speed } => self.set_speed(core, &speed),
            Command::Suggest { client, .. } => core.reply(client, json!({"suggestions": []})),
            // kiln exports the conversation from its timeline.
            Command::Control { id, value } if id == "export" => {
                let client = value.as_ref().and_then(|v| v.get("client")).and_then(Value::as_str).unwrap_or("").to_owned();
                core.reply(client, json!({"title": "Conversation", "text": core.export(), "format": "markdown", "filename": "conversation.md"}));
            }
            Command::Control { id, .. } if id == "account" && self.account => {
                if core.state.status != Status::Exited {
                    self.refresh = true;
                    self.next(core);
                }
            }
            // Its go-back is kiln's where its own ends (`harness::goback`).
            Command::Control { id, value } if super::goback::control(core, &id, value.as_ref()) => {}
            Command::Control { id, value } => {
                if id.starts_with("acp:")
                    && let Some(value) = value
                {
                    self.change(core, id, value);
                }
            }
            Command::StopTask { .. } | Command::StopTasks | Command::Restart | Command::ForceStop | Command::Snapshot | Command::MoveTo { .. } | Command::Moving { .. } | Command::HandedOver { .. } => {}
            // A pause's commands are the session's (`harness::pause`).
            Command::Resume | Command::AutoResume { .. } | Command::PauseAt { .. } | Command::MaxSubagents { .. } | Command::MoveOnLimit { .. } | Command::LimitHold { .. } => {}
            // An ACP agent reports no MCP servers or skills of its own (its client hands it MCP
            // servers when the session opens); its commands run as "/name args" messages.
            Command::SkillRun { name, args, id, mode } => {
                let name = name.trim().trim_start_matches('/').to_owned();
                if name.is_empty() {
                    return;
                }
                let text = match args.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
                    Some(a) => format!("/{name} {a}"),
                    None => format!("/{name}"),
                };
                self.send(core, QueueItem { id: id.unwrap_or_else(crate::util::new_id), text, at: Core::now(), attachments: Vec::new(), overrides: None, reply_to: None }, mode);
            }
            Command::SkillView { client, .. } | Command::McpAuth { client, .. } => core.reply(client, json!({"error": UNSUPPORTED})),
            Command::McpAdd { client, .. } | Command::McpRemove { client, .. } => match client.filter(|c| !c.is_empty()) {
                Some(c) => core.reply(c, json!({"ok": false, "error": UNSUPPORTED})),
                None => self.notice(core, Level::Warn, UNSUPPORTED),
            },
            Command::McpEnable { .. } | Command::McpReconnect { .. } => self.notice(core, Level::Warn, UNSUPPORTED),
            Command::McpRefresh | Command::SkillsReload => {}
        }
    }

    /// Nothing runs beside an ACP agent.
    pub fn run_done(&mut self, _core: &mut Core, _job: &str, _code: Option<i32>, _output: &str) {}

    fn account_refresh(&mut self, core: &mut Core) -> bool {
        if !self.refresh {
            return false;
        }
        if self.busy(core) || core.state.status == Status::Exited {
            return true;
        }
        self.refresh = false;
        if self.ready && self.startup_mode.is_none() && !core.state.mode.is_empty() && core.state.mode != KILN_ASK && core.state.mode != KILN_BYPASS {
            self.startup_mode = Some(core.state.mode.clone());
        }
        self.ready = false;
        self.replaying = core.events().any(|e| matches!(e.body, Body::User { .. } | Body::Assistant { .. }));
        let cwd = core.state.cwd.clone();
        match self.session.clone() {
            Some(s) if self.can_load => self.request(core, Pending::Refresh, "session/load", json!({"sessionId": s, "cwd": cwd, "mcpServers": self.mcp})),
            Some(_) => self.notice(core, Level::Error, "This agent cannot reload its account in an open session."),
            None => self.request(core, Pending::RefreshNew, "session/new", json!({"cwd": cwd, "mcpServers": self.mcp})),
        }
        true
    }

    fn change(&mut self, core: &mut Core, id: String, value: Value) {
        if !self.ready && let Some(category) = self.failed_setting.as_deref()
            && (id == category || id.strip_prefix("acp:").is_some_and(|id| self.config_id(category).as_deref() == Some(id)))
            && let Some(value) = value.as_str()
        {
            self.applying = Some((category.into(), value.into()));
            if category == "mode" {
                self.startup_mode = Some(value.into());
            }
        }
        self.changes.push((id, value));
        self.next_change(core);
    }

    fn next_change(&mut self, core: &mut Core) -> bool {
        if self.changes.is_empty() {
            return false;
        }
        if self.refresh || self.session.is_none() || (!self.ready && self.failed_setting.is_none()) || self.pending.values().any(|p| matches!(p, Pending::Initialize | Pending::Session | Pending::Refresh | Pending::RefreshNew | Pending::SetMode(_) | Pending::SetModel(_) | Pending::SetConfig(_, _))) {
            return true;
        }
        while !self.changes.is_empty() {
            let (id, value) = self.changes.remove(0);
            if let Some(config) = id.strip_prefix("acp:") {
                if self.set_option(core, config, value) {
                    return true;
                }
                continue;
            }
            let Some(value) = value.as_str() else { continue };
            if id == "mode" {
                if self.local_mode(core, value) {
                    if !self.ready && self.failed_setting.as_deref() == Some("mode") && self.startup_mode.as_deref() == Some(value) {
                        self.failed_setting = None;
                        self.applying = None;
                        self.configured(core);
                        return true;
                    }
                    continue;
                }
                self.bypass = false;
            }
            if self.config_id(&id).is_some() {
                if self.set_config(core, &id, value) {
                    return true;
                }
            } else if !self.has_config && let Some(s) = self.session.clone() {
                match id.as_str() {
                    "mode" => self.request(core, Pending::SetMode(value.into()), "session/set_mode", json!({"sessionId": s, "modeId": value})),
                    "model" => self.request(core, Pending::SetModel(value.into()), "session/set_model", json!({"sessionId": s, "modelId": value})),
                    _ => continue,
                }
                return true;
            }
        }
        false
    }

    fn local_mode(&mut self, core: &mut Core, mode: &str) -> bool {
        if mode != KILN_BYPASS && mode != KILN_ASK {
            return false;
        }
        // kiln's own: the agent's mode stays as it is.
        self.bypass = mode == KILN_BYPASS;
        core.state.mode = mode.into();
        core.touch_state();
        if self.bypass {
            core.effects.push(Effect::AllowBypass);
            let waiting: Vec<String> = self.asks.keys().cloned().collect();
            for id in waiting {
                self.answer(core, &id, Decision::Allow, None, None);
            }
        }
        true
    }

    fn config_id(&self, category: &str) -> Option<String> {
        self.config.iter().filter(|c| c.get("category").and_then(Value::as_str) == Some(category) && c.get("type").and_then(Value::as_str) == Some("select") && c.get("currentValue").and_then(Value::as_str).is_some()).find_map(|c| c.get("id").and_then(Value::as_str).map(str::to_owned))
    }

    /// The category picks the prominent selector; every other option remains a regular control.
    fn config_options(&mut self, core: &mut Core, options: &[Value]) {
        self.has_config = true;
        self.config = options.to_vec();
        let mut known = std::mem::take(&mut core.meta.models);
        if self.account {
            let keep_default = known.iter().any(|m| m.default);
            for mut model in super::kinds::models_of("devin") {
                if keep_default {
                    model.default = false;
                }
                match known.iter_mut().find(|m| m.id == model.id) {
                    Some(kept) => {
                        kept.default |= model.default;
                        if kept.efforts.is_empty() {
                            kept.efforts = model.efforts;
                        }
                    }
                    None => known.push(model),
                }
            }
        }
        core.meta.modes.clear();
        core.meta.efforts.clear();
        core.meta.controls.retain(|c| !c.id.starts_with("acp:"));
        core.state.control_values.retain(|id, _| !id.starts_with("acp:"));
        core.meta.features.retain(|f| f != "set_model" && f != "set_effort" && f != "set_speed");
        // Its speeds are kiln's own selector (the head's bolt), not a control.
        let speed = self.speed_option();
        match &speed {
            Some((standard, faster)) => {
                let current = self.config.iter().find(|c| c.get("id").and_then(Value::as_str) == Some(SPEED)).and_then(|c| c.get("currentValue")).and_then(Value::as_str).unwrap_or_default();
                core.state.speed = if current == standard { String::new() } else { current.to_owned() };
                core.meta.speeds.clone_from(faster);
                core.meta.features.push("set_speed".into());
            }
            None => {
                core.state.speed.clear();
                core.meta.speeds.clear();
            }
        }
        let mut primary = std::collections::HashSet::new();
        for c in options {
            let Some(id) = c.get("id").and_then(Value::as_str) else { continue };
            if id == SPEED && speed.is_some() {
                continue;
            }
            let category = c.get("category").and_then(Value::as_str).unwrap_or("");
            let current = c.get("currentValue").cloned().unwrap_or(Value::Null);
            let kind = match c.get("type").and_then(Value::as_str) {
                Some("select") if current.is_string() => ControlKind::Select,
                Some("boolean") if current.is_boolean() => ControlKind::Toggle,
                _ => continue,
            };
            let choices = config_choices(c);
            let prominent = kind == ControlKind::Select && matches!(category, "model" | "mode" | "thought_level") && primary.insert(category);
            if prominent {
                match category {
                    "model" => {
                        core.meta.models = choices.iter().map(|o| ModelOption { id: o.id.clone(), label: o.label.clone(), description: o.description.clone(), efforts: known.iter().find(|m| m.id == o.id).map(|m| m.efforts.clone()).unwrap_or_default(), default: known.iter().find(|m| m.id == o.id).is_some_and(|m| m.default) }).collect();
                        core.state.model = current.as_str().unwrap_or_default().to_owned();
                        core.meta.features.push("set_model".into());
                    }
                    "mode" => {
                        core.meta.modes = choices.iter().map(|o| ModeOption { id: o.id.clone(), label: o.label.clone(), description: o.description.clone(), dangerous: o.dangerous }).collect();
                        if !self.bypass {
                            core.state.mode = current.as_str().unwrap_or_default().to_owned();
                        }
                    }
                    "thought_level" => {
                        core.meta.efforts = choices;
                        core.state.effort = current.as_str().unwrap_or_default().to_owned();
                        core.meta.features.push("set_effort".into());
                    }
                    _ => {}
                }
                continue;
            }
            let key = format!("acp:{id}");
            core.state.control_values.insert(key.clone(), current.clone());
            core.meta.controls.push(Control {
                id: key,
                group: match category { "mode" => "Permissions", "thought_level" => "Thinking", "model" | "model_config" => "Model", _ => "Session" }.into(),
                label: c.get("name").and_then(Value::as_str).unwrap_or(id).into(),
                description: c.get("description").and_then(Value::as_str).unwrap_or_default().into(),
                kind,
                options: choices,
                value: Some(current),
                dangerous: false,
                per_turn: false,
                requires: None,
                scope: Default::default(),
                input: None,
            });
        }
        if !primary.contains("model") {
            core.state.model.clear();
        }
        if !primary.contains("thought_level") {
            core.state.effort.clear();
        }
        if let Some(model) = core.meta.models.iter_mut().find(|m| m.id == core.state.model) {
            model.efforts = core.meta.efforts.iter().map(|e| e.id.clone()).collect();
        }
        if !primary.contains("mode") {
            core.meta.modes.push(kiln_ask());
            core.state.mode = KILN_ASK.into();
        }
        if !core.meta.modes.iter().any(|m| m.dangerous) {
            core.meta.modes.push(kiln_bypass());
        }
        if self.bypass {
            core.state.mode = KILN_BYPASS.into();
        }
        // Its go-back (rewind, restore files, branch, fork) is kiln's own (`harness::goback`).
        super::goback::declare(core);
        core.touch_meta();
        core.touch_state();
    }

    fn set_option(&mut self, core: &mut Core, id: &str, value: Value) -> bool {
        let Some(s) = self.session.clone() else { return false };
        let Some(c) = self.config.iter().find(|c| c.get("id").and_then(Value::as_str) == Some(id)) else { return false };
        let boolean = match c.get("type").and_then(Value::as_str) {
            Some("boolean") if c.get("currentValue").is_some_and(Value::is_boolean) => true,
            Some("select") if c.get("currentValue").is_some_and(Value::is_string) => false,
            _ => return false,
        };
        let valid = if boolean { value.is_boolean() } else { value.as_str().is_some_and(|v| config_choices(c).iter().any(|o| o.id == v)) };
        if !valid {
            self.notice(core, Level::Warn, format!("The agent does not offer that value for {id}."));
            return false;
        }
        let params = if boolean {
            json!({"sessionId": s, "configId": id, "type": "boolean", "value": value})
        } else {
            json!({"sessionId": s, "configId": id, "value": value})
        };
        let category = c.get("category").and_then(Value::as_str).filter(|category| self.config_id(category).as_deref() == Some(id)).map(str::to_owned);
        let bypass = category.as_deref() == Some("mode") && value.as_str().is_some_and(|mode| super::lineage::acp_level(mode) == Some(super::lineage::Level::Bypass));
        self.request(core, Pending::SetConfig(category, bypass), "session/set_config_option", params);
        true
    }

    fn set_config(&mut self, core: &mut Core, category: &str, value: &str) -> bool {
        let Some(id) = self.config_id(category) else { return false };
        self.set_option(core, &id, json!(value))
    }

    fn configured(&mut self, core: &mut Core) {
        self.configuring = None;
        while !self.settings.is_empty() {
            let (category, value) = self.settings.remove(0);
            self.applying = Some((category.clone(), value.clone()));
            if category == "mode" && self.local_mode(core, &value) {
                self.applying = None;
                continue;
            }
            if self.config_id(&category).is_some() {
                if self.set_config(core, &category, &value) {
                    self.configuring = Some(self.next_id);
                }
            } else if !self.has_config {
                let Some(s) = self.session.clone() else { return };
                match category.as_str() {
                    "mode" => self.request(core, Pending::SetMode(value.clone()), "session/set_mode", json!({"sessionId": s, "modeId": value})),
                    "model" => self.request(core, Pending::SetModel(value.clone()), "session/set_model", json!({"sessionId": s, "modelId": value})),
                    _ => {
                        self.applying = None;
                        continue;
                    }
                }
                self.configuring = Some(self.next_id);
            }
            if self.configuring.is_none() {
                self.failed_setting = Some(category.clone());
                self.notice(core, Level::Error, format!("The agent could not apply the requested {category}; no prompt was sent."));
                self.next(core);
            }
            return;
        }
        if let Some(mode) = &self.startup_mode && core.state.mode != *mode {
            self.failed_setting = Some("mode".into());
            self.applying = Some(("mode".into(), mode.clone()));
            self.notice(core, Level::Error, format!("The agent did not keep the requested mode {mode}; no prompt was sent. Choose a mode to continue."));
            self.next(core);
            return;
        }
        self.startup_mode = None;
        self.ready = true;
        core.set_status(Status::Idle);
        // The speed it had, where the model in use has one: a speed it can't take holds no prompt.
        if let Some(speed) = self.speed_start.take() {
            self.set_speed(core, &speed);
        }
        self.next(core);
    }

    /// The agent's `speed` option (Devin's, on the models that go faster), as kiln says it: its
    /// standard value ("standard", else its first), and the faster ones in its order.
    fn speed_option(&self) -> Option<(String, Vec<ControlOption>)> {
        let c = self.config.iter().find(|c| c.get("id").and_then(Value::as_str) == Some(SPEED) && c.get("type").and_then(Value::as_str) == Some("select") && c.get("currentValue").is_some_and(Value::is_string))?;
        let choices = config_choices(c);
        let standard = choices.iter().find(|o| o.id == "standard").or_else(|| choices.first())?.id.clone();
        let faster: Vec<ControlOption> = choices.into_iter().filter(|o| o.id != standard).collect();
        (!faster.is_empty()).then_some((standard, faster))
    }

    /// The speed it runs at ("" standard), set as its `speed` option, where the model in use has one.
    fn set_speed(&mut self, core: &mut Core, speed: &str) {
        let Some((standard, _)) = self.speed_option() else { return };
        let value = if speed.is_empty() { standard } else { speed.to_owned() };
        self.change(core, format!("acp:{SPEED}"), json!(value));
    }

    fn setting_done(&mut self, core: &mut Core, id: &Value, category: Option<&str>) {
        let corrected = !self.ready && self.configuring.is_none() && category.is_some() && category == self.failed_setting.as_deref();
        if (self.configuring.is_some() && id.as_u64() == self.configuring) || corrected {
            self.failed_setting = None;
            self.applying = None;
            self.configured(core);
        } else {
            self.next(core);
        }
    }

    fn setting_failed(&mut self, core: &mut Core, id: &Value, category: Option<&str>, message: &str) {
        if self.configuring.is_some() && id.as_u64() == self.configuring {
            self.configuring = None;
            self.failed_setting = category.map(str::to_owned);
            self.notice(core, Level::Error, format!("The agent could not apply the requested settings: {message}. No prompt was sent."));
            self.next(core);
        } else {
            self.notice(core, Level::Warn, message);
            self.next(core);
        }
    }

    fn cancel(&mut self, core: &mut Core) {
        let Some(s) = self.session.clone() else { return };
        if self.prompting.is_none() {
            return;
        }
        Self::notify(core, "session/cancel", json!({"sessionId": s}));
        // Pending permission requests are answered "cancelled", as the protocol asks.
        for (id, ask) in std::mem::take(&mut self.asks) {
            core.write(&json!({"jsonrpc": "2.0", "id": ask.rpc, "result": {"outcome": {"outcome": "cancelled"}}}));
            core.take_approval(&id);
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
        if !self.ready || self.session.is_none() {
            self.show(core, &m, Delivery::Queued);
            self.waiting.push(m);
            return;
        }
        if self.busy(core) {
            if mode == SendMode::Steer && self.steering == Some(true) && self.prompting.is_some() && self.prompt_result.is_none() {
                self.prompt(core, m);
            } else if mode == SendMode::Now {
                self.show(core, &m, Delivery::Queued);
                self.waiting.insert(0, m);
                self.cancel(core);
            } else {
                core.state.queue.push(m);
                core.touch_state();
            }
            return;
        }
        self.prompt(core, m);
    }

    fn prompt(&mut self, core: &mut Core, m: QueueItem) {
        let Some(s) = self.session.clone() else { return };
        self.empty = false;
        self.close_streams(core);
        // Images inline where the agent takes them; everything else as a link to the file. The
        // conversation so far comes first, once, when it went on here from another machine.
        let mut blocks = Vec::new();
        if let Some(c) = self.context.take() {
            blocks.push(json!({"type": "text", "text": c}));
        }
        if !m.text.is_empty() {
            blocks.push(json!({"type": "text", "text": m.text}));
        }
        for a in &m.attachments {
            if a.is_image() && self.images && !a.data_base64.is_empty() {
                blocks.push(json!({"type": "image", "mimeType": a.mime, "data": a.data_base64}));
            } else if let Some(p) = &a.path {
                blocks.push(json!({"type": "resource_link", "uri": format!("file://{p}"), "name": a.name, "mimeType": a.mime}));
            }
        }
        self.request(core, Pending::Prompt(m.id.clone()), "session/prompt", json!({"sessionId": s, "prompt": blocks}));
        if self.prompting.is_none() {
            self.prompting = Some(m.id.clone());
            core.state.turn_started = Some(Core::now());
            core.state.doing = Some("Thinking".to_owned());
        }
        // Its own `/compact`: the prompt's turn is the compaction, which it may say nothing else of.
        let t = m.text.trim();
        if (t == "/compact" || t.starts_with("/compact ")) && core.meta.commands.iter().any(|c| c.name == "compact") {
            self.compact_prompt = Some(m.id.clone());
            core.compacting(None);
        }
        self.show(core, &m, Delivery::Started);
        self.settle(core);
    }

    fn show(&mut self, core: &mut Core, m: &QueueItem, d: Delivery) {
        core.state.delivery.insert(m.id.clone(), d);
        let (images, names) = super::model::attachment_summary(&m.attachments);
        core.note_sent(&m.id, &m.text);
        core.upsert(&m.id, None, Body::User { text: m.text.clone(), images, client_id: Some(m.id.clone()), origin: Some(super::core::origin_of(&m.id).into()), attachments: names, reply_to: m.reply_to.clone() });
        core.touch_state();
    }

    pub(crate) fn next(&mut self, core: &mut Core) {
        if self.account_refresh(core) {
            return;
        }
        if self.next_change(core) {
            return;
        }
        if self.busy(core) || !self.ready || self.session.is_none() || core.state.status == Status::Exited {
            return;
        }
        // A pause (or a switch) holds what waits, and the stop point may start one here.
        if (!self.waiting.is_empty() || !core.state.queue.is_empty()) && !super::pause::open(core) {
            return;
        }
        if !self.waiting.is_empty() {
            let m = self.waiting.remove(0);
            self.prompt(core, m);
        } else if !core.state.queue.is_empty() {
            let item = core.state.queue.remove(0);
            core.touch_state();
            let id = item.id.clone();
            self.prompt(core, item);
            super::pause::sent_from_queue(core, &id);
        }
    }

    /// A pause ended: the turn it cut off goes on first, then kiln's queue.
    pub fn go_on(&mut self, core: &mut Core, p: super::model::Pause) {
        if p.cut.is_some() && !self.busy(core) && core.state.status != Status::Exited {
            let id = crate::util::new_id();
            self.send(core, QueueItem { id: id.clone(), text: super::pause::CONTINUE.to_owned(), at: Core::now(), attachments: Vec::new(), overrides: None, reply_to: None }, SendMode::Queue);
            super::pause::from_kiln(core, &id);
            return;
        }
        if p.cut.is_some() {
            super::pause::continue_later(core, super::pause::CONTINUE);
        }
        self.next(core);
    }

    fn answer(&mut self, core: &mut Core, id: &str, decision: Decision, message: Option<String>, always: Option<usize>) {
        let Some(approval) = core.take_approval(id) else { return };
        let Some(ask) = self.asks.remove(id) else { return };
        let pick = |kinds: &[&str]| ask.options.iter().find(|(_, k)| kinds.contains(&k.as_str())).map(|(o, _)| o.clone());
        let option = match decision {
            Decision::Allow => pick(&["allow_once"]).or_else(|| pick(&["allow_always"])),
            Decision::Always => always.and_then(|i| approval.always.get(i)).and_then(|o| o.value.as_str().map(str::to_owned)).or_else(|| pick(&["allow_always"])).or_else(|| pick(&["allow_once"])),
            Decision::Deny => pick(&["reject_once"]).or_else(|| pick(&["reject_always"])),
        };
        let outcome = match option {
            Some(o) => json!({"outcome": "selected", "optionId": o}),
            None => json!({"outcome": "cancelled"}),
        };
        core.write(&json!({"jsonrpc": "2.0", "id": ask.rpc, "result": {"outcome": outcome}}));
        if let Some(tid) = approval.tool_id.as_deref()
            && let Some(e) = core.get_mut(tid)
            && let Body::Tool(t) = &mut e.body
        {
            t.status = if decision == Decision::Deny { ToolStatus::Denied } else { ToolStatus::Running };
        }
        // ACP carries no words with a refusal: the feedback becomes the next prompt.
        if decision == Decision::Deny
            && let Some(f) = message.filter(|m| !m.trim().is_empty())
        {
            let m = QueueItem { id: crate::util::new_id(), text: f, at: Core::now(), attachments: Vec::new(), overrides: None, reply_to: None };
            self.show(core, &m, Delivery::Queued);
            self.waiting.insert(0, m);
        }
        self.settle(core);
        if self.refresh {
            self.next(core);
        }
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
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        match (method, v.get("id").cloned()) {
            (Some(m), Some(id)) => self.agent_request(core, &m, id, params),
            (Some(m), None) => {
                if m == "session/update" {
                    self.update(core, params.get("update").unwrap_or(&Value::Null));
                }
            }
            (None, Some(id)) => self.response(core, &id, &v),
            (None, None) => {}
        }
        self.settle(core);
    }

    fn response(&mut self, core: &mut Core, id: &Value, v: &Value) {
        let Some(kind) = id.as_u64().and_then(|n| self.pending.remove(&n)) else { return };
        let response;
        let v = if let Pending::Prompt(client) = &kind {
            let cancelled = v.get("error").is_some() || v.pointer("/result/stopReason").and_then(Value::as_str) == Some("cancelled");
            core.state.delivery.insert(client.clone(), if cancelled { Delivery::Cancelled } else { Delivery::Completed });
            core.touch_state();
            // Its `/compact` is over: its words say how it went (one it reports ends as it says).
            if self.compact_prompt.as_deref() == Some(client.as_str()) {
                self.compact_prompt = None;
                if self.compaction.is_none() {
                    core.compacting_over(None);
                }
            }
            if self.prompting.as_deref() == Some(client.as_str()) {
                self.prompt_result = Some(v.clone());
            } else if let Some(err) = v.get("error") {
                self.notice(core, Level::Error, err.get("message").and_then(Value::as_str).unwrap_or("request failed"));
            }
            if self.pending.values().any(|p| matches!(p, Pending::Prompt(_))) {
                return;
            }
            response = self.prompt_result.take().unwrap_or_else(|| v.clone());
            &response
        } else {
            v
        };
        let refreshed = matches!(&kind, Pending::Refresh | Pending::RefreshNew);
        let new_session = matches!(&kind, Pending::RefreshNew) || (matches!(&kind, Pending::Session) && self.resume.is_none());
        let result = v.get("result").cloned().unwrap_or(Value::Null);
        if let Some(err) = v.get("error") {
            let msg = err.get("message").and_then(Value::as_str).unwrap_or("request failed").to_owned();
            match kind {
                Pending::Prompt(_) => {
                    self.prompting = None;
                    if super::pause::says_limit(&msg) {
                        // The account's usage limit: pause until it resets (the agent can't say
                        // when; kiln checks back now and then).
                        let hit = super::pause::Hit::from_limits(&core.state.limits, msg.clone(), None, Core::now());
                        let cut = super::pause::cut_now(core);
                        super::pause::hold(core, super::pause::LIMIT, "harness", hit, Some(cut));
                    } else {
                        self.notice(core, Level::Error, msg);
                    }
                    core.state.turn_started = None;
                    core.state.doing = None;
                    core.touch_state();
                    self.next(core);
                }
                Pending::Session => {
                    let hint = if msg.to_lowercase().contains("auth") { " (sign in to it in a terminal on this machine first)" } else { "" };
                    self.notice(core, Level::Error, format!("The agent could not open a session: {msg}{hint}"));
                    core.set_status(Status::Idle);
                }
                Pending::Refresh if self.empty && err.get("code").and_then(Value::as_i64) == Some(-32016) => {
                    self.session = None;
                    core.state.harness_session = None;
                    self.replaying = false;
                    core.touch_state();
                    self.request(core, Pending::RefreshNew, "session/new", json!({"cwd": core.state.cwd, "mcpServers": self.mcp}));
                }
                Pending::Refresh | Pending::RefreshNew => {
                    self.replaying = false;
                    self.notice(core, Level::Error, format!("The agent could not reload its account: {msg}. Sign in again to retry."));
                    core.set_status(Status::Idle);
                }
                Pending::Initialize => self.notice(core, Level::Error, format!("The agent did not start: {msg}")),
                Pending::Capabilities => {}
                Pending::SetMode(_) => self.setting_failed(core, id, Some("mode"), &msg),
                Pending::SetModel(_) => self.setting_failed(core, id, Some("model"), &msg),
                Pending::SetConfig(category, _) => self.setting_failed(core, id, category.as_deref(), &msg),
            }
            return;
        }
        match kind {
            Pending::Initialize => {
                self.ready = false;
                self.capabilities(core, &result);
                self.can_load = result.pointer("/agentCapabilities/loadSession").and_then(Value::as_bool).unwrap_or(false);
                self.images = result.pointer("/agentCapabilities/promptCapabilities/image").and_then(Value::as_bool).unwrap_or(false);
                if let Some(info) = result.get("agentInfo") {
                    core.meta.version = info.get("version").and_then(Value::as_str).unwrap_or("").to_owned();
                    core.touch_meta();
                }
                core.state.harness_status = status_rows(&result);
                core.touch_state();
                let cwd = core.state.cwd.clone();
                match self.resume.clone().filter(|_| self.can_load) {
                    Some(sid) => {
                        self.session = Some(sid.clone());
                        core.state.harness_session = Some(sid.clone());
                        self.request(core, Pending::Session, "session/load", json!({"sessionId": sid, "cwd": cwd, "mcpServers": self.mcp}));
                    }
                    None => {
                        if self.resume.is_some() {
                            self.notice(core, Level::Warn, "This agent cannot load earlier sessions; starting a new one.");
                        }
                        self.request(core, Pending::Session, "session/new", json!({"cwd": cwd, "mcpServers": self.mcp}));
                    }
                }
            }
            Pending::Capabilities => self.capabilities(core, &result),
            Pending::Session | Pending::Refresh | Pending::RefreshNew => {
                self.close_streams(core);
                self.replaying = false;
                if let Some(s) = result.get("sessionId").and_then(Value::as_str) {
                    self.session = Some(s.to_owned());
                    core.state.harness_session = Some(s.to_owned());
                }
                if let Some(modes) = result.get("modes") {
                    core.meta.modes = modes
                        .get("availableModes")
                        .and_then(Value::as_array)
                        .map(|ms| {
                            ms.iter()
                                .filter_map(|m| {
                                    let id = m.get("id")?.as_str()?.to_owned();
                                    let dangerous = id.to_lowercase().contains("yolo") || id.to_lowercase().contains("bypass");
                                    Some(ModeOption {
                                        label: m.get("name").and_then(Value::as_str).unwrap_or(&id).to_owned(),
                                        description: m.get("description").and_then(Value::as_str).unwrap_or("").to_owned(),
                                        id,
                                        dangerous,
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    if let Some(c) = modes.get("currentModeId").and_then(Value::as_str) {
                        core.state.mode = c.to_owned();
                    }
                    // Its modes all keep its checks: kiln's own Bypass joins them.
                    if !core.meta.modes.iter().any(|m| m.dangerous) {
                        core.meta.modes.push(kiln_bypass());
                    }
                    core.touch_meta();
                } else {
                    // No modes of its own: kiln asks you, or approves everything itself.
                    core.meta.modes = vec![kiln_ask(), kiln_bypass()];
                    core.state.mode = KILN_ASK.to_owned();
                    core.touch_meta();
                }
                if self.bypass {
                    core.state.mode = KILN_BYPASS.to_owned();
                    core.touch_state();
                }
                if let Some(models) = result.get("models") {
                    core.meta.models = models
                        .get("availableModels")
                        .and_then(Value::as_array)
                        .map(|ms| {
                            ms.iter()
                                .filter_map(|m| {
                                    let id = m.get("modelId")?.as_str()?.to_owned();
                                    Some(ModelOption { label: m.get("name").and_then(Value::as_str).unwrap_or(&id).to_owned(), description: m.get("description").and_then(Value::as_str).unwrap_or("").to_owned(), id, ..ModelOption::default() })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    if let Some(c) = models.get("currentModelId").and_then(Value::as_str) {
                        core.state.model = c.to_owned();
                    }
                    core.touch_meta();
                    if !core.meta.features.iter().any(|f| f == "set_model") {
                        core.meta.features.push("set_model".into());
                    }
                }
                if let Some(options) = result.get("configOptions").and_then(Value::as_array) {
                    self.config_options(core, options);
                } else if self.has_config {
                    self.config_options(core, &self.config.clone());
                }
                if new_session {
                    for model in &mut core.meta.models {
                        model.default = model.id == core.state.model;
                    }
                }
                core.touch_state();
                if refreshed {
                    if self.applying.as_ref().is_some_and(|(category, _)| Some(category) == self.failed_setting.as_ref()) && let Some(setting) = self.applying.take() {
                        self.settings.insert(0, setting);
                        self.failed_setting = None;
                    }
                    if let Some(mode) = &self.startup_mode {
                        if core.state.mode != *mode && !self.settings.iter().any(|(category, _)| category == "mode") {
                            self.settings.insert(0, ("mode".into(), mode.clone()));
                        }
                        if self.failed_setting.as_deref() == Some("mode") {
                            self.failed_setting = None;
                        }
                    }
                    if self.failed_setting.is_some() {
                        self.next(core);
                        return;
                    }
                }
                self.configured(core);
            }
            Pending::Prompt(_) => {
                self.close_streams(core);
                self.prompting = None;
                let stop = result.get("stopReason").and_then(Value::as_str).unwrap_or("end_turn");
                match stop {
                    "cancelled" => self.notice(core, Level::Info, "Interrupted"),
                    "refusal" => self.notice(core, Level::Warn, "The agent refused to continue."),
                    "max_tokens" => self.notice(core, Level::Warn, "Stopped: the output limit was reached."),
                    "max_turn_requests" => self.notice(core, Level::Warn, "Stopped: the turn limit was reached."),
                    _ => {}
                }
                // Calls the turn left open were cut off with it.
                let open: Vec<String> = core
                    .events()
                    .filter(|e| matches!(&e.body, Body::Tool(t) if matches!(t.status, ToolStatus::Running | ToolStatus::Pending)))
                    .map(|e| e.id.clone())
                    .collect();
                for tid in open {
                    if let Some(e) = core.get_mut(&tid)
                        && let Body::Tool(t) = &mut e.body
                    {
                        t.status = ToolStatus::Cancelled;
                    }
                }
                core.state.usage.turns += 1;
                if let Some(usage) = result.get("usage") {
                    core.state.usage.input_tokens += usage.get("inputTokens").and_then(Value::as_u64).unwrap_or(0);
                    core.state.usage.output_tokens += usage.get("outputTokens").and_then(Value::as_u64).unwrap_or(0);
                    core.state.usage.cache_read_tokens += usage.get("cachedReadTokens").and_then(Value::as_u64).unwrap_or(0);
                }
                // How it ended, for kiln's keep-going.
                let how = match stop {
                    "end_turn" => "done",
                    "cancelled" => "stopped",
                    "refusal" => "refused",
                    "max_turn_requests" => "limit",
                    _ => "cut",
                };
                core.end_turn(core.state.turn_started, how, Some(stop));
                core.state.turn_started = None;
                core.state.doing = None;
                core.touch_state();
                self.next(core);
            }
            Pending::SetMode(m) => {
                let bypass = super::lineage::acp_level(&m) == Some(super::lineage::Level::Bypass);
                if !self.has_config {
                    core.state.mode = m;
                    core.touch_state();
                }
                if bypass && super::lineage::acp_level(&core.state.mode) == Some(super::lineage::Level::Bypass) {
                    core.effects.push(Effect::AllowBypass);
                }
                self.setting_done(core, id, Some("mode"));
            }
            Pending::SetModel(m) => {
                if !self.has_config {
                    core.state.model = m;
                    core.touch_state();
                }
                self.setting_done(core, id, Some("model"));
            }
            Pending::SetConfig(category, bypass) => {
                if let Some(options) = result.get("configOptions").and_then(Value::as_array) {
                    self.config_options(core, options);
                    if bypass && super::lineage::acp_level(&core.state.mode) == Some(super::lineage::Level::Bypass) {
                        core.effects.push(Effect::AllowBypass);
                    }
                    self.setting_done(core, id, category.as_deref());
                } else {
                    self.setting_failed(core, id, category.as_deref(), "The agent returned no configuration after changing a setting");
                }
            }
        }
    }

    fn agent_request(&mut self, core: &mut Core, method: &str, rpc: Value, p: Value) {
        if method != "session/request_permission" {
            // kiln offered no file system or terminal: the agent should not ask for them.
            core.write(&json!({"jsonrpc": "2.0", "id": rpc, "error": {"code": -32601, "message": format!("kiln does not provide {method}")}}));
            return;
        }
        let id = match &rpc {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let tc = p.get("toolCall").cloned().unwrap_or(Value::Null);
        let tool_id = tc.get("toolCallId").and_then(Value::as_str).map(str::to_owned);
        // What the call is: from the request, else from the call already on the timeline.
        let known = tool_id.as_deref().and_then(|t| core.get(t)).and_then(|e| match &e.body {
            Body::Tool(t) => Some(t.as_ref().clone()),
            _ => None,
        });
        let call = merge(known, &tc, &core.state.cwd);
        let options: Vec<(String, String)> = p
            .get("options")
            .and_then(Value::as_array)
            .map(|o| o.iter().filter_map(|x| Some((x.get("optionId")?.as_str()?.to_owned(), x.get("kind")?.as_str()?.to_owned()))).collect())
            .unwrap_or_default();
        // kiln's Bypass: yes, at once (once, so the agent's own rules stay as they are).
        if self.bypass {
            let pick = |kind: &str| options.iter().find(|(_, k)| k == kind).map(|(o, _)| o.clone());
            if let Some(o) = pick("allow_once").or_else(|| pick("allow_always")) {
                core.write(&json!({"jsonrpc": "2.0", "id": rpc, "result": {"outcome": {"outcome": "selected", "optionId": o}}}));
                if let Some(t) = &tool_id {
                    let mut running = call;
                    running.status = ToolStatus::Running;
                    core.upsert(t, None, Body::Tool(Box::new(running)));
                }
                return;
            }
        }
        let always: Vec<AlwaysOption> = p
            .get("options")
            .and_then(Value::as_array)
            .map(|o| {
                o.iter()
                    .filter(|x| x.get("kind").and_then(Value::as_str) == Some("allow_always"))
                    .filter_map(|x| {
                        Some(AlwaysOption {
                            label: x.get("name").and_then(Value::as_str).unwrap_or("Always allow").to_owned(),
                            scope: "the agent's rules".into(),
                            value: x.get("optionId")?.clone(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let approval = Approval {
            id: id.clone(),
            tool_id: tool_id.clone(),
            tool_name: call.name.clone(),
            tool: call.tool,
            title: call.title.clone(),
            detail: call.detail.clone(),
            input: call.input.clone(),
            command: (call.tool == ToolKind::Shell).then(|| call.title.clone()),
            cwd: core.state.cwd.clone(),
            diffs: call.diffs.clone(),
            reason: None,
            always,
            default_to_no: false,
            questions: Vec::new(),
            plan: None,
            agent: None,
            at: Core::now(),
        };
        if let Some(t) = &tool_id {
            let mut pending = call;
            pending.status = ToolStatus::Pending;
            core.upsert(t, None, Body::Tool(Box::new(pending)));
        }
        core.state.doing = Some("Waiting for your approval".to_owned());
        self.asks.insert(id, Ask { rpc, options });
        core.add_approval(approval);
    }

    fn update(&mut self, core: &mut Core, u: &Value) {
        let history = matches!(
            u.get("sessionUpdate").and_then(Value::as_str),
            Some("user_message_chunk" | "agent_message_chunk" | "agent_thought_chunk" | "tool_call" | "tool_call_update" | "compaction_update" | "compaction_summary_chunk")
        );
        if history {
            self.empty = false;
        }
        if self.replaying && history {
            return;
        }
        match u.get("sessionUpdate").and_then(Value::as_str).unwrap_or("") {
            "agent_message_chunk" | "agent_thought_chunk" | "user_message_chunk" => {
                let kind = u.get("sessionUpdate").and_then(Value::as_str).unwrap_or("");
                if (kind == "user_message_chunk" && (self.cur_agent.is_some() || self.cur_thought.is_some())) || (kind != "user_message_chunk" && self.cur_user.is_some()) {
                    self.close_streams(core);
                }
                let text = content_text(u.get("content"));
                if text.is_empty() {
                    return;
                }
                let slot = match kind {
                    "agent_message_chunk" => &mut self.cur_agent,
                    "agent_thought_chunk" => &mut self.cur_thought,
                    _ => &mut self.cur_user,
                };
                match slot.clone() {
                    Some(id) => core.append(&id, &text),
                    None => {
                        self.seq += 1;
                        let id = format!("acp-{kind}-{}-{}", self.run, self.seq);
                        let body = match kind {
                            "agent_message_chunk" => Body::Assistant { text: text.clone(), streaming: true },
                            "agent_thought_chunk" => Body::Thinking { text: text.clone(), streaming: true, redacted: false },
                            _ => Body::User { text: text.clone(), images: 0, client_id: None, origin: Some("human".into()), attachments: Vec::new(), reply_to: None },
                        };
                        core.upsert(&id, None, body);
                        match kind {
                            "agent_message_chunk" => self.cur_agent = Some(id),
                            "agent_thought_chunk" => self.cur_thought = Some(id),
                            _ => self.cur_user = Some(id),
                        }
                    }
                }
                if kind == "agent_message_chunk" {
                    // The agent's latest words.
                    if let Some(Body::Assistant { text, .. }) = self.cur_agent.as_deref().and_then(|i| core.get(i)).map(|e| &e.body) {
                        core.state.summary = clip_middle(text.trim(), 600).0;
                    }
                    core.state.doing = Some("Writing".to_owned());
                    core.touch_state();
                } else if kind == "agent_thought_chunk" && core.state.doing.as_deref() != Some("Thinking") {
                    core.state.doing = Some("Thinking".to_owned());
                    core.touch_state();
                }
            }
            "tool_call" | "tool_call_update" => {
                self.close_streams(core);
                let Some(tid) = u.get("toolCallId").and_then(Value::as_str).map(str::to_owned) else { return };
                let known = core.get(&tid).and_then(|e| match &e.body {
                    Body::Tool(t) => Some(t.as_ref().clone()),
                    _ => None,
                });
                let call = merge(known, u, &core.state.cwd);
                let entries = diff_entries(u);
                if !entries.is_empty() {
                    self.befores.insert(tid.clone(), entries);
                }
                if call.status == ToolStatus::Done {
                    for (path, old) in self.befores.remove(&tid).unwrap_or_default() {
                        let abs = if std::path::Path::new(&path).is_absolute() { path.clone() } else { format!("{}/{path}", core.state.cwd.trim_end_matches('/')) };
                        let rel = super::claude::rel(&path, &core.state.cwd);
                        core.note_baseline(&abs, old.map_or(Baseline::Absent, Baseline::Text));
                        core.want_change(&abs, &rel);
                    }
                }
                if matches!(call.status, ToolStatus::Running | ToolStatus::Pending) {
                    core.state.doing = Some(super::claude::doing(&call));
                    core.touch_state();
                }
                core.upsert(&tid, None, Body::Tool(Box::new(call)));
            }
            "plan" => {
                core.state.todos = u
                    .get("entries")
                    .and_then(Value::as_array)
                    .map(|es| {
                        es.iter()
                            .filter_map(|e| Some(Todo { text: e.get("content")?.as_str()?.to_owned(), status: e.get("status").and_then(Value::as_str).unwrap_or("pending").to_owned() }))
                            .collect()
                    })
                    .unwrap_or_default();
                core.touch_state();
            }
            "available_commands_update" => {
                let mut cmds: Vec<SlashCommand> = u
                    .get("availableCommands")
                    .and_then(Value::as_array)
                    .map(|cs| {
                        cs.iter()
                            .filter_map(|c| {
                                let name = c.get("name")?.as_str()?;
                                let hint = c.pointer("/input/hint").and_then(Value::as_str).map(|h| format!("[{h}]")).unwrap_or_default();
                                Some(SlashCommand::prompt(name, c.get("description").and_then(Value::as_str).unwrap_or(""), &hint, "Session"))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if self.account {
                    cmds.retain(|c| c.name != "login" && c.name != "logout");
                }
                cmds.extend(acp_extras(self.account));
                core.meta.commands = sort_catalog(cmds);
                core.touch_meta();
            }
            "current_mode_update" => {
                if !self.has_config && !self.bypass && let Some(m) = u.get("currentModeId").and_then(Value::as_str) {
                    core.state.mode = m.to_owned();
                    core.touch_state();
                }
            }
            "config_option_update" => {
                if let Some(options) = u.get("configOptions").and_then(Value::as_array) {
                    self.config_options(core, options);
                }
            }
            "usage_update" => {
                core.state.usage.context_tokens = u.get("used").and_then(Value::as_u64);
                core.state.usage.context_window = u.get("size").and_then(Value::as_u64);
                if let Some(cost) = u.get("cost").filter(|c| c.get("currency").and_then(Value::as_str) == Some("USD")).and_then(|c| c.get("amount")).and_then(Value::as_f64) {
                    core.state.usage.cost_usd = cost;
                }
                core.touch_state();
            }
            "compaction_update" => self.compaction_update(core, u),
            // A compaction's summary (`compaction_summary_chunk`) isn't shown.
            _ => {}
        }
    }

    /// A compaction it reports (ACP's `session.compaction`, which kiln offers): one by its id,
    /// under way until it says how it ended, with what claude-agent-acp adds of it
    /// (`_meta.contextCompaction`).
    fn compaction_update(&mut self, core: &mut Core, u: &Value) {
        let Some(id) = u.get("compactionId").and_then(Value::as_str) else { return };
        let key = format!("{}-{id}", self.run);
        let meta = u.pointer("/_meta/contextCompaction");
        let n = |k: &str| meta.and_then(|m| m.get(k)).and_then(Value::as_u64);
        match u.get("status").and_then(Value::as_str) {
            Some("in_progress") => {
                self.compaction = Some(id.to_owned());
                core.compacting(None);
            }
            Some(status @ ("completed" | "failed" | "cancelled")) => {
                self.compaction = None;
                let c = core.compacting_of(None).cloned();
                let ms = n("durationMs").and_then(|d| i64::try_from(d).ok()).or_else(|| c.as_ref().map(|c| Core::now() - c.since));
                match status {
                    "completed" => core.compacted(&key, None, Level::Info, super::core::compacted_words(n("preTokens").or(c.and_then(|c| c.tokens)), n("postTokens"), ms)),
                    "failed" => {
                        let why = u.get("error").or_else(|| meta.and_then(|m| m.get("error"))).and_then(Value::as_str);
                        core.compacted(&key, None, Level::Warn, super::core::compaction_failed_words(why));
                    }
                    _ if c.is_some() => core.compaction_stopped(None),
                    _ => core.compacted(&key, None, Level::Warn, "Compaction stopped".to_owned()),
                }
            }
            // Statuses ACP adds later: nothing is read into them.
            _ => {}
        }
    }
}

/// ACP selects are flat or grouped; kiln's existing selectors keep each group's name in its labels.
fn config_choices(c: &Value) -> Vec<ControlOption> {
    let mut choices = Vec::new();
    for option in c.get("options").and_then(Value::as_array).into_iter().flatten() {
        let (group, values) = match option.get("options").and_then(Value::as_array) {
            Some(values) => (option.get("name").and_then(Value::as_str), values.as_slice()),
            None => (None, std::slice::from_ref(option)),
        };
        for value in values {
            let Some(id) = value.get("value").and_then(Value::as_str) else { continue };
            let name = value.get("name").and_then(Value::as_str).unwrap_or(id);
            choices.push(ControlOption {
                id: id.into(),
                label: group.map_or_else(|| name.to_owned(), |g| format!("{g} · {name}")),
                description: value.get("description").and_then(Value::as_str).unwrap_or_default().into(),
                dangerous: super::lineage::acp_level(id) == Some(super::lineage::Level::Bypass),
            });
        }
    }
    choices
}

fn content_text(c: Option<&Value>) -> String {
    match c {
        Some(Value::Object(o)) if o.get("type").and_then(Value::as_str) == Some("text") => o.get("text").and_then(Value::as_str).unwrap_or("").to_owned(),
        Some(Value::Object(o)) if o.get("type").and_then(Value::as_str) == Some("resource_link") => o.get("uri").and_then(Value::as_str).map(|u| format!("[{u}]")).unwrap_or_default(),
        Some(Value::Object(o)) if o.get("type").and_then(Value::as_str) == Some("resource") => o.get("resource").and_then(|r| r.get("text")).and_then(Value::as_str).unwrap_or_default().to_owned(),
        Some(Value::Array(a)) => a.iter().map(|x| content_text(Some(x))).collect::<Vec<_>>().join(""),
        _ => String::new(),
    }
}

/// (path, text before) of each diff a tool call carries.
fn diff_entries(u: &Value) -> Vec<(String, Option<String>)> {
    u.get("content")
        .and_then(Value::as_array)
        .map(|c| {
            c.iter()
                .filter(|x| x.get("type").and_then(Value::as_str) == Some("diff"))
                .filter_map(|x| Some((x.get("path")?.as_str()?.to_owned(), x.get("oldText").and_then(Value::as_str).map(str::to_owned))))
                .collect()
        })
        .unwrap_or_default()
}

/// A tool call (or an update to one) merged onto what is known of it.
fn merge(known: Option<ToolCall>, u: &Value, cwd: &str) -> ToolCall {
    let tid = u.get("toolCallId").and_then(Value::as_str).unwrap_or("").to_owned();
    let mut t = known.unwrap_or_else(|| ToolCall {
        tool_id: tid.clone(),
        name: "tool".into(),
        tool: ToolKind::Other,
        title: String::new(),
        detail: None,
        input: Value::Null,
        status: ToolStatus::Pending,
        output: None,
        truncated: false,
        diffs: Vec::new(),
        todos: Vec::new(),
        task: None,
        exit_code: None,
        started: Core::now(),
        ended: None,
        server: None,
        tool_short: None,
        skill: None,
        runner: None,
    });
    if let Some(k) = u.get("kind").and_then(Value::as_str) {
        t.name = k.to_owned();
        t.tool = match k {
            "read" => ToolKind::Read,
            "edit" | "delete" | "move" => ToolKind::Edit,
            "search" => ToolKind::Search,
            "execute" => ToolKind::Shell,
            "fetch" => ToolKind::Web,
            "switch_mode" => ToolKind::Plan,
            _ => ToolKind::Other,
        };
    }
    if let Some(title) = u.get("title").and_then(Value::as_str) {
        t.title = title.to_owned();
    }
    if let Some(raw) = u.get("rawInput") {
        t.input = clip_json(raw, 16 * 1024);
        // A command, where the agent gives one, reads better than its title.
        if t.tool == ToolKind::Shell
            && let Some(cmd) = raw.get("command").and_then(Value::as_str).or_else(|| raw.get("cmd").and_then(Value::as_str))
        {
            t.title = cmd.to_owned();
        }
    }
    if let Some(locs) = u.get("locations").and_then(Value::as_array)
        && let Some(path) = locs.first().and_then(|l| l.get("path")).and_then(Value::as_str)
    {
        let shown = super::claude::rel(path, cwd);
        if t.title.is_empty() {
            t.title = shown;
        } else if t.tool != ToolKind::Shell {
            t.detail = Some(shown);
        }
    }
    if let Some(s) = u.get("status").and_then(Value::as_str) {
        t.status = match s {
            "in_progress" => ToolStatus::Running,
            "completed" => ToolStatus::Done,
            "failed" if t.status == ToolStatus::Denied => ToolStatus::Denied,
            "failed" => ToolStatus::Error,
            _ => ToolStatus::Pending,
        };
        if matches!(t.status, ToolStatus::Done | ToolStatus::Error | ToolStatus::Denied) {
            t.ended = Some(Core::now());
        }
    }
    let mut content_output = false;
    if let Some(content) = u.get("content").and_then(Value::as_array) {
        let mut diffs: Vec<FileDiff> = Vec::new();
        let mut text = String::new();
        for c in content {
            match c.get("type").and_then(Value::as_str) {
                Some("diff") => {
                    let path = c.get("path").and_then(Value::as_str).unwrap_or("");
                    let shown = super::claude::rel(path, cwd);
                    let new = c.get("newText").and_then(Value::as_str).unwrap_or("");
                    match c.get("oldText").and_then(Value::as_str) {
                        None => diffs.push(super::diff::created(&shown, new)),
                        Some(old) => diffs.push(FileDiff { path: shown, created: false, hunks: super::diff::hunks(old, new, super::diff::CONTEXT) }),
                    }
                }
                Some("content") => text.push_str(&content_text(c.get("content"))),
                _ => {}
            }
        }
        if !diffs.is_empty() {
            t.diffs = diffs;
        }
        if !text.is_empty() {
            content_output = true;
            let (o, cut) = clip_middle(&text, OUTPUT_MAX);
            t.output = Some(o);
            t.truncated = cut;
        }
    }
    if !content_output
        && let Some(raw) = u.get("rawOutput").filter(|v| !v.is_null())
    {
        let text = raw.get("output").or_else(|| raw.get("stdout")).and_then(Value::as_str).map_or_else(|| raw.to_string(), str::to_owned);
        let (output, truncated) = clip_middle(&text, OUTPUT_MAX);
        t.output = Some(output);
        t.truncated = truncated;
    }
    if t.title.is_empty() {
        t.title = t.name.clone();
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(core: &mut Core) -> Vec<Value> {
        core.take_writes().iter().filter_map(|w| serde_json::from_str(w).ok()).collect()
    }

    fn feed(a: &mut Acp, core: &mut Core, msgs: &[Value]) {
        for m in msgs {
            a.line(core, &m.to_string());
        }
    }

    /// kiln offers ACP's session compaction: a compaction the agent reports is under way until it
    /// says how it ended (claude-agent-acp's `_meta.contextCompaction` saying more); Devin's own
    /// `/compact`, which it says nothing else of, for its turn.
    #[test]
    fn compactions_it_reports_and_its_own_compact() {
        let mut core = Core::new();
        core.state.cwd = "/w".into();
        let mut a = Acp::new(None);
        a.start(&mut core);
        let w = written(&mut core);
        assert_eq!(w.first().and_then(|v| v.pointer("/params/clientCapabilities/session/compaction")), Some(&json!({})));
        feed(&mut a, &mut core, &[json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": 1, "agentCapabilities": {}}})]);
        feed(&mut a, &mut core, &[json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "s1"}})]);
        let update = |u: Value| json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": u}});
        let marker = |core: &Core, id: &str| {
            core.events().find(|e| e.id.starts_with(super::super::core::COMPACTION) && e.id.ends_with(id)).and_then(|e| match &e.body {
                Body::Notice { level, text } => Some((*level, text.clone())),
                _ => None,
            })
        };
        feed(&mut a, &mut core, &[update(json!({"sessionUpdate": "compaction_update", "compactionId": "cmp_1", "status": "in_progress"}))]);
        assert!(core.state.compacting.is_some());
        feed(
            &mut a,
            &mut core,
            &[
                update(json!({"sessionUpdate": "compaction_summary_chunk", "compactionId": "cmp_1", "content": {"type": "text", "text": "## Retained"}})),
                update(json!({"sessionUpdate": "compaction_update", "compactionId": "cmp_1", "status": "completed", "_meta": {"contextCompaction": {"version": 1, "trigger": "automatic", "preTokens": 180_000, "postTokens": 22_000, "durationMs": 41_000}}})),
            ],
        );
        assert!(core.state.compacting.is_none());
        assert_eq!(marker(&core, "cmp_1"), Some((Level::Info, "Compacted · 180k → 22k tokens · 41s".to_owned())));
        feed(&mut a, &mut core, &[update(json!({"sessionUpdate": "compaction_update", "compactionId": "cmp_2", "status": "failed", "error": "No model"}))]);
        assert_eq!(marker(&core, "cmp_2"), Some((Level::Warn, "Couldn't compact: No model".to_owned())));
        feed(&mut a, &mut core, &[update(json!({"sessionUpdate": "available_commands_update", "availableCommands": [{"name": "compact", "description": "Force conversation compaction"}]}))]);
        a.command(&mut core, Command::Send { id: "m1".into(), text: "/compact".into(), mode: SendMode::Queue, attachments: vec![], overrides: None, reply_to: None, task: None });
        assert_eq!(core.state.doing.as_deref(), Some(super::super::core::COMPACTING));
        let pid = written(&mut core).iter().find(|v| v.get("method").and_then(Value::as_str) == Some("session/prompt")).and_then(|p| p.get("id").cloned()).unwrap_or_default();
        feed(&mut a, &mut core, &[update(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Context compacted"}})), json!({"jsonrpc": "2.0", "id": pid, "result": {"stopReason": "end_turn"}})]);
        assert!(core.state.compacting.is_none());
        assert_eq!(core.state.doing, None);
    }

    #[test]
    fn the_agents_commands_and_kilns_in_one_catalog() -> anyhow::Result<()> {
        use super::super::model::Route;
        let mut core = Core::new();
        let mut a = Acp::new(None);
        a.start(&mut core);
        assert!(core.meta.commands.iter().any(|c| c.name == "export"));
        feed(
            &mut a,
            &mut core,
            &[json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": {"sessionUpdate": "available_commands_update", "availableCommands": [
                {"name": "init", "description": "Write AGENTS.md", "input": null},
                {"name": "review", "description": "Review changes", "input": {"hint": "what to review"}},
                {"name": "export", "description": "The agent's own export"}
            ]}}})],
        );
        let get = |n: &str| core.meta.commands.iter().find(|c| c.name == n).cloned();
        // The agent's commands are sent as text; its own wins over kiln's of the same name.
        assert!(matches!(get("review"), Some(c) if c.hint == "[what to review]" && c.args == "optional" && matches!(c.route, Route::Prompt { text: None, skill: false })));
        assert!(matches!(get("export").map(|c| c.route), Some(Route::Prompt { .. })));
        assert!(matches!(get("diff").map(|c| c.route), Some(Route::View { view }) if view == "changes"));
        // kiln's export from the timeline.
        a.command(&mut core, Command::Control { id: "export".into(), value: Some(json!({"client": "k1"})) });
        let reply = core.take_out().into_iter().find_map(|u| match u {
            super::super::core::Update::Reply { client, value } => Some((client, value)),
            _ => None,
        });
        assert!(matches!(reply, Some((c, v)) if c == "k1" && v.get("format") == Some(&json!("markdown"))));
        Ok(())
    }

    #[test]
    fn kiln_bypass_answers_every_request_itself() -> anyhow::Result<()> {
        let mut core = Core::new();
        core.state.cwd = "/w".into();
        let mut a = Acp::new(None);
        a.start(&mut core);
        let _ = written(&mut core);
        feed(&mut a, &mut core, &[json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": 1, "agentCapabilities": {}}})]);
        let _ = written(&mut core);
        // Its modes all keep their checks: kiln's own Bypass joins them.
        feed(&mut a, &mut core, &[json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "s1", "modes": {"currentModeId": "build", "availableModes": [{"id": "build", "name": "Build"}, {"id": "plan", "name": "Plan"}]}}})]);
        assert!(core.meta.modes.iter().any(|m| m.id == KILN_BYPASS && m.dangerous));
        // Asked while it asks you: it waits for you. Turning Bypass on lets it go ahead.
        let ask = |id: u64, tool: &str| json!({"jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": {"sessionId": "s1", "toolCall": {"toolCallId": tool, "kind": "execute", "title": "Run", "rawInput": {"command": "ls"}}, "options": [{"optionId": "y", "name": "Allow", "kind": "allow_once"}, {"optionId": "n", "name": "Reject", "kind": "reject_once"}]}});
        feed(&mut a, &mut core, &[ask(7, "t1")]);
        assert_eq!(core.state.approvals.len(), 1);
        let _ = written(&mut core);
        a.command(&mut core, Command::SetMode { mode: KILN_BYPASS.into() });
        assert_eq!(core.state.mode, KILN_BYPASS);
        let w = written(&mut core);
        assert!(!w.iter().any(|v| v.get("method").and_then(Value::as_str) == Some("session/set_mode")));
        assert_eq!(w.first().and_then(|v| v.pointer("/result/outcome/optionId")).and_then(Value::as_str), Some("y"));
        assert!(core.state.approvals.is_empty());
        // From then on kiln answers at once.
        feed(&mut a, &mut core, &[ask(8, "t2")]);
        assert!(core.state.approvals.is_empty());
        assert_eq!(written(&mut core).first().and_then(|v| v.pointer("/result/outcome/optionId")).and_then(Value::as_str), Some("y"));
        // One of the agent's own modes: it asks again.
        a.command(&mut core, Command::SetMode { mode: "build".into() });
        assert!(written(&mut core).iter().any(|v| v.get("method").and_then(Value::as_str) == Some("session/set_mode")));
        feed(&mut a, &mut core, &[ask(9, "t3")]);
        assert_eq!(core.state.approvals.len(), 1);
        // An agent with no modes of its own: Ask, or kiln's Bypass.
        let mut core = Core::new();
        core.state.cwd = "/w".into();
        let mut a = Acp::new(None).with_mode(Some(KILN_BYPASS));
        a.start(&mut core);
        feed(&mut a, &mut core, &[json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": 1, "agentCapabilities": {}}})]);
        feed(&mut a, &mut core, &[json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "s2"}})]);
        assert_eq!(core.meta.modes.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), [KILN_ASK, KILN_BYPASS]);
        assert_eq!(core.state.mode, KILN_BYPASS);
        Ok(())
    }

    #[test]
    fn a_session_that_went_on_here_gives_its_conversation_with_its_first_message_once() -> anyhow::Result<()> {
        let mut core = Core::new();
        core.state.cwd = "/w".into();
        let mut a = Acp::new(None);
        a.offer_context(Some("<kiln_handoff>earlier</kiln_handoff>".into()));
        // Through a re-execution (an in-place upgrade) it still has it.
        let mut a: Acp = serde_json::from_str(&serde_json::to_string(&a)?)?;
        a.start(&mut core);
        feed(&mut a, &mut core, &[json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": 1, "agentCapabilities": {}}})]);
        feed(&mut a, &mut core, &[json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "s1"}})]);
        let _ = written(&mut core);
        a.command(&mut core, Command::Send { id: "m1".into(), text: "go on".into(), mode: SendMode::Queue, attachments: vec![], overrides: None, reply_to: None, task: None});
        let w = written(&mut core);
        let prompt = w.iter().find(|v| v.get("method").and_then(Value::as_str) == Some("session/prompt")).cloned().unwrap_or_default();
        assert_eq!(prompt.pointer("/params/prompt/0/text").and_then(Value::as_str), Some("<kiln_handoff>earlier</kiln_handoff>"));
        assert_eq!(prompt.pointer("/params/prompt/1/text").and_then(Value::as_str), Some("go on"));
        // The timeline shows what the person said, not the conversation it went with.
        assert!(core.events().any(|e| matches!(&e.body, Body::User { text, .. } if text == "go on")));
        let pid = prompt.get("id").cloned().unwrap_or_default();
        feed(&mut a, &mut core, &[json!({"jsonrpc": "2.0", "id": pid, "result": {"stopReason": "end_turn"}})]);
        a.command(&mut core, Command::Send { id: "m2".into(), text: "and then".into(), mode: SendMode::Queue, attachments: vec![], overrides: None, reply_to: None, task: None});
        let w = written(&mut core);
        let prompt = w.iter().find(|v| v.get("method").and_then(Value::as_str) == Some("session/prompt")).cloned().unwrap_or_default();
        assert_eq!(prompt.pointer("/params/prompt/0/text").and_then(Value::as_str), Some("and then"), "only the first message carries it");
        Ok(())
    }

    /// The harness started again (after a crash, or on another machine after a move) says new
    /// things under new ids: what it said before stays in the timeline, rather than its first
    /// answers there taking the ids of the first ones here.
    #[test]
    fn a_harness_started_again_keeps_what_was_said_before() -> anyhow::Result<()> {
        let mut core = Core::new();
        core.state.cwd = "/w".into();
        for (n, answer) in [(1, "the first answer"), (2, "the second answer")] {
            let mut a = Acp::new(None);
            a.start(&mut core);
            feed(&mut a, &mut core, &[json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": 1, "agentCapabilities": {}}})]);
            feed(&mut a, &mut core, &[json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": format!("s{n}")}})]);
            let _ = written(&mut core);
            a.command(&mut core, Command::Send { id: format!("m{n}"), text: format!("question {n}"), mode: SendMode::Queue, attachments: vec![], overrides: None, reply_to: None, task: None});
            let w = written(&mut core);
            let pid = w.iter().find(|v| v.get("method").and_then(Value::as_str) == Some("session/prompt")).and_then(|p| p.get("id").cloned()).unwrap_or_default();
            feed(&mut a, &mut core, &[json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": format!("s{n}"), "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": answer}}}})]);
            feed(&mut a, &mut core, &[json!({"jsonrpc": "2.0", "id": pid, "result": {"stopReason": "end_turn"}})]);
            a.notice(&mut core, Level::Info, format!("run {n}"));
        }
        let answers: Vec<&str> = core.events().filter_map(|e| if let Body::Assistant { text, .. } = &e.body { Some(text.as_str()) } else { None }).collect();
        assert_eq!(answers, ["the first answer", "the second answer"]);
        let notices: Vec<&str> = core.events().filter_map(|e| if let Body::Notice { text, .. } = &e.body { Some(text.as_str()) } else { None }).filter(|t| t.starts_with("run ")).collect();
        assert_eq!(notices, ["run 1", "run 2"]);
        Ok(())
    }

    #[test]
    fn a_session_a_prompt_a_permission_and_the_turn_end() -> anyhow::Result<()> {
        let mut core = Core::new();
        core.state.cwd = "/w".into();
        let mut a = Acp::new(None);
        a.start(&mut core);
        let w = written(&mut core);
        assert_eq!(w.first().and_then(|v| v.pointer("/params/protocolVersion")).and_then(Value::as_u64), Some(1));
        a.command(&mut core, Command::Send { id: "m1".into(), text: "refactor".into(), mode: SendMode::Queue, attachments: vec![], overrides: None, reply_to: None, task: None});
        feed(&mut a, &mut core, &[json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": 1, "agentCapabilities": {"loadSession": true}}})]);
        let w = written(&mut core);
        assert_eq!(w.first().and_then(|v| v.get("method")).and_then(Value::as_str), Some("session/new"));
        feed(&mut a, &mut core, &[json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "s1", "modes": {"currentModeId": "default", "availableModes": [{"id": "default", "name": "Default"}, {"id": "yolo", "name": "YOLO"}]}}})]);
        assert!(core.meta.modes.iter().any(|m| m.id == "yolo" && m.dangerous));
        let w = written(&mut core);
        let prompt = w.iter().find(|v| v.get("method").and_then(Value::as_str) == Some("session/prompt")).cloned().unwrap_or_default();
        assert_eq!(prompt.pointer("/params/prompt/0/text").and_then(Value::as_str), Some("refactor"));
        let pid = prompt.get("id").cloned().unwrap_or_default();
        feed(
            &mut a,
            &mut core,
            &[
                json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": {"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "Let me look."}}}}),
                json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Editing "}}}}),
                json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "main.rs."}}}}),
                json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": {"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Edit main.rs", "kind": "edit", "status": "pending", "locations": [{"path": "/w/main.rs"}], "content": [{"type": "diff", "path": "/w/main.rs", "oldText": "fn a() {}\n", "newText": "fn b() {}\n"}]}}}),
                json!({"jsonrpc": "2.0", "id": 7, "method": "session/request_permission", "params": {"sessionId": "s1", "toolCall": {"toolCallId": "t1"}, "options": [{"optionId": "once", "name": "Allow", "kind": "allow_once"}, {"optionId": "ever", "name": "Always allow edits", "kind": "allow_always"}, {"optionId": "no", "name": "Reject", "kind": "reject_once"}]}}),
            ],
        );
        assert!(matches!(core.events().find(|e| e.id.starts_with("acp-agent_message")).map(|e| e.body.clone()), Some(Body::Assistant { text, streaming: false }) if text == "Editing main.rs."));
        let ap = core.state.approvals.first().cloned().ok_or_else(|| anyhow::anyhow!("no approval"))?;
        assert_eq!((ap.tool, ap.always.len()), (ToolKind::Edit, 1));
        assert_eq!(ap.diffs.first().and_then(|d| d.hunks.first()).map(|h| h.lines.clone()), Some(vec!["-fn a() {}".to_owned(), "+fn b() {}".to_owned()]));
        assert_eq!(core.state.status, Status::Waiting);
        // Always: the allow_always option.
        a.command(&mut core, Command::Approve { id: "7".into(), decision: Decision::Always, message: None, always: Some(0), answers: None, choices: None });
        let w = written(&mut core);
        assert_eq!(w.first().and_then(|v| v.pointer("/result/outcome/optionId")).and_then(Value::as_str), Some("ever"));
        // A second message waits (no steering in ACP).
        a.command(&mut core, Command::Send { id: "m2".into(), text: "then tests".into(), mode: SendMode::Steer, attachments: vec![], overrides: None, reply_to: None, task: None});
        assert_eq!(core.state.queue.len(), 1);
        feed(
            &mut a,
            &mut core,
            &[
                json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": {"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed"}}}),
                json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": {"sessionUpdate": "plan", "entries": [{"content": "edit", "priority": "high", "status": "completed"}, {"content": "test", "priority": "medium", "status": "pending"}]}}}),
                json!({"jsonrpc": "2.0", "id": pid, "result": {"stopReason": "end_turn"}}),
            ],
        );
        assert_eq!(core.state.todos.len(), 2);
        // How the turn ended, for kiln's keep-going.
        assert_eq!(core.state.last_turn.as_ref().map(|t| (t.how.as_str(), t.why.as_deref())), Some(("done", Some("end_turn"))));
        assert!(core.change_jobs.iter().any(|j| j.rel == "main.rs"));
        assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Completed));
        // The queued message went out as the next prompt.
        let w = written(&mut core);
        assert!(w.iter().any(|v| v.pointer("/params/prompt/0/text").and_then(Value::as_str) == Some("then tests")));
        Ok(())
    }

    #[test]
    fn resumed_turns_stay_separate_and_a_kept_replay_is_not_appended_again() {
        let mut core = Core::new();
        let replay = [
            json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "first question"}}),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "first answer"}}),
            json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "second question"}}),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "second answer"}}),
        ];
        for _ in 0..2 {
            let mut a = Acp::new(Some("saved".into()));
            a.start(&mut core);
            feed(&mut a, &mut core, &[json!({"id": 1, "result": {"agentCapabilities": {"loadSession": true}}})]);
            for update in &replay {
                a.update(&mut core, update);
            }
            feed(&mut a, &mut core, &[json!({"id": 2, "result": null})]);
            let messages: Vec<&str> = core.events().filter_map(|e| match &e.body { Body::User { text, .. } | Body::Assistant { text, .. } => Some(text.as_str()), _ => None }).collect();
            assert_eq!(messages, ["first question", "first answer", "second question", "second answer"]);
            assert!(a.ready);
        }
    }

    #[test]
    fn context_updates_replace_occupancy_and_prompt_usage_is_counted_once() {
        let mut core = Core::new();
        let mut a = Acp::new(None);
        a.start(&mut core);
        feed(&mut a, &mut core, &[json!({"id": 1, "result": {}}), json!({"id": 2, "result": {"sessionId": "s1"}})]);
        a.command(&mut core, Command::Send { id: "m1".into(), text: "hello".into(), mode: SendMode::Queue, attachments: vec![], overrides: None, reply_to: None, task: None});
        for _ in 0..2 {
            a.update(&mut core, &json!({"sessionUpdate": "usage_update", "used": 120, "size": 2000, "cost": {"amount": 2.5, "currency": "USD"}, "_meta": {"cognition.ai/inputTokens": 100}}));
        }
        assert_eq!(core.state.usage.context_tokens, Some(120));
        assert_eq!(core.state.usage.input_tokens, 0);
        let response = json!({"id": 3, "result": {"stopReason": "end_turn", "usage": {"inputTokens": 100, "outputTokens": 20, "cachedReadTokens": 10}}});
        feed(&mut a, &mut core, &[response.clone(), response]);
        assert_eq!((core.state.usage.input_tokens, core.state.usage.output_tokens, core.state.usage.cache_read_tokens, core.state.usage.turns), (100, 20, 10, 1));
        a.update(&mut core, &json!({"sessionUpdate": "usage_update", "used": 80, "size": 2000, "cost": {"amount": 999, "currency": "credits"}}));
        assert_eq!(core.state.usage.context_tokens, Some(80));
        assert_eq!(core.state.usage.cost_usd, 2.5);
    }

    #[test]
    fn rejected_calls_keep_their_denial_and_embedded_previews_are_readable() {
        let mut call = merge(None, &json!({"toolCallId": "t1", "kind": "execute", "title": "Run", "content": [{"type": "content", "content": {"type": "resource", "resource": {"uri": "tool://preview", "mimeType": "text/x-shellscript", "text": "printf hello"}}}]}), "/w");
        assert_eq!(call.output.as_deref(), Some("printf hello"));
        call.status = ToolStatus::Denied;
        let call = merge(Some(call), &json!({"toolCallId": "t1", "status": "failed"}), "/w");
        assert_eq!(call.status, ToolStatus::Denied);
        assert!(call.ended.is_some());
        let call = merge(Some(call), &json!({"toolCallId": "t1", "rawOutput": {"stdout": "the actual result"}}), "/w");
        assert_eq!(call.output.as_deref(), Some("the actual result"));
    }

    #[test]
    fn cancel_answers_pending_permissions_and_send_now_goes_next() {
        let mut core = Core::new();
        let mut a = Acp::new(Some("old".into()));
        a.start(&mut core);
        core.take_writes();
        feed(&mut a, &mut core, &[json!({"jsonrpc": "2.0", "id": 1, "result": {"agentCapabilities": {"loadSession": true}}})]);
        let w = written(&mut core);
        assert_eq!(w.first().and_then(|v| v.get("method")).and_then(Value::as_str), Some("session/load"));
        // History replays as updates, then the load answers.
        feed(
            &mut a,
            &mut core,
            &[
                json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "old", "update": {"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "earlier"}}}}),
                json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "old", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "earlier reply"}}}}),
                json!({"jsonrpc": "2.0", "id": 2, "result": null}),
            ],
        );
        assert_eq!(core.state.harness_session.as_deref(), Some("old"));
        assert!(core.events().any(|e| matches!(&e.body, Body::User { text, .. } if text == "earlier")));
        a.command(&mut core, Command::Send { id: "m1".into(), text: "long task".into(), mode: SendMode::Queue, attachments: vec![], overrides: None, reply_to: None, task: None});
        core.take_writes();
        feed(&mut a, &mut core, &[json!({"jsonrpc": "2.0", "id": 9, "method": "session/request_permission", "params": {"sessionId": "old", "toolCall": {"toolCallId": "t9", "kind": "execute", "title": "Run", "rawInput": {"command": "rm -rf x"}}, "options": [{"optionId": "y", "name": "Allow", "kind": "allow_once"}]}})]);
        assert_eq!(core.state.approvals.first().and_then(|a| a.command.clone()).as_deref(), Some("rm -rf x"));
        a.command(&mut core, Command::Send { id: "m2".into(), text: "stop, do this".into(), mode: SendMode::Now, attachments: vec![], overrides: None, reply_to: None, task: None});
        let w = written(&mut core);
        assert!(w.iter().any(|v| v.get("method").and_then(Value::as_str) == Some("session/cancel")));
        assert!(w.iter().any(|v| v.pointer("/result/outcome/outcome").and_then(Value::as_str) == Some("cancelled")));
        assert!(core.state.approvals.is_empty());
        feed(&mut a, &mut core, &[json!({"jsonrpc": "2.0", "id": 3, "result": {"stopReason": "cancelled"}})]);
        assert_eq!(core.state.last_turn.as_ref().map(|t| t.how.as_str()), Some("stopped"), "you stopped it");
        let w = written(&mut core);
        assert!(w.iter().any(|v| v.pointer("/params/prompt/0/text").and_then(Value::as_str) == Some("stop, do this")));
        assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Cancelled));
    }

    fn steering_session(capability: Option<Value>, devin: bool) -> (Acp, Core) {
        let mut core = Core::new();
        core.state.cwd = "/w".into();
        let mut a = Acp::from_spec(&super::super::session::Spec { kind: if devin { "devin" } else { "gemini" }.into(), ..Default::default() });
        a.start(&mut core);
        let mut capabilities = serde_json::Map::new();
        if let Some(value) = capability {
            capabilities.insert("_meta".into(), json!({"cognition.ai/midTurnPrompts": value}));
        }
        feed(&mut a, &mut core, &[json!({"id": 1, "result": {"agentCapabilities": capabilities}}), json!({"id": 2, "result": {"sessionId": "s1"}})]);
        core.take_writes();
        (a, core)
    }

    fn message(a: &mut Acp, core: &mut Core, id: &str, mode: SendMode) {
        a.command(core, Command::Send { id: id.into(), text: id.into(), mode, attachments: vec![], overrides: None, reply_to: None, task: None});
    }

    #[test]
    fn steering_requires_devins_explicit_mid_turn_capability() {
        for (capability, devin) in [(None, true), (Some(json!(false)), true), (Some(json!("true")), true), (Some(json!(true)), false)] {
            let (mut a, mut core) = steering_session(capability, devin);
            assert!(!core.meta.features.iter().any(|f| matches!(f.as_str(), "steer" | "queue_steer")));
            message(&mut a, &mut core, "main", SendMode::Queue);
            core.take_writes();
            message(&mut a, &mut core, "later", SendMode::Steer);
            a.command(&mut core, Command::QueueSteer { id: "later".into() });
            assert_eq!(core.state.queue.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["later"]);
            assert!(written(&mut core).is_empty());
        }
    }

    #[test]
    fn devin_steers_and_promotes_queued_work_without_ending_the_shared_turn_twice() -> anyhow::Result<()> {
        for replies in [[4, 3, 5], [3, 5, 4]] {
            let (mut a, mut core) = steering_session(Some(json!(true)), true);
            assert!(core.meta.features.iter().any(|f| f == "steer"));
            assert!(core.meta.features.iter().any(|f| f == "queue_steer"));
            message(&mut a, &mut core, "main", SendMode::Queue);
            let started = core.state.turn_started;
            message(&mut a, &mut core, "queued-steer", SendMode::Queue);
            message(&mut a, &mut core, "steer", SendMode::Steer);
            a.command(&mut core, Command::QueueSteer { id: "queued-steer".into() });
            a.command(&mut core, Command::QueueSteer { id: "absent".into() });
            message(&mut a, &mut core, "later", SendMode::Queue);
            let writes = written(&mut core);
            assert_eq!(writes.len(), 3);
            assert!(writes.iter().all(|v| v.get("method").and_then(Value::as_str) == Some("session/prompt") && v.pointer("/params/sessionId").and_then(Value::as_str) == Some("s1")));
            assert_eq!(writes.iter().filter_map(|v| v.pointer("/params/prompt/0/text").and_then(Value::as_str)).collect::<Vec<_>>(), ["main", "steer", "queued-steer"]);
            assert_eq!(a.prompting.as_deref(), Some("main"));
            assert_eq!(core.state.turn_started, started);
            for id in ["main", "steer", "queued-steer"] {
                assert_eq!(core.state.delivery.get(id), Some(&Delivery::Started));
                assert_eq!(core.events().filter(|e| matches!(&e.body, Body::User { client_id: Some(client), .. } if client == id)).count(), 1);
            }
            a.update(&mut core, &json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "answer"}}));
            for (i, id) in replies.into_iter().enumerate() {
                feed(&mut a, &mut core, &[json!({"id": id, "result": {"stopReason": "end_turn", "usage": {"inputTokens": 100, "outputTokens": 20, "cachedReadTokens": 30}}})]);
                if i < 2 {
                    assert_eq!(core.state.status, Status::Running);
                    assert_eq!(core.state.turn_started, started);
                    assert_eq!(core.state.usage.turns, 0);
                    assert!(core.state.last_turn.is_none());
                    assert_eq!(core.state.queue.len(), 1);
                    assert!(written(&mut core).is_empty());
                    assert!(core.events().any(|e| matches!(&e.body, Body::Assistant { streaming: true, .. })));
                    a = serde_json::from_value(serde_json::to_value(&a)?)?;
                }
            }
            assert_eq!((core.state.usage.turns, core.state.usage.input_tokens, core.state.usage.output_tokens, core.state.usage.cache_read_tokens), (1, 100, 20, 30));
            assert_eq!(core.state.last_turn.as_ref().map(|t| t.how.as_str()), Some("done"));
            assert!(core.state.queue.is_empty());
            assert_eq!(a.prompting.as_deref(), Some("later"));
            assert_eq!(written(&mut core).iter().filter_map(|v| v.pointer("/params/prompt/0/text").and_then(Value::as_str)).collect::<Vec<_>>(), ["later"]);
            for id in ["main", "steer", "queued-steer"] {
                assert_eq!(core.state.delivery.get(id), Some(&Delivery::Completed));
            }
        }
        Ok(())
    }

    #[test]
    fn a_rejected_steer_does_not_cancel_the_original_turn_or_count_its_usage_again() {
        for replies in [[4, 3], [3, 4]] {
            let (mut a, mut core) = steering_session(Some(json!(true)), true);
            message(&mut a, &mut core, "main", SendMode::Queue);
            message(&mut a, &mut core, "steer", SendMode::Steer);
            message(&mut a, &mut core, "later", SendMode::Queue);
            core.take_writes();
            for (i, id) in replies.into_iter().enumerate() {
                let reply = if id == 4 { json!({"id": id, "error": {"code": -32602, "message": "Steering rejected"}}) } else { json!({"id": id, "result": {"stopReason": "end_turn", "usage": {"inputTokens": 12}}}) };
                feed(&mut a, &mut core, &[reply]);
                if i == 0 {
                    assert_eq!(core.state.status, Status::Running);
                    assert_eq!(core.state.usage.turns, 0);
                    assert_eq!(core.state.queue.len(), 1);
                    assert!(written(&mut core).is_empty());
                }
            }
            assert_eq!(core.state.delivery.get("main"), Some(&Delivery::Completed));
            assert_eq!(core.state.delivery.get("steer"), Some(&Delivery::Cancelled));
            assert!(core.events().any(|e| matches!(&e.body, Body::Notice { level: Level::Error, text } if text == "Steering rejected")));
            assert_eq!((core.state.usage.turns, core.state.usage.input_tokens), (1, 12));
            assert_eq!(a.prompting.as_deref(), Some("later"));
        }
    }

    #[test]
    fn steering_after_the_original_reply_starts_a_separate_turn_after_outstanding_replies() {
        let (mut a, mut core) = steering_session(Some(json!(true)), true);
        message(&mut a, &mut core, "main", SendMode::Queue);
        message(&mut a, &mut core, "steer", SendMode::Steer);
        core.take_writes();
        feed(&mut a, &mut core, &[json!({"id": 3, "result": {"stopReason": "end_turn", "usage": {"inputTokens": 12}}})]);
        message(&mut a, &mut core, "next", SendMode::Steer);
        assert_eq!(core.state.queue.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["next"]);
        assert!(written(&mut core).is_empty());
        feed(&mut a, &mut core, &[json!({"id": 4, "result": {"stopReason": "end_turn", "usage": {"inputTokens": 12}}})]);
        assert_eq!(a.prompting.as_deref(), Some("next"));
        assert_eq!(written(&mut core).iter().filter_map(|v| v.pointer("/params/prompt/0/text").and_then(Value::as_str)).collect::<Vec<_>>(), ["next"]);
        feed(&mut a, &mut core, &[json!({"id": 5, "result": {"stopReason": "end_turn", "usage": {"inputTokens": 20}}})]);
        assert_eq!((core.state.usage.turns, core.state.usage.input_tokens), (2, 32));
        assert_eq!(core.state.status, Status::Idle);
    }

    #[test]
    fn cancelling_devins_shared_turn_waits_for_every_prompt_before_sending_now() {
        let (mut a, mut core) = steering_session(Some(json!(true)), true);
        message(&mut a, &mut core, "main", SendMode::Queue);
        message(&mut a, &mut core, "steer", SendMode::Steer);
        core.take_writes();
        message(&mut a, &mut core, "now", SendMode::Now);
        assert_eq!(written(&mut core).iter().filter_map(|v| v.get("method").and_then(Value::as_str)).collect::<Vec<_>>(), ["session/cancel"]);
        for (i, id) in [3, 4].into_iter().enumerate() {
            feed(&mut a, &mut core, &[json!({"id": id, "result": {"stopReason": "cancelled", "usage": {"inputTokens": 12}}})]);
            if i == 0 {
                assert_eq!(core.state.status, Status::Running);
                assert!(written(&mut core).is_empty());
            }
        }
        assert_eq!(core.state.delivery.get("main"), Some(&Delivery::Cancelled));
        assert_eq!(core.state.delivery.get("steer"), Some(&Delivery::Cancelled));
        assert_eq!((core.state.usage.turns, core.state.usage.input_tokens), (1, 12));
        assert_eq!(core.state.last_turn.as_ref().map(|t| t.how.as_str()), Some("stopped"));
        assert_eq!(a.prompting.as_deref(), Some("now"));
        assert_eq!(written(&mut core).iter().filter_map(|v| v.pointer("/params/prompt/0/text").and_then(Value::as_str)).collect::<Vec<_>>(), ["now"]);
    }

    #[test]
    fn adopted_devin_sessions_learn_steering_once_without_reopening_or_interrupting() -> anyhow::Result<()> {
        for busy in [false, true] {
            let (mut a, mut core) = steering_session(None, true);
            if busy {
                message(&mut a, &mut core, "main", SendMode::Queue);
            }
            core.take_writes();
            let mut frozen = serde_json::to_value(&a)?;
            let object = frozen.as_object_mut().ok_or_else(|| anyhow::anyhow!("the frozen adapter is not an object"))?;
            object.remove("steering");
            object.remove("prompt_result");
            let mut a: Acp = serde_json::from_value(frozen)?;
            let started = core.state.turn_started;
            a.tick(&mut core);
            a.tick(&mut core);
            let writes = written(&mut core);
            assert_eq!(writes.len(), 1);
            let request = writes.first().ok_or_else(|| anyhow::anyhow!("no capabilities request"))?;
            assert_eq!(request.get("method").and_then(Value::as_str), Some("initialize"));
            assert_eq!(a.steering, Some(false));
            let id = request.get("id").cloned().ok_or_else(|| anyhow::anyhow!("no capabilities request id"))?;
            feed(&mut a, &mut core, &[json!({"id": id, "result": {"agentCapabilities": {"_meta": {"cognition.ai/midTurnPrompts": true}}}})]);
            assert!(a.ready);
            assert_eq!(a.session.as_deref(), Some("s1"));
            assert_eq!(a.prompting.as_deref(), busy.then_some("main"));
            assert_eq!(core.state.turn_started, started);
            assert_eq!(a.steering, Some(true));
            assert!(core.meta.features.iter().any(|f| f == "steer"));
            a.tick(&mut core);
            assert!(written(&mut core).is_empty());
        }
        let (mut a, mut core) = steering_session(None, true);
        a.steering = None;
        a.tick(&mut core);
        let id = written(&mut core).first().and_then(|v| v.get("id")).cloned().ok_or_else(|| anyhow::anyhow!("no capabilities request id"))?;
        feed(&mut a, &mut core, &[json!({"id": id, "error": {"code": -32601, "message": "Unsupported"}})]);
        a.tick(&mut core);
        assert_eq!(a.steering, Some(false));
        assert!(a.ready);
        assert!(written(&mut core).is_empty());
        assert!(!core.meta.features.iter().any(|f| f == "steer"));
        Ok(())
    }
}
