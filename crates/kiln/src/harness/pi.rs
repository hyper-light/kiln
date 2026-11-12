//! Pi (`pi`, Earendil's coding agent, `@earendil-works/pi-coding-agent`) in its RPC mode: kiln
//! starts `pi --mode rpc` and writes one JSON command a line (`prompt`, `steer`, `abort`,
//! `set_model`, ...); Pi answers each with a `response` under the command's `id` and streams its
//! session's events (`agent_start`, `message_update`, `tool_execution_*`, `agent_settled`) as JSON
//! lines (pi.dev/docs/latest/rpc; docs/rpc.md, docs/rpc-commands.md and docs/json.md in its package).
//!
//! It never asks before a tool call (its docs/security.md: "it does not ask for approval before
//! every tool call"): what it may do is the tools it's started with (`--tools`), so its modes are
//! tool sets, and a new mode starts it again on the same session (`--session-id`). Its model and
//! thinking level change over RPC. A message for a turn under way goes in as Pi's own steering,
//! after the turn's tool calls and before its next model call; a stop is its `abort`, what was
//! steered in and not taken yet coming back to kiln's queue first (`clear_queue`). An extension's
//! dialogs (`extension_ui_request`: select, confirm, input, editor) are a person's to answer.
//!
//! Written against Pi 1.5.2, from its docs and its RPC mode's source (`dist/modes/rpc`).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::core::{Core, Effect};
use super::model::{
    Approval, Body, ChoiceAnswer, Command, ControlOption, Decision, Delivery, ExitInfo, FileDiff, Inventory, Known, Level, ModeOption, ModelOption, Question, QuestionOption, QueueItem, SendMode,
    SlashCommand, Status, StatusItem, Task, ToolCall, ToolKind, ToolStatus, OUTPUT_MAX, choices_of, clip_json, clip_middle, sort_catalog,
};
use super::session::Spec;

/// What a session of it can do, said when it starts (and again after an in-place upgrade).
pub(super) const FEATURES: &[&str] = &["send_now", "queue", "steer", "queue_steer", "interrupt", "set_mode", "set_model", "set_effort", "resume", "attachments", "switch_harness", "own_edits"];
/// What it turns on once it runs: nothing.
pub(super) const LEARNED: &[&str] = &[];

/// Its modes: the tools it's started with. Pi asks before none of them.
pub const READ_ONLY: &str = "read-only";
pub const EDIT: &str = "edit";
pub const FULL: &str = "full";
/// A new session's: it edits files and runs no commands.
pub const DEFAULT_MODE: &str = EDIT;

/// The built-in tools each mode allows (`--tools`, an allowlist); Full access is Pi's own default
/// set. MCP tools stay unless excluded (its docs/cli.md, "Tools"), so the narrower modes exclude
/// them too.
const READ_TOOLS: &str = "read,grep,find,ls";
const EDIT_TOOLS: &str = "read,grep,find,ls,edit,write";
const NO_MCP: &str = "mcp__*";

/// Pi's thinking levels, lowest first (pi-ai's `EXTENDED_THINKING_LEVELS`).
pub const LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max"];
/// What a new session can start at before it says its model's: every reasoning model's levels
/// (xhigh and max only where a model maps them).
pub const EFFORTS: &[&str] = &["off", "minimal", "low", "medium", "high"];

/// What a run of its CLI beside the session is for (`Effect::Run`).
const VERSION_JOB: &str = "pi-version";

pub fn modes() -> Vec<ModeOption> {
    let m = |id: &str, label: &str, description: &str, dangerous: bool| ModeOption { id: id.into(), label: label.into(), description: description.into(), dangerous };
    vec![
        m(READ_ONLY, "Read only", "It reads and searches files; it edits nothing and runs no commands", false),
        m(EDIT, "Edit files", "It reads and edits files; it runs no commands", false),
        m(FULL, "Full access", "Every tool it has, commands included, each run unasked: Pi asks before none", true),
    ]
}

/// The arguments for a session: RPC mode, the session it goes on with (or a new one under an id
/// kiln picks), its model and thinking level, and its mode's tools.
pub fn args(spec: &Spec) -> Vec<String> {
    let mut a: Vec<String> = ["--mode", "rpc"].map(str::to_owned).to_vec();
    let mut flag = |f: &str, v: &str| {
        a.push(f.to_owned());
        a.push(v.to_owned());
    };
    // An existing session of this folder with the id is opened, else one is made under it.
    let session = spec.resume.as_deref().map(str::trim).filter(|s| session_id(s)).map_or_else(|| uuid::Uuid::now_v7().to_string(), str::to_owned);
    flag("--session-id", &session);
    if let Some(m) = spec.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        flag("--model", m);
    }
    if let Some(e) = spec.effort.as_deref().filter(|e| LEVELS.contains(e)) {
        flag("--thinking", e);
    }
    match spec.mode.as_deref().unwrap_or(DEFAULT_MODE) {
        READ_ONLY => {
            flag("--tools", READ_TOOLS);
            flag("--exclude-tools", NO_MCP);
        }
        // Its own tools, unasked, only where it may skip checks.
        FULL if spec.allow_bypass => {}
        _ => {
            flag("--tools", EDIT_TOOLS);
            flag("--exclude-tools", NO_MCP);
        }
    }
    a
}

/// A session id Pi takes (`--session-id`: letters, numbers, `.`, `_` and `-`, starting and ending
/// with a letter or number).
fn session_id(s: &str) -> bool {
    let ends = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric());
    ends(s.chars().next()) && ends(s.chars().last()) && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// kiln's own commands for a Pi session (Pi's own, its extensions', templates and skills, run as
/// "/name" messages).
fn extras() -> Vec<SlashCommand> {
    vec![
        SlashCommand::control("compact", "Summarize the conversation so far, to free context", "[instructions]", "Context", json!({"type": "control", "id": "compact", "value": {"text": "{args}"}})),
        SlashCommand::control("export", "The conversation as Markdown", "", "Session", json!({"type": "control", "id": "export", "value": {"client": "{client}"}})),
        SlashCommand::view("status", "The agent and what it can do", "Session", "status"),
        SlashCommand::view("model", "The model and mode", "Model & mode", "model"),
        SlashCommand::view("login", "Sign in to a model provider on this machine", "Session", "login"),
        SlashCommand::view("logout", "Sign out of the providers kiln signs in to", "Session", "logout"),
        SlashCommand::control("restart", "Start the agent again", "", "Kiln", json!({"type": "restart"})),
        SlashCommand::view("new", "Start another agent", "Kiln", "new_agent"),
        SlashCommand::view("terminal", "A shell in this agent's folder, on its machine", "Kiln", "terminal").aka(&["shell"]),
        SlashCommand::view("screen", "The agent's own terminal interface, in its folder", "Kiln", "screen"),
        SlashCommand::view("goals", "Goals one after another: each sent once the one before is done", "Kiln", "goals"),
    ]
}

/// What it can't do from kiln.
const UNSUPPORTED: &str = "Pi's RPC mode has no way to manage its MCP servers or skills from kiln: do it in its own screen (/screen).";

/// A request kiln made of it, by the id it went under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Req {
    State,
    Models,
    Commands,
    Levels,
    Stats,
    Prompt(String),
    Steer(String),
    Abort,
    Clear,
    SetModel(String),
    SetLevel(String),
    Compact,
}

