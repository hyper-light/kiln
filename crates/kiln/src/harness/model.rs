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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Question {
    pub question: String,
    #[serde(default)]
    pub header: String,
    #[serde(default)]
    pub multi: bool,
    pub options: Vec<QuestionOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelOption {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SlashCommand {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub hint: String,
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

/// Everything about a session that is not the timeline. Small; sent whole on change.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct State {
    pub status: Status,
    /// The harness's own session id (what `--resume` takes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_session: Option<String>,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub title: String,
    /// What the person first asked for (the session's goal), shortened.
    #[serde(default)]
    pub goal: String,
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
    /// The effort / reasoning level in use, where the harness has one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub effort: String,
    /// The live value of each control (`Meta::controls`), by id.
    #[serde(default)]
    pub control_values: std::collections::BTreeMap<String, serde_json::Value>,
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
    /// "resume", "attachments", "images", "overrides".
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
    },
    Interrupt,
    SetMode {
        mode: String,
    },
    SetModel {
        model: String,
    },
    SetEffort {
        effort: String,
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
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Always,
    Deny,
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
    fn events_serialize_flat_with_a_kind_tag() -> anyhow::Result<()> {
        let e = Event {
            id: "e1".into(),
            seq: 1,
            at: 5,
            parent: None,
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
        Ok(())
    }
}
