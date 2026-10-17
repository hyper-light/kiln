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

use serde_json::{Value, json};

use super::core::{Baseline, Core, Effect};
use super::model::{
    AlwaysOption, Approval, Body, Command, Control, ControlKind, ControlOption, ControlScope, Decision, Delivery, ExitInfo, FileDiff, HookInfo, Level, ModeOption,
    ModelOption, Question, SlashCommand,
    QuestionOption, QueueItem, SendMode, Status, Task, Todo, ToolCall, ToolKind, ToolStatus, OUTPUT_MAX, clip_json, clip_middle,
};

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

#[derive(Debug, Clone)]
enum Pending {
    Initialize,
    Thread,
    Turn(String),
    Steer(String),
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
}

/// An approval request Codex is waiting on.
#[derive(Debug, Clone)]
struct Ask {
    /// The JSON-RPC id to answer under.
    rpc: Value,
    kind: AskKind,
    /// Question ids by question text (user-input requests).
    questions: Vec<(String, String)>,
    /// "decline" is on offer (the turn goes on with the refusal); otherwise only "cancel".
    can_decline: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AskKind {
    Command,
    FileChange,
    UserInput,
    Permissions,
    LegacyExec,
    LegacyPatch,
}

pub struct Codex {
    next_id: u64,
    pending: HashMap<u64, Pending>,
    asks: HashMap<String, Ask>,
    thread: Option<String>,
    /// The turn under way.
    turn: Option<String>,
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
    /// MCP servers: (name, status).
    mcp: Vec<(String, String)>,
    /// A new thread branched off `resume` at this turn (instead of resuming it).
    fork_at: Option<Option<String>>,
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
            mcp: Vec::new(),
            fork_at: None,
        }
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
        for (name, status) in &self.mcp {
            c.push(ctl(&format!("mcp_info:{name}"), "MCP", name, &format!("MCP server ({status})"), ControlKind::Text));
        }
        c.push(ctl("mcp_refresh", "MCP", "Refresh servers", "Ask for every server's status again", ControlKind::Action));
        c.push(ctl("mcp_reload", "MCP", "Reload configuration", "Reload the MCP servers from Codex's configuration", ControlKind::Action));
        core.meta.controls = c;
        let model = core.state.model.clone();
        core.meta.efforts = self
            .efforts
            .get(&model)
            .map(|l| l.iter().map(|(e, d)| opt(e, &capitalize(e), d, false)).collect())
            .unwrap_or_default();
        core.touch_meta();
    }

    fn control(&mut self, core: &mut Core, id: &str, value: Option<Value>) {
        let text = || value.as_ref().and_then(|v| v.as_str().map(str::to_owned).or_else(|| v.get("text").and_then(Value::as_str).map(str::to_owned))).unwrap_or_default();
        let event = || value.as_ref().and_then(|v| v.get("event")).and_then(Value::as_str).map(str::to_owned);
        let Some(thread) = self.thread.clone() else { return };
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
            "mcp_refresh" => self.request(core, Pending::Mcp, "mcpServerStatus/list", json!({})),
            "mcp_reload" => {
                self.request(core, Pending::Plain("reload MCP servers".into()), "config/mcpServer/reload", json!({}));
                self.request(core, Pending::Mcp, "mcpServerStatus/list", json!({}));
            }
            _ => {}
        }
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
        self.turn.is_some() || !core.state.approvals.is_empty() || self.pending.values().any(|p| matches!(p, Pending::Turn(_)))
    }

    fn settle(&self, core: &mut Core) {
        let running = self.turn.is_some() || self.pending.values().any(|p| matches!(p, Pending::Turn(_)));
        core.settle_status(running);
    }

    pub fn start(&mut self, core: &mut Core) {
        if !modes(self.allow_bypass).iter().any(|m| m.id == core.state.mode) {
            core.state.mode = self.mode.clone();
            core.touch_state();
        }
        core.meta.modes = modes(self.allow_bypass);
        core.meta.features = ["steer", "send_now", "queue", "interrupt", "set_mode", "set_model", "set_effort", "file_suggestions", "resume", "attachments", "images", "overrides"]
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
        self.turn = None;
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
            Command::Approve { id, decision, message, always, answers } => self.answer(core, &id, decision, message, always, answers),
            Command::Interrupt => self.interrupt(core),
            Command::SetMode { mode } => {
                // Takes effect from the next turn (Codex sets policy per turn).
                if modes(true).iter().any(|m| m.id == mode) && (mode != "full-access" || self.allow_bypass) {
                    core.state.mode = mode;
                    core.touch_state();
                }
            }
            Command::SetModel { model } => {
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
            Command::StopTask { .. } | Command::Restart | Command::Snapshot => {}
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
                self.request(
                    core,
                    Pending::Steer(m.id.clone()),
                    "turn/steer",
                    json!({"threadId": thread, "expectedTurnId": turn, "input": input(&m.text, &m.attachments), "clientUserMessageId": m.id}),
                );
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

    fn start_turn(&mut self, core: &mut Core, thread: &str, m: QueueItem) {
        let (approval, _, sandbox) = self.policies(core);
        // "/skill words": the skill, then the words.
        let mut items = input(&m.text, &m.attachments);
        if let Some(rest) = m.text.strip_prefix('/')
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
        core.upsert(&m.id, None, Body::User { text: m.text.clone(), images, client_id: Some(m.id.clone()), origin: Some("human".into()), attachments: names });
        core.touch_state();
    }

    /// Shown for a message kiln sends on its own (feedback after a refusal).
    fn note(&mut self, core: &mut Core, id: &str, text: &str, d: Delivery) {
        let m = QueueItem { id: id.to_owned(), text: text.to_owned(), at: Core::now(), attachments: Vec::new(), overrides: None };
        self.show_sent(core, &m, d);
    }

    /// Between turns: the next waiting message, then kiln's queue.
    fn next(&mut self, core: &mut Core) {
        if self.busy(core) || core.state.status == Status::Exited {
            return;
        }
        let Some(thread) = self.thread.clone() else { return };
        if !self.waiting.is_empty() {
            let m = self.waiting.remove(0);
            self.start_turn(core, &thread, m);
            return;
        }
        if !core.state.queue.is_empty() {
            let item = core.state.queue.remove(0);
            core.touch_state();
            self.start_turn(core, &thread, item);
        }
    }

    fn answer(&mut self, core: &mut Core, id: &str, decision: Decision, message: Option<String>, always: Option<usize>, answers: Option<std::collections::BTreeMap<String, String>>) {
        let Some(approval) = core.take_approval(id) else { return };
        let Some(ask) = self.asks.remove(id) else { return };
        let feedback = message.filter(|m| !m.trim().is_empty());
        let result = match ask.kind {
            AskKind::UserInput => {
                let mut out = serde_json::Map::new();
                if decision != Decision::Deny {
                    for (qid, text) in &ask.questions {
                        if let Some(a) = answers.as_ref().and_then(|a| a.get(text)) {
                            let list: Vec<&str> = a.split(", ").collect();
                            out.insert(qid.clone(), json!({"answers": list}));
                        }
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
                    self.request(core, Pending::Steer(mid.clone()), "turn/steer", json!({"threadId": thread, "expectedTurnId": turn, "input": [{"type": "text", "text": f}], "clientUserMessageId": mid}));
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
            match kind {
                Pending::Suggest(client) => core.reply(client, json!({"error": msg})),
                Pending::Turn(id) | Pending::Steer(id) => {
                    core.state.delivery.insert(id, Delivery::Cancelled);
                    self.notice(core, Level::Error, format!("Codex refused the message: {msg}"));
                    core.touch_state();
                }
                Pending::Thread => {
                    self.notice(core, Level::Error, format!("Codex could not open the conversation: {msg}"));
                    core.set_status(Status::Idle);
                }
                Pending::Models | Pending::Interrupt | Pending::Skills | Pending::Hooks | Pending::Mcp => {}
                Pending::Revert(_) => self.notice(core, Level::Warn, format!("Could not rewind: {msg}")),
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
                    (Some(t), Some(at)) => {
                        if let Some(o) = params.as_object_mut() {
                            o.insert("threadId".into(), json!(t));
                            if let Some(at) = at {
                                o.insert("lastTurnId".into(), json!(at));
                            }
                        }
                        self.request(core, Pending::Thread, "thread/fork", params);
                    }
                    (Some(t), None) => {
                        if let Some(o) = params.as_object_mut() {
                            o.insert("threadId".into(), json!(t));
                        }
                        self.request(core, Pending::Thread, "thread/resume", params);
                    }
                    (None, _) => self.request(core, Pending::Thread, "thread/start", params),
                }
                self.request(core, Pending::Models, "model/list", json!({}));
                self.request(core, Pending::Skills, "skills/list", json!({"cwds": [cwd]}));
                self.request(core, Pending::Hooks, "hooks/list", json!({"cwds": [core.state.cwd.clone()]}));
                self.request(core, Pending::Mcp, "mcpServerStatus/list", json!({}));
                if let Some(ua) = result.get("userAgent").and_then(Value::as_str) {
                    core.meta.version = ua.split_whitespace().next().unwrap_or(ua).to_owned();
                    core.touch_meta();
                }
            }
            Pending::Thread => {
                let thread = result.get("thread").cloned().unwrap_or(Value::Null);
                if let Some(id) = thread.get("id").and_then(Value::as_str) {
                    self.thread = Some(id.to_owned());
                    core.state.harness_session = Some(id.to_owned());
                }
                if let Some(m) = result.get("model").and_then(Value::as_str) {
                    core.state.model = m.to_owned();
                }
                if let Some(c) = result.get("cwd").and_then(Value::as_str) {
                    core.state.cwd = c.to_owned();
                }
                if let Some(v) = thread.get("cliVersion").and_then(Value::as_str) {
                    core.meta.version = v.to_owned();
                    core.touch_meta();
                }
                if let Some(n) = thread.get("name").and_then(Value::as_str) {
                    core.state.title = n.to_owned();
                }
                // A resumed thread: its history, as items.
                for turn in thread.get("turns").and_then(Value::as_array).cloned().unwrap_or_default() {
                    let tid = turn.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
                    for item in turn.get("items").and_then(Value::as_array).cloned().unwrap_or_default() {
                        if item.get("type").and_then(Value::as_str) == Some("userMessage") {
                            let key = item.get("clientId").or_else(|| item.get("id")).and_then(Value::as_str).unwrap_or("").to_owned();
                            self.turns.insert(key, tid.clone());
                        }
                        self.item(core, &item, true);
                    }
                }
                self.refresh_controls(core);
                core.touch_state();
                core.set_status(Status::Idle);
                self.next(core);
            }
            Pending::Turn(id) => {
                if let Some(t) = result.get("turn").and_then(|t| t.get("id")).and_then(Value::as_str) {
                    self.turn = Some(t.to_owned());
                }
                core.state.delivery.insert(id, Delivery::Started);
                core.touch_state();
            }
            Pending::Steer(id) => {
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
                for entry in result.get("data").and_then(Value::as_array).cloned().unwrap_or_default() {
                    for sk in entry.get("skills").and_then(Value::as_array).cloned().unwrap_or_default() {
                        if sk.get("enabled").and_then(Value::as_bool) == Some(false) {
                            continue;
                        }
                        let (Some(name), Some(path)) = (sk.get("name").and_then(Value::as_str), sk.get("path").and_then(Value::as_str)) else { continue };
                        self.skills.insert(name.to_owned(), path.to_owned());
                        cmds.push(SlashCommand {
                            name: name.to_owned(),
                            description: sk.get("shortDescription").or_else(|| sk.get("description")).and_then(Value::as_str).unwrap_or("").to_owned(),
                            hint: String::new(),
                        });
                    }
                }
                core.meta.commands = cmds;
                core.touch_meta();
            }
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
                self.mcp = result
                    .get("data")
                    .and_then(Value::as_array)
                    .map(|d| {
                        d.iter()
                            .filter_map(|m| Some((m.get("name")?.as_str()?.to_owned(), m.get("runtimeStatus").and_then(Value::as_str).unwrap_or("unknown").to_owned())))
                            .collect()
                    })
                    .unwrap_or_default();
                for (name, status) in self.mcp.clone() {
                    core.set_control(&format!("mcp_info:{name}"), json!(status));
                }
                self.refresh_controls(core);
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
                                    question: q.get("question").and_then(Value::as_str)?.to_owned(),
                                    header: q.get("header").and_then(Value::as_str).unwrap_or("").to_owned(),
                                    multi: false,
                                    options: q
                                        .get("options")
                                        .and_then(Value::as_array)
                                        .map(|o| {
                                            o.iter()
                                                .filter_map(|x| {
                                                    Some(QuestionOption {
                                                        label: x.get("label").and_then(Value::as_str)?.to_owned(),
                                                        description: x.get("description").and_then(Value::as_str).unwrap_or("").to_owned(),
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
                core.write(&json!({"id": rpc, "result": {"action": "decline"}}));
                return;
            }
            other => {
                core.write(&json!({"id": rpc, "error": {"code": -32601, "message": format!("kiln does not handle {other}")}}));
                return;
            }
        };
        let question_ids: Vec<(String, String)> = p
            .get("questions")
            .and_then(Value::as_array)
            .map(|qs| qs.iter().filter_map(|q| Some((q.get("id")?.as_str()?.to_owned(), q.get("question")?.as_str()?.to_owned()))).collect())
            .unwrap_or_default();
        if kind == AskKind::LegacyExec || kind == AskKind::LegacyPatch {
            always.truncate(1);
        }
        let approval = Approval {
            id: id.clone(),
            tool_id: item_id.clone(),
            tool_name: method.to_owned(),
            tool,
            title,
            detail: None,
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

    fn notification(&mut self, core: &mut Core, method: &str, p: Value) {
        match method {
            "turn/started" => {
                if let Some(t) = p.get("turn").and_then(|t| t.get("id")).and_then(Value::as_str) {
                    self.turn = Some(t.to_owned());
                }
                if core.state.turn_started.is_none() {
                    core.state.turn_started = Some(Core::now());
                }
                core.state.doing = Some("Thinking".to_owned());
                core.touch_state();
            }
            "turn/completed" => {
                let turn = p.get("turn").cloned().unwrap_or(Value::Null);
                let status = turn.get("status").and_then(Value::as_str).unwrap_or("completed");
                match status {
                    "interrupted" => self.notice(core, Level::Info, "Interrupted"),
                    "failed" => {
                        let msg = turn.get("error").and_then(|e| e.get("message")).and_then(Value::as_str).unwrap_or("The turn failed.").to_owned();
                        self.notice(core, Level::Error, msg);
                    }
                    _ => {}
                }
                let done = if status == "completed" { Delivery::Completed } else { Delivery::Cancelled };
                for id in std::mem::take(&mut self.turn_messages) {
                    if core.state.delivery.contains_key(&id) {
                        core.state.delivery.insert(id, done);
                    }
                }
                // Calls the turn left open were cut off with it.
                let open: Vec<String> = core
                    .events()
                    .filter_map(|e| match &e.body {
                        Body::Tool(t) if matches!(t.status, ToolStatus::Running | ToolStatus::Pending) => Some(e.id.clone()),
                        _ => None,
                    })
                    .collect();
                for id in open {
                    set_status(core, &id, ToolStatus::Cancelled);
                }
                self.turn = None;
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
                    let body = if method == "item/agentMessage/delta" {
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
            "item/commandExecution/outputDelta" | "item/fileChange/outputDelta" => {
                let (Some(id), Some(d)) = (p.get("itemId").and_then(Value::as_str), p.get("delta").and_then(Value::as_str)) else { return };
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
                let msg = p.get("error").and_then(|e| e.get("message")).and_then(Value::as_str).unwrap_or("Codex reported an error").to_owned();
                let retry = p.get("willRetry").and_then(Value::as_bool).unwrap_or(false);
                self.notice(core, if retry { Level::Warn } else { Level::Error }, if retry { format!("{msg} (retrying)") } else { msg });
            }
            "warning" | "deprecationNotice" | "configWarning" | "guardianWarning" | "model/rerouted" => {
                let msg = p.get("message").or_else(|| p.get("summary")).and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| method.to_owned());
                self.notice(core, Level::Warn, msg);
            }
            "thread/compacted" => self.notice(core, Level::Info, "Conversation compacted"),
            _ => {}
        }
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
                if let Some(text) = item.get("text").and_then(Value::as_str).filter(|_| done) {
                    core.upsert(&id, None, Body::Assistant { text: text.to_owned(), streaming: false });
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
                };
                if !done {
                    core.state.doing = Some(super::claude::doing(&call));
                    core.touch_state();
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
                let call = ToolCall {
                    tool_id: id.clone(),
                    name: format!("mcp__{server}__{tool_name}"),
                    tool: if ty == "mcpToolCall" { ToolKind::Mcp } else { ToolKind::Other },
                    title: if server.is_empty() { tool_name.to_owned() } else { format!("{server} · {tool_name}") },
                    detail: None,
                    input: clip_json(item.get("arguments").unwrap_or(&Value::Null), 16 * 1024),
                    status,
                    output,
                    truncated: false,
                    diffs: Vec::new(),
                    todos: Vec::new(),
                    task: None,
                    exit_code: None,
                    started: started_of(core, &id),
                    ended: done.then(Core::now),
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
                };
                core.upsert(&id, None, Body::Tool(Box::new(call)));
            }
            "collabAgentToolCall" | "subAgentActivity" => {
                let desc = item.get("prompt").or_else(|| item.get("agentPath")).and_then(Value::as_str).unwrap_or("subagent").to_owned();
                let status = match item.get("status").and_then(Value::as_str) {
                    Some("completed") => "completed",
                    Some("failed") => "failed",
                    _ if done => "completed",
                    _ => "running",
                };
                let task_id = item.get("agentThreadId").and_then(Value::as_str).unwrap_or(&id).to_owned();
                match core.state.tasks.iter_mut().find(|t| t.id == task_id) {
                    Some(t) => {
                        t.status = status.to_owned();
                        if done {
                            t.ended.get_or_insert_with(Core::now);
                        }
                    }
                    None => core.state.tasks.push(Task {
                        id: task_id.clone(),
                        tool_id: Some(id.clone()),
                        description: clip_middle(&desc, 300).0,
                        kind: item.get("kind").and_then(Value::as_str).unwrap_or("subagent").to_owned(),
                        status: status.to_owned(),
                        depth: 1,
                        started: Core::now(),
                        ended: done.then(Core::now),
                        ..Task::default()
                    }),
                }
                core.touch_state();
                let call = ToolCall {
                    tool_id: id.clone(),
                    name: ty.to_owned(),
                    tool: ToolKind::Agent,
                    title: clip_middle(&desc, 200).0,
                    detail: item.get("model").and_then(Value::as_str).map(str::to_owned),
                    input: json!({}),
                    status: if status == "running" { ToolStatus::Running } else if status == "failed" { ToolStatus::Error } else { ToolStatus::Done },
                    output: None,
                    truncated: false,
                    diffs: Vec::new(),
                    todos: Vec::new(),
                    task: Some(task_id),
                    exit_code: None,
                    started: started_of(core, &id),
                    ended: done.then(Core::now),
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

fn started_of(core: &Core, id: &str) -> i64 {
    match core.get(id).map(|e| &e.body) {
        Some(Body::Tool(t)) => t.started,
        _ => Core::now(),
    }
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

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
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
        assert_eq!(methods, vec!["initialized", "thread/start", "model/list", "skills/list", "hooks/list", "mcpServerStatus/list"]);
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
        c.command(&mut core, Command::Approve { id: "srv-7".into(), decision: Decision::Always, message: None, always: Some(1), answers: None });
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
        c.command(&mut core, Command::Approve { id: "44".into(), decision: Decision::Deny, message: Some("keep old".into()), always: None, answers: None });
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
        c.command(&mut core, Command::Approve { id: "q1".into(), decision: Decision::Allow, message: None, always: None, answers: Some(answers) });
        let w = written(&mut core);
        assert_eq!(w.first().and_then(|v| v.pointer("/result/answers/db/answers/0")).and_then(Value::as_str), Some("SQLite"));
        Ok(())
    }
}