/// An extension's dialog a person answers (`extension_ui_request`).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Dialog {
    /// Pi's id for it, which the answer goes under.
    request: String,
    /// "select", "confirm", "input" or "editor".
    method: String,
    /// When Pi answers it itself (its `timeout`), Unix ms.
    #[serde(default)]
    until: Option<i64>,
}

/// Plain data, written out whole when its runner hands it to its next image or another runner
/// process of the machine (an upgrade: `crate::harness::kinds::Frozen`).
#[derive(Serialize, Deserialize)]
pub struct Pi {
    /// The session it was started on (`--session-id`), to tell when that one wasn't there.
    resumed: Option<String>,
    /// Its session (from `get_state`).
    session: Option<String>,
    /// `get_state` answered: it takes prompts.
    ready: bool,
    next_id: u64,
    /// What kiln asked it, by the request's id.
    pending: HashMap<String, Req>,
    /// The message whose run is under way (its client id).
    turn: Option<String>,
    /// That message as it was sent, until Pi takes it (a refusal puts it back in kiln's queue whole).
    #[serde(default)]
    item: Option<QueueItem>,
    /// A run is under way (`agent_start` until `agent_settled`), kiln's or an extension's.
    running: bool,
    /// Messages steered into the run under way, in order: the last ones until Pi takes them.
    steered: Vec<QueueItem>,
    /// The streamed reply's blocks on the timeline, by their index in its message.
    blocks: HashMap<u64, String>,
    /// Assistant messages seen, for their blocks' ids.
    message: u64,
    /// Its tool calls on the timeline, by Pi's id for each.
    tools: HashMap<String, String>,
    /// The last error a model call or a retry ended with, in this run.
    error: Option<String>,
    /// This run's tag, in the ids of the events it makes ([`super::core::run_tag`]).
    run: String,
    seq: u64,
    /// A setting it takes when it starts changed (said in words): it starts again once nothing is
    /// under way, the session going on.
    reload: Option<String>,
    /// That start may skip every check (Full access).
    bypass_next: bool,
    /// It may skip them now.
    allow_bypass: bool,
    /// Its process is being ended to start again: what is sent waits for the new one.
    restarting: bool,
    /// The conversation so far, in words, given with the first message it sends (a session that
    /// went on here from another machine, `super::moving`, or from another harness).
    #[serde(default)]
    context: Option<String>,
    /// Its model takes images (`input` of its model); unknown until it says.
    #[serde(default)]
    images: Option<bool>,
    /// An extension's dialogs waiting on a person, by the approval kiln shows for each.
    #[serde(default)]
    asks: HashMap<String, Dialog>,
    /// A compaction asked for during a turn: it runs once the turn ends (its instructions).
    #[serde(default)]
    compact: Option<String>,
}

impl Pi {
    pub(crate) fn retained_input_ids(&self) -> Vec<super::admission::LegacyFlight> {
        self.turn.iter().chain(self.steered.iter().map(|m| &m.id)).cloned().map(|id| super::admission::LegacyFlight { id, protocol: super::admission::FlightProtocol::Pi, request: None }).collect()
    }

    pub fn new(spec: &Spec) -> Self {
        Self {
            resumed: spec.resume.clone().filter(|s| session_id(s.trim())),
            session: None,
            ready: false,
            next_id: 0,
            pending: HashMap::new(),
            turn: None,
            item: None,
            running: false,
            steered: Vec::new(),
            blocks: HashMap::new(),
            message: 0,
            tools: HashMap::new(),
            error: None,
            run: super::core::run_tag(),
            seq: 0,
            reload: None,
            bypass_next: false,
            allow_bypass: spec.allow_bypass,
            restarting: false,
            context: None,
            images: None,
            asks: HashMap::new(),
            compact: None,
        }
    }

    fn notice(&mut self, core: &mut Core, level: Level, text: impl Into<String>) {
        self.seq += 1;
        core.upsert(&format!("pi-notice-{}-{}", self.run, self.seq), None, Body::Notice { level, text: text.into() });
    }

    /// Ask it something; its answer comes under the id this returns.
    fn request(&mut self, core: &mut Core, req: Req, mut command: Value) -> String {
        self.next_id += 1;
        let id = format!("kiln-{}", self.next_id);
        if let Some(o) = command.as_object_mut() {
            o.insert("id".into(), json!(id));
        }
        core.write(&command);
        self.pending.insert(id.clone(), req);
        id
    }

    fn busy(&self, core: &Core) -> bool {
        self.turn.is_some() || self.running || core.state.compacting.is_some() || self.pending.values().any(|r| *r == Req::Compact)
    }

    pub fn start(&mut self, core: &mut Core) {
        core.meta.features = FEATURES.iter().map(|s| (*s).to_owned()).collect();
        core.meta.modes = modes();
        core.meta.efforts = efforts(EFFORTS);
        // Its go-back (rewind, restore files, branch, fork) is kiln's own (`harness::goback`).
        super::goback::declare(core);
        if core.meta.commands.is_empty() {
            core.meta.commands = sort_catalog(extras());
        }
        core.touch_meta();
        if core.state.mode.is_empty() {
            core.state.mode = DEFAULT_MODE.to_owned();
        }
        core.state.inventory = Inventory { mcp: Known::Unsupported, skills: Known::Unsupported, plugins: Known::Unsupported, limits: Known::Unsupported };
        core.touch_state();
        // Its session (which says it's ready), its models and its commands; its version from its CLI.
        self.request(core, Req::State, json!({"type": "get_state"}));
        self.request(core, Req::Models, json!({"type": "get_available_models"}));
        self.request(core, Req::Commands, json!({"type": "get_commands"}));
        core.effects.push(Effect::Run { job: VERSION_JOB.to_owned(), args: vec!["--version".to_owned()] });
    }

    pub fn exited(&mut self, core: &mut Core, code: Option<i32>, error: Option<String>) {
        // Its dialogs went with its process.
        for id in std::mem::take(&mut self.asks).into_keys() {
            core.take_approval(&id);
        }
        self.close_blocks(core);
        self.finish_tools(core, ToolStatus::Cancelled);
        self.pending.clear();
        self.turn = None;
        self.item = None;
        self.running = false;
        self.ready = false;
        core.state.exit = Some(ExitInfo::now(code, error));
        core.state.doing = None;
        core.state.turn_started = None;
        core.set_status(Status::Exited);
        core.touch_state();
    }

    // ------------------------------------------------------------------ commands

