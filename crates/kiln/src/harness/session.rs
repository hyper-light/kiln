//! One agent session on a machine: the harness process, its adapter and its state. The process
//! lives as long as the session (not as long as any browser); when it exits, the session stays and
//! can be continued where it left off (the harness's own resume), automatically when a person
//! sends it a message.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{FutureExt as _, StreamExt as _};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::Instrument as _;

use super::core::{Core, Update};
use super::kinds::{Kind, Protocol};
use super::model::{Command, ExitInfo, Level, State, Status, Task, ToolKind};

fn is_zero_epoch(n: &u64) -> bool {
    *n == 0
}

/// What to start.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Spec {
    /// Harness id ("claude", "codex", ...).
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Continue this harness session (its own id) instead of starting a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
    /// The first message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The terminal session (shell) this agent belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// Environment captured from that shell (`kiln spawn`), applied over the machine's own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<(String, String)>,
    /// Offer the harness's skip-all-permissions mode (it is never the default, only a choice).
    /// On unless a session says otherwise, so a person can always choose it.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub allow_bypass: bool,
    /// Run this executable instead of the harness's usual one (a custom install, tests).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    /// The effort / reasoning level to start with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The speed to start at (`State::speed`: "" standard); None: the harness's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<String>,
    /// The most subagents it runs at once (as `State::max_subagents`): Codex takes it as its own
    /// limit when it starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_subagents: Option<u32>,
    /// With `resume`: continue only up to this point of that session (a message or turn id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_at: Option<String>,
    /// With `resume`: a new session that branches off it, leaving it as it is.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fork: bool,
    /// With `resume`: it takes over that session, found running as this process (which is being
    /// ended): the harness starts only once the process is gone, never beside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takeover_pid: Option<u32>,
    /// With `takeover_pid`: that session runs outside kiln, so kiln ends its process itself (asks
    /// it to quit, then kills it) rather than only waiting for it to end.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub takeover_stop: bool,
    /// Reach (`crate::reach`): the agent's tools for running commands on other runners. None:
    /// it has them; `"off"`: it does not; anything else: where its commands go unless it says
    /// otherwise (a runner's name, or `linux/amd64`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reach: Option<String>,
    /// The agent that started it (`harness::lineage`): kiln sets it from who asked, never from
    /// what was asked (`POST /api/harness/sessions` drops it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// The plan run it does a step of (`plans::agents`): set by kiln, never from what was asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_run: Option<String>,
    /// What it helps with when it's a card's helper (`harness::about`: `about`, answering what a
    /// person tapped in its parent's conversation): no row of Mission Control, its parent not told
    /// of it, ended once no card reads it. Set by kiln, never from what was asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helper: Option<String>,
    /// It's its parent's subagent (`harness::lineage::start_subagent`): kiln started it for one of
    /// its parent's goals or plan steps, on its parent's machine with its parent's settings, and it
    /// shows in its parent's chat rather than as an agent of its own. Set by kiln, never from what
    /// was asked (`POST /api/harness/sessions` drops it). Its machine copies its parent's
    /// environment and program into it as it opens ([`super::host::Host::open`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub subagent: bool,
    /// Where it works when not in its folder itself: a Slates volume of its own laid over the
    /// folder (`crate::workspace`), made on its machine before it starts and kept with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<crate::workspace::Workspace>,
}

fn yes() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

impl Default for Spec {
    fn default() -> Self {
        Self {
            kind: String::new(),
            cwd: None,
            model: None,
            mode: None,
            resume: None,
            prompt: None,
            name: None,
            shell: None,
            env: Vec::new(),
            allow_bypass: true,
            program: None,
            effort: None,
            speed: None,
            max_subagents: None,
            resume_at: None,
            fork: false,
            takeover_pid: None,
            takeover_stop: false,
            reach: None,
            parent: None,
            plan_run: None,
            helper: None,
            subagent: false,
            workspace: None,
        }
    }
}

/// A pending approval, as lists show it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ApprovalBrief {
    pub id: String,
    pub tool: ToolKind,
    pub tool_name: String,
    pub title: String,
    pub at: i64,
}

/// A native subagent as lists show it. Its work still belongs to its harness's task, not a
/// separate kiln session, so this is tracking only and carries no execution settings or output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SubagentBrief {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub tool_id: Option<String>,
    pub description: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub parent_task: Option<String>,
    pub started: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub ended: Option<i64>,
}

impl SubagentBrief {
    pub(super) fn of(tasks: &[Task]) -> Vec<Self> {
        tasks
            .iter()
            .filter(|t| t.subagent())
            .map(|t| Self {
                id: t.id.clone(),
                tool_id: t.tool_id.clone(),
                description: super::model::clip_middle(&t.description, 200).0,
                status: t.status.clone(),
                parent_task: t.parent_task.clone(),
                started: t.started,
                ended: t.ended,
            })
            .collect()
    }
}

/// Goals an agent paused for a person, until someone resumes them (`Info::held`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "HeldGoals"))]
pub struct Held {
    /// Why, as the stack says it ("Paused by the agent: …").
    pub why: String,
    /// The plan it waits for you to look at first, when that's why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub plan: Option<String>,
    /// Unix ms it paused.
    pub since: i64,
}

/// A session as lists show it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentInfo"))]
pub struct Info {
    pub id: String,
    pub kind: String,
    pub kind_name: String,
    /// "local" or "agent:<machine>" (set by the server).
    #[serde(default)]
    pub target: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub shell: Option<String>,
    pub cwd: String,
    /// The working tree its work is in (`State::tree`): what's asked of git about its work is
    /// asked there ([`Info::work_dir`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub tree: Option<String>,
    pub created_at: i64,
    pub status: Status,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub harness_session: Option<String>,
    #[serde(default)]
    pub approvals: Vec<ApprovalBrief>,
    #[serde(default)]
    pub queued: usize,
    /// What waits in its queue, each message by its id and a digest of its words (as
    /// `State::queue`): the server's, for the messages kiln keeps there itself
    /// (`super::queued`). Lists don't show it.
    #[serde(skip)]
    pub queue: Vec<super::queued::Queued>,
    /// kiln's own messages its harness was handed lately (a goal stack's words, a parent's news),
    /// each by its id and a digest of the words it had then, as its timeline shows them: the
    /// server's (`super::queued::Kept::look`). Lists don't show it.
    #[serde(skip)]
    pub said: Vec<super::model::Given>,
    #[serde(default)]
    pub tasks_running: usize,
    /// Native harness subagents, including finished work, for the list's child disclosure.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagents: Vec<SubagentBrief>,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub activity: Option<String>,
    pub last_activity: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub exit: Option<ExitInfo>,
    /// What the person first asked for.
    #[serde(default)]
    pub goal: String,
    /// The goal the harness itself pursues, and how it stands (see `State::objective`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub objective: Option<super::model::Objective>,
    /// The agent's latest words, shortened.
    #[serde(default)]
    pub summary: String,
    /// What it is doing right now, in plain words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub doing: Option<String>,
    /// Its writer's epoch ([`State::epoch`]): a machine that announces a copy from before the
    /// session went on elsewhere lets go of it.
    #[serde(default, skip_serializing_if = "is_zero_epoch")]
    pub epoch: u64,
    #[serde(default)]
    pub todos_done: u32,
    #[serde(default)]
    pub todos_total: u32,
    #[serde(default)]
    pub files_changed: u32,
    #[serde(default)]
    pub lines_added: u32,
    #[serde(default)]
    pub lines_removed: u32,
    /// How full the context window is, 0–100, when the harness says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub context_pct: Option<u8>,
    /// Unix ms the current turn started (None when idle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub turn_started: Option<i64>,
    /// The plan limits of the account it runs on (as `State::limits`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limits: Vec<super::model::Limit>,
    /// That account, as `State::account` (never an address).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub plan: Option<String>,
    /// Tasks it left running with no turn under way (as `State::left_behind`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub left_behind: u32,
    /// Where its commands may run besides its machine (as `State::reach`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub reach: Option<String>,
    /// It waits for a usage limit to reset (as `State::pause`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub pause: Option<super::model::Pause>,
    /// How its last turn ended (as `State::last_turn`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub last_turn: Option<super::model::TurnEnd>,
    /// Kiln doesn't keep it going when it stops part-way (`goals::keep`; set on the server).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep_off: bool,
    /// Its goals wait on a person: it paused them for you, until you resume them (`goals::held_stacks`;
    /// set on the server).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub held: Option<Held>,
    /// What of its messages couldn't be read (`crate::wire::unread`; set on the server).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub refused: Option<crate::wire::unread::Kept>,
    /// At a usage limit it goes on on another machine with room (as `State::move_on_limit`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub move_on_limit: bool,
    /// Where it goes on then (as `State::move_plan`).
    #[serde(default, skip_serializing_if = "super::model::MovePlan::is_empty")]
    pub move_plan: super::model::MovePlan,
    /// Where it pauses as its limits fill (as `State::pause_at`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub pause_at: Option<u8>,
    /// The most subagents it runs at once (as `State::max_subagents`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub max_subagents: Option<u32>,
    /// The agent that started it (as `Spec::parent`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub parent: Option<String>,
    /// The plan run it does a step of (as `Spec::plan_run`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub plan_run: Option<String>,
    /// What it helps with when it's a card's helper (as `Spec::helper`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub helper: Option<String>,
    /// Its parent's subagent (as `Spec::subagent`): lists show it in its parent's chat and on its
    /// parent's row, not as an agent of its own.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub subagent: bool,
    /// It was started without its harness's skip-every-check mode (`Spec::allow_bypass` off), so
    /// an agent it starts can't have it either.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_bypass: bool,
    /// Its workspace of its own, when it has one (as `Spec::workspace`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub workspace: Option<crate::workspace::Workspace>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl Info {
    /// Where git is asked about its work: the working tree its latest change is in, else its
    /// folder (an agent in a worktree commits and pushes from there, not from the checkout it
    /// started in).
    pub fn work_dir(&self) -> &str {
        self.tree.as_deref().filter(|t| !t.is_empty()).unwrap_or(&self.cwd)
    }

    /// Refresh the live fields from the session's state.
    pub fn apply(&mut self, s: &State) {
        if !s.name.is_empty() {
            self.name.clone_from(&s.name);
        }
        self.status = s.status;
        self.title.clone_from(&s.title);
        self.model.clone_from(&s.model);
        self.mode.clone_from(&s.mode);
        if !s.cwd.is_empty() {
            self.cwd.clone_from(&s.cwd);
        }
        self.tree.clone_from(&s.tree);
        self.harness_session.clone_from(&s.harness_session);
        self.epoch = s.epoch;
        self.approvals = s
            .approvals
            .iter()
            .map(|a| ApprovalBrief { id: a.id.clone(), tool: a.tool, tool_name: a.tool_name.clone(), title: a.title.clone(), at: a.at })
            .collect();
        self.queued = s.queue.len();
        self.queue = s.queue.iter().map(|q| super::queued::Queued::of(&q.id, &q.text)).collect();
        self.tasks_running = s.tasks.iter().filter(|t| t.status == "running").count();
        self.subagents = SubagentBrief::of(&s.tasks);
        self.cost_usd = s.usage.cost_usd;
        self.activity.clone_from(&s.activity);
        self.exit.clone_from(&s.exit);
        self.goal = super::model::clip_middle(&s.goal, 200).0;
        self.objective.clone_from(&s.objective);
        self.summary = super::model::clip_middle(&s.summary, 300).0;
        self.doing.clone_from(&s.doing);
        let total = u32::try_from(s.todos.len()).unwrap_or(u32::MAX);
        self.todos_total = total;
        self.todos_done = u32::try_from(s.todos.iter().filter(|t| t.status == "completed").count()).unwrap_or(total);
        self.files_changed = s.files_changed;
        self.lines_added = s.lines_added;
        self.lines_removed = s.lines_removed;
        self.context_pct = match (s.usage.context_tokens, s.usage.context_window) {
            (Some(t), Some(w)) if w > 0 => u8::try_from((t.saturating_mul(100) / w).min(100)).ok(),
            _ => None,
        };
        self.turn_started = s.turn_started;
        // What its harness was handed, added to what was known (a runner started again knows less).
        for g in &s.given {
            super::queued::note_given(&mut self.said, &g.id, g.digest.clone());
        }
        self.limits.clone_from(&s.limits);
        self.account.clone_from(&s.account);
        self.plan.clone_from(&s.plan);
        self.left_behind = s.count_left_behind();
        self.reach.clone_from(&s.reach);
        self.pause.clone_from(&s.pause);
        self.last_turn.clone_from(&s.last_turn);
        self.move_on_limit = s.move_on_limit;
        self.move_plan.clone_from(&s.move_plan);
        self.pause_at = s.pause_at;
        self.max_subagents = s.max_subagents;
    }

    /// The parts lists care about changed (activity timestamps aside, the words of what's queued: a
    /// message edited while it waits changes nothing they show, and what kiln's own were handed).
    pub fn differs(&self, o: &Self) -> bool {
        let strip = |i: &Self| Self { last_activity: 0, queue: Vec::new(), said: Vec::new(), ..i.clone() };
        strip(self) != strip(o)
    }
}

/// Where a session's updates go (the server's registry, or the uplink to it).
pub type Sink = async_channel::Sender<(String, Update)>;

/// What a session needs from the machine that holds it: where it keeps the files attached to
/// its messages, how it collects ones that arrived separately (uploaded, or sent in pieces from
/// the server), and how it starts another session (a branch).
/// What gives the ids of the messages a session took, by its id, to go with it as it moves.
pub type CarriedIds = Arc<dyn Fn(String) -> futures::future::BoxFuture<'static, Result<Vec<(String, String)>, String>> + Send + Sync>;

/// Fences a session's inbox as it moves to another machine (`true`), or lifts the fence (`false`:
/// its move failed, and it goes on here) ([`super::host::Host::moving_fence`]).
pub type FenceFor = Arc<dyn Fn(String, bool) -> futures::future::BoxFuture<'static, Result<(), String>> + Send + Sync>;

/// The machine's decision on a try at handing a session to its newer runner process (the try, the
/// session), or (`true`) made now, not taken over, if there's none yet
/// ([`super::host::Host::settle`]).
pub type SettleFor = Arc<dyn Fn(String, String, bool) -> futures::future::BoxFuture<'static, Result<Option<super::host::Decided>, String>> + Send + Sync>;

#[derive(Clone)]
pub struct Files {
    pub dir: std::path::PathBuf,
    pub take: Arc<dyn Fn(&str) -> Option<std::path::PathBuf> + Send + Sync>,
    pub spawn: Arc<dyn Fn(Spec) + Send + Sync>,
    /// The session went on on another machine: this one lets go of it (it isn't closed).
    pub release: Arc<dyn Fn(&str) + Send + Sync>,
    /// The ids of the messages a session took that go with it when it moves ([`super::host::Host::carried`]).
    pub carried: CarriedIds,
    /// Its inbox fenced before its conversation goes to the machine it moves to, or lifted when
    /// the move fails (CP-T3.2).
    pub fence: FenceFor,
    /// How a try at handing it to this machine's newer runner process went, as the machine decided
    /// it (CP-T3.2).
    pub settle: SettleFor,
}

/// What a session that moved here starts from (`super::moving`): its timeline and state as they
/// were, and for a harness that takes one, the history of its new thread.
pub struct Seed {
    pub events: Vec<super::model::Event>,
    pub state: State,
    pub items: Option<Vec<serde_json::Value>>,
    /// Where it came from: the runner and its folder there.
    pub from: String,
    pub from_cwd: String,
    /// Its turn was interrupted to move: it picks the turn up here.
    pub picks_up: bool,
    /// Said with the move (its folder isn't here, so it works in home).
    pub note: Option<String>,
    /// Not a move: the session is started again here after a restart, its timeline and state
    /// taken from the conversation store (`crate::convo`).
    pub restored: bool,
    /// Where its folder stood in the sync log on the machine it left: it goes on once the folder
    /// here has taken the log that far.
    pub synced: Option<crate::sync::runner::Mark>,
    /// Not from another machine: an older runner process of this one handed it over, taking its
    /// leave (`super::handing`). Same folder, same record: only the process holding it changed.
    pub local: bool,
}

/// Put a message's attachments on disk, so the harness can read them: each gets a `path`;
/// images keep (or get) their bytes inline for harnesses that take them that way. Blocking.
fn store_attachments(files: &Files, msg: &str, list: Vec<super::model::Attachment>) -> Vec<super::model::Attachment> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut out = Vec::new();
    if std::fs::create_dir_all(&files.dir).is_err() {
        return out;
    }
    let tag: String = msg.chars().filter(char::is_ascii_alphanumeric).take(12).collect();
    for (i, mut a) in list.into_iter().enumerate() {
        let name: String = a.name.chars().map(|c| if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' }).take(80).collect();
        let name = if name.trim_matches('.').is_empty() { format!("file-{i}") } else { name };
        let path = files.dir.join(format!("{tag}-{i}-{name}"));
        let wrote = match a.file_id.as_deref().and_then(|f| (files.take)(f)) {
            Some(src) => std::fs::rename(&src, &path).or_else(|_| std::fs::copy(&src, &path).map(|_| ())).is_ok(),
            None => match b64.decode(a.data_base64.as_bytes()) {
                Ok(bytes) if !bytes.is_empty() && bytes.len() as u64 <= super::host::ATTACHMENT_MAX => std::fs::write(&path, &bytes).is_ok(),
                _ => false,
            },
        };
        if !wrote {
            tracing::warn!(attachment = %a.name, "could not store an attachment; it is left out");
            continue;
        }
        if a.is_image() {
            if a.data_base64.is_empty()
                && let Ok(bytes) = std::fs::read(&path)
            {
                a.data_base64 = b64.encode(bytes);
            }
        } else {
            a.data_base64.clear();
        }
        a.file_id = None;
        a.path = Some(path.to_string_lossy().into_owned());
        out.push(a);
    }
    out
}

/// The longest name a session takes.
pub(crate) const MAX_NAME: usize = 80;

/// Names a session (the "rename" control: the console, `/rename`, a broadcast): in kiln's lists,
/// and kept across restarts in its spec. Returns the control to pass on when the harness names
/// its conversations too (Claude Code, Codex). An empty name gives it back its harness's name.
fn name_session(s: &Session, value: Option<&serde_json::Value>) -> Option<Command> {
    let name: String = value.and_then(serde_json::Value::as_str).unwrap_or("").trim().chars().take(MAX_NAME).collect();
    let kind_name = s.info.lock().kind_name.clone();
    s.spec.lock().name = (!name.is_empty()).then(|| name.clone());
    let mut core = s.core.lock();
    core.state.name = if name.is_empty() { kind_name } else { name.clone() };
    core.touch_state();
    let harness_names = core.meta.controls.iter().any(|c| c.id == "rename");
    (harness_names && !name.is_empty()).then(|| Command::Control { id: "rename".into(), value: Some(serde_json::Value::String(name)) })
}

/// Where its commands may run besides its machine (the "reach" control, as `Spec::reach`): ""
/// anywhere it needs, "off" only its own machine, else a runner or platform it prefers. Kept in
/// its spec (its harness takes it when it next starts) and shown in its state. Whether it changed.
fn set_reach(s: &Session, value: Option<&serde_json::Value>) -> bool {
    let v = value.and_then(serde_json::Value::as_str).map(str::trim).unwrap_or("");
    let reach = (!v.is_empty()).then(|| v.chars().take(MAX_NAME).collect::<String>());
    {
        let mut spec = s.spec.lock();
        if spec.reach == reach {
            return false;
        }
        spec.reach.clone_from(&reach);
    }
    let mut core = s.core.lock();
    core.state.reach = reach;
    core.touch_state();
    true
}

/// The "reach" setting in words, for the timeline.
fn reach_words(reach: Option<&str>) -> String {
    match reach {
        None => "Its commands can run on any runner it needs".to_owned(),
        Some("off") => "Its commands run only on its own machine".to_owned(),
        Some(r) => format!("Its commands can run on other runners, {r} first"),
    }
}

/// A command as the adapter should see it (attachments stored first).
async fn prepare(files: &Files, c: Command) -> Command {
    match c {
        Command::Send { id, text, mode, attachments, overrides, reply_to, task } if !attachments.is_empty() => {
            let files = files.clone();
            let msg = id.clone();
            let stored = crate::rt::blocking(move || store_attachments(&files, &msg, attachments)).await.unwrap_or_default();
            Command::Send { id, text, mode, attachments: stored, overrides, reply_to, task }
        }
        other => other,
    }
}

enum Ctl {
    Cmd(Box<Command>),
    Close,
    /// This image of the process ends next (an in-place upgrade): the harness process, still
    /// running, goes to the next one.
    HandOver(futures::channel::oneshot::Sender<Option<Box<Handed>>>),
    /// It went on on another machine: it stops here, and isn't closed.
    LetGo,
}

/// A session handed from this image of the process to the next (an in-place upgrade) with its
/// harness process still running: the next image goes on with that very process, where this one
/// stopped, so nothing the agent was doing is cut short.
#[derive(Serialize, Deserialize)]
pub struct Handed {
    /// The harness it runs in now (a switch may have changed it from its spec's).
    pub kind: String,
    pub process: crate::proc::Detached,
    pub core: super::core::Frozen,
    pub adapter: super::kinds::Frozen,
    /// What the harness wrote after its last whole line (base64): the rest of that line follows.
    pub partial: String,
}

/// A harness process taken over from this process's previous image, still running.
struct Adopt {
    process: crate::proc::Detached,
    adapter: super::kinds::Adapter,
    partial: Vec<u8>,
}

/// A running session, as its machine holds it.
pub struct Session {
    pub id: String,
    pub spec: Mutex<Spec>,
    pub created_at: i64,
    pub core: Mutex<Core>,
    info: Mutex<Info>,
    ctl: async_channel::Sender<Ctl>,
    /// The process's pid while it runs.
    pub pid: Mutex<Option<u32>>,
    /// A turn of its is under way: set as one starts, cleared only as one ends (its harness ending
    /// leaves it set), so the record a restart reads says which turns it cut short.
    pub turn_open: std::sync::atomic::AtomicBool,
    /// What its harness runs, as last seen (`survivors`): the harness process, and what's under it
    /// with what it left running.
    pub ran: Mutex<(Option<super::survivors::Ran>, Vec<super::survivors::Ran>)>,
    /// What it started that still runs once what started it is gone, each shown as a task
    /// (`left-<pid>`) and stoppable from kiln.
    pub left: Mutex<Vec<super::survivors::Ran>>,
    /// It's on its way to another machine: messages sent to it from now on go where it goes, so
    /// it takes none here (CP-T3.2). A message is checked against it and handed over under this
    /// lock, and a move sets it and gathers what it was sent under it too: none slips in between.
    moving: Mutex<bool>,
    prompted: std::sync::atomic::AtomicBool,
}

impl Session {
    pub fn info(&self) -> Info {
        let mut i = self.info.lock().clone();
        i.apply(&self.core.lock().state);
        i
    }

