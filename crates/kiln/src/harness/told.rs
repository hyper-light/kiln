//! A parent hears from its children: when an agent another agent started settles (its turn over,
//! a person needed, stopped, ended, paused at a usage limit), kiln tells its parent, as Claude
//! Code tells an agent of its own background subagents: a message after the parent's turn, which
//! starts one if the parent is idle. So a parent needn't wait for its children: it can start
//! them, end its turn, and take up what each concluded when it's told. `agents_wait` is for the
//! parent that would rather wait; what it saw that way (or through `agents_result`) isn't told
//! again. Children that settle together are told of in one message.
//!
//! News is explicitly replaceable status. Each changed snapshot has a fresh immutable message
//! id and uses the internal-news ledger stream; every accepted original stays in history. Only
//! the latest never-dispatched successor is coalesced. An already dispatched snapshot is frozen
//! until its exact source receipt, and a parent seeing a line does not erase that original.
//!
//! An agent's runs on kiln's machines are told of the same way, in the same message (docs/reach.md
//! §7; agents are told, not polled): a run it left in the background (`run` with `background`, or
//! past its wait; `kiln.machines.start`) or detached (`kiln run --detach` whose client went) has a
//! line of its own there once it ends (where, how long, its exit code, the end of its output). What
//! follows the run says so ([`RunNews`], from the agent's MCP server or its runner,
//! `crate::reach::told`), in order, through one sender (T10); what the agent saw itself (`output`,
//! `stop`) comes out of it. A run's start, and where a queued run stands ([`answer`], from
//! placement), are state: they join a message that goes anyway, and wake no one (T8). An agent that
//! ended its turn waiting for runs it handed over in that turn isn't prodded while they go
//! ([`runs_awaited`], T2). What kiln holds of runs survives a restart (`told_runs`, E1).

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::model::{Command, SendMode, Status};
use super::queued::{Kept, NEWS, Now};
use super::session::Info;
use super::wait::{settled, stand};

/// The most of a child's last words a message carries (`agents_result` has them whole).
const SAID_MAX: usize = 400;
/// The most children a parent's news names, those that settled last: past them it says how many
/// more did. Its runs count among them.
const LINES_MAX: usize = 64;
/// The most of one line a news message can show: a message is kept up to
/// [`super::model::TEXT_MAX`] and names at most [`LINES_MAX`] lines, so this is each line's share
/// of it. A run's line (its command, its error, the end of its output) is clipped to it where the
/// text is made (`RunNews::clipped`), and the end of its output to what's left of it after the rest
/// of the line (`run_line`): no more is ever carried than the message shows (T11, T13).
pub const LINE_SHARE: usize = super::model::TEXT_MAX / LINES_MAX;
/// How a run's line is keyed among its agent's news (a child's by the child's id).
const RUN: &str = "run:";

/// A retransmission timeout from measured round trips, as RFC 6298 computes it: SRTT and RTTVAR
/// from each sample (§2.2, §2.3, α = 1/8, β = 1/4), RTO = SRTT + max(G, 4·RTTVAR), at least 1 s
/// (§2.4), 1 s before any sample (§2.1), doubled on each expiry (§5.5) up to 60 s (§2.5's floor for
/// a maximum). What retries news (`crate::reach::told`) waits it; how long a child stands settled
/// before its parent is told is it, over the measured delivery of queued messages (T13).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rto {
    /// SRTT and RTTVAR in ms, once a sample came.
    est: Option<(u64, u64)>,
    /// Expiries since the last sample (§5.5).
    backoff: u32,
}

impl Rto {
    /// RFC 6298 §2.1: before any sample.
    pub const INITIAL_MS: u64 = 1_000;
    /// RFC 6298 §2.4: an RTO under a second is rounded up to one.
    pub const MIN_MS: u64 = 1_000;
    /// RFC 6298 §2.5: a maximum, if any, is at least 60 s.
    pub const MAX_MS: u64 = 60_000;
    /// The clock's granularity, ms (what [`crate::util::now_ms`] counts in).
    const G_MS: u64 = 1;

    /// A round trip of `ms` measured (Karn: only of what wasn't sent again).
    pub fn sample(&mut self, ms: u64) {
        self.est = Some(match self.est {
            None => (ms, ms / 2),
            Some((srtt, rttvar)) => {
                let rttvar = rttvar - rttvar / 4 + srtt.abs_diff(ms) / 4;
                (srtt - srtt / 8 + ms / 8, rttvar)
            }
        });
        self.backoff = 0;
    }

    /// The timer expired with no answer: the next waits twice as long (§5.5).
    pub fn expired(&mut self) {
        self.backoff = self.backoff.saturating_add(1).min(u64::BITS);
    }

    /// The timeout now, ms.
    pub fn ms(&self) -> u64 {
        let base = self.est.map_or(Self::INITIAL_MS, |(srtt, rttvar)| srtt.saturating_add(Self::G_MS.max(rttvar.saturating_mul(4))));
        let base = base.clamp(Self::MIN_MS, Self::MAX_MS);
        base.checked_shl(self.backoff).unwrap_or(Self::MAX_MS).min(Self::MAX_MS)
    }

    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.ms())
    }
}

/// What became of a run an agent left in the background or detached, in order: later news of a
/// run counts, earlier news that came late doesn't (`Told::ran`).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RunEvent {
    /// Handed to its agent while it goes, nothing waiting on it: work the agent has going, not news.
    #[default]
    Going,
    /// Waiting for a machine with room, at a place in line (`answer`): state, not a wake.
    Queued,
    /// It started after the answer that handed it over said it hadn't: state, not a wake (T8).
    Started,
    /// It ended with nothing waiting on it.
    Ended,
    /// It will never start (`answer`): no machine kiln knows can run it.
    Never,
    /// The agent saw it end itself (`output`, `stop`, a call that waited on it).
    Seen,
}

impl RunEvent {
    /// Over: nothing more of it is to come but its agent's look.
    pub fn over(self) -> bool {
        self >= RunEvent::Ended
    }

    /// News that wakes its agent: an end (Going, Queued and Started are state, T8).
    fn wakes(self) -> bool {
        matches!(self, RunEvent::Ended | RunEvent::Never)
    }
}

/// News of one of an agent's runs, as what follows it says it (`crate::reach::told`; the server
/// hears it as `crate::reach::Question::Told`). Times are unix ms. Two equal are one (T9).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RunNews {
    /// The run, unique among its agent's: its follower's name for it.
    pub key: String,
    /// What the agent calls it: `output`'s id (`r3`); none for a `kiln run`, or once what knew it
    /// by that has gone (its MCP server ended, T7).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub id: String,
    pub event: RunEvent,
    pub command: String,
    /// The runner it runs on.
    pub on: String,
    /// How it was started: `run`, `kiln.machines.start`, `kiln run --detach`.
    pub how: String,
    /// Times its agent was handed it while it went: an answer that hands it over again (`output`
    /// that returned with it going) is news of its own, the agent waiting on it in that turn (T2).
    #[serde(skip_serializing_if = "is_zero32")]
    pub handed: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Why it ended with no exit code, or why it will never start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Bytes of its output that were dropped, unread.
    #[serde(skip_serializing_if = "is_zero")]
    pub lost: u64,
    /// The end of its output, [`LINE_SHARE`] bytes at most ([`clip_tail`]).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub tail: String,
    /// Queued: its place in line (1: next).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub place: Option<u32>,
    /// Queued: when it's expected to start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_start: Option<i64>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

fn is_zero32(n: &u32) -> bool {
    *n == 0
}

impl RunNews {
    /// Its text clipped where it's made to what a news message shows of a line ([`LINE_SHARE`]):
    /// the command's head, the error's head, the output's end (T11).
    #[must_use]
    pub fn clipped(mut self) -> Self {
        self.command = clip_head(&self.command, LINE_SHARE);
        self.error = self.error.map(|e| clip_head(&e, LINE_SHARE));
        self.tail = clip_tail(&self.tail, LINE_SHARE);
        self.on = clip_head(&self.on, LINE_SHARE);
        self.how = clip_head(&self.how, LINE_SHARE);
        self
    }
}

/// The start of `s`, at most `max` bytes, cut at a character's boundary.
pub fn clip_head(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or_default().to_owned()
}

/// The end of `output`, at most `max` bytes, from the start of a line when one starts within them;
/// trailing blank space trimmed. It looks at no more of `output` than its last `max` bytes and the
/// blank space after them, however long `output` is (T11: it was scanned whole, twice, under the
/// run's lock).
pub fn clip_tail(output: &str, max: usize) -> String {
    let out = output.trim_end();
    let mut start = out.len().saturating_sub(max);
    while !out.is_char_boundary(start) {
        start += 1;
    }
    let tail = out.get(start..).unwrap_or_default();
    if start == 0 {
        return tail.to_owned();
    }
    match tail.find('\n') {
        Some(at) if at + 1 < tail.len() => tail.get(at + 1..).unwrap_or(tail).to_owned(),
        _ => tail.to_owned(),
    }
}

/// One run kiln was told of ([`ran`]).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct Ran {
    /// The latest news of it.
    news: RunNews,
    /// When its agent was last handed it (the server's clock): it holds its agent back only after
    /// a turn that ended since then (T2).
    handed_at: i64,
    /// The runner that follows it, the only one whose news of it counts (E8).
    runner: String,
    /// How many of that runner's news kiln had heard when this came ([`Told::heard`]): once a later
    /// one comes, the runner had this one's answer, and won't say it again (the dedupe window, T13).
    heard_at: u64,
    /// Its latest news was told, or has nothing to tell.
    told: bool,
    /// It holds its agent back while it goes: not once the runner that follows it is away (nothing
    /// is left to say how it ends until it's back and says it again, T4).
    held: bool,
}

impl Ran {
    fn new(news: RunNews, now: i64, runner: &str, heard_at: u64) -> Self {
        let told = matches!(news.event, RunEvent::Going | RunEvent::Seen);
        Self { news, handed_at: now, runner: runner.to_owned(), heard_at, told, held: true }
    }

    /// Still going, as its follower last said, and its follower here.
    fn going(&self) -> bool {
        self.held && !self.news.event.over()
    }

    /// Over, and nothing more of it to tell.
    fn finished(&self) -> bool {
        self.told && self.news.event.over()
    }
}

/// When kiln saw an agent's turns end (the server's clock): runs handed over after the one before
/// the last are those it ended its last turn waiting for (T2).
#[derive(Debug, Clone, Copy, Default)]
struct Turns {
    /// Its last turn's end, as its harness stamps it.
    at: i64,
    /// When kiln first saw that end.
    seen: i64,
    /// When kiln saw the end of the turn before it (0: none seen since this server started).
    before: i64,
}

/// What was last seen of a child.
#[derive(Debug, Clone, Default)]
struct Seen {
    /// Its stand and the end of its last turn.
    key: String,
    /// Since when it has stood so.
    since: i64,
    /// Seen at work since its parent was last told: only a settle after work is told of.
    worked: bool,
    /// The settle its parent was told of, or saw itself.
    told: String,
    /// Messages waited in its queue when last seen.
    queued: bool,
    /// Since when it has stood settled right after messages waited in its queue (its harness was
    /// handed them): its next work is their delivery, which [`Told::settle`] measures (T13).
    queued_since: Option<i64>,
}

/// A parent's news, waiting for it: each child's latest line, by child, the latest last.
#[derive(Debug, Default)]
struct News {
    /// The message that holds it, while it may still wait in the parent's queue (none: not sent
    /// yet, the parent's machine away).
    msg: Option<Kept>,
    /// What that message said when this server found it waiting, queued before it started: said
    /// first, as it was.
    before: Option<String>,
    lines: Vec<(String, String)>,
    /// Children whose lines went to make room (`LINES_MAX`).
    more: usize,
    /// Runs whose lines went to make room.
    more_runs: usize,
    /// The lines the message says, as kiln last had it say them (as `Kept::text`).
    said: Vec<(String, String)>,
    /// The lines that reach the parent should the message go now (as `Kept::held`).
    held: Vec<(String, String)>,
    /// The words its message was given, sent with and then each edit, the newest last
    /// (`queued::stable` of each, [`GIVEN_MAX`] of them) and the lines each said: once its runner
    /// says what its harness was handed (`Info::said`), those lines reached the parent, whatever
    /// kiln had it say since (an edit too late, or one its own read of a child made: `saw`).
    given: Vec<(String, Vec<(String, String)>)>,
    custody: NewsCustody,
    said_more: (usize, usize),
    previous: Option<super::mail::Receipt>,
}

