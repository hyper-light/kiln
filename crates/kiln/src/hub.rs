//! The server core: agent sessions, job bookkeeping and the UI event bus.
//!
//! All network input arrives as [`NetEvent`]s from the QUIC thread and is handled here; every
//! outcome is persisted through [`Store`] before it is broadcast, so a crash at any point loses
//! at most in-flight log bytes, never job state.

mod executor_enrollment;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;

use hyper_transport::PeerId;
use parking_lot::Mutex;
use serde::Serialize;
use futures::FutureExt as _;

use crate::rt::{Broadcast, Interval, Notify, Shutdown};

use crate::config::Config;
use crate::logs::Logs;
use crate::metrics::{self, HostSample};
use crate::net::{Kind, NetEvent, NetHandle};
use crate::proto::{self, AgentMsg, Hello, JobResult, LANE_CONTROL, LANE_TERM, ServerMsg};
use crate::store::{AgentRow, Job, Status, Store};
use crate::sql::OptionalExtension as _;
use crate::wire::events::ServerEvent;
use crate::wire::payload::{Carried, FromRunner};
use crate::wire::unread::{Code, Wire};

/// U17: marks a failed durable evidence write, distinct from validation/policy refusals.
#[derive(Debug)]
pub(crate) struct EvidenceStorageError(anyhow::Error);
impl std::fmt::Display for EvidenceStorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "durable runner evidence: {:#}", self.0) }
}
impl std::error::Error for EvidenceStorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> { Some(self.0.as_ref()) }
}

/// Apply only around storage operations; invalid requests must retain their final refusal.
pub(crate) fn evidence_storage(error: anyhow::Error) -> anyhow::Error { EvidenceStorageError(error).into() }


/// How long a persistent agent may be gone before its jobs are requeued.
pub(crate) const AGENT_GRACE: Duration = Duration::from_secs(45);
/// How often the newest runner process is reminded that an older one can be retired.
const RETIRE_REPEAT: Duration = Duration::from_secs(60);
/// Host samples kept per agent for the UI charts (1 h at 5 s).
const HISTORY: usize = 720;
/// How long an agent that declined a job is not offered that job again.
const DECLINE_BACKOFF: Duration = Duration::from_secs(15);
/// How long a command whose caller doesn't wait for it is waited for, to log what came of it.
pub(crate) const LOG_WITHIN: Duration = Duration::from_secs(300);
/// How long a person's command (through the API) waits for its runner's answer.
pub const COMMAND_WITHIN: Duration = Duration::from_secs(30);
/// How often the server says again what it has of each runner's channel streams.
const HAVE_AGAIN: Duration = Duration::from_secs(5);
/// How many sessions' digests one `ServerMsg::Kept` carries at the most.
const KEPT_A_FRAME: usize = 32;
/// How long an outpost invite works ([`OutpostInvites`]).
pub const OUTPOST_INVITE_TTL: Duration = Duration::from_secs(3600);

/// One-time invites for outposts (`kiln outpost`): each enrolls one, once, within
/// [`OUTPOST_INVITE_TTL`].
#[derive(Default)]
pub struct OutpostInvites(Mutex<HashMap<String, Instant>>);

impl OutpostInvites {
    /// A new invite, and when it stops working (unix seconds).
    pub fn mint(&self) -> (String, i64) {
        let token = crate::util::random_token();
        self.add(token.clone(), Instant::now() + OUTPOST_INVITE_TTL);
        (token, crate::util::now_unix() + i64::try_from(OUTPOST_INVITE_TTL.as_secs()).unwrap_or(0))
    }

    fn add(&self, token: String, until: Instant) {
        let mut g = self.0.lock();
        g.retain(|_, u| *u > Instant::now());
        g.insert(token, until);
    }

    /// Uses up an invite: whether it was one and still worked.
    pub fn take(&self, token: &str) -> bool {
        self.0.lock().remove(token).is_some_and(|until| until > Instant::now())
    }
}

/// Whether a runner carries a label a job asks for (`runs-on`), in any case: one of its own, its
/// name, or what its platform says (`linux`, `macos`, `windows`; `x86_64`, `amd64`, `x64`;
/// `aarch64`, `arm64`; `self-hosted`), so `[linux]` fits every Linux runner, not only those labelled so.
pub(crate) fn carries(labels: &[String], os: &str, arch: &str, name: &str, want: &str) -> bool {
    want == name || labels.iter().any(|l| l.eq_ignore_ascii_case(want)) || platform_labels(os, arch).iter().any(|l| l.eq_ignore_ascii_case(want))
}

/// Each constraint holds for the runner facts; absent facts satisfy no requirement.
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
    let g = hub.agents.lock();
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

/// An outpost (`kiln outpost`) says so with its label and no slots; a runner given the label by
/// hand still takes jobs.
fn is_outpost(labels: &[String], slots: u32) -> bool {
    slots == 0 && labels.iter().any(|l| l == crate::proto::OUTPOST_LABEL)
}

