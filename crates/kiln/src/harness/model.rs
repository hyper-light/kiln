//! The normalized model every harness adapter produces and the UI renders: a timeline of events
//! (prompts, replies, thinking, tool calls with real diffs, notices), the live state of the session
//! (status, mode, pending approvals, kiln's message queue, subagent tasks, usage) and the commands
//! a person can give it. Adapters translate their harness's own protocol into this; nothing here is
//! specific to one harness.

use serde::{Deserialize, Serialize};

/// Longest tool output kept per call (the head and the tail survive; the middle is elided).
pub const OUTPUT_MAX: usize = 24 * 1024;
/// Longest text kept per message.
pub const TEXT_MAX: usize = 256 * 1024;

/// One item of the timeline. Upserted by `id`: a tool call is first sent running, then again
/// with its result; a streaming reply grows until it is final.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub id: String,
    /// Order of first appearance in the session.
    pub seq: u64,
    /// Unix milliseconds.
    pub at: i64,
    /// The subagent run (its spawning tool call's id) this event belongs to; None for the main thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// The harness that was running when it appeared ("Claude Code", "Codex"): a conversation
    /// switched between harnesses keeps who said what.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(flatten)]
    pub body: Body,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Body {
    /// Something the person (or a peer, or the system) said to the agent.
    User {
        text: String,
        #[serde(default, skip_serializing_if = "is_zero")]
        images: u32,
        /// The id kiln gave the message when it sent it (matches a queue item / delivery state).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
        /// "human", "system" (task notifications, reminders) ...
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<String>,
        /// Names of the files attached to it (images are counted in `images` too).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<String>,
    },
    Assistant {
        text: String,
        #[serde(default, skip_serializing_if = "is_false")]
        streaming: bool,
    },
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "is_false")]
        streaming: bool,
        /// The model thought, but only a signature was kept (the text is not available).
        #[serde(default, skip_serializing_if = "is_false")]
        redacted: bool,
    },
    Tool(Box<ToolCall>),
    /// Interruptions, compactions, errors, retries, hook output.
    Notice {
        level: Level,
        text: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Info,
    Warn,
    Error,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// What kind of thing a tool does, so the UI can render it properly whatever the harness named it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// Runs a shell command.
    Shell,
    /// Changes a file (edit, write, patch, notebook).
    Edit,
    Read,
    /// Searches files or names (grep, glob, ls).
    Search,
    Web,
    /// Starts a subagent.
    Agent,
    Skill,
    Todo,
    /// Asks the person something (multiple choice).
    Question,
    /// Presents a plan for approval.
    Plan,
    Mcp,
    Other,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// Proposed, not yet started (or waiting for approval).
    Pending,
    Running,
    Done,
    Error,
    Denied,
    /// The turn was interrupted before it finished.
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub tool_id: String,
    /// The harness's own name for the tool (Bash, Edit, mcp__github__search ...).
    pub name: String,
    pub tool: ToolKind,
    /// One line: the command, the path, the pattern, the question.
    pub title: String,
    /// A second line: the command's description, a line range, the search root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The input as the harness gave it (long strings shortened).
    pub input: serde_json::Value,
    pub status: ToolStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// File changes, as unified hunks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diffs: Vec<FileDiff>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub todos: Vec<Todo>,
    /// The subagent task this call started (for Agent/Task tools).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// Exit code for shell commands, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub started: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<i64>,
    /// For an MCP tool: its server, and the tool's own name (`mcp__github__search` → "github",
    /// "search").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_short: Option<String>,
    /// For a skill run: the skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
}

impl ToolCall {
    /// Fill in what the harness's naming says: an MCP tool's server and own name, a skill's name.
    pub fn annotate(&mut self) {
        if self.server.is_none()
            && let Some((server, tool)) = self.name.strip_prefix("mcp__").and_then(|rest| rest.split_once("__"))
            && !server.is_empty()
        {
            self.server = Some(server.to_owned());
            self.tool_short = Some(tool.to_owned());
            if self.tool == ToolKind::Other {
                self.tool = ToolKind::Mcp;
            }
        }
        if self.skill.is_none() && (self.tool == ToolKind::Skill || self.name == "Skill") {
            self.skill = ["skill", "command", "name"]
                .iter()
                .find_map(|k| self.input.get(*k).and_then(serde_json::Value::as_str))
                .map(|s| s.trim_start_matches('/').to_owned())
                .filter(|s| !s.is_empty());
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    /// The file did not exist before.
    #[serde(default, skip_serializing_if = "is_false")]
    pub created: bool,
    pub hunks: Vec<Hunk>,
}

/// One unified-diff hunk; `lines` carry their ' ', '-' or '+' prefix.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub lines: Vec<String>,
}

/// One file's cumulative change over the whole session: from how it was before the agent first
/// touched it to how it is now.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileChange {
    /// Relative to the session's directory when inside it.
    pub path: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub created: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub deleted: bool,
    pub added: u32,
    pub removed: u32,
    pub hunks: Vec<Hunk>,
    /// Too large (or binary) to show; counts only.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// Unix ms of the last change.
    pub at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Todo {
    pub text: String,
    /// "pending", "in_progress", "completed".
    pub status: String,
}

/// Where a session is in its life.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Starting,
    /// Waiting for the person.
    Idle,
    /// Working on a turn.
    Running,
    /// Waiting for an approval or an answer.
    Waiting,
    /// The harness process ended (it can be restarted from where it was).
    Exited,
    /// Its machine is not connected right now (the agent keeps running there).
    Disconnected,
}