    /// Its epoch ([`State::epoch`]): how many times it went on on another machine.
    pub fn epoch(&self) -> u64 {
        self.core.lock().state.epoch
    }

    /// It's on its way to another machine: it takes no messages here.
    pub fn is_moving(&self) -> bool {
        *self.moving.lock()
    }

    /// It's being handed to another runner process of this machine (blue/green): it takes no
    /// messages from now on (each refused `Moved`; sent again, it goes where the session went).
    pub fn stop_taking(&self) {
        *self.moving.lock() = true;
    }

    /// Hands `c` to the session. False when it can't take it: it has ended, or `c` is a message
    /// and the session is on its way to another machine.
    pub fn command(&self, c: Command) -> bool {
        let moving = self.moving.lock();
        if *moving && is_message(&c) {
            return false;
        }
        self.ctl.try_send(Ctl::Cmd(Box::new(c))).is_ok()
    }

    pub fn close(&self) {
        let _ = self.ctl.try_send(Ctl::Close);
    }

    /// Hand its harness process to this process's next image (an in-place upgrade, next):
    /// `None` when none runs, or it couldn't be handed over (it was stopped then, and starts again
    /// in the next image, continuing its conversation).
    pub async fn hand_over(&self) -> Option<Box<Handed>> {
        let (tx, rx) = futures::channel::oneshot::channel();
        self.ctl.send(Ctl::HandOver(tx)).await.ok()?;
        rx.await.ok().flatten()
    }

    /// It went on on another machine since: its harness here stops and this machine lets go of
    /// it, without closing it (its viewers follow it there).
    pub fn let_go(&self) {
        let _ = self.ctl.try_send(Ctl::LetGo);
    }

    /// Everything a new viewer (or a restarted server) needs, starting with the announcement.
    pub fn snapshot(&self) -> Vec<Update> {
        let mut out = vec![Update::Open { info: Box::new(self.info()), since: None }];
        out.extend(self.core.lock().snapshot(256 * 1024));
        out
    }

    /// A task's output after byte `since` (its last `TASK_TAIL` bytes without one), read where the
    /// harness keeps it: `{data, offset, live, truncated}` (ask from `offset` next), or `{error}`.
    /// Native Codex children return a replaceable transcript snapshot (`reset: true`).
    /// Blocking (reads a file).
    pub fn task_output(&self, task: &str, since: Option<u64>) -> serde_json::Value {
        use serde_json::json;
        let (t, kept, session, cwd, env, codex) = {
            let core = self.core.lock();
            let spec = self.spec.lock();
            let codex = core.harness.as_deref().map_or(spec.kind == "codex", |h| h == "Codex");
            let t = core.state.tasks.iter().find(|x| x.id == task).cloned();
            if codex && let Some(t) = t.as_ref().filter(|t| t.class.as_deref() == Some("agent")) {
                return codex_task_output(&core, t);
            }
            (t, core.task_output.get(task).cloned(), core.state.harness_session.clone(), core.state.cwd.clone(), spec.env.clone(), codex)
        };
        let Some(t) = t else { return json!({"error": "no such task in this session"}) };
        // Kept by kiln (a harness that streams it).
        if let Some(k) = kept {
            let (bytes, offset, truncated) = k.since(since.unwrap_or_else(|| k.total.saturating_sub(TASK_TAIL)));
            return json!({"data": String::from_utf8_lossy(&bytes), "offset": offset, "live": t.live(), "truncated": truncated});
        }
        // A file the harness writes (Claude Code: while the task lives).
        let Some(path) = t.output.clone().or_else(|| (!codex).then(|| claude_task_file(&env, &cwd, session.as_deref(), task)).flatten()) else {
            return json!({"error": "this task's output is not kept anywhere kiln can read", "live": t.live()});
        };
        let p = std::path::Path::new(&path);
        let named = p.extension().and_then(|e| e.to_str()) == Some("output") && p.parent().and_then(|d| d.file_name()).and_then(|n| n.to_str()) == Some("tasks");
        if !named || !p.is_absolute() {
            return json!({"error": "not a task output file", "live": t.live()});
        }
        read_tail(p, since).map_or_else(
            || json!({"data": "", "offset": since.unwrap_or(0), "live": t.live(), "gone": true}),
            |(data, offset, truncated)| json!({"data": data, "offset": offset, "live": t.live(), "truncated": truncated, "path": path}),
        )
    }

    /// What resuming this session takes: its spec with the harness's session id to continue.
    /// What its harness keeps as its own record of the conversation, for the conversation store
    /// to follow (`crate::convo::record`): none until the harness has a session of its own. Its
    /// locks are taken one at a time.
    pub fn record_source(&self) -> Option<crate::convo::record::Source> {
        let (env, first) = {
            let spec = self.spec.lock();
            (spec.env.clone(), spec.kind.clone())
        };
        let (harness, own) = {
            let core = self.core.lock();
            (core.harness.clone(), core.state.harness_session.clone())
        };
        // The harness it runs in now (a switch changes it), else the one it started in.
        let kind = harness.as_deref().and_then(|name| Kind::all().iter().find(|k| k.name == name)).or_else(|| Kind::get(&first))?;
        let harness_session = own.filter(|h| !h.is_empty())?;
        Some(crate::convo::record::Source { session: self.id.clone(), kind, env, harness_session })
    }

    pub fn resume_spec(&self) -> Spec {
        let mut s = self.spec.lock().clone();
        let core = self.core.lock();
        if let Some(h) = core.state.harness_session.clone() {
            s.resume = Some(h);
        }
        // Where it is now, not where it started: its own session, settings as last chosen.
        if self.prompted.load(std::sync::atomic::Ordering::Relaxed) {
            s.prompt = None;
        }
        s.resume_at = None;
        s.fork = false;
        if !core.state.effort.is_empty() {
            s.effort = Some(core.state.effort.clone());
        }
        // Standard too, once it has a faster speed to choose: its harness's own settings may name one.
        if !core.state.speed.is_empty() || !core.meta.speeds.is_empty() {
            s.speed = Some(core.state.speed.clone());
        }
        if !core.state.mode.is_empty() {
            s.mode = Some(core.state.mode.clone());
        }
        if !core.state.model.is_empty() {
            s.model = Some(core.state.model.clone());
        }
        s.max_subagents = core.state.max_subagents;
        s
    }
}

/// Stop what it left running that kiln took back over: one (its task id), or all of them.
async fn stop_left(s: &Arc<Session>, task: Option<&str>) {
    let pid: Option<u32> = task.and_then(|t| t.strip_prefix(super::survivors::LEFT)).and_then(|p| p.parse().ok());
    let roots: Vec<super::survivors::Ran> = s.left.lock().iter().filter(|r| task.is_none() || Some(r.pid) == pid).cloned().collect();
    if roots.is_empty() {
        return;
    }
    let stopping = roots.clone();
    let _ = crate::rt::blocking(move || stopping.iter().for_each(super::survivors::stop_tree)).await;
    let gone: std::collections::HashSet<u32> = roots.iter().map(|r| r.pid).collect();
    s.left.lock().retain(|r| !gone.contains(&r.pid));
    s.ran.lock().1.retain(|r| !gone.contains(&r.pid));
    let mut core = s.core.lock();
    for t in core.state.tasks.iter_mut().filter(|t| t.id.strip_prefix(super::survivors::LEFT).and_then(|p| p.parse::<u32>().ok()).is_some_and(|p| gone.contains(&p))) {
        t.status = "stopped".to_owned();
        t.ended = Some(Core::now());
    }
    core.touch_state();
}

/// Native Codex subagents whose work kiln never read, in a session that runs another harness now
/// (it switched before kiln read them): their work comes from Codex's own record of their threads,
/// read off the event loop; no Codex is asked. It shows at the next flush.
fn read_unread_children(s: &Arc<Session>) {
    let unread = super::codex::unread_children(&s.core.lock());
    if unread.is_empty() {
        return;
    }
    let home = super::handoff::codex_home(&s.spec.lock().env);
    let s = Arc::clone(s);
    crate::rt::spawn(async move {
        let Ok(read) = crate::rt::blocking(move || super::codex::read_rollouts(&home, unread)).await else { return };
        let mut core = s.core.lock();
        for (thread, records) in read {
            super::codex::child_from_rollout(&mut core, &thread, &super::codex::rollout_turns(&records));
        }
    });
}

/// The agent ended: what its harness ran ended with it, and so did what kiln took back over for it
/// (stopped with the rest). Its last state, what an ended agent shows, says so.
fn end_tasks(core: &mut Core) {
    core.end_harness_work();
    for t in core.state.tasks.iter_mut().filter(|t| t.live()) {
        t.status = "stopped".to_owned();
        t.ended.get_or_insert_with(Core::now);
    }
    core.touch_state();
}

/// Whether a turn is under way once a session's status is `status`: one starts it, its end ends it,
/// and the harness ending or starting again leaves it as it was (a turn cut short stays open).
pub fn turn_open(status: Status, was: bool) -> bool {
    match status {
        Status::Running | Status::Waiting => true,
        Status::Idle => false,
        Status::Starting | Status::Exited | Status::Disconnected => was,
    }
}

/// The folder an agent starts in, as this machine knows it: `~` and `~/…` are the home directory
/// of the user kiln runs as (people type them; a process can't `chdir("~")`), nothing means home.
pub fn resolve_cwd(cwd: Option<&str>) -> String {
    let home = crate::os::home_dir();
    match cwd.map(str::trim).filter(|c| !c.is_empty()) {
        None | Some("~") => home.to_string_lossy().into_owned(),
        Some(c) => match c.strip_prefix("~/").or_else(|| c.strip_prefix("~\\")) {
            Some(rest) => home.join(rest).to_string_lossy().into_owned(),
            None => c.to_owned(),
        },
    }
}

/// Start a session; its updates go to `sink`, tagged with its id.
pub fn start(id: String, spec: Spec, sink: Sink, on_change: Arc<dyn Fn() + Send + Sync>, files: Files) -> anyhow::Result<Arc<Session>> {
    start_seeded(id, spec, sink, on_change, files, None)
}

/// Go on with a session this process's previous image handed over ([`Session::hand_over`]): its
/// timeline and state as they were, and its harness process, still running.
pub fn start_handed(id: String, spec: Spec, sink: Sink, on_change: Arc<dyn Fn() + Send + Sync>, files: Files, h: Box<Handed>) -> anyhow::Result<Arc<Session>> {
    use base64::Engine as _;
    let kind = Kind::get(&h.kind).ok_or_else(|| anyhow::anyhow!("unknown harness {:?}", h.kind))?;
    let h = *h;
    let core = Core::thaw(h.core);
    let adopt = Adopt { process: h.process, adapter: super::kinds::Adapter::thaw(h.adapter), partial: base64::engine::general_purpose::STANDARD.decode(h.partial).unwrap_or_default() };
    begin(id, spec, kind, core, sink, on_change, files, None, Some(adopt))
}

/// Start a session, seeded with what it had on another machine when it moved here.
pub fn start_seeded(id: String, spec: Spec, sink: Sink, on_change: Arc<dyn Fn() + Send + Sync>, files: Files, seed: Option<Seed>) -> anyhow::Result<Arc<Session>> {
    let kind = Kind::get(&spec.kind).ok_or_else(|| anyhow::anyhow!("unknown harness {:?}", spec.kind))?;
    let cwd = resolve_cwd(spec.cwd.as_deref());
    let mut core = Core::new();
    core.state.cwd.clone_from(&cwd);
    core.state.model = spec.model.clone().unwrap_or_default();
    core.state.mode = spec.mode.clone().unwrap_or_default();
    core.state.harness_session.clone_from(&spec.resume);
    // Moved here: its timeline as it was, then the line saying so.
    let mut items = None;
    let restored = seed.as_ref().is_some_and(|sd| sd.restored);
    if let Some(sd) = seed.as_ref().filter(|_| restored) {
        // Restarted here: its timeline and state as the store kept them, and nothing said of it.
        core.restore(sd.events.clone(), sd.state.clone());
        core.state.model = spec.model.clone().unwrap_or_default();
        core.state.mode = spec.mode.clone().unwrap_or_default();
        core.harness = Some(kind.name.to_owned());
    } else if let Some(sd) = seed {
        core.adopt(sd.events, sd.state);
        // A new writer of its conversation: what the machine it left still says no longer counts.
        core.state.epoch += 1;
        core.state.model = spec.model.clone().unwrap_or_default();
        core.state.mode = spec.mode.clone().unwrap_or_default();
        core.harness = Some(kind.name.to_owned());
        let folder = if core.state.cwd == sd.from_cwd { String::new() } else { format!(" (it works in {} here; it was in {})", core.state.cwd, sd.from_cwd) };
        let mut text = if sd.local {
            "Taken over by this machine's newer kiln runner, which replaces the older one — the conversation, its timeline and queue came along".to_owned()
        } else {
            format!("Moved here from {}{folder} — the conversation, its timeline and queue came along", sd.from)
        };
        // Interrupted to move: the turn goes on here, first thing.
        if sd.picks_up {
            text.push_str(", and the turn it was in the middle of goes on here");
            let ask = format!("This conversation just moved here from {}, which interrupted your last turn. Pick up where you left off.", sd.from);
            core.state.queue.insert(0, super::model::QueueItem { id: crate::util::new_id(), text: ask, at: Core::now(), attachments: Vec::new(), overrides: None, reply_to: None });
        }
        if let Some(n) = &sd.note {
            text.push_str(". ");
            text.push_str(n);
        }
        core.upsert(&format!("harness-switch:moved-{}", Core::now()), None, super::model::Body::Notice { level: Level::Info, text });
        items = sd.items;
        core.state.arriving = sd.synced;
    }
    core.state.reach.clone_from(&spec.reach);
    begin(id, spec, kind, core, sink, on_change, files, items, None)
}

/// A session with this core, its task started (`adopt`: going on with a harness process the
/// previous image of this process handed over).
#[allow(clippy::too_many_arguments)]
fn begin(id: String, spec: Spec, kind: &'static Kind, mut core: Core, sink: Sink, on_change: Arc<dyn Fn() + Send + Sync>, files: Files, items: Option<Vec<serde_json::Value>>, adopt: Option<Adopt>) -> anyhow::Result<Arc<Session>> {
    let now = crate::util::now_ms();
    core.session_id.clone_from(&id);
    core.state.max_subagents = core.state.max_subagents.or(spec.max_subagents);
    let cwd = core.state.cwd.clone();
    let info = Info {
        id: id.clone(),
        kind: kind.id.to_owned(),
        kind_name: kind.name.to_owned(),
        target: String::new(),
        name: spec.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| kind.name.to_owned()),
        shell: spec.shell.clone(),
        cwd,
        tree: None,
        created_at: now,
        status: Status::Starting,
        title: String::new(),
        model: String::new(),
        mode: String::new(),
        harness_session: spec.resume.clone(),
        approvals: Vec::new(),
        queued: 0,
        queue: Vec::new(),
        said: Vec::new(),
        tasks_running: 0,
        subagents: Vec::new(),
        cost_usd: 0.0,
        activity: None,
        last_activity: now,
        exit: None,
        goal: String::new(),
        objective: None,
        summary: String::new(),
        doing: None,
        epoch: 0,
        todos_done: 0,
        todos_total: 0,
        files_changed: 0,
        lines_added: 0,
        lines_removed: 0,
        context_pct: None,
        turn_started: None,
        limits: Vec::new(),
        account: None,
        plan: None,
        left_behind: 0,
        reach: spec.reach.clone(),
        pause: None,
        last_turn: None,
        keep_off: false,
        held: None,
        refused: None,
        move_on_limit: false,
        move_plan: Default::default(),
        pause_at: None,
        max_subagents: core.state.max_subagents,
        parent: spec.parent.clone(),
        plan_run: spec.plan_run.clone(),
        helper: spec.helper.clone(),
        subagent: spec.subagent,
        no_bypass: !spec.allow_bypass,
        workspace: spec.workspace.clone(),
    };
    let (ctl, ctl_rx) = async_channel::unbounded();
    let s = Arc::new(Session {
        id,
        spec: Mutex::new(spec),
        created_at: now,
        core: Mutex::new(core),
        info: Mutex::new(info),
        ctl,
        pid: Mutex::new(None),
        turn_open: std::sync::atomic::AtomicBool::new(false),
        ran: Mutex::new((None, Vec::new())),
        left: Mutex::new(Vec::new()),
        moving: Mutex::new(false),
        prompted: std::sync::atomic::AtomicBool::new(false),
    });
    let run = Arc::clone(&s);
    crate::rt::spawn(async move {
        let id = run.id.clone();
        if let Err(p) = std::panic::AssertUnwindSafe(drive(Arc::clone(&run), kind, ctl_rx, sink.clone(), on_change, files, items, adopt)).catch_unwind().await {
            tracing::error!(session = %id, panic = %crate::sup::panic_message(p.as_ref()), "agent session task panicked");
        }
    });
    Ok(s)
}

/// A message for the agent (what a session's inbox takes once, CP-T3.2), not a command to kiln.
pub fn is_message(c: &Command) -> bool {
    matches!(c, Command::Send { .. } | Command::SkillRun { .. })
}

/// Most of what's waiting in a session's channel a move looks at.
const GATHER_MAX: usize = 100_000;

/// A move begins: the session takes no messages from now on, and those it was sent and hasn't taken
/// (still in its channel, or carried while its harness was down) join the end of its queue, in the
/// order they came, to go with it (one for a subagent, its subagent's). Everything else in its
/// channel is held back, for if it stays.
fn gather(s: &Session, ctl_rx: &async_channel::Receiver<Ctl>, carry: &mut Vec<Command>) -> Vec<Ctl> {
    let mut held = Vec::new();
    let mut waiting = Vec::new();
    {
        let mut moving = s.moving.lock();
        *moving = true;
        // Under the lock no message goes in, so this ends; bounded besides.
        for _ in 0..GATHER_MAX {
            match ctl_rx.try_recv() {
                Ok(Ctl::Cmd(c)) if is_message(&c) => waiting.push(*c),
                Ok(other) => held.push(other),
                Err(_) => break,
            }
        }
    }
    let (carried, rest): (Vec<Command>, Vec<Command>) = std::mem::take(carry).into_iter().partition(is_message);
    *carry = rest;
    let mut core = s.core.lock();
    let before = core.state.queue.len();
    for c in carried.into_iter().chain(waiting) {
        let task = match &c {
            Command::Send { task, .. } => task.clone(),
            _ => None,
        };
        let Some(m) = queued(c) else { continue };
        match task {
            // One for a subagent waits in its queue, which goes with the session, or (one that takes
            // none now) is shown not sent there: never in the agent's own (CP-T3.2, the nineteenth
            // adversarial check's).
            Some(task) if core.state.tasks.iter().any(|t| t.id == task && t.takes.contains(&super::model::SendMode::Queue)) => {
                super::subagent::hold(&mut core, &task, super::model::QueueItem { overrides: None, ..m });
            }
            Some(task) => super::subagent::refuse(&mut core, &task, &m, "it takes no message from kiln now."),
            None => core.state.queue.push(m),
        }
    }
    if core.state.queue.len() != before {
        core.touch_state();
    }
    held
}

/// A message as it waits in a session's queue.
fn queued(c: Command) -> Option<super::model::QueueItem> {
    let at = Core::now();
    match c {
        Command::Send { id, text, attachments, overrides, reply_to, .. } => Some(super::model::QueueItem { id, text, at, attachments, overrides, reply_to }),
        Command::SkillRun { name, args, id, .. } => {
            let text = match args.filter(|a| !a.trim().is_empty()) {
                Some(a) => format!("/{name} {a}"),
                None => format!("/{name}"),
            };
            Some(super::model::QueueItem { id: id.unwrap_or_else(crate::util::new_id), text, at, attachments: Vec::new(), overrides: None, reply_to: None })
        }
        _ => None,
    }
}

/// Background work a step asked for.
struct Jobs {
    previews: Vec<super::core::PreviewJob>,
    changes: Vec<super::core::ChangeJob>,
}

/// Hand out what the core produced: updates to the sink, lines to the process.
async fn flush(s: &Session, sink: &Sink, stdin: Option<&async_channel::Sender<Vec<u8>>>) -> Jobs {
    let (updates, writes, previews, changes) = {
        let mut core = s.core.lock();
        let u = core.take_out();
        (u, core.take_writes(), std::mem::take(&mut core.previews), core.take_change_jobs())
    };
    if let Some(stdin) = stdin {
        for w in writes {
            let mut line = w.into_bytes();
            line.push(b'\n');
            let _ = stdin.send(line).await;
        }
    }
    if !updates.is_empty() {
        s.info.lock().last_activity = crate::util::now_ms();
    }
    for u in updates {
        if sink.send((s.id.clone(), u)).await.is_err() {
            break;
        }
    }
    Jobs { previews, changes }
}

/// The next chunk from one of the harness's pipes. One already closed never answers again: a
/// closed channel is always ready, so it would be chosen first on every turn of the loop, which
/// would spin without ever yielding (starving every task on its thread, the one that reaps the
/// harness among them) and never see the other pipe close or the process exit.
async fn pipe(rx: &async_channel::Receiver<Vec<u8>>, open: bool) -> Option<Vec<u8>> {
    if open { rx.recv().await.ok() } else { std::future::pending().await }
}

enum Ev {
    Change((String, (Option<super::model::FileChange>, Option<super::core::Baseline>))),
    Out(Option<Vec<u8>>),
    Err(Option<Vec<u8>>),
    Ctl(Option<Ctl>),
    Preview(Option<(String, Vec<super::model::FileDiff>)>),
    Exit(Option<i32>),
    Fx(Option<Done>),
    Tick,
    /// Headroom has room for work it held (docs/headroom.md).
    Room,
}

/// What of a session's pause is kept on disk (`host::Saved`): a change is written there.
type PauseMarks = (Option<(i64, Option<i64>, bool, bool)>, Option<u8>, bool);

fn pause_marks(st: &State) -> PauseMarks {
    (st.pause.as_ref().map(|p| (p.since, p.until, p.auto, p.checkpoint.is_some())), st.pause_at, st.manual_resume)
}

/// What an effect carried out off the event loop came back with.
enum Done {
    /// A run (or a fetch) ended.
    Run { job: String, code: Option<i32>, output: String },
    /// The answer for one client.
    Reply { client: String, value: serde_json::Value },
}

/// Longest a run of the harness's CLI may take.
const RUN_TIMEOUT: Duration = Duration::from_secs(120);
/// Most of a run's output handed back.
const RUN_OUTPUT_MAX: usize = 32 * 1024;
/// Largest file a read hands back.
const READ_MAX: u64 = 512 * 1024;
/// How often the adapter gets a tick while the process runs.
const TICK: Duration = Duration::from_secs(5);

