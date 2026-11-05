//! Telling an agent of its runs (docs/reach.md §7; agents are told, not polled), on the runner's
//! side. A run an agent left in the background (`run` with `background`, or past its wait;
//! `kiln.machines.start`) is followed by its own MCP server (`super::mcp`, [`Telling`]), which
//! says what became of it through one ordered [`Teller`] (T6, T10); a detached one whose client
//! went (`kiln run --detach`) by this runner ([`Watch`], [`detached`]). Either way the news
//! ([`RunNews`]) lands in this runner's outbox ([`say`]), which one forwarder sends to the server
//! in order, each once the one before it was answered, retrying on the link's measured RTO (RFC
//! 6298, [`Rto`]; T1, T6). The server tells the agent in its news (`crate::harness::told`).
//!
//! The outbox is the runner's record of the runs it follows: it's kept in a file beside the reach
//! node's identity (`<state>/reach/told.json`) and read again when the runner starts, so a detached
//! run it followed is followed on after a restart or an upgrade ([`Handle::refollow`]; T3, E2), and
//! news the server hadn't had goes to it then. Whenever the server says it takes news anew (a
//! reconnect, a server restart), the latest news of every run still followed goes again, so the
//! holds it lost are back (T4, E1).
//!
//! The session news is for is the one the sending process belongs to by its process alone
//! (`crate::runner::custody::session_by_pid`), never by the environment's mark any process can set
//! (T12, E8).

use std::collections::{HashMap, HashSet};
#[cfg(unix)]
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
#[cfg(unix)]
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub use crate::harness::told::{DETACHED, LINE_SHARE, RunEvent, RunNews, Rto, clip_tail};

#[cfg(unix)]
use super::local::{Client, Req};
use super::local::{Ev, RunReq};
use super::node::Handle;
use super::wire::Say;

/// The named feature a server says when it takes news of its agents' runs (docs/wire.md, "Named
/// features"; it was capability bit 58, which four branches claimed).
// TODO(lb/core Features): add to `proto::feature::ALL`; the runner's `ServerMsg::Features` handler calls `server_features(&names)`.
pub const FEATURE: &str = "told";

/// Why a run's end isn't known: what followed it went.
const STOPPED: &str = "stopped: the kiln MCP server that followed it ended (its agent's harness started again, or ended)";
const LOST: &str = "this runner lost track of it";
const LOST_HERE: &str = "it ran on this machine under the runner process before this one, which can't be followed on: how it ended isn't known";

/// One run this runner follows, as the outbox holds it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Entry {
    session: String,
    /// Its latest news.
    news: RunNews,
    /// The server has its latest news.
    #[serde(default)]
    sent: bool,
    /// Its place in what goes: what changed later goes later.
    seq: u64,
    /// The process that follows it for its agent (its MCP server); none: this runner.
    #[serde(default)]
    by: Option<u32>,
    /// A detached run this runner follows: how to follow it on after a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resume: Option<Resume>,
    /// A detached run's output so far, its end ([`LINE_SHARE`]): its news's tail once it ends.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    tail: String,
}

/// Where a detached run is followed: run `run` of reach session `session` on `peer` (none: it ran
/// on this machine, which a new runner process can't follow on).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct Resume {
    peer: String,
    session: String,
    run: u64,
}

#[derive(Default)]
struct State {
    /// The server welcomed this runner (T1: not on its Caps, which come first).
    up: bool,
    /// The server says it takes news, on this connection ([`FEATURE`]), before its Welcome or
    /// after; cleared when it goes (T1).
    takes: bool,
    /// A server took news on the last connection: answers say news goes, once it's back.
    took: bool,
    entries: Vec<Entry>,
    /// Runs over that each MCP server told of, by its pid: what it says of them again isn't news.
    over: HashMap<u32, HashSet<String>>,
    seq: u64,
    /// The server link's RTO, from the news it answered.
    rto: Rto,
    /// Where the outbox is kept.
    file: Option<PathBuf>,
    /// The outbox kept was read in (not while the runner process before this one still runs).
    loaded: bool,
    forwarding: bool,
    writing: bool,
    dirty: bool,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Default::default);