/// A decision the agent is waiting for: a tool call to approve, questions to answer, a plan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Approval {
    /// The harness's request id (answers go back under it).
    pub id: String,
    pub tool_id: Option<String>,
    pub tool_name: String,
    pub tool: ToolKind,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub input: serde_json::Value,
    /// For shell tools: the exact command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub cwd: String,
    /// For file changes: what would change, computed against the file as it is now.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diffs: Vec<FileDiff>,
    /// Why the harness is asking (its own words).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Ways to allow it from now on, as the harness offers them ("always allow npm test in this
    /// project", "accept edits for this session").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub always: Vec<AlwaysOption>,
    /// Do not pre-select approve (dangerous command).
    #[serde(default, skip_serializing_if = "is_false")]
    pub default_to_no: bool,
    /// Questions to answer (AskUserQuestion-style tools).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<Question>,
    /// A plan to approve (plan-mode exits).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Which subagent is asking, if not the main thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AlwaysOption {
    pub label: String,
    /// Where the rule is kept: "session", "project (local)", "project", "user".
    pub scope: String,
    /// The harness's own permission update, sent back verbatim when chosen.
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Question {
    /// The harness's own id for it (Codex), when it has one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub header: String,
    #[serde(default)]
    pub multi: bool,
    /// A free-text answer ("Other") is accepted.
    #[serde(default)]
    pub other: bool,
    /// The answer is a secret: masked as it is typed.
    #[serde(default)]
    pub secret: bool,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct QuestionOption {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: String,
    /// What to show while it is focused (Claude Code: a mockup, code, a comparison).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub preview: String,
}

/// An approval's answers, one per question: `choices`, or the older map (question → its answer as
/// one string) read as one choice each.
pub fn choices_of(answers: Option<std::collections::BTreeMap<String, String>>, choices: Option<Vec<ChoiceAnswer>>) -> Option<Vec<ChoiceAnswer>> {
    choices.or_else(|| answers.map(|m| m.into_iter().map(|(question, a)| ChoiceAnswer { question, selected: vec![a], ..ChoiceAnswer::default() }).collect()))
}

/// The answer to one question (`Command::Approve::choices`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ChoiceAnswer {
    /// The question's text; `id` its id when it has one.
    #[serde(default)]
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The labels of the options chosen.
    #[serde(default)]
    pub selected: Vec<String>,
    /// A free-text answer ("Other").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<String>,
    /// A note with the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A message kiln holds until the agent's current turn ends.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueueItem {
    pub id: String,
    pub text: String,
    pub at: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrides: Option<Overrides>,
}

/// What the timeline shows of a message's attachments: (images, file names).
pub fn attachment_summary(list: &[Attachment]) -> (u32, Vec<String>) {
    let images = u32::try_from(list.iter().filter(|a| a.is_image()).count()).unwrap_or(u32::MAX);
    (images, list.iter().map(|a| a.name.clone()).collect())
}

/// How a message should reach a busy agent.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SendMode {
    /// Wait in kiln's queue until the current turn ends.
    #[default]
    Queue,
    /// Into the running turn without stopping it (the agent sees it at its next step).
    Steer,
    /// Stop the current turn and send at once.
    Now,
}

/// Where a sent message is: written to the agent, accepted into its queue, being worked on,
/// finished, or dropped.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Sent,
    Queued,
    Started,
    Completed,
    Cancelled,
}

/// A subagent or background task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Task {
    pub id: String,
    /// The tool call that started it (its events carry this as `parent`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_id: Option<String>,
    pub description: String,
    /// "general-purpose", "Explore", "shell", ...
    #[serde(default)]
    pub kind: String,
    /// "running", "completed", "failed", "stopped", "killed", "paused", "pending".
    pub status: String,
    #[serde(default)]
    pub background: bool,
    #[serde(default)]
    pub depth: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tool: Option<String>,
    #[serde(default)]
    pub tokens: u64,
    #[serde(default)]
    pub tool_uses: u64,
    #[serde(default)]
    pub duration_ms: u64,
    pub started: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<i64>,
    /// What it is: "shell" (a command left running), "monitor" (a watch whose events wake the
    /// agent), "agent" (a subagent), "workflow", "mcp" (an MCP call moved to the background).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    /// The command it runs (shells, monitors) and where.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Where its output is written on its machine (read through the task output endpoint).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// The harness's own handle for its process (Codex).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// A monitor's events so far, and the latest one (one line).
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub events: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event: Option<String>,
    /// Who a monitor's events wake: "agent" (the session), or the subagent task that started it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wakes: Option<String>,
    /// kiln can stop it (`stop_task`).
    #[serde(default)]
    pub stoppable: bool,
}

fn is_zero_u64(n: &u64) -> bool {
    *n == 0
}

impl Task {
    /// Still running (or paused): not finished in any way.
    pub fn live(&self) -> bool {
        matches!(self.status.as_str(), "running" | "pending" | "paused")
    }

    /// Something it leaves running on its own: a shell, a monitor, a workflow, a background call
    /// or a subagent sent to the background.
    pub fn left_running(&self) -> bool {
        self.live() && (self.background || matches!(self.class.as_deref(), Some("shell" | "monitor" | "workflow" | "mcp")))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Usage {
    pub cost_usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub turns: u64,
    /// Tokens in the context window right now, and its size, when the harness says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
}

/// One window of the plan limits of the account a session runs on (what the harness's own
/// /usage shows): how much of it is used and when it starts over.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Limit {
    /// Stable for the harness: "five_hour", "seven_day", "seven_day_opus", "codex.primary", ...
    pub id: String,
    /// "Session (5h)", "Weekly", "Weekly · Opus", ...
    pub label: String,
    /// How much of it is used, 0–100 (None: not a percentage, see `used` and `limit`).
    pub used_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    /// What `used` and `limit` count ("credits", "USD").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Unix ms the window starts over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
    /// The window's length in short words: "5h", "7d", "month".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// The account it belongs to (as `State::account`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// "allowed", "warning" (close to the limit) or "rejected" (reached), when the harness says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// Whether the harness has said what it has: an empty list reads as "none" only once it did.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Known {
    /// Not reported yet (the harness is starting, or has not answered).
    #[default]
    Loading,
    /// Reported: an empty list means there are none.
    Ready,
    /// The harness has no way to report it.
    Unsupported,
}

/// What is known so far of each of the session's inventories (`State::mcp`, `State::skills`,
/// `State::plugins`, `State::limits`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Inventory {
    #[serde(default)]
    pub mcp: Known,
    #[serde(default)]
    pub skills: Known,
    #[serde(default)]
    pub plugins: Known,
    #[serde(default)]
    pub limits: Known,
}