#[derive(Debug, Default)]
enum NewsCustody { #[default] Legacy, Pending, Retained(super::mail::Receipt) }

const PRODUCER_PREFIX: &str = "told/news/";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct Desired {
    before: Option<String>,
    lines: Vec<(String, String)>,
    more: usize,
    more_runs: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Owned {
    Quiet,
    Pending { original: Command },
    Retained { original: Command, receipt: super::mail::Receipt },
}

impl Owned {
    fn original(&self) -> Option<&Command> { match self { Self::Quiet => None, Self::Pending { original } | Self::Retained { original, .. } => Some(original) } }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct ProducerContent {
    owned: Owned,
    desired: Desired,
    said: Vec<(String, String)>,
    said_more: (usize, usize),
    previous: Option<super::mail::Receipt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProducerRecord {
    version: i64,
    content: ProducerContent,
    captured_runs: Vec<(String, i64)>,
}

struct ProducerWrite { parent: String, record: ProducerRecord, json: String }
struct Persist { runs: Vec<Row>, producers: Vec<ProducerWrite> }

#[derive(Debug, Serialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum Refusal { Rejected { reason: String }, Persistence { error: super::mail::Error } }
impl std::fmt::Display for Refusal { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{self:?}") } }
impl std::error::Error for Refusal {}
impl From<super::mail::Error> for Refusal { fn from(error: super::mail::Error) -> Self { Self::Persistence { error } } }
impl Refusal { fn status(&self) -> u16 { match self { Self::Rejected { .. } => http::StatusCode::FORBIDDEN.as_u16(), Self::Persistence { error } => error.status_code().as_u16() } } }

/// The most of a news message's words kept to tell which its harness was handed.
const GIVEN_MAX: usize = 16;

impl News {
    fn text(&self) -> String {
        text_of(self.before.as_deref(), &self.lines, self.more, self.more_runs)
    }
}

/// News message words `text`, saying `lines`, given to it (`given`: [`News::given`]).
fn gave(given: &mut Vec<(String, Vec<(String, String)>)>, text: &str, lines: &[(String, String)]) {
    given.push((super::queued::stable(text), lines.to_vec()));
    let over = given.len().saturating_sub(GIVEN_MAX);
    given.drain(..over);
}

/// What kiln knows of children and of their parents' news: the server's is [`TOLD`].
#[derive(Debug, Default)]
struct Told {
    /// Each child, by id.
    seen: HashMap<String, Seen>,
    /// Each parent's news, by parent.
    news: HashMap<String, News>,
    /// Each agent's runs it was told of ([`ran`]), by agent, in the order they came.
    runs: HashMap<String, Vec<Ran>>,
    /// How many news of runs each runner said, by runner ([`Ran::heard_at`]).
    heard: HashMap<String, u64>,
    /// When each agent's turns were seen to end ([`Turns`]).
    turns: HashMap<String, Turns>,
    /// How long a queued message takes to reach a child's harness, as measured ([`Seen::queued_since`]):
    /// a child stands settled this long before its parent is told (a wait under way for it returns
    /// first, and a settle only between two steps isn't reported). It was 3 s, picked (T13).
    settle: Rto,
    /// The runs kept (`told_runs`) were read in.
    loaded: bool,
    /// Runs changed or gone since they were last written, by agent and key ([`flush`]).
    dirty: BTreeSet<(String, String)>,
    news_dirty: BTreeSet<String>,
    captured_runs: HashMap<(String, String), i64>,
    captured_news: HashMap<String, i64>,
    removed: HashMap<String, Option<super::mail::Receipt>>,
    producers_loaded: HashSet<String>,
}

static TOLD: LazyLock<Mutex<Told>> = LazyLock::new(Default::default);
/// What goes to each parent, in the order it was decided: `tick` and `saw` queue it here as they
/// decide it (under `TOLD`'s lock, so nothing decided after it goes before it), and one sender a
/// parent sends it, each command once the one before it reached the parent's machine or is said
/// unsent. Two edits of one message can't cross on their way, and nothing that decides waits on a
/// parent's machine: a command waits up to 30 s for an answer, and one lock held across every
/// parent's sends in turn once held `agents_wait` and `agents_result` waiting on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SenderState { Sending, Blocked }
struct Outbound { commands: VecDeque<Command>, state: SenderState }
static TO_SEND: LazyLock<Mutex<HashMap<String, Outbound>>> = LazyLock::new(Default::default);

/// Queues `c` for `parent` (its sender starts when nothing waited for it): true when it starts one,
/// which [`send_queued`] then is. Taken under `TOLD`'s lock alone (never the other way around).
fn queue_for(parent: &str, c: Command) -> bool {
    let mut g = TO_SEND.lock();
    let q = g.entry(parent.to_owned()).or_insert_with(|| Outbound { commands: VecDeque::new(), state: SenderState::Blocked });
    q.commands.push_back(c);
    let start = q.state == SenderState::Blocked;
    q.state = SenderState::Sending;
    start
}

/// A real source capacity/connection event may retry a producer-owned, unaccepted original.
/// One sender per parent is retained; ledger changes never create a waiter per message.
pub fn wake(hub: &Arc<crate::hub::Hub>, parent: &str) {
    let dirty = TOLD.lock().news_dirty.contains(parent);
    let mut sends = TO_SEND.lock();
    if dirty { sends.entry(parent.to_owned()).or_insert_with(|| Outbound { commands: VecDeque::new(), state: SenderState::Blocked }); }
    let start = sends.get_mut(parent).is_some_and(|q| {
        if q.state != SenderState::Blocked || (q.commands.is_empty() && !dirty) { return false; }
        q.state = SenderState::Sending; true
    });
    drop(sends);
    if start { start_senders(hub, vec![parent.to_owned()]); }
}

/// Only committed receipt metadata from the authenticated internal producer reaches this hook.
pub fn mail_progress(hub: &Arc<crate::hub::Hub>, parent: &str, receipt: &super::mail::Receipt, mail_scope: &str) {
    if mail_scope != super::mail::scope("server") { return; }
    let start = {
        let mut told = TOLD.lock();
        told.mail_progress(parent, receipt).is_some_and(|c| queue_for(parent, c))
    };
    if start { start_senders(hub, vec![parent.to_owned()]); }
    wake(hub, parent);
}

/// `parent`'s sender: what's queued for it, in order, each once the one before it went (or is said
/// unsent, its words to go at the next look). It ends once nothing's left.
async fn send_queued(hub: Arc<crate::hub::Hub>, parent: String) {
    loop {
        if let Err(error) = flush(&hub).await {
            if let Some(q) = TO_SEND.lock().get_mut(&parent) { q.state = SenderState::Blocked; }
            tracing::warn!(agent=%parent, error=?error, "internal news original was not persisted; custody remains with its producer");
            return;
        }
        // The front stays queued while it goes: what's queued meanwhile waits behind it.
        let next = {
            let mut sends = TO_SEND.lock();
            let next = sends.get(&parent).and_then(|q| q.commands.front().cloned());
            if next.is_none() { sends.remove(&parent); }
            next
        };
        let Some(c) = next else { return };
        let sent = if matches!(c, Command::Send { .. }) {
            hub.harness.command_status(&hub, &parent, c.clone(), super::mail::StatusKey { task: None, key: "internal-news".into() }).await
        } else { hub.harness.command(&hub, &parent, c.clone()).await };
        match sent {
            super::registry::Sent::Published(receipt) => mail_progress(&hub, &parent, &receipt, &super::mail::scope("server")),
            sent if sent.went() => {}
            _ => {
                if let Command::Send { id, .. } = &c
                    && let Ok(Some(receipt)) = super::mail::receipt(&hub.store, "server".into(), "server".into(), parent.clone(), id.clone()).await
                {
                    // One completion-event reconciliation handles a commit whose response was
                    // lost. This is not a capacity probe or a periodic publisher retry.
                    mail_progress(&hub, &parent, &receipt, &super::mail::scope("server"));
                } else {
                // No custody transfer: retain this exact original at the existing sender. Desired
                // lines may change, but no new revision is generated until this one is accepted.
                if let Some(q) = TO_SEND.lock().get_mut(&parent) { q.state = SenderState::Blocked; }
                tracing::info!(agent = %parent, "internal news retains its original until a source event permits publication");
                return;
                }
            }
        }
        let mut g = TO_SEND.lock();
        let done = g.get_mut(&parent).is_none_or(|q| {
            q.commands.pop_front();
            q.commands.is_empty()
        });
        if done {
            g.remove(&parent);
            return;
        }
    }
}

/// Starts a sender for each of `parents` (those [`queue_for`] said have none).
fn start_senders(hub: &Arc<crate::hub::Hub>, parents: Vec<String>) {
    for parent in parents {
        drop(crate::rt::spawn(send_queued(Arc::clone(hub), parent)));
    }
}

fn key_of(info: &Info) -> String {
    format!("{}:{}", stand(info), info.last_turn.as_ref().map_or(0, |t| t.at))
}

/// Its parent saw `child` as it stands now (`agents_wait`, `agents_result`): it isn't told of it
/// again, and its line comes out of the parent's news waiting in its queue (the message with it,
/// once no line is left).
pub async fn saw(hub: &Arc<crate::hub::Hub>, child: &Info) {
    if let Err(error) = load(hub).await { tracing::warn!(error=?error,"internal news canonical state unavailable"); return; }
    if let Some(parent) = child.parent.as_deref() && let Err(error) = load_parent(hub,parent).await { tracing::warn!(error=?error,"internal news producer state unavailable"); return; }
    // Decided and queued as one: an edit decided later can't go before it.
    let start = {
        let mut told = TOLD.lock();
        told.saw(child).and_then(|(parent, c)| queue_for(&parent, c).then_some(parent))
    };
    if let Err(error) = flush(hub).await { if let Some(parent) = &start && let Some(q) = TO_SEND.lock().get_mut(parent) { q.state = SenderState::Blocked; } tracing::warn!(error=?error,"internal news seen-state not persisted"); return; }
    start_senders(hub, start.into_iter().collect());
}

/// News of one of `session`'s runs, from runner `runner` (`crate::reach::Question::Told`), heard
/// before any check of where the session is now or who holds it (T5: a detached run's runner may
/// no longer host its session, and news is no act). It counts only from the runner that follows
/// the run: the first news of a run from one where the session is now, the rest from that one
/// (E8: no runner tells an agent of runs it doesn't follow). Kept (and written, E1), and told in
/// the agent's news at the next look ([`tick`]); a run its agent saw end itself comes out of the
/// news waiting, as a child it saw does ([`saw`]).
pub async fn ran(hub: &Arc<crate::hub::Hub>, runner: &str, session: &str, news: RunNews) -> Result<(), Refusal> {
    load(hub).await?;
    let Some(info) = hub.harness.info(session) else { return Err(Refusal::Rejected { reason: format!("kiln knows no agent session {session:?}") }) };
    if info.status == Status::Exited {
        return Err(Refusal::Rejected { reason: format!("agent session {session} has ended") });
    }
    load_parent(hub, session).await?;
    let located = hub.harness.located_on(session, runner);
    let start = {
        let mut told = TOLD.lock();
        let c = told.ran(runner, located, session, news.clipped(), crate::util::now_ms()).map_err(|reason| Refusal::Rejected { reason })?;
        c.and_then(|c| queue_for(session, c).then(|| session.to_owned()))
    };
    if let Err(error) = flush(hub).await { if let Some(parent) = &start && let Some(q) = TO_SEND.lock().get_mut(parent) { q.state = SenderState::Blocked; } return Err(error.into()); }
    start_senders(hub, start.into_iter().collect());
    Ok(())
}

/// [`ran`], answered as a runner's question is (`crate::mcp::runner_ask`).
pub async fn heard(hub: &Arc<crate::hub::Hub>, runner: &str, session: &str, news: &RunNews) -> serde_json::Value {
    match ran(hub, runner, session, news.clone()).await {
        Ok(()) => serde_json::json!({"ok": {}}),
        Err(e) => serde_json::json!({"error": e.to_string(), "refusal": e, "status": e.status()}),
    }
}

/// What placement says of a run an agent asked for, as it changes (docs/needs.md E5): queued (its
/// place and when it's expected to start), started, or never. It folds into the agent's one
/// message of news: queued and started are state, edited into a message that waits (no new turn,
/// T8); never wakes the agent, as an end does. Later news of the run (its end, from what follows
/// it) takes its line's place.
pub async fn answer(hub: &Arc<crate::hub::Hub>, session: &str, news: AnswerNews) {
    if let Err(error) = load(hub).await { tracing::warn!(error=?error, "internal news canonical state unavailable"); return; }
    if let Err(error) = load_parent(hub, session).await { tracing::warn!(error=?error, "internal news producer state unavailable"); return; }
    let run = news.into_run();
    let start = {
        let mut told = TOLD.lock();
        // kiln's own word: no runner's, so the runner that follows the run may say the rest.
        match told.ran("", true, session, run.clipped(), crate::util::now_ms()) {
            Ok(c) => c.and_then(|c| queue_for(session, c).then(|| session.to_owned())),
            Err(e) => {
                tracing::debug!(%session, error = %e, "placement's answer for a run not told");
                None
            }
        }
    };
    if let Err(error) = flush(hub).await { if let Some(parent) = &start && let Some(q) = TO_SEND.lock().get_mut(parent) { q.state = SenderState::Blocked; } tracing::warn!(error=?error, "internal news original not persisted"); return; }
    start_senders(hub, start.into_iter().collect());
}

/// Where placement's answer for a queued run stands ([`answer`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Waiting for room: `place` in line (1: next), expected to start at `expected_start` (unix
    /// ms) when kiln can say.
    Queued { place: u32, expected_start: Option<i64> },
    /// It started on runner `on`.
    Started { on: String },
    /// It will never start, and why (no machine kiln knows can run it).
    Never { why: String },
}

/// Placement's answer for one of an agent's runs ([`answer`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerNews {
    /// The run among the agent's: what follows it names it (`RunNews::key`) when placement knows
    /// that name, else placement's own id for it. News of one key is one line.
    pub key: String,
    /// What the agent calls it (`output`'s id), when it has one.
    pub id: String,
    /// Its command, as the agent gave it.
    pub command: String,
    pub answer: Answer,
}

impl AnswerNews {
    /// Placement's answer as its asking runner says it (`crate::reach::Question::Answer`): queued,
    /// started or never; none for anything else.
    pub fn of(news: &RunNews) -> Option<AnswerNews> {
        let answer = match news.event {
            RunEvent::Queued => Answer::Queued { place: news.place.unwrap_or(0), expected_start: news.expected_start },
            RunEvent::Started => Answer::Started { on: news.on.clone() },
            RunEvent::Never => Answer::Never { why: news.error.clone().unwrap_or_default() },
            RunEvent::Going | RunEvent::Ended | RunEvent::Seen => return None,
        };
        Some(AnswerNews { key: news.key.clone(), id: news.id.clone(), command: news.command.clone(), answer })
    }

    fn into_run(self) -> RunNews {
        let mut run = RunNews { key: self.key, id: self.id, command: self.command, how: "run".to_owned(), ..RunNews::default() };
        match self.answer {
            Answer::Queued { place, expected_start } => {
                run.event = RunEvent::Queued;
                run.place = Some(place);
                run.expected_start = expected_start;
            }
            Answer::Started { on } => {
                run.event = RunEvent::Started;
                run.on = on;
                run.started_at = Some(crate::util::now_ms());
            }
            Answer::Never { why } => {
                run.event = RunEvent::Never;
                run.error = Some(why);
            }
        }
        run
    }
}

/// Whether `agent` ended its last turn waiting for runs that go still: runs it was handed in that
/// turn (or handed again then, an `output` that returned with one going), as what follows them
/// last said, their runners here. It isn't reminded of its goal nor kept going (it's told when
/// each ends), as one whose children work isn't. A dev server it started turns ago holds nothing
/// (T2: any run going held it back for the run's whole life).
pub fn runs_awaited(agent: &Info) -> bool {
    TOLD.lock().awaited(agent, crate::util::now_ms())
}

/// The runs kept (`told_runs`) read in, once: what a server before this one heard (E1).
async fn load(hub: &Arc<crate::hub::Hub>) -> Result<(), super::mail::Error> {
    if TOLD.lock().loaded { return Ok(()); }
    let rows = read_rows(&hub.store).await.map_err(persistence_error)?;
    TOLD.lock().adopt(rows);
    Ok(())
}

/// Dirty keys stay owned until the outer Store::call has committed. A failure never restores
/// old values over newer events and never acknowledges a producer original that was not kept.
async fn flush(hub: &Arc<crate::hub::Hub>) -> Result<(), super::mail::Error> {
    let batch = TOLD.lock().capture()?;
    if batch.runs.is_empty() && batch.producers.is_empty() { return Ok(()); }
    let mut bytes = Bytes(0);
    serde_json::to_writer(&mut bytes, &batch.runs).map_err(|e| super::mail::Error::Unavailable { reason: e.to_string() })?;
    let mut confirmed = HashSet::new();
    for producer in &batch.producers {
        use std::io::Write as _;
        bytes.write_all(format!("{PRODUCER_PREFIX}{}", producer.parent).as_bytes()).map_err(|e| super::mail::Error::Unavailable { reason: e.to_string() })?;
        bytes.write_all(producer.json.as_bytes()).map_err(|e| super::mail::Error::Unavailable { reason: e.to_string() })?;
        if let Some(previous) = &producer.record.content.previous {
            let receipt = super::mail::receipt(&hub.store, "server".into(), "server".into(), producer.parent.clone(), previous.id.clone()).await?;
            if receipt.as_ref().is_none_or(|r| r.revision != previous.revision) { return Err(super::mail::Error::Unavailable { reason: "previous news original has no authoritative archive receipt".into() }); }
            confirmed.insert((producer.parent.clone(), previous.id.clone()));
        }
    }
    let rows = batch.runs.len().checked_add(batch.producers.len()).and_then(|n| u64::try_from(n).ok()).ok_or_else(|| super::mail::Error::Unavailable { reason: "producer row count overflow".into() })?;
    storage_room(hub, bytes.0, rows).await?;
    let batch = hub.store.call(move |c| write_persist(c, batch, &confirmed)).await.map_err(persistence_error)?;
    TOLD.lock().acknowledge(&batch);
    Ok(())
}

/// A run as `told_runs` keeps it, by agent and key (none: gone), and the version it's written at:
/// taken as it's read from [`TOLD`], so a later state always has a later version.
type Row = (String, String, Option<Ran>, i64);

/// The version the next row is written at: past every one kept ([`read_rows`] raises it).
static VERSION: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);

fn next_version() -> Result<i64, super::mail::Error> {
    VERSION.try_update(std::sync::atomic::Ordering::Relaxed, std::sync::atomic::Ordering::Relaxed, |v| v.checked_add(1))
        .map_err(|_| super::mail::Error::Unavailable { reason: "internal news version exhausted".into() })
}

fn persistence_error(error: anyhow::Error) -> super::mail::Error {
    if let Some(error) = error.downcast_ref::<super::mail::Error>() { return error.clone(); }
    if matches!(error.downcast_ref::<rusqlite::Error>(), Some(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::DiskFull) { return super::mail::Error::StorageFull; }
    super::mail::Error::Unavailable { reason: error.to_string() }
}

fn original_id(command: &Command) -> Option<&str> { match command { Command::Send { id, .. } if !id.is_empty() => Some(id), _ => None } }

fn content(news: Option<&News>, previous: Option<super::mail::Receipt>) -> ProducerContent {
    let Some(n) = news else { return ProducerContent { owned: Owned::Quiet, desired: Desired::default(), said: Vec::new(), said_more: (0, 0), previous } };
    let original = n.msg.as_ref().map(|m| Command::Send { id: m.id.clone(), text: m.text.clone(), mode: SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: None });
    let owned = match (&n.custody, original) {
        (NewsCustody::Pending, Some(original)) => Owned::Pending { original },
        (NewsCustody::Retained(receipt), Some(original)) => {
            let mut receipt = receipt.clone(); receipt.repeat = false;
            Owned::Retained { original, receipt }
        }
        _ => Owned::Quiet,
    };
    ProducerContent { owned, desired: Desired { before: n.before.clone(), lines: n.lines.clone(), more: n.more, more_runs: n.more_runs }, said: n.said.clone(), said_more: n.said_more, previous: n.previous.clone() }
}

fn restore_news(record: &ProducerRecord) -> Result<News, super::mail::Error> {
    let d = &record.content.desired;
    let mut n = News { before: d.before.clone(), lines: d.lines.clone(), more: d.more, more_runs: d.more_runs, said: record.content.said.clone(), said_more: record.content.said_more, previous: record.content.previous.clone(), ..News::default() };
    if let Some(original) = record.content.owned.original() {
        let Command::Send { id, text, .. } = original else { return Err(super::mail::Error::Invalid { reason: "saved internal news is not a send".into() }); };
        if id.is_empty() { return Err(super::mail::Error::Invalid { reason: "saved internal news has no id".into() }); }
        // The saved body is known, but this record proves producer custody only. Reset the
        // legacy constructor's source observation before it is visible to the planner.
        let mut message = Kept::found(id.clone(), text.clone(), 0); message.awaiting_source();
        n.msg = Some(message);
        n.custody = match &record.content.owned { Owned::Retained { receipt, .. } => NewsCustody::Retained(receipt.clone()), _ => NewsCustody::Pending };
    }
    Ok(n)
}

fn read_producer(c: &rusqlite::Connection, parent: &str) -> anyhow::Result<Option<ProducerRecord>> {
    use rusqlite::OptionalExtension as _;
    let budget = i64::try_from(super::state_view::FRAME)?;
    let saved: Option<(i64, Option<String>)> = c.query_row("SELECT length(CAST(v AS BLOB)),CASE WHEN length(CAST(v AS BLOB))<=?2 THEN v END FROM kv WHERE k=?1", rusqlite::params![format!("{PRODUCER_PREFIX}{parent}"), budget], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    let json = match saved { None => return Ok(None), Some((_, Some(json))) => json, Some((_, None)) => return Err(super::mail::Error::TooLarge { revision: 0, budget: super::state_view::FRAME }.into()) };
    let record: ProducerRecord = serde_json::from_str(&json)?;
    anyhow::ensure!(record.version > 0, "invalid internal news version");
    let next = record.version.checked_add(1).ok_or_else(|| anyhow::anyhow!("internal news version exhausted"))?;
    VERSION.fetch_max(next, std::sync::atomic::Ordering::Relaxed);
    Ok(Some(record))
}

async fn load_parent(hub: &Arc<crate::hub::Hub>, parent: &str) -> Result<(), super::mail::Error> {
    if TOLD.lock().producers_loaded.contains(parent) { return Ok(()); }
    let id = parent.to_owned();
    let record = hub.store.read(move |c| read_producer(c, &id)).await.map_err(persistence_error)?;
    let reconciled = if let Some(original) = record.as_ref().and_then(|r| r.content.owned.original()) {
        let id = original_id(original).ok_or_else(|| super::mail::Error::Invalid { reason: "saved producer original has no id".into() })?;
        super::mail::receipt(&hub.store, "server".into(), "server".into(), parent.to_owned(), id.to_owned()).await?
    } else { None };
    let start = {
        let mut told = TOLD.lock();
        if told.producers_loaded.contains(parent) { return Ok(()); }
        if let Some(record) = &record {
            if matches!(record.content.owned, Owned::Retained { .. }) && reconciled.is_none() { return Err(super::mail::Error::Unavailable { reason: "saved retained news has no authoritative ledger receipt".into() }); }
            let news = restore_news(record)?;
            told.news.insert(parent.to_owned(), news);
        }
        told.producers_loaded.insert(parent.to_owned());
        let next = if let Some(receipt) = &reconciled { told.mail_progress(parent, receipt) }
            else { record.as_ref().and_then(|r| r.content.owned.original()).cloned() };
        next.is_some_and(|command| queue_for(parent, command))
    };
    if start { start_senders(hub, vec![parent.to_owned()]); }
    Ok(())
}

async fn storage_room(hub: &crate::hub::Hub, bytes: u64, rows: u64) -> Result<(), super::mail::Error> {
    let floor = super::mail_delivery::storage_floor(hub)?;
    let data = hub.cfg.server.data_dir.clone();
    let free = crate::rt::blocking(move || crate::os::disk_available(&data)).await.map_err(|e| super::mail::Error::Unavailable { reason: e.to_string() })?.ok_or_else(|| super::mail::Error::Unavailable { reason: "producer volume free space is unknown".into() })?;
    // A shrinking snapshot may replace a larger previously bounded producer/run row.
    let old_bound = u64::try_from(super::state_view::FRAME).map_err(|error| persistence_error(error.into()))?.checked_mul(rows).ok_or_else(|| super::mail::Error::Unavailable { reason: "producer prior payload bound overflow".into() })?;
    let scratch = hub.store.storage_reserve_tables(bytes.max(old_bound), vec!["kv".into(), "told_runs".into()], rows).await.map_err(persistence_error)?;
    let required = floor.checked_add(scratch).ok_or_else(|| super::mail::Error::Unavailable { reason: "producer storage scratch overflow".into() })?;
    if free < required { return Err(super::mail::Error::Capacity { required, authority: free }); }
    Ok(())
}

struct Bytes(u64);
impl std::io::Write for Bytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let n = u64::try_from(bytes.len()).map_err(std::io::Error::other)?;
        self.0 = self.0.checked_add(n).ok_or_else(|| std::io::Error::other("producer payload size overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

/// `rows` written to `told_runs`: an older state never overwrites (nor deletes) a newer one,
/// whichever write reaches the store first.
#[cfg(test)]
async fn write_rows(store: &crate::store::Store, rows: Vec<Row>) -> anyhow::Result<()> {
    if rows.is_empty() { return Ok(()); }
    store.call(move |c| { let tx = c.savepoint()?; write_rows_on(&tx, &rows)?; tx.commit()?; Ok(()) }).await
}

fn write_rows_on(c: &rusqlite::Connection, rows: &[Row]) -> anyhow::Result<()> {
    for (session, key, ran, version) in rows {
        match ran {
            Some(r) => {
                let news = serde_json::to_string(&r.news)?;
                c.execute("INSERT INTO told_runs (session,key,runner,news,handed_at,heard_at,told,version) VALUES (?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(session,key) DO UPDATE SET runner=excluded.runner,news=excluded.news,handed_at=excluded.handed_at,heard_at=excluded.heard_at,told=excluded.told,version=excluded.version WHERE excluded.version>told_runs.version",
                    rusqlite::params![session,key,r.runner,news,r.handed_at,i64::try_from(r.heard_at).unwrap_or(i64::MAX),r.told,version])?;
            }
            None => { c.execute("DELETE FROM told_runs WHERE session=?1 AND key=?2 AND version<?3", rusqlite::params![session,key,version])?; }
        }
    }
    Ok(())
}

fn write_persist(c: &mut rusqlite::Connection, mut batch: Persist, confirmed: &HashSet<(String, String)>) -> anyhow::Result<Persist> {
    let tx = c.savepoint()?;
    let mut stale = HashSet::new();
    for producer in &batch.producers {
        if let Some(previous) = read_producer(&tx, &producer.parent)? {
            if previous.version >= producer.record.version {
                if previous.version == producer.record.version { anyhow::ensure!(serde_json::to_string(&previous)? == producer.json, "one producer version names different state"); }
                stale.insert(producer.parent.clone()); continue;
            }
            if let Some(original) = previous.content.owned.original() {
                let old_id = original_id(original).ok_or_else(|| anyhow::anyhow!("saved producer original is invalid"))?;
                if producer.record.content.owned.original() != Some(original) {
                    if producer.record.content.owned.original().and_then(original_id) == Some(old_id) { return Err(super::mail::Error::Reused { id: old_id.to_owned() }.into()); }
                    anyhow::ensure!(confirmed.contains(&(producer.parent.clone(), old_id.to_owned())), "an unarchived producer original cannot be replaced");
                }
            }
        }
        tx.execute("INSERT INTO kv(k,v) VALUES(?1,?2) ON CONFLICT(k) DO UPDATE SET v=excluded.v", rusqlite::params![format!("{PRODUCER_PREFIX}{}", producer.parent), producer.json])?;
    }
    for row in &batch.runs {
        if !stale.contains(row.0.as_str()) { write_rows_on(&tx, std::slice::from_ref(row))?; }
    }
    tx.commit()?;
    batch.runs.retain(|row| !stale.contains(row.0.as_str()));
    batch.producers.retain(|producer| !stale.contains(producer.parent.as_str()));
    Ok(batch)
}

/// What `told_runs` keeps, by agent, the oldest written first.
async fn read_rows(store: &crate::store::Store) -> anyhow::Result<Vec<(String, Ran)>> {
    store
        .call(|c| {
            let top: i64 = c.query_row("SELECT COALESCE(MAX(version), 0) FROM told_runs", [], |r| r.get(0))?;
            VERSION.fetch_max(top.saturating_add(1), std::sync::atomic::Ordering::Relaxed);
            let mut st = c.prepare("SELECT session, runner, news, handed_at, heard_at, told FROM told_runs ORDER BY version")?;
            let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?, r.get::<_, bool>(5)?)))?;
            let mut out = Vec::new();
            for row in rows {
                let (session, runner, news, handed_at, heard_at, told) = row?;
                let news = serde_json::from_str::<RunNews>(&news)?;
                out.push((session, Ran { news, handed_at, runner, heard_at: u64::try_from(heard_at)?, told, held: true }));
            }
            Ok(out)
        })
        .await
}

/// Tells parents of their children that settled, and agents of their runs: in the one message of
/// news waiting for each (`Told::plan`), queued after the agent's turn. `driven`: the agents a goal
/// stack drives now; `quiet`: those paused for usage (docs/programs.md), which get no new message,
/// since one would start their turn: their news waits, and goes once they go on.
pub async fn tick(hub: &Arc<crate::hub::Hub>, all: &[Info], now: i64, driven: &[String], quiet: &[String]) {
    if let Err(error) = load(hub).await { tracing::warn!(error=?error, "internal news canonical state unavailable"); return; }
    for parent in all { if let Err(error) = load_parent(hub, &parent.id).await { tracing::warn!(agent=%parent.id,error=?error,"internal news producer state unavailable"); return; } }
    // News a server before this one queued, waiting still: this one adds to it.
    for p in all.iter().filter(|p| !matches!(p.status, Status::Exited | Status::Disconnected | Status::Crashed) && (p.queue_partial || p.queue.iter().any(|q| super::queued::from_before(&q.id, NEWS)))) {
        if TOLD.lock().news.contains_key(&p.id) {
            continue;
        }
        let found = super::state_reads::first(hub, &p.id, p, |q| super::queued::from_before(&q.id, NEWS)).await;
        if let Ok(Some(found)) = found
            && let Ok(Some(q)) = super::state_reads::item(hub, &p.id, p, &found.id).await
        {
            TOLD.lock().found(&p.id, q.value.id, q.value.text, now);
        }
    }
    // The runners up now: a run whose runner is away has no one to say how it ends until it's back
    // (and says it again), and holds its agent back no more meanwhile.
    let up: HashSet<String> = hub.agents.load().iter().filter(|(_, a)| a.connected).map(|(n, _)| n.clone()).collect();
    // Decided and queued as one (under `TOLD`'s lock): what `saw` decides after goes after it. Each
    // parent's goes by its sender, in order; none of it waited for here.
    let start: Vec<String> = {
        let mut told = TOLD.lock();
        let plan = told.plan_with(all, now, driven, quiet, &|r| up.contains(r));
        // Each parent found by its id, not by a scan of every agent for each (goal 9, plan 01a1a87b2cec step 3).
        let by_id = by_id(all);
        let mut start = Vec::new();
        for (parent, c) in plan {
            if matches!(c, Command::Send { .. })
                && let Some(p) = by_id.get(parent.as_str())
            {
                super::wait::sent(&p.id, p.last_turn.as_ref().map(|t| t.at));
            }
            if queue_for(&parent, c) {
                start.push(parent);
            }
        }
        start
    };
    if let Err(error) = flush(hub).await { for parent in &start { if let Some(q) = TO_SEND.lock().get_mut(parent) { q.state = SenderState::Blocked; } } tracing::warn!(error=?error, "internal news original not persisted"); return; }
    start_senders(hub, start);
}

/// Each of `all` by its id, the first of any id given twice (as a scan for it finds).
fn by_id(all: &[Info]) -> HashMap<&str, &Info> {
    let mut by = HashMap::with_capacity(all.len());
    for i in all {
        by.entry(i.id.as_str()).or_insert(i);
    }
    by
}

impl Told {
    fn mark_news(&mut self, parent: &str) { self.news_dirty.insert(parent.to_owned()); }

    fn remove_news(&mut self, parent: &str) {
        if let Some(news) = self.news.remove(parent) { self.removed.insert(parent.to_owned(), news.previous); }
        self.mark_news(parent);
    }

    fn capture(&mut self) -> Result<Persist, super::mail::Error> {
        let runs = self.dirty_rows()?;
        let mut producers = Vec::new();
        for parent in &self.news_dirty {
            let content = content(self.news.get(parent), self.removed.get(parent).cloned().flatten());
            let captured_runs = runs.iter().filter(|(session, _, row, _)| session == parent && row.as_ref().is_some_and(|r| r.told)).map(|(_, key, _, version)| (key.clone(), *version)).collect();
            let record = ProducerRecord { version: next_version()?, content, captured_runs };
            let bytes = super::state_view::encode(&record, super::state_view::FRAME).map_err(|_| super::mail::Error::TooLarge { revision: 0, budget: super::state_view::FRAME })?;
            let json = String::from_utf8(bytes).map_err(|e| super::mail::Error::Unavailable { reason: e.to_string() })?;
            producers.push(ProducerWrite { parent: parent.clone(), record, json });
        }
        // A later in-flight capture may commit after this one. Equality alone cannot fence
        // X→Y→X changes: only the newest capture may acknowledge its still-current values.
        for (parent, key, _, version) in &runs { self.captured_runs.insert((parent.clone(), key.clone()), *version); }
        for producer in &producers { self.captured_news.insert(producer.parent.clone(), producer.record.version); }
        Ok(Persist { runs, producers })
    }

    fn acknowledge(&mut self, batch: &Persist) {
        for (parent, key, saved, version) in &batch.runs {
            let current = self.runs.get(parent).and_then(|rs| rs.iter().find(|r| r.news.key == *key));
            let identity = (parent.clone(), key.clone());
            if self.captured_runs.get(&identity) == Some(version) && current == saved.as_ref() { self.dirty.remove(&identity); self.captured_runs.remove(&identity); }
        }
        for producer in &batch.producers {
            if self.captured_news.get(&producer.parent) == Some(&producer.record.version) && content(self.news.get(&producer.parent), self.removed.get(&producer.parent).cloned().flatten()) == producer.record.content {
                self.news_dirty.remove(&producer.parent); self.removed.remove(&producer.parent);
                self.captured_news.remove(&producer.parent);
            }
        }
    }

    /// A changed explicit news snapshot is another immutable publication, never a source edit.
    fn revision(n: &mut News, text: String) -> Option<Command> {
        let mut message = n.msg.as_ref()?.clone();
        let id = super::queued::made(NEWS);
        message.id = id.clone(); message.text = text.clone(); message.held.clear(); message.awaiting_source();
        n.msg = Some(message); n.custody = NewsCustody::Pending;
        n.said.clone_from(&n.lines); n.said_more = (n.more, n.more_runs);
        n.given.clear(); gave(&mut n.given, &text, &n.lines);
        Some(Command::Send { id, text, mode: SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: None })
    }

    fn mail_progress(&mut self, parent: &str, receipt: &super::mail::Receipt) -> Option<Command> {
        let mut changed = false;
        let result = self.progress(parent, receipt, &mut changed);
        if changed { self.mark_news(parent); }
        result
    }

    fn progress(&mut self, parent: &str, receipt: &super::mail::Receipt, changed: &mut bool) -> Option<Command> {
        use super::mail::Disposition;
        let n = self.news.get_mut(parent)?;
        let message = n.msg.as_mut()?;
        if message.id != receipt.id { return None; }
        if let NewsCustody::Retained(previous) = &n.custody {
            if previous.revision != receipt.revision || previous.generation > receipt.generation { return None; }
            if previous.generation == receipt.generation && previous.disposition == receipt.disposition { return None; }
        }
        *changed = true;
        let mut previous = receipt.clone(); previous.repeat = false; n.previous = Some(previous);
        message.awaiting_source();
        n.custody = NewsCustody::Retained(receipt.clone());
        if matches!(receipt.disposition, Disposition::Consumed | Disposition::Cancelled | Disposition::Superseded) {
            n.lines.retain(|line| !n.said.contains(line));
            n.more = n.more.saturating_sub(n.said_more.0); n.more_runs = n.more_runs.saturating_sub(n.said_more.1);
            n.msg = None; n.before = None; n.said.clear(); n.held.clear(); n.given.clear(); n.said_more = (0, 0);
            n.custody = NewsCustody::Legacy;
            return None;
        }
        // Publication ACK can race a newer desired snapshot. The original stays immutable;
        // make the latest successor only now, after its predecessor has committed custody.
        let desired = n.text();
        if !desired.is_empty() && n.msg.as_ref().is_some_and(|m| m.text != desired) { return Self::revision(n, desired); }
        None
    }

    /// Runs a server before this one kept (`told_runs`), taken in under what this one heard since
    /// (newer); each runner's count of news as high as any of its runs says, so the window a repeat
    /// can come in goes on from there.
    fn adopt(&mut self, rows: Vec<(String, Ran)>) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        for (session, ran) in rows {
            let runs = self.runs.entry(session).or_default();
            if runs.iter().any(|r| r.news.key == ran.news.key) {
                continue;
            }
            if !ran.runner.is_empty() {
                let h = self.heard.entry(ran.runner.clone()).or_default();
                *h = (*h).max(ran.heard_at);
            }
            runs.push(ran);
        }
    }

    /// The runs changed or gone since they were last written, as they stand now.
    fn dirty_rows(&self) -> Result<Vec<Row>, super::mail::Error> {
        self.dirty
            .iter()
            .map(|(session, key)| {
                let ran = self.runs.get(session).and_then(|rs| rs.iter().find(|r| r.news.key == *key)).cloned();
                Ok((session.clone(), key.clone(), ran, next_version()?))
            })
            .collect()
    }

    /// News message `id` was found waiting for `parent` at `now`, saying `text`, queued before this
    /// server started: kept as its news, said first.
    fn found(&mut self, parent: &str, id: String, text: String, now: i64) {
        self.news.entry(parent.to_owned()).or_insert_with(|| {
            let given = vec![(super::queued::stable(&text), Vec::new())];
            News { msg: Some(Kept::found(id, text.clone(), now)), before: Some(text), given, ..News::default() }
        });
    }

    /// [`saw`]: what goes to the parent for it, the line taken out (an edit, or the message removed).
    fn saw(&mut self, child: &Info) -> Option<(String, Command)> {
        self.seen.entry(child.id.clone()).or_default().told = key_of(child);
        let parent = child.parent.clone()?;
        self.take_line(&parent, &child.id).map(|c| (parent, c))
    }

    /// The line keyed `key` (a child's id, a run's `run:` key) taken out of `parent`'s news: what
    /// goes to it for that (an edit, or the message removed), if the line was there.
    fn take_line(&mut self, parent: &str, key: &str) -> Option<Command> {
        let n = self.news.get_mut(parent)?;
        let had = n.lines.len();
        n.lines.retain(|(c, _)| c != key);
        if n.lines.len() == had {
            return None;
        }
        self.news_dirty.insert(parent.to_owned());
        let (empty, text) = (n.lines.is_empty() && n.before.is_none(), n.text());
        if !matches!(n.custody, NewsCustody::Legacy) {
            if !empty && matches!(n.custody, NewsCustody::Retained(_)) { return Self::revision(n, text); }
            // The caller saw a line; that is not permission to remove an accepted original
            // from its ledger or mutate a frozen source prompt. It retires by exact receipt.
            return None;
        }
        let c = match n.msg.as_mut() {
            Some(m) if empty => Some(Command::QueueRemove { id: m.id.clone() }),
            Some(_) => Self::revision(n, text),
            None => None,
        };
        if empty {
            self.remove_news(parent);
        }
        c
    }

    /// [`ran`], at `now`, from `runner` ("": kiln's own word, placement's answer), the session
    /// `located` there now or not: the run's latest news kept, to be told at the next look
    /// (`plan`), or why it doesn't count (E8). Two equal are one: the hold is refreshed, nothing
    /// told again (T9, T4). News older than what came of the run (two that came the other way
    /// round) changes nothing. A run seen end takes its line out of the news waiting: what goes to
    /// `session` for that.
    fn ran(&mut self, runner: &str, located: bool, session: &str, news: RunNews, now: i64) -> Result<Option<Command>, String> {
        if news.key.is_empty() {
            return Err("news of a run names the run (`key`)".to_owned());
        }
        let (key, event) = (news.key.clone(), news.event);
        let heard = if runner.is_empty() {
            0
        } else {
            let h = self.heard.entry(runner.to_owned()).or_default();
            *h += 1;
            *h
        };
        let runs = self.runs.entry(session.to_owned()).or_default();
        match runs.iter_mut().find(|r| r.news.key == key) {
            Some(r) if !runner.is_empty() && !r.runner.is_empty() && r.runner != runner => {
                return Err(format!("run {key} of agent session {session} is followed by {}: news of it from {runner} doesn't count", r.runner));
            }
            Some(r) if !runner.is_empty() && r.runner.is_empty() && !located => {
                return Err(format!("agent session {session} isn't on {runner}: news of its run {key} from there doesn't count"));
            }
            Some(r) => {
                if !runner.is_empty() {
                    r.runner = runner.to_owned();
                    // It said it again: it hadn't had the answer (or followed it on after a restart).
                    r.heard_at = heard;
                    r.held = true;
                }
                // Handed over again while it goes (an `output` that returned with it going): its agent
                // waits on it in this turn (T2), whatever it is now.
                if news.handed > r.news.handed && !r.news.event.over() {
                    r.handed_at = now;
                    r.news.handed = news.handed;
                }
                if news == r.news || event < r.news.event || (event == r.news.event && event.over()) {
                    // The same again, earlier news that came late, or the end said again: nothing new.
                } else if event == RunEvent::Seen {
                    // What it last said stands (its line goes), nothing more to tell of it.
                    r.news.event = RunEvent::Seen;
                    r.told = true;
                } else {
                    if event == RunEvent::Going {
                        r.handed_at = now;
                    }
                    r.told = matches!(event, RunEvent::Going | RunEvent::Seen);
                    r.news = news;
                }
            }
            None if !located => return Err(format!("agent session {session} isn't on {runner}, and kiln knows no run {key} of it from there")),
            None => runs.push(Ran::new(news, now, runner, heard)),
        }
        self.dirty.insert((session.to_owned(), key.clone()));
        self.mark_news(session);
        Ok(if event == RunEvent::Seen { self.take_line(session, &format!("{RUN}{key}")) } else { None })
    }

    /// When kiln saw `info`'s last turn end, at `now`: when it saw the end of the turn before
    /// that one (0: not since this server started, every run counting as handed in the last).
    fn observe(&mut self, info: &Info, now: i64) -> i64 {
        let at = info.last_turn.as_ref().map_or(0, |t| t.at);
        let t = self.turns.entry(info.id.clone()).or_insert(Turns { at, seen: 0, before: 0 });
        if t.at != at {
            t.before = t.seen;
            t.seen = now;
            t.at = at;
        }
        t.before
    }

    /// [`runs_awaited`], at `now`.
    fn awaited(&mut self, info: &Info, now: i64) -> bool {
        let floor = self.observe(info, now);
        self.runs.get(&info.id).is_some_and(|rs| rs.iter().any(|r| r.going() && r.handed_at > floor))
    }

    /// Each run whose latest news wasn't told yet: its line, by agent, at `now`. Runs of agents
    /// gone or ended are forgotten, and runs over and told once their runner said more since
    /// (it had their answer, and says them no more: the window a repeat can come in, T13); one
    /// going whose runner is away (`up` says which are here) holds its agent back no more.
    fn due_runs(&mut self, all: &[Info], now: i64, up: &dyn Fn(&str) -> bool) -> HashMap<String, Vec<(String, String)>> {
        let Told { runs, heard, dirty, news_dirty, .. } = self;
        let live: HashSet<&str> = all.iter().filter(|i| i.status != Status::Exited).map(|i| i.id.as_str()).collect();
        runs.retain(|s, rs| {
            let keep = live.contains(s.as_str());
            if !keep {
                dirty.extend(rs.iter().map(|r| (s.clone(), r.news.key.clone())));
                news_dirty.insert(s.clone());
            }
            keep
        });
        let mut by_agent: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for (agent, rs) in runs.iter_mut() {
            for r in rs.iter_mut() {
                if r.going() && !r.runner.is_empty() && !up(&r.runner) {
                    r.held = false;
                }
                if r.told {
                    continue;
                }
                r.told = true;
                dirty.insert((agent.clone(), r.news.key.clone()));
                news_dirty.insert(agent.clone());
                if !matches!(r.news.event, RunEvent::Going | RunEvent::Seen) {
                    by_agent.entry(agent.clone()).or_default().push((format!("{RUN}{}", r.news.key), run_line(&r.news, now)));
                }
            }
            rs.retain(|r| {
                let keep = !(r.finished() && heard.get(&r.runner).is_some_and(|h| *h > r.heard_at));
                if !keep {
                    dirty.insert((agent.clone(), r.news.key.clone()));
                    news_dirty.insert(agent.clone());
                }
                keep
            });
        }
        by_agent
    }

    /// Each child that settled since it was last looked at, [`Told::settle`] on: its line, by
    /// parent. Children first seen settled (the server just started) aren't told of, nor ones a
    /// goal stack drives (`driven`: they settle between goals; their parent is told once no stack
    /// drives them). A child that settled with messages queued and went back to work measures how
    /// long their delivery took.
    fn due(&mut self, all: &[Info], now: i64, driven: &[String]) -> HashMap<String, Vec<(String, String)>> {
        let settle = i64::try_from(self.settle.ms()).unwrap_or(i64::MAX);
        let mut delivered = Vec::new();
        let mut by_parent: HashMap<String, Vec<(String, String)>> = HashMap::new();
        // Those a goal stack drives, looked up in a set, not scanned for each child (goal 9, plan 01a1a87b2cec step 3).
        let driven: HashSet<&str> = driven.iter().map(String::as_str).collect();
        for c in all {
            let Some(parent) = c.parent.as_deref() else { continue };
            // A card's helper (`super::about`) answers a person: its parent isn't told of it.
            if c.helper.is_some() {
                continue;
            }
            let (st, key) = (stand(c), key_of(c));
            let e = self.seen.entry(c.id.clone()).or_insert_with(|| Seen { key: key.clone(), since: now, worked: false, told: key.clone(), queued: false, queued_since: None });
            if e.key != key {
                if st == "working"
                    && let Some(q) = e.queued_since.take()
                {
                    delivered.push(now.saturating_sub(q));
                }
                e.key.clone_from(&key);
                e.since = now;
                e.queued_since = (settled(st) && e.queued).then_some(now);
            }
            e.queued = c.queued > 0;
            if st == "working" {
                e.worked = true;
                continue;
            }
            if driven.contains(c.id.as_str()) {
                continue;
            }
            if !settled(st) || !e.worked || e.told == key || now - e.since < settle {
                continue;
            }
            e.told = key;
            e.worked = false;
            by_parent.entry(parent.to_owned()).or_default().push((c.id.clone(), line(c)));
        }
        for ms in delivered {
            self.settle.sample(u64::try_from(ms).unwrap_or(0));
        }
        let live: HashSet<&str> = all.iter().map(|i| i.id.as_str()).collect();
        self.seen.retain(|id, _| live.contains(id.as_str()));
        by_parent
    }

    /// [`Told::plan_with`], every runner taken as here.
    #[cfg(test)]
    fn plan(&mut self, all: &[Info], now: i64, driven: &[String], quiet: &[String]) -> Vec<(String, Command)> {
        self.plan_with(all, now, driven, quiet, &|_| true)
    }

    /// What goes to each parent now, at `now`, for its children that settled (`due`) and its runs
    /// with news (`due_runs`, runners here as `up` says): its news waiting in its queue, edited to
    /// hold each child's and run's latest line; or, none waiting, a message with them, when one of
    /// them wakes it (a run's start or place in line is state: it waits for one that does, T8).
    /// What a message that went may not have said (an edit that came too late) goes in the next.
    /// An agent that has ended isn't sent anything; one whose machine is away, once it's back; one
    /// paused for usage (`quiet`), no new message (it would start its turn), its news waiting here
    /// until it goes on.
    fn plan_with(&mut self, all: &[Info], now: i64, driven: &[String], quiet: &[String], up: &dyn Fn(&str) -> bool) -> Vec<(String, Command)> {
        for i in all {
            self.observe(i, now);
        }
        self.turns.retain(|a, _| all.iter().any(|i| i.id == *a));
        let mut due = self.due(all, now, driven);
        for (agent, lines) in self.due_runs(all, now, up) {
            due.entry(agent).or_default().extend(lines);
        }
        // Lines of runs that wake no one: started, or in line.
        let calm: HashSet<String> = self.runs.values().flatten().filter(|r| !r.news.event.wakes()).map(|r| format!("{RUN}{}", r.news.key)).collect();
        // Each agent found by its id, and those paused for usage looked up in a set: each was a scan of every agent, or
        // of each of those, for each parent, under `TOLD`'s lock, on the goal driver's shard (goal 9, plan 01a1a87b2cec
        // step 3).
        let (by_id, quiet) = (by_id(all), quiet.iter().map(String::as_str).collect::<HashSet<&str>>());
        let removed: Vec<String> = self.news.iter().filter(|(p, n)| !by_id.contains_key(p.as_str()) && (n.msg.is_none() || matches!(n.custody, NewsCustody::Legacy))).map(|(p, _)| p.clone()).collect();
        for parent in removed { self.remove_news(&parent); }
        let parents: BTreeSet<String> = self.news.keys().chain(due.keys()).cloned().collect();
        let mut out = Vec::new();
        for parent in parents {
            let Some(info) = by_id.get(parent.as_str()).copied() else { continue };
            let n = self.news.entry(parent.clone()).or_default();
            let mut unheard = false;
            if let Some(m) = n.msg.as_mut() && matches!(n.custody, NewsCustody::Legacy) {
                let at = m.look(Some(info), now);
                // What it says reaches the parent, unless an edit of it didn't go (its words not
                // known since: what it held before stands).
                if !m.lost() && !m.text.is_empty() {
                    n.held.clone_from(&n.said);
                }
                // What its harness was handed, as its runner says: the lines of those words
                // (CP-T3.2, the seventeenth adversarial check's).
                if let Some((_, lines)) = info.said.iter().find(|g| g.id == m.id).and_then(|h| n.given.iter().rev().find(|(d, _)| *d == h.digest)) {
                    n.held.clone_from(lines);
                }
                // Gone from its queue, its handing not heard of yet (a runner from before this
                // release says it in its events, which may come just after its state): it waits
                // `queued::HEARD_MS` for that, nothing going meanwhile, so lines an edit gave it
                // aren't told again (CP-T3.2, the sixteenth adversarial check's; the goal driver's
                // word waits so too).
                unheard = at == Now::Gone && m.look_heard(Some(info), now) == Now::Waiting;
                if at == Now::Gone && !unheard {
                    n.lines.retain(|l| !n.held.contains(l));
                    n.msg = None;
                    n.before = None;
                    n.said.clear();
                    n.held.clear();
                    n.given.clear();
                    n.more = 0;
                    n.more_runs = 0;
                }
            }
            let additions = due.remove(&parent).unwrap_or_default();
            if !additions.is_empty() { self.news_dirty.insert(parent.clone()); }
            for (child, l) in additions {
                n.lines.retain(|(c, _)| *c != child);
                n.lines.push((child, l));
            }
            let over = n.lines.len().saturating_sub(LINES_MAX);
            if over > 0 {
                let runs = n.lines.iter().take(over).filter(|(k, _)| k.starts_with(RUN)).count();
                n.lines.drain(..over);
                n.more = n.more.saturating_add(over.saturating_sub(runs));
                n.more_runs = n.more_runs.saturating_add(runs);
            }
            if ((n.lines.is_empty() && n.before.is_none()) || info.status == Status::Exited) && (n.msg.is_none() || matches!(n.custody, NewsCustody::Legacy)) {
                self.remove_news(&parent);
                continue;
            }
            if matches!(info.status, Status::Disconnected | Status::Crashed | Status::Exited) || unheard {
                continue;
            }
            let text = n.text();
            match n.msg.as_mut() {
                Some(m) if m.text == text => {}
                Some(_) if matches!(n.custody, NewsCustody::Pending) => {}
                Some(_) => { if !text.is_empty() && let Some(c) = Self::revision(n, text) { self.news_dirty.insert(parent.clone()); out.push((parent, c)); } }
                None if quiet.contains(parent.as_str()) => {}
                None if n.lines.iter().all(|(k, _)| calm.contains(k)) => {}
                None => {
                    let id = super::queued::made(NEWS);
                    self.news_dirty.insert(parent.clone());
                    out.push((parent, Command::Send { id: id.clone(), text: text.clone(), mode: SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: None }));
                    n.given = vec![(super::queued::stable(&text), n.lines.clone())];
                    n.msg = Some(Kept::sent(id, text, info, now));
                    if let Some(message) = n.msg.as_mut() { message.awaiting_source(); }
                    n.custody = NewsCustody::Pending;
                    n.said_more = (n.more, n.more_runs);
                    n.said.clone_from(&n.lines);
                    n.held.clone_from(&n.lines);
                }
            }
        }
        out
    }

    /// Command `c` from `plan` didn't reach `parent`: what it said goes at the next look (an edit
    /// that didn't go says nothing the message holds, so the lines it carried aren't taken as held).
    #[cfg(test)]
    fn unsent(&mut self, parent: &str, c: &Command) {
        let Some(n) = self.news.get_mut(parent) else { return };
        if !matches!(n.custody, NewsCustody::Legacy) { return; }
        match (c, n.msg.as_mut()) {
            (Command::Send { id, .. }, Some(m)) if m.id == *id => {
                n.msg = None;
                n.held.clear();
                n.given.clear();
            }
            (Command::QueueEdit { id, .. }, Some(m)) if m.id == *id => m.text.clear(),
            _ => {}
        }
    }
}

/// A parent's news as one message: one child's or run's line, or each child's under one heading
/// and each run's under another (and how many more settled or ended, past the most it names);
/// after what a message found waiting said (`before`).
fn text_of(before: Option<&str>, lines: &[(String, String)], more: usize, more_runs: usize) -> String {
    let (runs, agents): (Vec<&(String, String)>, Vec<&(String, String)>) = lines.iter().partition(|(k, _)| k.starts_with(RUN));
    let text = match (agents.as_slice(), runs.as_slice(), more, more_runs) {
        ([], [], 0, 0) => String::new(),
        ([(_, one)], [], 0, 0) | ([], [(_, one)], 0, 0) => one.clone(),
        _ => {
            let mut parts = Vec::new();
            if !agents.is_empty() || more > 0 {
                let mut a = format!("Agents you started:\n{}", agents.iter().map(|(_, l)| format!("- {l}")).collect::<Vec<_>>().join("\n"));
                if more > 0 {
                    a.push_str(&format!("\n- and {more} more that settled before these (agents_list with `parent` lists them all)."));
                }
                parts.push(a);
            }
            if !runs.is_empty() || more_runs > 0 {
                // A run's line may go on over several (the end of its output): those stay in its item.
                let mut r = format!("Runs you started:\n{}", runs.iter().map(|(_, l)| format!("- {}", l.replace('\n', "\n  "))).collect::<Vec<_>>().join("\n"));
                if more_runs > 0 {
                    r.push_str(&format!("\n- and {more_runs} more that ended before these."));
                }
                parts.push(r);
            }
            parts.join("\n\n")
        }
    };
    match before {
        Some(b) if text.is_empty() => b.to_owned(),
        Some(b) => format!("{b}\n\n{text}"),
        None => text,
    }
}

/// How a detached run whose client went is named in its news (`crate::reach::told`).
pub const DETACHED: &str = "kiln run --detach";

/// One of an agent's runs as it's told of it ([`RunNews`]) at `now`: it's in line, it started, it
/// will never start, or it ended, where, after how long, how (its exit code, or why it has none),
/// and the end of its output, the line no longer than its share of the message ([`LINE_SHARE`]).
/// A run whose id went with what knew it (its MCP server ended) gets no `output` hint (T7).
fn run_line(r: &RunNews, now: i64) -> String {
    let first = r.command.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    let more = r.command.trim().lines().nth(1).is_some();
    let command = if more { format!("{first}…") } else { first.to_owned() };
    let what = match (r.id.as_str(), r.how.as_str()) {
        ("", DETACHED) => format!("`{DETACHED} -- {command}`, which you left running,"),
        ("", "kiln.machines.start") => format!("A run (`{command}`) your program started in the background"),
        ("", _) => format!("A run (`{command}`) you started in the background"),
        (id, "kiln.machines.start") => format!("Run {id} (`{command}`), which your program started in the background,"),
        (id, _) => format!("Run {id} (`{command}`), which you started in the background,"),
    };
    let on = if r.on.trim().is_empty() { String::new() } else { format!(" on {}", r.on.trim()) };
    let why = || r.error.as_deref().map(str::trim).filter(|e| !e.is_empty()).map(sentence);
    let mut s = match r.event {
        RunEvent::Queued => {
            let place = r.place.map(|p| format!(", {} in line", if p <= 1 { "next".to_owned() } else { format!("number {p}") })).unwrap_or_default();
            let when = r.expected_start.map(|at| format!(", expected to start in about {}", took(at.saturating_sub(now)))).unwrap_or_default();
            format!("{what} waits for a machine with room{place}{when}: kiln tells you when it ends.")
        }
        RunEvent::Started => format!("{what} started{on}: kiln tells you when it ends."),
        RunEvent::Never => format!("{what} will never start: {}", why().unwrap_or_else(|| "no machine kiln knows can run it.".to_owned())),
        RunEvent::Ended => {
            let after = match (r.started_at, r.ended_at) {
                (Some(s), Some(e)) => format!(" after {}", took(e.saturating_sub(s))),
                _ => String::new(),
            };
            let how = match (r.exit_code, why()) {
                (Some(c), None) => format!(" with exit code {c}."),
                (Some(c), Some(e)) => format!(" with exit code {c}: {e}"),
                (None, Some(e)) => format!(", with no exit code: {e}"),
                (None, None) => ", with no exit code (it was killed).".to_owned(),
            };
            let mut s = format!("{what} ended{on}{after}{how}");
            if r.lost > 0 {
                s.push_str(&format!(" {} bytes of its output were dropped unread.", r.lost));
            }
            if !r.id.is_empty() {
                s.push_str(&format!("\n`output {{\"id\": \"{}\"}}` has its output since you last read it.", r.id));
            }
            s
        }
        RunEvent::Going | RunEvent::Seen => return String::new(),
    };
    // The end of its output, in what's left of the line's share.
    const OPEN: &str = "\nIts output ended with:\n```\n";
    const CLOSE: &str = "\n```";
    let room = LINE_SHARE.saturating_sub(s.len() + OPEN.len() + CLOSE.len());
    let tail = clip_tail(&r.tail, room);
    if r.event == RunEvent::Ended && !tail.is_empty() {
        let at = s.find("\n`output").unwrap_or(s.len());
        s.insert_str(at, &format!("{OPEN}{tail}{CLOSE}"));
    }
    if s.len() > LINE_SHARE {
        s = clip_head(&s, LINE_SHARE);
    }
    s
}

/// How long `ms` was, as a run's news says it: `4m 12s`.
fn took(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    match s {
        0 => "less than a second".to_owned(),
        1..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {}s", s / 60, s % 60),
        _ => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

/// One child as its parent is told of it.
fn line(c: &Info) -> String {
    let short: String = c.id.chars().take(8).collect();
    let who = if c.name.trim().is_empty() { format!("Agent {short}") } else { format!("Agent \u{201c}{}\u{201d} ({short})", c.name.trim()) };
    let said = super::model::clip_middle(c.summary.trim(), SAID_MAX).0;
    match stand(c) {
        "idle" => {
            let how = match c.last_turn.as_ref().map(|t| t.how.as_str()) {
                Some("cut") => " part-way",
                Some("refused") => ", refusing",
                Some("limit") => " at its turn limit",
                _ => "",
            };
            let said = if said.is_empty() { String::new() } else { format!(": {said}") };
            format!("{} agents_result {{\"agent\": \"{}\"}} has its whole reply and what it changed.", sentence(&format!("{who}, which you started, ended its turn{how}{said}")), c.id)
        }
        "needs_you" => {
            let asks: Vec<String> = c.approvals.iter().map(|a| a.title.clone()).filter(|t| !t.trim().is_empty()).collect();
            let asks = if asks.is_empty() { String::new() } else { format!(" ({})", asks.join("; ")) };
            format!("{who}, which you started, waits on a person{asks}: tell the person what it asks and where (its page in kiln). It goes on once they answer.")
        }
        "stopped" => {
            let why = c.exit.as_ref().and_then(|e| e.error.clone()).map(|e| format!(": {}", super::model::clip_middle(e.trim(), SAID_MAX).0)).unwrap_or_default();
            sentence(&format!("{who}, which you started, stopped{why}"))
        }
        "paused" => format!("{who}, which you started, is paused at a usage limit: it goes on when the limit resets."),
        _ => format!("{who}, which you started, ended."),
    }
}

/// `words` as a sentence: a full stop after them, unless they end in one of their own (a child's
/// last words often do).
fn sentence(words: &str) -> String {
    let words = words.trim_end();
    if words.ends_with(['.', '!', '?', '…']) { words.to_owned() } else { format!("{words}.") }
}

/// Whether `agent` has children at work: an agent that ended its turn to let them work isn't
/// prodded to go on (it's told when each settles).
pub fn children_working(all: &[Info], agent: &str) -> bool {
    all.iter().any(|c| c.parent.as_deref() == Some(agent) && stand(c) == "working")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::model::{Status, TurnEnd};
    use crate::harness::queued::Queued;

    /// How long a child stands settled before its parent is told, before any delivery was measured:
    /// RFC 6298's initial RTO ([`Rto::INITIAL_MS`]; T13).
    const SETTLE_MS: i64 = 1_000;

    fn asks(title: &str) -> anyhow::Result<crate::harness::session::ApprovalBrief> {
        Ok(crate::harness::session::ApprovalBrief { id: "r1".into(), tool: crate::harness::model::ToolKind::Shell, tool_name: "Bash".into(), title: title.into(), at: 1, agent: None })
    }

    fn child(id: &str, status: Status, ended_at: i64, summary: &str) -> Info {
        Info {
            id: id.into(),
            name: format!("Child {id}"),
            parent: Some("p1".into()),
            status,
            summary: summary.into(),
            last_turn: (ended_at > 0).then(|| TurnEnd { at: ended_at, how: "done".into(), ..TurnEnd::default() }),
            ..Info::default()
        }
    }

    /// What's due, as the messages it makes: by parent, with its children's lines.
    fn told(t: &mut Told, all: &[Info], now: i64, driven: &[String]) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = t.due(all, now, driven).into_iter().map(|(p, lines)| (p, text_of(None, &lines, 0, 0))).collect();
        out.sort();
        out
    }

    /// A child that worked and settled is told of once, after it has stood settled a while; one
    /// first seen settled (a restart) isn't; children that settle together go in one message; what
    /// the parent saw itself isn't told again.
    #[test]
    fn a_parent_is_told_when_its_children_settle_and_needn_t_wait() -> anyhow::Result<()> {
        let mut t = Told::default();
        let t0 = 1_000_000;
        // First seen: one already done (a restart), one at work.
        let done_before = child("c0", Status::Idle, 10, "old news");
        assert!(told(&mut t, &[done_before.clone(), child("c1", Status::Running, 0, "")], t0, &[]).is_empty());
        assert!(children_working(&[child("c1", Status::Running, 0, "")], "p1"));
        // c1 and c2 at work, then both done.
        assert!(told(&mut t, &[done_before.clone(), child("c1", Status::Running, 0, ""), child("c2", Status::Running, 0, "")], t0 + 100, &[]).is_empty());
        let settled = [done_before.clone(), child("c1", Status::Idle, t0 + 200, "Fixed the parser; tests pass."), child("c2", Status::Idle, t0 + 300, "Docs updated.")];
        assert!(told(&mut t, &settled, t0 + 400, &[]).is_empty(), "not before it has stood settled a while");
        let news = told(&mut t, &settled, t0 + 400 + SETTLE_MS, &[]);
        assert_eq!(news.len(), 1, "one message for the parent: {news:?}");
        let (parent, text) = news.first().cloned().unwrap_or_default();
        assert_eq!(parent, "p1");
        assert!(text.contains("Agents you started") && text.contains("Fixed the parser; tests pass.") && text.contains("Docs updated.") && text.contains("agents_result"), "{text}");
        assert!(!text.contains("old news"), "what was done before the server saw it isn't told");
        assert!(told(&mut t, &settled, t0 + 10_000, &[]).is_empty(), "told once");
        assert!(!children_working(&settled, "p1"));
        // c1 at work again, then waiting on a person; the parent saw that itself (agents_wait).
        let _ = told(&mut t, &[child("c1", Status::Running, t0 + 200, "")], t0 + 11_000, &[]);
        let mut asking = child("c1", Status::Waiting, t0 + 200, "");
        asking.approvals.push(asks("Run cargo publish")?);
        let _ = t.saw(&asking);
        assert!(told(&mut t, std::slice::from_ref(&asking), t0 + 20_000, &[]).is_empty());
        assert!(told(&mut t, std::slice::from_ref(&asking), t0 + 20_000 + SETTLE_MS, &[]).is_empty(), "what the parent saw isn't told again");
        // Children of no agent (a person started them) are nobody's to tell.
        let mut orphan = child("c3", Status::Running, 0, "");
        orphan.parent = None;
        let _ = told(&mut t, std::slice::from_ref(&orphan), t0, &[]);
        orphan.status = Status::Idle;
        assert!(told(&mut t, &[orphan], t0 + 30_000, &[]).is_empty());
        // A child its goal stack drives settles between goals unannounced; once no stack drives
        // it, its parent is told.
        let driven = vec!["c4".to_owned()];
        let _ = told(&mut t, &[child("c4", Status::Running, 0, "")], t0 + 40_000, &driven);
        let between = child("c4", Status::Idle, t0 + 41_000, "Goal 1 met.");
        assert!(told(&mut t, std::slice::from_ref(&between), t0 + 50_000, &driven).is_empty(), "between goals");
        let news = told(&mut t, std::slice::from_ref(&between), t0 + 60_000, &[]);
        assert!(news.first().is_some_and(|(p, t)| p == "p1" && t.contains("Goal 1 met.")), "once left to itself: {news:?}");
        Ok(())
    }

    /// Goal 9, plan 01a1a87b2cec step 3: a fleet's news planned by its agents' ids and sets, not scans of every agent for
    /// each parent and child: each parent of children that settled gets one message with each one's line, but those a
    /// goal stack drives and a parent paused for usage, as before, whatever ids `driven` and `quiet` give twice; and
    /// `plan`, `due` and `tick` (and `elsewhere::tick`) scan none of their lists for each one.
    #[test]
    fn g9_s3_a_fleets_news_is_planned_by_id_not_by_scans() -> anyhow::Result<()> {
        let mut t = Told::default();
        let (parents, per) = (40, 25);
        let ps: Vec<Info> = (0..parents).map(|p| coordinator(&format!("g9p{p}"))).collect();
        let mut kids: Vec<Info> = Vec::new();
        for p in 0..parents {
            let ids: Vec<String> = (0..per).map(|k| format!("g9p{p}k{k}")).collect();
            kids.extend(helpers(&format!("g9p{p}"), &ids.iter().map(String::as_str).collect::<Vec<_>>()));
        }
        let all = |kids: &[Info]| ps.iter().cloned().chain(kids.iter().cloned()).collect::<Vec<Info>>();
        // Each parent's first child driven by a goal stack, the last parent paused for usage; ids given twice.
        let driven: Vec<String> = (0..parents).flat_map(|p| [format!("g9p{p}k0"), format!("g9p{p}k0")]).collect();
        let quiet: Vec<String> = vec![format!("g9p{}", parents - 1), format!("g9p{}", parents - 1)];
        let now = 5_000_000;
        anyhow::ensure!(t.plan(&all(&kids), now, &driven, &quiet).is_empty(), "seen at work first");
        for k in &mut kids {
            k.status = Status::Idle;
            k.summary = format!("{} done.", k.id);
            k.last_turn = Some(TurnEnd { at: now + 1, how: "done".into(), ..TurnEnd::default() });
        }
        anyhow::ensure!(t.plan(&all(&kids), now + 1, &driven, &quiet).is_empty(), "not before they've stood settled a while");
        let sent = t.plan(&all(&kids), now + 1 + SETTLE_MS, &driven, &quiet);
        anyhow::ensure!(sent.len() == parents - 1, "one message for each parent but the one paused: {}", sent.len());
        for (parent, c) in &sent {
            let Command::Send { text, .. } = c else { anyhow::bail!("{parent}: {c:?}") };
            anyhow::ensure!(!text.contains(&format!("{parent}k0 done.")), "{parent}: its driven child isn't told of");
            anyhow::ensure!(text.contains(&format!("{parent}k1 done.")), "{parent}: its others are: {text}");
        }
        anyhow::ensure!(!sent.iter().any(|(p, _)| *p == format!("g9p{}", parents - 1)), "the parent paused for usage gets no message");
        let code = |src: &str| src.split("\n#[cfg(test)]\nmod tests {").next().unwrap_or_default().lines().map(|l| l.split("//").next().unwrap_or_default()).collect::<Vec<_>>().join("\n");
        let (told_code, elsewhere_code) = (code(include_str!("told.rs")), code(include_str!("elsewhere.rs")));
        // A method's body ends at its own indent's closing brace, a free function's at the first column's.
        let body = |code: &str, head: &str, end: &str| code.split(head).nth(1).and_then(|rest| rest.split(end).next()).unwrap_or_default().to_owned();
        for (code, head, end) in [(&told_code, "fn plan(&mut self", "\n    }\n"), (&told_code, "fn due(&mut self", "\n    }\n"), (&told_code, "pub async fn tick(", "\n}\n"), (&elsewhere_code, "pub fn tick(", "\n}\n")] {
            let b = body(code, head, end);
            anyhow::ensure!(!b.is_empty(), "{head} not found");
            for scan in ["all.iter().find(", "all.iter().any(", "driven.contains(&", "quiet.contains(&"] {
                anyhow::ensure!(!b.contains(scan), "{head} scans: {scan}");
            }
        }
        Ok(())
    }

    /// CP-M3 (docs/programs.md, "Usage"): a parent paused for usage (`quiet`) gets no new message
    /// of news, since one would start its turn: what its children did waits, and goes once it goes
    /// on.
    #[test]
    fn a_parent_paused_for_usage_gets_its_news_once_it_goes_on() -> anyhow::Result<()> {
        let mut t = Told::default();
        let p = coordinator("q9");
        let mut kids = helpers("q9", &["k9"]);
        let (now, quiet) = (3_000_000, vec!["q9".to_owned()]);
        anyhow::ensure!(t.plan(&all(&p, &kids), now, &[], &quiet).is_empty(), "seen at work first");
        if let Some(c) = kids.first_mut() {
            c.status = Status::Idle;
            "Track 7's goal met.".clone_into(&mut c.summary);
            c.last_turn = Some(TurnEnd { at: now + 1, how: "done".into(), ..TurnEnd::default() });
        }
        anyhow::ensure!(t.plan(&all(&p, &kids), now + 1, &[], &quiet).is_empty());
        anyhow::ensure!(t.plan(&all(&p, &kids), now + 1 + SETTLE_MS, &[], &quiet).is_empty(), "nothing while it's paused for usage");
        let sent = t.plan(&all(&p, &kids), now + 2 + SETTLE_MS, &[], &[]);
        anyhow::ensure!(matches!(sent.first(), Some((parent, Command::Send { text, .. })) if parent == "q9" && text.contains("Track 7's goal met.")), "once it goes on: {sent:?}");
        Ok(())
    }

    #[test]
    fn what_a_parent_is_told_says_what_its_child_needs() -> anyhow::Result<()> {
        let mut asking = child("c9", Status::Waiting, 5, "");
        asking.approvals.push(asks("Run cargo publish")?);
        let text = line(&asking);
        assert!(text.contains("waits on a person (Run cargo publish)") && text.contains("tell the person"), "{text}");
        let mut crashed = child("c8", Status::Exited, 5, "");
        crashed.exit = Some(crate::harness::model::ExitInfo { code: Some(1), error: Some("claude exited with 1".into()), ..Default::default() });
        assert!(line(&crashed).contains("stopped: claude exited with 1"), "{}", line(&crashed));
        Ok(())
    }

    /// A child's last words end the sentence they're in once: a full stop of their own (or another
    /// ending) isn't followed by kiln's.
    #[test]
    fn a_child_s_last_words_end_its_line_with_one_full_stop() {
        for (said, then) in [("Fixed the parser.", "the parser. agents_result"), ("Shipped it!", "it! agents_result"), ("Is the lexer next?", "next? agents_result"), ("Docs updated", "updated. agents_result")] {
            let text = line(&child("c7", Status::Idle, 5, said));
            assert!(text.contains(then) && !text.contains(".."), "{text}");
        }
        assert!(line(&child("c6", Status::Idle, 5, "")).contains("ended its turn. agents_result"), "no last words: the sentence still ends");
        let mut crashed = child("c5", Status::Exited, 5, "");
        crashed.exit = Some(crate::harness::model::ExitInfo { code: Some(1), error: Some("claude exited with 1.".into()), ..Default::default() });
        assert!(line(&crashed).ends_with("stopped: claude exited with 1."), "{}", line(&crashed));
    }

    /// A parent at work on one long turn, its queue empty.
    fn coordinator(id: &str) -> Info {
        Info { id: id.into(), name: "Coordinator".into(), status: Status::Running, ..Info::default() }
    }

    /// Children of `parent`, at work.
    fn helpers(parent: &str, ids: &[&str]) -> Vec<Info> {
        ids.iter().map(|id| Info { parent: Some(parent.into()), ..child(id, Status::Running, 0, "") }).collect()
    }

    fn all(p: &Info, kids: &[Info]) -> Vec<Info> {
        std::iter::once(p.clone()).chain(kids.iter().cloned()).collect()
    }

    /// What the parent's harness does with `c` behind its turn, as kiln then sees its queue.
    fn apply(p: &mut Info, c: &Command) {
        match c {
            Command::Send { id, text, .. } => p.queue.push(Queued::of(id, text)),
            Command::QueueEdit { id, text, .. } => {
                if let Some(q) = p.queue.iter_mut().find(|q| q.id == *id) {
                    *q = Queued::of(id, text);
                }
            }
            Command::QueueRemove { id } => p.queue.retain(|q| q.id != *id),
            _ => {}
        }
        p.queued = p.queue.len();
    }

    /// The parent's harness is handed what waits in its queue (`handed`: each message's id and the
    /// words it had then), as this release's runner says it beside its queue (`State::given`, kept
    /// by the server as `Info::said`).
    fn take(p: &mut Info, handed: &[(&str, &str)]) {
        for (id, text) in handed {
            p.said.push(crate::harness::model::Given { id: (*id).to_owned(), digest: crate::harness::queued::stable(text) });
        }
        p.queue.clear();
        p.queued = 0;
    }

    /// Real immutable ledger custody, kept separate from apply/take's source observations.
    /// The two unresolved slots represent one frozen original and its latest desired successor.
    struct NewsLedger { store: crate::store::Store, parent: String }

    impl NewsLedger {
        fn new(parent: &str) -> anyhow::Result<Self> { Ok(Self { store: crate::store::Store::in_memory()?, parent: parent.into() }) }

        fn publication(&self, command: &Command) -> anyhow::Result<super::super::mail::Publish> {
            let id=original_id(command).ok_or_else(||anyhow::anyhow!("news is not an immutable send"))?;
            Ok(super::super::mail::Publish { owner:"server".into(),sender:"server".into(),recipient:self.parent.clone(),id:id.into(),command:serde_json::to_value(command)?,provenance:None,status:Some(super::super::mail::StatusKey { task:None,key:"internal-news".into() }) })
        }

        async fn publish(&self, told: &mut Told, command: &Command) -> anyhow::Result<Option<Command>> {
            let receipt=super::super::mail::publish(&self.store,self.publication(command)?,u64::MAX).await?;
            Ok(told.mail_progress(&self.parent,&receipt))
        }

        async fn admit(&self, told: &mut Told, parent: &mut Info, command: &Command) -> anyhow::Result<super::super::mail::Delivery> {
            let id=original_id(command).ok_or_else(||anyhow::anyhow!("not a send"))?;
            let receipt=super::super::mail::receipt(&self.store,"server".into(),"server".into(),self.parent.clone(),id.into()).await?.ok_or_else(||anyhow::anyhow!("original was not committed"))?;
            let delivery=super::super::mail::claim_at(&self.store,receipt.revision,1,1,"fixture-owner".into(),super::super::state_view::FRAME).await?.ok_or_else(||anyhow::anyhow!("original is not eligible"))?;
            assert_eq!(delivery.publication.command,serde_json::to_value(command)?,"source receives the immutable stored command");
            let admitted=super::super::mail::mark_admitted(&self.store,delivery.token,delivery.epoch).await?;
            assert!(told.mail_progress(&self.parent,&admitted).is_none());
            apply(parent,command);
            Ok(delivery)
        }

        async fn consume(&self, told: &mut Told, delivery: &super::super::mail::Delivery) -> anyhow::Result<()> {
            let consumed=super::super::mail::mark_consumed(&self.store,delivery.token,delivery.epoch).await?;
            assert!(told.mail_progress(&self.parent,&consumed).is_none());
            Ok(())
        }

        async fn history(&self) -> anyhow::Result<Vec<super::super::mail::Publication>> {
            let page=super::super::mail::history(&self.store,"server".into(),"server".into(),self.parent.clone(),None,0,super::super::state_view::FRAME).await?;
            anyhow::ensure!(page.next.is_none(),"fixture history must fit its bounded page");
            Ok(page.items)
        }

        async fn originals(&self, commands: &[Command]) -> anyhow::Result<()> {
            let history=self.history().await?;
            assert_eq!(history.len(),commands.len());
            for command in commands {
                let id=original_id(command).ok_or_else(||anyhow::anyhow!("not a send"))?;
                assert_eq!(history.iter().find(|row|row.receipt.id==id).map(|row|&row.publication.command),Some(&serde_json::to_value(command)?),"all accepted words stay immutable: {id}");
            }
            Ok(())
        }
    }

    fn one_send(commands: &[Command]) -> anyhow::Result<Command> {
        match commands { [command @ Command::Send { .. }] => Ok(command.clone()),_ => anyhow::bail!("expected one immutable send: {commands:?}") }
    }

    fn words(command: &Command) -> anyhow::Result<(&str,&str)> {
        match command { Command::Send { id,text,.. } => Ok((id,text)),_ => anyhow::bail!("expected an immutable send") }
    }

    /// Children `which` (by index, with their last words) go to work, then end a turn and stand
    /// settled a while, from `now`: what goes to their parent `p` meanwhile.
    fn settle(t: &mut Told, p: &Info, kids: &mut [Info], which: &[(usize, &str)], now: i64) -> Vec<Command> {
        let mut out = Vec::new();
        for (step, at) in [(0, now), (1, now + 1), (2, now + 1 + SETTLE_MS)] {
            for (k, said) in which {
                if let Some(c) = kids.get_mut(*k) {
                    if step == 0 {
                        c.status = Status::Running;
                    } else if step == 1 {
                        c.status = Status::Idle;
                        (*said).clone_into(&mut c.summary);
                        c.last_turn = Some(TurnEnd { at: now + 1, how: "done".into(), ..TurnEnd::default() });
                    }
                }
            }
            out.extend(t.plan(&all(p, kids), at, &[], &[]).into_iter().map(|(_, c)| c));
        }
        out
    }

    /// What kiln last had the parent's news say.
    fn news_of(t: &Told, parent: &str) -> String {
        t.news.get(parent).and_then(|n| n.msg.as_ref()).map(|m| m.text.clone()).unwrap_or_default()
    }

    #[test]
    fn durable_news_keeps_unaccepted_original_and_revises_only_after_custody() -> anyhow::Result<()> {
        let mut t = Told::default(); let p = coordinator("parent");
        let original = "original status".to_owned(); let id = "news-original".to_owned();
        let mut message = Kept::sent(id.clone(), original.clone(), &p, 0); message.awaiting_source();
        t.news.insert(p.id.clone(), News { msg: Some(message), lines: vec![("child".into(), "latest desired status".into())], said: vec![("child".into(), original.clone())], custody: NewsCustody::Pending, ..News::default() });
        let failed = Command::Send { id: id.clone(), text: original.clone(), mode: SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: None };
        t.unsent(&p.id, &failed);
        assert_eq!(news_of(&t, &p.id), original);
        assert!(t.plan(std::slice::from_ref(&p), 1_000_000, &[], &[]).is_empty(), "unaccepted original must not generate fresh ids or mutate on timeout/queue absence");
        let accepted = super::super::mail::Receipt { id: id.clone(), revision: 7, generation: 0, disposition: super::super::mail::Disposition::Waiting, repeat: false };
        let successor = t.mail_progress(&p.id, &accepted).ok_or_else(|| anyhow::anyhow!("latest desired snapshot was not published"))?;
        let Command::Send { id: next, text, .. } = successor else { anyhow::bail!("news revised by a mutable source edit") };
        assert_ne!(next, id); assert_eq!(text, "latest desired status");
        assert!(t.plan(std::slice::from_ref(&p), 2_000_000, &[], &[]).is_empty());
        let old_consumed = super::super::mail::Receipt { disposition: super::super::mail::Disposition::Consumed, generation: 1, ..accepted };
        assert!(t.mail_progress(&p.id, &old_consumed).is_none());
        assert_eq!(t.news.get(&p.id).and_then(|n| n.msg.as_ref()).map(|m| m.id.as_str()), Some(next.as_str()), "old receipt cannot retire the successor");
        Ok(())
    }

    #[test]
    fn only_exact_committed_consumption_retires_durable_news() -> anyhow::Result<()> {
        let mut t = Told::default(); let p = coordinator("parent"); let id = "news-exact".to_owned();
        let mut message = Kept::sent(id.clone(), "status".into(), &p, 0); message.awaiting_source();
        t.news.insert(p.id.clone(), News { msg: Some(message), lines: vec![("child".into(), "status".into())], said: vec![("child".into(), "status".into())], custody: NewsCustody::Pending, ..News::default() });
        let admitted = super::super::mail::Receipt { id: id.clone(), revision: 9, generation: 2, disposition: super::super::mail::Disposition::Admitted, repeat: false };
        assert!(t.mail_progress(&p.id, &admitted).is_none());
        let mut observed = p.clone(); observed.said.push(super::super::model::Given { id: id.clone(), digest: super::super::queued::stable("status") });
        assert!(t.plan(std::slice::from_ref(&observed), 5_000_000, &[], &[]).is_empty());
        assert!(t.news.get(&p.id).is_some_and(|n| n.msg.is_some()), "an index/old note_sent echo is not consumption");
        let stale = super::super::mail::Receipt { generation: 1, disposition: super::super::mail::Disposition::Consumed, ..admitted.clone() };
        t.mail_progress(&p.id, &stale); assert!(t.news.get(&p.id).is_some_and(|n| n.msg.is_some()));
        let consumed = super::super::mail::Receipt { disposition: super::super::mail::Disposition::Consumed, ..admitted };
        t.mail_progress(&p.id, &consumed);
        assert!(t.news.get(&p.id).is_some_and(|n| n.msg.is_none() && n.lines.is_empty()));
        Ok(())
    }

    fn producer_db() -> anyhow::Result<rusqlite::Connection> {
        let c = rusqlite::Connection::open_in_memory()?;
        c.execute_batch("CREATE TABLE kv(k TEXT PRIMARY KEY,v TEXT NOT NULL);")?;
        c.execute_batch(include_str!("../store/migrations/20261107-0600_told_runs.sql"))?;
        Ok(c)
    }

    fn producer_write(version: i64, id: &str, original: &str, desired: &str) -> anyhow::Result<ProducerWrite> {
        let command = Command::Send { id: id.into(), text: original.into(), mode: SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: None };
        let content = ProducerContent { owned: Owned::Pending { original: command }, desired: Desired { lines: vec![("run:r1".into(), desired.into())], ..Desired::default() }, said: vec![("run:r1".into(), original.into())], said_more: (0,0), previous: None };
        let record = ProducerRecord { version, content, captured_runs: vec![("r1".into(),version)] };
        let json = serde_json::to_string(&record)?;
        Ok(ProducerWrite { parent: "parent".into(), record, json })
    }

    #[test]
    fn producer_original_and_run_ack_commit_together_and_restore_exact_bytes() -> anyhow::Result<()> {
        let directory=tempfile::tempdir()?;let path=directory.path().join("producer.sqlite");
        let mut c=rusqlite::Connection::open(&path)?;
        c.execute_batch("CREATE TABLE kv(k TEXT PRIMARY KEY,v TEXT NOT NULL);")?;
        c.execute_batch(include_str!("../store/migrations/20261107-0600_told_runs.sql"))?;
        let mut ran = Ran::new(run_ended("r1"), 0, "runner", 1); ran.told = true;
        let producer = producer_write(20,"news-stable","original 日本語 \\\"","latest desired is separate")?;
        let expected = producer.record.content.clone();
        write_persist(&mut c, Persist { runs: vec![("parent".into(),"r1".into(),Some(ran),20)], producers: vec![producer] }, &HashSet::new())?;
        drop(c);let c=rusqlite::Connection::open(&path)?;
        let saved = read_producer(&c,"parent")?.ok_or_else(|| anyhow::anyhow!("producer was not committed"))?;
        assert_eq!(saved.content,expected);
        assert!(c.query_row("SELECT told FROM told_runs WHERE session='parent' AND key='r1'",[],|r|r.get::<_,bool>(0))?);
        let restored = restore_news(&saved)?;
        assert_eq!(restored.msg.as_ref().map(|m| m.id.as_str()),Some("news-stable"));
        assert_eq!(restored.msg.as_ref().map(|m| m.text.as_str()),Some("original 日本語 \\\""));
        assert_eq!(restored.lines.first().map(|l|l.1.as_str()),Some("latest desired is separate"));
        Ok(())
    }

    #[test]
    fn unarchived_original_cannot_be_replaced_or_acknowledge_a_run() -> anyhow::Result<()> {
        let mut c=producer_db()?;
        write_persist(&mut c,Persist { runs:Vec::new(),producers:vec![producer_write(30,"news-old","exact original","desired-a")?] },&HashSet::new())?;
        let mut ran=Ran::new(run_ended("r1"),0,"runner",1);ran.told=true;
        let failed=write_persist(&mut c,Persist { runs:vec![("parent".into(),"r1".into(),Some(ran),31)],producers:vec![producer_write(31,"news-new","new body","desired-b")?] },&HashSet::new());
        assert!(failed.is_err());
        let original=read_producer(&c,"parent")?.ok_or_else(||anyhow::anyhow!("original disappeared"))?;
        assert_eq!(original.content.owned.original().and_then(original_id),Some("news-old"));
        assert_eq!(c.query_row("SELECT count(*) FROM told_runs",[],|r|r.get::<_,i64>(0))?,0);
        Ok(())
    }

    #[test]
    fn stale_producer_capture_cannot_resurrect_a_deleted_newer_run_row() -> anyhow::Result<()> {
        let mut c=producer_db()?;
        write_persist(&mut c,Persist { runs:vec![("parent".into(),"r1".into(),None,50)],producers:vec![producer_write(50,"news-one","original","newer desired")?] },&HashSet::new())?;
        let mut ran=Ran::new(run_ended("r1"),0,"runner",1);ran.told=true;
        write_persist(&mut c,Persist { runs:vec![("parent".into(),"r1".into(),Some(ran),40)],producers:vec![producer_write(40,"news-one","original","older desired")?] },&HashSet::new())?;
        assert_eq!(c.query_row("SELECT count(*) FROM told_runs",[],|r|r.get::<_,i64>(0))?,0);
        assert_eq!(read_producer(&c,"parent")?.ok_or_else(||anyhow::anyhow!("no producer"))?.version,50);
        Ok(())
    }

    #[test]
    fn persistence_failure_keeps_dirty_keys_and_does_not_mutate_owned_original() -> anyhow::Result<()> {
        let mut c=producer_db()?;
        c.execute_batch("CREATE TRIGGER refuse_producer BEFORE INSERT ON kv BEGIN SELECT RAISE(ABORT,'injected producer failure'); END;")?;
        let mut t=Told::default();
        t.ran("runner",true,"parent",run_ended("r1"),1).map_err(anyhow::Error::msg)?;
        let saved=producer_write(60,"news-stable","original","desired")?;
        let news=restore_news(&saved.record)?;t.news.insert("parent".into(),news);t.mark_news("parent");
        let rows=t.dirty_rows()?;
        assert!(write_persist(&mut c,Persist { runs:rows,producers:vec![saved] },&HashSet::new()).is_err());
        assert!(t.dirty.contains(&("parent".into(),"r1".into())));
        assert!(t.news_dirty.contains("parent"));
        assert_eq!(t.news.get("parent").and_then(|n|n.msg.as_ref()).map(|m|m.id.as_str()),Some("news-stable"));
        assert_eq!(c.query_row("SELECT count(*) FROM told_runs",[],|r|r.get::<_,i64>(0))?,0);
        let refusal=Refusal::Persistence { error:super::super::mail::Error::StorageFull };
        assert_eq!(refusal.status(),http::StatusCode::INSUFFICIENT_STORAGE.as_u16());
        Ok(())
    }

    #[test]
    fn in_flight_newer_capture_fences_equal_older_acknowledgement() -> anyhow::Result<()> {
        let mut c=producer_db()?;
        let mut t=Told::default();
        t.ran("runner",true,"parent",run_ended("r1"),1).map_err(anyhow::Error::msg)?;
        t.news.insert("parent".into(),restore_news(&producer_write(1,"news-owned","original","desired-x")?.record)?);
        let older=t.capture()?;
        if let Some(n)=t.news.get_mut("parent") { n.lines=vec![("run:r1".into(),"desired-y".into())]; }
        t.mark_news("parent"); let newer=t.capture()?;
        if let Some(n)=t.news.get_mut("parent") { n.lines=vec![("run:r1".into(),"desired-x".into())]; }
        t.mark_news("parent");
        let committed=write_persist(&mut c,older,&HashSet::new())?;t.acknowledge(&committed);
        assert!(t.news_dirty.contains("parent"),"an equal X snapshot must not clear a newer in-flight Y capture");
        assert!(t.dirty.contains(&("parent".into(),"r1".into())));
        let committed=write_persist(&mut c,newer,&HashSet::new())?;t.acknowledge(&committed);
        assert!(t.news_dirty.contains("parent"));
        let latest=t.capture()?;let committed=write_persist(&mut c,latest,&HashSet::new())?;t.acknowledge(&committed);
        assert!(!t.news_dirty.contains("parent"));
        assert!(t.dirty.is_empty());
        assert_eq!(read_producer(&c,"parent")?.ok_or_else(||anyhow::anyhow!("no producer"))?.content.desired.lines.first().map(|l|l.1.as_str()),Some("desired-x"));
        Ok(())
    }

    #[test]
    fn archived_original_can_advance_but_stale_write_is_not_acknowledged() -> anyhow::Result<()> {
        let mut c=producer_db()?;
        write_persist(&mut c,Persist { runs:Vec::new(),producers:vec![producer_write(70,"old","original","desired")?] },&HashSet::new())?;
        let proof=HashSet::from([("parent".into(),"old".into())]);
        write_persist(&mut c,Persist { runs:Vec::new(),producers:vec![producer_write(80,"new","new original","new desired")?] },&proof)?;
        let stale=write_persist(&mut c,Persist { runs:Vec::new(),producers:vec![producer_write(75,"old","original","desired")?] },&HashSet::new())?;
        assert!(stale.producers.is_empty(),"a skipped write cannot clear an owned dirty key");
        assert_eq!(read_producer(&c,"parent")?.ok_or_else(||anyhow::anyhow!("no producer"))?.content.owned.original().and_then(original_id),Some("new"));
        Ok(())
    }

    /// One admitted original stays frozen through a long turn; the ledger keeps one latest
    /// never-dispatched successor and every prior immutable status body.
    #[test]
    fn a_parent_in_a_long_turn_has_one_message_of_news_waiting_each_child_s_latest_line_once() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let mut t=Told::default();let mut p=coordinator("q1");let ledger=NewsLedger::new(&p.id)?;
            let mut kids=helpers(&p.id,&["k1","k2","k3"]);let mut now=2_000_000;
            assert!(t.plan(&all(&p,&kids),now,&[],&[]).is_empty());
            now+=10_000;
            let first=one_send(&settle(&mut t,&p,&mut kids,&[(0,"The lexer is done."),(1,"Docs updated.")],now))?;
            assert!(words(&first)?.1.contains("Agents you started"));
            assert!(ledger.publish(&mut t,&first).await?.is_none());
            let frozen=ledger.admit(&mut t,&mut p,&first).await?;
            let mut originals=vec![first.clone()];let mut latest=first.clone();
            let rounds:[&[(usize,&str)];4]=[&[(0,"The parser is done."),(2,"The bench is recorded.")],&[(1,"Docs: round 1.")],&[(1,"Docs: round 2.")],&[(1,"Docs: round 3.")]];
            for changes in rounds {
                now+=10_000;latest=one_send(&settle(&mut t,&p,&mut kids,changes,now))?;
                assert_ne!(words(&latest)?.0,words(originals.last().ok_or_else(||anyhow::anyhow!("no original"))?)?.0);
                assert!(ledger.publish(&mut t,&latest).await?.is_none());originals.push(latest.clone());
                assert_eq!(p.queue,[Queued::of(words(&first)?.0,words(&first)?.1)],"source-owned original never changes");
            }
            let text=words(&latest)?.1;
            for old in ["The lexer is done.","Docs updated.","Docs: round 1.","Docs: round 2."] { assert!(!text.contains(old),"{old}: {text}"); }
            for expected in ["The parser is done.","The bench is recorded.","Docs: round 3."] { assert!(text.contains(expected),"{text}"); }
            assert_eq!(text.matches("which you started").count(),3);
            let history=ledger.history().await?;
            assert_eq!(history.iter().filter(|r|r.receipt.disposition==super::super::mail::Disposition::Waiting).count(),1,"only the latest never-dispatched status waits");
            assert_eq!(history.iter().filter(|r|r.receipt.disposition==super::super::mail::Disposition::Superseded).count(),3);
            take(&mut p,&[(words(&first)?.0,words(&first)?.1)]);
            assert!(t.plan(&all(&p,&kids),now+100_000,&[],&[]).is_empty(),"queue absence grants no consumption authority");
            assert_eq!(news_of(&t,&p.id),text);
            ledger.consume(&mut t,&frozen).await?;
            assert_eq!(news_of(&t,&p.id),text,"old receipt cannot retire a newer original");
            let delivered=ledger.admit(&mut t,&mut p,&latest).await?;
            take(&mut p,&[(words(&latest)?.0,words(&latest)?.1)]);ledger.consume(&mut t,&delivered).await?;
            now+=200_000;
            let fresh=one_send(&settle(&mut t,&p,&mut kids,&[(2,"The bench is green.")],now))?;
            assert!(words(&fresh)?.1.contains("The bench is green.")&&!words(&fresh)?.1.contains("Docs: round 3.")&&!words(&fresh)?.1.contains("Agents you started"));
            assert!(ledger.publish(&mut t,&fresh).await?.is_none());originals.push(fresh);
            ledger.originals(&originals).await?;
            Ok(())
        })
    }