/// Completes when headroom has room for this session's held work: only while it holds some (so
/// sessions with nothing waiting don't wake for it).
async fn held_room(core: &Mutex<Core>) {
    let waiting = {
        let c = core.lock();
        !c.state.queue.is_empty() && crate::headroom::held(&c.session_id)
    };
    if waiting {
        crate::headroom::room().await;
    } else {
        futures::future::pending::<()>().await;
    }
}
/// How long a take-over waits for the session it takes over to end.
const TAKEOVER_WAIT: Duration = Duration::from_secs(15);
/// How long a session taken over from outside kiln has to quit before it is killed.
const TAKEOVER_QUIT: Duration = Duration::from_secs(10);

/// Most of a task's output returned at once, and the tail returned without an offset.
const TASK_READ_MAX: u64 = 256 * 1024;
const TASK_TAIL: u64 = 64 * 1024;

/// Codex's child messages and tool results are kept with the parent's events, not in Claude's
/// task files or the terminal byte buffer. Project only this child's output; neither its
/// assignment nor a parent's, sibling's or descendant's messages belong in this view.
/// History reads can replace earlier items, so byte offsets cannot safely append snapshots.
pub(super) fn codex_task_output(core: &Core, task: &Task) -> serde_json::Value {
    use super::core::TaskOutput;
    use super::model::{Body, OUTPUT_MAX, clip_middle};
    use serde_json::json;

    let mut output = TaskOutput::default();
    let mut summary_seen = false;
    let mut truncated = false;
    let mut add = |text: &str| {
        if !text.trim().is_empty() {
            if output.total > 0 {
                output.append(b"\n\n");
            }
            output.append(text.as_bytes());
        }
    };
    if let Some(parent) = task.tool_id.as_deref() {
        let prefix = format!("child:{}:", task.id);
        // A batch spawn may give siblings the same parent tool; the child thread is also
        // part of every Codex event id, so that cannot mix their output.
        for event in core.events().filter(|e| e.parent.as_deref() == Some(parent) && e.id.starts_with(&prefix)) {
            match &event.body {
                Body::Assistant { text, .. } => {
                    summary_seen |= task.summary.as_deref().is_some_and(|s| s == text || s == clip_middle(text, OUTPUT_MAX).0);
                    add(text);
                }
                Body::Tool(tool) => {
                    if let Some(text) = tool.output.as_deref().filter(|s| !s.trim().is_empty()) {
                        add(&format!("{}\n{text}", tool.title));
                        truncated |= tool.truncated;
                    }
                }
                Body::Notice { text, .. } => add(text),
                _ => {}
            }
        }
    }
    // Completed children can outlive the bounded timeline, or report a result before their
    // history is available. Keep that result readable without inventing a transcript.
    if !summary_seen && let Some(summary) = task.summary.as_deref() {
        add(summary);
    }
    if output.total == 0 && !task.live() {
        return json!({"error": "No readable output has been received from this Codex subagent.", "live": false});
    }
    let (bytes, offset, skipped) = output.since(output.total.saturating_sub(TASK_TAIL));
    json!({"data": String::from_utf8_lossy(&bytes), "offset": offset, "live": task.live(),
        "truncated": truncated || skipped || output.total > TASK_TAIL, "reset": true, "pending": output.total == 0})
}

/// Where Claude Code streams a task's output while it lives: `<tmp>/<cwd as a name>/<session>/
/// tasks/<task>.output`, `<tmp>` being `CLAUDE_CODE_TMPDIR` or the system's `claude-<uid>`.
fn claude_task_file(env: &[(String, String)], cwd: &str, session: Option<&str>, task: &str) -> Option<String> {
    let session = session.filter(|s| !s.is_empty() && !s.contains(['/', '\\']))?;
    if task.is_empty() || task.contains(['/', '\\', '.']) {
        return None;
    }
    let root = env
        .iter()
        .rev()
        .find(|(k, _)| k == "CLAUDE_CODE_TMPDIR")
        .map(|(_, v)| std::path::PathBuf::from(v))
        .or_else(|| std::env::var_os("CLAUDE_CODE_TMPDIR").map(std::path::PathBuf::from))
        .unwrap_or_else(|| std::env::temp_dir().join(format!("claude-{}", crate::os::uid())));
    let dir: String = cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    Some(root.join(dir).join(session).join("tasks").join(format!("{task}.output")).to_string_lossy().into_owned())
}

/// A file's bytes after `since` (its tail without one, the start again when it shrank below
/// `since`), at most `TASK_READ_MAX`: (text, the offset after them, whether older bytes were
/// skipped). None when it cannot be read. Blocking.
fn read_tail(p: &std::path::Path, since: Option<u64>) -> Option<(String, u64, bool)> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut f = std::fs::File::open(p).ok()?;
    let len = f.metadata().ok()?.len();
    let want = match since {
        Some(s) if s <= len => s,
        Some(_) => 0,
        None => len.saturating_sub(TASK_TAIL),
    };
    let start = want.max(len.saturating_sub(TASK_READ_MAX));
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.take(len - start).read_to_end(&mut buf).ok()?;
    Some((String::from_utf8_lossy(&buf).into_owned(), start + buf.len() as u64, start > want || (since.is_none() && start > 0)))
}

/// An ending agent's group of processes, after `after` (its harness's time to stop): whatever is
/// still in it ended at once, then the group removed.
fn end_group(g: crate::os::contain::Group, after: Duration) {
    crate::rt::spawn(async move {
        crate::rt::sleep(after).await;
        let _ = crate::rt::blocking(move || {
            g.kill();
            for _ in 0..20 {
                if g.pids().is_empty() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            g.remove();
        })
        .await;
    });
}

/// The processes under `root`, newest first: (pid, command line). Blocking.
fn descendants(root: u32) -> Vec<(u32, Vec<String>)> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always).without_tasks());
    let mut kids: std::collections::HashMap<Pid, Vec<Pid>> = std::collections::HashMap::new();
    for (pid, p) in sys.processes() {
        if let Some(parent) = p.parent() {
            kids.entry(parent).or_default().push(*pid);
        }
    }
    let mut out: Vec<(u64, u32, Vec<String>)> = Vec::new();
    let mut todo = vec![Pid::from_u32(root)];
    let mut seen = std::collections::HashSet::new();
    while let Some(p) = todo.pop() {
        for c in kids.get(&p).into_iter().flatten() {
            if seen.insert(*c) {
                todo.push(*c);
                if let Some(proc_) = sys.process(*c) {
                    out.push((proc_.start_time(), c.as_u32(), proc_.cmd().iter().map(|a| a.to_string_lossy().into_owned()).collect()));
                }
            }
        }
    }
    out.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    out.into_iter().map(|(_, pid, cmd)| (pid, cmd)).collect()
}

/// Stop the newest process under `root` that runs `command` (its command line, or the last
/// argument of a `shell -c` one), with what it started: whether one was found. Blocking.
fn kill_command(root: u32, command: &str) -> bool {
    let want = command.trim();
    if want.is_empty() {
        return false;
    }
    let all = descendants(root);
    let found = all.iter().find(|(_, cmd)| cmd.join(" ") == want || cmd.last().is_some_and(|a| a.trim() == want));
    let Some((pid, _)) = found else { return false };
    let mut pids: Vec<u32> = vec![*pid];
    pids.extend(descendants(*pid).into_iter().map(|(p, _)| p));
    crate::os::stop_processes(&pids, Some(root), Duration::from_secs(2));
    true
}

/// Wait (up to `wait`) for `pid` to be gone: whether it is.
async fn gone(pid: u32, wait: Duration) -> bool {
    let end = Instant::now() + wait;
    while crate::os::pid_alive(pid) {
        if Instant::now() >= end {
            return false;
        }
        crate::rt::sleep(Duration::from_millis(200)).await;
    }
    true
}

/// A take-over waits for the process it takes over to end (its terminal is being closed, or with
/// `takeover_stop` kiln ends it): an error to show when it has not, `None` once it is gone (and
/// the spec forgets it).
async fn takeover_ready(s: &Session, sink: &Sink, wait: Duration) -> Option<String> {
    let (pid, stop, kind) = {
        let sp = s.spec.lock();
        (sp.takeover_pid?, sp.takeover_stop, sp.kind.clone())
    };
    // Only the harness it was found as: never whatever has since been given its pid.
    let stop = stop && crate::aisessions::runs(pid, &kind);
    // Said in the session's state, not its timeline (which its history fills once it is free).
    let waiting = stop || crate::os::pid_alive(pid);
    if waiting {
        {
            let mut core = s.core.lock();
            core.state.doing = Some(if stop {
                format!("Ending the session it takes over (process {pid})")
            } else {
                format!("Waiting for the session it takes over (process {pid}) to end")
            });
            core.touch_state();
        }
        flush(s, sink, None).await;
    }
    // Asked to quit first (it keeps its conversation); killed, with what it started, if it won't.
    if stop && !(crate::aisessions::end(pid, false) && gone(pid, TAKEOVER_QUIT).await) {
        crate::aisessions::end(pid, true);
    }
    let free = gone(pid, wait).await;
    if waiting {
        let mut core = s.core.lock();
        core.state.doing = None;
        core.touch_state();
    }
    if free {
        let mut sp = s.spec.lock();
        sp.takeover_pid = None;
        sp.takeover_stop = false;
        return None;
    }
    Some(format!("The session found running as process {pid} has not ended, so kiln did not start a second one beside it. Quit it, then restart this agent."))
}

impl Done {
    fn apply(self, adapter: &mut super::kinds::Adapter, core: &mut Core) {
        match self {
            Self::Run { job, code, output } => adapter.run_done(core, &job, code, &output),
            Self::Reply { client, value } => core.reply(client, value),
        }
    }
}

/// Carry out what the adapter asked for. A branch starts another session; runs, reads and fetches
/// go off the event loop and come back on `done`. A restart is the caller's: its point (the
/// harness's own resume point, if any) is returned.
fn effects(s: &Arc<Session>, kind: &'static Kind, files: &Files, done: &async_channel::Sender<Done>) -> Option<Option<String>> {
    let effects = std::mem::take(&mut s.core.lock().effects);
    let mut restart = None;
    for e in effects {
        match e {
            super::core::Effect::Branch { resume, resume_at } => {
                let mut b = s.resume_spec();
                b.resume = Some(resume);
                b.resume_at = resume_at;
                b.fork = true;
                b.name = Some(format!("{} (branch)", s.info().name));
                (files.spawn)(b);
            }
            super::core::Effect::Restart { resume_at } => restart = Some(resume_at),
            // The go-back a harness without one gets from kiln (`harness::goback`): the store's
            // timeline truncates, its transcript seeds a branch, the workspace's checkpoint
            // (a hidden ref) restores its files.
            super::core::Effect::GoBack { op, event } => {
                match op {
                    super::core::GoBackOp::Fork | super::core::GoBackOp::Branch => {
                        let mut b = s.resume_spec();
                        // No native resume: the branch is seeded with the conversation in words.
                        b.resume = None;
                        b.resume_at = None;
                        b.fork = false;
                        b.name = Some(format!("{} (branch)", s.info().name));
                        b.prompt = Some(super::goback::branch_prompt(&s.core.lock(), if op == super::core::GoBackOp::Branch { Some(event.as_str()) } else { None }));
                        (files.spawn)(b);
                    }
                    _ => {
                        let mut core = s.core.lock();
                        let (at, words) = match core.get(&event) {
                            Some(e) => (e.at, match &e.body { super::model::Body::User { text, .. } => super::model::clip_middle(text.trim(), 80).0, _ => String::new() }),
                            None => continue,
                        };
                        let cwd = core.state.cwd.clone();
                        // The workspace goes back to the checkpoint the prompt's own moment made.
                        let did = if cwd.is_empty() { "the folder isn't known".to_owned() } else { super::goback::restore_files(&cwd, &s.id, at) };
                        if op == super::core::GoBackOp::RewindFiles {
                            super::goback::notice(&mut core, super::model::Level::Info, format!("Restored files to \"{words}\": {did}."));
                        } else {
                            // The process can't come back at the point natively: it starts over
                            // and the truncated conversation is said to it (the store keeps the
                            // record of what went).
                            core.truncate_from(&event);
                            let transcript = super::goback::seed_prompt(&core, None);
                            let keep: std::collections::HashSet<String> = core.events().map(|e| e.id.clone()).collect();
                            core.state.delivery.retain(|k, _| keep.contains(k));
                            core.state.harness_session = None;
                            s.spec.lock().resume = None;
                            core.state.queue.insert(0, super::model::QueueItem { id: crate::util::new_id(), text: format!("The conversation was rewound here. What it is now:\n\n{transcript}\n\nGo on from the last message."), at: Core::now(), attachments: Vec::new(), overrides: None, reply_to: None });
                            super::goback::notice(&mut core, super::model::Level::Info, format!("Rewound to \"{words}\": {did}. The agent starts again with the conversation up to here."));
                            restart = Some(None);
                        }
                        core.touch_state();
                    }
                }
            }
            super::core::Effect::AllowBypass => s.spec.lock().allow_bypass = true,
            super::core::Effect::Run { job, args } => {
                let mut spec = s.spec.lock().clone();
                let cwd = s.core.lock().state.cwd.clone();
                if !cwd.is_empty() {
                    spec.cwd = Some(cwd);
                }
                let tx = done.clone();
                crate::rt::spawn(async move {
                    let (code, output) = match kind.cli(&spec, &args) {
                        Err(e) => (None, format!("{e:#}")),
                        Ok(cmd) => match crate::proc::output(cmd, RUN_TIMEOUT).await {
                            Ok(o) => {
                                let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
                                let err = String::from_utf8_lossy(&o.stderr);
                                if !err.trim().is_empty() {
                                    if !text.is_empty() && !text.ends_with('\n') {
                                        text.push('\n');
                                    }
                                    text.push_str(&err);
                                }
                                (o.status.code(), super::model::clip_middle(text.trim(), RUN_OUTPUT_MAX).0)
                            }
                            Err(e) => (None, format!("{e:#}")),
                        },
                    };
                    let _ = tx.send(Done::Run { job, code, output }).await;
                });
            }
            super::core::Effect::TranscriptTail { job, config, session, bytes } => {
                let tx = done.clone();
                crate::rt::spawn(async move {
                    let lines = crate::rt::blocking(move || super::transcript::tail(std::path::Path::new(&config), &session, bytes)).await.ok().flatten();
                    let (code, output) = match lines {
                        Some(l) => (Some(0), l.join("\n")),
                        None => (Some(1), String::new()),
                    };
                    let _ = tx.send(Done::Run { job, code, output }).await;
                });
            }
            super::core::Effect::Read { client, paths } => {
                let tx = done.clone();
                crate::rt::spawn(async move {
                    let value = crate::rt::blocking(move || read_first(&paths)).await.unwrap_or_else(|e| serde_json::json!({"error": format!("{e:#}")}));
                    let _ = tx.send(Done::Reply { client, value }).await;
                });
            }
            super::core::Effect::Fetch { job, url } => {
                let tx = done.clone();
                crate::rt::spawn(async move {
                    let (code, output) = crate::rt::blocking(move || fetch_loopback(&url)).await.unwrap_or_else(|e| (None, format!("{e:#}")));
                    let _ = tx.send(Done::Run { job, code, output }).await;
                });
            }
            super::core::Effect::Checkpoint { at } => {
                let cwd = s.core.lock().state.cwd.clone();
                let id = s.id.clone();
                let tx = done.clone();
                crate::rt::spawn(async move {
                    let made = if cwd.is_empty() { None } else { crate::rt::blocking(move || super::pause::snapshot(&cwd, &id, at)).await.ok().flatten() };
                    let (code, output) = match made {
                        Some((git_ref, commit)) => (Some(0), format!("{git_ref} {commit}")),
                        None => (Some(1), String::new()),
                    };
                    let _ = tx.send(Done::Run { job: format!("{}{at}", super::pause::CHECKPOINT_JOB), code, output }).await;
                });
            }
            super::core::Effect::Kill { job, command } => {
                let tx = done.clone();
                let root = *s.pid.lock();
                crate::rt::spawn(async move {
                    let killed = match root {
                        Some(root) => crate::rt::blocking(move || kill_command(root, &command)).await.unwrap_or(false),
                        None => false,
                    };
                    let (code, output) = if killed { (Some(0), String::new()) } else { (None, "it is not running any more".to_owned()) };
                    let _ = tx.send(Done::Run { job, code, output }).await;
                });
            }
        }
    }
    restart
}

/// A path with `*` components, matched against the directories there (the most recently changed
/// first); a path without them, as it is.
fn expand(path: &str) -> Vec<String> {
    let p = std::path::Path::new(path);
    let mut out: Vec<std::path::PathBuf> = vec![std::path::PathBuf::new()];
    for part in p.components() {
        if part.as_os_str() == "*" {
            let mut next = Vec::new();
            for base in &out {
                let Ok(entries) = std::fs::read_dir(base) else { continue };
                let mut dirs: Vec<(std::time::SystemTime, std::path::PathBuf)> = entries
                    .filter_map(Result::ok)
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| (e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), e.path()))
                    .collect();
                dirs.sort_by_key(|d| std::cmp::Reverse(d.0));
                next.extend(dirs.into_iter().take(64).map(|(_, d)| d));
            }
            out = next;
        } else {
            for o in &mut out {
                o.push(part);
            }
        }
    }
    out.into_iter().map(|p| p.to_string_lossy().into_owned()).collect()
}

/// The first of these Markdown files (skills, commands) that can be read: `{"content", "path"}`,
/// else `{"error"}`. `*` stands for any directory (see `expand`). Blocking.
fn read_first(paths: &[String]) -> serde_json::Value {
    for p in paths.iter().flat_map(|p| expand(p)) {
        let p = &p;
        let path = std::path::Path::new(p);
        if !path.is_absolute() || path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() != Some("md") {
            continue;
        }
        let Ok(meta) = std::fs::metadata(path) else { continue };
        if !meta.is_file() {
            continue;
        }
        if meta.len() > READ_MAX {
            return serde_json::json!({"error": format!("{p} is too large to show here"), "path": p});
        }
        if let Ok(bytes) = std::fs::read(path) {
            return serde_json::json!({"content": String::from_utf8_lossy(&bytes), "path": p});
        }
    }
    serde_json::json!({"error": "The file is not on this machine (any more)."})
}

/// GET a URL on this machine's loopback (a sign-in callback a harness waits for): the HTTP status
/// and the start of the answer, or None and why it failed. Nothing but loopback is fetched.
/// Blocking.
fn fetch_loopback(url: &str) -> (Option<i32>, String) {
    use std::io::{Read as _, Write as _};
    let Some((host, port, target)) = loopback_target(url) else {
        return (None, "Only an http://localhost or http://127.0.0.1 address can be completed on the agent's machine.".to_owned());
    };
    let addrs: Vec<std::net::SocketAddr> = match host.as_str() {
        "::1" => vec![std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port))],
        "127.0.0.1" => vec![std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port))],
        _ => vec![std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port)), std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port))],
    };
    let mut last = String::from("nothing is listening there");
    for addr in addrs {
        let mut conn = match std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
            Ok(c) => c,
            Err(e) => {
                last = e.to_string();
                continue;
            }
        };
        let _ = conn.set_read_timeout(Some(Duration::from_secs(15)));
        let _ = conn.set_write_timeout(Some(Duration::from_secs(5)));
        let host_header = if host == "::1" { format!("[::1]:{port}") } else { format!("{host}:{port}") };
        let req = format!("GET {target} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: kiln\r\nAccept: */*\r\nConnection: close\r\n\r\n");
        if let Err(e) = conn.write_all(req.as_bytes()) {
            last = e.to_string();
            continue;
        }
        let mut answer = Vec::new();
        let _ = conn.take(64 * 1024).read_to_end(&mut answer);
        let text = String::from_utf8_lossy(&answer);
        let code = text.lines().next().and_then(|l| l.split_whitespace().nth(1)).and_then(|c| c.parse::<i32>().ok());
        let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
        return (code, super::model::clip_middle(body.trim(), 2000).0);
    }
    (None, format!("Could not reach {host}:{port} on the agent's machine: {last}"))
}

/// (host, port, request target) of an http URL on loopback; None for anything else.
fn loopback_target(url: &str) -> Option<(String, u16, String)> {
    let rest = url.trim().strip_prefix("http://")?;
    let split = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, target) = rest.split_at(split);
    let target = target.split('#').next().unwrap_or("");
    let target = match target {
        "" => "/".to_owned(),
        t if t.starts_with('?') => format!("/{t}"),
        t => t.to_owned(),
    };
    if !target.bytes().all(|b| b.is_ascii_graphic()) || authority.contains('@') {
        return None;
    }
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => {
            let (h, p) = v6.split_once(']')?;
            (h.to_owned(), p.strip_prefix(':').map_or(Some(80), |p| p.parse().ok())?)
        }
        None => match authority.split_once(':') {
            Some((h, p)) => (h.to_owned(), p.parse().ok()?),
            None => (authority.to_owned(), 80),
        },
    };
    let host = host.to_ascii_lowercase();
    matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1").then_some((host, port, target))
}

/// A switch to another harness, waiting for the turn under way to end.
struct Switch {
    to: &'static Kind,
    model: String,
}

/// Nothing is under way: no turn, nothing waiting for a person.
/// Between turns: idle, nothing waiting on a person, no turn under way.
pub(super) fn settled(s: &Session) -> bool {
    settled_core(&s.core.lock())
}

fn settled_core(core: &Core) -> bool {
    core.state.status == Status::Idle && core.state.approvals.is_empty() && core.state.turn_started.is_none()
}

/// A person chose a model of harness `to`: the session goes on in it once nothing is under way
/// (kiln's queue waits for it meanwhile). None: it can't (said in the timeline).
fn wanted(s: &Session, from: &'static Kind, to: &str, model: String) -> Option<Switch> {
    let target = Kind::get(to).filter(|k| k.switchable() && from.switchable());
    let installed = target.is_some_and(Kind::available);
    // Its spec before its core: never the one lock inside the other.
    let allow_bypass = s.spec.lock().allow_bypass;
    let mut core = s.core.lock();
    let Some(target) = target else {
        core.upsert(&format!("switch-{}", Core::now()), None, super::model::Body::Notice { level: Level::Warn, text: format!("A conversation can't be carried from {} to {to}.", from.name) });
        return None;
    };
    if !installed {
        core.upsert(&format!("switch-{}", Core::now()), None, super::model::Body::Notice { level: Level::Warn, text: format!("{} is not installed on this machine.", target.name) });
        return None;
    }
    core.hold = true;
    // What it will run with there, shown while the switch waits.
    let (mode, effort, modes, efforts) = carried_choices(from, target, &core.state.mode, &core.state.effort, allow_bypass, &model);
    core.state.switching = Some(super::model::Switching { kind: target.id.to_owned(), model: model.clone(), mode, effort, modes, efforts });
    core.touch_state();
    if !(core.state.status == Status::Idle && core.state.approvals.is_empty()) && !matches!(core.state.status, Status::Exited) {
        let text = format!("Switching to {} ({}) when this turn ends — the conversation carries over", model_label(target, &model), target.name);
        core.upsert("switch-pending", None, super::model::Body::Notice { level: Level::Info, text });
    }
    Some(Switch { to: target, model })
}