/// One line of the harness's status (its own /status), for the inspector.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StatusItem {
    pub group: String,
    /// Empty for a line that is a sentence on its own.
    #[serde(default)]
    pub label: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tone: Option<Tone>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    Ok,
    Warn,
    Error,
}

/// An MCP server the session can use, as the harness reports it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpServer {
    pub name: String,
    /// Where it is configured: "user", "project", "local", "config", "plugin", "dynamic", ...
    pub scope: String,
    /// "stdio", "http", "sse", "ws", ...
    pub transport: String,
    pub status: McpStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub enabled: bool,
    #[serde(default)]
    pub tools: Vec<McpTool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<u32>,
    /// Its command line (never its environment) or its URL (never credentials or the query).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// What the server calls itself, with its version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// How kiln's harness is signed in to it, where the harness says: "oauth", "bearer",
    /// "signed out".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpStatus {
    Connected,
    Pending,
    Failed,
    NeedsAuth,
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpTool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A skill the harness can use.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Skill {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// "user", "project", "plugin", "builtin", "system", "admin", ...
    pub source: String,
    /// Its SKILL.md on the machine, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_hint: Option<String>,
    /// A person can run it (`skill_run`); otherwise only the model uses it.
    pub user_invocable: bool,
    /// Switched on (one that is off is listed, not offered to the model).
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

/// A plugin the harness has installed (read-only).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Plugin {
    pub name: String,
    #[serde(default)]
    pub source: String,
    pub enabled: bool,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// A permission mode the harness offers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModeOption {
    pub id: String,
    pub label: String,
    pub description: String,
    /// Needs an explicit warning before switching (skips all checks).
    #[serde(default, skip_serializing_if = "is_false")]
    pub dangerous: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelOption {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    /// The effort levels it takes, lowest first, when the harness says.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<String>,
    /// The harness's default model: what a session started without one runs.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub default: bool,
}

/// An effort level as people read it: "xhigh" is "Extra high", the rest as they are, capitalized.
pub fn effort_label(id: &str) -> String {
    match id {
        "xhigh" => "Extra high".to_owned(),
        _ => {
            let mut c = id.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
        }
    }
}

/// One entry of a harness's command catalog (`Meta::commands`): every slash command it has, and
/// what running it from kiln does (`route`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SlashCommand {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// The placeholder for its words ("<model>", "[instructions]").
    #[serde(default)]
    pub hint: String,
    /// "Session", "Model & mode", "Context", "Tools & MCP", "Skills", "Plugins" or "Kiln".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub group: String,
    /// Whether it takes words after its name: "none", "optional" or "required".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub args: String,
    /// Other names it answers to (never another entry's name).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub route: Route,
}

/// What running a catalog command does.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Route {
    /// Sent as a message: `text` when given (with "{args}" replaced by the words typed after the
    /// name), else "/name words". A skill (`skill`) runs with `skill_run`.
    Prompt {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        skill: bool,
    },
    /// Sends this kiln `Command` (JSON), with "{args}" and "{client}" in its strings replaced (the
    /// words typed after the name; the asking client). One that answers replies to that client
    /// with `{title, text, format?, filename?}` ("text" or "markdown").
    Control { command: serde_json::Value },
    /// Opens a kiln panel: "model", "permissions", "settings", "mcp", "skills", "plugins",
    /// "hooks", "tasks", "changes", "usage", "status", "context", "files", "new_agent".
    View { view: String },
    /// Not available from kiln, and why.
    Unsupported { reason: String },
    /// Done in the harness's own terminal interface: kiln opens it beside the agent, in a terminal
    /// in the agent's folder on its machine, on `command` (typed by the user when the harness
    /// can't start on one). The agent starts again once it closes, to load what was changed.
    Terminal { command: String },
}

impl Default for Route {
    fn default() -> Self {
        Self::Prompt { text: None, skill: false }
    }
}

impl SlashCommand {
    pub fn new(name: &str, description: &str, hint: &str, group: &str, route: Route) -> Self {
        let args = if hint.starts_with('<') {
            "required"
        } else if hint.is_empty() {
            "none"
        } else {
            "optional"
        };
        Self { name: name.to_owned(), description: description.to_owned(), hint: hint.to_owned(), group: group.to_owned(), args: args.to_owned(), aliases: Vec::new(), route }
    }

    /// The same, answering to other names too.
    pub fn aka(mut self, aliases: &[&str]) -> Self {
        self.aliases = aliases.iter().map(|a| (*a).to_owned()).collect();
        self
    }

    /// Run by the harness itself: sent as "/name words".
    pub fn prompt(name: &str, description: &str, hint: &str, group: &str) -> Self {
        Self::new(name, description, hint, group, Route::default())
    }

    /// A kiln command (`command` as JSON, "{args}" and "{client}" filled in when sent).
    pub fn control(name: &str, description: &str, hint: &str, group: &str, command: serde_json::Value) -> Self {
        Self::new(name, description, hint, group, Route::Control { command })
    }

    pub fn view(name: &str, description: &str, group: &str, view: &str) -> Self {
        Self::new(name, description, "", group, Route::View { view: view.to_owned() })
    }

