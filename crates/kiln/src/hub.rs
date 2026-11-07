//! The server core: agent sessions, the scheduler, job bookkeeping and the UI event bus.
//!
//! All network input arrives as [`NetEvent`]s from the QUIC thread and is handled here; every
//! outcome is persisted through [`Store`] before it is broadcast, so a crash at any point loses
//! at most in-flight log bytes, never job state.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hyper_transport::PeerId;
use parking_lot::Mutex;
use serde::Serialize;
use futures::FutureExt as _;

use crate::rt::{Interval, Notify, Shutdown};

use crate::config::Config;
use crate::logs::Logs;
use crate::metrics::{self, HostSample};
use crate::net::{Kind, NetEvent, NetHandle};
use crate::pipeline::Context;
use crate::proto::{self, AgentMsg, Assignment, Hello, JobResult, LANE_CONTROL, LANE_TERM, ServerMsg};
use crate::store::{AgentRow, Job, Status, Store};
use crate::wire::events::ServerEvent;
use crate::wire::payload::{Carried, FromRunner};
use crate::wire::unread::{Code, Wire};

mod build;

/// The server's secrets as `cfg` says them, a `file:` one read from its file (blocking: [`Hub::job_secrets`] reads them
/// off the shards).
fn secrets_of(cfg: &Config) -> Vec<(String, String)> {
    cfg.secrets
        .iter()
        .map(|(k, v)| {
            let val = match v.strip_prefix("file:") {
                Some(path) => std::fs::read_to_string(path).map(|s| s.trim().to_owned()).unwrap_or_default(),
                None => v.clone(),
            };
            (k.clone(), val)
        })
        .collect()
}

/// How often the newest runner process is reminded that an older one can be retired.
const RETIRE_REPEAT: Duration = Duration::from_secs(60);
/// Host samples kept per agent for the UI charts (1 h at 5 s).
const HISTORY: usize = 720;
/// How long an agent that declined a job is not offered that job again.
const DECLINE_BACKOFF: Duration = Duration::from_secs(15);
/// How long a command whose caller doesn't wait for it is waited for, to log what came of it.
pub(crate) const LOG_WITHIN: Duration = Duration::from_secs(300);
/// How long a runner is given to say it froze a job, or let it go on (docs/ci.md W26-R22): its
/// cgroup's freezer and `docker pause` are given 5 and 10 s there, and undoing half of it as long.
const FREEZE_WITHIN: Duration = Duration::from_secs(45);
/// How long a person's command (through the API) waits for its runner's answer.
pub const COMMAND_WITHIN: Duration = Duration::from_secs(30);
/// How often the server says again what it has of each runner's channel streams.
const HAVE_AGAIN: Duration = Duration::from_secs(5);
/// How many sessions' digests one `ServerMsg::Kept` carries at the most.
const KEPT_A_FRAME: usize = 32;
/// How soon a refused pod launch is tried again.
const LAUNCH_RETRY: Duration = Duration::from_secs(15);
/// How long after a runner says what it holds its sessions that still read disconnected are asked
/// for their snapshots: time for the snapshots its streams send by themselves (a new server) to
/// come first.
const STALE_GRACE: Duration = Duration::from_secs(15);
/// The events ([`Hub::events`]) kept for their slowest subscriber, at the most: their texts are bounded in
/// bytes too, at [`crate::net::EVENTS_BYTES`] (goal 11, plan 01a1a87b2e56 step 3:
/// `crate::memory::owners::UI_EVENTS`).
pub const EVENTS: usize = 4096;

/// Where a Kubernetes job's pod stands until its agent connects.
#[derive(Debug, Clone, Copy)]
pub enum Launch {
    /// A pod was created (or adopted after a restart) at this instant.
    Pod(Instant),
    /// No pod is on its way: launch again from `at`. `check_pods` when the failed attempt may
    /// have left a pod behind (an API timeout) or a dead one may hold the name.
    Retry { at: Instant, check_pods: bool },
}

/// How long an outpost invite works ([`OutpostInvites`]).
pub const OUTPOST_INVITE_TTL: Duration = Duration::from_secs(3600);

/// One-time invites for outposts (`kiln outpost`): each enrolls one, once, within
/// [`OUTPOST_INVITE_TTL`].
#[derive(Default)]
pub struct OutpostInvites(crate::rcu::Rcu<HashMap<String, Instant>>);

impl OutpostInvites {
    /// A new invite, and when it stops working (unix seconds).
    pub fn mint(&self) -> (String, i64) {
        let token = crate::util::random_token();
        self.add(token.clone(), Instant::now() + OUTPOST_INVITE_TTL);
        (token, crate::util::now_unix() + i64::try_from(OUTPOST_INVITE_TTL.as_secs()).unwrap_or(0))
    }

    fn add(&self, token: String, until: Instant) {
        self.0.update(|g| {
            g.retain(|_, u| *u > Instant::now());
            g.insert(token.clone(), until);
        });
    }

    /// Uses up an invite: whether it was one and still worked.
    pub fn take(&self, token: &str) -> bool {
        self.0.update(|g| g.remove(token)).is_some_and(|until| until > Instant::now())
    }
}

/// Whether a runner carries a label a job asks for (`runs-on`), in any case: one of its own, its
/// name, or what its platform says (`linux`, `macos`, `windows`; `x86_64`, `amd64`, `x64`;
/// `aarch64`, `arm64`; `self-hosted`), so `[linux]` fits every Linux runner, not only those labelled so.
pub(crate) fn carries(labels: &[String], os: &str, arch: &str, name: &str, want: &str) -> bool {
    want == name || labels.iter().any(|l| l.eq_ignore_ascii_case(want)) || platform_labels(os, arch).iter().any(|l| l.eq_ignore_ascii_case(want))
}

/// Whether runner `a` meets every constraint of `job`'s `requires` (docs/ci.md W05-R22), against
/// the facts it last reported. One that hasn't reported any meets none: a job that needs a tool
/// waits for a runner known to have it, rather than going where it would fail (the release's
/// macOS job on a Mac with no developer tools, 2026-10-24).
fn meets(job: &Job, a: &AgentLive) -> bool {
    job.spec.as_ref().is_none_or(|spec| facts_meet(&spec.requires, a.facts.as_deref(), &a.labels))
}

/// [`meets`]: each of `requires` holds for `facts` (none reported: none holds).
fn facts_meet(requires: &[String], facts: Option<&kiln_ci::facts::RunnerFacts>, labels: &[String]) -> bool {
    if requires.is_empty() {
        return true;
    }
    let Some(f) = facts else { return false };
    requires.iter().all(|r| kiln_ci::sched::routing::Constraint::parse(r).is_ok_and(|c| c.eval(f, labels)))
}

/// Why runner `a` can't take a job of `labels` (`runs-on`) and `requires` whatever its load, as
/// [`Hub::pick_agent`] places it: each label it lacks, and each constraint its facts don't meet
/// ([`meets`]). Empty: it can. Where a retry may ask the job to run (docs/ci.md W05-R28).
pub(crate) fn cannot_take(labels: &[String], requires: &[String], a: &AgentLive) -> Vec<String> {
    let mut why: Vec<String> = labels.iter().filter(|l| !carries(&a.labels, &a.os, &a.arch, &a.name, l)).map(|l| format!("lacks the label {l}")).collect();
    match a.facts.as_deref() {
        None if !requires.is_empty() => why.push("hasn't said what it has, which its `requires` needs".to_owned()),
        facts => why.extend(requires.iter().filter(|r| !facts_meet(std::slice::from_ref(*r), facts, &a.labels)).map(|r| format!("needs {r}"))),
    }
    why
}

/// For a job waiting on its `requires`: each connected runner its labels fit, with the constraints
/// its facts don't meet (every one, when it hasn't reported facts) and the tools they name, those
/// an install would fix first, then those that lack least. None when one already meets them all:
/// the job waits for its slot, not a tool.
pub fn unmet(hub: &Hub, labels: &[String], requires: &[String]) -> Vec<crate::toolfix::Unmet> {
    let g = hub.agents.load();
    let fits: Vec<Fit<'_>> = g
        .values()
        .filter(|a| a.connected && !a.ephemeral && labels.iter().all(|l| carries(&a.labels, &a.os, &a.arch, &a.name, l)))
        .map(|a| (a.name.as_str(), a.facts.as_deref(), a.labels.as_slice(), crate::toolfix::agent_cannot(Some(a), &a.name)))
        .collect();
    unmet_of(&fits, requires)
}

/// A runner as [`unmet_of`] weighs it: its name, facts and labels, and why it hosts no agent.
type Fit<'a> = (&'a str, Option<&'a kiln_ci::facts::RunnerFacts>, &'a [String], Option<String>);

/// [`unmet`] for these runners.
fn unmet_of(fits: &[Fit<'_>], requires: &[String]) -> Vec<crate::toolfix::Unmet> {
    if fits.iter().any(|(_, facts, labels, _)| facts_meet(requires, *facts, labels)) {
        return Vec::new();
    }
    let mut out: Vec<crate::toolfix::Unmet> = fits
        .iter()
        .map(|(name, facts, labels, no_agent)| {
            let unmet: Vec<String> = requires.iter().filter(|r| !facts_meet(std::slice::from_ref(*r), *facts, labels)).cloned().collect();
            let fixable = no_agent.is_none() && unmet.iter().all(|r| crate::toolfix::only_tools(r));
            crate::toolfix::Unmet { runner: (*name).to_owned(), tools: crate::toolfix::tools_in(&unmet), unmet, fixable, no_agent: no_agent.clone() }
        })
        .collect();
    out.sort_by(|a, b| b.fixable.cmp(&a.fixable).then_with(|| a.unmet.len().cmp(&b.unmet.len())).then_with(|| a.runner.cmp(&b.runner)));
    out
}

/// The labels a runner's platform gives it.
fn platform_labels<'a>(os: &'a str, arch: &'a str) -> Vec<&'a str> {
    let mut v = vec!["self-hosted", os, arch];
    if os == "macos" {
        v.push("darwin");
    }
    match arch {
        "x86_64" => v.extend(["amd64", "x64"]),
        "aarch64" => v.push("arm64"),
        _ => {}
    }
    v.retain(|l| !l.is_empty());
    v
}

/// How hard a runner is pressed, by its last sample (none yet: unknown, not overloaded).
fn strain_of(a: &AgentLive) -> crate::reach::directory::Strain {
    a.sample.as_ref().map(|s| crate::reach::directory::strain(s.cpus, s.cpu_pct, s.load1, s.mem_total, s.mem_used)).unwrap_or_default()
}

/// An outpost (`kiln outpost`) says so with its label and no slots; a runner given the label by
/// hand still takes jobs.
fn is_outpost(labels: &[String], slots: u32) -> bool {
    slots == 0 && labels.iter().any(|l| l == crate::proto::OUTPOST_LABEL)
}

/// A command on its way to a runner ([`Hub::command_start`]): its answer, once its runner gives
/// it.
pub struct Pending {
    agent: String,
    /// The name people see of its runner ([`Hub::shown`]), for what's said of its answer.
    shown: String,
    what: &'static str,
    waits: Vec<futures::channel::oneshot::Receiver<crate::wire::commands::Answer>>,
    /// What it is already: sent to processes that can't answer, or reaching none.
    at_once: crate::wire::commands::Answer,
    /// A parked command whose runner is away: it goes once the runner is back.
    away: bool,
}

impl Pending {
    /// A parked command whose runner is away: it goes once the runner is back.
    pub fn is_away(&self) -> bool {
        self.away
    }

    /// Its answer, when it has one already: none of its runner's processes could take it, or
    /// none can answer.
    pub fn at_once(&self) -> Option<&crate::wire::commands::Answer> {
        self.waits.is_empty().then_some(&self.at_once)
    }

    /// Its answer, waited for up to `within`: every process it went to answers, and the first
    /// answer that isn't a taking is what comes back.
    pub async fn answer(self, within: Duration) -> crate::wire::commands::Answer {
        use crate::wire::commands::Answer;
        let deadline = Instant::now() + within;
        let mut answer = self.at_once;
        for rx in self.waits {
            let a = match crate::rt::timeout(deadline.saturating_duration_since(Instant::now()), rx).await {
                Ok(Ok(a)) => a,
                Ok(Err(_)) => Answer::Unanswered { why: "the server let go of it".into() },
                Err(_) => Answer::Unanswered { why: format!("no answer from {} in {} s", self.shown, within.as_secs()) },
            };
            if answer.taken() && !a.taken() {
                answer = a;
            }
        }
        if !answer.taken() && answer != Answer::Sent {
            tracing::info!(agent = %self.agent, command = self.what, answer = %answer.words(), "a command to a runner wasn't taken");
        }
        answer
    }

    /// Its answer, or [`Answer::Parked`] at once when its runner is away and it waits for it: for
    /// a person's command, which needn't wait for the runner to come back.
    ///
    /// [`Answer::Parked`]: crate::wire::commands::Answer::Parked
    pub async fn answer_or_parked(self, within: Duration) -> crate::wire::commands::Answer {
        if self.away {
            self.log_when_answered(LOG_WITHIN);
            return crate::wire::commands::Answer::Parked;
        }
        self.answer(within).await
    }

    /// Its answer whenever it comes (`within` at the most), said in the log when it isn't a
    /// taking: for a caller that doesn't wait for it.
    pub fn log_when_answered(self, within: Duration) {
        if self.waits.is_empty() {
            return;
        }
        crate::rt::spawn(async move {
            let _ = self.answer(within).await;
        });
    }
}

/// A refusal (`n` of them, for a runner's word with those it folded in), at debug under its own
/// target: what the count says, in a log that outlives the process (the wire lab counts it across
/// the server's restarts, `tests/wire_lab.rs`).
fn said_unread(u: &crate::wire::unread::Unread, n: u64) {
    tracing::debug!(target: "kiln::refused", wire = u.wire.name(), code = u.code.name(), what = %u.what, n, "refused what it couldn't read");
}

/// Which of a runner process's command streams `msg` rides (docs/wire.md, "Commands"): live for
/// a command refused while its runner is away, parked for one that waits for it. A `HarnessCmd`
/// rides as the command it carries says, but for a message with an id, which rides live (CP-T3.2):
/// it goes this way only to a process whose inbox isn't known (one from before inboxes, or one just
/// connected that hasn't said yet), with no epoch, and parked it would go once that process was
/// back, though its session went on elsewhere meanwhile and took it there, sent again under its id
/// (the twenty-second adversarial check's). Live, a link that drops before it's answered ends it,
/// and its sender sends it again under its id, wherever its session runs then. None: not a command.
fn stream_of(msg: &ServerMsg) -> Option<crate::wire::commands::Kind> {
    use crate::wire::commands::Kind as Stream;
    use crate::wire::{Away, Class};
    let class = match (crate::wire::server_class(msg), msg) {
        (Class::Command(Away::AsItsCommand), ServerMsg::HarnessCmd { json, .. }) => crate::wire::payload::command_class(json),
        (c, _) => c,
    };
    match class {
        Class::Command(Away::Refuse) => Some(Stream::Live),
        Class::Command(Away::Park | Away::AsItsCommand) | Class::State => Some(Stream::Parked),
        _ => None,
    }
}

/// The agent session a command is one of a session's for, on a runner that takes those on the session's own command streams
/// (`CAP_SESSION_STREAMS`; runner-fan): what it tells a session there, in the session's order, on the loop of the shard that
/// owns it, waiting for no other session's. A session's opening and a package to go on from are the process's: they make
/// the session there, and what's for it comes once they're answered.
fn session_lane(msg: &ServerMsg) -> Option<&str> {
    match msg {
        ServerMsg::HarnessCmd { session, .. }
        | ServerMsg::HarnessClose { session }
        | ServerMsg::HarnessFile { session, .. }
        | ServerMsg::HarnessRelease { session }
        | ServerMsg::StreamOpen { session, .. }
        | ServerMsg::StreamControl { session, .. }
        | ServerMsg::StreamInput { session, .. }
        | ServerMsg::StreamClose { session, .. } => Some(session),
        _ => None,
    }
}

/// Queues command `body` (a `what`, riding `stream`; for agent session `session` where it's one of a session's:
/// [`session_lane`]) to runner processes `peers` of machine `agent` (shown as `shown`), run by the commands' owner on its
/// commands: on its command stream for one that keeps the channel, else handed to `legacy` (which says whether it went).
#[allow(clippy::too_many_arguments)]
fn queue_on(c: &mut crate::wire::commands::Commands, agent: &str, shown: &str, what: &'static str, peers: Vec<PeerId>, stream: Option<crate::wire::commands::Kind>, session: Option<&str>, body: &[u8], mut legacy: impl FnMut(PeerId, Vec<u8>) -> bool) -> Pending {
    use crate::wire::commands::{Answer, Kind as Stream};
    if peers.is_empty() {
        return Pending { agent: agent.to_owned(), shown: shown.to_owned(), what, waits: Vec::new(), at_once: Answer::Unreachable { why: format!("{shown} is not connected") }, away: false };
    }
    let mut waits = Vec::new();
    // Answers a process gave at once (away, or with too much waiting for it already): what the
    // command is now when no other process takes it, so a caller sees it (and puts a leg back
    // in the queue) rather than wait on an answer that came and went.
    let mut given = Vec::new();
    let mut sent = false;
    let mut away = stream == Some(Stream::Parked);
    for p in peers {
        match stream.filter(|_| c.keeps(p)) {
            Some(kind) => {
                away &= c.is_away(p);
                let mut rx = c.push(p, kind, session, body.to_vec(), Instant::now());
                match rx.try_recv() {
                    Ok(Some(a)) => given.push(a),
                    _ => waits.push(rx),
                }
            }
            None => {
                away = false;
                sent |= legacy(p, body.to_vec());
            }
        }
    }
    let first = if sent {
        Answer::Sent
    } else if !waits.is_empty() {
        Answer::Taken
    } else {
        given.first().cloned().unwrap_or_else(|| Answer::Unreachable { why: format!("{shown} is not connected") })
    };
    // Beside processes still to answer, those that answered at once count with them.
    if !waits.is_empty() {
        for a in given {
            let (tx, rx) = futures::channel::oneshot::channel();
            let _ = tx.send(a);
            waits.push(rx);
        }
    }
    Pending { agent: agent.to_owned(), shown: shown.to_owned(), what, away: away && !waits.is_empty(), waits, at_once: first }
}

/// A question asked of a runner, waiting for its answer (by its `req`): what reads the answer as
/// the type its asker wants (`crate::wire::payload`) and hands it over. An answer it can't read is
/// refused to the runner that sent it: its error says why.
type Asked = Box<dyn FnOnce(&'static str, Option<String>) -> Result<(), Box<crate::wire::payload::Unreadable>> + Send + Sync>;

/// What reads a question's answer as a `T`, and what its asker waits on: the answer, or why it
/// couldn't be read.
fn asked<T: serde::de::DeserializeOwned + Serialize + Send + 'static>() -> (Asked, futures::channel::oneshot::Receiver<Result<Option<T>, String>>) {
    let (tx, rx) = futures::channel::oneshot::channel();
    let read: Asked = Box::new(move |of, json| {
        let read = json.map(|j| crate::wire::payload::<T>(Wire::RunnerPayload, of, &j)).transpose();
        let refused = read.as_ref().err().cloned();
        let _ = tx.send(read.map_err(|e| e.why));
        refused.map_or(Ok(()), Err)
    });
    (read, rx)
}

/// What the asker of a question of `machine`'s gets of it: its answer; or that it wasn't sent
/// (`sent` false), didn't come in time (`None`), or couldn't be read.
fn answered<T>(machine: &str, sent: bool, answer: Option<Result<Result<Option<T>, String>, futures::channel::oneshot::Canceled>>) -> anyhow::Result<Option<T>> {
    match answer {
        Some(Ok(Ok(v))) => Ok(v),
        Some(Ok(Err(why))) => anyhow::bail!("{machine}'s answer couldn't be read: {why}"),
        Some(Err(_)) => anyhow::bail!("{machine} did not answer"),
        None if !sent => anyhow::bail!("{machine} is not connected"),
        None => anyhow::bail!("{machine} did not answer in time"),
    }
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct AgentLive {
    pub name: String,
    /// The name people see, for an agent kiln knows by its machine id (its `name`,
    /// docs/machines.md); none for one known by its name.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub label: Option<String>,
    #[serde(skip)]
    pub peer: Option<PeerId>,
    pub labels: Vec<String>,
    pub slots: u32,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub version: String,
    pub ephemeral: bool,
    pub connected: bool,
    pub last_seen: i64,
    pub enrolled_at: i64,
    pub running: Vec<String>,
    pub busy: u32,
    pub draining: bool,
    pub sample: Option<HostSample>,
    /// Its battery, when it has one and its runner says ([`AgentMsg::Power`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub battery: Option<crate::os::battery::Battery>,
    /// What its QUIC layer let go before the wire, by lane and reason, as it last said
    /// ([`AgentMsg::NetDrops`]); and what the server's let go of what it had for it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(test, ts(as = "Option<Vec<proto::NetDrop>>", optional))]
    pub dropped: Vec<proto::NetDrop>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(test, ts(as = "Option<Vec<proto::NetDrop>>", optional))]
    pub dropped_toward: Vec<proto::NetDrop>,
    /// What of its messages the server couldn't read, and of the server's it couldn't
    /// (`crate::wire::unread`): how many by wire and code, and the newest.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub refused: Option<crate::wire::unread::Kept>,
    pub terminal: bool,
    /// An outpost (`kiln outpost`, [`crate::proto::CAP_OUTPOST`]): kiln run where it isn't
    /// installed. It takes no jobs and the server doesn't upgrade it.
    pub outpost: bool,
    #[serde(skip)]
    pub dedicated_job: Option<String>,
    #[serde(skip)]
    pub exclusive: bool,
    /// The shares of it its exclusive jobs and legs hold, by job (docs/ci.md W05-R03): their CPUs
    /// and memory are theirs alone, other work runs in the rest.
    #[serde(skip)]
    pub held: Vec<(String, kiln_ci::sched::state::Resources)>,
    #[serde(skip)]
    pub lost_at: Option<Instant>,
    /// What the runner's probes found and its administrator declared (docs/ci.md W13-R01, R02).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub facts: Option<Box<kiln_ci::facts::RunnerFacts>>,
    /// The machine's runner processes, while it has more than one (a runner replaced without a
    /// restart: the old one keeps its terminals until they end). See `crate::instances`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(test, ts(as = "Option<Vec<crate::instances::InstanceView>>", optional))]
    pub instances: Vec<crate::instances::InstanceView>,
    /// How its connection reaches the server, while connected (`GET /api/agents`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub transport: Option<crate::tunnel::Transport>,
    /// How far its (newest process's) kiln is behind the server's (`crate::skew`): to be upgraded,
    /// it takes no new work.
    pub skew: crate::skew::Skew,
    /// A record that can't be proven to be any machine (docs/machines.md, "Records that are stale"):
    /// not connected, known by a name, and its runner never said its machine id. kiln neither
    /// folds nor forgets it on its own; it's shown as stale, for a person to forget.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    #[cfg_attr(test, ts(as = "Option<bool>", optional))]
    pub stale: bool,
}

impl AgentLive {
    /// The name people see (docs/machines.md): its label, else its name. What the sentences kiln
    /// writes for people say of it; what's looked up by, its name.
    pub fn shown(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.name)
    }
}

/// The names people see of the runners kiln knows by their machine ids, by name (docs/machines.md):
/// [`Hub`]'s, shared with what names machines away from it (repo sync's activity). Read wherever kiln
/// names a runner to a person, written as one is renamed: a copy swapped in whole (`crate::rcu`).
pub type Labels = Arc<crate::rcu::Rcu<HashMap<String, String>>>;

/// Output from a remote (agent) terminal session, routed to its websocket.
#[derive(Debug)]
pub enum TermOut {
    Data(Vec<u8>),
    /// The agent's replay for an adopted session.
    Replay(Vec<u8>),
    Exit { code: Option<i32>, error: Option<String> },
    /// The machine disconnected; its shells keep running there.
    Gone,
}

/// A runner's frame on its way to being taken (goal 10, plan 01a1a87b2dae step 4): when it came off the wire
/// and when the hub's loop took it. Handed down with it, and taken by what times it elsewhere: an agent
/// session's update, by the shard that owns its copy (`Hub::runner_update`).
#[derive(Clone, Copy)]
struct Arrived {
    at: Instant,
    taken: Instant,
}

pub struct Hub {
    pub cfg: Arc<Config>,
    pub store: Store,
    /// The records kiln keeps through the Records seam (docs/seams.md): kiln's own, in its store,
    /// until Amphora's (CP-T1.5) take their place. Deploying by API's are there.
    pub records: Arc<dyn kiln_seams::Records>,
    pub logs: Logs,
    pub net: NetHandle,
    pub pki: Arc<crate::pki::Pki>,
    /// Every runner's live record, by name: a copy read without a lock, each record changed by
    /// swapping a new copy of it in with the rest ([`Hub::change_agent`]; goal 5, plan 01a1a59c2aae).
    pub agents: crate::rcu::Rcu<HashMap<String, Arc<AgentLive>>>,
    /// The labels of the runners kiln knows by their machine id, by name, as the store keeps them
    /// (docs/machines.md): what [`Hub::shown`] says of one that hasn't connected since this server
    /// started, and repo sync of the machines its activity names. Loaded as it starts
    /// ([`Hub::load_labels`]), kept as runners are renamed or move.
    labels: Labels,
    /// The names machines' certificates carried before kiln knew them by their ids, each to its
    /// machine id (docs/machines.md, "One record a machine"): resolved where a runner names itself
    /// ([`Hub::machine_of`]), so nothing in the server keys a runner by one. Loaded as it starts
    /// ([`Hub::one_record_a_machine`]), kept as runners move.
    machine_names: crate::rcu::Rcu<HashMap<String, String>>,
    /// The machine id each connection's runner said before its Hello ([`AgentMsg::Machine`]). By link: the net thread's own
    /// numbers for its connections, no peer's to choose ([`crate::util::FastMap`]), as [`Hub::peers`]'s.
    said_machine: crate::rcu::Rcu<crate::util::FastMap<PeerId, String>>,
    /// The name each connection's certificate carries, where it isn't the machine's id it's keyed
    /// by (a runner process from before its certificate for the id): what that process signs and
    /// checks its custody words as, until it restarts with its certificate for the id.
    pub(crate) peer_certs: crate::rcu::Rcu<crate::util::FastMap<PeerId, String>>,
    /// The CI engine's dispatcher (docs/ci.md W05): its runs, legs and their messages.
    pub ci: crate::ci::dispatch::Handle,
    /// Each connected runner process's machine, by its link (read on every message a runner sends:
    /// a copy swapped in whole as runners come and go, `crate::rcu`). Links are the net thread's own
    /// numbers for its connections, no peer's to choose: a [`crate::util::FastMap`] (goal 9, plan
    /// 01a1a87b2cec step 5: a SipHash of the link on every frame).
    peers: crate::rcu::Rcu<crate::util::FastMap<PeerId, String>>,
    pending_peers: crate::rcu::Rcu<crate::util::FastMap<PeerId, String>>,
    /// Runners' HTTP tunnels, and which QUIC connections they carry (see `crate::tunnel`).
    pub tunnels: Arc<crate::tunnel::Registry>,
    /// What kiln's own listeners hear (`GET /api/events` sockets read [`Hub::feed`]), each event as its JSON
    /// text, but agent sessions' own ([`Hub::harness_events`]): runs, jobs, runners, samples, the queue and the rest, sent from
    /// the hub's loop, CI and the API's handlers, on every shard. One lane, to each listener's own channel
    /// (`crate::memory::lanes`; goal 4, plan 01a1a59c2a9c step 5, and goal 5's rule): no send or receive takes
    /// a lock two shards share, as a ring's did (async-broadcast's `Mutex`), and every listener has the events
    /// in the order they were sent, as that lock made it. Each listener holds at most [`EVENTS`] of them
    /// ([`Hub::subscribe_events`]) and one receive window of their texts, a listener that falls behind starting
    /// over on its own (goal 11, plan 01a1a87b2e56 step 3: `crate::memory::owners::UI_EVENTS`).
    pub events: crate::memory::lanes::Lanes<Arc<str>>,
    /// Agent sessions' own events (`harness`: an info changed, `harness_closed`: a session gone), apart from
    /// [`Hub::events`] (goal 4, plan 01a1a59c2a9c step 5). Every change of a session's info is one, sent from
    /// the session's owner shard: about 1,700 a second in the lab's many-sessions profile. Only the
    /// notifications' listener (`crate::push`) and `agents.wait` read them (`GET /api/events` sockets, the
    /// feed), so the rest of kiln's listeners aren't woken for each. A session's go on its owner shard's own lane (the lane
    /// its id hashes to, `crate::harness::copies::owner_of`), to each reader's own channel for that lane, so
    /// no send or receive of them takes a lock two shards share (`crate::memory::lanes`; goal 5's rule): a
    /// session's reach every reader in the order they're sent. Each reader holds one receive window of their
    /// texts at the most, counted as the hub's events' are (`UI_EVENTS`).
    pub harness_events: crate::memory::lanes::Lanes<Arc<str>>,
    /// The machines' low batteries (`machine.battery_low`), on a lane of their own beside [`Hub::events`], which has them
    /// too: what the notifications' listener (`crate::push`) reads of the hub's own events, so every other event doesn't
    /// wake it to be passed over (goal 9, plan 01a1a87b2cec step 5: about 30 of its 71 wakes a second in a Mac's lab).
    pub battery_events: crate::memory::lanes::Lanes<Arc<str>>,
    /// Every frame of both, numbered and kept for the events socket's pages, which resume from them
    /// (`crate::wire::events::feed`, W21-R15): the sockets read this, kiln's own listeners the lanes. Pushed
    /// from every shard that emits (a session's change from its owner's, about 1,700 a second in the
    /// many-sessions profile) and read by every socket's, with no lock on either path (goal 5's rule).
    pub feed: crate::wire::events::feed::Feed,
    /// Each remote terminal's output, by `machine/session`: read on every frame a terminal sends,
    /// written as one opens and closes (`crate::rcu`). Each relay's queue is lock-free (goal 4, plan
    /// 01a1a59c2a9c step 5: `crate::termsess`'s `relay`).
    pub terms: crate::rcu::Rcu<HashMap<String, crate::memory::queue::Sender<TermOut>>>,
    /// Every terminal session, local and on machines.
    pub terminals: crate::termsess::Sessions,
    /// Every kiln-native agent session, local and on machines.
    pub harness: crate::harness::registry::Registry,
    /// What the server has of each runner's channel streams (docs/wire.md): the control loop's, which takes
    /// terminals' streams, and the keep flags. A session's streams are its owner's shard's session loop's
    /// ([`Hub::session_wire`]).
    pub wire: crate::wire::link::Links,
    /// What the server has of runners' agent sessions' channel streams, a `Links` for each shard's session loop
    /// (goal 10, plan 01a1a87b2dae step 4; `crate::wire::fan`): a session's streams are taken by the loop on its
    /// owner's shard, with these receivers of its own, whose lock that loop alone takes ([`Hub::links_of`]).
    session_wire: Box<[crate::wire::link::Links]>,
    /// Commands to each runner process on the channel, until it answers them (docs/wire.md,
    /// "Commands").
    pub cmds: crate::wire::commands::Owner,
    /// The process each machine that's away last had: its parked commands wait for it there.
    away_peers: crate::rcu::Rcu<HashMap<String, PeerId>>,
    /// Commands queued where nothing could wait to send them ([`Hub::command_soon`]): the commands
    /// loop sends them now rather than on its next tick.
    commands_due: Notify,
    /// The AI coding sessions each connected machine last reported.
    pub ai_sessions: crate::rcu::Rcu<HashMap<String, Arc<Vec<crate::aisessions::AiSession>>>>,
    /// Questions asked of runners (transcripts, views, folders, a doctor's look) waiting for their
    /// answers, by request number: each read as its asker wants it.
    ai_requests: crate::rcu::Pending<u64, Asked>,
    ai_seq: AtomicU64,
    /// File reads (`crate::harness::files`) waiting for their machine's answer: what it says of the
    /// file, and the bytes.
    file_requests: crate::rcu::Pending<u64, futures::channel::oneshot::Sender<Result<(crate::harness::files::Answer, Vec<u8>), String>>>,
    /// Each machine's recent samples (the server's too): read by the overview, written a sample at a
    /// time, each machine's own copy swapped in with it (`crate::rcu`).
    pub history: crate::rcu::Rcu<HashMap<String, Arc<VecDeque<HostSample>>>>,
    pub wake: Notify,
    pub launching: crate::rcu::Rcu<HashMap<String, Launch>>,
    pub started: Instant,
    pub generation: u64,
    /// Runners connected just before this server started ([`crate::store::Store::runners_before`]).
    pub runners_before: AtomicU64,
    /// How long a runner is given to come back, learned from how long runners took to ([`crate::comeback`]).
    pub comeback: crate::comeback::Comeback,
    pub admin_token: String,
    pub join_token: String,
    /// One-time enrollment tokens for pod agents: token → (job id, expiry).
    pub ephemeral_tokens: crate::rcu::Rcu<HashMap<String, (String, Instant)>>,
    /// One-time enrollment tokens for a `k8s` pool's executor pods (docs/ci.md W13-R07): token →
    /// (executor, the pod's name, which the runner must take, expiry).
    pub executor_tokens: crate::rcu::Rcu<HashMap<String, (String, String, Instant)>>,
    /// The executor each executor pod's runner is, by the name it enrolled with.
    pub pod_executors: crate::rcu::Rcu<HashMap<String, String>>,
    /// One-time outpost invites (`POST /api/outposts/invite`).
    pub outpost_invites: OutpostInvites,
    pub k8s: std::sync::OnceLock<Option<Arc<crate::k8s::K8s>>>,
    /// Wakes the repository poller ([`crate::gitops::poll_loop`]) to look at what's due: what a
    /// kick names ([`Hub::gitops_wanted`], [`crate::gitops::want`]) and what its interval says.
    pub gitops_kick: Notify,
    /// Repositories (by id) asked to be polled now: a push, a webhook, a new repository.
    pub gitops_wanted: crate::rcu::Rcu<HashSet<String>>,
    /// Trunk moves whose open changes are to be replayed onto them (W26-R16, [`crate::ci::fresh`]).
    pub fresh: crate::ci::fresh::Queue,
    /// Agents installing what jobs lack, by job ([`crate::toolfix`]).
    pub tool_fixes: crate::rcu::Rcu<HashMap<String, crate::toolfix::Fix>>,
    pub shutdown: Shutdown,
    /// Time, as the goal driver and what else moves onto it take it: kiln's runtime in production,
    /// a simulated clock in tests (kiln-seams' `Clock`, docs/seams.md).
    pub clock: Arc<dyn kiln_seams::Clock>,
    /// What the wires couldn't read (`crate::wire::unread`): counted, kept for each runner's and
    /// agent session's page, audited.
    pub refusals: crate::rcu::Rcu<crate::wire::unread::Refusals>,
    /// What the hub sent each runner process on its control lane, in order: for tests to read.
    #[cfg(test)]
    pub(crate) sent: Mutex<Vec<(PeerId, ServerMsg)>>,
    /// The jobs paused in this server's life (docs/ci.md W26-R22), and those a restart found paused:
    /// how long each was paused before, and since when it is now (unix seconds). A paused leg's
    /// token lives that while longer ([`Hub::paused_for`]). Kept for the server's life: a person
    /// pauses few jobs.
    paused: crate::rcu::Rcu<HashMap<String, (i64, Option<i64>)>>,
    /// Agents that have been sent Cancel for a job, with when (for the timeout escalation).
    cancel_sent: crate::rcu::Rcu<HashMap<String, Instant>>,
    /// (job, agent) pairs recently declined: not offered again until DECLINE_BACKOFF passes.
    declined: crate::rcu::Rcu<HashMap<(String, String), Instant>>,
    /// Multicast DNS discovery: what this server announces and the kiln instances it heard.
    pub discovery: Arc<crate::discovery::Discovery>,
    /// Machines asking to join as agents (`kiln join`), waiting for an admin.
    pub joins: crate::join::Requests,
    /// Each machine's runner processes and what each one holds (see `crate::instances`): a copy read
    /// without a lock, changed by swapping a new one in (goal 5, plan 01a1a59c2aae).
    pub instances: crate::rcu::Rcu<crate::instances::Instances>,
    /// Runner processes that went away while their machine stayed connected through another
    /// one, with when and the jobs they held (requeued if they do not come back in time).
    lost_instances: crate::rcu::Rcu<HashMap<PeerId, (String, Instant, Vec<String>)>>,
    /// Agent sessions on their way from a machine's older runner processes to its newest
    /// (`crate::handovers`).
    pub(crate) handovers: Mutex<crate::handovers::Handovers>,
    /// Upgrades of runners and of this server, started from System → Updates.
    pub updates: crate::updates::Updates,
    /// Runners' front doors: their settings and what each last reported (`crate::frontdoor`).
    pub front_doors: crate::frontdoor::control::FrontDoors,
    /// The browser protocol over one WebSocket (`crate::live`, goal 3 of the browser protocol
    /// program): its tickets and its topics' owner, when `[server] live` serves it.
    pub live: crate::live::Live,
    /// How many agent sessions each machine runs at once, as set here, and what each said it
    /// holds (`crate::harness::limit`).
    pub agent_limits: crate::harness::limit::AgentLimits,
    /// Each runner's headroom settings and what it reports (docs/headroom.md).
    pub headrooms: crate::headroom::control::Headrooms,
    /// Peered clusters (`crate::peers`).
    pub peered: crate::peers::Peers,
    /// The tailnet's split DNS for the front doors' names (`crate::frontdoor::tailnet`).
    pub tailnet: crate::frontdoor::tailnet::TailnetDns,
    /// Repo sync: every synced repo's group, led from here (`crate::sync`).
    pub sync: crate::sync::server::Sync,
    /// Reach: the links between runners this server relays, and their identities (`crate::reach`):
    /// one owner changes them, the hub's `relay_loop`, started on first use ([`Hub::relay`]).
    pub reach: crate::reach::relay::Owner,
    /// What kiln takes from relays and relays itself, checked and numbered (docs/ci.md W27-R08's
    /// relays: `crate::relays`).
    pub(crate) relays: crate::relays::Relays,
    /// Runners' custody records on their way off the net loop to the one task that reads, checks and
    /// keeps them in the order they came, started on first use ([`Hub::custody_intake`]; goal 9, plan
    /// 01a1a87b2cec step 3).
    pub(crate) custody_in: crate::custody::runners::Intake,
    /// The copies of runners' trees other runners keep: the rules for them, and what each runner
    /// said it holds and let go (`crate::reach::copies`, docs/reach.md §5.1).
    pub copies: crate::reach::copies::Copies,
    /// The git repositories each machine holds, as it last said (`crate::sync::places`).
    pub places: crate::sync::places::Places,
    /// Clones, syncs and moves of repositories between machines (`crate::sync::tasks`).
    pub tasks: crate::sync::tasks::Tasks,
    /// Work kept in step between the machines that hold a repository (`crate::sync::share`).
    pub shares: crate::sync::share::Shares,
    /// The runners' learners of the cluster group (docs/p2p.md, P2).
    pub cluster: crate::store::learner::Learners,
    /// Notifications to people's devices (`crate::push`).
    pub push: crate::push::Service,
    /// Where kiln keeps its bytes (`crate::blobs`, docs/blobs.md): the server's data directory, or
    /// any S3.
    pub blobs: crate::blobs::Opened,
    /// The bus kiln's events ride: its sessions' live feeds, and its CI and deploy events on the
    /// persisted class (docs/bus.md).
    pub bus: crate::bus::QuiverBus,
    /// The search across a change (docs/ci.md W26-R07): vorpal, its indexes under the server's
    /// cache (`crate::ci::search`).
    pub search: Arc<dyn kiln_seams::Search>,
    /// Memo steps in flight, one execution per key and domain (docs/ci.md W28-R72).
    pub flight: crate::ci::services::memo::Flights,
    /// What wakes the relay of kiln's events from their outbox to the bus (`crate::events`).
    pub relay: crate::events::Relay,
}

impl Hub {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: Arc<Config>,
        store: Store,
        logs: Logs,
        net: NetHandle,
        pki: Arc<crate::pki::Pki>,
        admin_token: String,
        join_token: String,
        shutdown: Shutdown,
    ) -> anyhow::Result<Arc<Self>> {
        Self::new_on(crate::rt::clock(), cfg, store, logs, net, pki, admin_token, join_token, shutdown)
    }

    /// A hub whose time is `clock`'s.
    #[allow(clippy::too_many_arguments)]
    pub fn new_on(
        clock: Arc<dyn kiln_seams::Clock>,
        cfg: Arc<Config>,
        store: Store,
        logs: Logs,
        net: NetHandle,
        pki: Arc<crate::pki::Pki>,
        admin_token: String,
        join_token: String,
        shutdown: Shutdown,
    ) -> anyhow::Result<Arc<Self>> {
        // The bus every session's live feed rides, and kiln's CI and deploy events (docs/bus.md):
        // Quiver's node, in this process, the one node of a cluster of one for the persisted class.
        let bus = crate::bus::QuiverBus::open_server(&cfg.bus, &cfg.bus.data_in(&cfg.server.data_dir), Arc::clone(&clock))?;
        Self::new_with_bus(clock, cfg, store, logs, net, pki, admin_token, join_token, shutdown, bus)
    }

    /// A hub on `bus`, opened by its maker: the server's with the persisted class
    /// ([`Hub::new_on`]), a test's with what the test needs.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_bus(
        clock: Arc<dyn kiln_seams::Clock>,
        cfg: Arc<Config>,
        store: Store,
        logs: Logs,
        net: NetHandle,
        pki: Arc<crate::pki::Pki>,
        admin_token: String,
        join_token: String,
        shutdown: Shutdown,
        bus: crate::bus::QuiverBus,
    ) -> anyhow::Result<Arc<Self>> {
        let mut blobs = crate::blobs::open(&cfg, &store, Arc::clone(&clock))?;
        // What it keeps there and reads back, recorded as it changes hands (docs/ci.md W27-R08).
        blobs.blobs = Arc::new(crate::custody::blobs::Attested::new(Arc::clone(&blobs.blobs), blobs.kind.as_str(), store.clone(), Arc::clone(&pki)));
        let generation =
            std::env::var("KILN_GENERATION").ok().and_then(|g| g.parse().ok()).unwrap_or(1);
        let discovery = Arc::new(crate::discovery::Discovery::for_server(&cfg, &pki.ca_der));
        let harness_dir = cfg.data("harness");
        // The search across a change (W26-R07): an index per tree in the server's cache.
        let search: Arc<dyn kiln_seams::Search> = Arc::new(kiln_search::VorpalSearch::open(cfg.data("cache").join("search"), kiln_search::Limits::default()).map_err(anyhow::Error::msg)?);
        let harness = crate::harness::registry::Registry::with(harness_dir, &cfg.conversations, Arc::clone(&clock), Arc::new(bus.clone()));
        let contents = crate::sync::contents::Contents::start(&blobs, cfg.data("sync").join("blobs"));
        let labels = Labels::default();
        let sync = crate::sync::server::Sync::start(cfg.data("sync"), admin_token.clone(), contents, Arc::clone(&labels));
        let places = crate::sync::places::Places::open(cfg.data("sync").join("places.json"));
        let tasks = crate::sync::tasks::Tasks::open(&cfg.data("sync"));
        let copies = crate::reach::copies::Copies::open(cfg.data("reach").join("copies.json"));
        let records: Arc<dyn kiln_seams::Records> = Arc::new(crate::records::StoreRecords::open(store.clone())?);
        let live = crate::live::Live::new(&cfg.server);
        let hub = Arc::new(Self {
            cfg,
            store,
            records,
            logs,
            net,
            pki,
            agents: crate::rcu::Rcu::default(),
            labels,
            machine_names: Default::default(),
            said_machine: Default::default(),
            peer_certs: Default::default(),
            ci: crate::ci::dispatch::Handle::default(),
            peers: Default::default(),
            pending_peers: Default::default(),
            tunnels: Arc::default(),
            events: crate::memory::lanes::Lanes::new(&crate::memory::owners::UI_EVENTS, 1, crate::net::EVENTS_BYTES),
            harness_events: crate::memory::lanes::Lanes::new(&crate::memory::owners::UI_EVENTS, crate::rt::shard_count().max(1), crate::net::EVENTS_BYTES),
            battery_events: crate::memory::lanes::Lanes::new(&crate::memory::owners::UI_EVENTS, 1, crate::net::EVENTS_BYTES),
            // This server process's: a page's place in another's feed isn't taken (53 bits, as JSON
            // carries a number exactly).
            feed: crate::wire::events::feed::Feed::new(rand::random::<u64>() >> 11),
            terms: Default::default(),
            terminals: crate::termsess::Sessions::new(),
            harness,
            wire: crate::wire::link::Links::new(),
            session_wire: (0..crate::wire::fan::shards()).map(|_| crate::wire::link::Links::new()).collect(),
            // Named for this server process (53 bits, as JSON carries a number exactly), so a runner
            // never mistakes another's commands for these.
            cmds: crate::wire::commands::Owner::start(crate::wire::commands::Commands::new(rand::random::<u64>() >> 11)),
            away_peers: Default::default(),
            commands_due: Notify::new(),
            ai_sessions: Default::default(),
            ai_requests: Default::default(),
            ai_seq: AtomicU64::new(1),
            file_requests: Default::default(),
            history: Default::default(),
            wake: Notify::new(),
            launching: Default::default(),
            started: Instant::now(),
            generation,
            runners_before: AtomicU64::new(0),
            comeback: crate::comeback::Comeback::default(),
            admin_token,
            join_token,
            ephemeral_tokens: Default::default(),
            executor_tokens: Default::default(),
            pod_executors: Default::default(),
            outpost_invites: OutpostInvites::default(),
            k8s: std::sync::OnceLock::new(),
            gitops_kick: Notify::new(),
            gitops_wanted: Default::default(),
            fresh: crate::ci::fresh::Queue::default(),
            tool_fixes: Default::default(),
            shutdown,
            refusals: crate::rcu::Rcu::new(crate::wire::unread::Refusals::new(Arc::clone(&clock))),
            #[cfg(test)]
            sent: Mutex::new(Vec::new()),
            paused: Default::default(),
            clock,
            cancel_sent: Default::default(),
            declined: Default::default(),
            discovery,
            joins: crate::join::Requests::default(),
            instances: crate::rcu::Rcu::default(),
            lost_instances: Default::default(),
            handovers: Mutex::new(crate::handovers::Handovers::default()),
            updates: crate::updates::Updates::default(),
            front_doors: crate::frontdoor::control::FrontDoors::default(),
            live,
            agent_limits: crate::harness::limit::AgentLimits::default(),
            headrooms: crate::headroom::control::Headrooms::default(),
            peered: crate::peers::Peers::default(),
            tailnet: crate::frontdoor::tailnet::TailnetDns::default(),
            sync,
            reach: crate::reach::relay::Owner::default(),
            relays: crate::relays::Relays::default(),
            custody_in: crate::custody::runners::Intake::default(),
            copies,
            places,
            tasks,
            shares: crate::sync::share::Shares::default(),
            cluster: crate::store::learner::Learners::default(),
            push: crate::push::Service::default(),
            blobs,
            bus,
            search,
            flight: Default::default(),
            relay: crate::events::Relay::default(),
        });
        // What each agent session's owner shard tells the rest of kiln goes through the hub
        // (`crate::harness::copies`).
        hub.harness.bind(&hub);
        Ok(hub)
    }

    /// A job of kiln's own release pipeline succeeded (`[updates]`), a v1 job or a leg of the CI
    /// engine's run: the binaries it built are what each other platform's runners upgrade to now.
    pub(crate) async fn adopt_release(&self, job: &str, run: &crate::store::Run) {
        // The engine's run of a tag is a `push` of it.
        let tagged;
        let run = if run.event == "push" && run.branch != self.cfg.updates.release_branch && crate::updates::pushed_tag(self, &run.id).await {
            tagged = crate::store::Run { event: "tag".into(), ..run.clone() };
            &tagged
        } else {
            run
        };
        // The server's commit, or the newest one a runner runs (`crate::fleet`).
        let newest = self.updates.fleet.target().map(|t| t.commit).unwrap_or_default();
        if !crate::updates::adopts(&self.cfg.updates, run, crate::api::COMMIT) && (newest.is_empty() || !crate::updates::adopts(&self.cfg.updates, run, &newest)) {
            return;
        }
        // v1's job keeps its artifacts as a tarball; the engine's leg uploaded them to CI's store.
        let v1 = crate::api::artifact_path(self, job);
        let (path, copied) = if v1.exists() {
            (v1, false)
        } else {
            match crate::updates::leg_artifacts(self, job).await {
                Ok(Some(p)) => (p, true),
                Ok(None) => return,
                Err(e) => return tracing::warn!(run = run.number, leg = job, error = %format!("{e:#}"), "the release pipeline's artifacts couldn't be read"),
            }
        };
        let adopted = crate::updates::adopt_into(self, &path, run).await;
        if copied {
            let _ = std::fs::remove_file(&path);
        }
        match adopted {
            Ok(kept) if !kept.is_empty() => {
                let targets: Vec<&str> = kept.iter().map(|b| b.target.as_str()).collect();
                tracing::info!(run = run.number, targets = ?targets, "runners' binaries built by the release pipeline");
                self.store.audit("kiln".into(), "upgrade.binary".into(), format!("built by {} run #{} ({}): {}", run.pipeline, run.number, run.sha.get(..12).unwrap_or(&run.sha), targets.join(", "))).await;
                self.emit(&ServerEvent::Updates);
                self.updates.fleet.wake();
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(run = run.number, error = %format!("{e:#}"), "the release pipeline's binaries were not kept"),
        }
    }

    /// Sends what repo sync says to each machine's syncing runner process, on the sync lane.
    pub async fn sync_loop(self: Arc<Self>) -> anyhow::Result<()> {
        let out = self.sync.outgoing();
        while let Ok(o) = out.recv().await {
            let peer = self.instances.load().newest_with(&o.machine, proto::CAP_SYNC);
            let Some(peer) = peer else { continue };
            let msg = ServerMsg::Sync { json: crate::sync::wire::encode(&o.msg), raft: o.raft };
            if let Ok(b) = proto::encode(&msg) {
                self.net.send(peer, proto::LANE_SYNC, Kind::Control, b).await;
            }
        }
        Ok(())
    }

    /// Sends the cluster group's messages to the runners' learners (the newest process of each
    /// machine that holds one), on the sync lane; a learner whose runner is away misses them.
    pub async fn cluster_loop(self: Arc<Self>) -> anyhow::Result<()> {
        let Some(out) = self.store.group().map(crate::store::group::Group::outgoing) else { return Ok(()) };
        // What every whole message says of itself, written once (goal 9, plan 01a1a87b2cec step 5: once a frame).
        let whole_json = crate::store::learner::encode(&crate::store::learner::Msg::Raft { id: 0, part: 0, parts: 0 });
        // Each message's learner found here, its frames (a copy of up to half a mebibyte each, a learner's catch-up
        // many) made on the compute pool and sent in order, in turns (goal 9, plan 01a1a87b2cec step 3: made here
        // a part at a time, a poll still ran 0.3 to 1.4 ms on the CPU in rt9's rounds); a write's few short messages
        // made here where that fits a turn and is no dearer than the pool's round trip, by their bytes at the rate making is learned to take
        // (`crate::net::send_made`, step 5; consts-audit R2).
        let route = |(member, bytes): (u64, Vec<u8>)| match self.cluster.machine(member).and_then(|m| self.instances.load().newest_with(&m, proto::CAP_CLUSTER)) {
            Some(peer) => Some((peer, (bytes.len(), self.cluster.frames(member, bytes)))),
            None => {
                if let Some(g) = self.store.group() {
                    g.unreachable(member);
                }
                None
            }
        };
        let make = move |(_, frames): (usize, crate::store::learner::Frames)| crate::store::learner::encoded(frames, &whole_json);
        crate::net::send_made(&out, route, |(bytes, _)| *bytes, &crate::net::MADE, make, |peer, b| self.net.send(peer, proto::LANE_SYNC, Kind::Control, b)).await;
        Ok(())
    }

    /// A runner process of `machine` that can hold a learner said so, or its newest process changed:
    /// the newest such process holds the machine's (it's told its member and the token its copies
    /// are fetched with), and every other one lets its go, so one process at a time has the
    /// replica's files.
    async fn cluster_announced(&self, machine: &str) {
        // The server's own runner, in its process, reads kiln.db as it is.
        let own = self.cfg.local_agent.enabled && machine == self.cfg.local_agent_key();
        let group = self.store.group().filter(|_| self.cfg.cluster.learners && !own);
        let (newest, mut others) = self.instances.load().holder_of(machine, proto::CAP_CLUSTER);
        // With learners off, the newest lets its replica go too: its front door pipes to the
        // server again rather than wait for a replica that no longer follows.
        if group.is_none() {
            others.extend(newest);
        }
        let say = |m: &crate::store::learner::Msg| proto::encode(&ServerMsg::Cluster { json: crate::store::learner::encode(m), raft: Vec::new() });
        if let Ok(b) = say(&crate::store::learner::Msg::Leave) {
            for p in others {
                self.net.send(p, proto::LANE_SYNC, Kind::Control, b.clone()).await;
            }
        }
        let (Some(g), Some(peer)) = (group, newest) else { return };
        let member = self.cluster.announced(g, machine);
        let token = crate::store::learner::token(&self.admin_token, machine);
        if let Ok(b) = say(&crate::store::learner::Msg::Join { member, token }) {
            self.net.send(peer, proto::LANE_SYNC, Kind::Control, b).await;
        }
    }

    /// The runners that could keep a copy of the conversation store, as they are now: the
    /// mirrors are picked again from them, and fed (`crate::convo::mirror`).
    fn mirrors_seen(&self) {
        let Some(m) = &self.harness.copies else { return };
        // The machines first, then their runner processes.
        let machines: Vec<(String, Option<String>, u64)> = self
            .agents
            .load()
            .values()
            .filter(|a| a.connected && !a.ephemeral && !a.labels.iter().any(|l| l == proto::OUTPOST_LABEL))
            .map(|a| (a.name.clone(), a.label.clone(), a.sample.as_ref().map_or(0, |s| s.disk_total.saturating_sub(s.disk_used))))
            .collect();
        let candidates: Vec<crate::convo::mirror::Candidate> = {
            let inst = self.instances.load();
            machines
                .into_iter()
                .filter_map(|(machine, label, disk_free)| {
                    let peer = inst.newest_with(&machine, proto::CAP_CONVO)?;
                    let connected_at = inst.get(peer).map_or(0, |i| i.connected_at);
                    Some(crate::convo::mirror::Candidate { machine, label, peer, connected_at, disk_free })
                })
                .collect()
        };
        m.connected(candidates);
    }

    /// What a machine's runner said on the sync lane: of its copies (to the groups), of the git
    /// repositories it holds, or of its part of a task.
    fn sync_heard(&self, machine: &str, msg: crate::sync::wire::Msg, raft: Vec<u8>) {
        match msg {
            crate::sync::wire::Msg::Repos(r) => {
                if self.places.heard(machine, r) {
                    self.emit(&ServerEvent::Places);
                }
                // Said when it connects, and when what it holds changes: what it shares.
                self.shares.arrange(self);
            }
            crate::sync::wire::Msg::Shared(w) => self.shares.shared(self, machine, *w),
            crate::sync::wire::Msg::Task(news) => self.tasks.heard(machine, news),
            msg => self.sync.from_runner(machine, msg, raft),
        }
    }

    /// A runner process of `machine` that syncs said so: the newest such process syncs, and every
    /// other one lets go.
    async fn sync_announced(&self, machine: &str) {
        let (newest, others) = {
            let inst = self.instances.load();
            let newest = inst.newest_with(machine, proto::CAP_SYNC);
            let others: Vec<PeerId> = inst.of(machine).into_iter().filter(|p| Some(*p) != newest && inst.get(*p).is_some_and(|i| i.caps & proto::CAP_SYNC != 0)).collect();
            (newest, others)
        };
        let release = ServerMsg::Sync { json: crate::sync::wire::encode(&crate::sync::wire::Msg::Release), raft: Vec::new() };
        if let Ok(b) = proto::encode(&release) {
            for p in others {
                self.net.send(p, proto::LANE_SYNC, Kind::Control, b.clone()).await;
            }
        }
        self.sync.machine(machine, newest.is_some());
    }

    // ------------------------------------------------------------ UI events

    /// Close every runner's connection (server shutdown): runners see the close at once and
    /// reconnect to the next server process, instead of waiting out the QUIC idle timeout.
    pub async fn disconnect_all(&self) {
        let peers: Vec<PeerId> = self.peers.load().keys().chain(self.pending_peers.load().keys()).copied().collect();
        for peer in peers {
            let _ = self.net.command(crate::net::Out::Disconnect { peer }).await;
        }
    }

    /// Tell every UI that the session list changed.
    pub fn emit_terminals(&self) {
        self.emit(&ServerEvent::Terminals);
    }

    /// Sends `e` down every events socket (`GET /api/events`), numbered in the feed they resume from
    /// (W21-R15), and to kiln's own listeners: an agent session's own (its info, its end) on its lane of
    /// [`Hub::harness_events`], the rest on [`Hub::events`]. One text, shared by the feed and the lanes.
    pub fn emit(&self, e: &ServerEvent) {
        // The browser protocol's topics hear every event once, whoever listens on the events socket.
        self.live.event(e);
        let text: Arc<str> = Arc::from(e.text());
        self.feed.push(Arc::clone(&text));
        // What the viewers hear of mostly follows a change whose event its trigger recorded; and
        // what outgoing webhooks take of it besides is recorded (crate::events).
        self.relay.heard(&text);
        self.relay.kick();
        match e {
            ServerEvent::Harness { info } => self.emit_session(&info.id, text),
            ServerEvent::HarnessClosed { id } => self.emit_session(id, text),
            _ => {
                if matches!(e, ServerEvent::MachineBatteryLow { .. }) && self.battery_events.receiver_count() > 0 {
                    self.battery_events.send(0, Arc::clone(&text));
                }
                if self.events.receiver_count() > 0 {
                    self.events.send(0, text);
                }
            }
        }
    }

    /// [`Hub::emit`] of an event its caller wrote out already ([`ServerEvent::text`]), off the shards: a CI verdict's,
    /// the whole verdict in it, written on the compute pool (goal 9, plan 01a1a87b2cec step 3). Never an agent
    /// session's own ([`ServerEvent::Harness`], [`ServerEvent::HarnessClosed`]), which go on their lanes. Numbered in
    /// the feed the events sockets read (W21-R15), as [`Hub::emit`]'s are: the sockets read no lane.
    pub fn emit_written(&self, text: Arc<str>) {
        self.feed.push(Arc::clone(&text));
        if self.events.receiver_count() > 0 {
            self.events.send(0, text);
        }
    }

    /// A listener of the hub's events ([`Hub::events`]) from now, holding as many as the hub's ring held for
    /// its slowest ([`EVENTS`]): past them it's told how many it lost. Refused when the runtime has no channel
    /// to give it (its cells all in use).
    pub fn subscribe_events(&self) -> Result<crate::memory::lanes::Reader<Arc<str>>, hyper_rt::RtError> {
        self.events.subscribe(EVENTS.div_ceil(self.events.lanes()))
    }

    /// A listener of the machines' low batteries alone ([`Hub::battery_events`]): as many as [`Hub::subscribe_events`]'s
    /// listener holds of every event.
    pub fn subscribe_battery(&self) -> Result<crate::memory::lanes::Reader<Arc<str>>, hyper_rt::RtError> {
        self.battery_events.subscribe(EVENTS.div_ceil(self.battery_events.lanes()))
    }

    /// Agent session `id`'s info changed: [`ServerEvent::Harness`]'s frame, made of `info_json`, the bytes
    /// its copy's view encoded it as, not encoded again (goal 10, plan 01a1a87b2dae step 5), on the session's
    /// lane of [`Hub::harness_events`] (goal 4, plan 01a1a59c2a9c step 5). Made always: the feed keeps it for
    /// the pages that resume (W21-R15). Its text, as the feed lends it ([`crate::wire::events::feed::Lent`]):
    /// held by the session's copy until its next info's frame is in, then let go of, and the feed's readers pass
    /// over this one (goal 11, plan 01a1a87b2e56 step 5), with nothing more put in the feed to say so (goal 9, plan
    /// 01a1a87b2cec step 5).
    pub fn emit_harness(&self, id: &str, info_json: &[u8]) -> crate::wire::events::feed::Lent {
        let frame = crate::wire::events::harness_frame(info_json);
        let lent = self.feed.push_lent(Arc::clone(&frame));
        self.emit_session(id, frame);
        lent
    }

    /// Session `id`'s event `text`, if anyone reads them, on the session's lane: its owner shard's (goal 4,
    /// plan 01a1a59c2a9c step 5).
    fn emit_session(&self, id: &str, text: Arc<str>) {
        if self.harness_events.receiver_count() > 0 {
            self.harness_events.send(crate::harness::copies::owner_of(id, self.harness_events.lanes()), text);
        }
    }

    pub async fn emit_job(&self, job_id: &str) {
        if let Ok(Some(mut j)) = self.store.job(job_id.to_owned()).await {
            j.spec = None;
            self.emit(&ServerEvent::Job { job: Box::new(j) });
        }
    }

    pub async fn emit_run(&self, run_id: &str) {
        if let Ok(Some(r)) = self.store.run(run_id.to_owned()).await {
            self.emit(&ServerEvent::Run { run: Box::new(r) });
        }
    }

    /// A runner's record to the pages, with what's kept of its refusals as `GET /api/agents` has
    /// it: told for anything else, it doesn't say it has none (`crate::wire::unread`).
    pub(crate) fn emit_agent(&self, a: &AgentLive) {
        let mut a = a.clone();
        a.refused = self.refusals.load().kept(&crate::wire::unread::Page::Runner(a.name.clone())).cloned();
        self.emit(&ServerEvent::Agent { agent: Box::new(a) });
    }

    /// Runner `name`'s live record changed by `f`, on a copy swapped in with the rest of the
    /// runners' ([`crate::rcu::Rcu::update`]: `f` may run again, on a newer copy, so it only changes
    /// the record it's given), and what `f` said; none when the runner isn't known.
    pub(crate) fn change_agent<R>(&self, name: &str, mut f: impl FnMut(&mut AgentLive) -> R) -> Option<R> {
        let changed = self.agents.update(|m| m.get_mut(name).map(|a| f(Arc::make_mut(a))));
        // `GET /api/agents` and the overview, kept encoded, are made again (goal 10, plan 01a1a87b2dae step 2).
        self.harness.views.changed();
        changed
    }

    /// Runner `a`'s live record from now on, in place of one by its name.
    pub(crate) fn put_agent(&self, a: AgentLive) {
        let a = Arc::new(a);
        self.agents.update(|m| m.insert(a.name.clone(), Arc::clone(&a)));
        self.harness.views.changed();
    }

    /// Where `msg` for machine `name` goes ([`crate::instances::Instances::route`]): read from the
    /// runner processes' copy, which only a message creating what it's about changes (its holder).
    fn route(&self, name: &str, msg: &ServerMsg) -> Option<PeerId> {
        if crate::instances::Owned::creates(msg) {
            self.instances.update(|i| i.route(name, msg))
        } else {
            self.instances.load().routes(name, msg)
        }
    }

    /// Agent `name` is shown as `label` from now on (docs/machines.md): kept (an agent not known yet
    /// with `hostname`, until its runner connects), and its pages told.
    pub async fn set_label(&self, name: &str, label: &str, hostname: &str) -> anyhow::Result<()> {
        self.store.set_agent_label(name.to_owned(), label.to_owned(), hostname.to_owned()).await?;
        self.labels.update(|m| m.insert(name.to_owned(), label.to_owned()));
        let live = self.change_agent(name, |a| {
            a.label = Some(label.to_owned());
            a.clone()
        });
        if let Some(a) = live {
            self.emit_agent(&a);
        }
        self.note_machine(name);
        Ok(())
    }

    /// The runner people mean by `typed`: the one kiln knows by that name, else the machine a
    /// name its certificate carried from before is, else the one shown by it (its label,
    /// docs/machines.md).
    pub fn machine_named(&self, typed: &str) -> Option<String> {
        let g = self.agents.load();
        if g.contains_key(typed) {
            return Some(typed.to_owned());
        }
        if let Some(id) = self.machine_names.load().get(typed) {
            return Some(id.clone());
        }
        let live = g.values().find(|a| a.label.as_deref() == Some(typed)).map(|a| a.name.clone());
        live.or_else(|| self.labels.load().iter().find(|(_, l)| l.as_str() == typed).map(|(n, _)| n.clone()))
    }

    /// The name people see for runner `name` (docs/machines.md): its label, for one kiln knows by
    /// its machine id, else `name` (one known by its name, or not known here). What the sentences
    /// kiln writes for people say; what's looked up by (routes, rows, JSON keys, logs' fields,
    /// links), `name` itself.
    pub fn shown(&self, name: &str) -> String {
        let live = self.agents.load().get(name).and_then(|a| a.label.clone());
        live.or_else(|| self.labels.load().get(name).cloned()).unwrap_or_else(|| name.to_owned())
    }

    /// The machine a runner that names itself `name` is (docs/machines.md, "One record a machine"):
    /// the id a name its certificate carried from before maps to, else `name` itself. Where a
    /// runner says who it is (its connection, its requests' `?machine=`), this is what it's keyed by.
    pub fn machine_of(&self, name: &str) -> String {
        self.machine_names.load().get(name).cloned().unwrap_or_else(|| name.to_owned())
    }

    /// The names machine `id`'s certificates carried from before.
    pub fn names_of(&self, id: &str) -> Vec<String> {
        let mut v: Vec<String> = self.machine_names.load().iter().filter(|(_, m)| *m == id).map(|(n, _)| n.clone()).collect();
        v.sort();
        v
    }

    /// The runners' labels as the store keeps them, for [`Hub::shown`]: as this server starts.
    pub async fn load_labels(&self) {
        match self.store.agent_labels().await {
            Ok(labels) => self.labels.set(labels),
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "the runners' labels couldn't be read: sentences name them by their ids"),
        }
    }

    /// The labels the metrics give runner `name`'s series ([`crate::metrics::set_machine`]): its
    /// machine's id (the one it's known by, else the one it said) and the name people see.
    fn note_machine(&self, name: &str) {
        let known = self.agents.load().get(name).map(|a| {
            let id = if crate::join::is_machine_key(name) { name.to_owned() } else { a.facts.as_ref().and_then(|f| f.machine_id.clone()).unwrap_or_default() };
            (id, a.label.clone().unwrap_or_else(|| name.to_owned()))
        });
        if let Some((id, shown)) = known {
            crate::metrics::set_machine(name, &id, &shown);
        }
    }

    /// A message a wire couldn't read (`crate::wire::unread`): counted, kept for its sender's page
    /// and `on`'s, audited, and each page it changed told. Its sender isn't told of it here:
    /// [`Hub::note_unread_told`] is for one it is.
    pub async fn note_unread(&self, u: crate::wire::unread::Unread, on: Option<crate::wire::unread::Page>) {
        said_unread(&u, 1);
        let noted = self.refusals.update(|r| r.note(u.clone(), on.clone()));
        self.noted_unread(noted).await;
    }

    /// As [`Hub::note_unread`], for a refusal its sender is told of (`ServerMsg::Unread`). Some:
    /// tell it now, of how many before it it wasn't told; None: not now.
    pub async fn note_unread_told(&self, u: crate::wire::unread::Unread, on: Option<crate::wire::unread::Page>) -> Option<u64> {
        said_unread(&u, 1);
        let noted = self.refusals.update(|r| r.note_told(u.clone(), on.clone()));
        let tell = noted.tell;
        self.noted_unread(noted).await;
        tell
    }

    /// A runner's word of a frame of the server's it couldn't read (`AgentMsg::Unread`), with the
    /// `folded` before it it didn't tell of: noted as that many.
    pub async fn note_unread_folded(&self, u: crate::wire::unread::Unread, on: Option<crate::wire::unread::Page>, folded: u64) {
        said_unread(&u, folded.saturating_add(1));
        let noted = self.refusals.update(|r| r.note_folded(u.clone(), on.clone(), folded));
        self.noted_unread(noted).await;
    }

    /// `noted`'s rows audited and its pages told, each boxed where it's made: what notes a message that couldn't be read
    /// holds a pointer to them, so a runner message's handlers, which may note one, stay small (goal 9, plan 01a1a87b2cec
    /// step 5; `Hub::on_net_labeled`).
    async fn noted_unread(&self, noted: crate::wire::unread::Noted) {
        crate::api::boxed(|| self.audit_unread(noted.rows)).await;
        for page in &noted.changed {
            crate::api::boxed(|| self.emit_refused(page)).await;
        }
    }

    /// What came up a page's socket that couldn't be read (`crate::wire::ui`), noted from whoever's
    /// on it, on the socket's page. The socket answers it itself, each time, on the socket it came on.
    pub async fn note_refused(&self, s: &crate::wire::ui::Socket, r: &crate::wire::ui::Refused) {
        let _ = self.note_unread(s.refused(r), s.page.clone()).await;
    }

    /// A frame the server sent down a page's socket that the page couldn't read, as the page said
    /// (`crate::wire::ui::UnreadFrame`): noted, on the socket's page.
    pub async fn note_told(&self, s: &crate::wire::ui::Socket, f: &crate::wire::ui::UnreadFrame) {
        let _ = self.note_unread(s.unread(f), s.page.clone()).await;
    }

    /// What came up a page's socket whose only frame up is `unread` (the events and log sockets): a
    /// frame the page couldn't read of what came down, noted; anything else noted and answered with
    /// the socket's `refused` frame (`refused` makes it), or the socket closed, past its limit. True:
    /// the socket is done.
    pub async fn unread_only(&self, sock: &crate::wire::ui::Socket, tx: &mut crate::http::ws::Sender, came: crate::wire::ui::Came, refused: impl Fn(crate::wire::ui::Refused) -> String) -> anyhow::Result<bool> {
        use crate::wire::ui::{Came, Refused};
        let r = match came {
            Came::Text(t) => match crate::wire::ui::unread_up(t.as_str()) {
                Ok(f) => {
                    self.note_told(sock, &f).await;
                    return Ok(false);
                }
                Err(r) => r,
            },
            Came::Binary(_) => Refused::binary(),
            Came::Control => return Ok(false),
            Came::Gone => return Ok(true),
            Came::Ends { close, refused } => {
                self.note_refused(sock, &refused).await;
                let _ = tx.close(Some(close)).await;
                return Ok(true);
            }
        };
        self.note_refused(sock, &r).await;
        tx.send(crate::http::ws::text(refused(r))).await?;
        Ok(false)
    }

    /// The audit log's rows a minute for what the wires couldn't read, as they come due.
    pub(crate) async fn tick_unread(&self) {
        let rows = self.refusals.update(crate::wire::unread::Refusals::tick);
        self.audit_unread(rows).await;
    }

    async fn audit_unread(&self, rows: Vec<crate::wire::unread::Row>) {
        for r in rows {
            let e = crate::audit::Entry {
                principal: r.principal,
                action: crate::wire::unread::ACTION.to_owned(),
                target: Some(r.target.to_owned()),
                result: r.result.to_owned(),
                ip: r.address,
                detail: r.detail,
                ..Default::default()
            };
            crate::audit::write(&self.store, e).await;
        }
    }

    /// `page`'s refusals changed: its viewers are told, a runner's in its record, an agent
    /// session's in its info.
    async fn emit_refused(&self, page: &crate::wire::unread::Page) {
        match page {
            crate::wire::unread::Page::Runner(name) => {
                let a = self.agents.load().get(name).cloned();
                if let Some(a) = a {
                    self.emit_agent(&a);
                }
            }
            crate::wire::unread::Page::Session(id) => {
                let kept = self.refusals.load().kept(page).cloned();
                self.harness.set_refused(self, id, kept).await;
            }
        }
    }

    pub fn emit_queue(&self) {
        // The overview says them (goal 10, plan 01a1a87b2dae step 2).
        self.harness.views.changed();
        let m = metrics::global();
        self.emit(&ServerEvent::Queue(crate::wire::events::Queue::Counts { queued: m.jobs_queued.get(), running: m.jobs_running.get() }));
    }

    pub fn record_sample(&self, agent: &str, s: HostSample) {
        self.history.update(|h| {
            let q = Arc::make_mut(h.entry(agent.to_owned()).or_default());
            if q.len() >= HISTORY {
                q.pop_front();
            }
            q.push_back(s.clone());
        });
        // The overview says the server's newest (goal 10, plan 01a1a87b2dae step 2).
        if agent == "server" {
            self.harness.views.changed();
        }
        if agent != "server" {
            metrics::agent_samples().update(|m| m.insert(agent.to_owned(), s.clone()));
            // A runner too old to report its own figures (`crate::observe::host`): its samples, as
            // metrics. The server's own machine reports itself.
            if self.instances.load().newest_with(agent, proto::CAP_OBSERVE).is_none() {
                crate::observe::hub::host_sample(agent, &s);
            }
        }
        self.emit(&ServerEvent::Sample { agent: agent.to_owned(), sample: Box::new(s) });
    }

    // ------------------------------------------------------------ agent view

    /// Persistent agents from the DB merged with live sessions.
    pub async fn agent_list(&self) -> Vec<AgentLive> {
        let rows = self.store.agents().await.unwrap_or_default();
        let labels = self.store.agent_labels().await.unwrap_or_default();
        let machines = self.store.agent_machines().await.unwrap_or_default();
        self.agent_list_of(rows, &labels, &machines, &self.agents.get())
    }

    /// [`Hub::agent_list`] of the store's rows, labels and machines given (kept in memory for `GET
    /// /api/agents` and the overview: goal 10, plan 01a1a87b2dae step 2) and the runners' live records
    /// `live`.
    pub fn agent_list_of(&self, rows: Vec<crate::store::AgentRow>, labels: &HashMap<String, String>, machines: &HashMap<String, String>, live: &HashMap<String, Arc<AgentLive>>) -> Vec<AgentLive> {
        let mut out: Vec<AgentLive> = rows
            .into_iter()
            .map(|r| match live.get(&r.name) {
                Some(l) => AgentLive { label: labels.get(&r.name).cloned(), ..AgentLive::clone(l) },
                None => AgentLive {
                    label: labels.get(&r.name).cloned(),
                    name: r.name,
                    peer: None,
                    outpost: is_outpost(&r.labels, r.slots),
                    labels: r.labels,
                    slots: r.slots,
                    hostname: r.hostname,
                    os: r.os,
                    arch: r.arch,
                    version: r.version,
                    ephemeral: r.ephemeral,
                    connected: false,
                    last_seen: r.last_seen,
                    enrolled_at: r.enrolled_at,
                    running: vec![],
                    busy: 0,
                    draining: false,
                    sample: None,
                    battery: None,
                    dropped: Vec::new(),
                    dropped_toward: Vec::new(),
                    refused: None,
                    terminal: false,
                    dedicated_job: None,
                    exclusive: false,
                    held: Vec::new(),
                    lost_at: None,
                    facts: None,
                    instances: vec![],
                    transport: None,
                    skew: crate::skew::Skew::default(),
                    stale: false,
                },
            })
            .collect();
        for a in &mut out {
            a.instances = self.instance_views(&a.name);
        }
        for (name, l) in live.iter() {
            if !out.iter().any(|a| &a.name == name) {
                out.push(AgentLive::clone(l));
            }
        }
        for a in &mut out {
            a.transport = a.peer.filter(|_| a.connected).map(|p| self.tunnels.transport(p));
            a.dropped_toward = a.peer.map(|p| metrics::net().toward(p)).unwrap_or_default();
            a.refused = self.refusals.load().kept(&crate::wire::unread::Page::Runner(a.name.clone())).cloned();
            // docs/machines.md, "Records that are stale": no machine id, so no machine it's proven to be.
            let said = machines.contains_key(&a.name) || a.facts.as_ref().is_some_and(|f| f.machine_id.is_some());
            a.stale = !a.connected && !a.ephemeral && !crate::join::is_machine_key(&a.name) && !said;
        }
        out.sort_by(|a, b| b.connected.cmp(&a.connected).then(a.name.cmp(&b.name)));
        out
    }

    fn update_slot_gauges(&self) {
        let g = self.agents.load();
        let (mut conn, mut total, mut busy) = (0i64, 0i64, 0i64);
        for a in g.values().filter(|a| a.connected) {
            conn += 1;
            total += i64::from(a.slots);
            busy += i64::from(a.busy);
        }
        let m = metrics::global();
        m.agents_connected.set(conn);
        m.agent_slots_total.set(total);
        m.agent_slots_busy.set(busy);
    }

    /// The recent messages of the AI session running as `pid` on a machine (`None`: it has no
    /// transcript).
    pub async fn ai_transcript(&self, machine: &str, pid: u32) -> anyhow::Result<Option<serde_json::Value>> {
        self.ai_ask(machine, |req| ServerMsg::AiTranscript { req, pid }).await
    }

    /// The normalized view (events, state, changeset) of an AI session running on a machine.
    pub async fn ai_view(&self, machine: &str, pid: u32) -> anyhow::Result<Option<serde_json::Value>> {
        self.ai_ask(machine, |req| ServerMsg::AiView { req, pid }).await
    }

    /// The output of a task of an agent session running on a machine (`{data, offset, live, ...}`
    /// or `{error}`).
    pub async fn harness_task_output(&self, machine: &str, session: &str, task: &str, since: Option<u64>) -> anyhow::Result<Option<serde_json::Value>> {
        let (session, task) = (session.to_owned(), task.to_owned());
        self.ai_ask(machine, move |req| ServerMsg::HarnessTaskOutput { req, session, task, since }).await
    }

    /// The folders at `path` on a machine, read by its newest runner process. Only one that
    /// announced [`proto::CAP_DIRS`] is sent [`ServerMsg::DirList`]: `Ok(None)` when it is too old
    /// to list folders.
    pub async fn dir_list(&self, machine: &str, path: &str) -> anyhow::Result<Option<crate::dirs::Listing>> {
        let peer = {
            let inst = self.instances.load();
            if inst.newest(machine).is_none() {
                anyhow::bail!("{} is not connected", self.shown(machine));
            }
            inst.newest_with(machine, proto::CAP_DIRS)
        };
        let Some(peer) = peer else { return Ok(None) };
        let req = self.ai_seq.fetch_add(1, Ordering::Relaxed);
        let frame = proto::encode(&ServerMsg::DirList { req, path: path.to_owned() })?;
        let (read, rx) = asked::<crate::dirs::Listing>();
        self.ai_requests.put(req, read);
        let sent = self.net.send(peer, LANE_TERM, Kind::Term, frame).await;
        let answer = if sent { crate::rt::timeout(crate::dirs::WAIT, rx).await.ok() } else { None };
        self.ai_requests.take(&req);
        match answered(&self.shown(machine), sent, answer)? {
            Some(listing) => Ok(Some(listing)),
            None => anyhow::bail!("{} did not answer", self.shown(machine)),
        }
    }

    /// A piece of a file on a machine, for whoever views agent session `session` there
    /// (`crate::harness::files`): what it says of the file, and the bytes. Asked of the runner
    /// process holding the session; `Ok(None)` when that one doesn't read files
    /// ([`proto::CAP_FILES`]).
    pub async fn harness_file_read(&self, machine: &str, session: &str, path: &str, offset: u64, len: u32) -> anyhow::Result<Option<(crate::harness::files::Answer, Vec<u8>)>> {
        /// A piece may be a megabyte from a machine on a slow link.
        const WAIT: Duration = Duration::from_secs(30);
        let req = self.ai_seq.fetch_add(1, Ordering::Relaxed);
        let msg = ServerMsg::HarnessFileRead { req, session: session.to_owned(), path: path.to_owned(), offset, len };
        let peer = {
            let inst = self.instances.load();
            if inst.newest(machine).is_none() {
                anyhow::bail!("{} is not connected", self.shown(machine));
            }
            // A read creates nothing: where it goes is read off the copy.
            let to = inst.routes(machine, &msg);
            to.filter(|p| inst.get(*p).is_some_and(|i| i.caps & proto::CAP_FILES != 0))
        };
        let Some(peer) = peer else { return Ok(None) };
        let frame = proto::encode(&msg)?;
        let (tx, rx) = futures::channel::oneshot::channel();
        self.file_requests.put(req, tx);
        let sent = self.net.send(peer, LANE_TERM, Kind::Term, frame).await;
        let answer = if sent { crate::rt::timeout(WAIT, rx).await.ok() } else { None };
        self.file_requests.take(&req);
        match answer {
            Some(Ok(Ok(a))) => Ok(Some(a)),
            Some(Ok(Err(why))) => anyhow::bail!("{}'s answer couldn't be read: {why}", self.shown(machine)),
            Some(Err(_)) => anyhow::bail!("{} did not answer", self.shown(machine)),
            None if !sent => anyhow::bail!("{} is not connected", self.shown(machine)),
            None => anyhow::bail!("{} did not answer in time", self.shown(machine)),
        }
    }

    /// What the runner process holding what `msg` is about (`crate::instances`' routing) says it
    /// can do: 0 for nothing, or no such process. One that has just connected says it a moment
    /// after (its build, then its capabilities), so this waits up to three seconds for it.
    pub async fn routed_caps(&self, machine: &str, msg: &ServerMsg) -> u32 {
        for _ in 0..30 {
            let to = self.route(machine, msg);
            let caps = to.and_then(|p| self.instances.load().get(p).map(|i| i.caps)).unwrap_or(0);
            if caps != 0 {
                return caps;
            }
            crate::rt::sleep(Duration::from_millis(100)).await;
        }
        0
    }

    /// Ask the runner process holding what `msg` is about (a session, say: `crate::instances`'
    /// routing) something, if it announced `cap`, and wait up to `wait` for its
    /// [`AgentMsg::Answer`], read as a `T`. `Ok(None)`: that process doesn't know `cap` (its kiln is
    /// too old).
    pub async fn ask_routed<T: serde::de::DeserializeOwned + Serialize + Send + 'static>(&self, machine: &str, cap: u32, wait: Duration, msg: impl FnOnce(u64) -> ServerMsg) -> anyhow::Result<Option<Option<T>>> {
        let req = self.ai_seq.fetch_add(1, Ordering::Relaxed);
        let msg = msg(req);
        if self.instances.load().newest(machine).is_none() {
            anyhow::bail!("{} is not connected", self.shown(machine));
        }
        if self.routed_caps(machine, &msg).await & cap != cap {
            return Ok(None);
        }
        let Some(peer) = self.route(machine, &msg) else { return Ok(None) };
        let frame = proto::encode(&msg)?;
        let (read, rx) = asked::<T>();
        self.ai_requests.put(req, read);
        let sent = self.net.send(peer, LANE_TERM, Kind::Term, frame).await;
        let answer = if sent { crate::rt::timeout(wait, rx).await.ok() } else { None };
        self.ai_requests.take(&req);
        answered(&self.shown(machine), sent, answer).map(Some)
    }

    /// Ask machine `machine` something by its newest runner process announcing `cap` (`msg` given
    /// the question's number), and wait up to `wait` for its [`AgentMsg::Answer`], read as a `T`.
    /// `Ok(None)`: no process of it announces `cap` (its kiln is too old); `Ok(Some(answer))`: what
    /// it answered.
    pub async fn ask_runner<T: serde::de::DeserializeOwned + Serialize + Send + 'static>(&self, machine: &str, cap: u32, wait: Duration, msg: impl FnOnce(u64) -> ServerMsg) -> anyhow::Result<Option<Option<T>>> {
        let peer = {
            let inst = self.instances.load();
            if inst.newest(machine).is_none() {
                anyhow::bail!("{} is not connected", self.shown(machine));
            }
            inst.newest_with(machine, cap)
        };
        let Some(peer) = peer else { return Ok(None) };
        let req = self.ai_seq.fetch_add(1, Ordering::Relaxed);
        let frame = proto::encode(&msg(req))?;
        let (read, rx) = asked::<T>();
        self.ai_requests.put(req, read);
        let sent = self.net.send(peer, LANE_TERM, Kind::Term, frame).await;
        let answer = if sent { crate::rt::timeout(wait, rx).await.ok() } else { None };
        self.ai_requests.take(&req);
        answered(&self.shown(machine), sent, answer).map(Some)
    }

    /// A machine's past sessions.
    pub async fn harness_history(&self, machine: &str, kind: Option<String>, limit: u32) -> anyhow::Result<Option<Vec<crate::aisessions::PastSession>>> {
        self.ai_ask(machine, move |req| ServerMsg::HarnessHistory { req, kind, limit }).await
    }

    async fn ai_ask<T: serde::de::DeserializeOwned + Serialize + Send + 'static>(&self, machine: &str, msg: impl FnOnce(u64) -> ServerMsg) -> anyhow::Result<Option<T>> {
        let req = self.ai_seq.fetch_add(1, Ordering::Relaxed);
        let (read, rx) = asked::<T>();
        self.ai_requests.put(req, read);
        if !self.send_to_agent(machine, LANE_TERM, &msg(req)).await {
            self.ai_requests.take(&req);
            anyhow::bail!("machine {} is not connected", self.shown(machine));
        }
        let answer = crate::rt::timeout(Duration::from_secs(10), rx).await;
        self.ai_requests.take(&req);
        answered(&self.shown(machine), true, answer.ok())
    }

    /// The runner processes of machine `agent` a message goes to. While it runs more than one
    /// (see `crate::instances`), a message about something one of them holds (a terminal, an
    /// agent session, a job) goes to that one, anything new to the newest, and Drain to all of
    /// them.
    fn peers_for(&self, agent: &str, msg: &ServerMsg) -> Vec<PeerId> {
        let mut peers: Vec<PeerId> = if matches!(msg, ServerMsg::Drain) { self.instances.load().of(agent) } else { self.route(agent, msg).into_iter().collect() };
        if peers.is_empty() {
            peers.extend(self.agents.load().get(agent).and_then(|a| a.peer));
        }
        peers
    }

    /// Sends `msg` to machine `agent` as a command (docs/wire.md, "Commands"), to the processes
    /// [`Hub::peers_for`] picks, and waits up to `within` for its answer.
    /// A parked command whose runner is away answers [`Answer::Parked`] at once.
    ///
    /// [`Answer::Parked`]: crate::wire::commands::Answer::Parked
    pub async fn command(&self, agent: &str, msg: &ServerMsg, within: Duration) -> crate::wire::commands::Answer {
        self.command_start(agent, msg).await.answer_or_parked(within).await
    }

    /// Sends `msg` to machine `agent` as a command, to the processes [`Hub::peers_for`] picks: its
    /// answer to come (a caller that mustn't wait for it, the scheduler say, takes it later).
    pub async fn command_start(&self, agent: &str, msg: &ServerMsg) -> Pending {
        let (peers, last) = self.command_targets(agent, msg);
        let peers = match last {
            Some(p) if self.cmds.ask(move |c| c.keeps(p)).await.unwrap_or(false) => vec![p],
            _ => peers,
        };
        self.command_peers(agent, peers, msg).await
    }

    /// [`Hub::command_start`] where nothing can wait (a sync path): the command is queued in its turn
    /// by the commands' owner, and the commands loop sends it at once. A runner from before the
    /// channel gets it if its lane has room. What it comes to is its one wait's: what it is at once,
    /// or whether its runner is away, isn't known here.
    pub fn command_soon(&self, agent: &str, msg: &ServerMsg) -> Pending {
        use crate::wire::commands::Answer;
        let (peers, last) = self.command_targets(agent, msg);
        let (shown, what) = (self.shown(agent), crate::wire::server_name(msg));
        let body = match proto::encode(msg) {
            Ok(b) => b,
            Err(e) => return Pending { agent: agent.to_owned(), shown, what, waits: Vec::new(), at_once: Answer::Unreachable { why: format!("it couldn't be encoded: {e}") }, away: false },
        };
        let (tx, rx) = futures::channel::oneshot::channel();
        let (stream, net, a, s, lane) = (stream_of(msg), self.net.clone(), agent.to_owned(), shown.clone(), session_lane(msg).map(str::to_owned));
        self.cmds.tell(move |c| {
            let mut peers = peers;
            peers.extend(last.filter(|p| c.keeps(*p)));
            let pending = queue_on(c, &a, &s, what, peers, stream, lane.as_deref(), &body, |p, body| net.try_send(p, LANE_CONTROL, Kind::Control, body));
            // What it comes to, for whoever waits on it here.
            drop(crate::rt::spawn(async move {
                let _ = tx.send(pending.answer(LOG_WITHIN).await);
            }));
        });
        self.commands_due.notify();
        Pending { agent: agent.to_owned(), shown, what, waits: vec![rx], at_once: Answer::Taken, away: false }
    }

    /// The runner processes a command to machine `agent` goes to ([`Hub::peers_for`]); and, for a
    /// parked one to a machine that's away, the process it last had, which comes back under the
    /// same certificate: it goes there when that one keeps the channel.
    fn command_targets(&self, agent: &str, msg: &ServerMsg) -> (Vec<PeerId>, Option<PeerId>) {
        let peers = self.peers_for(agent, msg);
        let last = if peers.is_empty() && stream_of(msg) == Some(crate::wire::commands::Kind::Parked) { self.away_peers.load().get(agent).copied() } else { None };
        (peers, last)
    }

    /// Sends `msg` as a command to runner processes `peers` of machine `agent`. One that keeps the
    /// channel gets it on its command stream (parked or live, as the message's class says) and
    /// answers it once it applied it; one from before the channel gets it as it always did.
    pub async fn command_peers(&self, agent: &str, peers: Vec<PeerId>, msg: &ServerMsg) -> Pending {
        use crate::wire::commands::Answer;
        let (shown, what) = (self.shown(agent), crate::wire::server_name(msg));
        let body = match proto::encode(msg) {
            Ok(b) => b,
            Err(e) => return Pending { agent: agent.to_owned(), shown, what, waits: Vec::new(), at_once: Answer::Unreachable { why: format!("it couldn't be encoded: {e}") }, away: false },
        };
        // Queued by the commands' owner, in its turn. A runner from before the channel gets it as it
        // always did, sent after the rest.
        let (stream, a, s, lane) = (stream_of(msg), agent.to_owned(), shown.clone(), session_lane(msg).map(str::to_owned));
        let queued = self
            .cmds
            .ask(move |c| {
                let mut legacy: Vec<(PeerId, Vec<u8>)> = Vec::new();
                let pending = queue_on(c, &a, &s, what, peers, stream, lane.as_deref(), &body, |p, body| {
                    legacy.push((p, body));
                    true
                });
                (pending, legacy)
            })
            .await;
        let Some((mut pending, legacy)) = queued else {
            return Pending { agent: agent.to_owned(), shown, what, waits: Vec::new(), at_once: Answer::Unreachable { why: "the server's commands have stopped".into() }, away: false };
        };
        if !legacy.is_empty() {
            let mut sent = false;
            for (p, body) in legacy {
                sent |= self.net.send(p, LANE_CONTROL, Kind::Control, body).await;
            }
            if !sent {
                use crate::wire::commands::Answer;
                pending.at_once = if pending.waits.is_empty() { Answer::Unreachable { why: format!("{} is not connected", pending.shown) } } else { Answer::Taken };
            }
        }
        self.flush_commands().await;
        pending
    }

    /// The commands that waited for runner process `peer` to say whether it keeps the channel, and
    /// it doesn't: sent as a runner's from before the channel gets them, answered as sent (it
    /// can't say more), or unreachable when they couldn't go.
    async fn send_waited(&self, peer: PeerId, waited: Vec<(Vec<u8>, futures::channel::oneshot::Sender<crate::wire::commands::Answer>)>) {
        use crate::wire::commands::Answer;
        for (body, answer) in waited {
            let sent = self.net.send(peer, LANE_CONTROL, Kind::Control, body).await;
            let _ = answer.send(if sent { Answer::Sent } else { Answer::Unreachable { why: "its runner isn't connected".into() } });
        }
    }

    /// A test build's (`--features chaos`, `POST /api/chaos/send`): `bytes`, which the server never
    /// sends, to runner `runner`: a frame on `lane`, or an item of its live command stream (`item`),
    /// whose answer comes back. tests/hostile.rs holds a runner to what it can't read of the
    /// server's with it.
    #[cfg(feature = "chaos")]
    pub(crate) async fn chaos_send(&self, runner: &str, lane: u32, item: bool, bytes: Vec<u8>) -> Result<serde_json::Value, String> {
        use crate::wire::commands::{Answer, Kind as Stream};
        let peer = self.agents.load().get(runner).filter(|a| a.connected).and_then(|a| a.peer).ok_or_else(|| format!("{runner} is not connected"))?;
        if !item {
            let kind = match lane {
                LANE_TERM => Kind::Term,
                proto::LANE_CONVO => Kind::Log,
                _ => Kind::Control,
            };
            return Ok(serde_json::json!({ "sent": self.net.send(peer, lane, kind, bytes).await }));
        }
        let Some(rx) = self.cmds.ask(move |c| c.push(peer, Stream::Live, None, bytes, Instant::now())).await else { return Err("the server's commands have stopped".into()) };
        self.flush_commands().await;
        let answer = match crate::rt::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(a)) => a,
            Ok(Err(_)) => return Err("its answer was let go".into()),
            Err(_) => return Err("no answer in 30 s".into()),
        };
        Ok(match answer {
            Answer::Taken => serde_json::json!({ "answer": "taken" }),
            Answer::Refused { code, why } => serde_json::json!({ "answer": "refused", "code": code, "why": why }),
            Answer::Unreachable { why } => serde_json::json!({ "answer": "unreachable", "why": why }),
            Answer::Unanswered { why } => serde_json::json!({ "answer": "unanswered", "why": why }),
            Answer::Sent => serde_json::json!({ "answer": "sent" }),
            Answer::Parked => serde_json::json!({ "answer": "parked" }),
        })
    }

    /// The runners' commands due now: those not sent yet on their link, and those whose answer is
    /// overdue (lost below the channel).
    async fn flush_commands(&self) {
        let frames = self.cmds.ask(|c| c.poll(Instant::now())).await.unwrap_or_default();
        for (peer, f) in frames {
            self.send_peer(peer, &ServerMsg::Wire(f)).await;
        }
    }

    /// Sends the runners' commands that come due on their own (docs/wire.md, "Commands"), once a
    /// second: answers overdue, parked commands whose runner never came back.
    pub async fn commands_loop(self: Arc<Self>) -> anyhow::Result<()> {
        let mut tick = Interval::new(Duration::from_secs(1));
        let mut said = Instant::now();
        loop {
            futures::select_biased! {
                () = self.shutdown.cancelled().fuse() => return Ok(()),
                () = self.commands_due.notified().fuse() => {}
                () = tick.tick().fuse() => {}
            }
            // Runners that never said they keep the channel: from before it, what waited for them
            // goes their way.
            let overdue = self.cmds.ask(|c| c.overdue(Instant::now())).await.unwrap_or_default();
            for peer in overdue {
                let waited = self.cmds.ask(move |c| c.not_wire(peer)).await.unwrap_or_default();
                self.send_waited(peer, waited).await;
            }
            self.flush_commands().await;
            // The audit log's rows a minute for what the wires couldn't read.
            self.tick_unread().await;
            // What the server has of each runner's streams, said again every few seconds: one lost
            // below the channel mustn't hold a runner's updates up until its next connection.
            if said.elapsed() >= HAVE_AGAIN {
                said = Instant::now();
                let runners: Vec<(String, PeerId)> = self.agents.load().values().filter(|a| a.connected && self.wire.keeps(&a.name)).filter_map(|a| Some((a.name.clone(), a.peer?))).collect();
                for (name, peer) in runners {
                    self.send_peer(peer, &ServerMsg::Wire(self.wire_have(&name))).await;
                }
            }
        }
    }

    /// Send to a machine, to the processes [`Hub::peers_for`] picks.
    pub async fn send_to_agent(&self, agent: &str, lane: u32, msg: &ServerMsg) -> bool {
        let peers = self.peers_for(agent, msg);
        if peers.is_empty() {
            return false;
        }
        let kind = match lane {
            LANE_TERM => Kind::Term,
            // The conversation lane's frames are bulk (a session's package goes in large pieces).
            proto::LANE_CONVO => Kind::Log,
            _ => Kind::Control,
        };
        let b = match proto::encode(msg) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "encode failed");
                return false;
            }
        };
        let mut sent = false;
        for p in peers {
            sent |= self.net.send(p, lane, kind, b.clone()).await;
        }
        sent
    }

    /// Runs of reach links' frames the relay passed on, recorded (docs/ci.md W27-R08's relays): signed,
    /// kept and sent to the runners they went to, on their own (`crate::relays::frames`).
    /// Reach's relay's owner, started on first use: it needs the hub to send what the relay passes on
    /// (goal 5, plan 01a1a59c2aae step 2: an owner with messages, no lock on the relay).
    pub(crate) fn relay(self: &Arc<Self>) -> &crate::reach::relay::Owner {
        if let Some(rx) = self.reach.starting() {
            let hub = Arc::downgrade(self);
            drop(crate::rt::spawn(Self::relay_loop(hub, rx)));
        }
        &self.reach
    }

    /// Custody's intake of runners' records, its task started on first use, on the next shard round-robin rather than the
    /// net loop's (goal 9, plan 01a1a87b2cec step 3): it reads, checks and keeps each record, one at a time in the order
    /// they came, and holds the hub weakly (`crate::custody::runners::intake_loop`). Its place is here, so the shards'
    /// table of what held them names it among the hub's tasks, as runners' custody records were.
    pub(crate) fn custody_intake(self: &Arc<Self>) -> &crate::custody::runners::Intake {
        if let Some(rx) = self.custody_in.starting() {
            drop(crate::rt::spawn_spread_joined(crate::custody::runners::intake_loop(Arc::downgrade(self), rx)));
        }
        &self.custody_in
    }

    /// Reach's relay's one owner: it alone changes the relay, taking each frame, lost connection and
    /// question in the order sent, and sends what the relay passes on in that order. It holds the hub
    /// weakly: once the hub goes, its sender with it, so does this.
    async fn relay_loop(hub: std::sync::Weak<Self>, rx: crate::memory::queue::Receiver<crate::reach::relay::Ask>) {
        use crate::reach::relay::Ask;
        let mut relay = crate::reach::relay::Relay::recording();
        while let Ok(ask) = rx.recv().await {
            let Some(hub) = hub.upgrade() else { return };
            match ask {
                Ask::Frame { from, peer, frame } => hub.relay_frame(&mut relay, &from, peer, frame).await,
                Ask::Lost { peer } => {
                    for (p, m) in relay.lost(peer, "disconnected from the kiln server") {
                        if let Ok(b) = proto::encode(&m) {
                            hub.net.send(p, proto::LANE_REACH, Kind::Term, b).await;
                        }
                    }
                    hub.reach_passed(relay.take_passed());
                }
                Ask::Due => hub.reach_passed(relay.due()),
                Ask::Call(f) => f(&mut relay),
            }
        }
    }

    /// Runner `agent`'s frame on reach's relay, from its connection `peer`, as the relay's owner takes
    /// it: forwarded to the link's other end, refused back to it, or noted.
    async fn relay_frame(self: &Arc<Self>, relay: &mut crate::reach::relay::Relay, agent: &str, peer: PeerId, frame: crate::reach::relay::Frame) {
        let forwarded = relay.forward(
            agent,
            peer,
            frame,
            |n| {
                if self.agents.load().get(n).is_none_or(|a| a.ephemeral || !a.connected) {
                    return None;
                }
                self.instances.load().newest_with(n, proto::CAP_REACH)
            },
            |n| self.shown(n),
        );
        let passed = relay.take_passed();
        // One from a runner that isn't an end of the link it names goes nowhere: refused to it, and
        // noted naming the link's ends.
        if let crate::reach::relay::Forwarded::NotItsLink { link, ends: (a, b) } = &forwarded {
            let why = format!("not its link: link {link} is {a}'s and {b}'s");
            self.unreadable(peer, Wire::Relay, "Reach".to_owned(), Code::Malformed, why, true).await;
        }
        let typed = self.instances.load().get(peer).is_some_and(|i| i.wide & proto::CAP_UNREAD != 0);
        for (p, m) in forwarded.sends(peer, typed) {
            self.send_reach(p, &m).await;
        }
        self.reach_passed(passed);
    }

    pub(crate) fn reach_passed(self: &Arc<Self>, passed: Vec<crate::reach::relay::Passed>) {
        if passed.is_empty() {
            return;
        }
        let hub = Arc::clone(self);
        crate::rt::spawn(async move { crate::relays::frames(&hub, passed).await });
    }

    /// `msg` to runner process `peer` on reach's lane.
    async fn send_reach(&self, peer: PeerId, msg: &ServerMsg) {
        #[cfg(test)]
        self.sent.lock().push((peer, msg.clone()));
        if let Ok(b) = proto::encode(msg) {
            self.net.send(peer, proto::LANE_REACH, Kind::Term, b).await;
        }
    }

    pub(crate) async fn send_peer(&self, peer: PeerId, msg: &ServerMsg) {
        #[cfg(test)]
        self.sent.lock().push((peer, msg.clone()));
        if let Ok(b) = proto::encode(msg) {
            self.net.send(peer, LANE_CONTROL, Kind::Control, b).await;
        }
    }

    pub async fn set_draining(&self, agent: &str, draining: bool) -> bool {
        let Some(snapshot) = self.change_agent(agent, |a| {
            a.draining = draining;
            a.clone()
        }) else {
            return false;
        };
        self.emit_agent(&snapshot);
        self.wake.notify();
        true
    }

    pub async fn forget_agent(&self, agent: &str, by: &str) -> anyhow::Result<()> {
        // Its certificates are revoked for good, recorded so a restart keeps it, and refused at
        // once (W27-R01); enrolling again under the name brings a new key.
        let at = crate::util::now_unix();
        // The names its certificates carried from before go with it, and their keys are revoked
        // too (docs/machines.md, "One record a machine").
        let names = self.store.forget_machine_names(agent.to_owned()).await?;
        self.machine_names.update(|n| n.retain(|_, m| m != agent));
        let mut keys = crate::identities::revoke(&self.store, crate::identities::Target::Subject(agent.to_owned()), "runner forgotten", by, at).await?;
        for n in &names {
            keys.extend(crate::identities::revoke(&self.store, crate::identities::Target::Subject(n.clone()), "runner forgotten", by, at).await?);
            self.net.command(crate::net::Out::Revoke { name: n.clone() }).await;
        }
        if !keys.is_empty() {
            self.net.command(crate::net::Out::RevokeKeys { keys, why: crate::identities::refusal(at, "runner forgotten", by) }).await;
        }
        let mut peers: Vec<PeerId> = self.agents.update(|m| m.remove(agent)).and_then(|a| a.peer).into_iter().collect();
        peers.extend(self.instances.load().of(agent));
        peers.sort_unstable();
        peers.dedup();
        for p in peers {
            // What it hadn't answered of the server's commands ends here.
            self.cmds.ask(move |c| c.gone(p, "its runner was forgotten")).await;
            self.net.command(crate::net::Out::Disconnect { peer: p }).await;
        }
        self.net.command(crate::net::Out::Revoke { name: agent.to_owned() }).await;
        self.away_peers.update(|m| m.remove(agent));
        self.reach.forget(agent);
        metrics::agent_samples().update(|m| m.remove(agent));
        self.store.delete_agent(agent.to_owned()).await?;
        self.front_doors.forget(&self.store, agent).await;
        self.agent_limits.forget(&self.store, agent).await;
        self.headrooms.forget(&self.store, agent).await;
        self.places.forget(agent);
        self.tailnet.wake.notify();
        self.update_slot_gauges();
        Ok(())
    }

    /// Runner `from` is known as `to` from now on, shown as `label` (docs/machines.md: a machine known
    /// by its name moving to its machine id): its rows and what's set for it, its agent sessions, its
    /// synced copies and places, and its member in the cluster group go with it. Refused while a
    /// process of it is connected: what it holds would move from under it.
    ///
    /// `to` may be known already (a second record of the machine folding into its id,
    /// [`Hub::fold_duplicates`]): what it has wins over `from`'s, its label included. The label
    /// `to` is shown by now.
    pub async fn move_runner(&self, from: &str, to: &str, label: &str, hostname: &str) -> anyhow::Result<String> {
        anyhow::ensure!(from != to, "{from} is known by that name already");
        anyhow::ensure!(!self.is_connected(from), "{from} has a runner process connected: it moves to its machine id once none is");
        let prefixes = vec![crate::frontdoor::control::KEY_PREFIX, crate::harness::limit::KEY_PREFIX, crate::updates::POLICY_PREFIX];
        self.store.move_runner(from.to_owned(), to.to_owned(), label.to_owned(), hostname.to_owned(), prefixes).await?;
        // What the store kept: `to`'s own label when it had one.
        let label = self.store.agent_label(to.to_owned()).await.ok().flatten().unwrap_or_else(|| label.to_owned());
        self.labels.update(|labels| {
            labels.remove(from);
            labels.insert(to.to_owned(), label.clone());
        });
        self.change_agent(to, |a| a.label = Some(label.clone()));
        self.machine_names.update(|names| {
            for m in names.values_mut().filter(|m| m.as_str() == from) {
                *m = to.to_owned();
            }
            if crate::join::is_machine_key(to) && !crate::join::is_machine_key(from) {
                names.insert(from.to_owned(), to.to_owned());
            }
        });
        self.agents.update(|m| m.remove(from));
        self.away_peers.update(|m| m.remove(from));
        self.reach.forget(from);
        metrics::agent_samples().update(|m| m.remove(from));
        self.front_doors.moved(from, to);
        self.agent_limits.moved(from, to);
        self.places.moved(from, to);
        self.sync.moved(from, to);
        let moved = self.harness.move_machine(self, from, to).await;
        // Onto a machine connected now: what it holds reads as it is at once, what it doesn't
        // hold is gone, as its reconnect would say (docs/machines.md, "One record a machine").
        if !moved.is_empty() {
            self.reconcile_moved(to, &moved).await;
        }
        if let Some(group) = self.store.group() {
            self.cluster.moved(group, from);
        }
        self.update_slot_gauges();
        Ok(label)
    }

    /// One record a machine (docs/machines.md), as the server starts and before any runner
    /// connects: the names machines' certificates carried are loaded, and every record kept under
    /// a name whose runner said its machine id folds into that id ([`Hub::move_runner`]), merged
    /// with what the id has. Records of job pods (each its own, forgotten when done) and records
    /// that never said an id (stale, a person's to forget) stay. The names folded.
    pub async fn one_record_a_machine(&self) -> Vec<String> {
        match self.store.machine_names().await {
            Ok(n) => self.machine_names.set(n),
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "the names machines' certificates carried couldn't be read"),
        }
        let (rows, machines) = match (self.store.agents().await, self.store.agent_machines().await) {
            (Ok(r), Ok(m)) => (r, m),
            (Err(e), _) | (_, Err(e)) => {
                tracing::warn!(error = %format!("{e:#}"), "the runners' records couldn't be read: none folded into its machine id");
                return Vec::new();
            }
        };
        let mut folded = Vec::new();
        let mut ids: std::collections::HashSet<String> = rows.iter().map(|r| r.name.clone()).collect();
        for r in rows.iter().filter(|r| !r.ephemeral && !crate::join::is_machine_key(&r.name)) {
            let Some(id) = machines.get(&r.name).and_then(|m| crate::join::machine_key(Some(m))) else { continue };
            let shown = self.shown(&r.name);
            let known = !ids.insert(id.clone());
            match self.move_runner(&r.name, &id, &shown, &r.hostname).await {
                Ok(label) => {
                    let (action, said) = if known {
                        ("agent.merged", format!("{shown} was machine {id}: folded into {label}"))
                    } else {
                        ("agent.moved", format!("{shown} is known by its machine id {id}, shown as {label}"))
                    };
                    self.store.audit("kiln".into(), action.into(), said).await;
                    folded.push(r.name.clone());
                }
                Err(e) => tracing::warn!(runner = %shown, machine = %id, error = %format!("{e:#}"), "a runner's record didn't fold into its machine id"),
            }
        }
        if !folded.is_empty() {
            tracing::info!(records = folded.len(), "runners' records kept under names folded into their machine ids");
        }
        folded
    }

    /// Sessions `moved` onto machine `id`: when every connected process of it has said what it
    /// holds, those it holds that read disconnected are asked for their snapshots (as its
    /// reconnect asks, `still_disconnected`) and those it doesn't hold are gone. A machine not
    /// connected, or one whose process hasn't said yet, says on its own as it does.
    async fn reconcile_moved(&self, id: &str, moved: &[String]) {
        let held: Option<Vec<String>> = {
            let inst = self.instances.load();
            let peers = inst.of(id);
            (!peers.is_empty() && peers.iter().all(|p| inst.get(*p).is_some_and(|i| i.reported))).then(|| {
                peers
                    .iter()
                    .flat_map(|p| inst.owned_by(*p))
                    .filter_map(|o| match o {
                        crate::instances::Owned::Agent(s) => Some(s),
                        _ => None,
                    })
                    .collect()
            })
        };
        let Some(held) = held else { return };
        let ask = self.harness.moved_onto_connected(self, id, moved, &held).await;
        // Sent, not waited for: each snapshot comes back on its session's stream.
        for s in ask {
            drop(self.harness.dispatch(self, &s, crate::harness::model::Command::Snapshot).await);
            tracing::debug!(session = %s, "asked a session moved onto its connected machine for its snapshot");
        }
    }

    /// The machine a connecting runner is (docs/machines.md, "One record a machine"), whose
    /// certificate names it `cert`: the id itself for a certificate that names one, and for a job's
    /// pod its pod; else the id it said before its Hello ([`AgentMsg::Machine`]), else the one kiln
    /// knows for that name (one its certificate carried from before, or its record's). One known by
    /// a name it never said an id for stays by it (stale once it's away). What kiln kept under the
    /// name moves to the id now, merged with what the id has, and the name is the id's label:
    /// nothing is keyed by it from then on. Refused, saying why, when it says another machine than
    /// the one kiln knows for its name (a copy of that machine, or one whose id changed).
    async fn machine_of_connection(&self, peer: PeerId, cert: &str, h: &Hello) -> Result<String, String> {
        if h.ephemeral || crate::join::is_machine_key(cert) {
            return Ok(cert.to_owned());
        }
        let said = self.said_machine.load().get(&peer).cloned();
        let known = self.machine_names.load().get(cert).cloned();
        let stored = match &known {
            Some(_) => None,
            None => self.store.agent_machines().await.ok().and_then(|m| m.get(cert).and_then(|s| crate::join::machine_key(Some(s)))),
        };
        let kept = known.clone().or(stored);
        let id = match (said, kept) {
            (Some(s), Some(k)) if s != k => {
                return Err(format!("{} is machine {k}, and this runner says it is machine {s}: a copy of it, or a machine whose id changed. Forget it and join this machine again", self.shown(cert)));
            }
            (Some(id), _) | (None, Some(id)) => id,
            (None, None) => return Ok(cert.to_owned()),
        };
        // Another process of it already connected by its name (it said no id then): this one joins
        // it there, and the machine moves to its id once none is connected.
        if self.is_connected(cert) {
            tracing::info!(certificate = %cert, machine = %id, "a process of this runner is connected by its name: this one joins it, the machine moves to its id once none is");
            return Ok(cert.to_owned());
        }
        let has_record = self.store.agents().await.map(|rows| rows.iter().any(|r| r.name == cert)).unwrap_or(false);
        if known.is_none() || has_record {
            let shown = self.shown(cert);
            match self.move_runner(cert, &id, &shown, &h.hostname).await {
                Ok(label) => {
                    self.store.audit("kiln".into(), "agent.moved".into(), format!("{shown} connected as machine {id}: known by its id, shown as {label}")).await;
                    tracing::info!(certificate = %cert, machine = %id, label = %label, "a runner whose certificate carries a name from before is known by its machine id");
                }
                Err(e) => return Err(format!("{shown} couldn't be known by its machine id {id} now: {e:#}")),
            }
        }
        Ok(id)
    }

    /// Whether runner `name` has a process connected.
    fn is_connected(&self, name: &str) -> bool {
        !self.instances.load().of(name).is_empty() || self.agents.load().get(name).is_some_and(|a| a.connected)
    }

    /// The other records of machine `id` (docs/machines.md, "Duplicates fold into the machine's
    /// id"): each runner known by a name whose runner said it is machine `id`, and whether a
    /// process of it is connected. Never one known by another machine id (that one is its own
    /// machine, or a copy caught as one), never one by its host name alone.
    pub async fn duplicates_of(&self, id: &str) -> anyhow::Result<Vec<(String, bool)>> {
        if !crate::join::is_machine_key(id) {
            return Ok(Vec::new());
        }
        let machines = self.store.agent_machines().await?;
        let mut out: Vec<(String, bool)> = machines
            .into_iter()
            .filter(|(name, said)| name != id && !crate::join::is_machine_key(name) && crate::join::machine_key(Some(said)).as_deref() == Some(id))
            .map(|(name, _)| {
                let up = self.is_connected(&name);
                (name, up)
            })
            .collect();
        out.sort();
        Ok(out)
    }

    /// Every other record of machine `id` that isn't connected folds into `id`
    /// (docs/machines.md, "Duplicates fold into the machine's id"): what kiln kept under its name
    /// moves to the id as a runner from before's does ([`Hub::move_runner`]), merged with what
    /// the id has, and its record is gone, audited (`agent.merged`). A connected one stays, said
    /// in the log: it folds once it isn't. The names folded.
    pub async fn fold_duplicates(&self, id: &str) -> anyhow::Result<Vec<String>> {
        let dups = self.duplicates_of(id).await?;
        if dups.is_empty() {
            return Ok(Vec::new());
        }
        let rows = self.store.agents().await?;
        let mut folded = Vec::new();
        for (dup, up) in dups {
            let shown = self.shown(&dup);
            if up {
                tracing::warn!(machine = %id, runner = %shown, "a second record of this machine is connected: it folds into the machine's id once it isn't");
                continue;
            }
            let hostname = rows.iter().find(|r| r.name == dup).map(|r| r.hostname.clone()).unwrap_or_default();
            // The id's own label wins; one without takes the duplicate's (it's gone, so its name is free).
            let label = match self.move_runner(&dup, id, &shown, &hostname).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(machine = %id, runner = %shown, error = %format!("{e:#}"), "a second record of this machine didn't fold");
                    continue;
                }
            };
            self.store.audit("kiln".into(), "agent.merged".into(), format!("{shown} was machine {id}: folded into {label}")).await;
            tracing::info!(machine = %id, runner = %shown, into = %label, "a second record of a machine folded into its id");
            folded.push(dup);
        }
        Ok(folded)
    }

    /// What runners took to come back before this start, from kiln.db ([`crate::comeback`]): the grace their jobs and
    /// attaches get from now on.
    pub async fn take_comebacks(&self) {
        if let Ok(Some(kept)) = self.store.kv_get(crate::comeback::KEY.to_owned()).await {
            self.comeback.take_kept(&kept);
        }
    }

    /// Job pods' records that are done with (docs/machines.md, "Finished job pods are forgotten"):
    /// an ephemeral runner not connected, past its reconnect grace, whose jobs have all ended, is
    /// forgotten. Its jobs and their history keep its name. Nothing while the server is younger
    /// than the grace, as pods connected before it started may still come back.
    pub async fn forget_finished_pods(&self) -> Vec<String> {
        if self.started.elapsed() <= self.comeback.grace() {
            return Vec::new();
        }
        self.forget_pods_but_waiting().await
    }

    /// [`Hub::forget_finished_pods`], whatever the server's age.
    async fn forget_pods_but_waiting(&self) -> Vec<String> {
        // Those connected, or gone less than the grace ago: they may come back for their jobs.
        let grace = self.comeback.grace();
        let keep: Vec<String> = self
            .agents
            .load()
            .values()
            .filter(|a| a.ephemeral && (a.connected || a.lost_at.is_none_or(|t| t.elapsed() <= grace)))
            .map(|a| a.name.clone())
            .collect();
        let gone = match self.store.forget_finished_pods(keep).await {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "finished job pods' records weren't forgotten");
                return Vec::new();
            }
        };
        if !gone.is_empty() {
            self.agents.update(|live| {
                for name in &gone {
                    if live.get(name).is_some_and(|a| a.ephemeral && !a.connected) {
                        live.remove(name);
                    }
                }
            });
            metrics::agent_samples().update(|m| {
                for name in &gone {
                    m.remove(name);
                }
            });
            tracing::info!(pods = gone.len(), "finished job pods' records forgotten");
            self.update_slot_gauges();
        }
        gone
    }

    // ------------------------------------------------------------ reach

    /// Reach's directory as runners and tools read it (`kiln runners`, MCP): [`Hub::reach_directory`],
    /// and the records that are stale (docs/machines.md), so they're shown for a person to forget.
    pub async fn directory(&self) -> Vec<crate::reach::directory::RunnerInfo> {
        let mut out = self.reach_directory();
        for a in self.agent_list().await.into_iter().filter(|a| a.stale) {
            match out.iter_mut().find(|r| r.name == a.name) {
                Some(r) => r.stale = true,
                None => out.push(crate::reach::directory::RunnerInfo { name: a.name, hostname: a.hostname, label: a.label, os: a.os, arch: a.arch, stale: true, ..crate::reach::directory::RunnerInfo::default() }),
            }
        }
        out
    }

    /// Reach's directory (`crate::reach::directory`): every runner that is not a job pod — what it
    /// is, how busy it is, its harnesses, its reach identity and whether it takes links, and the
    /// facts it reported (what `kiln run --needs` checks).
    pub fn reach_directory(&self) -> Vec<crate::reach::directory::RunnerInfo> {
        let kinds = self.harness.kinds();
        let was: HashMap<String, Vec<String>> = {
            let mut m: HashMap<String, Vec<String>> = HashMap::new();
            for (n, id) in self.machine_names.load().iter() {
                m.entry(id.clone()).or_default().push(n.clone());
            }
            m
        };
        let announced = self.reach.announced();
        let agents = self.agents.load();
        let mut out: Vec<crate::reach::directory::RunnerInfo> = agents
            .values()
            .filter(|a| !a.ephemeral)
            .map(|a| {
                let ann = announced.get(&a.name);
                let s = a.sample.as_ref();
                crate::reach::directory::RunnerInfo {
                    name: a.name.clone(),
                    hostname: a.hostname.clone(),
                    label: a.label.clone(),
                    was: was.get(&a.name).cloned().unwrap_or_default(),
                    stale: false,
                    os: a.os.clone(),
                    arch: a.arch.clone(),
                    labels: a.labels.clone(),
                    connected: a.connected,
                    reach: a.connected && ann.is_some_and(|x| x.accept),
                    fingerprint: ann.map(|x| x.fingerprint.clone()),
                    reach_version: ann.map(|x| x.version).filter(|v| *v > 0),
                    cpus: s.map_or(0, |s| s.cpus),
                    cpu_pct: s.map_or(0.0, |s| s.cpu_pct),
                    load1: s.map_or(0.0, |s| s.load1),
                    mem_total: s.map_or(0, |s| s.mem_total),
                    mem_used: s.map_or(0, |s| s.mem_used),
                    slots: a.slots,
                    busy: a.busy,
                    draining: a.draining,
                    harnesses: kinds.get(&format!("agent:{}", a.name)).map(|ks| ks.iter().filter(|k| k.available).map(|k| k.id.clone()).collect()).unwrap_or_default(),
                    via: a.transport.map(|t| t.to_string()).unwrap_or_default(),
                    facts: a.facts.clone(),
                }
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    // ------------------------------------------------------------ network

    /// The channel receivers that take the items of runner stream `stream`: those of the loop the net thread hands its
    /// frames to (`crate::wire::fan::peek`), a session's that of its owner's shard ([`Hub::session_wire`]), the rest the
    /// control loop's ([`Hub::wire`]). Which loop is read off the stream's name as the fan reads it, so each loop takes
    /// its own receivers alone and their lock is never another loop's (goal 10, plan 01a1a87b2dae step 4; goal 5's rule).
    fn links_of(&self, stream: &str) -> &crate::wire::link::Links {
        crate::wire::uplink::session_of(stream).and_then(|id| self.session_wire.get(crate::harness::copies::owner_of(id, self.session_wire.len()))).unwrap_or(&self.wire)
    }

    /// What the server has of `runner`'s channel streams, the hub's loops' together (`crate::wire::fan::have_of`): as the
    /// one loop's receivers said it, sent as its link comes up and every few seconds (docs/wire.md).
    pub fn wire_have(&self, runner: &str) -> crate::wire::channel::Frame {
        crate::wire::fan::have_of(std::iter::once(&self.wire).chain(self.session_wire.iter()), runner)
    }

    /// The hub's loops, each taking what the net thread fanned out to it (`crate::wire::fan`; goal 10, plan 01a1a87b2dae
    /// step 4): the control loop, placed as the one loop was, and a session loop on each shard, beside that shard's owner of
    /// agent sessions' copies, so a session's update goes to its owner on the shard it came to rather than through the
    /// control loop's. Each supervised as the one loop was: one that ends is said, and nothing waits for it any more. The
    /// caller's place is the loops' spawn site, which the shards' table of what held them names (`hyper_rt::slow`).
    #[track_caller]
    pub fn net_loops(self: &Arc<Self>, loops: crate::wire::fan::Loops) {
        let crate::wire::fan::Loops { queues, marks } = loops;
        for (me, rx) in queues.into_iter().enumerate() {
            let (hub, marks) = (Arc::clone(self), Arc::clone(&marks));
            let Some(shard) = me.checked_sub(1) else {
                crate::sup::spawn_guarded("net-events", async move { hub.net_loop(me, rx, marks).await });
                continue;
            };
            crate::rt::spawn_owner(shard, async move {
                match futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(hub.net_loop(me, rx, marks))).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::warn!(task = "net-events", shard, error = %format!("{e:#}"), "task failed"),
                    Err(p) => {
                        crate::metrics::global().panics_caught.inc();
                        tracing::error!(task = "net-events", shard, panic = %crate::sup::panic_message(p.as_ref()), "task panicked; contained");
                    }
                }
            });
        }
    }

    /// One of the hub's loops, `me` among them (`crate::wire::fan`; goal 10, plan 01a1a87b2dae step 4): the control loop
    /// (`crate::wire::fan::CONTROL`), placed as the one loop was, or a shard's session loop, on that shard beside its
    /// owner of agent sessions' copies. Takes transport events forever (supervised), each once its runner's earlier
    /// events on the other loops that it may not overtake are taken (`crate::wire::fan::Marks`), then marks its number
    /// taken, so what waits for it goes. The channel receivers its items are taken with are its own ([`Hub::links_of`]).
    pub async fn net_loop(self: Arc<Self>, me: usize, rx: crate::memory::queue::Receiver<crate::wire::fan::Routed>, marks: Arc<crate::wire::fan::Marks>) -> anyhow::Result<()> {
        // Said ended as it goes, however it goes: nothing waits for it then.
        let _ending = marks.ending(me);
        // Runners keep its channel full: its turns are bounded, or it holds its shard for as long as they
        // send.
        let mut turns = crate::rt::Turns::new();
        // A turn starts afresh with an event this loop waited for (goal 10, plan 01a1a87b2dae step 5).
        while let Some(crate::wire::fan::Routed { ev, n, after, handed }) = turns.next(&rx).await {
            // Its runner's earlier events it may not overtake, taken first where they went: mostly none (goal 10, plan
            // 01a1a87b2dae step 4). A wait that parked gave the shard to its other work, as an empty queue's does, so
            // its turn starts afresh too (step 5).
            let order = if after.is_empty() {
                Duration::ZERO
            } else {
                let dequeued = Instant::now();
                if marks.wait(&after).await {
                    turns = crate::rt::Turns::new();
                }
                dequeued.elapsed()
            };
            // A frame that may hold the shard past what's left of its turn's quantum, its reads there and its handler at the
            // most, waits for the next turn, the shard given up first (goal 9, plan 01a1a87b2cec step 3;
            // `crate::wire::payload::Piece`).
            let piece = Self::piece_of(&ev);
            if let Some(p) = &piece {
                turns.room_for(p.bound_ns()).await;
            }
            // A runner's message: how long it waited for this loop, and how long applying it took
            // (goal 1, plan 01a1a59c2a68 step 2).
            let began = Instant::now();
            let mut arrived = match &ev {
                NetEvent::Frame { at, .. } => Some(Arrived { at: *at, taken: began }),
                _ => None,
            };
            // Its first leg, every frame's: from the network thread handing it over to this loop taking it, but for its
            // wait for its runner's earlier frames on the other loops, a leg of its own (goal 10, plan 01a1a87b2dae
            // step 4).
            if let Some(a) = arrived {
                let to_loop = began.saturating_duration_since(a.at).saturating_sub(order);
                crate::route_latency::observe_leg(crate::route_latency::RunnerLeg::ToLoop, to_loop);
                // The control loop's apart, so the session loops' is told from it; and the net thread's hold of it, part
                // of it, so the net thread's is told from the queue's and the loop's wake.
                if me == crate::wire::fan::CONTROL {
                    crate::route_latency::observe_leg(crate::route_latency::RunnerLeg::ToControl, to_loop);
                }
                crate::route_latency::observe_leg(crate::route_latency::RunnerLeg::Net, handed.saturating_duration_since(a.at));
                if !after.is_empty() {
                    crate::route_latency::observe_leg(crate::route_latency::RunnerLeg::Order, order);
                }
            }
            // Its polls labeled by what came, should one hold the shard: a frame by its message's kind
            // once it's read (goal 9, plan 01a1a87b2cec step 3).
            let label = Self::net_label(&ev);
            // What can't be read is refused and noted where it's read (`unreadable`); what's left is
            // a message read whole that couldn't be applied. What its piece held the shard for is learned as
            // it's taken (goal 9, plan 01a1a87b2cec step 3).
            if let Err(e) = crate::wire::payload::Piece::take(piece, self.on_net_labeled(label, ev, &mut arrived)).await {
                tracing::warn!(error = %format!("{e:#}"), "a runner's message couldn't be applied");
            }
            // Taken: what of its runner's waits for it goes.
            marks.took(me, n);
            // An agent session's update is timed where it's taken, by the shard that owns its copy,
            // from when it came (`Hub::runner_update`): its arrival went there with it.
            if let Some(a) = arrived {
                crate::route_latency::observe(crate::route_latency::Class::Runner, Some(began.saturating_duration_since(a.at)), began.elapsed());
            }
            // How long this loop held the event, its waits included, whatever it was: what the frames queued
            // behind it waited for (goal 10, plan 01a1a87b2dae step 5).
            crate::route_latency::observe_leg(crate::route_latency::RunnerLeg::Held, began.elapsed());
            // Its turn over, its poll ends, whether or not anything else wants the shard now: a poll is a turn at the
            // most (goal 9, plan 01a1a87b2cec step 3; `rt::Turns::end`).
            turns.end().await;
        }
        anyhow::bail!("transport event stream ended")
    }

    /// Whether a frame of `kind` on `lane` is job output: bulk frames are, but on the conversation
    /// feed's and the files' lanes (messages, in bulk).
    pub(crate) fn job_log(kind: Kind, lane: u32) -> bool {
        kind == Kind::Log && lane != proto::LANE_CONVO && lane != proto::LANE_FILES
    }

    /// What [`Hub::net_loop`] labels its polls with for event `ev` in the shards' table of what held
    /// them (`hyper_rt::slow`; goal 9, plan 01a1a87b2cec step 3); a runner's message, read, then by its
    /// kind (`crate::wire::agent_label`).
    fn net_label(ev: &NetEvent) -> &'static hyper_rt::slow::Label {
        use hyper_rt::slow::Label;
        static CONNECTED: Label = Label { name: "runner connected" };
        static CLOSED: Label = Label { name: "runner closed" };
        static LOG: Label = Label { name: "runner job log" };
        static FRAME: Label = Label { name: "runner frame" };
        match ev {
            NetEvent::Connected { .. } => &CONNECTED,
            NetEvent::Closed { .. } | NetEvent::Unreachable { .. } => &CLOSED,
            NetEvent::Frame { kind, lane, .. } if Self::job_log(*kind, *lane) => &LOG,
            NetEvent::Frame { .. } => &FRAME,
        }
    }

    /// What a hub loop awaits for transport event `ev`, its polls labeled `label`: [`Hub::on_net_at`]'s future made boxed,
    /// so the label and the loop's slot each hold a pointer to it, not a copy of it. Goal 9, plan 01a1a87b2cec step 5
    /// (g10s4-loops' 072ac3e6e6f, item fb6c9ad6): carried by value, each runner message's future was copied whole into
    /// each layer as it was made and handed down (the label keeps what it carries twice, as it's handed and as it's
    /// pinned), 50,512 bytes in the test build at rt-next 39fdabc55a1, and the net loop's copies were 2.4% of a shard's CPU
    /// at rt6 (train 16's profile, g9p16fp: about 35% of the shards' memmove samples were the loop's own). The rare
    /// messages' handlers are boxed apart where they're made, so what every message's future holds stays small
    /// (`on_net_at`, `on_agent_msg`, `on_read`).
    fn on_net_labeled<'a>(self: &'a Arc<Self>, label: &'static hyper_rt::slow::Label, ev: NetEvent, arrived: &'a mut Option<Arrived>) -> impl Future<Output = anyhow::Result<()>> + 'a {
        crate::rt::labeled(label, crate::api::boxed(move || self.on_net_at(ev, arrived)))
    }

    /// What [`Hub::net_loop`] takes event `ev` as, a piece of its turn on its shard (`crate::wire::payload::Piece`; goal 9,
    /// plan 01a1a87b2cec step 3): a runner's frame, by what it carries; none for its link coming or going.
    fn piece_of(ev: &NetEvent) -> Option<crate::wire::payload::Piece> {
        match ev {
            NetEvent::Frame { kind, lane, .. } if Self::job_log(*kind, *lane) => Some(crate::wire::payload::Piece::job_log()),
            NetEvent::Frame { data, .. } => Some(crate::wire::payload::Piece::of(data)),
            NetEvent::Connected { .. } | NetEvent::Closed { .. } | NetEvent::Unreachable { .. } => None,
        }
    }

    /// [`Hub::on_net_at`], for the tests, which time nothing.
    #[cfg(test)]
    async fn on_net(self: &Arc<Self>, ev: NetEvent) -> anyhow::Result<()> {
        self.on_net_at(ev, &mut None).await
    }

    /// A transport event, taken. `arrived`: when its frame came and this loop took it, taken by what's timed
    /// elsewhere (an agent session's update, by the shard that owns its copy: `Hub::runner_update`). A message's
    /// handler, and those of a runner going (once a link), are boxed where they're made: what every event's future
    /// holds of them is a pointer (goal 9, plan 01a1a87b2cec step 5; `Hub::on_net_labeled`).
    async fn on_net_at(self: &Arc<Self>, ev: NetEvent, arrived: &mut Option<Arrived>) -> anyhow::Result<()> {
        match ev {
            NetEvent::Connected { peer, name, binding } => {
                self.tunnels.connected(peer, binding);
                // A runner connecting again comes back under the peer id it joined with.
                self.instances.update(|i| i.connected(peer));
                if let Some(n) = name {
                    self.pending_peers.update(|m| m.insert(peer, n.clone()));
                }
            }
            NetEvent::Closed { peer, reason } | NetEvent::Unreachable { peer, reason } => {
                self.tunnels.closed(peer);
                // Its live commands not yet answered end; its parked ones wait for it.
                self.cmds.ask(move |c| c.down(peer)).await;
                // Its reach links end; the runners at their other ends hear so (and resume later), from
                // the relay's owner in its turn.
                self.relay().send(crate::reach::relay::Ask::Lost { peer });
                self.pending_peers.update(|m| m.remove(&peer));
                self.said_machine.update(|m| m.remove(&peer));
                self.peer_certs.update(|m| m.remove(&peer));
                let name = self.peers.update(|m| m.remove(&peer));
                self.relays.lost(peer);
                crate::api::boxed(|| self.handovers_lost(peer)).await;
                let lost = self.instances.update(|i| i.lost(peer));
                if let (Some(name), Some(m)) = (&name, &self.harness.copies) {
                    m.disconnected(name, peer);
                }
                if let Some(name) = name {
                    // Sync moves to another runner process of the machine that syncs, if any.
                    let syncing = self.instances.load().newest_with(&name, proto::CAP_SYNC).is_some();
                    self.sync.machine(&name, syncing);
                    crate::observe::hub::lost(self, &name);
                    if self.instances.load().of(&name).is_empty() {
                        crate::api::boxed(|| self.agent_lost(&name, &reason)).await;
                    } else {
                        // Gone while another of its machine's processes goes on: retired, and its
                        // identity with it (docs/ci.md W27-R01).
                        if let Some(label) = lost.as_ref().map(|(i, _)| i.label())
                            && let Err(e) = crate::identities::runner_retired(self, &name, &label).await
                        {
                            tracing::warn!(agent = %name, error = %format!("{e:#}"), "a retired runner process's identity wasn't revoked");
                        }
                        crate::api::boxed(|| self.instance_lost(&name, peer, lost.map(|(_, owned)| owned).unwrap_or_default(), &reason)).await;
                    }
                }
            }
            NetEvent::Frame { peer, kind, lane, data, .. } if Self::job_log(kind, lane) => match proto::read_whole::<proto::LogChunk>(&data) {
                Ok(chunk) => {
                    if self.owns_job(peer, &chunk.job) {
                        // Written by the job's writer on the blocking pool, in order (each waited for):
                        // a busy disk holds up the next of this loop's messages, never the shard and its
                        // sockets (the HTTP listener among them).
                        self.logs.append_written(&chunk.job, chunk.idx, &chunk.data).await;
                    }
                }
                Err(e) => self.unreadable(peer, Wire::Runner, "a log chunk".to_owned(), e.code(), e.why("LogChunk"), true).await,
            },
            // Read where it's quick to, else off the shard, in order either way (`crate::wire::payload::read_frame`;
            // goal 9, plan 01a1a87b2cec step 3). Whether its runner's Hello came is read off the links' map, its
            // name not copied out of it (step 5).
            NetEvent::Frame { peer, lane, data, .. } => match crate::wire::payload::read_frame(data, self.said_hello(peer)).await {
                // Anything on its control lane before its Hello is out of order: Hello comes first
                // there. On its other lanes a frame may come before its Hello is read (it sends them
                // once it's connected): it's let go, as it was.
                crate::wire::payload::Frame::BeforeHello { what } => {
                    if lane == proto::LANE_CONTROL {
                        self.unreadable(peer, Wire::Runner, what, Code::Malformed, "out of order: before its Hello".to_owned(), true).await;
                    } else {
                        tracing::debug!(peer, lane = proto::lane_name(lane), %what, "a runner's frame before its Hello, on a lane that may outrun it: let go");
                    }
                }
                crate::wire::payload::Frame::Read { label, carried } => crate::rt::labeled(label, crate::api::boxed(move || self.on_carried(peer, carried, arrived))).await?,
                crate::wire::payload::Frame::Unreadable(e) => self.unreadable(peer, Wire::Runner, e.what(), e.code(), e.why("AgentMsg"), true).await,
            },
        }
        Ok(())
    }

    /// A runner's message read whole off one of its lanes: what it carries read too
    /// (`crate::wire::payload_of`), then applied; or, when that can't be read, refused to it
    /// (`Unread`) and noted. The tests' way in: a frame comes through `on_net` (`read_frame`).
    #[cfg(test)]
    async fn on_runner_msg(self: &Arc<Self>, peer: PeerId, msg: AgentMsg) -> anyhow::Result<()> {
        self.on_carried(peer, crate::wire::payload_of(msg), &mut None).await
    }

    /// A runner's message with what it carries read ([`Hub::on_runner_msg`]), applied; or, when that
    /// couldn't be read, refused to it (`Unread`) and noted. `arrived`: as [`Hub::on_net_at`]'s.
    async fn on_carried(self: &Arc<Self>, peer: PeerId, carried: Result<Carried<AgentMsg, FromRunner>, Box<crate::wire::payload::Unreadable>>, arrived: &mut Option<Arrived>) -> anyhow::Result<()> {
        match carried {
            Ok(m) => self.on_agent_msg(peer, m, arrived).await,
            Err(e) => {
                self.unreadable_payload(peer, e, true).await;
                Ok(())
            }
        }
    }

    /// Who sent what came from `peer`, as `crate::wire::unread` names a sender: its runner and which
    /// of its processes, or, before its Hello, what its link says of it.
    fn sender_of(&self, peer: PeerId) -> crate::wire::unread::Sender {
        let instance = self.instances.load().get(peer).and_then(|i| i.id.clone()).unwrap_or_default();
        match self.agent_of(peer) {
            Some(name) => crate::wire::unread::Sender::Runner { name, instance },
            None => crate::wire::unread::Sender::Peer { cert: self.pending_peers.load().get(&peer).cloned().unwrap_or_default(), addr: String::new() },
        }
    }

    /// Something of `peer`'s on `wire` couldn't be read (`crate::wire::unread`): noted; and, when
    /// `tell` and its runner takes word of it (`CAP_UNREAD`), it's told, as often as it may be. Only
    /// what it's told of counts toward what it's told: one answered by a NACK, or noted before its
    /// `Caps`, never changes a `folded`.
    async fn unreadable(&self, peer: PeerId, wire: Wire, what: String, code: Code, why: String, tell: bool) {
        let u = crate::wire::unread::Unread { wire, from: self.sender_of(peer), what, code, why };
        self.refuse_unread(peer, u, None, tell).await;
    }

    /// What a message of `peer`'s carries couldn't be read (`crate::wire::payload`): noted as
    /// [`Hub::unreadable`] notes a frame, and on its agent session's page too for a session's.
    pub(crate) async fn unreadable_payload(&self, peer: PeerId, e: Box<crate::wire::payload::Unreadable>, tell: bool) {
        use crate::wire::payload::Waits;
        // What waits on it is told why: the runner's question answered so, the server's own asker
        // told so.
        match &e.waits {
            Some(Waits::Asking(req)) => self.send_peer(peer, &ServerMsg::ReachAnswer { req: *req, json: e.error().to_string() }).await,
            Some(Waits::Answering(req)) => {
                let waiting = self.file_requests.take(req);
                if let Some(tx) = waiting {
                    let _ = tx.send(Err(e.to_string()));
                }
            }
            // A send is the server's own: the session's inbox answers it, where it runs.
            Some(Waits::Stream(_) | Waits::Opening(_) | Waits::Sending { .. }) | None => {}
        }
        let u = crate::wire::unread::Unread { wire: Wire::RunnerPayload, from: self.sender_of(peer), what: e.what, code: e.code, why: e.why };
        self.refuse_unread(peer, u, e.session.map(crate::wire::unread::Page::Session), tell).await;
    }

    /// `u` noted, on `on`'s page too; and, when `tell` and its runner takes word of it
    /// (`CAP_UNREAD`), it's told, as often as it may be. Only what it's told of counts toward what
    /// it's told: one answered by a NACK, or noted before its `Caps`, never changes a `folded`.
    async fn refuse_unread(&self, peer: PeerId, u: crate::wire::unread::Unread, on: Option<crate::wire::unread::Page>, tell: bool) {
        let u = u.cut();
        let (wire, what, code, why) = (u.wire, u.what.clone(), u.code, u.why.clone());
        let takes = tell && self.instances.load().get(peer).is_some_and(|i| i.wide & proto::CAP_UNREAD != 0);
        if !takes {
            self.note_unread(u, on).await;
        } else if let Some(folded) = self.note_unread_told(u, on).await {
            self.send_peer(peer, &ServerMsg::Unread { wire, what, code, why, folded }).await;
        }
    }

    /// [`AgentMsg::Power`]: a machine's battery. Kept with the machine (the Runners page shows it);
    /// a machine on battery that gets low is said once, as it gets there, to whoever is watching
    /// (the UI) and to webhook subscribers (`machine.battery_low`), before it sleeps and its runner
    /// with it.
    async fn on_power(&self, agent: &str, b: crate::os::battery::Battery) {
        let Some((changed, was_low, snapshot)) = self.change_agent(agent, |a| {
            let was_low = a.battery.as_ref().is_some_and(crate::os::battery::Battery::low);
            let changed = a.battery.as_ref() != Some(&b);
            a.battery = Some(b.clone());
            (changed, was_low, a.clone())
        }) else {
            return;
        };
        if changed {
            self.emit_agent(&snapshot);
        }
        if b.low() && !was_low {
            let left = b.minutes.map(|m| format!(", about {m} min left")).unwrap_or_default();
            let what = format!("{} is on battery at {}%{left}: plug it in, or it sleeps and its runner with it", snapshot.shown(), b.percent);
            tracing::warn!(machine = %agent, percent = b.percent, minutes = ?b.minutes, "a machine on battery is low");
            self.store.audit("kiln".into(), "machine.battery_low".into(), what.clone()).await;
            self.emit(&ServerEvent::MachineBatteryLow { machine: agent.to_owned(), battery: b, text: what });
        }
    }

    fn agent_of(&self, peer: PeerId) -> Option<String> {
        self.peers.load().get(&peer).cloned()
    }

    /// Whether runner process `peer`'s Hello was taken: [`Hub::agent_of`] without its name copied out (goal 9, plan
    /// 01a1a87b2cec step 5: asked of every frame on the hub's loop).
    fn said_hello(&self, peer: PeerId) -> bool {
        self.peers.load().contains_key(&peer)
    }

    /// Whether `job` is one of the CI engine's legs: one it knows, or, while it rebuilds its runs
    /// after a restart (knowing none yet), one the store has (its message waits for it then).
    async fn ci_leg(&self, job: &str) -> bool {
        self.ci.is_leg(job) || (self.ci.enabled() && self.ci.restored().is_none() && self.store.ci_legs(vec![job.to_owned()]).await.is_ok_and(|l| l.contains(job)))
    }

    fn owns_job(&self, peer: PeerId, job: &str) -> bool {
        let Some(name) = self.agent_of(peer) else { return false };
        // A failed leg it keeps: a step rerun in place reports as the leg (W17-R04).
        self.agents.load().get(&name).is_some_and(|a| a.running.iter().any(|j| j == job)) || self.ci.kept_by(job, &name)
    }

    async fn agent_lost(&self, name: &str, reason: &str) {
        // Agents leaving as the server stops are expected.
        if self.shutdown.is_cancelled() {
            tracing::info!(agent = name, reason, "agent disconnected");
        } else {
            tracing::warn!(agent = name, reason, "agent disconnected");
        }
        let lost_at = Instant::now();
        let Some((away, snapshot)) = self.change_agent(name, |a| {
            a.connected = false;
            a.lost_at = Some(lost_at);
            (a.peer.take(), a.clone())
        }) else {
            return;
        };
        if let Some(p) = away {
            self.away_peers.update(|m| m.insert(name.to_owned(), p));
        }
        if let Err(e) = self.store.agent_disconnected(name.to_owned()).await {
            tracing::warn!(error = %e, "agent_disconnected");
        }
        // That machine's terminals keep running there; park them until it reconnects. One that
        // keeps the channel sends their output again once it's back, so they stay open meanwhile
        // (docs/wire.md).
        let sessions: Vec<String> = if self.wire.keeps(name) {
            Vec::new()
        } else {
            self.terms.load().keys().filter(|k| k.starts_with(&format!("{name}/"))).cloned().collect()
        };
        for s in sessions {
            if let Some(tx) = self.terms.update(|m| m.remove(&s)) {
                crate::termsess::forward(&tx, None, TermOut::Gone);
            }
        }
        metrics::agent_samples().update(|m| m.remove(name));
        // Its agent sessions keep running there too; they are announced again on reconnect.
        self.harness.machine_lost(self, name).await;
        if self.ai_sessions.update(|m| m.remove(name)).is_some() {
            self.emit(&ServerEvent::AiSessions { machine: name.to_owned() });
        }
        self.update_slot_gauges();
        self.emit_agent(&snapshot);
        // Connections drop transiently (network blips, server restarts): every agent, pods
        // included, gets its grace to reconnect and reclaim its jobs (as long as runners took to
        // come back, `crate::comeback`). A pod that is really gone (deleted, evicted, OOM-killed) is
        // requeued right away.
        if snapshot.ephemeral && !snapshot.running.is_empty() {
            let gone = match self.k8s.get().and_then(Option::as_ref) {
                Some(k) => !k.agent_pod_alive(name).await,
                None => false,
            };
            if gone {
                self.requeue_agent_jobs(name, "agent pod is gone").await;
                self.agents.update(|m| m.remove(name));
            }
        }
    }

    async fn requeue_agent_jobs(&self, name: &str, reason: &str) {
        let jobs: Vec<String> = self
            .change_agent(name, |a| {
                a.busy = 0;
                a.exclusive = false;
                a.held.clear();
                std::mem::take(&mut a.running)
            })
            .unwrap_or_default();
        if jobs.iter().any(|j| self.ci.is_leg(j)) {
            self.ci.send_held(crate::ci::dispatch::Msg::RunnerLost { agent: name.to_owned() }).await;
        }
        for j in jobs.into_iter().filter(|j| !self.ci.is_leg(j)) {
            match self.store.requeue_job(j.clone(), format!("{reason} ({})", self.shown(name))).await {
                Ok(true) => tracing::info!(job = %j, agent = name, "job requeued"),
                Ok(false) => tracing::warn!(job = %j, agent = name, "job failed: out of retries"),
                Err(e) => tracing::error!(error = %e, job = %j, "requeue failed"),
            }
            self.logs.append(&j, 0, format!("\r\n\x1b[33m[kiln] {reason}; job requeued\x1b[0m\r\n").as_bytes());
            self.emit_job(&j).await;
        }
        self.update_slot_gauges();
        self.wake.notify();
    }

    /// A frame of a runner's channel (docs/wire.md). An item is applied as the message it carries,
    /// then answered: taken, or refused saying why. Never answered before it's applied, or for an agent
    /// session's update, before the shard that owns its copy has it in its inbox (`Hub::runner_update`).
    /// `arrived`: as [`Hub::on_net_at`]'s.
    async fn on_wire(self: &Arc<Self>, peer: PeerId, agent: &str, frame: crate::wire::channel::Frame, arrived: &mut Option<Arrived>) {
        use crate::wire::channel::{Arrival, Frame, Refusal};
        match frame {
            Frame::Item { stream, seq, low, body } => {
                // Taken with its stream's loop's own receivers: this loop's (goal 10, plan 01a1a87b2dae step 4).
                let links = self.links_of(&stream);
                let answer = match links.on_item(agent, &stream, seq, low, body, Instant::now()) {
                    Arrival::Take(body) => {
                        // Read where it's quick to, else off the shard, in order either way, as a frame is
                        // (`crate::wire::payload::read_item`; goal 9, plan 01a1a87b2cec step 3).
                        let refusal = match crate::wire::payload::read_item(body).await {
                            crate::wire::payload::Item::Read { carried: Ok(Carried::Nothing(AgentMsg::Wire(_))), .. } => {
                                let why = "a channel frame inside a channel item".to_owned();
                                self.unreadable(peer, Wire::RunnerItem, "Wire".to_owned(), Code::Malformed, why.clone(), false).await;
                                Some((Refusal::Malformed, why))
                            }
                            // An agent session's update, most of what the channel carries: handed on as `on_read` hands
                            // one on, without a runner message's whole handling again (its runner was read off its frame
                            // already, and heard), so with nothing made for it on the heap (goal 9, plan 01a1a87b2cec step
                            // 5). Labeled as the item's kind, as the rest are.
                            crate::wire::payload::Item::Read { label, carried: Ok(Carried::Read(FromRunner::Harness { session, update, taken })) } => {
                                crate::rt::labeled(label, self.on_session_update(peer, agent, session, update, taken, arrived)).await;
                                None
                            }
                            // Its polls labeled by the kind of message it carries. Its handler boxed inside the
                            // label, so the label holds a pointer to it, not a copy of it twice (goal 9, plan
                            // 01a1a87b2cec step 5; `Hub::on_net_labeled`). Boxed as its own type, not as a
                            // `dyn Future + Send` (`crate::api::boxed`): it holds this future in turn, so whether
                            // it's Send can be shown only where the loops are spawned, not inside this one.
                            crate::wire::payload::Item::Read { label, carried: Ok(m) } => crate::rt::labeled(label, Box::pin(self.on_agent_msg(peer, m, arrived))).await.err().map(|e| (Refusal::Refused, format!("{e:#}"))),
                            // What it carries can't be read: its NACK says so, as for an item
                            // that can't be read, and it's noted.
                            crate::wire::payload::Item::Read { carried: Err(e), .. } => {
                                let refusal = (e.code.refusal(), e.why.clone());
                                self.unreadable_payload(peer, e, false).await;
                                Some(refusal)
                            }
                            // Its NACK says it: unsupported for a variant this build doesn't know.
                            crate::wire::payload::Item::Unreadable(e) => {
                                let why = e.why("AgentMsg");
                                self.unreadable(peer, Wire::RunnerItem, e.what(), e.code(), why.clone(), false).await;
                                Some((e.code().refusal(), why))
                            }
                        };
                        Some(links.answer(agent, &stream, seq, refusal))
                    }
                    Arrival::Answered(a) => a,
                    Arrival::Gap(want) => Some(want),
                };
                if let Some(a) = answer {
                    self.send_peer(peer, &ServerMsg::Wire(a)).await;
                }
            }
            // The runner's answers to the server's commands, its wants, and what it has of them
            // after a reconnect: what's due goes now.
            frame @ (Frame::Ack(_) | Frame::Want { .. } | Frame::Have(_)) => {
                self.cmds.ask(move |c| c.on_frame(peer, frame)).await;
                self.flush_commands().await;
            }
        }
    }

    /// A runner's message, read. `arrived`: as [`Hub::on_net_at`]'s. The rare messages' handlers that are larger
    /// than what the frequent ones' need (a hello, a process taking over, a job's end, a hand-over's pieces, what
    /// comes with its Caps, a terminal's end) are boxed where they're made: this future, every message's, holds a
    /// pointer to each, not room for the largest (goal 9, plan 01a1a87b2cec step 5; `Hub::on_net_labeled`).
    async fn on_agent_msg(self: &Arc<Self>, peer: PeerId, msg: Carried<AgentMsg, FromRunner>, arrived: &mut Option<Arrived>) -> anyhow::Result<()> {
        if let Carried::Nothing(AgentMsg::Hello(h)) = msg {
            return crate::api::boxed(|| self.on_hello(peer, h)).await;
        }
        // Says which runner process of its machine this is; precedes Hello. Again later: a new
        // build that has taken over (blue/green) says it's the newest now.
        // The machine it is, said before its Hello (docs/machines.md, "One record a machine"): what
        // its Hello is keyed by when its certificate carries a name from before.
        if let Carried::Nothing(AgentMsg::Machine { id }) = msg {
            if self.agent_of(peer).is_none()
                && let Some(id) = crate::join::machine_key(Some(&id))
            {
                self.said_machine.update(|m| m.insert(peer, id.clone()));
            }
            return Ok(());
        }
        if let Carried::Nothing(AgentMsg::Instance(ih)) = msg {
            let again = self.instances.update(|i| i.reannounce(peer, &ih));
            match again {
                Some(drain) => crate::api::boxed(|| self.instance_promoted(peer, drain)).await,
                None => self.instances.update(|i| i.announce(peer, ih.clone())),
            }
            return Ok(());
        }
        let Some(agent) = self.agent_of(peer) else {
            anyhow::bail!("message from peer {peer} before hello");
        };
        // When it was last heard, to the second: its record is copied anew once a second at most,
        // not once a message.
        let now = crate::util::now_unix();
        if self.agents.load().get(&agent).is_some_and(|a| a.last_seen != now) {
            self.change_agent(&agent, |a| a.last_seen = now);
        }
        // A runner process that keeps the channel says so (its Caps) right after its Build, before
        // anything else on its control lane. One that says its facts or its upgrade preference
        // first is from before the channel: what waited for it goes its way now, however long the
        // server took to hear it (docs/wire.md, "Commands").
        if matches!(msg, Carried::Read(FromRunner::Facts(_)) | Carried::Nothing(AgentMsg::UpdatePrefs { .. }))
            && let Some(waited) = self.cmds.ask(move |c| c.is_coming(peer).then(|| c.not_wire(peer))).await.flatten()
        {
            self.send_waited(peer, waited).await;
        }
        let msg = match msg {
            Carried::Nothing(m) => m,
            Carried::Read(read) => return self.on_read(peer, &agent, read, arrived).await,
        };
        match msg {
            AgentMsg::Hello(_) | AgentMsg::Instance(_) | AgentMsg::Machine { .. } => {}
            // What carries JSON came read (`wire::payload_of`), to `on_read`.
            AgentMsg::AiSessions { .. }
            | AgentMsg::LegResults { .. }
            | AgentMsg::LegSignature { .. }
            | AgentMsg::Custody { .. }
            | AgentMsg::Identity { .. }
            | AgentMsg::Observe { .. }
            | AgentMsg::HeadroomStatus { .. }
            | AgentMsg::AiTranscript { .. }
            | AgentMsg::Harness { .. }
            | AgentMsg::HarnessInventory { .. }
            | AgentMsg::AiView { .. }
            | AgentMsg::HarnessTaskOutput { .. }
            | AgentMsg::DirList { .. }
            | AgentMsg::HarnessHistory { .. }
            | AgentMsg::Sync { .. }
            | AgentMsg::FrontDoorStatus { .. }
            | AgentMsg::JobEvent { .. }
            | AgentMsg::Facts { .. }
            | AgentMsg::ReachHello { .. }
            | AgentMsg::ReachAsk { .. }
            | AgentMsg::Convo { .. }
            | AgentMsg::Power { .. }
            | AgentMsg::Answer { .. }
            | AgentMsg::HarnessFileData { .. }
            | AgentMsg::StreamEvent { .. }
            | AgentMsg::Executor { .. }
            | AgentMsg::Undeployed { .. }
            | AgentMsg::Cluster { .. }
            | AgentMsg::WorkState { .. }
            | AgentMsg::AgentLimit { .. } => {}
            AgentMsg::Heartbeat { .. } | AgentMsg::JobStarted { .. } => {}
            AgentMsg::Sample(s) => {
                self.change_agent(&agent, |a| a.sample = Some(s.clone()));
                self.record_sample(&agent, s);
                // The CI engine weighs its headroom by it: once a sample, as it comes.
                self.ci.send_held(crate::ci::dispatch::Msg::RunnerSample { agent: agent.clone() }).await;
            }
            AgentMsg::StepStarted { job, idx } => {
                if self.owns_job(peer, &job) {
                    self.store.step_update(job.clone(), idx, Status::Running, None).await?;
                    self.logs.step(&job, idx, "running");
                    self.emit_job(&job).await;
                }
            }
            AgentMsg::StepFinished { job, idx, status, exit_code } => {
                if self.owns_job(peer, &job) {
                    self.store.step_update(job.clone(), idx, status, exit_code).await?;
                    self.logs.step(&job, idx, status.as_str());
                    metrics::global().steps_finished.inc(&[status.as_str()]);
                    self.emit_job(&job).await;
                }
            }
            AgentMsg::JobSample { .. } => {}
            AgentMsg::JobFinished(r) => {
                let job = r.job.clone();
                if self.owns_job(peer, &r.job) {
                    if self.ci_leg(&r.job).await {
                        self.ci.send_held(crate::ci::dispatch::Msg::Finished { agent: agent.clone(), result: r }).await;
                    } else {
                        crate::api::boxed(|| self.job_done(&agent, r)).await?;
                    }
                }
                // Heard (taken, or not this runner's to give any more): it needn't send it again.
                if self.instances.load().get(peer).is_some_and(|i| i.caps & proto::CAP_RECEIVED != 0) {
                    self.send_peer(peer, &ServerMsg::Received { job }).await;
                }
            }
            AgentMsg::Reject { job, .. } if self.ci.is_leg(&job) => {
                if self.owns_job(peer, &job) {
                    self.ci.send_held(crate::ci::dispatch::Msg::Rejected { agent: agent.clone(), leg: job }).await;
                }
            }
            AgentMsg::Reject { job, reason } => {
                if self.owns_job(peer, &job) {
                    self.release_slot(&agent, &job);
                    self.declined.update(|d| d.insert((job.clone(), agent.clone()), Instant::now()));
                    self.store.unclaim_job(job.clone(), format!("declined by {}: {reason}", self.shown(&agent))).await?;
                    self.emit_job(&job).await;
                    self.wake.notify();
                }
            }
            // Handed to the terminal's relay without waiting: no room is a gap the relay repairs
            // (goal 3, plan 01a1a59c2a8e step 5; `crate::termsess::forward`).
            AgentMsg::TermOutput { session, data } => {
                let key = format!("{agent}/{session}");
                let tx = self.terms.load().get(&key).cloned();
                if let Some(tx) = tx {
                    crate::termsess::forward(&tx, self.terminals.get(&session).as_deref(), TermOut::Data(data));
                }
            }
            AgentMsg::TermList { sessions } => {
                self.instances.update(|inst| {
                    for info in &sessions {
                        inst.own(crate::instances::Owned::Terminal(info.session.clone()), peer);
                    }
                });
                for info in &sessions {
                    crate::termsess::adopt_agent(self, &agent, info);
                }
            }
            // A streamed page's chunks (`crate::harness::streams`): to its viewers.
            AgentMsg::StreamFrame { stream, data } => self.harness.streams.chunk(&agent, &stream, data),
            // Connections to its ports (`crate::harness::opened`).
            AgentMsg::PortOpened { conn, error } => self.harness.ports.opened(&agent, conn, error),
            AgentMsg::PortData { conn, data } => self.harness.ports.data(&agent, conn, data),
            AgentMsg::PortAck { conn, bytes } => self.harness.ports.ack(&agent, conn, bytes),
            AgentMsg::PortClose { conn, error } => self.harness.ports.ended(&agent, conn, error),
            // In pieces, in their order, its shard given up between them (`build`; goal 9, plan 01a1a87b2cec
            // step 3).
            AgentMsg::Build { build, caps } => Box::pin(self.on_build(peer, &agent, build, caps)).await,
            AgentMsg::UpdatePrefs { accept } => {
                self.updates.fleet.set_accept(&agent, accept);
                self.updates.fleet.wake();
                self.emit(&ServerEvent::Updates);
            }
            // What it can do past Build's 32 bits. One that keeps the channel too is told what the
            // server has of its streams, and sends from there (docs/wire.md).
            AgentMsg::Caps { caps } => {
                self.instances.update(|i| i.set_wide(peer, caps));
                crate::api::boxed(|| crate::headroom::control::on_caps(self, &agent, caps)).await;
                // One that takes hand-overs: its machine's older processes hand their sessions on.
                if caps & proto::CAP_HANDOVER != 0 {
                    crate::api::boxed(|| self.check_handover()).await;
                }
                self.wire.set_keeps(&agent, caps & proto::CAP_WIRE != 0);
                if caps & proto::CAP_WIRE != 0 {
                    // One that takes an agent session's commands on the session's own streams gets them there (runner-fan).
                    let lanes = caps & proto::CAP_SESSION_STREAMS != 0;
                    self.cmds.ask(move |c| {
                        c.lanes(peer, lanes);
                        c.connected(peer);
                    })
                    .await;
                    self.send_peer(peer, &ServerMsg::Wire(self.wire_have(&agent))).await;
                } else {
                    let waited = self.cmds.ask(move |c| c.not_wire(peer)).await.unwrap_or_default();
                    self.send_waited(peer, waited).await;
                }
                // One that takes its agent limit from here is sent what's set for its machine.
                crate::api::boxed(|| crate::harness::limit::on_caps(self, &agent, peer, caps)).await;
            }
            AgentMsg::Wire(frame) => self.on_wire(peer, &agent, frame, arrived).await,
            AgentMsg::Reach { to, link, open, data, close } => {
                // Job pods neither send nor receive reach links: one's frame is refused, and noted.
                if self.agents.load().get(&agent).is_none_or(|a| a.ephemeral) {
                    let why = format!("a job pod's, for link {link}: job pods neither send nor receive reach links");
                    self.unreadable(peer, Wire::Relay, "Reach".to_owned(), Code::Malformed, why, true).await;
                    return Ok(());
                }
                // Handed to the relay's owner, which forwards it in its turn: this loop goes on to the
                // next runner's message.
                self.relay().send(crate::reach::relay::Ask::Frame { from: agent.clone(), peer, frame: crate::reach::relay::Frame { to, link, open, data, close } });
            }
            AgentMsg::NetDrops { dropped } => {
                let series = dropped.iter().map(|d| (vec![agent.clone(), d.lane.clone(), d.reason.clone()], i64::try_from(d.frames).unwrap_or(i64::MAX)));
                metrics::global().runner_net_dropped.replace_owned(&std::collections::BTreeSet::from([agent.clone()]), series);
                self.change_agent(&agent, |a| a.dropped.clone_from(&dropped));
            }
            // Hand-overs between a machine's runner processes (`crate::handovers`).
            AgentMsg::HarnessHandOver { session, offset, data, done } => crate::api::boxed(|| self.handover_piece(&agent, peer, session, offset, data, done)).await,
            AgentMsg::HarnessAdopted { session, error } => crate::api::boxed(|| self.handover_adopted(&agent, peer, session, error)).await,
            // A frame of the server's this runner couldn't read: noted against it, and on the page
            // of the session it was about when it holds that session now. (A send it couldn't read
            // its session's inbox there answers, as it answers any.) It's the runner's word, so
            // nothing goes back, and it's no part of what the runner is told.
            AgentMsg::Unread { wire, what, code, why, folded, session } => {
                let held = session.filter(|s| self.harness.located_on(s, &agent) && self.instances.load().owner(&crate::instances::Owned::Agent(s.clone())).is_none_or(|q| q == peer));
                let on = held.map(crate::wire::unread::Page::Session);
                let u = crate::wire::unread::Unread { wire, from: self.sender_of(peer), what, code, why };
                self.note_unread_folded(u, on, folded).await;
            }
            // A message on a reach link this runner couldn't read: noted against the runner that
            // sent it, the link's other end as the relay knows it, on this one's page too. Word of
            // one about a link the relay doesn't know with those ends is refused, and noted.
            AgentMsg::LinkUnread { from, link, what, code, why } => {
                // The relay's owner answers after the frames handed it before this.
                let asked = link.clone();
                let ends = self.relay().ask(move |r| r.ends(&asked)).await.flatten();
                if ends.as_ref().is_some_and(|(a, b)| (*a == agent && *b == from) || (*a == from && *b == agent)) {
                    let sender = crate::wire::unread::Sender::Runner { name: from, instance: String::new() };
                    let u = crate::wire::unread::Unread { wire: Wire::ReachLink, from: sender, what, code, why: format!("{why} (as {agent} read it, on link {link})") };
                    let _ = self.note_unread(u, Some(crate::wire::unread::Page::Runner(agent.clone()))).await;
                } else {
                    let why = format!("word of what link {link} couldn't read, of {from}'s, where the relay knows no such link between them");
                    self.unreadable(peer, Wire::Relay, "LinkUnread".to_owned(), Code::Malformed, why, true).await;
                }
            }
            AgentMsg::UpgradeLog { job, line } => self.updates.on_log(self, &job, line),
            AgentMsg::UpgradeDone { job, ok, error } => self.updates.on_done(self, &job, ok, error),
            AgentMsg::TermReplay { session, data } => {
                let key = format!("{agent}/{session}");
                let tx = self.terms.load().get(&key).cloned();
                if let Some(tx) = tx {
                    crate::termsess::forward(&tx, self.terminals.get(&session).as_deref(), TermOut::Replay(data));
                }
            }
            AgentMsg::TermExit { session, code, error } => {
                let key = format!("{agent}/{session}");
                let tx = self.terms.update(|m| m.remove(&key));
                if let Some(tx) = tx {
                    crate::termsess::forward(&tx, None, TermOut::Exit { code, error });
                }
                let what = crate::instances::Owned::Terminal(session);
                self.instances.update(|i| i.disown(&what));
                crate::api::boxed(|| self.check_retire()).await;
            }
        }
        Ok(())
    }

    /// An agent session's update from runner process `r.peer`, judged and taken by the shard that owns
    /// the session's copy (`s`), in the order its runner sent it (goal 10, plan 01a1a87b2dae step 4).
    /// The hub's loop judged every runner's updates itself, on the HTTP listener's shard, behind every
    /// runner's other messages: the lab's runner wait at p99 was 155 to 195 ms. What it starts that waits
    /// (a runner told to let go of a copy, custody's ask, a hand-over's checks) goes on beside it. Its
    /// wait, from its frame's arrival, and the time it took here are the runner class's
    /// (`crate::route_latency`).
    /// What's left of it when the owner's turn ran out (`over`; goal 9, plan 01a1a87b2cec step 3) comes
    /// back, for the owner to take first at its next turn.
    pub(crate) fn runner_update(self: &Arc<Self>, s: &mut crate::harness::copies::Shard, r: crate::harness::copies::RunnerUpdate, over: &dyn Fn(u64) -> bool) -> Option<crate::harness::copies::Ingest> {
        let began = Instant::now();
        let crate::harness::copies::RunnerUpdate { peer, agent, session, update, shipped, arrived, handed, taken } = r;
        // Its third leg: from the hub's loop handing it over to its owner taking it (goal 10, plan 01a1a87b2dae
        // step 4).
        if let Some(h) = handed {
            crate::route_latency::observe_leg(crate::route_latency::RunnerLeg::ToOwner, began.saturating_duration_since(h));
        }
        let rest = self.judge_runner_update(s, peer, &agent, &session, update, shipped, over);
        let done = Instant::now();
        crate::route_latency::observe(crate::route_latency::Class::Runner, arrived.map(|at| began.saturating_duration_since(at)), done.saturating_duration_since(began));
        // The owner's own time with it, its frames published, to the microsecond: the first of a live frame's legs past
        // its runner's wait (goal 10, plan 01a1a87b2dae step 5).
        crate::route_latency::observe_frame(crate::route_latency::FrameLeg::Apply, done.saturating_duration_since(began));
        // Its runner's hold of it, as its runner said it, and the server's time with it, from its frame's arrival
        // to its owner done with it: each read on one clock, its own side's (goal 10, plan 01a1a87b2dae step 5).
        if let Some(t) = taken {
            crate::route_latency::observe_leg(crate::route_latency::RunnerLeg::Runner, t);
            if let Some(at) = arrived {
                crate::route_latency::observe_leg(crate::route_latency::RunnerLeg::Server, done.saturating_duration_since(at));
            }
        }
        rest
    }

    /// [`Hub::runner_update`]'s judgment, as the hub's loop made it: whether the update counts, which of
    /// its machine's processes holds the session, then the copy takes it.
    #[allow(clippy::too_many_arguments)]
    fn judge_runner_update(self: &Arc<Self>, s: &mut crate::harness::copies::Shard, peer: PeerId, agent: &str, session: &str, update: crate::harness::core::Update, shipped: bool, over: &dyn Fn(u64) -> bool) -> Option<crate::harness::copies::Ingest> {
        use crate::harness::core::Update;
        match update {
            // A copy of a session that went on elsewhere since (an older epoch): its machine lets go
            // of it, and nothing it says of it counts.
            Update::Open { info, .. } if self.harness.stale(session, info.epoch) => {
                tracing::info!(%agent, %session, epoch = info.epoch, "an agent session that went on elsewhere: this machine lets go of its copy");
                // A runner from before letting go stops its copy instead (what it says of it doesn't
                // count either way).
                let lets_go = self.instances.load().get(peer).is_some_and(|i| i.caps & proto::CAP_CONVO != 0);
                let msg = if lets_go { ServerMsg::HarnessRelease { session: session.to_owned() } } else { ServerMsg::HarnessClose { session: session.to_owned() } };
                let (hub, agent) = (Arc::clone(self), agent.to_owned());
                crate::rt::spawn(async move { hub.command_peers(&agent, vec![peer], &msg).await.log_when_answered(LOG_WITHIN) });
                None
            }
            // The rest of what a copy left behind says (its machine was told to let go of it): none
            // of it counts, and it doesn't take the session's messages from where it runs.
            u if !matches!(u, Update::Open { .. }) && !self.harness.runs_on(session, agent) => None,
            u => {
                let closed = matches!(u, Update::Closed);
                // Two processes of one machine never both run a session: of two that say they hold
                // it, the newer keeps it and the older lets go (a hand-over whose word came late,
                // `crate::handovers`). Read off the runner processes' copy; one is swapped in only
                // when the session's holder changes (it opens here, moves here, or closes), the
                // decision taken again on the copy that's changed. A process whose link has closed
                // since it sent this holds nothing: its disconnect, taken on the hub's loop before
                // this, let go of what it held (`Instances::lost`), as it did when this was judged
                // there first.
                let linked = self.peers.load().contains_key(&peer);
                let what = crate::instances::Owned::Agent(session.to_owned());
                let decide = |inst: &crate::instances::Instances| {
                    if !linked {
                        return (None, None, false);
                    }
                    let other = inst.owner(&what).filter(|q| *q != peer && inst.hands_over(*q) && inst.hands_over(peer));
                    match other.and_then(|q| inst.newer_of(q, peer).map(|newer| (q, newer))) {
                        Some((q, newer)) if newer == q => (None, Some(peer), false),
                        pair => {
                            let change = if closed { inst.owner(&what).is_some() } else { inst.owner(&what) != Some(peer) };
                            (inst.get(peer).map(crate::instances::Instance::label).filter(|_| !closed), pair.map(|(q, _)| q), change)
                        }
                    }
                };
                let (holder, older, change) = decide(&self.instances.load());
                let (holder, older) = if change {
                    self.instances.update(|inst| {
                        let (holder, older, change) = decide(inst);
                        if change && closed {
                            inst.disown(&what);
                        } else if change {
                            inst.own(what.clone(), peer);
                        }
                        (holder, older)
                    })
                } else {
                    (holder, older)
                };
                // Not one a hand-over is taking there now: the newer says so once it took it.
                if let Some(older) = older.filter(|_| !self.handovers.lock().going_to(session, peer)) {
                    let (hub, agent, session) = (Arc::clone(self), agent.to_owned(), session.to_owned());
                    crate::rt::spawn(async move { hub.let_go_of(&agent, older, &session).await });
                }
                if older == Some(peer) {
                    return None;
                }
                if let Some(label) = holder {
                    self.harness.held_by(session, &label);
                }
                let opened = matches!(u, Update::Open { .. });
                let rest = self.harness.take_from(s, self, &crate::harness::registry::Location::Machine(agent.to_owned()), session, u, shipped, over);
                // Opened: the work kiln handed it as it started is asked to be signed for, as it takes
                // it up (docs/ci.md W27-R08).
                if opened {
                    let (hub, id) = (Arc::clone(self), session.to_owned());
                    crate::rt::spawn(async move { crate::custody::ask_open(&hub, &id).await });
                }
                // Let go of by its copy: what's held is read from the copies.
                if closed {
                    let hub = Arc::clone(self);
                    crate::rt::spawn(async move {
                        hub.check_retire().await;
                        hub.check_handover().await;
                    });
                }
                rest
            }
        }
    }

    /// Runner process `peer`'s update to its agent session `session` (`FromRunner::Harness`), on a frame of its own or a
    /// channel item ([`Hub::on_wire`]), its runner `agent` heard. `arrived`: as [`Hub::on_net_at`]'s.
    async fn on_session_update(self: &Arc<Self>, peer: PeerId, agent: &str, session: String, update: Box<crate::harness::core::Update>, taken: Option<Duration>, arrived: &mut Option<Arrived>) {
        match *update {
            // An inbox's answer counts wherever it came from (it's for the message sent there),
            // and says nothing of where the session runs: whoever waits for it is told, and
            // the session's messages still go where it runs.
            u @ crate::harness::core::Update::Inbox { .. } => {
                self.harness.ingest_from(&crate::harness::registry::Location::Machine(agent.to_owned()), &session, u, false).await;
            }
            // The rest goes to the shard that owns the session's copy, which judges it there and
            // takes it, in the order its runner sent it (goal 10, plan 01a1a87b2dae step 4;
            // `Hub::runner_update`). This loop goes on with the runner's next word as soon as that
            // owner's inbox has it, and that's when a channel item is answered (`Hub::on_wire`).
            u => {
                let lifecycle = matches!(u, crate::harness::core::Update::Open { .. } | crate::harness::core::Update::Closed);
                // A runner process that sends its conversation store: the server keeps its
                // sessions from that.
                let shipped = self.instances.load().get(peer).is_some_and(|i| i.caps & proto::CAP_CONVO != 0);
                // Its second leg: from this loop taking its frame to handing it over (goal 10, plan
                // 01a1a87b2dae step 4); the third is the owner's to time.
                let handed = Instant::now();
                let arrival = arrived.take();
                if let Some(a) = arrival {
                    crate::route_latency::observe_leg(crate::route_latency::RunnerLeg::InLoop, handed.saturating_duration_since(a.taken));
                }
                // An Open says where the session runs now, a Closed lets it go: what this loop reads
                // of it next (the runner's next word, its inventory, a hand-over) waits for its owner
                // to have taken it, as it did when this loop took it itself. Only theirs keep the
                // session's id for that: the rest hand theirs on, not a copy (goal 9, plan
                // 01a1a87b2cec step 5).
                let applied = lifecycle.then(|| session.clone());
                let r = crate::harness::copies::RunnerUpdate { peer, agent: agent.to_owned(), session, update: u, shipped, arrived: arrival.map(|a| a.at), handed: Some(handed), taken };
                self.harness.from_runner(r).await;
                if let Some(id) = applied {
                    self.harness.applied(&id).await;
                }
            }
        }
    }

    /// What a runner's message carried, read where its frame was (`crate::wire::payload_of`): each
    /// handler takes its value. `arrived`: as [`Hub::on_net_at`]'s. As in [`Hub::on_agent_msg`], the rare
    /// messages' larger handlers (a front door's status, its facts' fold, its power, an identity's ask, a
    /// leg's records and signature, what its inventory lets go of) are boxed where they're made (goal 9, plan
    /// 01a1a87b2cec step 5).
    async fn on_read(self: &Arc<Self>, peer: PeerId, agent: &str, read: FromRunner, arrived: &mut Option<Arrived>) -> anyhow::Result<()> {
        match read {
            // One report per machine: its newest runner process's.
            FromRunner::AiSessions(_) if self.instances.load().newest(agent).is_some_and(|n| n != peer) => {}
            FromRunner::AiSessions(sessions) => {
                let sessions = Arc::new(sessions);
                let old = self.ai_sessions.update(|m| m.insert(agent.to_owned(), Arc::clone(&sessions)));
                if old.as_deref() != Some(&*sessions) {
                    self.emit(&ServerEvent::AiSessions { machine: agent.to_owned() });
                }
            }
            // An answer, read as its question asks for: one that can't be is refused to the runner,
            // and its asker told why.
            FromRunner::Answer(a) => {
                let asked = self.ai_requests.take(&a.req);
                if let Some(read) = asked
                    && let Err(e) = read(a.of, a.json)
                {
                    self.unreadable_payload(peer, e, true).await;
                }
            }
            FromRunner::HarnessFileData { req, answer, data } => {
                if let Some(tx) = self.file_requests.take(&req) {
                    let _ = tx.send(Ok((answer, data)));
                }
            }
            // A streamed page's events (`crate::harness::streams`): to its viewers.
            FromRunner::StreamEvent { stream, event } => {
                let after = self.harness.streams.event(agent, &stream, event);
                // What it said of a live page that can't be read went to no viewer: refused to it
                // (told, if it takes word of it), noted, on its page and the stream's session's.
                if let Some(r) = after.refused {
                    use crate::wire::unread::{Page, Unread, Wire};
                    let session = self.harness.streams.get(&stream).map(|l| Page::Session(l.session.clone()));
                    let what = if r.of == "text" { "StreamEvent".to_owned() } else { format!("StreamEvent {}", r.of) };
                    self.refuse_unread(peer, Unread { wire: Wire::RunnerPayload, from: self.sender_of(peer), what, code: r.code, why: r.why }, session, true).await;
                }
                // A recording that ended: its files kept in kiln's store of bytes.
                if let Some((session, rec)) = after.ended {
                    self.harness.streams.keep(self, session, rec);
                }
                // Opened: the size its viewer asked for while it was being set up, asked again.
                if let Some((live, again)) = after.again {
                    live.control(self, &again).await;
                }
            }
            FromRunner::Sync { msg, raft } => self.sync_heard(agent, *msg, raft),
            FromRunner::Cluster { msg, raft } => match (self.store.group(), *msg) {
                // Where kiln.db stands, for a read the runner answers once its replica holds it;
                // and whether its learner checks the log's checkpoints (docs/ci.md W27-R08).
                (Some(g), crate::store::learner::Msg::Ask { id, checks }) => {
                    if let Some(holds) = checks {
                        self.cluster.checks(g, agent, peer, holds);
                    }
                    let at = crate::store::learner::Msg::At { id, seq: g.standing().0 };
                    if let Ok(b) = proto::encode(&ServerMsg::Cluster { json: crate::store::learner::encode(&at), raft: Vec::new() }) {
                        self.net.send(peer, proto::LANE_SYNC, Kind::Control, b).await;
                    }
                }
                (Some(g), msg) => self.cluster.heard(g, agent, &msg, raft),
                (None, _) => {}
            },
            FromRunner::Convo { msg, data } => {
                // About the server's own store: a mirror's answer. A mirror's copy sent back to
                // restore it from, when asked for. Else the runner's own store's feed.
                match (&self.harness.copies, *msg) {
                    // What a runner's store is doing, for the board.
                    (_, crate::convo::feed::Msg::Stats { store, report }) => self.harness.board.heard(agent, &store, *report, crate::util::now_ms()),
                    (_, crate::convo::feed::Msg::Hello { store, restore: true }) => {
                        if self.harness.restore_accepts(agent, &store)
                            && let Some(f) = &self.harness.feed
                        {
                            f.accept_restore(&store);
                            f.heard(peer, agent, crate::convo::feed::Msg::Hello { store, restore: true }, data);
                        }
                    }
                    (_, crate::convo::feed::Msg::Restored { store, .. }) => self.harness.restore_done(self, agent, &store),
                    (Some(m), msg) if msg.store() == m.store_id() => m.heard(agent, msg),
                    (_, msg) => {
                        if let Some(f) = &self.harness.feed {
                            f.heard(peer, agent, msg, data);
                        }
                    }
                }
            }
            FromRunner::FrontDoorStatus(status) => crate::api::boxed(|| crate::frontdoor::control::on_status(self, peer, agent, *status)).await,
            FromRunner::HeadroomStatus(status) => crate::headroom::control::on_status(self, peer, agent, status),
            // CI (docs/ci.md §2.5): the dispatcher's; facts are W13's.
            FromRunner::JobEvent { job, event } => {
                if self.owns_job(peer, &job) {
                    self.ci.send_held(crate::ci::dispatch::Msg::Event { agent: agent.to_owned(), leg: job, event }).await;
                }
            }
            // A pool's executor it was asked to start (docs/ci.md W13-R09).
            FromRunner::Executor(said) => self.ci.send_held(crate::ci::dispatch::Msg::Executor { agent: agent.to_owned(), said }).await,
            // A preview's deployment it was asked to take down (docs/ci.md W15-R07).
            FromRunner::Undeployed(u) => crate::ci::previews::undeployed(self, agent, u).await,
            // What it holds warm, for placement (docs/ci.md W24-R18); a job's pod holds nothing.
            FromRunner::WorkState(state) => {
                if !self.agents.load().get(agent).is_none_or(|a| a.ephemeral) {
                    self.ci.send_held(crate::ci::dispatch::Msg::WorkState { agent: agent.to_owned(), state }).await;
                }
            }
            // The kiln it runs (`crate::fleet`); the rest is W13's.
            FromRunner::Facts(f) => {
                if let Some(k) = f.kiln.clone() {
                    // How far behind the server's kiln it is: to be upgraded, it takes no new work.
                    self.skew_check(agent, peer, Some(k.commit.clone()));
                    crate::fleet::remember(self, &k.build, &k.target, &k.commit);
                    self.instances.update(|i| i.set_kiln(peer, k.clone()));
                    self.updates.late_by_commit(self, agent);
                    self.updates.fleet.wake();
                }
                // A probe's whole report (a runner that only said its kiln has no CPU count):
                // routing decides over it from now on (docs/ci.md W13-R01).
                if f.cpu.threads > 0 {
                    // The machine it is, kept with its name for the joins to come
                    // (`crate::join::free_name`); a job's pod is its node's, and goes.
                    let machine = f.machine_id.clone();
                    let known = self.change_agent(agent, |a| {
                        a.facts = Some(f.clone());
                        !a.ephemeral
                    });
                    if known.is_some() {
                        self.ci.send_held(crate::ci::dispatch::Msg::RunnerFacts { agent: agent.to_owned() }).await;
                    }
                    let kept = known.unwrap_or(false);
                    if let Some(m) = machine.filter(|_| kept) {
                        if let Err(e) = self.store.set_agent_machine(agent.to_owned(), m.clone()).await {
                            tracing::warn!(agent = %agent, error = %format!("{e:#}"), "couldn't keep the runner's machine id");
                        }
                        // A machine known by its id saying so: its other records fold into it
                        // (docs/machines.md, "Duplicates fold into the machine's id").
                        if crate::join::machine_key(Some(&m)).as_deref() == Some(agent)
                            && let Err(e) = crate::api::boxed(|| self.fold_duplicates(agent)).await
                        {
                            tracing::warn!(agent = %agent, error = %format!("{e:#}"), "couldn't fold this machine's other records");
                        }
                    }
                    self.note_machine(agent);
                }
            }
            FromRunner::ReachHello(a) => {
                // Job pods neither send nor receive reach links.
                if !self.agents.load().get(agent).is_none_or(|r| r.ephemeral) {
                    tracing::info!(agent = %agent, fingerprint = %crate::reach::e2e::short(&a.fingerprint), accept = a.accept, "reach identity");
                    self.relay().announce(agent, a);
                }
            }
            FromRunner::ReachAsk { req, ask, raw } => match *ask {
                crate::reach::Question::Directory => {
                    let answer = serde_json::json!({ "runners": self.directory().await });
                    self.send_peer(peer, &ServerMsg::ReachAnswer { req, json: answer.to_string() }).await;
                }
                // What the runner holds of other runners' trees and let go: kept, and answered
                // with the rules it weighs them with (docs/reach.md §5.1).
                crate::reach::Question::Keep { report } => {
                    let (hub, runner) = (Arc::clone(self), agent.to_owned());
                    crate::rt::spawn(async move {
                        let answer = hub.copies.heard(&hub, &runner, report.as_ref()).await;
                        hub.send_peer(peer, &ServerMsg::ReachAnswer { req, json: answer.to_string() }).await;
                    });
                }
                // Which runner holds which object of the runners' caches (docs/ci.md W24-R27).
                crate::reach::Question::Holds { repo, digest } => {
                    crate::reach::directory::holds(agent, &repo, &digest);
                    self.send_peer(peer, &ServerMsg::ReachAnswer { req, json: serde_json::json!({}).to_string() }).await;
                }
                crate::reach::Question::Holders { repo, digest } => {
                    let up: Vec<String> = crate::reach::directory::holders(&repo, &digest, agent).into_iter().filter(|r| self.agents.load().get(r).is_some_and(|a| a.connected)).collect();
                    self.send_peer(peer, &ServerMsg::ReachAnswer { req, json: serde_json::json!({ "runners": up }).to_string() }).await;
                }
                // A runner's process that takes reach, as kiln names it (its identity's URI), for
                // a custody record of what passes between two runners (docs/ci.md W27-R08).
                crate::reach::Question::Custody { runner } => {
                    let instance = {
                        let inst = self.instances.load();
                        inst.newest_with(&runner, proto::CAP_REACH).and_then(|p| inst.get(p).map(|i| i.label()))
                    };
                    let hub = Arc::clone(self);
                    crate::rt::spawn(async move {
                        let uri = match instance {
                            Some(instance) => crate::identities::runner_identity(&hub, &runner, &instance).await.ok().and_then(|i| i.uri().ok()),
                            None => None,
                        };
                        hub.send_peer(peer, &ServerMsg::ReachAnswer { req, json: serde_json::json!({ "uri": uri }).to_string() }).await;
                    });
                }
                // An agent's MCP server on the runner asks for kiln's operations: answered on
                // their own, so a slow one holds up nothing else the runner says. The runner's
                // record of passing an act on is placed on its stream here, as its messages come in
                // order (docs/ci.md W27-R08's relays).
                ask => {
                    let placed = self.relays.placed(peer, &raw);
                    let (hub, runner) = (Arc::clone(self), agent.to_owned());
                    crate::rt::spawn(async move {
                        let answer = crate::mcp::runner_ask(&hub, &runner, peer, &ask, &raw, placed).await;
                        hub.send_peer(peer, &ServerMsg::ReachAnswer { req, json: answer.to_string() }).await;
                    });
                }
            },
            FromRunner::Power(b) => crate::api::boxed(|| self.on_power(agent, b)).await,
            FromRunner::AgentLimit(held) => crate::harness::limit::on_held(self, peer, agent, held),
            // An identity it asks for, for a key it made and keeps (docs/ci.md W27-R01).
            FromRunner::Identity(ask) => crate::api::boxed(|| crate::identities::on_ask(self, peer, agent, ask)).await,
            // A leg's records, signed as the leg on its runner at its end (docs/ci.md W27-R05).
            FromRunner::LegResults { job, bundles } => {
                if self.owns_job(peer, &job) {
                    crate::api::boxed(|| crate::ci::signed::taken(self, agent, &job, bundles)).await;
                }
            }
            // The provenance kiln wrote of what a leg built, signed as the leg, or why not (W27-R06).
            FromRunner::LegSignature { job, nonce, bundle, error } => {
                if self.owns_job(peer, &job) {
                    crate::api::boxed(|| crate::ci::attest::leg_signed(self, agent, &job, &nonce, bundle, error)).await;
                }
            }
            // A signed custody record of one of its sessions' hand-overs (docs/ci.md W27-R08): read, checked and kept
            // off this loop, one at a time in the order its runner sent them, by custody's intake (goal 9, plan
            // 01a1a87b2cec step 3; `crate::custody::runners`), the process it came over named as it is now.
            FromRunner::Custody(record) => {
                let instance = self.instances.load().get(peer).map(crate::instances::Instance::label);
                self.custody_intake().send(crate::custody::runners::Sent { peer, machine: agent.to_owned(), instance, record }).await;
            }
            // Its replica's word in observe's group (docs/observe.md §5).
            FromRunner::Observe { msg, bytes } => crate::observe::hub::heard(agent, *msg, bytes),
            FromRunner::Harness { session, update, taken } => self.on_session_update(peer, agent, session, update, taken, arrived).await,
            FromRunner::HarnessInventory(inv) => {
                let mut inv = *inv;
                // A copy it kept of a session that went on elsewhere since isn't its own: it's
                // told to let go of it now, at its runner's first word since, not once the copy
                // announces it again (W27-R08: the twin of 2026-10-25 ran on for ten minutes).
                // One just arriving, its Open on its way, is kept.
                let left = self.harness.left_behind(agent, &inv);
                inv.sessions.retain(|id| self.harness.runs_on(id, agent));
                for id in &left {
                    crate::api::boxed(|| crate::mcp::let_go(self, agent, peer, id)).await;
                }
                // This process's sessions are its own; another process of the machine keeps
                // the ones it holds (the registry forgets sessions a report leaves out), and
                // so does one that isn't connected now (`Registry::holder`).
                // Work kiln handed them that no receipt answers yet is asked for again.
                let (hub, held) = (Arc::clone(self), inv.sessions.clone());
                crate::rt::spawn(async move { crate::custody::ask_again(&hub, peer, &held).await });
                let (me, present, others) = self.instances.update(|inst| {
                    for id in &inv.sessions {
                        inst.own(crate::instances::Owned::Agent(id.clone()), peer);
                    }
                    inst.reported(peer);
                    let others: Vec<String> = inst
                        .of(agent)
                        .into_iter()
                        .filter(|p| *p != peer)
                        .flat_map(|p| inst.owned_by(p))
                        .filter_map(|o| match o {
                            crate::instances::Owned::Agent(id) => Some(id),
                            _ => None,
                        })
                        .collect();
                    let present: Vec<String> = inst.of(agent).into_iter().filter_map(|p| inst.get(p).map(crate::instances::Instance::label)).collect();
                    (inst.get(peer).map(crate::instances::Instance::label), present, others)
                });
                if let Some(me) = &me {
                    self.harness.held_by_all(&inv.sessions, me);
                }
                let own = inv.sessions.clone();
                inv.sessions.extend(others);
                self.harness.inventory(self, agent, me.as_deref(), &present, inv).await;
                // Those it holds that still read disconnected a little later: its link dropped and
                // came back, and they've had nothing to say since. Their snapshots bring them back
                // as they are. Not at once: after a server restart every session reads disconnected
                // until its stream's snapshot comes, which a request now would only build twice.
                if !self.harness.still_disconnected(agent, &own).is_empty() {
                    let (hub, machine, held) = (Arc::clone(self), agent.to_owned(), own.clone());
                    crate::rt::spawn(async move {
                        crate::rt::sleep(STALE_GRACE).await;
                        if !hub.agents.load().get(&machine).is_some_and(|a| a.connected) {
                            return;
                        }
                        let stale = hub.harness.still_disconnected(&machine, &held);
                        if stale.is_empty() {
                            return;
                        }
                        tracing::info!(%machine, sessions = stale.len(), "sessions its runner holds still read disconnected after it reconnected: asking for their snapshots");
                        // One at a time: each is the session's whole timeline, built on its runner.
                        for id in stale {
                            let sent = hub.harness.command(&hub, &id, crate::harness::model::Command::Snapshot).await;
                            tracing::debug!(session = %id, ?sent, "asked a session still read disconnected for its snapshot");
                        }
                    });
                }
                // Hand-overs dropped as it went away get their word now it's back.
                crate::api::boxed(|| self.handovers_dropped(agent, peer, &own)).await;
                crate::api::boxed(|| self.check_retire()).await;
            }
        }
        Ok(())
    }

    async fn on_hello(&self, peer: PeerId, h: Hello) -> anyhow::Result<()> {
        let cert_name = self.pending_peers.load().get(&peer).cloned();
        let refuse = |reason: String| async move {
            tracing::warn!(peer, %reason, "agent refused");
            self.send_peer(peer, &ServerMsg::Refused { reason }).await;
        };
        if h.protocol != proto::VERSION {
            refuse(format!("protocol {} != server {}", h.protocol, proto::VERSION)).await;
            return Ok(());
        }
        let name = crate::pki::sanitize_name(&h.name);
        if cert_name.as_deref() != Some(name.as_str()) {
            refuse(format!("hello name {name} does not match certificate {cert_name:?}")).await;
            return Ok(());
        }
        // Keyed by its machine id from here on, whatever name its certificate carries
        // (docs/machines.md, "One record a machine").
        let name = match self.machine_of_connection(peer, &name, &h).await {
            Ok(id) => id,
            Err(why) => {
                refuse(why).await;
                return Ok(());
            }
        };
        if h.name != name {
            self.peer_certs.update(|m| m.insert(peer, crate::pki::sanitize_name(&h.name)));
        }
        let mut h = h;
        h.name = name.clone();
        let now = crate::util::now_unix();
        // It says in a moment whether it keeps the channel (its Caps follow its hello and Build):
        // until then its commands wait for that (docs/wire.md, "Commands"), from before it can be
        // sent any. A job pod never says.
        if !h.ephemeral {
            self.cmds.ask(move |c| c.coming(peer, Instant::now())).await;
        }
        // A machine may connect more than one runner process (a new one beside an old one that
        // keeps its terminals, see `crate::instances`). The same process on a new connection
        // replaces its stale one.
        let joined = self.instances.update(|i| i.join(peer, &name, h.clone(), now));
        for p in &joined.stale {
            self.peers.update(|m| m.remove(p));
            self.lost_instances.update(|l| l.remove(p));
            self.net.command(crate::net::Out::Disconnect { peer: *p }).await;
        }
        self.lost_instances.update(|l| l.remove(&peer));
        let others: Vec<PeerId> = self.instances.load().of(&name).into_iter().filter(|p| *p != peer).collect();
        let held_by_other = |job: &str| {
            self.instances.load().owner(&crate::instances::Owned::Job(job.to_owned())).is_some_and(|o| o != peer && others.contains(&o))
        };
        // Reconcile what the agent says it runs with what we think it runs: the ids of the jobs the
        // store has running on it, the rest of their rows unread (the DB query audit, 2026-11-06).
        let db_running: Vec<String> = self.store.running_on(name.clone()).await?;
        let mut keep = Vec::new();
        for id in &db_running {
            if h.running.contains(id) {
                keep.push(id.clone());
            } else if held_by_other(id) {
                // Another runner process of this machine runs it.
            } else {
                self.store
                    .requeue_job(id.clone(), format!("agent {} restarted without the job", self.shown(&name)))
                    .await?;
                self.emit_job(id).await;
            }
        }
        // Until the CI engine has rebuilt its runs after a restart it knows none of its legs: those
        // the runner says it has are its own still, for the dispatcher to re-adopt or stop once it
        // has (`dispatch::spawn`), never taken back as v1's jobs (which would cancel them).
        let rebuilding = if self.ci.enabled() && self.ci.restored().is_none() {
            let said: Vec<String> = h.running.iter().filter(|j| !keep.contains(j)).cloned().collect();
            self.store.ci_legs(said).await.unwrap_or_default()
        } else {
            HashSet::new()
        };
        for j in &h.running {
            if keep.contains(j) {
                continue;
            }
            // CI's legs are the dispatcher's to re-adopt, or to stop if they were given up while
            // the runner was away (`Msg::RunnerUp` below, W05-R05).
            if self.ci.is_leg(j) || rebuilding.contains(j) {
                keep.push(j.clone());
                continue;
            }
            // The server requeued this job while the agent was away, but the agent kept working:
            // take it back (claim) rather than throwing the work away. Only a job that has since
            // been taken by someone else (or settled) is cancelled on this agent.
            let executor = if h.ephemeral { "k8s" } else { "baremetal" };
            if self.store.claim_job(j.clone(), name.clone(), executor.into()).await? {
                tracing::info!(job = %j, agent = %name, "agent reclaimed a requeued job");
                self.launching.update(|l| l.remove(j));
                keep.push(j.clone());
                self.emit_job(j).await;
            } else {
                self.command_peers(&name, vec![peer], &ServerMsg::Cancel { job: j.clone(), reason: "job was reassigned".into() }).await.log_when_answered(LOG_WITHIN);
            }
        }
        let outpost = is_outpost(&h.labels, h.slots);
        let label = self.store.agent_label(name.clone()).await.ok().flatten();
        let live = AgentLive {
            name: name.clone(),
            label,
            peer: Some(peer),
            labels: h.labels.clone(),
            // An outpost takes no jobs: it keeps no slots (`pick_agent`, CI's dispatch).
            slots: if outpost { 0 } else { h.slots.max(1) },
            outpost,
            hostname: h.hostname.clone(),
            os: h.os.clone(),
            arch: h.arch.clone(),
            version: h.version.clone(),
            ephemeral: h.ephemeral,
            connected: true,
            last_seen: now,
            enrolled_at: now,
            busy: u32::try_from(keep.len()).unwrap_or(u32::MAX),
            running: keep,
            draining: false,
            sample: None,
            battery: None,
            dropped: Vec::new(),
            dropped_toward: Vec::new(),
            refused: None,
            terminal: h.terminal,
            dedicated_job: h.dedicated_job.clone(),
            exclusive: false,
            held: Vec::new(),
            lost_at: None,
            facts: None,
            instances: Vec::new(),
            transport: None,
            skew: crate::skew::Skew::default(),
            stale: false,
        };
        let prev = self.agents.load().get(&name).map(|a| AgentLive::clone(a));
        // Back still running jobs: how long it took to come back (`crate::comeback`), since the server lost it, or,
        // one this server hasn't seen since it started (it ran them through the restart), since the start.
        let came_back = match &prev {
            _ if h.running.is_empty() => None,
            Some(p) if !p.connected => p.lost_at.map(|t| t.elapsed()),
            Some(_) => None,
            None => Some(self.started.elapsed()),
        };
        if let Some(after) = came_back {
            let kept = self.comeback.came_back(after);
            if let Err(e) = self.store.kv_set(crate::comeback::KEY.to_owned(), kept).await {
                tracing::warn!(error = %format!("{e:#}"), "how long runners take to come back wasn't kept for the next start");
            }
        }
        // Its facts stay until its new connection's probe reports.
        let prev_facts = prev.as_ref().and_then(|a| a.facts.clone());
        // Its battery as last said, until it says again.
        let prev_battery = prev.as_ref().and_then(|a| a.battery.clone());
        let draining = prev.as_ref().is_some_and(|a| a.draining);
        self.instances.update(|inst| {
            for j in &live.running {
                inst.own(crate::instances::Owned::Job(j.clone()), peer);
            }
        });
        // The machine as a whole: its newest process describes it; its jobs are every process's.
        let mut running = live.running.clone();
        for j in prev.iter().flat_map(|a| a.running.iter()) {
            if !running.contains(j) && held_by_other(j) {
                running.push(j.clone());
            }
        }
        let busy = u32::try_from(running.len()).unwrap_or(u32::MAX);
        // What its jobs still running hold of it, as before (W05-R03).
        let held: Vec<(String, kiln_ci::sched::state::Resources)> = prev.iter().flat_map(|a| a.held.iter()).filter(|(j, _)| running.contains(j)).cloned().collect();
        let exclusive = prev.as_ref().is_some_and(|a| a.exclusive) && !running.is_empty();
        let live = match prev {
            Some(prev) if !joined.newest => AgentLive { connected: true, last_seen: now, running, busy, lost_at: None, ..prev },
            _ => AgentLive { draining, running, busy, facts: prev_facts, battery: prev_battery, held, exclusive, ..live },
        };
        self.store
            .upsert_agent(AgentRow {
                name: name.clone(),
                labels: live.labels.clone(),
                slots: live.slots,
                hostname: live.hostname.clone(),
                os: live.os.clone(),
                arch: live.arch.clone(),
                version: live.version.clone(),
                ephemeral: live.ephemeral,
                connected: true,
                last_seen: now,
                enrolled_at: now,
            })
            .await?;
        self.peers.update(|m| m.insert(peer, name.clone()));
        self.pending_peers.update(|m| m.remove(&peer));
        self.put_agent(live.clone());
        self.note_machine(&name);
        // A machine known by its id connecting: its other records fold into it (docs/machines.md).
        if !live.ephemeral
            && let Err(e) = self.fold_duplicates(&name).await
        {
            tracing::warn!(agent = %name, error = %format!("{e:#}"), "couldn't fold this machine's other records");
        }
        if let Some(j) = &h.dedicated_job {
            self.launching.update(|l| l.remove(j));
        }
        // It keeps the channel (docs/wire.md): said first, so a runner that keeps it too knows on
        // Welcome not to send its sessions whole. An older runner skips the frame.
        self.send_peer(peer, &ServerMsg::Caps { caps: proto::CAP_WIRE | proto::CAP_NET_DROPS | proto::CAP_HANDOVER | proto::CAP_WORK | proto::CAP_AGENT_LIMIT | proto::CAP_IDENTITY | proto::CAP_HEADROOM | proto::CAP_CHANGES_PATCH | proto::CAP_UNREAD | proto::CAP_AGENT_GIT | proto::CAP_LEG_SIGNS | proto::CAP_CUSTODY | proto::CAP_RELAYS }).await;
        // What its sessions' copies have here, so a stream it starts sends only what they lack
        // (`crate::wire::sums`): a few dozen sessions a frame, each about a kilobyte. Once each
        // copy's owner has applied what this loop handed it before (`crate::harness::copies`): a
        // copy said to have less than it has would let go of the rest when the runner's Open
        // comes, sending its viewers back to the session whole.
        self.harness.settled().await;
        for part in self.harness.kept_on(&name).chunks(KEPT_A_FRAME) {
            self.send_peer(peer, &ServerMsg::Kept { sessions: part.to_vec() }).await;
        }
        self.send_peer(
            peer,
            &ServerMsg::Welcome {
                server_version: env!("CARGO_PKG_VERSION").into(),
                sample_seconds: self.cfg.telemetry.sample_seconds,
            },
        )
        .await;
        // One that hosts no agent sessions (no harness: an ephemeral or terminal-less runner) sends no inventory: it's
        // back holding none, for an attach waiting as the server starts (`crate::harness::returning`).
        if h.ephemeral || !h.terminal {
            self.harness.returning.reported(&name, Vec::new());
        }
        tracing::info!(agent = %name, labels = ?live.labels, slots = live.slots, "agent connected");
        self.ci.send_held(crate::ci::dispatch::Msg::RunnerUp { agent: name.clone(), running: h.running.clone() }).await;
        // Older processes of this machine take no new jobs; what they run, they keep.
        for p in &joined.drain {
            tracing::info!(agent = %name, peer = p, "a newer runner process of this machine connected; the older one keeps what it runs and takes nothing new");
            self.command_peers(&name, vec![*p], &ServerMsg::Drain).await.log_when_answered(LOG_WITHIN);
        }
        self.update_slot_gauges();
        self.emit_agent(&live);
        self.wake.notify();
        self.check_retire().await;
        self.check_handover().await;
        Ok(())
    }

    /// One runner process of a machine went away while another keeps the machine connected (see
    /// `crate::instances`): what it held comes back when it reconnects, like a machine's.
    async fn instance_lost(&self, name: &str, peer: PeerId, owned: Vec<crate::instances::Owned>, reason: &str) {
        use crate::instances::Owned;
        tracing::info!(agent = name, peer, reason, "a runner process of this machine disconnected; another keeps it connected");
        let (mut agents, mut jobs) = (Vec::new(), Vec::new());
        for o in owned {
            match o {
                Owned::Terminal(s) => {
                    if let Some(tx) = self.terms.update(|m| m.remove(&format!("{name}/{s}"))) {
                        crate::termsess::forward(&tx, None, TermOut::Gone);
                    }
                }
                Owned::Agent(id) => agents.push(id),
                Owned::Job(j) => jobs.push(j),
            }
        }
        if !agents.is_empty() {
            self.harness.sessions_lost(self, &agents).await;
        }
        if !jobs.is_empty() {
            self.lost_instances.update(|l| l.insert(peer, (name.to_owned(), Instant::now(), jobs.clone())));
        }
        self.newest_changed(name).await;
    }

    /// A runner process of `peer`'s machine, joined as the oldest (a blue/green upgrade's new
    /// build), has taken over and is its newest now: the others take no new jobs, and new work
    /// goes to it.
    async fn instance_promoted(&self, peer: PeerId, drain: Vec<PeerId>) {
        let Some(name) = self.agent_of(peer) else { return };
        tracing::info!(agent = %name, peer, "a new runner process of this machine took over: new work goes to it");
        for p in &drain {
            self.command_peers(&name, vec![*p], &ServerMsg::Drain).await.log_when_answered(LOG_WITHIN);
        }
        self.newest_changed(&name).await;
        // The copy of the conversation store a mirror keeps goes with its machine's newest process:
        // its feed goes on there now, not once the machine connects again (the old one handed it the
        // copy, `crate::bluegreen::Frame::Stores`, or it starts one).
        self.mirrors_seen();
        self.wake.notify();
    }

    /// The machine's newest process (maybe another one now) describes it, and runs its front door.
    async fn newest_changed(&self, name: &str) {
        let newest = {
            let inst = self.instances.load();
            inst.newest(name).and_then(|p| inst.get(p).map(|i| (p, i.hello.clone())))
        };
        let Some(snapshot) = self.change_agent(name, |a| {
            if let Some((p, h)) = &newest {
                a.peer = Some(*p);
                a.outpost = is_outpost(&h.labels, h.slots);
                a.labels.clone_from(&h.labels);
                a.slots = if a.outpost { 0 } else { h.slots.max(1) };
                a.hostname.clone_from(&h.hostname);
                a.os.clone_from(&h.os);
                a.arch.clone_from(&h.arch);
                a.version.clone_from(&h.version);
                a.terminal = h.terminal;
            }
            a.clone()
        }) else {
            return;
        };
        self.emit_agent(&snapshot);
        self.emit_terminals();
        self.update_slot_gauges();
        self.check_retire().await;
        self.check_handover().await;
        // The front door moves to the process that is newest now.
        crate::frontdoor::control::on_instance_lost(self, name).await;
        // So does the replica: a blue/green upgrade's new build was told to let it go as it joined
        // (ranked oldest), and nothing else tells it to hold it now (until 2026-10-25 a machine
        // upgraded so held no replica until its runner next connected).
        self.cluster_announced(name).await;
        // And observe's: the newest replicates it (the group's messages go to it), and the build it
        // took over from lets its own go. Until then that one held it, hearing nothing, until it
        // ended (its jobs finished), and the new build waited for its folder.
        crate::observe::hub::announced(self, name).await;
    }

    /// A machine's runner processes, while it has more than one, with what each holds.
    fn instance_views(&self, name: &str) -> Vec<crate::instances::InstanceView> {
        if self.instances.load().of(name).len() < 2 {
            return Vec::new();
        }
        self.runner_processes(name)
    }

    /// Every runner process of a machine and what each one holds (System → Updates lists
    /// them always; agent views only while a machine has more than one).
    /// What a machine's newest runner process says it can do ([`crate::proto::CAP_UPGRADE`] …);
    /// 0 for runners from before builds were announced.
    pub fn runner_caps(&self, name: &str) -> u32 {
        let inst = self.instances.load();
        inst.newest(name).and_then(|p| inst.get(p)).map_or(0, |i| i.caps)
    }

    /// Whether a machine's newest runner process has said its build and what it can do (`Build`,
    /// which every runner but a pod sends as it connects, once it's worked its build out).
    pub fn runner_built(&self, name: &str) -> bool {
        let inst = self.instances.load();
        inst.newest(name).and_then(|p| inst.get(p)).is_some_and(|i| i.build.is_some())
    }

    /// What a machine's newest runner can do in 64 bits: `Build`'s 32 and the rest
    /// ([`proto::AgentMsg::Caps`]: [`proto::CAP_WIRE`] ...).
    pub fn runner_wide_caps(&self, name: &str) -> u64 {
        let inst = self.instances.load();
        inst.newest(name).and_then(|p| inst.get(p)).map_or(0, |i| u64::from(i.caps) | i.wide)
    }

    /// The kiln a machine's newest runner process says it runs (`crate::fleet`).
    pub fn runner_kiln(&self, name: &str) -> Option<kiln_ci::facts::KilnFacts> {
        let inst = self.instances.load();
        inst.newest(name).and_then(|p| inst.get(p)).and_then(|i| i.kiln.clone())
    }

    pub fn runner_processes(&self, name: &str) -> Vec<crate::instances::InstanceView> {
        use crate::instances::Owned;
        let target = format!("agent:{name}");
        let terms: Vec<Owned> = self.terminals.list().into_iter().filter(|t| t.target == target && !t.exited).map(|t| Owned::Terminal(t.id)).collect();
        // Its agent sessions by id, none of their infos copied (goal 9, plan 01a1a87b2cec step 3).
        let agents: Vec<Owned> = self.harness.ids_on(&target).into_iter().map(Owned::Agent).collect();
        let jobs: Vec<Owned> = self.agents.load().get(name).map(|a| a.running.iter().cloned().map(Owned::Job).collect()).unwrap_or_default();
        let inst = self.instances.load();
        inst.views(name, |p| {
            let held = |v: &[Owned]| v.iter().filter(|o| inst.owner(o) == Some(p)).count();
            (held(&terms), held(&agents), held(&jobs))
        })
    }

    /// A machine's newest runner process retires the older ones once they hold nothing: no live
    /// terminal, agent session or job of the machine is held by any process but the newest.
    /// Anything whose holder is unknown counts as held by the older ones, so nothing is retired
    /// early (and the newest checks the old process itself before stopping it).
    pub(crate) async fn check_retire(&self) {
        use crate::instances::Owned;
        let crowded = self.instances.load().crowded();
        if crowded.is_empty() {
            return;
        }
        let mut sends = Vec::new();
        for (name, peer) in crowded {
            let target = format!("agent:{name}");
            let mut held: Vec<Owned> =
                self.terminals.list().into_iter().filter(|t| t.target == target && !t.exited).map(|t| Owned::Terminal(t.id)).collect();
            // Its agent sessions by id, none of their infos copied: every session's whole info was, for each machine
            // with an older process (goal 9, plan 01a1a87b2cec step 3).
            held.extend(self.harness.ids_on(&target).into_iter().map(Owned::Agent));
            held.extend(self.agents.load().get(&name).into_iter().flat_map(|a| a.running.iter().cloned().map(Owned::Job)));
            let now = Instant::now();
            let send = self.instances.update(|inst| {
                let newest = inst.newest(&name)?;
                if peer == newest || held.iter().any(|o| inst.owner(o) != Some(newest)) {
                    return None;
                }
                let i = inst.get_mut(peer)?;
                // Not before it said what it holds: just connected (the server started again, or it
                // did), its agent sessions aren't known here yet, and it would look as if it held none.
                if !i.reported || i.retire_sent.is_some_and(|t| t.elapsed() < RETIRE_REPEAT) {
                    return None;
                }
                i.retire_sent = Some(now);
                Some((newest, i.label()))
            });
            if let Some((newest, instance)) = send {
                sends.push((name, newest, instance));
            }
        }
        for (name, newest, instance) in sends {
            tracing::info!(agent = %name, %instance, "an older runner process of this machine holds nothing any more; asking the newest to retire it");
            self.command_peers(&name, vec![newest], &ServerMsg::Retire { instance }).await.log_when_answered(LOG_WITHIN);
        }
    }

    pub(crate) fn release_slot(&self, agent: &str, job: &str) {
        let what = crate::instances::Owned::Job(job.to_owned());
        self.instances.update(|i| i.disown(&what));
        self.change_agent(agent, |a| {
            a.running.retain(|j| j != job);
            a.held.retain(|(j, _)| j != job);
            a.busy = u32::try_from(a.running.len()).unwrap_or(0);
            if a.running.is_empty() {
                a.exclusive = false;
                a.held.clear();
            }
        });
    }

    async fn job_done(&self, agent: &str, r: JobResult) -> anyhow::Result<()> {
        self.release_slot(agent, &r.job);
        self.cancel_sent.update(|c| c.remove(&r.job));
        let job = self.store.job(r.job.clone()).await?;
        let (run_id, settled) = self
            .store
            .finish_job(r.job.clone(), r.status, r.exit_code, r.error.clone(), r.peak_rss, r.cpu_seconds)
            .await?;
        self.logs.finish(&r.job);
        let m = metrics::global();
        let run = self.store.run(run_id.clone()).await?;
        let repo = run.as_ref().map(|r| r.repo_name.clone()).unwrap_or_default();
        let executor = job.as_ref().and_then(|j| j.executor.clone()).unwrap_or_default();
        m.jobs_finished.inc(&[&repo, r.status.as_str(), &executor]);
        if let Some(started) = job.as_ref().and_then(|j| j.started_at) {
            #[allow(clippy::cast_precision_loss)]
            m.job_duration.observe((crate::util::now_unix() - started) as f64);
        }
        if let Some(j) = &job {
            let mut j = j.clone();
            j.peak_rss = r.peak_rss.or(j.peak_rss);
            j.cpu_seconds = r.cpu_seconds.or(j.cpu_seconds);
            j.error = r.error.clone().or(j.error);
            crate::observe::kiln::job_finished(&j, run.as_ref(), r.status, crate::util::now_unix());
        }
        tracing::info!(job = %r.job, agent, status = r.status.as_str(), "job finished");
        let snapshot = self.agents.load().get(agent).cloned();
        if let Some(a) = snapshot {
            self.emit_agent(&a);
            if a.ephemeral {
                self.command_start(agent, &ServerMsg::Drain).await.log_when_answered(LOG_WITHIN);
            }
        }
        self.emit_job(&r.job).await;
        self.emit_run(&run_id).await;
        if r.status == crate::store::Status::Succeeded
            && let Some(run) = &run
        {
            self.adopt_release(&r.job, run).await;
        }
        if let (Some(status), Some(run)) = (settled, run) {
            crate::observe::kiln::run_finished(&run, status, crate::util::now_unix());
            crate::gitops::report_status(self, &run, status).await;
        }
        self.update_slot_gauges();
        self.wake.notify();
        self.check_retire().await;
        Ok(())
    }

    // ------------------------------------------------------------ runs

    pub async fn cancel_run(&self, run_id: &str, reason: &str) -> anyhow::Result<()> {
        // The CI engine's runs are cancelled through their plans.
        if self.ci.is_run(run_id) {
            self.ci.send(crate::ci::dispatch::Msg::Cancel { run: run_id.to_owned(), reason: reason.to_owned() });
            return Ok(());
        }
        let running = self.store.cancel_run(run_id.to_owned(), reason.to_owned()).await?;
        for (job, agent) in running {
            if let Some(agent) = agent {
                self.command_start(&agent, &ServerMsg::Cancel { job: job.clone(), reason: reason.into() }).await.log_when_answered(LOG_WITHIN);
                self.release_slot(&agent, &job);
            }
            self.logs.finish(&job);
            self.emit_job(&job).await;
            if let Some(Some(k)) = self.k8s.get() {
                k.delete_job_pods(&job).await;
            }
        }
        self.emit_run(run_id).await;
        self.update_slot_gauges();
        self.wake.notify();
        Ok(())
    }

    /// Some of a run's jobs stopped, the run going on (docs/ci.md W26-R21): each named one that
    /// hasn't finished is cancelled, the jobs that need it are skipped, and a running one's runner
    /// is told to stop it, as [`Hub::cancel_run`] tells them. A CI engine run's legs are cancelled
    /// through its plan. Each job named and what became of it.
    pub async fn cancel_jobs(&self, run_id: &str, jobs: Vec<String>, reason: &str) -> anyhow::Result<Vec<(String, crate::store::JobCancel)>> {
        use crate::store::JobCancel;
        if self.ci.is_run(run_id) || self.store.is_engine_run(run_id.to_owned()).await? {
            // Its answer on a one-shot, lock-free (goal 4, plan 01a1a59c2a9c step 5).
            let (tx, rx) = hyper_rt::sync::oneshot().map_err(|e| anyhow::anyhow!("the CI dispatcher's answer can't be waited for: {e}"))?;
            self.ci.send(crate::ci::dispatch::Msg::CancelLegs { run: run_id.to_owned(), legs: jobs, reason: reason.to_owned(), reply: tx });
            let each = crate::rt::timeout(std::time::Duration::from_secs(60), rx).await.ok().and_then(Result::ok).ok_or_else(|| anyhow::anyhow!("the CI dispatcher didn't answer"))?;
            return Ok(each.into_iter().map(|(leg, r)| (leg, r.map_or_else(JobCancel::Refused, |()| JobCancel::Cancelled))).collect());
        }
        let done = self.store.cancel_jobs(run_id.to_owned(), jobs, reason.to_owned()).await?;
        for (job, agent) in &done.running {
            if let Some(agent) = agent {
                self.command_start(agent, &ServerMsg::Cancel { job: job.clone(), reason: reason.into() }).await.log_when_answered(LOG_WITHIN);
                self.release_slot(agent, job);
            }
            self.logs.finish(job);
            if let Some(Some(k)) = self.k8s.get() {
                k.delete_job_pods(job).await;
            }
        }
        for (job, r) in &done.jobs {
            if *r == JobCancel::Cancelled {
                self.emit_job(job).await;
            }
        }
        for job in &done.skipped {
            self.emit_job(job).await;
        }
        self.emit_run(run_id).await;
        self.update_slot_gauges();
        self.wake.notify();
        Ok(done.jobs)
    }

    /// Pause and Resume (docs/ci.md W26-R22): each named job of a run, either engine's, paused
    /// (`paused`) or resumed. A running one is frozen where it runs, or let go on, by its runner,
    /// only one that says it can (`CAP_FREEZE`); its time limit doesn't run while it's paused. A
    /// waiting one is held, dispatched nowhere until it's resumed. Nothing resumes a job by itself.
    /// Each job named, and whether it was, or why not.
    pub async fn pause_jobs(&self, run_id: &str, jobs: Vec<String>, paused: bool) -> anyhow::Result<Vec<(String, Result<(), String>)>> {
        let mut out = Vec::new();
        let mut any = false;
        for (id, t) in self.store.pause_targets(run_id.to_owned(), jobs).await? {
            let r = match t {
                None => Err("not a job of this run".to_owned()),
                Some(t) => self.pause_one(&t, paused).await,
            };
            if let Ok(changed) = &r {
                any |= *changed;
                if *changed {
                    self.emit_job(&id).await;
                }
            }
            out.push((id, r.map(|_| ())));
        }
        if any {
            self.emit_run(run_id).await;
            if !paused {
                self.wake.notify();
            }
        }
        Ok(out)
    }

    /// How long job `job` has been paused at `now`, all told, the while it is now included
    /// (docs/ci.md W26-R22): its token lives that while longer.
    pub fn paused_for(&self, job: &str, now: i64) -> i64 {
        self.paused.load().get(job).map_or(0, |(before, since)| before + since.map_or(0, |s| (now - s).max(0)))
    }

    /// Job `job` found paused, or paused before, as a restart restores it: `paused_ms` before, and
    /// `since` now.
    pub fn restore_paused(&self, job: &str, paused_ms: i64, since: Option<i64>) {
        self.paused.update(|p| p.insert(job.to_owned(), (paused_ms / 1000, since)));
    }

    /// One job paused or resumed ([`Hub::pause_jobs`]): whether it changed, or why not.
    async fn pause_one(&self, t: &crate::store::PauseTarget, paused: bool) -> Result<bool, String> {
        if t.status.is_terminal() {
            return Err(format!("`{}` has ended: it's {}", t.key, t.status.as_str()));
        }
        match (t.paused_at.is_some(), paused) {
            (true, true) => return Ok(false),
            (false, false) => return Err(format!("`{}` isn't paused", t.key)),
            _ => {}
        }
        if t.status == Status::Running {
            let Some(agent) = &t.agent else { return Err(format!("`{}` is starting: it has no runner yet", t.key)) };
            // Only the runner process that holds it, and only one that can freeze a job in place.
            let caps = self.instances.load().wide_caps_for(agent, &crate::instances::Owned::Job(t.id.clone()));
            if caps & crate::proto::CAP_FREEZE == 0 {
                return Err(format!("its runner {agent} can't freeze a job in place: it runs a kiln from before Pause came to its platform"));
            }
            let answer = self.command(agent, &ServerMsg::Freeze { job: t.id.clone(), frozen: paused }, FREEZE_WITHIN).await;
            if let Some((_, why)) = answer.failure(agent) {
                return Err(why);
            }
        }
        let now = crate::util::now_unix();
        let changed = self.store.set_paused(t.id.clone(), paused, now).await.map_err(|e| format!("{e:#}"))?;
        if changed {
            self.paused.update(|p| {
                let e = p.entry(t.id.clone()).or_insert((0, None));
                match (paused, e.1.take()) {
                    (true, _) => e.1 = Some(now),
                    (false, Some(since)) => e.0 += (now - since).max(0),
                    (false, None) => {}
                }
            });
        }
        // The engine's scheduler holds a paused waiting leg until it's resumed.
        if t.engine {
            self.ci.send(crate::ci::dispatch::Msg::LegPaused { leg: t.id.clone(), paused });
        }
        Ok(changed)
    }

    // ------------------------------------------------------------ scheduler

    /// Assign queued jobs to agents (or launch pods) until nothing more fits. Runs on every wake
    /// and once a second as a safety net.
    pub async fn scheduler_loop(self: Arc<Self>) -> anyhow::Result<()> {
        let mut tick = Interval::new(Duration::from_secs(1));
        let mut slow = Instant::now();
        let mut pods = Instant::now();
        loop {
            futures::select_biased! {
                () = self.shutdown.cancelled().fuse() => return Ok(()),
                () = self.wake.notified().fuse() => {}
                () = tick.tick().fuse() => {}
            }
            self.schedule_once().await?;
            if slow.elapsed() >= Duration::from_secs(5) {
                slow = Instant::now();
                self.check_timeouts().await?;
                self.check_lost_agents().await;
                crate::ci::notify::runners_gone(&self);
                crate::ci::notify::secrets_unused(&self);
                crate::ci::notify::quarantined(&self);
            }
            // Finished job pods' records (docs/machines.md), once a minute.
            if pods.elapsed() >= Duration::from_secs(60) {
                pods = Instant::now();
                self.forget_finished_pods().await;
            }
        }
    }

    /// What a job's environment gets from the server: its secrets, and where each image kiln
    /// publishes lives (`KILN_IMAGE_<NAME>`, docs/cd-targets.md §7.1), set in the UI. Those kept in files are read on the
    /// blocking pool: each job's assignment read them on the scheduler's shard (goal 9, plan 01a1a87b2cec step 3).
    async fn job_secrets(&self) -> Vec<(String, String)> {
        let cfg = Arc::clone(&self.cfg);
        let mut s = match crate::rt::blocking(move || secrets_of(&cfg)).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "the server's secrets couldn't be read off the shard: read here");
                self.secrets()
            }
        };
        if let Some(cd) = crate::cd::get() {
            s.extend(cd.image_env());
        }
        s
    }

    /// The server's secrets, for the CI engine's trusted runs (docs/ci.md W09 narrows them).
    pub fn ci_secrets(&self) -> Vec<(String, String)> {
        self.secrets()
    }

    fn secrets(&self) -> Vec<(String, String)> {
        secrets_of(&self.cfg)
    }

    async fn schedule_once(&self) -> anyhow::Result<()> {
        // The waiting jobs, how many run, and the waiting jobs' runs, as one read saw them (the DB query
        // audit, 2026-11-06: where it read the first two apart and then each run on its own).
        let crate::store::Scheduling { queued, running, runs } = self.store.scheduling(2000).await?;
        metrics::global().jobs_queued.set(i64::try_from(queued.len()).unwrap_or(i64::MAX));
        metrics::global().jobs_running.set(running);
        if queued.is_empty() {
            return Ok(());
        }
        let k8s = self.k8s.get().and_then(Option::as_ref);
        let mut pods_live = None;
        // Its turns on its shard: a queue that finds no runner was walked whole in one poll, every second; one job's
        // pick at a time now, its poll ended once its turn would pass the quantum (goal 9, plan 01a1a87b2cec step 3: no
        // poll past the quantum, `rt::Turns::end`).
        let mut turns = crate::rt::Turns::new();
        for job in queued {
            turns.end().await;
            let Some(spec) = job.spec.clone() else { continue };
            let Some(run) = runs.get(&job.run_id) else { continue };
            // `if:` conditions are evaluated against the run.
            if let Some(cond) = &spec.condition {
                let mut ctx = Context::default();
                ctx.set("branch", run.branch.clone());
                ctx.set("event", run.event.clone());
                ctx.set("sha", run.sha.clone());
                ctx.set("repo", run.repo_name.clone());
                if !ctx.eval(cond) {
                    self.store.claim_job(job.id.clone(), "-".into(), "none".into()).await?;
                    self.store.finish_job(job.id.clone(), Status::Skipped, None, Some(format!("if: {cond}")), None, None).await?;
                    self.emit_job(&job.id).await;
                    self.emit_run(&run.id).await;
                    continue;
                }
            }
            let chosen = self.pick_agent(&job);
            match chosen {
                Some(agent) => {
                    let ephemeral = self.agents.load().get(&agent).is_some_and(|a| a.ephemeral);
                    let executor = if job.labels.iter().any(|l| l == "k8s") && ephemeral { "k8s" } else { "baremetal" };
                    if !self.store.claim_job(job.id.clone(), agent.clone(), executor.into()).await? {
                        continue;
                    }
                    self.change_agent(&agent, |a| {
                        a.running.push(job.id.clone());
                        a.busy = u32::try_from(a.running.len()).unwrap_or(u32::MAX);
                        hold_on(a, &job.id, v1_incoming(spec.exclusive, &spec.resources));
                    });
                    if let Some(q) = job.queued_at {
                        #[allow(clippy::cast_precision_loss)]
                        metrics::global().queue_wait.observe((crate::util::now_unix() - q).max(0) as f64);
                    }
                    let clone_url = self
                        .store
                        .repos()
                        .await?
                        .into_iter()
                        .find(|r| r.id == run.repo_id)
                        .map(|r| r.clone_url)
                        .unwrap_or_default();
                    let assign = Assignment {
                        job_id: job.id.clone(),
                        run_id: run.id.clone(),
                        repo: run.repo_name.clone(),
                        clone_url,
                        sha: run.sha.clone(),
                        branch: run.branch.clone(),
                        event: run.event.clone(),
                        run_number: run.number,
                        attempt: job.attempt,
                        spec: spec.for_assignee(self.runner_caps(&agent), ephemeral),
                        secrets: self.job_secrets().await,
                        job_token: crate::api::job_token(&self.admin_token, &job.id),
                        server_url: self.server_url_for(&agent),
                    };
                    tracing::info!(job = %job.id, key = %job.key, agent = %agent, "job assigned");
                    let _rx = self.logs.subscribe(&job.id); // create the live channel early
                    // A live command: refused at once while its runner is away. Once taken, the leg's
                    // own answer (JobStarted or Reject) says what came of it.
                    let pending = self.command_start(&agent, &ServerMsg::Assign(Box::new(assign))).await;
                    match pending.at_once() {
                        Some(a) if *a != crate::wire::commands::Answer::Sent => {
                            let why = format!("assignment could not be delivered: {}", a.words());
                            self.release_slot(&agent, &job.id);
                            self.store.requeue_job(job.id.clone(), why).await?;
                        }
                        _ => pending.log_when_answered(LOG_WITHIN),
                    }
                    self.emit_job(&job.id).await;
                    self.emit_run(&run.id).await;
                    self.update_slot_gauges();
                }
                None => {
                    let Some(k) = k8s else { continue };
                    if !job.labels.iter().any(|l| l == "k8s") {
                        continue;
                    }
                    // A retry asked for a runner by name (W05-R28): a pod made for it isn't that one.
                    if job.placement.as_ref().is_some_and(|p| p.runner.is_some()) {
                        continue;
                    }
                    let timeout = Duration::from_secs(u64::from(self.cfg.kubernetes.start_timeout_seconds));
                    let launch = self.launching.load().get(&job.id).copied();
                    match launch {
                        Some(Launch::Pod(t)) if t.elapsed() < timeout => continue,
                        Some(Launch::Retry { at, .. }) if Instant::now() < at => continue,
                        // After a server restart the launch table is empty, but the job's pod may
                        // already exist (Pending or about to connect): adopt it instead of duplicating.
                        None => {
                            if k.job_pod_alive(&job.id).await {
                                self.launching.update(|l| l.insert(job.id.clone(), Launch::Pod(Instant::now())));
                                continue;
                            }
                        }
                        Some(Launch::Pod(_)) => {
                            // Still Pending for capacity (or pulling a large image) is not a failure:
                            // keep waiting instead of churning pods.
                            if k.job_pod_alive(&job.id).await {
                                self.launching.update(|l| l.insert(job.id.clone(), Launch::Pod(Instant::now())));
                                continue;
                            }
                            tracing::warn!(job = %job.id, "pod agent did not connect in time; relaunching");
                            k.delete_job_pods(&job.id).await;
                            // Not again on the next pass if the launch below has to wait.
                            self.launching.update(|l| l.insert(job.id.clone(), Launch::Retry { at: Instant::now(), check_pods: false }));
                        }
                        Some(Launch::Retry { check_pods: true, .. }) => {
                            if k.job_pod_alive(&job.id).await {
                                self.launching.update(|l| l.insert(job.id.clone(), Launch::Pod(Instant::now())));
                                continue;
                            }
                            k.delete_job_pods(&job.id).await;
                        }
                        Some(Launch::Retry { check_pods: false, .. }) => {}
                    }
                    if pods_live.is_none() {
                        pods_live = Some(self.launching.load().values().filter(|l| matches!(l, Launch::Pod(_))).count());
                    }
                    let live = pods_live.unwrap_or(0);
                    if u32::try_from(live).unwrap_or(u32::MAX) >= self.cfg.kubernetes.max_pods {
                        continue;
                    }
                    match k.launch(self, &job, run).await {
                        Ok(pod) => {
                            tracing::info!(job = %job.id, pod, "launched agent pod");
                            self.launching.update(|l| l.insert(job.id.clone(), Launch::Pod(Instant::now())));
                            pods_live = Some(live + 1);
                            metrics::global().k8s_pods_launched.inc(&["ok"]);
                        }
                        Err(e) => {
                            metrics::global().k8s_pods_launched.inc(&["error"]);
                            // Usually the jobs-namespace quota is full: retry soon rather than after
                            // the full start timeout. Each job retries on its own, since a smaller
                            // one may still fit where this one did not.
                            let msg = format!("{e:#}");
                            let quota = msg.contains("exceeded quota");
                            if quota {
                                tracing::debug!(job = %job.id, "job quota full; waiting for capacity");
                            } else {
                                tracing::error!(job = %job.id, error = %msg, "pod launch failed");
                            }
                            let now = Instant::now();
                            let at = now.checked_add(LAUNCH_RETRY).unwrap_or(now);
                            // A quota refusal created nothing, so its retry needs no pod checks.
                            self.launching.update(|l| l.insert(job.id.clone(), Launch::Retry { at, check_pods: !quota }));
                        }
                    }
                }
            }
        }
        self.emit_queue();
        Ok(())
    }

    /// The HTTP base URL an agent should use: loopback for the in-process agent, the in-cluster
    /// service for pods, the public URL for everyone else.
    fn server_url_for(&self, agent: &str) -> String {
        let local = self.cfg.local_agent_key();
        if self.cfg.local_agent.enabled && agent == local {
            let port = self.cfg.server.http_addr.rsplit_once(':').map_or("7070", |(_, p)| p);
            return format!("http://127.0.0.1:{port}");
        }
        let ephemeral = self.agents.load().get(agent).is_some_and(|a| a.ephemeral);
        if ephemeral
            && let Some(u) = &self.cfg.kubernetes.server_url {
                return u.clone();
            }
        self.cfg.server.public_url.clone()
    }

    fn pick_agent(&self, job: &Job) -> Option<String> {
        let declined = HashMap::clone(&self.declined.load());
        // A v1 job is written for what its runner can do, so a runner that hasn't said yet (it
        // connected a moment ago) waits for its `Build`: one written before, its capabilities
        // reading none, went to it out of step, read with no steps and passed without running
        // (W05-R22). A pod never says, and its jobs are written for this build's own.
        let built: std::collections::HashSet<String> = {
            let names: Vec<String> = self.agents.load().values().map(|a| a.name.clone()).collect();
            names.into_iter().filter(|n| self.runner_built(n)).collect()
        };
        let g = self.agents.load();
        let incoming = job.spec.as_ref().map_or(kiln_ci::sched::state::Incoming::Ordinary, |s| v1_incoming(s.exclusive, &s.resources));
        let asked = job.spec.as_ref().map(|s| v1_asked(&s.resources)).unwrap_or_default();
        let (target, now) = (crate::ci::dispatch::target_use(&self.cfg.ci.scheduler), crate::util::now_ms());
        let found = g
            .values()
            .filter(|a| a.connected && !a.draining && !a.exclusive && a.skew.takes_work())
            .filter(|a| a.ephemeral || built.contains(&a.name))
            .filter(|a| !a.running.contains(&job.id))
            .filter(|a| {
                declined
                    .get(&(job.id.clone(), a.name.clone()))
                    .is_none_or(|t| t.elapsed() >= DECLINE_BACKOFF)
            })
            .filter(|a| match &a.dedicated_job {
                Some(d) => d == &job.id,
                None => true,
            })
            .filter(|a| job.labels.iter().all(|l| carries(&a.labels, &a.os, &a.arch, &a.name, l)))
            .filter(|a| meets(job, a))
            // Where a person's retry asked it to run (docs/ci.md W05-R28).
            .filter(|a| job.placement.as_ref().is_none_or(|p| p.allows(&a.name)))
            .filter(|a| agent_room(a.slots, a.facts.as_deref(), &a.held, a.exclusive, a.running.len(), incoming))
            // Not one that's overloaded (nearly out of memory, or loaded past twice its cores): the
            // job goes to another, or waits for one (a pod made for it is its own).
            .filter(|a| a.ephemeral || !strain_of(a).overloaded)
            // By the engine's own rule (W05): within the target use, the most headroom left.
            .map(|a| V1Candidate { name: &a.name, slots: a.slots, headroom: if a.ephemeral || !self.cfg.ci.scheduler.headroom { None } else { fresh_headroom(a, now) }, room: crate::reach::directory::room(strain_of(a), a.busy, a.slots) });
        v1_best(found, &asked, target).map(str::to_owned)
    }

    async fn check_timeouts(&self) -> anyhow::Result<()> {
        // The jobs past their limit (its time doesn't run while a job's paused, W26-R22), read alone (the
        // DB query audit, 2026-11-06: where every running job was read whole and its spec parsed).
        for job in self.store.timed_out(crate::util::now_unix()).await? {
            let sent = self.cancel_sent.load().get(&job.id).copied();
            match sent {
                None => {
                    if let Some(agent) = &job.agent {
                        self.command_start(agent, &ServerMsg::Cancel { job: job.id.clone(), reason: "timeout".into() }).await.log_when_answered(LOG_WITHIN);
                    }
                    self.cancel_sent.update(|c| c.insert(job.id.clone(), Instant::now()));
                }
                Some(t) if t.elapsed() > Duration::from_secs(60) => {
                    // The agent did not report back: settle it ourselves.
                    if let Some(agent) = &job.agent {
                        self.release_slot(agent, &job.id);
                    }
                    self.cancel_sent.update(|c| c.remove(&job.id));
                    let (run_id, _) = self
                        .store
                        .finish_job(job.id.clone(), Status::TimedOut, None, Some(format!("exceeded {} min", job.minutes)), None, None)
                        .await?;
                    self.logs.finish(&job.id);
                    self.emit_job(&job.id).await;
                    self.emit_run(&run_id).await;
                }
                Some(_) => {}
            }
        }
        // Jobs whose agent session vanished entirely (e.g. server restart, agent never returned).
        Ok(())
    }

    async fn check_lost_agents(&self) {
        // As long as runners took to come back (`crate::comeback`).
        let grace = self.comeback.grace();
        // Jobs of a runner process that went away while its machine stayed connected through
        // another one, and did not come back in time.
        let expired: Vec<(String, Vec<String>)> = self.lost_instances.update(|g| {
            let peers: Vec<PeerId> = g.iter().filter(|(_, (_, t, _))| t.elapsed() > grace).map(|(p, _)| *p).collect();
            peers.into_iter().filter_map(|p| g.remove(&p)).map(|(name, _, jobs)| (name, jobs)).collect()
        });
        for (name, jobs) in expired {
            if jobs.iter().any(|j| self.ci.is_leg(j)) {
                self.ci.send_held(crate::ci::dispatch::Msg::RunnerLost { agent: name.clone() }).await;
            }
            for j in jobs.into_iter().filter(|j| !self.ci.is_leg(j)) {
                let still = self.agents.load().get(&name).is_some_and(|a| a.running.contains(&j));
                if !still {
                    continue;
                }
                self.release_slot(&name, &j);
                match self.store.requeue_job(j.clone(), format!("runner process did not reconnect ({})", self.shown(&name))).await {
                    Ok(_) => self.emit_job(&j).await,
                    Err(e) => tracing::error!(error = %e, job = %j, "requeue failed"),
                }
            }
        }
        self.check_retire().await;
        let lost: Vec<String> = self
            .agents
            .load()
            .values()
            .filter(|a| !a.connected && a.lost_at.is_some_and(|t| t.elapsed() > grace) && !a.running.is_empty())
            .map(|a| a.name.clone())
            .collect();
        for name in lost {
            self.requeue_agent_jobs(&name, "agent did not reconnect").await;
        }
        // A job's pod gone past its grace with nothing left to run: a new copy only when there's one.
        let gone = |a: &AgentLive| a.ephemeral && !a.connected && a.running.is_empty() && a.lost_at.is_some_and(|t| t.elapsed() > grace);
        if self.agents.load().values().any(|a| gone(a)) {
            self.agents.update(|m| m.retain(|_, a| !gone(a)));
        }
        self.declined.update(|d| d.retain(|_, t| t.elapsed() < DECLINE_BACKOFF));
        // Running jobs assigned to agents we have no session for at all (after a server restart).
        // By their ids and runners alone, the rest of their rows unread (the DB query audit, 2026-11-06).
        if self.started.elapsed() > grace
            && let Ok(running) = self.store.running_holders().await {
                // Jobs held by a connected agent, or by one still inside its reconnect grace, are
                // not orphans.
                let known: HashSet<String> = self
                    .agents
                    .load()
                    .values()
                    .filter(|a| a.connected || a.lost_at.is_some_and(|t| t.elapsed() <= grace))
                    .flat_map(|a| a.running.clone())
                    .collect();
                for (id, agent) in running {
                    if !known.contains(&id) && agent.as_deref() != Some("-")
                        && let Ok(true) = self.store.requeue_job(id.clone(), "orphaned after restart".into()).await {
                            self.emit_job(&id).await;
                        }
                }
            }
        // Expire unused pod enrollment tokens.
        self.ephemeral_tokens.update(|t| t.retain(|_, (_, exp)| *exp > Instant::now()));
        self.executor_tokens.update(|t| t.retain(|_, (_, _, exp)| *exp > Instant::now()));
    }

    /// Sample the server host itself for the UI ("server" series).
    pub async fn sampler_loop(self: Arc<Self>) -> anyhow::Result<()> {
        let mut sampler = Some(crate::host::Sampler::new(Some(&self.cfg.server.data_dir)));
        let mut tick = Interval::new(Duration::from_secs(u64::from(self.cfg.telemetry.sample_seconds.max(1))));
        loop {
            futures::select_biased! {
                () = self.shutdown.cancelled().fuse() => return Ok(()),
                () = tick.tick().fuse() => {}
            }
            // Taken on the blocking pool, as a runner's is (`crate::agent`): the sample reads the host's
            // files, and sysinfo's refresh of every process where it's used, which held the shard that ran
            // this loop about 2 s every minute (found by the Bare Metal Migration agent, 01a195ef; goal 9,
            // plan 01a1a87b2cec step 3). The sampler goes with it and comes back, so each sample is the
            // difference from the one before, as it was.
            let mut s = sampler.take().unwrap_or_else(|| crate::host::Sampler::new(Some(&self.cfg.server.data_dir)));
            match crate::rt::blocking(move || {
                let sample = s.sample();
                (s, sample)
            })
            .await
            {
                Ok((back, sample)) => {
                    sampler = Some(back);
                    self.record_sample("server", sample);
                }
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "the server's host sample wasn't taken"),
            }
        }
    }

    /// Daily retention: delete old runs and their logs.
    /// Close terminal sessions left idle with nobody attached; forget ended ones.
    pub async fn terminal_loop(self: Arc<Self>) -> anyhow::Result<()> {
        let idle = Duration::from_secs(self.cfg.terminal.idle_minutes.max(1).saturating_mul(60));
        loop {
            if !self.shutdown.sleep(Duration::from_secs(30)).await {
                return Ok(());
            }
            if self.terminals.sweep(idle) {
                self.emit_terminals();
            }
        }
    }

    pub async fn retention_loop(self: Arc<Self>) -> anyhow::Result<()> {
        let mut tick = Interval::new(Duration::from_secs(3600));
        loop {
            futures::select_biased! {
                () = self.shutdown.cancelled().fuse() => return Ok(()),
                () = tick.tick().fuse() => {}
            }
            crate::audit::prune(&self.store, self.cfg.retention.audit_days).await;
            let jobs = self.store.prune(self.cfg.retention.days).await?;
            if !jobs.is_empty() {
                tracing::info!(jobs = jobs.len(), "pruned old runs");
                crate::ci::logsearch::forget(&self, jobs.clone()).await;
                self.logs.remove(jobs).await;
            }
        }
    }
}

/// What a v1 job holds of the runner it runs on (docs/ci.md W05-R03): an exclusive one asking for
/// CPUs (`resources: { cpu, memory }`) holds them and that memory; asking for none, the whole
/// runner. A `resources:` that isn't a quantity asks for none.
pub(crate) fn v1_incoming(exclusive: bool, resources: &crate::pipeline::Resources) -> kiln_ci::sched::state::Incoming {
    kiln_ci::sched::state::Incoming::of(exclusive, &v1_asked(resources))
}

/// What a v1 job asks of its runner: its `resources:` (none: nothing).
fn v1_asked(resources: &crate::pipeline::Resources) -> kiln_ci::sched::state::Resources {
    kiln_ci::sched::pools::ResourceSpec { cpu: resources.cpu.clone(), memory: resources.memory.clone(), ..Default::default() }.resources().unwrap_or_default()
}

/// A runner as the v1 dispatcher's last choice weighs it ([`v1_best`]).
pub(crate) struct V1Candidate<'a> {
    pub name: &'a str,
    pub slots: u32,
    /// Its headroom by a fresh sample; none for a pod made for the job, or a runner without one.
    pub headroom: Option<kiln_ci::sched::state::Headroom>,
    /// Its room as reach measures it, for runners without a fresh sample.
    pub room: f32,
}

/// Where a v1 job goes of the runners that could take it, by the engine's own rule (W05, the
/// scheduler's headroom, [`kiln_ci::sched::place::place`]): within the target use by its live
/// sample, the one with the most headroom left after it; ties, and runners without a fresh
/// sample after those, by reach's room, then the most slots, then the name.
pub(crate) fn v1_best<'a>(candidates: impl IntoIterator<Item = V1Candidate<'a>>, asked: &kiln_ci::sched::state::Resources, target: f64) -> Option<&'a str> {
    let mut found: Vec<V1Candidate<'a>> = candidates.into_iter().collect();
    found.sort_by(|a, b| b.room.total_cmp(&a.room).then(b.slots.cmp(&a.slots)).then(a.name.cmp(b.name)));
    let nodes: Vec<kiln_ci::sched::place::Node<'a>> = found.iter().map(|c| kiln_ci::sched::place::Node { name: c.name, headroom: c.headroom }).collect();
    kiln_ci::sched::place::place(asked, &nodes, target).ok()
}

/// A runner's headroom by its sample when it's fresh ([`kiln_ci::sched::state::SAMPLE_FRESH_MS`]).
fn fresh_headroom(a: &AgentLive, now_ms: i64) -> Option<kiln_ci::sched::state::Headroom> {
    let s = a.sample.as_ref().filter(|s| now_ms.saturating_sub(s.ts_ms) <= kiln_ci::sched::state::SAMPLE_FRESH_MS)?;
    Some(kiln_ci::sched::state::Headroom::of(&crate::ci::dispatch::sample_of(s, a.facts.as_deref())))
}

/// Whether a runner has room for one more job or leg, by the rule the engine's scheduler and the
/// runner's own admission use (`kiln_ci::sched::state::room`): its slots, its size as its facts
/// give it, what's held of it and what runs.
pub(crate) fn agent_room(slots: u32, facts: Option<&kiln_ci::facts::RunnerFacts>, held: &[(String, kiln_ci::sched::state::Resources)], whole: bool, running: usize, incoming: kiln_ci::sched::state::Incoming) -> bool {
    let size = facts.and_then(|f| {
        let cpus = if f.cpu.threads > 0 { f.cpu.threads } else { f.cpu.cores };
        (cpus > 0 && f.mem.total > 0).then(|| kiln_ci::sched::state::Resources { cpu: f64::from(cpus), mem: f.mem.total, ..Default::default() })
    });
    let mut shares = kiln_ci::sched::state::Resources::default();
    for (_, q) in held {
        shares.cpu += q.cpu;
        shares.mem = shares.mem.saturating_add(q.mem);
    }
    kiln_ci::sched::state::room(slots, size, &shares, whole, running.saturating_sub(held.len()), running, incoming)
}

/// A job or leg now on a runner holds what it holds of it: its share, or the whole runner.
pub(crate) fn hold_on(a: &mut AgentLive, job: &str, incoming: kiln_ci::sched::state::Incoming) {
    match incoming {
        kiln_ci::sched::state::Incoming::Share(q) => a.held.push((job.to_owned(), q)),
        kiln_ci::sched::state::Incoming::Whole => a.exclusive = true,
        kiln_ci::sched::state::Incoming::Ordinary => {}
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn unread(wire: crate::wire::unread::Wire, from: crate::wire::unread::Sender) -> crate::wire::unread::Unread {
        crate::wire::unread::Unread { wire, from, what: "variant 51".into(), code: crate::wire::unread::Code::Unknown, why: "AgentMsg has 51 variants".into() }
    }

    /// A runner's frames that can't be read (typed messages, step 3): a variant this build doesn't
    /// know, one cut short, and anything on its control lane before its Hello, each noted (kept for
    /// its page, audited), and nothing taken from it.
    #[test]
    fn a_runners_frames_that_cant_be_read_are_noted_and_nothing_is_taken() -> anyhow::Result<()> {
        use crate::wire::unread::{Code as C, Page, Wire as W};
        crate::rt::block_on(async {
            let hub = scratch_hub().await?;
            hub.peers.update(|m| m.insert(1, "fedora".into()));
            hub.put_agent(live_runner("fedora", "linux", "x86_64"));
            let frame = |data: Vec<u8>, lane: u32| NetEvent::Frame { peer: 1, lane, kind: Kind::Control, data, at: Instant::now() };
            // A newer runner's variant, and a known one cut short.
            hub.on_net(frame(vec![0xc8, 0x01, 7], proto::LANE_CONTROL)).await?;
            let adopted = proto::encode(&AgentMsg::HarnessAdopted { session: "s1".into(), error: None })?;
            hub.on_net(frame(adopted.get(..2).unwrap_or_default().to_vec(), proto::LANE_CONTROL)).await?;
            let kept = hub.refusals.load().kept(&Page::Runner("fedora".into())).cloned().unwrap_or_default();
            let counts: Vec<(W, C, u64)> = kept.counts.iter().map(|c| (c.wire, c.code, c.n)).collect();
            anyhow::ensure!(counts == vec![(W::Runner, C::Unknown, 1), (W::Runner, C::Malformed, 1)], "{counts:?}");
            let why = format!("AgentMsg has {} variants", crate::wire::agent_classes().len());
            anyhow::ensure!(kept.newest.front().is_some_and(|r| r.unread.what == "variant 200" && r.unread.why == why), "{:?}", kept.newest);
            let rows: i64 = hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM audit2 WHERE action = 'security.wire_refused' AND principal = 'runner:fedora'", [], |r| r.get(0))?)).await?;
            anyhow::ensure!(rows == 2, "{rows} rows");
            // Before its Hello, a peer's control frame is out of order; one on another lane is let
            // go as before (it may come before the Hello does).
            hub.pending_peers.update(|m| m.insert(2, "newbox".into()));
            let facts = proto::encode(&AgentMsg::NetDrops { dropped: Vec::new() })?;
            hub.on_net(NetEvent::Frame { peer: 2, lane: proto::LANE_TERM, kind: Kind::Term, data: facts.clone(), at: Instant::now() }).await.ok();
            hub.on_net(NetEvent::Frame { peer: 2, lane: proto::LANE_CONTROL, kind: Kind::Control, data: facts, at: Instant::now() }).await?;
            let early: Vec<(String, String, String)> = hub
                .store
                .call(|c| {
                    let mut q = c.prepare("SELECT principal, result, detail FROM audit2 WHERE action = 'security.wire_refused' AND principal LIKE 'peer:%'")?;
                    let rows = q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
                    Ok(rows)
                })
                .await?;
            anyhow::ensure!(early == vec![("peer:newbox".to_owned(), "malformed".to_owned(), "NetDrops: out of order: before its Hello".to_owned())], "{early:?}");
            Ok(())
        })
    }

    /// A message a runner sent that couldn't be read (typed messages, step 1): its row is in the audit
    /// log, its runner's record carries it, and the runner's page is told.
    #[test]
    fn a_refusal_noted_is_audited_kept_on_its_runner_and_its_page_told() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = scratch_hub_on(Arc::new(kiln_seams::clock::SimClock::new()), |_| {}).await?;
            hub.put_agent(live_runner("fedora", "linux", "x86_64"));
            let mut events = hub.subscribe_events()?;
            let fedora = crate::wire::unread::Sender::Runner { name: "fedora".into(), instance: "main".into() };
            anyhow::ensure!(hub.note_unread_told(unread(crate::wire::unread::Wire::Runner, fedora), None).await == Some(0), "its sender may be told");
            let row: (String, Option<String>, String, String) = hub
                .store
                .call(|c| Ok(c.query_row("SELECT principal, target, result, detail FROM audit2 WHERE action = 'security.wire_refused'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?))
                .await?;
            anyhow::ensure!(row == ("runner:fedora".into(), Some("runner".into()), "unknown".into(), "variant 51: AgentMsg has 51 variants".into()), "{row:?}");
            let kept = hub.agent_list().await.into_iter().find(|a| a.name == "fedora").and_then(|a| a.refused);
            anyhow::ensure!(kept.as_ref().map(crate::wire::unread::Kept::total) == Some(1), "GET /api/agents carries it: {kept:?}");
            let told: serde_json::Value = serde_json::from_str(&crate::rt::timeout(Duration::from_secs(5), events.recv()).await??)?;
            anyhow::ensure!(told.pointer("/type") == Some(&serde_json::json!("agent")) && told.pointer("/agent/refused/counts/0/n") == Some(&serde_json::json!(1)), "{told}");
            // Its record told for anything else carries them too: its page doesn't say it has none.
            anyhow::ensure!(hub.set_draining("fedora", true).await, "fedora drains");
            let told: serde_json::Value = serde_json::from_str(&crate::rt::timeout(Duration::from_secs(5), events.recv()).await??)?;
            anyhow::ensure!(told.pointer("/agent/draining") == Some(&serde_json::json!(true)) && told.pointer("/agent/refused/counts/0/n") == Some(&serde_json::json!(1)), "{told}");
            Ok(())
        })
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: an agent session's own events (its info changed, however it's
    /// sent, and its end) go on its lane of the sessions' events alone, the lane of its owner shard, in the
    /// order they're sent; and every other event on the hub's events alone: a listener of the hub's events
    /// isn't woken for a session's change.
    #[test]
    fn g4_s5_a_sessions_events_go_on_its_owners_lane() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = scratch_hub().await?;
            let (mut all, mut sessions) = (hub.subscribe_events()?, hub.harness_events.subscribe(16)?);
            let ids = ["s1", "s2", "s3", "s4", "s5"];
            for id in ids {
                let info = crate::harness::session::Info { id: id.into(), kind: "claude".into(), ..Default::default() };
                hub.emit(&ServerEvent::Harness { info: Box::new(info.clone()) });
                drop(hub.emit_harness(id, &serde_json::to_vec(&info)?));
                hub.emit(&ServerEvent::HarnessClosed { id: id.into() });
            }
            hub.emit(&ServerEvent::Updates);
            hub.emit(&ServerEvent::Terminals);
            let mut on_all = Vec::new();
            while let Ok(text) = all.try_recv() {
                on_all.push(crate::wire::events::kind_of(&text).unwrap_or_default().to_owned());
            }
            let mut on_lanes: Vec<(String, String)> = Vec::new();
            while let Ok(text) = sessions.try_recv() {
                let v: serde_json::Value = serde_json::from_str(&text)?;
                let id = v.pointer("/info/id").or_else(|| v.get("id")).and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
                on_lanes.push((id, crate::wire::events::kind_of(&text).unwrap_or_default().to_owned()));
            }
            // Each session's three, in the order sent, whatever the order between sessions.
            for id in ids {
                let theirs: Vec<&str> = on_lanes.iter().filter(|(i, _)| i == id).map(|(_, k)| k.as_str()).collect();
                anyhow::ensure!(theirs == ["harness", "harness", "harness_closed"], "{id}'s: {theirs:?} of {on_lanes:?}");
            }
            anyhow::ensure!(on_lanes.len() == 3 * ids.len(), "{on_lanes:?}");
            // The hub's events: these two, in order, among whatever its own work emits meanwhile.
            anyhow::ensure!(!on_all.iter().any(|k| k.starts_with("harness")), "a session's event on the hub's: {on_all:?}");
            let ours: Vec<&String> = on_all.iter().filter(|k| *k == "updates" || *k == "terminals").collect();
            anyhow::ensure!(ours == ["updates", "terminals"], "the hub's events: {on_all:?}");
            anyhow::ensure!(hub.harness_events.lanes() == crate::rt::shard_count().max(1), "a lane for each owner shard");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 5: a machine's low battery reaches the notifications' listener's lane alone of the
    /// hub's own events (`Hub::battery_events`), and every listener of the hub's events still: the listener isn't woken for
    /// each event to pass it over. Do: emit the hub's events of three kinds, a low battery among them. Expect: the battery
    /// lane holds the battery's alone; the hub's events hold all three.
    #[test]
    fn g9_s5_a_low_battery_goes_on_its_own_lane_and_the_hubs() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = scratch_hub().await?;
            let (mut all, mut battery) = (hub.subscribe_events()?, hub.subscribe_battery()?);
            let low = crate::os::battery::Battery { percent: 12, ac: false, charging: false, minutes: None };
            hub.emit(&ServerEvent::Updates);
            hub.emit(&ServerEvent::MachineBatteryLow { machine: "mac".into(), battery: low, text: "low".into() });
            hub.emit(&ServerEvent::Terminals);
            let kinds = |r: &mut crate::memory::lanes::Reader<Arc<str>>| {
                let mut out = Vec::new();
                while let Ok(text) = r.try_recv() {
                    out.push(crate::wire::events::kind_of(&text).unwrap_or_default().to_owned());
                }
                out
            };
            let (on_all, on_battery) = (kinds(&mut all), kinds(&mut battery));
            anyhow::ensure!(on_battery == ["machine.battery_low"], "the battery lane: {on_battery:?}");
            let ours: Vec<&String> = on_all.iter().filter(|k| ["updates", "machine.battery_low", "terminals"].contains(&k.as_str())).collect();
            anyhow::ensure!(ours == ["updates", "machine.battery_low", "terminals"], "the hub's events: {on_all:?}");
            Ok(())
        })
    }

    /// Goal 4, plan 01a1a59c2a9c step 5 (goal 5's rule: no lock two shards share): the hub's events, sent from
    /// tasks on every shard at once, reach every listener once each, each sender's in the order it sent them;
    /// and a listener that joins after some were sent has those that came after, in order.
    #[test]
    fn g4_s5_the_hubs_events_reach_every_listener_in_the_order_sent() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = scratch_hub().await?;
            let (senders, per) = (6usize, 150usize);
            // Ours alone, among whatever else the hub sends: `ai_sessions` frames naming their sender and number.
            fn ours(text: &str) -> anyhow::Result<Option<(usize, usize)>> {
                let v: serde_json::Value = serde_json::from_str(text)?;
                let Some(m) = v.get("machine").and_then(serde_json::Value::as_str).and_then(|m| m.strip_prefix("lab-")) else { return Ok(None) };
                let (s, n) = m.split_once('-').ok_or_else(|| anyhow::anyhow!("{m}"))?;
                Ok(Some((s.parse()?, n.parse()?)))
            }
            let mut listeners = Vec::new();
            for _ in 0..3 {
                let mut rx = hub.subscribe_events()?;
                let want = senders * per;
                listeners.push(crate::rt::spawn_spread_joined(async move {
                    let mut got: Vec<(usize, usize)> = Vec::with_capacity(want);
                    while got.len() < want {
                        let text = rx.recv().await.map_err(|e| anyhow::anyhow!("a listener was told {e:?} after {}", got.len()))?;
                        got.extend(ours(&text)?);
                    }
                    Ok::<_, anyhow::Error>(got)
                }));
            }
            let mut sent = Vec::new();
            for s in 0..senders {
                let hub = Arc::clone(&hub);
                sent.push(crate::rt::spawn_spread_joined(async move {
                    for n in 0..per {
                        hub.emit(&ServerEvent::AiSessions { machine: format!("lab-{s}-{n}") });
                        if n % 25 == 0 {
                            crate::rt::maybe_yield().await;
                        }
                    }
                }));
            }
            for s in sent {
                s.await.map_err(|_| anyhow::anyhow!("a sender ended early"))?;
            }
            for l in listeners {
                let got = crate::rt::timeout(Duration::from_secs(30), l).await.map_err(|_| anyhow::anyhow!("a listener never had them all"))?.map_err(|_| anyhow::anyhow!("a listener ended"))??;
                for s in 0..senders {
                    let theirs: Vec<usize> = got.iter().filter(|(g, _)| *g == s).map(|(_, n)| *n).collect();
                    anyhow::ensure!(theirs == (0..per).collect::<Vec<_>>(), "sender {s}'s events: {} of {per}, in order: {}", theirs.len(), theirs.windows(2).all(|w| w.first() < w.get(1)));
                }
            }
            // One joining now has what comes after it, in order, and nothing from before.
            let mut late = hub.subscribe_events()?;
            for n in 0..3 {
                hub.emit(&ServerEvent::AiSessions { machine: format!("lab-9-{n}") });
            }
            let mut after = Vec::new();
            while let Ok(text) = late.try_recv() {
                after.extend(ours(&text)?);
            }
            anyhow::ensure!(after == [(9, 0), (9, 1), (9, 2)], "the late listener's: {after:?}");
            Ok(())
        })
    }

    /// A runner's update of an agent session that couldn't be read shows on the session's page too:
    /// its info carries it, its viewers are told, and the runner's next Open of it keeps it.
    #[test]
    fn a_refusal_about_a_session_is_kept_in_its_info() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::registry::Location;
        crate::rt::block_on(async {
            let hub = scratch_hub().await?;
            let there = Location::Machine("box".into());
            let open = || Update::Open { info: Box::new(crate::harness::session::Info { id: "s1".into(), kind: "claude".into(), ..Default::default() }), since: None };
            hub.harness.ingest(&there, "s1", open()).await;
            let mut events = hub.harness_events.subscribe(16)?;
            let from = crate::wire::unread::Sender::Runner { name: "box".into(), instance: "main".into() };
            hub.note_unread(unread(crate::wire::unread::Wire::RunnerPayload, from), Some(crate::wire::unread::Page::Session("s1".into()))).await;
            let told: serde_json::Value = serde_json::from_str(&crate::rt::timeout(Duration::from_secs(5), events.recv()).await??)?;
            anyhow::ensure!(told.pointer("/type") == Some(&serde_json::json!("harness")) && told.pointer("/info/refused/counts/0/wire") == Some(&serde_json::json!("runner_payload")), "{told}");
            hub.harness.ingest(&there, "s1", open()).await;
            let kept = hub.harness.list().into_iter().find(|i| i.id == "s1").and_then(|i| i.refused);
            anyhow::ensure!(kept.as_ref().map(crate::wire::unread::Kept::total) == Some(1), "kept across its runner's Open: {kept:?}");
            Ok(())
        })
    }

    /// CP-T3.2 (the twenty-second adversarial check's): a message with an id that rides a
    /// `HarnessCmd` (to a process whose inbox isn't known) rides the live stream, so it's never kept
    /// for a runner that's away and sent there once it's back; one without an id still parks,
    /// unchecked, and so does a change to the queue.
    #[test]
    fn a_message_with_an_id_rides_the_live_stream_and_one_without_parks() {
        use crate::harness::model::{Command, SendMode};
        use crate::wire::commands::Kind as Stream;
        let cmd = |c: Command| ServerMsg::HarnessCmd { session: "s".into(), json: serde_json::to_string(&c).unwrap_or_default() };
        let send = |id: &str| Command::Send { id: id.into(), text: "hi".into(), mode: SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: None };
        assert_eq!(stream_of(&cmd(send("m1"))), Some(Stream::Live), "a message with an id");
        assert_eq!(stream_of(&cmd(Command::SkillRun { name: "review".into(), args: None, id: Some("k1".into()), mode: Default::default() })), Some(Stream::Live), "a skill run with an id");
        assert_eq!(stream_of(&cmd(send(""))), Some(Stream::Parked), "a message without one");
        assert_eq!(stream_of(&cmd(Command::SkillRun { name: "review".into(), args: None, id: None, mode: Default::default() })), Some(Stream::Parked), "a skill run without one");
        assert_eq!(stream_of(&cmd(Command::QueueRemove { id: "m1".into() })), Some(Stream::Parked), "a change to the queue");
    }

    /// A job's `requires` route it by the runner's facts (docs/ci.md W05-R22): the release's
    /// macOS job goes to a Mac with cargo and node, never to one with no developer tools (which
    /// failed it at `npm`, 2026-10-24), nor to one that hasn't said what it has.
    #[test]
    fn a_job_goes_only_where_its_requires_hold() {
        let mac = |tools: &[&str]| kiln_ci::facts::RunnerFacts {
            os: "macos".into(),
            arch: "aarch64".into(),
            tools: tools.iter().map(|t| ((*t).to_owned(), "1".to_owned())).collect(),
            ..Default::default()
        };
        let release: Vec<String> = ["os == macos", "arch == aarch64", "tools has cargo", "tools has node"].iter().map(|r| (*r).to_owned()).collect();
        let labels = vec!["baremetal".to_owned(), "macos".to_owned()];
        assert!(facts_meet(&release, Some(&mac(&["cargo", "node", "git"])), &labels), "the Mac with its tools");
        assert!(!facts_meet(&release, Some(&mac(&["git"])), &labels), "a Mac with no developer tools");
        assert!(!facts_meet(&release, None, &labels), "a runner that hasn't said what it has");
        assert!(facts_meet(&[], None, &labels), "a job with no requires goes where its labels say");
        assert!(!facts_meet(&["tools has".to_owned()], Some(&mac(&["node"])), &labels), "a requirement that doesn't parse holds nowhere");
    }

    /// W05-R28: the runners a retry may ask a pipeline's job onto are those `pick_agent` would
    /// place it on, its labels and its `requires` both (the release's Linux build, `runs-on:
    /// [baremetal]`, isn't for a Mac); each other says why not.
    #[test]
    fn w05_r28_a_retry_s_runners_are_where_v1_places_the_job() {
        let runner = |name: &str, os: &str, labels: &[&str], tools: Option<&[&str]>| {
            let mut a = live_runner(name, os, "x86_64");
            a.labels = labels.iter().map(|l| (*l).to_owned()).collect();
            a.facts = tools.map(|t| {
                Box::new(kiln_ci::facts::RunnerFacts { os: os.into(), arch: "x86_64".into(), tools: t.iter().map(|t| ((*t).to_owned(), "1".to_owned())).collect(), ..Default::default() })
            });
            a
        };
        let labels = vec!["baremetal".to_owned()];
        let requires: Vec<String> = ["os == linux", "tools has cargo"].iter().map(|r| (*r).to_owned()).collect();
        assert!(cannot_take(&labels, &requires, &runner("fedora", "linux", &["baremetal"], Some(&["cargo"]))).is_empty());
        assert_eq!(cannot_take(&labels, &requires, &runner("mac", "macos", &["baremetal", "macos"], Some(&["cargo"]))), ["needs os == linux"]);
        assert_eq!(cannot_take(&labels, &requires, &runner("bare", "linux", &["baremetal"], Some(&[]))), ["needs tools has cargo"]);
        assert_eq!(cannot_take(&labels, &requires, &runner("quiet", "linux", &["baremetal"], None)), ["hasn't said what it has, which its `requires` needs"]);
        assert_eq!(cannot_take(&labels, &requires, &runner("pod", "linux", &["k8s"], Some(&["cargo"]))), ["lacks the label baremetal"]);
        // No `requires`: its labels alone, facts or none.
        assert!(cannot_take(&labels, &[], &runner("quiet", "linux", &["baremetal"], None)).is_empty());
    }

    /// What a waiting job's runners lack, for an agent to install (`GET /api/jobs/:id/tools`):
    /// nothing while one meets it all (the job waits for a slot), else each runner with what it
    /// doesn't meet: a Mac that only lacks tools and hosts agents first, then one without
    /// terminals, then one that hasn't said what it has.
    #[test]
    fn a_waiting_job_says_what_its_runners_lack() {
        let mac = |tools: &[&str]| kiln_ci::facts::RunnerFacts {
            os: "macos".into(),
            arch: "aarch64".into(),
            tools: tools.iter().map(|t| ((*t).to_owned(), "1".to_owned())).collect(),
            ..Default::default()
        };
        let release: Vec<String> = ["os == macos", "arch == aarch64", "tools has cargo", "tools has node"].iter().map(|r| (*r).to_owned()).collect();
        let labels = vec!["macos".to_owned()];
        let (studio, mbp) = (mac(&["git"]), mac(&["git", "cargo", "node"]));
        assert!(unmet_of(&[("studio-3", Some(&studio), &labels, None), ("mbp", Some(&mbp), &labels, None)], &release).is_empty(), "one meets it: it waits for a slot");
        let shut = Some("its runner was started without terminals".to_owned());
        let lacks = unmet_of(&[("old", None, &labels, None), ("studio-3", Some(&studio), &labels, None), ("studio-2", Some(&studio), &labels, shut)], &release);
        let seen: Vec<(&str, bool, usize, Vec<String>)> = lacks.iter().map(|u| (u.runner.as_str(), u.fixable, u.unmet.len(), u.tools.clone())).collect();
        let tools = vec!["cargo".to_owned(), "node".to_owned()];
        assert_eq!(seen, vec![("studio-3", true, 2, tools.clone()), ("studio-2", false, 2, tools.clone()), ("old", false, 4, tools)], "an agent can fix only the one that hosts agents and lacks only tools");
    }

    /// A bench holding 16 of a 32-CPU, 64 GiB runner's CPUs leaves two of its four slots: the v1
    /// dispatcher packs ordinary jobs beside it, and an exclusive job asking nothing still waits
    /// for the runner to be empty.
    #[test]
    fn a_v1_bench_holds_its_share_and_jobs_run_beside_it() {
        use kiln_ci::sched::state::{Incoming, Resources};
        let mut facts = kiln_ci::facts::RunnerFacts::default();
        facts.cpu.threads = 32;
        facts.mem.total = 64 << 30;
        let spec = |exclusive: bool, cpu: Option<&str>, memory: Option<&str>| crate::pipeline::JobSpec {
            exclusive,
            resources: crate::pipeline::Resources { cpu: cpu.map(str::to_owned), memory: memory.map(str::to_owned) },
            ..crate::pipeline::JobSpec::default()
        };
        let of = |s: crate::pipeline::JobSpec| v1_incoming(s.exclusive, &s.resources);
        let bench = of(spec(true, Some("16"), Some("16Gi")));
        assert_eq!(bench, Incoming::Share(Resources { cpu: 16.0, mem: 16 << 30, ..Resources::default() }));
        assert_eq!(of(spec(true, None, None)), Incoming::Whole);
        assert_eq!(of(spec(false, Some("16"), None)), Incoming::Ordinary);
        // Empty: the bench goes in; then two ordinary jobs beside it, not three.
        assert!(agent_room(4, Some(&facts), &[], false, 0, bench));
        let held = vec![("bench".to_owned(), Resources { cpu: 16.0, mem: 16 << 30, ..Resources::default() })];
        assert!(agent_room(4, Some(&facts), &held, false, 1, Incoming::Ordinary));
        assert!(agent_room(4, Some(&facts), &held, false, 2, Incoming::Ordinary));
        assert!(!agent_room(4, Some(&facts), &held, false, 3, Incoming::Ordinary), "two slots beside half the runner held");
        assert!(!agent_room(4, Some(&facts), &held, false, 1, Incoming::Whole), "the whole runner waits for it to be empty");
        // Before: an exclusive job took the whole runner; asking nothing, it still does.
        assert!(!agent_room(4, Some(&facts), &[], true, 1, Incoming::Ordinary));
        // A runner whose facts aren't in yet: the bench waits for it to be empty.
        assert!(!agent_room(4, None, &[], false, 1, bench) && agent_room(4, None, &[], false, 0, bench));
    }

    /// A job's labels fit a runner by its own labels, its name, or its platform (`linux`, `amd64`,
    /// `arm64`, `self-hosted`), in any case: `[linux]` isn't only for runners labelled so.
    #[test]
    fn a_job_for_linux_fits_every_linux_runner() {
        let own = vec!["baremetal".to_owned(), "bench".to_owned(), "local".to_owned()];
        let fits = |want: &str| carries(&own, "linux", "x86_64", "adacachy", want);
        for w in ["linux", "Linux", "x86_64", "amd64", "X64", "self-hosted", "baremetal", "BENCH", "adacachy"] {
            assert!(fits(w), "{w}");
        }
        for w in ["macos", "arm64", "gpu", "windows"] {
            assert!(!fits(w), "{w}");
        }
        assert!(carries(&[], "macos", "aarch64", "mac", "arm64") && carries(&[], "macos", "aarch64", "mac", "darwin"));
    }

    /// W05-R21 in v1: `[baremetal, linux]` fits a Linux runner labelled only `baremetal` (its
    /// platform says Linux), `[Linux]` fits one labelled `linux`, and `[baremetal, linux]` doesn't
    /// fit a Mac labelled `baremetal`.
    #[test]
    fn w05_r21_labels_match_in_any_case() {
        let fits = |labels: &[&str], os: &str, arch: &str, want: &[&str]| {
            let own: Vec<String> = labels.iter().map(|l| (*l).to_owned()).collect();
            want.iter().all(|w| carries(&own, os, arch, "runner", w))
        };
        assert!(fits(&["baremetal"], "linux", "x86_64", &["baremetal", "linux"]), "the platform's Linux");
        assert!(fits(&["baremetal"], "linux", "x86_64", &["baremetal", "Linux"]) && fits(&["baremetal"], "linux", "x86_64", &["BAREMETAL", "LINUX"]));
        assert!(fits(&["linux"], "linux", "x86_64", &["Linux"]), "`Linux` against `linux`");
        assert!(fits(&["Linux"], "linux", "x86_64", &["linux"]), "`linux` against `Linux`");
        assert!(!fits(&["baremetal"], "macos", "aarch64", &["baremetal", "linux"]), "a Mac isn't Linux");
    }

    /// W26-R22: a runner that doesn't say it can freeze a job in place (`CAP_FREEZE`: one from
    /// before Pause came to its platform) is asked nothing: pausing its running job is refused, saying
    /// why, and the job stays as it was. One that says it is asked, and its job reads paused (a
    /// test's runner takes the command as one before the channel did, unanswered). A waiting job is
    /// held with no word to any runner, and resumed; one that has ended is refused, and so is
    /// resuming one that isn't paused.
    #[test]
    fn w26_r22_an_old_runner_gets_no_freeze() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = scratch_hub_on(Arc::new(kiln_seams::clock::SimClock::new()), |_| {}).await?;
            join_runner(&hub, 1, "old", proto::CAP_WIRE);
            join_runner(&hub, 2, "new", proto::CAP_WIRE | proto::CAP_FREEZE);
            let repo = hub
                .store
                .upsert_repo(crate::store::Repo { id: "r1".into(), name: "acme/x".into(), clone_url: "https://example/x".into(), branches: vec![], poll_seconds: 60, enabled: true, created_at: 0, last_polled_at: None, last_error: None, mode: "upstream".into(), default_branch: None, mirror: None })
                .await?;
            let p = crate::pipeline::parse("name: ci\njobs:\n  a: {steps: [{run: x}]}\n  b: {steps: [{run: y}]}\n  c: {steps: [{run: z}]}\n  d: {steps: [{run: w}]}\n")?;
            let planned = crate::pipeline::plan(&p, &crate::pipeline::Context::default())?;
            let run = crate::store::Run {
                id: "run1".into(),
                repo_id: repo.id.clone(),
                repo_name: repo.name.clone(),
                number: 0,
                pipeline: "ci".into(),
                event: "push".into(),
                branch: "main".into(),
                sha: "abc".into(),
                message: String::new(),
                author: String::new(),
                status: Status::Queued,
                created_at: 0,
                started_at: None,
                finished_at: None,
                error: None,
                concurrency_group: None,
            };
            hub.store.create_run(run, planned).await?;
            let ids: HashMap<String, String> = hub.store.jobs("run1".into(), false).await?.into_iter().map(|j| (j.key, j.id)).collect();
            let id = |k: &str| ids.get(k).cloned().unwrap_or_default();
            let (a, b, c, d) = (id("a"), id("b"), id("c"), id("d"));
            anyhow::ensure!(hub.store.claim_job(a.clone(), "old".into(), "baremetal".into()).await?);
            anyhow::ensure!(hub.store.claim_job(b.clone(), "new".into(), "baremetal".into()).await?);
            anyhow::ensure!(hub.store.claim_job(d.clone(), "new".into(), "baremetal".into()).await?);
            hub.store.finish_job(d.clone(), Status::Succeeded, Some(0), None, None, None).await?;
            let each: HashMap<String, Result<(), String>> = hub.pause_jobs("run1", vec![a.clone(), b.clone(), c.clone(), d.clone(), "nope".into()], true).await?.into_iter().collect();
            let why = |j: &str| each.get(j).cloned().and_then(Result::err).unwrap_or_default();
            anyhow::ensure!(why(&a).contains("its runner old can't freeze a job in place"), "{each:?}");
            anyhow::ensure!(each.get(&b) == Some(&Ok(())), "the one that can is asked: {each:?}");
            anyhow::ensure!(each.get(&c) == Some(&Ok(())), "a waiting job is held: {each:?}");
            anyhow::ensure!(why(&d).contains("has ended: it's succeeded"), "{each:?}");
            anyhow::ensure!(why("nope") == "not a job of this run", "{each:?}");
            let paused = |j: String| {
                let hub = Arc::clone(&hub);
                async move { anyhow::Ok(hub.store.job(j).await?.and_then(|j| j.paused_at).is_some()) }
            };
            anyhow::ensure!(!paused(a.clone()).await? && paused(b.clone()).await? && paused(c.clone()).await?, "the old runner's job doesn't read paused");
            anyhow::ensure!(!hub.sent.lock().iter().any(|(_, m)| matches!(m, ServerMsg::Freeze { .. })), "no runner hears of a waiting job's pause");
            // Resumed: c again; a, never paused, is refused.
            let each: HashMap<String, Result<(), String>> = hub.pause_jobs("run1", vec![c.clone(), a.clone()], false).await?.into_iter().collect();
            anyhow::ensure!(each.get(&c) == Some(&Ok(())) && !paused(c).await?, "{each:?}");
            anyhow::ensure!(each.get(&a).cloned().and_then(Result::err).is_some_and(|w| w.contains("isn't paused")), "{each:?}");
            Ok(())
        })
    }

    /// A test's hub, on `clock`, with runner `name` connected as process 1: its Hello taken, its
    /// record live, and `wide` what it says it can do past Build's 32 bits.
    pub(crate) async fn hub_with_runner(clock: Arc<dyn kiln_seams::Clock>, name: &str, wide: u64) -> anyhow::Result<Arc<Hub>> {
        let hub = scratch_hub_on(clock, |_| {}).await?;
        join_runner(&hub, 1, name, wide);
        Ok(hub)
    }

    /// Runner `name` connected to a test's hub as process `peer`, its Hello taken, its record
    /// live; it takes reach links, and `wide` is what it says it can do past Build's 32 bits.
    pub(crate) fn join_runner(hub: &Hub, peer: PeerId, name: &str, wide: u64) {
        hub.peers.update(|m| m.insert(peer, name.to_owned()));
        hub.put_agent(live_runner(name, "linux", "x86_64"));
        let hello = Hello {
            protocol: proto::VERSION,
            name: name.to_owned(),
            labels: Vec::new(),
            slots: 1,
            hostname: name.to_owned(),
            os: "linux".into(),
            arch: "x86_64".into(),
            version: "test".into(),
            ephemeral: false,
            dedicated_job: None,
            running: Vec::new(),
            terminal: true,
        };
        let now = crate::util::now_unix();
        hub.instances.update(|i| {
            i.join(peer, name, hello.clone(), now);
            i.set_build(peer, "test".into(), proto::CAP_REACH);
            i.set_wide(peer, wide);
        });
        hub.wire.set_keeps(name, wide & proto::CAP_WIRE != 0);
    }

    /// `msg`, from runner process `peer` on `lane`, as the net loop hears it.
    pub(crate) async fn hear(hub: &Arc<Hub>, peer: PeerId, lane: u32, msg: &AgentMsg) -> anyhow::Result<()> {
        hub.on_net(NetEvent::Frame { peer, lane, kind: Kind::Control, data: proto::encode(msg)?, at: Instant::now() }).await
    }

    /// `msg`, item `seq` of runner process `peer`'s channel stream `stream`, as the hub hears it.
    pub(crate) async fn hear_item(hub: &Arc<Hub>, peer: PeerId, stream: &str, seq: u64, msg: &AgentMsg) -> anyhow::Result<()> {
        let agent = hub.agent_of(peer).ok_or_else(|| anyhow::anyhow!("no runner as {peer}"))?;
        let body = proto::encode(msg)?;
        hub.on_wire(peer, &agent, crate::wire::channel::Frame::Item { stream: stream.to_owned(), seq, low: seq.saturating_sub(1), body }, &mut None).await;
        Ok(())
    }

    /// A question asked of a runner by `req`, its answer to be read as a `T`: what its asker gets.
    pub(crate) fn asking<T: serde::de::DeserializeOwned + Serialize + Send + 'static>(hub: &Hub, req: u64) -> futures::channel::oneshot::Receiver<Result<Option<T>, String>> {
        let (read, rx) = asked::<T>();
        hub.ai_requests.put(req, read);
        rx
    }

    /// A file's piece asked of a runner by `req`: what its asker gets.
    pub(crate) fn asking_file(hub: &Hub, req: u64) -> futures::channel::oneshot::Receiver<Result<(crate::harness::files::Answer, Vec<u8>), String>> {
        let (tx, rx) = futures::channel::oneshot::channel();
        hub.file_requests.put(req, tx);
        rx
    }

    /// The audit log's `security.wire_refused` rows naming `principal`: each one's target, result
    /// and detail.
    pub(crate) async fn refused_rows(hub: &Hub, principal: &str) -> anyhow::Result<Vec<(String, String, String)>> {
        let principal = principal.to_owned();
        hub.store
            .call(move |c| {
                let mut q = c.prepare("SELECT COALESCE(target, ''), result, detail FROM audit2 WHERE action = 'security.wire_refused' AND principal = ?1 ORDER BY rowid")?;
                let rows = q.query_map([principal], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
    }

    /// A connected runner on `os`/`arch` that hosts agents (its terminals on), for a test's hub.
    pub(crate) fn live_runner(name: &str, os: &str, arch: &str) -> AgentLive {
        AgentLive {
            name: name.into(),
            label: None,
            peer: None,
            labels: vec![],
            slots: 1,
            hostname: name.into(),
            os: os.into(),
            arch: arch.into(),
            version: String::new(),
            ephemeral: false,
            connected: true,
            last_seen: 0,
            enrolled_at: 0,
            running: vec![],
            busy: 0,
            held: Vec::new(),
            draining: false,
            sample: None,
            battery: None,
            dropped: Vec::new(),
            dropped_toward: Vec::new(),
            refused: None,
            terminal: true,
            outpost: false,
            dedicated_job: None,
            exclusive: false,
            lost_at: None,
            facts: None,
            instances: Vec::new(),
            transport: None,
            skew: crate::skew::Skew::default(),
            stale: false,
        }
    }

    /// The test runtime's shards, as its tests share them (goal 4, plan 01a1a59c2a9c step 5). A test that keeps them busy
    /// (tasks that always want a shard, a shard held, an owner given more than a turn takes) holds this shared, beside
    /// others such; a test that times how a shard's tasks take turns holds it alone, so it times its own tasks and no
    /// other test's. Each takes it on its own thread before its work on the runtime begins, never on a shard. Beside step
    /// 5's busy tests, the turns test (`harness::copies::tests::g9_s3_an_owner_takes_a_long_update_in_turns_that_give_its_shard_up`)
    /// found its other task 1.4 to 3.1 ms apart at the median, past its four quanta (1.07 ms), in each of five runs; alone,
    /// within them in each.
    static SHARDS: parking_lot::RwLock<()> = parking_lot::RwLock::new(());

    /// Held by a test that keeps the runtime's shards busy ([`SHARDS`]).
    pub(crate) fn shards_kept_busy() -> parking_lot::RwLockReadGuard<'static, ()> {
        SHARDS.read()
    }

    /// Held by a test that times how a shard's tasks take turns ([`SHARDS`]).
    pub(crate) fn shards_timed() -> parking_lot::RwLockWriteGuard<'static, ()> {
        SHARDS.write()
    }

    /// A hub of its own for a test: its data in a folder that lives as long as the process (tests
    /// end with it), a QUIC endpoint on a free loopback port, no runners, its agents off unless a
    /// test turns terminals on.
    pub(crate) async fn scratch_hub() -> anyhow::Result<Arc<Hub>> {
        scratch_hub_with(|_| {}).await
    }

    pub(crate) async fn scratch_hub_with(f: impl FnOnce(&mut Config)) -> anyhow::Result<Arc<Hub>> {
        scratch_hub_on(crate::rt::clock(), f).await
    }

    /// A scratch hub whose time is `clock`'s (a `kiln_seams::clock::SimClock`, mostly).
    pub(crate) async fn scratch_hub_on(clock: Arc<dyn kiln_seams::Clock>, f: impl FnOnce(&mut Config)) -> anyhow::Result<Arc<Hub>> {
        scratch_hub_in(clock, None, false, f).await
    }

    /// A scratch hub whose bus serves the persisted class too, as the server's does: for the tests
    /// of kiln's events (crate::events), which ride it. The others' buses serve the ephemeral class
    /// alone: a test's hub lives as long as its process, and each persisted node holds its threads
    /// and logs for as long, a test binary's hundred of them its cores.
    pub(crate) async fn persisted_hub_on(clock: Arc<dyn kiln_seams::Clock>, f: impl FnOnce(&mut Config)) -> anyhow::Result<Arc<Hub>> {
        scratch_hub_in(clock, None, true, f).await
    }

    /// A scratch hub whose kiln.db is the file `db` and whose bus keeps its data in `bus`: a
    /// second one on them takes up where the first stopped (a restart).
    pub(crate) async fn restartable_hub(clock: Arc<dyn kiln_seams::Clock>, db: &std::path::Path, bus: &std::path::Path) -> anyhow::Result<Arc<Hub>> {
        let bus = bus.to_string_lossy().into_owned();
        scratch_hub_in(clock, Some(db.to_path_buf()), true, move |cfg| cfg.bus.data = bus).await
    }

    async fn scratch_hub_in(clock: Arc<dyn kiln_seams::Clock>, db: Option<std::path::PathBuf>, persisted: bool, f: impl FnOnce(&mut Config)) -> anyhow::Result<Arc<Hub>> {
        let dir = tempfile::tempdir()?.keep();
        let mut cfg = Config::default();
        cfg.server.data_dir = dir.clone();
        cfg.terminal.enabled = false;
        f(&mut cfg);
        // Its slowest parts, and apart: the store's migrations (about 20 ms) and its keys (the
        // process's first key starts the crypto library, about as long), made side by side on the
        // blocking pool, not on the runtime's threads, so the tests beside it go on meanwhile. The
        // keys are throwaway: written without waiting for the disk.
        let pki_dir = dir.join("pki");
        let (store, keys) = futures::join!(
            crate::rt::blocking(move || match db {
                Some(db) => Store::open(&db),
                None => Store::in_memory(),
            }),
            crate::rt::blocking(move || -> anyhow::Result<_> {
                let pki = crate::pki::Pki::open_scratch(&pki_dir)?;
                let leaf = pki.server_leaf(&[])?;
                Ok((pki, leaf))
            })
        );
        let (store, (pki, leaf)) = (store??, keys??);
        let pki = Arc::new(pki);
        // What comes in, taken by none: the fan's control loop alone, its queue kept (runner-fan: `net::start` went).
        let (fan, _events) = crate::wire::fan::Fan::with(crate::wire::fan::InOrder, 1, crate::net::EVENTS_BYTES);
        let net = crate::net::start_with(crate::net::Spec { bind: "127.0.0.1:0".parse()?, ca_der: pki.ca_der.clone(), leaf, server: true, dial_deadline: crate::net::DIAL_DEADLINE, revoked_keys: Vec::new() }, crate::net::OUT_BYTES, Box::new(fan))?;
        let logs = Logs::new(dir.join("logs"), 1 << 20);
        let bus = if persisted { crate::bus::QuiverBus::open_server(&cfg.bus, &cfg.bus.data_in(&cfg.server.data_dir), Arc::clone(&clock))? } else { crate::bus::QuiverBus::open(&cfg.bus, Arc::clone(&clock))? };
        Hub::new_with_bus(clock, Arc::new(cfg), store, logs, net, pki, "test-admin-token".into(), "test-join-token".into(), Shutdown::new(), bus)
    }

    /// Goal 3, plan 01a1a59c2a8e step 5 (perf/api-pass's leftover): a terminal whose relay has no
    /// room holds up none of the runners' messages, and loses nothing for good: its output that finds
    /// no room is a gap its relay repairs from the machine's replay, and its end waits for room on its
    /// own, after what came before it. The hub's loop used to wait for room, every runner's messages
    /// behind it.
    #[test]
    fn g3_s5_a_terminal_relay_with_no_room_holds_up_no_runners_message() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = scratch_hub().await?;
            hub.peers.update(|m| m.insert(1, "box".into()));
            let session = crate::termsess::Session::detached("t1".into(), "agent:box".into(), None, 80, 24);
            hub.terminals.insert(&session);
            // Its relay's queue full, nobody taking from it.
            let (tx, rx) = crate::memory::queue::places::<TermOut>(&crate::memory::owners::CHANNELS, 1);
            tx.try_send(TermOut::Data(b"first".to_vec())).map_err(|_| anyhow::anyhow!("the relay's queue had no room for its first"))?;
            hub.terms.update(|m| m.insert("box/t1".into(), tx.clone()));
            let heard = crate::rt::timeout(Duration::from_secs(5), hub.on_runner_msg(1, AgentMsg::TermOutput { session: "t1".into(), data: b"second".to_vec() })).await;
            anyhow::ensure!(heard.is_ok(), "the runners' messages waited on a terminal's relay with no room");
            anyhow::ensure!(session.gaps() == 1, "output with no room is a gap to repair: {}", session.gaps());
            let ended = crate::rt::timeout(Duration::from_secs(5), hub.on_runner_msg(1, AgentMsg::TermExit { session: "t1".into(), code: Some(0), error: None })).await;
            anyhow::ensure!(ended.is_ok(), "the runners' messages waited to hand a terminal its end");
            // What the relay takes: what it had, then its end, which waited for room.
            let first = crate::rt::timeout(Duration::from_secs(5), rx.recv()).await;
            anyhow::ensure!(matches!(&first, Ok(Ok(TermOut::Data(d))) if d == b"first"), "{first:?}");
            let end = crate::rt::timeout(Duration::from_secs(5), rx.recv()).await;
            anyhow::ensure!(matches!(end, Ok(Ok(TermOut::Exit { code: Some(0), .. }))), "{end:?}");
            Ok(())
        })
    }

    /// CP-T3.2: an inbox's answer from a runner that no longer holds the session (a copy left
    /// behind there) counts for whoever waits for it, and leaves the session where it runs: its
    /// machine and owner as they were, so its messages still go there.
    #[test]
    fn an_inbox_answer_from_another_machine_leaves_the_session_where_it_runs() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = scratch_hub().await?;
            hub.peers.update(|m| m.insert(1, "box-a".into()));
            hub.peers.update(|m| m.insert(2, "box-b".into()));
            let say = |u: &crate::harness::core::Update| serde_json::to_string(u).map(|json| AgentMsg::Harness { session: "s1".into(), json });
            let spec = crate::harness::session::Spec { kind: "claude".into(), cwd: Some("/w".into()), ..Default::default() };
            let open = crate::harness::host::refusal("s1", &spec, "").into_iter().find(|u| matches!(u, crate::harness::core::Update::Open { .. })).ok_or_else(|| anyhow::anyhow!("no announcement"))?;
            hub.on_runner_msg(1, say(&open)?).await?;
            anyhow::ensure!(hub.harness.runs_on("s1", "box-a"), "it runs on box-a");
            let refused = crate::harness::inbox::Said { message: "m1".into(), answer: Err(crate::harness::inbox::Refusal::Moved { session: "s1".into(), epoch: 0 }) };
            hub.on_runner_msg(2, say(&crate::harness::core::Update::Inbox { said: Box::new(refused) })?).await?;
            assert!(hub.harness.runs_on("s1", "box-a"), "it still runs on box-a");
            assert_eq!(hub.instances.load().owner(&crate::instances::Owned::Agent("s1".into())), Some(1), "box-a's runner still holds it");
            Ok(())
        })
    }

    /// Goal 10, plan 01a1a87b2dae step 4: a runner's updates to its agent sessions are judged and taken
    /// by the shards that own their copies, each session's in the order its runner sent them. The hub's
    /// loop answers each channel item taken once its owner's inbox has it, and waits for no owner to
    /// take one, but an Open or a Closed, which it reads the runner's next word after (where the session
    /// runs, which process holds it). What another machine says of a session that runs here counts for
    /// nothing, and its item is still answered taken.
    #[test]
    fn g10_s4_a_runners_session_updates_are_judged_and_taken_by_their_owners() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::model::{Body, Event};
        use crate::wire::channel::Frame;
        crate::rt::block_on(async {
            let hub = hub_with_runner(crate::rt::clock(), "fedora", proto::CAP_UNREAD | proto::CAP_WIRE).await?;
            join_runner(&hub, 2, "box-b", proto::CAP_UNREAD | proto::CAP_WIRE);
            let say = |id: &str, u: &Update| serde_json::to_string(u).map(|json| AgentMsg::Harness { session: id.to_owned(), json });
            let said = |id: &str, n: u64| Event { id: format!("{id}-e{n}"), seq: n, at: 1, parent: None, by: None, body: Body::Assistant { text: format!("{n}: the retry loop reads its deadline"), streaming: false } };
            let ids = ["s0", "s1", "s2", "s3"];
            for id in ids {
                let open = Update::Open { info: Box::new(crate::harness::session::Info { id: id.into(), kind: "claude".into(), ..Default::default() }), since: None };
                hear_item(&hub, 1, &format!("s:{id}#a1"), 1, &say(id, &open)?).await?;
                // Taken by its owner before the loop went on: where it runs, and which process holds it.
                anyhow::ensure!(hub.harness.located_on(id, "fedora"), "{id} opened on fedora");
                assert_eq!(hub.instances.load().owner(&crate::instances::Owned::Agent(id.into())), Some(1), "{id} held by the runner's process");
            }
            // Ten events each, the sessions' interleaved, each one an item of its session's stream.
            for n in 1..=10u64 {
                for id in ids {
                    hear_item(&hub, 1, &format!("s:{id}#a1"), n + 1, &say(id, &Update::Events { events: vec![said(id, n)] })?).await?;
                }
            }
            // Another machine's word on s0, which runs on fedora: a copy left behind there.
            hear_item(&hub, 2, "s:s0#b1", 1, &say("s0", &Update::Events { events: vec![said("s0", 11)] })?).await?;
            let answered = |stream: &str| {
                hub.sent.lock().iter().rev().find_map(|(_, m)| match m {
                    ServerMsg::Wire(Frame::Ack(has)) if has.stream == stream => Some((has.upto, has.nacks.len())),
                    _ => None,
                })
            };
            for id in ids {
                hub.harness.applied(id).await;
                let events = hub.harness.events_json(id, None, 20).ok_or_else(|| anyhow::anyhow!("{id}'s copy didn't say its events"))?;
                let events: serde_json::Value = serde_json::from_slice(&events)?;
                let seen: Vec<&str> = events.get("events").and_then(serde_json::Value::as_array).into_iter().flatten().filter_map(|e| e.get("id").and_then(serde_json::Value::as_str)).collect();
                let want: Vec<String> = (1..=10).map(|n| format!("{id}-e{n}")).collect();
                assert_eq!(seen, want, "{id}: in the order sent, nothing from box-b");
                assert_eq!(answered(&format!("s:{id}#a1")), Some((11, 0)), "{id}: every item answered taken");
            }
            assert_eq!(answered("s:s0#b1"), Some((1, 0)), "box-b's item answered taken, counting for nothing");
            Ok(())
        })
    }

    /// A test hub's loops, one a shard and its control loop, as the server starts them (`Hub::net_loops`): the net
    /// thread's end that hands them what a runner sends, and how far each has taken (goal 10, plan 01a1a87b2dae step 4).
    /// runner-fan (P): what's for one agent session rides the session's own command streams (a command for it, its close, a
    /// file its message carries, its release, a page streamed for whoever views it); its opening and a package it goes on
    /// from stay on the process's, as does all that isn't a session's. And an item the server's commands put on a session's
    /// own stream is one the runner's fan hands that session's loop.
    #[test]
    fn runner_fan_a_sessions_commands_ride_its_own_streams_to_its_loop() -> anyhow::Result<()> {
        let s = || "s1".to_owned();
        let lanes = [
            ServerMsg::HarnessCmd { session: s(), json: "{}".into() },
            ServerMsg::HarnessClose { session: s() },
            ServerMsg::HarnessFile { session: s(), file: "f".into(), offset: 0, data: Vec::new(), done: true },
            ServerMsg::HarnessRelease { session: s() },
            ServerMsg::StreamOpen { stream: "st".into(), session: s(), json: "{}".into() },
            ServerMsg::StreamControl { stream: "st".into(), session: s(), json: "{}".into() },
            ServerMsg::StreamInput { stream: "st".into(), session: s(), json: "{}".into() },
            ServerMsg::StreamClose { stream: "st".into(), session: s() },
        ];
        for m in &lanes {
            anyhow::ensure!(session_lane(m) == Some("s1"), "{} rides its session's own streams", crate::wire::server_name(m));
        }
        for m in [ServerMsg::HarnessOpen { session: s(), json: "{}".into() }, ServerMsg::HarnessPackage { session: s(), offset: 0, data: Vec::new(), done: true }, ServerMsg::Cancel { job: "j".into(), reason: "r".into() }] {
            anyhow::ensure!(session_lane(&m).is_none(), "{} rides the process's", crate::wire::server_name(&m));
        }
        let mut c = crate::wire::commands::Commands::new(7);
        c.on_frame(1, crate::wire::channel::Receiver::new().have());
        c.lanes(1, true);
        let body = proto::encode(lanes.first().ok_or_else(|| anyhow::anyhow!("none"))?)?;
        let _answer = c.push(1, crate::wire::commands::Kind::Live, Some("s1"), body, Instant::now());
        let (_, frame) = c.poll(Instant::now()).into_iter().next().ok_or_else(|| anyhow::anyhow!("nothing sent"))?;
        let data = proto::encode(&ServerMsg::Wire(frame))?;
        anyhow::ensure!(crate::wire::fan::peek_server(Kind::Control, proto::LANE_CONTROL, &data).0 == crate::wire::fan::Head::Session("s1"), "the runner's fan hands it session s1's loop");
        Ok(())
    }

    fn hub_loops(hub: &Arc<Hub>) -> (crate::wire::fan::Fan, Arc<crate::wire::fan::Marks>) {
        let (fan, loops) = crate::wire::fan::Fan::new(crate::wire::fan::shards(), crate::net::EVENTS_BYTES);
        let marks = Arc::clone(&loops.marks);
        hub.net_loops(loops);
        (fan, marks)
    }

    /// `msg` from runner process `peer`, as the net thread hands it to the hub's loops.
    fn hand(fan: &mut crate::wire::fan::Fan, peer: PeerId, msg: &AgentMsg) -> anyhow::Result<()> {
        use crate::net::Hand as _;
        let ev = NetEvent::Frame { peer, lane: proto::LANE_TERM, kind: Kind::Term, data: proto::encode(msg)?, at: Instant::now() };
        anyhow::ensure!(fan.hand(ev).is_ok(), "the hub's loops took nothing");
        Ok(())
    }

    /// Sessions whose copies are owned by each shard in turn, `n` of them: their updates go to every session loop
    /// there is.
    fn spread_sessions(n: usize) -> Vec<String> {
        let shards = crate::wire::fan::shards();
        (0..n).map(|i| (0u64..).map(|j| format!("s{i}-{j}")).find(|id| crate::harness::copies::owner_of(id, shards) == i % shards).unwrap_or_default()).collect()
    }

    /// Goal 10, plan 01a1a87b2dae step 4, wire/link.rs as it is: each stream's items are taken with the receivers of the
    /// loop the net thread hands its frames to, which is read off its name alike: a session's, its owner's shard's session
    /// loop's own; a terminal's, the control loop's. So no loop takes another's receivers, and their lock is one loop's.
    /// What the server says it has of a runner's streams is the loops' together, in the order of their names.
    #[test]
    fn g10_s4_a_streams_items_are_taken_with_its_loops_own_receivers() -> anyhow::Result<()> {
        use crate::wire::channel::Frame;
        crate::rt::block_on(async {
            let hub = hub_with_runner(crate::rt::clock(), "fedora", proto::CAP_UNREAD | proto::CAP_WIRE).await?;
            let shards = crate::wire::fan::shards();
            anyhow::ensure!(hub.session_wire.len() == shards, "{} session loops' receivers for {shards} shards", hub.session_wire.len());
            let ids = spread_sessions(2 * shards);
            for id in &ids {
                let stream = format!("s:{id}#a1");
                let data = proto::encode(&AgentMsg::Wire(Frame::Item { stream: stream.clone(), seq: 1, low: 0, body: Vec::new() }))?;
                let crate::wire::fan::Head::Session(peeked) = crate::wire::fan::peek(Kind::Term, proto::LANE_TERM, &data) else { anyhow::bail!("{stream}: not a session's") };
                let own = hub.session_wire.get(crate::harness::copies::owner_of(peeked, shards));
                anyhow::ensure!(own.is_some_and(|own| std::ptr::eq(hub.links_of(&stream), own)), "{stream}: taken with another loop's receivers");
            }
            anyhow::ensure!(std::ptr::eq(hub.links_of("t:term1#a1"), &hub.wire), "a terminal's stream: the control loop's");
            // Items of each, taken: what's said again is every stream, in the order of its name, as one loop's said it.
            let none = serde_json::to_string(&crate::harness::core::Update::Events { events: Vec::new() })?;
            let say = |id: &str| proto::encode(&AgentMsg::Harness { session: id.to_owned(), json: none.clone() });
            for id in &ids {
                let _ = hub.on_net(NetEvent::Frame { peer: 1, lane: proto::LANE_TERM, kind: Kind::Term, data: proto::encode(&AgentMsg::Wire(Frame::Item { stream: format!("s:{id}#a1"), seq: 1, low: 0, body: say(id)? }))?, at: Instant::now() }).await;
            }
            let _ = hub.on_net(NetEvent::Frame { peer: 1, lane: proto::LANE_TERM, kind: Kind::Term, data: proto::encode(&AgentMsg::Wire(Frame::Item { stream: "t:term1#a1".into(), seq: 1, low: 0, body: proto::encode(&AgentMsg::TermOutput { session: "term1".into(), data: vec![1] })? }))?, at: Instant::now() }).await;
            let Frame::Have(has) = hub.wire_have("fedora") else { anyhow::bail!("not a Have") };
            let mut want: Vec<String> = ids.iter().map(|id| format!("s:{id}#a1")).collect();
            want.push("t:term1#a1".into());
            want.sort();
            assert_eq!(has.iter().map(|h| h.stream.clone()).collect::<Vec<_>>(), want, "every stream, once, in the order of its name");
            assert!(has.iter().all(|h| h.upto == 1), "each item answered: {has:?}");
            Ok(())
        })
    }

    /// Goal 10, plan 01a1a87b2dae step 4: a runner's Caps, a barrier on the hub's control loop, comes after its sessions'
    /// updates on their owners' session loops, however long those take: the Have it's answered with says every item before
    /// it taken (its loops answered each), as the one loop's did.
    #[test]
    fn g10_s4_a_barrier_reads_what_its_runner_sent_before_it_on_the_session_loops() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::wire::channel::Frame;
        crate::rt::block_on(async {
            let hub = hub_with_runner(crate::rt::clock(), "fedora", proto::CAP_UNREAD | proto::CAP_WIRE).await?;
            let (mut fan, _) = hub_loops(&hub);
            let say = |id: &str, u: &Update| -> anyhow::Result<Vec<u8>> { proto::encode(&AgentMsg::Harness { session: id.to_owned(), json: serde_json::to_string(u)? }) };
            let ids = spread_sessions(6);
            const ITEMS: u64 = 12;
            for id in &ids {
                let open = Update::Open { info: Box::new(crate::harness::session::Info { id: id.clone(), kind: "claude".into(), ..Default::default() }), since: None };
                hand(&mut fan, 1, &AgentMsg::Wire(Frame::Item { stream: format!("s:{id}#a1"), seq: 1, low: 0, body: say(id, &open)? }))?;
            }
            for seq in 2..=ITEMS {
                for id in &ids {
                    let ev = crate::harness::model::Event { id: format!("{id}-e{seq}"), seq, at: 1, parent: None, by: None, body: crate::harness::model::Body::Assistant { text: format!("{seq}"), streaming: false } };
                    hand(&mut fan, 1, &AgentMsg::Wire(Frame::Item { stream: format!("s:{id}#a1"), seq, low: seq - 1, body: say(id, &Update::Events { events: vec![ev] })? }))?;
                }
            }
            hand(&mut fan, 1, &AgentMsg::Caps { caps: proto::CAP_UNREAD | proto::CAP_WIRE })?;
            let have = crate::rt::timeout(Duration::from_secs(30), async {
                loop {
                    let said = hub.sent.lock().iter().find_map(|(p, m)| match m {
                        ServerMsg::Wire(Frame::Have(h)) if *p == 1 => Some(h.clone()),
                        _ => None,
                    });
                    if let Some(h) = said {
                        return h;
                    }
                    crate::rt::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .map_err(|_| anyhow::anyhow!("no Have in 30 s"))?;
            for id in &ids {
                let stream = format!("s:{id}#a1");
                let upto = have.iter().find(|h| h.stream == stream).map(|h| h.upto);
                assert_eq!(upto, Some(ITEMS), "{stream}: the Have said before its runner's updates were taken");
            }
            // Each answered on its stream, in order, before the Have.
            let sent = hub.sent.lock().clone();
            let have_at = sent.iter().position(|(_, m)| matches!(m, ServerMsg::Wire(Frame::Have(_)))).unwrap_or(0);
            for id in &ids {
                let stream = format!("s:{id}#a1");
                let acks: Vec<u64> = sent
                    .iter()
                    .take(have_at)
                    .filter_map(|(_, m)| match m {
                        ServerMsg::Wire(Frame::Ack(h)) if h.stream == stream => Some(h.upto),
                        _ => None,
                    })
                    .collect();
                assert_eq!(acks, (1..=ITEMS).collect::<Vec<_>>(), "{stream}: each item answered once, in order, before the Have");
            }
            Ok(())
        })
    }

    /// Goal 10, plan 01a1a87b2dae step 4: a runner's Opens on their sessions' loops, then its inventory, a barrier on the
    /// control loop. The inventory is read once the Opens are taken by the sessions' owners: the sessions it no longer
    /// lists are let go of, as when one loop took both, never left to an Open taken after it; the one it lists stays,
    /// where it runs.
    #[test]
    fn g10_s4_a_runners_inventory_is_read_after_its_opens() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::wire::channel::Frame;
        crate::rt::block_on(async {
            let hub = hub_with_runner(crate::rt::clock(), "fedora", proto::CAP_UNREAD | proto::CAP_WIRE).await?;
            let (mut fan, marks) = hub_loops(&hub);
            let say = |id: &str, u: &Update| -> anyhow::Result<Vec<u8>> { proto::encode(&AgentMsg::Harness { session: id.to_owned(), json: serde_json::to_string(u)? }) };
            let ids = spread_sessions(4);
            let mut handed = 0u64;
            for id in &ids {
                let open = Update::Open { info: Box::new(crate::harness::session::Info { id: id.clone(), kind: "claude".into(), ..Default::default() }), since: None };
                hand(&mut fan, 1, &AgentMsg::Wire(Frame::Item { stream: format!("s:{id}#a1"), seq: 1, low: 0, body: say(id, &open)? }))?;
                handed += 1;
            }
            let kept = ids.first().cloned().unwrap_or_default();
            let inventory = crate::harness::host::Inventory { sessions: vec![kept.clone()], ..Default::default() };
            hand(&mut fan, 1, &AgentMsg::HarnessInventory { json: serde_json::to_string(&inventory)? })?;
            handed += 1;
            crate::rt::timeout(Duration::from_secs(30), async {
                while marks.taken(crate::wire::fan::CONTROL) < handed {
                    crate::rt::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .map_err(|_| anyhow::anyhow!("the inventory wasn't taken in 30 s"))?;
            for id in &ids {
                hub.harness.applied(id).await;
            }
            anyhow::ensure!(hub.harness.located_on(&kept, "fedora"), "{kept}, which the inventory lists, was let go of");
            for id in ids.iter().skip(1) {
                assert!(hub.harness.info(id).is_none(), "{id}: its Open was taken after the inventory that no longer lists it");
            }
            Ok(())
        })
    }

    /// Goal 10, plan 01a1a87b2dae step 4: a runner's link closing as its sessions' updates are still being taken on their
    /// loops. The close, a barrier, waits for them: its sessions end disconnected, as their runner is, none brought back by
    /// an update taken after the close.
    #[test]
    fn g10_s4_a_close_racing_its_runners_updates_leaves_its_sessions_disconnected() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::wire::channel::Frame;
        crate::rt::block_on(async {
            let hub = hub_with_runner(crate::rt::clock(), "fedora", proto::CAP_UNREAD | proto::CAP_WIRE).await?;
            let (mut fan, marks) = hub_loops(&hub);
            let say = |id: &str, u: &Update| -> anyhow::Result<Vec<u8>> { proto::encode(&AgentMsg::Harness { session: id.to_owned(), json: serde_json::to_string(u)? }) };
            let ids = spread_sessions(4);
            let mut handed = 0u64;
            for id in &ids {
                let open = Update::Open { info: Box::new(crate::harness::session::Info { id: id.clone(), kind: "claude".into(), ..Default::default() }), since: None };
                hand(&mut fan, 1, &AgentMsg::Wire(Frame::Item { stream: format!("s:{id}#a1"), seq: 1, low: 0, body: say(id, &open)? }))?;
                handed += 1;
            }
            // Many updates each, the last of them handed just before the link goes.
            for seq in 2..=40u64 {
                for id in &ids {
                    let state = Update::State { state: Box::new(crate::harness::model::State { status: crate::harness::model::Status::Running, ..Default::default() }) };
                    hand(&mut fan, 1, &AgentMsg::Wire(Frame::Item { stream: format!("s:{id}#a1"), seq, low: seq - 1, body: say(id, &state)? }))?;
                    handed += 1;
                }
            }
            {
                use crate::net::Hand as _;
                anyhow::ensure!(fan.hand(NetEvent::Closed { peer: 1, reason: "the link dropped".into() }).is_ok(), "the hub's loops took nothing");
                handed += 1;
            }
            // The close, the last event handed, taken whole on the control loop: its sessions said disconnected by their
            // owners. However long a loaded machine's loop takes over the 161 events (30 s for all of them ran out): the
            // wait gives up only once the loop takes none for a hang's bound.
            let stall = Duration::from_secs(300);
            let (mut taken, mut still_since) = (0, std::time::Instant::now());
            while marks.taken(crate::wire::fan::CONTROL) < handed {
                let now = marks.taken(crate::wire::fan::CONTROL);
                if now > taken {
                    (taken, still_since) = (now, std::time::Instant::now());
                }
                anyhow::ensure!(still_since.elapsed() < stall, "the close wasn't taken: the control loop took {taken} of {handed}, then nothing for {stall:?}");
                crate::rt::sleep(Duration::from_millis(5)).await;
            }
            anyhow::ensure!(hub.agents.load().get("fedora").is_some_and(|a| !a.connected), "the runner reads connected after its link closed");
            for id in &ids {
                hub.harness.applied(id).await;
                let status = hub.harness.info(id).map(|i| i.status);
                assert_eq!(status, Some(crate::harness::model::Status::Disconnected), "{id}: an update taken after its runner's link closed");
            }
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 5: an agent session's update on the channel is handed to its owner as one on a
    /// frame of its own is, without a runner message's whole handling again: its Open taken before the loop goes on,
    /// its events in the order sent, its arrival handed on with it (the loop doesn't time it as its own), each item
    /// answered taken, an inbox's answer from another machine's process leaving the session where it runs, and its
    /// Closed letting the session go before the loop goes on.
    #[test]
    fn g9_s5_a_session_update_on_the_channel_is_handed_on_as_one_on_its_own_frame_is() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::model::{Body, Event};
        use crate::wire::channel::Frame;
        crate::rt::block_on(async {
            let hub = hub_with_runner(crate::rt::clock(), "fedora", proto::CAP_UNREAD | proto::CAP_WIRE).await?;
            join_runner(&hub, 2, "box-b", proto::CAP_UNREAD | proto::CAP_WIRE);
            let say = |id: &str, u: &Update| -> anyhow::Result<AgentMsg> { Ok(AgentMsg::Harness { session: id.to_owned(), json: serde_json::to_string(u)? }) };
            // Item `seq` of `stream`, from runner process `peer`, carrying an update of `id`'s.
            let item = |peer: PeerId, stream: &str, seq: u64, id: &str, u: &Update| -> anyhow::Result<NetEvent> {
                let body = proto::encode(&say(id, u)?)?;
                let data = proto::encode(&AgentMsg::Wire(Frame::Item { stream: stream.to_owned(), seq, low: seq.saturating_sub(1), body }))?;
                Ok(NetEvent::Frame { peer, lane: LANE_CONTROL, kind: Kind::Control, data, at: Instant::now() })
            };
            let open = |id: &str| Update::Open { info: Box::new(crate::harness::session::Info { id: id.into(), kind: "claude".into(), ..Default::default() }), since: None };
            let said = |id: &str, n: u64| Event { id: format!("{id}-e{n}"), seq: n, at: 1, parent: None, by: None, body: Body::Assistant { text: format!("{n}: the retry loop reads its deadline"), streaming: false } };
            let answered = |stream: &str| {
                hub.sent.lock().iter().rev().find_map(|(_, m)| match m {
                    ServerMsg::Wire(Frame::Ack(has)) if has.stream == stream => Some((has.upto, has.nacks.len())),
                    _ => None,
                })
            };
            let holder = |id: &str| hub.instances.load().owner(&crate::instances::Owned::Agent(id.into()));
            // s1's on the channel, s2's on frames of their own.
            let mut arrived = Some(Arrived { at: Instant::now(), taken: Instant::now() });
            hub.on_net_at(item(1, "s:s1#a1", 1, "s1", &open("s1"))?, &mut arrived).await?;
            anyhow::ensure!(arrived.is_none(), "its arrival went to its owner with it");
            hear(&hub, 1, LANE_CONTROL, &say("s2", &open("s2"))?).await?;
            for id in ["s1", "s2"] {
                anyhow::ensure!(hub.harness.located_on(id, "fedora"), "{id}'s Open taken before the loop went on");
                assert_eq!(holder(id), Some(1), "{id} held by the runner's process");
            }
            for n in 1..=3u64 {
                hub.on_net(item(1, "s:s1#a1", n + 1, "s1", &Update::Events { events: vec![said("s1", n)] })?).await?;
                hear(&hub, 1, LANE_CONTROL, &say("s2", &Update::Events { events: vec![said("s2", n)] })?).await?;
            }
            for id in ["s1", "s2"] {
                hub.harness.applied(id).await;
                let events = hub.harness.events_json(id, None, 20).ok_or_else(|| anyhow::anyhow!("{id}'s copy didn't say its events"))?;
                let events: serde_json::Value = serde_json::from_slice(&events)?;
                let seen: Vec<&str> = events.get("events").and_then(serde_json::Value::as_array).into_iter().flatten().filter_map(|e| e.get("id").and_then(serde_json::Value::as_str)).collect();
                assert_eq!(seen, (1..=3).map(|n| format!("{id}-e{n}")).collect::<Vec<_>>(), "{id}: its events in the order sent");
            }
            assert_eq!(answered("s:s1#a1"), Some((4, 0)), "each of s1's items answered taken");
            // An inbox's answer from box-b, on its channel: it counts for whoever waits for it, and s1 runs where it ran.
            let refused = crate::harness::inbox::Said { message: "m1".into(), answer: Err(crate::harness::inbox::Refusal::Moved { session: "s1".into(), epoch: 0 }) };
            hub.on_net(item(2, "s:s1#b1", 1, "s1", &Update::Inbox { said: Box::new(refused) })?).await?;
            anyhow::ensure!(hub.harness.runs_on("s1", "fedora") && holder("s1") == Some(1), "s1 still runs on fedora, held by its process");
            assert_eq!(answered("s:s1#b1"), Some((1, 0)), "box-b's item answered taken");
            // s1's Closed: let go of before the loop goes on.
            hub.on_net(item(1, "s:s1#a1", 5, "s1", &Update::Closed)?).await?;
            assert_eq!(holder("s1"), None, "s1 let go of before the loop went on");
            assert_eq!(answered("s:s1#a1"), Some((5, 0)), "its Closed answered taken");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: the hub's loops take each runner frame as a piece of their turns on their shards,
    /// each given up for where it won't fit what's left of the turn's quantum, and lose none of them nor their order: a
    /// session's Open and its events, one a frame, end in its copy as sent (on its owner's session loop); a custody record
    /// among them goes on from the control loop to custody's intake and is kept there. Half the page of events a copy keeps
    /// in memory (`crate::harness::copies::EVENTS_PAGE`): none of them is spilled, so the copy's page from memory has them all.
    #[test]
    fn g9_s3_the_hubs_loops_take_each_frame_in_order_as_pieces_of_their_turns() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::model::{Body, Event};
        crate::rt::block_on(async {
            let hub = hub_with_runner(crate::rt::clock(), "fedora", proto::CAP_UNREAD | proto::CAP_WIRE).await?;
            let (mut fan, marks) = hub_loops(&hub);
            let say = |u: &Update| -> anyhow::Result<AgentMsg> { Ok(AgentMsg::Harness { session: "s1".into(), json: serde_json::to_string(u)? }) };
            let open = Update::Open { info: Box::new(crate::harness::session::Info { id: "s1".into(), kind: "claude".into(), ..Default::default() }), since: None };
            hand(&mut fan, 1, &say(&open)?)?;
            let events = u64::try_from(crate::harness::copies::EVENTS_PAGE / 2)?;
            for n in 1..=events {
                let said = Event { id: format!("e{n}"), seq: n, at: 1, parent: None, by: None, body: Body::Assistant { text: format!("{n}: the retry loop reads its deadline"), streaming: false } };
                hand(&mut fan, 1, &say(&Update::Events { events: vec![said] })?)?;
                if n == events / 2 {
                    let json = serde_json::json!({ "refused_hop": { "what": "a run", "from": "box-b", "why": "its digest differs" } }).to_string();
                    hand(&mut fan, 1, &AgentMsg::Custody { json })?;
                }
            }
            let deadline = Instant::now() + Duration::from_secs(60);
            let seen = loop {
                hub.harness.applied("s1").await;
                let page = hub.harness.events_json("s1", None, crate::harness::copies::EVENTS_PAGE).and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
                let seen: Vec<String> = page.as_ref().and_then(|v| v.get("events")).and_then(serde_json::Value::as_array).into_iter().flatten().filter_map(|e| e.get("id").and_then(serde_json::Value::as_str).map(str::to_owned)).collect();
                if seen.len() >= usize::try_from(events)? {
                    break seen;
                }
                if Instant::now() >= deadline {
                    let taken: Vec<u64> = (0..=crate::wire::fan::shards()).map(|k| marks.taken(k)).collect();
                    anyhow::bail!("{} of {events} events taken in 60 s; the loops took through {taken:?}, its page {page:?}", seen.len());
                }
                crate::rt::sleep(Duration::from_millis(5)).await;
            };
            assert_eq!(seen, (1..=events).map(|n| format!("e{n}")).collect::<Vec<_>>(), "every event, in the order sent");
            while crate::custody::refusals(&hub.store, 10).await?.is_empty() {
                anyhow::ensure!(Instant::now() < deadline, "the custody record among them wasn't kept");
                crate::rt::sleep(Duration::from_millis(5)).await;
            }
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 5 (g10s4-loops' 072ac3e6e6f, item fb6c9ad6): what a hub loop awaits for each runner
    /// message holds the message's handler by pointer, not a copy of it in each layer it's carried in (its piece, its label,
    /// the loop's slot, the channel item's label), and each handler holds the rare messages' larger handlers by pointer too.
    /// A future that holds another inline is at least that one's size, so each is smaller than what it holds by pointer. In
    /// the test build at rt-next 39fdabc55a1 the loop's per-message future was 50,512 bytes, on_net_at's 25,240, on_carried's
    /// 12,200, on_agent_msg's 11,960 and on_read's 10,840, each copied whole as it was made and handed down: the net loop's
    /// copies were 2.4% of a shard's CPU at rt6, and about 35% of the shards' memmoves in train 16's profile (g9p16fp).
    #[test]
    fn g9_s5_a_runner_messages_futures_hold_their_handlers_by_pointer() -> anyhow::Result<()> {
        static LABEL: hyper_rt::slow::Label = hyper_rt::slow::Label { name: "g9 s5" };
        crate::rt::block_on(async {
            let hub = scratch_hub().await?;
            let mut arrived = None;
            let ev = || NetEvent::Frame { peer: 1, lane: proto::LANE_CONTROL, kind: Kind::Control, data: Vec::new(), at: Instant::now() };
            let beat = || AgentMsg::Heartbeat { free_slots: 0, running: Vec::new() };
            // Each made and measured, never polled, and gone by the next.
            let per_message = size_of_val(&crate::wire::payload::Piece::take(Some(crate::wire::payload::Piece::job_log()), hub.on_net_labeled(&LABEL, ev(), &mut arrived)));
            let on_net_at = size_of_val(&hub.on_net_at(ev(), &mut arrived));
            let on_carried = size_of_val(&hub.on_carried(1, Ok(Carried::Nothing(beat())), &mut arrived));
            let on_agent_msg = size_of_val(&hub.on_agent_msg(1, Carried::Nothing(beat()), &mut arrived));
            let on_wire = size_of_val(&hub.on_wire(1, "a", crate::wire::channel::Frame::Have(Vec::new()), &mut arrived));
            let on_read = size_of_val(&hub.on_read(1, "a", FromRunner::AiSessions(Vec::new()), &mut arrived));
            // The largest of the rare handlers each boxes apart: a runner's machine gone, a job's end, an identity's ask.
            let agent_lost = size_of_val(&hub.agent_lost("a", "gone"));
            let job_done = size_of_val(&hub.job_done("a", JobResult { job: String::new(), status: Status::Running, exit_code: None, error: None, peak_rss: None, cpu_seconds: None }));
            let on_ask = size_of_val(&crate::identities::on_ask(&hub, 1, "a", proto::IdentityAsk::default()));
            let holds = [
                ("the loop's per-message future", per_message, "on_net_at", on_net_at),
                ("on_net_at", on_net_at, "on_carried", on_carried),
                ("on_wire", on_wire, "on_agent_msg", on_agent_msg),
                ("on_net_at", on_net_at, "agent_lost", agent_lost),
                ("on_agent_msg", on_agent_msg, "job_done", job_done),
                ("on_read", on_read, "identities::on_ask", on_ask),
            ];
            for (outer, o, inner, i) in holds {
                anyhow::ensure!(o < i, "{outer}'s future is {o} bytes, {inner}'s {i}: it holds {inner} by value");
            }
            Ok(())
        })
    }

    /// A runner process from before the channel is known by what it says, not by the clock: its
    /// upgrade preference before any Caps sends what waited for it its way at once. One that keeps
    /// the channel isn't taken for one from before it while the server is slow to hear its Caps.
    #[test]
    fn a_runner_from_before_the_channel_is_known_by_what_it_says_not_by_the_clock() -> anyhow::Result<()> {
        use crate::wire::commands::{Answer, COMING_FOR, Kind};
        crate::rt::block_on(async {
            let hub = scratch_hub().await?;
            hub.peers.update(|m| m.insert(1, "box-a".into()));
            hub.peers.update(|m| m.insert(2, "box-b".into()));
            let now = Instant::now();
            for peer in [1, 2] {
                hub.cmds.ask(move |c| c.coming(peer, now)).await;
            }
            let mut old = hub.cmds.ask(move |c| c.push(1, Kind::Parked, None, b"close s1".to_vec(), now)).await.ok_or_else(|| anyhow::anyhow!("the commands' owner stopped"))?;
            let mut new = hub.cmds.ask(move |c| c.push(2, Kind::Parked, None, b"close s2".to_vec(), now)).await.ok_or_else(|| anyhow::anyhow!("the commands' owner stopped"))?;
            // Heard long after its hello, a runner that keeps the channel still has time to say so.
            anyhow::ensure!(hub.cmds.ask(move |c| c.overdue(now + Duration::from_secs(10))).await.unwrap_or_default().is_empty(), "taken for old on the clock");
            // box-a says its upgrade preference before any Caps: from before the channel.
            hub.on_runner_msg(1, AgentMsg::UpdatePrefs { accept: true }).await?;
            anyhow::ensure!(hub.cmds.ask(|c| !c.is_coming(1)).await == Some(true), "still waiting for box-a's Caps");
            anyhow::ensure!(matches!(old.try_recv(), Ok(Some(Answer::Sent | Answer::Unreachable { .. }))), "what waited for box-a didn't go its way");
            // box-b says its Caps: what waited for it goes on the channel.
            hub.on_runner_msg(2, AgentMsg::Caps { caps: proto::CAP_WIRE }).await?;
            anyhow::ensure!(hub.cmds.ask(|c| !c.is_coming(2) && c.keeps(2)).await == Some(true), "box-b doesn't keep the channel");
            anyhow::ensure!(matches!(new.try_recv(), Ok(None)), "box-b's command was answered before it went");
            anyhow::ensure!(hub.cmds.ask(move |c| c.overdue(now + COMING_FOR)).await.unwrap_or_default().is_empty(), "box-b was taken for old after all");
            Ok(())
        })
    }

    #[test]
    fn an_outpost_has_its_label_and_no_slots() {
        let label = vec![crate::proto::OUTPOST_LABEL.to_owned()];
        assert!(is_outpost(&label, 0));
        // A runner given the label by hand still takes jobs.
        assert!(!is_outpost(&label, 4));
        assert!(!is_outpost(&["baremetal".to_owned()], 0));
    }

    #[test]
    fn an_outpost_invite_works_once_and_not_once_it_has_expired() {
        let invites = OutpostInvites::default();
        let (token, until) = invites.mint();
        assert!(until > crate::util::now_unix());
        assert!(!invites.take("not-an-invite"));
        assert!(invites.take(&token));
        assert!(!invites.take(&token), "an invite works once");
        invites.add("old".into(), Instant::now() - Duration::from_secs(1));
        assert!(!invites.take("old"), "an expired invite doesn't work");
    }

    /// docs/machines.md: what kiln says of a runner it knows by its machine id names it by its
    /// label, connected or not seen since the server started; its id stays what's looked up by. One
    /// known by its name is named by it, as before.
    #[test]
    fn a_runner_known_by_its_machine_id_is_named_by_its_label_in_what_kiln_says() -> anyhow::Result<()> {
        use crate::wire::commands::Answer;
        const STUDIO: &str = "5f2a9c3e0b1d4e6f8a7c9b0d1e2f3a4b";
        const ATTIC: &str = "0b1d4e6f8a7c9b0d1e2f3a4b5f2a9c3e";
        crate::rt::block_on(async {
            let hub = scratch_hub().await?;
            hub.put_agent(AgentLive { label: Some("studio".into()), connected: false, ..live_runner(STUDIO, "macos", "aarch64") });
            hub.put_agent(live_runner("fedora", "linux", "x86_64"));
            // Named while it hasn't connected since the server started.
            hub.set_label(ATTIC, "attic", "attic.local").await?;
            let shown = [STUDIO, ATTIC, "fedora", "nowhere"].map(|n| hub.shown(n));
            anyhow::ensure!(shown == ["studio", "attic", "fedora", "nowhere"], "{shown:?}");
            // A command to it while it's away, and an agent asked of it by the name people type.
            let away = hub.command_start(STUDIO, &ServerMsg::ProbeFacts).await;
            anyhow::ensure!(away.at_once() == Some(&Answer::Unreachable { why: "studio is not connected".into() }), "{:?}", away.at_once());
            let refused = crate::harness::lineage::pick(&hub, Some("studio"), "claude", None).err().map(|(_, why)| why);
            anyhow::ensure!(refused.as_deref() == Some("studio isn't connected (or doesn't run agents): pick another machine"), "{refused:?}");
            // Its target by the name people type is its id's; one by its id stays so.
            let by_id = format!("agent:{STUDIO}");
            let targets = ["agent:studio", by_id.as_str(), "agent:fedora"].map(|on| crate::harness::lineage::pick(&hub, Some(on), "claude", None).ok());
            anyhow::ensure!(targets == [Some(by_id.clone()), Some(by_id.clone()), Some("agent:fedora".to_owned())], "{targets:?}");
            Ok(())
        })
    }

    /// A runner record for a test's store: `name`, not connected, on `hostname`.
    fn record(name: &str, hostname: &str, ephemeral: bool) -> crate::store::AgentRow {
        crate::store::AgentRow {
            name: name.into(),
            labels: vec![],
            slots: 1,
            hostname: hostname.into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            version: "1".into(),
            ephemeral,
            connected: false,
            last_seen: 5,
            enrolled_at: 1,
        }
    }

    /// docs/machines.md, "Duplicates fold into the machine's id": the machine known by its id
    /// connected, every other record whose runner said it is that machine, not connected, folds
    /// into it, audited; the id keeps its label. A connected one stays until it isn't; one known by
    /// another machine id, and one sharing only its host name, never fold.
    #[test]
    fn machines_a_duplicate_of_a_machine_folds_into_its_id() -> anyhow::Result<()> {
        const ID: &str = "5f2a9c3e0b1d4e6f8a7c9b0d1e2f3a4b";
        const OTHER: &str = "0b1d4e6f8a7c9b0d1e2f3a4b5f2a9c3e";
        crate::rt::block_on(async {
            let hub = scratch_hub().await?;
            for name in [ID, "fedora.attlocal.net", "attic", OTHER, "same-host-name"] {
                hub.store.upsert_agent(record(name, "fedora.attlocal.net", false)).await?;
            }
            hub.set_label(ID, "kiln-a094a12d.attlocal.net", "fedora.attlocal.net").await?;
            for name in ["fedora.attlocal.net", "attic", OTHER] {
                hub.store.set_agent_machine(name.into(), ID.into()).await?;
            }
            hub.store.kv_set(format!("{}fedora.attlocal.net", crate::harness::limit::KEY_PREFIX), "{}".into()).await?;
            join_runner(&hub, 1, ID, 0);
            join_runner(&hub, 2, "attic", 0);
            let folded = hub.fold_duplicates(ID).await?;
            anyhow::ensure!(folded == ["fedora.attlocal.net"], "{folded:?}");
            let left: Vec<String> = hub.store.agents().await?.into_iter().map(|a| a.name).collect();
            anyhow::ensure!(!left.iter().any(|n| n == "fedora.attlocal.net"), "the duplicate's record is gone: {left:?}");
            for name in [ID, "attic", OTHER, "same-host-name"] {
                anyhow::ensure!(left.iter().any(|n| n == name), "{name} stays: {left:?}");
            }
            anyhow::ensure!(hub.shown(ID) == "kiln-a094a12d.attlocal.net", "the id keeps its label: {}", hub.shown(ID));
            anyhow::ensure!(hub.store.kv_get(format!("{}{ID}", crate::harness::limit::KEY_PREFIX)).await?.is_some(), "what was set for the duplicate moved");
            let said = hub.store.call(|c| Ok(c.query_row("SELECT detail FROM audit2 WHERE action = 'agent.merged'", [], |r| r.get::<_, String>(0))?)).await?;
            anyhow::ensure!(said == format!("fedora.attlocal.net was machine {ID}: folded into kiln-a094a12d.attlocal.net"), "{said}");
            // The connected one goes away: it folds the next time the machine says which it is.
            hub.instances.update(|i| i.lost(2));
            hub.change_agent("attic", |a| a.connected = false);
            anyhow::ensure!(hub.fold_duplicates(ID).await? == ["attic"]);
            anyhow::ensure!(hub.fold_duplicates(ID).await?.is_empty(), "nothing left to fold");
            anyhow::ensure!(hub.fold_duplicates("fedora.attlocal.net").await?.is_empty(), "a name isn't a machine's id");
            Ok(())
        })
    }

    /// A Hello of runner `name`, as a runner from before on linux says it.
    fn hello_of(name: &str) -> Hello {
        Hello {
            protocol: proto::VERSION,
            name: name.to_owned(),
            labels: Vec::new(),
            slots: 1,
            hostname: "fedora.attlocal.net".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            version: "test".into(),
            ephemeral: false,
            dedicated_job: None,
            running: Vec::new(),
            terminal: true,
        }
    }

    /// docs/machines.md, "One record a machine": a runner whose certificate carries a name from
    /// before is keyed by its machine id the moment it connects, without a restart: by the id it
    /// says before its Hello, or by the one kiln knows for the name (an older runner that says
    /// none). What was kept under the name is the id's, the name is its label and resolves to it,
    /// and nothing is kept under it. One saying another machine than kiln knows for its name is
    /// refused.
    #[test]
    fn machines_a_runner_connecting_by_a_name_from_before_is_keyed_by_its_id_at_once() -> anyhow::Result<()> {
        const ID: &str = "5f2a9c3e0b1d4e6f8a7c9b0d1e2f3a4b";
        const OTHER: &str = "0b1d4e6f8a7c9b0d1e2f3a4b5f2a9c3e";
        crate::rt::block_on(async {
            let hub = scratch_hub().await?;
            hub.store.upsert_agent(record("fedora", "fedora.attlocal.net", false)).await?;
            hub.store.set_agent_machine("fedora".into(), ID.into()).await?;
            hub.store.kv_set(format!("{}fedora", crate::harness::limit::KEY_PREFIX), "{}".into()).await?;
            hub.pending_peers.update(|m| m.insert(1, "fedora".into()));
            hear(&hub, 1, proto::LANE_CONTROL, &AgentMsg::Machine { id: ID.into() }).await?;
            hear(&hub, 1, proto::LANE_CONTROL, &AgentMsg::Hello(hello_of("fedora"))).await?;
            anyhow::ensure!(hub.agent_of(1).as_deref() == Some(ID), "keyed by its id: {:?}", hub.agent_of(1));
            anyhow::ensure!(hub.agents.load().get(ID).is_some_and(|a| a.connected) && !hub.agents.load().contains_key("fedora"));
            let names: Vec<String> = hub.store.agents().await?.into_iter().map(|a| a.name).collect();
            anyhow::ensure!(names == [ID], "one record, by its id: {names:?}");
            anyhow::ensure!(hub.shown(ID) == "fedora", "shown by the name it had: {}", hub.shown(ID));
            anyhow::ensure!(hub.machine_of("fedora") == ID && hub.machine_named("fedora").as_deref() == Some(ID), "the name resolves to the id");
            anyhow::ensure!(hub.store.kv_get(format!("{}{ID}", crate::harness::limit::KEY_PREFIX)).await?.is_some(), "what was set for it is the id's");
            anyhow::ensure!(hub.store.machine_names().await?.get("fedora").map(String::as_str) == Some(ID), "the name kept as the machine's");
            // Another process of it, from before machine ids were said (no `Machine`): the same.
            hub.pending_peers.update(|m| m.insert(2, "fedora".into()));
            hear(&hub, 2, proto::LANE_CONTROL, &AgentMsg::Instance(proto::InstanceHello { id: "u1".into(), created_at: 5, pid: 2 })).await?;
            hear(&hub, 2, proto::LANE_CONTROL, &AgentMsg::Hello(hello_of("fedora"))).await?;
            anyhow::ensure!(hub.agent_of(2).as_deref() == Some(ID), "{:?}", hub.agent_of(2));
            // One saying it's another machine: refused, keyed by nothing.
            hub.pending_peers.update(|m| m.insert(3, "fedora".into()));
            hear(&hub, 3, proto::LANE_CONTROL, &AgentMsg::Machine { id: OTHER.into() }).await?;
            hear(&hub, 3, proto::LANE_CONTROL, &AgentMsg::Hello(hello_of("fedora"))).await?;
            anyhow::ensure!(hub.agent_of(3).is_none(), "refused: {:?}", hub.agent_of(3));
            Ok(())
        })
    }

    /// docs/machines.md, "One record a machine": as the server starts, two records of one machine
    /// kept under names (it joined twice, by two host names) end as one record by its id, shown by
    /// the first's name, both names resolving to it; a record that never said its id, and a job's
    /// pod, stay as they are.
    #[test]
    fn machines_two_records_of_one_machine_end_as_one_as_the_server_starts() -> anyhow::Result<()> {
        const ID: &str = "5f2a9c3e0b1d4e6f8a7c9b0d1e2f3a4b";
        crate::rt::block_on(async {
            let hub = scratch_hub().await?;
            for (name, ephemeral) in [("fedora.attlocal.net", false), ("kiln-a094a12d.attlocal.net", false), ("old-box", false), ("kiln-abc-1", true)] {
                hub.store.upsert_agent(record(name, name, ephemeral)).await?;
            }
            for name in ["fedora.attlocal.net", "kiln-a094a12d.attlocal.net"] {
                hub.store.set_agent_machine(name.into(), ID.into()).await?;
            }
            let folded = hub.one_record_a_machine().await;
            anyhow::ensure!(folded == ["fedora.attlocal.net", "kiln-a094a12d.attlocal.net"], "{folded:?}");
            let mut names: Vec<String> = hub.store.agents().await?.into_iter().map(|a| a.name).collect();
            names.sort();
            anyhow::ensure!(names == [ID, "kiln-abc-1", "old-box"], "{names:?}");
            anyhow::ensure!(hub.shown(ID) == "fedora.attlocal.net", "{}", hub.shown(ID));
            anyhow::ensure!(hub.machine_of("kiln-a094a12d.attlocal.net") == ID && hub.machine_of("fedora.attlocal.net") == ID);
            let said = hub.store.call(|c| Ok(c.query_row("SELECT detail FROM audit2 WHERE action = 'agent.merged'", [], |r| r.get::<_, String>(0))?)).await?;
            anyhow::ensure!(said == format!("kiln-a094a12d.attlocal.net was machine {ID}: folded into fedora.attlocal.net"), "{said}");
            anyhow::ensure!(hub.one_record_a_machine().await.is_empty(), "once");
            Ok(())
        })
    }

    /// docs/machines.md, "One record a machine": sessions a record held fold onto its machine
    /// connected now: one the machine's runner holds is asked for its snapshot at once (it reads as
    /// it is, not disconnected), one it doesn't hold is gone, as its next inventory would say.
    #[test]
    fn machines_sessions_folded_onto_a_connected_machine_read_as_they_are() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::model::Status;
        use crate::harness::registry::Location;
        use crate::harness::session::Info;
        const ID: &str = "5f2a9c3e0b1d4e6f8a7c9b0d1e2f3a4b";
        crate::rt::block_on(async {
            let hub = scratch_hub().await?;
            let at = Location::Machine("fedora".into());
            for s in ["held", "gone"] {
                hub.harness.ingest(&at, s, Update::Open { info: Box::new(Info { id: s.into(), kind: "claude".into(), ..Info::default() }), since: None }).await;
            }
            hub.harness.machine_lost(&hub, "fedora").await;
            hub.store.upsert_agent(record("fedora", "fedora", false)).await?;
            hub.store.set_agent_machine("fedora".into(), ID.into()).await?;
            join_runner(&hub, 1, ID, 0);
            hub.instances.update(|inst| {
                inst.own(crate::instances::Owned::Agent("held".into()), 1);
                inst.reported(1);
            });
            anyhow::ensure!(hub.harness.info("held").is_some_and(|i| i.status == Status::Disconnected));
            anyhow::ensure!(hub.fold_duplicates(ID).await? == ["fedora"]);
            anyhow::ensure!(hub.harness.location("held") == Some(Location::Machine(ID.into())), "{:?}", hub.harness.location("held"));
            anyhow::ensure!(hub.harness.info("gone").is_none(), "one the machine doesn't hold is gone");
            // What the fold asked: the snapshot of the one it holds that reads disconnected.
            hub.harness.machine_lost(&hub, ID).await;
            let asked = hub.harness.moved_onto_connected(&hub, ID, &["held".to_owned()], &["held".to_owned()]).await;
            anyhow::ensure!(asked == ["held"], "its snapshot is asked for: {asked:?}");
            Ok(())
        })
    }

    /// docs/machines.md, "The grace to come back" (Ada, 2026-11-06: no arbitrary constants): the grace a runner gets to come back
    /// is learned from how long runners took to come back holding their work (`crate::comeback`), not 45 s whatever they
    /// did. A runner back holding a job 3 s after the server lost it teaches the grace (RFC 6298's bound: 3 s and four
    /// times 1.5 s); then one lost 30 s ago holding a job has it taken back, and one lost 1 s ago keeps its own. With the
    /// 45 s grace the one lost 30 s ago kept its job. What's learned is kept for the next start, which takes it.
    #[test]
    fn comeback_the_grace_a_runner_gets_is_learned_from_how_long_runners_took_to_come_back() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = scratch_hub().await?;
            let lost = |name: &str, ago: Duration, job: &str| AgentLive { connected: false, lost_at: Instant::now().checked_sub(ago), running: vec![job.to_owned()], busy: 1, ..live_runner(name, "linux", "x86_64") };
            anyhow::ensure!(hub.comeback.grace() == crate::comeback::prior(), "nothing learned yet: the runners' longest redial");
            hub.put_agent(lost("box", Duration::from_secs(3), "job-b"));
            hub.pending_peers.update(|m| m.insert(1, "box".into()));
            hear(&hub, 1, proto::LANE_CONTROL, &AgentMsg::Hello(Hello { running: vec!["job-b".into()], hostname: "box".into(), ..hello_of("box") })).await?;
            let grace = hub.comeback.grace();
            anyhow::ensure!((Duration::from_secs(9)..Duration::from_secs(20)).contains(&grace), "back in 3 s: {grace:?}");
            hub.put_agent(lost("gone", Duration::from_secs(30), "job-g"));
            hub.put_agent(lost("blip", Duration::from_secs(1), "job-x"));
            hub.check_lost_agents().await;
            let running = |n: &str| hub.agents.load().get(n).map(|a| a.running.clone()).unwrap_or_default();
            anyhow::ensure!(running("gone").is_empty(), "lost 30 s ago, past the learned grace: its job taken back");
            anyhow::ensure!(running("blip") == ["job-x"], "lost 1 s ago, within it: its job waits for it");
            // Kept: the next start takes it.
            let kept = hub.store.kv_get(crate::comeback::KEY.to_owned()).await?.ok_or_else(|| anyhow::anyhow!("not kept in kiln.db"))?;
            let next = crate::comeback::Comeback::default();
            next.take_kept(&kept);
            anyhow::ensure!(next.grace() == hub.comeback.grace(), "{kept}");
            Ok(())
        })
    }

    /// docs/machines.md, "Finished job pods are forgotten": a job pod's record goes once it's gone
    /// past its grace and its job ended; one gone within the grace stays.
    #[test]
    fn machines_a_finished_job_pod_is_forgotten_past_its_grace() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = scratch_hub().await?;
            for name in ["kiln-abc-1", "kiln-def-1", "box"] {
                hub.store.upsert_agent(record(name, name, name != "box")).await?;
            }
            let lost = |name: &str, ago: Duration| AgentLive {
                ephemeral: true,
                connected: false,
                lost_at: Instant::now().checked_sub(ago),
                running: Vec::new(),
                ..live_runner(name, "linux", "x86_64")
            };
            hub.put_agent(lost("kiln-abc-1", hub.comeback.grace() * 2));
            hub.put_agent(lost("kiln-def-1", Duration::from_secs(1)));
            // A server younger than the grace forgets none: pods may be coming back to it.
            anyhow::ensure!(hub.forget_finished_pods().await.is_empty());
            anyhow::ensure!(hub.forget_pods_but_waiting().await == ["kiln-abc-1"]);
            anyhow::ensure!(!hub.agents.load().contains_key("kiln-abc-1"));
            let names: Vec<String> = hub.agent_list().await.into_iter().map(|a| a.name).collect();
            anyhow::ensure!(names.contains(&"kiln-def-1".to_owned()) && names.contains(&"box".to_owned()) && !names.contains(&"kiln-abc-1".to_owned()), "{names:?}");
            Ok(())
        })
    }

    /// docs/machines.md, "Records that are stale": a record not connected, known by a name, whose
    /// runner never said its machine id, is stale (the API says `stale: true`); one that said it,
    /// one known by its id, a connected one and a job pod aren't, and say nothing of it.
    #[test]
    fn machines_a_record_that_never_said_its_machine_is_stale() -> anyhow::Result<()> {
        const ID: &str = "5f2a9c3e0b1d4e6f8a7c9b0d1e2f3a4b";
        crate::rt::block_on(async {
            let hub = scratch_hub().await?;
            for name in ["old-box", "said-box", ID, "live-box"] {
                hub.store.upsert_agent(record(name, name, false)).await?;
            }
            hub.store.upsert_agent(record("kiln-abc-1", "kiln-abc-1", true)).await?;
            hub.store.set_agent_machine("said-box".into(), ID.into()).await?;
            join_runner(&hub, 1, "live-box", 0);
            let list = hub.agent_list().await;
            let stale: Vec<&str> = list.iter().filter(|a| a.stale).map(|a| a.name.as_str()).collect();
            anyhow::ensure!(stale == ["old-box"], "{stale:?}");
            for a in &list {
                let v = serde_json::to_value(a)?;
                anyhow::ensure!(v.get("stale") == a.stale.then_some(&serde_json::Value::Bool(true)), "{}: {v}", a.name);
            }
            Ok(())
        })
    }

    /// The scheduler's headroom (W05, step 3): one rule places a CI leg, a v1 job and a command
    /// alike. On one fleet (a at 80% of its CPUs, b at 30%, c at 10% with its disk under pressure),
    /// asking 4 CPUs and 2 GiB: a would pass the target use, c's disk counts against it, and the
    /// engine, the v1 dispatcher and kiln run's ranking all pick b.
    #[test]
    fn one_headroom_rule_places_a_leg_a_v1_job_and_a_command_alike() -> anyhow::Result<()> {
        use kiln_ci::sched::routing::{LegRoute, RunnerView};
        use kiln_ci::sched::state::{Command, Config, Headroom, Input, LegIn, Resources, RunnerIn, Sample, Scheduler};
        const GIB: u64 = 1 << 30;
        let fleet = [("a", 0.8, 0.0), ("b", 0.3, 0.0), ("c", 0.1, 60.0)];
        let sample = |busy: f64, io: f64| Sample { cpus: 32, busy, load1: 0.0, mem_total: 64 * GIB, mem_used: 16 * GIB, io_full: Some(io) };
        let asked = Resources { cpu: 4.0, mem: 2 * GIB, ..Resources::default() };
        // The engine.
        let mut engine = Scheduler::new(Config::default());
        for (name, busy, io) in fleet {
            let view = RunnerView { name: name.into(), labels: vec!["local".into()], caps: 1 << 10, ..RunnerView::default() };
            engine.handle(Input::RunnerUp(Box::new(RunnerIn { id: name.into(), view, slots: 4, ..RunnerIn::default() })), 0);
            engine.handle(Input::RunnerSample { runner: name.into(), sample: sample(busy, io) }, 1_000);
        }
        let leg = LegIn { id: "build".into(), run: "r1".into(), repo: "acme/x".into(), request: asked, route: LegRoute { labels: vec!["local".into()], ..LegRoute::default() }, ..LegIn::default() };
        let c = engine.handle(Input::LegQueued(Box::new(leg)), 2_000);
        let by_engine = c.iter().find_map(|x| match x {
            Command::Assign { runner, .. } => Some(runner.clone()),
            _ => None,
        });
        // The v1 dispatcher.
        let by_v1 = v1_best(fleet.iter().map(|(name, busy, io)| V1Candidate { name, slots: 4, headroom: Some(Headroom::of(&sample(*busy, *io))), room: 0.0 }), &asked, 0.85);
        // kiln run.
        let info: Vec<crate::reach::directory::RunnerInfo> = fleet
            .iter()
            .map(|(name, busy, io)| crate::reach::directory::RunnerInfo {
                name: (*name).into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                connected: true,
                reach: true,
                cpus: 32,
                cpu_pct: (*busy * 100.0) as f32,
                mem_total: 64 * GIB,
                mem_used: 16 * GIB,
                facts: Some(Box::new(kiln_ci::facts::RunnerFacts { work: Some(kiln_ci::facts::WorkFacts { io_full: Some(*io) }), ..kiln_ci::facts::RunnerFacts::default() })),
                ..crate::reach::directory::RunnerInfo::default()
            })
            .collect();
        let (ranked, _) = crate::reach::directory::rank(&info, &crate::reach::directory::Want::parse("cpus=4,mem=2g")?, "elsewhere");
        let by_run = ranked.first().map(|c| c.runner.name.clone());
        assert_eq!((by_engine.as_deref(), by_v1, by_run.as_deref()), (Some("b"), Some("b"), Some("b")), "{c:?} {ranked:?}");
        Ok(())
    }

    /// The DB query audit (2026-11-06): the scheduler's timeout check acts on the jobs past their limit
    /// alone: a cancel to the runner of each (its time paused left out), then, a minute on without its
    /// runner's word, the job ended as timed out, saying its limit.
    #[test]
    fn dbq_r4_the_timeout_check_acts_on_the_jobs_past_their_limit() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = scratch_hub().await?;
            let now = crate::util::now_unix();
            hub.store
                .call(move |c| {
                    c.execute_batch(
                        "INSERT INTO repos (id, name, clone_url, created_at) VALUES ('r1', 'acme/x', '', 0);
                         INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, status, created_at) VALUES ('run1', 'r1', 1, 'ci', 'push', 'main', 'abc', 'running', 0);",
                    )?;
                    for (id, agent, started, paused_ms) in [("over", "ag-a", now - 200, 0), ("paused", "ag-a", now - 200, 100_000), ("within", "ag-b", now - 30, 0)] {
                        c.execute(
                            "INSERT INTO jobs (id, run_id, key, display, status, labels, needs, spec, agent, started_at, paused_ms) VALUES (?1, 'run1', ?1, ?1, 'running', '[]', '[]', '{\"timeout_minutes\":2}', ?2, ?3, ?4)",
                            rusqlite::params![id, agent, started, paused_ms],
                        )?;
                    }
                    Ok(())
                })
                .await?;
            hub.check_timeouts().await?;
            let sent: Vec<String> = hub.cancel_sent.load().keys().cloned().collect();
            anyhow::ensure!(sent == ["over"], "a cancel for the one past its limit alone: {sent:?}");
            let long_ago = Instant::now().checked_sub(Duration::from_secs(61)).ok_or_else(|| anyhow::anyhow!("no minute before now"))?;
            hub.cancel_sent.update(|c| c.insert("over".into(), long_ago));
            hub.check_timeouts().await?;
            let over = hub.store.job("over".into()).await?.ok_or_else(|| anyhow::anyhow!("over is gone"))?;
            anyhow::ensure!(over.status == Status::TimedOut && over.error.as_deref() == Some("exceeded 2 min"), "{:?} {:?}", over.status, over.error);
            anyhow::ensure!(hub.cancel_sent.load().is_empty(), "settled");
            for id in ["paused", "within"] {
                anyhow::ensure!(hub.store.job(id.into()).await?.is_some_and(|j| j.status == Status::Running), "{id} runs on");
            }
            Ok(())
        })
    }
}