/// A model's name as its harness lists it, else its id.
fn model_label(kind: &Kind, model: &str) -> String {
    super::kinds::models_of(kind.id).into_iter().find(|m| m.id == model && !m.label.is_empty()).map_or_else(|| model.to_owned(), |m| m.label)
}

/// Most of a conversation carried to another harness (bytes, about): what fits a smaller model's
/// context window with room to work (≈ 160k tokens). Larger ones lose their long tool outputs'
/// middles first, then their oldest messages.
const CARRY_MAX: usize = 640 * 1024;

/// A conversation as the harness it goes on in keeps its own (`carried`).
enum Record {
    /// Claude Code: a new session's id, and its transcript's records.
    Claude { session: String, records: Vec<serde_json::Value> },
    /// Codex: a new thread's history.
    Codex(Vec<serde_json::Value>),
    /// Nothing to carry: it never said anything (or a harness that takes nothing).
    Nothing,
}

/// A conversation carried from one harness to another (`carried`).
struct Carried {
    record: Record,
    /// Its permission mode and effort there: the same latitude (never more), the nearest effort.
    mode: Option<String>,
    effort: Option<String>,
    /// Older messages left out to fit.
    dropped: usize,
    /// There was nothing to carry.
    empty: bool,
}

/// The conversation so far, carried from harness `from` to `to`: read from `from`'s own record of
/// session `session` (never kiln's timeline), fitted, and put the way `to` keeps its own, with a
/// note telling the model so (and `more`, after it); its permission mode and effort carried over.
/// `cwd`: where it goes on (a Claude Code transcript names it); `model`: the one it goes on with
/// (empty: the harness's default). Err: `from`'s record couldn't be read.
#[allow(clippy::too_many_arguments)]
async fn carried(from: &'static Kind, to: &'static Kind, env: &[(String, String)], session: Option<String>, cwd: &str, old_model: &str, mode: &str, effort: &str, allow_bypass: bool, model: &str, more: Option<&str>) -> Result<Carried, String> {
    use super::handoff;
    // What the model saw, from the harness's own record (no session yet: nothing to carry).
    let read = match session {
        None => Some(handoff::Conversation::default()),
        Some(id) => {
            let env = env.to_vec();
            crate::rt::blocking(move || match from.protocol {
                Protocol::ClaudeStreamJson => handoff::read_claude(&env, &id),
                Protocol::CodexAppServer => handoff::read_codex(&env, &id),
                Protocol::Acp(_) | Protocol::AgyStreamJson | Protocol::Terminal => None,
            })
            .await
            .ok()
            .flatten()
        }
    };
    let Some(mut conv) = read else { return Err(format!("kiln could not read {}'s record of this conversation", from.name)) };
    let dropped = handoff::fit(&mut conv, CARRY_MAX);
    let empty = conv.entries.is_empty() && conv.summary.is_none();
    let (mode, effort) = carried_settings(from, to, mode, effort, allow_bypass, model);
    let mut note = handoff::note(from.name, old_model, to.name, &conv.todos, dropped);
    if let Some(more) = more {
        note.push(' ');
        note.push_str(more);
    }
    let record = match to.protocol {
        _ if empty => Record::Nothing,
        Protocol::ClaudeStreamJson => {
            let id = uuid::Uuid::now_v7().to_string();
            let records = handoff::to_claude(&conv, Some(&format!("<system-reminder>\n{note}\n</system-reminder>")), &id, cwd, if old_model.is_empty() { "unknown" } else { old_model });
            Record::Claude { session: id, records }
        }
        Protocol::CodexAppServer => Record::Codex(handoff::to_codex(&conv, Some(&format!("<kiln_handoff>\n{note}\n</kiln_handoff>")))),
        Protocol::Acp(_) | Protocol::AgyStreamJson | Protocol::Terminal => Record::Nothing,
    };
    Ok(Carried { record, mode, effort, dropped, empty })
}

/// The permission mode and effort a session goes on with in harness `to`, carried from `from`: the
/// same latitude (never more; a mode that asks nothing only where `allow_bypass`) and the nearest
/// effort `model` takes there. With `to`'s choices of each, for a page to say them by their names.
fn carried_settings(from: &Kind, to: &Kind, mode: &str, effort: &str, allow_bypass: bool, model: &str) -> (Option<String>, Option<String>) {
    let (mode, effort, _, _) = carried_choices(from, to, mode, effort, allow_bypass, model);
    (mode, effort)
}

fn carried_choices(from: &Kind, to: &Kind, mode: &str, effort: &str, allow_bypass: bool, model: &str) -> (Option<String>, Option<String>, Vec<super::model::ModeOption>, Vec<super::model::ControlOption>) {
    use super::handoff;
    let modes: Vec<super::model::ModeOption> = to.modes().0.into_iter().filter(|m| !m.dangerous || allow_bypass).collect();
    let offered: Vec<String> = modes.iter().map(|m| m.id.clone()).collect();
    let mode = handoff::mode_for(from.id, mode, to.id, &offered);
    let efforts: Vec<super::model::ControlOption> = super::kinds::models_of(to.id)
        .into_iter()
        .find(|m| m.id == model)
        .map(|m| m.efforts)
        .filter(|e| !e.is_empty())
        .map(|levels| levels.iter().map(|e| super::model::ControlOption { id: e.clone(), label: super::model::effort_label(e), description: String::new(), dangerous: false }).collect())
        .unwrap_or_else(|| to.efforts());
    let levels: Vec<String> = efforts.iter().map(|e| e.id.clone()).collect();
    (mode, handoff::effort_for(effort, &levels), modes, efforts)
}

/// The session's state for the harness it goes on in (`session`: its conversation's id there):
/// the model, mode and effort it runs with; what was the old harness's own (its servers, skills,
/// account, plan and limits, what it was doing) goes with it.
fn for_harness(st: &mut super::model::State, session: Option<String>, model: &str, mode: Option<String>, effort: Option<String>) {
    st.harness_session = session;
    model.clone_into(&mut st.model);
    st.mode = mode.unwrap_or_default();
    st.effort = effort.unwrap_or_default();
    // A speed is its harness's own (Claude Code's fast mode isn't one of Codex's tiers).
    st.speed.clear();
    st.speed_blocked = None;
    st.speed_cooldown = false;
    st.mcp.clear();
    st.skills.clear();
    st.plugins.clear();
    st.inventory = Default::default();
    st.harness_status.clear();
    st.limits.clear();
    st.account = None;
    st.plan = None;
    st.control_values.clear();
    st.objective = None;
    st.usage.context_tokens = None;
    st.usage.context_window = None;
    st.exit = None;
    st.activity = None;
    st.doing = None;
}

/// A paused session moving to another machine goes on there: the pause (for this machine's
/// account) stays behind, and so do that account's limits. True when the pause had cut a turn
/// off, which goes on there first.
fn picks_up_there(st: &mut super::model::State) -> bool {
    let Some(p) = st.pause.take() else { return false };
    st.limits.clear();
    st.stop_waived.clear();
    p.cut.is_some()
}

/// Go on in harness `sw.to` (the one the session ran in, `from`, has ended): the conversation so
/// far read from `from`'s own record and written the way `sw.to` keeps its own, the permission
/// mode and effort carried over. The session's id, timeline, changeset and queue stay. Returns
/// the harness to start and, for one that takes it once started (Codex), the conversation; None:
/// it couldn't be carried (said in the timeline), and the session stays in `from`.
async fn carry_over(s: &Arc<Session>, from: &'static Kind, sw: Switch, left: usize, sink: &Sink) -> Option<(&'static Kind, Option<Vec<serde_json::Value>>)> {
    use super::handoff;
    let to = sw.to;
    let (env, allow_bypass) = {
        let sp = s.spec.lock();
        (sp.env.clone(), sp.allow_bypass)
    };
    let (cwd, session, old_model, mode, effort) = {
        let core = s.core.lock();
        let st = &core.state;
        (st.cwd.clone(), st.harness_session.clone(), st.model.clone(), st.mode.clone(), st.effort.clone())
    };
    let fail = |s: &Arc<Session>, text: String| {
        let mut core = s.core.lock();
        core.hold = false;
        core.state.switching = None;
        core.touch_state();
        core.upsert(&format!("switch-{}", Core::now()), None, super::model::Body::Notice { level: Level::Warn, text });
        if let Some(e) = core.get_mut("switch-pending") {
            e.body = super::model::Body::Notice { level: Level::Info, text: "Switch cancelled".to_owned() };
        }
    };
    let c = match carried(from, to, &env, session, &cwd, &old_model, &mode, &effort, allow_bypass, &sw.model, None).await {
        Ok(c) => c,
        Err(e) => {
            fail(s, format!("{e}, so it goes on in {}.", from.name));
            return None;
        }
    };
    let (dropped, empty, mode, effort) = (c.dropped, c.empty, c.mode, c.effort);
    let (resume, seed) = match c.record {
        Record::Claude { session: id, records } => {
            let (env, cwd, sid) = (env.clone(), cwd.clone(), id.clone());
            match crate::rt::blocking(move || handoff::write_claude(&env, &cwd, &sid, &records)).await {
                Ok(Ok(_)) => (Some(id), None),
                Ok(Err(e)) | Err(e) => {
                    fail(s, format!("kiln could not write the conversation for {}: {e:#}", to.name));
                    return None;
                }
            }
        }
        Record::Codex(items) => (None, Some(items)),
        Record::Nothing => (None, None),
    };
    {
        let mut sp = s.spec.lock();
        sp.kind = to.id.to_owned();
        sp.model = Some(sw.model.clone());
        sp.mode.clone_from(&mode);
        sp.effort.clone_from(&effort);
        sp.speed = None;
        sp.resume.clone_from(&resume);
        sp.resume_at = None;
        sp.fork = false;
        sp.prompt = None;
        // A program of its own was the old harness's.
        sp.program = None;
        sp.takeover_pid = None;
        sp.takeover_stop = false;
    }
    {
        let mut core = s.core.lock();
        let st = &mut core.state;
        for_harness(st, resume, &sw.model, mode, effort);
        st.status = Status::Starting;
        core.meta = super::model::Meta::default();
        core.hold = false;
        core.state.switching = None;
        // A pause for the old harness's limit ends: the new one runs on its own account. The turn
        // the limit cut off goes on first.
        let unpaused = core.state.pause.take();
        if unpaused.as_ref().is_some_and(|p| p.cut.is_some()) {
            core.state.queue.insert(0, super::model::QueueItem { id: format!("{}{}", super::pause::KILN_ID, crate::util::new_id()), text: super::pause::CONTINUE_ELSEWHERE.to_owned(), at: Core::now(), attachments: Vec::new(), overrides: None, reply_to: None });
        }
        let mut text = format!("Now {} ({})", model_label(to, &sw.model), to.name);
        if !empty {
            text.push_str(" — the conversation carried over");
        }
        if unpaused.is_some() {
            text.push_str(&format!("; the pause for {}'s usage limit ended", from.name));
        }
        if dropped > 0 {
            text.push_str(&format!(" ({dropped} older messages left out to fit)"));
        }
        if left > 0 {
            text.push_str(&format!("; the {left} task(s) {} left running were stopped", from.name));
        }
        // `harness-switch:` marks it for viewers: a line between the turns of one harness and the next.
        core.upsert(&format!("harness-switch:{}", Core::now()), None, super::model::Body::Notice { level: Level::Info, text });
        core.touch_meta();
        core.touch_state();
    }
    {
        let mut info = s.info.lock();
        if info.name == from.name {
            to.name.clone_into(&mut info.name);
        }
        to.id.clone_into(&mut info.kind);
        to.name.clone_into(&mut info.kind_name);
    }
    // Everyone sees it as the harness it is now.
    for u in s.snapshot() {
        let _ = sink.send((s.id.clone(), u)).await;
    }
    tracing::info!(session = %s.id, from = from.id, to = to.id, model = %sw.model, dropped, "agent session switched harness");
    Some((to, seed))
}

/// A move to another runner, waiting for the turn under way to end.
struct Move {
    to: String,
    cwd: String,
    note: Option<String>,
    /// The turn under way was interrupted to move now: it is picked up over there.
    picks_up: bool,
    /// It goes on there in this harness (its conversation carried over), with this model.
    kind: Option<&'static Kind>,
    model: Option<String>,
    /// To this machine's newer runner process, which takes this one's place (`super::handing`).
    local: bool,
}

/// A move asked for: kiln's queue waits (what is queued goes along), and the timeline says so.
/// `now`: the turn under way isn't waited for (the caller interrupts it).
#[allow(clippy::too_many_arguments)]
fn move_wanted(s: &Session, to: String, cwd: String, note: Option<String>, now: bool, kind: Option<&'static Kind>, model: Option<String>, local: bool) -> Move {
    let mut core = s.core.lock();
    core.hold = true;
    let busy = !(core.state.status == Status::Idle && core.state.approvals.is_empty()) && !matches!(core.state.status, Status::Exited);
    let picks_up = now && busy;
    let into = kind.map(|k| format!(", to go on in {}", k.name)).unwrap_or_default();
    let text = if local {
        if busy { "This machine's newer kiln runner takes this agent over when this turn ends".to_owned() } else { "This machine's newer kiln runner takes this agent over…".to_owned() }
    } else if picks_up {
        format!("Moving to {to}{into} now: this turn stops here and goes on there")
    } else if busy {
        format!("Moving to {to}{into} when this turn ends")
    } else {
        format!("Moving to {to}{into}…")
    };
    core.upsert("move-pending", None, super::model::Body::Notice { level: Level::Info, text });
    core.touch_state();
    Move { to, cwd, note, picks_up: picks_up && !local, kind: kind.filter(|_| !local), model, local }
}

/// How a move is getting on before it starts (its repository being copied there).
fn moving_note(s: &Session, text: &str) {
    s.core.lock().upsert("move-pending", None, super::model::Body::Notice { level: Level::Info, text: text.to_owned() });
}

/// How long a session leaving may take to get its folder's last changes into the sync log.
const FILES_OUT_FOR: Duration = Duration::from_secs(600);
/// How often a session leaving, or arriving, asks how its folder's copy stands.
const FILES_EVERY: Duration = Duration::from_millis(250);

/// Where a leaving session's synced folder stood once its last changes were in the log: the index
/// the machine it goes to takes the log to before it goes on there, its copy's folder here, and
/// the repo's name.
struct Synced {
    mark: crate::sync::runner::Mark,
    folder: String,
    name: String,
}

/// Before a session leaves: its folder's last changes into its repo's sync log (looked at now,
/// sent, their contents on the server, the log having brought them back), then where the copy
/// stands, for the machine it goes to to take the log that far before it goes on there. None: its
/// folder isn't a synced copy here. An error, and it doesn't leave: it would leave work behind.
async fn files_out(s: &Session, cwd: &str, to: &str) -> Result<Option<Synced>, String> {
    let Some(sync) = crate::sync::runner::global() else { return Ok(None) };
    let folder = std::path::Path::new(cwd);
    let started = Instant::now();
    let mut said = String::new();
    loop {
        let Some(looked) = sync.look_now(folder).await else { return Ok(None) };
        let st = sync.standing(folder).await.unwrap_or_default();
        if let Some(mark) = crate::sync::runner::left_at(&looked, &st) {
            left_behind(cwd, &st).await?;
            return Ok(Some(Synced { mark, folder: st.folder, name: st.name }));
        }
        let why = match (&st.error, st.connected) {
            (Some(e), _) => format!(" ({e})"),
            (None, false) => " (this machine isn't connected to kiln)".to_owned(),
            (None, true) => String::new(),
        };
        if started.elapsed() > FILES_OUT_FOR {
            return Err(format!("its folder's last changes didn't reach kiln in {} minutes{why}, so they'd be left behind", FILES_OUT_FOR.as_secs() / 60));
        }
        let now = match st.unsent + looked.proposed {
            0 => format!("Moving to {to}: its folder's last changes are being checked{why}"),
            n => format!("Moving to {to}: sending its folder's last changes ({n} to go){why}"),
        };
        if now != said {
            moving_note(s, &now);
            said = now;
        }
        crate::rt::sleep(FILES_EVERY).await;
    }
}

/// What a session's repository carries besides its synced files.
#[derive(Default)]
struct GitOut {
    git: Option<super::gitstate::GitState>,
    ignored: Option<super::carry::Carried>,
    worktree: Option<super::worktree::Worktree>,
}

/// Before a session leaves: its repository besides its files, less what the machine it goes to
/// has: its branches, stash and what's staged ([`super::gitstate`]), and the files git ignores
/// ([`super::carry`]); or, working in a worktree inside the synced repository, that worktree
/// ([`super::worktree`]). `copy`: the synced folder it's in; `there`: its folder over there. An
/// error, and it doesn't leave: something would be left behind.
async fn git_out(s: &Session, cwd: &str, copy: &str, to: &str, there: &str) -> Result<GitOut, String> {
    let (c, k) = (cwd.to_owned(), copy.to_owned());
    let (root, nested) = crate::rt::blocking(move || {
        let here = std::path::Path::new(&c);
        (super::gitstate::root_of(here), super::worktree::nested(here, std::path::Path::new(&k)))
    })
    .await
    .map_err(|e| format!("{e:#}"))?;
    let Some(root) = root else { return Ok(GitOut::default()) };
    moving_note(s, &format!("Moving to {to}: its repository's branches, stash, what's staged and what git ignores"));
    // Its copy over there answers for what it has: the folder it goes on in, or, for a worktree
    // that isn't there yet, the repository it's made in.
    let ask = match &nested {
        Some(super::worktree::Nested::Worktree { .. }) => {
            let depth = std::path::Path::new(cwd).strip_prefix(copy).map_or(0, |r| r.components().count());
            let mut p = std::path::PathBuf::from(there);
            for _ in 0..depth {
                p.pop();
            }
            p.to_string_lossy().into_owned()
        }
        _ => there.to_owned(),
    };
    let theirs = match their_tips(to, &ask).await {
        Some(t) => t,
        // Not told: what was pushed, it likely has.
        None => {
            let r = root.clone();
            crate::rt::blocking(move || super::gitstate::pushed(&r)).await.unwrap_or_default()
        }
    };
    let c = cwd.to_owned();
    crate::rt::blocking(move || -> Result<GitOut, String> {
        match nested {
            Some(super::worktree::Nested::Worktree { root: w, rel }) => {
                let git = super::gitstate::capture(&w, &theirs)?;
                let mut wt = super::worktree::capture(&w, &rel)?;
                wt.sub = std::path::Path::new(&c).strip_prefix(&w).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
                Ok(GitOut { git: Some(git), ignored: None, worktree: Some(wt) })
            }
            Some(super::worktree::Nested::Own(p)) => Err(format!("its folder is in {}, a repository of its own inside the synced one (or a submodule), whose files sync doesn't carry: commit and push its work, and it moves", p.display())),
            None => Ok(GitOut { git: Some(super::gitstate::capture(&root, &theirs)?), ignored: Some(super::carry::ignored(&root)?), worktree: None }),
        }
    })
    .await
    .map_err(|e| format!("{e:#}"))?
}

/// What the copy of the folder on machine `to` (at `there`) has at the tips of its refs, as it
/// says over reach. None when it can't be asked.
async fn their_tips(to: &str, there: &str) -> Option<Vec<String>> {
    use crate::reach::wire::{RunSpec, Say, Stream};
    let reach = crate::reach::node::global()?;
    reach.connect(to).await.ok()?;
    let spec = RunSpec {
        command: "git for-each-ref --format='%(objectname)' && git rev-parse -q --verify HEAD; true".into(),
        cwd: Some(there.to_owned()),
        timeout_ms: Some(30_000),
        ..RunSpec::default()
    };
    let (tx, rx) = async_channel::unbounded();
    reach.run(to, spec, tx).await.ok()?;
    let mut out = Vec::new();
    let until = Instant::now() + Duration::from_secs(40);
    loop {
        let left = until.saturating_duration_since(Instant::now());
        match crate::rt::timeout(left, rx.recv()).await {
            Ok(Ok(Say::Output { stream: Stream::Out, data, .. })) => out.extend(data),
            Ok(Ok(Say::Exit { code: Some(0), .. })) => break,
            Ok(Ok(Say::Exit { .. })) | Ok(Err(_)) | Err(_) => return None,
            Ok(Ok(_)) => {}
        }
    }
    let tips: Vec<String> = String::from_utf8_lossy(&out).lines().map(str::trim).filter(|l| l.len() >= 40 && l.chars().all(|c| c.is_ascii_hexdigit())).map(str::to_owned).collect();
    Some(tips)
}

/// A moved session's repository put as it was on the machine it left, once its files are in:
/// what [`super::gitstate::restore`] did, in its timeline.
async fn git_in(s: &Session, sink: &Sink, files: &Files) {
    let path = super::moving::moved_git(&files.dir);
    let Ok(bytes) = std::fs::read(&path) else { return };
    let cwd = s.core.lock().state.cwd.clone();
    // The synced folder a worktree goes in: the copy its folder here is in.
    let copy = match crate::sync::runner::global() {
        Some(sync) => sync.standing(std::path::Path::new(&cwd)).await.map(|st| st.folder),
        None => None,
    };
    let said = crate::rt::blocking(move || -> Vec<String> {
        let Ok(kept) = serde_json::from_slice::<super::moving::MovedGit>(&bytes) else { return vec!["Its repository's state from the machine it left couldn't be read".into()] };
        if let Some(w) = &kept.worktree {
            let copy = copy.map(std::path::PathBuf::from).or_else(|| {
                // Without sync's word: its folder less the worktree's place in the repository.
                let up = std::path::Path::new(&w.rel).components().count() + std::path::Path::new(&w.sub).components().count();
                std::path::Path::new(&cwd).ancestors().nth(up).map(std::path::Path::to_path_buf)
            });
            let Some(copy) = copy else { return vec![format!("Its worktree couldn't be made here: no copy of its repository holds {cwd}")] };
            if let Err(e) = super::gitstate::fetch(&copy, &kept.state, &kept.from) {
                return vec![format!("Its worktree couldn't be made here ({e}); it stays on {}", kept.from)];
            }
            return match super::worktree::place(&copy, w, &kept.from) {
                Ok((at, mut said)) => {
                    said.extend(super::gitstate::restore_at(&copy, &at, &kept.state, &kept.from));
                    said
                }
                Err(e) => vec![format!("Its worktree couldn't be made here ({e}); it stays on {}", kept.from)],
            };
        }
        match super::gitstate::root_of(std::path::Path::new(&cwd)) {
            Some(root) => {
                let mut said = super::gitstate::restore(&root, &kept.state, &kept.from);
                if let Some(ig) = &kept.ignored {
                    said.extend(super::carry::place(&root, ig, &kept.from));
                }
                said
            }
            None => vec![format!("Its folder here, {cwd}, isn't a git repository, so its branches, stash and the files git ignores stay on {}", kept.from)],
        }
    })
    .await
    .unwrap_or_default();
    {
        let mut core = s.core.lock();
        for (i, text) in said.into_iter().enumerate() {
            let level = if i == 0 { Level::Info } else { Level::Warn };
            core.upsert(&format!("move-git-{}-{i}", Core::now()), None, super::model::Body::Notice { level, text });
        }
    }
    let _ = std::fs::remove_file(&path);
    flush(s, sink, None).await;
}