    pub fn command(&mut self, core: &mut Core, cmd: Command) {
        match cmd {
            Command::Admit { token, epoch, .. } | Command::MailAck { token, epoch, .. } => core.refuse_owner_mail(Some(token), epoch),
            Command::MailPeers { epoch, .. } => core.refuse_owner_mail(None, epoch),
            Command::MailHandoff { client, .. } => {
                let epoch = core.state.epoch;
                core.refuse_owner_mail(None, epoch);
                core.reply(client, json!({"error": {"code": super::state_view::Error::Unreadable}}));
            }
            Command::Send { id, text, mode, attachments, overrides, reply_to, .. } => {
                // Its go-back is kiln's (`harness::goback`): a checkpoint anchors the prompt's
                // moment so "restore files to here" reaches back to it.
                core.effects.push(Effect::Checkpoint { at: Core::now() });
                self.send(core, QueueItem { id, text, at: Core::now(), attachments, overrides, reply_to }, mode);
            }
            Command::QueueEdit { id, text, attachments } => {
                if let Some(q) = core.state.queue.iter_mut().find(|q| q.id == id) {
                    q.text = text;
                    if let Some(a) = attachments {
                        q.attachments = a;
                    }
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
            // Into the turn under way as Pi's steering; with none, it's the next turn.
            Command::QueueSteer { id } => {
                if let Some(pos) = core.state.queue.iter().position(|q| q.id == id) {
                    let item = core.state.queue.remove(pos);
                    core.touch_state();
                    self.send(core, item, SendMode::Steer);
                }
            }
            Command::Approve { id, decision, message, answers, choices, .. } => self.answer(core, &id, decision, message, choices_of(answers, choices)),
            Command::Interrupt => self.stop(core),
            Command::SetMode { mode } => {
                if mode == core.state.mode || !modes().iter().any(|m| m.id == mode) {
                    return;
                }
                if mode == FULL && !self.allow_bypass {
                    self.bypass_next = true;
                }
                let label = modes().into_iter().find(|m| m.id == mode).map(|m| m.label).unwrap_or_default();
                core.state.mode = mode;
                core.touch_state();
                self.reload_for(core, label);
            }
            Command::SetModel { model, .. } => self.set_model(core, &model),
            Command::SetEffort { effort } => {
                if effort == core.state.effort || !LEVELS.contains(&effort.as_str()) || (!core.meta.efforts.is_empty() && !core.meta.efforts.iter().any(|e| e.id == effort)) {
                    return;
                }
                self.request(core, Req::SetLevel(effort.clone()), json!({"type": "set_thinking_level", "level": effort}));
            }
            // Pi has no faster speed (`meta.speeds` stays empty).
            Command::SetSpeed { .. } => {}
            Command::Suggest { client, .. } => core.reply(client, json!({"suggestions": []})),
            Command::Control { id, value } if id == "export" => {
                let client = value.as_ref().and_then(|v| v.get("client")).and_then(Value::as_str).unwrap_or("").to_owned();
                core.reply(client, json!({"title": "Conversation", "text": core.export(), "format": "markdown", "filename": "conversation.md"}));
            }
            Command::Control { id, value } if id == "compact" => {
                let text = value.as_ref().and_then(|v| v.get("text")).and_then(Value::as_str).map(str::trim).unwrap_or("").to_owned();
                self.compact = Some(text);
                if self.busy(core) {
                    self.notice(core, Level::Info, "Pi compacts the conversation once this turn ends");
                }
                self.next(core);
            }
            // Signed in anew (`crate::accounts`): it starts again with the credentials, once idle.
            Command::Control { id, .. } if id == "account" => self.reload_for(core, "The new sign-in".to_owned()),
            // Its go-back is kiln's (`harness::goback`).
            Command::Control { id, value } => {
                super::goback::control(core, &id, value.as_ref());
            }
            Command::StopTask { .. } | Command::StopTasks | Command::Restart | Command::ForceStop | Command::Snapshot | Command::MoveTo { .. } | Command::Moving { .. } | Command::HandedOver { .. } | Command::OwnEdits { .. } | Command::RemoveTask { .. } => {}
            // A pause's commands are the session's (`harness::pause`); it has no subagents.
            Command::Resume | Command::AutoResume { .. } | Command::PauseAt { .. } | Command::MaxSubagents { .. } | Command::MoveOnLimit { .. } | Command::LimitHold { .. } | Command::Pause { .. } | Command::Perms { .. } | Command::Branch { .. } => {}
            // Its own commands, its extensions', templates and skills run as "/name args" messages.
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
            Command::StateRead { client, .. } | Command::AtState { client, .. } => core.reply(client, json!({"error": {"code": super::state_view::Error::Unreadable}})),
            Command::McpAdd { client, .. } | Command::McpRemove { client, .. } => match client.filter(|c| !c.is_empty()) {
                Some(c) => core.reply(c, json!({"ok": false, "error": UNSUPPORTED})),
                None => self.notice(core, Level::Warn, UNSUPPORTED),
            },
            Command::McpEnable { .. } | Command::McpReconnect { .. } => self.notice(core, Level::Warn, UNSUPPORTED),
            Command::McpRefresh => {}
            Command::SkillsReload => core.skills_changed(),
        }
    }

    /// Its model, by kiln's id for it (`provider/id`, Pi's `set_model`).
    fn set_model(&mut self, core: &mut Core, model: &str) {
        if model == core.state.model {
            return;
        }
        let Some((provider, id)) = model.split_once('/').filter(|(p, i)| !p.is_empty() && !i.is_empty()) else {
            self.notice(core, Level::Warn, format!("Pi names its models provider/model: {model} isn't one"));
            return;
        };
        self.request(core, Req::SetModel(model.to_owned()), json!({"type": "set_model", "provider": provider, "modelId": id}));
    }

    /// A setting it takes when it starts: from its next start, which is now if nothing is under way.
    fn reload_for(&mut self, core: &mut Core, what: String) {
        self.reload = Some(what.clone());
        if self.busy(core) {
            self.notice(core, Level::Info, format!("{what} from the next turn: Pi starts again with it once this one ends (the session goes on)"));
        }
        self.reload_when_idle(core);
    }

    fn reload_when_idle(&mut self, core: &mut Core) {
        if self.reload.is_none() || self.busy(core) || core.state.status == Status::Starting {
            return;
        }
        let Some(what) = self.reload.take() else { return };
        if std::mem::take(&mut self.bypass_next) {
            core.effects.push(Effect::AllowBypass);
        }
        // Not running: its next start takes it anyway.
        if core.state.status == Status::Exited {
            return;
        }
        self.notice(core, Level::Info, format!("{what}: Pi starts again with it (the session goes on)"));
        self.restarting = true;
        core.effects.push(Effect::Restart { resume_at: None });
    }

    /// Every few seconds: a setting waiting for the turn to end, and dialogs Pi answered itself.
    pub fn tick(&mut self, core: &mut Core) {
        let now = Core::now();
        let gone: Vec<String> = self.asks.iter().filter(|(_, d)| d.until.is_some_and(|u| u <= now)).map(|(id, _)| id.clone()).collect();
        for id in gone {
            self.asks.remove(&id);
            core.take_approval(&id);
            self.notice(core, Level::Info, "Pi answered the extension's question itself: it waited no longer");
        }
        self.reload_when_idle(core);
    }

    /// Its CLI's answers: its version.
    pub fn run_done(&mut self, core: &mut Core, job: &str, code: Option<i32>, output: &str) {
        if code != Some(0) || job != VERSION_JOB {
            return;
        }
        let v = output.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_owned();
        if !v.is_empty() {
            core.meta.version = v;
            core.touch_meta();
            self.status_rows(core);
        }
    }

    /// A stop: what was steered in and not taken yet comes back first (`clear_queue`), then the
    /// run is aborted; its end (`agent_settled`) ends the turn.
    fn stop(&mut self, core: &mut Core) {
        if !self.busy(core) || self.pending.values().any(|r| *r == Req::Abort) {
            core.touch_state();
            return;
        }
        if !self.steered.is_empty() {
            self.request(core, Req::Clear, json!({"type": "clear_queue"}));
        }
        self.request(core, Req::Abort, json!({"type": "abort"}));
        core.state.doing = Some("Stopping".to_owned());
        core.touch_state();
    }

    fn send(&mut self, core: &mut Core, mut m: QueueItem, mode: SendMode) {
        m.text = m.text.trim_end().to_owned();
        if m.text.is_empty() && m.attachments.is_empty() {
            return;
        }
        if core.state.goal.is_empty() {
            core.state.goal = clip_middle(&m.text, 600).0;
        }
        let blocked = !self.ready || self.reload.is_some() || self.restarting || core.state.status == Status::Exited;
        if self.busy(core) {
            match mode {
                // First in line, then the turn under way is stopped (it goes once that has ended).
                SendMode::Now => {
                    core.state.queue.insert(0, m);
                    core.touch_state();
                    self.stop(core);
                }
                SendMode::Steer if !blocked && (self.turn.is_some() || self.running) => self.steer(core, m),
                _ => {
                    core.state.queue.push(m);
                    core.touch_state();
                }
            }
            return;
        }
        if blocked || (mode == SendMode::Queue && !core.state.queue.is_empty()) {
            core.state.queue.push(m);
            core.touch_state();
            self.next(core);
            return;
        }
        self.prompt(core, m);
    }

    /// The conversation so far, in words, to give with the first message it sends.
    pub fn offer_context(&mut self, text: Option<String>) {
        self.context = text.filter(|t| !t.is_empty());
    }

    /// The conversation it was offered went with a message (or it was offered none).
    pub fn seeded(&self) -> bool {
        self.context.is_none()
    }

    /// A message as Pi takes one: its words (with the conversation so far before the first), the
    /// files attached by their paths on this machine, and pictures inline where its model takes them.
    fn words(&mut self, m: &QueueItem, first: bool) -> (String, Vec<Value>) {
        let context = if first { self.context.take() } else { None };
        let mut text = match context {
            Some(c) => format!("{c}\n\n{}", m.text),
            None => m.text.clone(),
        };
        let mut images = Vec::new();
        for a in &m.attachments {
            if a.is_image() && !a.data_base64.is_empty() && self.images != Some(false) {
                images.push(json!({"type": "image", "data": a.data_base64, "mimeType": a.mime}));
            } else if let Some(p) = &a.path {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(&format!("[Attached: {p}]"));
            }
        }
        (text, images)
    }

    fn command_of(kind: &str, id: &str, text: String, images: Vec<Value>) -> Value {
        let mut c = json!({"id": id, "type": kind, "message": text});
        if !images.is_empty()
            && let Some(o) = c.as_object_mut()
        {
            o.insert("images".into(), Value::Array(images));
        }
        c
    }

    fn shown(core: &mut Core, m: &QueueItem, d: Delivery) {
        core.state.delivery.insert(m.id.clone(), d);
        let (images, names) = super::model::attachment_summary(&m.attachments);
        core.note_sent(&m.id, &m.text);
        core.upsert(&m.id, None, Body::User { text: m.text.clone(), images, client_id: Some(m.id.clone()), origin: Some(super::core::origin_of(&m.id).into()), attachments: names, reply_to: m.reply_to.clone() });
    }

    fn prompt(&mut self, core: &mut Core, m: QueueItem) {
        self.close_blocks(core);
        let (text, images) = self.words(&m, true);
        // Its id is the message's: Pi's answer to it names it.
        core.write(&Self::command_of("prompt", &m.id, text, images));
        self.pending.insert(m.id.clone(), Req::Prompt(m.id.clone()));
        self.turn = Some(m.id.clone());
        self.error = None;
        Self::shown(core, &m, Delivery::Started);
        self.item = Some(m);
        core.state.turn_started = Some(Core::now());
        core.state.doing = Some("Thinking".to_owned());
        core.settle_status(true);
        core.touch_state();
    }

    /// Into the run under way: Pi takes it after the turn's tool calls, before its next model call.
    fn steer(&mut self, core: &mut Core, m: QueueItem) {
        let (text, images) = self.words(&m, false);
        core.write(&Self::command_of("steer", &m.id, text, images));
        self.pending.insert(m.id.clone(), Req::Steer(m.id.clone()));
        Self::shown(core, &m, Delivery::Sent);
        self.steered.push(m);
        core.touch_state();
    }

    pub(crate) fn next(&mut self, core: &mut Core) {
        if self.busy(core) || !self.ready || self.reload.is_some() || self.restarting || core.state.status == Status::Exited {
            return;
        }
        // A compaction asked for during the turn goes first.
        if let Some(text) = self.compact.take() {
            let mut c = json!({"type": "compact"});
            if !text.is_empty()
                && let Some(o) = c.as_object_mut()
            {
                o.insert("customInstructions".into(), json!(text));
            }
            self.request(core, Req::Compact, c);
            core.compacting(None);
            core.settle_status(true);
            core.touch_state();
            return;
        }
        // A pause (or a switch) holds what waits, and the stop point may start one here.
        if core.state.queue.is_empty() || !super::pause::open(core) {
            return;
        }
        let item = core.state.queue.remove(0);
        core.touch_state();
        let id = item.id.clone();
        self.prompt(core, item);
        super::pause::sent_from_queue(core, &id);
    }

    /// There may be room on its machine for it to go to work: its queue goes, as between turns.
    pub fn room(&mut self, core: &mut Core) {
        self.next(core);
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

    /// It has no subagents: a message for one goes nowhere.
    pub fn takes(&self, _core: &Core, _t: &Task) -> (Vec<SendMode>, Option<&'static str>) {
        (Vec::new(), None)
    }

    pub fn send_task(&mut self, core: &mut Core, t: &Task, m: QueueItem) {
        super::subagent::refuse(core, &t.id, &m, "its harness takes no message for it from kiln.");
    }

    // ------------------------------------------------------------------ dialogs

    /// An extension's request (`extension_ui_request`): a dialog a person answers in kiln, or what
    /// it only tells.
    fn dialog(&mut self, core: &mut Core, v: &Value) {
        let Some(request) = v.get("id").and_then(Value::as_str).map(str::to_owned) else { return };
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
        let method = s("method");
        let title = s("title");
        let (questions, detail) = match method.as_str() {
            "select" => {
                let options = v.get("options").and_then(Value::as_array).map(|o| o.iter().filter_map(Value::as_str).map(|l| QuestionOption { label: l.to_owned(), ..QuestionOption::default() }).collect()).unwrap_or_default();
                (vec![Question { question: title.clone(), options, ..Question::default() }], None)
            }
            "confirm" => (Vec::new(), Some(s("message")).filter(|m| !m.is_empty())),
            "input" => (vec![Question { question: title.clone(), header: s("placeholder"), other: true, ..Question::default() }], None),
            "editor" => (vec![Question { question: title.clone(), other: true, ..Question::default() }], Some(s("prefill")).filter(|p| !p.is_empty())),
            "notify" => {
                let level = match v.get("notifyType").and_then(Value::as_str) {
                    Some("warning") => Level::Warn,
                    Some("error") => Level::Error,
                    _ => Level::Info,
                };
                self.notice(core, level, clip_middle(&s("message"), 2000).0);
                return;
            }
            // Its status line, widgets, title and editor text are its terminal's.
            _ => return,
        };
        let until = v.get("timeout").and_then(Value::as_i64).filter(|t| *t > 0).map(|t| Core::now().saturating_add(t));
        let id = format!("pi-ui-{request}");
        let approval = Approval {
            id: id.clone(),
            tool_id: None,
            tool_name: format!("extension {method}"),
            tool: if questions.is_empty() { ToolKind::Other } else { ToolKind::Question },
            title: if title.is_empty() { "An extension asks".to_owned() } else { title },
            detail,
            input: clip_json(v, 16 * 1024),
            command: None,
            cwd: core.state.cwd.clone(),
            diffs: Vec::new(),
            reason: None,
            always: Vec::new(),
            default_to_no: false,
            questions,
            plan: None,
            agent: None,
            at: Core::now(),
        };
        self.asks.insert(id, Dialog { request, method, until });
        core.state.doing = Some("Waiting for your answer".to_owned());
        core.add_approval(approval);
        core.settle_status(self.busy(core));
        core.touch_state();
    }

    /// A person answered a dialog: its value, a yes or a no, or it's dismissed.
    fn answer(&mut self, core: &mut Core, id: &str, decision: Decision, message: Option<String>, choices: Option<Vec<ChoiceAnswer>>) {
        let Some(d) = self.asks.remove(id) else { return };
        core.take_approval(id);
        let first = choices.as_ref().and_then(|c| c.first());
        let value = match d.method.as_str() {
            "select" => first.and_then(|c| c.selected.first().cloned().or_else(|| c.other.clone())),
            _ => first.and_then(|c| c.other.clone().or_else(|| c.selected.first().cloned())).or_else(|| message.clone().filter(|m| !m.trim().is_empty())),
        };
        let reply = match (d.method.as_str(), decision) {
            ("confirm", Decision::Deny) => json!({"type": "extension_ui_response", "id": d.request, "confirmed": false}),
            ("confirm", _) => json!({"type": "extension_ui_response", "id": d.request, "confirmed": true}),
            (_, Decision::Deny) => json!({"type": "extension_ui_response", "id": d.request, "cancelled": true}),
            (_, _) => match value {
                Some(v) => json!({"type": "extension_ui_response", "id": d.request, "value": v}),
                None => json!({"type": "extension_ui_response", "id": d.request, "cancelled": true}),
            },
        };
        core.write(&reply);
        if core.state.approvals.is_empty() && self.busy(core) {
            core.state.doing = Some("Working".to_owned());
        }
        core.settle_status(self.busy(core));
        core.touch_state();
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
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "response" => self.response(core, &v),
            "agent_start" => self.agent_start(core),
            "agent_settled" => self.settled(core, v.get("aborted").and_then(Value::as_bool).unwrap_or(false)),
            "message_start" => {
                if v.pointer("/message/role").and_then(Value::as_str) == Some("assistant") {
                    self.close_blocks(core);
                    self.message += 1;
                }
            }
            "message_update" => self.update(core, v.get("assistantMessageEvent").unwrap_or(&Value::Null)),
            "message_end" => self.message_end(core, v.get("message").unwrap_or(&Value::Null)),
            "tool_execution_start" | "tool_execution_update" | "tool_execution_end" => self.tool(core, &v),
            "compaction_start" => core.compacting(None),
            "compaction_end" => self.compaction_end(core, &v),
            "auto_retry_start" => {
                let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
                let why = v.get("errorMessage").and_then(Value::as_str).unwrap_or("");
                self.notice(core, Level::Info, format!("Pi tries again ({} of {}) in {} s: {}", n("attempt"), n("maxAttempts"), n("delayMs") / 1000, clip_middle(why, 500).0));
            }
            "auto_retry_end" => {
                if v.get("success").and_then(Value::as_bool) == Some(false)
                    && let Some(e) = v.get("finalError").and_then(Value::as_str)
                {
                    self.error = Some(e.to_owned());
                }
            }
            "thinking_level_changed" => {
                if let Some(l) = v.get("level").and_then(Value::as_str) {
                    core.state.effort = l.to_owned();
                    core.touch_state();
                }
            }
            "extension_ui_request" => self.dialog(core, &v),
            "extension_error" => {
                let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("");
                self.notice(core, Level::Warn, clip_middle(&format!("An extension failed ({}, {}): {}", s("extensionPath"), s("event"), s("error")), 2000).0);
            }
            _ => {}
        }
    }

    fn response(&mut self, core: &mut Core, v: &Value) {
        let Some(req) = v.get("id").and_then(Value::as_str).and_then(|id| self.pending.remove(id)) else {
            // One without an id: a line it couldn't read (`parse`).
            if v.get("success").and_then(Value::as_bool) == Some(false)
                && let Some(e) = v.get("error").and_then(Value::as_str)
            {
                self.notice(core, Level::Warn, clip_middle(e, 2000).0);
            }
            return;
        };
        let data = v.get("data").unwrap_or(&Value::Null);
        if v.get("success").and_then(Value::as_bool) != Some(true) {
            let error = v.get("error").and_then(Value::as_str).unwrap_or("it failed").to_owned();
            self.refused(core, req, error);
            return;
        }
        match req {
            Req::State => self.state(core, data),
            Req::Models => {
                let models: Vec<ModelOption> = data.get("models").and_then(Value::as_array).map(|ms| ms.iter().filter_map(model_option).collect()).unwrap_or_default();
                if !models.is_empty() {
                    core.meta.models = models;
                    for m in &mut core.meta.models {
                        m.default = m.id == core.state.model;
                    }
                    core.touch_meta();
                }
            }
            Req::Commands => {
                let mut catalog = extras();
                for c in data.get("commands").and_then(Value::as_array).into_iter().flatten() {
                    let Some(name) = c.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()) else { continue };
                    if catalog.iter().any(|k| k.name == name) {
                        continue;
                    }
                    let group = match c.get("source").and_then(Value::as_str) {
                        Some("skill") => "Skills",
                        Some("extension") => "Plugins",
                        _ => "Session",
                    };
                    catalog.push(SlashCommand::prompt(name, c.get("description").and_then(Value::as_str).unwrap_or(""), "", group));
                }
                core.meta.commands = sort_catalog(catalog);
                core.touch_meta();
            }
            Req::Levels => {
                let levels: Vec<String> = data.get("levels").and_then(Value::as_array).map(|l| l.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
                if !levels.is_empty() {
                    core.meta.efforts = efforts(&levels.iter().map(String::as_str).collect::<Vec<_>>());
                    let model = core.state.model.clone();
                    if let Some(m) = core.meta.models.iter_mut().find(|m| m.id == model) {
                        m.efforts = levels;
                    }
                    core.touch_meta();
                }
            }
            Req::Stats => {
                let n = |p: &str| data.pointer(p).and_then(Value::as_u64);
                if let Some(t) = n("/contextUsage/tokens") {
                    core.state.usage.context_tokens = Some(t);
                }
                if let Some(w) = n("/contextUsage/contextWindow") {
                    core.state.usage.context_window = Some(w);
                }
                core.touch_state();
            }
            Req::Prompt(id) => {
                // Pi took it.
                if self.item.as_ref().is_some_and(|m| m.id == id) {
                    self.item = None;
                }
                // An extension's command took it: no run starts for it.
                if data.get("disposition").and_then(Value::as_str) == Some("handled") && self.turn.as_deref() == Some(id.as_str()) && !self.running {
                    self.turn = None;
                    core.state.delivery.insert(id.clone(), Delivery::Completed);
                    core.input_consumed(&id, super::admission::Consumption::ProviderCompleted);
                    core.end_turn(core.state.turn_started, "done", Some("handled"));
                    core.state.turn_started = None;
                    core.state.doing = None;
                    core.settle_status(false);
                    core.touch_state();
                    self.next(core);
                }
            }
            Req::Steer(_) | Req::Abort => {}
            // It answers once the compaction is over (its events said how it went).
            Req::Compact => {
                core.compacting_over(None);
                core.settle_status(self.busy(core));
                core.touch_state();
                self.next(core);
            }
            Req::Clear => self.cleared(core, data),
            Req::SetModel(model) => {
                core.state.model = model;
                self.images = data.get("input").and_then(Value::as_array).map(|i| i.iter().any(|x| x.as_str() == Some("image")));
                core.touch_state();
                // Its levels are the model's.
                self.request(core, Req::Levels, json!({"type": "get_available_thinking_levels"}));
            }
            Req::SetLevel(level) => {
                core.state.effort = level;
                core.touch_state();
            }
        }
    }

    /// It refused a request: why, where a person should hear it.
    fn refused(&mut self, core: &mut Core, req: Req, error: String) {
        match req {
            // It didn't say its session: it takes prompts all the same.
            Req::State => {
                self.notice(core, Level::Warn, format!("Pi didn't say its session: {}", clip_middle(&error, 2000).0));
                self.state(core, &json!({}));
            }
            // A run kiln didn't start (an extension's) was under way: the message waits for its end.
            Req::Prompt(id) if error.contains("already processing") && self.item.as_ref().is_some_and(|m| m.id == id) => {
                if self.turn.as_deref() == Some(id.as_str()) {
                    self.turn = None;
                }
                self.running = true;
                if let Some(m) = self.item.take() {
                    core.state.delivery.insert(m.id.clone(), Delivery::Queued);
                    core.state.queue.insert(0, m);
                }
                core.touch_state();
            }
            Req::Prompt(id) => {
                if self.turn.as_deref() == Some(id.as_str()) {
                    self.turn = None;
                }
                core.state.delivery.insert(id, Delivery::Cancelled);
                self.said_error(core, &error);
                core.end_turn(core.state.turn_started, "cut", Some("refused"));
                core.state.turn_started = None;
                core.state.doing = None;
                core.settle_status(self.busy(core));
                core.touch_state();
                self.next(core);
            }
            // Not taken: it waits for the next turn instead.
            Req::Steer(id) => {
                if let Some(pos) = self.steered.iter().position(|m| m.id == id) {
                    let m = self.steered.remove(pos);
                    core.state.delivery.insert(m.id.clone(), Delivery::Queued);
                    core.state.queue.insert(0, m);
                    core.touch_state();
                }
            }
            Req::Compact => {
                core.compacted(&format!("{}-{}", self.run, self.next_id), None, Level::Warn, super::core::compaction_failed_words(Some(&error)));
                core.settle_status(self.busy(core));
                self.next(core);
            }
            Req::SetModel(_) | Req::SetLevel(_) | Req::Abort | Req::Clear => self.notice(core, Level::Warn, clip_middle(&error, 2000).0),
            Req::Models | Req::Commands | Req::Levels | Req::Stats => {}
        }
    }

    /// An error a turn ended with: a usage limit pauses the session until it resets; anything else
    /// is said.
    fn said_error(&mut self, core: &mut Core, error: &str) {
        if super::pause::says_limit(error) {
            // The account's usage limit: paused until it resets (Pi can't say when; kiln checks
            // back now and then).
            let hit = super::pause::Hit::from_limits(&core.state.limits, error.to_owned(), None, Core::now());
            let cut = super::pause::cut_now(core);
            super::pause::hold(core, super::pause::LIMIT, "harness", hit, Some(cut));
        } else {
            self.notice(core, Level::Error, clip_middle(error, 2000).0);
        }
    }

    /// `get_state`: its session, model and thinking level; it takes prompts from now on.
    fn state(&mut self, core: &mut Core, data: &Value) {
        if let Some(s) = data.get("sessionId").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            // Asked to go on with one it didn't have, Pi made a new one under its id.
            let had = core.events().any(|e| matches!(e.body, Body::User { .. } | Body::Assistant { .. }));
            if self.resumed.as_deref() == Some(s) && data.get("messageCount").and_then(Value::as_u64) == Some(0) && had && self.context.is_none() {
                self.notice(core, Level::Warn, format!("Pi couldn't find its session {s} in this folder on this machine, so this is a new one (what it said before isn't in it)"));
            }
            self.session = Some(s.to_owned());
            core.state.harness_session = Some(s.to_owned());
        }
        if let Some(m) = data.get("model").filter(|m| m.is_object()) {
            if let Some(id) = model_id(m) {
                core.state.model = id;
            }
            self.images = m.get("input").and_then(Value::as_array).map(|i| i.iter().any(|x| x.as_str() == Some("image")));
        }
        if let Some(l) = data.get("thinkingLevel").and_then(Value::as_str) {
            core.state.effort = l.to_owned();
        }
        if !self.ready {
            core.skills_provider_ready();
        }
        self.ready = true;
        self.status_rows(core);
        // The levels its model takes.
        self.request(core, Req::Levels, json!({"type": "get_available_thinking_levels"}));
        if !self.busy(core) {
            core.set_status(Status::Idle);
        }
        core.touch_state();
        // A setting chosen while it started: it starts again with it before anything is sent.
        self.reload_when_idle(core);
        self.next(core);
    }

    fn status_rows(&mut self, core: &mut Core) {
        let mut rows = vec![StatusItem { group: "Agent".into(), label: "Name".into(), value: "Pi".into(), tone: None }];
        if !core.meta.version.is_empty() {
            rows.push(StatusItem { group: "Agent".into(), label: "Version".into(), value: core.meta.version.clone(), tone: None });
        }
        rows.push(StatusItem { group: "Agent".into(), label: "Protocol".into(), value: "RPC (JSON lines)".into(), tone: None });
        if let Some(s) = &self.session {
            rows.push(StatusItem { group: "Agent".into(), label: "Session".into(), value: s.clone(), tone: None });
        }
        rows.push(StatusItem { group: "Permissions".into(), label: String::new(), value: "It asks before nothing: what it may do is its mode's tools".into(), tone: None });
        core.state.harness_status = rows;
        core.touch_state();
    }

    fn agent_start(&mut self, core: &mut Core) {
        self.running = true;
        // A run kiln didn't start (an extension's): the session works all the same.
        if core.state.turn_started.is_none() {
            core.state.turn_started = Some(Core::now());
        }
        if core.state.doing.is_none() {
            core.state.doing = Some("Thinking".to_owned());
        }
        core.settle_status(true);
        core.touch_state();
    }

    /// `agent_settled`: Pi won't go on by itself; the turn is over.
    fn settled(&mut self, core: &mut Core, aborted: bool) {
        self.running = false;
        self.close_blocks(core);
        self.finish_tools(core, if aborted { ToolStatus::Cancelled } else { ToolStatus::Error });
        let end = if aborted { Delivery::Cancelled } else { Delivery::Completed };
        // What was steered in and taken went with the turn (what wasn't comes back with the
        // clear's answer, still on its way).
        if !self.pending.values().any(|r| *r == Req::Clear) {
            for m in std::mem::take(&mut self.steered) {
                if !aborted {
                    core.input_consumed(&m.id, super::admission::Consumption::ProviderCompleted);
                }
                core.state.delivery.insert(m.id, end);
            }
        }
        let error = self.error.take();
        self.item = None;
        if let Some(id) = self.turn.take() {
            if !aborted && error.is_none() {
                core.input_consumed(&id, super::admission::Consumption::ProviderCompleted);
            }
            core.state.delivery.insert(id, end);
        }
        let (how, why) = match (&error, aborted) {
            (_, true) => ("stopped", "interrupted"),
            (Some(e), false) if super::pause::says_limit(e) => ("limit", "limit"),
            (Some(_), false) => ("cut", "error"),
            (None, false) => ("done", "success"),
        };
        if aborted {
            self.notice(core, Level::Info, "Interrupted");
        } else if let Some(e) = &error {
            self.said_error(core, e);
        }
        core.state.usage.turns += 1;
        if core.state.turn_started.is_some() {
            core.end_turn(core.state.turn_started, how, Some(why));
        }
        core.state.turn_started = None;
        core.state.doing = None;
        core.settle_status(false);
        core.touch_state();
        // How full its context is now.
        self.request(core, Req::Stats, json!({"type": "get_session_stats"}));
        self.reload_when_idle(core);
        self.next(core);
    }

    /// `clear_queue`'s answer: the steering Pi hadn't taken yet, the last of what kiln steered in,
    /// goes back to the front of kiln's queue, in order.
    fn cleared(&mut self, core: &mut Core, data: &Value) {
        let left = data.get("steering").and_then(Value::as_array).map_or(0, Vec::len);
        let keep = self.steered.len().saturating_sub(left);
        let back = self.steered.split_off(keep);
        for (i, m) in back.into_iter().enumerate() {
            core.state.delivery.insert(m.id.clone(), Delivery::Queued);
            core.state.queue.insert(i.min(core.state.queue.len()), m);
        }
        core.touch_state();
        // The run ended meanwhile: what it took went with it.
        if !self.running && self.turn.is_none() {
            for m in std::mem::take(&mut self.steered) {
                core.state.delivery.insert(m.id, Delivery::Cancelled);
            }
            self.next(core);
        }
    }

    /// A content block of the reply as it streams (`message_update`).
    fn update(&mut self, core: &mut Core, e: &Value) {
        let Some(index) = e.get("contentIndex").and_then(Value::as_u64) else { return };
        let kind = e.get("type").and_then(Value::as_str).unwrap_or("");
        let thinking = kind.starts_with("thinking");
        match kind {
            "text_delta" | "thinking_delta" => {
                let delta = e.get("delta").and_then(Value::as_str).unwrap_or("");
                let id = self.block(core, index, thinking);
                core.append(&id, delta);
                if !thinking {
                    self.summarize(core, &id);
                    core.state.doing = Some("Writing".to_owned());
                }
                core.touch_state();
            }
            "text_end" | "thinking_end" => {
                let text = e.get("content").and_then(Value::as_str).unwrap_or("").to_owned();
                let id = self.block(core, index, thinking);
                let body = if thinking { Body::Thinking { text, streaming: false, redacted: false } } else { Body::Assistant { text, streaming: false } };
                core.upsert(&id, None, body);
                if !thinking {
                    self.summarize(core, &id);
                }
                core.touch_state();
            }
            // A tool call the model wrote: shown once it's whole, before it runs.
            "toolcall_end" => {
                let Some(call) = e.get("toolCall") else { return };
                let (Some(tid), Some(name)) = (call.get("id").and_then(Value::as_str), call.get("name").and_then(Value::as_str)) else { return };
                let id = self.tool_event(tid);
                self.tools.insert(tid.to_owned(), id.clone());
                if core.get(&id).is_none() {
                    let mut c = tool_call(&id, name, call.get("arguments").unwrap_or(&Value::Null), &core.state.cwd);
                    c.status = ToolStatus::Pending;
                    core.upsert(&id, None, Body::Tool(Box::new(c)));
                }
            }
            _ => {}
        }
    }

    /// The timeline event of block `index` of the message under way, made if it isn't there yet.
    fn block(&mut self, core: &mut Core, index: u64, thinking: bool) -> String {
        if let Some(id) = self.blocks.get(&index) {
            return id.clone();
        }
        let id = format!("pi-{}-{}-{index}", self.run, self.message);
        let body = if thinking { Body::Thinking { text: String::new(), streaming: true, redacted: false } } else { Body::Assistant { text: String::new(), streaming: true } };
        core.upsert(&id, None, body);
        self.blocks.insert(index, id.clone());
        id
    }

    fn summarize(&self, core: &mut Core, id: &str) {
        if let Some(Body::Assistant { text, .. }) = core.get(id).map(|e| &e.body) {
            core.state.summary = clip_middle(text.trim(), 600).0;
        }
    }

    /// `message_end`: the whole message, as Pi keeps it. An assistant's words and thoughts replace
    /// what streamed (or come now, from a model that streamed none), and its usage counts.
    fn message_end(&mut self, core: &mut Core, m: &Value) {
        if m.get("role").and_then(Value::as_str) != Some("assistant") {
            return;
        }
        for (i, b) in m.get("content").and_then(Value::as_array).into_iter().flatten().enumerate() {
            let index = u64::try_from(i).unwrap_or(u64::MAX);
            let (thinking, text) = match b.get("type").and_then(Value::as_str) {
                Some("text") => (false, b.get("text").and_then(Value::as_str).unwrap_or("")),
                Some("thinking") => (true, b.get("thinking").and_then(Value::as_str).unwrap_or("")),
                _ => continue,
            };
            let redacted = b.get("redacted").and_then(Value::as_bool).unwrap_or(false);
            if text.is_empty() && !redacted && !self.blocks.contains_key(&index) {
                continue;
            }
            let id = self.block(core, index, thinking);
            let body = if thinking { Body::Thinking { text: text.to_owned(), streaming: false, redacted } } else { Body::Assistant { text: text.to_owned(), streaming: false } };
            core.upsert(&id, None, body);
            if !thinking {
                self.summarize(core, &id);
            }
        }
        self.close_blocks(core);
        if let Some(u) = m.get("usage") {
            let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
            core.state.usage.input_tokens += n("input");
            core.state.usage.output_tokens += n("output");
            core.state.usage.cache_read_tokens += n("cacheRead");
            core.state.usage.cost_usd += u.pointer("/cost/total").and_then(Value::as_f64).filter(|c| c.is_finite() && *c >= 0.0).unwrap_or(0.0);
        }
        if m.get("stopReason").and_then(Value::as_str) == Some("error")
            && let Some(e) = m.get("errorMessage").and_then(Value::as_str).filter(|e| !e.is_empty())
        {
            self.error = Some(e.to_owned());
        }
        if self.busy(core) {
            core.state.doing = Some("Thinking".to_owned());
        }
        core.touch_state();
    }

    fn tool_event(&self, tid: &str) -> String {
        self.tools.get(tid).cloned().unwrap_or_else(|| format!("pi-tool-{tid}"))
    }

    /// A tool's run: started, its output so far, ended.
    fn tool(&mut self, core: &mut Core, v: &Value) {
        let Some(tid) = v.get("toolCallId").and_then(Value::as_str) else { return };
        let name = v.get("toolName").and_then(Value::as_str).unwrap_or("tool");
        let id = self.tool_event(tid);
        self.tools.insert(tid.to_owned(), id.clone());
        let mut call = match core.get(&id).map(|e| &e.body) {
            Some(Body::Tool(t)) => t.as_ref().clone(),
            _ => tool_call(&id, name, v.get("args").unwrap_or(&Value::Null), &core.state.cwd),
        };
        match v.get("type").and_then(Value::as_str) {
            Some("tool_execution_start") => {
                call.status = ToolStatus::Running;
                core.state.doing = Some(super::claude::doing(&call));
            }
            Some("tool_execution_update") => {
                if let Some(out) = result_text(v.get("partialResult")) {
                    let (text, cut) = clip_middle(&out, OUTPUT_MAX);
                    call.output = Some(text);
                    call.truncated = cut;
                }
            }
            _ => {
                let failed = v.get("isError").and_then(Value::as_bool).unwrap_or(false);
                if let Some(out) = result_text(v.get("result")) {
                    let (text, cut) = clip_middle(&out, OUTPUT_MAX);
                    call.output = Some(text);
                    call.truncated = cut;
                }
                call.status = if failed { ToolStatus::Error } else { ToolStatus::Done };
                call.ended = Some(Core::now());
                if self.busy(core) {
                    core.state.doing = Some("Thinking".to_owned());
                }
            }
        }
        core.upsert(&id, None, Body::Tool(Box::new(call)));
        core.touch_state();
    }

    /// `compaction_end`: how it went, marked where it ran.
    fn compaction_end(&mut self, core: &mut Core, v: &Value) {
        self.seq += 1;
        let key = format!("{}-{}", self.run, self.seq);
        let started = core.compacting_of(None).cloned();
        if let Some(r) = v.get("result").filter(|r| r.is_object()) {
            let n = |k: &str| r.get(k).and_then(Value::as_u64);
            let ms = started.as_ref().map(|c| Core::now() - c.since);
            core.compacted(&key, None, Level::Info, super::core::compacted_words(n("tokensBefore").or(started.and_then(|c| c.tokens)), n("estimatedTokensAfter"), ms));
        } else if v.get("aborted").and_then(Value::as_bool) == Some(true) {
            core.compaction_stopped(None);
        } else {
            core.compacted(&key, None, Level::Warn, super::core::compaction_failed_words(v.get("errorMessage").and_then(Value::as_str)));
        }
        core.settle_status(self.busy(core));
        core.touch_state();
        if !self.busy(core) {
            self.next(core);
        }
    }

    /// The streamed reply stops growing.
    fn close_blocks(&mut self, core: &mut Core) {
        for id in std::mem::take(&mut self.blocks).into_values() {
            if let Some(e) = core.get_mut(&id) {
                match &mut e.body {
                    Body::Assistant { streaming, .. } | Body::Thinking { streaming, .. } => *streaming = false,
                    _ => {}
                }
            }
        }
    }

    /// Calls the run left open ended with it.
    fn finish_tools(&mut self, core: &mut Core, how: ToolStatus) {
        for id in std::mem::take(&mut self.tools).into_values() {
            if let Some(e) = core.get_mut(&id)
                && let Body::Tool(t) = &mut e.body
                && matches!(t.status, ToolStatus::Running | ToolStatus::Pending)
            {
                t.status = how;
                t.ended = Some(Core::now());
            }
        }
    }
}

/// Its effort levels as kiln offers them.
fn efforts(levels: &[&str]) -> Vec<ControlOption> {
    levels.iter().map(|e| ControlOption { id: (*e).to_owned(), label: super::model::effort_label(e), description: String::new(), dangerous: false }).collect()
}

/// kiln's id for a model of Pi's: `provider/id`, as `--model` takes one.
fn model_id(m: &Value) -> Option<String> {
    let provider = m.get("provider").and_then(Value::as_str).filter(|p| !p.is_empty())?;
    let id = m.get("id").and_then(Value::as_str).filter(|i| !i.is_empty())?;
    Some(format!("{provider}/{id}"))
}

/// A model of `get_available_models`, with the thinking levels it takes, as Pi works them out
/// (pi-ai's `getSupportedThinkingLevels`: off alone without reasoning; xhigh and max only where its
/// `thinkingLevelMap` maps them, and none it maps to null).
fn model_option(m: &Value) -> Option<ModelOption> {
    let id = model_id(m)?;
    let name = m.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).unwrap_or(&id);
    let provider = m.get("provider").and_then(Value::as_str).unwrap_or("");
    let map = m.get("thinkingLevelMap");
    let efforts = if m.get("reasoning").and_then(Value::as_bool) == Some(true) {
        LEVELS
            .iter()
            .filter(|l| {
                let mapped = map.and_then(|m| m.get(**l));
                match mapped {
                    Some(Value::Null) => false,
                    Some(_) => true,
                    None => !matches!(**l, "xhigh" | "max"),
                }
            })
            .map(|l| (*l).to_owned())
            .collect()
    } else {
        vec!["off".to_owned()]
    };
    Some(ModelOption { label: format!("{name} ({provider})"), id, efforts, ..ModelOption::default() })
}