    /// A successor racing older delivery is a distinct original. Old observations and receipts
    /// cannot remove its words; the actual source admits it after the frozen predecessor ends.
    #[test]
    fn what_an_edit_too_late_said_goes_again_in_the_next_message() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let mut t=Told::default();let mut p=coordinator("q2");let ledger=NewsLedger::new(&p.id)?;
            let mut kids=helpers(&p.id,&["j1","j2"]);let now=3_000_000;
            assert!(t.plan(&all(&p,&kids),now,&[],&[]).is_empty());
            let first=one_send(&settle(&mut t,&p,&mut kids,&[(0,"First news.")],now+10_000))?;
            assert!(ledger.publish(&mut t,&first).await?.is_none());let frozen=ledger.admit(&mut t,&mut p,&first).await?;
            let successor=one_send(&settle(&mut t,&p,&mut kids,&[(1,"Second news.")],now+20_000))?;
            assert_ne!(words(&successor)?.0,words(&first)?.0);
            assert!(words(&successor)?.1.contains("First news.")&&words(&successor)?.1.contains("Second news."));
            assert!(ledger.publish(&mut t,&successor).await?.is_none());
            take(&mut p,&[(words(&first)?.0,words(&first)?.1)]);
            assert!(t.plan(&all(&p,&kids),now+100_000,&[],&[]).is_empty());
            assert_eq!(news_of(&t,&p.id),words(&successor)?.1,"the late successor cannot disappear with the older queue observation");
            ledger.consume(&mut t,&frozen).await?;
            assert_eq!(news_of(&t,&p.id),words(&successor)?.1);
            let delivered=ledger.admit(&mut t,&mut p,&successor).await?;
            take(&mut p,&[(words(&successor)?.0,words(&successor)?.1)]);ledger.consume(&mut t,&delivered).await?;
            assert!(t.plan(&all(&p,&kids),now+200_000,&[],&[]).is_empty(),"exact consumed original is not republished");
            ledger.originals(&[first,successor]).await?;
            Ok(())
        })
    }

    /// CP-T3.2: queue absence, Info.said and the old grace interval grant no receipt authority.
    /// Exact source consumption, including for a later successor, retires each original once.
    #[test]
    fn news_edited_and_taken_between_two_looks_waits_to_hear_it_was_handed() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let mut t=Told::default();let mut p=coordinator("q9");let ledger=NewsLedger::new(&p.id)?;
            let mut kids=helpers(&p.id,&["t1","t2"]);let now=9_000_000;
            assert!(t.plan(&all(&p,&kids),now,&[],&[]).is_empty());
            let first=one_send(&settle(&mut t,&p,&mut kids,&[(0,"First news.")],now+10_000))?;
            assert!(ledger.publish(&mut t,&first).await?.is_none());
            let second=one_send(&settle(&mut t,&p,&mut kids,&[(1,"Second news.")],now+20_000))?;
            assert!(ledger.publish(&mut t,&second).await?.is_none());
            let delivered=ledger.admit(&mut t,&mut p,&second).await?;
            p.queue.clear();p.queued=0;
            for elapsed in [10_000,12_000,100_000+super::super::queued::HEARD_MS] {
                assert!(t.plan(&all(&p,&kids),now+20_000+elapsed,&[],&[]).is_empty());
                assert_eq!(news_of(&t,&p.id),words(&second)?.1,"a grace timeout cannot consume or regenerate an original");
            }
            p.said.push(super::super::model::Given { id:words(&second)?.0.into(),digest:super::super::queued::stable(words(&second)?.1) });
            assert!(t.plan(&all(&p,&kids),now+200_000,&[],&[]).is_empty());
            assert!(!news_of(&t,&p.id).is_empty(),"old note_sent/Info.said is not native acceptance");
            ledger.consume(&mut t,&delivered).await?;
            let third=one_send(&settle(&mut t,&p,&mut kids,&[(0,"Third news.")],now+300_000))?;
            assert!(words(&third)?.1.contains("Third news.")&&!words(&third)?.1.contains("Second news."));
            assert!(ledger.publish(&mut t,&third).await?.is_none());let third_delivery=ledger.admit(&mut t,&mut p,&third).await?;
            let fourth=one_send(&settle(&mut t,&p,&mut kids,&[(1,"Fourth news.")],now+400_000))?;
            assert!(ledger.publish(&mut t,&fourth).await?.is_none());p.queue.clear();p.queued=0;
            assert!(t.plan(&all(&p,&kids),now+500_000+super::super::queued::HEARD_MS,&[],&[]).is_empty());
            assert_eq!(news_of(&t,&p.id),words(&fourth)?.1);
            ledger.consume(&mut t,&third_delivery).await?;let last=ledger.admit(&mut t,&mut p,&fourth).await?;
            ledger.consume(&mut t,&last).await?;ledger.originals(&[first,second,third,fourth]).await?;
            Ok(())
        })
    }

    /// CP-T3.2, AC-T3.2.1: consumption can commit between planner looks. Reading a child after
    /// that exact receipt does not create another status for lines already delivered.
    #[test]
    fn news_taken_then_a_child_read_before_the_next_look_is_told_once() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let mut t=Told::default();let mut p=coordinator("q10");let ledger=NewsLedger::new(&p.id)?;
            let mut kids=helpers(&p.id,&["s1","s2"]);let now=10_000_000;
            assert!(t.plan(&all(&p,&kids),now,&[],&[]).is_empty());
            let first=one_send(&settle(&mut t,&p,&mut kids,&[(0,"First news.")],now+10_000))?;
            assert!(ledger.publish(&mut t,&first).await?.is_none());
            let second=one_send(&settle(&mut t,&p,&mut kids,&[(1,"Second news.")],now+20_000))?;
            assert!(words(&second)?.1.contains("First news.")&&words(&second)?.1.contains("Second news."));
            assert!(ledger.publish(&mut t,&second).await?.is_none());let delivery=ledger.admit(&mut t,&mut p,&second).await?;
            take(&mut p,&[(words(&second)?.0,words(&second)?.1)]);
            assert_eq!(news_of(&t,&p.id),words(&second)?.1,"source observation alone does not retire it");
            // The exact committed receipt arrives before the parent reads a result, without a planner look.
            ledger.consume(&mut t,&delivery).await?;
            let child=kids.first().ok_or_else(||anyhow::anyhow!("no child"))?;
            assert!(t.saw(child).is_none(),"already delivered lines need no successor");
            assert!(t.plan(&all(&p,&kids),now+100_000,&[],&[]).is_empty(),"nothing is told again after the exact receipt");
            ledger.originals(&[first,second]).await?;
            Ok(())
        })
    }

    /// A read before proven consumption creates a fresh reduced desired status, never a mutable
    /// source edit. The older receipt cannot retire that separately accepted successor.
    #[test]
    fn news_taken_then_a_child_read_while_its_handing_is_awaited_is_told_once() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let mut t=Told::default();let mut p=coordinator("q11");let ledger=NewsLedger::new(&p.id)?;
            let mut kids=helpers(&p.id,&["r1","r2"]);let now=11_000_000;
            assert!(t.plan(&all(&p,&kids),now,&[],&[]).is_empty());
            let first=one_send(&settle(&mut t,&p,&mut kids,&[(0,"First news.")],now+10_000))?;
            assert!(ledger.publish(&mut t,&first).await?.is_none());
            let second=one_send(&settle(&mut t,&p,&mut kids,&[(1,"Second news.")],now+20_000))?;
            assert!(ledger.publish(&mut t,&second).await?.is_none());let delivery=ledger.admit(&mut t,&mut p,&second).await?;
            p.queue.clear();p.queued=0;
            assert!(t.plan(&all(&p,&kids),now+30_000,&[],&[]).is_empty());
            let child=kids.first().ok_or_else(||anyhow::anyhow!("no child"))?;
            let (parent,third)=t.saw(child).ok_or_else(||anyhow::anyhow!("desired status was not revised"))?;
            assert_eq!(parent,p.id);assert_ne!(words(&third)?.0,words(&second)?.0);
            assert!(words(&third)?.1.contains("Second news.")&&!words(&third)?.1.contains("First news."));
            assert!(ledger.publish(&mut t,&third).await?.is_none());
            p.said.push(super::super::model::Given { id:words(&second)?.0.into(),digest:super::super::queued::stable(words(&second)?.1) });
            assert!(t.plan(&all(&p,&kids),now+100_000,&[],&[]).is_empty());
            ledger.consume(&mut t,&delivery).await?;
            assert_eq!(news_of(&t,&p.id),words(&third)?.1,"the old receipt cannot retire the immutable successor");
            let last=ledger.admit(&mut t,&mut p,&third).await?;take(&mut p,&[(words(&third)?.0,words(&third)?.1)]);
            ledger.consume(&mut t,&last).await?;
            assert!(t.plan(&all(&p,&kids),now+200_000,&[],&[]).is_empty());
            ledger.originals(&[first,second,third]).await?;
            Ok(())
        })
    }

    /// A preaccept refusal (no room on disk) preserves the producer's exact original until publication commits;
    /// newer desired words cannot silently replace it and planning does not create retries.
    #[test]
    fn news_that_didn_t_reach_its_parent_goes_at_the_next_look() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let mut t=Told::default();let mut p=coordinator("q6");let ledger=NewsLedger::new(&p.id)?;
            let mut kids=helpers(&p.id,&["w1","w2"]);let now=6_000_000;
            assert!(t.plan(&all(&p,&kids),now,&[],&[]).is_empty());
            let first=one_send(&settle(&mut t,&p,&mut kids,&[(0,"First news.")],now+10_000))?;
            // No room on disk for it: the one refusal left before custody (none for depth).
            let refused=super::super::mail::publish(&ledger.store,ledger.publication(&first)?,0).await;
            assert!(matches!(refused,Err(super::super::mail::Error::Capacity { .. })));
            assert!(ledger.history().await?.is_empty(),"preaccept refusal commits no body or custody");
            t.unsent(&p.id,&first);
            assert!(t.plan(&all(&p,&kids),now+20_000,&[],&[]).is_empty(),"the existing sender, not planning, owns retry");
            assert!(settle(&mut t,&p,&mut kids,&[(1,"Second news.")],now+30_000).is_empty());
            assert_eq!(news_of(&t,&p.id),words(&first)?.1,"new desired lines cannot overwrite the unaccepted original");
            let successor=ledger.publish(&mut t,&first).await?.ok_or_else(||anyhow::anyhow!("latest desired revision was not produced after custody"))?;
            assert_ne!(words(&successor)?.0,words(&first)?.0);assert!(words(&successor)?.1.contains("Second news."));
            assert!(ledger.publish(&mut t,&successor).await?.is_none());
            let delivery=ledger.admit(&mut t,&mut p,&successor).await?;
            take(&mut p,&[(words(&successor)?.0,words(&successor)?.1)]);ledger.consume(&mut t,&delivery).await?;
            assert!(t.plan(&all(&p,&kids),now+200_000,&[],&[]).is_empty());
            let history=ledger.history().await?;assert_eq!(history.first().map(|r|r.receipt.disposition),Some(super::super::mail::Disposition::Superseded));
            ledger.originals(&[first,successor]).await?;
            Ok(())
        })
    }

    /// New desired words wait through disconnection. Reconnection and a stale empty queue do
    /// not release the frozen stream; its exact receipt permits admission of the successor.
    #[test]
    fn news_that_came_while_its_parent_s_machine_was_away_goes_once_it_s_back() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let mut t=Told::default();let mut p=coordinator("q7");let ledger=NewsLedger::new(&p.id)?;
            let mut kids=helpers(&p.id,&["v1","v2"]);let now=7_000_000;
            assert!(t.plan(&all(&p,&kids),now,&[],&[]).is_empty());
            let first=one_send(&settle(&mut t,&p,&mut kids,&[(0,"v1 is done.")],now+10_000))?;
            assert!(ledger.publish(&mut t,&first).await?.is_none());let frozen=ledger.admit(&mut t,&mut p,&first).await?;
            p.status=Status::Disconnected;
            assert!(settle(&mut t,&p,&mut kids,&[(1,"v2 is done.")],now+20_000).is_empty());
            assert_eq!(news_of(&t,&p.id),words(&first)?.1);
            p.status=Status::Running;take(&mut p,&[(words(&first)?.0,words(&first)?.1)]);
            let back:Vec<Command>=t.plan(&all(&p,&kids),now+40_000,&[],&[]).into_iter().map(|(_,c)|c).collect();
            let successor=one_send(&back)?;assert_ne!(words(&successor)?.0,words(&first)?.0);
            assert!(words(&successor)?.1.contains("v2 is done."));assert!(ledger.publish(&mut t,&successor).await?.is_none());
            assert!(super::super::mail::pending(&ledger.store,p.id.clone(),0,super::super::state_view::FRAME).await?.items.is_empty(),"reconnect and absent queue cannot release the frozen stream");
            ledger.consume(&mut t,&frozen).await?;
            let latest=ledger.admit(&mut t,&mut p,&successor).await?;ledger.consume(&mut t,&latest).await?;
            assert!(t.plan(&all(&p,&kids),now+100_000,&[],&[]).is_empty());ledger.originals(&[first,successor]).await?;
            Ok(())
        })
    }

    /// A legacy source original stays untouched. New desired words enter the durable stream
    /// under a fresh managed identity, and its exact receipt clears inherited old context.
    #[test]
    fn news_a_server_before_left_waiting_is_added_to_not_queued_again() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let mut t=Told::default();let old="news-01a16251c49a75fba41b1fae971ba8a9";
            let before="Agent “Helper 0” (h0), which you started, ended its turn: from before the restart.";
            let mut p=Info { queue:vec![Queued::of(old,before)],queued:1,..coordinator("q8") };let ledger=NewsLedger::new(&p.id)?;
            let mut kids=helpers(&p.id,&["u1"]);let now=8_000_000;t.found(&p.id,old.into(),before.into(),now);
            assert!(t.plan(&all(&p,&kids),now,&[],&[]).is_empty());
            let managed=one_send(&settle(&mut t,&p,&mut kids,&[(0,"u1 is done.")],now+10_000))?;
            assert_ne!(words(&managed)?.0,old,"legacy source identity is never edited or reused");
            assert!(words(&managed)?.1.starts_with(before)&&words(&managed)?.1.contains("u1 is done."));
            assert_eq!(p.queue,[Queued::of(old,before)],"legacy original remains exactly as found");
            assert!(ledger.publish(&mut t,&managed).await?.is_none());let delivered=ledger.admit(&mut t,&mut p,&managed).await?;
            assert_eq!(p.queue.first(),Some(&Queued::of(old,before)));
            take(&mut p,&[(old,before),(words(&managed)?.0,words(&managed)?.1)]);
            assert!(t.plan(&all(&p,&kids),now+50_000,&[],&[]).is_empty());
            assert!(!news_of(&t,&p.id).is_empty(),"legacy queue observations cannot consume the managed successor");
            ledger.consume(&mut t,&delivered).await?;
            let next=one_send(&settle(&mut t,&p,&mut kids,&[(0,"u1 again.")],now+100_000))?;
            assert!(words(&next)?.1.contains("u1 again.")&&!words(&next)?.1.contains("before the restart"));
            assert!(ledger.publish(&mut t,&next).await?.is_none());ledger.originals(&[managed,next]).await?;
            Ok(())
        })
    }

    /// Reading results changes desired lines and coalesces pending successors. It cannot remove
    /// the frozen source original or erase an accepted body when the last desired line is read.
    #[test]
    fn what_a_parent_sees_itself_comes_out_of_its_news_and_the_message_with_the_last_line() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let mut t=Told::default();let mut p=coordinator("q3");let ledger=NewsLedger::new(&p.id)?;
            let mut kids=helpers(&p.id,&["x1","x2","x3"]);let now=4_000_000;
            assert!(t.plan(&all(&p,&kids),now,&[],&[]).is_empty());
            let first=one_send(&settle(&mut t,&p,&mut kids,&[(0,"x1 is done."),(1,"x2 is done."),(2,"x3 is done.")],now+10_000))?;
            assert!(ledger.publish(&mut t,&first).await?.is_none());let frozen=ledger.admit(&mut t,&mut p,&first).await?;
            let (x1,x2,x3)=match kids.as_slice() { [a,b,c]=>(a.clone(),b.clone(),c.clone()),_=>anyhow::bail!("three helpers") };
            let (parent,second)=t.saw(&x1).ok_or_else(||anyhow::anyhow!("first desired line did not change"))?;
            assert_eq!(parent,p.id);assert_ne!(words(&second)?.0,words(&first)?.0);
            assert!(!words(&second)?.1.contains("x1 is done.")&&words(&second)?.1.contains("x2 is done.")&&words(&second)?.1.contains("x3 is done."));
            assert!(ledger.publish(&mut t,&second).await?.is_none());
            let (_,third)=t.saw(&x2).ok_or_else(||anyhow::anyhow!("second desired line did not change"))?;
            assert!(words(&third)?.1.contains("x3 is done.")&&!words(&third)?.1.contains("x2 is done.")&&!words(&third)?.1.contains("Agents you started"));
            assert!(ledger.publish(&mut t,&third).await?.is_none());
            assert!(t.saw(&x3).is_none(),"an empty desired snapshot is not permission to remove accepted originals");
            assert!(t.news.get(&p.id).is_some_and(|n|n.lines.is_empty()&&n.msg.is_some()));
            assert_eq!(p.queue,[Queued::of(words(&first)?.0,words(&first)?.1)]);
            assert!(t.saw(&x1).is_none());assert!(t.plan(&all(&p,&kids),now+50_000,&[],&[]).is_empty());
            ledger.consume(&mut t,&frozen).await?;
            assert_eq!(news_of(&t,&p.id),words(&third)?.1);
            take(&mut p,&[(words(&first)?.0,words(&first)?.1)]);
            let last=ledger.admit(&mut t,&mut p,&third).await?;ledger.consume(&mut t,&last).await?;
            assert!(t.plan(&all(&p,&kids),now+100_000,&[],&[]).is_empty());
            let again=one_send(&settle(&mut t,&p,&mut kids,&[(0,"x1 is done again.")],now+200_000))?;
            assert!(words(&again)?.1.contains("x1 is done again.")&&!words(&again)?.1.contains("x2 is done."));
            assert!(ledger.publish(&mut t,&again).await?.is_none());ledger.originals(&[first,second,third,again]).await?;
            Ok(())
        })
    }

    /// An ended parent isn't sent its children's news; one whose machine is away is, once it's
    /// back; and a parent's news names at most `LINES_MAX` children, saying how many more settled.
    #[test]
    fn news_waits_for_a_parent_s_machine_and_names_a_bounded_number_of_children() -> anyhow::Result<()> {
        let mut t = Told::default();
        let mut p = Info { status: Status::Disconnected, ..coordinator("q4") };
        let ids: Vec<String> = (0..LINES_MAX + 3).map(|n| format!("y{n}")).collect();
        let mut kids = helpers("q4", &ids.iter().map(String::as_str).collect::<Vec<_>>());
        let mut now = 5_000_000;
        let _ = t.plan(&all(&p, &kids), now, &[], &[]);
        now += 10_000;
        let which: Vec<(usize, &str)> = (0..kids.len()).map(|k| (k, "Done.")).collect();
        anyhow::ensure!(settle(&mut t, &p, &mut kids, &which, now).is_empty(), "its machine is away");
        p.status = Status::Running;
        let back = t.plan(&all(&p, &kids), now + 20_000, &[], &[]);
        let Some((_, Command::Send { text, .. })) = back.first() else { anyhow::bail!("nothing once it's back: {back:?}") };
        anyhow::ensure!(text.matches("which you started").count() == LINES_MAX && text.contains("and 3 more that settled before these"), "{text}");
        // Ended: nothing for it, and its news goes.
        let mut ended = Info { status: Status::Exited, ..coordinator("q5") };
        let mut kids = helpers("q5", &["z1"]);
        let _ = t.plan(&all(&ended, &kids), now, &[], &[]);
        anyhow::ensure!(settle(&mut t, &ended, &mut kids, &[(0, "Done.")], now + 10_000).is_empty());
        ended.status = Status::Running;
        anyhow::ensure!(t.plan(&all(&ended, &kids), now + 30_000, &[], &[]).is_empty(), "told once, while it had ended: not again");
        Ok(())
    }

    /// News of run `key` (`r1` as its agent knows it) of `cargo test -p kiln` on kiln-a094a12d,
    /// handed to its agent once.
    fn run_news(key: &str, event: RunEvent) -> RunNews {
        RunNews { key: key.into(), id: key.into(), event, command: "cargo test -p kiln".into(), on: "kiln-a094a12d".into(), how: "run".into(), handed: 1, ..RunNews::default() }
    }

    /// Run `key`'s end: exit code 3 after 4m 12s, its output ending in a test summary.
    fn run_ended(key: &str) -> RunNews {
        RunNews {
            started_at: Some(1_000_000),
            ended_at: Some(1_252_000),
            exit_code: Some(3),
            tail: "running 412 tests\ntest result: FAILED. 409 passed; 3 failed".into(),
            ..run_news(key, RunEvent::Ended)
        }
    }

    /// `news` from runner `kiln-a094a12d`, where `session` is.
    fn heard(t: &mut Told, session: &str, news: RunNews, now: i64) -> Option<Command> {
        t.ran(RUNNER, true, session, news.clipped(), now).ok().flatten()
    }

    const RUNNER: &str = "kiln-a094a12d";

    fn going(t: &Told, agent: &str) -> bool {
        t.runs.get(agent).is_some_and(|rs| rs.iter().any(Ran::going))
    }

    /// An idle agent whose last turn ended at `at` (its harness's stamp).
    fn idle_after(id: &str, at: i64) -> Info {
        Info { last_turn: Some(TurnEnd { at, how: "done".into(), ..TurnEnd::default() }), status: Status::Idle, ..coordinator(id) }
    }

    /// Agents are told, not polled (docs/reach.md §7): a run its agent left in the background is
    /// work it has going until it ends, not news; its end is, at the next look: where it ran, how
    /// long, its exit code and the end of its output, in the one message of news behind the
    /// agent's turn. Nothing calls `output` for it, and it's told once.
    #[test]
    fn agents_are_told_not_polled_a_background_run_s_end_reaches_its_news_with_no_output_call() -> anyhow::Result<()> {
        let mut t = Told::default();
        let p = coordinator("a1");
        let now = 1_000_000;
        anyhow::ensure!(heard(&mut t, "a1", run_news("r1", RunEvent::Going), now).is_none());
        anyhow::ensure!(going(&t, "a1"), "a run in the background is work its agent has going");
        anyhow::ensure!(t.plan(std::slice::from_ref(&p), now + 1, &[], &[]).is_empty(), "going isn't news");
        let end = now + 252_000;
        anyhow::ensure!(heard(&mut t, "a1", run_ended("r1"), end).is_none());
        anyhow::ensure!(!going(&t, "a1"), "it ended");
        let sent = t.plan(std::slice::from_ref(&p), end, &[], &[]);
        let [(to, Command::Send { text, mode: SendMode::Queue, id, .. })] = sent.as_slice() else { anyhow::bail!("no news: {sent:?}") };
        anyhow::ensure!(to == "a1" && id.starts_with(NEWS), "{sent:?}");
        for said in [
            "Run r1 (`cargo test -p kiln`), which you started in the background, ended on kiln-a094a12d after 4m 12s with exit code 3.",
            "Its output ended with:\n```\nrunning 412 tests\ntest result: FAILED. 409 passed; 3 failed\n```",
            "`output {\"id\": \"r1\"}` has its output since you last read it.",
        ] {
            anyhow::ensure!(text.contains(said), "{said:?} missing: {text}");
        }
        anyhow::ensure!(t.plan(&[p], end + 10_000, &[], &[]).is_empty(), "told once");
        Ok(())
    }

    /// T8: a start alone does not wake. A later end supersedes the never-dispatched status,
    /// while both immutable originals remain in history and only the latest is admitted.
    #[test]
    fn t08_a_run_s_start_is_state_its_end_wakes() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let mut t=Told::default();let mut p=coordinator("a2");let ledger=NewsLedger::new(&p.id)?;
            let mut kids=helpers(&p.id,&["c2"]);let now=2_000_000;
            assert!(t.plan(&all(&p,&kids),now,&[],&[]).is_empty());
            let _=heard(&mut t,&p.id,run_news("r2",RunEvent::Going),now);
            let _=heard(&mut t,&p.id,run_news("r2",RunEvent::Started),now+500);
            assert!(going(&t,&p.id));assert!(t.plan(&all(&p,&kids),now+600,&[],&[]).is_empty(),"start alone wakes no one");
            let started=one_send(&settle(&mut t,&p,&mut kids,&[(0,"The parser is done.")],now+700))?;
            assert!(words(&started)?.1.contains("The parser is done.")&&words(&started)?.1.contains("started on kiln-a094a12d"));
            assert!(ledger.publish(&mut t,&started).await?.is_none());
            let _=heard(&mut t,&p.id,run_ended("r2"),now+60_000);
            let changes:Vec<Command>=t.plan(&all(&p,&kids),now+60_000,&[],&[]).into_iter().map(|(_,c)|c).collect();
            let ended=one_send(&changes)?;assert_ne!(words(&ended)?.0,words(&started)?.0);
            assert!(words(&ended)?.1.contains("ended on kiln-a094a12d")&&!words(&ended)?.1.contains("started on"));
            assert!(ledger.publish(&mut t,&ended).await?.is_none());
            let history=ledger.history().await?;assert_eq!(history.first().map(|r|r.receipt.disposition),Some(super::super::mail::Disposition::Superseded));
            let delivery=ledger.admit(&mut t,&mut p,&ended).await?;
            assert_eq!(p.queue,[Queued::of(words(&ended)?.0,words(&ended)?.1)],"only the latest never-dispatched status reaches source admission");
            ledger.consume(&mut t,&delivery).await?;ledger.originals(&[started,ended]).await?;
            Ok(())
        })
    }

    /// T9: news said twice is one: a start said again isn't told again, nor an end.
    #[test]
    fn t09_the_same_news_twice_is_told_once() -> anyhow::Result<()> {
        let mut t = Told::default();
        let p = coordinator("a9");
        let now = 9_000_000;
        let _ = heard(&mut t, "a9", run_news("r9", RunEvent::Started), now);
        let first = t.due_runs(std::slice::from_ref(&p), now, &|_| true);
        anyhow::ensure!(first.get("a9").is_some_and(|l| l.len() == 1), "{first:?}");
        let _ = heard(&mut t, "a9", run_news("r9", RunEvent::Started), now + 10);
        anyhow::ensure!(t.due_runs(std::slice::from_ref(&p), now + 10, &|_| true).is_empty(), "a start said again isn't news again");
        let _ = heard(&mut t, "a9", run_ended("r9"), now + 20);
        anyhow::ensure!(t.plan(std::slice::from_ref(&p), now + 20, &[], &[]).len() == 1);
        let _ = heard(&mut t, "a9", run_ended("r9"), now + 30);
        anyhow::ensure!(t.plan(std::slice::from_ref(&p), now + 30, &[], &[]).is_empty(), "its end said again isn't news again");
        Ok(())
    }

    /// T2: an agent is held back only by runs it ended its turn waiting for: those handed over in
    /// the turn that just ended (or handed again then, an `output` that returned with one going).
    /// A dev server it started turns ago holds nothing.
    #[test]
    fn t02_only_runs_handed_over_in_the_turn_that_ended_hold_the_agent() -> anyhow::Result<()> {
        let mut t = Told::default();
        // This server first sees it after a turn ended (stamped 100): every run counts.
        anyhow::ensure!(!t.awaited(&idle_after("a7", 100), 900));
        let _ = heard(&mut t, "a7", run_news("dev", RunEvent::Going), 1_000);
        anyhow::ensure!(t.awaited(&idle_after("a7", 100), 1_100), "handed in the turn it ended");
        // Its next turn ends (seen at 2 000): the dev server was handed in the turn before.
        anyhow::ensure!(t.awaited(&idle_after("a7", 200), 2_000), "first seen ends count every run");
        anyhow::ensure!(!t.awaited(&idle_after("a7", 300), 3_000), "a run handed turns ago holds nothing");
        anyhow::ensure!(going(&t, "a7"), "it goes still");
        // A test run handed in the turn that ends next holds it, and so does the dev server
        // handed again then.
        let _ = heard(&mut t, "a7", run_news("test", RunEvent::Going), 3_500);
        anyhow::ensure!(t.awaited(&idle_after("a7", 400), 4_000));
        let _ = heard(&mut t, "a7", run_ended("test"), 4_100);
        anyhow::ensure!(!t.awaited(&idle_after("a7", 400), 4_200), "it ended");
        let _ = heard(&mut t, "a7", RunNews { handed: 2, ..run_news("dev", RunEvent::Going) }, 4_300);
        anyhow::ensure!(t.awaited(&idle_after("a7", 500), 5_000), "handed again in that turn");
        Ok(())
    }

    /// T4: a run whose runner went holds its agent no more (nothing is left to say how it ends);
    /// its runner back says it again, and the hold is back, nothing told twice.
    #[test]
    fn t04_a_lost_hold_comes_back_when_its_runner_says_the_run_again() -> anyhow::Result<()> {
        let mut t = Told::default();
        let p = coordinator("a4");
        let _ = heard(&mut t, "a4", run_news("k1", RunEvent::Going), 1_000);
        anyhow::ensure!(t.plan_with(std::slice::from_ref(&p), 1_001, &[], &[], &|_| true).is_empty() && going(&t, "a4"));
        anyhow::ensure!(t.plan_with(std::slice::from_ref(&p), 1_002, &[], &[], &|r| r != RUNNER).is_empty());
        anyhow::ensure!(!going(&t, "a4"), "its runner went");
        let _ = heard(&mut t, "a4", run_news("k1", RunEvent::Going), 1_003);
        anyhow::ensure!(going(&t, "a4"), "said again: held again");
        anyhow::ensure!(t.plan(std::slice::from_ref(&p), 1_004, &[], &[]).is_empty(), "and nothing told");
        Ok(())
    }

    /// T5, E8: news of a run counts from the runner that follows it, wherever its session is now
    /// (a detached run's runner may host it no more); from another runner it doesn't, nor a first
    /// word of a run from a runner its session isn't on.
    #[test]
    fn t05_e8_news_counts_from_the_runner_that_follows_the_run_wherever_its_session_is() -> anyhow::Result<()> {
        let mut t = Told::default();
        t.ran(RUNNER, true, "a5", run_news("d1", RunEvent::Going), 1_000).map_err(anyhow::Error::msg)?;
        // The session moved to another runner; the detached run's runner says how it ended.
        t.ran(RUNNER, false, "a5", run_ended("d1"), 2_000).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(t.ran("mallory", true, "a5", RunNews { exit_code: Some(0), ..run_ended("d1") }, 2_001).is_err(), "another runner's word of it doesn't count");
        anyhow::ensure!(t.ran("mallory", false, "a5", run_ended("d2"), 2_002).is_err(), "a first word from where the session isn't doesn't count");
        let p = coordinator("a5");
        let sent = t.plan(std::slice::from_ref(&p), 2_003, &[], &[]);
        anyhow::ensure!(matches!(sent.as_slice(), [(_, Command::Send { text, .. })] if text.contains("exit code 3")), "{sent:?}");
        Ok(())
    }

    /// T7: news made when the MCP server that followed a run ended names no id to read its output
    /// by (none is left), and says what stopped it.
    #[test]
    fn t07_news_of_a_run_whose_mcp_server_ended_names_no_output() {
        let stopped = RunNews { id: String::new(), exit_code: None, error: Some("stopped: the kiln MCP server that followed it ended".into()), tail: String::new(), ..run_ended("k7") };
        let line = run_line(&stopped, 0);
        assert!(!line.contains("`output") && line.starts_with("A run (`cargo test -p kiln`) you started in the background ended on kiln-a094a12d after 4m 12s, with no exit code: stopped"), "{line}");
    }

    /// T11, T13: the end of a run's output is cut where it's made to its line's share of a message
    /// (`LINE_SHARE`, what a message shows of a line), looking at no more than that of the output;
    /// a run's line is never longer than its share.
    #[test]
    fn t11_t13_a_run_s_text_is_clipped_at_the_source_to_what_its_line_shows() {
        assert_eq!(LINE_SHARE, super::super::model::TEXT_MAX / LINES_MAX);
        let long: String = (0..400_000).map(|n| format!("line {n}\n")).collect();
        let tail = clip_tail(&long, LINE_SHARE);
        assert!(tail.len() <= LINE_SHARE && tail.starts_with("line ") && tail.ends_with("line 399999"), "{}", tail.len());
        assert_eq!(clip_tail("short\n\n", 40), "short");
        assert_eq!(clip_tail("ééé", 3), "é", "a character's boundary");
        let news = RunNews { command: "x".repeat(3 * LINE_SHARE), tail: long.clone(), error: Some("e".repeat(2 * LINE_SHARE)), ..run_ended("k11") }.clipped();
        assert!(news.command.len() <= LINE_SHARE && news.tail.len() <= LINE_SHARE && news.error.as_ref().is_some_and(|e| e.len() <= LINE_SHARE));
        assert!(run_line(&news, 0).len() <= LINE_SHARE);
        let line = run_line(&RunNews { tail: long, ..run_ended("k12") }, 0);
        assert!(line.len() <= LINE_SHARE && line.contains("line 399999\n```"), "the end of its output in what's left of the line");
    }

    /// T13: a child stands settled as long as a queued message takes to reach its harness, measured
    /// (RFC 6298's RTO over those deliveries), not 3 s picked; before any is measured, RFC 6298's 1 s.
    #[test]
    fn t13_a_child_s_settle_is_the_measured_delivery_rto() {
        let mut t = Told::default();
        assert_eq!(i64::try_from(t.settle.ms()).ok(), Some(SETTLE_MS));
        let mut kid = Info { parent: Some("p1".into()), ..child("s1", Status::Running, 0, "") };
        let mut now = 1_000_000;
        for _ in 0..8 {
            // It ends a turn with a message waiting; its harness is handed it, and starts on it 4 s on.
            kid.status = Status::Idle;
            kid.queued = 1;
            kid.last_turn = Some(TurnEnd { at: now, how: "done".into(), ..TurnEnd::default() });
            let _ = t.due(std::slice::from_ref(&kid), now, &[]);
            kid.queued = 0;
            let _ = t.due(std::slice::from_ref(&kid), now + 1, &[]);
            now += 4_001;
            kid.status = Status::Running;
            let _ = t.due(std::slice::from_ref(&kid), now, &[]);
            now += 10_000;
        }
        assert!(t.settle.ms() >= 4_000, "{}", t.settle.ms());
        let mut r = Rto::default();
        r.sample(2_000);
        assert_eq!(r.ms(), 6_000, "RFC 6298 §2.2: SRTT = R, RTTVAR = R/2, RTO = SRTT + 4·RTTVAR");
        r.sample(2_000);
        assert_eq!(r.ms(), 5_000, "§2.3: RTTVAR = 3/4·1000 + 1/4·0, SRTT = 2000");
        r.expired();
        assert_eq!(r.ms(), 10_000, "§5.5: doubled");
        for _ in 0..10 {
            r.expired();
        }
        assert_eq!(r.ms(), Rto::MAX_MS, "§2.5: at most 60 s");
        let mut fast = Rto::default();
        fast.sample(2);
        assert_eq!(fast.ms(), Rto::MIN_MS, "§2.4: at least 1 s");
    }

    /// T13: a run over and told is kept only while its runner may say it again: once that runner
    /// says more, it had the answer, and the run is forgotten (no count of runs kept, picked).
    #[test]
    fn t13_a_finished_run_is_forgotten_once_its_runner_said_more() -> anyhow::Result<()> {
        let mut t = Told::default();
        let p = coordinator("a8");
        let _ = heard(&mut t, "a8", run_ended("e1"), 1_000);
        anyhow::ensure!(t.plan(std::slice::from_ref(&p), 1_000, &[], &[]).len() == 1);
        let _ = heard(&mut t, "a8", run_ended("e1"), 1_100);
        anyhow::ensure!(t.plan(std::slice::from_ref(&p), 1_100, &[], &[]).is_empty() && t.runs.get("a8").is_some_and(|r| r.len() == 1), "said again in the window: kept, not told");
        let _ = heard(&mut t, "a8", run_news("e2", RunEvent::Going), 1_200);
        let _ = t.plan(std::slice::from_ref(&p), 1_200, &[], &[]);
        anyhow::ensure!(t.runs.get("a8").is_some_and(|r| r.len() == 1 && r.iter().all(|r| r.news.key == "e2")), "its runner said more: e1 forgotten");
        Ok(())
    }

    /// E1: what kiln holds of runs outlives a server restart: a run going holds its agent still, a
    /// run's end not yet told is told by the next server, once, and one told isn't told again.
    #[test]
    fn e1_runs_and_holds_outlive_a_server_restart() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let store = crate::store::Store::in_memory()?;
            let mut before = Told { loaded: true, ..Told::default() };
            let p = coordinator("a10");
            let _ = heard(&mut before, "a10", run_news("g1", RunEvent::Going), 1_000);
            let _ = heard(&mut before, "a10", run_ended("t1"), 1_000);
            anyhow::ensure!(before.plan(std::slice::from_ref(&p), 1_000, &[], &[]).len() == 1, "t1 told by the first server");
            let first = before.dirty_rows()?;
            let _ = heard(&mut before, "a10", RunNews { handed: 2, ..run_news("g1", RunEvent::Going) }, 1_200);
            let _ = heard(&mut before, "a10", run_ended("u1"), 1_300);
            // The later state reaches the store first: the earlier one, after, changes nothing.
            write_rows(&store, before.dirty_rows()?).await?;
            write_rows(&store, first).await?;
            let mut after = Told::default();
            after.adopt(read_rows(&store).await?);
            anyhow::ensure!(after.runs.get("a10").and_then(|r| r.iter().find(|r| r.news.key == "g1")).is_some_and(|r| r.news.handed == 2), "the later state kept");
            anyhow::ensure!(after.awaited(&idle_after("a10", 1_150), 2_000), "g1 holds its agent after the restart");
            let sent = after.plan(std::slice::from_ref(&p), 2_000, &[], &[]);
            anyhow::ensure!(matches!(sent.as_slice(), [(_, Command::Send { text, .. })] if text.contains("Run u1") && !text.contains("Run t1")), "u1 told once, t1 not again: {sent:?}");
            let _ = heard(&mut after, "a10", run_ended("u1"), 2_100);
            anyhow::ensure!(after.plan(std::slice::from_ref(&p), 2_100, &[], &[]).is_empty(), "u1 said again after the restart isn't news");
            Ok(())
        })
    }

    /// Seeing a run changes desired news without cancelling the accepted original. Only an
    /// exact source receipt retires it; seen-before-publication news still creates no message.
    #[test]
    fn what_an_agent_saw_of_its_run_comes_out_of_its_news() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let mut t=Told::default();let mut p=coordinator("a3");let ledger=NewsLedger::new(&p.id)?;let now=3_000_000;
            let _=heard(&mut t,&p.id,run_ended("r3"),now);
            let commands:Vec<Command>=t.plan(std::slice::from_ref(&p),now,&[],&[]).into_iter().map(|(_,c)|c).collect();
            let original=one_send(&commands)?;assert!(ledger.publish(&mut t,&original).await?.is_none());
            let delivered=ledger.admit(&mut t,&mut p,&original).await?;
            let seen=heard(&mut t,&p.id,run_news("r3",RunEvent::Seen),now+100);
            assert!(seen.is_none(),"reading a run cannot issue QueueRemove against a frozen accepted original");
            assert!(t.news.get(&p.id).is_some_and(|n|n.lines.is_empty()&&n.msg.is_some()));
            assert_eq!(p.queue,[Queued::of(words(&original)?.0,words(&original)?.1)]);
            take(&mut p,&[(words(&original)?.0,words(&original)?.1)]);
            assert_eq!(news_of(&t,&p.id),words(&original)?.1,"queue absence is not delivery");
            ledger.consume(&mut t,&delivered).await?;
            let _=heard(&mut t,&p.id,run_ended("r4"),now+20_000);
            assert!(heard(&mut t,&p.id,run_news("r4",RunEvent::Seen),now+20_001).is_none());
            assert!(t.plan(std::slice::from_ref(&p),now+20_002,&[],&[]).is_empty(),"seen-before-publication news is not published");
            ledger.originals(&[original]).await?;
            Ok(())
        })
    }

    /// News of a run that came after later news of it (two that came the other way round) changes
    /// nothing: a run that ended isn't held going again (T10).
    #[test]
    fn t10_news_of_a_run_that_came_late_changes_nothing() -> anyhow::Result<()> {
        let mut t = Told::default();
        let p = coordinator("a6");
        let _ = heard(&mut t, "a6", run_ended("r5"), 4_000);
        let _ = heard(&mut t, "a6", run_news("r5", RunEvent::Going), 4_010);
        anyhow::ensure!(!going(&t, "a6"), "a going that came late holds nothing");
        anyhow::ensure!(t.plan(std::slice::from_ref(&p), 4_020, &[], &[]).len() == 1);
        Ok(())
    }

    /// A child settling while a run's original awaits custody becomes one combined successor.
    /// The earlier run-only body stays immutable; an ended agent gets no new run news.
    #[test]
    fn a_run_s_news_and_a_child_s_are_one_message() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let mut t = Told::default();
            let p = coordinator("a11");
            let ledger = NewsLedger::new(&p.id)?;
            let mut kids = helpers("a11", &["c11"]);
            let now = 5_000_000;
            anyhow::ensure!(t.plan(&all(&p, &kids), now, &[], &[]).is_empty(), "seen at work first");
            let _ = heard(&mut t, "a11", run_ended("r6"), now);
            let original = one_send(&settle(&mut t, &p, &mut kids, &[(0, "The parser is done.")], now))?;
            assert!(words(&original)?.1.starts_with("Run r6") && !words(&original)?.1.contains("The parser is done."));
            assert_eq!(news_of(&t, "a11"), words(&original)?.1, "pre-custody original cannot be overwritten");
            let combined = ledger.publish(&mut t, &original).await?.ok_or_else(|| anyhow::anyhow!("settled child missing from successor"))?;
            assert_ne!(words(&combined)?.0, words(&original)?.0);
            let text = words(&combined)?.1;
            anyhow::ensure!(text.starts_with("Agents you started:\n- Agent") && text.contains("The parser is done.") && text.contains("\n\nRuns you started:\n- Run r6 (`cargo test -p kiln`)"), "{text}");
            anyhow::ensure!(text.contains("\n  Its output ended with:\n  ```\n  running 412 tests"), "the end of its output stays in its item: {text}");
            assert!(ledger.publish(&mut t, &combined).await?.is_none());
            assert_eq!(news_of(&t, "a11"), text);
            ledger.originals(&[original.clone(), combined.clone()]).await?;
            let history = ledger.history().await?;
            let original_id = words(&original)?.0;
            assert_eq!(history.iter().find(|r| r.receipt.id == original_id).map(|r| &r.receipt.disposition), Some(&super::super::mail::Disposition::Superseded));
            let _ = heard(&mut t, "a11", run_ended("r7"), now + 200);
            let ended = Info { status: Status::Exited, ..p };
            anyhow::ensure!(t.plan(&[ended], now + 201, &[], &[]).is_empty() && !t.runs.contains_key("a11"));
            ledger.originals(&[original, combined]).await?;
            Ok(())
        })
    }

    /// Placement's answer folds into the one message (docs/needs.md E5): a place in line is state
    /// (no message of its own), "never" wakes the agent, saying why.
    #[test]
    fn placement_s_answer_folds_into_the_one_message() -> anyhow::Result<()> {
        let mut t = Told::default();
        let p = coordinator("a12");
        let q = AnswerNews { key: "q1".into(), id: "r12".into(), command: "make".into(), answer: Answer::Queued { place: 3, expected_start: Some(1_240_000) } }.into_run();
        t.ran("", true, "a12", q, 1_000_000).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(t.plan(std::slice::from_ref(&p), 1_000_000, &[], &[]).is_empty(), "a place in line wakes no one");
        anyhow::ensure!(t.awaited(&idle_after("a12", 1), 1_000_001), "a run in line it ended its turn waiting for holds it");
        let line = run_line(&t.runs.get("a12").and_then(|r| r.first()).map(|r| r.news.clone()).unwrap_or_default(), 1_000_000);
        anyhow::ensure!(line == "Run r12 (`make`), which you started in the background, waits for a machine with room, number 3 in line, expected to start in about 4m 0s: kiln tells you when it ends.", "{line}");
        let never = AnswerNews { key: "q1".into(), id: "r12".into(), command: "make".into(), answer: Answer::Never { why: "no runner has a GPU".into() } }.into_run();
        t.ran("", true, "a12", never, 1_000_100).map_err(anyhow::Error::msg)?;
        let sent = t.plan(std::slice::from_ref(&p), 1_000_100, &[], &[]);
        anyhow::ensure!(matches!(sent.as_slice(), [(_, Command::Send { text, .. })] if text == "Run r12 (`make`), which you started in the background, will never start: no runner has a GPU."), "{sent:?}");
        // The runner that runs it may say the rest, though kiln said the first word.
        t.ran(RUNNER, true, "a12", RunNews { key: "q1".into(), ..run_ended("q1") }, 1_000_200).map_err(anyhow::Error::msg)?;
        Ok(())
    }

    /// AUDIT O5: placement's answer as its asking runner says it (`Question::Answer`) is the answer
    /// it was, queued, started or never; anything else isn't one.
    #[test]
    fn o5_an_answer_said_by_its_runner_is_the_answer_it_was() {
        for answer in [Answer::Queued { place: 2, expected_start: Some(9_000) }, Answer::Started { on: "linux".into() }, Answer::Never { why: "never: no runner has a GPU".into() }] {
            let a = AnswerNews { key: "k".into(), id: "r1".into(), command: "make".into(), answer };
            assert_eq!(AnswerNews::of(&a.clone().into_run()), Some(a));
        }
        assert_eq!(AnswerNews::of(&run_ended("r7")), None);
    }

    /// How a run's news says how long it took, and a detached run.
    #[test]
    fn a_run_s_news_says_how_long_and_how_it_was_started() {
        assert_eq!([took(400), took(12_900), took(252_000), took(3_780_000)], ["less than a second", "12s", "4m 12s", "1h 3m"]);
        let killed = RunNews { exit_code: None, error: Some("kiln-a094a12d no longer has this run (its runner restarted)".into()), tail: String::new(), ..run_ended("r7") };
        assert!(run_line(&killed, 0).ends_with("ended on kiln-a094a12d after 4m 12s, with no exit code: kiln-a094a12d no longer has this run (its runner restarted).\n`output {\"id\": \"r7\"}` has its output since you last read it."), "{}", run_line(&killed, 0));
        let program = RunNews { how: "kiln.machines.start".into(), command: "make\nmake test".into(), ..run_news("r8", RunEvent::Started) };
        assert_eq!(run_line(&program, 0), "Run r8 (`make…`), which your program started in the background, started on kiln-a094a12d: kiln tells you when it ends.");
        let detached = RunNews { id: String::new(), how: DETACHED.into(), ..run_ended("k1") };
        assert!(run_line(&detached, 0).starts_with("`kiln run --detach -- cargo test -p kiln`, which you left running, ended on kiln-a094a12d after 4m 12s with exit code 3.") && !run_line(&detached, 0).contains("`output"));
    }
}
