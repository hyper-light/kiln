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
use std::hash::{Hash, Hasher};

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

/// Whether permissions `a` (approval policy, sandbox) allow something `b` doesn't: they ask
/// less or reach further. Unknown ones might, unless `b` is the most there is.
fn exceeds(a: Option<&(String, String)>, b: &(String, String)) -> bool {
    let asks = |p: &str| match p {
        "untrusted" => 0,
        "never" => 2,
        _ => 1,
    };
    let reach = |s: &str| match s {
        "read-only" => 0,
        "danger-full-access" => 2,
        _ => 1,
    };
    match a {
        Some((approval, sandbox)) => asks(approval) > asks(&b.0) || reach(sandbox) > reach(&b.1),
        None => asks(&b.0) < 2 || reach(&b.1) < 2,
    }
}

/// "a", "a and b", "a, b and c".
fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum Pending {
    Initialize,
    Thread,
    /// thread/turns/list: a resumed thread's history, page by page.
    History,
    /// Read-only views of a native subagent; never resume or attach an executor to it.
    Child { thread: String, revision: u64 },
    ChildHistory { thread: String, revision: u64 },
    Turn(String),
    /// turn/steer for a message; the message itself (to send it as the next turn should the
    /// running one end first), unless kiln wrote it (feedback).
    Steer(String, Option<QueueItem>),
    Interrupt,
    PermissionInterrupt(String),
    /// turn/interrupt of a native subagent's turn (its thread), as it works under other
    /// permissions than the session's.
    ChildInterrupt(String),
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
    /// thread/backgroundTerminals/list, asked at `at` while kiln showed these terminal tasks
    /// running.
    Terminals { at: i64, live: Vec<String> },
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
const CHILD_REFRESH_MS: i64 = 5_000;
/// How long a request is held back while the work that made it is stopped.
const WITHHELD_MS: i64 = 30_000;
/// A terminal's task id: `terminal-<item id>`.
const TERMINAL: &str = "terminal-";
/// How often kiln asks Codex which terminals it still runs while it shows some running.
const TERMINALS_EVERY_MS: i64 = 30_000;
/// A terminal that started this shortly before kiln asked may not be among Codex's yet.
const TERMINAL_GRACE_MS: i64 = 2_000;

/// A child's snapshots supplement notifications (which Codex does not always forward).
#[derive(Default, Serialize, Deserialize)]
struct Child {
    read_at: i64,
    revision: u64,
    refresh: bool,
    #[serde(default)]
    runtime: Option<String>,
    #[serde(default)]
    turn: Option<String>,
    #[serde(default)]
    seen: HashMap<String, u64>,
    #[serde(default)]
    failures: u32,
    /// The approval policy and sandbox it started with, which Codex keeps for its life (None:
    /// it started before kiln saw it).
    #[serde(default)]
    permissions: Option<(String, String)>,
    /// Its turn kiln is stopping, as it works under other permissions than the session's.
    #[serde(default)]
    stopping: Option<String>,
    /// Its turn Codex wouldn't stop (what it asks is the person's).
    #[serde(default)]
    unstoppable: Option<String>,
}

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
    #[serde(default)]
    children: HashMap<String, Child>,
    /// The message that started (or was steered into) the turn under way.
    turn_messages: Vec<String>,
    /// Messages to send once the thread exists or the turn under way has been interrupted.
    waiting: Vec<QueueItem>,
    /// The reasoning effort for turns (None: the model's default).
    effort: Option<String>,
    resume: Option<String>,
    model: Option<String>,
    allow_bypass: bool,
    /// This run's tag, in the ids of the notices it makes ([`super::core::run_tag`]).
    #[serde(default = "super::core::run_tag")]
    run: String,
    notice_seq: u64,
    mode: String,
    /// Reasoning summaries in turns (auto, concise, detailed, none).
    summary: Option<String>,
    /// Approval policy and sandbox chosen directly (over the mode's).
    approval: Option<String>,
    sandbox: Option<String>,
    /// A person's permission change is interrupting the current turn. Codex cannot change a
    /// running turn's policy; its continuation starts with the newly chosen policy.
    #[serde(default)]
    permissions_changing: bool,
    /// Policies sent for the current turn; old handed-over adapters may not know them.
    #[serde(default)]
    turn_permissions: Option<(String, String)>,
    /// Notifications can finish a turn before its turn/start RPC reply arrives.
    #[serde(default)]
    completed_turn: Option<String>,
    /// Subagents kiln stopped for working under other permissions than the session's, until
    /// their parent is told.
    #[serde(default)]
    stopped: Vec<String>,
    /// Requests for approval no work started with the session's permissions makes, kept from
    /// the person while that work is moved onto them (shown after all if it can't be).
    #[serde(default)]
    withheld: HashMap<String, Approval>,
    /// The turn kiln last interrupted to apply the session's permissions to (once each).
    #[serde(default)]
    applied: Option<String>,
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
    /// When kiln last asked Codex which terminals it still runs, and whether it can't say (an
    /// older Codex without `thread/backgroundTerminals/list`).
    #[serde(default)]
    terminals_at: i64,
    #[serde(default)]
    terminals_unknown: bool,
    /// Terminals kiln ended as Codex no longer ran them, before Codex said how they ended: its
    /// word, when it comes, replaces kiln's.
    #[serde(default)]
    unheard: std::collections::HashSet<String>,
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
    /// The goal the thread it went on from pursued (a move), set on the new thread once its
    /// conversation is in (`thread/goal/set`'s params, but the thread).
    #[serde(default)]
    goal: Option<Value>,
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
            children: HashMap::new(),
            turn_messages: Vec::new(),
            waiting: Vec::new(),
            effort: None,
            resume,
            model: model.filter(|m| !m.is_empty()),
            allow_bypass,
            run: super::core::run_tag(),
            notice_seq: 0,
            mode,
            summary: None,
            approval: None,
            sandbox: None,
            permissions_changing: false,
            turn_permissions: None,
            completed_turn: None,
            stopped: Vec::new(),
            withheld: HashMap::new(),
            applied: None,
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
            terminals_at: 0,
            terminals_unknown: false,
            unheard: std::collections::HashSet::new(),
            steer_on_start: Vec::new(),
            rollout: None,
            opening: None,
            open_tries: 0,
            reopen: false,
            reload: None,
            seed: None,
            seeded: false,
            injecting: false,
            goal: None,
        }
    }

    /// Start the new thread with this conversation (one carried over from another harness), and
    /// the goal it pursued, if one came with it (`moving::GOAL`).
    pub fn seed(&mut self, items: Vec<Value>) {
        let (goals, items): (Vec<Value>, Vec<Value>) = items.into_iter().partition(|i| i.get("type").and_then(Value::as_str) == Some(super::moving::GOAL));
        self.goal = goals.into_iter().last().map(|mut g| {
            if let Some(o) = g.as_object_mut() {
                o.remove("type");
                o.retain(|_, v| !v.is_null());
            }
            g
        });
        self.seeded = items.is_empty();
        self.seed = (!items.is_empty()).then_some(items);
    }

    /// The goal that came with the conversation, set on the thread it goes on in.
    fn set_carried_goal(&mut self, core: &mut Core) {
        let (Some(mut params), Some(thread)) = (self.goal.take(), self.thread.clone()) else { return };
        if let Some(o) = params.as_object_mut() {
            o.insert("threadId".into(), json!(thread));
            o.insert("origin".into(), json!("user"));
        }
        self.request(core, Pending::Plain("Setting the goal it pursued again".into()), "thread/goal/set", params);
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

    /// Keep the preset label honest when the two permission controls are chosen separately.
    fn permission_controls(&mut self, core: &mut Core) {
        // Older live adapters are adopted without start(); publish the capability there too.
        if !core.meta.features.iter().any(|f| f == "permissions_live") {
            core.meta.features.push("permissions_live".into());
            core.touch_meta();
        }
        let (approval, sandbox, _) = self.policies(core);
        // Older adapters could keep just one override. Preserve its other, implicit value
        // before changing the label to Custom (which has no preset of its own).
        if self.approval.is_some() || self.sandbox.is_some() {
            self.approval = Some(approval.clone());
            self.sandbox = Some(sandbox.clone());
        }
        let mode = modes(true).into_iter().find(|m| {
            let (a, s, _) = policy(&m.id);
            a == approval && s == sandbox
        }).map_or_else(|| "custom".to_owned(), |m| m.id);
        if core.state.mode != mode {
            core.state.mode = mode;
            core.touch_state();
        }
        if core.state.mode == "custom" && !core.meta.modes.iter().any(|m| m.id == "custom") {
            core.meta.modes.push(ModeOption { id: "custom".into(), label: "Custom".into(), description: "The approval and sandbox settings selected separately".into(), dangerous: false });
            core.touch_meta();
        } else if core.state.mode != "custom" && core.meta.modes.iter().any(|m| m.id == "custom") {
            // Custom describes the controls in force; it isn't a saved preset to switch back to.
            core.meta.modes.retain(|m| m.id != "custom");
            core.touch_meta();
        }
        core.set_control("approval_policy", json!(approval));
        core.set_control("sandbox", json!(sandbox));
        self.compose_status(core);
    }

    fn permissions_changed(&mut self, core: &mut Core) {
        self.permission_controls(core);
        let (approval, sandbox, _) = self.policies(core);
        let now = (approval, sandbox);
        // Subagents keep the permissions they started with: those allowed more stop now.
        let over: Vec<(String, Option<String>)> = self.children.iter()
            .filter(|(id, c)| c.turn.is_some() && exceeds(c.permissions.as_ref(), &now) && core.state.tasks.iter().any(|t| t.id == **id && t.live()))
            .map(|(id, c)| (id.clone(), c.turn.clone()))
            .collect();
        for (child, turn) in over {
            let _ = self.stop_child(core, &child, turn);
        }
        let running = self.turn.is_some() || self.pending.values().any(|p| matches!(p, Pending::Turn(_)));
        if running && (self.permissions_changing || self.turn_permissions.as_ref() != Some(&now)) {
            if !self.permissions_changing {
                self.notice(core, Level::Info, "Applying the new permissions: interrupting this turn, then continuing its work.");
                self.permissions_changing = true;
            }
            self.interrupt_for_permissions(core);
        }
        self.reconcile(core);
        // The change stopped work: the parent goes on with it now (one being interrupted, as
        // it continues).
        self.tell_parent(core, true);
    }

    /// A request for approval nothing started with the session's permissions makes: under
    /// "never" nothing asks, so it comes from work started with earlier ones (a turn begun
    /// before they changed, or a subagent, which keeps the permissions it started with).
    fn outdated(&self, core: &Core, kind: AskKind, thread: Option<&str>) -> bool {
        if !matches!(kind, AskKind::Command | AskKind::LegacyExec | AskKind::FileChange | AskKind::LegacyPatch | AskKind::Permissions) {
            return false;
        }
        let (approval, sandbox, _) = self.policies(core);
        if approval != "never" {
            return false;
        }
        let now = (approval, sandbox);
        match thread.filter(|t| self.thread.as_deref() != Some(*t)) {
            Some(child) => self.children.get(child).and_then(|c| c.permissions.as_ref()) != Some(&now),
            None => self.turn.is_some() && self.turn_permissions.as_ref() != Some(&now),
        }
    }

    /// Move the work that asked (`thread`'s turn `turn`) onto the session's permissions rather
    /// than ask the person, and never by answering for them: the session's own turn is
    /// interrupted and goes on with them; a subagent is stopped and its parent told to start a
    /// new one. False: it can't be (kiln tried for that turn, and Codex refused).
    fn apply_permissions(&mut self, core: &mut Core, thread: Option<&str>, turn: Option<String>) -> bool {
        match thread.filter(|t| self.thread.as_deref() != Some(*t)) {
            Some(child) => self.stop_child(core, child, turn),
            None => {
                if self.permissions_changing {
                    return true;
                }
                if self.turn.is_none() || self.applied == self.turn {
                    return false;
                }
                self.applied.clone_from(&self.turn);
                self.notice(core, Level::Info, "Applying your permissions: this turn started with earlier ones, so it's interrupted, then its work continues.");
                self.permissions_changing = true;
                self.interrupt_for_permissions(core);
                true
            }
        }
    }

    /// Requests for approval shown, or held back, that only work started with earlier
    /// permissions makes: that work is moved onto the session's.
    fn reconcile(&mut self, core: &mut Core) {
        let outdated: Vec<(Option<String>, Option<String>)> = core.state.approvals.iter().chain(self.withheld.values())
            .filter(|a| self.asks.get(&a.id).is_some_and(|k| self.outdated(core, k.kind, a.input.get("threadId").and_then(Value::as_str))))
            .map(|a| (a.input.get("threadId").and_then(Value::as_str).map(str::to_owned), a.input.get("turnId").and_then(Value::as_str).map(str::to_owned)))
            .collect();
        for (thread, turn) in outdated {
            let _ = self.apply_permissions(core, thread.as_deref(), turn);
        }
    }

    /// Stop a subagent's turn: it keeps the permissions it started with, and works under other
    /// ones than the session's. What it asked goes with the turn. False: it can't be (kiln
    /// tried for that turn, and Codex refused).
    /// A person stops subagent `thread` (its row's Stop): the turn it works on is interrupted. Its
    /// parent isn't told to start it again (that's for the permission change above). False: it
    /// has no turn under way.
    fn stop_child_asked(&mut self, core: &mut Core, thread: &str) -> bool {
        let Some(turn) = self.children.get(thread).and_then(|c| c.turn.clone()) else { return false };
        let name = core.state.tasks.iter().find(|t| t.id == thread).map(|t| t.description.trim().to_owned()).filter(|d| !d.is_empty()).unwrap_or_else(|| thread.to_owned());
        self.notice(core, Level::Info, format!("Stopping subagent {name}, as asked."));
        self.request(core, Pending::ChildInterrupt(thread.to_owned()), "turn/interrupt", json!({"threadId": thread, "turnId": turn}));
        true
    }

    fn stop_child(&mut self, core: &mut Core, thread: &str, turn: Option<String>) -> bool {
        let Some(turn) = turn.or_else(|| self.children.get(thread).and_then(|c| c.turn.clone())) else { return false };
        let child = self.children.entry(thread.to_owned()).or_default();
        if child.unstoppable.as_deref() == Some(turn.as_str()) {
            return false;
        }
        if child.stopping.as_deref() == Some(turn.as_str()) {
            return true;
        }
        child.stopping = Some(turn.clone());
        if !self.stopped.iter().any(|s| s == thread) {
            self.stopped.push(thread.to_owned());
        }
        let name = core.state.tasks.iter().find(|t| t.id == thread).map(|t| t.description.trim().to_owned()).filter(|d| !d.is_empty()).unwrap_or_else(|| thread.to_owned());
        self.notice(core, Level::Info, format!("Stopping subagent {name}: it keeps the permissions it was started with, which differ from this session's now. Its parent is told to start a new one for its unfinished work."));
        self.request(core, Pending::ChildInterrupt(thread.to_owned()), "turn/interrupt", json!({"threadId": thread, "turnId": turn}));
        true
    }

    /// What the parent hears about the subagents kiln stopped (and forgets them).
    fn stopped_note(&mut self, core: &Core) -> Option<String> {
        if self.stopped.is_empty() {
            return None;
        }
        let names: Vec<String> = std::mem::take(&mut self.stopped).iter()
            .map(|id| core.state.tasks.iter().find(|t| t.id == *id).map(|t| t.description.trim()).filter(|d| !d.is_empty()).map_or_else(|| id.clone(), |d| format!("{d} ({id})")))
            .collect();
        let (they, them) = if names.len() == 1 { ("it", "it") } else { ("they", "them") };
        Some(format!(
            "kiln stopped your subagent{} {}: subagents keep the permissions they were started with, and {they} started with other ones than yours now. \
             Start new subagents for any of {} work that isn't done (new ones get your current permissions) rather than sending {them} more.",
            if names.len() == 1 { "" } else { "s" },
            and_list(&names),
            if names.len() == 1 { "its" } else { "their" },
        ))
    }

    /// The parent hears which subagents kiln stopped: in its turn (steered in). Between turns,
    /// `now` (a person's permission change stopped them) starts one to go on with their work;
    /// else it hears with the next message it's sent. A turn interrupted for new permissions
    /// hears as it goes on.
    fn tell_parent(&mut self, core: &mut Core, now: bool) {
        if self.permissions_changing || self.stopped.is_empty() {
            return;
        }
        if self.turn.is_some() {
            let Some(text) = self.stopped_note(core) else { return };
            let id = format!("{}{}", super::pause::KILN_ID, crate::util::new_id());
            self.send(core, QueueItem { id: id.clone(), text, at: Core::now(), attachments: Vec::new(), overrides: None }, SendMode::Steer);
            super::pause::from_kiln(core, &id);
        } else if now && !self.busy(core) {
            let Some(note) = self.stopped_note(core) else { return };
            super::pause::continue_later(core, &format!("Your permissions were updated. {note}"));
            self.next(core);
        }
    }

    /// Requests held back from the person are theirs after all (the work that asked could not
    /// be stopped): those of `thread` (None: the session's own).
    fn show_withheld(&mut self, core: &mut Core, thread: Option<&str>) {
        let own = self.thread.clone();
        let ids: Vec<String> = self.withheld.iter()
            .filter(|(_, a)| {
                let from = a.input.get("threadId").and_then(Value::as_str);
                match thread {
                    Some(t) => from == Some(t),
                    None => from.is_none() || from == own.as_deref(),
                }
            })
            .map(|(id, _)| id.clone())
            .collect();
        self.release(core, ids);
    }

    fn release(&mut self, core: &mut Core, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        for id in ids {
            if let Some(a) = self.withheld.remove(&id) {
                if let Some(tool) = a.tool_id.as_deref() {
                    set_status(core, tool, ToolStatus::Pending);
                }
                core.add_approval(a);
            }
        }
        self.settle(core);
    }

    /// Subagents a spawn the session saw start: Codex gives each its spawner's permissions.
    fn note_spawns(&mut self, item: &Value, spawner: Option<&str>) {
        let targets: Vec<String> = match item.get("type").and_then(Value::as_str) {
            Some("collabAgentToolCall") if item.get("tool").and_then(Value::as_str) == Some("spawnAgent") => {
                item.get("receiverThreadIds").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect()
            }
            Some("subAgentActivity") if item.get("kind").and_then(Value::as_str) == Some("started") => {
                item.get("agentThreadId").and_then(Value::as_str).map(str::to_owned).into_iter().collect()
            }
            _ => return,
        };
        let permissions = match spawner {
            None => self.turn_permissions.clone(),
            Some(t) => self.children.get(t).and_then(|c| c.permissions.clone()),
        };
        for target in targets {
            if self.thread.as_deref() == Some(target.as_str()) || spawner == Some(target.as_str()) {
                continue;
            }
            let child = self.children.entry(target).or_default();
            if child.permissions.is_none() {
                child.permissions.clone_from(&permissions);
            }
        }
    }

    fn interrupt_for_permissions(&mut self, core: &mut Core) {
        if self.permissions_changing
            && let (Some(thread), Some(turn)) = (self.thread.clone(), self.turn.clone())
            && !self.pending.values().any(|p| matches!(p, Pending::PermissionInterrupt(t) if *t == turn))
        {
            self.request(core, Pending::PermissionInterrupt(turn.clone()), "turn/interrupt", json!({"threadId": thread, "turnId": turn}));
        }
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
                let valid = if id == "sandbox" { matches!(v.as_str(), "read-only" | "workspace-write" | "danger-full-access") }
                    else { matches!(v.as_str(), "untrusted" | "on-request" | "never") };
                if !valid {
                    self.notice(core, Level::Warn, format!("{v}: not a {id} setting this session offers"));
                    return;
                }
                let dangerous = matches!(v.as_str(), "never" | "danger-full-access");
                if dangerous && !self.allow_bypass {
                    self.notice(core, Level::Warn, "That needs the session to be started with bypass allowed.");
                    return;
                }
                let before = self.policies(core);
                // Both values stay explicit when a combination has no named preset.
                self.approval = Some(before.0.clone());
                self.sandbox = Some(before.1.clone());
                if id == "sandbox" {
                    self.sandbox = Some(v.clone());
                } else {
                    self.approval = Some(v.clone());
                }
                if self.policies(core) != before {
                    self.permissions_changed(core);
                }
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
            // Signed in on its machine (kiln's sign-in, `codex login`): it reads its account at
            // once (another one: the old one's limits go and the new one's are read), and starts
            // again when idle so its app-server certainly runs on the new login.
            "account" => {
                self.request(core, Pending::Account, "account/read", json!({}));
                self.reload = Some("Signed in".to_owned());
                self.reload_when_idle(core);
            }
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
        core.upsert(&format!("notice-{}-{}", self.run, self.notice_seq), None, Body::Notice { level, text: text.into() });
    }

    fn busy(&self, core: &Core) -> bool {
        let own_approval = core.state.approvals.iter().any(|a| {
            a.input.get("threadId").and_then(Value::as_str).is_none_or(|id| self.thread.as_deref().is_none_or(|own| own == id))
        });
        self.injecting || self.turn.is_some() || matches!(self.thread_status, Some(Status::Running | Status::Waiting)) || own_approval || self.pending.values().any(|p| matches!(p, Pending::Turn(_)))
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
        // Explicit controls survive a process restart. Older versions could persist a custom
        // combination under a preset label; do not silently drop its restrictions.
        if core.state.control_values.contains_key("approval_policy") || core.state.control_values.contains_key("sandbox") {
            let (a, s, _) = policy(&core.state.mode);
            let a = core.state.control_values.get("approval_policy").and_then(Value::as_str).unwrap_or(a);
            let s = core.state.control_values.get("sandbox").and_then(Value::as_str).unwrap_or(s);
            if matches!(a, "untrusted" | "on-request" | "never")
                && matches!(s, "read-only" | "workspace-write" | "danger-full-access")
                && (self.allow_bypass || (a != "never" && s != "danger-full-access"))
            {
                self.approval = Some(a.to_owned());
                self.sandbox = Some(s.to_owned());
            }
        }
        if !modes(self.allow_bypass).iter().any(|m| m.id == core.state.mode)
            && !(core.state.mode == "custom" && self.approval.is_some() && self.sandbox.is_some())
        {
            core.state.mode = self.mode.clone();
            core.touch_state();
        }
        // Full access is always a choice; choosing it allows it.
        core.meta.modes = modes(true);
        self.permission_controls(core);
        core.meta.features = ["steer", "send_now", "queue", "queue_steer", "interrupt", "set_mode", "permissions_live", "set_model", "set_effort", "file_suggestions", "resume", "attachments", "images", "overrides", "switch_harness"]
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
        self.permissions_changing = false;
        self.turn_permissions = None;
        self.withheld.clear();
        self.stopped.clear();
        self.unheard.clear();
        core.state.approvals.clear();
        core.state.exit = Some(ExitInfo::now(code, error));
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
            Command::Interrupt => {
                // A person's stop wins over the automatic continuation of a setting change.
                self.permissions_changing = false;
                self.interrupt(core);
            }
            Command::SetMode { mode } => {
                if modes(true).iter().any(|m| m.id == mode) {
                    if mode == "full-access" {
                        // Persist even for an adopted adapter whose earlier version only
                        // changed the in-memory flag.
                        core.effects.push(Effect::AllowBypass);
                    }
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
                    self.permissions_changed(core);
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
            Command::Restart | Command::ForceStop | Command::Snapshot | Command::MoveTo { .. } | Command::Moving { .. } | Command::HandedOver { .. } => {}
            // A terminal Codex left running: Codex has no way to stop it from outside, so kiln stops
            // the process itself (the newest one under Codex running that command).
            // A subagent: its turn under way is interrupted, as a person asked.
            Command::StopTask { task } if self.children.contains_key(&task) => {
                if !self.stop_child_asked(core, &task) {
                    self.notice(core, Level::Info, "That subagent has no turn under way to stop.".to_owned());
                }
            }
            Command::StopTask { task } => self.stop_terminal(core, &task),
            Command::StopTasks => {
                let live: Vec<String> = core.state.tasks.iter().filter(|t| t.live() && t.process.is_some()).map(|t| t.id.clone()).collect();
                for task in live {
                    self.stop_terminal(core, &task);
                }
            }
            // A pause's commands are the session's (`harness::pause`).
            Command::Resume | Command::AutoResume { .. } | Command::PauseAt { .. } | Command::MaxSubagents { .. } | Command::MoveOnLimit { .. } | Command::LimitHold { .. } => {}
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
        // A hot upgrade adopts the adapter without start(); reconcile legacy override labels.
        self.permission_controls(core);
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
        // Requests an older kiln showed that work under the session's permissions doesn't make.
        self.reconcile(core);
        self.tell_parent(core, false);
        let now = Core::now();
        // Held back while the work that asked was stopped: Codex withdraws what a stopped turn
        // asked at once, so one still waiting wasn't stopped, and it's the person's.
        let overdue: Vec<String> = self.withheld.iter().filter(|(_, a)| now - a.at > WITHHELD_MS).map(|(id, _)| id.clone()).collect();
        self.release(core, overdue);
        let children: Vec<String> = core.state.tasks.iter()
            .filter(|t| t.class.as_deref() == Some("agent"))
            // Hot adoption skips start/replay. Older adapters kept task rows without a
            // child reader, so reconcile each missing reader once, even for ended tasks
            // whose persisted output has never been loaded.
            .filter(|t| self.children.get(&t.id).is_none_or(|c| (c.refresh || t.live()) && now - c.read_at >= CHILD_REFRESH_MS))
            .map(|t| t.id.clone()).collect();
        for child in children {
            self.read_child(core, &child);
        }
        // Terminals still shown running are checked against Codex's own (a missed end, an older
        // image's rows).
        if now - self.terminals_at >= TERMINALS_EVERY_MS {
            self.ask_terminals(core);
        }
        if core.state.mcp.iter().any(|m| m.status == McpStatus::Pending) && now - self.mcp_at > 4_000 {
            self.ask_mcp(core);
        }
        // Every few minutes its account, read again (one signed in outside kiln, `codex login` in a
        // terminal, shows), and with a ChatGPT account its rate limits.
        if now - self.usage_at > USAGE_EVERY_MS && !self.pending.values().any(|p| matches!(p, Pending::RateLimits | Pending::Account)) {
            self.usage_at = now;
            self.request(core, Pending::Account, "account/read", json!({}));
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
        if mode == SendMode::Now {
            self.permissions_changing = false;
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
        let (approval, sandbox_mode, sandbox) = self.policies(core);
        self.turn_permissions = Some((approval.clone(), sandbox_mode));
        let mut items = self.items(&m);
        // Subagents kiln stopped that it hasn't heard of yet.
        if let Some(note) = self.stopped_note(core)
            && let Some(list) = items.as_array_mut()
        {
            list.push(json!({"type": "text", "text": format!("<kiln_note>\n{note}\n</kiln_note>")}));
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
                if !collab_replayed(core, &self.children, item, None) {
                    self.item(core, item, true);
                }
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
                Pending::Turn(id) => {
                    self.permissions_changing = false;
                    self.turn_permissions = None;
                    core.state.delivery.insert(id, Delivery::Cancelled);
                    self.notice(core, Level::Error, format!("Codex refused the message: {msg}"));
                    core.touch_state();
                }
                Pending::Steer(id, _) => {
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
                Pending::Child { thread, .. } | Pending::ChildHistory { thread, .. } => {
                    // Rollouts can lag a new child. Retry without turning a read failure into
                    // either a parent error or a claim that the child itself failed.
                    if let Some(child) = self.children.get_mut(&thread) {
                        child.failures = child.failures.saturating_add(1);
                        child.refresh = child.failures < 3;
                    }
                }
                Pending::RateLimits => {
                    // A pause's check that can't be answered: go on, and let Codex say.
                    if core.state.pause.as_ref().is_some_and(|p| p.checking.is_some())
                        && let Some(p) = super::pause::release(core, "The limit's reset time came; going on.")
                    {
                        self.go_on(core, p);
                    }
                }
                Pending::PermissionInterrupt(turn) if self.permissions_changing && self.turn.as_deref() == Some(&turn) => {
                    self.permissions_changing = false;
                    self.notice(core, Level::Warn, format!("Codex could not interrupt to apply the permissions: {msg}. They apply when the next turn starts."));
                    self.show_withheld(core, None);
                    self.tell_parent(core, false);
                }
                // Not stopped (its turn may have just ended): what it asked is the person's after
                // all. It isn't tried again for that turn.
                Pending::ChildInterrupt(thread) => {
                    // Not a permission change's stop (that one is `stopping`): a person asked.
                    let asked = self.children.get(&thread).is_some_and(|c| c.stopping.is_none());
                    if let Some(child) = self.children.get_mut(&thread) {
                        child.unstoppable = child.stopping.take();
                    }
                    self.stopped.retain(|s| *s != thread);
                    self.show_withheld(core, Some(&thread));
                    if asked {
                        self.notice(core, Level::Warn, format!("Codex couldn't stop that subagent: {msg}"));
                    }
                }
                Pending::Models | Pending::Interrupt | Pending::PermissionInterrupt(_) | Pending::Hooks | Pending::Account => {}
                // An older Codex can't list its terminals (the method is experimental): kiln
                // goes on hearing of their ends alone. Anything else is asked again later.
                Pending::Terminals { .. } => {
                    let code = err.get("code").and_then(Value::as_i64);
                    if code == Some(-32601) || ["experimental", "unknown variant", "method not found"].iter().any(|w| msg.to_lowercase().contains(w)) {
                        self.terminals_unknown = true;
                    }
                }
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
                    // Without the conversation, its goal would be pursued from nothing.
                    self.goal = None;
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
                if let Some(e) = result.get("reasoningEffort").and_then(Value::as_str) {
                    self.effort_reported(core, e);
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
                // A goal that came without a conversation to go in first.
                self.set_carried_goal(core);
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
            Pending::Child { thread, revision } => {
                if self.children.get(&thread).is_some_and(|c| c.revision == revision) {
                    if let Some(metadata) = result.get("thread") {
                        self.child_metadata(core, &thread, metadata);
                        let runtime = metadata.get("status").and_then(child_runtime_status);
                        if let Some(child) = self.children.get_mut(&thread) {
                            child.runtime = runtime.map(str::to_owned);
                        }
                        if let Some(status) = runtime {
                            child_status(core, &thread, status);
                        }
                    }
                    self.request(core, Pending::ChildHistory { thread: thread.clone(), revision }, "thread/turns/list",
                        json!({"threadId": thread, "itemsView": "full", "sortDirection": "desc", "limit": HISTORY_TURNS}));
                } else if let Some(child) = self.children.get_mut(&thread) {
                    child.refresh = true;
                }
            }
            Pending::ChildHistory { thread, revision } => {
                if self.children.get(&thread).is_some_and(|c| c.revision == revision) {
                    let mut turns = result.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
                    turns.reverse();
                    let ids: std::collections::HashSet<&str> = turns.iter()
                        .flat_map(|turn| turn.get("items").and_then(Value::as_array).into_iter().flatten())
                        .filter_map(|item| item.get("id").and_then(Value::as_str)).collect();
                    if let Some(child) = self.children.get_mut(&thread) {
                        child.seen.retain(|id, _| ids.contains(id.as_str()));
                    }
                    self.child_history(core, &thread, &turns);
                    if let Some(child) = self.children.get_mut(&thread) {
                        child.failures = 0;
                    }
                } else if let Some(child) = self.children.get_mut(&thread) {
                    child.refresh = true;
                }
            }
            Pending::Turn(id) => {
                if let Some(t) = result.get("turn").and_then(|t| t.get("id")).and_then(Value::as_str) {
                    if self.completed_turn.as_deref() == Some(t) {
                        self.next(core);
                        return;
                    }
                    self.turn = Some(t.to_owned());
                }
                core.state.delivery.insert(id, Delivery::Started);
                core.touch_state();
                self.interrupt_for_permissions(core);
                self.steer_waiting(core);
            }
            Pending::Steer(id, _) => {
                core.state.delivery.insert(id, Delivery::Started);
                core.touch_state();
            }
            Pending::Models => {
                let data = result.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
                for m in &data {
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
                }
                // An effort to show when none was chosen and Codex hasn't said what the thread runs
                // at ([`Self::effort_reported`]; it says none when the model's default is all
                // there is): its own model's default (the default model's when its own isn't
                // listed). Only the default model's was taken before, so a session on any other
                // model said nothing of how hard it thinks.
                if core.state.effort.is_empty() {
                    let model = core.state.model.clone();
                    let default_of = |pick: &dyn Fn(&Value) -> bool| data.iter().find(|m| pick(m)).and_then(|m| m.get("defaultReasoningEffort")).and_then(Value::as_str).map(str::to_owned);
                    let own = default_of(&|m| [m.get("model"), m.get("id")].into_iter().flatten().filter_map(Value::as_str).any(|k| !model.is_empty() && k == model));
                    if let Some(e) = self.effort.clone().or(own).or_else(|| default_of(&|m| m.get("isDefault").and_then(Value::as_bool) == Some(true))) {
                        core.state.effort = e;
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
                    // What was read of the old account's limits is never shown as the new one's.
                    self.rate_limits.clear();
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
                // The conversation is in: the goal it pursued goes on too.
                self.set_carried_goal(core);
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
            Pending::Interrupt | Pending::PermissionInterrupt(_) | Pending::ChildInterrupt(_) => {}
            Pending::Terminals { at, live } => self.terminals_listed(core, at, &live, &result),
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
            agent: p.get("threadId").and_then(Value::as_str)
                .filter(|id| self.thread.as_deref().is_some_and(|own| own != *id))
                .map(|id| core.state.tasks.iter().find(|t| t.id == id)
                    .map_or_else(|| id.to_owned(), |t| t.description.clone())),
            at: Core::now(),
        };
        let from = p.get("threadId").and_then(Value::as_str).map(str::to_owned);
        self.asks.insert(id.clone(), Ask { rpc, kind, questions: question_ids, can_decline });
        if self.outdated(core, kind, from.as_deref()) && self.apply_permissions(core, from.as_deref(), p.get("turnId").and_then(Value::as_str).map(str::to_owned)) {
            // Not the person's to answer (nor answered for them): the work that asked goes on
            // under the session's permissions instead, and this goes with its turn.
            self.withheld.insert(id, approval);
            self.tell_parent(core, false);
            return;
        }
        if let Some(tid) = item_id.as_deref() {
            set_status(core, tid, ToolStatus::Pending);
        }
        core.state.doing = Some(match tool {
            ToolKind::Question => "Asking you a question".to_owned(),
            _ => "Waiting for your approval".to_owned(),
        });
        core.add_approval(approval);
        self.settle(core);
    }

    /// The effort Codex says the thread runs at (when it opens, and as its settings change): what
    /// the page shows unless one was chosen here, which goes with every turn and so is what runs.
    /// With none chosen it comes from the person's own Codex configuration
    /// (`model_reasoning_effort`), which a model's default from model/list only guesses at; and
    /// what the page shows is what a resumed session goes on with (`Session::resume_spec`), so a
    /// guess kept there set every later turn to it.
    fn effort_reported(&mut self, core: &mut Core, effort: &str) {
        if self.effort.is_none() && !effort.is_empty() && core.state.effort != effort {
            core.state.effort = effort.to_owned();
            core.touch_state();
        }
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
        core.state.exit = (next == Status::Exited).then(|| ExitInfo::now(None, Some("Codex reported a thread error".into())));
        core.touch_state();
        self.settle(core);
    }

    fn read_child(&mut self, core: &mut Core, thread: &str) {
        let child = self.children.entry(thread.to_owned()).or_default();
        if self.pending.values().any(|p| matches!(p, Pending::Child { thread: t, .. } | Pending::ChildHistory { thread: t, .. } if t == thread)) {
            child.refresh = true;
            return;
        }
        child.read_at = Core::now();
        child.refresh = false;
        let revision = child.revision;
        self.request(core, Pending::Child { thread: thread.to_owned(), revision }, "thread/read", json!({"threadId": thread, "includeTurns": false}));
    }

    fn child_changed(&mut self, core: &mut Core, thread: &str) {
        let child = self.children.entry(thread.to_owned()).or_default();
        child.revision += 1;
        child.failures = 0;
        self.read_child(core, thread);
    }

    fn child_metadata(&mut self, core: &mut Core, thread: &str, metadata: &Value) {
        let Some(task) = core.state.tasks.iter_mut().find(|t| t.id == thread) else { return };
        if let Some(name) = metadata.get("name").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
            task.description = clip_middle(name, 300).0;
        }
        if let Some(role) = metadata.get("agentRole").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            task.kind = role.to_owned();
        }
        if let Some(cwd) = metadata.get("cwd").and_then(Value::as_str) {
            task.cwd = Some(cwd.to_owned());
        }
        if let Some(created) = metadata.get("createdAt").and_then(Value::as_i64) {
            task.started = created.saturating_mul(1_000);
        }
        if task.prompt.is_none()
            && let Some(prompt) = metadata.get("preview").and_then(Value::as_str).filter(|s| !s.trim().is_empty())
        {
            task.prompt = Some(clip_middle(prompt, OUTPUT_MAX).0);
        }
        if let Some(depth) = metadata.pointer("/source/subAgent/thread_spawn/depth").and_then(Value::as_u64) {
            task.depth = u32::try_from(depth).unwrap_or(u32::MAX);
        }
        core.touch_state();
    }

    fn child_history(&mut self, core: &mut Core, thread: &str, turns: &[Value]) {
        for turn in turns {
            let active = turn.get("status").and_then(Value::as_str) == Some("inProgress");
            let items = turn.get("items").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
            for (index, item) in items.iter().enumerate() {
                let done = !active || match item.get("status").and_then(Value::as_str) {
                    Some(status) => status != "inProgress",
                    None => index + 1 < items.len() || item.get("phase").and_then(Value::as_str) == Some("final_answer"),
                };
                let Some(id) = item.get("id").and_then(Value::as_str) else { continue };
                let mut hash = std::collections::hash_map::DefaultHasher::new();
                item.to_string().hash(&mut hash);
                done.hash(&mut hash);
                let hash = hash.finish();
                let seen = &mut self.children.entry(thread.to_owned()).or_default().seen;
                if seen.insert(id.to_owned(), hash) == Some(hash) || collab_replayed(core, &self.children, item, Some(thread)) {
                    continue;
                }
                self.item_in(core, item, done, Some(thread));
            }
        }
        let Some(turn) = turns.last() else { return };
        if turn.get("status").and_then(Value::as_str) == Some("inProgress")
            && let Some(child) = self.children.get_mut(thread)
        {
            child.turn = turn.get("id").and_then(Value::as_str).map(str::to_owned);
        }
        let Some(status) = self.children.get(thread).and_then(|c| c.runtime.as_deref()).or_else(|| child_turn_status(turn)).map(str::to_owned) else { return };
        child_status(core, thread, &status);
        if let Some(task) = core.state.tasks.iter_mut().find(|t| t.id == thread) {
            if !task.live() {
                task.ended = turn.get("completedAt").and_then(Value::as_i64).map(|s| s.saturating_mul(1_000)).or(task.ended);
            }
            task.duration_ms = turns.iter().filter_map(|t| t.get("durationMs").and_then(Value::as_u64)).sum();
            if status == "failed"
                && let Some(message) = turn.pointer("/error/message").and_then(Value::as_str)
            {
                task.summary = Some(clip_middle(message, OUTPUT_MAX).0);
            }
        }
        core.touch_state();
    }

    /// A native child's work stays under its spawning call. None of these events may settle
    /// the parent's turn, replace its answer, account its tokens, or change its files.
    fn child_notification(&mut self, core: &mut Core, thread: &str, method: &str, p: &Value) {
        let Some(parent) = core.state.tasks.iter().find(|t| t.id == thread).and_then(|t| t.tool_id.clone()) else { return };
        let child = self.children.entry(thread.to_owned()).or_default();
        child.revision += 1;
        if let Some(id) = p.get("itemId").and_then(Value::as_str).or_else(|| p.pointer("/item/id").and_then(Value::as_str)) {
            child.seen.remove(id);
        }
        match method {
            "item/started" | "item/completed" => {
                if let Some(item) = p.get("item") {
                    self.item_in(core, item, method == "item/completed", Some(thread));
                    self.note_spawns(item, Some(thread));
                }
            }
            "item/agentMessage/delta" | "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" | "item/plan/delta" => {
                let (Some(id), Some(delta)) = (p.get("itemId").and_then(Value::as_str), p.get("delta").and_then(Value::as_str)) else { return };
                let id = child_event_id(thread, id);
                if core.get(&id).is_none() {
                    let body = if matches!(method, "item/agentMessage/delta" | "item/plan/delta") {
                        Body::Assistant { text: String::new(), streaming: true }
                    } else {
                        Body::Thinking { text: String::new(), streaming: true, redacted: false }
                    };
                    core.upsert(&id, Some(parent), body);
                }
                core.append(&id, delta);
            }
            "item/commandExecution/outputDelta" | "item/fileChange/outputDelta" => {
                let (Some(id), Some(delta)) = (p.get("itemId").and_then(Value::as_str), p.get("delta").and_then(Value::as_str)) else { return };
                if let Some(e) = core.get_mut(&child_event_id(thread, id))
                    && let Body::Tool(t) = &mut e.body
                {
                    let output = t.output.get_or_insert_with(String::new);
                    if output.len() < OUTPUT_MAX * 2 {
                        output.push_str(delta);
                    } else {
                        t.truncated = true;
                    }
                }
            }
            "turn/started" => {
                let child = self.children.entry(thread.to_owned()).or_default();
                child.runtime = Some("running".into());
                child.turn = p.pointer("/turn/id").and_then(Value::as_str).map(str::to_owned);
                child_status(core, thread, "running");
            }
            "turn/completed" => {
                let child = self.children.entry(thread.to_owned()).or_default();
                if let (Some(current), Some(completed)) = (child.turn.as_deref(), p.pointer("/turn/id").and_then(Value::as_str))
                    && current != completed
                {
                    return;
                }
                child.runtime = None;
                child.turn = None;
                // What it asked goes with its turn (Codex withdraws it).
                let ended = p.pointer("/turn/id").and_then(Value::as_str);
                let gone: Vec<String> = core.state.approvals.iter().chain(self.withheld.values())
                    .filter(|a| a.input.get("threadId").and_then(Value::as_str) == Some(thread))
                    .filter(|a| a.input.get("turnId").and_then(Value::as_str).is_none_or(|id| ended.is_none_or(|e| e == id)))
                    .map(|a| a.id.clone())
                    .collect();
                if !gone.is_empty() {
                    for id in gone {
                        self.asks.remove(&id);
                        self.withheld.remove(&id);
                        core.take_approval(&id);
                    }
                    self.settle(core);
                }
                if let Some(turn) = p.get("turn") {
                    self.child_history(core, thread, std::slice::from_ref(turn));
                }
                self.read_child(core, thread);
            }
            "thread/status/changed" => {
                let runtime = p.get("status").and_then(child_runtime_status);
                self.children.entry(thread.to_owned()).or_default().runtime = runtime.map(str::to_owned);
                if let Some(status) = runtime {
                    child_status(core, thread, status);
                }
            }
            "thread/tokenUsage/updated" => {
                if let Some(task) = core.state.tasks.iter_mut().find(|t| t.id == thread)
                    && let Some(total) = p.pointer("/tokenUsage/total")
                {
                    task.tokens = total.get("inputTokens").and_then(Value::as_u64).unwrap_or(0)
                        + total.get("outputTokens").and_then(Value::as_u64).unwrap_or(0);
                    core.touch_state();
                }
            }
            _ => {}
        }
    }

    fn notification(&mut self, core: &mut Core, method: &str, p: Value) {
        // One app-server can report child threads too. Their turns must not end the parent's.
        if method != "serverRequest/resolved"
            && let (Some(own), Some(thread)) = (self.thread.as_deref(), p.get("threadId").and_then(Value::as_str))
            && own != thread
        {
            let thread = thread.to_owned();
            self.child_notification(core, &thread, method, &p);
            return;
        }
        match method {
            "thread/status/changed" => self.thread_status(core, p.get("status").unwrap_or(&Value::Null)),
            "turn/started" => {
                if p.pointer("/turn/id").and_then(Value::as_str).is_some_and(|id| self.completed_turn.as_deref() == Some(id)) {
                    return;
                }
                self.thread_status = Some(Status::Running);
                if let Some(t) = p.get("turn").and_then(|t| t.get("id")).and_then(Value::as_str) {
                    self.turn = Some(t.to_owned());
                }
                self.steer_waiting(core);
                self.interrupt_for_permissions(core);
                self.tell_parent(core, false);
                if core.state.turn_started.is_none() {
                    core.state.turn_started = Some(Core::now());
                }
                core.state.doing = Some("Thinking".to_owned());
                core.touch_state();
                self.settle(core);
            }
            "turn/completed" => {
                let turn = p.get("turn").cloned().unwrap_or(Value::Null);
                if turn.get("id").and_then(Value::as_str).is_some_and(|id| self.completed_turn.as_deref() == Some(id)) {
                    return;
                }
                if let (Some(current), Some(completed)) = (self.turn.as_deref(), turn.get("id").and_then(Value::as_str))
                    && current != completed
                {
                    return;
                }
                let status = turn.get("status").and_then(Value::as_str).unwrap_or("completed");
                self.completed_turn = turn.get("id").and_then(Value::as_str).map(str::to_owned);
                let permissions_changed = std::mem::take(&mut self.permissions_changing);
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
                // Codex keeps them past the turn (its own TUI lists them until they exit) and says
                // when each ends; whether it still runs those kiln shows running, it says too.
                self.ask_terminals(core);
                let open: Vec<String> = core
                    .events()
                    .filter_map(|e| match &e.body {
                        Body::Tool(t) if e.parent.is_none() && matches!(t.status, ToolStatus::Running | ToolStatus::Pending) && !terminals.contains(&e.id) => Some(e.id.clone()),
                        _ => None,
                    })
                    .collect();
                for id in open {
                    set_status(core, &id, ToolStatus::Cancelled);
                }
                // Interrupting a turn cancels its pending requests. Never answer them as an
                // approval; native children's requests still belong to their own turns.
                let ended = turn.get("id").and_then(Value::as_str);
                let resolved: Vec<String> = core.state.approvals.iter().chain(self.withheld.values())
                    .filter(|a| a.input.get("threadId").and_then(Value::as_str).is_none_or(|id| self.thread.as_deref() == Some(id)))
                    .filter(|a| a.input.get("turnId").and_then(Value::as_str).is_none_or(|id| ended == Some(id)))
                    .map(|a| a.id.clone()).collect();
                for id in resolved {
                    self.asks.remove(&id);
                    self.withheld.remove(&id);
                    core.take_approval(&id);
                }
                self.turn = None;
                self.turn_permissions = None;
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
                // Work the change stopped goes on: the turn's, and its subagents' (who keep the
                // permissions they started with, so new ones take over). Otherwise subagents
                // kiln stopped are heard of with the next message it's sent.
                if permissions_changed && (status == "interrupted" || !self.stopped.is_empty()) {
                    let mut text = "Your permissions were updated.".to_owned();
                    if status == "interrupted" {
                        text.push_str(" Continue the work interrupted by that change, checking the state of any commands already started before retrying them.");
                    }
                    let (approval, sandbox, _) = self.policies(core);
                    let now = (approval, sandbox);
                    if let Some(note) = self.stopped_note(core) {
                        text.push(' ');
                        text.push_str(&note);
                    } else if core.state.tasks.iter().any(|t| t.class.as_deref() == Some("agent") && self.children.get(&t.id).and_then(|c| c.permissions.as_ref()) != Some(&now)) {
                        text.push_str(" Subagents keep the permissions they were started with: start new ones rather than sending more work to those started before this change.");
                    }
                    super::pause::continue_later(core, &text);
                }
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
                    self.note_spawns(item, None);
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
            "thread/settings/updated" => {
                if let Some(e) = p.pointer("/threadSettings/effort").and_then(Value::as_str) {
                    self.effort_reported(core, e);
                }
            }
            "serverRequest/resolved" => {
                let id = match p.get("requestId") {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => return,
                };
                self.asks.remove(&id);
                self.withheld.remove(&id);
                if core.take_approval(&id).is_some() {
                    self.settle(core);
                    self.next(core);
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
        let task_id = format!("{TERMINAL}{item_id}");
        let status = match (done, item.get("status").and_then(Value::as_str)) {
            (false, _) => "running",
            (true, _) if self.stopping.remove(&task_id) => "stopped",
            (true, Some("failed")) => "failed",
            (true, _) if exit.unwrap_or(0) != 0 => "failed",
            (true, _) => "completed",
        };
        // Ended by kiln once Codex no longer ran it: how it really ended replaces that.
        let heard = done && self.unheard.remove(&task_id);
        match core.state.tasks.iter_mut().find(|t| t.id == task_id) {
            Some(t) => {
                if t.live() || heard {
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

    /// Ask Codex which terminals it still runs (`thread/backgroundTerminals/list`, experimental):
    /// one kiln shows running that it no longer has ended without kiln hearing how.
    fn ask_terminals(&mut self, core: &mut Core) {
        if self.terminals_unknown || self.pending.values().any(|p| matches!(p, Pending::Terminals { .. })) {
            return;
        }
        let Some(thread) = self.thread.clone() else { return };
        let live: Vec<String> = core.state.tasks.iter().filter(|t| t.live() && t.process.is_some() && t.id.starts_with(TERMINAL)).map(|t| t.id.clone()).collect();
        if live.is_empty() {
            return;
        }
        let at = Core::now();
        self.terminals_at = at;
        // Without a limit, Codex lists them all at once.
        self.request(core, Pending::Terminals { at, live }, "thread/backgroundTerminals/list", json!({"threadId": thread}));
    }

    /// The terminals Codex runs now: those kiln showed running when it asked, started before that
    /// and not among them, have ended. How, Codex says when its end comes (`unheard`).
    fn terminals_listed(&mut self, core: &mut Core, at: i64, live: &[String], result: &Value) {
        // Only the whole list says what's gone.
        let Some(data) = result.get("data").and_then(Value::as_array).filter(|_| result.get("nextCursor").is_none_or(Value::is_null)) else { return };
        let running: std::collections::HashSet<&str> = data.iter().flat_map(|t| [t.get("itemId"), t.get("processId")]).flatten().filter_map(Value::as_str).collect();
        let gone: Vec<(String, Option<String>)> = core
            .state
            .tasks
            .iter()
            .filter(|t| t.live() && live.contains(&t.id) && t.started <= at - TERMINAL_GRACE_MS)
            .filter(|t| !t.tool_id.as_deref().is_some_and(|id| running.contains(id)) && !t.process.as_deref().is_some_and(|p| running.contains(p)))
            .map(|t| (t.id.clone(), t.tool_id.clone()))
            .collect();
        if gone.is_empty() {
            return;
        }
        let now = Core::now();
        for (task, tool) in gone {
            if let Some(t) = core.state.tasks.iter_mut().find(|t| t.id == task) {
                t.status = "completed".into();
                t.ended = Some(now);
            }
            if let Some(tool) = tool {
                set_status(core, &tool, ToolStatus::Done);
            }
            self.unheard.insert(task);
        }
        self.unheard.retain(|id| core.state.tasks.iter().any(|t| t.id == *id));
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
        self.item_in(core, item, done, None);
    }

    fn item_in(&mut self, core: &mut Core, item: &Value, done: bool, thread: Option<&str>) {
        let Some(raw_id) = item.get("id").and_then(Value::as_str) else { return };
        // Older Codex versions can include inherited items in a fork's history.
        if thread.is_some() && core.get(raw_id).is_some_and(|e| e.parent.is_none()) {
            return;
        }
        let id = thread.map_or_else(|| raw_id.to_owned(), |thread| child_event_id(thread, raw_id));
        let parent = thread.and_then(|thread| core.state.tasks.iter().find(|t| t.id == thread)).and_then(|t| t.tool_id.clone());
        let ty = item.get("type").and_then(Value::as_str).unwrap_or("");
        let (started, ended) = item_times(core, &id, item, done);
        let cwd = thread.and_then(|thread| core.state.tasks.iter().find(|t| t.id == thread)).and_then(|t| t.cwd.clone()).unwrap_or_else(|| core.state.cwd.clone());
        match ty {
            "userMessage" => {
                let text = item
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|c| c.iter().filter_map(|x| x.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"))
                    .unwrap_or_default();
                let text = text.split("\n\nAttached files:\n").next().unwrap_or(&text).to_owned();
                let client = thread.is_none().then(|| item.get("clientId").and_then(Value::as_str).map(str::to_owned)).flatten();
                let key = client.clone().unwrap_or_else(|| id.clone());
                if let Some(c) = &client
                    && core.state.delivery.contains_key(c)
                {
                    core.state.delivery.insert(c.clone(), if done { Delivery::Completed } else { Delivery::Started });
                    core.touch_state();
                }
                if thread.is_none() && core.state.goal.is_empty() && !text.trim().is_empty() {
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
                core.upsert(&key, parent.clone(), Body::User { text, images, client_id: client, origin: Some("human".into()), attachments });
            }
            "agentMessage" => {
                let text = item.get("text").and_then(Value::as_str).unwrap_or("").to_owned();
                if thread.is_none() && done && !text.trim().is_empty() {
                    core.state.summary = clip_middle(text.trim(), 600).0;
                    core.touch_state();
                }
                if done || !text.is_empty() {
                    core.upsert(&id, parent.clone(), Body::Assistant { text, streaming: !done });
                } else if core.get(&id).is_none() {
                    core.upsert(&id, parent.clone(), Body::Assistant { text: String::new(), streaming: true });
                }
            }
            "reasoning" => {
                let parts = |k: &str| -> String {
                    item.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().or_else(|| x.get("text").and_then(Value::as_str))).collect::<Vec<_>>().join("\n\n")).unwrap_or_default()
                };
                let text = if parts("summary").is_empty() { parts("content") } else { parts("summary") };
                if done || core.get(&id).is_none() {
                    let redacted = done && text.is_empty();
                    core.upsert(&id, parent.clone(), Body::Thinking { text, streaming: !done, redacted });
                }
            }
            "plan" => {
                if done || core.get(&id).is_none() {
                    let text = item.get("text").and_then(Value::as_str).unwrap_or("").to_owned();
                    core.upsert(&id, parent.clone(), Body::Assistant { text, streaming: !done });
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
                    started,
                    ended,
                    server: None,
                    tool_short: None,
                    skill: None,
                    runner: None,
                };
                if thread.is_none() && !done {
                    core.state.doing = Some(super::claude::doing(&call));
                    core.touch_state();
                }
                // A terminal (unified exec): it can go on after its call and the turn.
                if thread.is_none() && let Some(process) = item.get("processId").and_then(Value::as_str) {
                    self.terminal(core, &id, process, &command, item, done, exit);
                }
                core.upsert(&id, parent.clone(), Body::Tool(Box::new(call)));
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
                if thread.is_none() && done && status == ToolStatus::Done {
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
                    started,
                    ended,
                    server: None,
                    tool_short: None,
                    skill: None,
                    runner: None,
                };
                if thread.is_none() && !done {
                    core.state.doing = Some(super::claude::doing(&call));
                    core.touch_state();
                }
                core.upsert(&id, parent.clone(), Body::Tool(Box::new(call)));
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
                    started,
                    ended,
                    server: None,
                    tool_short: None,
                    skill: None,
                    runner: shown.as_ref().and_then(|s| s.runner.clone()).or_else(|| crate::reach::remote(server, tool_name, item.get("arguments").unwrap_or(&Value::Null))),
                };
                core.upsert(&id, parent.clone(), Body::Tool(Box::new(call)));
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
                    started,
                    ended,
                    server: None,
                    tool_short: None,
                    skill: None,
                    runner: None,
                };
                core.upsert(&id, parent.clone(), Body::Tool(Box::new(call)));
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
                        // Child communications can address their parent or siblings; they do
                        // not establish a new parent/child relationship.
                        if self.thread.as_deref() == Some(target) || thread == Some(target)
                            || (thread.is_some() && !spawned && !core.state.tasks.iter().any(|t| t.id == target && t.parent_task.as_deref() == thread))
                        {
                            continue;
                        }
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
                            subagent(core, target, &id, &desc, child, spawned, thread);
                            if let Some(t) = core.state.tasks.iter_mut().find(|t| t.id == target) {
                                if spawned || item.get("tool").and_then(Value::as_str) == Some("followupTask") {
                                    t.prompt = item.get("prompt").and_then(Value::as_str).map(|p| clip_middle(p, OUTPUT_MAX).0).or(t.prompt.take());
                                }
                                if let Some(message) = states.and_then(|s| s.get(target)).and_then(|s| s.get("message")).and_then(Value::as_str) {
                                    t.summary = Some(clip_middle(message, OUTPUT_MAX).0);
                                }
                            }
                            self.child_changed(core, target);
                        }
                        task.get_or_insert_with(|| target.to_owned());
                    }
                    status
                } else {
                    let Some(target) = item.get("agentThreadId").and_then(Value::as_str) else { return };
                    let kind = item.get("kind").and_then(Value::as_str);
                    let known = core.state.tasks.iter().any(|t| t.id == target);
                    let child = match kind {
                        Some("started") => Some("running"),
                        Some("interrupted") => Some("stopped"),
                        Some("completed") => Some("completed"),
                        // As in Codex's own TUI, an interaction (`send_message`, `followup_task`)
                        // doesn't change whether an agent runs: a message queued for an idle one
                        // starts nothing (its last "completed" stands), and a follow-up's turn is
                        // heard from the agent itself (its turn, or the read that follows). One
                        // not known yet is read to find out.
                        Some("interacted") if known => None,
                        Some("interacted") => Some("completed"),
                        _ => return,
                    };
                    let spawned = kind == Some("started");
                    if self.thread.as_deref() != Some(target) && thread != Some(target)
                        && (thread.is_none() || spawned || core.state.tasks.iter().any(|t| t.id == target && t.parent_task.as_deref() == thread))
                    {
                        if let Some(child) = child {
                            subagent(core, target, &id, &desc, child, spawned, thread);
                        }
                        self.child_changed(core, target);
                        // A follow-up's turn can start just after that read: once more, later.
                        if kind == Some("interacted")
                            && let Some(c) = self.children.get_mut(target)
                        {
                            c.refresh = true;
                        }
                        task = Some(target.to_owned());
                    }
                    if done { ToolStatus::Done } else { ToolStatus::Running }
                };
                let call = ToolCall {
                    tool_id: id.clone(),
                    name: ty.to_owned(),
                    tool: ToolKind::Agent,
                    title: clip_middle(&desc, 200).0,
                    detail: item.get("model").and_then(Value::as_str).map(str::to_owned),
                    input: json!({"kind": item.get("kind"), "tool": item.get("tool"), "model": item.get("model"), "reasoningEffort": item.get("reasoningEffort")}),
                    status,
                    output: None,
                    truncated: false,
                    diffs: Vec::new(),
                    todos: Vec::new(),
                    task,
                    exit_code: None,
                    started,
                    ended,
                    server: None,
                    tool_short: None,
                    skill: None,
                    runner: None,
                };
                core.upsert(&id, parent.clone(), Body::Tool(Box::new(call)));
            }
            "contextCompaction" if thread.is_none() && done => self.notice(core, Level::Info, "Conversation compacted"),
            "enteredReviewMode" if thread.is_none() && done => self.notice(core, Level::Info, "Reviewing"),
            "exitedReviewMode" if thread.is_none() && done => self.notice(core, Level::Info, "Review finished"),
            _ => {}
        }
        if let Some(thread) = thread {
            let activity = core.get(&id).and_then(|e| match &e.body {
                Body::Tool(t) => Some(super::claude::doing(t)),
                _ => None,
            });
            let count = core.events().filter(|e| e.parent == parent && matches!(&e.body, Body::Tool(_))).count();
            if let Some(task) = core.state.tasks.iter_mut().find(|t| t.id == thread) {
                if let Some(activity) = activity {
                    task.last_tool = Some(activity);
                }
                task.tool_uses = count as u64;
                match ty {
                    "userMessage" if task.prompt.is_none() => {
                        let text = item.get("content").and_then(Value::as_array).into_iter().flatten()
                            .filter_map(|v| v.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n");
                        if !text.trim().is_empty() {
                            task.prompt = Some(clip_middle(&text, OUTPUT_MAX).0);
                        }
                    }
                    "agentMessage" if done => {
                        if let Some(text) = item.get("text").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
                            task.summary = Some(clip_middle(text, OUTPUT_MAX).0);
                        }
                    }
                    _ => {}
                }
                core.touch_state();
            }
        } else {
            self.settle(core);
        }
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

fn item_times(core: &Core, id: &str, item: &Value, done: bool) -> (i64, Option<i64>) {
    let previous = core.get(id).and_then(|e| match &e.body { Body::Tool(t) => Some(t), _ => None });
    let ended = done.then(|| previous.and_then(|t| t.ended).unwrap_or_else(Core::now));
    let duration = item.get("durationMs").and_then(Value::as_i64).unwrap_or(0).max(0);
    let started = previous.map_or_else(|| ended.unwrap_or_else(Core::now).saturating_sub(duration), |t| t.started);
    (started, ended)
}

fn child_event_id(thread: &str, item: &str) -> String {
    format!("child:{thread}:{item}")
}

fn collab_replayed(core: &Core, children: &HashMap<String, Child>, item: &Value, thread: Option<&str>) -> bool {
    let mut targets: Vec<&str> = if let Some(target) = item.get("agentThreadId").and_then(Value::as_str) {
        vec![target]
    } else {
        item.get("receiverThreadIds").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect()
    };
    targets.extend(item.get("agentsStates").and_then(Value::as_object).into_iter().flat_map(|states| states.keys().map(String::as_str)));
    matches!(item.get("type").and_then(Value::as_str), Some("subAgentActivity" | "collabAgentToolCall"))
        && targets.iter().all(|target| children.contains_key(*target) && core.state.tasks.iter().any(|t| t.id == *target))
        && item.get("id").and_then(Value::as_str).is_some_and(|id| {
            let id = thread.map_or_else(|| id.to_owned(), |thread| child_event_id(thread, id));
            core.get(&id).is_some_and(|e| matches!(&e.body, Body::Tool(t) if !matches!(t.status, ToolStatus::Running | ToolStatus::Pending)))
        })
}

/// Largest rollout read for a native child kiln never read (its end).
const ROLLOUT_MAX: u64 = 64 << 20;

/// Native children kiln never read: their rows, with none of their work in the timeline (an older
/// image kept rows without reading them). In a session that runs another harness now, no Codex can
/// be asked, so their work comes from Codex's own record of their threads ([`read_rollouts`]).
pub(super) fn unread_children(core: &Core) -> Vec<String> {
    let thread_id = |id: &str| id.len() == 36 && id.char_indices().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { c == '-' } else { c.is_ascii_hexdigit() });
    core.state
        .tasks
        .iter()
        .filter(|t| t.class.as_deref() == Some("agent") && t.tool_id.is_some() && thread_id(&t.id))
        .filter(|t| {
            let prefix = child_event_id(&t.id, "");
            !core.events().any(|e| e.id.starts_with(&prefix))
        })
        .map(|t| t.id.clone())
        .collect()
}

/// The records of these native children's threads, from their rollouts under Codex's home `home`
/// (blocking: reads them), each oldest first; one without a rollout is left out.
pub(super) fn read_rollouts(home: &std::path::Path, threads: Vec<String>) -> Vec<(String, Vec<Value>)> {
    let mut out = Vec::new();
    for thread in threads {
        let mut segments = Vec::new();
        for file in super::handoff::codex_files(home, &thread) {
            let Some(lines) = super::transcript::tail_lines(&file, ROLLOUT_MAX) else { continue };
            let records: Vec<Value> = lines.iter().filter_map(|l| serde_json::from_str(l).ok()).collect();
            let meta = records.iter().find(|r| r.get("type").and_then(Value::as_str) == Some("session_meta"));
            // Its own (a branch of it is another thread's file).
            if meta.and_then(|m| m.pointer("/payload/id")).and_then(Value::as_str).is_some_and(|id| id != thread) {
                continue;
            }
            let first = meta.and_then(|m| m.get("ordinal")).and_then(Value::as_u64);
            let base = meta.and_then(|m| m.pointer("/payload/history_base/end_ordinal_exclusive")).and_then(Value::as_u64);
            segments.push((first, base, records));
        }
        let records = super::handoff::codex_history(segments);
        if !records.is_empty() {
            out.push((thread, records));
        }
    }
    out
}

/// A native child's own turns from its rollout records, as `thread/turns/list` gives them (oldest
/// first). The history of its parent a fork starts with is left out (its own turns start with the
/// first one another agent started), and a turn without an end was cut off: no Codex runs it.
pub(super) fn rollout_turns(records: &[Value]) -> Vec<Value> {
    fn event(r: &Value) -> Option<&str> {
        if r.get("type").and_then(Value::as_str) == Some("event_msg") { r.pointer("/payload/type").and_then(Value::as_str) } else { None }
    }
    let own = records.iter().position(|r| event(r) == Some("task_started") && r.pointer("/payload/turn_attribution/initiating_agent_path").is_some_and(|p| !p.is_null())).unwrap_or(0);
    let mut turns: Vec<Value> = Vec::new();
    for r in records.iter().skip(own) {
        let p = r.get("payload").unwrap_or(&Value::Null);
        let id = p.get("turn_id").and_then(Value::as_str);
        if event(r) == Some("task_started") {
            turns.push(json!({"id": id, "status": "inProgress", "startedAt": p.get("started_at"), "items": []}));
            continue;
        }
        let Some(turn) = turns.iter_mut().rev().find(|t| id.is_none() || t.get("id").and_then(Value::as_str) == id).and_then(Value::as_object_mut) else { continue };
        let ended = |turn: &mut serde_json::Map<String, Value>, status: &str| {
            if turn.get("status").and_then(Value::as_str) != Some("failed") {
                turn.insert("status".into(), json!(status));
            }
            turn.insert("completedAt".into(), p.get("completed_at").cloned().unwrap_or(Value::Null));
            turn.insert("durationMs".into(), p.get("duration_ms").cloned().unwrap_or(Value::Null));
        };
        match event(r) {
            Some("task_complete") => {
                if let Some(message) = p.pointer("/error/message").and_then(Value::as_str) {
                    turn.insert("status".into(), json!("failed"));
                    turn.insert("error".into(), json!({"message": message}));
                }
                ended(turn, "completed");
            }
            Some("turn_aborted") => ended(turn, "interrupted"),
            Some("item_completed") => {
                if let (Some(item), Some(items)) = (p.get("item").and_then(rollout_item), turn.get_mut("items").and_then(Value::as_array_mut)) {
                    items.push(item);
                }
            }
            _ => {}
        }
    }
    for turn in turns.iter_mut().filter_map(Value::as_object_mut) {
        if turn.get("status").and_then(Value::as_str) == Some("inProgress") {
            turn.insert("status".into(), json!("interrupted"));
        }
    }
    turns
}

/// One item of a rollout (Codex's own form) as the app server shows it; None: one kiln doesn't
/// show for a child.
fn rollout_item(item: &Value) -> Option<Value> {
    let s = |k: &str| item.get(k).cloned().unwrap_or(Value::Null);
    let id = item.get("id").and_then(Value::as_str)?;
    Some(match item.get("type").and_then(Value::as_str)? {
        "UserMessage" => json!({"type": "userMessage", "id": id, "content": s("content")}),
        "AgentMessage" => {
            let text: String = item.get("content").and_then(Value::as_array).into_iter().flatten().filter_map(|c| c.get("text").and_then(Value::as_str)).collect();
            json!({"type": "agentMessage", "id": id, "text": text, "phase": s("phase")})
        }
        "Reasoning" => json!({"type": "reasoning", "id": id, "summary": s("summary_text"), "content": s("raw_content")}),
        "CommandExecution" => {
            let command = match item.get("command") {
                Some(Value::Array(words)) => words.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "),
                Some(Value::String(c)) => c.clone(),
                _ => String::new(),
            };
            let actions: Vec<Value> = item
                .get("parsed_cmd")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|a| {
                    let kind = match a.get("type").and_then(Value::as_str) {
                        Some("list_files") => "listFiles",
                        Some(other) => other,
                        None => "unknown",
                    };
                    json!({"type": kind, "command": a.get("cmd"), "path": a.get("path"), "name": a.get("name")})
                })
                .collect();
            let duration = item.get("duration").map(|d| d.get("secs").and_then(Value::as_u64).unwrap_or(0) * 1000 + d.get("nanos").and_then(Value::as_u64).unwrap_or(0) / 1_000_000);
            let status = match item.get("status").and_then(Value::as_str) {
                Some("in_progress") | None => "inProgress",
                Some(other) => other,
            };
            let cwd = item.get("cwd").and_then(Value::as_str).map(|c| c.strip_prefix("file://").unwrap_or(c).to_owned());
            json!({
                "type": "commandExecution", "id": id, "command": command, "cwd": cwd, "status": status, "exitCode": s("exit_code"), "durationMs": duration,
                "aggregatedOutput": s("aggregated_output"), "commandActions": actions
            })
        }
        "FileChange" => {
            let changes: Vec<Value> = item
                .get("changes")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .map(|(path, c)| {
                    let diff = c.get("unified_diff").or_else(|| c.get("content")).cloned().unwrap_or(Value::Null);
                    json!({"path": path, "kind": {"type": c.get("type")}, "diff": diff})
                })
                .collect();
            json!({"type": "fileChange", "id": id, "status": s("status"), "changes": changes})
        }
        "McpToolCall" => {
            json!({"type": "mcpToolCall", "id": id, "server": s("server"), "tool": s("tool"), "arguments": s("arguments"), "status": s("status"), "result": s("result"), "error": s("error")})
        }
        "SubAgentActivity" => json!({"type": "subAgentActivity", "id": id, "kind": s("kind"), "agentThreadId": s("agent_thread_id"), "agentPath": s("agent_path"), "model": s("model")}),
        _ => return None,
    })
}

/// A native child's turns read from its rollout, under its spawning call as a read of its thread
/// puts them; nothing is asked of any harness, and nothing it shows runs (no Codex runs it).
pub(super) fn child_from_rollout(core: &mut Core, thread: &str, turns: &[Value]) {
    // Its row went meanwhile: what it did would have no call to go under (not the main thread's).
    if !core.state.tasks.iter().any(|t| t.id == thread && t.tool_id.is_some()) {
        return;
    }
    let writes = core.writes.len();
    let live: Vec<String> = core.state.tasks.iter().filter(|t| t.live()).map(|t| t.id.clone()).collect();
    // Codex's work, said by Codex (not the harness running now).
    let running = std::mem::replace(&mut core.harness, super::kinds::Kind::get("codex").map(|k| k.name.to_owned()));
    Codex::new(None, None, None, false).child_history(core, thread, turns);
    core.harness = running;
    // What it would ask of a Codex about the children it started: there's none.
    core.writes.truncate(writes);
    let now = Core::now();
    for t in core.state.tasks.iter_mut().filter(|t| t.live() && !live.contains(&t.id)) {
        t.status = "stopped".into();
        t.ended.get_or_insert(now);
    }
    core.touch_state();
}

fn child_turn_status(turn: &Value) -> Option<&'static str> {
    match turn.get("status").and_then(Value::as_str)? {
        "inProgress" => Some("running"),
        "completed" => Some("completed"),
        "failed" => Some("failed"),
        "interrupted" => Some("stopped"),
        _ => None,
    }
}

/// Unloaded and idle threads need their latest turn to distinguish completion from a failure
/// or interruption. In particular, `notLoaded` does not mean an agent is still initializing.
fn child_runtime_status(status: &Value) -> Option<&'static str> {
    match status.get("type").and_then(Value::as_str)? {
        "active" if status.get("activeFlags").and_then(Value::as_array).is_some_and(|flags| flags.iter().any(|f| matches!(f.as_str(), Some("waitingOnApproval" | "waitingOnUserInput")))) => Some("paused"),
        "active" => Some("running"),
        "systemError" => Some("failed"),
        _ => None,
    }
}

fn child_status(core: &mut Core, id: &str, status: &str) {
    let mut ended_parent = None;
    if let Some(t) = core.state.tasks.iter_mut().find(|t| t.id == id) {
        t.status = status.to_owned();
        if t.live() {
            t.ended = None;
        } else {
            t.ended.get_or_insert_with(Core::now);
            ended_parent = t.tool_id.clone();
        }
        core.touch_state();
    }
    if let Some(parent) = ended_parent {
        let ids: Vec<String> = core.events().filter(|e| e.parent.as_deref() == Some(&parent)).map(|e| e.id.clone()).collect();
        for id in ids {
            if let Some(event) = core.get_mut(&id) {
                match &mut event.body {
                    Body::Assistant { streaming, .. } | Body::Thinking { streaming, .. } => *streaming = false,
                    Body::Tool(tool) if matches!(tool.status, ToolStatus::Running | ToolStatus::Pending) => {
                        tool.status = ToolStatus::Cancelled;
                        tool.ended.get_or_insert_with(Core::now);
                    }
                    _ => {}
                }
            }
        }
    }
}

/// A collab call can finish while its child keeps working; only the child's state ends its task.
#[allow(clippy::too_many_arguments)]
fn subagent(core: &mut Core, id: &str, tool: &str, desc: &str, status: &str, spawned: bool, parent_task: Option<&str>) {
    let depth = parent_task.and_then(|parent| core.state.tasks.iter().find(|t| t.id == parent)).map_or(1, |t| t.depth + 1);
    match core.state.tasks.iter_mut().find(|t| t.id == id) {
        Some(t) => {
            t.status = status.to_owned();
            if spawned {
                t.tool_id.get_or_insert_with(|| tool.to_owned());
                if t.prompt.is_none() {
                    t.description = clip_middle(desc, 300).0;
                }
            }
            if t.live() {
                t.ended = None;
            } else {
                t.ended.get_or_insert_with(Core::now);
            }
        }
        None => core.state.tasks.push(Task {
            id: id.to_owned(),
            tool_id: Some(if spawned { tool.to_owned() } else { format!("codex-agent:{id}") }),
            description: clip_middle(desc, 300).0,
            kind: "subagent".into(),
            class: Some("agent".into()),
            status: status.to_owned(),
            depth,
            parent_task: parent_task.map(str::to_owned),
            started: Core::now(),
            ended: (!matches!(status, "running" | "pending" | "paused")).then(Core::now),
            // Its turn under way can be interrupted (`stop_child_asked`).
            stoppable: true,
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
        if !matches!(status, ToolStatus::Running | ToolStatus::Pending) {
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

    /// A move's goal goes on in the new thread, once the conversation is in; without a
    /// conversation, as soon as the thread is open; not when Codex didn't take the conversation.
    #[test]
    fn a_goal_carried_over_is_set_on_the_new_thread_once_the_conversation_is_in() {
        let goal = json!({"type": super::super::moving::GOAL, "objective": "port the parser", "status": "active", "tokenBudget": 200_000});
        let said = json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "port the parser"}]});
        let open = |items: Vec<Value>| {
            let mut core = Core::new();
            core.state.cwd = "/work".into();
            let mut c = Codex::new(None, None, None, false);
            c.seed(items);
            c.start(&mut core);
            feed(&mut c, &mut core, &[json!({"id": 1, "result": {"userAgent": "codex_cli_rs/0.162.1"}})]);
            feed(&mut c, &mut core, &[json!({"id": 2, "result": {"thread": {"id": "th2", "turns": []}, "model": "gpt-5", "cwd": "/work"}})]);
            let w = written(&mut core);
            (c, core, w)
        };
        let set = |w: &[Value]| w.iter().find(|v| v.get("method") == Some(&json!("thread/goal/set"))).and_then(|v| v.get("params")).cloned();
        let (mut c, mut core, w) = open(vec![said.clone(), goal.clone()]);
        let inject = w.iter().find(|v| v.get("method") == Some(&json!("thread/inject_items"))).cloned().unwrap_or_default();
        // The conversation alone goes in; the goal waits for it.
        assert_eq!(inject.pointer("/params/items"), Some(&json!([said.clone()])));
        assert_eq!(set(&w), None);
        respond(&mut c, &mut core, &w, "thread/inject_items", json!({}));
        assert!(c.seeded());
        assert_eq!(set(&written(&mut core)), Some(json!({"threadId": "th2", "objective": "port the parser", "status": "active", "tokenBudget": 200_000, "origin": "user"})));
        // A goal alone: set once the thread is open.
        let (c, _, w) = open(vec![goal.clone()]);
        assert!(c.seeded());
        assert!(w.iter().all(|v| v.get("method") != Some(&json!("thread/inject_items"))));
        assert_eq!(set(&w).and_then(|p| p.get("threadId").cloned()), Some(json!("th2")));
        // Codex didn't take the conversation: no goal to pursue from nothing.
        let (mut c, mut core, w) = open(vec![said, goal]);
        let id = w.iter().find(|v| v.get("method") == Some(&json!("thread/inject_items"))).and_then(|v| v.get("id")).cloned().unwrap_or_default();
        feed(&mut c, &mut core, &[json!({"id": id, "error": {"code": -32600, "message": "no"}})]);
        assert_eq!(set(&written(&mut core)), None);
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
    fn an_idle_mode_change_sets_the_next_turns_policies_and_persists_bypass() -> anyhow::Result<()> {
        let (mut c, mut core, _) = opened();
        assert!(core.meta.modes.iter().any(|m| m.id == "read-only"));
        assert!(core.meta.features.iter().any(|f| f == "permissions_live"));
        // An approval policy chosen on its own, then a mode: the mode's policies win.
        c.command(&mut core, Command::Control { id: "approval_policy".into(), value: Some(json!("untrusted")) });
        c.command(&mut core, Command::SetMode { mode: "read-only".into() });
        assert_eq!(core.state.mode, "read-only");
        assert_eq!(core.state.control_values.get("approval_policy"), Some(&json!("on-request")));
        assert!(!written(&mut core).iter().any(|v| v.get("method") == Some(&json!("turn/interrupt"))));
        c.command(&mut core, Command::Send { id: "m1".into(), text: "hi".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        let w = written(&mut core);
        let start = w.iter().find(|v| v.get("method").and_then(Value::as_str) == Some("turn/start")).cloned().ok_or_else(|| anyhow::anyhow!("no turn"))?;
        assert_eq!(start.pointer("/params/approvalPolicy").and_then(Value::as_str), Some("on-request"));
        assert_eq!(start.pointer("/params/sandboxPolicy/type").and_then(Value::as_str), Some("readOnly"));
        // Full access is always a choice: chosen, it is allowed, its sandbox with it.
        c.command(&mut core, Command::SetMode { mode: "full-access".into() });
        assert_eq!(core.state.mode, "full-access");
        assert_eq!(core.state.control_values.get("sandbox"), Some(&json!("danger-full-access")));
        assert!(core.effects.contains(&Effect::AllowBypass));
        // Not a mode at all: refused.
        c.command(&mut core, Command::SetMode { mode: "nope".into() });
        assert_eq!(core.state.mode, "full-access");
        Ok(())
    }

    #[test]
    fn a_running_permission_change_cancels_only_its_requests_and_continues_before_the_queue() -> anyhow::Result<()> {
        let (mut c, mut core, _) = opened();
        c.command(&mut core, Command::Send { id: "m1".into(), text: "finish the change".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        let start = written(&mut core);
        respond(&mut c, &mut core, &start, "turn/start", json!({"turn": {"id": "t1", "status": "inProgress"}}));
        c.command(&mut core, Command::Send { id: "m2".into(), text: "then run tests".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        core.state.tasks.push(Task { id: "child".into(), description: "Review".into(), class: Some("agent".into()), ..Task::default() });
        for (id, thread) in [("own-ask", "th1"), ("child-ask", "child")] {
            feed(&mut c, &mut core, &[json!({"id": id, "method": "item/commandExecution/requestApproval",
                "params": {"threadId": thread, "turnId": "t1", "itemId": id, "command": "gh pr list", "cwd": "/work"}})]);
        }
        assert_eq!(core.state.approvals.iter().find(|a| a.id == "child-ask").and_then(|a| a.agent.as_deref()), Some("Review"));
        c.command(&mut core, Command::SetMode { mode: "full-access".into() });
        let w = written(&mut core);
        // The turn, and the subagent (it keeps the permissions it started with, which asked):
        // both stopped, neither request answered.
        let stopped: Vec<(&str, &str)> = w.iter().filter(|v| v.get("method") == Some(&json!("turn/interrupt")))
            .filter_map(|v| Some((v.pointer("/params/threadId")?.as_str()?, v.pointer("/params/turnId")?.as_str()?)))
            .collect();
        assert_eq!(stopped, [("th1", "t1"), ("child", "t1")]);
        assert!(!w.iter().any(|v| v.get("result").is_some() || v.get("method") == Some(&json!("turn/start"))));
        // A duplicate choice does not interrupt twice or add another continuation.
        c.command(&mut core, Command::SetMode { mode: "full-access".into() });
        assert!(written(&mut core).is_empty());
        feed(&mut c, &mut core, &[json!({"method": "turn/completed", "params": {"threadId": "th1", "turn": {"id": "t1", "status": "interrupted"}}})]);
        let w = written(&mut core);
        let next = w.iter().find(|v| v.get("method") == Some(&json!("turn/start"))).ok_or_else(|| anyhow::anyhow!("no continuation"))?;
        assert_eq!(next.pointer("/params/approvalPolicy"), Some(&json!("never")));
        assert_eq!(next.pointer("/params/sandboxPolicy"), Some(&json!({"type": "dangerFullAccess"})));
        let said = next.pointer("/params/input/0/text").and_then(Value::as_str).unwrap_or_default();
        assert!(said.contains("Continue the work") && said.contains("kiln stopped your subagent Review (child)") && said.contains("Start new subagents"), "{said}");
        assert_eq!(core.state.queue.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["m2"]);
        assert_eq!(core.state.approvals.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["child-ask"]);
        assert!(c.asks.contains_key("child-ask") && !c.asks.contains_key("own-ask"));
        assert!(core.events().any(|e| matches!(&e.body, Body::User { origin: Some(o), text, .. } if o == "kiln" && text.contains("permissions"))));
        // An ordinary request in full access is still a request, never an automatic approval.
        feed(&mut c, &mut core, &[json!({"id": "new-ask", "method": "item/commandExecution/requestApproval", "params": {"threadId": "th1", "turnId": "t2", "command": "gh pr list"}})]);
        assert!(core.state.approvals.iter().any(|a| a.id == "new-ask"));
        assert!(!written(&mut core).iter().any(|v| v.get("result").is_some()));
        Ok(())
    }

    #[test]
    fn changing_permissions_while_the_turn_starts_waits_for_its_id() {
        let (mut c, mut core, _) = opened();
        c.command(&mut core, Command::Send { id: "m1".into(), text: "work".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        let start = written(&mut core);
        c.command(&mut core, Command::SetMode { mode: "full-access".into() });
        assert!(!written(&mut core).iter().any(|v| v.get("method") == Some(&json!("turn/interrupt"))));
        respond(&mut c, &mut core, &start, "turn/start", json!({"turn": {"id": "t1", "status": "inProgress"}}));
        let w = written(&mut core);
        assert!(w.iter().any(|v| v.get("method") == Some(&json!("turn/interrupt")) && v.pointer("/params/turnId") == Some(&json!("t1"))));
        feed(&mut c, &mut core, &[json!({"method": "turn/started", "params": {"threadId": "th1", "turn": {"id": "t1"}}})]);
        assert!(!written(&mut core).iter().any(|v| v.get("method") == Some(&json!("turn/interrupt"))));
    }

    #[test]
    fn a_late_start_reply_cannot_resurrect_the_turn_interrupted_for_permissions() {
        let (mut c, mut core, _) = opened();
        c.command(&mut core, Command::Send { id: "m1".into(), text: "work".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        let start = written(&mut core);
        feed(&mut c, &mut core, &[json!({"method": "turn/started", "params": {"threadId": "th1", "turn": {"id": "t1"}}})]);
        c.command(&mut core, Command::SetMode { mode: "full-access".into() });
        written(&mut core);
        feed(&mut c, &mut core, &[json!({"method": "turn/completed", "params": {"threadId": "th1", "turn": {"id": "t1", "status": "interrupted"}}})]);
        assert!(!written(&mut core).iter().any(|v| v.get("method") == Some(&json!("turn/start"))));
        respond(&mut c, &mut core, &start, "turn/start", json!({"turn": {"id": "t1", "status": "inProgress"}}));
        assert!(written(&mut core).iter().any(|v| v.get("method") == Some(&json!("turn/start")) && v.pointer("/params/approvalPolicy") == Some(&json!("never"))));
        feed(&mut c, &mut core, &[json!({"method": "turn/completed", "params": {"threadId": "th1", "turn": {"id": "t1", "status": "interrupted"}}})]);
        assert_eq!(c.turn_permissions, Some(("never".into(), "danger-full-access".into())));
        assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Cancelled));
        assert!(c.turn.is_none());
    }

    #[test]
    fn a_permission_change_does_not_continue_finished_or_explicitly_stopped_work() {
        for (status, stop) in [("completed", false), ("interrupted", true)] {
            let (mut c, mut core, _) = opened();
            c.command(&mut core, Command::Send { id: "m1".into(), text: "work".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
            let start = written(&mut core);
            respond(&mut c, &mut core, &start, "turn/start", json!({"turn": {"id": "t1", "status": "inProgress"}}));
            c.command(&mut core, Command::SetMode { mode: "full-access".into() });
            let interrupt = written(&mut core);
            if stop {
                c.command(&mut core, Command::Interrupt);
                written(&mut core);
            }
            feed(&mut c, &mut core, &[json!({"method": "turn/completed", "params": {"threadId": "th1", "turn": {"id": "t1", "status": status}}})]);
            // The turn may finish naturally before its interrupt is acknowledged.
            respond(&mut c, &mut core, &interrupt, "turn/interrupt", json!({}));
            assert!(!written(&mut core).iter().any(|v| v.get("method") == Some(&json!("turn/start"))));
            assert!(!c.permissions_changing && core.state.queue.is_empty());
        }
    }

    #[test]
    fn an_old_permission_interrupt_cannot_block_or_cancel_the_next_turns_change() {
        for failed in [false, true] {
            let (mut c, mut core, _) = opened();
            c.command(&mut core, Command::Send { id: "m1".into(), text: "work".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
            let start = written(&mut core);
            respond(&mut c, &mut core, &start, "turn/start", json!({"turn": {"id": "t1", "status": "inProgress"}}));
            c.command(&mut core, Command::SetMode { mode: "full-access".into() });
            let interrupt = written(&mut core);
            feed(&mut c, &mut core, &[json!({"method": "turn/completed", "params": {"threadId": "th1", "turn": {"id": "t1", "status": "interrupted"}}})]);
            let next = written(&mut core);
            respond(&mut c, &mut core, &next, "turn/start", json!({"turn": {"id": "t2", "status": "inProgress"}}));
            c.command(&mut core, Command::SetMode { mode: "read-only".into() });
            let w = written(&mut core);
            assert!(w.iter().any(|v| v.get("method") == Some(&json!("turn/interrupt")) && v.pointer("/params/turnId") == Some(&json!("t2"))));
            if failed {
                let id = interrupt.iter().find(|v| v.get("method") == Some(&json!("turn/interrupt"))).and_then(|v| v.get("id")).cloned().unwrap_or_default();
                feed(&mut c, &mut core, &[json!({"id": id, "error": {"message": "turn already ended"}})]);
            } else {
                respond(&mut c, &mut core, &interrupt, "turn/interrupt", json!({}));
            }
            assert!(c.permissions_changing);
            feed(&mut c, &mut core, &[json!({"method": "turn/completed", "params": {"threadId": "th1", "turn": {"id": "t2", "status": "interrupted"}}})]);
            assert!(written(&mut core).iter().any(|v| v.get("method") == Some(&json!("turn/start")) && v.pointer("/params/sandboxPolicy/type") == Some(&json!("readOnly"))));
        }
    }

    #[test]
    fn an_explicit_replacement_does_not_requeue_the_work_interrupted_for_permissions() {
        let (mut c, mut core, _) = opened();
        c.command(&mut core, Command::Send { id: "m1".into(), text: "old work".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        let start = written(&mut core);
        respond(&mut c, &mut core, &start, "turn/start", json!({"turn": {"id": "t1", "status": "inProgress"}}));
        c.command(&mut core, Command::SetMode { mode: "full-access".into() });
        written(&mut core);
        c.command(&mut core, Command::Send { id: "m2".into(), text: "do this instead".into(), mode: SendMode::Now, attachments: vec![], overrides: None });
        written(&mut core);
        feed(&mut c, &mut core, &[json!({"method": "turn/completed", "params": {"threadId": "th1", "turn": {"id": "t1", "status": "interrupted"}}})]);
        let w = written(&mut core);
        assert!(w.iter().any(|v| v.get("method") == Some(&json!("turn/start")) && v.pointer("/params/clientUserMessageId") == Some(&json!("m2"))));
        assert!(core.state.queue.is_empty() && c.waiting.is_empty());
    }

    #[test]
    fn separate_permission_controls_are_validated_labeled_and_restored() {
        let (mut c, mut core, _) = opened();
        c.command(&mut core, Command::SetMode { mode: "full-access".into() });
        c.command(&mut core, Command::Control { id: "approval_policy".into(), value: Some(json!("on-request")) });
        assert_eq!(core.state.mode, "custom");
        assert_eq!(c.policies(&core).0, "on-request");
        assert_eq!(c.policies(&core).1, "danger-full-access");
        let mut restarted = Codex::new(None, None, Some(core.state.mode.clone()), true);
        restarted.start(&mut core);
        assert_eq!(restarted.policies(&core), c.policies(&core));
        assert_eq!(core.state.mode, "custom");
        assert!(core.meta.modes.iter().any(|m| m.id == "custom"));
        c.command(&mut core, Command::Control { id: "sandbox".into(), value: Some(json!("no-such-policy")) });
        assert_eq!(c.policies(&core).1, "danger-full-access");
        c.command(&mut core, Command::Control { id: "sandbox".into(), value: Some(json!("workspace-write")) });
        assert_eq!(core.state.mode, "auto");
        assert!(!core.meta.modes.iter().any(|m| m.id == "custom"));
        // Explicit session boundaries still exclude bypass controls.
        let mut limited = Codex::new(None, None, None, false);
        limited.thread = Some("limited".into());
        limited.command(&mut core, Command::Control { id: "approval_policy".into(), value: Some(json!("never")) });
        assert_eq!(limited.policies(&core).0, "on-request");
    }

    #[test]
    fn an_adopted_adapter_repairs_legacy_labels_and_reapplies_an_unknown_turns_mode() {
        let (mut c, mut core, _) = opened();
        core.state.mode = "full-access".into();
        c.allow_bypass = true;
        c.approval = Some("on-request".into());
        c.sandbox = None;
        c.turn = Some("legacy".into());
        c.turn_permissions = None;
        core.meta.features.retain(|f| f != "permissions_live");
        c.tick(&mut core);
        assert!(core.meta.features.iter().any(|f| f == "permissions_live"));
        assert_eq!(core.state.mode, "custom");
        assert_eq!(c.policies(&core).1, "danger-full-access");
        assert!(!written(&mut core).iter().any(|v| v.get("method") == Some(&json!("turn/interrupt"))));
        c.command(&mut core, Command::SetMode { mode: "full-access".into() });
        assert!(core.effects.contains(&Effect::AllowBypass));
        assert!(written(&mut core).iter().any(|v| v.get("method") == Some(&json!("turn/interrupt"))));
        // An old header already saying full access can be explicitly selected again too.
        c.permissions_changing = false;
        c.pending.retain(|_, p| !matches!(p, Pending::PermissionInterrupt(_)));
        c.command(&mut core, Command::SetMode { mode: "full-access".into() });
        assert!(written(&mut core).iter().any(|v| v.get("method") == Some(&json!("turn/interrupt"))));
    }

    /// Thread `from`'s turn `turn` starts subagent `kid`, which starts its turn `<kid>-turn`.
    fn spawn(c: &mut Codex, core: &mut Core, from: &str, turn: &str, kid: &str) {
        feed(c, core, &[
            json!({"method": "item/completed", "params": {"threadId": from, "turnId": turn, "item": {
                "type": "collabAgentToolCall", "id": format!("spawn-{kid}"), "tool": "spawnAgent", "senderThreadId": from,
                "receiverThreadIds": [kid], "status": "completed", "agentsStates": {kid: {"status": "running"}}, "prompt": "Audit the changes"}}}),
            json!({"method": "turn/started", "params": {"threadId": kid, "turn": {"id": format!("{kid}-turn")}}}),
        ]);
    }

    fn interrupts(w: &[Value]) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = w.iter().filter(|v| v.get("method") == Some(&json!("turn/interrupt")))
            .filter_map(|v| Some((v.pointer("/params/threadId")?.as_str()?.to_owned(), v.pointer("/params/turnId")?.as_str()?.to_owned())))
            .collect();
        v.sort();
        v
    }

    fn pair(a: &str, b: &str) -> (String, String) {
        (a.to_owned(), b.to_owned())
    }

    #[test]
    fn what_permissions_allow_beyond_others() {
        let auto = pair("on-request", "workspace-write");
        let full = pair("never", "danger-full-access");
        assert!(exceeds(Some(&full), &auto) && !exceeds(Some(&auto), &full) && !exceeds(Some(&auto), &auto));
        assert!(exceeds(Some(&pair("on-request", "danger-full-access")), &auto));
        assert!(exceeds(Some(&auto), &pair("untrusted", "workspace-write")));
        // Unknown ones might allow more, unless nothing is more.
        assert!(exceeds(None, &auto) && !exceeds(None, &full));
        assert_eq!(and_list(&["a".into()]), "a");
        assert_eq!(and_list(&["a".into(), "b".into(), "c".into()]), "a, b and c");
    }

    #[test]
    fn under_full_access_a_subagent_started_before_asks_nobody_and_is_replaced() -> anyhow::Result<()> {
        let (mut c, mut core, _) = opened();
        // Auto: the turn, and the subagent it starts, work in the folder and ask beyond it.
        c.command(&mut core, Command::Send { id: "m1".into(), text: "review the PRs".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        let start = written(&mut core);
        respond(&mut c, &mut core, &start, "turn/start", json!({"turn": {"id": "t1", "status": "inProgress"}}));
        spawn(&mut c, &mut core, "th1", "t1", "kid");
        assert_eq!(c.children.get("kid").and_then(|k| k.permissions.clone()), Some(pair("on-request", "workspace-write")));
        // Full access chosen mid-turn: the turn goes on with it. The subagent may now do more
        // than it does, so it's left working.
        c.command(&mut core, Command::SetMode { mode: "full-access".into() });
        assert_eq!(interrupts(&written(&mut core)), [pair("th1", "t1")]);
        feed(&mut c, &mut core, &[json!({"method": "turn/completed", "params": {"threadId": "th1", "turn": {"id": "t1", "status": "interrupted"}}})]);
        let w = written(&mut core);
        let next = w.iter().find(|v| v.get("method") == Some(&json!("turn/start"))).ok_or_else(|| anyhow::anyhow!("no continuation"))?;
        let said = next.pointer("/params/input/0/text").and_then(Value::as_str).unwrap_or_default();
        assert!(said.contains("Subagents keep the permissions they were started with"), "{said}");
        respond(&mut c, &mut core, &w, "turn/start", json!({"turn": {"id": "t2", "status": "inProgress"}}));
        // Then it asks, which nothing started with full access does. The person isn't asked and
        // it isn't answered: the subagent is stopped, its parent told to start a new one.
        feed(&mut c, &mut core, &[json!({"id": "kid-ask", "method": "item/commandExecution/requestApproval",
            "params": {"threadId": "kid", "turnId": "kid-turn", "itemId": "x1", "command": "gh api repos/o/r/pulls", "cwd": "/work"}})]);
        assert!(core.state.approvals.is_empty());
        let w = written(&mut core);
        assert!(!w.iter().any(|v| v.get("result").is_some()));
        assert_eq!(interrupts(&w), [pair("kid", "kid-turn")]);
        let steer = w.iter().find(|v| v.get("method") == Some(&json!("turn/steer"))).ok_or_else(|| anyhow::anyhow!("parent not told"))?;
        assert_eq!(steer.pointer("/params/expectedTurnId"), Some(&json!("t2")));
        let told = steer.pointer("/params/input/0/text").and_then(Value::as_str).unwrap_or_default();
        assert!(told.contains("kiln stopped your subagent Audit the changes (kid)") && told.contains("new ones get your current permissions"), "{told}");
        assert!(core.events().any(|e| matches!(&e.body, Body::User { origin: Some(o), text, .. } if o == "kiln" && text.contains("kiln stopped"))));
        // Its turn ends, and what it asked with it.
        feed(&mut c, &mut core, &[
            json!({"method": "turn/completed", "params": {"threadId": "kid", "turn": {"id": "kid-turn", "status": "interrupted"}}}),
            json!({"method": "serverRequest/resolved", "params": {"threadId": "kid", "requestId": "kid-ask"}}),
        ]);
        assert!(c.withheld.is_empty() && !c.asks.contains_key("kid-ask") && core.state.approvals.is_empty());
        // A subagent started now has full access: what it might still ask is the person's.
        spawn(&mut c, &mut core, "th1", "t2", "kid2");
        assert_eq!(c.children.get("kid2").and_then(|k| k.permissions.clone()), Some(pair("never", "danger-full-access")));
        feed(&mut c, &mut core, &[json!({"id": "kid2-ask", "method": "item/commandExecution/requestApproval",
            "params": {"threadId": "kid2", "turnId": "kid2-turn", "itemId": "x2", "command": "gh api", "cwd": "/work"}})]);
        assert!(core.state.approvals.iter().any(|a| a.id == "kid2-ask"));
        assert!(interrupts(&written(&mut core)).is_empty());
        Ok(())
    }

    #[test]
    fn narrowing_permissions_stops_subagents_allowed_more_and_the_parent_replaces_them() -> anyhow::Result<()> {
        let (mut c, mut core, _) = opened();
        c.command(&mut core, Command::SetMode { mode: "full-access".into() });
        c.command(&mut core, Command::Send { id: "m1".into(), text: "work".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        let start = written(&mut core);
        respond(&mut c, &mut core, &start, "turn/start", json!({"turn": {"id": "t1", "status": "inProgress"}}));
        spawn(&mut c, &mut core, "th1", "t1", "kid");
        // A subagent's own subagent starts with its spawner's permissions.
        spawn(&mut c, &mut core, "kid", "kid-turn", "grandkid");
        assert_eq!(c.children.get("grandkid").and_then(|k| k.permissions.clone()), Some(pair("never", "danger-full-access")));
        c.command(&mut core, Command::SetMode { mode: "auto".into() });
        assert_eq!(interrupts(&written(&mut core)), [pair("grandkid", "grandkid-turn"), pair("kid", "kid-turn"), pair("th1", "t1")]);
        feed(&mut c, &mut core, &[json!({"method": "turn/completed", "params": {"threadId": "th1", "turn": {"id": "t1", "status": "interrupted"}}})]);
        let w = written(&mut core);
        let next = w.iter().find(|v| v.get("method") == Some(&json!("turn/start"))).ok_or_else(|| anyhow::anyhow!("no continuation"))?;
        assert_eq!(next.pointer("/params/approvalPolicy"), Some(&json!("on-request")));
        let said = next.pointer("/params/input/0/text").and_then(Value::as_str).unwrap_or_default();
        assert!(said.contains("Continue the work") && said.contains("kiln stopped your subagents") && said.contains("(kid)") && said.contains("(grandkid)"), "{said}");
        assert!(c.stopped.is_empty());
        Ok(())
    }

    /// Signed in on its machine: Codex reads its account at once, and a new account's sign-in takes
    /// the old one's limits away and reads the new one's.
    #[test]
    fn a_sign_in_reads_the_account_again_and_shows_the_new_one_s_limits() {
        let (mut c, mut core, w) = opened();
        respond(&mut c, &mut core, &w, "account/read", json!({"account": {"type": "chatgpt", "email": "a@example.com", "planType": "pro"}}));
        let old = core.state.account.clone();
        core.state.limits = vec![Limit { id: "codex.primary".into(), label: "Weekly".into(), used_percent: Some(95.0), account: old.clone(), ..Limit::default() }];
        let _ = written(&mut core);
        c.command(&mut core, Command::Control { id: "account".into(), value: None });
        let w = written(&mut core);
        assert!(w.iter().any(|v| v.get("method") == Some(&json!("account/read"))), "it reads its account again");
        respond(&mut c, &mut core, &w, "account/read", json!({"account": {"type": "chatgpt", "email": "b@example.com", "planType": "pro"}}));
        assert_ne!(core.state.account, old);
        assert!(core.state.limits.is_empty(), "the old account's limits go");
        assert!(written(&mut core).iter().any(|v| v.get("method") == Some(&json!("account/rateLimits/read"))), "the new account's are read");
    }

    /// Signed in as someone else outside kiln (`codex login` in a terminal): the account read every
    /// few minutes names the new one, the old one's limits go and the new one's are read.
    #[test]
    fn a_sign_in_made_outside_kiln_shows_at_the_next_look_at_the_account() {
        let (mut c, mut core, w) = opened();
        respond(&mut c, &mut core, &w, "account/read", json!({"account": {"type": "chatgpt", "email": "a@example.com", "planType": "pro"}}));
        let old = core.state.account.clone();
        let w = written(&mut core);
        respond(&mut c, &mut core, &w, "account/rateLimits/read", json!({"rateLimits": {"limitId": "codex", "primary": {"usedPercent": 95.0, "windowDurationMins": 10080}}}));
        assert!(!core.state.limits.is_empty());
        c.usage_at = 0;
        c.tick(&mut core);
        let w = written(&mut core);
        assert!(w.iter().any(|v| v.get("method") == Some(&json!("account/read"))), "it reads its account again");
        respond(&mut c, &mut core, &w, "account/read", json!({"account": {"type": "chatgpt", "email": "b@example.com", "planType": "pro"}}));
        assert_eq!(core.state.account, Some(account_id("codex", "Pro", Some("b@example.com"))));
        assert_ne!(core.state.account, old);
        assert!(core.state.limits.is_empty(), "the old account's limits go");
        assert!(written(&mut core).iter().any(|v| v.get("method") == Some(&json!("account/rateLimits/read"))), "the new account's are read");
    }

    /// A person stops a subagent from its row: its turn is interrupted, and its parent isn't told
    /// to start it again (that's only for a permission change's stop).
    #[test]
    fn a_person_stops_a_subagent_from_its_row() {
        let (mut c, mut core, _) = opened();
        c.command(&mut core, Command::Send { id: "m1".into(), text: "work".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        let start = written(&mut core);
        respond(&mut c, &mut core, &start, "turn/start", json!({"turn": {"id": "t1", "status": "inProgress"}}));
        spawn(&mut c, &mut core, "th1", "t1", "kid");
        assert!(core.state.tasks.iter().find(|t| t.id == "kid").is_some_and(|t| t.stoppable), "a subagent's row can stop it");
        let _ = written(&mut core);
        c.command(&mut core, Command::StopTask { task: "kid".into() });
        assert_eq!(interrupts(&written(&mut core)), [pair("kid", "kid-turn")]);
        assert!(c.stopped.is_empty(), "its parent isn't asked to start it again");
    }

    #[test]
    fn a_change_between_turns_that_stops_subagents_goes_on_with_their_work() -> anyhow::Result<()> {
        let (mut c, mut core, _) = opened();
        c.command(&mut core, Command::SetMode { mode: "full-access".into() });
        c.command(&mut core, Command::Send { id: "m1".into(), text: "work".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        let start = written(&mut core);
        respond(&mut c, &mut core, &start, "turn/start", json!({"turn": {"id": "t1", "status": "inProgress"}}));
        spawn(&mut c, &mut core, "th1", "t1", "kid");
        feed(&mut c, &mut core, &[json!({"method": "turn/completed", "params": {"threadId": "th1", "turn": {"id": "t1", "status": "completed"}}})]);
        written(&mut core);
        // The subagent works on after its parent's turn; read only is chosen.
        c.command(&mut core, Command::SetMode { mode: "read-only".into() });
        let w = written(&mut core);
        assert_eq!(interrupts(&w), [pair("kid", "kid-turn")]);
        let next = w.iter().find(|v| v.get("method") == Some(&json!("turn/start"))).ok_or_else(|| anyhow::anyhow!("parent not told"))?;
        assert_eq!(next.pointer("/params/sandboxPolicy/type"), Some(&json!("readOnly")));
        assert!(next.pointer("/params/input/0/text").and_then(Value::as_str).is_some_and(|s| s.starts_with("Your permissions were updated. kiln stopped your subagent")));
        Ok(())
    }

    #[test]
    fn an_adopted_session_already_in_full_access_moves_its_waiting_older_work_onto_it() -> anyhow::Result<()> {
        let (mut c, mut core, _) = opened();
        c.command(&mut core, Command::Send { id: "m1".into(), text: "work".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        let start = written(&mut core);
        respond(&mut c, &mut core, &start, "turn/start", json!({"turn": {"id": "t1", "status": "inProgress"}}));
        spawn(&mut c, &mut core, "th1", "t1", "kid");
        for (id, thread, turn) in [("own-ask", "th1", "t1"), ("kid-ask", "kid", "kid-turn")] {
            feed(&mut c, &mut core, &[json!({"id": id, "method": "item/commandExecution/requestApproval",
                "params": {"threadId": thread, "turnId": turn, "itemId": id, "command": "gh pr list", "cwd": "/work"}})]);
        }
        assert_eq!(core.state.approvals.len(), 2);
        // An older kiln changed the label only: the turn and its subagent kept what they started
        // with, and kiln didn't know what that was.
        core.state.mode = "full-access".into();
        c.allow_bypass = true;
        c.turn_permissions = None;
        if let Some(k) = c.children.get_mut("kid") {
            k.permissions = None;
        }
        written(&mut core);
        c.tick(&mut core);
        let w = written(&mut core);
        assert_eq!(interrupts(&w), [pair("kid", "kid-turn"), pair("th1", "t1")]);
        assert!(!w.iter().any(|v| v.get("result").is_some()));
        // Once each.
        c.tick(&mut core);
        assert!(interrupts(&written(&mut core)).is_empty());
        feed(&mut c, &mut core, &[json!({"method": "turn/completed", "params": {"threadId": "th1", "turn": {"id": "t1", "status": "interrupted"}}})]);
        let w = written(&mut core);
        let next = w.iter().find(|v| v.get("method") == Some(&json!("turn/start"))).ok_or_else(|| anyhow::anyhow!("no continuation"))?;
        assert_eq!(next.pointer("/params/approvalPolicy"), Some(&json!("never")));
        assert!(next.pointer("/params/input/0/text").and_then(Value::as_str).is_some_and(|s| s.contains("Continue the work") && s.contains("(kid)")));
        assert_eq!(core.state.approvals.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["kid-ask"]);
        Ok(())
    }

    #[test]
    fn a_subagent_kiln_could_not_stop_leaves_its_request_to_the_person() {
        let (mut c, mut core, _) = opened();
        c.command(&mut core, Command::Send { id: "m1".into(), text: "work".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        let start = written(&mut core);
        respond(&mut c, &mut core, &start, "turn/start", json!({"turn": {"id": "t1", "status": "inProgress"}}));
        spawn(&mut c, &mut core, "th1", "t1", "kid");
        c.command(&mut core, Command::SetMode { mode: "full-access".into() });
        written(&mut core);
        feed(&mut c, &mut core, &[json!({"id": "kid-ask", "method": "item/commandExecution/requestApproval",
            "params": {"threadId": "kid", "turnId": "kid-turn", "itemId": "x1", "command": "gh api", "cwd": "/work"}})]);
        assert!(core.state.approvals.is_empty());
        let w = written(&mut core);
        let id = w.iter().find(|v| v.get("method") == Some(&json!("turn/interrupt")) && v.pointer("/params/threadId") == Some(&json!("kid"))).and_then(|v| v.get("id")).cloned().unwrap_or_default();
        feed(&mut c, &mut core, &[json!({"id": id, "error": {"code": -32600, "message": "no active turn"}})]);
        assert!(core.state.approvals.iter().any(|a| a.id == "kid-ask") && c.withheld.is_empty() && c.stopped.is_empty());
        // Not tried again for that turn, and never answered: what it asks next is shown at once.
        c.tick(&mut core);
        feed(&mut c, &mut core, &[json!({"id": "kid-ask-2", "method": "item/commandExecution/requestApproval",
            "params": {"threadId": "kid", "turnId": "kid-turn", "itemId": "x2", "command": "gh run list", "cwd": "/work"}})]);
        assert!(core.state.approvals.iter().any(|a| a.id == "kid-ask-2"));
        assert!(!written(&mut core).iter().any(|v| v.get("method") == Some(&json!("turn/interrupt")) || v.get("result").is_some()));
    }

    #[test]
    fn a_request_held_back_is_shown_if_its_work_never_stops_and_a_refused_turn_asks_the_person() {
        let (mut c, mut core, _) = opened();
        c.command(&mut core, Command::Send { id: "m1".into(), text: "work".into(), mode: SendMode::Queue, attachments: vec![], overrides: None });
        let start = written(&mut core);
        respond(&mut c, &mut core, &start, "turn/start", json!({"turn": {"id": "t1", "status": "inProgress"}}));
        spawn(&mut c, &mut core, "th1", "t1", "kid");
        c.command(&mut core, Command::SetMode { mode: "full-access".into() });
        let interrupt = written(&mut core);
        feed(&mut c, &mut core, &[json!({"id": "kid-ask", "method": "item/commandExecution/requestApproval",
            "params": {"threadId": "kid", "turnId": "kid-turn", "itemId": "x1", "command": "gh api", "cwd": "/work"}})]);
        assert!(core.state.approvals.is_empty());
        // The stop was acknowledged, yet the turn goes on asking: after half a minute, the person's.
        if let Some(a) = c.withheld.get_mut("kid-ask") {
            a.at -= WITHHELD_MS + 1;
        }
        c.tick(&mut core);
        assert!(core.state.approvals.iter().any(|a| a.id == "kid-ask") && c.withheld.is_empty());
        // The session's own turn: Codex refuses the interrupt the change sent. What the turn asks
        // then is held back while kiln tries once more for it; refused again, it's the person's,
        // and so is what it asks after.
        let id = interrupt.iter().find(|v| v.get("method") == Some(&json!("turn/interrupt")) && v.pointer("/params/threadId") == Some(&json!("th1"))).and_then(|v| v.get("id")).cloned().unwrap_or_default();
        feed(&mut c, &mut core, &[json!({"id": id, "error": {"code": -32600, "message": "cannot interrupt"}})]);
        assert!(!c.permissions_changing);
        written(&mut core);
        feed(&mut c, &mut core, &[json!({"id": "own-ask", "method": "item/commandExecution/requestApproval",
            "params": {"threadId": "th1", "turnId": "t1", "itemId": "x3", "command": "gh api", "cwd": "/work"}})]);
        assert!(!core.state.approvals.iter().any(|a| a.id == "own-ask"));
        let again = written(&mut core);
        assert_eq!(interrupts(&again), [pair("th1", "t1")]);
        let id = again.iter().find(|v| v.get("method") == Some(&json!("turn/interrupt"))).and_then(|v| v.get("id")).cloned().unwrap_or_default();
        feed(&mut c, &mut core, &[json!({"id": id, "error": {"code": -32600, "message": "cannot interrupt"}})]);
        assert!(core.state.approvals.iter().any(|a| a.id == "own-ask"));
        feed(&mut c, &mut core, &[json!({"id": "own-ask-2", "method": "item/commandExecution/requestApproval",
            "params": {"threadId": "th1", "turnId": "t1", "itemId": "x4", "command": "gh api", "cwd": "/work"}})]);
        assert!(core.state.approvals.iter().any(|a| a.id == "own-ask-2"));
        assert!(!written(&mut core).iter().any(|v| v.get("method") == Some(&json!("turn/interrupt")) || v.get("result").is_some()));
    }

    #[test]
    fn a_cold_restart_keeps_a_legacy_single_control_override() {
        let mut core = Core::new();
        core.state.mode = "full-access".into();
        core.set_control("approval_policy", json!("on-request"));
        let mut c = Codex::new(None, None, Some("full-access".into()), true);
        c.start(&mut core);
        assert_eq!(core.state.mode, "custom");
        assert_eq!(c.policies(&core).0, "on-request");
        assert_eq!(c.policies(&core).1, "danger-full-access");
        let initialize = written(&mut core);
        respond(&mut c, &mut core, &initialize, "initialize", json!({}));
        assert!(written(&mut core).iter().any(|v| v.get("method") == Some(&json!("thread/start"))
            && v.pointer("/params/approvalPolicy") == Some(&json!("on-request"))
            && v.pointer("/params/sandbox") == Some(&json!("danger-full-access"))));
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

    /// A session on a model other than Codex's default, no effort chosen, shows its own model's
    /// default effort: only the default model's was taken, so its page showed none at all.
    #[test]
    fn an_unchosen_effort_is_its_own_model_s_default() {
        let mut core = Core::new();
        core.state.cwd = "/work".into();
        let mut c = Codex::new(Some("th1".into()), Some("o-mini".into()), None, false);
        c.start(&mut core);
        core.state.model = "o-mini".into();
        let w = written(&mut core);
        respond(&mut c, &mut core, &w, "initialize", json!({"userAgent": "kiln/0.162.1"}));
        let w = written(&mut core);
        respond(
            &mut c,
            &mut core,
            &w,
            "model/list",
            json!({"data": [
                {"id": "gpt-5", "model": "gpt-5", "displayName": "GPT-5", "description": "", "isDefault": true, "defaultReasoningEffort": "medium", "supportedReasoningEfforts": [{"reasoningEffort": "low", "description": ""}, {"reasoningEffort": "medium", "description": ""}]},
                {"id": "o-mini", "model": "o-mini", "displayName": "o mini", "description": "", "defaultReasoningEffort": "high", "supportedReasoningEfforts": [{"reasoningEffort": "low", "description": ""}, {"reasoningEffort": "high", "description": ""}]}
            ]}),
        );
        assert_eq!(core.state.effort, "high");
    }

    /// With no effort chosen here, the page shows the effort Codex says the thread runs at (the
    /// person's own `model_reasoning_effort`), not its model's default: fedora's Codex agents ran
    /// at "ultra" while their page said "low", and a resumed session went on sending "low".
    #[test]
    fn the_effort_codex_reports_is_shown_over_a_model_s_default() {
        let models = json!({"data": [
            {"id": "gpt-5", "model": "gpt-5", "displayName": "GPT-5", "description": "", "isDefault": true, "defaultReasoningEffort": "low", "supportedReasoningEfforts": [{"reasoningEffort": "low", "description": ""}, {"reasoningEffort": "high", "description": ""}, {"reasoningEffort": "ultra", "description": ""}]}
        ]});
        let thread = json!({"thread": {"id": "th1", "turns": []}, "model": "gpt-5", "cwd": "/work", "reasoningEffort": "ultra"});
        let settings = |effort: &str| json!({"method": "thread/settings/updated", "params": {"threadId": "th1", "threadSettings": {"model": "gpt-5", "effort": effort}}});
        let opening = |chosen: Option<&str>| {
            let mut core = Core::new();
            core.state.cwd = "/work".into();
            let mut c = Codex::new(None, None, None, false);
            c.start(&mut core);
            if let Some(e) = chosen {
                c.command(&mut core, Command::SetEffort { effort: e.into() });
            }
            let w = written(&mut core);
            respond(&mut c, &mut core, &w, "initialize", json!({"userAgent": "kiln/0.162.1"}));
            let w = written(&mut core);
            (c, core, w)
        };
        // Whichever answers first.
        for model_list_first in [true, false] {
            let (mut c, mut core, w) = opening(None);
            for method in if model_list_first { ["model/list", "thread/start"] } else { ["thread/start", "model/list"] } {
                respond(&mut c, &mut core, &w, method, if method == "model/list" { models.clone() } else { thread.clone() });
            }
            assert_eq!(core.state.effort, "ultra", "model/list answered first: {model_list_first}");
            // Its settings change: what it runs at from then on.
            feed(&mut c, &mut core, &[settings("high")]);
            assert_eq!(core.state.effort, "high");
        }
        // One chosen here goes with every turn: what Codex says of its own doesn't replace it.
        let (mut c, mut core, w) = opening(Some("low"));
        respond(&mut c, &mut core, &w, "thread/start", thread.clone());
        respond(&mut c, &mut core, &w, "model/list", models.clone());
        feed(&mut c, &mut core, &[settings("high")]);
        assert_eq!(core.state.effort, "low");
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
        // Activity items have no status field. Completing a "started" marker does not end its agent,
        // and an interaction leaves it as it was (Codex's TUI: it "does not change liveness").
        let activity = |id: &str, kind: &str| json!({"type": "subAgentActivity", "id": id, "agentThreadId": "child", "agentPath": "/root/audit", "kind": kind});
        for (kind, expected) in [("started", "running"), ("completed", "completed"), ("interacted", "completed"), ("interrupted", "stopped")] {
            c.item(&mut core, &activity(kind, kind), true);
            let child = core.state.tasks.first().ok_or_else(|| anyhow::anyhow!("no child task"))?;
            assert_eq!((core.state.tasks.len(), child.id.as_str(), child.tool_id.as_deref(), child.status.as_str()), (1, "child", Some("started"), expected));
            assert_eq!(child.ended.is_none(), child.live());
        }
        Ok(())
    }

    #[test]
    fn native_child_history_is_read_without_resuming_or_changing_the_parent() -> anyhow::Result<()> {
        let (mut c, mut core, _) = opened();
        core.state.summary = "Parent answer".into();
        core.state.goal = "Parent assignment".into();
        c.item(&mut core, &json!({"type": "subAgentActivity", "id": "spawn", "kind": "started", "agentThreadId": "child", "agentPath": "/root/model_dropdown", "model": "gpt-6-astra"}), true);
        let requests = written(&mut core);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests.first().and_then(|v| v.get("method")).and_then(Value::as_str), Some("thread/read"));
        assert_eq!(requests.first().and_then(|v| v.pointer("/params/includeTurns")).and_then(Value::as_bool), Some(false));
        // As reported by a real native child: unloaded metadata, persisted completed turn,
        // no plaintext assignment, and messages back to the parent in its own history.
        respond(&mut c, &mut core, &requests, "thread/read", json!({"thread": {
            "id": "child", "parentThreadId": "th1", "historyMode": "paginated", "status": {"type": "notLoaded"},
            "createdAt": 100, "cwd": "/child", "preview": "", "agentNickname": "James", "turns": []
        }}));
        let requests = written(&mut core);
        assert_eq!(requests.first().and_then(|v| v.get("method")).and_then(Value::as_str), Some("thread/turns/list"));
        let history = json!({"data": [{"id": "child-turn", "status": "completed", "startedAt": 100, "completedAt": 102, "durationMs": 2000, "items": [
            {"type": "commandExecution", "id": "command", "command": "cargo check", "cwd": "/child", "status": "completed", "durationMs": 250, "exitCode": 0, "aggregatedOutput": "passed"},
            {"type": "subAgentActivity", "id": "message-parent", "kind": "interacted", "agentThreadId": "th1", "agentPath": "/root"},
            {"type": "fileChange", "id": "edit", "status": "completed", "changes": [{"path": "/child/a.rs", "kind": {"type": "add"}, "diff": "+new"}]},
            {"type": "agentMessage", "id": "answer", "text": "The dropdown now lists the available models.", "phase": "final_answer"}
        ]}], "nextCursor": null});
        respond(&mut c, &mut core, &requests, "thread/turns/list", history);
        let task = core.state.tasks.first().ok_or_else(|| anyhow::anyhow!("missing child"))?;
        assert_eq!((core.state.tasks.len(), task.status.as_str(), task.tool_id.as_deref(), task.started, task.ended, task.duration_ms), (1, "completed", Some("spawn"), 100_000, Some(102_000), 2000));
        assert_eq!(task.summary.as_deref(), Some("The dropdown now lists the available models."));
        assert!(task.prompt.is_none(), "encrypted assignments must not be invented");
        assert_eq!((core.state.summary.as_str(), core.state.goal.as_str(), core.state.status), ("Parent answer", "Parent assignment", Status::Idle));
        assert!(core.change_jobs.is_empty());
        assert!(core.get("child:child:answer").is_some_and(|e| e.parent.as_deref() == Some("spawn")));
        assert!(core.get("child:child:command").is_some_and(|e| matches!(&e.body, Body::Tool(t) if t.output.as_deref() == Some("passed") && t.ended == Some(t.started + 250))));
        assert!(written(&mut core).is_empty(), "a child's messages to its parent must not read the parent as another child");
        Ok(())
    }

    #[test]
    fn native_subagent_output_reads_completed_history_and_streaming_messages() -> anyhow::Result<()> {
        let (mut c, mut core, _) = opened();
        c.item(&mut core, &json!({"type": "collabAgentToolCall", "id": "spawn", "tool": "spawnAgent", "status": "completed",
            "receiverThreadIds": ["child", "sibling"], "agentsStates": {"child": {"status": "running"}, "sibling": {"status": "running"}}}), true);
        let read = |core: &Core, id: &str| -> anyhow::Result<Value> {
            let task = core.state.tasks.iter().find(|t| t.id == id).ok_or_else(|| anyhow::anyhow!("missing child"))?;
            Ok(super::super::session::codex_task_output(core, task))
        };
        let pending = read(&core, "child")?;
        assert_eq!((pending.get("data").and_then(serde_json::Value::as_str), pending.get("pending").and_then(serde_json::Value::as_bool), pending.get("live").and_then(serde_json::Value::as_bool)), (Some(""), Some(true), Some(true)));
        c.child_history(&mut core, "child", &[json!({"id": "child-turn", "status": "completed", "items": [
            {"type": "userMessage", "id": "assignment", "content": [{"type": "text", "text": "Private assignment"}]},
            {"type": "reasoning", "id": "thought", "summary": [{"text": "Private reasoning"}]},
            {"type": "commandExecution", "id": "command", "command": "cargo check", "status": "completed", "aggregatedOutput": "passed", "exitCode": 0},
            {"type": "agentMessage", "id": "result", "text": "Mobile and desktop checks passed.", "phase": "final_answer"}
        ]})]);
        c.child_history(&mut core, "sibling", &[json!({"id": "sibling-turn", "status": "completed", "items": [
            {"type": "agentMessage", "id": "result", "text": "Sibling result", "phase": "final_answer"}
        ]})]);
        core.upsert("parent-answer", None, Body::Assistant { text: "Parent result".into(), streaming: false });
        core.upsert("child:grandchild:answer", Some("grandchild-spawn".into()), Body::Assistant { text: "Grandchild result".into(), streaming: false });
        let completed = read(&core, "child")?;
        assert_eq!(completed.get("data").and_then(serde_json::Value::as_str), Some("cargo check\npassed\n\nMobile and desktop checks passed."));
        assert_eq!((completed.get("live").and_then(serde_json::Value::as_bool), completed.get("reset").and_then(serde_json::Value::as_bool), completed.get("truncated").and_then(serde_json::Value::as_bool)), (Some(false), Some(true), Some(false)));
        assert_eq!(read(&core, "sibling")?.get("data").and_then(serde_json::Value::as_str), Some("Sibling result"));
        // Starting more work and streamed text updates are visible without reopening an
        // executor, waiting for a final answer, or appending an earlier snapshot twice.
        feed(&mut c, &mut core, &[
            json!({"method": "turn/started", "params": {"threadId": "child", "turn": {"id": "follow-up"}}}),
            json!({"method": "item/agentMessage/delta", "params": {"threadId": "child", "itemId": "progress", "delta": "Checking"}})
        ]);
        assert_eq!(read(&core, "child")?.get("data").and_then(serde_json::Value::as_str), Some("cargo check\npassed\n\nMobile and desktop checks passed.\n\nChecking"));
        feed(&mut c, &mut core, &[json!({"method": "item/agentMessage/delta", "params": {"threadId": "child", "itemId": "progress", "delta": " again"}})]);
        let running = read(&core, "child")?;
        assert_eq!(running.get("data").and_then(serde_json::Value::as_str), Some("cargo check\npassed\n\nMobile and desktop checks passed.\n\nChecking again"));
        assert_eq!(running.get("live").and_then(serde_json::Value::as_bool), Some(true));
        // Repeated history import updates the same events, never duplicating their output.
        c.child_history(&mut core, "child", &[json!({"id": "follow-up", "status": "completed", "items": [
            {"type": "agentMessage", "id": "progress", "text": "Rechecked successfully.", "phase": "final_answer"}
        ]})]);
        assert_eq!(read(&core, "child")?.get("data").and_then(serde_json::Value::as_str), Some("cargo check\npassed\n\nMobile and desktop checks passed.\n\nRechecked successfully."));
        Ok(())
    }

    #[test]
    fn native_child_notifications_are_scoped_and_outlive_the_parent_turn() {
        let (mut c, mut core, _) = opened();
        c.item(&mut core, &json!({"type": "subAgentActivity", "id": "spawn", "kind": "started", "agentThreadId": "child", "agentPath": "/root/audit"}), true);
        feed(&mut c, &mut core, &[
            json!({"method": "turn/started", "params": {"threadId": "th1", "turn": {"id": "parent-turn"}}}),
            json!({"method": "turn/started", "params": {"threadId": "child", "turn": {"id": "child-turn"}}}),
            json!({"method": "item/started", "params": {"threadId": "child", "item": {"id": "command", "type": "commandExecution", "command": "cargo test", "status": "inProgress", "processId": "42"}}}),
            json!({"method": "item/agentMessage/delta", "params": {"threadId": "child", "itemId": "reply", "delta": "Checking"}}),
            json!({"method": "thread/tokenUsage/updated", "params": {"threadId": "child", "tokenUsage": {"total": {"inputTokens": 100, "outputTokens": 25}}}}),
            json!({"method": "turn/completed", "params": {"threadId": "th1", "turn": {"id": "parent-turn", "status": "completed", "items": []}}}),
            json!({"method": "turn/completed", "params": {"threadId": "child", "turn": {"id": "older-child-turn", "status": "completed", "items": []}}}),
        ]);
        assert_eq!(core.state.status, Status::Idle);
        assert_eq!(core.state.usage.input_tokens, 0);
        assert_eq!(core.state.tasks.len(), 1, "child terminals are not parent-owned terminals");
        assert_eq!(core.state.tasks.first().map(|t| (t.status.as_str(), t.tokens)), Some(("running", 125)));
        assert!(core.get("child:child:command").is_some_and(|e| e.parent.as_deref() == Some("spawn") && matches!(&e.body, Body::Tool(t) if t.status == ToolStatus::Running)));
        assert!(core.get("child:child:reply").is_some_and(|e| matches!(&e.body, Body::Assistant { streaming: true, .. })));
        feed(&mut c, &mut core, &[json!({"method": "turn/completed", "params": {"threadId": "child", "turn": {"id": "child-turn", "status": "completed", "items": []}}})]);
        assert_eq!(core.state.tasks.first().map(|t| t.status.as_str()), Some("completed"));
        assert!(core.get("child:child:reply").is_some_and(|e| matches!(&e.body, Body::Assistant { streaming: false, .. })));
        assert_eq!(core.state.usage.turns, 1);
    }

    #[test]
    fn child_snapshots_preserve_waiting_status_and_ignore_stale_reads() {
        let (mut c, mut core, _) = opened();
        c.item(&mut core, &json!({"type": "subAgentActivity", "id": "spawn", "kind": "started", "agentThreadId": "child", "agentPath": "/root/audit"}), true);
        let requests = written(&mut core);
        respond(&mut c, &mut core, &requests, "thread/read", json!({"thread": {"id": "child", "status": {"type": "active", "activeFlags": ["waitingOnApproval"]}}}));
        let requests = written(&mut core);
        respond(&mut c, &mut core, &requests, "thread/turns/list", json!({"data": [{"id": "child-turn", "status": "inProgress", "items": []}]}));
        assert_eq!(core.state.tasks.first().map(|t| t.status.as_str()), Some("paused"));
        assert_eq!(core.state.status, Status::Idle);
        c.read_child(&mut core, "child");
        let stale = written(&mut core);
        c.item(&mut core, &json!({"type": "subAgentActivity", "id": "finished", "kind": "completed", "agentThreadId": "child", "agentPath": "/root/audit"}), true);
        respond(&mut c, &mut core, &stale, "thread/read", json!({"thread": {"id": "child", "status": {"type": "active", "activeFlags": []}}}));
        assert_eq!(core.state.tasks.first().map(|t| t.status.as_str()), Some("completed"));
        assert!(written(&mut core).is_empty(), "the stale response must not start another stale history read");
        if let Some(child) = c.children.get_mut("child") {
            child.read_at = 0;
        }
        c.tick(&mut core);
        assert!(written(&mut core).iter().any(|v| v.get("method").and_then(Value::as_str) == Some("thread/read")), "a final snapshot is retried even after the child ends");
    }

    #[test]
    fn repeated_child_history_does_not_restart_descendants_or_follow_peer_messages() {
        let (mut c, mut core, _) = opened();
        c.item(&mut core, &json!({"type": "subAgentActivity", "id": "spawn", "kind": "started", "agentThreadId": "child", "agentPath": "/root/audit"}), true);
        let turn = json!({"id": "child-turn", "status": "completed", "items": [
            {"type": "subAgentActivity", "id": "spawn-grandchild", "kind": "started", "agentThreadId": "grandchild", "agentPath": "/root/audit/check"},
            {"type": "commandExecution", "id": "command", "command": "check", "status": "completed", "exitCode": 0, "durationMs": 10}
        ]});
        c.child_history(&mut core, "child", std::slice::from_ref(&turn));
        child_status(&mut core, "grandchild", "completed");
        let command_end = core.get("child:child:command").and_then(|e| match &e.body { Body::Tool(t) => t.ended, _ => None });
        let _ = written(&mut core);
        c.child_history(&mut core, "child", std::slice::from_ref(&turn));
        c.child_history(&mut core, "grandchild", &[json!({"id": "grandchild-turn", "status": "completed", "items": [
            {"type": "subAgentActivity", "id": "message-parent", "kind": "interacted", "agentThreadId": "child", "agentPath": "/root/audit"},
            {"type": "subAgentActivity", "id": "message-peer", "kind": "interacted", "agentThreadId": "sibling", "agentPath": "/root/sibling"}
        ]})]);
        assert_eq!(core.state.tasks.len(), 2);
        assert!(core.state.tasks.iter().all(|t| t.status == "completed"));
        assert_eq!(core.state.tasks.iter().find(|t| t.id == "grandchild").map(|t| (t.parent_task.as_deref(), t.depth)), Some((Some("child"), 2)));
        assert!(written(&mut core).is_empty());
        assert_eq!(core.get("child:child:command").and_then(|e| match &e.body { Body::Tool(t) => t.ended, _ => None }), command_end);
    }

    #[test]
    fn child_polling_does_not_reinsert_evicted_history_or_end_parallel_running_tools() {
        let (mut c, mut core, _) = opened();
        c.item(&mut core, &json!({"type": "subAgentActivity", "id": "spawn", "kind": "started", "agentThreadId": "child", "agentPath": "/root/audit"}), true);
        let history = [json!({"id": "child-turn", "status": "inProgress", "items": [
            {"type": "commandExecution", "id": "long-running", "command": "watch tests", "status": "inProgress"},
            {"type": "commandExecution", "id": "finished", "command": "check", "status": "completed", "durationMs": 20}
        ]})];
        c.child_history(&mut core, "child", &history);
        assert!(core.get("child:child:long-running").is_some_and(|e| matches!(&e.body, Body::Tool(t) if t.status == ToolStatus::Running && t.ended.is_none())));
        for n in 0..super::super::core::MAX_EVENTS {
            core.upsert(&format!("root-{n}"), None, Body::Assistant { text: "Parent work".into(), streaming: false });
        }
        assert!(core.get("child:child:finished").is_none());
        let before = (core.events().count(), core.state.dropped);
        c.child_history(&mut core, "child", &history);
        assert_eq!((core.events().count(), core.state.dropped), before);
        assert!(core.get("child:child:finished").is_none(), "unchanged polled history must not evict fresh parent work");
    }

    #[test]
    fn restored_subagents_rebuild_mappings_and_unknown_children_have_separate_threads() -> anyhow::Result<()> {
        let (mut c, mut core, _) = opened();
        let spawn = json!({"type": "subAgentActivity", "id": "spawn", "kind": "started", "agentThreadId": "child", "agentPath": "/root/audit"});
        c.item(&mut core, &spawn, true);
        let _ = written(&mut core);
        // Core::adopt retains the timeline but clears task state; the new adapter rebuilds it.
        core.state.tasks.clear();
        c.children.clear();
        c.pending.retain(|_, p| !matches!(p, Pending::Child { .. } | Pending::ChildHistory { .. }));
        c.replay(&mut core, &[json!({"id": "parent-turn", "items": [spawn]})]);
        assert_eq!(core.state.tasks.len(), 1);
        assert!(written(&mut core).iter().any(|v| v.get("method").and_then(Value::as_str) == Some("thread/read")));
        c.item(&mut core, &json!({"type": "collabAgentToolCall", "id": "wait", "tool": "wait", "status": "completed",
            "receiverThreadIds": ["unknown-a", "unknown-b"], "agentsStates": {"unknown-a": {"status": "running"}, "unknown-b": {"status": "running"}}}), true);
        let first = core.state.tasks.iter().find(|t| t.id == "unknown-a").and_then(|t| t.tool_id.clone()).ok_or_else(|| anyhow::anyhow!("missing first link"))?;
        let second = core.state.tasks.iter().find(|t| t.id == "unknown-b").and_then(|t| t.tool_id.clone()).ok_or_else(|| anyhow::anyhow!("missing second link"))?;
        assert_ne!(first, second);
        for thread in ["unknown-a", "unknown-b"] {
            c.child_history(&mut core, thread, &[json!({"id": "turn", "status": "completed", "items": [{"type": "agentMessage", "id": "result", "text": thread, "phase": "final_answer"}]})]);
        }
        assert_eq!(core.get("child:unknown-a:result").and_then(|e| e.parent.as_deref()), Some(first.as_str()));
        assert_eq!(core.get("child:unknown-b:result").and_then(|e| e.parent.as_deref()), Some(second.as_str()));
        c.item(&mut core, &json!({"type": "collabAgentToolCall", "id": "late-spawn", "tool": "spawnAgent", "status": "completed",
            "prompt": "Audit the models", "receiverThreadIds": ["unknown-a"], "agentsStates": {"unknown-a": {"status": "completed"}}}), true);
        assert_eq!(core.state.tasks.iter().find(|t| t.id == "unknown-a").and_then(|t| t.tool_id.as_deref()), Some(first.as_str()), "later spawn metadata must not strand imported work");
        c.item(&mut core, &json!({"type": "collabAgentToolCall", "id": "follow-up", "tool": "followupTask", "status": "completed",
            "prompt": "Also check mobile", "receiverThreadIds": ["unknown-a"], "agentsStates": {"unknown-a": {"status": "running"}}}), true);
        assert_eq!(core.state.tasks.iter().find(|t| t.id == "unknown-a").map(|t| (t.status.as_str(), t.prompt.as_deref(), t.ended)), Some(("running", Some("Also check mobile"), None)));
        Ok(())
    }

    #[test]
    fn hot_adopted_legacy_subagents_reconcile_completion_without_replaying_the_parent() -> anyhow::Result<()> {
        let (c, mut core, _) = opened();
        // A hot upgrade of a pre-reader adapter carries its task rows, but no Child map.
        // Both reported real-world cases had already completed in Codex's durable history.
        let mut frozen = serde_json::to_value(c)?;
        frozen.as_object_mut().ok_or_else(|| anyhow::anyhow!("missing adapter object"))?.remove("children");
        let mut c: Codex = serde_json::from_value(frozen)?;
        core.state.summary = "Parent answer".into();
        core.state.goal = "Parent assignment".into();
        core.state.tasks = vec![
            Task { id: "stale".into(), tool_id: Some("spawn-stale".into()), class: Some("agent".into()), status: "running".into(), ..Task::default() },
            Task { id: "ended".into(), tool_id: Some("spawn-ended".into()), class: Some("agent".into()), status: "completed".into(), ended: Some(100_000), ..Task::default() },
            Task { id: "shell".into(), class: Some("shell".into()), status: "running".into(), ..Task::default() },
        ];
        written(&mut core);
        c.tick(&mut core);
        let reads = written(&mut core);
        let requests: Vec<&Value> = reads.iter().filter(|v| v.get("method").and_then(Value::as_str) == Some("thread/read")).collect();
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|v| v.pointer("/params/includeTurns").and_then(Value::as_bool) == Some(false)));
        assert!(!requests.iter().any(|v| v.pointer("/params/threadId").and_then(Value::as_str) == Some("shell")));
        assert!(core.state.tasks.iter().find(|t| t.id == "stale").is_some_and(Task::live), "the read request alone cannot claim completion");
        for request in requests {
            let thread = request.pointer("/params/threadId").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("missing thread"))?;
            respond(&mut c, &mut core, std::slice::from_ref(request), "thread/read", json!({"thread": {
                "id": thread, "status": {"type": "notLoaded"}, "createdAt": 100
            }}));
        }
        let history = written(&mut core);
        assert_eq!(history.len(), 2);
        for request in &history {
            let thread = request.pointer("/params/threadId").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("missing thread"))?;
            respond(&mut c, &mut core, std::slice::from_ref(request), "thread/turns/list", json!({"data": [{
                "id": format!("{thread}-turn"), "status": "completed", "startedAt": 100, "completedAt": 102, "durationMs": 2000,
                "items": [{"type": "agentMessage", "id": "answer", "text": format!("{thread} finished"), "phase": "final_answer"}]
            }], "nextCursor": null}));
        }
        for id in ["stale", "ended"] {
            let task = core.state.tasks.iter().find(|t| t.id == id).ok_or_else(|| anyhow::anyhow!("missing child"))?;
            assert_eq!((task.status.as_str(), task.ended, task.duration_ms), ("completed", Some(102_000), 2000));
            assert!(!task.live());
            assert_eq!(task.summary.as_deref(), Some(format!("{id} finished").as_str()));
            assert!(core.get(&child_event_id(id, "answer")).is_some_and(|e| e.parent.as_deref() == task.tool_id.as_deref()));
        }
        assert_eq!((core.state.summary.as_str(), core.state.goal.as_str(), core.state.status), ("Parent answer", "Parent assignment", Status::Idle));
        // Successful completed snapshots are not polled indefinitely; a new turn's
        // notification (or interaction) can activate the same child again as usual.
        for child in c.children.values_mut() { child.read_at = 0; }
        c.tick(&mut core);
        assert!(written(&mut core).is_empty());
        feed(&mut c, &mut core, &[json!({"method": "turn/started", "params": {"threadId": "stale", "turn": {"id": "followup"}}})]);
        assert!(core.state.tasks.iter().find(|t| t.id == "stale").is_some_and(|t| t.live() && t.ended.is_none()));
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

    #[test]
    fn a_message_sent_after_a_subagent_completed_does_not_start_it_again() -> anyhow::Result<()> {
        // As codex-cli 0.162.1 recorded /root/tracking_review and /root/column_fix_audit: the agent
        // completes, then its parent sends it one more `send_message`. That queues a message without
        // a turn (`trigger_turn: false`), so no turn starts and no "completed" ever follows it.
        let (mut c, mut core, _) = opened();
        let activity = |id: &str, kind: &str| json!({"type": "subAgentActivity", "id": id, "agentThreadId": "child", "agentPath": "/root/tracking_review", "kind": kind});
        c.item(&mut core, &activity("call_spawn", "started"), true);
        c.item(&mut core, &activity("call_message", "interacted"), true);
        assert_eq!(core.state.tasks.first().map(|t| t.status.as_str()), Some("running"), "a message to a working agent leaves it working");
        c.item(&mut core, &activity("subagent-completed-turn", "completed"), true);
        let ended = core.state.tasks.first().and_then(|t| t.ended);
        assert!(ended.is_some());
        c.item(&mut core, &activity("call_late_message", "interacted"), true);
        let task = core.state.tasks.first().ok_or_else(|| anyhow::anyhow!("no child"))?;
        assert_eq!((core.state.tasks.len(), task.status.as_str(), task.ended), (1, "completed", ended), "an interaction is no turn");
        assert!(core.get("call_late_message").is_some_and(|e| matches!(&e.body, Body::Tool(t) if t.status == ToolStatus::Done && t.task.as_deref() == Some("child"))));
        // Its thread is read (now, or once the read under way answers) and once more later, in case
        // a follow-up's turn starts just after.
        assert!(c.pending.values().any(|p| matches!(p, Pending::Child { thread, .. } if thread == "child")));
        assert!(c.children.get("child").is_some_and(|ch| ch.refresh));
        // A follow-up's turn is heard from the agent itself.
        feed(&mut c, &mut core, &[json!({"method": "turn/started", "params": {"threadId": "child", "turn": {"id": "followup"}}})]);
        assert!(core.state.tasks.first().is_some_and(|t| t.live() && t.ended.is_none()));
        Ok(())
    }

    #[test]
    fn a_subagent_first_heard_of_by_an_interaction_is_read_not_assumed_running() -> anyhow::Result<()> {
        // A replayed history whose spawn is older than the turns listed.
        let (mut c, mut core, _) = opened();
        c.replay(&mut core, &[json!({"id": "turn", "items": [{"type": "subAgentActivity", "id": "call_message", "kind": "interacted", "agentThreadId": "old", "agentPath": "/root/old"}]})]);
        let task = core.state.tasks.first().cloned().ok_or_else(|| anyhow::anyhow!("no child"))?;
        assert_eq!((task.id.as_str(), task.status.as_str(), task.live()), ("old", "completed", false));
        let reads = written(&mut core);
        assert!(reads.iter().any(|v| v.get("method").and_then(Value::as_str) == Some("thread/read") && v.pointer("/params/threadId").and_then(Value::as_str) == Some("old")));
        // Its thread says it is at work: then it runs.
        respond(&mut c, &mut core, &reads, "thread/read", json!({"thread": {"id": "old", "status": {"type": "active", "activeFlags": []}}}));
        assert!(core.state.tasks.first().is_some_and(|t| t.status == "running" && t.ended.is_none()));
        Ok(())
    }

    #[test]
    fn terminals_codex_no_longer_runs_end_and_its_own_word_replaces_kilns() -> anyhow::Result<()> {
        let (mut c, mut core, _) = opened();
        let item = |id: &str, pid: &str, status: &str, exit: Value| {
            json!({"type": "commandExecution", "id": id, "command": format!("run {id}"), "cwd": "/work", "processId": pid, "status": status, "exitCode": exit, "commandActions": []})
        };
        // Started inside code mode (`tools.exec_command`), they are items of the parent's turn.
        feed(&mut c, &mut core, &[
            json!({"method": "turn/started", "params": {"threadId": "th1", "turn": {"id": "t1"}}}),
            json!({"method": "item/started", "params": {"threadId": "th1", "turnId": "t1", "item": item("exec-server", "41", "inProgress", Value::Null)}}),
            json!({"method": "item/started", "params": {"threadId": "th1", "turnId": "t1", "item": item("exec-missed", "42", "inProgress", Value::Null)}}),
        ]);
        for t in &mut core.state.tasks {
            t.started -= 10_000;
        }
        let _ = written(&mut core);
        // The turn ends (or is interrupted): Codex keeps its terminals running, and is asked which.
        feed(&mut c, &mut core, &[json!({"method": "turn/completed", "params": {"threadId": "th1", "turn": {"id": "t1", "status": "interrupted", "items": []}}})]);
        assert!(core.state.tasks.iter().all(|t| t.live() && t.background), "a turn's end doesn't end its terminals");
        let asked = written(&mut core);
        let list = asked.iter().find(|v| v.get("method").and_then(Value::as_str) == Some("thread/backgroundTerminals/list")).ok_or_else(|| anyhow::anyhow!("not asked"))?;
        assert_eq!(list.pointer("/params/threadId").and_then(Value::as_str), Some("th1"));
        // Codex runs only the server: the other one's end never reached kiln.
        let running = json!({"data": [{"itemId": "exec-server", "processId": "41", "command": "run exec-server", "cwd": "/work"}], "nextCursor": null});
        respond(&mut c, &mut core, &asked, "thread/backgroundTerminals/list", running);
        let missed = core.state.tasks.iter().find(|t| t.id == "terminal-exec-missed").cloned().ok_or_else(|| anyhow::anyhow!("no terminal"))?;
        assert_eq!((missed.status.as_str(), missed.exit_code, missed.ended.is_some()), ("completed", None, true));
        assert!(core.get("exec-missed").is_some_and(|e| matches!(&e.body, Body::Tool(t) if t.status == ToolStatus::Done && t.ended.is_some())));
        assert!(core.state.tasks.iter().any(|t| t.id == "terminal-exec-server" && t.live()));
        // Codex's own word, when it comes, says how it ended.
        feed(&mut c, &mut core, &[json!({"method": "item/completed", "params": {"threadId": "th1", "turnId": "t1", "item": item("exec-missed", "42", "failed", json!(1))}})]);
        let missed = core.state.tasks.iter().find(|t| t.id == "terminal-exec-missed").cloned().unwrap_or_default();
        assert_eq!((missed.status.as_str(), missed.exit_code), ("failed", Some(1)));
        assert!(c.unheard.is_empty());
        // Only a whole list says what's gone, and one started just before the question waits.
        feed(&mut c, &mut core, &[json!({"method": "item/started", "params": {"threadId": "th1", "turnId": "t2", "item": item("exec-new", "43", "inProgress", Value::Null)}})]);
        c.terminals_at = 0;
        c.tick(&mut core);
        let asked = written(&mut core);
        respond(&mut c, &mut core, &asked, "thread/backgroundTerminals/list", json!({"data": [], "nextCursor": "page-2"}));
        assert!(core.state.tasks.iter().any(|t| t.id == "terminal-exec-server" && t.live()), "a partial list ended a terminal");
        c.terminals_at = 0;
        c.tick(&mut core);
        let asked = written(&mut core);
        respond(&mut c, &mut core, &asked, "thread/backgroundTerminals/list", json!({"data": [], "nextCursor": null}));
        assert!(core.state.tasks.iter().any(|t| t.id == "terminal-exec-server" && !t.live()));
        assert!(core.state.tasks.iter().any(|t| t.id == "terminal-exec-new" && t.live()), "one started as kiln asked may not be listed yet");
        Ok(())
    }

    /// A child's rollout as codex-cli 0.162.1 writes it: its own meta, its parent's (a fork copies
    /// the parent's history first, its turns without an initiating agent), then its own turns.
    fn child_rollout(thread: &str) -> Vec<Value> {
        let ev = |payload: Value| json!({"timestamp": "2026-10-20T23:20:01.000Z", "type": "event_msg", "payload": payload});
        let done = |turn: &str, item: Value| ev(json!({"type": "item_completed", "thread_id": thread, "turn_id": turn, "item": item}));
        vec![
            json!({"type": "session_meta", "ordinal": 0, "payload": {"id": thread, "forked_from_id": "parent"}}),
            json!({"type": "session_meta", "ordinal": 1, "payload": {"id": "parent"}}),
            ev(json!({"type": "task_started", "turn_id": "inherited", "turn_attribution": null, "started_at": 90})),
            done("inherited", json!({"type": "AgentMessage", "id": "msg_parent", "content": [{"type": "Text", "text": "The parent's words"}], "phase": "final_answer"})),
            ev(json!({"type": "task_complete", "turn_id": "inherited", "completed_at": 91, "duration_ms": 1000})),
            ev(json!({"type": "task_started", "turn_id": "own", "turn_attribution": {"initiating_agent_path": "/root"}, "started_at": 100})),
            done("own", json!({"type": "Reasoning", "id": "rs_1", "summary_text": ["Checking the columns"], "raw_content": []})),
            done("own", json!({"type": "CommandExecution", "id": "exec-1", "process_id": "44714", "command": ["/usr/bin/bash", "-lc", "git show --stat HEAD"],
                "cwd": "file:///work", "parsed_cmd": [{"type": "unknown", "cmd": "git show --stat HEAD"}], "source": "unified_exec_startup",
                "status": "completed", "aggregated_output": "1 file changed", "exit_code": 0, "duration": {"secs": 1, "nanos": 500_000_000}})),
            done("own", json!({"type": "FileChange", "id": "call_patch", "status": "completed", "changes": {"/work/a.css": {"type": "update", "unified_diff": "@@ -1 +1 @@\n-a\n+b"}}})),
            done("own", json!({"type": "SubAgentActivity", "id": "call_grandchild", "kind": "started", "agent_thread_id": "grandchild", "agent_path": "/root/audit/check"})),
            done("own", json!({"type": "AgentMessage", "id": "msg_answer", "content": [{"type": "Text", "text": "The column fix is intact."}], "phase": "final_answer"})),
            ev(json!({"type": "task_complete", "turn_id": "own", "started_at": 100, "completed_at": 103, "duration_ms": 3000})),
            ev(json!({"type": "task_started", "turn_id": "cut", "turn_attribution": {"initiating_agent_path": "/root"}, "started_at": 200})),
            done("cut", json!({"type": "AgentMessage", "id": "msg_more", "content": [{"type": "Text", "text": "Looking again"}], "phase": "commentary"})),
        ]
    }

    #[test]
    fn a_child_never_read_shows_its_work_from_its_rollout_once_codex_is_gone() -> anyhow::Result<()> {
        // /root/hyperlight_design_audit: a row an older image kept without reading it, in a session
        // that went on in Claude Code. Its thread showed "No work has been reported in this thread yet."
        let thread = "01a15b9d-5652-7722-8f9b-c750ebfe6940";
        let mut core = Core::new();
        core.harness = Some("Claude Code".into());
        core.state.tasks.push(Task { id: thread.into(), tool_id: Some("call_spawn".into()), class: Some("agent".into()), status: "completed".into(), ended: Some(5), ..Task::default() });
        core.state.tasks.push(Task { id: "a575".into(), tool_id: Some("toolu_5".into()), class: Some("agent".into()), status: "running".into(), ..Task::default() });
        assert_eq!(unread_children(&core), [thread], "only native Codex children, only those never read");
        let turns = rollout_turns(&child_rollout(thread));
        assert_eq!(turns.iter().filter_map(|t| t.get("id").and_then(Value::as_str)).collect::<Vec<_>>(), ["own", "cut"], "the parent's copied history is not the child's");
        child_from_rollout(&mut core, thread, &turns);
        let ev = |id: &str| core.get(&child_event_id(thread, id));
        // Its messages, commands, reasoning summary and answer, under its spawning call: where its
        // thread view, and its output, look for them.
        assert!(ev("rs_1").is_some_and(|e| e.parent.as_deref() == Some("call_spawn") && matches!(&e.body, Body::Thinking { text, streaming: false, .. } if text == "Checking the columns")));
        let command = ev("exec-1").and_then(|e| match &e.body {
            Body::Tool(t) => Some((t.title.clone(), t.output.clone(), t.exit_code, t.status)),
            _ => None,
        });
        assert_eq!(command, Some(("git show --stat HEAD".to_owned(), Some("1 file changed".to_owned()), Some(0), ToolStatus::Done)));
        assert!(ev("call_patch").is_some_and(|e| matches!(&e.body, Body::Tool(t) if t.tool == ToolKind::Edit && t.diffs.len() == 1)));
        assert!(ev("msg_answer").is_some_and(|e| matches!(&e.body, Body::Assistant { text, streaming: false } if text == "The column fix is intact.")));
        assert!(ev("msg_more").is_some_and(|e| matches!(&e.body, Body::Assistant { streaming: false, .. })));
        assert!(ev("msg_parent").is_none() && core.get("msg_parent").is_none());
        // Said by Codex, in a session Claude Code runs now.
        assert!(ev("msg_answer").is_some_and(|e| e.by.as_deref() == Some("Codex")));
        assert_eq!(core.harness.as_deref(), Some("Claude Code"));
        // As its latest turn ended (cut off, as no Codex runs it), with what it said last.
        let task = core.state.tasks.iter().find(|t| t.id == thread).cloned().unwrap_or_default();
        assert_eq!((task.status.as_str(), task.ended, task.duration_ms, task.summary.as_deref()), ("stopped", Some(5), 3000, Some("Looking again")));
        assert!(core.events().all(|e| !matches!(&e.body, Body::Tool(t) if matches!(t.status, ToolStatus::Running | ToolStatus::Pending))));
        // Nothing is asked of the harness running now, and nothing read runs.
        assert!(core.writes.is_empty());
        assert!(core.state.tasks.iter().filter(|t| t.id != "a575").all(|t| !t.live()));
        assert!(core.state.tasks.iter().any(|t| t.id == "a575" && t.live()), "the running harness's own work is left alone");
        assert!(unread_children(&core).is_empty());
        // A turn that failed says why (/root/codex_completed_cleanup's last one did).
        let failed = rollout_turns(&[
            json!({"type": "event_msg", "payload": {"type": "task_started", "turn_id": "t", "turn_attribution": {"initiating_agent_path": "/root"}}}),
            json!({"type": "event_msg", "payload": {"type": "task_complete", "turn_id": "t", "error": {"message": "Selected model is at capacity."}, "completed_at": 9, "duration_ms": 10}}),
        ]);
        let first = failed.first().cloned().unwrap_or_default();
        assert_eq!((first.get("status"), first.pointer("/error/message")), (Some(&json!("failed")), Some(&json!("Selected model is at capacity."))));
        // A child whose row went while its rollout was read: nothing lands in the main thread.
        let gone = "01a15afd-679c-7743-b019-b2450a7fa13b";
        let before = core.events().count();
        child_from_rollout(&mut core, gone, &rollout_turns(&child_rollout(gone)));
        assert_eq!(core.events().count(), before);
        Ok(())
    }

    #[test]
    fn a_childs_rollout_is_read_from_codexs_home() -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        let thread = "01a15afd-679c-7743-b019-b2450a7fa13b";
        let day = home.path().join("sessions/2026/10/20");
        std::fs::create_dir_all(&day)?;
        let lines: Vec<String> = child_rollout(thread).iter().map(Value::to_string).collect();
        std::fs::write(day.join(format!("rollout-2026-10-20T13-25-19-{thread}.jsonl")), lines.join("\n"))?;
        let read = read_rollouts(home.path(), vec![thread.to_owned(), "01a15a2f-a370-7901-8100-f622fd7d31d3".to_owned()]);
        assert_eq!(read.len(), 1, "a child without a rollout is left out");
        let (got, records) = read.first().cloned().ok_or_else(|| anyhow::anyhow!("nothing read"))?;
        assert_eq!((got.as_str(), records.len()), (thread, lines.len()));
        Ok(())
    }

    #[test]
    fn a_codex_that_cannot_list_its_terminals_is_not_asked_again() -> anyhow::Result<()> {
        let (mut c, mut core, _) = opened();
        feed(&mut c, &mut core, &[json!({"method": "item/started", "params": {"threadId": "th1", "turnId": "t1", "item": {
            "type": "commandExecution", "id": "exec-dev", "command": "npm run dev", "cwd": "/work", "processId": "7", "status": "inProgress", "commandActions": []
        }}})]);
        let _ = written(&mut core);
        c.tick(&mut core);
        let asked = written(&mut core);
        let list = asked.iter().find(|v| v.get("method").and_then(Value::as_str) == Some("thread/backgroundTerminals/list"));
        let id = list.and_then(|v| v.get("id")).cloned().ok_or_else(|| anyhow::anyhow!("not asked"))?;
        feed(&mut c, &mut core, &[json!({"id": id, "error": {"code": -32601, "message": "unknown variant `thread/backgroundTerminals/list`"}})]);
        assert!(core.state.tasks.iter().all(Task::live), "not knowing ends nothing");
        c.terminals_at = 0;
        c.tick(&mut core);
        assert!(!written(&mut core).iter().any(|v| v.get("method").and_then(Value::as_str) == Some("thread/backgroundTerminals/list")));
        Ok(())
    }
}
