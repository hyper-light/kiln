//! A session's normalized state and timeline, plus what came out of the last step: updates for
//! the UI, lines for the harness's stdin, previews to compute. Adapters mutate it; the brain that
//! owns the process drains it. No I/O here, so adapters are tested by feeding lines.

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use super::model::{Approval, Body, Event, Meta, State, Status, Task, ToolStatus, TurnEnd, clip_middle};

/// The ids of the messages kiln sends to keep an agent going (`goals::keep`); they show as kiln's.
pub const KEEP_ID: &str = "kiln-keep-";

/// Tasks a session's state keeps: beyond these the oldest finished go. The state goes to viewers
/// whole, in one frame, and a long Codex session runs hundreds of terminals.
pub const TASKS_KEPT: usize = 200;
/// Longest command a finished task keeps (its call in the timeline keeps it whole).
const ENDED_COMMAND_MAX: usize = 2_000;

/// The tasks a state carries kept bounded: a finished one keeps the start and the end of a long
/// command, and beyond [`TASKS_KEPT`] the oldest finished go, subagents last (a subagent's row is
/// how its thread is found, and there are few of them). The ids of those that went.
pub fn bound_tasks(tasks: &mut Vec<Task>) -> Vec<String> {
    for t in tasks.iter_mut().filter(|t| !t.live()) {
        if let Some(c) = t.command.as_mut().filter(|c| c.len() > ENDED_COMMAND_MAX) {
            // Under the limit once shortened, so never shortened again.
            *c = clip_middle(c, ENDED_COMMAND_MAX - 64).0;
        }
    }
    let mut gone = Vec::new();
    for agents in [false, true] {
        let mut over = tasks.len().saturating_sub(TASKS_KEPT);
        if over == 0 {
            break;
        }
        tasks.retain(|t| {
            if over == 0 || t.live() || (t.class.as_deref() == Some("agent")) != agents {
                return true;
            }
            over -= 1;
            gone.push(t.id.clone());
            false
        });
    }
    gone
}

/// Who a sent message is from, by its id: one of kiln's own keep-going ones, or a person's.
pub fn origin_of(client_id: &str) -> &'static str {
    if client_id.starts_with(KEEP_ID) { "kiln" } else { "human" }
}

/// Words that end by asking something (their last line ends with a question mark).
fn asks_you(text: &str) -> bool {
    text.trim_end().trim_end_matches(['*', '_', '`', ')', '"']).ends_with('?')
}

/// A call still running or waiting, or text still being written.
fn still_open(body: &Body) -> bool {
    match body {
        Body::Tool(t) => matches!(t.status, ToolStatus::Running | ToolStatus::Pending),
        Body::Assistant { streaming, .. } | Body::Thinking { streaming, .. } => *streaming,
        _ => false,
    }
}

/// Timeline events kept per session (the oldest go first).
pub const MAX_EVENTS: usize = 4_000;

/// A change for the UI (and for the server's mirror of a machine's session).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Update {
    /// A session appeared, or is announced again after a reconnect (a snapshot follows).
    Open { info: Box<super::session::Info> },
    /// The session is gone.
    Closed,
    State { state: Box<State> },
    Meta { meta: Box<Meta> },
    /// Upserts by id.
    Events { events: Vec<Event> },
    /// The session's whole changeset so far (replaces the previous one).
    Changes { files: Vec<super::model::FileChange> },
    /// Text to append to a streaming event.
    Stream { id: String, text: String },
    /// The answer to one client's request (file suggestions ...).
    Reply { client: String, value: serde_json::Value },
    /// The timeline starts over (a snapshot follows). `from`: the number of the snapshot's first
    /// event. What came before it isn't in memory any more but is still the timeline's: a store
    /// keeps it, and drops only its events from there on that the snapshot doesn't bring again (a
    /// rewind's). None: from the start.
    Reset {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<u64>,
    },
}

/// How a file was before the agent first changed it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Baseline {
    /// It did not exist.
    Absent,
    Text(String),
    /// Too large to keep: changes to it are counted, not shown.
    TooLarge,
    /// Not known yet: it is the file as it is now with these hunks undone (harnesses that report
    /// patches, not the file before them).
    Reverse(Vec<super::model::Hunk>),
}

/// Largest file content kept as a baseline.
pub const BASELINE_MAX: usize = 2 << 20;
/// Most files tracked in a session's changeset.
const MAX_CHANGED_FILES: usize = 500;

/// Recompute one file's cumulative change (reads the file as it is now; off the event loop).
#[derive(Debug, Clone, PartialEq)]
pub struct ChangeJob {
    pub abs: String,
    pub rel: String,
    pub baseline: Baseline,
}

/// Something only the session (which owns the process) can do, asked for by an adapter.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Start the harness again, continuing its session from `resume_at` (a rewind).
    Restart { resume_at: Option<String> },
    /// From its next start the harness may skip every permission check: a person chose a mode
    /// that turns checks off, which it was not started able to use.
    AllowBypass,
    /// Start a new session that branches off this one at `resume_at`.
    Branch { resume: String, resume_at: Option<String> },
    /// Run the harness's own CLI beside the session (`mcp add ...`), in its directory and
    /// environment. Its exit and output come back to the adapter (`run_done`) under `job`.
    Run { job: String, args: Vec<String> },
    /// Answer `client` with the first of these files that can be read (`{"content", "path"}`),
    /// or `{"error"}`.
    Read { client: String, paths: Vec<String> },
    /// Read the end of the harness's own transcript of `session` (Claude Code's, under its
    /// configuration directory `config`). The lines come back as a run's end (`run_done`, code 0,
    /// one record a line; code 1 when it can't be read) under `job`.
    TranscriptTail { job: String, config: String, session: String, bytes: u64 },
    /// Make a GET request to a sign-in callback the harness listens for on this machine's
    /// loopback (a browser on another device cannot reach it). Only loopback addresses are
    /// fetched. Its result comes back as a run's end (`run_done`) under `job`.
    Fetch { job: String, url: String },
    /// Stop a process the harness started and cannot stop itself (a terminal Codex left
    /// running): the newest process under the harness whose command line runs `command`, with
    /// everything it started. The outcome comes back as a run's end (`run_done`, code 0: stopped).
    Kill { job: String, command: String },
    /// Keep the session's folder as it is now in a hidden git ref (the pause that started at `at`
    /// gets it as `Pause::checkpoint`), when the folder is a repository. Nothing in the folder,
    /// its index or its branches changes.
    Checkpoint { at: i64 },
}

