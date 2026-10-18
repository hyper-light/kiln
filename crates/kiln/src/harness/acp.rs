//! Harnesses that speak the Agent Client Protocol (ACP: JSON-RPC 2.0 over stdio, as Zed and
//! other editors drive agents): Gemini CLI (`gemini --acp`), opencode (`opencode acp`), Goose
//! (`goose acp`), Amp through its ACP adapter, and any other ACP agent. kiln is the client: it
//! opens (or loads) a session, sends prompts, streams the agent's messages, thoughts, tool calls
//! (with diffs) and plan, answers its permission requests, switches modes and cancels turns.
//!
//! kiln advertises no file-system or terminal capabilities, so agents use their own tools.
//! ACP has no way into a running turn: a "steer" waits for the turn like a queued message.
//!
//! Written against the ACP specification (protocol version 1); not yet run against a real
//! install of any of these harnesses (they are marked unverified).

use std::collections::HashMap;

use serde_json::{Value, json};

use super::core::{Baseline, Core};
use super::model::{
    AlwaysOption, Approval, Body, Command, Decision, Delivery, ExitInfo, FileDiff, Level, ModeOption, ModelOption, QueueItem, SendMode,
    Inventory, Known, SlashCommand, Status, StatusItem, Todo, ToolCall, ToolKind, ToolStatus, OUTPUT_MAX, clip_json, clip_middle, sort_catalog,
};

