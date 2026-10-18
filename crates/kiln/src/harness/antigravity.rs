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
//! Written against agy 1.4.2, from what it was seen to send.

use std::collections::HashMap;

use serde_json::{Value, json};

use super::core::{Core, Effect};
use super::model::{
    Body, Command, ControlOption, Delivery, ExitInfo, Inventory, Known, Level, ModeOption, ModelOption, QueueItem, SendMode, SlashCommand, Status, StatusItem, ToolCall, ToolKind, ToolStatus,
    OUTPUT_MAX, clip_json, clip_middle, sort_catalog,
};
use super::session::Spec;

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
}

impl Agy {
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
        core.meta.features = ["send_now", "queue", "interrupt", "set_mode", "set_model", "set_effort", "resume", "attachments"].iter().map(|s| (*s).to_owned()).collect();
        core.meta.modes = modes();
        core.meta.efforts = EFFORTS.iter().map(|e| ControlOption { id: (*e).to_owned(), label: capitalize(e), description: String::new(), dangerous: false }).collect();
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
        self.close_text(core);
        self.turn = None;
        self.ready = false;
        let error = error.map(|e| {
            // Older versions have no multi-turn mode: say what it needs.
            if e.contains("input-format") { format!("{e}\nkiln needs agy 1.4 or newer (`agy update`).") } else { e }
        });
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
            // Nothing gets into a turn under way: it stays queued.
            Command::QueueSteer { .. } => {}
            // It never asks: there is nothing to answer.
            Command::Approve { .. } => {}
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
            Command::Suggest { client, .. } => core.reply(client, json!({"suggestions": []})),
            Command::Control { id, value } if id == "export" => {
                let client = value.as_ref().and_then(|v| v.get("client")).and_then(Value::as_str).unwrap_or("").to_owned();
                core.reply(client, json!({"title": "Conversation", "text": core.export(), "format": "markdown", "filename": "conversation.md"}));
            }
            Command::Control { .. } => {}
            Command::StopTask { .. } | Command::StopTasks | Command::Restart | Command::ForceStop | Command::Snapshot | Command::MoveTo { .. } | Command::Moving { .. } => {}
            // A pause's commands are the session's (`harness::pause`).
            Command::Resume | Command::AutoResume { .. } | Command::PauseAt { .. } | Command::MoveOnLimit { .. } | Command::LimitHold { .. } => {}
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

    fn prompt(&mut self, core: &mut Core, m: QueueItem) {
        self.close_text(core);
        // It takes text only: files go as their paths on this machine, which it reads itself.
        let mut text = m.text.clone();
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
        core.note_sent(&m.id);
        core.upsert(&m.id, None, Body::User { text: m.text.clone(), images, client_id: Some(m.id.clone()), origin: Some(super::core::origin_of(&m.id).into()), attachments: names });
        core.state.turn_started = Some(Core::now());
        core.state.doing = Some("Thinking".to_owned());
        core.settle_status(true);
        core.touch_state();
    }

    fn next(&mut self, core: &mut Core) {
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

    /// A pause ended: the turn it cut off goes on first, then kiln's queue.
    pub fn go_on(&mut self, core: &mut Core, p: super::model::Pause) {
        if p.cut.is_some() && !self.busy() && core.state.status != Status::Exited {
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
            "tool" => {
                self.close_text(core);
                let info = s.get("tool_info").unwrap_or(&Value::Null);
                let name = s.get("tool_name").or_else(|| info.get("name")).and_then(Value::as_str).unwrap_or("tool").to_owned();
                let id = self.tools.entry(index).or_insert_with(|| format!("agy-{}-{index}", self.tag)).clone();
                let mut call = match core.get(&id).map(|e| &e.body) {
                    Some(Body::Tool(t)) => t.as_ref().clone(),
                    _ => tool_call(&id, &name, info.get("parameters").unwrap_or(&Value::Null), &core.state.cwd),
                };
                if let Some(out) = info.get("output").and_then(Value::as_str).filter(|o| !o.is_empty()) {
                    let (text, cut) = clip_middle(out, OUTPUT_MAX);
                    call.output = Some(text);
                    call.truncated = cut;
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
                if done {
                    call.ended = Some(Core::now());
                    if self.busy() {
                        core.state.doing = Some("Thinking".to_owned());
                    }
                } else {
                    core.state.doing = Some(super::claude::doing(&call));
                }
                core.upsert(&id, None, Body::Tool(Box::new(call)));
                core.touch_state();
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
            (!id.is_empty() && !id.contains(' ')).then(|| ModelOption { id: id.to_owned(), label: label.trim().to_owned(), description: String::new(), efforts: Vec::new() })
        })
        .collect()
}

/// A tool call as it starts: what kind of tool, and its title from its input.
fn tool_call(id: &str, name: &str, params: &Value, cwd: &str) -> ToolCall {
    let tool = match name {
        "run_command" | "send_command_input" | "command_status" => ToolKind::Shell,
        "write_to_file" | "replace_file_content" | "multi_replace_file_content" | "sed_file" | "notebook_edit" => ToolKind::Edit,
        "view_file" | "list_dir" | "read_resource" | "list_resources" => ToolKind::Read,
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
    let path = ["TargetFile", "AbsolutePath", "DirectoryPath", "SearchPath", "File", "Path"].iter().find_map(|k| s(k)).map(|p| super::claude::rel(&p, cwd));
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
    }
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
        a.command(core, Command::Send { id: id.into(), text: text.into(), mode, attachments: vec![], overrides: None });
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
}