    pub fn unsupported(name: &str, description: &str, group: &str, reason: &str) -> Self {
        Self::new(name, description, "", group, Route::Unsupported { reason: reason.to_owned() })
    }

    /// One done in the harness's own screen, opened beside the agent on `/name`.
    pub fn terminal(name: &str, description: &str, group: &str) -> Self {
        Self::new(name, description, "", group, Route::Terminal { command: format!("/{name}") })
    }
}

/// The order groups are shown in.
pub fn group_rank(group: &str) -> usize {
    ["Session", "Model & mode", "Context", "Tools & MCP", "Skills", "Plugins", "Kiln"].iter().position(|g| *g == group).unwrap_or(7)
}

/// A catalog in group order, then by name, one entry per name (the first kept), and no alias
/// that is another entry's name (or an alias an earlier entry has).
pub fn sort_catalog(mut c: Vec<SlashCommand>) -> Vec<SlashCommand> {
    let mut seen = std::collections::HashSet::new();
    c.retain(|e| seen.insert(e.name.clone()));
    for e in &mut c {
        e.aliases.retain(|a| seen.insert(a.clone()));
    }
    c.sort_by(|a, b| group_rank(&a.group).cmp(&group_rank(&b.group)).then_with(|| a.name.cmp(&b.name)));
    c
}

/// A control the harness offers (a setting or an action), described so the UI can render it
/// without knowing the harness. Live values are in [`State::control_values`]; changes and
/// actions come back as [`Command::Control`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Control {
    pub id: String,
    /// Where it belongs: "Model", "Permissions", "Thinking", "Subagents", "Context", "MCP",
    /// "Session", "Output", "Hooks".
    pub group: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    pub kind: ControlKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<ControlOption>,
    /// The initial value (the live one is in `State::control_values`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    /// Ask before doing it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub dangerous: bool,
    /// Can also be set for a single message (`Command::Send::overrides`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub per_turn: bool,
    /// Only while the session has this feature (see `Meta::features`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires: Option<String>,
    /// "session": a session setting or action. "turn": an action on one prompt (rewind to it,
    /// branch from it): sent with `value: {"event": <the prompt's event id>}`.
    #[serde(default)]
    pub scope: ControlScope,
    /// An action that takes text (compact instructions, ...): its placeholder. Absent: no text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ControlKind {
    Select,
    Toggle,
    Action,
    Text,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ControlScope {
    #[default]
    Session,
    Turn,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ControlOption {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub dangerous: bool,
}

/// A file sent with a message: inline (`data_base64`), or uploaded first
/// (`POST /api/harness/files`, then `file_id`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Attachment {
    pub name: String,
    pub mime: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub data_base64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
    /// Where it was written on the agent's machine (set there; never taken from a client).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl Attachment {
    pub fn is_image(&self) -> bool {
        matches!(self.mime.as_str(), "image/png" | "image/jpeg" | "image/gif" | "image/webp")
    }
}

/// Settings for one message only, where the harness can.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Overrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// A goal the harness itself pursues across turns (Claude Code's /goal, Codex's thread goal).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Objective {
    /// The condition or objective, as given.
    pub text: String,
    /// "active", "met", "unmet" (it gave up, or it can't be met: `reason`), "paused", "cleared".
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Unix ms it last changed.
    #[serde(default)]
    pub at: i64,
}

impl Objective {
    pub fn new(text: &str, status: &str, reason: Option<String>) -> Self {
        Self { text: text.trim().to_owned(), status: status.to_owned(), reason, at: super::core::Core::now() }
    }
}