/// What of a session's folder sync doesn't carry, and would be left behind: then it doesn't move.
/// A repository of its own nested inside the synced one, or a submodule (sync carries the outer
/// one's files, never a nested repository's; a worktree of the same repository moves with the
/// session, [`super::worktree`]), and files too big for sync.
async fn left_behind(cwd: &str, st: &crate::sync::runner::Standing) -> Result<(), String> {
    let (c, copy) = (cwd.to_owned(), st.folder.clone());
    let nested = crate::rt::blocking(move || super::worktree::nested(std::path::Path::new(&c), std::path::Path::new(&copy))).await.ok().flatten();
    if let Some(super::worktree::Nested::Own(inner)) = nested {
        return Err(format!(
            "its folder is in {}, a repository of its own inside {} (or a submodule): sync carries {}'s files and not its, so its work would be left behind. Commit and push it, and it moves",
            inner.display(),
            st.folder,
            st.folder
        ));
    }
    let here = std::path::Path::new(cwd);
    let big: Vec<&String> = st.skipped.iter().filter(|p| std::path::Path::new(&st.folder).join(p).starts_with(here)).collect();
    if !big.is_empty() {
        let names: Vec<&str> = big.iter().take(5).map(|p| p.as_str()).collect();
        return Err(format!(
            "{} {} too big for sync to carry ({}{}), so {} be left behind: move {} out of the repository, or have git ignore {}, and it moves",
            big.len(),
            if big.len() == 1 { "file is" } else { "files are" },
            names.join(", "),
            if big.len() > 5 { ", …" } else { "" },
            if big.len() == 1 { "it would" } else { "they would" },
            if big.len() == 1 { "it" } else { "them" },
            if big.len() == 1 { "it" } else { "them" },
        ));
    }
    Ok(())
}

/// How waiting for a moved session's files ended.
#[derive(Debug, PartialEq, Eq)]
enum Arrived {
    /// They're here (or can't be waited for): it goes on.
    GoOn,
    /// It was closed meanwhile, or (`let_go`) went on elsewhere.
    End { let_go: bool },
}

/// A session that moved here goes on once its folder has taken its repo's sync log as far as the
/// machine it left had it: it never starts on files older than the ones it left. Messages sent
/// meanwhile wait in `carry`; closing it ends the wait.
async fn files_in(s: &Session, sink: &Sink, mark: &crate::sync::runner::Mark, ctl_rx: &async_channel::Receiver<Ctl>, carry: &mut Vec<Command>, files: &Files) -> Arrived {
    let note = |text: String, level: Level| s.core.lock().upsert("move-files", None, super::model::Body::Notice { level, text });
    let Some(sync) = crate::sync::runner::global() else {
        note("This machine doesn't sync folders, so its folder here may not have its last changes".into(), Level::Warn);
        return Arrived::GoOn;
    };
    let cwd = s.core.lock().state.cwd.clone();
    let folder = std::path::Path::new(&cwd);
    let started = Instant::now();
    let mut said = String::new();
    loop {
        let now = match sync.standing(folder).await {
            Some(st) if crate::sync::runner::arrived(&st, mark) => {
                let waited = started.elapsed();
                note(format!("Its folder's last changes are here (in {}s); it goes on", waited.as_secs()), Level::Info);
                crate::metrics::sync().move_files_wait.observe(waited.as_secs_f64());
                tracing::Span::current().record("repo", st.name.as_str());
                // The server knows this machine as the one it arrived on.
                tell_move(crate::sync::wire::MoveNews {
                    repo: st.repo.clone(),
                    index: mark.index,
                    session: s.id.clone(),
                    phase: "arrived".into(),
                    waited_ms: u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
                    ..crate::sync::wire::MoveNews::default()
                });
                flush(s, sink, None).await;
                return Arrived::GoOn;
            }
            Some(st) if st.repo != mark.repo => {
                note(format!("Its folder here, {cwd}, is a copy of another repository, so it goes on without waiting for its files"), Level::Warn);
                flush(s, sink, None).await;
                return Arrived::GoOn;
            }
            Some(st) => {
                let why = st.error.as_ref().map(|e| format!(" ({e})")).unwrap_or_default();
                format!("Waiting for its folder's last changes to arrive: {} to take, {} files coming{why}. It goes on once they're here", st.behind.max(1), st.waiting)
            }
            None => "Waiting for its folder's last changes to arrive: this machine's copy of it isn't syncing yet. It goes on once they're here".to_owned(),
        };
        if now != said {
            note(now.clone(), Level::Info);
            flush(s, sink, None).await;
            said = now;
        }
        let tick = crate::rt::sleep(Duration::from_secs(1)).fuse();
        let msg = ctl_rx.recv().fuse();
        futures::pin_mut!(tick, msg);
        futures::select_biased! {
            m = msg => match m {
                Ok(Ctl::Cmd(c)) => match *c {
                    Command::Moving { text } => moving_note(s, &text),
                    c @ (Command::Send { .. } | Command::SkillRun { .. }) => carry.push(prepare(files, c).await),
                    c => carry.push(c),
                },
                // No process to hand over: the next image starts it from its record.
                Ok(Ctl::HandOver(reply)) => {
                    let _ = reply.send(None);
                }
                Ok(Ctl::LetGo) => return Arrived::End { let_go: true },
                Ok(Ctl::Close) | Err(_) => return Arrived::End { let_go: false },
            },
            () = tick => {}
        }
    }
}

/// A move that couldn't be: the session goes on here, and the timeline says why.
fn move_failed(s: &Session, mv: &Move, why: &str) {
    let mut core = s.core.lock();
    core.hold = false;
    core.touch_state();
    let text = if mv.local {
        format!("This machine's newer kiln runner couldn't take this agent over: {why} — it goes on here")
    } else {
        format!("Couldn't move to {}: {why} — it goes on here", mv.to)
    };
    core.upsert("move-pending", None, super::model::Body::Notice { level: Level::Warn, text });
}

/// Hand the session to this machine's newer runner process (its turn is over, its harness down),
/// which takes this one's place: its timeline, state and queue, by way of the server. Same disk,
/// same home: its folder and its harness's own record stay where they are, and it resumes from
/// them there under the same harness session, as it would after a restart. A harness whose own
/// record kiln doesn't keep (ACP agents, Antigravity) may not load it again: its conversation
/// goes in words, as a move carries it. Ok once that process has taken it over. Its inbox's ids go
/// with it and its inbox here is fenced first, as for a move (`ids`, `fence`; CP-T3.2): the newer
/// process keeps an inbox of its own. With no word of how it went, it waits here and asks again
/// until it's known, as a move does.
async fn hand_to_newer(s: &Arc<Session>, kind: &'static Kind, mv: &Move, ids: &CarriedIds, fence: &FenceFor, settle: &SettleFor) -> Result<(), String> {
    let id = s.id.clone();
    let send = |bytes: Vec<u8>| {
        let id = id.clone();
        async move { super::handing::send(&id, &bytes).await }
    };
    let handed = hand_to_newer_by(s, kind, mv, ids, fence, settle, send, crate::rt::sleep).await;
    super::handing::done(&s.id);
    handed
}

