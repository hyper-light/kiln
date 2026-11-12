//! Antigravity's CLI (`agy`) in its headless, multi-turn stream-json mode. kiln starts
//! `agy --input-format stream-json --output-format stream-json` and writes one message per turn
//! (`{"event":"user","message":{"content":…}}`); agy answers with `init` (its conversation), then
//! for each turn `step_update`s (the reply as it streams, each tool call with its input and output)
//! and a `result`.
//!
//! It can't be asked anything once started. A tool that needs your approval is denied, and the
//! turn's result says which (`denied_actions`): kiln says so, with the modes that let it go ahead.
//! A stop ends the process (agy keeps the turn so far) and kiln starts it again on the same
//! conversation (`--conversation`). Its model, effort and mode are flags, so a new one starts it
//! again too, between turns.
//!
//! kiln's MCP server is in agy's own config, the only place agy reads one from (`Kind::prepare`),
//! whom it serves in agy's environment; its calls (`call_mcp_tool`) read as Claude Code's and
//! Codex's do. As agy can't ask, kiln's page tools are allowed in agy's settings (a person's own
//! rule for them wins); agy decides on its other tools itself (1.4.2 denied MCP calls outside
//! Bypass; 1.4.8 judges each).
//!
//! Its subagents (`invoke_subagent`, each a conversation of its own) come to kiln two ways: the
//! stream names each one its agent starts (a `subagent` step's `subagent_info`), and kiln's hook,
//! in agy's own hooks.json (`hooks_with`), hands kiln every conversation's tool calls, model calls
//! and stops as they happen (`kiln harness-hook --agy`, then [`Agy::hook`]): a pause holds a
//! subagent's next tool call, what a person lets each do refuses it, asks them or lets it go, and a
//! person's message to one goes in before its next model call (docs/agents.md, "What a person does
//! with an agent's subagents").
//!
//! Written against agy 1.4.2, from what it was seen to send; seen the same with 1.4.8; its hooks
//! and subagents against 1.4.10.

use std::collections::HashMap;

use futures::channel::oneshot::Sender;
use serde_json::{Value, json};

use super::core::{Core, Effect};
use super::model::{
    Approval, Body, Command, ControlOption, Decision, Delivery, ExitInfo, Inventory, Known, Level, ModeOption, ModelOption, Perms, QueueItem, SendMode, SlashCommand, Status, StatusItem, Task,
    ToolCall, ToolKind, ToolStatus, OUTPUT_MAX, clip_json, clip_middle, sort_catalog,
};
use super::session::Spec;

/// What a session of it can do, said when it starts (and again after an in-place upgrade).
pub(super) const FEATURES: &[&str] = &["send_now", "queue", "interrupt", "set_mode", "set_model", "set_effort", "resume", "attachments", "switch_harness", "own_edits", "remove_task"];
/// What it turns on once it runs: "limits" once kiln's hook is heard from it ([`Agy::hook`]), so
/// a session whose hook never ran doesn't offer what it can't hold.
pub(super) const LEARNED: &[&str] = &["limits"];

/// Its modes. Ask is agy's own default; in this mode nothing can ask you, so whatever needs your
/// approval is denied.
pub const ASK: &str = "default";
pub const ACCEPT_EDITS: &str = "accept-edits";
pub const PLAN: &str = "plan";
pub const BYPASS: &str = "bypass";
/// A new session's: it edits files in its folder; commands that need you are denied and said.
pub const DEFAULT_MODE: &str = ACCEPT_EDITS;

/// Its effort levels, lowest first.
pub const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// What a run of its CLI beside the session is for (`Effect::Run`).
const MODELS_JOB: &str = "agy-models";
const VERSION_JOB: &str = "agy-version";

pub fn modes() -> Vec<ModeOption> {
    let m = |id: &str, label: &str, description: &str, dangerous: bool| ModeOption { id: id.into(), label: label.into(), description: description.into(), dangerous };
    vec![
        m(ASK, "Ask", "Whatever needs your approval is denied (it can't ask from here), and kiln says what", false),
        m(ACCEPT_EDITS, "Accept edits", "It edits files in its folder; commands that need your approval are denied, and kiln says which", false),
        m(PLAN, "Plan", "It plans and changes nothing", false),
        m(BYPASS, "Bypass", "Everything it does goes ahead, unasked", true),
    ]
}

/// The arguments for a session: the headless multi-turn mode, the conversation it goes on with,
/// and its model, effort and mode.
pub fn args(spec: &Spec) -> Vec<String> {
    let mut a: Vec<String> = ["--input-format", "stream-json", "--output-format", "stream-json"].map(str::to_owned).to_vec();
    let mut flag = |f: &str, v: &str| {
        a.push(f.to_owned());
        a.push(v.to_owned());
    };
    if let Some(c) = spec.resume.as_deref().filter(|c| !c.trim().is_empty()) {
        flag("--conversation", c);
    }
    if let Some(m) = spec.model.as_deref().filter(|m| !m.trim().is_empty()) {
        flag("--model", m);
    }
    if let Some(e) = spec.effort.as_deref().filter(|e| EFFORTS.contains(e)) {
        flag("--effort", e);
    }
    match spec.mode.as_deref().unwrap_or(DEFAULT_MODE) {
        ACCEPT_EDITS => flag("--mode", "accept-edits"),
        PLAN => flag("--mode", "plan"),
        BYPASS if spec.allow_bypass => a.push("--dangerously-skip-permissions".to_owned()),
        _ => {}
    }
    a
}

/// kiln's own commands for an Antigravity session (agy's own run as "/name" messages).
fn extras() -> Vec<SlashCommand> {
    vec![
        SlashCommand::control("export", "The conversation as Markdown", "", "Session", json!({"type": "control", "id": "export", "value": {"client": "{client}"}})),
        SlashCommand::view("status", "The agent and what it can do", "Session", "status"),
        SlashCommand::view("model", "The model and mode", "Model & mode", "model"),
        SlashCommand::control("restart", "Start the agent again", "", "Kiln", json!({"type": "restart"})),
        SlashCommand::view("new", "Start another agent", "Kiln", "new_agent"),
        SlashCommand::view("terminal", "A shell in this agent's folder, on its machine", "Kiln", "terminal").aka(&["shell"]),
        SlashCommand::view("screen", "The agent's own terminal interface, in its folder", "Kiln", "screen"),
        SlashCommand::view("goals", "Goals one after another: each sent once the one before is done", "Kiln", "goals"),
    ]
}

/// What it can't do from kiln.
const UNSUPPORTED: &str = "Antigravity's headless mode has no way to manage its MCP servers or skills from kiln.";

/// Plain data, written out whole when its runner hands it to its next image or another runner
/// process of the machine (an upgrade: `crate::harness::kinds::Frozen`).
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Agy {
    /// The conversation it was started on (`--conversation`), to tell when that one was gone.
    resumed: Option<String>,
    /// Its conversation (from `init`), and the start of it that marks its events.
    conversation: Option<String>,
    tag: String,
    /// It said `init`: it takes messages.
    ready: bool,
    /// The message whose turn is under way (its client id).
    turn: Option<String>,
    /// The reply being streamed: (step, event id).
    text: Option<(u64, String)>,
    /// The turn's tool calls on the timeline, by step.
    tools: HashMap<u64, String>,
    /// The turn said something.
    said: bool,
    /// A setting it takes when it starts changed (said in words): it starts again once nothing
    /// is under way, the conversation going on.
    reload: Option<String>,
    /// That start may skip every permission check.
    bypass_next: bool,
    /// It may skip them now.
    allow_bypass: bool,
    /// Its process is being ended to start again: what is sent waits for the new one.
    restarting: bool,
    seq: u64,
    /// The conversation so far, in words, given with the first message it sends (a session that
    /// went on here from another machine, `super::moving`, or from another harness).
    #[serde(default)]
    context: Option<String>,
    /// kiln's hook was heard from it ([`Agy::hook`]): its calls come to kiln first, so its
    /// subagents can be paused and limited (its `limits` feature).
    #[serde(default)]
    heard: bool,
    /// Calls held for a person's pause: the subagent each is in, the hook's input, and its answer,
    /// given once it's resumed (`release_held`). Not kept over an in-place upgrade: the hook's link
    /// to this process goes with it, and agy fails the call.
    #[serde(skip)]
    held: Vec<(String, Value, Sender<Value>)>,
    /// Calls a person sees first (Ask), by the id of the approval kiln shows them: the hook's
    /// answer, given as they answer it.
    #[serde(skip)]
    asks: HashMap<String, Sender<Value>>,
    /// A person's messages that went in to a subagent before its last model call (its task, the
    /// message's id): done once it goes on past that call.
    #[serde(default)]
    sent: Vec<(String, String)>,
}

impl Agy {
    pub(crate) fn retained_input_ids(&self) -> Vec<super::admission::LegacyFlight> { self.turn.iter().cloned().map(|id| super::admission::LegacyFlight { id, protocol: super::admission::FlightProtocol::Agy, request: None }).collect() }
    pub fn new(spec: &Spec) -> Self {
        Self {
            resumed: spec.resume.clone().filter(|c| !c.trim().is_empty()),
            conversation: None,
            tag: "new".to_owned(),
            ready: false,
            turn: None,
            text: None,
            tools: HashMap::new(),
            said: false,
            reload: None,
            bypass_next: false,
            allow_bypass: spec.allow_bypass,
            restarting: false,
            seq: 0,
            context: None,
            heard: false,
            held: Vec::new(),
            asks: HashMap::new(),
            sent: Vec::new(),
        }
    }

    fn notice(&mut self, core: &mut Core, level: Level, text: impl Into<String>) {
        self.seq += 1;
        core.upsert(&format!("agy-notice-{}-{}", Core::now(), self.seq), None, Body::Notice { level, text: text.into() });
    }

    fn busy(&self) -> bool {
        self.turn.is_some()
    }

    pub fn start(&mut self, core: &mut Core) {
        core.meta.features = FEATURES.iter().map(|s| (*s).to_owned()).collect();
        core.meta.modes = modes();
        core.meta.efforts = EFFORTS.iter().map(|e| ControlOption { id: (*e).to_owned(), label: capitalize(e), description: String::new(), dangerous: false }).collect();
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
        // Its models and version, from its CLI.
        core.effects.push(Effect::Run { job: MODELS_JOB.to_owned(), args: vec!["models".to_owned()] });
        core.effects.push(Effect::Run { job: VERSION_JOB.to_owned(), args: vec!["--version".to_owned()] });
    }