/// The output of a task kiln keeps itself (a harness that streams it rather than writing a
/// file): the last `TASK_OUTPUT_MAX` bytes, and how many there have been.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaskOutput {
    pub data: VecDeque<u8>,
    /// Bytes appended so far (the offset just past the newest).
    pub total: u64,
}

/// Most output kept per task.
pub const TASK_OUTPUT_MAX: usize = 256 * 1024;

impl TaskOutput {
    pub fn append(&mut self, bytes: &[u8]) {
        self.data.extend(bytes);
        let excess = self.data.len().saturating_sub(TASK_OUTPUT_MAX);
        self.data.drain(..excess);
        self.total = self.total.saturating_add(bytes.len() as u64);
    }

    /// What came after `since` (all that is kept when `since` is older than that), and the offset
    /// to ask from next.
    pub fn since(&self, since: u64) -> (Vec<u8>, u64, bool) {
        let first = self.total.saturating_sub(self.data.len() as u64);
        let from = since.max(first);
        let skip = usize::try_from(from - first).unwrap_or(usize::MAX);
        (self.data.iter().skip(skip).copied().collect(), self.total, since < first)
    }
}

/// Work the brain does off the event loop and feeds back: e.g. reading a file to show what an
/// edit would change.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewJob {
    pub approval: String,
    pub cwd: String,
    pub tool: String,
    pub input: serde_json::Value,
}

/// A tag of one run of a harness's adapter: the ids of the events it makes carry it, so a harness
/// started again (after a crash, or on another machine after a move) never takes the ids of an
/// earlier run's, which would put what it says over what was said before.
pub fn run_tag() -> String {
    crate::util::new_id().chars().skip(20).collect()
}

pub struct Core {
    pub state: State,
    pub meta: Meta,
    events: VecDeque<Event>,
    /// id → seq of every event still kept.
    index: HashMap<String, u64>,
    next_seq: u64,
    /// Updates not yet sent, in order.
    pub out: Vec<Update>,
    /// Lines for the harness's stdin.
    pub writes: Vec<String>,
    pub previews: Vec<PreviewJob>,
    state_dirty: bool,
    meta_dirty: bool,
    /// Streaming text not yet flushed, by event id (coalesced).
    stream: Vec<(String, String)>,
    /// Events changed since the last flush, by id (coalesced).
    changed: Vec<String>,
    /// Files the agent changed, as they were before its first change (absolute paths).
    pub baselines: HashMap<String, Baseline>,
    /// Files whose cumulative change must be recomputed.
    pub change_jobs: Vec<ChangeJob>,
    changing: HashSet<String>,
    /// The changeset: one entry per file that differs from its baseline (absolute path keys).
    changes: std::collections::BTreeMap<String, super::model::FileChange>,
    changes_dirty: bool,
    /// What the session has to do for the adapter (restart, branch).
    pub effects: Vec<Effect>,
    /// Output of tasks kiln keeps itself, by task id (not sent with the state).
    pub task_output: HashMap<String, TaskOutput>,
    /// The timeline was cut (a rewind): viewers start over from a snapshot.
    reset: bool,
    /// kiln's queue waits: the session goes on in another harness once the turn under way ends,
    /// and what is queued goes to that one.
    pub hold: bool,
    /// The harness running now (its name): new events say so (`Event::by`).
    pub harness: Option<String>,
}

impl Default for Core {
    fn default() -> Self {
        Self::new()
    }
}

/// A session's core written out whole, for this process's next image (an in-place upgrade): the
/// session goes on there, with the same harness process, where it was.
#[derive(Debug, Serialize, Deserialize)]
pub struct Frozen {
    state: State,
    meta: Meta,
    events: Vec<Event>,
    next_seq: u64,
    baselines: HashMap<String, Baseline>,
    changes: std::collections::BTreeMap<String, super::model::FileChange>,
    /// Task output kiln keeps itself: its bytes (base64), and how many there have been.
    task_output: HashMap<String, (String, u64)>,
    stream: Vec<(String, String)>,
    changed: Vec<String>,
    /// Updates not sent yet, and lines for the harness not written yet: the next image sends them.
    out: Vec<Update>,
    writes: Vec<String>,
    hold: bool,
    harness: Option<String>,
}


impl Core {
    pub fn new() -> Self {
        Self {
            state: State::default(),
            meta: Meta::default(),
            events: VecDeque::new(),
            index: HashMap::new(),
            next_seq: 1,
            out: Vec::new(),
            writes: Vec::new(),
            previews: Vec::new(),
            state_dirty: true,
            meta_dirty: true,
            stream: Vec::new(),
            changed: Vec::new(),
            baselines: HashMap::new(),
            change_jobs: Vec::new(),
            changing: HashSet::new(),
            changes: std::collections::BTreeMap::new(),
            changes_dirty: false,
            effects: Vec::new(),
            task_output: HashMap::new(),
            reset: false,
            hold: false,
            harness: None,
        }
    }

    /// Everything in it, for this process's next image ([`Core::thaw`]).
    pub fn freeze(&self) -> Frozen {
        use base64::Engine as _;
        Frozen {
            state: self.state.clone(),
            meta: self.meta.clone(),
            events: self.events.iter().cloned().collect(),
            next_seq: self.next_seq,
            baselines: self.baselines.clone(),
            changes: self.changes.clone(),
            task_output: self.task_output.iter().map(|(id, t)| (id.clone(), (base64::engine::general_purpose::STANDARD.encode(t.data.iter().copied().collect::<Vec<u8>>()), t.total))).collect(),
            stream: self.stream.clone(),
            changed: self.changed.clone(),
            out: self.out.clone(),
            writes: self.writes.clone(),
            hold: self.hold,
            harness: self.harness.clone(),
        }
    }

