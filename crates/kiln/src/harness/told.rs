//! A parent hears from its children: when an agent another agent started settles (its turn over,
//! a person needed, stopped, ended, paused at a usage limit), kiln tells its parent, as Claude
//! Code tells an agent of its own background subagents: a message after the parent's turn, which
//! starts one if the parent is idle. So a parent needn't wait for its children: it can start
//! them, end its turn, and take up what each concluded when it's told. `agents_wait` is for the
//! parent that would rather wait; what it saw that way (or through `agents_result`) isn't told
//! again. Children that settle together are told of in one message.
//!
//! The news waits for the parent in that one message (`super::queued`): a child that settles
//! again while it waits changes its line in it, one that settles anew joins it, and what the parent
//! sees itself comes out of it (the message too, once nothing is left). So a parent that spends
//! hours in one turn, its children settling again and again, finds each child's latest line when
//! the turn ends, once, rather than every settle since. A server that started since finds the news
//! a server before it left waiting (its id says what it is) and adds to that one.
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
}

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
}

static TOLD: LazyLock<Mutex<Told>> = LazyLock::new(Default::default);
/// What goes to each parent, in the order it was decided: `tick` and `saw` queue it here as they
/// decide it (under `TOLD`'s lock, so nothing decided after it goes before it), and one sender a
/// parent sends it, each command once the one before it reached the parent's machine or is said
/// unsent. Two edits of one message can't cross on their way, and nothing that decides waits on a
/// parent's machine: a command waits up to 30 s for an answer, and one lock held across every
/// parent's sends in turn once held `agents_wait` and `agents_result` waiting on it.
static TO_SEND: LazyLock<Mutex<HashMap<String, VecDeque<Command>>>> = LazyLock::new(Default::default);

/// Queues `c` for `parent` (its sender starts when nothing waited for it): true when it starts one,
/// which [`send_queued`] then is. Taken under `TOLD`'s lock alone (never the other way around).
fn queue_for(parent: &str, c: Command) -> bool {
    let mut g = TO_SEND.lock();
    let q = g.entry(parent.to_owned()).or_default();
    q.push_back(c);
    q.len() == 1
}