/// News came to the outbox.
static NEWS: LazyLock<event_listener::Event> = LazyLock::new(event_listener::Event::new);
/// The server came, went, or said whether it takes news.
static LINK: LazyLock<event_listener::Event> = LazyLock::new(event_listener::Event::new);

/// Where the outbox is kept (`dir`, the reach node's), read in now unless `taking_over` (a new
/// build taking over from the runner being upgraded, which follows its runs until it ends:
/// [`predecessor_ended`] reads it then).
pub fn start(dir: &Path, taking_over: bool) {
    STATE.lock().file = Some(dir.join("told.json"));
    if !taking_over {
        predecessor_ended();
    }
}

/// The runner this one took over from has ended (or nothing was taken over): the outbox it kept is
/// read in, merged under what came since, and the detached runs it followed are followed on.
pub fn predecessor_ended() {
    let Some(file) = STATE.lock().file.clone() else { return };
    drop(crate::rt::spawn(async move {
        let kept = crate::rt::blocking(move || read(&file)).await.ok().unwrap_or_default();
        let resume = merge(kept);
        for e in resume {
            follow_on(e).await;
        }
        persist();
        NEWS.notify(usize::MAX);
    }));
}

fn read(file: &Path) -> Vec<Entry> {
    match std::fs::read(file) {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
            tracing::warn!(target: "kiln.reach", file = %file.display(), error = %e, "the runs this runner followed couldn't be read");
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}

/// `kept` taken in under what came since (newer), each to go to the server again; the detached
/// runs among them still going, to follow on.
fn merge(kept: Vec<Entry>) -> Vec<Entry> {
    let mut st = STATE.lock();
    st.loaded = true;
    let mut resume = Vec::new();
    for mut e in kept {
        if st.entries.iter().any(|m| m.news.key == e.news.key && m.session == e.session) {
            continue;
        }
        st.seq += 1;
        e.seq = st.seq;
        e.sent = false;
        if e.resume.is_some() && !e.news.event.over() {
            resume.push(e.clone());
        }
        st.entries.push(e);
    }
    resume
}

/// A detached run the runner process before this one followed, followed on here: on its peer, in
/// the reach session it was followed in; one that ran on this machine, or can't be, is told as
/// ended unknown.
async fn follow_on(e: Entry) {
    let Some(r) = e.resume.clone() else { return };
    let d = Detached { session: e.session.clone(), news: e.news.clone() };
    if !r.peer.is_empty()
        && let Some(h) = super::node::global()
    {
        let (tx, rx) = async_channel::unbounded();
        if h.refollow(&r.peer, &r.session, r.run, tx).await {
            drop(crate::rt::spawn(d.follow(rx)));
            return;
        }
    }
    d.ended(None, Some(if r.peer.is_empty() { LOST_HERE } else { LOST }.to_owned()), 0);
}

/// The server welcomed this runner (its node is up): news goes once it says it takes it.
pub fn server_up() {
    set_link(Some(true), None);
}

/// The server connection went: nothing goes until it's back and says it takes news again (T1).
pub fn server_down() {
    set_link(Some(false), None);
}

/// The features the server says it speaks, by name: whether it takes news ([`FEATURE`]).
pub fn server_features(names: &[String]) {
    set_link(None, Some(names.iter().any(|n| n == FEATURE)));
}

fn set_link(up: Option<bool>, takes: Option<bool>) {
    let start = {
        let mut st = STATE.lock();
        let was = st.up && st.takes;
        if let Some(u) = up {
            st.up = u;
            if !u {
                st.takes = false;
            }
        }
        // Its features may come before its Welcome or after: either way, news goes once both have.
        if let Some(t) = takes {
            st.takes = t;
            st.took = t;
        }
        let now = st.up && st.takes;
        // Taken anew: the latest news of every run still followed goes again, after what waited
        // (a server that restarted, or lost a hold while this runner was away, has it back: T4).
        if now && !was {
            let State { entries, seq, .. } = &mut *st;
            for e in entries.iter_mut().filter(|e| !e.news.event.over()) {
                *seq += 1;
                e.seq = *seq;
                e.sent = false;
            }
        }
        let start = (now && !st.forwarding).then(super::node::global).flatten();
        if start.is_some() {
            st.forwarding = true;
        }
        start
    };
    if let Some(h) = start {
        drop(crate::rt::spawn(forward(h.clone())));
    }
    LINK.notify(usize::MAX);
    NEWS.notify(usize::MAX);
}

/// News goes from this runner now (its server takes it).
#[cfg(test)]
fn takes() -> bool {
    let st = STATE.lock();
    st.up && st.takes
}

/// Whether what's said now reaches the agent: the server takes news, or the last one that said
/// took it (it's away, or hasn't said yet on this connection), and none said since it doesn't.
fn will_tell() -> bool {
    let st = STATE.lock();
    st.takes || st.took
}

/// `news` of a run of agent session `session`'s, which process `by` follows (its MCP server;
/// none: this runner): into the outbox, after what's there, of each run its latest only. The same
/// again is no news (T9), nor is what an MCP server says again of a run it told was over.
pub fn say(session: &str, news: RunNews, by: Option<u32>) {
    if session.is_empty() || news.key.is_empty() {
        return;
    }
    let news = news.clipped();
    #[cfg(test)]
    HEARD.lock().push((session.to_owned(), news.clone()));
    {
        let mut st = STATE.lock();
        if by.is_some_and(|pid| st.over.get(&pid).is_some_and(|o| o.contains(&news.key))) {
            return;
        }
        st.seq += 1;
        let seq = st.seq;
        match st.entries.iter_mut().find(|e| e.news.key == news.key && e.session == session) {
            Some(e) if news == e.news || news.event < e.news.event => return,
            Some(e) => {
                e.news = news;
                e.sent = false;
                e.seq = seq;
                e.by = by.or(e.by);
            }
            None => st.entries.push(Entry { session: session.to_owned(), news, sent: false, seq, by, resume: None, tail: String::new() }),
        }
    }
    persist();
    NEWS.notify(usize::MAX);
}

/// Sends the outbox to the server, one at a time and in order, each once the one before it was
/// answered: the server hears each run's news in the order it came (T10). One unanswered within
/// the link's RTO goes again after it, the RTO doubling (RFC 6298 §5.5), or at once when the server
/// is back (T1); a round trip answered at the first try is measured into it.
async fn forward(handle: Handle) {
    use futures::FutureExt as _;
    let mut tries = 0u32;
    loop {
        let news_ear = NEWS.listen();
        let next = {
            let st = STATE.lock();
            if st.up && st.takes { st.entries.iter().filter(|e| !e.sent).min_by_key(|e| e.seq).map(|e| (e.session.clone(), e.news.clone())) } else { None }
        };
        let Some((session, news)) = next else {
            news_ear.await;
            tries = 0;
            continue;
        };
        let ask = match serde_json::to_string(&super::Question::Told { session: session.clone(), news: news.clone() }) {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(target: "kiln.reach", error = %e, "news of a run that can't be written");
                answered(&session, &news, false);
                continue;
            }
        };
        let rto = STATE.lock().rto.timeout();
        let link_ear = LINK.listen();
        let sent = Instant::now();
        let got = futures::select_biased! {
            a = handle.query(ask).fuse() => Some(a),
            () = crate::rt::sleep(rto).fuse() => None,
        };
        match got {
            Some(Ok(a)) => {
                if tries == 0 {
                    STATE.lock().rto.sample(u64::try_from(sent.elapsed().as_millis()).unwrap_or(u64::MAX));
                }
                tries = 0;
                let refused = serde_json::from_str::<Value>(&a).ok().and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_owned));
                if let Some(why) = &refused {
                    tracing::info!(target: "kiln.reach", %session, run = %news.key, %why, "the server didn't take news of a run");
                }
                answered(&session, &news, refused.is_some());
            }
            Some(Err(_)) | None => {
                tries = tries.saturating_add(1);
                let wait = {
                    let mut st = STATE.lock();
                    st.rto.expired();
                    st.rto.timeout()
                };
                tracing::debug!(target: "kiln.reach", wait_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX), "news of a run waits for the server");
                futures::select_biased! {
                    () = link_ear.fuse() => {}
                    () = crate::rt::sleep(wait).fuse() => {}
                }
            }
        }
    }
}