/// kiln's own commands for an ACP agent.
fn acp_extras() -> Vec<SlashCommand> {
    vec![
        SlashCommand::control("export", "The conversation as Markdown", "", "Session", json!({"type": "control", "id": "export", "value": {"client": "{client}"}})),
        SlashCommand::view("status", "The agent and what it can do", "Session", "status"),
        SlashCommand::view("model", "The model and mode", "Model & mode", "model"),
        SlashCommand::view("diff", "What this agent changed", "Context", "changes"),
        SlashCommand::control("restart", "Start the agent again", "", "Kiln", json!({"type": "restart"})),
        SlashCommand::view("new", "Start another agent", "Kiln", "new_agent"),
        SlashCommand::view("terminal", "A shell in this agent's folder, on its machine", "Kiln", "terminal").aka(&["shell"]),
        SlashCommand::view("screen", "The agent's own terminal interface, in its folder", "Kiln", "screen"),
        SlashCommand::view("goals", "Goals one after another: each sent once the one before is done", "Kiln", "goals"),
    ]
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

#[derive(Debug, Clone)]
enum Pending {
    Initialize,
    Session,
    Prompt(String),
    SetMode(String),
    SetModel(String),
}

#[derive(Debug, Clone)]
struct Ask {
    rpc: Value,
    /// (option id, kind) as the agent offered them.
    options: Vec<(String, String)>,
}

pub struct Acp {
    next_id: u64,
    pending: HashMap<u64, Pending>,
    asks: HashMap<String, Ask>,
    session: Option<String>,
    resume: Option<String>,
    /// The prompt in flight (its client id).
    prompting: Option<String>,
    /// Messages to send once the session exists or the turn has been cancelled.
    waiting: Vec<QueueItem>,
    can_load: bool,
    /// The agent takes images in prompts.
    images: bool,
    /// The event being streamed into, per kind of chunk.
    cur_agent: Option<String>,
    cur_thought: Option<String>,
    cur_user: Option<String>,
    seq: u64,
    notice_seq: u64,
    /// The files each tool call changes, with their text before it (from its diff content).
    befores: HashMap<String, Vec<(String, Option<String>)>>,
    /// kiln's Bypass: every request to go ahead is answered yes by kiln, at once.
    bypass: bool,
}

/// kiln's own permission modes, for agents whose own all keep their checks: kiln asks you, or
/// kiln answers every request to go ahead itself.
pub const KILN_ASK: &str = "kiln-ask";
pub const KILN_BYPASS: &str = "kiln-bypass";

fn kiln_ask() -> ModeOption {
    ModeOption { id: KILN_ASK.into(), label: "Ask".into(), description: "Ask before anything the agent asks to do".into(), dangerous: false }
}

fn kiln_bypass() -> ModeOption {
    ModeOption { id: KILN_BYPASS.into(), label: "Bypass".into(), description: "kiln approves everything the agent asks to do".into(), dangerous: true }
}

impl Acp {
    pub fn new(resume: Option<String>) -> Self {
        Self {
            next_id: 0,
            pending: HashMap::new(),
            asks: HashMap::new(),
            session: None,
            resume,
            prompting: None,
            waiting: Vec::new(),
            can_load: false,
            images: false,
            cur_agent: None,
            cur_thought: None,
            cur_user: None,
            seq: 0,
            notice_seq: 0,
            befores: HashMap::new(),
            bypass: false,
        }
    }

    /// Starts in this mode: kiln's Bypass goes on at once (the agent's own modes it is told of).
    pub fn with_mode(mut self, mode: Option<&str>) -> Self {
        self.bypass = mode == Some(KILN_BYPASS);
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
        core.upsert(&format!("notice-{}", self.notice_seq), None, Body::Notice { level, text: text.into() });
    }

    fn busy(&self, core: &Core) -> bool {
        self.prompting.is_some() || !core.state.approvals.is_empty()
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
        core.meta.features = ["send_now", "queue", "interrupt", "set_mode", "resume", "attachments"].iter().map(|s| (*s).to_owned()).collect();
        core.touch_meta();
        // kiln's commands until the agent lists its own.
        if core.meta.commands.is_empty() {
            core.meta.commands = sort_catalog(acp_extras());
        }
        // ACP has no way to report MCP servers, skills, plugins or plan limits.
        core.state.inventory = Inventory { mcp: Known::Unsupported, skills: Known::Unsupported, plugins: Known::Unsupported, limits: Known::Unsupported };
        core.touch_state();
        self.request(
            core,
            Pending::Initialize,
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
                "clientInfo": {"name": "kiln", "version": env!("CARGO_PKG_VERSION")},
            }),
        );
    }

    pub fn exited(&mut self, core: &mut Core, code: Option<i32>, error: Option<String>) {
        self.close_streams(core);
        self.prompting = None;
        self.pending.clear();
        self.asks.clear();
        core.state.approvals.clear();
        core.state.exit = Some(ExitInfo { code, error });
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
            // ACP cannot steer a turn: it stays queued.
            Command::QueueSteer { .. } => {}
            Command::Approve { id, decision, message, always, .. } => self.answer(core, &id, decision, message, always),
            Command::Interrupt => self.cancel(core),
            Command::SetMode { mode } => {
                if mode == KILN_BYPASS || mode == KILN_ASK {
                    // kiln's own: the agent's mode stays as it is.
                    self.bypass = mode == KILN_BYPASS;
                    core.state.mode = mode;
                    core.touch_state();
                    if self.bypass {
                        // What waits for you now goes ahead too.
                        let waiting: Vec<String> = self.asks.keys().cloned().collect();
                        for id in waiting {
                            self.answer(core, &id, Decision::Allow, None, None);
                        }
                    }
                } else if let Some(s) = self.session.clone() {
                    self.bypass = false;
                    self.request(core, Pending::SetMode(mode.clone()), "session/set_mode", json!({"sessionId": s, "modeId": mode}));
                }
            }
            Command::SetModel { model, .. } => {
                if let Some(s) = self.session.clone() {
                    self.request(core, Pending::SetModel(model.clone()), "session/set_model", json!({"sessionId": s, "modelId": model}));
                }
            }
            Command::Suggest { client, .. } => core.reply(client, json!({"suggestions": []})),
            // ACP has no effort setting and no other controls than modes and models; kiln exports
            // the conversation from its timeline.
            Command::Control { id, value } if id == "export" => {
                let client = value.as_ref().and_then(|v| v.get("client")).and_then(Value::as_str).unwrap_or("").to_owned();
                core.reply(client, json!({"title": "Conversation", "text": core.export(), "format": "markdown", "filename": "conversation.md"}));
            }
            Command::SetEffort { .. } | Command::Control { .. } => {}
            Command::StopTask { .. } | Command::StopTasks | Command::Restart | Command::ForceStop | Command::Snapshot | Command::MoveTo { .. } | Command::Moving { .. } => {}
            // A pause's commands are the session's (`harness::pause`).
            Command::Resume | Command::AutoResume { .. } | Command::PauseAt { .. } | Command::LimitHold { .. } => {}
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
                self.send(core, QueueItem { id: id.unwrap_or_else(crate::util::new_id), text, at: Core::now(), attachments: Vec::new(), overrides: None }, mode);
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
        if self.session.is_none() {
            self.show(core, &m, Delivery::Queued);
            self.waiting.push(m);
            return;
        }
        if self.busy(core) {
            if mode == SendMode::Now {
                self.show(core, &m, Delivery::Queued);
                self.waiting.insert(0, m);
                self.cancel(core);
            } else {
                // No way into a running ACP turn: steering waits for it too.
                core.state.queue.push(m);
                core.touch_state();
            }
            return;
        }
        self.prompt(core, m);
    }

    fn prompt(&mut self, core: &mut Core, m: QueueItem) {
        let Some(s) = self.session.clone() else { return };
        self.close_streams(core);
        // Images inline where the agent takes them; everything else as a link to the file.
        let mut blocks = Vec::new();
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
        self.prompting = Some(m.id.clone());
        self.show(core, &m, Delivery::Started);
        core.state.turn_started = Some(Core::now());
        core.state.doing = Some("Thinking".to_owned());
        self.settle(core);
    }

    fn show(&mut self, core: &mut Core, m: &QueueItem, d: Delivery) {
        core.state.delivery.insert(m.id.clone(), d);
        let (images, names) = super::model::attachment_summary(&m.attachments);
        core.note_sent(&m.id);
        core.upsert(&m.id, None, Body::User { text: m.text.clone(), images, client_id: Some(m.id.clone()), origin: Some(super::core::origin_of(&m.id).into()), attachments: names });
        core.touch_state();
    }

    fn next(&mut self, core: &mut Core) {
        if self.busy(core) || self.session.is_none() || core.state.status == Status::Exited {
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
            self.send(core, QueueItem { id: id.clone(), text: super::pause::CONTINUE.to_owned(), at: Core::now(), attachments: Vec::new(), overrides: None }, SendMode::Queue);
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
            let m = QueueItem { id: crate::util::new_id(), text: f, at: Core::now(), attachments: Vec::new(), overrides: None };
            self.show(core, &m, Delivery::Queued);
            self.waiting.insert(0, m);
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
        let result = v.get("result").cloned().unwrap_or(Value::Null);
        if let Some(err) = v.get("error") {
            let msg = err.get("message").and_then(Value::as_str).unwrap_or("request failed").to_owned();
            match kind {
                Pending::Prompt(id) => {
                    self.prompting = None;
                    core.state.delivery.insert(id, Delivery::Cancelled);
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
                Pending::Initialize => self.notice(core, Level::Error, format!("The agent did not start: {msg}")),
                Pending::SetMode(_) | Pending::SetModel(_) => self.notice(core, Level::Warn, msg),
            }
            return;
        }
        match kind {
            Pending::Initialize => {
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
                        self.request(core, Pending::Session, "session/load", json!({"sessionId": sid, "cwd": cwd, "mcpServers": []}));
                    }
                    None => {
                        if self.resume.is_some() {
                            self.notice(core, Level::Warn, "This agent cannot load earlier sessions; starting a new one.");
                        }
                        self.request(core, Pending::Session, "session/new", json!({"cwd": cwd, "mcpServers": []}));
                    }
                }
            }
            Pending::Session => {
                self.close_streams(core);
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
                                    Some(ModelOption { label: m.get("name").and_then(Value::as_str).unwrap_or(&id).to_owned(), description: m.get("description").and_then(Value::as_str).unwrap_or("").to_owned(), id, efforts: Vec::new() })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    if let Some(c) = models.get("currentModelId").and_then(Value::as_str) {
                        core.state.model = c.to_owned();
                    }
                    core.touch_meta();
                }
                core.set_status(Status::Idle);
                core.touch_state();
                self.next(core);
            }
            Pending::Prompt(id) => {
                self.close_streams(core);
                self.prompting = None;
                let stop = result.get("stopReason").and_then(Value::as_str).unwrap_or("end_turn");
                core.state.delivery.insert(id, if stop == "cancelled" { Delivery::Cancelled } else { Delivery::Completed });
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
                core.state.mode = m;
                core.touch_state();
            }
            Pending::SetModel(m) => {
                core.state.model = m;
                core.touch_state();
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
        match u.get("sessionUpdate").and_then(Value::as_str).unwrap_or("") {
            "agent_message_chunk" | "agent_thought_chunk" | "user_message_chunk" => {
                let kind = u.get("sessionUpdate").and_then(Value::as_str).unwrap_or("");
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
                        let id = format!("acp-{kind}-{}", self.seq);
                        let body = match kind {
                            "agent_message_chunk" => Body::Assistant { text: text.clone(), streaming: true },
                            "agent_thought_chunk" => Body::Thinking { text: text.clone(), streaming: true, redacted: false },
                            _ => Body::User { text: text.clone(), images: 0, client_id: None, origin: Some("human".into()), attachments: Vec::new() },
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
                cmds.extend(acp_extras());
                core.meta.commands = sort_catalog(cmds);
                core.touch_meta();
            }
            "current_mode_update" => {
                if let Some(m) = u.get("currentModeId").and_then(Value::as_str) {
                    core.state.mode = m.to_owned();
                    core.touch_state();
                }
            }
            _ => {}
        }
    }
}

fn content_text(c: Option<&Value>) -> String {
    match c {
        Some(Value::Object(o)) if o.get("type").and_then(Value::as_str) == Some("text") => o.get("text").and_then(Value::as_str).unwrap_or("").to_owned(),
        Some(Value::Object(o)) if o.get("type").and_then(Value::as_str) == Some("resource_link") => o.get("uri").and_then(Value::as_str).map(|u| format!("[{u}]")).unwrap_or_default(),
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
            "failed" => ToolStatus::Error,
            _ => ToolStatus::Pending,
        };
        if matches!(t.status, ToolStatus::Done | ToolStatus::Error) {
            t.ended = Some(Core::now());
        }
    }
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
            let (o, cut) = clip_middle(&text, OUTPUT_MAX);
            t.output = Some(o);
            t.truncated = cut;
        }
    }
    if t.output.is_none()
        && let Some(raw) = u.get("rawOutput")
    {
        let text = raw.get("output").or_else(|| raw.get("stdout")).and_then(Value::as_str).map_or_else(|| raw.to_string(), str::to_owned);
        t.output = Some(clip_middle(&text, OUTPUT_MAX).0);
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
    fn a_session_a_prompt_a_permission_and_the_turn_end() -> anyhow::Result<()> {
        let mut core = Core::new();
        core.state.cwd = "/w".into();
        let mut a = Acp::new(None);
        a.start(&mut core);
        let w = written(&mut core);
        assert_eq!(w.first().and_then(|v| v.pointer("/params/protocolVersion")).and_then(Value::as_u64), Some(1));
        a.command(&mut core, Command::Send { id: "m1".into(), text: "refactor".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
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
        a.command(&mut core, Command::Send { id: "m2".into(), text: "then tests".into(), mode: SendMode::Steer, attachments: vec![], overrides: None });
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
        assert!(core.change_jobs.iter().any(|j| j.rel == "main.rs"));
        assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Completed));
        // The queued message went out as the next prompt.
        let w = written(&mut core);
        assert!(w.iter().any(|v| v.pointer("/params/prompt/0/text").and_then(Value::as_str) == Some("then tests")));
        Ok(())
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
        a.command(&mut core, Command::Send { id: "m1".into(), text: "long task".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        core.take_writes();
        feed(&mut a, &mut core, &[json!({"jsonrpc": "2.0", "id": 9, "method": "session/request_permission", "params": {"sessionId": "old", "toolCall": {"toolCallId": "t9", "kind": "execute", "title": "Run", "rawInput": {"command": "rm -rf x"}}, "options": [{"optionId": "y", "name": "Allow", "kind": "allow_once"}]}})]);
        assert_eq!(core.state.approvals.first().and_then(|a| a.command.clone()).as_deref(), Some("rm -rf x"));
        a.command(&mut core, Command::Send { id: "m2".into(), text: "stop, do this".into(), mode: SendMode::Now, attachments: vec![], overrides: None });
        let w = written(&mut core);
        assert!(w.iter().any(|v| v.get("method").and_then(Value::as_str) == Some("session/cancel")));
        assert!(w.iter().any(|v| v.pointer("/result/outcome/outcome").and_then(Value::as_str) == Some("cancelled")));
        assert!(core.state.approvals.is_empty());
        feed(&mut a, &mut core, &[json!({"jsonrpc": "2.0", "id": 3, "result": {"stopReason": "cancelled"}})]);
        let w = written(&mut core);
        assert!(w.iter().any(|v| v.pointer("/params/prompt/0/text").and_then(Value::as_str) == Some("stop, do this")));
        assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Cancelled));
    }
}