/// `parent`'s sender: what's queued for it, in order, each once the one before it went (or is said
/// unsent, its words to go at the next look). It ends once nothing's left.
async fn send_queued(hub: Arc<crate::hub::Hub>, parent: String) {
    loop {
        // The front stays queued while it goes: what's queued meanwhile waits behind it.
        let Some(c) = TO_SEND.lock().get(&parent).and_then(|q| q.front().cloned()) else { return };
        // It went: queued, or taken by its session's inbox (CP-T3.2).
        if !hub.harness.command(&hub, &parent, c.clone()).await.went() {
            tracing::info!(agent = %parent, "news of the agents it started didn't reach it");
            TOLD.lock().unsent(&parent, &c);
        }
        let mut g = TO_SEND.lock();
        let done = g.get_mut(&parent).is_none_or(|q| {
            q.pop_front();
            q.is_empty()
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
    // Decided and queued as one: an edit decided later can't go before it.
    let start = {
        let mut told = TOLD.lock();
        told.saw(child).and_then(|(parent, c)| queue_for(&parent, c).then_some(parent))
    };
    start_senders(hub, start.into_iter().collect());
}

/// News of one of `session`'s runs, from runner `runner` (`crate::reach::Question::Told`), heard
/// before any check of where the session is now or who holds it (T5: a detached run's runner may
/// no longer host its session, and news is no act). It counts only from the runner that follows
/// the run: the first news of a run from one where the session is now, the rest from that one
/// (E8: no runner tells an agent of runs it doesn't follow). Kept (and written, E1), and told in
/// the agent's news at the next look ([`tick`]); a run its agent saw end itself comes out of the
/// news waiting, as a child it saw does ([`saw`]).
pub async fn ran(hub: &Arc<crate::hub::Hub>, runner: &str, session: &str, news: RunNews) -> Result<(), String> {
    load(hub).await;
    let Some(info) = hub.harness.info(session) else { return Err(format!("kiln knows no agent session {session:?}")) };
    if info.status == Status::Exited {
        return Err(format!("agent session {session} has ended"));
    }
    let located = hub.harness.located_on(session, runner);
    let start = {
        let mut told = TOLD.lock();
        let c = told.ran(runner, located, session, news.clipped(), crate::util::now_ms())?;
        c.and_then(|c| queue_for(session, c).then(|| session.to_owned()))
    };
    start_senders(hub, start.into_iter().collect());
    flush(hub).await;
    Ok(())
}

/// [`ran`], answered as a runner's question is (`crate::mcp::runner_ask`).
pub async fn heard(hub: &Arc<crate::hub::Hub>, runner: &str, session: &str, news: &RunNews) -> serde_json::Value {
    match ran(hub, runner, session, news.clone()).await {
        Ok(()) => serde_json::json!({"ok": {}}),
        Err(e) => serde_json::json!({"error": e, "status": 403}),
    }
}

/// What placement says of a run an agent asked for, as it changes (docs/needs.md E5): queued (its
/// place and when it's expected to start), started, or never. It folds into the agent's one
/// message of news: queued and started are state, edited into a message that waits (no new turn,
/// T8); never wakes the agent, as an end does. Later news of the run (its end, from what follows
/// it) takes its line's place.
pub async fn answer(hub: &Arc<crate::hub::Hub>, session: &str, news: AnswerNews) {
    load(hub).await;
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
    start_senders(hub, start.into_iter().collect());
    flush(hub).await;
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
async fn load(hub: &Arc<crate::hub::Hub>) {
    if TOLD.lock().loaded {
        return;
    }
    let rows = read_rows(&hub.store).await.unwrap_or_else(|e| {
        tracing::warn!(error = %format!("{e:#}"), "the runs agents are told of couldn't be read");
        Vec::new()
    });
    TOLD.lock().adopt(rows);
}

/// Every run changed or gone since it was last written, written as it stands now (E1).
async fn flush(hub: &Arc<crate::hub::Hub>) {
    let rows = TOLD.lock().dirty_rows();
    if let Err(e) = write_rows(&hub.store, rows).await {
        tracing::warn!(error = %format!("{e:#}"), "the runs agents are told of couldn't be kept");
    }
}

/// A run as `told_runs` keeps it, by agent and key (none: gone), and the version it's written at:
/// taken as it's read from [`TOLD`], so a later state always has a later version.
type Row = (String, String, Option<Ran>, i64);

/// The version the next row is written at: past every one kept ([`read_rows`] raises it).
static VERSION: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);

/// `rows` written to `told_runs`: an older state never overwrites (nor deletes) a newer one,
/// whichever write reaches the store first.
async fn write_rows(store: &crate::store::Store, rows: Vec<Row>) -> anyhow::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    store
        .call(move |c| {
            let tx = c.transaction()?;
            for (session, key, ran, version) in rows {
                match ran {
                    Some(r) => {
                        let news = serde_json::to_string(&r.news)?;
                        tx.execute(
                            "INSERT INTO told_runs (session, key, runner, news, handed_at, heard_at, told, version) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                             ON CONFLICT (session, key) DO UPDATE SET runner = excluded.runner, news = excluded.news, handed_at = excluded.handed_at, heard_at = excluded.heard_at, told = excluded.told, version = excluded.version
                             WHERE excluded.version > told_runs.version",
                            rusqlite::params![session, key, r.runner, news, r.handed_at, i64::try_from(r.heard_at).unwrap_or(i64::MAX), r.told, version],
                        )?;
                    }
                    None => {
                        tx.execute("DELETE FROM told_runs WHERE session = ?1 AND key = ?2 AND version < ?3", rusqlite::params![session, key, version])?;
                    }
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
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
                let Ok(news) = serde_json::from_str::<RunNews>(&news) else { continue };
                out.push((session, Ran { news, handed_at, runner, heard_at: u64::try_from(heard_at).unwrap_or(0), told, held: true }));
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
    load(hub).await;
    // News a server before this one queued, waiting still: this one adds to it.
    for p in all.iter().filter(|p| !matches!(p.status, Status::Exited | Status::Disconnected) && p.queue.iter().any(|q| super::queued::from_before(&q.id, NEWS))) {
        if TOLD.lock().news.contains_key(&p.id) {
            continue;
        }
        if let Some(q) = hub.harness.queue(&p.id).into_iter().find(|q| super::queued::from_before(&q.id, NEWS)) {
            TOLD.lock().found(&p.id, q.id, q.text, now);
        }
    }
    // The runners up now: a run whose runner is away has no one to say how it ends until it's back
    // (and says it again), and holds its agent back no more meanwhile.
    let up: HashSet<String> = hub.agents.lock().iter().filter(|(_, a)| a.connected).map(|(n, _)| n.clone()).collect();
    // Decided and queued as one (under `TOLD`'s lock): what `saw` decides after goes after it. Each
    // parent's goes by its sender, in order; none of it waited for here.
    let start: Vec<String> = {
        let mut told = TOLD.lock();
        let plan = told.plan_with(all, now, driven, quiet, &|r| up.contains(r));
        let mut start = Vec::new();
        for (parent, c) in plan {
            if matches!(c, Command::Send { .. })
                && let Some(p) = all.iter().find(|i| i.id == parent)
            {
                super::wait::sent(&p.id, p.last_turn.as_ref().map(|t| t.at));
            }
            if queue_for(&parent, c) {
                start.push(parent);
            }
        }
        start
    };
    start_senders(hub, start);
    flush(hub).await;
}

impl Told {
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
    fn dirty_rows(&mut self) -> Vec<Row> {
        let dirty = std::mem::take(&mut self.dirty);
        dirty
            .into_iter()
            .map(|(session, key)| {
                let ran = self.runs.get(&session).and_then(|rs| rs.iter().find(|r| r.news.key == key)).cloned();
                (session, key, ran, VERSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
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
        let (empty, text) = (n.lines.is_empty() && n.before.is_none(), n.text());
        let c = match n.msg.as_mut() {
            Some(m) if empty => Some(Command::QueueRemove { id: m.id.clone() }),
            Some(m) => {
                m.text = text;
                n.said.clone_from(&n.lines);
                gave(&mut n.given, &m.text, &n.lines);
                Some(Command::QueueEdit { id: m.id.clone(), text: m.text.clone(), attachments: None })
            }
            None => None,
        };
        if empty {
            self.news.remove(parent);
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
        let Told { runs, heard, dirty, .. } = self;
        let live: HashSet<&str> = all.iter().filter(|i| i.status != Status::Exited).map(|i| i.id.as_str()).collect();
        runs.retain(|s, rs| {
            let keep = live.contains(s.as_str());
            if !keep {
                dirty.extend(rs.iter().map(|r| (s.clone(), r.news.key.clone())));
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
                if !matches!(r.news.event, RunEvent::Going | RunEvent::Seen) {
                    by_agent.entry(agent.clone()).or_default().push((format!("{RUN}{}", r.news.key), run_line(&r.news, now)));
                }
            }
            rs.retain(|r| {
                let keep = !(r.finished() && heard.get(&r.runner).is_some_and(|h| *h > r.heard_at));
                if !keep {
                    dirty.insert((agent.clone(), r.news.key.clone()));
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
            if driven.contains(&c.id) {
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
        self.news.retain(|p, _| all.iter().any(|i| i.id == *p));
        let parents: BTreeSet<String> = self.news.keys().chain(due.keys()).cloned().collect();
        let mut out = Vec::new();
        for parent in parents {
            let Some(info) = all.iter().find(|i| i.id == parent) else { continue };
            let n = self.news.entry(parent.clone()).or_default();
            let mut unheard = false;
            if let Some(m) = n.msg.as_mut() {
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
            for (child, l) in due.remove(&parent).unwrap_or_default() {
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
            if (n.lines.is_empty() && n.before.is_none()) || info.status == Status::Exited {
                self.news.remove(&parent);
                continue;
            }
            if info.status == Status::Disconnected || unheard {
                continue;
            }
            let text = n.text();
            match n.msg.as_mut() {
                Some(m) if m.text == text => {}
                Some(m) => {
                    out.push((parent, Command::QueueEdit { id: m.id.clone(), text: text.clone(), attachments: None }));
                    gave(&mut n.given, &text, &n.lines);
                    m.text = text;
                    n.said.clone_from(&n.lines);
                }
                None if quiet.contains(&parent) => {}
                None if n.lines.iter().all(|(k, _)| calm.contains(k)) => {}
                None => {
                    let id = super::queued::made(NEWS);
                    out.push((parent, Command::Send { id: id.clone(), text: text.clone(), mode: SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: None }));
                    n.given = vec![(super::queued::stable(&text), n.lines.clone())];
                    n.msg = Some(Kept::sent(id, text, info, now));
                    n.said.clone_from(&n.lines);
                    n.held.clone_from(&n.lines);
                }
            }
        }
        out
    }

    /// Command `c` from `plan` didn't reach `parent`: what it said goes at the next look (an edit
    /// that didn't go says nothing the message holds, so the lines it carried aren't taken as held).
    fn unsent(&mut self, parent: &str, c: &Command) {
        let Some(n) = self.news.get_mut(parent) else { return };
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

    /// The incident: a coordinator in one turn for hours, its children settling again and again.
    /// What waits for it is one message, each child's latest line in it once; when its turn ends
    /// and it takes that, what settles next is news again, in a message of its own.
    #[test]
    fn a_parent_in_a_long_turn_has_one_message_of_news_waiting_each_child_s_latest_line_once() -> anyhow::Result<()> {
        let mut t = Told::default();
        let mut p = coordinator("q1");
        let mut kids = helpers("q1", &["k1", "k2", "k3"]);
        let mut now = 2_000_000;
        assert!(t.plan(&all(&p, &kids), now, &[], &[]).is_empty(), "seen at work first");
        // Two settle: one message, behind the parent's turn.
        now += 10_000;
        let first = settle(&mut t, &p, &mut kids, &[(0, "The lexer is done."), (1, "Docs updated.")], now);
        let Some(Command::Send { id, text, mode: SendMode::Queue, .. }) = first.first() else { anyhow::bail!("no news: {first:?}") };
        anyhow::ensure!(first.len() == 1 && text.contains("Agents you started") && text.contains("The lexer is done.") && text.contains("Docs updated."), "{first:?}");
        let id = id.clone();
        first.iter().for_each(|c| apply(&mut p, c));
        // While it waits, one settles again and another for the first time; then one again and
        // again: each time the one message is edited, never another queued.
        let rounds: [&[(usize, &str)]; 4] = [&[(0, "The parser is done."), (2, "The bench is recorded.")], &[(1, "Docs: round 1.")], &[(1, "Docs: round 2.")], &[(1, "Docs: round 3.")]];
        for which in rounds {
            now += 10_000;
            let sent = settle(&mut t, &p, &mut kids, which, now);
            anyhow::ensure!(!sent.is_empty() && sent.iter().all(|c| matches!(c, Command::QueueEdit { id: e, .. } if *e == id)), "the waiting one edited, nothing more queued: {sent:?}");
            sent.iter().for_each(|c| apply(&mut p, c));
        }
        // A look later: nothing more to say, and its queue is seen holding the last edit.
        anyhow::ensure!(t.plan(&all(&p, &kids), now + 5_000, &[], &[]).is_empty());
        let latest = news_of(&t, "q1");
        anyhow::ensure!(p.queue == [Queued::of(&id, &latest)], "its queue holds the one message, as kiln last had it: {:?}", p.queue);
        for old in ["The lexer is done.", "Docs updated.", "Docs: round 1.", "Docs: round 2."] {
            anyhow::ensure!(!latest.contains(old), "{old:?} isn't news any more: {latest}");
        }
        for now_news in ["The parser is done.", "The bench is recorded.", "Docs: round 3."] {
            anyhow::ensure!(latest.contains(now_news), "{now_news:?} missing: {latest}");
        }
        anyhow::ensure!(latest.matches("which you started").count() == 3, "each child once: {latest}");
        // Its turn ends and it takes the news; what settles next is news again, on its own.
        take(&mut p, &[(&id, &latest)]);
        now += 10_000;
        let next = settle(&mut t, &p, &mut kids, &[(2, "The bench is green.")], now);
        let Some(Command::Send { id: again, text, .. }) = next.first() else { anyhow::bail!("no news after the parent took the last: {next:?}") };
        anyhow::ensure!(next.len() == 1 && *again != id && text.contains("The bench is green.") && !text.contains("Docs: round 3.") && !text.contains("Agents you started"), "{next:?}");
        Ok(())
    }

    /// An edit that reaches the parent's machine just after its turn ended, the news gone with
    /// what it said before, does nothing there: what the edit said goes again, in the next message.
    #[test]
    fn what_an_edit_too_late_said_goes_again_in_the_next_message() -> anyhow::Result<()> {
        let mut t = Told::default();
        let mut p = coordinator("q2");
        let mut kids = helpers("q2", &["j1", "j2"]);
        let mut now = 3_000_000;
        let _ = t.plan(&all(&p, &kids), now, &[], &[]);
        now += 10_000;
        let first = settle(&mut t, &p, &mut kids, &[(0, "First news.")], now);
        first.iter().for_each(|c| apply(&mut p, c));
        let [Command::Send { id, text, .. }] = first.as_slice() else { anyhow::bail!("{first:?}") };
        now += 10_000;
        let edit = settle(&mut t, &p, &mut kids, &[(1, "Second news.")], now);
        anyhow::ensure!(matches!(edit.as_slice(), [Command::QueueEdit { .. }]), "{edit:?}");
        // Before the edit came, the turn ended and the message went as it was.
        take(&mut p, &[(id, text)]);
        let resent = t.plan(&all(&p, &kids), now + 10_000, &[], &[]);
        let Some((to, Command::Send { text, .. })) = resent.first() else { anyhow::bail!("the edit's words were lost: {resent:?}") };
        anyhow::ensure!(resent.len() == 1 && to == "q2" && text.contains("Second news.") && !text.contains("First news."), "only what didn't reach it: {resent:?}");
        Ok(())
    }

    /// CP-T3.2 (the sixteenth adversarial check's): a runner from before this release says what its
    /// harness was handed only in its events, and coming back it says its state first. News edited
    /// and taken between two looks, its handing not heard of yet, waits for it (`queued::HEARD_MS`)
    /// rather than tell again the lines the edit gave it; heard of, they aren't news any more. Not
    /// heard of in that time, it's taken for lost, and what it may not have said goes again.
    #[test]
    fn news_edited_and_taken_between_two_looks_waits_to_hear_it_was_handed() -> anyhow::Result<()> {
        let mut t = Told::default();
        let mut p = coordinator("q9");
        let mut kids = helpers("q9", &["t1", "t2"]);
        let mut now = 9_000_000;
        let _ = t.plan(&all(&p, &kids), now, &[], &[]);
        now += 10_000;
        let first = settle(&mut t, &p, &mut kids, &[(0, "First news.")], now);
        first.iter().for_each(|c| apply(&mut p, c));
        anyhow::ensure!(t.plan(&all(&p, &kids), now + 5_000, &[], &[]).is_empty(), "seen waiting");
        now += 10_000;
        let edit = settle(&mut t, &p, &mut kids, &[(1, "Second news.")], now);
        let [Command::QueueEdit { id, text, .. }] = edit.as_slice() else { anyhow::bail!("{edit:?}") };
        let (id, text) = (id.clone(), text.clone());
        // The edit applies and the parent takes the news before the next look; its runner comes
        // back saying its queue (empty) before its events.
        edit.iter().for_each(|c| apply(&mut p, c));
        p.queue.clear();
        p.queued = 0;
        for wait in [10_000, 12_000] {
            let sent = t.plan(&all(&p, &kids), now + wait, &[], &[]);
            anyhow::ensure!(sent.is_empty(), "nothing goes while its handing isn't heard of: {sent:?}");
        }
        // Its events say it was handed the news as edited: nothing to tell again.
        p.said.push(crate::harness::model::Given { id: id.clone(), digest: crate::harness::queued::stable(&text) });
        let sent = t.plan(&all(&p, &kids), now + 13_000, &[], &[]);
        anyhow::ensure!(sent.is_empty(), "the edit's lines reached it: {sent:?}");
        // What settles next is news again, on its own; edited and taken the same way, and its
        // handing never heard of, what the edit said goes again once the wait is over.
        now += 20_000;
        let next = settle(&mut t, &p, &mut kids, &[(0, "Third news.")], now);
        anyhow::ensure!(matches!(next.as_slice(), [Command::Send { text, .. }] if text.contains("Third news.") && !text.contains("Second news.")), "{next:?}");
        next.iter().for_each(|c| apply(&mut p, c));
        anyhow::ensure!(t.plan(&all(&p, &kids), now + 5_000, &[], &[]).is_empty(), "seen waiting");
        now += 10_000;
        let edit = settle(&mut t, &p, &mut kids, &[(1, "Fourth news.")], now);
        anyhow::ensure!(matches!(edit.as_slice(), [Command::QueueEdit { .. }]), "{edit:?}");
        edit.iter().for_each(|c| apply(&mut p, c));
        p.queue.clear();
        p.queued = 0;
        anyhow::ensure!(t.plan(&all(&p, &kids), now + 10_000, &[], &[]).is_empty(), "waiting to hear of it");
        let lost = t.plan(&all(&p, &kids), now + 10_000 + crate::harness::queued::HEARD_MS, &[], &[]);
        anyhow::ensure!(matches!(lost.as_slice(), [(_, Command::Send { text, .. })] if text.contains("Fourth news.") && !text.contains("Third news.")), "{lost:?}");
        Ok(())
    }

    /// CP-T3.2, AC-T3.2.1 (the seventeenth adversarial check's): news edited and taken between two looks,
    /// this release's runner saying what its harness was handed beside its queue, and the parent,
    /// reading it, asks for one child's result (agents_result, as the news tells it to) before the
    /// server's next look. That child's line comes out of the news kiln keeps, so the words it
    /// keeps no longer match the words handed. The parent was handed every line: none is told again.
    #[test]
    fn news_taken_then_a_child_read_before_the_next_look_is_told_once() -> anyhow::Result<()> {
        let mut t = Told::default();
        let mut p = coordinator("q10");
        let mut kids = helpers("q10", &["s1", "s2"]);
        let mut now = 10_000_000;
        let _ = t.plan(&all(&p, &kids), now, &[], &[]);
        now += 10_000;
        let first = settle(&mut t, &p, &mut kids, &[(0, "First news.")], now);
        first.iter().for_each(|c| apply(&mut p, c));
        anyhow::ensure!(t.plan(&all(&p, &kids), now + 5_000, &[], &[]).is_empty(), "seen waiting");
        now += 10_000;
        let edit = settle(&mut t, &p, &mut kids, &[(1, "Second news.")], now);
        let [Command::QueueEdit { id, text, .. }] = edit.as_slice() else { anyhow::bail!("{edit:?}") };
        let (id, text) = (id.clone(), text.clone());
        anyhow::ensure!(text.contains("First news.") && text.contains("Second news."), "{text}");
        // The edit applies and the parent takes the news, both lines, before the next look; its
        // runner says so beside its queue.
        edit.iter().for_each(|c| apply(&mut p, c));
        take(&mut p, &[(&id, &text)]);
        // Reading it, the parent asks for the first child's result before the server looks again.
        let s1 = kids.first().cloned().ok_or_else(|| anyhow::anyhow!("no child"))?;
        anyhow::ensure!(t.saw(&s1).is_some(), "its line was in the news");
        let sent = t.plan(&all(&p, &kids), now + 10_000, &[], &[]);
        anyhow::ensure!(sent.is_empty(), "news the parent was handed is told again: {sent:?}");
        Ok(())
    }

    /// The same, its runner from before this release (its events say what was handed, after its
    /// state): the parent reads the first child's result while the server waits to hear of the
    /// handing (`queued::HEARD_MS`); then heard of, it was handed every line: none is told again.
    #[test]
    fn news_taken_then_a_child_read_while_its_handing_is_awaited_is_told_once() -> anyhow::Result<()> {
        let mut t = Told::default();
        let mut p = coordinator("q11");
        let mut kids = helpers("q11", &["r1", "r2"]);
        let mut now = 11_000_000;
        let _ = t.plan(&all(&p, &kids), now, &[], &[]);
        now += 10_000;
        let first = settle(&mut t, &p, &mut kids, &[(0, "First news.")], now);
        first.iter().for_each(|c| apply(&mut p, c));
        anyhow::ensure!(t.plan(&all(&p, &kids), now + 5_000, &[], &[]).is_empty(), "seen waiting");
        now += 10_000;
        let edit = settle(&mut t, &p, &mut kids, &[(1, "Second news.")], now);
        let [Command::QueueEdit { id, text, .. }] = edit.as_slice() else { anyhow::bail!("{edit:?}") };
        let (id, text) = (id.clone(), text.clone());
        edit.iter().for_each(|c| apply(&mut p, c));
        p.queue.clear();
        p.queued = 0;
        anyhow::ensure!(t.plan(&all(&p, &kids), now + 10_000, &[], &[]).is_empty(), "waiting to hear of its handing");
        let r1 = kids.first().cloned().ok_or_else(|| anyhow::anyhow!("no child"))?;
        anyhow::ensure!(t.saw(&r1).is_some(), "its line was in the news");
        // Its events: it was handed the news as edited, both lines.
        p.said.push(crate::harness::model::Given { id: id.clone(), digest: crate::harness::queued::stable(&text) });
        let sent = t.plan(&all(&p, &kids), now + 12_000, &[], &[]);
        anyhow::ensure!(sent.is_empty(), "news the parent was handed is told again: {sent:?}");
        Ok(())
    }

    /// News that didn't reach the parent (its machine away a moment) goes at the next look: a
    /// message again, an edit again; and should the message go meanwhile, what the edit said goes
    /// in the next.
    #[test]
    fn news_that_didn_t_reach_its_parent_goes_at_the_next_look() -> anyhow::Result<()> {
        let mut t = Told::default();
        let mut p = coordinator("q6");
        let mut kids = helpers("q6", &["w1", "w2"]);
        let mut now = 6_000_000;
        let _ = t.plan(&all(&p, &kids), now, &[], &[]);
        now += 10_000;
        let first = settle(&mut t, &p, &mut kids, &[(0, "First news.")], now);
        let Some(send) = first.first() else { anyhow::bail!("no news") };
        t.unsent("q6", send);
        let again = t.plan(&all(&p, &kids), now + 10_000, &[], &[]);
        let [(_, Command::Send { id: sent, text: said, .. })] = again.as_slice() else { anyhow::bail!("{again:?}") };
        anyhow::ensure!(said.contains("First news."), "{again:?}");
        let (sent, said) = (sent.clone(), said.clone());
        again.iter().for_each(|(_, c)| apply(&mut p, c));
        now += 20_000;
        let edit = settle(&mut t, &p, &mut kids, &[(1, "Second news.")], now);
        let Some(c) = edit.first() else { anyhow::bail!("no edit") };
        t.unsent("q6", c);
        let again = t.plan(&all(&p, &kids), now + 10_000, &[], &[]);
        anyhow::ensure!(again.len() == 1 && again.first().map(|(_, c)| c) == Some(c), "the same edit again: {again:?}");
        // It didn't go either, and the parent took the message as it was.
        t.unsent("q6", c);
        take(&mut p, &[(&sent, &said)]);
        let next = t.plan(&all(&p, &kids), now + 20_000, &[], &[]);
        anyhow::ensure!(matches!(next.as_slice(), [(_, Command::Send { text, .. })] if text.contains("Second news.") && !text.contains("First news.")), "{next:?}");
        Ok(())
    }

    /// While the parent's machine is away, kiln's view of its queue stands still: news that comes
    /// meanwhile waits, and isn't taken for what the message waiting said. Its turn ended during the
    /// outage and it took that message as it was; once back, it's told what came meanwhile.
    #[test]
    fn news_that_came_while_its_parent_s_machine_was_away_goes_once_it_s_back() -> anyhow::Result<()> {
        let mut t = Told::default();
        let mut p = coordinator("q7");
        let mut kids = helpers("q7", &["v1", "v2"]);
        let mut now = 7_000_000;
        let _ = t.plan(&all(&p, &kids), now, &[], &[]);
        now += 10_000;
        let first = settle(&mut t, &p, &mut kids, &[(0, "v1 is done.")], now);
        first.iter().for_each(|c| apply(&mut p, c));
        let [Command::Send { id, text, .. }] = first.as_slice() else { anyhow::bail!("{first:?}") };
        anyhow::ensure!(t.plan(&all(&p, &kids), now + 5_000, &[], &[]).is_empty(), "seen waiting");
        p.status = Status::Disconnected;
        now += 10_000;
        anyhow::ensure!(settle(&mut t, &p, &mut kids, &[(1, "v2 is done.")], now).is_empty(), "nothing goes while it's away");
        anyhow::ensure!(t.plan(&all(&p, &kids), now + 10_000, &[], &[]).is_empty());
        p.status = Status::Running;
        take(&mut p, &[(id, text)]);
        let back = t.plan(&all(&p, &kids), now + 20_000, &[], &[]);
        anyhow::ensure!(matches!(back.as_slice(), [(_, Command::Send { text, .. })] if text.contains("v2 is done.") && !text.contains("v1 is done.")), "{back:?}");
        Ok(())
    }

    /// A server that started since finds the news a server before it left waiting for a parent (its
    /// id says what it is): it adds to that message, what it said first, rather than queue another.
    #[test]
    fn news_a_server_before_left_waiting_is_added_to_not_queued_again() -> anyhow::Result<()> {
        let mut t = Told::default();
        let old = "news-01a16251c49a75fba41b1fae971ba8a9";
        let before = "Agent “Helper 0” (h0), which you started, ended its turn: from before the restart.";
        let mut p = Info { queue: vec![Queued::of(old, before)], queued: 1, ..coordinator("q8") };
        let mut kids = helpers("q8", &["u1"]);
        let mut now = 8_000_000;
        t.found("q8", old.into(), before.into(), now);
        anyhow::ensure!(t.plan(&all(&p, &kids), now, &[], &[]).is_empty(), "nothing new to say");
        now += 10_000;
        let sent = settle(&mut t, &p, &mut kids, &[(0, "u1 is done.")], now);
        let [Command::QueueEdit { id, text, .. }] = sent.as_slice() else { anyhow::bail!("{sent:?}") };
        anyhow::ensure!(id == old && text.starts_with(before) && text.contains("u1 is done."), "{text}");
        sent.iter().for_each(|c| apply(&mut p, c));
        // Once the parent takes it, what came before goes with it.
        anyhow::ensure!(t.plan(&all(&p, &kids), now + 10_000, &[], &[]).is_empty());
        take(&mut p, &[(id, text)]);
        now += 20_000;
        let next = settle(&mut t, &p, &mut kids, &[(0, "u1 again.")], now);
        anyhow::ensure!(matches!(next.as_slice(), [Command::Send { id, text, .. }] if id.starts_with(NEWS) && !text.contains("before the restart")), "{next:?}");
        Ok(())
    }

    /// What the parent sees itself (agents_wait, agents_result) comes out of its news still waiting;
    /// the last line out takes the message out of its queue.
    #[test]
    fn what_a_parent_sees_itself_comes_out_of_its_news_and_the_message_with_the_last_line() -> anyhow::Result<()> {
        let mut t = Told::default();
        let mut p = coordinator("q3");
        let mut kids = helpers("q3", &["x1", "x2", "x3"]);
        let mut now = 4_000_000;
        let _ = t.plan(&all(&p, &kids), now, &[], &[]);
        now += 10_000;
        let first = settle(&mut t, &p, &mut kids, &[(0, "x1 is done."), (1, "x2 is done."), (2, "x3 is done.")], now);
        first.iter().for_each(|c| apply(&mut p, c));
        let Some(Command::Send { id, .. }) = first.first() else { anyhow::bail!("{first:?}") };
        let id = id.clone();
        let (x1, x2, x3) = match kids.as_slice() {
            [a, b, c] => (a.clone(), b.clone(), c.clone()),
            _ => anyhow::bail!("three helpers"),
        };
        // It waited for x1: x1's line comes out, the rest stay, in the same message.
        let Some((to, c)) = t.saw(&x1) else { anyhow::bail!("x1's line stayed in the news") };
        let Command::QueueEdit { id: edited, text, .. } = &c else { anyhow::bail!("{c:?}") };
        anyhow::ensure!(to == "q3" && *edited == id && !text.contains("x1 is done.") && text.contains("x2 is done.") && text.contains("x3 is done."), "{text}");
        apply(&mut p, &c);
        // It read x2's result: one line left, said on its own.
        let Some((_, c)) = t.saw(&x2) else { anyhow::bail!("x2's line stayed in the news") };
        anyhow::ensure!(matches!(&c, Command::QueueEdit { text, .. } if text.contains("x3 is done.") && !text.contains("x2 is done.") && !text.contains("Agents you started")), "{c:?}");
        apply(&mut p, &c);
        // And x3's: nothing left, so the message goes.
        let Some((_, c)) = t.saw(&x3) else { anyhow::bail!("x3's line stayed in the news") };
        anyhow::ensure!(c == Command::QueueRemove { id: id.clone() }, "{c:?}");
        apply(&mut p, &c);
        anyhow::ensure!(p.queue.is_empty(), "{:?}", p.queue);
        anyhow::ensure!(t.saw(&x1).is_none(), "nothing of it waits");
        // Nothing comes back for what it saw.
        anyhow::ensure!(t.plan(&all(&p, &kids), now + 20_000, &[], &[]).is_empty());
        anyhow::ensure!(t.plan(&all(&p, &kids), now + 20_000 + SETTLE_MS, &[], &[]).is_empty());
        // x1 settles again: news again.
        now += 30_000;
        let again = settle(&mut t, &p, &mut kids, &[(0, "x1 is done again.")], now);
        anyhow::ensure!(matches!(again.as_slice(), [Command::Send { text, .. }] if text.contains("x1 is done again.")), "{again:?}");
        Ok(())
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

    /// T8: a run's start is state, not a wake: alone it sends nothing (it cost the agent a turn);
    /// a message that goes anyway (a child settled) says it; and its end, which wakes, takes its
    /// line's place in that message while it waits.
    #[test]
    fn t08_a_run_s_start_is_state_its_end_wakes() -> anyhow::Result<()> {
        let mut t = Told::default();
        let mut p = coordinator("a2");
        let mut kids = helpers("a2", &["c2"]);
        let now = 2_000_000;
        anyhow::ensure!(t.plan(&all(&p, &kids), now, &[], &[]).is_empty(), "seen at work first");
        let _ = heard(&mut t, "a2", run_news("r2", RunEvent::Going), now);
        let _ = heard(&mut t, "a2", run_news("r2", RunEvent::Started), now + 500);
        anyhow::ensure!(going(&t, "a2"), "started is still going");
        anyhow::ensure!(t.plan(&all(&p, &kids), now + 600, &[], &[]).is_empty(), "a start alone wakes no one");
        let sent = settle(&mut t, &p, &mut kids, &[(0, "The parser is done.")], now + 700);
        let [c @ Command::Send { id, text, .. }] = sent.as_slice() else { anyhow::bail!("one message: {sent:?}") };
        anyhow::ensure!(text.contains("The parser is done.") && text.contains("Run r2 (`cargo test -p kiln`), which you started in the background, started on kiln-a094a12d: kiln tells you when it ends."), "{text}");
        let id = id.clone();
        apply(&mut p, c);
        let _ = heard(&mut t, "a2", run_ended("r2"), now + 60_000);
        let edit = t.plan(&all(&p, &kids), now + 60_000, &[], &[]);
        anyhow::ensure!(matches!(edit.as_slice(), [(_, Command::QueueEdit { id: e, text, .. })] if *e == id && text.contains("ended on kiln-a094a12d") && !text.contains("started on")), "the waiting message edited, its line now the end: {edit:?}");
        Ok(())
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
            let first = before.dirty_rows();
            let _ = heard(&mut before, "a10", RunNews { handed: 2, ..run_news("g1", RunEvent::Going) }, 1_200);
            let _ = heard(&mut before, "a10", run_ended("u1"), 1_300);
            // The later state reaches the store first: the earlier one, after, changes nothing.
            write_rows(&store, before.dirty_rows()).await?;
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

    /// What the agent saw of a run itself (`output` or `stop` showing its end) comes out of its news
    /// waiting, as a child's line it read does: the message goes with its last line; and a run seen
    /// before its end was told of isn't told at all.
    #[test]
    fn what_an_agent_saw_of_its_run_comes_out_of_its_news() -> anyhow::Result<()> {
        let mut t = Told::default();
        let mut p = coordinator("a3");
        let now = 3_000_000;
        let _ = heard(&mut t, "a3", run_ended("r3"), now);
        let sent = t.plan(std::slice::from_ref(&p), now, &[], &[]);
        let [(_, c @ Command::Send { id, .. })] = sent.as_slice() else { anyhow::bail!("{sent:?}") };
        let id = id.clone();
        apply(&mut p, c);
        let seen = heard(&mut t, "a3", run_news("r3", RunEvent::Seen), now + 100);
        anyhow::ensure!(seen == Some(Command::QueueRemove { id }), "its one line out, the message with it: {seen:?}");
        // Seen ended before its news was looked at: nothing told.
        let _ = heard(&mut t, "a3", run_ended("r4"), now + 20_000);
        anyhow::ensure!(heard(&mut t, "a3", run_news("r4", RunEvent::Seen), now + 20_001).is_none());
        anyhow::ensure!(t.plan(std::slice::from_ref(&p), now + 20_002, &[], &[]).is_empty(), "the agent saw it");
        Ok(())
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

    /// A run and a child that come due together are told in one message, each under its heading;
    /// an agent that ended is told nothing of its runs, and they're forgotten.
    #[test]
    fn a_run_s_news_and_a_child_s_are_one_message() -> anyhow::Result<()> {
        let mut t = Told::default();
        let p = coordinator("a11");
        let mut kids = helpers("a11", &["c11"]);
        let now = 5_000_000;
        anyhow::ensure!(t.plan(&all(&p, &kids), now, &[], &[]).is_empty(), "seen at work first");
        let _ = heard(&mut t, "a11", run_ended("r6"), now);
        let _ = settle(&mut t, &p, &mut kids, &[(0, "The parser is done.")], now);
        let text = news_of(&t, "a11");
        anyhow::ensure!(text.starts_with("Agents you started:\n- Agent") && text.contains("The parser is done.") && text.contains("\n\nRuns you started:\n- Run r6 (`cargo test -p kiln`)"), "{text}");
        anyhow::ensure!(text.contains("\n  Its output ended with:\n  ```\n  running 412 tests"), "the end of its output stays in its item: {text}");
        let _ = heard(&mut t, "a11", run_ended("r7"), now + 200);
        let ended = Info { status: Status::Exited, ..p };
        anyhow::ensure!(t.plan(&[ended], now + 201, &[], &[]).is_empty() && !t.runs.contains_key("a11"));
        Ok(())
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