/// The server answered `news` (`refused`: it didn't take it, and won't): it has it, unless later
/// news came meanwhile; a run over is followed no more.
fn answered(session: &str, news: &RunNews, refused: bool) {
    {
        let mut st = STATE.lock();
        let Some(i) = st.entries.iter().position(|e| e.session == session && e.news.key == news.key) else { return };
        let Some(e) = st.entries.get_mut(i) else { return };
        if e.news != *news {
            return;
        }
        e.sent = true;
        if e.news.event.over() || refused {
            let gone = st.entries.remove(i);
            if let Some(pid) = gone.by {
                st.over.entry(pid).or_default().insert(gone.news.key);
            }
        }
    }
    persist();
}

/// The outbox written to its file as it stands, one write at a time: changes while one is under
/// way are in the next, so writes never pile up, however fast news comes.
fn persist() {
    {
        let mut st = STATE.lock();
        if st.file.is_none() || !st.loaded {
            return;
        }
        st.dirty = true;
        if st.writing {
            return;
        }
        st.writing = true;
    }
    drop(crate::rt::spawn(async {
        loop {
            let (file, bytes) = {
                let mut st = STATE.lock();
                if !st.dirty {
                    st.writing = false;
                    return;
                }
                st.dirty = false;
                (st.file.clone(), serde_json::to_vec(&st.entries))
            };
            let (Some(file), Ok(bytes)) = (file, bytes) else { continue };
            let wrote = crate::rt::blocking(move || {
                let tmp = file.with_extension("json.tmp");
                std::fs::write(&tmp, &bytes)?;
                std::fs::rename(&tmp, &file)
            })
            .await;
            if let Ok(Err(e)) = wrote {
                tracing::warn!(target: "kiln.reach", error = %e, "the runs this runner follows couldn't be kept");
            }
        }
    }));
}