    /// The core a previous image of this process froze ([`Core::freeze`]), whole. What it hadn't
    /// sent yet goes out with the next flush; viewers get it all again from the session's
    /// announcement.
    pub fn thaw(f: Frozen) -> Self {
        use base64::Engine as _;
        let mut c = Self::new();
        c.index = f.events.iter().map(|e| (e.id.clone(), e.seq)).collect();
        c.events = f.events.into();
        c.next_seq = f.next_seq;
        c.state = f.state;
        c.meta = f.meta;
        c.baselines = f.baselines;
        c.changes = f.changes;
        c.changes_dirty = !c.changes.is_empty();
        c.task_output = f
            .task_output
            .into_iter()
            .map(|(id, (data, total))| (id, TaskOutput { data: base64::engine::general_purpose::STANDARD.decode(data).unwrap_or_default().into(), total }))
            .collect();
        c.stream = f.stream;
        c.changed = f.changed;
        c.out = f.out;
        c.writes = f.writes;
        c.hold = f.hold;
        c.harness = f.harness;
        // An older image kept every task: the first state this one sends must fit a frame.
        for id in bound_tasks(&mut c.state.tasks) {
            c.task_output.remove(&id);
        }
        c
    }

    /// Take over the timeline and state the session had on another machine (it moved here): its
    /// events as they were (ids, order, times, who said them), its queue, todos and title. What
    /// belonged to the process over there (its tasks, a turn's state) stays there.
    pub fn adopt(&mut self, mut events: Vec<Event>, state: State) {
        // A conversation cleared since: what came before it is history, not the timeline.
        if let Some(from) = state.cleared {
            events.retain(|e| e.seq >= from);
        }
        let first = events.first().map_or(1, |e| e.seq.max(1));
        // Numbers stay contiguous: an event's place is found from its number.
        self.events = events
            .into_iter()
            .enumerate()
            .map(|(i, mut e)| {
                e.seq = first + i as u64;
                e
            })
            .collect();
        self.index = self.events.iter().map(|e| (e.id.clone(), e.seq)).collect();
        self.next_seq = self.events.back().map_or(1, |e| e.seq + 1).max(state.cleared.unwrap_or(1));
        let cwd = std::mem::take(&mut self.state.cwd);
        let harness_session = self.state.harness_session.take();
        self.state = state;
        self.state.cwd = cwd;
        self.state.harness_session = harness_session;
        self.state.status = super::model::Status::Starting;
        self.state.turn_started = None;
        self.state.approvals.clear();
        self.state.tasks.clear();
        self.state.exit = None;
        self.state.activity = None;
        self.state.doing = None;
        self.state.switching = None;
        self.changed.clear();
        self.stream.clear();
        self.reset = true;
        self.state_dirty = true;
        self.meta_dirty = true;
    }

    /// Drop `id` and every event after it (a rewind); viewers get the timeline again.
    pub fn truncate_from(&mut self, id: &str) {
        let Some(seq) = self.index.get(id).copied() else { return };
        while self.events.back().is_some_and(|e| e.seq >= seq) {
            if let Some(e) = self.events.pop_back() {
                self.index.remove(&e.id);
            }
        }
        // Sequence numbers stay contiguous: an event's place is found from its number.
        self.next_seq = seq;
        self.changed.clear();
        self.stream.clear();
        self.reset = true;
        self.state_dirty = true;
    }

    /// The conversation starts over (`/clear`): the timeline empties and its numbers go on. What
    /// it held stays history, as events dropped to bound memory do: viewers get the timeline
    /// again, the store keeps the events, and a timeline taken up again leaves them out.
    pub fn clear(&mut self) {
        self.events.clear();
        self.index.clear();
        self.changed.clear();
        self.stream.clear();
        self.state.cleared = Some(self.next_seq);
        self.state.dropped = 0;
        self.reset = true;
        self.state_dirty = true;
    }

    /// Set a control's live value (sent with the state).
    pub fn set_control(&mut self, id: &str, value: serde_json::Value) {
        if self.state.control_values.get(id) != Some(&value) {
            self.state.control_values.insert(id.to_owned(), value);
            self.state_dirty = true;
        }
    }

    /// The agent is about to change (or just changed) `abs`; `before` is how it was. Only the
    /// first one counts: the changeset is relative to how the file was before the session.
    pub fn note_baseline(&mut self, abs: &str, before: Baseline) {
        if self.baselines.contains_key(abs) || self.baselines.len() >= MAX_CHANGED_FILES {
            return;
        }
        self.baselines.insert(abs.to_owned(), before);
    }

    /// Recompute `abs` (shown as `rel`) once the brain gets to it.
    pub fn want_change(&mut self, abs: &str, rel: &str) {
        let Some(baseline) = self.baselines.get(abs).cloned() else { return };
        self.change_jobs.retain(|j| j.abs != abs);
        self.change_jobs.push(ChangeJob { abs: abs.to_owned(), rel: rel.to_owned(), baseline });
    }

    /// Only one recomputation per file runs; later edits wait as its latest pending job.
    pub fn take_change_jobs(&mut self) -> Vec<ChangeJob> {
        self.change_jobs.extract_if(.., |j| !self.changing.contains(&j.abs)).collect()
    }

    pub fn start_change(&mut self, abs: &str) {
        self.changing.insert(abs.to_owned());
    }

    /// A file's cumulative change, recomputed (None: it is back to how it was), and its baseline
    /// when the recomputation had to work it out.
    pub fn set_change(&mut self, abs: &str, mut change: Option<super::model::FileChange>, resolved: Option<Baseline>) {
        self.changing.remove(abs);
        if let Some(b) = resolved {
            self.baselines.insert(abs.to_owned(), b);
        }
        if let Some(next) = self.change_jobs.iter_mut().find(|j| j.abs == abs) {
            if let Some(b) = self.baselines.get(abs) {
                next.baseline.clone_from(b);
            }
            return;
        }
        if let Some(c) = change.as_mut() {
            if let Some(before) = self.changes.get(abs) {
                // Recomputing the same diff does not make it a newer change.
                let at = c.at;
                c.at = before.at;
                if *c == *before {
                    return;
                }
                c.at = at;
            }
        } else if !self.changes.contains_key(abs) {
            return;
        }
        match change {
            Some(c) => {
                self.changes.insert(abs.to_owned(), c);
            }
            None => {
                self.changes.remove(abs);
            }
        }
        let s = &mut self.state;
        s.files_changed = u32::try_from(self.changes.len()).unwrap_or(u32::MAX);
        s.lines_added = self.changes.values().map(|c| c.added).sum();
        s.lines_removed = self.changes.values().map(|c| c.removed).sum();
        self.changes_dirty = true;
        self.state_dirty = true;
    }