/// Everything about a session that is not the timeline. Small; sent whole on change.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct State {
    pub status: Status,
    /// The harness's own session id (what `--resume` takes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_session: Option<String>,
    /// How many times the session went on on another machine (a move, a resume elsewhere): its
    /// conversation's writer. A store takes a session's rows from its latest writer alone
    /// (`docs/conversations.md`, "Fencing").
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub epoch: u64,
    /// Where its commands may run besides its machine (`Spec::reach`): None anywhere it needs,
    /// `"off"` only its own machine, else a runner or platform it prefers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reach: Option<String>,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub title: String,
    /// The session's name as a person last gave it (empty: the one it started with).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// What the person first asked for (the session's goal), shortened.
    #[serde(default)]
    pub goal: String,
    /// The goal the harness itself pursues across turns (Claude Code's /goal, Codex's thread
    /// goal), and how it stands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective: Option<Objective>,
    /// Unix ms the current turn started (None when idle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_started: Option<i64>,
    /// The agent's latest words (main thread), shortened: what it last said it did or found.
    #[serde(default)]
    pub summary: String,
    /// What it is doing right now, in plain words ("Running cargo test", "Editing src/main.rs").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doing: Option<String>,
    /// The changeset so far, in numbers (the files themselves come as a `changes` update).
    #[serde(default)]
    pub files_changed: u32,
    #[serde(default)]
    pub lines_added: u32,
    #[serde(default)]
    pub lines_removed: u32,
    #[serde(default)]
    pub approvals: Vec<Approval>,
    #[serde(default)]
    pub queue: Vec<QueueItem>,
    /// Delivery state of every message kiln sent this session, by client id.
    #[serde(default)]
    pub delivery: std::collections::BTreeMap<String, Delivery>,
    #[serde(default)]
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub todos: Vec<Todo>,
    #[serde(default)]
    pub usage: Usage,
    /// What the harness is doing right now, in a few words ("requesting", "compacting").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<ExitInfo>,
    /// Events dropped from the front of the timeline to bound memory.
    #[serde(default)]
    pub dropped: u64,
    /// The conversation was cleared (`/clear`): the number of the timeline's first event since.
    /// The events before it stay history (the store keeps them and search finds them), and a
    /// timeline taken up again (a restart, a move) leaves them out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleared: Option<u64>,
    /// The effort / reasoning level in use, where the harness has one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub effort: String,
    /// The live value of each control (`Meta::controls`), by id.
    #[serde(default)]
    pub control_values: std::collections::BTreeMap<String, serde_json::Value>,
    /// The account it runs on, as "<harness>:<plan>[:<short hash of the account>]" (never the
    /// address itself), so one account's limits show once across sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// The plan, in words ("Max", "Pro", "API key").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// The account's plan limits, window by window, when the harness reports them.
    #[serde(default)]
    pub limits: Vec<Limit>,
    /// The harness's own status lines (version, account, model, servers, ...).
    #[serde(default)]
    pub harness_status: Vec<StatusItem>,
    /// Its MCP servers, live.
    #[serde(default)]
    pub mcp: Vec<McpServer>,
    /// Its skills (and the commands it treats as skills).
    #[serde(default)]
    pub skills: Vec<Skill>,
    /// Its plugins, read-only.
    #[serde(default)]
    pub plugins: Vec<Plugin>,
    /// Which of those lists the harness has reported yet (loading / ready / unsupported).
    #[serde(default)]
    pub inventory: Inventory,
    /// Tasks left running while the agent is idle or exited (shells, monitors, background
    /// work): `Task::left_running` ones, counted when no turn is under way.
    #[serde(default)]
    pub left_behind: u32,
    /// A switch to another harness's model, waiting for the turn under way to end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switching: Option<Switching>,
    /// Nothing more goes to the harness until this ends: its account reached a usage limit
    /// (`harness::pause`). The queue, goals and plan steps wait; a message sent meanwhile joins
    /// the queue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause: Option<Pause>,
    /// Stop between turns once any of the account's limits is this full (percent), before the
    /// limit cuts a turn off. None: only when a limit is reached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_at: Option<u8>,
    /// A pause ends only when a person resumes it, not by itself at the reset.
    #[serde(default, skip_serializing_if = "is_false")]
    pub manual_resume: bool,
    /// At a usage limit (or its stop point) it goes on on another machine whose harness is signed
    /// in to an account with room, its conversation and repository going along: opted in to, a
    /// conversation at a time (the server sees to it, `harness::elsewhere`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub move_on_limit: bool,
    /// Windows a person resumed it in past its stop point (their `Limit::id`, and when they start
    /// over): the stop point leaves them be until then (`pause::waive_stop_point`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_waived: Vec<(String, i64)>,
    /// How its last turn ended (`Core::end_turn`): what kiln's keep-going reads (`goals::keep`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn: Option<TurnEnd>,
    /// Turns in a row kiln started to keep it going (`Core::note_sent`), since a person last said
    /// something.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub kept: u32,
}

/// How a turn ended (`Core::end_turn`): enough for kiln to tell a turn that's done from one that
/// stopped part-way through the work (`goals::keep`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TurnEnd {
    /// Unix ms.
    pub at: i64,
    /// "done": it ended as turns do; "cut": it stopped part-way (an unexpected stop, out of
    /// output, an error); "stopped": you stopped it; "limit": a turn or budget limit ended it;
    /// "refused": the model wouldn't go on; "blocked": what it needed to do wasn't allowed (a
    /// harness that can't ask, denying it).
    pub how: String,
    /// The harness's own word for it ("end_turn", "stop_sequence", "max_tokens", "failed" …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    /// The tools it called in the turn.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tools: u32,
    /// Its last step was a tool that failed, with nothing said after it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub after_error: bool,
    /// Its last words ask you something.
    #[serde(default, skip_serializing_if = "is_false")]
    pub asks: bool,
    /// `State::kept` when it ended: kiln started it and that many before it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub kept: u32,
}

/// Why a session waits, and until when (`State::pause`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Pause {
    /// "limit" (a usage limit was reached) or "threshold" (a limit got as full as `pause_at`).
    pub reason: String,
    /// The window, as `Limit::id` ("five_hour", "seven_day", "codex.primary", ...), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// That window's label ("Session", "Weekly").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// What the harness said, as it said it ("You've hit your session limit · resets 10:30pm").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub said: String,
    /// Unix ms the window starts over, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
    /// Unix ms it paused.
    pub since: i64,
    /// It goes on by itself once the window starts over (else when a person resumes it).
    pub auto: bool,
    /// "harness" (it reached the limit itself), "account" (another session on its account did)
    /// or "threshold".
    pub by: String,
    /// The turn the limit cut off: it goes on first when the pause ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cut: Option<Cut>,
    /// Its folder as it was when it paused, kept in a hidden git ref.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<Checkpoint>,
    /// Checks at the reset that found the limit still reached.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tries: u32,
    /// Unix ms a check started, while it waits for the harness's answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checking: Option<i64>,
    /// Unix ms of the next check, when the reset time isn't known (or a check found the limit
    /// still reached).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_check: Option<i64>,
}

/// A turn a limit cut off (`Pause::cut`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Cut {
    /// The client id of the message that started it, when kiln sent it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// What that message said, shortened.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    pub at: i64,
}

/// A folder kept as it was (`Pause::checkpoint`): a commit outside every branch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Checkpoint {
    /// "refs/kiln/checkpoints/<session>/<unix ms>".
    pub git_ref: String,
    pub commit: String,
    pub at: i64,
}

/// A switch the session will make (`State::switching`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Switching {
    /// The harness ("codex").
    pub kind: String,
    pub model: String,
}