/// Process `client` went under a run it followed, its run not detached (its connection closed
/// before the run ended): the runs it followed in the background went with it, and their agents
/// are told, with no id to read their output by, since what knew them by it has gone (T7).
pub fn gone(client: Option<u32>) {
    let Some(pid) = client else { return };
    let now = crate::util::now_ms();
    let runs: Vec<(String, RunNews)> = {
        let mut st = STATE.lock();
        st.over.remove(&pid);
        st.entries.iter().filter(|e| e.by == Some(pid) && !e.news.event.over()).map(|e| (e.session.clone(), e.news.clone())).collect()
    };
    for (session, news) in runs {
        let news = RunNews { event: RunEvent::Ended, id: String::new(), ended_at: Some(now), exit_code: None, error: Some(STOPPED.to_owned()), tail: String::new(), ..news };
        say(&session, news, None);
    }
}

/// News said over this runner's socket (`Req::Told`, JSON) by process `pid`: for the agent session
/// that process belongs to by the process alone (T12), into the outbox; what to answer. News with
/// no run (`key`) asks whether news goes from here.
#[cfg(unix)]
pub async fn heard(json: &str, pid: Option<u32>) -> Ev {
    let news = match serde_json::from_str::<RunNews>(json) {
        Ok(n) => n,
        Err(e) => return Ev::Error(format!("that isn't news of a run: {e}")),
    };
    if !news.key.is_empty() {
        let session = match pid {
            Some(p) => crate::rt::blocking(move || crate::runner::custody::session_by_pid(p)).await.ok().flatten(),
            None => None,
        };
        let Some(session) = session else { return Ev::Error("news of a run comes from an agent session's own processes".into()) };
        say(&session, news, pid);
    }
    Ev::Answer(json!({ "told": will_tell() }).to_string())
}

/// The note a detached run's client gets when its agent is told how it ends, should the client go
/// first ([`Ev::Note`]; `kiln run` says it on).
pub const DETACHED_NOTE: &str = "detached:";

/// The note for `req`, if it's a detached run an agent session asked for and news goes from here.
pub fn note(req: &RunReq) -> Option<Ev> {
    (req.detach && req.asker.is_some() && will_tell()).then(|| Ev::Note(format!("{DETACHED_NOTE} should this stop first, the command goes on, and kiln tells you how it ends (where, how long, its exit code, the end of its output): no need to wait for it")))
}