    pub fn changes(&self) -> Vec<super::model::FileChange> {
        self.changes.values().cloned().collect()
    }

    pub fn now() -> i64 {
        crate::util::now_ms()
    }

    /// A message went to the agent: one kiln sent to keep it going counts toward `State::kept`;
    /// anyone else's starts the count again.
    pub fn note_sent(&mut self, client_id: &str) {
        let kept = if client_id.starts_with(KEEP_ID) { self.state.kept.saturating_add(1) } else { 0 };
        if kept != self.state.kept {
            self.state.kept = kept;
            self.touch_state();
        }
    }

    /// The turn under way ended (`how`, `why`: see [`TurnEnd`]). What it did is read from the
    /// timeline since it `started` (unknown: since the last thing said to it).
    pub fn end_turn(&mut self, started: Option<i64>, how: &str, why: Option<&str>) {
        let since = started.unwrap_or_else(|| self.events.iter().rev().find(|e| e.parent.is_none() && matches!(e.body, Body::User { .. })).map_or(0, |e| e.at));
        let (tools, after_error, asks) = {
            let mut tools = 0u32;
            let mut last: Option<&Body> = None;
            for e in self.events.iter().filter(|e| e.parent.is_none() && e.at >= since) {
                match &e.body {
                    Body::Tool(_) => {
                        tools = tools.saturating_add(1);
                        last = Some(&e.body);
                    }
                    Body::Assistant { text, .. } if !text.trim().is_empty() => last = Some(&e.body),
                    _ => {}
                }
            }
            let after_error = matches!(last, Some(Body::Tool(t)) if t.status == ToolStatus::Error);
            let asks = matches!(last, Some(Body::Assistant { text, .. }) if asks_you(text));
            (tools, after_error, asks)
        };
        self.state.last_turn = Some(TurnEnd { at: Self::now(), how: how.to_owned(), why: why.map(str::to_owned), tools, after_error, asks, kept: self.state.kept });
        self.touch_state();
    }

    pub fn events(&self) -> impl DoubleEndedIterator<Item = &Event> {
        self.events.iter()
    }