impl State {
    /// How many tasks are left running with no turn under way (`State::left_behind`).
    pub fn count_left_behind(&self) -> u32 {
        if !matches!(self.status, Status::Idle | Status::Exited) {
            return 0;
        }
        u32::try_from(self.tasks.iter().filter(|t| t.left_running()).count()).unwrap_or(u32::MAX)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ExitInfo {
    pub code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What rarely changes: what the harness supports and offers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Meta {
    pub version: String,
    #[serde(default)]
    pub modes: Vec<ModeOption>,
    #[serde(default)]
    pub models: Vec<ModelOption>,
    #[serde(default)]
    pub commands: Vec<SlashCommand>,
    /// Subagent types it can start.
    #[serde(default)]
    pub agents: Vec<String>,
    /// Features this adapter drives for this harness: "steer", "send_now", "queue_cancel",
    /// "interrupt", "set_mode", "set_model", "set_effort", "stop_task", "file_suggestions",
    /// "resume", "attachments", "images", "overrides", "switch_harness" (a model of another
    /// harness: `Command::SetModel::kind`).
    #[serde(default)]
    pub features: Vec<String>,
    /// Effort / reasoning levels the harness offers (empty: none).
    #[serde(default)]
    pub efforts: Vec<ControlOption>,
    /// Every other control the harness offers.
    #[serde(default)]
    pub controls: Vec<Control>,
    /// The hooks configured for the harness, read-only.
    #[serde(default)]
    pub hooks: Vec<HookInfo>,
}

/// A hook the harness runs on an event (read-only).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HookInfo {
    pub event: String,
    #[serde(default)]
    pub matcher: String,
    pub command: String,
    #[serde(default)]
    pub source: String,
}

/// A command from a person (any device) to a session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    Send {
        /// Client-made id; delivery state is reported under it.
        id: String,
        text: String,
        #[serde(default)]
        mode: SendMode,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        overrides: Option<Overrides>,
    },
    QueueEdit {
        id: String,
        text: String,
    },
    QueueRemove {
        id: String,
    },
    QueueMove {
        id: String,
        index: usize,
    },
    /// Stop the turn and send this queued message now.
    QueueSendNow {
        id: String,
    },
    /// Deliver this queued message into the running turn as steering (no interrupt); with no
    /// turn running it is sent as the next one. (Feature "queue_steer".)
    QueueSteer {
        id: String,
    },
    Approve {
        /// The approval's id.
        id: String,
        decision: Decision,
        /// Feedback for a denial, or a note with an approval.
        #[serde(default)]
        message: Option<String>,
        /// For "always": which of the approval's `always` options.
        #[serde(default)]
        always: Option<usize>,
        /// Answers to questions: question text → chosen label(s) (multi-select comma-separated).
        #[serde(default)]
        answers: Option<std::collections::BTreeMap<String, String>>,
        /// Answers to questions, one per question (over `answers`): labels with commas, free
        /// text and notes come through as they are.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        choices: Option<Vec<ChoiceAnswer>>,
    },
    Interrupt,
    /// The turn didn't stop when asked: the harness starts again at once, the conversation going
    /// on, and what it was running in the turn stops with it.
    ForceStop,
    SetMode {
        mode: String,
    },
    SetModel {
        model: String,
        /// The harness the model belongs to, when it is another one than the session's (feature
        /// "switch_harness"): the session goes on in that one once the turn under way ends, with
        /// the conversation so far.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
    },
    SetEffort {
        effort: String,
    },
    /// Go on on runner `to`, in its folder `cwd` (a path there, or `~/…`), once the turn under way
    /// ends: the session moves there, conversation, timeline and queue alike (`harness::moving`).
    MoveTo {
        to: String,
        cwd: String,
        /// What came along of the folder, said in the timeline ("its repository, kept in step").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        /// Not waiting for the turn under way: it is interrupted, and picked up over there.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        now: bool,
        /// It goes on there in another harness (`Kind::id`), its conversation carried over, with
        /// this model (None: that harness's default).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// How a move is getting on before it starts (its repository being copied over): shown.
    Moving {
        text: String,
    },
    /// Set a control's value, or run an action (its argument, if any, in `value`).
    Control {
        id: String,
        #[serde(default)]
        value: Option<serde_json::Value>,
    },
    StopTask {
        task: String,
    },
    /// Start the harness again where it left off (after it exited).
    Restart,
    /// File-path completions for an @-mention; answered as a `reply` update to `client`.
    Suggest {
        client: String,
        query: String,
    },
    /// Announce the session again with its whole state (a server that lost track of it).
    Snapshot,
    /// Switch an MCP server on or off.
    McpEnable {
        name: String,
        enabled: bool,
    },
    McpReconnect {
        name: String,
    },
    /// Ask the harness for every MCP server's state again.
    McpRefresh,
    /// Sign in to an MCP server, the harness's own way. Answered to `client` with
    /// `{"url": ...}` to open. When the sign-in ends on a page that does not load (the harness
    /// waits for it on its own machine), send that page's address back as `redirect`.
    McpAuth {
        name: String,
        #[serde(default)]
        client: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        redirect: Option<String>,
    },
    /// Add an MCP server with the harness's own CLI (answered to `client`, when given, with
    /// `{"ok": bool, "message": ...}`).
    McpAdd {
        #[serde(default)]
        scope: String,
        name: String,
        #[serde(default)]
        transport: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        env: std::collections::BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        headers: std::collections::BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client: Option<String>,
    },
    McpRemove {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client: Option<String>,
    },
    /// A skill's SKILL.md, answered to `client` as `{"content": ..., "path": ...}`.
    SkillView {
        name: String,
        client: String,
    },
    /// Run a skill the harness's way; delivered like a message (`id` and `mode` as for Send).
    SkillRun {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default)]
        mode: SendMode,
    },
    /// Read the skills from disk again.
    SkillsReload,
    /// Stop every task still running (shells, monitors, background agents).
    StopTasks,
    /// End a pause now (`State::pause`): the turn it cut off goes on, then the queue.
    Resume,
    /// Whether a pause ends by itself once its limit resets.
    AutoResume {
        on: bool,
    },
    /// Pause once a limit is this full (`State::pause_at`): between turns, or at the next step of
    /// one under way. None: only when one is reached.
    PauseAt {
        #[serde(default)]
        percent: Option<u8>,
    },
    /// Whether at a usage limit it goes on on another machine with room (`State::move_on_limit`).
    MoveOnLimit {
        on: bool,
    },
    /// Another session on the same account reached a usage limit: wait for its reset too (the
    /// server sends it).
    LimitHold {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        until: Option<i64>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        said: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Always,
    Deny,
}