/// What this runner sees of a run while its client is there, for its agent's news should the
/// client go first and the run be detached ([`detached`]): when it started, the end of its output.
pub struct Watch {
    /// The client's process.
    client: Option<u32>,
    /// The run is detached: should its client go, it goes on, and its agent is told how it ends.
    detach: bool,
    command: String,
    started_at: Option<i64>,
    tail: String,
}

impl Watch {
    pub fn new(req: &RunReq) -> Self {
        Self { client: req.client, detach: req.detach, command: req.command.clone(), started_at: None, tail: String::new() }
    }

    /// The client's process ([`gone`]).
    pub fn client(&self) -> Option<u32> {
        self.client
    }

    pub fn started(&mut self) {
        if self.detach && self.started_at.is_none() {
            self.started_at = Some(crate::util::now_ms());
        }
    }

    pub fn output(&mut self, data: &[u8]) {
        if self.detach {
            push_tail(&mut self.tail, data);
        }
    }

    /// The agent session the client belongs to by its process alone (T12).
    async fn session(&self) -> Option<String> {
        let pid = self.client.filter(|_| self.detach)?;
        crate::rt::blocking(move || crate::runner::custody::session_by_pid(pid)).await.ok().flatten()
    }
}

/// `data` added to the end of output `tail`, which keeps its last [`LINE_SHARE`] bytes.
fn push_tail(tail: &mut String, data: &[u8]) {
    let data = data.get(data.len().saturating_sub(LINE_SHARE.saturating_mul(2))..).unwrap_or(data);
    tail.push_str(&String::from_utf8_lossy(data));
    if tail.len() > LINE_SHARE.saturating_mul(2) {
        *tail = clip_tail(tail, LINE_SHARE);
    }
}

/// Detached run `run` on `peer`, its client gone: followed on alone (`Handle::detach_in`), and,
/// asked for by an agent session, followed for it ([`Detached`]) until it ends, what it says
/// coming on `says`.
pub async fn detached(handle: &Handle, peer: &str, run: u64, w: Watch, says: async_channel::Receiver<Say>) -> anyhow::Result<()> {
    let reach_session = handle.detach_in(peer, run).await;
    if let Some(session) = w.session().await {
        let resume = reach_session.map(|s| Resume { peer: peer.to_owned(), session: s, run });
        let d = Detached::going(&session, &w, peer, resume);
        drop(crate::rt::spawn(d.follow(says)));
    }
    Ok(())
}

/// A detached run on this machine (runner `me`), its client gone: its events read to its end so
/// its output never makes it wait, and, asked for by an agent session, followed for it.
pub async fn follow_here(rx: async_channel::Receiver<super::serve::Event>, w: Watch, me: String) {
    use super::serve::Event;
    let mut d = w.session().await.map(|session| Detached::going(&session, &w, &me, Some(Resume::default())));
    while let Ok(e) = rx.recv().await {
        match (e, d.as_mut()) {
            (Event::Started { .. }, Some(d)) => d.started(),
            (Event::Output { data, .. }, Some(d)) => d.output(&data),
            (Event::Exit { code, error }, d) => {
                if let Some(d) = d {
                    d.clone().ended(code, error, 0);
                }
                return;
            }
            _ => {}
        }
    }
    if let Some(d) = d {
        d.ended(None, Some(LOST.to_owned()), 0);
    }
}

/// A detached run this runner follows for agent session `session` once its client went
/// (`kiln run --detach`, docs/reach.md §4.2): what it tells of it.
#[derive(Clone)]
struct Detached {
    session: String,
    news: RunNews,
}

impl Detached {
    /// The run `w` watched, on runner `on`, its client gone: in the background now. Its agent's
    /// news holds it as work it has going, and the outbox keeps how to follow it on (`resume`).
    fn going(session: &str, w: &Watch, on: &str, resume: Option<Resume>) -> Self {
        let event = if w.started_at.is_some() { RunEvent::Started } else { RunEvent::Going };
        let news = RunNews { key: crate::util::new_id(), event: RunEvent::Going, command: w.command.clone(), on: on.to_owned(), how: DETACHED.to_owned(), handed: 1, started_at: w.started_at, ..RunNews::default() };
        say(session, news.clone(), None);
        {
            let mut st = STATE.lock();
            if let Some(e) = st.entries.iter_mut().find(|e| e.news.key == news.key) {
                e.resume = resume;
                e.tail.clone_from(&w.tail);
            }
        }
        persist();
        let mut d = Self { session: session.to_owned(), news };
        if event == RunEvent::Started {
            d.news.event = RunEvent::Started;
            say(&d.session, d.news.clone(), None);
        }
        d
    }