    pub fn get(&self, id: &str) -> Option<&Event> {
        let seq = *self.index.get(id)?;
        let front = self.events.front()?.seq;
        self.events.get(usize::try_from(seq.checked_sub(front)?).ok()?)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Event> {
        let seq = *self.index.get(id)?;
        let front = self.events.front()?.seq;
        let pos = usize::try_from(seq.checked_sub(front)?).ok()?;
        if !self.changed.iter().any(|c| c == id) {
            self.changed.push(id.to_owned());
        }
        self.events.get_mut(pos)
    }

    /// Add an event, or replace the one with the same id (keeping its place and time).
    pub fn upsert(&mut self, id: &str, parent: Option<String>, mut body: Body) {
        if let Body::Tool(t) = &mut body {
            t.annotate();
        }
        if let Some(e) = self.get_mut(id) {
            e.body = body;
            if parent.is_some() {
                e.parent = parent;
            }
            return;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.events.push_back(Event { id: id.to_owned(), seq, at: Self::now(), parent, by: self.harness.clone(), body });
        self.index.insert(id.to_owned(), seq);
        self.changed.push(id.to_owned());
        while self.events.len() > MAX_EVENTS {
            if let Some(old) = self.events.pop_front() {
                self.index.remove(&old.id);
                self.state.dropped += 1;
                self.state_dirty = true;
            }
        }
    }

    /// Text arriving for a streaming event (created by the caller beforehand).
    pub fn append(&mut self, id: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        let Some(seq) = self.index.get(id).copied() else { return };
        let Some(front) = self.events.front().map(|e| e.seq) else { return };
        let Some(e) = usize::try_from(seq.saturating_sub(front)).ok().and_then(|p| self.events.get_mut(p)) else { return };
        match &mut e.body {
            Body::Assistant { text: t, .. } | Body::Thinking { text: t, .. } => {
                if t.len() >= super::model::TEXT_MAX {
                    return;
                }
                t.push_str(text);
            }
            _ => return,
        }
        // An event created in this same step goes out whole; otherwise only the delta.
        if self.changed.iter().any(|c| c == id) {
            return;
        }
        match self.stream.iter_mut().find(|(i, _)| i == id) {
            Some((_, buf)) => buf.push_str(text),
            None => self.stream.push((id.to_owned(), text.to_owned())),
        }
    }

    pub fn touch_state(&mut self) {
        self.state_dirty = true;
    }

    pub fn touch_meta(&mut self) {
        self.meta_dirty = true;
    }

    pub fn write(&mut self, v: &serde_json::Value) {
        self.writes.push(v.to_string());
    }

    pub fn reply(&mut self, client: String, value: serde_json::Value) {
        self.flush();
        self.out.push(Update::Reply { client, value });
    }

    pub fn set_status(&mut self, s: Status) {
        if self.state.status != s {
            self.state.status = s;
            self.state_dirty = true;
        }
    }

    /// Recompute the status from what is pending: an open approval means the agent waits.
    pub fn settle_status(&mut self, running: bool) {
        let s = match self.state.status {
            Status::Exited | Status::Disconnected | Status::Starting if !running => self.state.status,
            _ if !self.state.approvals.is_empty() => Status::Waiting,
            _ if running => Status::Running,
            _ => Status::Idle,
        };
        self.set_status(s);
    }

    pub fn add_approval(&mut self, a: Approval) {
        self.state.approvals.retain(|x| x.id != a.id);
        self.state.approvals.push(a);
        self.state_dirty = true;
    }

    pub fn take_approval(&mut self, id: &str) -> Option<Approval> {
        let pos = self.state.approvals.iter().position(|a| a.id == id)?;
        self.state_dirty = true;
        Some(self.state.approvals.remove(pos))
    }

    /// The harness's process ended, or a new one starts: nothing the old one ran goes on (what
    /// kiln took back over runs apart from it: `survivors`), whichever harness it was and
    /// whatever its adapter knew. Its tasks still live end stopped, its calls still running or
    /// waiting end cancelled, text it was writing stops; all of it stays to read.
    pub fn end_harness_work(&mut self) {
        let now = Self::now();
        for t in self.state.tasks.iter_mut().filter(|t| t.live() && !t.id.starts_with(super::survivors::LEFT)) {
            t.status = "stopped".into();
            t.ended.get_or_insert(now);
            self.state_dirty = true;
        }
        let ids: Vec<String> = self.events.iter().filter(|e| still_open(&e.body)).map(|e| e.id.clone()).collect();
        self.close(&ids, now);
    }

    /// Going on with a harness process after an upgrade: what another harness (the one before a
    /// switch) left running in the timeline, and the tasks it started, can't still be running (an
    /// older image left them so).
    pub fn settle_foreign(&mut self) {
        let Some(now_in) = self.harness.clone() else { return };
        let foreign = |e: &Event| e.by.as_deref().is_some_and(|by| by != now_in);
        let ids: Vec<String> = self.events.iter().filter(|e| foreign(e) && still_open(&e.body)).map(|e| e.id.clone()).collect();
        let theirs: Vec<String> = self
            .state
            .tasks
            .iter()
            .filter(|t| t.live() && !t.id.starts_with(super::survivors::LEFT))
            .filter(|t| t.tool_id.as_deref().and_then(|id| self.get(id)).is_some_and(foreign))
            .map(|t| t.id.clone())
            .collect();
        let now = Self::now();
        for t in self.state.tasks.iter_mut().filter(|t| theirs.contains(&t.id)) {
            t.status = "stopped".into();
            t.ended.get_or_insert(now);
            self.state_dirty = true;
        }
        self.close(&ids, now);
    }

    /// These events end: a call cancelled, text no longer streaming.
    fn close(&mut self, ids: &[String], now: i64) {
        for id in ids {
            let Some(e) = self.get_mut(id) else { continue };
            match &mut e.body {
                Body::Tool(t) if matches!(t.status, ToolStatus::Running | ToolStatus::Pending) => {
                    t.status = ToolStatus::Cancelled;
                    t.ended.get_or_insert(now);
                }
                Body::Assistant { streaming, .. } | Body::Thinking { streaming, .. } => *streaming = false,
                _ => {}
            }
        }
    }

    /// Move everything pending into `out`, in order: changed events, streamed text, then state.
    pub fn flush(&mut self) {
        if self.state_dirty || self.reset {
            for id in bound_tasks(&mut self.state.tasks) {
                self.task_output.remove(&id);
            }
        }
        if std::mem::take(&mut self.reset) {
            let snap = self.snapshot(256 * 1024);
            self.out.extend(snap);
            self.changed.clear();
            self.stream.clear();
            self.state_dirty = false;
            self.meta_dirty = false;
            self.changes_dirty = false;
            return;
        }
        let changed = std::mem::take(&mut self.changed);
        if !changed.is_empty() {
            let events: Vec<Event> = changed.iter().filter_map(|id| self.get(id).cloned()).collect();
            // Text streamed into those same events is already in them.
            self.stream.retain(|(id, _)| !changed.contains(id));
            if !events.is_empty() {
                self.out.push(Update::Events { events });
            }
        }
        for (id, text) in std::mem::take(&mut self.stream) {
            self.out.push(Update::Stream { id, text });
        }
        if self.meta_dirty {
            self.meta_dirty = false;
            self.out.push(Update::Meta { meta: Box::new(self.meta.clone()) });
        }
        if self.changes_dirty {
            self.changes_dirty = false;
            self.out.push(Update::Changes { files: self.changes() });
        }
        if self.state_dirty {
            self.state_dirty = false;
            self.state.left_behind = self.state.count_left_behind();
            self.out.push(Update::State { state: Box::new(self.state.clone()) });
        }
    }

    /// Everything a newly attached viewer needs, as updates (events in bounded batches).
    pub fn snapshot(&self, batch_bytes: usize) -> Vec<Update> {
        // Cleared and nothing since: the store still keeps what came before.
        let from = self.events.front().map(|e| e.seq).or(self.state.cleared);
        let mut state = self.state.clone();
        bound_tasks(&mut state.tasks);
        let mut out = vec![Update::Reset { from }, Update::Meta { meta: Box::new(self.meta.clone()) }, Update::State { state: Box::new(state) }];
        if !self.changes.is_empty() {
            out.push(Update::Changes { files: self.changes() });
        }
        let mut batch: Vec<Event> = Vec::new();
        let mut size = 0usize;
        for e in &self.events {
            let n = serde_json::to_vec(e).map(|v| v.len()).unwrap_or(0);
            if size + n > batch_bytes && !batch.is_empty() {
                out.push(Update::Events { events: std::mem::take(&mut batch) });
                size = 0;
            }
            size += n;
            batch.push(e.clone());
        }
        if !batch.is_empty() {
            out.push(Update::Events { events: batch });
        }
        out
    }

    pub fn take_out(&mut self) -> Vec<Update> {
        self.flush();
        std::mem::take(&mut self.out)
    }

    pub fn take_writes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.writes)
    }