/// The id of the account a session runs on: "<harness>:<plan>", and a short hash of who it is
/// when known (an address or an organization), never the thing itself.
pub fn account_id(harness: &str, plan: &str, who: Option<&str>) -> String {
    let plan: String = plan.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    match who.and_then(who_hash) {
        Some(h) => format!("{harness}:{plan}:{h}"),
        None => format!("{harness}:{plan}"),
    }
}

/// The part of an account id that says who it is, without saying it: a short hash of the address.
fn who_hash(who: &str) -> Option<String> {
    use sha2::Digest as _;
    let w = who.trim().to_lowercase();
    (!w.is_empty()).then(|| hex::encode(sha2::Sha256::digest(w.as_bytes()).get(..5).unwrap_or_default()))
}

/// Whether account id `account` (`account_id`) is `harness` signed in as `who`: Some(true) or
/// Some(false), None when the id doesn't say who (an API key, a provider of its own).
pub fn account_is(account: &str, harness: &str, who: &str) -> Option<bool> {
    let mut parts = account.split(':');
    let (Some(h), Some(_plan), Some(hash)) = (parts.next(), parts.next(), parts.next()) else { return None };
    Some(h == harness && who_hash(who).is_some_and(|w| w == hash))
}

/// Plan words for the UI: "max" → "Max", "self_serve_business_prolite" → "Self serve business prolite".
pub fn plan_label(plan: &str) -> String {
    let words = plan.replace(['_', '-'], " ");
    let mut c = words.trim().chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// Email addresses in `s`, shortened to their first letter and domain ("a…@example.com").
pub fn mask_emails(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, word) in s.split(' ').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        match word.split_once('@') {
            Some((user, domain)) if !user.is_empty() && domain.contains('.') && !user.contains('/') && !user.contains(':') => {
                let lead: String = user.chars().take(1).collect();
                out.push_str(&format!("{lead}…@{domain}"));
            }
            _ => out.push_str(word),
        }
    }
    out
}

/// A server URL safe to show: no credentials, query or fragment.
pub fn safe_url(url: &str) -> String {
    let url = url.split(['?', '#']).next().unwrap_or("");
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let (host, path) = rest.split_once('/').map_or((rest, ""), |(h, p)| (h, p));
            let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
            if path.is_empty() { format!("{scheme}://{host}") } else { format!("{scheme}://{host}/{path}") }
        }
        None => url.to_owned(),
    }
}

/// A limit window's length in short words: 300 → "5h", 10080 → "7d".
pub fn window_words(minutes: i64) -> String {
    match minutes {
        m if m > 0 && m % 1440 == 0 => format!("{}d", m / 1440),
        m if m > 0 && m % 60 == 0 => format!("{}h", m / 60),
        m => format!("{m}m"),
    }
}

/// Shorten `s` to at most `max` bytes, keeping its head and tail (a char boundary each).
pub fn clip_middle(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_owned(), false);
    }
    let half = max / 2;
    let mut head = half;
    while head > 0 && !s.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = s.len().saturating_sub(half);
    while tail < s.len() && !s.is_char_boundary(tail) {
        tail += 1;
    }
    let omitted = tail.saturating_sub(head);
    let (a, b) = (s.get(..head).unwrap_or(""), s.get(tail..).unwrap_or(""));
    (format!("{a}\n… {omitted} bytes omitted …\n{b}"), true)
}