    /// It started after its client went: state its agent has (no wake, T8).
    fn started(&mut self) {
        if self.news.started_at.is_none() {
            self.news.started_at = Some(crate::util::now_ms());
            self.news.event = RunEvent::Started;
            say(&self.session, self.news.clone(), None);
        }
    }

    fn output(&mut self, data: &[u8]) {
        {
            let mut st = STATE.lock();
            let Some(e) = st.entries.iter_mut().find(|e| e.news.key == self.news.key) else { return };
            push_tail(&mut e.tail, data);
        }
        persist();
    }

    /// It ended as its exit says: news, with the end of its output.
    fn ended(self, code: Option<i32>, error: Option<String>, lost: u64) {
        let tail = STATE.lock().entries.iter().find(|e| e.news.key == self.news.key).map(|e| clip_tail(&e.tail, LINE_SHARE)).unwrap_or_default();
        let news = RunNews { event: RunEvent::Ended, ended_at: Some(crate::util::now_ms()), exit_code: code, error, lost, tail, ..self.news };
        say(&self.session, news, None);
    }

    /// Follow it to its end on what its node says of it (`says`: once detached, its start, its
    /// output, its exit).
    async fn follow(mut self, says: async_channel::Receiver<Say>) {
        loop {
            match says.recv().await {
                Ok(Say::Started { .. }) => self.started(),
                Ok(Say::Output { data, .. }) => self.output(&data),
                Ok(Say::Exit { code, error, lost, .. }) => return self.ended(code, error, lost),
                Ok(_) => {}
                Err(_) => return self.ended(None, Some(LOST.to_owned()), 0),
            }
        }
    }
}

/// Says news of an agent's runs to its runner (`Req::Told`), in the order it came, on a thread of
/// its own: nothing waits on it (T6), and what didn't go goes again after the socket's RTO,
/// measured from the news it answered (RFC 6298), each attempt bounded by it. Of each run only its
/// latest news waits. One per runner socket ([`teller`]).
#[cfg(unix)]
#[derive(Clone)]
pub struct Teller {
    tx: std::sync::mpsc::Sender<RunNews>,
    /// The runner said news goes from it, as it last said.
    takes: Arc<AtomicBool>,
}

/// The teller for the runner at `sock`, started the first time it's asked for.
#[cfg(unix)]
pub fn teller(sock: &Path) -> Teller {
    static TELLERS: LazyLock<Mutex<HashMap<PathBuf, Teller>>> = LazyLock::new(Default::default);
    TELLERS
        .lock()
        .entry(sock.to_owned())
        .or_insert_with(|| {
            let (tx, rx) = std::sync::mpsc::channel::<RunNews>();
            let takes = Arc::new(AtomicBool::new(false));
            let (sock, t) = (sock.to_owned(), Arc::clone(&takes));
            let _ = std::thread::Builder::new().name("kiln-told".into()).spawn(move || tell_all(&sock, &rx, &t));
            // Asks first whether news goes from the runner (news of no run), for the first answers.
            let _ = tx.send(RunNews::default());
            Teller { tx, takes }
        })
        .clone()
}

#[cfg(unix)]
impl Teller {
    pub fn tell(&self, news: RunNews) {
        let _ = self.tx.send(news.clipped());
    }

    /// Whether news goes from the runner, as it last said (no waiting for it).
    pub fn takes(&self) -> bool {
        self.takes.load(Ordering::Relaxed)
    }
}