    /// The conversation as Markdown, from the timeline (for harnesses without an export of their
    /// own): what was said, and the tool calls in one line each.
    pub fn export(&self) -> String {
        use super::model::Body;
        let mut out = String::new();
        if !self.state.title.is_empty() {
            out.push_str(&format!("# {}\n\n", self.state.title));
        }
        for e in self.events().filter(|e| e.parent.is_none()) {
            match &e.body {
                Body::User { text, origin, .. } if origin.as_deref() != Some("system") => out.push_str(&format!("## You\n\n{}\n\n", text.trim())),
                Body::Assistant { text, .. } if !text.trim().is_empty() => out.push_str(&format!("## Agent\n\n{}\n\n", text.trim())),
                Body::Tool(t) => out.push_str(&format!("- `{}` {}\n\n", t.name, t.title.lines().next().unwrap_or(""))),
                Body::Notice { text, .. } => out.push_str(&format!("> {}\n\n", text.lines().next().unwrap_or(""))),
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(status: ToolStatus) -> Body {
        use super::super::model::{ToolCall, ToolKind};
        Body::Tool(Box::new(ToolCall {
            tool_id: "t".into(),
            name: "Bash".into(),
            tool: ToolKind::Shell,
            title: "run it".into(),
            detail: None,
            input: serde_json::Value::Null,
            status,
            output: None,
            truncated: false,
            diffs: Vec::new(),
            todos: Vec::new(),
            task: None,
            exit_code: None,
            started: 0,
            ended: None,
            server: None,
            tool_short: None,
            skill: None,
            runner: None,
        }))
    }

    fn said(text: &str) -> Body {
        Body::Assistant { text: text.into(), streaming: false }
    }

    #[test]
    fn a_turn_end_says_what_the_turn_did() {
        let mut core = Core::new();
        core.note_sent("u1");
        core.upsert("u1", None, Body::User { text: "go".into(), images: 0, client_id: Some("u1".into()), origin: Some("human".into()), attachments: Vec::new() });
        core.upsert("a1", None, said("Running it."));
        core.upsert("t1", None, tool(ToolStatus::Done));
        core.upsert("t2", None, tool(ToolStatus::Error));
        core.end_turn(None, "done", Some("end_turn"));
        let t = core.state.last_turn.clone().unwrap_or_default();
        assert_eq!((t.tools, t.after_error, t.asks, t.kept), (2, true, false, 0), "a failed last step, nothing said after it");
        // Words after the failure: not ended on it; a question at the end asks you.
        core.upsert("a2", None, said("That failed on the box. **Should I retry there?**"));
        core.end_turn(None, "done", None);
        let t = core.state.last_turn.clone().unwrap_or_default();
        assert_eq!((t.after_error, t.asks), (false, true));
    }

    #[test]
    fn child_work_cannot_change_the_parent_turns_keep_going_decision() -> anyhow::Result<()> {
        let mut core = Core::new();
        core.upsert("parent-prompt", None, Body::User { text: "go".into(), images: 0, client_id: None, origin: Some("human".into()), attachments: Vec::new() });
        core.upsert("parent-tool", None, tool(ToolStatus::Error));
        core.upsert("child-prompt", Some("spawn".into()), Body::User { text: "audit".into(), images: 0, client_id: None, origin: Some("human".into()), attachments: Vec::new() });
        core.upsert("child-tool", Some("spawn".into()), tool(ToolStatus::Done));
        core.upsert("child-answer", Some("spawn".into()), said("Should I try again?"));
        // Distinct times also exercise the fallback to the parent's prompt when no start
        // timestamp survives a reconnect.
        for (id, at) in [("parent-prompt", 10), ("parent-tool", 20), ("child-prompt", 30), ("child-tool", 40), ("child-answer", 50)] {
            core.get_mut(id).ok_or_else(|| anyhow::anyhow!("missing event {id}"))?.at = at;
        }
        for started in [None, Some(10)] {
            core.end_turn(started, "done", None);
            let turn = core.state.last_turn.as_ref().ok_or_else(|| anyhow::anyhow!("missing turn end"))?;
            assert_eq!((turn.tools, turn.after_error, turn.asks), (1, true, false), "child work changed the parent turn");
        }

        core.upsert("parent-answer", None, said("Should I retry the failed command?"));
        core.upsert("child-error", Some("spawn".into()), tool(ToolStatus::Error));
        core.end_turn(Some(10), "done", None);
        let turn = core.state.last_turn.as_ref().ok_or_else(|| anyhow::anyhow!("missing turn end"))?;
        assert_eq!((turn.tools, turn.after_error, turn.asks), (1, false, true), "the child's error hid the parent's question");
        Ok(())
    }

    #[test]
    fn kiln_keep_going_messages_count_and_a_persons_restarts_the_count() {
        let mut core = Core::new();
        assert_eq!(origin_of("kiln-keep-1"), "kiln");
        assert_eq!(origin_of("m1"), "human");
        core.note_sent(&format!("{KEEP_ID}1"));
        core.note_sent(&format!("{KEEP_ID}2"));
        assert_eq!(core.state.kept, 2);
        core.end_turn(None, "cut", Some("stop_sequence"));
        assert_eq!(core.state.last_turn.as_ref().map(|t| t.kept), Some(2));
        core.note_sent("m2");
        assert_eq!(core.state.kept, 0);
    }

    #[test]
    fn upserts_keep_order_and_stream_deltas_coalesce() {
        let mut c = Core::new();
        c.upsert("a", None, Body::Assistant { text: "he".into(), streaming: true });
        c.append("a", "llo");
        let out = c.take_out();
        // Created this step: one event carrying all the text, no separate stream update.
        assert!(matches!(out.first(), Some(Update::Events { events }) if events.len() == 1 && events.first().is_some_and(|e| matches!(&e.body, Body::Assistant { text, .. } if text == "hello"))));
        assert!(!out.iter().any(|u| matches!(u, Update::Stream { .. })));
        c.append("a", " wor");
        c.append("a", "ld");
        let out = c.take_out();
        assert_eq!(out, vec![Update::Stream { id: "a".into(), text: " world".into() }]);
        c.upsert("b", None, Body::Notice { level: super::super::model::Level::Info, text: "x".into() });
        c.upsert("a", None, Body::Assistant { text: "hello world!".into(), streaming: false });
        let out = c.take_out();
        let ids: Vec<&str> = match out.first() {
            Some(Update::Events { events }) => events.iter().map(|e| e.id.as_str()).collect(),
            _ => Vec::new(),
        };
        assert_eq!(ids, vec!["b", "a"]);
        assert_eq!(c.events().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn streaming_deltas_stop_where_the_kept_text_stops() {
        for body in [
            Body::Assistant { text: "x".repeat(super::super::model::TEXT_MAX - 1), streaming: true },
            Body::Thinking { text: "x".repeat(super::super::model::TEXT_MAX - 1), streaming: true, redacted: false },
        ] {
            let mut c = Core::new();
            c.upsert("a", None, body);
            c.take_out();
            // Keep the existing whole-chunk boundary behavior, including multibyte text.
            c.append("a", "é");
            c.append("a", "discarded");
            assert_eq!(c.take_out(), vec![Update::Stream { id: "a".into(), text: "é".into() }]);
            assert!(c.get("a").is_some_and(|e| matches!(&e.body, Body::Assistant { text, .. } | Body::Thinking { text, .. } if text.len() == super::super::model::TEXT_MAX + 1 && text.ends_with('é'))));
            c.append("a", "also discarded");
            assert!(c.take_out().is_empty());
        }
    }

    #[test]
    fn the_timeline_is_bounded_and_snapshots_batch() {
        let mut c = Core::new();
        for i in 0..(MAX_EVENTS + 10) {
            c.upsert(&format!("e{i}"), None, Body::Assistant { text: "x".repeat(100), streaming: false });
        }
        assert_eq!(c.events().count(), MAX_EVENTS);
        assert_eq!(c.state.dropped, 10);
        assert!(c.get("e0").is_none() && c.get("e10").is_some());
        let snap = c.snapshot(64 * 1024);
        assert!(matches!(snap.first(), Some(Update::Reset { .. })));
        let total: usize = snap.iter().map(|u| if let Update::Events { events } = u { events.len() } else { 0 }).sum();
        assert_eq!(total, MAX_EVENTS);
        assert!(snap.len() > 4);
    }

    #[test]
    fn events_after_a_rewind_go_out() {
        let mut c = Core::new();
        for id in ["one", "reply one", "two", "reply two"] {
            c.upsert(id, None, Body::Assistant { text: id.into(), streaming: false });
        }
        c.take_out();
        c.truncate_from("two");
        let out = c.take_out();
        assert!(matches!(out.first(), Some(Update::Reset { .. })));
        c.upsert("three", None, Body::Assistant { text: "three".into(), streaming: true });
        c.append("three", "!");
        let out = c.take_out();
        assert!(matches!(out.first(), Some(Update::Events { events }) if events.iter().map(|e| e.id.as_str()).collect::<Vec<_>>() == ["three"]));
        c.append("three", "?");
        assert_eq!(c.take_out(), vec![Update::Stream { id: "three".into(), text: "?".into() }]);
        assert_eq!(c.events().map(|e| e.id.as_str()).collect::<Vec<_>>(), vec!["one", "reply one", "three"]);
        assert!(c.get("three").is_some_and(|e| matches!(&e.body, Body::Assistant { text, .. } if text == "three!?")));
    }

    #[test]
    fn changes_wait_for_the_same_file_and_keep_only_its_latest_job() {
        use super::super::model::FileChange;
        let mut c = Core::new();
        let change = |added, at| FileChange {
            path: "a.rs".into(), created: false, deleted: false, added, removed: 1,
            hunks: Vec::new(), truncated: true, at,
        };
        let before = change(1, 1);
        c.set_change("/a.rs", Some(before.clone()), None);
        c.take_out();
        c.note_baseline("/a.rs", Baseline::Reverse(Vec::new()));
        c.want_change("/a.rs", "first.rs");
        let jobs = c.take_change_jobs();
        assert_eq!(jobs.len(), 1);
        c.start_change("/a.rs");
        c.want_change("/a.rs", "old.rs");
        c.want_change("/a.rs", "latest.rs");
        assert_eq!(c.change_jobs.len(), 1);
        assert!(c.take_change_jobs().is_empty());
        // Another file need not wait for this one.
        c.note_baseline("/b.rs", Baseline::Absent);
        c.want_change("/b.rs", "b.rs");
        assert_eq!(c.take_change_jobs().iter().map(|j| j.abs.as_str()).collect::<Vec<_>>(), vec!["/b.rs"]);
        // A superseded result can resolve the original baseline, but cannot replace the shown diff.
        let baseline = Baseline::Text("original\n".into());
        c.set_change("/a.rs", None, Some(baseline.clone()));
        assert_eq!(c.changes(), vec![before]);
        assert!(c.take_out().is_empty());
        assert_eq!(c.take_change_jobs(), vec![ChangeJob { abs: "/a.rs".into(), rel: "latest.rs".into(), baseline }]);
        c.start_change("/a.rs");
        let latest = change(2, 3);
        c.set_change("/a.rs", Some(latest.clone()), None);
        assert_eq!(c.changes(), vec![latest]);
        assert_eq!((c.state.files_changed, c.state.lines_added, c.state.lines_removed), (1, 2, 1));
        assert!(c.take_out().iter().any(|u| matches!(u, Update::Changes { .. })));
    }

    #[test]
    fn unchanged_diffs_do_not_resend_the_changeset() {
        use super::super::model::{FileChange, Hunk};
        let mut c = Core::new();
        c.take_out();
        let first = FileChange {
            path: "a.rs".into(), created: true, deleted: false, added: 1, removed: 0,
            hunks: vec![Hunk { old_start: 0, old_lines: 0, new_start: 1, new_lines: 1, lines: vec!["+one".into()] }],
            truncated: false, at: 1,
        };
        c.set_change("/a.rs", Some(first.clone()), None);
        assert!(c.take_out().iter().any(|u| matches!(u, Update::Changes { files } if files.as_slice() == std::slice::from_ref(&first))));
        let mut same = first.clone();
        same.at = 2;
        c.set_change("/a.rs", Some(same), Some(Baseline::Absent));
        assert!(c.take_out().is_empty());
        assert_eq!(c.baselines.get("/a.rs"), Some(&Baseline::Absent));
        assert_eq!(c.changes(), vec![first.clone()]);
        let mut changed = first;
        changed.at = 3;
        changed.hunks = vec![Hunk { old_start: 0, old_lines: 0, new_start: 1, new_lines: 1, lines: vec!["+two".into()] }];
        c.set_change("/a.rs", Some(changed.clone()), None);
        assert!(c.take_out().iter().any(|u| matches!(u, Update::Changes { files } if files.as_slice() == std::slice::from_ref(&changed))));
        c.set_change("/a.rs", None, None);
        assert!(c.take_out().iter().any(|u| matches!(u, Update::Changes { files } if files.is_empty())));
        assert_eq!((c.state.files_changed, c.state.lines_added, c.state.lines_removed), (0, 0, 0));
        c.set_change("/a.rs", None, None);
        assert!(c.take_out().is_empty());
    }

    fn task(id: &str, tool: Option<&str>, status: &str) -> Task {
        Task { id: id.into(), tool_id: tool.map(str::to_owned), status: status.into(), class: Some("shell".into()), ..Task::default() }
    }

    fn status_of(c: &Core, id: &str) -> Option<ToolStatus> {
        c.get(id).and_then(|e| match &e.body {
            Body::Tool(t) => Some(t.status),
            _ => None,
        })
    }

    #[test]
    fn a_state_keeps_its_tasks_bounded_and_the_live_ones_whole() -> anyhow::Result<()> {
        let mut c = Core::new();
        c.take_out();
        // A long Codex session: a heredoc in nearly every finished terminal, one server running.
        let server = format!("python3 -m http.server 17873 # {}", "s".repeat(30_000));
        c.state.tasks.push(Task { command: Some(server.clone()), ..task("terminal-server", Some("server"), "running") });
        c.state.tasks.push(task("child", Some("spawn"), "paused"));
        // A subagent long finished: its row stays (it's how its thread is found) while shells go.
        c.state.tasks.push(Task { class: Some("agent".into()), ..task("01a15b9d", Some("call_spawn"), "completed") });
        for i in 0..TASKS_KEPT + 48 {
            c.state.tasks.push(Task { command: Some(format!("cat > index.html <<'HTML'\n{}\nHTML", "y".repeat(20_000))), ..task(&format!("terminal-{i}"), Some(&format!("exec-{i}")), "completed") });
        }
        c.task_output.entry("terminal-0".into()).or_default().append(b"old output");
        c.task_output.entry("terminal-60".into()).or_default().append(b"kept output");
        c.touch_state();
        let sent = |out: Vec<Update>| {
            out.into_iter().find_map(|u| match u {
                Update::State { state } => Some(*state),
                _ => None,
            })
        };
        let state = sent(c.take_out()).ok_or_else(|| anyhow::anyhow!("no state"))?;
        assert_eq!(state.tasks.len(), TASKS_KEPT);
        // The live ones whole (stopping a terminal finds it by its whole command), the oldest
        // finished gone with the output kiln kept of them.
        assert_eq!(state.tasks.first().and_then(|t| t.command.clone()), Some(server));
        assert!(state.tasks.iter().any(|t| t.id == "child") && state.tasks.iter().any(|t| t.id == "01a15b9d"));
        assert!(!state.tasks.iter().any(|t| t.id == "terminal-50") && state.tasks.iter().any(|t| t.id == "terminal-51"));
        assert!(!c.task_output.contains_key("terminal-0") && c.task_output.contains_key("terminal-60"));
        let ended = state.tasks.iter().find(|t| t.id == "terminal-60").and_then(|t| t.command.clone()).unwrap_or_default();
        assert!(ended.len() <= ENDED_COMMAND_MAX && ended.starts_with("cat > index.html") && ended.ends_with("\nHTML"), "{}", ended.len());
        assert!(serde_json::to_vec(&Update::State { state: Box::new(state.clone()) })?.len() < 512 * 1024, "fits one frame");
        // Bounded once, it stays as it is.
        c.touch_state();
        assert_eq!(sent(c.take_out()).map(|s| s.tasks), Some(state.tasks));
        Ok(())
    }

    #[test]
    fn what_a_harness_ran_ends_with_it_but_what_kiln_took_over_does_not() {
        let mut c = Core::new();
        c.state.tasks = vec![
            task("terminal-server", Some("server"), "running"),
            Task { class: Some("agent".into()), ..task("child", Some("spawn"), "paused") },
            task("left-4242", None, "running"),
            Task { ended: Some(5), ..task("terminal-done", Some("done"), "failed") },
        ];
        c.upsert("server", None, tool(ToolStatus::Running));
        c.upsert("spawn", None, tool(ToolStatus::Done));
        c.upsert("done", None, tool(ToolStatus::Error));
        c.upsert("child:child:command", Some("spawn".into()), tool(ToolStatus::Pending));
        c.upsert("child:child:said", Some("spawn".into()), Body::Assistant { text: "Checking".into(), streaming: true });
        c.take_out();
        c.end_harness_work();
        let status = |id: &str| c.state.tasks.iter().find(|t| t.id == id).map(|t| (t.status.clone(), t.ended.is_some()));
        assert_eq!(status("terminal-server"), Some(("stopped".to_owned(), true)));
        assert_eq!(status("child"), Some(("stopped".to_owned(), true)));
        assert_eq!(status("left-4242"), Some(("running".to_owned(), false)), "kiln's own, taken back over, runs apart from it");
        assert_eq!(c.state.tasks.iter().find(|t| t.id == "terminal-done").and_then(|t| t.ended), Some(5));
        assert_eq!(
            ["server", "spawn", "done", "child:child:command"].map(|id| status_of(&c, id)),
            [Some(ToolStatus::Cancelled), Some(ToolStatus::Done), Some(ToolStatus::Error), Some(ToolStatus::Cancelled)]
        );
        assert!(c.get("server").is_some_and(|e| matches!(&e.body, Body::Tool(t) if t.ended.is_some())));
        // All of it stays to read.
        assert!(c.get("child:child:said").is_some_and(|e| matches!(&e.body, Body::Assistant { text, streaming: false } if text == "Checking")));
        assert_eq!(c.events().count(), 5);
        assert!(c.take_out().iter().any(|u| matches!(u, Update::State { .. })));
    }

    #[test]
    fn going_on_after_an_upgrade_settles_only_what_the_harness_before_a_switch_left_open() {
        let mut c = Core::new();
        c.harness = Some("Codex".into());
        c.upsert("exec-server", None, tool(ToolStatus::Running));
        c.state.tasks.push(task("terminal-exec-server", Some("exec-server"), "running"));
        c.harness = Some("Claude Code".into());
        c.upsert("toolu_1", None, tool(ToolStatus::Running));
        c.upsert("msg:0", None, Body::Assistant { text: "Working".into(), streaming: true });
        c.state.tasks.push(task("b7", Some("toolu_1"), "running"));
        c.settle_foreign();
        assert_eq!([status_of(&c, "exec-server"), status_of(&c, "toolu_1")], [Some(ToolStatus::Cancelled), Some(ToolStatus::Running)]);
        assert_eq!(c.state.tasks.iter().map(|t| t.status.as_str()).collect::<Vec<_>>(), ["stopped", "running"]);
        assert!(c.get("msg:0").is_some_and(|e| matches!(&e.body, Body::Assistant { streaming: true, .. })));
    }

    #[test]
    fn kept_task_output_reads_from_an_offset() {
        let mut o = TaskOutput::default();
        o.append(b"hello ");
        o.append(b"world");
        assert_eq!(o.since(0), (b"hello world".to_vec(), 11, false));
        assert_eq!(o.since(6), (b"world".to_vec(), 11, false));
        assert_eq!(o.since(11), (Vec::new(), 11, false));
        let big = vec![b'x'; TASK_OUTPUT_MAX + 10];
        o.append(&big);
        let (bytes, offset, truncated) = o.since(0);
        assert_eq!((bytes.len(), offset, truncated), (TASK_OUTPUT_MAX, 11 + TASK_OUTPUT_MAX as u64 + 10, true));
    }
}