/// Shorten long strings inside a JSON value (tool inputs can carry whole files).
pub fn clip_json(v: &serde_json::Value, max: usize) -> serde_json::Value {
    match v {
        serde_json::Value::String(s) if s.len() > max => serde_json::Value::String(clip_middle(s, max).0),
        serde_json::Value::Array(a) => serde_json::Value::Array(a.iter().map(|x| clip_json(x, max)).collect()),
        serde_json::Value::Object(o) => serde_json::Value::Object(o.iter().map(|(k, x)| (k.clone(), clip_json(x, max))).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_catalog_has_one_entry_per_name_and_no_alias_hides_one() {
        let c = sort_catalog(vec![
            SlashCommand::prompt("usage", "", "", "Session").aka(&["cost", "stats"]),
            SlashCommand::control("cost", "", "", "Session", serde_json::json!({"type": "control", "id": "session_cost"})),
            SlashCommand::prompt("usage", "a second one", "", "Session"),
            SlashCommand::prompt("clear", "", "[name]", "Context").aka(&["stats", "reset"]),
            SlashCommand::prompt("zed", "", "<x>", "Nowhere"),
        ]);
        let names: Vec<(&str, Vec<String>, &str)> = c.iter().map(|e| (e.name.as_str(), e.aliases.clone(), e.args.as_str())).collect();
        assert_eq!(
            names,
            vec![("cost", vec![], "none"), ("usage", vec!["stats".to_owned()], "none"), ("clear", vec!["reset".to_owned()], "optional"), ("zed", vec![], "required")]
        );
        assert_eq!(c.get(1).map(|e| e.description.as_str()), Some(""));
        // The route's JSON: tagged by kind; a plain prompt is just its kind.
        assert_eq!(serde_json::to_value(Route::default()).ok(), Some(serde_json::json!({"kind": "prompt"})));
        assert_eq!(serde_json::to_value(Route::Unsupported { reason: "r".into() }).ok(), Some(serde_json::json!({"kind": "unsupported", "reason": "r"})));
        assert_eq!(serde_json::to_value(Route::Terminal { command: "/doctor".into() }).ok(), Some(serde_json::json!({"kind": "terminal", "command": "/doctor"})));
    }

    #[test]
    fn clipping_keeps_head_and_tail_on_char_boundaries() {
        let s = "é".repeat(100);
        let (c, cut) = clip_middle(&s, 41);
        assert!(cut);
        assert!(c.starts_with('é') && c.ends_with('é'));
        assert!(c.contains("bytes omitted"));
        assert_eq!(clip_middle("short", 10), ("short".to_owned(), false));
        let v = serde_json::json!({"content": "x".repeat(50), "n": 3, "list": ["y".repeat(50)]});
        let c = clip_json(&v, 20);
        assert!(c.get("content").and_then(serde_json::Value::as_str).is_some_and(|s| s.len() < 50));
        assert_eq!(c.get("n"), Some(&serde_json::json!(3)));
    }

    #[test]
    fn accounts_and_urls_are_shown_without_secrets() {
        let a = account_id("claude", "max", Some("Someone@Example.com"));
        assert!(a.starts_with("claude:max:") && a.len() == "claude:max:".len() + 10 && !a.contains("example"));
        assert_eq!(a, account_id("claude", "max", Some("someone@example.com ")));
        assert_eq!(account_id("codex", "API key", None), "codex:api-key");
        assert_eq!(plan_label("self_serve_business_prolite"), "Self serve business prolite");
        assert_eq!(mask_emails("Email: someone@example.com (pro)"), "Email: s…@example.com (pro)");
        assert_eq!(mask_emails("uds:/run/x.sock no@mail"), "uds:/run/x.sock no@mail");
        assert_eq!(safe_url("https://user:pw@mcp.example.com/v1/mcp?token=abc#x"), "https://mcp.example.com/v1/mcp");
        assert_eq!(safe_url("http://127.0.0.1:27190"), "http://127.0.0.1:27190");
        assert_eq!((window_words(300), window_words(10080), window_words(90)), ("5h".to_owned(), "7d".to_owned(), "90m".to_owned()));
    }

    #[test]
    fn tool_calls_name_their_server_and_skill() {
        let mut t = ToolCall {
            tool_id: "t".into(),
            name: "mcp__github__search_issues".into(),
            tool: ToolKind::Other,
            title: String::new(),
            detail: None,
            input: serde_json::json!({}),
            status: ToolStatus::Running,
            output: None,
            truncated: false,
            diffs: vec![],
            todos: vec![],
            task: None,
            exit_code: None,
            started: 0,
            ended: None,
            server: None,
            tool_short: None,
            skill: None,
        };
        t.annotate();
        assert_eq!((t.server.as_deref(), t.tool_short.as_deref(), t.tool), (Some("github"), Some("search_issues"), ToolKind::Mcp));
        let mut s = ToolCall { name: "Skill".into(), tool: ToolKind::Skill, input: serde_json::json!({"skill": "/review", "args": "x"}), server: None, tool_short: None, ..t };
        s.annotate();
        assert_eq!((s.skill.as_deref(), s.server.as_deref()), (Some("review"), None));
    }

    #[test]
    fn events_serialize_flat_with_a_kind_tag() -> anyhow::Result<()> {
        let e = Event {
            id: "e1".into(),
            seq: 1,
            at: 5,
            parent: None,
            by: None,
            body: Body::Assistant { text: "hi".into(), streaming: false },
        };
        let v = serde_json::to_value(&e)?;
        assert_eq!(v, serde_json::json!({"id": "e1", "seq": 1, "at": 5, "kind": "assistant", "text": "hi"}));
        let back: Event = serde_json::from_value(v)?;
        assert_eq!(back, e);
        let c: Command = serde_json::from_str(r#"{"type":"send","id":"m1","text":"go","mode":"steer"}"#)?;
        assert_eq!(c, Command::Send { id: "m1".into(), text: "go".into(), mode: SendMode::Steer, attachments: vec![], overrides: None });
        let c: Command = serde_json::from_str(r#"{"type":"control","id":"compact","value":"keep the API notes"}"#)?;
        assert_eq!(c, Command::Control { id: "compact".into(), value: Some(serde_json::json!("keep the API notes")) });
        let c: Command = serde_json::from_str(r#"{"type":"send","id":"m2","text":"see","attachments":[{"name":"a.png","mime":"image/png","data_base64":"iVBO"}],"overrides":{"effort":"high"}}"#)?;
        assert!(matches!(c, Command::Send { ref attachments, ref overrides, .. } if attachments.len() == 1 && attachments.first().is_some_and(Attachment::is_image) && overrides.as_ref().and_then(|o| o.effort.as_deref()) == Some("high")));
        let c: Command = serde_json::from_str(r#"{"type":"interrupt"}"#)?;
        assert_eq!(c, Command::Interrupt);
        let c: Command = serde_json::from_str(r#"{"type":"mcp_add","scope":"user","name":"echo","transport":"stdio","command":"python3","args":["-I","echo.py"],"env":{"K":"v"}}"#)?;
        assert!(matches!(c, Command::McpAdd { ref name, ref args, ref env, .. } if name == "echo" && args.len() == 2 && env.get("K").map(String::as_str) == Some("v")));
        let c: Command = serde_json::from_str(r#"{"type":"mcp_auth","name":"gh","client":"c1"}"#)?;
        assert_eq!(c, Command::McpAuth { name: "gh".into(), client: "c1".into(), redirect: None });
        let c: Command = serde_json::from_str(r#"{"type":"skill_run","name":"review","args":"src"}"#)?;
        assert_eq!(c, Command::SkillRun { name: "review".into(), args: Some("src".into()), id: None, mode: SendMode::Queue });
        Ok(())
    }
}