/// The teller's thread: each news to the runner at `sock` in turn ([`Teller`]).
#[cfg(unix)]
fn tell_all(sock: &Path, rx: &std::sync::mpsc::Receiver<RunNews>, takes: &AtomicBool) {
    let mut rto = Rto::default();
    let mut waiting: VecDeque<RunNews> = VecDeque::new();
    let mut tries = 0u32;
    let add = |waiting: &mut VecDeque<RunNews>, n: RunNews| match waiting.iter_mut().find(|w| !n.key.is_empty() && w.key == n.key) {
        Some(w) if n.event >= w.event => *w = n,
        Some(_) => {}
        None => waiting.push_back(n),
    };
    loop {
        if waiting.is_empty() {
            match rx.recv() {
                Ok(n) => add(&mut waiting, n),
                Err(_) => return,
            }
        }
        while let Ok(n) = rx.try_recv() {
            add(&mut waiting, n);
        }
        let Some(front) = waiting.front().cloned() else { continue };
        let sent = Instant::now();
        match tell_runner(sock, &front, rto.timeout()) {
            Ok(said) => {
                if tries == 0 {
                    rto.sample(u64::try_from(sent.elapsed().as_millis()).unwrap_or(u64::MAX));
                }
                tries = 0;
                if let Some(t) = said {
                    takes.store(t, Ordering::Relaxed);
                }
                waiting.pop_front();
            }
            Err(e) => {
                tries = tries.saturating_add(1);
                rto.expired();
                tracing::debug!(target: "kiln.reach", error = %format!("{e:#}"), wait_ms = rto.ms(), "news of a run waits for the runner");
                // Waits the RTO, taking in what comes meanwhile.
                let until = Instant::now() + rto.timeout();
                while let Some(left) = until.checked_duration_since(Instant::now()).filter(|l| !l.is_zero()) {
                    match rx.recv_timeout(left) {
                        Ok(n) => add(&mut waiting, n),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
            }
        }
    }
}

/// `news` said to the runner at `sock` (`Req::Told`), its connection bounded by `timeout`: whether
/// news goes from it (none: it refused this news, which goes no more). A runner from before
/// doesn't know it, and closes the connection: that's an error, and it goes again later.
#[cfg(unix)]
fn tell_runner(sock: &Path, news: &RunNews, timeout: Duration) -> anyhow::Result<Option<bool>> {
    let mut c = Client::connect(sock)?;
    let s = c.sender()?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    c.send(&Req::Told(serde_json::to_string(news)?))?;
    match c.event()? {
        Some(Ev::Answer(a)) => Ok(serde_json::from_str::<Value>(&a).ok().and_then(|v| v.get("told").and_then(Value::as_bool)).or(Some(false))),
        Some(Ev::Error(e)) => {
            tracing::info!(target: "kiln.reach", run = %news.key, error = %e, "the runner didn't take news of a run");
            Ok(None)
        }
        _ => anyhow::bail!("the kiln runner closed the connection"),
    }
}

/// What an MCP server tells of one of its runs (docs/reach.md §7): a run handed over while it goes
/// (a background answer, or one past its wait) is work its agent has going; each answer that hands
/// it over again (`output` that returned with it going) says so, its agent waiting on it in that
/// turn (T2); its start, when what handed it over didn't say it had, is state; its end, with no
/// call waiting on it, is news; an answer that shows its end after says it seen. Each said through
/// the one [`Teller`] while the run's lock is held, so in the order decided (T10).
#[cfg(unix)]
#[derive(Default)]
pub struct Telling {
    teller: Option<Teller>,
    /// What it told, for tests.
    #[cfg(test)]
    said: Vec<RunNews>,
    news: RunNews,
    /// What handed it over said it had started, or news since did.
    start_said: bool,
    /// Calls waiting on it now.
    waiting: usize,
    /// Its end was told, or its agent saw it, or stopped it.
    done: bool,
}

#[cfg(unix)]
impl Telling {
    /// Run `id` of this MCP server, `args` the tool's input, told to the runner at `sock`.
    pub fn begin(sock: &Path, id: &str, args: &Value) -> Self {
        static ME: LazyLock<String> = LazyLock::new(crate::util::new_id);
        let command = args.get("command").and_then(Value::as_str).unwrap_or_default().to_owned();
        let news = RunNews { key: format!("{}-{id}", *ME), id: id.to_owned(), command, how: "run".to_owned(), ..RunNews::default() };
        Self { teller: Some(teller(sock)), news, ..Self::default() }
    }

    /// How it was started (`kiln.machines.start`, …), when not by `run`.
    pub fn how(&mut self, how: &str) {
        how.clone_into(&mut self.news.how);
    }

    fn tell(&mut self, news: RunNews) {
        #[cfg(test)]
        self.said.push(news.clone());
        if let Some(t) = &self.teller {
            t.tell(news);
        }
    }

    /// What follows the run heard `ev` (before taking it in), the run on `runner`, its output so
    /// far `output`: its start, or its end, told when its agent has it and nothing waits on it.
    pub fn heard(&mut self, ev: &anyhow::Result<Option<Ev>>, runner: &str, output: &str) {
        let (code, error, lost) = match ev {
            Ok(Some(Ev::Started { .. })) => {
                self.news.started_at.get_or_insert_with(crate::util::now_ms);
                if self.news.handed > 0 && !self.start_said && !self.done {
                    self.start_said = true;
                    self.news.event = RunEvent::Started;
                    runner.clone_into(&mut self.news.on);
                    self.tell(self.news.clone());
                }
                return;
            }
            Ok(Some(Ev::Exit { code, error, lost })) => (*code, error.clone(), *lost),
            Ok(Some(Ev::Error(e))) => (None, Some(e.clone()), 0),
            Ok(Some(_)) => return,
            Ok(None) | Err(_) => (None, Some("the kiln runner closed the connection".to_owned()), 0),
        };
        if self.news.handed == 0 || self.waiting > 0 || self.done {
            return;
        }
        self.done = true;
        let news = RunNews { event: RunEvent::Ended, on: runner.to_owned(), ended_at: Some(crate::util::now_ms()), exit_code: code, error, lost, tail: clip_tail(output, LINE_SHARE), ..self.news.clone() };
        self.tell(news);
    }

    /// A call waits on it.
    pub fn waits(&mut self) {
        self.waiting += 1;
    }

    /// The call that waited answers `v`, the run `done` or going on `runner`: going, it's handed
    /// over (again), and the answer says kiln tells when it ends (when news goes from the runner);
    /// done, after it was handed over, its agent saw its end.
    pub fn answered(&mut self, v: &mut Value, done: bool, runner: &str) {
        self.waiting = self.waiting.saturating_sub(1);
        if self.done {
            return;
        }
        if done {
            if self.news.handed > 0 {
                self.done = true;
                self.tell(RunNews { event: RunEvent::Seen, ..self.news.clone() });
            }
            return;
        }
        self.news.handed = self.news.handed.saturating_add(1);
        if self.news.handed == 1 {
            self.start_said = self.news.started_at.is_some();
        }
        runner.clone_into(&mut self.news.on);
        self.news.event = if self.start_said { RunEvent::Started } else { RunEvent::Going };
        self.tell(RunNews { event: RunEvent::Going, ..self.news.clone() });
        if self.teller.as_ref().is_some_and(Teller::takes)
            && let Some(o) = v.as_object_mut()
        {
            o.insert("told".into(), json!(TOLD));
        }
    }

    /// Its agent stops it: how it ends is no news to it.
    pub fn stopped(&mut self) {
        if !self.done && self.news.handed > 0 {
            self.tell(RunNews { event: RunEvent::Seen, ..self.news.clone() });
        }
        self.done = true;
    }
}

/// What an answer about a run its agent was handed says while it goes, when news goes from the
/// runner (`Telling::answered`).
#[cfg(unix)]
const TOLD: &str = "kiln tells you when it ends (where, how long, its exit code and the end of its output): end your turn rather than wait on `output` for it.";

/// What this runner said of its sessions' runs, for tests: by session, in order.
#[cfg(test)]
pub static HEARD: LazyLock<Mutex<Vec<(String, RunNews)>>> = LazyLock::new(Default::default);

/// What this runner said of `session`'s runs, in order ([`HEARD`]).
#[cfg(all(test, unix))]
pub fn heard_of(session: &str) -> Vec<RunNews> {
    HEARD.lock().iter().filter(|(s, _)| s == session).map(|(_, n)| n.clone()).collect()
}

#[cfg(all(test, unix))]
mod tests;