/// A command on its way to a runner ([`Hub::command_start`]): its answer, once its runner gives
/// it.
pub struct Pending {
    agent: String,
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
                Err(_) => Answer::Unanswered { why: format!("no answer from {} in {} s", self.agent, within.as_secs()) },
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

/// Keep the existing stream/away law. Only explicit emergency session controls can
/// positively preempt a never-enqueued ordinary reservation in the SAME Commands owner.
fn preempts_ordinary(msg: &ServerMsg) -> bool {
    match msg {
        ServerMsg::HarnessCmd { json, .. } => matches!(serde_json::from_str::<crate::harness::model::Command>(json),Ok(crate::harness::model::Command::Interrupt | crate::harness::model::Command::ForceStop)),
        _ => false,
    }
}

/// A question asked of a runner, waiting for its answer (by its `req`): what reads the answer as
/// the type its asker wants (`crate::wire::payload`) and hands it over. An answer it can't read is
/// refused to the runner that sent it: its error says why.
type Asked = Box<dyn FnOnce(&'static str, Option<String>) -> Result<(), Box<crate::wire::payload::Unreadable>> + Send>;

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
}

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

pub struct Hub {
    pub cfg: Arc<Config>,
    pub store: Store,
    /// U09/U15: verified before every other Hub effect; raw key never enters canonical storage.
    pub(crate) master_key: crate::secrets::crypto::Master,
    /// The records kiln keeps through the Records seam (docs/seams.md): kiln's own, in its store,
    /// until Amphora's (CP-T1.5) take their place. Deploying by API's are there.
    pub records: Arc<dyn kiln_seams::Records>,
    pub logs: Logs,
    pub net: NetHandle,
    /// U15: advertise transport ownership only after the carrier coordinator installs it.
    pub replica_carrier_active: std::sync::OnceLock<Arc<std::sync::atomic::AtomicBool>>,
    pub pki: Arc<crate::pki::Pki>,
    pub agents: Mutex<HashMap<String, AgentLive>>,
    /// The CI engine's dispatcher (docs/ci.md W05): its runs, legs and their messages.
    pub ci: crate::ci::dispatch::Handle,
    peers: Mutex<HashMap<PeerId, String>>,
    pending_peers: Mutex<HashMap<PeerId, String>>,
    /// Runners' HTTP tunnels, and which QUIC connections they carry (see `crate::tunnel`).
    pub tunnels: Arc<crate::tunnel::Registry>,
    pub events: Broadcast<Arc<str>>,
    pub terms: Mutex<HashMap<String, async_channel::Sender<TermOut>>>,
    /// Every terminal session, local and on machines.
    pub terminals: crate::termsess::Sessions,
    /// Every kiln-native agent session, local and on machines.
    pub harness: crate::harness::registry::Registry,
    /// What the server has of each runner's channel streams (docs/wire.md).
    pub wire: crate::wire::link::Links,
    /// Commands to each runner process on the channel, until it answers them (docs/wire.md,
    /// "Commands").
    pub cmds: Mutex<crate::wire::commands::Commands>,
    /// The process each machine that's away last had: its parked commands wait for it there.
    away_peers: Mutex<HashMap<String, PeerId>>,
    /// Commands queued where nothing could wait to send them ([`Hub::command_soon`]): the commands
    /// loop sends them now rather than on its next tick.
    commands_due: Notify,
    /// The AI coding sessions each connected machine last reported.
    pub ai_sessions: Mutex<HashMap<String, Vec<crate::aisessions::AiSession>>>,
    /// Questions asked of runners (transcripts, views, folders, a doctor's look) waiting for their
    /// answers, by request number: each read as its asker wants it.
    ai_requests: Mutex<HashMap<u64, Asked>>,
    ai_seq: AtomicU64,
    /// File reads (`crate::harness::files`) waiting for their machine's answer: what it says of the
    /// file, and the bytes.
    file_requests: Mutex<HashMap<u64, futures::channel::oneshot::Sender<Result<(crate::harness::files::Answer, Vec<u8>), String>>>>,
    pub history: Mutex<HashMap<String, VecDeque<HostSample>>>,
    pub wake: Notify,
    pub started: Instant,
    pub generation: u64,
    /// Runners connected just before this server started ([`crate::store::Store::runners_before`]).
    pub runners_before: AtomicU64,
    pub admin_token: String,
    pub join_token: String,
    /// One-time enrollment tokens for pod agents: token → (job id, expiry).
    pub ephemeral_tokens: Mutex<HashMap<String, (String, Instant)>>,
    /// U03: authenticated index rebuilt from the durable pod/certificate receipt on Hello.
    pub pod_executors: Mutex<HashMap<String, String>>,
    /// One-time outpost invites (`POST /api/outposts/invite`).
    pub outpost_invites: OutpostInvites,
    pub k8s: std::sync::OnceLock<Option<Arc<crate::k8s::K8s>>>,
    /// Wakes the repository poller ([`crate::gitops::poll_loop`]) to look at what's due: what a
    /// kick names ([`Hub::gitops_wanted`], [`crate::gitops::want`]) and what its interval says.
    pub gitops_kick: Notify,
    /// Repositories (by id) asked to be polled now: a push, a webhook, a new repository.
    pub gitops_wanted: Mutex<HashSet<String>>,
    /// Agents installing what jobs lack, by job ([`crate::toolfix`]).
    pub tool_fixes: Mutex<HashMap<String, crate::toolfix::Fix>>,
    pub shutdown: Shutdown,
    /// Time, as the goal driver and what else moves onto it take it: kiln's runtime in production,
    /// a simulated clock in tests (kiln-seams' `Clock`, docs/seams.md).
    pub clock: Arc<dyn kiln_seams::Clock>,
    /// What the wires couldn't read (`crate::wire::unread`): counted, kept for each runner's and
    /// agent session's page, audited.
    pub refusals: Mutex<crate::wire::unread::Refusals>,
    /// What the hub sent each runner process on its control lane, in order: for tests to read.
    #[cfg(test)]
    pub(crate) sent: Mutex<Vec<(PeerId, ServerMsg)>>,
    /// Agents that have been sent Cancel for a job, with when (for the timeout escalation).
    cancel_sent: Mutex<HashMap<String, Instant>>,
    /// (job, agent) pairs recently declined: not offered again until DECLINE_BACKOFF passes.
    declined: Mutex<HashMap<(String, String), Instant>>,
    /// Multicast DNS discovery: what this server announces and the kiln instances it heard.
    pub discovery: Arc<crate::discovery::Discovery>,
    /// Machines asking to join as agents (`kiln join`), waiting for an admin.
    pub joins: crate::join::Requests,
    /// Each machine's runner processes and what each one holds (see `crate::instances`).
    pub instances: Mutex<crate::instances::Instances>,
    /// Runner processes that went away while their machine stayed connected through another
    /// one, with when and the jobs they held (requeued if they do not come back in time).
    lost_instances: Mutex<HashMap<PeerId, (String, Instant, Vec<String>)>>,
    /// Agent sessions on their way from a machine's older runner processes to its newest
    /// (`crate::handovers`).
    pub(crate) handovers: Mutex<crate::handovers::Handovers>,
    /// Upgrades of runners and of this server, started from System → Updates.
    pub updates: crate::updates::Updates,
    /// Runners' front doors: their settings and what each last reported (`crate::frontdoor`).
    pub front_doors: crate::frontdoor::control::FrontDoors,
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
    /// Reach: the links between runners this server relays, and their identities (`crate::reach`).
    pub reach: Mutex<crate::reach::relay::Relay>,
    /// The copies of runners' trees other runners keep: the rules for them, and what each runner
    /// said it holds and let go (`crate::reach::copies`, docs/reach.md §5.1).
    pub copies: crate::reach::copies::Copies,
    /// The git repositories each machine holds, as it last said (`crate::sync::places`).
    pub places: crate::sync::places::Places,
    /// Clones, syncs and moves of repositories between machines (`crate::sync::tasks`).
    pub tasks: crate::sync::tasks::Tasks,
    /// Work kept in step between the machines that hold a repository (`crate::sync::share`).
    pub shares: crate::sync::share::Shares,
    /// Notifications to people's devices (`crate::push`).
    pub push: crate::push::Service,
    /// Where kiln keeps its bytes (`crate::blobs`, docs/blobs.md): the server's data directory, or
    /// any S3.
    pub blobs: crate::blobs::Opened,
    /// The bus kiln's events ride: its sessions' live feeds (docs/bus.md).
    pub bus: crate::bus::QuiverBus,
    /// The search across a change (docs/ci.md W26-R07): vorpal, its indexes under the server's
    /// cache (`crate::ci::search`).
    pub search: Arc<dyn kiln_seams::Search>,
    /// Memo steps in flight, one execution per key and domain (docs/ci.md W28-R72).
    pub flight: crate::ci::services::memo::Flights,
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
        // U09/U15: this is the existing blocking construction path. Fail before recovery,
        // sessions, controllers or handlers can derive secrets or seal with an untrusted key.
        let master_key = crate::secrets::crypto::initialize(&cfg, &store)?;
        let blobs = crate::blobs::open(&cfg, &store, Arc::clone(&clock))?;
        let generation =
            std::env::var("KILN_GENERATION").ok().and_then(|g| g.parse().ok()).unwrap_or(1);
        let discovery = Arc::new(crate::discovery::Discovery::for_server(&cfg, &pki.ca_der));
        let harness_dir = cfg.data("harness");
        let machine_name = crate::pki::sanitize_name(&cfg.local_agent_name());
        // U19: Tasks recovery and Sync share one controller lease, including standby promotion.
        let sync_controller = crate::sync::server::Controller::open(&cfg.data("sync"))?;
        // U17: import host-affine sidecars before starting sync or task execution.
        let places = crate::sync::places::Places::from_database(cfg.data("sync").join("places.json"), store.database(), &machine_name)?;
        let tasks = crate::sync::tasks::Tasks::from_controller(&cfg.data("sync"), store.database(), &machine_name, sync_controller.clone())?;
        let copies = crate::reach::copies::Copies::from_database(cfg.data("reach").join("copies.json"), store.database(), &machine_name)?;
        // U15/U19: merge every retained local conversation source before Registry exposes a writer.
        let conversation_sources = if !cfg.conversations.off && crate::convo::enabled() {
            let mut sources = crate::server::legacy_conversation_sources(&cfg.data("local-agent"))?;
            anyhow::ensure!(sources.len() < 4096, "U15: too many server legacy conversation sources; source files retained");
            sources.insert(0, harness_dir.join(crate::convo::FILE));
            sources
        } else { Vec::new() };
        // The bus every session's live feed rides (docs/bus.md): Quiver's node, in this process.
        let bus = crate::bus::QuiverBus::open(&cfg.bus, Arc::clone(&clock))?;
        // The search across a change (W26-R07): an index per tree in the server's cache.
        let search: Arc<dyn kiln_seams::Search> = Arc::new(kiln_search::VorpalSearch::open(cfg.data("cache").join("search"), kiln_search::Limits::default()).map_err(anyhow::Error::msg)?);
        let harness = crate::harness::registry::Registry::from_database(harness_dir, &cfg.conversations, Arc::clone(&clock), Arc::new(bus.clone()), store.database(), &machine_name, &conversation_sources)?;
        let contents = crate::sync::contents::Contents::start(&blobs, cfg.data("sync").join("blobs"));
        let sync = crate::sync::server::Sync::start_with_controller(cfg.data("sync"), admin_token.clone(), contents, store.database(), machine_name.clone(), sync_controller.clone())?;
        let records: Arc<dyn kiln_seams::Records> = Arc::new(crate::records::StoreRecords::open(store.clone())?);
        // U17: terminal control names share this host's existing database; PTYs remain local.
        let terminal_namespace = format!("machine.{}", crate::util::sha256_hex(machine_name.as_bytes()).chars().take(56).collect::<String>());
        let terminals = crate::termsess::Sessions::from_records(Arc::new(crate::records::from_database(&store.database(), &terminal_namespace)?), Some(&cfg.server.data_dir.join("terminal-names.json")))?;
        logs.bind_accepted(store.clone(), blobs.blobs.clone(), machine_name.clone());
        Ok(Arc::new(Self {
            cfg,
            store,
            master_key,
            records,
            logs,
            net,
            replica_carrier_active: std::sync::OnceLock::new(),
            pki,
            agents: Mutex::new(HashMap::new()),
            ci: crate::ci::dispatch::Handle::default(),
            peers: Mutex::new(HashMap::new()),
            pending_peers: Mutex::new(HashMap::new()),
            tunnels: Arc::default(),
            events: Broadcast::on(Arc::new(bus.clone()), 4096)?,
            terms: Mutex::new(HashMap::new()),
            terminals,
            harness,
            wire: crate::wire::link::Links::new(),
            // Named for this server process (53 bits, as JSON carries a number exactly), so a runner
            // never mistakes another's commands for these.
            cmds: Mutex::new(crate::wire::commands::Commands::new(rand::random::<u64>() >> 11)),
            away_peers: Mutex::new(HashMap::new()),
            commands_due: Notify::new(),
            ai_sessions: Mutex::new(HashMap::new()),
            ai_requests: Mutex::new(HashMap::new()),
            ai_seq: AtomicU64::new(1),
            file_requests: Mutex::new(HashMap::new()),
            history: Mutex::new(HashMap::new()),
            wake: Notify::new(),
            started: Instant::now(),
            generation,
            runners_before: AtomicU64::new(0),
            admin_token,
            join_token,
            ephemeral_tokens: Mutex::new(HashMap::new()),
            pod_executors: Mutex::new(HashMap::new()),
            outpost_invites: OutpostInvites::default(),
            k8s: std::sync::OnceLock::new(),
            gitops_kick: Notify::new(),
            gitops_wanted: Mutex::new(HashSet::new()),
            tool_fixes: Mutex::new(HashMap::new()),
            shutdown,
            refusals: Mutex::new(crate::wire::unread::Refusals::new(Arc::clone(&clock))),
            #[cfg(test)]
            sent: Mutex::new(Vec::new()),
            clock,
            cancel_sent: Mutex::new(HashMap::new()),
            declined: Mutex::new(HashMap::new()),
            discovery,
            joins: crate::join::Requests::default(),
            instances: Mutex::new(crate::instances::Instances::default()),
            lost_instances: Mutex::new(HashMap::new()),
            handovers: Mutex::new(crate::handovers::Handovers::default()),
            updates: crate::updates::Updates::default(),
            front_doors: crate::frontdoor::control::FrontDoors::default(),
            agent_limits: crate::harness::limit::AgentLimits::default(),
            headrooms: crate::headroom::control::Headrooms::default(),
            peered: crate::peers::Peers::default(),
            tailnet: crate::frontdoor::tailnet::TailnetDns::default(),
            sync,
            reach: Mutex::new(crate::reach::relay::Relay::default()),
            copies,
            places,
            tasks,
            shares: crate::sync::share::Shares::default(),
            push: crate::push::Service::default(),
            blobs,
            bus,
            search,
            flight: Default::default(),
        }))
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
            let peer = self.instances.lock().newest_with(&o.machine, proto::CAP_SYNC);
            let Some(peer) = peer else { continue };
            let msg = ServerMsg::Sync { json: crate::sync::wire::encode(&o.msg), raft: o.raft };
            if let Ok(b) = proto::encode(&msg) {
                self.net.send(peer, proto::LANE_SYNC, Kind::Control, b).await;
            }
        }
        Ok(())
    }

    /// U15: an older runner must stop its retired learner, including its startup retry loop.
    async fn cluster_announced(&self, machine: &str) {
        let (newest, mut peers) = self.instances.lock().holder_of(machine, proto::CAP_CLUSTER);
        peers.extend(newest);
        if let Ok(bytes) = proto::encode(&ServerMsg::Cluster { json: r#"{"t":"leave"}"#.into(), raft: Vec::new() }) {
            for peer in peers { self.net.send(peer, proto::LANE_SYNC, Kind::Control, bytes.clone()).await; }
        }
    }

    /// What a machine's runner said on the sync lane: of its copies (to the groups), of the git
    /// repositories it holds, or of its part of a task.
    async fn sync_heard(&self, machine: &str, msg: crate::sync::wire::Msg, raft: Vec<u8>) {
        match msg {
            crate::sync::wire::Msg::Repos(r) => {
                match self.places.heard(machine, r).await {
                    Ok(true) => self.emit(&ServerEvent::Places),
                    Ok(false) => {}
                    Err(e) => { tracing::warn!(error = %format!("{e:#}"), "repository inventory could not commit"); return; }
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
            let inst = self.instances.lock();
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
        let peers: Vec<PeerId> = self.peers.lock().keys().chain(self.pending_peers.lock().keys()).copied().collect();
        for peer in peers {
            let _ = self.net.command(crate::net::Out::Disconnect { peer }).await;
        }
    }

    /// Tell every UI that the session list changed.
    pub fn emit_terminals(&self) {
        self.emit(&ServerEvent::Terminals);
    }

    /// Sends `e` down every events socket (`GET /api/events`) and to kiln's own listeners.
    pub fn emit(&self, e: &ServerEvent) {
        if self.events.receiver_count() > 0 {
            let _ = self.events.send(Arc::from(e.text()));
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
        a.refused = self.refusals.lock().kept(&crate::wire::unread::Page::Runner(a.name.clone())).cloned();
        self.emit(&ServerEvent::Agent { agent: Box::new(a) });
    }

    /// A message a wire couldn't read (`crate::wire::unread`): counted, kept for its sender's page
    /// and `on`'s, audited, and each page it changed told. Its sender isn't told of it here:
    /// [`Hub::note_unread_told`] is for one it is.
    pub async fn note_unread(&self, u: crate::wire::unread::Unread, on: Option<crate::wire::unread::Page>) {
        said_unread(&u, 1);
        let noted = self.refusals.lock().note(u, on);
        self.noted_unread(noted).await;
    }

    /// As [`Hub::note_unread`], for a refusal its sender is told of (`ServerMsg::Unread`). Some:
    /// tell it now, of how many before it it wasn't told; None: not now.
    pub async fn note_unread_told(&self, u: crate::wire::unread::Unread, on: Option<crate::wire::unread::Page>) -> Option<u64> {
        said_unread(&u, 1);
        let noted = self.refusals.lock().note_told(u, on);
        let tell = noted.tell;
        self.noted_unread(noted).await;
        tell
    }

    /// A runner's word of a frame of the server's it couldn't read (`AgentMsg::Unread`), with the
    /// `folded` before it it didn't tell of: noted as that many.
    pub async fn note_unread_folded(&self, u: crate::wire::unread::Unread, on: Option<crate::wire::unread::Page>, folded: u64) {
        said_unread(&u, folded.saturating_add(1));
        let noted = self.refusals.lock().note_folded(u, on, folded);
        self.noted_unread(noted).await;
    }

    async fn noted_unread(&self, noted: crate::wire::unread::Noted) {
        self.audit_unread(noted.rows).await;
        for page in &noted.changed {
            self.emit_refused(page);
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
        let rows = self.refusals.lock().tick();
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
    fn emit_refused(&self, page: &crate::wire::unread::Page) {
        match page {
            crate::wire::unread::Page::Runner(name) => {
                let a = self.agents.lock().get(name).cloned();
                if let Some(a) = a {
                    self.emit_agent(&a);
                }
            }
            crate::wire::unread::Page::Session(id) => {
                let kept = self.refusals.lock().kept(page).cloned();
                self.harness.set_refused(self, id, kept);
            }
        }
    }

    pub fn emit_queue(&self) {
        let m = metrics::global();
        self.emit(&ServerEvent::Queue(crate::wire::events::Queue::Counts { queued: m.jobs_queued.get(), running: m.jobs_running.get() }));
    }

    pub fn record_sample(&self, agent: &str, s: HostSample) {
        {
            let mut h = self.history.lock();
            let q = h.entry(agent.to_owned()).or_default();
            if q.len() >= HISTORY {
                q.pop_front();
            }
            q.push_back(s.clone());
        }
        if agent != "server" {
            metrics::agent_samples().write().insert(agent.to_owned(), s.clone());
            // A runner too old to report its own figures (`crate::observe::host`): its samples, as
            // metrics. The server's own machine reports itself.
            if self.instances.lock().newest_with(agent, proto::CAP_OBSERVE).is_none() {
                crate::observe::hub::host_sample(agent, &s);
            }
        }
        self.emit(&ServerEvent::Sample { agent: agent.to_owned(), sample: Box::new(s) });
    }

    // ------------------------------------------------------------ agent view

    /// Persistent agents from the DB merged with live sessions.
    pub async fn agent_list(&self) -> Vec<AgentLive> {
        let rows = self.store.agents().await.unwrap_or_default();
        let live = self.agents.lock().clone();
        let mut out: Vec<AgentLive> = rows
            .into_iter()
            .map(|r| match live.get(&r.name) {
                Some(l) => l.clone(),
                None => AgentLive {
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
                },
            })
            .collect();
        for a in &mut out {
            a.instances = self.instance_views(&a.name);
        }
        for (name, l) in &live {
            if !out.iter().any(|a| &a.name == name) {
                out.push(l.clone());
            }
        }
        for a in &mut out {
            a.transport = a.peer.filter(|_| a.connected).map(|p| self.tunnels.transport(p));
            a.dropped_toward = a.peer.map(|p| metrics::net().toward(p)).unwrap_or_default();
            a.refused = self.refusals.lock().kept(&crate::wire::unread::Page::Runner(a.name.clone())).cloned();
        }
        out.sort_by(|a, b| b.connected.cmp(&a.connected).then(a.name.cmp(&b.name)));
        out
    }

    fn update_slot_gauges(&self) {
        let g = self.agents.lock();
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
            let inst = self.instances.lock();
            if inst.newest(machine).is_none() {
                anyhow::bail!("{machine} is not connected");
            }
            inst.newest_with(machine, proto::CAP_DIRS)
        };
        let Some(peer) = peer else { return Ok(None) };
        let req = self.ai_seq.fetch_add(1, Ordering::Relaxed);
        let frame = proto::encode(&ServerMsg::DirList { req, path: path.to_owned() })?;
        let (read, rx) = asked::<crate::dirs::Listing>();
        self.ai_requests.lock().insert(req, read);
        let sent = self.net.send(peer, LANE_TERM, Kind::Term, frame).await;
        let answer = if sent { crate::rt::timeout(crate::dirs::WAIT, rx).await.ok() } else { None };
        self.ai_requests.lock().remove(&req);
        match answered(machine, sent, answer)? {
            Some(listing) => Ok(Some(listing)),
            None => anyhow::bail!("{machine} did not answer"),
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
            let mut inst = self.instances.lock();
            if inst.newest(machine).is_none() {
                anyhow::bail!("{machine} is not connected");
            }
            let to = inst.route(machine, &msg);
            to.filter(|p| inst.get(*p).is_some_and(|i| i.caps & proto::CAP_FILES != 0))
        };
        let Some(peer) = peer else { return Ok(None) };
        let frame = proto::encode(&msg)?;
        let (tx, rx) = futures::channel::oneshot::channel();
        self.file_requests.lock().insert(req, tx);
        let sent = self.net.send(peer, LANE_TERM, Kind::Term, frame).await;
        let answer = if sent { crate::rt::timeout(WAIT, rx).await.ok() } else { None };
        self.file_requests.lock().remove(&req);
        match answer {
            Some(Ok(Ok(a))) => Ok(Some(a)),
            Some(Ok(Err(why))) => anyhow::bail!("{machine}'s answer couldn't be read: {why}"),
            Some(Err(_)) => anyhow::bail!("{machine} did not answer"),
            None if !sent => anyhow::bail!("{machine} is not connected"),
            None => anyhow::bail!("{machine} did not answer in time"),
        }
    }

    /// What the runner process holding what `msg` is about (`crate::instances`' routing) says it
    /// can do: 0 for nothing, or no such process. One that has just connected says it a moment
    /// after (its build, then its capabilities), so this waits up to three seconds for it.
    pub async fn routed_caps(&self, machine: &str, msg: &ServerMsg) -> u32 {
        for _ in 0..30 {
            let caps = {
                let mut inst = self.instances.lock();
                let to = inst.route(machine, msg);
                to.and_then(|p| inst.get(p).map(|i| i.caps)).unwrap_or(0)
            };
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
        if self.instances.lock().newest(machine).is_none() {
            anyhow::bail!("{machine} is not connected");
        }
        if self.routed_caps(machine, &msg).await & cap != cap {
            return Ok(None);
        }
        let Some(peer) = self.instances.lock().route(machine, &msg) else { return Ok(None) };
        let frame = proto::encode(&msg)?;
        let (read, rx) = asked::<T>();
        self.ai_requests.lock().insert(req, read);
        let sent = self.net.send(peer, LANE_TERM, Kind::Term, frame).await;
        let answer = if sent { crate::rt::timeout(wait, rx).await.ok() } else { None };
        self.ai_requests.lock().remove(&req);
        answered(machine, sent, answer).map(Some)
    }

    /// Ask machine `machine` something by its newest runner process announcing `cap` (`msg` given
    /// the question's number), and wait up to `wait` for its [`AgentMsg::Answer`], read as a `T`.
    /// `Ok(None)`: no process of it announces `cap` (its kiln is too old); `Ok(Some(answer))`: what
    /// it answered.
    pub async fn ask_runner<T: serde::de::DeserializeOwned + Serialize + Send + 'static>(&self, machine: &str, cap: u32, wait: Duration, msg: impl FnOnce(u64) -> ServerMsg) -> anyhow::Result<Option<Option<T>>> {
        let peer = {
            let inst = self.instances.lock();
            if inst.newest(machine).is_none() {
                anyhow::bail!("{machine} is not connected");
            }
            inst.newest_with(machine, cap)
        };
        let Some(peer) = peer else { return Ok(None) };
        let req = self.ai_seq.fetch_add(1, Ordering::Relaxed);
        let frame = proto::encode(&msg(req))?;
        let (read, rx) = asked::<T>();
        self.ai_requests.lock().insert(req, read);
        let sent = self.net.send(peer, LANE_TERM, Kind::Term, frame).await;
        let answer = if sent { crate::rt::timeout(wait, rx).await.ok() } else { None };
        self.ai_requests.lock().remove(&req);
        answered(machine, sent, answer).map(Some)
    }

    /// A machine's past sessions.
    pub async fn harness_history(&self, machine: &str, kind: Option<String>, limit: u32) -> anyhow::Result<Option<Vec<crate::aisessions::PastSession>>> {
        self.ai_ask(machine, move |req| ServerMsg::HarnessHistory { req, kind, limit }).await
    }

    async fn ai_ask<T: serde::de::DeserializeOwned + Serialize + Send + 'static>(&self, machine: &str, msg: impl FnOnce(u64) -> ServerMsg) -> anyhow::Result<Option<T>> {
        let req = self.ai_seq.fetch_add(1, Ordering::Relaxed);
        let (read, rx) = asked::<T>();
        self.ai_requests.lock().insert(req, read);
        if !self.send_to_agent(machine, LANE_TERM, &msg(req)).await {
            self.ai_requests.lock().remove(&req);
            anyhow::bail!("machine {machine} is not connected");
        }
        let answer = crate::rt::timeout(Duration::from_secs(10), rx).await;
        self.ai_requests.lock().remove(&req);
        answered(machine, true, answer.ok())
    }

    /// The runner processes of machine `agent` a message goes to. While it runs more than one
    /// (see `crate::instances`), a message about something one of them holds (a terminal, an
    /// agent session, a job) goes to that one, anything new to the newest, and Drain to all of
    /// them.
    fn peers_for(&self, agent: &str, msg: &ServerMsg) -> Vec<PeerId> {
        let mut peers: Vec<PeerId> = {
            let mut inst = self.instances.lock();
            if matches!(msg, ServerMsg::Drain) { inst.of(agent) } else { inst.route(agent, msg).into_iter().collect() }
        };
        if peers.is_empty() {
            peers.extend(self.agents.lock().get(agent).and_then(|a| a.peer));
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
        let peers = self.command_targets(agent, msg);
        self.command_peers(agent, peers, msg).await
    }

    /// [`Hub::command_start`] where nothing can wait (a sync path, under a lock): the command is
    /// queued now, and the commands loop sends it at once. A runner from before the channel gets
    /// it if its lane has room.
    pub fn command_soon(&self, agent: &str, msg: &ServerMsg) -> Pending {
        let peers = self.command_targets(agent, msg);
        let pending = self.queue_command(agent, peers, msg, |p, body| self.net.try_send(p, LANE_CONTROL, Kind::Control, body));
        self.commands_due.notify();
        pending
    }

    /// The runner processes a command to machine `agent` goes to ([`Hub::peers_for`]): for a
    /// parked one to a machine that's away, the process it last had, which comes back under the
    /// same certificate.
    fn command_targets(&self, agent: &str, msg: &ServerMsg) -> Vec<PeerId> {
        let mut peers = self.peers_for(agent, msg);
        if peers.is_empty() && stream_of(msg) == Some(crate::wire::commands::Kind::Parked) {
            let last = self.away_peers.lock().get(agent).copied();
            peers.extend(last.filter(|p| self.cmds.lock().keeps(*p)));
        }
        peers
    }

    /// Sends `msg` as a command to runner processes `peers` of machine `agent`. One that keeps the
    /// channel gets it on its command stream (parked or live, as the message's class says) and
    /// answers it once it applied it; one from before the channel gets it as it always did.
    pub async fn command_peers(&self, agent: &str, peers: Vec<PeerId>, msg: &ServerMsg) -> Pending {
        // A runner from before the channel gets it as it always did, sent after the rest.
        let mut legacy: Vec<(PeerId, Vec<u8>)> = Vec::new();
        let mut pending = self.queue_command(agent, peers, msg, |p, body| {
            legacy.push((p, body));
            true
        });
        if !legacy.is_empty() {
            let mut sent = false;
            for (p, body) in legacy {
                sent |= self.net.send(p, LANE_CONTROL, Kind::Control, body).await;
            }
            if !sent {
                use crate::wire::commands::Answer;
                pending.at_once = if pending.waits.is_empty() { Answer::Unreachable { why: format!("{agent} is not connected") } } else { Answer::Taken };
            }
        }
        self.flush_commands().await;
        pending
    }

    /// Queues command `msg` to runner processes `peers` of machine `agent`: on its command stream
    /// for one that keeps the channel, else handed to `legacy` (which says whether it went).
    fn queue_command(&self, agent: &str, peers: Vec<PeerId>, msg: &ServerMsg, mut legacy: impl FnMut(PeerId, Vec<u8>) -> bool) -> Pending {
        use crate::wire::commands::{Answer, Kind as Stream};
        let what = crate::wire::server_name(msg);
        let at_once = |a: Answer| Pending { agent: agent.to_owned(), what, waits: Vec::new(), at_once: a, away: false };
        if peers.is_empty() {
            return at_once(Answer::Unreachable { why: format!("{agent} is not connected") });
        }
        let stream = stream_of(msg);
        let preempt = preempts_ordinary(msg);
        let body = match proto::encode(msg) {
            Ok(b) => b,
            Err(e) => return at_once(Answer::Unreachable { why: format!("it couldn't be encoded: {e}") }),
        };
        let mut waits = Vec::new();
        // Answers a process gave at once (away, or with too much waiting for it already): what the
        // command is now when no other process takes it, so a caller sees it (and puts a leg back
        // in the queue) rather than wait on an answer that came and went.
        let mut given = Vec::new();
        let mut sent = false;
        let mut away = stream == Some(Stream::Parked);
        for p in peers {
            let on_channel = stream.filter(|_| self.cmds.lock().keeps(p));
            match on_channel {
                Some(kind) => {
                    away &= !self.cmds.lock().is_up(p);
                    let mut rx = {
                        let mut commands = self.cmds.lock();
                        if kind == Stream::Live && preempt { commands.preempt_unexposed(p); }
                        commands.push(p,kind,body.clone(),Instant::now())
                    };
                    match rx.try_recv() {
                        Ok(Some(a)) => given.push(a),
                        _ => waits.push(rx),
                    }
                }
                None => {
                    away = false;
                    sent |= legacy(p, body.clone());
                }
            }
        }
        let first = if sent {
            Answer::Sent
        } else if !waits.is_empty() {
            Answer::Taken
        } else {
            given.first().cloned().unwrap_or_else(|| Answer::Unreachable { why: format!("{agent} is not connected") })
        };
        // Beside processes still to answer, those that answered at once count with them.
        if !waits.is_empty() {
            for a in given {
                let (tx, rx) = futures::channel::oneshot::channel();
                let _ = tx.send(a);
                waits.push(rx);
            }
        }
        Pending { agent: agent.to_owned(), what, away: away && !waits.is_empty(), waits, at_once: first }
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
        let peer = self.agents.lock().get(runner).filter(|a| a.connected).and_then(|a| a.peer).ok_or_else(|| format!("{runner} is not connected"))?;
        if !item {
            let kind = match lane {
                LANE_TERM => Kind::Term,
                proto::LANE_CONVO => Kind::Log,
                _ => Kind::Control,
            };
            return Ok(serde_json::json!({ "sent": self.net.send(peer, lane, kind, bytes).await }));
        }
        let rx = self.cmds.lock().push(peer, Stream::Live, bytes, Instant::now());
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
        let frames = self.cmds.lock().poll_routed(Instant::now());
        for (peer, f, connection) in frames {
            match connection {
                Some(connection) => match proto::encode(&ServerMsg::Wire(f)) {
                    Ok(body) => { let _ = self.net.send_bound(connection, LANE_CONTROL, Kind::Control, body).await; }
                    Err(error) => tracing::warn!(%error, "captured command could not be encoded; ownership remains pending"),
                },
                None => self.send_peer(peer, &ServerMsg::Wire(f)).await,
            }
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
            let overdue = self.cmds.lock().overdue(Instant::now());
            for peer in overdue {
                let waited = self.cmds.lock().not_wire(peer);
                self.send_waited(peer, waited).await;
            }
            self.flush_commands().await;
            // The audit log's rows a minute for what the wires couldn't read.
            self.tick_unread().await;
            // What the server has of each runner's streams, said again every few seconds: one lost
            // below the channel mustn't hold a runner's updates up until its next connection.
            if said.elapsed() >= HAVE_AGAIN {
                said = Instant::now();
                let runners: Vec<(String, PeerId)> = self.agents.lock().values().filter(|a| a.connected && self.wire.keeps(&a.name)).filter_map(|a| Some((a.name.clone(), a.peer?))).collect();
                for (name, peer) in runners {
                    self.send_peer(peer, &ServerMsg::Wire(self.wire.have(&name))).await;
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
        let snapshot = {
            let mut g = self.agents.lock();
            let Some(a) = g.get_mut(agent) else { return false };
            a.draining = draining;
            a.clone()
        };
        self.emit_agent(&snapshot);
        self.wake.notify();
        true
    }

    pub async fn forget_agent(&self, agent: &str, by: &str) -> anyhow::Result<()> {
        // Its certificates are revoked for good, recorded so a restart keeps it, and refused at
        // once (W27-R01); enrolling again under the name brings a new key.
        let at = crate::util::now_unix();
        let keys = crate::identities::revoke(&self.store, crate::identities::Target::Subject(agent.to_owned()), "runner forgotten", by, at).await?;
        if !keys.is_empty() {
            self.net.command(crate::net::Out::RevokeKeys { keys, why: crate::identities::refusal(at, "runner forgotten", by) }).await;
        }
        let mut peers: Vec<PeerId> = self.agents.lock().remove(agent).and_then(|a| a.peer).into_iter().collect();
        peers.extend(self.instances.lock().of(agent));
        peers.sort_unstable();
        peers.dedup();
        for p in peers {
            // What it hadn't answered of the server's commands ends here.
            self.cmds.lock().gone(p, "its runner was forgotten");
            self.net.command(crate::net::Out::Disconnect { peer: p }).await;
        }
        self.net.command(crate::net::Out::Revoke { name: agent.to_owned() }).await;
        self.away_peers.lock().remove(agent);
        self.reach.lock().forget(agent);
        metrics::agent_samples().write().remove(agent);
        self.store.delete_agent(agent.to_owned()).await?;
        self.front_doors.forget(&self.store, agent).await;
        self.agent_limits.forget(&self.store, agent).await;
        self.headrooms.forget(&self.store, agent).await;
        self.places.forget(agent).await?;
        self.tailnet.wake.notify();
        self.update_slot_gauges();
        Ok(())
    }

    // ------------------------------------------------------------ reach

    /// Reach's directory (`crate::reach::directory`): every runner that is not a job pod — what it
    /// is, how busy it is, its harnesses, its reach identity and whether it takes links, and the
    /// facts it reported (what `kiln run --needs` checks).
    pub fn reach_directory(&self) -> Vec<crate::reach::directory::RunnerInfo> {
        let kinds = self.harness.kinds();
        let relay = self.reach.lock();
        let agents = self.agents.lock();
        let mut out: Vec<crate::reach::directory::RunnerInfo> = agents
            .values()
            .filter(|a| !a.ephemeral)
            .map(|a| {
                let ann = relay.announced(&a.name);
                let s = a.sample.as_ref();
                crate::reach::directory::RunnerInfo {
                    name: a.name.clone(),
                    hostname: a.hostname.clone(),
                    os: a.os.clone(),
                    arch: a.arch.clone(),
                    labels: a.labels.clone(),
                    connected: a.connected,
                    reach: a.connected && ann.is_some_and(|x| x.accept),
                    fingerprint: ann.map(|x| x.fingerprint.clone()),
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

    /// Consume transport events forever (supervised).
    pub async fn net_loop(self: Arc<Self>, rx: async_channel::Receiver<NetEvent>) -> anyhow::Result<()> {
        loop {
            // U05: the existing FIFO remains canonical delivery. Retained I/O futures
            // live in Logs across this loop's cancellation; storage is never awaited by
            // the job-log handler. This is no universal bound on unrelated handlers.
            futures::select_biased! {
                // Ready retained I/O gets one turn before another FIFO item. A continuously
                // ready control/other-Bulk stream cannot starve an already durable receipt.
                done = self.logs.accepted_completion().fuse() => match done {
                    Ok(receipt) if receipt.accepted => tracing::debug!(operation=receipt.operation, revision=receipt.prefix.revision, "observed log-prefix operation has its own durable receipt"),
                    Ok(receipt) => tracing::warn!(operation=receipt.operation,job=receipt.job,reason=receipt.refusal.as_deref().unwrap_or("original operation refused"),"observed log append refused; no new acceptance or projection"),
                    Err(error) => tracing::warn!(error=%format!("{error:#}"), "observed log ingest remains incomplete"),
                },
                event = rx.recv().fuse() => {
                    let Ok(event) = event else { break };
                    if let Err(error) = self.on_net(event).await {
                        tracing::warn!(error=%format!("{error:#}"), "a runner's message couldn't be applied");
                    }
                }

            }
        }
        anyhow::bail!("transport event stream ended")
    }

    async fn on_net(self: &Arc<Self>, ev: NetEvent) -> anyhow::Result<()> {
        // W27/U15: queued bootstrap events never become eligible, and a fresh canonical
        // revocation invalidates a buffered Hello before it can mutate application ownership.
        if !self.net.application_admitted(&ev) { return Ok(()); }
        if let NetEvent::Closed { peer, epoch, .. } = &ev
            && !self.instances.lock().accepts_close(*peer,*epoch) { return Ok(()); }
        match ev {
            NetEvent::Connected { peer, epoch, name, binding,.. } => {
                {
                    let mut instances = self.instances.lock();
                    instances.connected(peer, epoch, binding);
                    // Transport proof is negotiated anew. This clears no process/grant,
                    // ownership or physical-stop evidence and says nothing about their death.
                    if let Some(instance) = instances.get_mut(peer) { instance.wide &= !proto::CAP_UNIFIED_LAYOUT; }
                }
                if let Some(agent) = self.agent_of(peer) { self.ci.send(crate::ci::dispatch::Msg::RunnerFacts { agent }); }
                self.tunnels.connected(peer, binding);
                if let Some(n) = name {
                    self.pending_peers.lock().insert(peer, n);
                }
            }
            NetEvent::Closed { peer, reason, .. } | NetEvent::Unreachable { peer, reason, .. } => {
                self.tunnels.closed(peer);
                // Its live commands not yet answered end; its parked ones wait for it.
                self.cmds.lock().down(peer);
                // Its reach links end; the runners at their other ends hear so (and resume later).
                let closes = self.reach.lock().lost(peer, "disconnected from the kiln server");
                for (p, m) in closes {
                    if let Ok(b) = proto::encode(&m) {
                        self.net.send(p, proto::LANE_REACH, Kind::Term, b).await;
                    }
                }
                self.pending_peers.lock().remove(&peer);
                let name = self.peers.lock().remove(&peer);
                self.handovers_lost(peer).await;
                let lost = self.instances.lock().lost(peer);
                if let Some(name) = name {
                    // Sync moves to another runner process of the machine that syncs, if any.
                    let syncing = self.instances.lock().newest_with(&name, proto::CAP_SYNC).is_some();
                    self.sync.machine(&name, syncing);
                    crate::observe::hub::lost(self, &name);
                    if self.instances.lock().of(&name).is_empty() {
                        self.agent_lost(&name, &reason).await;
                    } else {
                        // Gone while another of its machine's processes goes on: retired, and its
                        // identity with it (docs/ci.md W27-R01).
                        if let Some(label) = lost.as_ref().map(|(i, _)| i.label())
                            && let Err(e) = crate::identities::runner_retired(self, &name, &label).await
                        {
                            tracing::warn!(agent = %name, error = %format!("{e:#}"), "a retired runner process's identity wasn't revoked");
                        }
                        self.instance_lost(&name, peer, lost.map(|(_, owned)| owned).unwrap_or_default(), &reason).await;
                    }
                }
            }
            // Bulk frames are job output, but on the conversation feed's and the files' lanes
            // (messages, in bulk).
            NetEvent::Frame { peer, kind: Kind::Log, lane, data, .. } if lane != proto::LANE_CONVO && lane != proto::LANE_FILES => {
                // Same append-only legacy wire shape, borrowed from the original charged
                // frame. Neither this receive nor latest jobs.attempt invents delivery
                // identity/closure. Empty legacy chunks remain legal no-ops.
                match proto::read_whole::<crate::logs::accepted::LegacyChunk<'_>>(&data) {
                    Ok(chunk) if self.owns_job(peer, chunk.job) && !chunk.data.is_empty() => {
                        anyhow::ensure!(data.retained(), "job-log frame has no original receive custody; acceptance refused");
                        let from=(chunk.data.as_ptr() as usize).checked_sub(data.as_ptr() as usize).ok_or_else(||anyhow::anyhow!("borrowed log body is outside its retained frame"))?;
                        let end=from.checked_add(chunk.data.len()).filter(|end|*end<=data.len()).ok_or_else(||anyhow::anyhow!("borrowed log body exceeds its retained frame"))?;
                        let (job,step,origin)=(chunk.job.to_owned(),chunk.idx,data.origin());
                        self.logs.observe_frame(crate::logs::accepted::Observed{job,step,input:crate::logs::accepted::Input::Frame(data,from..end),origin,answer:None});
                    }
                    Ok(_) => {}
                    Err(error) => self.unreadable(peer, Wire::Runner, "a log chunk".to_owned(), error.code(), error.why("LogChunk"), true).await,
                }
            }
            NetEvent::Frame { peer, lane, data, connection, .. } => match proto::read_agent(&data) {
                // Anything on its control lane before its Hello is out of order: Hello comes first
                // there. On its other lanes a frame may come before its Hello is read (it sends them
                // once it's connected): it's let go, as it was.
                Ok(msg) if self.agent_of(peer).is_none() && !matches!(msg, AgentMsg::Hello(_) | AgentMsg::Instance(_)) => {
                    let what = crate::wire::agent_name(&msg).to_owned();
                    if lane == proto::LANE_CONTROL {
                        self.unreadable(peer, Wire::Runner, what, Code::Malformed, "out of order: before its Hello".to_owned(), true).await;
                    } else {
                        tracing::debug!(peer, lane = proto::lane_name(lane), %what, "a runner's frame before its Hello, on a lane that may outrun it: let go");
                    }
                }
                Ok(msg) => self.on_agent_msg_from(peer, msg, connection).await?,
                Err(e) => self.unreadable(peer, Wire::Runner, e.what(), e.code(), e.why("AgentMsg"), true).await,
            },
        }
        Ok(())
    }

    /// A runner's message read whole off one of its lanes: what it carries read too
    /// (`crate::wire::payload_of`), then applied; or, when that can't be read, refused to it
    /// (`Unread`) and noted.
    #[cfg(test)]
    async fn on_runner_msg(self: &Arc<Self>, peer: PeerId, msg: AgentMsg) -> anyhow::Result<()> {
        self.on_agent_msg_from(peer, msg, None).await
    }

    #[cfg(test)]
    async fn on_agent_msg(self: &Arc<Self>, peer: PeerId, msg: AgentMsg) -> anyhow::Result<()> {
        self.on_agent_msg_from(peer, msg, None).await
    }

    // Metadata and private-layout proof are properties of this actual authenticated connection,
    // checked before either the public payload reader or any private extension can affect state.
    async fn accepts_layout(&self, peer: PeerId, msg: &AgentMsg, origin: Option<crate::net::Connection>) -> anyhow::Result<bool> {
        let refusal = {
            let instances = self.instances.lock();
            let current = instances.accepts_connection(peer, origin);
            if matches!(msg, AgentMsg::Hello(_) | AgentMsg::Instance(_) | AgentMsg::Build { .. } | AgentMsg::Caps { .. } | AgentMsg::RunnerProcess { .. }) || proto::agent_extension(msg) {
                anyhow::ensure!(current, "runner identity/capability declaration or private extension came from a replaced or unverified connection");
            }
            let instance = instances.get(peer);
            // Preserve the explicit existing no-TLS in-process fixture policy only in tests;
            // production None origins never establish a negotiated private layout.
            let joined = current && instance.is_some_and(|i| i.on_connection(origin) || (cfg!(test) && origin.is_none()));
            match msg {
                AgentMsg::Caps { .. } if !joined => Some("capability declaration precedes this connection's joined Hello"),
                AgentMsg::Caps { caps } => proto::check_unified_layout(*caps).err(),
                m if proto::agent_extension(m) && !(joined && instance.is_some_and(|i| i.wide & proto::CAP_UNIFIED_LAYOUT != 0)) => Some("private protocol extension before this connection validated its unified layout"),
                _ => None,
            }
        };
        if let Some(reason) = refusal {
            self.send_peer(peer, &ServerMsg::Refused { reason: reason.to_owned() }).await;
            self.net.command(crate::net::Out::Disconnect { peer }).await;
            return Ok(false);
        }
        Ok(true)
    }

    async fn on_agent_msg_from(self: &Arc<Self>, peer: PeerId, msg: AgentMsg, origin: Option<crate::net::Connection>) -> anyhow::Result<()> {
        if !self.accepts_layout(peer, &msg, origin).await? { return Ok(()); }
        match crate::wire::payload_of(msg) {
            Ok(m) => self.on_carried_msg_from(peer, m, origin).await,
            Err(e) => {
                self.unreadable_payload(peer, e, true).await;
                Ok(())
            }
        }
    }

    /// Who sent what came from `peer`, as `crate::wire::unread` names a sender: its runner and which
    /// of its processes, or, before its Hello, what its link says of it.
    fn sender_of(&self, peer: PeerId) -> crate::wire::unread::Sender {
        let instance = self.instances.lock().get(peer).and_then(|i| i.id.clone()).unwrap_or_default();
        match self.agent_of(peer) {
            Some(name) => crate::wire::unread::Sender::Runner { name, instance },
            None => crate::wire::unread::Sender::Peer { cert: self.pending_peers.lock().get(&peer).cloned().unwrap_or_default(), addr: String::new() },
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
    async fn unreadable_payload(&self, peer: PeerId, e: Box<crate::wire::payload::Unreadable>, tell: bool) {
        use crate::wire::payload::Waits;
        // What waits on it is told why: the runner's question answered so, the server's own asker
        // told so.
        match &e.waits {
            Some(Waits::Asking(req)) => self.send_peer(peer, &ServerMsg::ReachAnswer { req: *req, json: e.error().to_string() }).await,
            Some(Waits::Answering(req)) => {
                let waiting = self.file_requests.lock().remove(req);
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
        let takes = tell && self.instances.lock().get(peer).is_some_and(|i| i.wide & proto::CAP_UNREAD != 0);
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
        let (changed, was_low, snapshot) = {
            let mut g = self.agents.lock();
            let Some(a) = g.get_mut(agent) else { return };
            let was_low = a.battery.as_ref().is_some_and(crate::os::battery::Battery::low);
            let changed = a.battery.as_ref() != Some(&b);
            a.battery = Some(b.clone());
            (changed, was_low, a.clone())
        };
        if changed {
            self.emit_agent(&snapshot);
        }
        if b.low() && !was_low {
            let left = b.minutes.map(|m| format!(", about {m} min left")).unwrap_or_default();
            let what = format!("{agent} is on battery at {}%{left}: plug it in, or it sleeps and its runner with it", b.percent);
            tracing::warn!(machine = %agent, percent = b.percent, minutes = ?b.minutes, "a machine on battery is low");
            self.store.audit("kiln".into(), "machine.battery_low".into(), what.clone()).await;
            self.emit(&ServerEvent::MachineBatteryLow { machine: agent.to_owned(), battery: b, text: what });
        }
    }

    fn agent_of(&self, peer: PeerId) -> Option<String> {
        self.peers.lock().get(&peer).cloned()
    }

    fn owns_job(&self, peer: PeerId, job: &str) -> bool {
        let Some(name) = self.agent_of(peer) else { return false };
        // U03: machine identity is insufficient while runner processes overlap.
        let instances = self.instances.lock();
        if let Some(owner) = instances.owner(&crate::instances::Owned::Job(job.to_owned())) {
            return owner == peer;
        }
        // Recovery before inventory restored ownership, or an explicitly kept leg (W17-R04).
        // Only the machine's current process may use this fallback, and only for known work.
        if instances.newest(&name) != Some(peer) { return false; }
        drop(instances);
        self.agents.lock().get(&name).is_some_and(|a| a.running.iter().any(|j| j == job)) || self.ci.kept_by(job, &name)
    }

    /// U03: a prior host may retain an execution while this job id runs elsewhere. Only
    /// identity-fenced retained events get this exception; ordinary job messages never do.
    fn owns_retained_event(&self, peer: PeerId, agent: &str, event: &kiln_ci::wire::JobEventV2) -> bool {
        self.instances.lock().newest(agent) == Some(peer)
            && matches!(event, kiln_ci::wire::JobEventV2::KeptOwned { .. } | kiln_ci::wire::JobEventV2::KeptReleased { .. })
    }

    /// U03: storage failure propagates before a receipt; retries retain their attempt identity.
    async fn runner_completion(&self, peer: PeerId, agent: &str, result: JobResult, attempt: Option<u32>) -> anyhow::Result<()> {
        let job = result.job.clone();
        let mut received = false;
        if self.owns_job(peer, &job) {
            let canonical = self.store.ci_legs(vec![job.clone()]).await?.contains(&job);
            received = self.finish_owned_completion(peer, agent, &result, attempt, canonical).await?;
        } else if let Some(attempt) = attempt {
            // A durable receipt may outlive in-memory ownership across a server crash.
            received = crate::ci::runs::completion(self, &job, attempt, agent).await?.is_some();
            if !received {
                let (job, agent) = (job.clone(), agent.to_owned());
                received = self.store.call(move |c| {
                    // Read-only: never stage work without authenticating the exact process.
                    // A retired receipt is represented by the same durable terminal attempt.
                    Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM jobs j JOIN ci_attempts a ON a.leg_id = j.id WHERE j.id = ?1 AND j.agent = ?3 AND a.runner = ?3 AND a.number = ?2 AND (CASE WHEN j.job_id IS NULL THEN j.attempt + 1 ELSE MAX(j.attempt, 1) END) = ?2 AND j.status IN ('succeeded', 'failed', 'cancelled', 'skipped') AND a.status = 'done' AND COALESCE(a.failure_class, '') != 'runner-lost')", crate::sql::params![job, attempt, agent], |r| r.get::<_, bool>(0))?)
                }).await?;
            }
        }
        if received {
            if let Some(attempt) = attempt {
                self.send_peer(peer, &ServerMsg::ReceivedV2 { job, attempt }).await;
            } else if self.instances.lock().get(peer).is_some_and(|i| i.caps & proto::CAP_RECEIVED != 0) {
                self.send_peer(peer, &ServerMsg::Received { job }).await;
            }
        }
        Ok(())
    }

    /// U01/U03: continue an authenticated completion after classification. A negative
    /// classification is only advisory; the legacy writer must fence concurrent adoption.
    async fn finish_owned_completion(&self, peer: PeerId, agent: &str, result: &JobResult, attempt: Option<u32>, mut canonical: bool) -> anyhow::Result<bool> {
        let mut received = false;
        if !canonical {
            match self.job_done(agent, result.clone()).await {
                Ok(()) => received = true,
                // Adoption can commit after the negative lookup. The old transaction made
                // no changes and released no ownership; use the original attempt fence.
                Err(error) if error.is::<crate::store::CanonicalJobMutation>() => canonical = true,
                Err(error) => return Err(error),
            }
        }
        if canonical {
            // U03: disabling ingress cannot let an attempt-capable process bypass fencing.
            if attempt.is_none() && self.instances.lock().get(peer).is_some_and(|i| i.wide & proto::CAP_JOB_ATTEMPT != 0) { return Ok(false); }
            if let Some((job, attempt)) = crate::ci::runs::stage_completion(self, agent, result, attempt).await? {
                self.ci.send(crate::ci::dispatch::Msg::Completion { agent: agent.to_owned(), job, attempt });
                received = true;
            }
        }
        Ok(received)
    }

    /// U03: legacy transport is an input adapter, never a second canonical metadata writer.
    async fn runner_step(&self, peer: PeerId, agent: &str, job: &str, idx: u32, status: Status, exit_code: Option<i32>) -> anyhow::Result<bool> {
        if !self.owns_job(peer, job) { return Ok(false); }
        match self.store.step_update(job.to_owned(), idx, status, exit_code).await {
            Ok(()) => Ok(true),
            Err(error) if error.is::<crate::store::CanonicalJobMutation>() => {
                let accepted = crate::ci::runs::imported_step(self, agent, job, idx, status, exit_code).await?;
                if !accepted {
                    tracing::warn!(job, peer, "legacy step progress ignored: no live original imported assignment or active step; canonical assignments require their attempt-fenced event protocol");
                }
                Ok(accepted)
            }
            Err(error) => Err(error),
        }
    }

    async fn agent_lost(&self, name: &str, reason: &str) {
        // Agents leaving as the server stops are expected.
        if self.shutdown.is_cancelled() {
            tracing::info!(agent = name, reason, "agent disconnected");
        } else {
            tracing::warn!(agent = name, reason, "agent disconnected");
        }
        let snapshot = {
            let mut g = self.agents.lock();
            let Some(a) = g.get_mut(name) else { return };
            a.connected = false;
            if let Some(p) = a.peer {
                self.away_peers.lock().insert(name.to_owned(), p);
            }
            a.peer = None;
            a.lost_at = Some(Instant::now());
            a.clone()
        };
        if let Err(e) = self.store.agent_disconnected(name.to_owned()).await {
            tracing::warn!(error = %e, "agent_disconnected");
        }
        // That machine's terminals keep running there; park them until it reconnects. One that
        // keeps the channel sends their output again once it's back, so they stay open meanwhile
        // (docs/wire.md).
        let sessions: Vec<String> = if self.wire.keeps(name) {
            Vec::new()
        } else {
            self.terms.lock().keys().filter(|k| k.starts_with(&format!("{name}/"))).cloned().collect()
        };
        for s in sessions {
            if let Some(tx) = self.terms.lock().remove(&s) {
                let _ = tx.try_send(TermOut::Gone);
            }
        }
        metrics::agent_samples().write().remove(name);
        // Its agent sessions keep running there too; they are announced again on reconnect.
        self.harness.machine_lost(self, name);
        if self.ai_sessions.lock().remove(name).is_some() {
            self.emit(&ServerEvent::AiSessions { machine: name.to_owned() });
        }
        self.update_slot_gauges();
        self.emit_agent(&snapshot);
        // Connections drop transiently (network blips, server restarts): every agent, pods
        // included, gets AGENT_GRACE to reconnect and reclaim its jobs. A pod that is really gone
        // (deleted, evicted, OOM-killed) is requeued right away.
        if snapshot.ephemeral && !snapshot.running.is_empty() {
            let gone = match self.k8s.get().and_then(Option::as_ref) {
                Some(k) => !k.agent_pod_alive(name).await,
                None => false,
            };
            if gone {
                self.requeue_agent_jobs(name, "agent pod is gone").await;
                self.agents.lock().remove(name);
            }
        }
    }

    async fn requeue_agent_jobs(&self, name: &str, reason: &str) {
        let jobs = self.agents.lock().get(name).map(|a| a.running.clone()).unwrap_or_default();
        let canonical = match self.store.ci_legs(jobs.clone()).await {
            Ok(owned) => owned,
            Err(error) => {
                tracing::error!(%error, agent = name, "ownership lookup failed; runner loss is fenced");
                return;
            }
        };
        if self.ci.available() || !canonical.is_empty() {
            self.ci.send(crate::ci::dispatch::Msg::RunnerLost { agent: name.to_owned() });
            return;
        }
        for j in jobs {
            match self.store.requeue_job(j.clone(), format!("{reason} ({name})")).await {
                Ok(true) => tracing::info!(job = %j, agent = name, "job requeued"),
                Ok(false) => tracing::warn!(job = %j, agent = name, "job failed: out of retries"),
                Err(e) => {
                    if e.is::<crate::store::CanonicalJobMutation>() {
                        self.ci.send(crate::ci::dispatch::Msg::RunnerLost { agent: name.to_owned() });
                    } else { tracing::error!(error = %e, job = %j, "requeue failed"); }
                    continue;
                }
            }
            self.release_slot(name, &j);
            // Compatibility/unavailable-CI fallback only: canonical plans above dispatch
            // immediately. Physical grant release does not wait for retained log custody.
            if let Err(error)=self.logs.observe_server(j.clone(),0,format!("\r\n\x1b[33m[kiln] {reason}; job requeued\x1b[0m\r\n").into_bytes()) {
                tracing::warn!(job=j,error=%format!("{error:#}"),"server log append not admitted; no durable acceptance or compatibility projection");
            }
            self.emit_job(&j).await;
        }
        self.update_slot_gauges();
        self.wake.notify();
    }

    /// A frame of a runner's channel (docs/wire.md). An item is applied as the message it carries,
    /// then answered: taken, or refused saying why. Never answered before it's applied.
    async fn on_wire(self: &Arc<Self>, peer: PeerId, agent: &str, frame: crate::wire::channel::Frame, origin: Option<crate::net::Connection>) {
        use crate::wire::channel::{Arrival, Frame, Refusal};
        match frame {
            Frame::Item { stream, seq, low, body } => {
                let answer = match self.wire.on_item(agent, &stream, seq, low, body) {
                    Arrival::Take(body) => {
                        let refusal = match proto::read_agent(&body) {
                            Ok(AgentMsg::Wire(_)) => {
                                let why = "a channel frame inside a channel item".to_owned();
                                self.unreadable(peer, Wire::RunnerItem, "Wire".to_owned(), Code::Malformed, why.clone(), false).await;
                                Some((Refusal::Malformed, why))
                            }
                            Ok(inner) => {
                                let evidence = matches!(&inner, AgentMsg::JobEvent { .. } | AgentMsg::JobFinished(_) | AgentMsg::JobFinishedV2 { .. });
                                match self.accepts_layout(peer, &inner, origin).await {
                                    Ok(true) => {},
                                    Ok(false) => {
                                        self.wire.retry(agent, &stream, seq);
                                        return;
                                    }
                                    Err(error) => {
                                        self.wire.retry(agent, &stream, seq);
                                        tracing::warn!(%agent, %stream, seq, error = %format!("{error:#}"), "runner connection evidence refused");
                                        return;
                                    }
                                }
                                match crate::wire::payload_of(inner) {
                                Ok(m) => match Box::pin(self.on_carried_msg_from(peer, m, origin)).await {
                                    Ok(()) => None,
                                    Err(error) if evidence || error.is::<EvidenceStorageError>() => {
                                        self.wire.retry(agent, &stream, seq);
                                        tracing::warn!(%agent, %stream, seq, error = %format!("{error:#}"), "durable runner evidence remains unacknowledged for retry");
                                        return;
                                    }
                                    Err(error) => Some((Refusal::Refused, format!("{error:#}"))),
                                },
                                // What it carries can't be read: its NACK says so, as for an item
                                // that can't be read, and it's noted.
                                Err(e) => {
                                    let refusal = (e.code.refusal(), e.why.clone());
                                    self.unreadable_payload(peer, e, false).await;
                                    Some(refusal)
                                }
                                }
                            },
                            // Its NACK says it: unsupported for a variant this build doesn't know.
                            Err(e) => {
                                let why = e.why("AgentMsg");
                                self.unreadable(peer, Wire::RunnerItem, e.what(), e.code(), why.clone(), false).await;
                                Some((e.code().refusal(), why))
                            }
                        };
                        Some(self.wire.answer(agent, &stream, seq, refusal))
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
                // Only the active connection may mutate its command stream's reconnect state.
                // Immutable completion/retained evidence above remains independently admissible.
                let ready_changed = {
                    // SAME actual connection and Commands transition, in the capture lock
                    // order. No stale Have can reopen an ordinary producer's readiness.
                    let instances = self.instances.lock();
                    if !instances.accepts_connection(peer,origin) { return; }
                    let mut commands = self.cmds.lock();
                    let before = commands.is_up(peer);
                    commands.on_frame(peer, frame);
                    before != commands.is_up(peer) && instances.effective_wide(peer) & proto::CAP_ORDINARY_EXECUTION_DEBT != 0
                };
                if ready_changed { self.ci.send(crate::ci::dispatch::Msg::RunnerFacts { agent: agent.to_owned() }); }
                self.flush_commands().await;
            }
        }
    }

    async fn on_carried_msg_from(self: &Arc<Self>, peer: PeerId, msg: Carried<AgentMsg, FromRunner>, origin: Option<crate::net::Connection>) -> anyhow::Result<()> {
        if let Carried::Nothing(AgentMsg::Hello(h)) = msg {
            return self.on_hello(peer, h).await;
        }
        // Says which runner process of its machine this is; precedes Hello. Again later: a new
        // build that has taken over (blue/green) says it's the newest now.
        if let Carried::Nothing(AgentMsg::Instance(ih)) = msg {
            let again = self.instances.lock().reannounce(peer, &ih);
            match again {
                Some(drain) => self.instance_promoted(peer, drain).await,
                None => self.instances.lock().announce(peer, ih),
            }
            return Ok(());
        }
        let Some(agent) = self.agent_of(peer) else {
            anyhow::bail!("message from peer {peer} before hello");
        };
        let now = crate::util::now_unix();
        if let Some(a) = self.agents.lock().get_mut(&agent) {
            a.last_seen = now;
        }
        // A runner process that keeps the channel says so (its Caps) right after its Build, before
        // anything else on its control lane. One that says its facts or its upgrade preference
        // first is from before the channel: what waited for it goes its way now, however long the
        // server took to hear it (docs/wire.md, "Commands").
        if matches!(msg, Carried::Read(FromRunner::Facts(_)) | Carried::Nothing(AgentMsg::UpdatePrefs { .. })) && self.cmds.lock().is_coming(peer) {
            let waited = self.cmds.lock().not_wire(peer);
            self.send_waited(peer, waited).await;
        }
        let msg = match msg {
            Carried::Nothing(m) => m,
            Carried::Read(read) => return self.on_read(peer, &agent, read, origin).await,
        };
        match msg {
            AgentMsg::Hello(_) | AgentMsg::Instance(_) => {}
            // What carries JSON came read (`wire::payload_of`), to `on_read`.
            AgentMsg::AiSessions { .. }
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
                if let Some(a) = self.agents.lock().get_mut(&agent) {
                    a.sample = Some(s.clone());
                }
                self.record_sample(&agent, s);
                // The CI engine weighs its headroom by it: once a sample, as it comes.
                self.ci.send(crate::ci::dispatch::Msg::RunnerSample { agent: agent.clone() });
            }
            AgentMsg::StepStarted { job, idx } => {
                if self.runner_step(peer, &agent, &job, idx, Status::Running, None).await? {
                    self.logs.step(&job, idx, "running");
                    self.emit_job(&job).await;
                }
            }
            AgentMsg::StepFinished { job, idx, status, exit_code } => {
                if self.runner_step(peer, &agent, &job, idx, status, exit_code).await? {
                    self.logs.step(&job, idx, status.as_str());
                    metrics::global().steps_finished.inc(&[status.as_str()]);
                    self.emit_job(&job).await;
                }
            }
            AgentMsg::JobSample { .. } => {}
            AgentMsg::JobFinished(r) => {
                self.runner_completion(peer, &agent, r, None).await?;
            }
            AgentMsg::JobFinishedV2 { result, attempt } => {
                self.runner_completion(peer, &agent, result, Some(attempt)).await?;
            }
            AgentMsg::Reject { job, .. } if self.ci.is_leg(&job) => {
                if self.owns_job(peer, &job) {
                    self.ci.send(crate::ci::dispatch::Msg::Rejected { agent: agent.clone(), leg: job });
                }
            }
            AgentMsg::Reject { job, reason } => {
                if self.owns_job(peer, &job) {
                    match self.store.unclaim_job(job.clone(), format!("declined by {agent}: {reason}")).await {
                        Ok(()) => {},
                        Err(error) if error.is::<crate::store::CanonicalJobMutation>() => {
                            self.ci.send(crate::ci::dispatch::Msg::Rejected { agent: agent.clone(), leg: job });
                            return Ok(());
                        }
                        Err(error) => return Err(error),
                    }
                    self.release_slot(&agent, &job);
                    self.declined.lock().insert((job.clone(), agent.clone()), Instant::now());
                    self.emit_job(&job).await;
                    self.wake.notify();
                }
            }
            AgentMsg::TermOutput { session, data } => {
                let key = format!("{agent}/{session}");
                let tx = self.terms.lock().get(&key).cloned();
                if let Some(tx) = tx {
                    let _ = tx.send(TermOut::Data(data)).await;
                }
            }
            AgentMsg::TermList { sessions } => {
                {
                    let mut inst = self.instances.lock();
                    for info in &sessions {
                        inst.own(crate::instances::Owned::Terminal(info.session.clone()), peer);
                    }
                }
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
            AgentMsg::Build { build, caps } => {
                self.instances.lock().set_build(peer, build.clone(), caps);
                // It may send its legs' events and results again after a reconnect: they're taken
                // once (a receipt without a job says so).
                if caps & proto::CAP_RECEIVED != 0 {
                    self.send_peer(peer, &ServerMsg::Received { job: String::new() }).await;
                }
                self.updates.on_build(self, &agent, &build);
                self.updates.fleet.wake();
                self.emit(&ServerEvent::Updates);
                crate::frontdoor::control::on_build(self, &agent, caps).await;
                if caps & proto::CAP_SYNC != 0 {
                    self.sync_announced(&agent).await;
                }
                if caps & proto::CAP_OBSERVE != 0 {
                    crate::observe::hub::announced(self, &agent).await;
                }
                if caps & proto::CAP_CLUSTER != 0 {
                    self.cluster_announced(&agent).await;
                }
                // What it can do decides which legs it may take.
                self.ci.send(crate::ci::dispatch::Msg::RunnerUp { agent: agent.clone(), running: Vec::new() });
                // Previews' deployments it's left to take down (docs/ci.md W15-R07).
                if caps & proto::CAP_UNDEPLOY != 0 {
                    let (hub, agent) = (Arc::clone(self), agent.clone());
                    crate::rt::spawn(async move { crate::ci::previews::runner_up(&hub, &agent).await });
                }
            }
            AgentMsg::UpdatePrefs { accept } => {
                self.updates.fleet.set_accept(&agent, accept);
                self.updates.fleet.wake();
                self.emit(&ServerEvent::Updates);
            }
            // What it can do past Build's 32 bits. One that keeps the channel too is told what the
            // server has of its streams, and sends from there (docs/wire.md).
            AgentMsg::Amphora { .. } => {
                self.send_peer(peer, &ServerMsg::Refused { reason: "Amphora transport requires its dedicated authenticated carrier lane".into() }).await;
            }
            AgentMsg::RunnerProcess { nonce } => {
                self.instances.lock().set_process(peer,nonce,origin)?;
                self.ci.send(crate::ci::dispatch::Msg::RunnerFacts { agent: agent.clone() });
            }
            AgentMsg::Caps { caps } => {
                let waited = {
                    // U01/U03: the first strict declaration ends the old Live generation
                    // BEFORE capability/capture publication. Repeated strict declarations
                    // retain the same prepared/published owner instead of rotating it again.
                    let mut instances = self.instances.lock();
                    anyhow::ensure!(instances.accepts_connection(peer,origin), "runner capability connection changed before acceptance");
                    let strict = caps & proto::CAP_ORDINARY_EXECUTION_DEBT != 0;
                    let repeated = strict && instances.effective_wide(peer) & (proto::CAP_ORDINARY_EXECUTION_DEBT | proto::CAP_WIRE) == (proto::CAP_ORDINARY_EXECUTION_DEBT | proto::CAP_WIRE);
                    if strict {
                        let paired = proto::CAP_UNIFIED_LAYOUT | proto::CAP_WIRE | proto::CAP_RESOURCE_OWNERSHIP | proto::CAP_JOB_ATTEMPT;
                        anyhow::ensure!(caps & paired == paired, "ordinary capability requires paired ownership support");
                        let nonce = instances.process_on(peer,origin).context("ordinary capability has no registered current process")?;
                        let captured = instances.capture_original_runner(&agent,peer,nonce,proto::CAP_WIRE)?;
                        anyhow::ensure!(captured.ordinary_epoch().is_some(), "ordinary capability precedes its registered publication domain");
                    }
                    let mut commands = self.cmds.lock();
                    let waited = if caps & proto::CAP_WIRE != 0 {
                        if !repeated { commands.connected(peer); }
                        Vec::new()
                    } else { commands.not_wire(peer) };
                    instances.set_wide(peer,caps);
                    waited
                };
                self.ci.send(crate::ci::dispatch::Msg::RunnerFacts { agent: agent.clone() });
                crate::headroom::control::on_caps(self, &agent, caps).await;
                // One that takes hand-overs: its machine's older processes hand their sessions on.
                if caps & proto::CAP_HANDOVER != 0 {
                    self.check_handover().await;
                }
                self.wire.set_keeps(&agent, caps & proto::CAP_WIRE != 0);
                if caps & proto::CAP_WIRE != 0 {
                    self.send_peer(peer, &ServerMsg::Wire(self.wire.have(&agent))).await;
                } else { self.send_waited(peer, waited).await; }
                // One that takes its agent limit from here is sent what's set for its machine.
                crate::harness::limit::on_caps(self, &agent, peer, caps).await;
            }
            AgentMsg::Wire(frame) => self.on_wire(peer, &agent, frame, origin).await,
            AgentMsg::Reach { to, link, open, data, close } => {
                // Job pods neither send nor receive reach links: one's frame is refused, and noted.
                if self.agents.lock().get(&agent).is_none_or(|a| a.ephemeral) {
                    let why = format!("a job pod's, for link {link}: job pods neither send nor receive reach links");
                    self.unreadable(peer, Wire::Relay, "Reach".to_owned(), Code::Malformed, why, true).await;
                    return Ok(());
                }
                let forwarded = {
                    let (agents, instances) = (&self.agents, &self.instances);
                    self.reach.lock().forward(&agent, peer, crate::reach::relay::Frame { to, link, open, data, close }, |n| {
                        if agents.lock().get(n).is_none_or(|a| a.ephemeral || !a.connected) {
                            return None;
                        }
                        instances.lock().newest_with(n, proto::CAP_REACH)
                    })
                };
                // One from a runner that isn't an end of the link it names goes nowhere: refused to
                // it, and noted naming the link's ends.
                if let crate::reach::relay::Forwarded::NotItsLink { link, ends: (a, b) } = &forwarded {
                    let why = format!("not its link: link {link} is {a}'s and {b}'s");
                    self.unreadable(peer, Wire::Relay, "Reach".to_owned(), Code::Malformed, why, true).await;
                }
                let typed = self.instances.lock().get(peer).is_some_and(|i| i.wide & proto::CAP_UNREAD != 0);
                for (p, m) in forwarded.sends(peer, typed) {
                    self.send_reach(p, &m).await;
                }
            }
            AgentMsg::NetDrops { dropped } => {
                let series = dropped.iter().map(|d| (vec![agent.clone(), d.lane.clone(), d.reason.clone()], i64::try_from(d.frames).unwrap_or(i64::MAX)));
                metrics::global().runner_net_dropped.replace_owned(&std::collections::BTreeSet::from([agent.clone()]), series);
                if let Some(a) = self.agents.lock().get_mut(&agent) {
                    a.dropped = dropped;
                }
            }
            // Hand-overs between a machine's runner processes (`crate::handovers`).
            AgentMsg::HarnessHandOver { session, offset, data, done } => self.handover_piece(&agent, peer, session, offset, data, done).await,
            AgentMsg::HarnessAdopted { session, error } => self.handover_adopted(&agent, peer, session, error).await?,
            // A frame of the server's this runner couldn't read: noted against it, and on the page
            // of the session it was about when it holds that session now. (A send it couldn't read
            // its session's inbox there answers, as it answers any.) It's the runner's word, so
            // nothing goes back, and it's no part of what the runner is told.
            AgentMsg::Unread { wire, what, code, why, folded, session } => {
                let held = session.filter(|s| self.harness.located_on(s, &agent) && self.instances.lock().owner(&crate::instances::Owned::Agent(s.clone())).is_none_or(|q| q == peer));
                let on = held.map(crate::wire::unread::Page::Session);
                let u = crate::wire::unread::Unread { wire, from: self.sender_of(peer), what, code, why };
                self.note_unread_folded(u, on, folded).await;
            }
            // A message on a reach link this runner couldn't read: noted against the runner that
            // sent it, the link's other end as the relay knows it, on this one's page too. Word of
            // one about a link the relay doesn't know with those ends is refused, and noted.
            AgentMsg::LinkUnread { from, link, what, code, why } => {
                let ends = self.reach.lock().ends(&link);
                if ends.as_ref().is_some_and(|(a, b)| (*a == agent && *b == from) || (*a == from && *b == agent)) {
                    let sender = crate::wire::unread::Sender::Runner { name: from, instance: String::new() };
                    let u = crate::wire::unread::Unread { wire: Wire::ReachLink, from: sender, what, code, why: format!("{why} (as {agent} read it, on link {link})") };
                    let _ = self.note_unread(u, Some(crate::wire::unread::Page::Runner(agent.clone()))).await;
                } else {
                    let why = format!("word of what link {link} couldn't read, of {from}'s, where the relay knows no such link between them");
                    self.unreadable(peer, Wire::Relay, "LinkUnread".to_owned(), Code::Malformed, why, true).await;
                }
            }
            AgentMsg::UpgradeLog { job, line } => self.updates.on_log(self, &agent, peer, &job, line).await?,
            AgentMsg::UpgradeDone { job, ok, error } => self.updates.on_done(self, &agent, peer, &job, ok, error).await?,
            AgentMsg::TermReplay { session, data } => {
                let key = format!("{agent}/{session}");
                let tx = self.terms.lock().get(&key).cloned();
                if let Some(tx) = tx {
                    let _ = tx.send(TermOut::Replay(data)).await;
                }
            }
            AgentMsg::TermExit { session, code, error } => {
                let key = format!("{agent}/{session}");
                let tx = self.terms.lock().remove(&key);
                if let Some(tx) = tx {
                    let _ = tx.send(TermOut::Exit { code, error }).await;
                }
                self.instances.lock().disown(&crate::instances::Owned::Terminal(session));
                self.check_retire().await;
            }
        }
        Ok(())
    }

    /// What a runner's message carried, read where its frame was (`crate::wire::payload_of`): each
    /// handler takes its value.
    async fn on_read(self: &Arc<Self>, peer: PeerId, agent: &str, read: FromRunner, origin: Option<crate::net::Connection>) -> anyhow::Result<()> {
        match read {
            // One report per machine: its newest runner process's.
            FromRunner::AiSessions(_) if self.instances.lock().newest(agent).is_some_and(|n| n != peer) => {}
            FromRunner::AiSessions(sessions) => {
                let old = self.ai_sessions.lock().insert(agent.to_owned(), sessions.clone());
                if old.as_ref() != Some(&sessions) {
                    self.emit(&ServerEvent::AiSessions { machine: agent.to_owned() });
                }
            }
            // An answer, read as its question asks for: one that can't be is refused to the runner,
            // and its asker told why.
            FromRunner::Answer(a) => {
                let asked = self.ai_requests.lock().remove(&a.req);
                if let Some(read) = asked
                    && let Err(e) = read(a.of, a.json)
                {
                    self.unreadable_payload(peer, e, true).await;
                }
            }
            FromRunner::HarnessFileData { req, answer, data } => {
                if let Some(tx) = self.file_requests.lock().remove(&req) {
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
            FromRunner::Sync { msg, raft } => self.sync_heard(agent, *msg, raft).await,

            FromRunner::FrontDoorStatus(status) => crate::frontdoor::control::on_status(self, peer, agent, *status).await,
            FromRunner::HeadroomStatus(status) => crate::headroom::control::on_status(self, peer, agent, status),
            // CI (docs/ci.md §2.5): the dispatcher's; facts are W13's.
            FromRunner::JobEvent { job, event, json } => {
                if matches!(&event.event, kiln_ci::wire::JobEventV2::OrdinaryDomain { .. } | kiln_ci::wire::JobEventV2::OrdinaryExecutionPublication { .. }) {
                    let actual = origin.context("ordinary evidence has no authenticated origin")?;
                    let nonce = self.instances.lock().process_on(peer,Some(actual)).map(str::to_owned).context("ordinary evidence has no current authenticated process")?;
                    let required = proto::CAP_UNIFIED_LAYOUT | proto::CAP_WIRE | proto::CAP_RESOURCE_OWNERSHIP | proto::CAP_JOB_ATTEMPT;
                    anyhow::ensure!(self.instances.lock().effective_wide(peer) & required == required, "ordinary evidence requires paired ownership support");
                    if matches!(&event.event,kiln_ci::wire::JobEventV2::OrdinaryExecutionPublication { .. }) {
                        anyhow::ensure!(self.instances.lock().effective_wide(peer) & proto::CAP_ORDINARY_EXECUTION_DEBT != 0, "ordinary physical publication requires the same initialized, registered producer role");
                    }
                    let domain = match &event.event {
                        kiln_ci::wire::JobEventV2::OrdinaryDomain { epoch, process_nonce } => {
                            anyhow::ensure!(process_nonce == &nonce, "publication domain changed authenticated process");
                            Some(epoch.clone())
                        },
                        _ => None,
                    };
                    let acknowledgement = self.ci.stage_ordinary_event(agent,&nonce,event.event).await.map_err(evidence_storage)?;
                    let json = if let Some(epoch) = domain {
                        self.instances.lock().set_ordinary_domain(peer,actual,epoch.clone())?;
                        serde_json::json!({"kind":"ordinary_domain","version":1,"epoch":epoch}).to_string()
                    } else {
                        serde_json::to_string(&acknowledgement.context("ordinary proof has no committed frontier")?)?
                    };
                    // ACK only the same authenticated reporting route, never newest substitution.
                    self.net.send_bound(actual,proto::LANE_CONTROL,crate::net::Kind::Control,proto::encode(&ServerMsg::RetainedReceived { json })?).await;
                } else if self.owns_job(peer, &job) || self.owns_retained_event(peer, agent, &event.event) {
                    if self.ci.stage_retained_event(agent, &job, event.event.clone()).await.map_err(evidence_storage)? {
                        if self.instances.lock().get(peer).is_some_and(|i| i.wide & proto::CAP_RETAINED_RECEIVED != 0)
                            && let Some(receipt) = crate::agent::retained::Receipt::of(&job, &json)?
                        {
                            self.send_peer(peer, &ServerMsg::RetainedReceived { json: serde_json::to_string(&receipt)? }).await;
                        }
                    } else {
                        self.ci.send(crate::ci::dispatch::Msg::Event { agent: agent.to_owned(), leg: job, event });
                    }
                }
            }
            // A pool's executor it was asked to start (docs/ci.md W13-R09).
            FromRunner::Executor { said, json } => {
                let shaped_ready = said.state == "ready" && matches!(&said.shape, Some(kiln_ci::sched::pools::ExecutorShape::Firecracker(_)));
                let declared_nonce = self.instances.lock().process_on(peer, origin).map(str::to_owned);
                let stopped_hint = said.state == "stopped" && said.process_nonce.is_some();
                if shaped_ready || stopped_hint {
                    anyhow::ensure!(said.process_nonce.is_some() && said.process_nonce == declared_nonce, "executor readiness/stop hint has no matching process declaration on its actual authenticated connection");
                }
                self.ci.stage_executor_event(agent, *said, origin.map(|connection| crate::ci::dispatch::ExecutorOrigin { peer: connection.peer(), connection: connection.connection_id(), process_nonce: declared_nonce })).await.map_err(evidence_storage)?;
                if self.instances.lock().get(peer).is_some_and(|i| i.wide & proto::CAP_RETAINED_RECEIVED != 0)
                    && let Some(receipt) = crate::agent::retained::Receipt::executor(&json)?
                {
                    self.send_peer(peer, &ServerMsg::RetainedReceived { json: serde_json::to_string(&receipt)? }).await;
                }
            },
            // A preview's deployment it was asked to take down (docs/ci.md W15-R07).
            FromRunner::Undeployed(u) => crate::ci::previews::undeployed(self, agent, u).await,
            // What it holds warm, for placement (docs/ci.md W24-R18); a job's pod holds nothing.
            FromRunner::WorkState(state) => {
                if !self.agents.lock().get(agent).is_none_or(|a| a.ephemeral) {
                    self.ci.send(crate::ci::dispatch::Msg::WorkState { agent: agent.to_owned(), state });
                }
            }
            // The kiln it runs (`crate::fleet`); the rest is W13's.
            FromRunner::Facts(f) => {
                if let Some(k) = f.kiln.clone() {
                    // How far behind the server's kiln it is: to be upgraded, it takes no new work.
                    self.skew_check(agent, peer, Some(k.commit.clone()));
                    crate::fleet::remember(self, &k.build, &k.target, &k.commit);
                    self.instances.lock().set_kiln(peer, k);
                    self.updates.late_by_commit(self, agent);
                    self.updates.fleet.wake();
                }
                // A probe's whole report (a runner that only said its kiln has no CPU count):
                // routing decides over it from now on (docs/ci.md W13-R01).
                if f.cpu.threads > 0
                    && let Some(a) = self.agents.lock().get_mut(agent)
                {
                    a.facts = Some(f);
                    self.ci.send(crate::ci::dispatch::Msg::RunnerFacts { agent: agent.to_owned() });
                }
            }
            FromRunner::ReachHello(a) => {
                // Job pods neither send nor receive reach links.
                if !self.agents.lock().get(agent).is_none_or(|r| r.ephemeral) {
                    tracing::info!(agent = %agent, fingerprint = %crate::reach::e2e::short(&a.fingerprint), accept = a.accept, "reach identity");
                    self.reach.lock().announce(agent, a);
                }
            }
            FromRunner::ReachAsk { req, ask } => match *ask {
                crate::reach::Question::Admission { mut ask } => {
                    ask.insert("op".into(), serde_json::json!("admission"));
                    let (hub, runner) = (Arc::clone(self), agent.to_owned());
                    crate::rt::spawn(async move {
                        let answer = hub.ci.admission(runner, serde_json::Value::Object(ask)).await;
                        hub.send_peer(peer, &ServerMsg::ReachAnswer { req, json: answer.to_string() }).await;
                    });
                }
                crate::reach::Question::Directory => {
                    let answer = serde_json::json!({ "runners": self.reach_directory() });
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
                    let up: Vec<String> = crate::reach::directory::holders(&repo, &digest, agent).into_iter().filter(|r| self.agents.lock().get(r).is_some_and(|a| a.connected)).collect();
                    self.send_peer(peer, &ServerMsg::ReachAnswer { req, json: serde_json::json!({ "runners": up }).to_string() }).await;
                }
                // An agent's MCP server on the runner asks for kiln's operations: answered on
                // their own, so a slow one holds up nothing else the runner says.
                ask => {
                    let (hub, runner) = (Arc::clone(self), agent.to_owned());
                    crate::rt::spawn(async move {
                        let answer = crate::mcp::runner_ask(&hub, &runner, &ask).await;
                        hub.send_peer(peer, &ServerMsg::ReachAnswer { req, json: answer.to_string() }).await;
                    });
                }
            },
            FromRunner::Power(b) => self.on_power(agent, b).await,
            FromRunner::AgentLimit(held) => crate::harness::limit::on_held(self, peer, agent, held),
            // An identity it asks for, for a key it made and keeps (docs/ci.md W27-R01).
            FromRunner::Identity(ask) => crate::identities::on_ask(self, peer, agent, ask).await,
            // Its replica's word in observe's group (docs/observe.md §5).
            FromRunner::Observe { msg, bytes } => {
                if matches!(&*msg, crate::observe::wire::Msg::Status(_)) {
                    let instances = self.instances.lock();
                    if crate::observe::hub::current_status(&instances, agent, peer, origin) {
                        crate::observe::hub::heard(agent, *msg, bytes);
                    }
                } else {
                    crate::observe::hub::heard(agent, *msg, bytes);
                }
            },
            FromRunner::Harness { session, update } => match *update {
                // A copy of a session that went on elsewhere since (an older epoch): its machine
                // lets go of it, and nothing it says of it counts.
                crate::harness::core::Update::Open { info, .. } if self.harness.stale(&session, info.epoch) => {
                    tracing::info!(%agent, %session, epoch = info.epoch, "an agent session that went on elsewhere: this machine lets go of its copy");
                    // A runner from before letting go stops its copy instead (what it says of it
                    // doesn't count either way).
                    let lets_go = self.instances.lock().get(peer).is_some_and(|i| i.wide & proto::CAP_HARNESS_RELEASE != 0 || i.caps & proto::CAP_CONVO != 0);
                    let msg = if lets_go { ServerMsg::HarnessRelease { session } } else { ServerMsg::HarnessClose { session } };
                    self.command_peers(agent, vec![peer], &msg).await.log_when_answered(LOG_WITHIN);
                }
                // An inbox's answer counts wherever it came from (it's for the message sent there),
                // and says nothing of where the session runs: whoever waits for it is told, and
                // the session's messages still go where it runs.
                u @ crate::harness::core::Update::Inbox { .. } => {
                    self.harness.ingest_from(self, &crate::harness::registry::Location::Machine(agent.to_owned()), &session, u, false);
                }
                // The rest of what a copy left behind says (its machine was told to let go of it):
                // none of it counts, and it doesn't take the session's messages from where it runs.
                u if !matches!(u, crate::harness::core::Update::Open { .. }) && !self.harness.runs_on(&session, agent) => {}
                u => {
                    let closed = matches!(u, crate::harness::core::Update::Closed);
                    // Two processes of one machine never both run a session: of two that say they
                    // hold it, the newer keeps it and the older lets go (a hand-over whose word
                    // came late, `crate::handovers`).
                    let (holder, older) = {
                        let inst = self.instances.lock();
                        let what = crate::instances::Owned::Agent(session.clone());
                        let other = inst.owner(&what).filter(|q| *q != peer && inst.hands_over(*q) && inst.hands_over(peer));
                        match other.and_then(|q| inst.newer_of(q, peer).map(|newer| (q, newer))) {
                            Some((q, newer)) if newer == q => (None, Some(peer)),
                            pair => (inst.get(peer).map(crate::instances::Instance::label).filter(|_| !closed), pair.map(|(q, _)| q)),
                        }
                    };
                    if older == Some(peer) {
                        if !self.handovers.lock().going_to(&session, peer) {
                            self.let_go_of(agent, peer, &session).await;
                        }
                        return Ok(());
                    }
                    // U17: commit ownership before routing or publishing the runner's update.
                    if closed {
                        self.harness.release_holder(&session).map_err(evidence_storage)?;
                    } else if let Some(label) = holder {
                        self.harness.held_by(&session, &label).map_err(evidence_storage)?;
                    }
                    {
                        let mut inst = self.instances.lock();
                        let what = crate::instances::Owned::Agent(session.clone());
                        if closed {
                            inst.disown(&what);
                        } else if inst.owner(&what) != Some(peer) {
                            inst.own(what, peer);
                        }
                    }
                    // Not one a hand-over is taking there now: the newer says so once it took it.
                    if let Some(older) = older.filter(|_| !self.handovers.lock().going_to(&session, peer)) {
                        self.let_go_of(agent, older, &session).await;
                    }
                    self.harness.ingest_from(self, &crate::harness::registry::Location::Machine(agent.to_owned()), &session, u, false);
                    if closed {
                        self.check_retire().await;
                        self.check_handover().await;
                    }
                }
            },
            FromRunner::HarnessInventory(inv) => {
                let mut inv = *inv;
                // A copy it kept of a session that went on elsewhere since isn't its own (it
                // lets go of it once it announces it).
                inv.sessions.retain(|id| self.harness.runs_on(id, agent));
                // This process's sessions are its own; another process of the machine keeps
                // the ones it holds (the registry forgets sessions a report leaves out), and
                // so does one that isn't connected now (`Registry::holder`).
                let (me, present, others) = {
                    let inst = self.instances.lock();
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
                };
                if let Some(me) = &me {
                    for id in &inv.sessions {
                        self.harness.held_by(id, me).map_err(evidence_storage)?;
                    }
                }
                let own = inv.sessions.clone();
                inv.sessions.extend(others);
                self.harness.inventory(self, agent, me.as_deref(), &present, inv).map_err(evidence_storage)?;
                {
                    let mut inst = self.instances.lock();
                    for id in &own {
                        inst.own(crate::instances::Owned::Agent(id.clone()), peer);
                    }
                    inst.reported(peer);
                }
                // Hand-overs dropped as it went away get their word now it's back.
                self.handovers_dropped(agent, peer, &own).await;
                self.check_retire().await;
            }
        }
        Ok(())
    }

    async fn on_hello(&self, peer: PeerId, h: Hello) -> anyhow::Result<()> {
        let cert_name = self.pending_peers.lock().get(&peer).cloned();
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
        // U03: bind this exact certificate to the verified pod UID/node before any registration.
        if let Err(error) = self.verify_executor_peer(&name, peer).await {
            refuse(format!("executor enrollment refused: {error:#}")).await;
            return Ok(());
        }
        let now = crate::util::now_unix();
        // It says in a moment whether it keeps the channel (its Caps follow its hello and Build):
        // until then its commands wait for that (docs/wire.md, "Commands"), from before it can be
        // sent any. A job pod never says.
        if !h.ephemeral {
            self.cmds.lock().coming(peer, Instant::now());
        }
        // A machine may connect more than one runner process (a new one beside an old one that
        // keeps its terminals, see `crate::instances`). The same process on a new connection
        // replaces its stale one.
        let joined = self.instances.lock().join(peer, &name, h.clone(), now);
        for p in &joined.stale {
            self.peers.lock().remove(p);
            self.lost_instances.lock().remove(p);
            self.net.command(crate::net::Out::Disconnect { peer: *p }).await;
        }
        self.lost_instances.lock().remove(&peer);
        let others: Vec<PeerId> = self.instances.lock().of(&name).into_iter().filter(|p| *p != peer).collect();
        let held_by_other = |job: &str| {
            self.instances.lock().owner(&crate::instances::Owned::Job(job.to_owned())).is_some_and(|o| o != peer && others.contains(&o))
        };
        // Reconcile what the agent says it runs with what we think it runs.
        let db_running: Vec<Job> = self
            .store
            .running_jobs()
            .await?
            .into_iter()
            .filter(|j| j.agent.as_deref() == Some(name.as_str()))
            .collect();
        let mut keep = Vec::new();
        let canonical = self.store.ci_legs(h.running.iter().cloned().chain(db_running.iter().map(|j| j.id.clone())).collect()).await?;
        for j in &db_running {
            if canonical.contains(&j.id) { continue; }
            if h.running.contains(&j.id) {
                keep.push(j.id.clone());
            } else if held_by_other(&j.id) {
                // Another runner process of this machine runs it.
            } else {
                match self.store.requeue_job(j.id.clone(), format!("agent {name} restarted without the job")).await {
                    Ok(_) => self.emit_job(&j.id).await,
                    Err(error) if error.is::<crate::store::CanonicalJobMutation>() => {},
                    Err(error) => return Err(error),
                }
            }
        }
        // U01/U03: canonical ownership survives disabled ingress and controller recovery.
        for j in &h.running {
            if keep.contains(j) {
                continue;
            }
            // CI's legs are the dispatcher's to re-adopt, or to stop if they were given up while
            // the runner was away (`Msg::RunnerUp` below, W05-R05).
            if self.ci.is_leg(j) || canonical.contains(j) {
                keep.push(j.clone());
                continue;
            }
            // The server requeued this job while the agent was away, but the agent kept working:
            // take it back (claim) rather than throwing the work away. Only a job that has since
            // been taken by someone else (or settled) is cancelled on this agent.
            let executor = if h.ephemeral { "k8s" } else { "baremetal" };
            let claimed = match self.store.claim_job(j.clone(), name.clone(), executor.into()).await {
                Ok(claimed) => claimed,
                Err(error) if error.is::<crate::store::CanonicalJobMutation>() => {
                    keep.push(j.clone());
                    continue;
                }
                Err(error) => return Err(error),
            };
            if claimed {
                tracing::info!(job = %j, agent = %name, "agent reclaimed a requeued job");
                keep.push(j.clone());
                self.emit_job(j).await;
            } else {
                self.command_peers(&name, vec![peer], &ServerMsg::Cancel { job: j.clone(), reason: "job was reassigned".into() }).await.log_when_answered(LOG_WITHIN);
            }
        }
        let outpost = is_outpost(&h.labels, h.slots);
        let live = AgentLive {
            name: name.clone(),
            peer: Some(peer),
            labels: h.labels.clone(),
            // An outpost takes no jobs: it keeps no slots (CI's dispatch).
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
        };
        let prev = self.agents.lock().get(&name).cloned();
        // Its facts stay until its new connection's probe reports.
        let prev_facts = prev.as_ref().and_then(|a| a.facts.clone());
        // Its battery as last said, until it says again.
        let prev_battery = prev.as_ref().and_then(|a| a.battery.clone());
        let draining = prev.as_ref().is_some_and(|a| a.draining);
        {
            let mut inst = self.instances.lock();
            for j in &live.running {
                inst.own(crate::instances::Owned::Job(j.clone()), peer);
            }
        }
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
        self.peers.lock().insert(peer, name.clone());
        self.pending_peers.lock().remove(&peer);
        self.agents.lock().insert(name.clone(), live.clone());
        // It keeps the channel (docs/wire.md): said first, so a runner that keeps it too knows on
        // Welcome not to send its sessions whole. An older runner skips the frame.
        let amphora = if self.cfg.cluster.replica.is_some() && self.replica_carrier_active.get().is_some_and(|active| active.load(Ordering::Acquire)) { proto::CAP_AMPHORA } else { 0 };
        self.send_peer(peer, &ServerMsg::Caps { caps: proto::CAP_WIRE | proto::CAP_NET_DROPS | proto::CAP_HANDOVER | proto::CAP_WORK | proto::CAP_AGENT_LIMIT | proto::CAP_JOB_ATTEMPT | proto::CAP_RETAINED_RECEIVED | proto::CAP_IDENTITY | proto::CAP_HEADROOM | proto::CAP_CHANGES_PATCH | proto::CAP_FIRECRACKER_SHAPE | amphora | proto::CAP_UNREAD | proto::CAP_UNIFIED_LAYOUT | proto::CAP_EXPLICIT_ACTIVATION | proto::CAP_ORDINARY_EXECUTION_DEBT }).await;
        // What its sessions' copies have here, so a stream it starts sends only what they lack
        // (`crate::wire::sums`): a few dozen sessions a frame, each about a kilobyte.
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
        tracing::info!(agent = %name, labels = ?live.labels, slots = live.slots, "agent connected");
        self.ci.send(crate::ci::dispatch::Msg::RunnerUp { agent: name.clone(), running: h.running.clone() });
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
                    if let Some(tx) = self.terms.lock().remove(&format!("{name}/{s}")) {
                        let _ = tx.try_send(TermOut::Gone);
                    }
                }
                Owned::Agent(id) => agents.push(id),
                Owned::Job(j) => jobs.push(j),
            }
        }
        if !agents.is_empty() {
            self.harness.sessions_lost(self, &agents);
        }
        if !jobs.is_empty() {
            self.lost_instances.lock().insert(peer, (name.to_owned(), Instant::now(), jobs));
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
        self.wake.notify();
    }

    /// The machine's newest process (maybe another one now) describes it, and runs its front door.
    async fn newest_changed(&self, name: &str) {
        let newest = {
            let inst = self.instances.lock();
            inst.newest(name).and_then(|p| inst.get(p).map(|i| (p, i.hello.clone())))
        };
        let snapshot = {
            let mut g = self.agents.lock();
            let Some(a) = g.get_mut(name) else { return };
            if let Some((p, h)) = newest {
                a.peer = Some(p);
                a.outpost = is_outpost(&h.labels, h.slots);
                a.labels = h.labels;
                a.slots = if a.outpost { 0 } else { h.slots.max(1) };
                a.hostname = h.hostname;
                a.os = h.os;
                a.arch = h.arch;
                a.version = h.version;
                a.terminal = h.terminal;
            }
            a.clone()
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
        self.ci.send(crate::ci::dispatch::Msg::RunnerFacts { agent: name.to_owned() });
    }

    /// A machine's runner processes, while it has more than one, with what each holds.
    fn instance_views(&self, name: &str) -> Vec<crate::instances::InstanceView> {
        if self.instances.lock().of(name).len() < 2 {
            return Vec::new();
        }
        self.runner_processes(name)
    }

    /// Every runner process of a machine and what each one holds (System → Updates lists
    /// them always; agent views only while a machine has more than one).
    /// What a machine's newest runner process says it can do ([`crate::proto::CAP_UPGRADE`] …);
    /// 0 for runners from before builds were announced.
    /// U03/W13-R09: capability and process selection are one Instances snapshot.
    pub fn capture_newest_runner(&self, machine: &str, required: u64) -> Result<crate::instances::CapturedRunner, crate::instances::CaptureRefusal> {
        let instances = self.instances.lock();
        let captured = instances.capture_newest_runner(machine,required | proto::CAP_WIRE)?;
        if required & proto::CAP_ORDINARY_EXECUTION_DEBT != 0 && !self.cmds.lock().is_up(captured.peer()) {
            return Err(crate::instances::CaptureRefusal::Capabilities);
        }
        Ok(captured)
    }
    pub fn capture_original_runner(&self, machine: &str, peer: PeerId, process_nonce: &str, required: u64) -> Result<crate::instances::CapturedRunner, crate::instances::CaptureRefusal> {
        let instances = self.instances.lock();
        let captured = instances.capture_original_runner(machine,peer,process_nonce,required | proto::CAP_WIRE)?;
        if required & proto::CAP_ORDINARY_EXECUTION_DEBT != 0 && !self.cmds.lock().is_up(peer) {
            return Err(crate::instances::CaptureRefusal::Capabilities);
        }
        Ok(captured)
    }
    /// U01/U03: reserve the SAME captured command owner before any persistence wait.
    pub(crate) fn prepare_ordinary_command(&self, captured: &crate::instances::CapturedRunner, grant: String) -> anyhow::Result<crate::wire::commands::PreparedCommand> {
        let instances = self.instances.lock();
        anyhow::ensure!(instances.still_captured(captured) && captured.ordinary_epoch().is_some(), "ordinary captured admission is not registered on this connection");
        self.cmds.lock().prepare_bound(captured.connection(),grant).map_err(anyhow::Error::msg)
    }
    pub(crate) fn abort_ordinary_command(&self, token: &crate::wire::commands::PreparedCommand) -> bool { self.cmds.lock().abort_prepared(token) }
    pub(crate) async fn publish_ordinary_command(&self, captured: &crate::instances::CapturedRunner, token: &crate::wire::commands::PreparedCommand, msg: &ServerMsg) -> anyhow::Result<Pending> {
        let body = proto::encode(msg)?;
        let answer = {
            let instances = self.instances.lock();
            if !instances.still_captured(captured) {
                self.cmds.lock().abort_prepared(token);
                anyhow::bail!("ordinary captured connection changed before exposure");
            }
            self.cmds.lock().publish_prepared(token,body,Instant::now()).map_err(anyhow::Error::msg)?
        };
        self.flush_commands().await;
        Ok(Pending { agent: captured.machine().to_owned(), what: crate::wire::server_name(msg), waits: vec![answer], at_once: crate::wire::commands::Answer::Taken, away: false })
    }

    /// U03: use the existing Live command seq/ACK owner, retaining the captured route on every retry.
    /// Grant persistence authorizes NEW items after a reconnect; an old item never changes route.
    pub async fn send_captured_runner(&self, captured: &crate::instances::CapturedRunner, msg: &ServerMsg) -> anyhow::Result<Pending> {
        use crate::wire::commands::Answer;
        if stream_of(msg) != Some(crate::wire::commands::Kind::Live) { anyhow::bail!("captured runner commands require the Live command stream"); }
        let frame = proto::encode(msg)?;
        let preempt = preempts_ordinary(msg);
        let mut answer = {
            let instances = self.instances.lock();
            if !instances.still_captured(captured) {
                return Ok(Pending { agent: captured.machine().to_owned(), what: crate::wire::server_name(msg), waits: Vec::new(), at_once: Answer::Unreachable { why: "its captured connection changed before command admission".into() }, away: false });
            }
            let mut commands = self.cmds.lock();
            if preempt { commands.preempt_unexposed(captured.connection().peer()); }
            commands.push_bound(captured.connection(),frame,Instant::now())
        };
        // Preserve positive queue NACKs for the existing authoritative dispatcher lifecycle.
        // Taken is an accepted ordered intent; only Said/physical proof establishes its result.
        let mut waits = Vec::new();
        let at_once = match answer.try_recv() {
            Ok(Some(answer)) => answer,
            Ok(None) => { waits.push(answer); Answer::Taken }
            Err(_) => Answer::Unanswered { why: "the captured command owner ended without an answer".into() },
        };
        self.flush_commands().await;
        Ok(Pending { agent: captured.machine().to_owned(), what: crate::wire::server_name(msg), waits, at_once, away: false })
    }

    pub fn runner_caps(&self, name: &str) -> u32 {
        let inst = self.instances.lock();
        inst.newest(name).and_then(|p| inst.get(p)).map_or(0, |i| i.caps)
    }

    /// What a machine's newest runner can do in 64 bits: `Build`'s 32 and the rest
    /// ([`proto::AgentMsg::Caps`]: [`proto::CAP_WIRE`] ...).
    pub fn runner_wide_caps(&self, name: &str) -> u64 {
        let inst = self.instances.lock();
        let Some(peer) = inst.newest(name) else { return 0 };
        let wide = inst.effective_wide(peer);
        // Other capability meanings remain unchanged. Strict new admission also needs
        // this exact joined process/domain and its post-transition current Have.
        if wide & proto::CAP_ORDINARY_EXECUTION_DEBT != 0
            && (inst.capture_newest_runner(name,proto::CAP_WIRE | proto::CAP_ORDINARY_EXECUTION_DEBT).is_err() || !self.cmds.lock().is_up(peer))
        { wide & !proto::CAP_ORDINARY_EXECUTION_DEBT } else { wide }
    }

    /// The kiln a machine's newest runner process says it runs (`crate::fleet`).
    pub fn runner_kiln(&self, name: &str) -> Option<kiln_ci::facts::KilnFacts> {
        let inst = self.instances.lock();
        inst.newest(name).and_then(|p| inst.get(p)).and_then(|i| i.kiln.clone())
    }

    pub fn runner_processes(&self, name: &str) -> Vec<crate::instances::InstanceView> {
        use crate::instances::Owned;
        let target = format!("agent:{name}");
        let terms: Vec<Owned> = self.terminals.list().into_iter().filter(|t| t.target == target && !t.exited).map(|t| Owned::Terminal(t.id)).collect();
        let agents: Vec<Owned> = self.harness.list().into_iter().filter(|i| i.target == target).map(|i| Owned::Agent(i.id)).collect();
        let jobs: Vec<Owned> = self.agents.lock().get(name).map(|a| a.running.iter().cloned().map(Owned::Job).collect()).unwrap_or_default();
        let inst = self.instances.lock();
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
        let crowded = self.instances.lock().crowded();
        if crowded.is_empty() {
            return;
        }
        let mut sends = Vec::new();
        for (name, peer) in crowded {
            let target = format!("agent:{name}");
            let mut held: Vec<Owned> =
                self.terminals.list().into_iter().filter(|t| t.target == target && !t.exited).map(|t| Owned::Terminal(t.id)).collect();
            held.extend(self.harness.list().into_iter().filter(|i| i.target == target).map(|i| Owned::Agent(i.id)));
            held.extend(self.agents.lock().get(&name).into_iter().flat_map(|a| a.running.iter().cloned().map(Owned::Job)));
            let mut inst = self.instances.lock();
            let Some(newest) = inst.newest(&name) else { continue };
            if peer == newest || held.iter().any(|o| inst.owner(o) != Some(newest)) {
                continue;
            }
            let Some(i) = inst.get_mut(peer) else { continue };
            // Not before it said what it holds: just connected (the server started again, or it
            // did), its agent sessions aren't known here yet, and it would look as if it held none.
            if !i.reported || i.retire_sent.is_some_and(|t| t.elapsed() < RETIRE_REPEAT) {
                continue;
            }
            i.retire_sent = Some(Instant::now());
            sends.push((name, newest, i.label()));
        }
        for (name, newest, instance) in sends {
            tracing::info!(agent = %name, %instance, "an older runner process of this machine holds nothing any more; asking the newest to retire it");
            self.command_peers(&name, vec![newest], &ServerMsg::Retire { instance }).await.log_when_answered(LOG_WITHIN);
        }
    }

    pub(crate) fn release_slot(&self, agent: &str, job: &str) {
        self.instances.lock().disown(&crate::instances::Owned::Job(job.to_owned()));
        let mut g = self.agents.lock();
        if let Some(a) = g.get_mut(agent) {
            a.running.retain(|j| j != job);
            a.held.retain(|(j, _)| j != job);
            a.busy = u32::try_from(a.running.len()).unwrap_or(0);
            if a.running.is_empty() {
                a.exclusive = false;
                a.held.clear();
            }
        }
    }

    async fn job_done(&self, agent: &str, r: JobResult) -> anyhow::Result<()> {
        let job = self.store.job(r.job.clone()).await?;
        let (run_id, settled) = self
            .store
            .finish_job(r.job.clone(), r.status, r.exit_code, r.error.clone(), r.peak_rss, r.cpu_seconds)
            .await?;
        // U01/U03: only a committed legacy finish permits host ownership cleanup.
        self.release_slot(agent, &r.job);
        self.cancel_sent.lock().remove(&r.job);
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
        let snapshot = self.agents.lock().get(agent).cloned();
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

    pub async fn cancel_run(&self, run_id: &str, reason: &str) -> Result<(), crate::ci::dispatch::CancelError> {
        use crate::ci::dispatch::CancelError;
        // W05-R25/U03: durable ownership wins even while dispatch is rebuilding or fenced.
        // Falling back on its transient live-run set could bypass the canonical replay log.
        let id = run_id.to_owned();
        let canonical: Option<(bool, u32, String)> = self.store.call(move |c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM ci_plans WHERE run_id = r.id), r.attempt, r.status FROM runs r WHERE r.id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?)).await.map_err(CancelError::commit)?;
        if let Some((true, attempt, status)) = canonical {
            if Status::parse(&status).is_terminal() { return Ok(()); }
            return self.ci.cancel(run_id, attempt, reason).await;
        }
        let running = self.store.cancel_run(run_id.to_owned(), reason.to_owned()).await.map_err(|error| {
            if error.is::<crate::store::CanonicalRunCancellation>() {
                CancelError::Unavailable("the workflow engine took ownership during cancellation")
            } else { CancelError::commit(error) }
        })?;
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

    // ------------------------------------------------------------ job maintenance

    /// Maintain agent liveness and old running assignments. The CI dispatcher owns all new
    /// placement, including jobs imported from the legacy workflow store.
    pub async fn job_maintenance_loop(self: Arc<Self>) -> anyhow::Result<()> {
        let mut tick = Interval::new(Duration::from_secs(5));
        loop {
            futures::select_biased! {
                () = self.shutdown.cancelled().fuse() => return Ok(()),
                () = tick.tick().fuse() => {}
            }
            self.check_timeouts().await?;
            self.check_lost_agents().await;
            crate::ci::notify::runners_gone(&self);
            crate::ci::notify::secrets_unused(&self);
            crate::ci::notify::quarantined(&self);
        }
    }

    /// The server's secrets, for the CI engine's trusted runs (docs/ci.md W09 narrows them).
    pub fn ci_secrets(&self) -> Vec<(String, String)> {
        self.secrets()
    }

    fn secrets(&self) -> Vec<(String, String)> {
        self.cfg
            .secrets
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

    async fn check_timeouts(&self) -> anyhow::Result<()> {
        if self.ci.restored().is_none() { return Ok(()); }
        let now = crate::util::now_unix();
        for job in self.store.running_jobs().await? {
            if self.ci.is_leg(&job.id) { continue; }
            let Some(spec) = &job.spec else { continue };
            let Some(started) = job.started_at else { continue };
            let limit = i64::from(spec.timeout_minutes) * 60;
            if now - started <= limit {
                continue;
            }
            let sent = self.cancel_sent.lock().get(&job.id).copied();
            match sent {
                None => {
                    if let Some(agent) = &job.agent {
                        self.command_start(agent, &ServerMsg::Cancel { job: job.id.clone(), reason: "timeout".into() }).await.log_when_answered(LOG_WITHIN);
                    }
                    self.cancel_sent.lock().insert(job.id.clone(), Instant::now());
                }
                Some(t) if t.elapsed() > Duration::from_secs(60) => {
                    // The agent did not report back: settle it ourselves.
                    let (run_id, _) = self
                        .store
                        .finish_job(job.id.clone(), Status::TimedOut, None, Some(format!("exceeded {} min", spec.timeout_minutes)), None, None)
                        .await?;
                    if let Some(agent) = &job.agent { self.release_slot(agent, &job.id); }
                    self.cancel_sent.lock().remove(&job.id);
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
        if self.ci.restored().is_none() { return; }
        // Jobs of a runner process that went away while its machine stayed connected through
        // another one, and did not come back in time.
        let expired: Vec<(String, Vec<String>)> = {
            let mut g = self.lost_instances.lock();
            let peers: Vec<PeerId> = g.iter().filter(|(_, (_, t, _))| t.elapsed() > AGENT_GRACE).map(|(p, _)| *p).collect();
            peers.into_iter().filter_map(|p| g.remove(&p)).map(|(name, _, jobs)| (name, jobs)).collect()
        };
        for (name, jobs) in expired {
            if jobs.iter().any(|j| self.ci.is_leg(j)) {
                self.ci.send(crate::ci::dispatch::Msg::RunnerLost { agent: name.clone() });
            }
            for j in jobs.into_iter().filter(|j| !self.ci.is_leg(j)) {
                let still = self.agents.lock().get(&name).is_some_and(|a| a.running.contains(&j));
                if !still {
                    continue;
                }
                match self.store.requeue_job(j.clone(), format!("runner process did not reconnect ({name})")).await {
                    Ok(_) => { self.release_slot(&name, &j); self.emit_job(&j).await; }
                    Err(error) if error.is::<crate::store::CanonicalJobMutation>() => self.ci.send(crate::ci::dispatch::Msg::RunnerLost { agent: name.clone() }),
                    Err(e) => tracing::error!(error = %e, job = %j, "requeue failed"),
                }
            }
        }
        self.check_retire().await;
        let lost: Vec<String> = self
            .agents
            .lock()
            .values()
            .filter(|a| !a.connected && a.lost_at.is_some_and(|t| t.elapsed() > AGENT_GRACE) && !a.running.is_empty())
            .map(|a| a.name.clone())
            .collect();
        for name in lost {
            self.requeue_agent_jobs(&name, "agent did not reconnect").await;
        }
        self.agents.lock().retain(|_, a| {
            !(a.ephemeral && !a.connected && a.running.is_empty() && a.lost_at.is_some_and(|t| t.elapsed() > AGENT_GRACE))
        });
        self.declined.lock().retain(|_, t| t.elapsed() < DECLINE_BACKOFF);
        // Running jobs assigned to agents we have no session for at all (after a server restart).
        if self.started.elapsed() > AGENT_GRACE
            && let Ok(running) = self.store.running_jobs().await {
                // Jobs held by a connected agent, or by one still inside its reconnect grace, are
                // not orphans.
                let known: HashSet<String> = self
                    .agents
                    .lock()
                    .values()
                    .filter(|a| a.connected || a.lost_at.is_some_and(|t| t.elapsed() <= AGENT_GRACE))
                    .flat_map(|a| a.running.clone())
                    .collect();
                for j in running {
                    if !self.ci.is_leg(&j.id) && !known.contains(&j.id) && j.agent.as_deref() != Some("-")
                        && let Ok(true) = self.store.requeue_job(j.id.clone(), "orphaned after restart".into()).await {
                            self.emit_job(&j.id).await;
                        }
                }
            }
        // Expire unused pod enrollment tokens.
        self.ephemeral_tokens.lock().retain(|_, (_, exp)| *exp > Instant::now());
    }

    /// Sample the server host itself for the UI ("server" series).
    pub async fn sampler_loop(self: Arc<Self>) -> anyhow::Result<()> {
        let mut sampler = crate::host::Sampler::new(Some(&self.cfg.server.data_dir));
        let mut tick = Interval::new(Duration::from_secs(u64::from(self.cfg.telemetry.sample_seconds.max(1))));
        loop {
            futures::select_biased! {
                () = self.shutdown.cancelled().fuse() => return Ok(()),
                () = tick.tick().fuse() => {}
            }
            let s = sampler.sample();
            self.record_sample("server", s);
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

/// Whether a runner has room for one more job or leg, by the rule the engine's scheduler and the
/// runner's own admission use (`kiln_ci::sched::state::room`): its slots, its size as its facts
/// give it, what's held of it and what runs.
#[cfg(test)]
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

    /// U15/U03: a replacement's current TLS origin cannot inherit the old joined instance's
    /// Caps, and refusing that transport proof never releases its physical job ownership.
    #[test]
    fn u15_private_layout_is_bound_to_joined_current_connection() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = hub_with_runner(Arc::new(kiln_seams::clock::SimClock::new()), "box", 0).await?;
            let (former, current) = (crate::net::Connection::new(1, 1, [1; 32]), crate::net::Connection::new(1, 2, [2; 32]));
            hub.instances.lock().connected(1, 1, Some([1; 32]));
            join_runner(&hub, 1, "box", proto::CAP_UNIFIED_LAYOUT | proto::CAP_JOB_ATTEMPT);
            hub.instances.lock().own(crate::instances::Owned::Job("physical".into()), 1);
            let completion = AgentMsg::JobFinishedV2 { result: JobResult { job: "physical".into(), status: Status::Succeeded, exit_code: Some(0), error: None, peak_rss: None, cpu_seconds: None }, attempt: 3 };
            assert!(hub.accepts_layout(1, &completion, Some(former)).await?);
            // Exercise the reproduced window directly: Connected changed the live connection
            // map, but Hello has not replaced the old joined Instance (its bit49 is still set).
            hub.instances.lock().connected(1, 2, Some([2; 32]));
            assert!(!hub.accepts_layout(1, &completion, Some(current)).await?, "new current origin cannot borrow former joined proof");
            assert!(hub.accepts_layout(1, &completion, Some(former)).await.is_err(), "stale origin cannot borrow cached proof");
            let caps = AgentMsg::Caps { caps: proto::CAP_UNIFIED_LAYOUT | proto::CAP_JOB_ATTEMPT };
            assert!(!hub.accepts_layout(1, &caps, Some(current)).await?, "Caps before a joined current Hello cannot modify the former Instance");
            assert_eq!(hub.instances.lock().owner(&crate::instances::Owned::Job("physical".into())), Some(1), "layout refusal is no physical stop proof");
            join_runner(&hub, 1, "box", 0);
            assert!(!hub.accepts_layout(1, &completion, Some(current)).await?, "Hello alone carries no reconciled capability proof");
            assert!(hub.accepts_layout(1, &caps, Some(current)).await?);
            hub.instances.lock().set_wide(1, proto::CAP_UNIFIED_LAYOUT | proto::CAP_JOB_ATTEMPT);
            assert!(hub.accepts_layout(1, &completion, Some(current)).await?);
            // A2: legacy Live can already be nonempty before the ordinary declaration.
            // First strict publication must rotate before eligibility, then wait for the
            // actual current Have; duplicate strict Caps must preserve the exact owner.
            use crate::wire::channel::{Frame,Has,Arrival,Receiver};
            use crate::wire::commands::{Answer,Kind as Stream};
            let base = proto::CAP_UNIFIED_LAYOUT | proto::CAP_WIRE | proto::CAP_RESOURCE_OWNERSHIP | proto::CAP_JOB_ATTEMPT;
            let strict = base | proto::CAP_ORDINARY_EXECUTION_DEBT;
            let nonce = "r".repeat(43);
            hub.instances.lock().set_process(1,nonce.clone(),Some(current))?;
            assert!(hub.on_agent_msg_from(1,AgentMsg::Caps{caps:strict},Some(current)).await.is_err(), "premature raw51 cannot activate retroactively when a domain later arrives");
            assert_eq!(hub.instances.lock().effective_wide(1) & proto::CAP_ORDINARY_EXECUTION_DEBT,0);
            hub.on_agent_msg_from(1,AgentMsg::Caps{caps:base},Some(current)).await?;
            hub.on_wire(1,"box",Frame::Have(Vec::new()),Some(current)).await;
            let command = ServerMsg::HarnessCmd{session:"s".into(),json:serde_json::to_string(&crate::harness::model::Command::Interrupt)?};
            assert_eq!(stream_of(&command),Some(Stream::Live));
            let mut legacy_answer = hub.cmds.lock().push(1,Stream::Live,proto::encode(&command)?,Instant::now());
            let legacy_frame = hub.cmds.lock().poll_routed(Instant::now()).into_iter().find_map(|(_,frame,_)|matches!(&frame,Frame::Item{..}).then_some(frame)).context("legacy Live frame missing")?;
            let Frame::Item{stream:old_stream,seq,low,body}=legacy_frame else {anyhow::bail!("legacy Live item missing")};
            let mut receiver=Receiver::new();assert!(matches!(receiver.on_item(&old_stream,seq,low,body),Arrival::Take(_)));
            hub.on_wire(1,"box",receiver.took(&old_stream,seq),Some(current)).await;
            assert_eq!(legacy_answer.try_recv()?,Some(Answer::Taken));assert!(receiver.state(&old_stream).upto>0);
            // The real domain handler has already committed before declaring this SAME
            // Instances registration. This fixture tests the subsequent channel boundary.
            hub.instances.lock().set_ordinary_domain(1,current,"ordinary-ready".into())?;
            hub.on_agent_msg_from(1,AgentMsg::Caps{caps:strict},Some(current)).await?;
            assert_eq!(hub.runner_wide_caps("box") & proto::CAP_ORDINARY_EXECUTION_DEBT,0);
            assert!(matches!(hub.capture_newest_runner("box",proto::CAP_ORDINARY_EXECUTION_DEBT),Err(crate::instances::CaptureRefusal::Capabilities)));
            assert!(matches!(hub.capture_original_runner("box",1,&nonce,proto::CAP_ORDINARY_EXECUTION_DEBT),Err(crate::instances::CaptureRefusal::Capabilities)));
            assert!(hub.capture_newest_runner("box",proto::CAP_RESOURCE_OWNERSHIP | proto::CAP_JOB_ATTEMPT).is_ok(), "managed/non51 capability meanings are unchanged");
            hub.on_wire(1,"box",Frame::Have(Vec::new()),Some(former)).await;
            assert_eq!(hub.runner_wide_caps("box") & proto::CAP_ORDINARY_EXECUTION_DEBT,0,"stale Have cannot open readiness");
            hub.on_wire(1,"box",Frame::Have(Vec::new()),Some(current)).await;
            assert_ne!(hub.runner_wide_caps("box") & proto::CAP_ORDINARY_EXECUTION_DEBT,0);
            let captured=hub.capture_newest_runner("box",proto::CAP_ORDINARY_EXECUTION_DEBT)?;
            let token=hub.prepare_ordinary_command(&captured,"retained-original".into())?;
            assert_ne!(token.stamp().stream,old_stream,"first strict declaration uses a fresh Live generation");
            hub.on_agent_msg_from(1,AgentMsg::Caps{caps:strict},Some(current)).await?;
            assert!(!token.never_exposed(),"duplicate Caps cannot retire its private original token");
            let mut answer=hub.cmds.lock().publish_prepared(&token,b"original bytes".to_vec(),Instant::now()).map_err(anyhow::Error::msg)?;
            hub.on_agent_msg_from(1,AgentMsg::Caps{caps:strict},Some(current)).await?;
            assert!(answer.try_recv()?.is_none(),"duplicate Caps cannot abandon an exposed original");
            let frame=hub.cmds.lock().poll_routed(Instant::now()).into_iter().find_map(|(_,frame,route)|match frame {
                Frame::Item{stream,seq,body,..} if stream==token.stamp().stream && seq==token.stamp().seq=>Some((stream,seq,body,route)),_=>None,
            }).context("original command identity disappeared")?;
            assert_eq!(frame.2.as_slice(),b"original bytes");assert_eq!(frame.3,Some(current));
            hub.on_wire(1,"box",Frame::Ack(Has{stream:frame.0,upto:frame.1,nacks:Vec::new()}),Some(current)).await;
            assert_eq!(answer.try_recv()?,Some(Answer::Taken));
            hub.instances.lock().connected(1,3,Some([3;32]));
            assert_eq!(hub.runner_wide_caps("box") & proto::CAP_ORDINARY_EXECUTION_DEBT,0,"replacement cannot borrow the former joined channel readiness");
            assert_eq!(hub.instances.lock().owner(&crate::instances::Owned::Job("physical".into())),Some(1));
            Ok(())
        })
    }

    async fn legacy_running_fixture() -> anyhow::Result<(Arc<Hub>, String, String)> {
        let hub = scratch_hub_with(|cfg| cfg.ci.enabled = false).await?;
        let repo = hub.store.upsert_repo(crate::store::Repo {
            id: "adoption-repo".into(), name: "test/adoption".into(), clone_url: "file:///missing".into(),
            branches: vec!["main".into()], poll_seconds: 60, enabled: true, created_at: 0,
            last_polled_at: None, last_error: None, mode: "upstream".into(), default_branch: Some("main".into()), mirror: None,
        }).await?;
        let workflow = crate::pipeline::parse("name: ci\njobs:\n  build:\n    steps: [{run: echo build}]\n  dependent:\n    needs: [build]\n    steps: [{run: echo dependent}]\n")?;
        hub.store.create_run(crate::store::Run {
            id: "adoption-run".into(), repo_id: repo.id, repo_name: repo.name, number: 0,
            pipeline: "ci".into(), event: "push".into(), branch: "main".into(), sha: "saved-sha".into(),
            message: String::new(), author: String::new(), status: Status::Queued, created_at: 0,
            started_at: None, finished_at: None, error: None, concurrency_group: None,
        }, crate::pipeline::plan(&workflow, &crate::pipeline::Context::default())?).await?;
        let jobs = hub.store.jobs("adoption-run".into(), true).await?;
        let job = jobs.iter().find(|j| j.key == "build").map(|j| j.id.clone()).ok_or_else(|| anyhow::anyhow!("missing build"))?;
        let dependent = jobs.iter().find(|j| j.key == "dependent").map(|j| j.id.clone()).ok_or_else(|| anyhow::anyhow!("missing dependent"))?;
        assert!(hub.store.claim_job(job.clone(), "box".into(), "baremetal".into()).await?);
        hub.pending_peers.lock().insert(77, "box".into());
        hub.on_hello(77, Hello { protocol: proto::VERSION, name: "box".into(), labels: vec![], slots: 1, hostname: "box".into(), os: "linux".into(), arch: "x86_64".into(), version: String::new(), ephemeral: false, dedicated_job: None, running: vec![job.clone()], terminal: false }).await?;
        if let Some(instance) = hub.instances.lock().get_mut(77) { instance.wide |= proto::CAP_JOB_ATTEMPT | proto::CAP_UNIFIED_LAYOUT; }
        if let Some(runner) = hub.agents.lock().get_mut("box") {
            runner.held = vec![(job.clone(), kiln_ci::sched::state::Resources { cpu: 2.0, mem: 4096, disk: 0, gpu: 0 })];
        }
        Ok((hub, job, dependent))
    }

    /// U03: old transport from an imported execution records only fields it actually carries.
    /// Exact-process and original-assignment fences reject native, stale and other-owner frames.
    #[test]
    fn u03_imported_old_step_progress_preserves_canonical_metadata() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (hub, job, dependent) = legacy_running_fixture().await?;
            assert!(crate::ci::legacy::import_run(&hub, "adoption-run").await?);
            let saved_job = job.clone();
            let metadata: (String, Option<String>, Option<String>, Option<String>) = hub.store.call(move |c| {
                c.execute(r#"UPDATE steps SET outcome = 'failure', conclusion = 'success', outputs = '{"saved":true}' WHERE job_id = ?1 AND idx = 0"#, [&saved_job])?;
                Ok(c.query_row("SELECT name, outcome, conclusion, outputs FROM steps WHERE job_id = ?1 AND idx = 0", [&saved_job], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?)
            }).await?;
            hub.peers.lock().insert(78, "box".into());
            assert!(!hub.runner_step(78, "box", &job, 0, Status::Running, None).await?, "same machine is not the exact owning process");
            assert!(!crate::ci::runs::imported_step(&hub, "another-box", &job, 0, Status::Running, None).await?, "durable assignment also authenticates the runner");
            assert!(hub.owns_job(77, &job));
            hub.on_agent_msg(77, AgentMsg::StepStarted { job: job.clone(), idx: 0 }).await?;
            assert_eq!(hub.store.job(job.clone()).await?.and_then(|j| j.steps.into_iter().find(|s| s.idx == 0)).map(|s| s.status), Some(Status::Running), "an attempt-capable process may report its proven original imported execution through the old frame");
            hub.on_agent_msg(77, AgentMsg::StepFinished { job: job.clone(), idx: 0, status: Status::Failed, exit_code: Some(9) }).await?;
            let step = hub.store.job(job.clone()).await?.and_then(|j| j.steps.into_iter().find(|s| s.idx == 0)).ok_or_else(|| anyhow::anyhow!("missing step"))?;
            assert_eq!((step.status, step.exit_code), (Status::Failed, Some(9)));
            assert!(step.started_at.is_some() && step.finished_at.is_some());
            assert!(!hub.runner_step(77, "box", &job, 0, Status::Running, None).await?, "late starts cannot regress a terminal step");
            let saved_job = job.clone();
            let after: (String, Option<String>, Option<String>, Option<String>) = hub.store.call(move |c| Ok(c.query_row("SELECT name, outcome, conclusion, outputs FROM steps WHERE job_id = ?1 AND idx = 0", [&saved_job], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?)).await?;
            assert_eq!(after, metadata, "old frames cannot invent names, outcome, conclusion or outputs");
            assert_eq!(hub.store.job(job.clone()).await?.map(|j| j.status), Some(Status::Running));
            assert_eq!(hub.store.job(dependent).await?.map(|j| j.status), Some(Status::Pending), "progress makes no dependency or lifecycle decisions");
            Ok(())
        })
    }

    #[test]
    fn u03_old_step_frames_cannot_bypass_native_attempt_or_missing_import_provenance() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (hub, job, _) = legacy_running_fixture().await?;
            assert!(crate::ci::legacy::import_run(&hub, "adoption-run").await?);
            let before = serde_json::to_value(hub.store.job(job.clone()).await?)?;
            hub.peers.lock().insert(78, "box".into());
            assert!(!hub.runner_step(78, "box", &job, 0, Status::Failed, Some(99)).await?, "same machine cannot replace the exact owning process");
            assert!(!crate::ci::runs::imported_step(&hub, "another-box", &job, 0, Status::Failed, Some(99)).await?, "the metadata transaction also checks durable assignment");
            let saved_job = job.clone();
            hub.store.call(move |c| { c.execute("UPDATE jobs SET attempt = attempt + 1 WHERE id = ?1", [&saved_job])?; Ok(()) }).await?;
            assert!(!hub.runner_step(77, "box", &job, 0, Status::Failed, Some(99)).await?, "stale imported tuple cannot authorize a different current attempt");
            let saved_job = job.clone();
            hub.store.call(move |c| { c.execute("UPDATE jobs SET attempt = attempt - 1 WHERE id = ?1", [&saved_job])?; Ok(()) }).await?;
            assert_eq!(serde_json::to_value(hub.store.job(job.clone()).await?)?, before);
            let saved_job = job.clone();
            hub.store.call(move |c| { c.execute("UPDATE jobs SET status = 'succeeded' WHERE id = ?1", [&saved_job])?; Ok(()) }).await?;
            assert!(!hub.runner_step(77, "box", &job, 0, Status::Failed, Some(99)).await?, "finished jobs cannot accept old progress");
            let saved_job = job.clone();
            hub.store.call(move |c| { c.execute("UPDATE jobs SET status = 'running' WHERE id = ?1", [&saved_job])?; Ok(()) }).await?;
            // Native claim invalidates old transport even if it reuses machine and attempt.
            crate::ci::runs::claim(&hub, &job, "box", "baremetal", 1).await?;
            let native = serde_json::to_value(hub.store.job(job.clone()).await?)?;
            assert!(!hub.runner_step(77, "box", &job, 0, Status::Failed, Some(99)).await?, "native attempt-capable frames cannot use old transport");
            assert_eq!(serde_json::to_value(hub.store.job(job.clone()).await?)?, native);
            // No backfill: an older saved input without provenance is safely ignored.
            hub.store.call(|c| {
                let saved: String = c.query_row("SELECT input FROM ci_plans WHERE run_id = 'adoption-run'", [], |r| r.get(0))?;
                let mut input: serde_json::Value = serde_json::from_str(&saved)?;
                if let Some(input) = input.as_object_mut() { input.remove("legacy_running"); }
                c.execute("UPDATE ci_plans SET input = ?1 WHERE run_id = 'adoption-run'", [input.to_string()])?;
                Ok(())
            }).await?;
            assert!(!hub.runner_step(77, "box", &job, 0, Status::Running, None).await?);
            assert_eq!(serde_json::to_value(hub.store.job(job.clone()).await?)?, native);
            assert!(hub.owns_job(77, &job));
            assert_eq!(hub.agents.lock().get("box").map(|a| a.held.len()), Some(1));
            Ok(())
        })
    }

    /// U01/U03: pause at a real negative read, atomically adopt, then resume the stale
    /// legacy continuations. No sleeps or injected storage faults choose the interleaving.
    #[test]
    fn u03_adoption_between_lookup_and_legacy_write_preserves_plan_and_execution() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (hub, job, dependent) = legacy_running_fixture().await?;
            // Both hello's inventory and completion classification were read before adoption.
            let stale_inventory = hub.store.running_jobs().await?;
            let canonical = hub.store.ci_legs(vec![job.clone(), dependent.clone()]).await?;
            assert!(canonical.is_empty());
            assert!(stale_inventory.iter().any(|j| j.id == job));
            assert!(crate::ci::legacy::import_run(&hub, "adoption-run").await?);
            let plan_before: (String, String, i64, i64) = hub.store.call(|c| Ok(c.query_row("SELECT workflow, input, done, (SELECT COUNT(*) FROM ci_plan_log WHERE run_id = p.run_id) FROM ci_plans p WHERE run_id = 'adoption-run'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?)).await?;
            let before = serde_json::to_value((hub.store.jobs("adoption-run".into(), true).await?, hub.store.run("adoption-run".into()).await?))?;
            let report = JobResult { job: job.clone(), status: Status::Succeeded, exit_code: Some(0), error: None, peak_rss: None, cpu_seconds: None };
            let refused = [
                hub.store.finish_job(job.clone(), Status::Succeeded, Some(0), None, None, None).await.err(),
                hub.store.requeue_job(job.clone(), "stale hello".into()).await.err(),
                hub.store.claim_job(dependent.clone(), "box".into(), "baremetal".into()).await.err(),
                hub.store.unclaim_job(job.clone(), "stale rejection".into()).await.err(),
                hub.store.step_update(job.clone(), 0, Status::Failed, Some(1)).await.err(),
                hub.store.finish_job_less_run("adoption-run".into(), Status::Failed).await.err(),
                hub.job_done("box", report.clone()).await.err(),
            ];
            assert!(refused.iter().all(|e| e.as_ref().is_some_and(|e| e.is::<crate::store::CanonicalJobMutation>())), "adoption refusal remains typed across Store::call");
            assert_eq!(serde_json::to_value((hub.store.jobs("adoption-run".into(), true).await?, hub.store.run("adoption-run".into()).await?))?, before, "no legacy state machine, step reset, or dependency advance may run after adoption");
            // Resume the exact completion continuation with its now-stale negative classification.
            assert!(!hub.finish_owned_completion(77, "box", &report, None, false).await?, "old message variant cannot bypass attempt fencing after adoption");
            assert!(!hub.finish_owned_completion(77, "box", &report, Some(2), false).await?, "wrong attempt cannot gain a receipt");
            assert!(crate::ci::runs::pending_completions(&hub).await?.is_empty());
            assert!(hub.finish_owned_completion(77, "box", &report, Some(1), false).await?, "the refused legacy finish reroutes through the durable canonical receipt");
            assert_eq!(crate::ci::runs::completion(&hub, &job, 1, "box").await?, Some(report));
            assert!(hub.owns_job(77, &job), "refusals and staged results do not disown the process");
            let runner = hub.agents.lock().get("box").cloned().ok_or_else(|| anyhow::anyhow!("missing runner"))?;
            assert_eq!(runner.busy, 1);
            assert_eq!(runner.held.len(), 1, "host resource accounting stays until canonical lifecycle proof");
            assert_eq!(serde_json::to_value((hub.store.jobs("adoption-run".into(), true).await?, hub.store.run("adoption-run".into()).await?))?, before);
            let plan_after: (String, String, i64, i64) = hub.store.call(|c| Ok(c.query_row("SELECT workflow, input, done, (SELECT COUNT(*) FROM ci_plan_log WHERE run_id = p.run_id) FROM ci_plans p WHERE run_id = 'adoption-run'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?)).await?;
            assert_eq!(plan_after, plan_before, "legacy writes cannot mutate or retire the canonical replay input");
            Ok(())
        })
    }

    /// U01/U03: disabled ingress never transfers durable workflow jobs to the old writer,
    /// including imported rows and hellos arriving before dispatcher recovery.
    #[test]
    fn u03_disabled_ingress_hello_and_completion_keep_durable_ownership() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = scratch_hub_with(|cfg| cfg.ci.enabled = false).await?;
            hub.store.call(|c| {
                c.execute("INSERT INTO repos (id, name, clone_url, created_at) VALUES ('disabled-repo', 'disabled/repo', '', 0)", [])?;
                c.execute("INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, status, created_at) VALUES ('disabled-run', 'disabled-repo', 1, 'ci', 'manual', 'main', 'sha', 'running', 0)", [])?;
                c.execute("INSERT INTO ci_plans (run_id, workflow, input) VALUES ('disabled-run', '{}', '{}')", [])?;
                for (id, job_id) in [("canonical-leg", Some("build")), ("imported-leg", None)] {
                    c.execute("INSERT INTO jobs (id, run_id, key, display, status, labels, needs, spec, agent, attempt, job_id) VALUES (?1, 'disabled-run', ?1, ?1, 'running', '[]', '[]', '{}', 'box', 1, ?2)", crate::sql::params![id, job_id])?;
                }
                Ok(())
            }).await?;
            let running = vec!["canonical-leg".to_owned(), "imported-leg".to_owned()];
            hub.pending_peers.lock().insert(77, "box".into());
            hub.on_hello(77, Hello { protocol: proto::VERSION, name: "box".into(), labels: vec![], slots: 2, hostname: "box".into(), os: "linux".into(), arch: "x86_64".into(), version: String::new(), ephemeral: false, dedicated_job: None, running: running.clone(), terminal: false }).await?;
            assert!(!hub.ci.enabled());
            assert!(hub.ci.restored().is_none(), "hello precedes recovery");
            if let Some(instance) = hub.instances.lock().get_mut(77) { instance.wide |= proto::CAP_JOB_ATTEMPT | proto::CAP_UNIFIED_LAYOUT; }
            for job in running {
                assert!(hub.owns_job(77, &job), "hello retains exact process ownership");
                let attempt = if job == "imported-leg" { 2 } else { 1 };
                let report = JobResult { job: job.clone(), status: Status::Succeeded, exit_code: Some(0), error: None, peak_rss: None, cpu_seconds: None };
                hub.runner_completion(77, "box", report.clone(), None).await?;
                assert!(crate::ci::runs::completion(&hub, &job, attempt, "box").await?.is_none(), "disabled ingress cannot bypass attempt fencing through JobFinished");
                hub.runner_completion(77, "box", report.clone(), Some(attempt + 1)).await?;
                assert!(crate::ci::runs::completion(&hub, &job, attempt, "box").await?.is_none(), "a different attempt cannot stage a receipt");
                hub.runner_completion(77, "box", report.clone(), Some(attempt)).await?;
                assert_eq!(crate::ci::runs::completion(&hub, &job, attempt, "box").await?, Some(report));
                assert_eq!(hub.store.job(job.clone()).await?.map(|j| j.status), Some(Status::Running), "only the dispatcher may apply the staged report");
            }
            Ok(())
        })
    }

    #[test]
    fn u03_superseded_process_cannot_report_another_process_job() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = scratch_hub().await?;
            let hello = Hello { protocol: proto::VERSION, name: "box".into(), labels: vec![], slots: 1, hostname: "box".into(), os: "linux".into(), arch: "x86_64".into(), version: String::new(), ephemeral: false, dedicated_job: None, running: vec!["leg".into()], terminal: false };
            for (peer, id, created_at) in [(1, "old", 1), (2, "new", 2)] {
                hub.peers.lock().insert(peer, "box".into());
                let mut instances = hub.instances.lock();
                instances.announce(peer, proto::InstanceHello { id: id.into(), created_at, pid: 1 });
                instances.join(peer, "box", hello.clone(), created_at);
            }
            let mut runner = live_runner("box", "linux", "x86_64");
            runner.running.push("leg".into());
            hub.agents.lock().insert("box".into(), runner);
            let job = crate::instances::Owned::Job("leg".into());
            hub.instances.lock().own(job.clone(), 2);
            assert!(!hub.owns_job(1, "leg"), "same machine does not grant ownership");
            assert!(hub.owns_job(2, "leg"));
            hub.instances.lock().own(job.clone(), 1);
            assert!(hub.owns_job(1, "leg"), "an older process retains its own running work");
            assert!(!hub.owns_job(2, "leg"), "newest cannot override an exact owner");
            hub.instances.lock().disown(&job);
            assert!(!hub.owns_job(1, "leg"), "recovery fallback excludes superseded processes");
            assert!(hub.owns_job(2, "leg"), "known work recovers on current process");
            assert!(!hub.owns_job(2, "unknown"), "being current does not grant unknown jobs");
            hub.peers.lock().insert(3, "other-box".into());
            let mut other = hello;
            other.name = "other-box".into();
            hub.instances.lock().join(3, "other-box", other, 3);
            hub.instances.lock().own(job, 3);
            hub.ci.keep("leg", Some(("box".into(), 0)));
            let event = |json: &str| crate::wire::payload::<proto::JobEvent>(crate::wire::unread::Wire::RunnerPayload, "JobEvent", json)
                .map(|event| event.event).map_err(|e| anyhow::anyhow!("{}", e.why));
            let release = event(r#"{"t":"kept_released","execution":"old-execution","attempt":1}"#)?;
            let legacy = event(r#"{"t":"kept","until":10}"#)?;
            let ordinary = event(r#"{"t":"failure_class","class":"timeout"}"#)?;
            assert!(!hub.owns_job(2, "leg"), "a retained host cannot report ordinary work for the replacement");
            assert!(hub.owns_retained_event(2, "box", &release), "prior host can report exact retained release after reassignment");
            assert!(!hub.owns_retained_event(1, "box", &release), "superseded process cannot use retained fallback");
            assert!(!hub.owns_retained_event(2, "box", &legacy), "legacy retention lacks execution identity");
            assert!(!hub.owns_retained_event(2, "box", &ordinary), "normal outputs do not inherit retained auth");
            Ok(())
        })
    }

    #[test]
    fn u03_stale_attempt_is_refused_and_current_completion_survives_lost_owner() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = scratch_hub().await?;
            hub.store.call(|c| {
                c.execute("INSERT INTO repos (id, name, clone_url, created_at) VALUES ('u03-repo', 'u03/repo', '', 0)", [])?;
                c.execute("INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, status, created_at) VALUES ('u03-run', 'u03-repo', 1, 'ci', 'manual', 'main', 'sha', 'running', 0)", [])?;
                c.execute("INSERT INTO jobs (id, run_id, key, display, status, labels, needs, spec, agent, attempt, job_id) VALUES ('u03-leg', 'u03-run', 'build', 'build', 'running', '[]', '[]', '{}', 'box', 2, 'build')", [])?;
                Ok(())
            }).await?;
            let report = JobResult { job: "u03-leg".into(), status: Status::Succeeded, exit_code: Some(0), error: None, peak_rss: None, cpu_seconds: None };
            assert!(crate::ci::runs::stage_completion(&hub, "box", &report, Some(1)).await?.is_none(), "same machine's old attempt cannot stage a result");
            assert!(crate::ci::runs::completion(&hub, "u03-leg", 2, "box").await?.is_none());
            assert!(crate::ci::runs::stage_completion(&hub, "another-box", &report, Some(2)).await?.is_none(), "the current attempt still requires its machine owner");
            assert_eq!(crate::ci::runs::stage_completion(&hub, "box", &report, Some(2)).await?, Some(("u03-leg".into(), 2)));
            let mut conflicting = report.clone();
            conflicting.status = Status::Failed;
            assert_eq!(crate::ci::runs::stage_completion(&hub, "box", &conflicting, Some(2)).await?, Some(("u03-leg".into(), 2)));
            assert_eq!(crate::ci::runs::completion(&hub, "u03-leg", 2, "box").await?, Some(report.clone()), "retransmission cannot overwrite the first durable completion");
            // A crash before the receipt reaches the runner loses in-memory ownership, but the
            // exact durable completion remains enough to acknowledge its retransmission.
            hub.peers.lock().insert(77, "box".into());
            assert!(!hub.owns_job(77, "u03-leg"));
            hub.runner_completion(77, "box", report.clone(), Some(2)).await?;
            assert_eq!(crate::ci::runs::completion(&hub, "u03-leg", 2, "box").await?, Some(report.clone()));
            hub.runner_completion(77, "box", report, Some(1)).await?;
            assert!(crate::ci::runs::completion(&hub, "u03-leg", 1, "box").await?.is_none(), "retransmission cannot turn a stale attempt into a receipt");
            Ok(())
        })
    }

    #[test]
    fn u17_only_marked_storage_failure_is_retryable_evidence() {
        let storage = evidence_storage(anyhow::anyhow!("Amphora temporarily unavailable")).context("session ownership");
        let policy = anyhow::anyhow!("session belongs to another machine");
        assert!(storage.is::<EvidenceStorageError>(), "storage classification survives context");
        assert!(!policy.is::<EvidenceStorageError>(), "policy refusal must remain final");
    }

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
            hub.peers.lock().insert(1, "fedora".into());
            hub.agents.lock().insert("fedora".into(), live_runner("fedora", "linux", "x86_64"));
            let frame = |data: Vec<u8>, lane: u32| NetEvent::Frame { peer: 1, lane, kind: Kind::Control, data:data.into(), connection: None, admission: crate::net::ApplicationPermit::fixture() };
            // A newer runner's variant, and a known one cut short.
            hub.on_net(frame(vec![0xc8, 0x01, 7], proto::LANE_CONTROL)).await?;
            let adopted = proto::encode(&AgentMsg::HarnessAdopted { session: "s1".into(), error: None })?;
            hub.on_net(frame(adopted.get(..2).unwrap_or_default().to_vec(), proto::LANE_CONTROL)).await?;
            let kept = hub.refusals.lock().kept(&Page::Runner("fedora".into())).cloned().unwrap_or_default();
            let counts: Vec<(W, C, u64)> = kept.counts.iter().map(|c| (c.wire, c.code, c.n)).collect();
            anyhow::ensure!(counts == vec![(W::Runner, C::Unknown, 1), (W::Runner, C::Malformed, 1)], "{counts:?}");
            let why = format!("AgentMsg has {} variants", crate::wire::agent_classes().len());
            anyhow::ensure!(kept.newest.front().is_some_and(|r| r.unread.what == "variant 200" && r.unread.why == why), "{:?}", kept.newest);
            let rows: i64 = hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM audit2 WHERE action = 'security.wire_refused' AND principal = 'runner:fedora'", [], |r| r.get(0))?)).await?;
            anyhow::ensure!(rows == 2, "{rows} rows");
            // Before its Hello, a peer's control frame is out of order; one on another lane is let
            // go as before (it may come before the Hello does).
            hub.pending_peers.lock().insert(2, "newbox".into());
            let facts = proto::encode(&AgentMsg::NetDrops { dropped: Vec::new() })?;
            hub.on_net(NetEvent::Frame { peer: 2, lane: proto::LANE_TERM, kind: Kind::Term, data: facts.clone().into(), connection: None, admission: crate::net::ApplicationPermit::fixture() }).await.ok();
            hub.on_net(NetEvent::Frame { peer: 2, lane: proto::LANE_CONTROL, kind: Kind::Control, data: facts.into(), connection: None, admission: crate::net::ApplicationPermit::fixture() }).await?;
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
            hub.agents.lock().insert("fedora".into(), live_runner("fedora", "linux", "x86_64"));
            let mut events = hub.events.subscribe();
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
            hub.harness.ingest(&hub, &there, "s1", open());
            let mut events = hub.events.subscribe();
            let from = crate::wire::unread::Sender::Runner { name: "box".into(), instance: "main".into() };
            hub.note_unread(unread(crate::wire::unread::Wire::RunnerPayload, from), Some(crate::wire::unread::Page::Session("s1".into()))).await;
            let told: serde_json::Value = serde_json::from_str(&crate::rt::timeout(Duration::from_secs(5), events.recv()).await??)?;
            anyhow::ensure!(told.pointer("/type") == Some(&serde_json::json!("harness")) && told.pointer("/info/refused/counts/0/wire") == Some(&serde_json::json!("runner_payload")), "{told}");
            hub.harness.ingest(&hub, &there, "s1", open());
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
        for urgent in [Command::Interrupt,Command::ForceStop] {
            let urgent = cmd(urgent);
            assert_eq!(stream_of(&urgent),Some(Stream::Live), "emergency away behavior is unchanged");
            assert!(preempts_ordinary(&urgent));
        }
        assert!(!preempts_ordinary(&cmd(send("m1"))), "ordinary messages do not retire another grant");
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
        hub.peers.lock().insert(peer, name.to_owned());
        hub.agents.lock().insert(name.to_owned(), live_runner(name, "linux", "x86_64"));
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
        hub.instances.lock().join(peer, name, hello, crate::util::now_unix());
        hub.instances.lock().set_build(peer, "test".into(), proto::CAP_REACH);
        hub.instances.lock().set_wide(peer, wide);
        hub.wire.set_keeps(name, wide & proto::CAP_WIRE != 0);
    }

    /// `msg`, from runner process `peer` on `lane`, as the net loop hears it.
    pub(crate) async fn hear(hub: &Arc<Hub>, peer: PeerId, lane: u32, msg: &AgentMsg) -> anyhow::Result<()> {
        hub.on_net(NetEvent::Frame { peer, lane, kind: Kind::Control, data: proto::encode(msg)?.into(), connection: None, admission: crate::net::ApplicationPermit::fixture() }).await
    }

    /// `msg`, item `seq` of runner process `peer`'s channel stream `stream`, as the hub hears it.
    pub(crate) async fn hear_item(hub: &Arc<Hub>, peer: PeerId, stream: &str, seq: u64, msg: &AgentMsg) -> anyhow::Result<()> {
        let agent = hub.agent_of(peer).ok_or_else(|| anyhow::anyhow!("no runner as {peer}"))?;
        let body = proto::encode(msg)?;
        hub.on_wire(peer, &agent, crate::wire::channel::Frame::Item { stream: stream.to_owned(), seq, low: seq.saturating_sub(1), body }, None).await;
        Ok(())
    }

    /// A question asked of a runner by `req`, its answer to be read as a `T`: what its asker gets.
    pub(crate) fn asking<T: serde::de::DeserializeOwned + Serialize + Send + 'static>(hub: &Hub, req: u64) -> futures::channel::oneshot::Receiver<Result<Option<T>, String>> {
        let (read, rx) = asked::<T>();
        hub.ai_requests.lock().insert(req, read);
        rx
    }

    /// A file's piece asked of a runner by `req`: what its asker gets.
    pub(crate) fn asking_file(hub: &Hub, req: u64) -> futures::channel::oneshot::Receiver<Result<(crate::harness::files::Answer, Vec<u8>), String>> {
        let (tx, rx) = futures::channel::oneshot::channel();
        hub.file_requests.lock().insert(req, tx);
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
        }
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
            crate::rt::blocking(Store::in_memory),
            crate::rt::blocking(move || -> anyhow::Result<_> {
                let pki = crate::pki::Pki::open_scratch(&pki_dir)?;
                let leaf = pki.server_leaf(&[])?;
                Ok((pki, leaf))
            })
        );
        let (store, (pki, leaf)) = (store??, keys??);
        let pki = Arc::new(pki);
        let (net, _events) = crate::net::start(crate::net::Spec { bind: "127.0.0.1:0".parse()?, ca_der: pki.ca_der.clone(), leaf, server: true, dial_deadline: crate::net::DIAL_DEADLINE, revoked_keys: Vec::new() })?;
        let logs = Logs::new(dir.join("logs"), 1 << 20);
        Hub::new_on(clock, Arc::new(cfg), store, logs, net, pki, "test-admin-token".into(), "test-join-token".into(), Shutdown::new())
    }

    /// CP-T3.2: an inbox's answer from a runner that no longer holds the session (a copy left
    /// behind there) counts for whoever waits for it, and leaves the session where it runs: its
    /// machine and owner as they were, so its messages still go there.
    #[test]
    fn an_inbox_answer_from_another_machine_leaves_the_session_where_it_runs() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = scratch_hub().await?;
            hub.peers.lock().insert(1, "box-a".into());
            hub.peers.lock().insert(2, "box-b".into());
            let say = |u: &crate::harness::core::Update| serde_json::to_string(u).map(|json| AgentMsg::Harness { session: "s1".into(), json });
            let spec = crate::harness::session::Spec { kind: "claude".into(), cwd: Some("/w".into()), ..Default::default() };
            let open = crate::harness::host::refusal("s1", &spec, "").into_iter().find(|u| matches!(u, crate::harness::core::Update::Open { .. })).ok_or_else(|| anyhow::anyhow!("no announcement"))?;
            hub.on_runner_msg(1, say(&open)?).await?;
            anyhow::ensure!(hub.harness.runs_on("s1", "box-a"), "it runs on box-a");
            let refused = crate::harness::inbox::Said { message: "m1".into(), answer: Err(crate::harness::inbox::Refusal::Moved { session: "s1".into(), epoch: 0 }) };
            hub.on_runner_msg(2, say(&crate::harness::core::Update::Inbox { said: Box::new(refused) })?).await?;
            assert!(hub.harness.runs_on("s1", "box-a"), "it still runs on box-a");
            assert_eq!(hub.instances.lock().owner(&crate::instances::Owned::Agent("s1".into())), Some(1), "box-a's runner still holds it");
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
            hub.peers.lock().insert(1, "box-a".into());
            hub.peers.lock().insert(2, "box-b".into());
            let now = Instant::now();
            for peer in [1, 2] {
                hub.cmds.lock().coming(peer, now);
            }
            let mut old = hub.cmds.lock().push(1, Kind::Parked, b"close s1".to_vec(), now);
            let mut new = hub.cmds.lock().push(2, Kind::Parked, b"close s2".to_vec(), now);
            // Heard long after its hello, a runner that keeps the channel still has time to say so.
            anyhow::ensure!(hub.cmds.lock().overdue(now + Duration::from_secs(10)).is_empty(), "taken for old on the clock");
            // box-a says its upgrade preference before any Caps: from before the channel.
            hub.on_runner_msg(1, AgentMsg::UpdatePrefs { accept: true }).await?;
            anyhow::ensure!(!hub.cmds.lock().is_coming(1), "still waiting for box-a's Caps");
            anyhow::ensure!(matches!(old.try_recv(), Ok(Some(Answer::Sent | Answer::Unreachable { .. }))), "what waited for box-a didn't go its way");
            // box-b says its Caps: what waited for it goes on the channel.
            hub.on_runner_msg(2, AgentMsg::Caps { caps: proto::CAP_WIRE }).await?;
            anyhow::ensure!(!hub.cmds.lock().is_coming(2) && hub.cmds.lock().keeps(2), "box-b doesn't keep the channel");
            anyhow::ensure!(matches!(new.try_recv(), Ok(None)), "box-b's command was answered before it went");
            anyhow::ensure!(hub.cmds.lock().overdue(now + COMING_FOR).is_empty(), "box-b was taken for old after all");
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

    /// The scheduler's headroom (W05/U01): one rule places a CI leg and a reach command
    /// alike. On one fleet (a at 80% of its CPUs, b at 30%, c at 10% with its disk under pressure),
    /// asking 4 CPUs and 2 GiB: a would pass the target use, c's disk counts against it, and the
    /// unified engine and kiln run's ranking both pick b.
    #[test]
    fn one_headroom_rule_places_a_leg_and_a_command_alike() -> anyhow::Result<()> {
        use kiln_ci::sched::routing::{LegRoute, RunnerView};
        use kiln_ci::sched::state::{Command, Config, Input, LegIn, Resources, RunnerIn, Sample, Scheduler};
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
        assert_eq!((by_engine.as_deref(), by_run.as_deref()), (Some("b"), Some("b")), "{c:?} {ranked:?}");
        Ok(())
    }
}