/// The text of a tool's result (`{content: [{type: "text", text}], details}`).
fn result_text(r: Option<&Value>) -> Option<String> {
    let parts: Vec<&str> = r?.get("content")?.as_array()?.iter().filter(|c| c.get("type").and_then(Value::as_str) == Some("text")).filter_map(|c| c.get("text").and_then(Value::as_str)).collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// A tool call as it starts: what kind of tool, and its title from its arguments (Pi's built-in
/// tools' schemas: `read {path}`, `bash {command}`, `edit {path, edits: [{oldText, newText}]}`,
/// `write {path, content}`, `grep {pattern, path}`, `find {pattern, path}`, `ls {path}`).
fn tool_call(id: &str, name: &str, args: &Value, cwd: &str) -> ToolCall {
    let s = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_owned);
    let path = s("path").map(|p| super::claude::rel(&p, cwd));
    let tool = match name {
        "bash" | "powershell" => ToolKind::Shell,
        "edit" | "write" => ToolKind::Edit,
        "read" => ToolKind::Read,
        "grep" | "find" | "ls" | "tool_search" => ToolKind::Search,
        n if n.starts_with("mcp__") => ToolKind::Mcp,
        _ => ToolKind::Other,
    };
    let title = match tool {
        ToolKind::Shell => s("command").unwrap_or_else(|| name.to_owned()),
        ToolKind::Search => s("pattern").or_else(|| path.clone()).unwrap_or_else(|| name.to_owned()),
        _ => path.clone().unwrap_or_else(|| name.to_owned()),
    };
    let detail = match tool {
        ToolKind::Search if args.get("pattern").is_some() => path.clone(),
        _ => None,
    };
    // An edit's replacements, each as its own hunk.
    let diffs = match (name, &path) {
        ("edit", Some(p)) => {
            let hunks: Vec<_> = args
                .get("edits")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .flat_map(|e| super::diff::hunks(e.get("oldText").and_then(Value::as_str).unwrap_or(""), e.get("newText").and_then(Value::as_str).unwrap_or(""), super::diff::CONTEXT))
                .collect();
            if hunks.is_empty() { Vec::new() } else { vec![FileDiff { path: p.clone(), created: false, hunks }] }
        }
        _ => Vec::new(),
    };
    ToolCall {
        tool_id: id.to_owned(),
        name: name.to_owned(),
        tool,
        title,
        detail,
        input: clip_json(args, 16 * 1024),
        status: ToolStatus::Running,
        output: None,
        truncated: false,
        diffs,
        todos: Vec::new(),
        task: None,
        exit_code: None,
        started: Core::now(),
        ended: None,
        server: None,
        tool_short: None,
        skill: None,
        runner: None,
    }
}

#[cfg(test)]
mod tests;