    pub fn exited(&mut self, core: &mut Core, code: Option<i32>, error: Option<String>) {
        // Calls kiln held went with its process.
        self.held.clear();
        for id in std::mem::take(&mut self.asks).into_keys() {
            core.take_approval(&id);
        }
        self.sent.clear();
        self.close_text(core);
        self.turn = None;
        self.ready = false;
        let error = error.map(|e| {
            // Older versions have no multi-turn mode: say what it needs.
            if e.contains("input-format") { format!("{e}\nkiln needs agy 1.4 or newer (`agy update`).") } else { e }
        });
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
            Command::MailHandoff { client, .. } => { let epoch = core.state.epoch; core.refuse_owner_mail(None, epoch); core.reply(client, serde_json::json!({"error":{"code":super::state_view::Error::Unreadable}})); },
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
            // Nothing gets into a turn under way: it stays queued.
            Command::QueueSteer { .. } => {}
            // It never asks itself: what it asks is kiln's hook's, a call a person sees first.
            Command::Approve { id, decision, message, .. } => self.answer(core, &id, decision, message),
            Command::Interrupt => {
                if self.busy() {
                    self.stop(core);
                } else {
                    core.touch_state();
                }
            }
            Command::SetMode { mode } => {
                if mode == core.state.mode || !modes().iter().any(|m| m.id == mode) {
                    return;
                }
                if mode == BYPASS && !self.allow_bypass {
                    self.bypass_next = true;
                }
                let label = modes().into_iter().find(|m| m.id == mode).map(|m| m.label).unwrap_or_default();
                core.state.mode = mode;
                core.touch_state();
                self.reload_for(core, label);
            }
            Command::SetModel { model, .. } => {
                if model == core.state.model {
                    return;
                }
                let label = core.meta.models.iter().find(|m| m.id == model).map(|m| m.label.clone()).unwrap_or_else(|| model.clone());
                core.state.model = model;
                core.touch_state();
                self.reload_for(core, label);
            }
            Command::SetEffort { effort } => {
                if effort == core.state.effort || !EFFORTS.contains(&effort.as_str()) {
                    return;
                }
                core.state.effort = effort.clone();
                core.touch_state();
                self.reload_for(core, format!("{} effort", capitalize(&effort)));
            }
            // The Antigravity CLI has no faster speed (`meta.speeds` stays empty).
            Command::SetSpeed { .. } => {}
            Command::Suggest { client, .. } => core.reply(client, json!({"suggestions": []})),
            Command::Control { id, value } if id == "export" => {
                let client = value.as_ref().and_then(|v| v.get("client")).and_then(Value::as_str).unwrap_or("").to_owned();
                core.reply(client, json!({"title": "Conversation", "text": core.export(), "format": "markdown", "filename": "conversation.md"}));
            }
            // Its go-back is kiln's (`harness::goback`), Antigravity's own ending at export.
            Command::Control { id, value } => {
                super::goback::control(core, &id, value.as_ref());
            }
            Command::StopTask { .. } | Command::StopTasks | Command::Restart | Command::ForceStop | Command::Snapshot | Command::MoveTo { .. } | Command::Moving { .. } | Command::HandedOver { .. } | Command::OwnEdits { .. } | Command::RemoveTask { .. } => {}
            // One of its subagents, held at its calls through kiln's hook.
            Command::Pause { on, task: Some(task) } => self.pause_subagent(core, &task, on),
            Command::Perms { perms, task: Some(task) } => self.set_task_perms(core, &task, perms),
            // A pause's commands are the session's (`harness::pause`).
            Command::Resume | Command::AutoResume { .. } | Command::PauseAt { .. } | Command::MaxSubagents { .. } | Command::MoveOnLimit { .. } | Command::LimitHold { .. } | Command::Pause { .. } | Command::Perms { .. } | Command::Branch { .. } => {}
            // Its own commands and skills run as "/name args" messages.
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
            Command::StateRead { client, .. } => core.reply(client, json!({"error": {"code": super::state_view::Error::Unreadable}})),
            Command::AtState { client, .. } => core.reply(client, json!({"error": {"code": super::state_view::Error::Unreadable}})),
            Command::McpAdd { client, .. } | Command::McpRemove { client, .. } => match client.filter(|c| !c.is_empty()) {
                Some(c) => core.reply(c, json!({"ok": false, "error": UNSUPPORTED})),
                None => self.notice(core, Level::Warn, UNSUPPORTED),
            },
            Command::McpEnable { .. } | Command::McpReconnect { .. } => self.notice(core, Level::Warn, UNSUPPORTED),
            Command::McpRefresh => {}
            Command::SkillsReload => core.skills_changed(),
        }
    }

    /// A setting it takes when it starts: from its next start, which is now if nothing is under way.
    fn reload_for(&mut self, core: &mut Core, what: String) {
        self.reload = Some(what.clone());
        if self.busy() {
            self.notice(core, Level::Info, format!("{what} from the next turn: Antigravity starts again with it once this one ends (the conversation goes on)"));
        }
        self.reload_when_idle(core);
    }

    fn reload_when_idle(&mut self, core: &mut Core) {
        if self.reload.is_none() || self.busy() || core.state.status == Status::Starting {
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
        self.notice(core, Level::Info, format!("{what}: Antigravity starts again with it (the conversation goes on)"));
        self.restart(core);
    }

    /// Every few seconds: a setting waiting for the turn to end.
    pub fn tick(&mut self, core: &mut Core) {
        self.reload_when_idle(core);
    }

    /// Its CLI's answers: the models it has, its version.
    pub fn run_done(&mut self, core: &mut Core, job: &str, code: Option<i32>, output: &str) {
        if code != Some(0) {
            return;
        }
        match job {
            MODELS_JOB => {
                let models = parse_models(output);
                if !models.is_empty() {
                    core.meta.models = models;
                    core.touch_meta();
                }
            }
            VERSION_JOB => {
                let v = output.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_owned();
                if !v.is_empty() {
                    core.meta.version = v;
                    core.touch_meta();
                    self.status_rows(core, None);
                }
            }
            _ => {}
        }
    }

    /// A stop: agy can't be told one, so its process ends (it keeps the turn so far) and kiln starts
    /// it again on the same conversation.
    fn stop(&mut self, core: &mut Core) {
        let started = core.state.turn_started;
        if let Some(id) = self.turn.take() {
            core.state.delivery.insert(id, Delivery::Cancelled);
        }
        self.close_text(core);
        self.finish_tools(core, ToolStatus::Cancelled);
        core.end_turn(started, "stopped", Some("interrupted"));
        core.state.turn_started = None;
        core.state.doing = None;
        core.touch_state();
        self.notice(core, Level::Info, "Interrupted: Antigravity starts again on the same conversation");
        self.restart(core);
    }

    /// End its process and start it again (the session does), on the same conversation.
    fn restart(&mut self, core: &mut Core) {
        self.restarting = true;
        core.effects.push(Effect::Restart { resume_at: None });
    }

    fn send(&mut self, core: &mut Core, mut m: QueueItem, mode: SendMode) {
        m.text = m.text.trim_end().to_owned();
        if m.text.is_empty() && m.attachments.is_empty() {
            return;
        }
        if core.state.goal.is_empty() {
            core.state.goal = clip_middle(&m.text, 600).0;
        }
        if self.busy() && mode == SendMode::Now {
            // First in line, then the turn under way is stopped (it goes out once agy is back).
            core.state.queue.insert(0, m);
            core.touch_state();
            self.stop(core);
            return;
        }
        if self.busy() || !self.ready || self.reload.is_some() || self.restarting {
            core.state.queue.push(m);
            core.touch_state();
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

    fn prompt(&mut self, core: &mut Core, m: QueueItem) {
        self.close_text(core);
        // It takes text only: files go as their paths on this machine, which it reads itself. The
        // conversation so far comes first, once, when it went on here from another machine or
        // another harness.
        let mut text = match self.context.take() {
            Some(c) => format!("{c}\n\n{}", m.text),
            None => m.text.clone(),
        };
        for a in &m.attachments {
            if let Some(p) = &a.path {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(&format!("[Attached: {p}]"));
            }
        }
        core.write(&json!({"event": "user", "message": {"content": text}}));
        self.turn = Some(m.id.clone());
        self.said = false;
        self.tools.clear();
        core.state.delivery.insert(m.id.clone(), Delivery::Started);
        let (images, names) = super::model::attachment_summary(&m.attachments);
        core.note_sent(&m.id, &m.text);
        core.upsert(&m.id, None, Body::User { text: m.text.clone(), images, client_id: Some(m.id.clone()), origin: Some(super::core::origin_of(&m.id).into()), attachments: names, reply_to: m.reply_to.clone() });
        core.state.turn_started = Some(Core::now());
        core.state.doing = Some("Thinking".to_owned());
        core.settle_status(true);
        core.touch_state();
    }

    pub(crate) fn next(&mut self, core: &mut Core) {
        if self.busy() || !self.ready || self.reload.is_some() || self.restarting || core.state.status == Status::Exited {
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
        if p.cut.is_some() && !self.busy() && core.state.status != Status::Exited {
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

    // ------------------------------------------------------------------ kiln's hook

    /// Whether kiln's hook was heard from it, so its calls come to kiln first.
    pub fn holds_calls(&self) -> bool {
        self.heard
    }

    /// How a person's message reaches its subagent `t` now: in before its next model call, while
    /// it goes on (kiln's hook gives it then); none once it ended, or when its hook isn't heard.
    pub fn takes(&self, core: &Core, t: &Task) -> (Vec<SendMode>, Option<&'static str>) {
        if !self.heard || !t.live() || matches!(core.state.status, Status::Exited | Status::Starting) {
            return (Vec::new(), None);
        }
        (vec![SendMode::Steer], None)
    }

    /// A person's message to subagent `t`: it waits in its queue, shown in its thread, and goes in
    /// at its next model call ([`Agy::hook`], PreInvocation).
    pub fn send_task(&mut self, core: &mut Core, t: &Task, m: QueueItem, _mode: SendMode) {
        super::subagent::show(core, &t.id, &m, Delivery::Queued);
        super::subagent::hold(core, &t.id, m);
    }

    /// kiln's hook, in one of its conversations (`input`: its hook input, as `kiln harness-hook
    /// --agy` hands it on, with `kilnEvent` and `kilnTokens`): before a tool call, a paused
    /// subagent's waits until it's resumed, and what a person lets it do (or the agent, for its own)
    /// refuses it, asks them first, or lets agy decide; before a model call, what waits for a
    /// subagent goes in; at a stop, a subagent's work is done. The answer goes to `reply`.
    pub fn hook(&mut self, core: &mut Core, input: Value, reply: Sender<Value>) {
        let event = input.get("kilnEvent").and_then(Value::as_str).unwrap_or("").to_owned();
        self.hear(core);
        let conversation = input.get("conversationId").and_then(Value::as_str).unwrap_or("");
        // The subagent: the one whose conversation it is (none: the agent's own, or one the stream
        // didn't name, a subagent's own subagent).
        let task = core.state.tasks.iter().find(|t| t.subagent() && t.id == conversation).map(|t| t.id.clone());
        if let (Some(id), Some(n)) = (task.as_deref(), input.get("kilnTokens").and_then(Value::as_u64))
            && let Some(t) = core.state.tasks.iter_mut().find(|t| t.id == id)
            && t.tokens != n
        {
            t.tokens = n;
            core.touch_state();
        }
        match event.as_str() {
            "PreToolUse" => {
                if let Some(id) = task.as_deref() {
                    self.went_on(core, id);
                    let name = input.pointer("/toolCall/name").and_then(Value::as_str).unwrap_or("tool").to_owned();
                    if let Some(t) = core.state.tasks.iter_mut().find(|t| t.id == id) {
                        t.tool_uses += 1;
                        t.last_tool = Some(name);
                        if t.status != "running" {
                            t.status = "running".into();
                            t.ended = None;
                        }
                        core.touch_state();
                    }
                }
                self.before_call(core, task, input, reply);
            }
            "PreInvocation" => {
                let mut steps = Vec::new();
                if let Some(id) = task.as_deref() {
                    self.went_on(core, id);
                    while let Some(m) = super::subagent::next(core, id) {
                        super::subagent::delivered(core, &m.id, Delivery::Started);
                        self.sent.push((id.to_owned(), m.id.clone()));
                        steps.push(json!({"userMessage": message_words(&m)}));
                    }
                }
                let _ = reply.send(if steps.is_empty() { json!({}) } else { json!({"injectSteps": steps}) });
            }
            "Stop" => {
                if let Some(id) = task.as_deref() {
                    self.went_on(core, id);
                    if let Some(t) = core.state.tasks.iter_mut().find(|t| t.id == id && t.live()) {
                        t.status = "completed".into();
                        t.ended = Some(Core::now());
                        t.paused_at = None;
                        core.touch_state();
                    }
                }
                let _ = reply.send(hook_pass("Stop"));
            }
            other => {
                let _ = reply.send(hook_pass(other));
            }
        }
    }

    /// kiln's hook was heard: its subagents can be paused and limited from now on.
    fn hear(&mut self, core: &mut Core) {
        if self.heard {
            return;
        }
        self.heard = true;
        if !core.meta.features.iter().any(|f| f == "limits") {
            core.meta.features.push("limits".into());
            core.touch_meta();
        }
    }

    /// What went in to subagent `task` before its last model call is done: it went on past it.
    fn went_on(&mut self, core: &mut Core, task: &str) {
        let (done, still): (Vec<_>, Vec<_>) = std::mem::take(&mut self.sent).into_iter().partition(|(t, _)| t == task);
        self.sent = still;
        for (_, id) in done {
            super::subagent::delivered(core, &id, Delivery::Completed);
        }
    }

    /// A tool call in subagent `task` (none: the agent's own): held while it's paused, else as what
    /// a person lets it do says.
    fn before_call(&mut self, core: &mut Core, task: Option<String>, input: Value, reply: Sender<Value>) {
        if let Some(t) = task.as_ref().filter(|t| super::hold::paused(core, t)) {
            self.held.push((t.clone(), input, reply));
            return;
        }
        let name = input.pointer("/toolCall/name").and_then(Value::as_str).unwrap_or("");
        let args = input.pointer("/toolCall/args").cloned().unwrap_or(Value::Null);
        let (kind, paths) = hook_call(name, &args);
        let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
        match super::hold::verdict(core, task.as_deref(), kind, &paths) {
            None => {
                let _ = reply.send(hook_pass("PreToolUse"));
            }
            Some(("ask", why)) => self.ask_hook(core, task.as_deref(), name, &args, kind, why, reply),
            Some((_, why)) => {
                let _ = reply.send(json!({"decision": "deny", "reason": why}));
            }
        }
    }

    /// A call a person sees first: kiln's own approval (headless agy can't ask), held until they
    /// answer it (`answer`).
    #[allow(clippy::too_many_arguments)]
    fn ask_hook(&mut self, core: &mut Core, task: Option<&str>, name: &str, args: &Value, tool: ToolKind, why: String, reply: Sender<Value>) {
        self.seq += 1;
        let id = format!("kiln-ask-{}-{}", Core::now(), self.seq);
        let call = tool_call(&id, name, args, &core.state.cwd);
        let command = (tool == ToolKind::Shell).then(|| call.title.clone());
        let approval = Approval {
            id: id.clone(),
            tool_id: None,
            tool_name: call.name.clone(),
            tool,
            title: call.title.clone(),
            detail: call.detail.clone(),
            input: clip_json(args, 64 * 1024),
            command,
            cwd: args.get("Cwd").and_then(Value::as_str).unwrap_or(core.state.cwd.as_str()).to_owned(),
            diffs: Vec::new(),
            reason: Some(why),
            always: Vec::new(),
            default_to_no: false,
            questions: Vec::new(),
            plan: None,
            agent: task.map(|id| core.state.tasks.iter().find(|t| t.id == id).map_or_else(|| id.to_owned(), |t| t.description.clone())),
            at: Core::now(),
        };
        self.asks.insert(id, reply);
        core.state.doing = Some("Waiting for your approval".to_owned());
        core.add_approval(approval);
        core.settle_status(self.busy());
        core.touch_state();
    }

    /// A person answered a call kiln held for them: a yes lets it go even where agy itself would
    /// deny it (`auto_approve`), a no refuses it with their words.
    fn answer(&mut self, core: &mut Core, id: &str, decision: Decision, message: Option<String>) {
        let Some(reply) = self.asks.remove(id) else { return };
        core.take_approval(id);
        let why = message.filter(|m| !m.trim().is_empty()).unwrap_or_else(|| "A person said no to this call.".to_owned());
        let _ = reply.send(if decision == Decision::Deny { json!({"decision": "deny", "reason": why}) } else { json!({"decision": "auto_approve"}) });
        if core.state.approvals.is_empty() && self.busy() {
            core.state.doing = Some("Working".to_owned());
        }
        core.settle_status(self.busy());
        core.touch_state();
    }

    /// A person pauses subagent `task` (its next tool call, and those of the subagents under it,
    /// wait for their answer), or resumes it: what waited goes on, unless a pause above it holds it.
    fn pause_subagent(&mut self, core: &mut Core, task: &str, on: bool) {
        if on && !self.heard {
            self.notice(core, Level::Warn, "Not paused: kiln's hook hasn't been heard from Antigravity, so its calls can't be held.");
            return;
        }
        if !super::hold::pause(core, task, on) {
            self.notice(core, Level::Warn, "Not paused: it has no such subagent.");
            return;
        }
        if !on {
            self.release_held(core);
        }
    }

    /// What a person lets subagent `task` do (none: no limits of its own), within what it's under.
    fn set_task_perms(&mut self, core: &mut Core, task: &str, perms: Option<Perms>) {
        if !super::hold::set_perms(core, task, perms) {
            self.notice(core, Level::Warn, "Not limited: it has no such subagent.");
        }
    }

    /// The calls held for a pause that no longer holds them go on, as what each may do says.
    fn release_held(&mut self, core: &mut Core) {
        let (still, free): (Vec<_>, Vec<_>) = std::mem::take(&mut self.held).into_iter().partition(|(task, _, _)| super::hold::paused(core, task));
        self.held = still;
        for (task, input, reply) in free {
            self.before_call(core, Some(task), input, reply);
        }
    }

    /// The subagents a `subagent` step names: each a task of its own, under the call that started it.
    fn spawned(&mut self, core: &mut Core, call: &str, info: &Value) {
        for s in info.get("subagents").and_then(Value::as_array).into_iter().flatten() {
            let Some(id) = s.get("conversation_id").and_then(Value::as_str).filter(|c| !c.is_empty()) else { continue };
            let str_of = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
            if core.state.tasks.iter().any(|t| t.id == id) {
                continue;
            }
            let (role, kind, prompt) = (str_of("role"), str_of("type_name"), str_of("initial_prompt"));
            let description = if role.is_empty() { clip_middle(&prompt, 300).0 } else { clip_middle(&role, 300).0 };
            core.state.tasks.push(Task {
                id: id.to_owned(),
                tool_id: Some(call.to_owned()),
                description,
                kind: if kind.is_empty() { "subagent".into() } else { kind },
                class: Some("agent".into()),
                status: "running".into(),
                // It goes on beside the call that started it.
                background: true,
                depth: 1,
                prompt: (!prompt.is_empty()).then_some(prompt),
                output: s.get("log_uri").and_then(Value::as_str).map(|u| u.trim_start_matches("file://").to_owned()),
                started: Core::now(),
                ..Task::default()
            });
            core.touch_state();
        }
    }

    /// Subagents its agent ended (`manage_subagents` kill): stopped.
    fn killed(&mut self, core: &mut Core, params: &Value) {
        let all = params.get("Action").and_then(Value::as_str) == Some("kill_all");
        let ids: Vec<String> = params.get("ConversationIds").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
        for t in core.state.tasks.iter_mut().filter(|t| t.subagent() && t.live() && (all || ids.contains(&t.id))) {
            t.status = "stopped".into();
            t.ended = Some(Core::now());
            t.paused_at = None;
        }
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
        match v.get("event").and_then(Value::as_str).unwrap_or("") {
            "init" => self.init(core, &v),
            "step_update" => self.step(core, v.get("step_update").unwrap_or(&Value::Null)),
            "result" => self.result(core, v.get("result").unwrap_or(&Value::Null)),
            _ => {}
        }
    }

    fn init(&mut self, core: &mut Core, v: &Value) {
        if !self.ready { core.skills_provider_ready(); }
        let init = v.get("init").unwrap_or(&Value::Null);
        if let Some(c) = v.get("conversation_id").or_else(|| init.get("conversation_id")).and_then(Value::as_str) {
            self.set_conversation(core, c);
        }
        if let Some(m) = init.get("model").and_then(Value::as_str).filter(|m| !m.is_empty()) {
            core.state.model = m.to_owned();
        }
        self.ready = true;
        self.status_rows(core, init.get("permission_mode").and_then(Value::as_str));
        if !self.busy() {
            core.set_status(Status::Idle);
        }
        core.touch_state();
        // A setting chosen while it started: it starts again with it before anything is sent.
        self.reload_when_idle(core);
        self.next(core);
    }

    fn set_conversation(&mut self, core: &mut Core, c: &str) {
        if self.conversation.as_deref() == Some(c) {
            return;
        }
        core.skills_provider_ready();
        if let Some(old) = self.resumed.take().filter(|old| old != c) {
            self.notice(core, Level::Warn, format!("Antigravity couldn't find its conversation {old} on this machine, so this is a new one (what it said before isn't in it)"));
        }
        self.conversation = Some(c.to_owned());
        self.tag = c.chars().filter(char::is_ascii_alphanumeric).take(8).collect();
        core.state.harness_session = Some(c.to_owned());
        core.touch_state();
    }

    fn status_rows(&mut self, core: &mut Core, policy: Option<&str>) {
        let mut rows = vec![StatusItem { group: "Agent".into(), label: "Name".into(), value: "Antigravity CLI".into(), tone: None }];
        if !core.meta.version.is_empty() {
            rows.push(StatusItem { group: "Agent".into(), label: "Version".into(), value: core.meta.version.clone(), tone: None });
        }
        rows.push(StatusItem { group: "Agent".into(), label: "Protocol".into(), value: "Headless stream-json".into(), tone: None });
        let keep = core.state.harness_status.iter().find(|r| r.label == "Commands").cloned();
        if let Some(p) = policy.map(str::to_owned).or_else(|| keep.map(|r| r.value)) {
            rows.push(StatusItem { group: "Permissions".into(), label: "Commands".into(), value: p, tone: None });
        }
        rows.push(StatusItem { group: "Permissions".into(), label: String::new(), value: "It can't ask from here: what needs your approval is denied, and kiln says what".into(), tone: None });
        core.state.harness_status = rows;
        core.touch_state();
    }

    fn step(&mut self, core: &mut Core, s: &Value) {
        if let Some(c) = s.get("conversation_id").and_then(Value::as_str) {
            self.set_conversation(core, c);
        }
        let Some(index) = s.get("step_index").and_then(Value::as_u64) else { return };
        let state = s.get("state").and_then(Value::as_str).unwrap_or("");
        let done = state != "ACTIVE";
        match s.get("step_type").and_then(Value::as_str).unwrap_or("") {
            "agent_response" => {
                if let Some(u) = s.get("usage") {
                    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                    core.state.usage.input_tokens += n("input_tokens");
                    core.state.usage.output_tokens += n("output_tokens");
                    core.state.usage.cache_read_tokens += n("cache_read_tokens");
                }
                let delta = s.get("text_delta").and_then(Value::as_str).unwrap_or("");
                if !delta.is_empty() {
                    let id = match &self.text {
                        Some((i, id)) if *i == index => id.clone(),
                        _ => {
                            self.close_text(core);
                            let id = format!("agy-{}-{index}", self.tag);
                            core.upsert(&id, None, Body::Assistant { text: String::new(), streaming: true });
                            self.text = Some((index, id.clone()));
                            id
                        }
                    };
                    core.append(&id, delta);
                    self.said = true;
                    if let Some(Body::Assistant { text, .. }) = core.get(&id).map(|e| &e.body) {
                        core.state.summary = clip_middle(text.trim(), 600).0;
                    }
                    core.state.doing = Some("Writing".to_owned());
                }
                if done {
                    self.close_text(core);
                    // What comes next is thought out first.
                    if self.busy() {
                        core.state.doing = Some("Thinking".to_owned());
                    }
                }
                core.touch_state();
            }
            // A tool call; the subagents one started (`invoke_subagent`) end it as a `subagent` step.
            "tool" | "subagent" => {
                self.close_text(core);
                let info = s.get("tool_info").unwrap_or(&Value::Null);
                let name = s.get("tool_name").or_else(|| info.get("name")).and_then(Value::as_str).unwrap_or("tool").to_owned();
                let id = self.tools.entry(index).or_insert_with(|| format!("agy-{}-{index}", self.tag)).clone();
                let mut call = match core.get(&id).map(|e| &e.body) {
                    Some(Body::Tool(t)) => t.as_ref().clone(),
                    _ => tool_call(&id, &name, info.get("parameters").unwrap_or(&Value::Null), &core.state.cwd),
                };
                if let Some(out) = info.get("output").and_then(Value::as_str).filter(|o| !o.is_empty()) {
                    // kiln's own tools' answers as the conversation shows them (`crate::reach::shown`):
                    // a page's on one line, a command's output, exit code and machine.
                    let shown = crate::reach::shown(&call.name, &Value::String(out.to_owned()));
                    let (text, cut) = clip_middle(shown.as_ref().map_or(out, |s| s.output.as_str()), OUTPUT_MAX);
                    call.output = Some(text);
                    call.truncated = cut;
                    if let Some(s) = shown {
                        call.exit_code = s.exit_code.or(call.exit_code);
                        call.runner = s.runner.or(call.runner);
                    }
                }
                if let Some(code) = info.get("exit_code").and_then(Value::as_i64) {
                    call.exit_code = i32::try_from(code).ok();
                }
                call.status = match state {
                    "ACTIVE" => ToolStatus::Running,
                    "DONE" => ToolStatus::Done,
                    "CANCELED" | "CANCELLED" => ToolStatus::Cancelled,
                    _ => ToolStatus::Error,
                };
                if name == "mcp__kiln__agents_wait" && call.status == ToolStatus::Done && let Some(result) = info.get("output").and_then(Value::as_str).filter(|text| text.len() <= super::state_view::FRAME).and_then(|text| serde_json::from_str::<Value>(text).ok()) { core.observe_handoff_result(&id, &result); }
                if done {
                    call.ended = Some(Core::now());
                    if self.busy() {
                        core.state.doing = Some("Thinking".to_owned());
                    }
                } else {
                    core.state.doing = Some(super::claude::doing(&call));
                }
                let kill = (done && call.status == ToolStatus::Done && name == "manage_subagents").then(|| call.input.clone());
                core.upsert(&id, None, Body::Tool(Box::new(call)));
                core.touch_state();
                if let Some(info) = s.get("subagent_info") {
                    self.spawned(core, &id, info);
                }
                if let Some(params) = kill {
                    self.killed(core, &params);
                }
            }
            // Its own input (shown as kiln sent it) and its own notes to itself.
            _ => {}
        }
    }

    fn result(&mut self, core: &mut Core, r: &Value) {
        self.close_text(core);
        // A turn kiln stopped has ended already (its process is being ended).
        let Some(id) = self.turn.take() else { return };
        let status = r.get("status").and_then(Value::as_str).unwrap_or("SUCCESS").to_owned();
        let error = r.get("error").and_then(Value::as_str).unwrap_or("").to_owned();
        // Nothing streamed (an older version): its words from the result.
        let response = r.get("response").and_then(Value::as_str).unwrap_or("").trim().to_owned();
        if !self.said && !response.is_empty() {
            self.seq += 1;
            core.upsert(&format!("agy-{}-reply-{}", self.tag, self.seq), None, Body::Assistant { text: response.clone(), streaming: false });
            core.state.summary = clip_middle(&response, 600).0;
        }
        // What it wasn't allowed to do: denied, as it can't ask.
        let denied: Vec<String> = r
            .get("denied_actions")
            .and_then(Value::as_array)
            .map(|d| d.iter().filter_map(|x| x.get("display_name").or_else(|| x.get("action")).and_then(Value::as_str).map(str::to_owned)).collect())
            .unwrap_or_default();
        if !denied.is_empty() {
            let open: Vec<String> = self.tools.values().cloned().collect();
            for tid in open {
                if let Some(e) = core.get_mut(&tid)
                    && let Body::Tool(t) = &mut e.body
                    && t.status == ToolStatus::Error
                    && t.output.is_none()
                {
                    t.status = ToolStatus::Denied;
                }
            }
            let what = denied.join(", ");
            let modes = match core.state.mode.as_str() {
                ACCEPT_EDITS => "Bypass lets it",
                PLAN => "Accept edits or Bypass let it",
                _ => "Accept edits (for edits) or Bypass let it",
            };
            self.notice(core, Level::Warn, format!("Antigravity wasn't allowed to {what}: it can't ask from here, so it was denied. {modes} go ahead (⋯ → mode)."));
        }
        let interrupted = error.eq_ignore_ascii_case("interrupted");
        let (how, why) = match status.as_str() {
            "SUCCESS" if !denied.is_empty() => ("blocked", "denied".to_owned()),
            "SUCCESS" => ("done", "success".to_owned()),
            _ if interrupted => ("stopped", "interrupted".to_owned()),
            s if s.contains("CANCEL") => ("stopped", s.to_lowercase()),
            s if s.contains("LIMIT") || s.contains("MAX") => ("limit", s.to_lowercase()),
            s => ("cut", s.to_lowercase()),
        };
        self.finish_tools(core, if how == "stopped" { ToolStatus::Cancelled } else { ToolStatus::Error });
        if how == "cut" && !error.is_empty() {
            if super::pause::says_limit(&error) {
                // The account's usage limit: paused until it resets (agy can't say when; kiln
                // checks back now and then).
                let hit = super::pause::Hit::from_limits(&core.state.limits, error.clone(), None, Core::now());
                let cut = super::pause::cut_now(core);
                super::pause::hold(core, super::pause::LIMIT, "harness", hit, Some(cut));
            } else {
                self.notice(core, Level::Error, clip_middle(&error, 2000).0);
            }
        }
        if status == "SUCCESS" { core.input_consumed(&id, super::admission::Consumption::ProviderCompleted); }
        core.state.delivery.insert(id, if how == "stopped" { Delivery::Cancelled } else { Delivery::Completed });
        core.state.usage.turns += 1;
        core.end_turn(core.state.turn_started, how, Some(&why));
        core.state.turn_started = None;
        core.state.doing = None;
        core.settle_status(false);
        core.touch_state();
        self.reload_when_idle(core);
        self.next(core);
    }

    /// The streamed reply stops growing.
    fn close_text(&mut self, core: &mut Core) {
        if let Some((_, id)) = self.text.take()
            && let Some(e) = core.get_mut(&id)
            && let Body::Assistant { streaming, .. } = &mut e.body
        {
            *streaming = false;
        }
    }

    /// Calls the turn left running ended with it.
    fn finish_tools(&mut self, core: &mut Core, how: ToolStatus) {
        for tid in self.tools.values() {
            if let Some(e) = core.get_mut(tid)
                && let Body::Tool(t) = &mut e.body
                && matches!(t.status, ToolStatus::Running | ToolStatus::Pending)
            {
                t.status = how;
                t.ended = Some(Core::now());
            }
        }
    }
}

/// `agy models`: one "id<TAB>label" a line (anything else it prints is not a model).
fn parse_models(output: &str) -> Vec<ModelOption> {
    output
        .lines()
        .filter_map(|l| {
            let (id, label) = l.split_once('\t')?;
            let id = id.trim();
            (!id.is_empty() && !id.contains(' ')).then(|| ModelOption { id: id.to_owned(), label: label.trim().to_owned(), ..ModelOption::default() })
        })
        .collect()
}

/// A tool call as it starts: what kind of tool, and its title from its input.
fn tool_call(id: &str, name: &str, params: &Value, cwd: &str) -> ToolCall {
    // An MCP server's tool (`call_mcp_tool`), named as Claude Code's and Codex's are
    // (`mcp__<server>__<tool>`: its server and own name are read from that), its input the tool's;
    // kiln's own read as theirs do (`crate::reach::describe`).
    if name == "call_mcp_tool"
        && let (Some(server), Some(tool)) = (params.get("ServerName").and_then(Value::as_str), params.get("ToolName").and_then(Value::as_str))
    {
        let args = params.get("Arguments").unwrap_or(&Value::Null);
        let (kind, title, detail) = crate::reach::describe(server, tool, args).unwrap_or_else(|| (ToolKind::Mcp, format!("{server} · {tool}"), None));
        return ToolCall {
            tool_id: id.to_owned(),
            name: format!("mcp__{server}__{tool}"),
            tool: kind,
            title,
            detail,
            input: clip_json(args, 16 * 1024),
            status: ToolStatus::Running,
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
            runner: crate::reach::remote(server, tool, args),
        };
    }
    let tool = match name {
        "run_command" | "send_command_input" | "command_status" => ToolKind::Shell,
        "write_to_file" | "replace_file_content" | "multi_replace_file_content" | "sed_file" | "notebook_edit" => ToolKind::Edit,
        "view_file" | "read_file" | "list_dir" | "read_resource" | "list_resources" => ToolKind::Read,
        "grep_search" | "find_by_name" => ToolKind::Search,
        "search_web" | "read_url_content" | "open_browser_url" | "read_browser_page" => ToolKind::Web,
        "invoke_subagent" | "browser_subagent" | "define_subagent" | "manage_subagents" => ToolKind::Agent,
        "manage_task" => ToolKind::Todo,
        "ask_question" => ToolKind::Question,
        "call_mcp_tool" => ToolKind::Mcp,
        n if n.starts_with("browser_") => ToolKind::Web,
        _ => ToolKind::Other,
    };
    let s = |k: &str| params.get(k).and_then(Value::as_str).map(str::to_owned);
    let path = PATH_KEYS.iter().find_map(|k| s(k)).map(|p| super::claude::rel(&p, cwd));
    let title = match tool {
        ToolKind::Shell => s("CommandLine").or_else(|| s("Input")).unwrap_or_else(|| name.to_owned()),
        ToolKind::Search => s("Query").or_else(|| s("Pattern")).unwrap_or_else(|| path.clone().unwrap_or_else(|| name.to_owned())),
        ToolKind::Web => s("Url").or_else(|| s("query")).or_else(|| s("Query")).unwrap_or_else(|| name.to_owned()),
        ToolKind::Agent => s("Task").or_else(|| s("Prompt")).or_else(|| s("Name")).unwrap_or_else(|| name.to_owned()),
        _ => path.clone().unwrap_or_else(|| name.to_owned()),
    };
    let detail = match tool {
        ToolKind::Search => path,
        _ => None,
    };
    ToolCall {
        tool_id: id.to_owned(),
        name: name.to_owned(),
        tool,
        title,
        detail,
        input: clip_json(params, 16 * 1024),
        status: ToolStatus::Running,
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
    }
}

/// The arguments its tools name a file or folder by.
const PATH_KEYS: &[&str] = &["TargetFile", "AbsolutePath", "DirectoryPath", "SearchPath", "SearchDirectory", "File", "Path"];

/// What kind of call a hooked tool call is, by agy's name for it, and the files it names (as its
/// stream shows it: [`tool_call`]).
fn hook_call(name: &str, args: &Value) -> (ToolKind, Vec<String>) {
    let call = tool_call("", name, args, "");
    let paths = PATH_KEYS.iter().filter_map(|k| args.get(*k).and_then(Value::as_str)).map(str::to_owned).collect();
    (call.tool, paths)
}

/// A person's message as it goes in to a subagent: its words, with the files they attached by
/// their paths on this machine (agy takes text alone).
fn message_words(m: &QueueItem) -> String {
    let mut text = m.text.trim_end().to_owned();
    for p in m.attachments.iter().filter_map(|a| a.path.as_deref()) {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&format!("[Attached: {p}]"));
    }
    text
}

/// The hook events kiln puts in agy's hooks.json ([`hooks_with`]).
pub const HOOK_EVENTS: &[&str] = &["PreToolUse", "PreInvocation", "Stop"];

/// The name of kiln's own entry in agy's hooks.json.
pub const HOOK_NAME: &str = "kiln";

/// How long agy waits for kiln's hook before a tool call: a week, as a person may hold one that
/// long (past it, agy fails the call: it doesn't go ahead).
pub const HOOK_FOR_S: u64 = 7 * 24 * 3600;

/// What lets a call, a model call or a stop go as agy would without kiln (its hook's answer when no
/// kiln holds anything): a tool call is left to its own rules (`allow`), the rest changes nothing.
pub fn hook_pass(event: &str) -> Value {
    match event {
        "PreToolUse" => json!({"decision": "allow"}),
        "Stop" => json!({"decision": ""}),
        _ => json!({}),
    }
}

/// agy's hooks.json (`config`, the person's own hooks in it kept) with kiln's entry for this kiln
/// binary (`kiln`): each event's handler `kiln harness-hook --agy <event>`, run as agy runs a hook
/// command, through a shell ("The shell command to execute"). The entry is the person's file's
/// once written, so it outlives this binary (a version pruned, kiln uninstalled): where the binary
/// is gone it answers agy's pass ([`hook_pass`]) itself, as a hook that failed would fail the call.
/// None: it has it already.
pub fn hooks_with(config: &Value, kiln: &str) -> Option<Value> {
    let program = format!("'{}'", kiln.replace('\'', "'\\''"));
    let mut entry = serde_json::Map::new();
    for ev in HOOK_EVENTS {
        let command = format!("k={program}; if [ -x \"$k\" ]; then exec \"$k\" harness-hook --agy {ev}; else echo '{}'; fi", hook_pass(ev));
        let handler = if *ev == "PreToolUse" { json!({"type": "command", "command": command, "timeout": HOOK_FOR_S}) } else { json!({"type": "command", "command": command}) };
        entry.insert((*ev).to_owned(), if *ev == "PreToolUse" { json!([{"matcher": "*", "hooks": [handler]}]) } else { json!([handler]) });
    }
    let entry = Value::Object(entry);
    if config.get(HOOK_NAME) == Some(&entry) {
        return None;
    }
    let mut next = config.as_object().cloned().unwrap_or_default();
    next.insert(HOOK_NAME.to_owned(), entry);
    Some(Value::Object(next))
}

/// What hook `event`'s input is, handed on by `kiln harness-hook --agy`: agy's own (it names no
/// event), with the event and the tokens its conversation used so far, from its transcript
/// (`transcript`, JSON lines: each model call's `input_tokens` and `output_tokens`, numbers or
/// strings).
pub fn hook_input(mut input: Value, event: &str, transcript: Option<&str>) -> Value {
    if !input.is_object() {
        input = json!({});
    }
    if let Some(o) = input.as_object_mut() {
        o.insert("kilnEvent".into(), json!(event));
        if let Some(t) = transcript {
            o.insert("kilnTokens".into(), json!(transcript_tokens(t)));
        }
    }
    input
}

/// The tokens a transcript's model calls used.
fn transcript_tokens(t: &str) -> u64 {
    let n = |v: Option<&Value>| match v {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    };
    t.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).map(|v| n(v.get("input_tokens")) + n(v.get("output_tokens"))).sum()
}

/// "xhigh" → "Xhigh".
fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(core: &mut Core) -> Vec<Value> {
        core.take_writes().iter().filter_map(|w| serde_json::from_str(w).ok()).collect()
    }

    fn feed(a: &mut Agy, core: &mut Core, lines: &[Value]) {
        for l in lines {
            a.line(core, &l.to_string());
        }
    }

    const CID: &str = "cb7d060c-1cd6-45dd-b38b-d510bb1f6356";

    fn step(i: u64, state: &str, kind: &str, extra: Value) -> Value {
        let mut s = json!({"conversation_id": CID, "step_index": i, "state": state, "step_type": kind});
        if let (Some(o), Some(e)) = (s.as_object_mut(), extra.as_object()) {
            o.extend(e.clone());
        }
        json!({"event": "step_update", "step_update": s})
    }

    fn started(spec: &Spec) -> (Agy, Core) {
        let mut core = Core::new();
        core.state.cwd = "/w".into();
        let mut a = Agy::new(spec);
        a.start(&mut core);
        feed(&mut a, &mut core, &[json!({"event": "init", "conversation_id": CID, "init": {"model": "gemini-3.8-flash-low", "cwd": "/w", "tools": ["run_command"], "permission_mode": "always-proceed"}})]);
        (a, core)
    }

    fn send(a: &mut Agy, core: &mut Core, id: &str, text: &str, mode: SendMode) {
        a.command(core, Command::Send { id: id.into(), text: text.into(), mode, attachments: vec![], overrides: None, reply_to: None, task: None});
    }

    #[test]
    fn its_flags_follow_the_session() {
        let spec = Spec { resume: Some(CID.into()), model: Some("gemini-3.8-flash-high".into()), effort: Some("xhigh".into()), mode: Some(BYPASS.into()), allow_bypass: true, ..Spec::default() };
        let a = args(&spec);
        assert!(a.starts_with(&["--input-format", "stream-json", "--output-format", "stream-json"].map(str::to_owned)), "{a:?}");
        for pair in [["--conversation", CID], ["--model", "gemini-3.8-flash-high"], ["--effort", "xhigh"]] {
            assert!(a.windows(2).any(|w| w == pair), "{pair:?} in {a:?}");
        }
        assert!(a.contains(&"--dangerously-skip-permissions".to_owned()));
        // Bypass it wasn't allowed: it keeps its checks. A new session edits files.
        assert!(!args(&Spec { mode: Some(BYPASS.into()), allow_bypass: false, ..Spec::default() }).iter().any(|x| x.contains("dangerously")));
        assert!(args(&Spec::default()).windows(2).any(|w| w == ["--mode", "accept-edits"]));
        assert!(!args(&Spec { mode: Some(ASK.into()), effort: Some("ultra".into()), ..Spec::default() }).iter().any(|x| x == "--mode" || x == "--effort"));
    }

    #[test]
    fn a_turn_streams_runs_a_tool_and_ends() {
        let (mut a, mut core) = started(&Spec::default());
        assert_eq!(core.state.harness_session.as_deref(), Some(CID));
        assert_eq!(core.state.status, Status::Idle);
        send(&mut a, &mut core, "m1", "Run it", SendMode::Queue);
        let w = written(&mut core);
        assert_eq!(w.first().and_then(|v| v.pointer("/message/content")).and_then(Value::as_str), Some("Run it"));
        assert_eq!(w.first().and_then(|v| v.get("event")).and_then(Value::as_str), Some("user"));
        assert_eq!(core.state.status, Status::Running);
        // A second message waits for the turn (nothing gets into one under way).
        send(&mut a, &mut core, "m2", "then this", SendMode::Steer);
        assert_eq!(core.state.queue.len(), 1);
        assert!(written(&mut core).is_empty());
        feed(
            &mut a,
            &mut core,
            &[
                step(0, "DONE", "user_input", json!({})),
                step(1, "DONE", "agent_response", json!({"usage": {"input_tokens": 100, "output_tokens": 66, "thinking_tokens": 50}})),
                step(2, "ACTIVE", "tool", json!({"tool_name": "run_command", "tool_info": {"name": "run_command", "parameters": {"CommandLine": "echo hi"}}})),
            ],
        );
        assert_eq!(core.state.doing.as_deref(), Some("Running echo hi"));
        feed(
            &mut a,
            &mut core,
            &[
                step(2, "DONE", "tool", json!({"tool_name": "run_command", "tool_info": {"name": "run_command", "parameters": {"CommandLine": "echo hi"}, "output": "hi\r\n"}})),
                step(3, "ACTIVE", "agent_response", json!({"text_delta": "It said "})),
                step(3, "DONE", "agent_response", json!({"text_delta": "hi.\n", "usage": {"input_tokens": 120, "output_tokens": 7}})),
            ],
        );
        let tool = core.events().find_map(|e| match &e.body {
            Body::Tool(t) => Some(t.as_ref().clone()),
            _ => None,
        });
        assert!(matches!(&tool, Some(t) if t.tool == ToolKind::Shell && t.title == "echo hi" && t.status == ToolStatus::Done && t.output.as_deref() == Some("hi\r\n")), "{tool:?}");
        assert!(core.events().any(|e| matches!(&e.body, Body::Assistant { text, streaming: false } if text == "It said hi.\n")));
        assert_eq!((core.state.usage.input_tokens, core.state.usage.output_tokens), (220, 73));
        feed(&mut a, &mut core, &[json!({"event": "result", "result": {"conversation_id": CID, "status": "SUCCESS", "response": "It said hi.\n", "num_turns": 1}})]);
        assert_eq!(core.state.last_turn.as_ref().map(|t| (t.how.as_str(), t.tools)), Some(("done", 1)));
        assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Completed));
        // The queued message went next; the reply isn't said twice.
        assert_eq!(written(&mut core).first().and_then(|v| v.pointer("/message/content")).and_then(Value::as_str), Some("then this"));
        assert_eq!(core.events().filter(|e| matches!(&e.body, Body::Assistant { .. })).count(), 1);
    }

    /// An MCP tool's call (`call_mcp_tool`) as agy 1.4.2 sends it: named and read as Claude Code's
    /// and Codex's are, kiln's own read as theirs (a page's answer on one line; its picture is
    /// agy's to keep).
    #[test]
    fn mcp_calls_read_as_the_other_harnesses_do() {
        let (mut a, mut core) = started(&Spec::default());
        send(&mut a, &mut core, "m1", "Show it", SendMode::Queue);
        let call = json!({"name": "call_mcp_tool", "parameters": {"Arguments": {"html": "<p>hi</p>", "title": "Harness check"}, "ServerName": "kiln", "ToolName": "show_page"}});
        let answer = r#"{"kiln_page":{"id":"0123456789abcdef0123456789abcdef","title":"Harness check","path":"/v/0123456789abcdef0123456789abcdef/","heights":[[390,300]]},"seen":{"said":[]}}"#;
        let mut done = call.clone();
        if let Some(o) = done.as_object_mut() {
            o.insert("output".into(), json!(format!("{answer}\n[Resource offloaded to file:///Users/ada/.gemini/antigravity-cli/brain/{CID}/.system_generated/steps/2/media_0.png]")));
        }
        let tool = |core: &Core| {
            core.events().find_map(|e| match &e.body {
                Body::Tool(t) => Some(t.as_ref().clone()),
                _ => None,
            })
        };
        feed(&mut a, &mut core, &[step(2, "ACTIVE", "tool", json!({"tool_name": "call_mcp_tool", "tool_info": call}))]);
        let t = tool(&core);
        assert!(matches!(&t, Some(t) if t.name == "mcp__kiln__show_page" && t.server.as_deref() == Some("kiln") && t.tool_short.as_deref() == Some("show_page") && t.title == "Harness check" && t.input == json!({"html": "<p>hi</p>", "title": "Harness check"})), "{t:?}");
        feed(&mut a, &mut core, &[step(2, "DONE", "tool", json!({"tool_name": "call_mcp_tool", "tool_info": done}))]);
        let t = tool(&core);
        let shown: Option<Value> = t.as_ref().and_then(|t| t.output.as_deref()).and_then(|o| serde_json::from_str(o).ok());
        assert_eq!(shown.as_ref().and_then(|v| v.pointer("/kiln_page/id")).and_then(Value::as_str), Some("0123456789abcdef0123456789abcdef"), "{t:?}");
        assert!(matches!(&t, Some(t) if t.status == ToolStatus::Done));
        // Another server's: its server and tool as it said them.
        feed(&mut a, &mut core, &[step(3, "ACTIVE", "tool", json!({"tool_name": "call_mcp_tool", "tool_info": {"name": "call_mcp_tool", "parameters": {"Arguments": {"q": "x"}, "ServerName": "github", "ToolName": "search_issues"}}}))]);
        assert!(core.events().any(|e| matches!(&e.body, Body::Tool(t) if t.name == "mcp__github__search_issues" && t.title == "github · search_issues" && t.server.as_deref() == Some("github"))));
    }

    #[test]
    fn messages_wait_for_it_to_start() {
        let mut core = Core::new();
        let mut a = Agy::new(&Spec::default());
        a.start(&mut core);
        assert!(core.effects.iter().any(|e| matches!(e, Effect::Run { args, .. } if args == &["models"])));
        send(&mut a, &mut core, "m1", "hello", SendMode::Queue);
        assert!(written(&mut core).is_empty());
        assert_eq!(core.state.queue.len(), 1);
        feed(&mut a, &mut core, &[json!({"event": "init", "conversation_id": CID, "init": {}})]);
        assert_eq!(written(&mut core).first().and_then(|v| v.pointer("/message/content")).and_then(Value::as_str), Some("hello"));
        assert!(core.state.queue.is_empty());
    }

    /// A conversation carried in (from another harness, or one that moved here) goes in words with
    /// the first message, once; until then it's still to give, so a start before that (a new mode)
    /// gives it then.
    #[test]
    fn a_conversation_carried_in_goes_with_its_first_message_once() {
        let (mut a, mut core) = started(&Spec::default());
        a.offer_context(Some("<kiln_handoff>earlier</kiln_handoff>".into()));
        assert!(!a.seeded());
        send(&mut a, &mut core, "m1", "go on", SendMode::Queue);
        assert_eq!(written(&mut core).first().and_then(|v| v.pointer("/message/content")).and_then(Value::as_str), Some("<kiln_handoff>earlier</kiln_handoff>\n\ngo on"));
        assert!(a.seeded(), "its own conversation has it now");
        // The timeline shows what the person said, not the conversation it went with.
        assert!(core.events().any(|e| matches!(&e.body, Body::User { text, .. } if text == "go on")));
        feed(&mut a, &mut core, &[json!({"event": "result", "result": {"conversation_id": CID, "status": "SUCCESS", "response": "", "num_turns": 1}})]);
        send(&mut a, &mut core, "m2", "and then", SendMode::Queue);
        assert_eq!(written(&mut core).first().and_then(|v| v.pointer("/message/content")).and_then(Value::as_str), Some("and then"), "only the first message carries it");
    }

    #[test]
    fn a_stop_starts_it_again_on_the_conversation_and_send_now_goes_first() {
        let (mut a, mut core) = started(&Spec::default());
        send(&mut a, &mut core, "m1", "long task", SendMode::Queue);
        feed(&mut a, &mut core, &[step(5, "ACTIVE", "agent_response", json!({"text_delta": "one\ntwo\n"}))]);
        core.effects.clear();
        send(&mut a, &mut core, "m2", "stop, do this", SendMode::Now);
        assert!(core.effects.iter().any(|e| matches!(e, Effect::Restart { resume_at: None })));
        assert_eq!(core.state.queue.first().map(|q| q.text.as_str()), Some("stop, do this"));
        assert_eq!(core.state.last_turn.as_ref().map(|t| t.how.as_str()), Some("stopped"), "you stopped it");
        assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Cancelled));
        // The result of the turn it was ending doesn't end another.
        feed(&mut a, &mut core, &[json!({"event": "result", "result": {"status": "ERROR", "error": "interrupted", "response": "one\ntwo\n"}})]);
        assert_eq!(core.state.usage.turns, 0);
        // Started again: the message goes out to the new process.
        let mut b = Agy::new(&Spec { resume: Some(CID.into()), ..Spec::default() });
        b.start(&mut core);
        core.take_writes();
        feed(&mut b, &mut core, &[json!({"event": "init", "conversation_id": CID, "init": {}})]);
        assert_eq!(written(&mut core).first().and_then(|v| v.pointer("/message/content")).and_then(Value::as_str), Some("stop, do this"));
        assert!(!core.events().any(|e| matches!(&e.body, Body::Notice { text, .. } if text.contains("couldn't find"))));
    }

    #[test]
    fn what_it_wasnt_allowed_is_said_and_not_kept_going() {
        let (mut a, mut core) = started(&Spec::default());
        send(&mut a, &mut core, "m1", "run it", SendMode::Queue);
        feed(
            &mut a,
            &mut core,
            &[
                step(2, "ACTIVE", "tool", json!({"tool_name": "run_command", "tool_info": {"name": "run_command", "parameters": {"CommandLine": "make"}}})),
                step(2, "ERROR", "tool", json!({"tool_name": "run_command", "tool_info": {"name": "run_command", "parameters": {"CommandLine": "make"}}})),
                json!({"event": "result", "result": {"status": "SUCCESS", "response": "", "denied_actions": [{"action": "command", "display_name": "RunCommand"}]}}),
            ],
        );
        assert!(core.events().any(|e| matches!(&e.body, Body::Tool(t) if t.status == ToolStatus::Denied)));
        assert!(core.events().any(|e| matches!(&e.body, Body::Notice { text, .. } if text.contains("RunCommand") && text.contains("Bypass"))));
        let t = core.state.last_turn.clone().unwrap_or_default();
        assert_eq!((t.how.as_str(), t.after_error), ("blocked", false));
    }

    /// The session starts it again (after `Effect::Restart`): a new adapter, the new process's init.
    fn restarted(core: &mut Core, spec: &Spec) -> Agy {
        let mut b = Agy::new(spec);
        core.set_status(Status::Starting);
        b.start(core);
        core.effects.clear();
        core.take_writes();
        feed(&mut b, core, &[json!({"event": "init", "conversation_id": CID, "init": {}})]);
        b
    }

    #[test]
    fn a_new_mode_model_or_effort_starts_it_again_between_turns() {
        let spec = Spec { allow_bypass: false, ..Spec::default() };
        let (mut a, mut core) = started(&spec);
        core.effects.clear();
        // Idle: it starts again now, and what's sent meanwhile waits for the new process.
        a.command(&mut core, Command::SetEffort { effort: "max".into() });
        assert!(core.effects.iter().any(|e| matches!(e, Effect::Restart { .. })));
        assert_eq!(core.state.effort, "max");
        send(&mut a, &mut core, "m1", "work", SendMode::Queue);
        assert!(written(&mut core).is_empty());
        let mut b = restarted(&mut core, &spec);
        assert_eq!(written(&mut core).first().and_then(|v| v.pointer("/message/content")).and_then(Value::as_str), Some("work"));
        // During a turn: once it ends, then what was sent meanwhile goes to the new process.
        b.command(&mut core, Command::SetMode { mode: BYPASS.into() });
        assert!(core.effects.is_empty());
        assert!(core.events().any(|e| matches!(&e.body, Body::Notice { text, .. } if text.contains("from the next turn"))));
        send(&mut b, &mut core, "m2", "more", SendMode::Queue);
        feed(&mut b, &mut core, &[json!({"event": "result", "result": {"status": "SUCCESS", "response": "ok"}})]);
        assert!(core.effects.iter().any(|e| matches!(e, Effect::AllowBypass)));
        assert!(core.effects.iter().any(|e| matches!(e, Effect::Restart { .. })));
        assert!(written(&mut core).is_empty());
        assert_eq!(core.state.queue.len(), 1, "m2 waits for the new start");
        let mut c = restarted(&mut core, &Spec { mode: Some(BYPASS.into()), allow_bypass: true, ..Spec::default() });
        assert_eq!(written(&mut core).first().and_then(|v| v.pointer("/message/content")).and_then(Value::as_str), Some("more"));
        // An unknown mode or effort changes nothing.
        core.effects.clear();
        c.command(&mut core, Command::SetMode { mode: "yolo".into() });
        c.command(&mut core, Command::SetEffort { effort: "ultra".into() });
        assert!(core.effects.is_empty());
    }

    #[test]
    fn a_lost_conversation_is_said_and_errors_cut_the_turn() {
        let mut core = Core::new();
        let mut a = Agy::new(&Spec { resume: Some("00000000-0000-4000-8000-000000000000".into()), ..Spec::default() });
        a.start(&mut core);
        feed(&mut a, &mut core, &[json!({"event": "init", "conversation_id": CID, "init": {}})]);
        assert!(core.events().any(|e| matches!(&e.body, Body::Notice { text, .. } if text.contains("couldn't find"))));
        assert_eq!(core.state.harness_session.as_deref(), Some(CID));
        send(&mut a, &mut core, "m1", "go", SendMode::Queue);
        feed(&mut a, &mut core, &[json!({"event": "result", "result": {"status": "ERROR", "error": "model unavailable: 503", "response": "partial"}})]);
        assert_eq!(core.state.last_turn.as_ref().map(|t| t.how.as_str()), Some("cut"));
        assert!(core.events().any(|e| matches!(&e.body, Body::Notice { level: Level::Error, text } if text.contains("503"))));
        // Nothing streamed: its words from the result.
        assert!(core.events().any(|e| matches!(&e.body, Body::Assistant { text, .. } if text == "partial")));
    }

    #[test]
    fn its_models_from_its_cli() {
        let m = parse_models("Fetching available models...\ngemini-3.8-flash-high\tGemini 3.8 Flash (High)\nclaude-opus-4-6-thinking\tClaude Opus 4.6 (Thinking)\n");
        assert_eq!(m.iter().map(|x| (x.id.as_str(), x.label.as_str())).collect::<Vec<_>>(), [("gemini-3.8-flash-high", "Gemini 3.8 Flash (High)"), ("claude-opus-4-6-thinking", "Claude Opus 4.6 (Thinking)")]);
    }

    // ------------------------------------------------------------------ its subagents

    const KID: &str = "14298854-d6e0-4914-85e5-b83ae812b847";
    const KID2: &str = "f89f1044-e3f9-4133-aa11-997bbc6d15a4";

    type Adapter = crate::harness::kinds::Adapter;

    /// The adapter of a session whose agent sent two subagents off, as agy 1.4.10 says it in the
    /// stream (the probe's `subagent` step).
    fn with_kids() -> (Adapter, Core) {
        let (a, mut core) = started(&Spec::default());
        let mut a = Adapter::Agy(Box::new(a));
        a.command(&mut core, Command::Send { id: "m1".into(), text: "Look into it".into(), mode: SendMode::Queue, attachments: vec![], overrides: None, reply_to: None, task: None });
        let kid = |id: &str, role: &str| json!({"type_name": "self", "role": role, "initial_prompt": format!("{role}: read sub/a.txt"), "conversation_id": id, "log_uri": format!("file:///Users/ada/.gemini/antigravity-cli/brain/{id}/.system_generated/logs/transcript.jsonl"), "workspace_uris": ["file:///w"]});
        for l in [
            step(2, "ACTIVE", "tool", json!({"tool_name": "invoke_subagent", "tool_info": {"name": "invoke_subagent", "parameters": {"Role": "File and URL Inspector", "TypeName": "self"}}})),
            step(2, "DONE", "subagent", json!({"tool_name": "invoke_subagent", "subagent_info": {"subagents": [kid(KID, "File and URL Inspector"), kid(KID2, "Codebase Researcher")]}})),
        ] {
            a.line(&mut core, &l.to_string());
        }
        (a, core)
    }

    /// agy's hook input for `event` in conversation `conv`, as `kiln harness-hook --agy` hands it on.
    fn hooked(event: &str, conv: &str, extra: Value) -> Value {
        let mut v = json!({"conversationId": conv, "workspacePaths": ["/w"], "modelName": "gemini-3.8-flash-high"});
        if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            o.extend(e.clone());
        }
        hook_input(v, event, None)
    }

    fn call(conv: &str, name: &str, args: Value) -> Value {
        hooked("PreToolUse", conv, json!({"toolCall": {"name": name, "args": args}, "stepIdx": 2}))
    }

    /// kiln's answer to a hook, if it gave one yet, and where a later one comes.
    fn ask(a: &mut Adapter, core: &mut Core, input: Value) -> (Option<Value>, futures::channel::oneshot::Receiver<Value>) {
        let (tx, mut rx) = futures::channel::oneshot::channel();
        a.hook(core, input, tx);
        (rx.try_recv().ok().flatten(), rx)
    }

    fn task<'c>(core: &'c Core, id: &str) -> Option<&'c Task> {
        core.state.tasks.iter().find(|t| t.id == id)
    }

    /// What `kiln harness-hook --agy` hands on: agy's input with its event and its conversation's
    /// tokens from its transcript (numbers or strings); what lets agy go on as without kiln; and
    /// kiln's entry in agy's hooks.json, said once.
    #[test]
    fn kilns_hook_is_told_its_event_and_tokens_and_passes_without_kiln() {
        let transcript = "{\"step_index\":\"0\",\"type\":\"SYSTEM_MESSAGE\"}\n{\"input_tokens\":\"7003\",\"output_tokens\":\"699\"}\nnot json\n{\"input_tokens\":100,\"output_tokens\":1}\n";
        let v = hook_input(json!({"conversationId": KID}), "PreToolUse", Some(transcript));
        assert_eq!((v.get("kilnEvent"), v.get("kilnTokens"), v.get("conversationId")), (Some(&json!("PreToolUse")), Some(&json!(7803)), Some(&json!(KID))));
        assert_eq!(hook_input(Value::Null, "Stop", None), json!({"kilnEvent": "Stop"}));
        assert_eq!(hook_pass("PreToolUse"), json!({"decision": "allow"}));
        assert_eq!(hook_pass("PreInvocation"), json!({}));
        assert_eq!(hook_pass("Stop"), json!({"decision": ""}));
        let made = hooks_with(&json!({"mine": {}}), "/k/kiln");
        assert!(made.as_ref().is_some_and(|m| m.get("mine").is_some()));
        assert_eq!(made.as_ref().and_then(|m| hooks_with(m, "/k/kiln")), None, "once there, left alone");
    }

    /// W26: Antigravity's subagents are tasks of the session, each under the call that started it,
    /// from the stream; its hook, once heard, turns its limits on, and says each one's tokens and end.
    #[test]
    fn its_subagents_are_tasks_under_their_spawn_and_end_at_their_stop() {
        let (mut a, mut core) = with_kids();
        let t = task(&core, KID).cloned().unwrap_or_default();
        let spawn = format!("agy-{}-2", CID.chars().filter(char::is_ascii_alphanumeric).take(8).collect::<String>());
        assert_eq!(
            (t.tool_id.as_deref(), t.description.as_str(), t.kind.as_str(), t.class.as_deref(), t.status.as_str(), t.prompt.as_deref()),
            (Some(spawn.as_str()), "File and URL Inspector", "self", Some("agent"), "running", Some("File and URL Inspector: read sub/a.txt"))
        );
        assert!(t.subagent() && t.output.as_deref().is_some_and(|o| o.starts_with("/Users/ada/.gemini/")), "{t:?}");
        assert!(task(&core, KID2).is_some());
        assert!(core.events().any(|e| e.id == spawn && matches!(&e.body, Body::Tool(c) if c.tool == ToolKind::Agent && c.status == ToolStatus::Done)));
        // Not yet heard: nothing to pause or limit.
        assert!(!core.meta.features.iter().any(|f| f == "limits"));
        assert!(!task(&core, KID).is_some_and(|t| t.pausable));
        let mut first = hooked("PreInvocation", KID, json!({"invocationNum": 0}));
        if let Some(o) = first.as_object_mut() {
            o.insert("kilnTokens".into(), json!(7702));
        }
        assert_eq!(ask(&mut a, &mut core, first).0, Some(json!({})));
        assert!(core.meta.features.iter().any(|f| f == "limits"), "heard: its limits are on");
        assert!(task(&core, KID).is_some_and(|t| t.pausable && t.tokens == 7702 && t.takes == vec![SendMode::Steer]));
        assert_eq!(ask(&mut a, &mut core, call(KID, "read_file", json!({"Path": "/w/sub/a.txt"}))).0, Some(json!({"decision": "allow"})));
        assert!(task(&core, KID).is_some_and(|t| t.tool_uses == 1 && t.last_tool.as_deref() == Some("read_file")));
        assert_eq!(ask(&mut a, &mut core, hooked("Stop", KID, json!({"fullyIdle": true}))).0, Some(json!({"decision": ""})));
        assert!(task(&core, KID).is_some_and(|t| t.status == "completed" && t.ended.is_some() && !t.pausable && t.takes.is_empty()));
        // Its agent ending the other: stopped.
        a.line(&mut core, &step(4, "ACTIVE", "tool", json!({"tool_name": "manage_subagents", "tool_info": {"name": "manage_subagents", "parameters": {"Action": "kill", "ConversationIds": [KID2]}}})).to_string());
        a.line(&mut core, &step(4, "DONE", "tool", json!({"tool_name": "manage_subagents", "tool_info": {"name": "manage_subagents", "parameters": {"Action": "kill", "ConversationIds": [KID2]}, "output": "killed"}})).to_string());
        assert_eq!(task(&core, KID2).map(|t| t.status.as_str()), Some("stopped"));
    }

    /// W26: a person's message to one of its subagents goes in before that subagent's next model
    /// call, once, and is done once it goes on; one that ended takes none.
    #[test]
    fn a_message_to_a_subagent_goes_in_at_its_next_model_call() {
        let (mut a, mut core) = with_kids();
        let to = |id: &str, text: &str| Command::Send { id: id.into(), text: text.into(), mode: SendMode::Steer, attachments: vec![], overrides: None, reply_to: None, task: Some(KID.into()) };
        // Before kiln's hook is heard, nothing reaches it.
        a.command(&mut core, to("s0", "early"));
        assert_eq!(core.state.delivery.get("s0"), Some(&Delivery::Cancelled));
        assert_eq!(ask(&mut a, &mut core, hooked("PreInvocation", CID, json!({}))).0, Some(json!({})));
        a.command(&mut core, to("s1", "Also check the tests."));
        a.command(&mut core, to("s2", "And say PINEAPPLE."));
        assert_eq!(core.state.delivery.get("s1"), Some(&Delivery::Queued));
        assert!(core.events().any(|e| e.id == "s1" && e.parent.as_deref().is_some_and(|p| p.ends_with("-2"))), "shown in its thread");
        // Another conversation's model call doesn't take it.
        assert_eq!(ask(&mut a, &mut core, hooked("PreInvocation", KID2, json!({}))).0, Some(json!({})));
        let (now, _) = ask(&mut a, &mut core, hooked("PreInvocation", KID, json!({"invocationNum": 1})));
        assert_eq!(now, Some(json!({"injectSteps": [{"userMessage": "Also check the tests."}, {"userMessage": "And say PINEAPPLE."}]})));
        assert_eq!(core.state.delivery.get("s2"), Some(&Delivery::Started));
        assert_eq!(ask(&mut a, &mut core, hooked("PreInvocation", KID, json!({"invocationNum": 2}))).0, Some(json!({})), "once");
        assert_eq!(core.state.delivery.get("s2"), Some(&Delivery::Completed));
        assert_eq!(ask(&mut a, &mut core, hooked("Stop", KID, json!({}))).0, Some(json!({"decision": ""})));
        a.command(&mut core, to("s3", "too late"));
        assert_eq!(core.state.delivery.get("s3"), Some(&Delivery::Cancelled));
        assert!(core.events().any(|e| matches!(&e.body, Body::Notice { text, .. } if text.contains("Not sent to the subagent"))));
    }

    /// W26: a person's pause holds an Antigravity subagent at its next tool call until they resume
    /// it; the agent's own calls and its other subagents' go on meanwhile.
    #[test]
    fn a_paused_subagents_next_call_waits_until_its_resumed() {
        let (mut a, mut core) = with_kids();
        assert_eq!(ask(&mut a, &mut core, hooked("PreInvocation", CID, json!({}))).0, Some(json!({})), "kiln's hook heard");
        a.command(&mut core, Command::Pause { on: true, task: Some(KID.into()) });
        assert!(task(&core, KID).is_some_and(|t| t.paused_at.is_some()));
        let (now, mut held) = ask(&mut a, &mut core, call(KID, "run_command", json!({"CommandLine": "npm test"})));
        assert_eq!(now, None, "the subagent's call waits");
        assert_eq!(ask(&mut a, &mut core, call(CID, "run_command", json!({"CommandLine": "ls"}))).0, Some(json!({"decision": "allow"})), "the agent's own goes on");
        assert_eq!(ask(&mut a, &mut core, call(KID2, "view_file", json!({"AbsolutePath": "/w/a"}))).0, Some(json!({"decision": "allow"})), "and the other subagent's");
        assert_eq!(held.try_recv().ok().flatten(), None);
        a.command(&mut core, Command::Pause { on: false, task: Some(KID.into()) });
        assert_eq!(held.try_recv().ok().flatten(), Some(json!({"decision": "allow"})), "resumed, its call goes on");
    }

    /// W26: what a person lets the session and each subagent do holds at each tool call: Off is
    /// refused in words the agent reads, Ask waits for kiln's own approval (a yes goes even where
    /// agy itself would deny it), a folder, tokens and the web hold too.
    #[test]
    fn what_the_session_and_each_subagent_may_do_holds_at_each_call() {
        use super::super::model::Grant;
        let (mut a, mut core) = with_kids();
        assert_eq!(ask(&mut a, &mut core, hooked("PreInvocation", CID, json!({}))).0, Some(json!({})), "kiln's hook heard");
        let deny = |v: &Option<Value>| v.as_ref().filter(|v| v.get("decision") == Some(&json!("deny"))).and_then(|v| v.get("reason")).and_then(Value::as_str).map(str::to_owned);
        // The session's: its own calls.
        core.state.perms = Some(Perms { run: Grant::Off, ..Perms::default() });
        let why = deny(&ask(&mut a, &mut core, call(CID, "run_command", json!({"CommandLine": "make"}))).0);
        assert!(why.as_deref().is_some_and(|w| w.contains("You may not run commands")), "{why:?}");
        core.state.perms = None;
        // A subagent's: the web asked, a folder and its tokens.
        let perms = Perms { web: Grant::Ask, folder: Some("sub".into()), tokens: Some(10_000), ..Perms::default() };
        a.command(&mut core, Command::Perms { perms: Some(perms), task: Some(KID.into()) });
        let why = deny(&ask(&mut a, &mut core, call(KID, "read_file", json!({"Path": "/etc/hosts"}))).0);
        assert!(why.as_deref().is_some_and(|w| w.contains("/etc/hosts is outside your folder, sub")), "{why:?}");
        assert_eq!(ask(&mut a, &mut core, call(KID, "read_file", json!({"Path": "/w/sub/a.txt"}))).0, Some(json!({"decision": "allow"})));
        let (now, mut held) = ask(&mut a, &mut core, call(KID, "search_web", json!({"query": "rust oneshot"})));
        assert_eq!(now, None, "a person sees it first");
        let ap = core.state.approvals.first().cloned();
        assert_eq!(ap.as_ref().map(|a| (a.tool, a.title.as_str(), a.agent.as_deref())), Some((ToolKind::Web, "rust oneshot", Some("File and URL Inspector"))));
        assert_eq!(core.state.status, Status::Waiting);
        a.command(&mut core, Command::Approve { id: ap.map(|a| a.id).unwrap_or_default(), decision: Decision::Allow, message: None, always: None, answers: None, choices: None });
        assert_eq!(held.try_recv().ok().flatten(), Some(json!({"decision": "auto_approve"})));
        assert!(core.state.approvals.is_empty());
        let (_, mut held) = ask(&mut a, &mut core, call(KID, "read_url_content", json!({"Url": "https://example.com"})));
        let id = core.state.approvals.first().map(|a| a.id.clone()).unwrap_or_default();
        a.command(&mut core, Command::Approve { id, decision: Decision::Deny, message: Some("Not that site".into()), always: None, answers: None, choices: None });
        assert_eq!(held.try_recv().ok().flatten(), Some(json!({"decision": "deny", "reason": "Not that site"})));
        // Past its tokens: it finishes with what it has.
        let mut late = call(KID, "read_file", json!({"Path": "/w/sub/b.txt"}));
        if let Some(o) = late.as_object_mut() {
            o.insert("kilnTokens".into(), json!(12_000));
        }
        let why = deny(&ask(&mut a, &mut core, late).0);
        assert!(why.as_deref().is_some_and(|w| w.contains("10000 tokens")), "{why:?}");
        // The other subagent isn't held by this one's limits.
        assert_eq!(ask(&mut a, &mut core, call(KID2, "read_file", json!({"Path": "/etc/hosts"}))).0, Some(json!({"decision": "allow"})));
    }

    /// agy's tools by kind, as its hook names them, and the files they name.
    #[test]
    fn its_hooked_calls_are_told_apart_by_tool_and_files() {
        assert_eq!(hook_call("read_file", &json!({"Path": "/w/a"})), (ToolKind::Read, vec!["/w/a".to_owned()]));
        assert_eq!(hook_call("find_by_name", &json!({"SearchDirectory": "/w"})), (ToolKind::Search, vec!["/w".to_owned()]));
        assert_eq!(hook_call("write_to_file", &json!({"TargetFile": "/w/x"})).0, ToolKind::Edit);
        assert_eq!(hook_call("run_command", &json!({"CommandLine": "ls"})).0, ToolKind::Shell);
        assert_eq!(hook_call("call_mcp_tool", &json!({"ServerName": "github", "ToolName": "x", "Arguments": {}})).0, ToolKind::Mcp);
        assert_eq!(hook_call("search_web", &json!({"query": "q"})).0, ToolKind::Web);
        assert_eq!(hook_call("browser_click_element", &json!({})).0, ToolKind::Web);
        assert_eq!(hook_call("send_message", &json!({})).0, ToolKind::Other);
    }
}