/// [`hand_to_newer`], each try's package sent by `send` (`super::handing::send`), the time
/// between tries waited by `wait`.
#[allow(clippy::too_many_arguments)]
async fn hand_to_newer_by<F, Fut, W, WFut>(s: &Arc<Session>, kind: &'static Kind, mv: &Move, ids: &CarriedIds, fence: &FenceFor, settle: &SettleFor, send: F, wait: W) -> Result<(), String>
where
    F: Fn(Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = Result<(), super::handing::NotHanded>>,
    W: Fn(Duration) -> WFut,
    WFut: std::future::Future<Output = ()>,
{
    use super::handing::NotHanded;
    use super::moving::Carry;
    // Its inbox here fenced first, so it takes nothing more: a copy of it this process goes on with
    // takes nothing the newer one may have taken (CP-T3.2). Then the ids of the messages it took,
    // read once what its lane holds is done (a message its inbox took before the fence, and it then
    // refused, is taken back by then), and only then what it holds, copied. So no id goes without
    // its message, which a retry would find taken there and never deliver, and nothing it holds
    // stays behind (the hand-over review's point 2).
    fence(s.id.clone(), true).await.map_err(|e| format!("its inbox couldn't be fenced, so it stays here: {e}"))?;
    // The ids go with it (it takes none while it's handed over): the newer process's inbox would
    // take one sent again as new. Never without them: it stays here.
    let kept = match ids(s.id.clone()).await {
        Ok(kept) => kept,
        Err(e) => return Err(unfenced(s, fence, format!("its inbox couldn't be read, so it stays here: {e}")).await),
    };
    let spec = s.resume_spec();
    let (resume, program) = (spec.resume.clone(), spec.program.clone());
    let (events, state, cwd) = {
        let core = s.core.lock();
        // The note that it was about to go is this process's alone.
        (core.events().filter(|e| e.id != "move-pending").cloned().collect::<Vec<_>>(), core.state.clone(), core.state.cwd.clone())
    };
    let carry = match kind.protocol {
        super::kinds::Protocol::Acp(_) | super::kinds::Protocol::AgyStreamJson => {
            super::moving::words(&events, "This conversation went on in this machine's newer kiln runner, which took the older one's place.")
        }
        _ => Carry::Nothing,
    };
    let mut pkg = super::moving::pack(&s.id, spec, &cwd, &mv.to, &cwd, carry, events, state);
    // Its harness's own record is here already: it goes on from it, nothing started anew.
    if matches!(pkg.carry, Carry::Nothing) {
        pkg.spec.resume = resume;
    }
    pkg.spec.program = program;
    pkg.handover = true;
    // The same try each time it's sent again: the machine decides it once.
    let attempt = crate::util::new_id();
    pkg.attempt = Some(attempt.clone());
    (pkg.inbox, pkg.inbox_digests) = super::host::split_kept(kept);
    let bytes = match serde_json::to_vec(&pkg) {
        Ok(bytes) => bytes,
        Err(e) => return Err(unfenced(s, fence, e.to_string()).await),
    };
    // How it went, until it's known (CP-T3.2, the eighteenth and nineteenth adversarial checks'):
    // the machine decides each try once, whichever of its processes comes first (`settle`), so the
    // server's word, which may be an earlier try's come late, says only when to look. Taken over,
    // it lets go; refused (or, before any try went without a word, not passed on), it takes the try
    // back unless a newer process took it first, and goes on here. With no word, or the server's
    // own after a try that went without one, it looks: while it isn't decided, the newer process
    // may run it now, so it waits (its harness down, its queue held, its inbox fenced) and its
    // package goes again. Where the machine's record isn't kept, the word alone says, as before.
    let mut pause = UNKNOWN_FIRST;
    let mut unknown = false;
    let handed = loop {
        let word = send(bytes.clone()).await;
        let take_back = match &word {
            Ok(()) => break Ok(()),
            Err(NotHanded::Refused(_)) => true,
            Err(NotHanded::NotPassed(_)) => !unknown,
            Err(NotHanded::Unknown(_)) => false,
        };
        let why = word.err().map(|e| e.to_string()).unwrap_or_default();
        let why = match settle(attempt.clone(), s.id.clone(), take_back).await {
            Ok(Some(super::host::Decided::Taken)) => break Ok(()),
            Ok(Some(super::host::Decided::Refused)) => break Err(why),
            Ok(None) if take_back => break Err(why),
            Ok(None) => why,
            Err(e) => format!("{why}; how it went can't be read here now: {e}"),
        };
        unknown = true;
        tracing::warn!(target: "kiln.sends", event = "handover_unknown", session = %s.id, error = %why, "a session's hand-over to this machine's newer runner process isn't known to have gone or not: it takes no messages here, and runs nothing, until it is; asking again");
        moving_note(s, &format!("Handing over to this machine's newer kiln runner: how it went isn't known yet ({why}), so it may run there now. It takes no messages here until it's known; asking again"));
        wait(pause).await;
        pause = (pause * 2).min(UNKNOWN_MOST);
    };
    // Refused for sure: it goes on here, taking messages again.
    if handed.is_err()
        && let Err(e) = fence(s.id.clone(), false).await
    {
        tracing::warn!(target: "kiln.sends", event = "unfence_failed", session = %s.id, error = %e, "a session whose hand-over failed couldn't take messages again: its inbox stays fenced until it's handed over");
    }
    handed
}

/// A move whose answer never came asks the runner it went to again after this, then twice as long
/// each time, up to [`UNKNOWN_MOST`].
const UNKNOWN_FIRST: Duration = Duration::from_secs(5);
const UNKNOWN_MOST: Duration = Duration::from_secs(60);

/// Session `s`'s inbox, fenced as it began to go, takes messages again: nothing of it went, and it
/// goes on here. `why` it didn't go, as it's said.
async fn unfenced(s: &Session, fence: &FenceFor, why: String) -> String {
    if let Err(e) = fence(s.id.clone(), false).await {
        tracing::warn!(target: "kiln.sends", event = "unfence_failed", session = %s.id, error = %e, "a session that couldn't go couldn't take messages again: its inbox stays fenced until it goes");
    }
    why
}

/// Hand the session to runner `mv.to` (its turn is over, its harness down): its record, timeline
/// and queue, over reach. Ok once that runner has taken it over and runs it. One that may have taken
/// it (all of it went, and no answer came) is asked again until it says: meanwhile the session
/// takes nothing here.
async fn hand_over(s: &Arc<Session>, kind: &'static Kind, mv: &Move, ids: &CarriedIds, fence: &FenceFor, settle: &SettleFor) -> Result<(), String> {
    use super::moving::Carry;
    if mv.local {
        // Its process is a terminal's: it stays with the process it runs in until it ends.
        if matches!(kind.protocol, super::kinds::Protocol::Terminal) {
            return Err("an agent in a terminal stays where it runs until it ends".into());
        }
        return hand_to_newer(s, kind, mv, ids, fence, settle).await;
    }
    let reach = crate::reach::node::global().ok_or_else(|| "this runner can't reach other runners (an older one?)".to_owned())?;
    let me = reach.name().to_owned();
    let mut spec = s.resume_spec();
    // Going on there in another harness: its timeline says so (there too, as it travels with it).
    if let Some(to) = mv.kind {
        if !(kind.switchable() && to.switchable()) {
            return Err(format!("a conversation can't be carried from {} to {}", kind.name, to.name));
        }
        let model = mv.model.as_deref().map_or_else(|| "its default model".to_owned(), |m| model_label(to, m));
        let text = format!("Going on in {} ({model}) on {} — the conversation carries over", to.name, mv.to);
        s.core.lock().upsert(&format!("harness-switch:move-{}", Core::now()), None, super::model::Body::Notice { level: Level::Info, text });
    }
    let (events, mut state, from_cwd, session) = {
        let core = s.core.lock();
        // The note that it was about to move is this machine's alone.
        (core.events().filter(|e| e.id != "move-pending").cloned().collect::<Vec<_>>(), core.state.clone(), core.state.cwd.clone(), core.state.harness_session.clone())
    };
    let mut note = format!("This conversation moved from {me} (where it worked in {from_cwd}) to {} (it works in {} now).", mv.to, mv.cwd);
    if let Some(n) = &mv.note {
        note.push(' ');
        note.push_str(n);
    }
    let (carry, home) = match mv.kind {
        // Its record, and what its harness keeps for it in its home besides (`homes`).
        None => {
            let env = spec.env.clone();
            let timeline = events.clone();
            let (cwd, objective) = (from_cwd.clone(), state.objective.clone());
            crate::rt::blocking(move || {
                let carry = super::moving::record(&kind.protocol, &env, session.as_deref(), &timeline, &note)?;
                let home = super::homes::pack(kind.id, &super::homes::Dirs::here(&env), session.as_deref(), &cwd, super::moving::transcript_of(&carry), objective.as_ref());
                Ok::<_, String>((carry, Some(home)))
            })
            .await
            .map_err(|e| format!("{e:#}"))??
        }
        // The other harness's own record of it, as a switch writes one (`carried`).
        Some(to) => {
            let model = mv.model.clone().unwrap_or_default();
            let c = carried(kind, to, &spec.env, session, &mv.cwd, &state.model, &state.mode, &state.effort, spec.allow_bypass, &model, Some(&note)).await?;
            let (carry, there) = match c.record {
                Record::Claude { session, records } => (Carry::ClaudeLines { session: session.clone(), lines: records.iter().map(serde_json::Value::to_string).collect() }, Some(session)),
                Record::Codex(items) => (Carry::Seed { items }, None),
                Record::Nothing => (Carry::Nothing, None),
            };
            to.id.clone_into(&mut spec.kind);
            spec.model.clone_from(&mv.model);
            spec.mode.clone_from(&c.mode);
            spec.effort.clone_from(&c.effort);
            spec.speed = None;
            // A program of its own was the old harness's.
            spec.program = None;
            for_harness(&mut state, there, &model, c.mode, c.effort);
            // What the old harness keeps in its home is its own: the other one doesn't read it.
            (carry, None)
        }
    };
    // Its folder's last changes go first, so it goes on there from them; then its repository's
    // state besides its files (only a synced folder's: there's a repository there to take it).
    let barrier = tracing::info_span!(target: "kiln.sync", "move.barrier", repo = tracing::field::Empty, machine = %mv.to, result = tracing::field::Empty);
    let started = Instant::now();
    let synced = files_out(s, &from_cwd, &mv.to).instrument(barrier.clone()).await;
    let waited = started.elapsed();
    barrier.record("result", if synced.is_ok() { "ok" } else { "failed" });
    if let Ok(Some(x)) = &synced {
        barrier.record("repo", x.name.as_str());
        tracing::Span::current().record("repo", x.name.as_str());
        crate::metrics::sync().move_barrier.observe(waited.as_secs_f64());
    }
    drop(barrier);
    // How this machine's part went, for the repo's activity on the server.
    let news = |repo: &str, index: u64, phase: &str, error: Option<&str>| crate::sync::wire::MoveNews {
        repo: repo.to_owned(),
        index,
        session: s.id.clone(),
        from: me.clone(),
        to: mv.to.clone(),
        phase: phase.to_owned(),
        waited_ms: u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
        error: error.map(str::to_owned),
    };
    let synced = match synced {
        Ok(x) => x,
        Err(why) => {
            if let Some(st) = standing(&from_cwd).await {
                tell_move(news(&st.repo, st.reached, "failed", Some(&why)));
            }
            return Err(why);
        }
    };
    // Said now, before the machine it goes to says it went on there.
    if let Some(x) = &synced {
        tell_move(news(&x.mark.repo, x.mark.index, "left", None));
    }
    let out = match &synced {
        Some(x) => git_out(s, &from_cwd, &x.folder, &mv.to, &mv.cwd).await,
        None => Ok(GitOut::default()),
    };
    let out = match out {
        Ok(o) => o,
        Err(why) => {
            if let Some(x) = &synced {
                tell_move(news(&x.mark.repo, x.mark.index, "failed", Some(&why)));
            }
            return Err(why);
        }
    };
    let carried = carried_bytes(&out, home.as_ref());
    // Paused for a limit: it goes on there, where it moved for room, the cut-off turn first.
    let cut = picks_up_there(&mut state);
    let mut pkg = super::moving::pack(&s.id, spec, &mv.cwd, &me, &from_cwd, carry, events, state);
    if let Some(home) = home {
        pkg = pkg.with_home(home);
    }
    pkg.picks_up = mv.picks_up || cut;
    pkg.synced = synced.as_ref().map(|x| x.mark.clone());
    pkg.git = out.git;
    pkg.ignored = out.ignored;
    pkg.worktree = out.worktree;
    // Its inbox here fenced before its conversation goes: a copy of it this host goes on with
    // after a crash takes nothing for it (it may have gone on there), and is moved again
    // (CP-T3.2). Fenced before the ids of the messages it took are read, once what its lane holds
    // is done, so no id goes without its message (the hand-over review's point 2). What it holds
    // was copied above, after it stopped taking messages: none reached it since.
    fence(s.id.clone(), true).await.map_err(|e| format!("its inbox couldn't be fenced, so it stays here: {e}"))?;
    // The ids of the messages it took go with it (it takes none while it moves).
    // Never without them: a message sent again where it went would be taken there as new. It
    // stays here, and the move can be tried again.
    let kept = match ids(s.id.clone()).await {
        Ok(kept) => kept,
        Err(e) => return Err(unfenced(s, fence, format!("its inbox couldn't be read, so it stays here: {e}")).await),
    };
    (pkg.inbox, pkg.inbox_digests) = super::host::split_kept(kept);
    let bytes = match serde_json::to_vec(&pkg) {
        Ok(bytes) => bytes,
        Err(e) => return Err(unfenced(s, fence, e.to_string()).await),
    };
    moving_note(s, &format!("Moving to {}: handing over the conversation ({} KB)", mv.to, bytes.len() / 1024));
    let mut handed = reach.adopt(&mv.to, &bytes, false).await;
    // All of it went and no answer came: it may run there now. Going on here, it could take a
    // message it takes there too, so it doesn't: its inbox stays fenced, and it asks that runner
    // again until it says (it runs it: moved; it takes it now: moved; it refuses it: it stays).
    let mut pause = UNKNOWN_FIRST;
    while let Err(crate::reach::node::NotAdopted::Unknown(why)) = &handed {
        tracing::warn!(target: "kiln.sends", event = "move_unknown", session = %s.id, to = %mv.to, error = %why, "a session's move got no answer: it takes no messages here until the runner it went to says whether it took it over");
        moving_note(s, &format!("Moving to {}: no answer came ({why}), so it may run there now. It takes no messages here until {} says; asking again", mv.to, mv.to));
        crate::rt::sleep(pause).await;
        pause = (pause * 2).min(UNKNOWN_MOST);
        handed = reach.adopt(&mv.to, &bytes, true).await;
    }
    if handed.is_err()
        && let Err(e) = fence(s.id.clone(), false).await
    {
        tracing::warn!(target: "kiln.sends", event = "unfence_failed", session = %s.id, error = %e, "a session whose move failed couldn't take messages again: its inbox stays fenced until it's moved");
    }
    let handed = handed.map_err(|e| e.to_string());
    if let (Some(x), Err(e)) = (&synced, &handed) {
        tell_move(news(&x.mark.repo, x.mark.index, "failed", Some(e)));
    }
    if handed.is_ok() {
        for (what, n) in carried {
            if n > 0 {
                crate::metrics::sync().move_carried_bytes.add(&[what], n);
            }
        }
    }
    handed
}

/// How the synced copy a folder is in stands, if this machine syncs one.
async fn standing(cwd: &str) -> Option<crate::sync::runner::Standing> {
    crate::sync::runner::global()?.standing(std::path::Path::new(cwd)).await
}

/// Tells the server how this machine's part of a move went, for the repo's activity.
fn tell_move(news: crate::sync::wire::MoveNews) {
    if let Some(sync) = crate::sync::runner::global() {
        sync.tell(crate::sync::wire::Msg::Move(news));
    }
}

/// What a leaving session carries besides its synced files, in bytes: what the machine it goes to
/// lacks of its git history, the files git ignores, its worktree, and what its harness keeps for
/// it in its home.
fn carried_bytes(out: &GitOut, home: Option<&super::homes::Home>) -> [(&'static str, u64); 4] {
    // Base64, as the package carries them: three bytes in four.
    let b64 = |s: &str| u64::try_from(s.len() / 4 * 3).unwrap_or(u64::MAX);
    let files = |c: &super::carry::Carried| c.files.iter().map(|f| b64(&f.data)).sum::<u64>();
    [
        ("git", out.git.as_ref().and_then(|g| g.bundle.as_deref()).map_or(0, b64)),
        ("ignored", out.ignored.as_ref().map_or(0, files)),
        ("worktree", out.worktree.as_ref().map_or(0, |w| files(&w.changed).saturating_add(files(&w.ignored)))),
        ("home", home.map_or(0, super::homes::Home::size)),
    ]
}

/// The session's life: start the process, pump it, and after it exits wait to be restarted.
#[allow(clippy::too_many_arguments)]
async fn drive(s: Arc<Session>, kind: &'static Kind, ctl_rx: async_channel::Receiver<Ctl>, sink: Sink, on_change: Arc<dyn Fn() + Send + Sync>, files: Files, carried: Option<Vec<serde_json::Value>>, adopt: Option<Adopt>) {
    // The harness it runs in (a switch changes it); what it says is marked as its.
    let mut kind = kind;
    s.core.lock().harness = Some(kind.name.to_owned());
    let (pv_tx, pv_rx) = async_channel::unbounded::<(String, Vec<super::model::FileDiff>)>();
    let mut changes = futures::stream::FuturesUnordered::new();
    let (fx_tx, fx_rx) = async_channel::unbounded::<Done>();
    // Going on with the process the previous image of this process ran: its timeline came with it.
    let mut adopt = adopt;
    let handed = adopt.is_some();
    if handed {
        s.prompted.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    // Taking over a session: its transcript is read once it has ended (checked again before each
    // start while it has not).
    if !handed {
        let _ = takeover_ready(&s, &sink, TAKEOVER_WAIT).await;
    }
    // A resumed Claude Code session starts from its transcript (its history), not empty.
    if kind.protocol == super::kinds::Protocol::ClaudeStreamJson && !handed {
        let (resume, env, upto) = {
            let sp = s.spec.lock();
            (sp.resume.clone(), sp.env.clone(), sp.resume_at.clone())
        };
        let empty = s.core.lock().events().next().is_none();
        if let Some(r) = resume.filter(|_| empty) {
            let session = r.clone();
            match crate::rt::blocking(move || super::transcript::history(&env, &r)).await {
                Ok(Some(lines)) => {
                    let mut core = s.core.lock();
                    super::transcript::load_upto(&mut core, &lines, upto.as_deref());
                    tracing::info!(id = %s.id, %session, events = core.events().count(), "agent session history loaded from its transcript");
                }
                _ => tracing::info!(id = %s.id, %session, "no transcript found for the resumed agent session"),
            }
        }
    }
    // Announce it, whole, before anything else: whoever mirrors it starts from all of it. One
    // that started again from the conversation store announces all but its timeline: the store
    // has that, and the server's copy gets what it lacks as the session's stream starts.
    let stored = std::mem::take(&mut s.core.lock().stored);
    for u in s.snapshot() {
        if stored && matches!(u, Update::Reset { .. } | Update::Events { .. }) {
            continue;
        }
        let _ = sink.send((s.id.clone(), u)).await;
    }
    let mut first = !handed;
    let mut crashes: Vec<Instant> = Vec::new();
    // A message that arrived while the process was down, sent once it is back.
    let mut carry: Vec<Command> = Vec::new();
    // Moved here: on once its folder's last changes are (after a restart too: its state says so).
    let arriving = s.core.lock().state.arriving.clone();
    if let Some(mark) = arriving {
        let span = tracing::info_span!(target: "kiln.sync", "move.files", repo = tracing::field::Empty, session = %s.id, result = tracing::field::Empty);
        let arrived = files_in(&s, &sink, &mark, &ctl_rx, &mut carry, &files).instrument(span.clone()).await;
        span.record("result", if arrived == Arrived::GoOn { "ok" } else { "ended" });
        drop(span);
        if let Arrived::End { let_go } = arrived {
            if let_go {
                (files.release)(&s.id);
            } else {
                let _ = sink.send((s.id.clone(), Update::Closed)).await;
            }
            on_change();
            return;
        }
        {
            let mut core = s.core.lock();
            core.state.arriving = None;
            core.touch_state();
        }
        // Its files in: its repository as it was there.
        git_in(&s, &sink, &files).await;
    }
    // A rewind: the harness starts again, continuing its session from this point.
    let mut rewind_to: Option<Option<String>> = None;
    // A switch to another harness, waiting for the turn under way to end; and the conversation
    // carried over, for a harness that takes it once started (Codex), until it has.
    let mut switching: Option<Switch> = None;
    // A session that moved here with a new thread's history starts with it.
    let mut seed: Option<Vec<serde_json::Value>> = carried;
    // A move to another runner, waiting for the turn under way to end.
    let mut moving: Option<Move> = None;
    // A setting its harness takes when it starts (where its commands run) changed: it starts
    // again once nothing is under way.
    let mut reloading = false;
    // Tasks the harness left running that a switch stopped.
    let mut left = 0usize;
    // The models it last listed (the machine keeps the list for its "new agent" and switches).
    let mut models_seen: Vec<super::model::ModelOption> = Vec::new();
    loop {
        // The harness it ran in is down: on in the other one.
        // Its turn is over: on to the other runner (or, if that can't be, on here).
        if let Some(mv) = moving.take() {
            // From here on it takes no messages (they go where it goes); those it was sent and
            // hasn't taken go with it in its queue, and the rest it was told waits for the move.
            let held = gather(&s, &ctl_rx, &mut carry);
            let span = tracing::info_span!(target: "kiln.sync", "move", repo = tracing::field::Empty, machine = %mv.to, session = %s.id, result = tracing::field::Empty);
            let handed = hand_over(&s, kind, &mv, &files.carried, &files.fence, &files.settle).instrument(span.clone()).await;
            let result = if handed.is_ok() { "ok" } else { "failed" };
            span.record("result", result);
            drop(span);
            crate::metrics::sync().moves.inc(&[result]);
            match handed {
                Ok(()) => {
                    tracing::info!(session = %s.id, to = %mv.to, "agent session moved");
                    (files.release)(&s.id);
                    return;
                }
                Err(why) => {
                    tracing::warn!(session = %s.id, to = %mv.to, %why, "agent session could not move");
                    // It stays: it takes messages again, and what it was told meanwhile, in order.
                    *s.moving.lock() = false;
                    for c in held {
                        let _ = s.ctl.try_send(c);
                    }
                    move_failed(&s, &mv, &why);
                    flush(&s, &sink, None).await;
                    on_change();
                }
            }
            first = false;
        }
        // Started again for a setting (where its commands run): what waited in kiln's queue goes
        // to the new process.
        if std::mem::take(&mut reloading) {
            s.core.lock().hold = false;
            first = false;
        }
        if let Some(sw) = switching.take() {
            if let Some((to, carried)) = carry_over(&s, kind, sw, std::mem::take(&mut left), &sink).await {
                kind = to;
                seed = carried;
                models_seen.clear();
            }
            first = false;
            on_change();
        }
        let mut spec = if first { s.spec.lock().clone() } else { s.resume_spec() };
        if let Some(at) = rewind_to.take() {
            spec.resume_at = at;
        }
        // A start that takes a carried conversation is the harness's new session, not an old one.
        if seed.is_some() {
            spec.resume = None;
        }
        s.core.lock().harness = Some(kind.name.to_owned());
        // The process the previous image handed over, still running; else a new one.
        let (mut adapter, spawned, partial, adopted) = match adopt.take() {
            Some(a) => match crate::proc::adopt_duplex(a.process) {
                Ok(p) => {
                    tracing::info!(session = %s.id, pid = p.pid, "agent session went on with its process across the upgrade");
                    (a.adapter, Ok(p), a.partial, true)
                }
                Err(e) => {
                    // Its process can't be reached: stopped, and started again (its conversation
                    // goes on), so two of it never run.
                    tracing::warn!(session = %s.id, error = %format!("{e:#}"), "couldn't take over the agent's process after the upgrade: it starts again");
                    crate::proc::terminate_tree(i32::try_from(a.process.pid).unwrap_or(0), Duration::from_secs(2)).await;
                    continue;
                }
            },
            None => {
                let mut adapter = kind.adapter(&spec);
                adapter.offer_mcp(super::kinds::acp_servers(&spec, &s.id));
                if let Some(items) = &seed {
                    adapter.seed(items.clone());
                }
                let blocked = if spec.takeover_pid.is_some() { takeover_ready(&s, &sink, Duration::from_secs(3)).await } else { None };
                // One the machine's own PATH lacks may be on its user's: their login shell is asked
                // first (once, then kept ten minutes).
                if blocked.is_none() && spec.program.is_none() && !kind.available() {
                    crate::reach::serve::login_env().await;
                }
                let spawned = match blocked {
                    Some(why) => Err(anyhow::anyhow!(why)),
                    // In the session's group of processes where the machine has them, and marked as
                    // its, and so is everything it starts (`survivors`).
                    None => kind
                        .launch(&spec, &s.id)
                        .map(|mut p| {
                            p.env(super::survivors::MARK, &s.id);
                            p
                        })
                        .and_then(|p| crate::proc::spawn_duplex_in(p, crate::os::contain::Group::of(&s.id).as_ref())),
                };
                (adapter, spawned, Vec::new(), false)
            }
        };
        let started = Instant::now();
        let mut code = None;
        let mut error = None;
        match spawned {
            Err(e) => error = Some(format!("{e:#}")),
            Ok(mut p) => {
                *s.pid.lock() = Some(p.pid);
                if adopted {
                    let mut core = s.core.lock();
                    // An older image may have left what the harness before a switch ran looking live.
                    core.settle_foreign();
                    for c in carry.drain(..) {
                        adapter.command(&mut core, c);
                    }
                    drop(core);
                    if kind.protocol != super::kinds::Protocol::CodexAppServer {
                        read_unread_children(&s);
                    }
                } else {
                    let mut core = s.core.lock();
                    core.state.exit = None;
                    core.set_status(Status::Starting);
                    core.touch_state();
                    adapter.start(&mut core);
                    if !s.prompted.swap(true, std::sync::atomic::Ordering::Relaxed) && let Some(prompt) = spec.prompt.clone().filter(|p| !p.trim().is_empty()) {
                        // Its id is the session's own (CP-T3.2): made once, the same if it's ever made again.
                        let id = crate::util::derive_id(&["first-prompt", &s.id]);
                        adapter.command(&mut core, Command::Send { id, text: prompt, mode: Default::default(), attachments: Vec::new(), overrides: None, reply_to: None, task: None });
                    }
                    for c in carry.drain(..) {
                        adapter.command(&mut core, c);
                    }
                    drop(core);
                    if kind.protocol != super::kinds::Protocol::CodexAppServer {
                        read_unread_children(&s);
                    }
                }
                on_change();
                let mut jobs = flush(&s, &sink, Some(&p.stdin)).await;
                let mut buf: Vec<u8> = partial;
                let mut stderr_tail: Vec<u8> = Vec::new();
                let mut out_open = true;
                let mut err_open = true;
                let mut last_session = s.core.lock().state.harness_session.clone();
                let mut last_queue = 0usize;
                let mut last_pause = pause_marks(&s.core.lock().state);
                let mut next_tick = Instant::now() + TICK;
                // It is being ended for a switch (it is idle).
                let mut ending = false;
                // Stopped by force (`Command::ForceStop`): it starts again now, whatever is under way.
                let mut forcing = false;
                loop {
                    for job in jobs.previews.drain(..) {
                        let tx = pv_tx.clone();
                        crate::rt::spawn(async move {
                            let id = job.approval.clone();
                            let diffs = crate::rt::blocking(move || super::claude::preview(&job.tool, &job.input, &job.cwd)).await.unwrap_or_default();
                            let _ = tx.send((id, diffs)).await;
                        });
                    }
                    for job in jobs.changes.drain(..) {
                        // Placed among its folder's repository's working trees (a worktree it went into).
                        let home = {
                            let mut core = s.core.lock();
                            core.start_change(&job.abs);
                            core.state.cwd.clone()
                        };
                        changes.push(async move {
                            let abs = job.abs.clone();
                            let change = crate::rt::blocking(move || super::diff::placed_change(&job, &home)).await.unwrap_or((None, None));
                            (abs, change)
                        });
                    }
                    let ev = futures::select_biased! {
                        c = ctl_rx.recv().fuse() => Ev::Ctl(c.ok()),
                        c = changes.select_next_some() => Ev::Change(c),
                        o = pipe(&p.stdout, out_open).fuse() => Ev::Out(o),
                        e = pipe(&p.stderr, err_open).fuse() => Ev::Err(e),
                        d = pv_rx.recv().fuse() => Ev::Preview(d.ok()),
                        f = fx_rx.recv().fuse() => Ev::Fx(f.ok()),
                        x = (&mut p.exit).fuse() => Ev::Exit(x.ok().flatten()),
                        _ = crate::rt::sleep_until(next_tick).fuse() => Ev::Tick,
                        () = held_room(&s.core).fuse() => Ev::Room,
                    };
                    match ev {
                        Ev::Out(Some(chunk)) => {
                            buf.extend_from_slice(&chunk);
                            let mut core = s.core.lock();
                            while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                                let line: Vec<u8> = buf.drain(..=nl).collect();
                                adapter.line(&mut core, &String::from_utf8_lossy(&line));
                            }
                            // At its stop point in the middle of a turn: it stops at its next step.
                            if super::pause::stop_mid_turn(&mut core) {
                                adapter.command(&mut core, Command::Interrupt);
                            }
                            if buf.len() > 32 << 20 {
                                tracing::warn!(session = %s.id, "dropping an oversized line from the harness");
                                buf.clear();
                            }
                        }
                        Ev::Out(None) => {
                            out_open = false;
                            if !err_open {
                                // Both pipes closed: the exit status follows.
                                code = (&mut p.exit).await.ok().flatten();
                                break;
                            }
                        }
                        Ev::Err(Some(chunk)) => {
                            stderr_tail.extend_from_slice(&chunk);
                            let excess = stderr_tail.len().saturating_sub(8192);
                            stderr_tail.drain(..excess);
                        }
                        Ev::Err(None) => {
                            err_open = false;
                            if !out_open {
                                code = (&mut p.exit).await.ok().flatten();
                                break;
                            }
                        }
                        Ev::Ctl(Some(Ctl::Cmd(c))) => match *c {
                            Command::Restart => {}
                            Command::ForceStop => {
                                // You ended the turn: a restart before it's back doesn't take it up again.
                                s.turn_open.store(false, std::sync::atomic::Ordering::Relaxed);
                                let text = format!("Stopped by force: {} starts again, the conversation going on; what it was running stopped with it", kind.name);
                                s.core.lock().upsert(&format!("force-stop:{}", Core::now()), None, super::model::Body::Notice { level: Level::Info, text });
                                forcing = true;
                            }
                            Command::Snapshot => {
                                for u in s.snapshot() {
                                    let _ = sink.send((s.id.clone(), u)).await;
                                }
                            }
                            Command::Control { id, value } if id == "rename" => {
                                if let Some(c) = name_session(&s, value.as_ref()) {
                                    adapter.command(&mut s.core.lock(), c);
                                }
                                on_change();
                            }
                            // Where its commands run: its harness takes that when it starts, so it
                            // starts again (the conversation goes on) once nothing is under way.
                            Command::Control { id, value } if id == "reach" => {
                                if set_reach(&s, value.as_ref()) {
                                    let mut core = s.core.lock();
                                    let when = if settled_core(&core) { "now" } else { "once this turn ends" };
                                    let text = format!("{} — {} starts again with that {when}; the conversation goes on", reach_words(core.state.reach.as_deref()), kind.name);
                                    core.upsert(&format!("harness-switch:reach-{}", Core::now()), None, super::model::Body::Notice { level: Level::Info, text });
                                    core.hold = true;
                                    drop(core);
                                    reloading = true;
                                }
                                on_change();
                            }
                            Command::SetModel { model, kind: Some(to) } if to != kind.id => {
                                if let Some(sw) = wanted(&s, kind, &to, model) {
                                    switching = Some(sw);
                                }
                            }
                            Command::MoveTo { to, cwd, note, now, kind: into, model, local } => {
                                let mv = move_wanted(&s, to, cwd, note, now, into.as_deref().and_then(Kind::get).filter(|k| k.id != kind.id), model, local);
                                // Not waiting for the turn: it stops here, and goes on there.
                                if mv.picks_up {
                                    adapter.command(&mut s.core.lock(), Command::Interrupt);
                                }
                                moving = Some(mv);
                            }
                            Command::Moving { text } => moving_note(&s, &text),
                            // What it left running that kiln took back over: kiln stops it.
                            Command::StopTask { task } if task.starts_with(super::survivors::LEFT) => {
                                stop_left(&s, Some(&task)).await;
                                on_change();
                            }
                            Command::StopTasks => {
                                stop_left(&s, None).await;
                                adapter.command(&mut s.core.lock(), Command::StopTasks);
                                on_change();
                            }
                            // A pause (`harness::pause`): ended by a person, its timer set, its stop
                            // point set, or one started because another session on the account
                            // reached the limit.
                            Command::Resume => {
                                let mut core = s.core.lock();
                                // Going on regardless: the stop point leaves the windows past it be.
                                super::pause::waive_stop_point(&mut core);
                                if let Some(p) = super::pause::release(&mut core, "Resumed by you.") {
                                    adapter.go_on(&mut core, p);
                                }
                                drop(core);
                                on_change();
                            }
                            Command::AutoResume { on } => {
                                let mut core = s.core.lock();
                                core.state.manual_resume = !on;
                                if let Some(p) = core.state.pause.as_mut() {
                                    p.auto = on;
                                }
                                core.touch_state();
                                drop(core);
                                on_change();
                            }
                            Command::PauseAt { percent } => {
                                let mut core = s.core.lock();
                                core.state.pause_at = percent.filter(|p| (1..=100).contains(p));
                                core.touch_state();
                                drop(core);
                                on_change();
                            }
                            Command::MaxSubagents { count } => {
                                let mut core = s.core.lock();
                                core.state.max_subagents = count.filter(|n| *n >= 1);
                                core.touch_state();
                                drop(core);
                                on_change();
                            }
                            Command::MoveOnLimit { on, to, kind, model } => {
                                let mut core = s.core.lock();
                                core.state.move_on_limit = on;
                                core.state.move_plan = if on { super::model::MovePlan { to, kind, model } } else { super::model::MovePlan::default() };
                                core.touch_state();
                                drop(core);
                                on_change();
                            }
                            Command::LimitHold { window, until, said, account, .. } => {
                                let mut core = s.core.lock();
                                super::pause::account_hold(&mut core, super::pause::Hit { window, until, said }, account);
                                drop(core);
                                on_change();
                            }
                            c => {
                                // A model of its own harness: no switch after all.
                                if matches!(c, Command::SetModel { .. }) && switching.take().is_some() {
                                    let mut core = s.core.lock();
                                    core.hold = false;
                                    core.state.switching = None;
                                    core.touch_state();
                                    if let Some(e) = core.get_mut("switch-pending") {
                                        e.body = super::model::Body::Notice { level: Level::Info, text: "Switch cancelled".to_owned() };
                                    }
                                }
                                let c = prepare(&files, c).await;
                                let mut core = s.core.lock();
                                // Paused (or at its stop point): a message waits in the queue.
                                if let Some(c) = super::pause::let_through(&mut core, c) {
                                    adapter.command(&mut core, c);
                                }
                            }
                        },
                        Ev::Ctl(Some(Ctl::HandOver(reply))) => {
                            // This image of the process ends next (an in-place upgrade): the next
                            // one goes on with this very process. What it has said so far is taken
                            // in here, and nothing more is read, so nothing it says is lost.
                            let _ = flush(&s, &sink, Some(&p.stdin)).await;
                            // A switch, move or setting waiting for the turn to end is this image's
                            // alone: such a session starts again in the next one instead.
                            let waiting = switching.is_some() || moving.is_some() || reloading;
                            let detached = if waiting { None } else { p.detach().await };
                            let Some(process) = detached else {
                                // It can't be handed over: stopped, so the next image starts it again
                                // (its conversation goes on) without two of it running.
                                p.terminate(Duration::from_secs(2));
                                let _ = crate::rt::timeout(Duration::from_secs(5), &mut p.exit).await;
                                *s.pid.lock() = None;
                                let _ = reply.send(None);
                                return;
                            };
                            while let Ok(Some(chunk)) = crate::rt::timeout(Duration::from_secs(1), p.stdout.recv()).await.map(Result::ok) {
                                buf.extend_from_slice(&chunk);
                            }
                            let (updates, core) = {
                                let mut core = s.core.lock();
                                while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                                    let line: Vec<u8> = buf.drain(..=nl).collect();
                                    adapter.line(&mut core, &String::from_utf8_lossy(&line));
                                }
                                (core.take_out(), core.freeze())
                            };
                            for u in updates {
                                let _ = sink.send((s.id.clone(), u)).await;
                            }
                            *s.pid.lock() = None;
                            use base64::Engine as _;
                            let partial = base64::engine::general_purpose::STANDARD.encode(&buf);
                            let _ = reply.send(Some(Box::new(Handed { kind: kind.id.to_owned(), process, core, adapter: adapter.freeze(), partial })));
                            return;
                        }
                        Ev::Ctl(ctl @ (Some(Ctl::Close | Ctl::LetGo) | None)) => {
                            // Ending the agent ends what it left running: stopped through the
                            // harness first, then whatever is still under it (shells and
                            // terminals that left its process group included), then the harness.
                            // Letting go of it (it went on elsewhere) ends what ran here alike.
                            if s.core.lock().state.tasks.iter().any(super::model::Task::live) {
                                adapter.command(&mut s.core.lock(), Command::StopTasks);
                                let _ = flush(&s, &sink, Some(&p.stdin)).await;
                                crate::rt::sleep(Duration::from_millis(1500)).await;
                            }
                            let root = p.pid;
                            let left = std::mem::take(&mut *s.left.lock());
                            let group = crate::os::contain::Group::existing(&s.id);
                            let members = group.clone();
                            let _ = crate::rt::blocking(move || {
                                // Everything in its group where it has one (what left its tree
                                // too), else what's under it now.
                                let pids: Vec<u32> = match &members {
                                    Some(g) => g.pids().into_iter().filter(|p| *p != root).collect(),
                                    None => descendants(root).into_iter().map(|(pid, _)| pid).collect(),
                                };
                                crate::os::stop_processes(&pids, Some(root), Duration::from_secs(2));
                                for r in &left {
                                    super::survivors::stop_tree(r);
                                }
                            })
                            .await;
                            p.terminate(Duration::from_secs(3));
                            *s.pid.lock() = None;
                            if let Some(g) = group {
                                end_group(g, Duration::from_millis(3500));
                            }
                            if matches!(ctl, Some(Ctl::LetGo)) {
                                tracing::info!(session = %s.id, "agent session went on on another machine; this one lets go of it");
                                (files.release)(&s.id);
                            } else {
                                // Its last state, what an ended agent shows, says all of it stopped
                                // (the harness's own word on it was never read).
                                {
                                    let mut core = s.core.lock();
                                    adapter.exited(&mut core, None, None);
                                    end_tasks(&mut core);
                                }
                                let _ = flush(&s, &sink, None).await;
                                let _ = sink.send((s.id.clone(), Update::Closed)).await;
                            }
                            on_change();
                            return;
                        }
                        Ev::Preview(Some((id, diffs))) => {
                            let mut core = s.core.lock();
                            if let Some(a) = core.state.approvals.iter_mut().find(|a| a.id == id) {
                                a.diffs = diffs;
                                core.touch_state();
                            }
                        }
                        Ev::Preview(None) => {}
                        Ev::Change((abs, (change, resolved))) => {
                            s.core.lock().set_change(&abs, change, resolved);
                            while let Some(Some((abs, (change, resolved)))) = changes.next().now_or_never() {
                                s.core.lock().set_change(&abs, change, resolved);
                            }
                        }
                        Ev::Fx(Some(d)) => d.apply(&mut adapter, &mut s.core.lock()),
                        Ev::Fx(None) => {}
                        Ev::Tick => {}
                        Ev::Room => {
                            let mut core = s.core.lock();
                            adapter.drain(&mut core);
                        }
                        Ev::Exit(c) => {
                            code = c;
                            // Read what is left on stdout (the last lines often explain the exit).
                            while let Ok(Some(chunk)) = crate::rt::timeout(Duration::from_millis(300), p.stdout.recv()).await.map(Result::ok) {
                                buf.extend_from_slice(&chunk);
                            }
                            {
                                let mut core = s.core.lock();
                                while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                                    let line: Vec<u8> = buf.drain(..=nl).collect();
                                    adapter.line(&mut core, &String::from_utf8_lossy(&line));
                                }
                            }
                            break;
                        }
                    }
                    // Polls the harness only answers when asked: every few seconds, however busy.
                    if Instant::now() >= next_tick {
                        next_tick = Instant::now() + TICK;
                        let mut core = s.core.lock();
                        adapter.tick(&mut core);
                        // A step that ended without a word after it (limits that grew meanwhile).
                        if super::pause::stop_mid_turn(&mut core) {
                            adapter.command(&mut core, Command::Interrupt);
                        }
                    }
                    jobs = flush(&s, &sink, Some(&p.stdin)).await;
                    let listed = {
                        let core = s.core.lock();
                        (core.meta.models != models_seen).then(|| core.meta.models.clone())
                    };
                    if let Some(m) = listed {
                        super::kinds::note_models(kind.id, &m);
                        models_seen = m;
                    }
                    if let Some(at) = effects(&s, kind, &files, &fx_tx) {
                        rewind_to = Some(at);
                    }
                    if rewind_to.is_some() {
                        p.terminate(Duration::from_secs(2));
                        let _ = crate::rt::timeout(Duration::from_secs(5), &mut p.exit).await;
                        break;
                    }
                    if forcing {
                        p.terminate(Duration::from_secs(2));
                        let _ = crate::rt::timeout(Duration::from_secs(5), &mut p.exit).await;
                        ending = true;
                        break;
                    }
                    // A switch, now that nothing is under way: the harness ends by itself once its
                    // input closes (its record of the conversation complete), else it is stopped.
                    if (switching.is_some() || moving.is_some() || reloading) && settled(&s) {
                        left = s.core.lock().state.tasks.iter().filter(|t| t.left_running()).count();
                        p.stdin.close();
                        if crate::rt::timeout(Duration::from_secs(5), &mut p.exit).await.is_err() {
                            p.terminate(Duration::from_secs(2));
                            let _ = crate::rt::timeout(Duration::from_secs(5), &mut p.exit).await;
                        }
                        ending = true;
                        break;
                    }
                    let (session, queue, pause, status) = {
                        let core = s.core.lock();
                        (core.state.harness_session.clone(), core.state.queue.len(), pause_marks(&core.state), core.state.status)
                    };
                    let was = s.turn_open.load(std::sync::atomic::Ordering::Relaxed);
                    let open = turn_open(status, was);
                    s.turn_open.store(open, std::sync::atomic::Ordering::Relaxed);
                    if session != last_session || queue != last_queue || pause != last_pause || open != was {
                        last_session = session;
                        last_queue = queue;
                        last_pause = pause;
                        on_change();
                    }
                }
                *s.pid.lock() = None;
                if rewind_to.is_some() {
                    first = false;
                    continue;
                }
                if ending {
                    {
                        let mut core = s.core.lock();
                        adapter.exited(&mut core, None, None);
                    }
                    if adapter.seeded() {
                        seed = None;
                    }
                    first = false;
                    continue;
                }
                let tail = String::from_utf8_lossy(&stderr_tail).trim().to_owned();
                if !tail.is_empty() && code != Some(0) {
                    error = Some(super::model::clip_middle(&tail, 2000).0);
                }
            }
        }
        {
            let mut core = s.core.lock();
            adapter.exited(&mut core, code, error.clone());
        }
        flush(&s, &sink, None).await;
        on_change();
        first = false;
        if adapter.seeded() {
            seed = None;
        }
        // A switch, a move or a setting waiting for the turn: the turn is over (it ended with the
        // harness).
        if switching.is_some() || moving.is_some() || reloading {
            continue;
        }
        // A crash after a good run comes back by itself (a few times); a harness that will not
        // start waits for a person.
        let ran = started.elapsed();
        crashes.retain(|t| t.elapsed() < Duration::from_secs(600));
        let auto = code != Some(0) && error.is_none() && ran > Duration::from_secs(20) && crashes.len() < 3;
        if auto {
            crashes.push(Instant::now());
            crate::rt::sleep(Duration::from_secs(2)).await;
            continue;
        }
        // Down: a restart, or a message (which restarts it and is then delivered), brings it back.
        // Other commands still get answers (a skill's file, an MCP server added with the CLI).
        loop {
            let ev = futures::select_biased! {
                c = ctl_rx.recv().fuse() => Ev::Ctl(c.ok()),
                f = fx_rx.recv().fuse() => Ev::Fx(f.ok()),
            };
            match ev {
                Ev::Ctl(Some(Ctl::Cmd(c))) => match *c {
                    Command::Restart => break,
                    Command::Snapshot => {
                        for u in s.snapshot() {
                            let _ = sink.send((s.id.clone(), u)).await;
                        }
                    }
                    // Another harness's model: the session goes on in it, now.
                    Command::SetModel { model, kind: Some(to) } if to != kind.id => {
                        if let Some(sw) = wanted(&s, kind, &to, model) {
                            switching = Some(sw);
                            break;
                        }
                    }
                    // Another runner: it goes there, now.
                    Command::MoveTo { to, cwd, note, now, kind: into, model, local } => {
                        moving = Some(move_wanted(&s, to, cwd, note, now, into.as_deref().and_then(Kind::get).filter(|k| k.id != kind.id), model, local));
                        break;
                    }
                    Command::Moving { text } => moving_note(&s, &text),
                    Command::StopTask { task } if task.starts_with(super::survivors::LEFT) => {
                        stop_left(&s, Some(&task)).await;
                        on_change();
                    }
                    Command::StopTasks => {
                        stop_left(&s, None).await;
                        on_change();
                    }
                    // For a subagent: it ended with its harness, so it isn't started again for it.
                    c @ Command::Send { task: Some(_), .. } => {
                        adapter.command(&mut s.core.lock(), c);
                        on_change();
                    }
                    c @ (Command::Send { .. } | Command::SkillRun { .. }) => {
                        carry.push(prepare(&files, c).await);
                        break;
                    }
                    // Where its commands run, while it isn't running: from its next start.
                    Command::Control { id, value } if id == "reach" => {
                        if set_reach(&s, value.as_ref()) {
                            let mut core = s.core.lock();
                            let text = format!("{} from its next start", reach_words(core.state.reach.as_deref()));
                            core.upsert(&format!("harness-switch:reach-{}", Core::now()), None, super::model::Body::Notice { level: Level::Info, text });
                        }
                        on_change();
                    }
                    // Named while it isn't running: kiln's name now, the harness's when it next runs.
                    Command::Control { id, value } if id == "rename" => {
                        if let Some(c) = name_session(&s, value.as_ref()) {
                            carry.push(c);
                        }
                        on_change();
                    }
                    // Its threshold, set while it isn't running (a person's, or kiln's for a
                    // program's worker): kept, and it holds from its next start.
                    Command::PauseAt { percent } => {
                        let mut core = s.core.lock();
                        core.state.pause_at = percent.filter(|p| (1..=100).contains(p));
                        core.touch_state();
                        drop(core);
                        on_change();
                    }
                    c => adapter.command(&mut s.core.lock(), c),
                },
                // No process to hand over: the next image starts it from its record.
                Ev::Ctl(Some(Ctl::HandOver(reply))) => {
                    let _ = reply.send(None);
                }
                Ev::Ctl(ctl @ (Some(Ctl::Close | Ctl::LetGo) | None)) => {
                    // Ending the agent ends what it left running that kiln took back over, and the
                    // rest of its group of processes, which then goes.
                    let left = std::mem::take(&mut *s.left.lock());
                    if !left.is_empty() {
                        let _ = crate::rt::blocking(move || left.iter().for_each(super::survivors::stop_tree)).await;
                    }
                    if let Some(g) = crate::os::contain::Group::existing(&s.id) {
                        end_group(g, Duration::ZERO);
                    }
                    if matches!(ctl, Some(Ctl::LetGo)) {
                        tracing::info!(session = %s.id, "agent session went on on another machine; this one lets go of it");
                        (files.release)(&s.id);
                    } else {
                        end_tasks(&mut s.core.lock());
                        let _ = flush(&s, &sink, None).await;
                        let _ = sink.send((s.id.clone(), Update::Closed)).await;
                    }
                    on_change();
                    return;
                }
                Ev::Fx(Some(d)) => d.apply(&mut adapter, &mut s.core.lock()),
                _ => {}
            }
            flush(&s, &sink, None).await;
            if let Some(at) = effects(&s, kind, &files, &fx_tx) {
                rewind_to = Some(at);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// A session that runs nothing, and the receiving end of its channel (what its task would read).
    fn idle(id: &str) -> (Session, async_channel::Receiver<Ctl>) {
        let (ctl, rx) = async_channel::unbounded();
        let s = Session {
            id: id.into(),
            spec: Mutex::new(Spec::default()),
            created_at: 0,
            core: Mutex::new(Core::new()),
            info: Mutex::new(Info::default()),
            ctl,
            pid: Mutex::new(None),
            turn_open: std::sync::atomic::AtomicBool::new(false),
            ran: Mutex::new((None, Vec::new())),
            left: Mutex::new(Vec::new()),
            moving: Mutex::new(false),
            prompted: std::sync::atomic::AtomicBool::new(false),
        };
        (s, rx)
    }

    fn message(id: &str, text: &str) -> Command {
        Command::Send { id: id.into(), text: text.into(), mode: super::super::model::SendMode::Now, attachments: Vec::new(), overrides: None, reply_to: None, task: None }
    }

    #[test]
    fn a_first_message_its_harness_never_got_stays_in_what_resumes_it_until_it_goes() {
        let (s, _rx) = idle("never-started");
        *s.spec.lock() = Spec { kind: "claude".into(), prompt: Some("fix the flaky test".into()), ..Spec::default() };
        assert_eq!(s.resume_spec().prompt.as_deref(), Some("fix the flaky test"));
        s.prompted.store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(s.resume_spec().prompt, None);
    }

    /// CP-T3.2: a move gathers what a session was sent and hasn't taken (carried while its harness
    /// was down, then still in its channel) into its queue, in the order it came, to go with it; it
    /// holds back everything else; and from then on the session takes no message (its inbox takes
    /// it back, and the sender is told it moved), while commands still go in.
    #[test]
    fn a_move_takes_what_its_session_was_sent_and_lets_no_message_in_after() {
        let (s, rx) = idle("moving");
        s.core.lock().state.queue.push(super::super::model::QueueItem { id: "q0".into(), text: "queued before".into(), at: 1, attachments: Vec::new(), overrides: None, reply_to: None });
        let mut carry = vec![message("c1", "sent while it was down"), Command::Restart];
        assert!(s.command(message("m1", "in its channel")));
        assert!(s.command(Command::Interrupt));
        assert!(s.command(Command::SkillRun { name: "review".into(), args: Some("the diff".into()), id: Some("k1".into()), mode: Default::default() }));
        let held = gather(&s, &rx, &mut carry);
        let queue: Vec<(String, String)> = s.core.lock().state.queue.iter().map(|q| (q.id.clone(), q.text.clone())).collect();
        assert_eq!(
            queue,
            [("q0", "queued before"), ("c1", "sent while it was down"), ("m1", "in its channel"), ("k1", "/review the diff")].map(|(i, t)| (i.to_owned(), t.to_owned())),
            "in the order they came, after what was queued"
        );
        assert!(matches!(held.as_slice(), [Ctl::Cmd(c)] if matches!(**c, Command::Interrupt)), "the rest held back");
        assert!(matches!(carry.as_slice(), [Command::Restart]), "what isn't a message stays carried");
        assert!(s.is_moving());
        assert!(!s.command(message("m2", "too late")), "no message goes in once it's moving");
        assert!(s.command(Command::Interrupt), "a command still does");
    }

    /// CP-T3.2 (the nineteenth adversarial check's): a move gathers a message for one of its
    /// subagents into that subagent's queue, which goes with the session, never into the agent's
    /// own; one for a subagent that takes none now is shown not sent, there.
    #[test]
    fn a_move_keeps_a_message_for_a_subagent_its_subagent_s() {
        use super::super::model::{Delivery, SendMode, Task};
        let (s, rx) = idle("moving-tasks");
        s.core.lock().state.tasks = vec![
            Task { id: "t1".into(), class: Some("agent".into()), status: "running".into(), takes: vec![SendMode::Queue], ..Task::default() },
            Task { id: "t2".into(), class: Some("agent".into()), status: "completed".into(), ..Task::default() },
        ];
        let to = |id: &str, task: &str| Command::Send { id: id.into(), text: format!("for {task}"), mode: SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: Some(task.into()) };
        let mut carry = vec![to("c1", "t1")];
        assert!(s.command(to("m1", "t2")));
        assert!(s.command(message("m2", "for the agent")));
        let _held = gather(&s, &rx, &mut carry);
        let core = s.core.lock();
        let own: Vec<&str> = core.state.queue.iter().map(|q| q.id.as_str()).collect();
        assert_eq!(own, ["m2"], "the agent's own queue holds its own message alone");
        let t1: Vec<&str> = core.state.tasks.iter().find(|t| t.id == "t1").map(|t| t.queue.iter().map(|q| q.id.as_str()).collect()).unwrap_or_default();
        assert_eq!(t1, ["c1"], "the subagent's message waits in its queue, which goes with the session");
        assert!(matches!(core.state.delivery.get("m1"), Some(Delivery::Cancelled)), "one for a subagent that takes none is shown not sent: {:?}", core.state.delivery.get("m1"));
    }

    /// A switch carries the same latitude (never more) and the nearest effort the new model takes,
    /// named by the new harness's own choices: what its page shows while the switch waits, rather
    /// than the old harness's (Opus shown with Codex's Full access, an empty effort, Codex's limit).
    /// CP-T3.2 (the merge with main's one runner a machine, and the eighteenth adversarial check's):
    /// a session handed to its machine's newer runner process carries its inbox's ids, as a move
    /// does, its inbox here fenced before it goes: the newer process keeps an inbox of its own, which
    /// would take a message sent again as new. Refused, the fence lifts and it goes on here. With no
    /// word of it, the newer process may run it: it waits, fenced, and its package goes again until
    /// it's known; once a try went without a word, only the newer process's own word ends it (the
    /// server's own refusal doesn't). Its ids unreadable, it doesn't go at all. And (the nineteenth
    /// adversarial check's) where the machine keeps its record of its hand-overs, that says how it
    /// went, whatever the word: a refusal that's an earlier try's, come late while a newer process
    /// took this one, lets go of it; a refusal takes the try back, unless one took it first; with no
    /// word, it looks, and goes on waiting while it isn't decided, or while the record can't be read.
    #[test]
    fn a_session_handed_to_its_machine_s_newer_runner_takes_its_inbox_s_ids_and_waits_to_know() -> anyhow::Result<()> {
        use super::super::handing::NotHanded;
        use super::super::host::Decided;
        let kind = Kind::get("claude").ok_or_else(|| anyhow::anyhow!("no claude kind"))?;
        let mv = Move { to: "newer".into(), cwd: String::new(), note: None, picks_up: false, kind: None, model: None, local: true };
        let kept: CarriedIds = Arc::new(|_| Box::pin(async { Ok(vec![("m1".to_owned(), "d1".to_owned()), ("m2".to_owned(), "d2".to_owned())]) }));
        let unreadable: CarriedIds = Arc::new(|_| Box::pin(async { Err("its records are gone".to_owned()) }));
        let unknown = || -> Result<(), NotHanded> { Err(NotHanded::Unknown("the link dropped".into())) };
        let not_passed = || -> Result<(), NotHanded> { Err(NotHanded::NotPassed("no newer process".into())) };
        let refused = || -> Result<(), NotHanded> { Err(NotHanded::Refused("no room".into())) };
        let none = Vec::<Result<Option<Decided>, String>>::new;
        crate::rt::block_on(async move {
            // (its ids, each try's word, what the machine's record says each time it's asked, how
            // it's fenced, whether it went, how many tries, whether each ask takes the try back)
            for (ids, words, settled, fenced, ok, tries, asks) in [
                (&kept, vec![Ok(())], none(), vec![true], true, 1, vec![]),
                (&kept, vec![refused()], none(), vec![true, false], false, 1, vec![true]),
                (&kept, vec![not_passed()], none(), vec![true, false], false, 1, vec![true]),
                (&kept, vec![unknown(), unknown(), Ok(())], none(), vec![true], true, 3, vec![false, false]),
                (&kept, vec![unknown(), not_passed(), refused()], none(), vec![true, false], false, 3, vec![false, false, true]),
                // Its ids unreadable: fenced first, then the fence lifted, and it stays.
                (&unreadable, vec![Ok(())], none(), vec![true, false], false, 0, vec![]),
                // A refusal that's an earlier try's, come late: the record says it was taken over.
                (&kept, vec![refused()], vec![Ok(Some(Decided::Taken))], vec![true], true, 1, vec![true]),
                (&kept, vec![not_passed()], vec![Ok(Some(Decided::Taken))], vec![true], true, 1, vec![true]),
                // Taken back: decided not taken.
                (&kept, vec![refused()], vec![Ok(Some(Decided::Refused))], vec![true, false], false, 1, vec![true]),
                // No word: it looks, until it's decided.
                (&kept, vec![unknown(), unknown()], vec![Ok(None), Ok(Some(Decided::Taken))], vec![true], true, 2, vec![false, false]),
                (&kept, vec![unknown(), refused()], vec![Ok(None), Ok(Some(Decided::Refused))], vec![true, false], false, 2, vec![false, true]),
                // The record unreadable: a refusal isn't taken at its word; it asks again.
                (&kept, vec![refused(), Ok(())], vec![Err("a disk fault".to_owned())], vec![true], true, 2, vec![true]),
                (&kept, vec![unknown(), refused(), refused()], vec![Ok(None), Err("a disk fault".to_owned()), Ok(Some(Decided::Refused))], vec![true, false], false, 3, vec![false, true, true]),
            ] {
                let (s, _rx) = idle("h1");
                let s = Arc::new(s);
                let calls = Arc::new(Mutex::new(Vec::new()));
                let c = Arc::clone(&calls);
                let fence: FenceFor = Arc::new(move |_, on| {
                    c.lock().push(on);
                    Box::pin(async { Ok(()) })
                });
                let words = Arc::new(Mutex::new(std::collections::VecDeque::from(words)));
                let packages = Arc::new(Mutex::new(Vec::new()));
                let (w, got) = (Arc::clone(&words), Arc::clone(&packages));
                let send = move |bytes: Vec<u8>| {
                    got.lock().push(bytes);
                    let word = w.lock().pop_front().unwrap_or(Ok(()));
                    async move { word }
                };
                let record = Arc::new(Mutex::new(std::collections::VecDeque::from(settled)));
                let asked = Arc::new(Mutex::new(Vec::new()));
                let (r, a) = (Arc::clone(&record), Arc::clone(&asked));
                let settle: SettleFor = Arc::new(move |attempt, _, take_back| {
                    a.lock().push((attempt, take_back));
                    let said = r.lock().pop_front().unwrap_or(Ok(None));
                    Box::pin(async move { said })
                });
                let went = hand_to_newer_by(&s, kind, &mv, ids, &fence, &settle, send, |_| async {}).await;
                let sent = std::mem::take(&mut *packages.lock());
                let asked = std::mem::take(&mut *asked.lock());
                let takes: Vec<bool> = asked.iter().map(|(_, t)| *t).collect();
                anyhow::ensure!(went.is_ok() == ok && *calls.lock() == fenced && sent.len() == tries && takes == asks, "{went:?}, fenced {:?}, {} tries, asked {takes:?}", calls.lock(), sent.len());
                let mut attempts = std::collections::HashSet::new();
                for bytes in sent {
                    let pkg: serde_json::Value = serde_json::from_slice(&bytes)?;
                    anyhow::ensure!(pkg.get("inbox") == Some(&serde_json::json!(["m1", "m2"])) && pkg.get("inbox_digests") == Some(&serde_json::json!(["d1", "d2"])) && pkg.get("handover") == Some(&serde_json::json!(true)), "{pkg}");
                    attempts.insert(pkg.get("attempt").and_then(serde_json::Value::as_str).map(str::to_owned));
                }
                // Each try the same one, and the one the record is asked of.
                attempts.extend(asked.into_iter().map(|(a, _)| Some(a)));
                anyhow::ensure!(attempts.len() <= 1 && !attempts.contains(&None), "the tries' attempts: {attempts:?}");
            }
            Ok(())
        })
    }

    /// The hand-over review's point 2: a session handed to its machine's newer runner process fences
    /// its inbox, then reads the ids that go with it, then copies what it holds. What it holds once
    /// its ids are read goes, with every id, so no id goes without its message.
    #[test]
    fn a_hand_over_fences_its_inbox_then_reads_its_ids_then_copies_what_it_holds() -> anyhow::Result<()> {
        let kind = Kind::get("claude").ok_or_else(|| anyhow::anyhow!("no claude kind"))?;
        let mv = Move { to: "newer".into(), cwd: String::new(), note: None, picks_up: false, kind: None, model: None, local: true };
        crate::rt::block_on(async move {
            let (s, _rx) = idle("h1");
            let s = Arc::new(s);
            let log = Arc::new(Mutex::new(Vec::<String>::new()));
            let l = Arc::clone(&log);
            let fence: FenceFor = Arc::new(move |_, on| {
                l.lock().push(format!("fence {on}"));
                Box::pin(async { Ok(()) })
            });
            // As its ids are read, what it holds is as its lane left it: m1 in its queue.
            let (l, held) = (Arc::clone(&log), Arc::clone(&s));
            let ids: CarriedIds = Arc::new(move |_| {
                l.lock().push("ids".into());
                held.core.lock().state.queue.push(super::super::model::QueueItem { id: "m1".into(), text: "then test it".into(), at: 1, attachments: Vec::new(), overrides: None, reply_to: None });
                Box::pin(async { Ok(vec![("m1".to_owned(), "d1".to_owned())]) })
            });
            let settle: SettleFor = Arc::new(|_, _, _| Box::pin(async { Ok(None) }));
            let sent = Arc::new(Mutex::new(Vec::new()));
            let got = Arc::clone(&sent);
            let send = move |bytes: Vec<u8>| {
                got.lock().push(bytes);
                async { Ok(()) }
            };
            hand_to_newer_by(&s, kind, &mv, &ids, &fence, &settle, send, |_| async {}).await.map_err(|e| anyhow::anyhow!(e))?;
            anyhow::ensure!(*log.lock() == ["fence true", "ids"], "{:?}", log.lock());
            let bytes = sent.lock().first().cloned().ok_or_else(|| anyhow::anyhow!("nothing went"))?;
            let pkg: serde_json::Value = serde_json::from_slice(&bytes)?;
            anyhow::ensure!(pkg.pointer("/state/queue/0/id") == Some(&serde_json::json!("m1")) && pkg.get("inbox") == Some(&serde_json::json!(["m1"])), "{pkg}");
            Ok(())
        })
    }

    #[test]
    fn a_switch_carries_the_same_latitude_and_the_nearest_effort_named_by_the_new_harness() -> anyhow::Result<()> {
        use super::{Kind, carried_choices};
        let codex = Kind::get("codex").ok_or_else(|| anyhow::anyhow!("no codex"))?;
        let claude = Kind::get("claude").ok_or_else(|| anyhow::anyhow!("no claude"))?;
        let (mode, effort, modes, efforts) = carried_choices(codex, claude, "full-access", "ultra", true, "opus");
        // Claude offers asking nothing only where Claude Code allows it (not as root outside a
        // sandbox, as CI's containers run): else the most it may.
        let top = if modes.iter().any(|m| m.id == "bypassPermissions") { "bypassPermissions" } else { "auto" };
        assert_eq!((mode.as_deref(), effort.as_deref()), (Some(top), Some("max")));
        assert!(modes.iter().all(|m| m.id != "bypassPermissions" || m.label == "Bypass"), "{modes:?}");
        assert!(efforts.iter().any(|e| e.id == "max"), "{efforts:?}");
        // Without leave to ask nothing: the most it may. Codex's auto: Claude's accept-edits.
        assert_eq!(carried_choices(codex, claude, "full-access", "", false, "opus").0.as_deref(), Some("auto"));
        assert_eq!(carried_choices(codex, claude, "auto", "high", true, "sonnet").0.as_deref(), Some("acceptEdits"));
        // And back: Claude's max is the highest Codex's model takes.
        let (_, effort, _, efforts) = carried_choices(claude, codex, "acceptEdits", "max", true, "gpt-6.1-sol");
        let top = ["max", "xhigh", "high"].into_iter().find(|l| efforts.iter().any(|e| e.id == *l));
        assert_eq!(effort.as_deref(), top, "{efforts:?}");
        Ok(())
    }

    #[test]
    fn a_turn_stays_open_until_it_ends_not_when_its_harness_does() {
        use super::turn_open;
        assert!(turn_open(Status::Running, false));
        assert!(turn_open(Status::Waiting, false));
        // Its harness ends under it (a restart, a crash) and starts again: still open.
        assert!(turn_open(Status::Exited, true));
        assert!(turn_open(Status::Starting, true));
        assert!(turn_open(Status::Disconnected, true));
        // It ended: closed, and the harness ending after that doesn't open it.
        assert!(!turn_open(Status::Idle, true));
        assert!(!turn_open(Status::Exited, false));
    }

    #[test]
    fn resolve_cwd_expands_home() {
        let home = crate::os::home_dir();
        assert_eq!(super::resolve_cwd(None), home.to_string_lossy());
        assert_eq!(super::resolve_cwd(Some("~")), home.to_string_lossy());
        assert_eq!(super::resolve_cwd(Some("  ")), home.to_string_lossy());
        assert_eq!(super::resolve_cwd(Some("~/Projects/x")), home.join("Projects/x").to_string_lossy());
        assert_eq!(super::resolve_cwd(Some("/srv/repo")), "/srv/repo");
    }
    use super::*;

    /// A session that went on in Claude Code with a Codex subagent kiln never read: its work comes
    /// from Codex's own record of its thread, off the event loop, into the running session.
    #[cfg(unix)]
    #[test]
    fn a_codex_child_never_read_is_read_from_its_rollout_in_the_harness_after_a_switch() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let quiet = dir.path().join("quiet-claude");
            std::fs::write(&quiet, "#!/bin/sh\nexec sleep 30\n")?;
            std::fs::set_permissions(&quiet, std::fs::Permissions::from_mode(0o755))?;
            let thread = "01a15b9d-5652-7722-8f9b-c750ebfe6940";
            let day = dir.path().join("codex/sessions/2026/10/20");
            std::fs::create_dir_all(&day)?;
            let ev = |payload: serde_json::Value| serde_json::json!({"type": "event_msg", "payload": payload}).to_string();
            let rollout = [
                serde_json::json!({"type": "session_meta", "payload": {"id": thread}}).to_string(),
                ev(serde_json::json!({"type": "task_started", "turn_id": "own", "turn_attribution": {"initiating_agent_path": "/root"}, "started_at": 100})),
                ev(serde_json::json!({"type": "item_completed", "turn_id": "own", "item": {"type": "AgentMessage", "id": "msg_answer", "content": [{"type": "Text", "text": "Fits."}]}})),
                ev(serde_json::json!({"type": "task_complete", "turn_id": "own", "completed_at": 103, "duration_ms": 3000})),
            ];
            std::fs::write(day.join(format!("rollout-2026-10-20T16-20-00-{thread}.jsonl")), rollout.join("\n"))?;
            let (tx, _rx) = async_channel::unbounded();
            let host = super::super::host::Host::new(tx, None, super::super::host::Limits::default());
            let home = dir.path().join("codex").to_string_lossy().into_owned();
            let spec = Spec {
                kind: "claude".into(),
                cwd: Some(dir.path().to_string_lossy().into_owned()),
                program: Some(quiet.to_string_lossy().into_owned()),
                env: vec![("CODEX_HOME".into(), home)],
                ..Spec::default()
            };
            let s = host.open(Some("switched".into()), spec)?;
            let started = crate::rt::timeout(Duration::from_secs(10), async {
                while s.core.lock().meta.features.is_empty() {
                    crate::rt::sleep(Duration::from_millis(50)).await;
                }
            })
            .await;
            anyhow::ensure!(started.is_ok(), "the harness never started");
            s.core.lock().state.tasks.push(Task { id: thread.into(), tool_id: Some("call_spawn".into()), class: Some("agent".into()), status: "completed".into(), ..Task::default() });
            read_unread_children(&s);
            let read = crate::rt::timeout(Duration::from_secs(10), async {
                while s.core.lock().get(&format!("child:{thread}:msg_answer")).is_none() {
                    crate::rt::sleep(Duration::from_millis(50)).await;
                }
            })
            .await;
            anyhow::ensure!(read.is_ok(), "its work never came");
            let core = s.core.lock();
            assert_eq!(core.get(&format!("child:{thread}:msg_answer")).and_then(|e| e.parent.clone()).as_deref(), Some("call_spawn"));
            assert_eq!(core.state.tasks.iter().find(|t| t.id == thread).and_then(|t| t.summary.clone()).as_deref(), Some("Fits."));
            drop(core);
            host.close("switched");
            Ok(())
        })
    }

    #[test]
    fn an_ended_agent_shows_nothing_still_running() {
        let row = |id: &str, status: &str| Task { id: id.into(), tool_id: Some(format!("call-{id}")), class: Some("shell".into()), status: status.into(), ..Task::default() };
        let mut core = Core::new();
        core.state.tasks = vec![row("terminal-dev", "running"), row("left-4242", "running"), Task { ended: Some(7), ..row("terminal-done", "completed") }];
        core.upsert("call-terminal-dev", None, super::super::model::Body::Assistant { text: "Serving".into(), streaming: true });
        end_tasks(&mut core);
        assert_eq!(core.state.tasks.iter().map(|t| (t.status.as_str(), t.ended.is_some())).collect::<Vec<_>>(), [("stopped", true), ("stopped", true), ("completed", true)]);
        assert_eq!(core.state.tasks.last().and_then(|t| t.ended), Some(7));
        assert!(core.get("call-terminal-dev").is_some_and(|e| matches!(&e.body, super::super::model::Body::Assistant { streaming: false, .. })));
        assert!(core.take_out().iter().any(|u| matches!(u, Update::State { state } if state.tasks.iter().all(|t| !t.live()))));
    }

    #[test]
    fn info_lists_native_subagents_and_keeps_their_finished_work() -> anyhow::Result<()> {
        let native = |id: &str, kind: &str, status: &str| Task {
            id: id.into(),
            tool_id: Some(format!("spawn-{id}")),
            description: format!("Review {id}"),
            kind: kind.into(),
            class: Some("agent".into()),
            status: status.into(),
            started: 10,
            ..Task::default()
        };
        let mut state = State {
            tasks: vec![
                native("claude-child", "general-purpose", "running"),
                Task {
                    parent_task: Some("claude-child".into()),
                    ended: Some(30),
                    prompt: Some("The full task prompt".into()),
                    summary: Some("A long result".into()),
                    output: Some("/tmp/private-output".into()),
                    ..native("codex-thread", "reviewer", "completed")
                },
                native("failed-child", "", "failed"),
                native("stopped-child", "", "stopped"),
                Task {
                    class: None,
                    ..native("legacy-child", "Explore", "paused")
                },
            ],
            ..State::default()
        };
        for class in ["shell", "monitor", "workflow", "mcp"] {
            state.tasks.push(Task {
                class: Some(class.into()),
                ..native(class, "", "running")
            });
        }
        for kind in [
            "local_bash",
            "monitor",
            "monitor_background",
            "local_workflow",
            "mcp_task",
        ] {
            state.tasks.push(Task {
                class: None,
                ..native(kind, kind, "running")
            });
        }
        let mut info = Info::default();
        info.apply(&state);
        assert_eq!(
            info.subagents
                .iter()
                .map(|s| (s.id.as_str(), s.status.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("claude-child", "running"),
                ("codex-thread", "completed"),
                ("failed-child", "failed"),
                ("stopped-child", "stopped"),
                ("legacy-child", "paused")
            ],
        );
        let encoded = serde_json::to_value(&info.subagents)?;
        assert_eq!(
            encoded.get(1),
            Some(&serde_json::json!({
                "id": "codex-thread", "tool_id": "spawn-codex-thread", "description": "Review codex-thread",
                "status": "completed", "parent_task": "claude-child", "started": 10, "ended": 30,
            })),
            "list entries identify native work without copying prompts, results or output",
        );
        assert!(
            info.parent.is_none(),
            "native tasks are not kiln session lineage"
        );
        Ok(())
    }

    #[test]
    fn info_subagents_update_from_old_runner_state_and_notify_lists() -> anyhow::Result<()> {
        // A list entry from before subagent briefs still loads; the runner's state fills them.
        let old = serde_json::to_value(Info::default())?;
        assert!(
            old.get("subagents").is_none(),
            "empty briefs stay absent on the wire"
        );
        let mut info: Info = serde_json::from_value(old)?;
        assert!(info.subagents.is_empty());
        let mut state = State {
            tasks: vec![Task {
                id: "child".into(),
                description: "é".repeat(400),
                kind: "Explore".into(),
                status: "pending".into(),
                ..Task::default()
            }],
            ..State::default()
        };
        info.apply(&state);
        assert_eq!(info.subagents.len(), 1);
        assert!(
            info.subagents.iter().all(|s| s.description.len() < 256),
            "list labels stay bounded and valid UTF-8"
        );
        let pending = info.clone();
        for task in &mut state.tasks {
            task.status = "completed".into();
            task.ended = Some(50);
        }
        info.apply(&state);
        assert!(
            info.differs(&pending),
            "finishing a native subagent refreshes Mission Control"
        );
        let finished = info.clone();
        for task in &mut state.tasks {
            task.description = "Renamed native task".into();
        }
        info.apply(&state);
        assert!(
            info.differs(&finished),
            "renaming a native subagent refreshes Mission Control"
        );
        state.tasks.clear();
        info.apply(&state);
        assert!(
            info.subagents.is_empty(),
            "clearing task history clears the disclosure too"
        );
        Ok(())
    }

    #[test]
    fn info_follows_state() {
        let mut i = Info {
            id: "a".into(),
            kind: "claude".into(),
            kind_name: "Claude Code".into(),
            target: "local".into(),
            name: "x".into(),
            shell: None,
            cwd: "/w".into(),
            tree: None,
            created_at: 1,
            status: Status::Starting,
            title: String::new(),
            model: String::new(),
            mode: String::new(),
            harness_session: None,
            approvals: vec![],
            queued: 0,
            queue: Vec::new(),
            said: Vec::new(),
            tasks_running: 0,
            subagents: Vec::new(),
            cost_usd: 0.0,
            activity: None,
            last_activity: 1,
            exit: None,
            goal: String::new(),
            objective: None,
            summary: String::new(),
            doing: None,
            epoch: 0,
            todos_done: 0,
            todos_total: 0,
            files_changed: 0,
            lines_added: 0,
            lines_removed: 0,
            context_pct: None,
            turn_started: None,
            limits: vec![],
            account: None,
            plan: None,
            left_behind: 0,
            reach: None,
            pause: None,
            last_turn: None,
            keep_off: false,
            held: None,
            refused: None,
            move_on_limit: false,
            move_plan: Default::default(),
            pause_at: None,
            max_subagents: None,
            parent: None,
            plan_run: None,
            helper: None,
            subagent: false,
            no_bypass: false,
            workspace: None,
        };
        let before = i.clone();
        let mut st = State { status: Status::Running, mode: "plan".into(), ..State::default() };
        st.tasks.push(super::super::model::Task { id: "t".into(), status: "running".into(), ..Default::default() });
        i.apply(&st);
        assert_eq!((i.status, i.mode.as_str(), i.tasks_running, i.cwd.as_str()), (Status::Running, "plan", 1, "/w"));
        assert!(i.differs(&before));
        let mut later = i.clone();
        later.last_activity = 99;
        assert!(!later.differs(&i));

        // A name given to it reaches lists through its state; none given leaves the one it had.
        let named = i.clone();
        i.apply(&State { name: "Docs fixer".into(), ..State::default() });
        assert_eq!(i.name, "Docs fixer");
        assert!(i.differs(&named));
        i.apply(&State::default());
        assert_eq!(i.name, "Docs fixer");

        // Where its commands run reaches lists through its state too, and reads as said.
        i.apply(&State { reach: Some("off".into()), ..State::default() });
        assert_eq!(i.reach.as_deref(), Some("off"));
        i.apply(&State::default());
        assert_eq!(i.reach, None, "absent: anywhere it needs");
        assert_eq!(reach_words(None), "Its commands can run on any runner it needs");
        assert_eq!(reach_words(Some("off")), "Its commands run only on its own machine");
        assert_eq!(reach_words(Some("linux/amd64")), "Its commands can run on other runners, linux/amd64 first");
    }

    #[test]
    fn only_loopback_is_fetched() {
        assert_eq!(loopback_target("http://localhost:41234/callback?code=a&state=b"), Some(("localhost".into(), 41234, "/callback?code=a&state=b".into())));
        assert_eq!(loopback_target("http://127.0.0.1:9/?x=1#frag"), Some(("127.0.0.1".into(), 9, "/?x=1".into())));
        assert_eq!(loopback_target("http://[::1]:7/cb"), Some(("::1".into(), 7, "/cb".into())));
        assert_eq!(loopback_target("http://LOCALHOST?code=1"), Some(("localhost".into(), 80, "/?code=1".into())));
        for bad in ["https://localhost/cb", "http://example.com/cb", "http://localhost.example.com/cb", "http://user@localhost/cb", "http://localhost:1/a b", "http://localhost:99999/", "file:///etc/passwd", "http://10.0.0.1/"] {
            assert_eq!(loopback_target(bad), None, "{bad}");
        }
        assert_eq!(fetch_loopback("http://example.com/").0, None);
    }

    #[test]
    fn a_callback_on_loopback_is_fetched() -> anyhow::Result<()> {
        use std::io::{BufRead as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let server = std::thread::spawn(move || -> Option<String> {
            let (conn, _) = listener.accept().ok()?;
            let mut reader = std::io::BufReader::new(conn.try_clone().ok()?);
            let mut first = String::new();
            reader.read_line(&mut first).ok()?;
            let mut conn = conn;
            conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nsigned in").ok()?;
            Some(first)
        });
        let (code, body) = fetch_loopback(&format!("http://127.0.0.1:{port}/callback?code=abc&state=s"));
        assert_eq!((code, body.as_str()), (Some(200), "signed in"));
        let first = server.join().map_err(|_| anyhow::anyhow!("server thread"))?.unwrap_or_default();
        assert_eq!(first.trim_end(), "GET /callback?code=abc&state=s HTTP/1.1");
        Ok(())
    }

    #[test]
    fn reads_are_markdown_files_only() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join(format!("kiln-read-{}", crate::util::new_id()));
        std::fs::create_dir_all(dir.join("skill"))?;
        let md = dir.join("skill").join("SKILL.md");
        std::fs::write(&md, "# hi")?;
        let secret = dir.join("secret.txt");
        std::fs::write(&secret, "no")?;
        let p = |x: &std::path::Path| x.to_string_lossy().into_owned();
        let missing = dir.join("missing").join("SKILL.md");
        assert_eq!(read_first(&[p(&missing), p(&md)]).get("content").and_then(serde_json::Value::as_str), Some("# hi"));
        assert!(read_first(&[p(&secret)]).get("error").is_some());
        assert!(read_first(&["relative/SKILL.md".to_owned()]).get("error").is_some());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn a_star_matches_the_newest_directory_first() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join(format!("kiln-expand-{}", crate::util::new_id()));
        for (bucket, text) in [("old", "old"), ("new", "new")] {
            std::fs::create_dir_all(dir.join("synced").join(bucket).join("docs"))?;
            std::fs::write(dir.join("synced").join(bucket).join("docs").join("SKILL.md"), text)?;
            std::thread::sleep(Duration::from_millis(20));
        }
        let pattern = dir.join("synced").join("*").join("docs").join("SKILL.md").to_string_lossy().into_owned();
        assert_eq!(read_first(&[pattern]).get("content").and_then(serde_json::Value::as_str), Some("new"));
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn a_takeover_waits_for_the_process_to_end() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let mut short = std::process::Command::new("sleep").arg("0.3").spawn()?;
            let pid = short.id();
            // Reaped as soon as it exits (as the taken-over session's parent would).
            let reaper = std::thread::spawn(move || short.wait());
            assert!(gone(pid, Duration::from_secs(5)).await);
            let _ = reaper.join();
            let mut long = std::process::Command::new("sleep").arg("30").spawn()?;
            let t = Instant::now();
            assert!(!gone(long.id(), Duration::from_millis(400)).await);
            assert!(t.elapsed() < Duration::from_secs(3));
            let _ = long.kill();
            let _ = long.wait();
            anyhow::Ok(())
        })
    }

    #[test]
    fn native_subagent_output_keeps_results_after_history_eviction_and_bounds_snapshots() {
        use super::super::model::Body;
        let mut core = Core::new();
        let mut task = Task { id: "child".into(), tool_id: Some("spawn".into()), class: Some("agent".into()), status: "completed".into(), ..Task::default() };
        // An empty completed child is unavailable, not a successful empty file read.
        assert!(codex_task_output(&core, &task).get("error").is_some());
        task.summary = Some("The completed result remains available.".into());
        let result = codex_task_output(&core, &task);
        assert_eq!(result.get("data").and_then(serde_json::Value::as_str), task.summary.as_deref());
        assert_eq!(result.get("live").and_then(serde_json::Value::as_bool), Some(false));
        // Newest output is bounded; the final answer is present once even when Task.summary
        // contains its clipped representation.
        let long = format!("{}Final result.", "x".repeat(TASK_TAIL as usize + 20));
        task.summary = Some(super::super::model::clip_middle(&long, super::super::model::OUTPUT_MAX).0);
        core.upsert("child:child:answer", Some("spawn".into()), Body::Assistant { text: long.clone(), streaming: false });
        let result = codex_task_output(&core, &task);
        let text = result.get("data").and_then(serde_json::Value::as_str).unwrap_or_default();
        assert_eq!(text.len(), TASK_TAIL as usize);
        assert!(text.ends_with("Final result."));
        assert_eq!(text.matches("Final result.").count(), 1);
        assert_eq!(result.get("offset").and_then(serde_json::Value::as_u64), Some(long.len() as u64));
        assert_eq!(result.get("truncated").and_then(serde_json::Value::as_bool), Some(true));
    }

    #[test]
    fn task_output_files_and_their_tails() -> anyhow::Result<()> {
        let env = vec![("CLAUDE_CODE_TMPDIR".to_owned(), "/tmp/cc".to_owned())];
        assert_eq!(claude_task_file(&env, "/home/u/my app", Some("s1"), "b7").as_deref(), Some("/tmp/cc/-home-u-my-app/s1/tasks/b7.output"));
        assert!(claude_task_file(&env, "/w", Some("s1"), "../x").is_none());
        assert!(claude_task_file(&env, "/w", None, "b7").is_none());
        let dir = std::env::temp_dir().join(format!("kiln-tail-{}", crate::util::new_id())).join("tasks");
        std::fs::create_dir_all(&dir)?;
        let f = dir.join("b7.output");
        std::fs::write(&f, "one\ntwo\n")?;
        assert_eq!(read_tail(&f, None), Some(("one\ntwo\n".to_owned(), 8, false)));
        assert_eq!(read_tail(&f, Some(4)), Some(("two\n".to_owned(), 8, false)));
        // Shorter than asked for (written again from the start): from the start.
        assert_eq!(read_tail(&f, Some(100)), Some(("one\ntwo\n".to_owned(), 8, false)));
        std::fs::remove_dir_all(dir.parent().unwrap_or(&dir))?;
        Ok(())
    }
}
