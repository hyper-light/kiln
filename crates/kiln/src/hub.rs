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

use crate::rt::{Broadcast, Interval, Notify, Shutdown};

use crate::config::Config;
use crate::logs::Logs;
use crate::metrics::{self, HostSample};
use crate::net::{Kind, NetEvent, NetHandle};
use crate::pipeline::Context;
use crate::proto::{self, AgentMsg, Assignment, Hello, JobResult, LANE_CONTROL, LANE_TERM, ServerMsg};
use crate::store::{AgentRow, Job, Status, Store};
use crate::wire::events::ServerEvent;

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
/// How soon a refused pod launch is tried again.
const LAUNCH_RETRY: Duration = Duration::from_secs(15);

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

/// Which of a runner process's command streams `msg` rides (docs/wire.md, "Commands"): live for
/// a command refused while its runner is away, parked for one that waits for it. A `HarnessCmd`
/// rides as the command it carries says. None: not a command.
fn stream_of(msg: &ServerMsg) -> Option<crate::wire::commands::Kind> {
    use crate::wire::commands::Kind as Stream;
    use crate::wire::{Away, Class};
    let class = match (crate::wire::server_class(msg), msg) {
        (Class::Command(Away::AsItsCommand), ServerMsg::HarnessCmd { json, .. }) => {
            serde_json::from_str::<crate::harness::model::Command>(json).map_or(Class::Command(Away::Park), |c| crate::wire::command_class(&c))
        }
        (c, _) => c,
    };
    match class {
        Class::Command(Away::Refuse) => Some(Stream::Live),
        Class::Command(Away::Park | Away::AsItsCommand) | Class::State => Some(Stream::Parked),
        _ => None,
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
    /// The records kiln keeps through the Records seam (docs/seams.md): kiln's own, in its store,
    /// until Amphora's (CP-T1.5) take their place. Deploying by API's are there.
    pub records: Arc<dyn kiln_seams::Records>,
    pub logs: Logs,
    pub net: NetHandle,
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
    /// Transcript requests waiting for their machine's answer, by request number.
    ai_requests: Mutex<HashMap<u64, futures::channel::oneshot::Sender<Option<String>>>>,
    ai_seq: AtomicU64,
    /// File reads (`crate::harness::files`) waiting for their machine's answer: its JSON and bytes.
    file_requests: Mutex<HashMap<u64, futures::channel::oneshot::Sender<(String, Vec<u8>)>>>,
    pub history: Mutex<HashMap<String, VecDeque<HostSample>>>,
    pub wake: Notify,
    pub launching: Mutex<HashMap<String, Launch>>,
    pub started: Instant,
    pub generation: u64,
    /// Runners connected just before this server started ([`crate::store::Store::runners_before`]).
    pub runners_before: AtomicU64,
    pub admin_token: String,
    pub join_token: String,
    /// One-time enrollment tokens for pod agents: token → (job id, expiry).
    pub ephemeral_tokens: Mutex<HashMap<String, (String, Instant)>>,
    /// One-time enrollment tokens for a `k8s` pool's executor pods (docs/ci.md W13-R07): token →
    /// (executor, the pod's name, which the runner must take, expiry).
    pub executor_tokens: Mutex<HashMap<String, (String, String, Instant)>>,
    /// The executor each executor pod's runner is, by the name it enrolled with.
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
    /// The runners' learners of the cluster group (docs/p2p.md, P2).
    pub cluster: crate::store::learner::Learners,
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
        let blobs = crate::blobs::open(&cfg, &store, Arc::clone(&clock))?;
        let generation =
            std::env::var("KILN_GENERATION").ok().and_then(|g| g.parse().ok()).unwrap_or(1);
        let discovery = Arc::new(crate::discovery::Discovery::for_server(&cfg, &pki.ca_der));
        let harness_dir = cfg.data("harness");
        // The bus every session's live feed rides (docs/bus.md): Quiver's node, in this process.
        let bus = crate::bus::QuiverBus::open(&cfg.bus, Arc::clone(&clock))?;
        // The search across a change (W26-R07): an index per tree in the server's cache.
        let search: Arc<dyn kiln_seams::Search> = Arc::new(kiln_search::VorpalSearch::open(cfg.data("cache").join("search"), kiln_search::Limits::default()).map_err(anyhow::Error::msg)?);
        let harness = crate::harness::registry::Registry::with(harness_dir, &cfg.conversations, Arc::clone(&clock), Arc::new(bus.clone()));
        let contents = crate::sync::contents::Contents::start(&blobs, cfg.data("sync").join("blobs"));
        let sync = crate::sync::server::Sync::start(cfg.data("sync"), admin_token.clone(), contents);
        let places = crate::sync::places::Places::open(cfg.data("sync").join("places.json"));
        let tasks = crate::sync::tasks::Tasks::open(&cfg.data("sync"));
        let copies = crate::reach::copies::Copies::open(cfg.data("reach").join("copies.json"));
        let records: Arc<dyn kiln_seams::Records> = Arc::new(crate::records::StoreRecords::open(store.clone())?);
        Ok(Arc::new(Self {
            cfg,
            store,
            records,
            logs,
            net,
            pki,
            agents: Mutex::new(HashMap::new()),
            ci: crate::ci::dispatch::Handle::default(),
            peers: Mutex::new(HashMap::new()),
            pending_peers: Mutex::new(HashMap::new()),
            tunnels: Arc::default(),
            events: Broadcast::new(4096),
            terms: Mutex::new(HashMap::new()),
            terminals: crate::termsess::Sessions::new(),
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
            launching: Mutex::new(HashMap::new()),
            started: Instant::now(),
            generation,
            runners_before: AtomicU64::new(0),
            admin_token,
            join_token,
            ephemeral_tokens: Mutex::new(HashMap::new()),
            executor_tokens: Mutex::new(HashMap::new()),
            pod_executors: Mutex::new(HashMap::new()),
            outpost_invites: OutpostInvites::default(),
            k8s: std::sync::OnceLock::new(),
            gitops_kick: Notify::new(),
            gitops_wanted: Mutex::new(HashSet::new()),
            tool_fixes: Mutex::new(HashMap::new()),
            shutdown,
            refusals: Mutex::new(crate::wire::unread::Refusals::new(Arc::clone(&clock))),
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
            peered: crate::peers::Peers::default(),
            tailnet: crate::frontdoor::tailnet::TailnetDns::default(),
            sync,
            reach: Mutex::new(crate::reach::relay::Relay::default()),
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

    /// Sends the cluster group's messages to the runners' learners (the newest process of each
    /// machine that holds one), on the sync lane; a learner whose runner is away misses them.
    pub async fn cluster_loop(self: Arc<Self>) -> anyhow::Result<()> {
        let Some(out) = self.store.group().map(crate::store::group::Group::outgoing) else { return Ok(()) };
        while let Ok((member, bytes)) = out.recv().await {
            let peer = self.cluster.machine(member).and_then(|m| self.instances.lock().newest_with(&m, proto::CAP_CLUSTER));
            let Some(peer) = peer else {
                if let Some(g) = self.store.group() {
                    g.unreachable(member);
                }
                continue;
            };
            for (msg, raft) in self.cluster.frames(bytes) {
                if let Ok(b) = proto::encode(&ServerMsg::Cluster { json: crate::store::learner::encode(&msg), raft }) {
                    self.net.send(peer, proto::LANE_SYNC, Kind::Control, b).await;
                }
            }
        }
        Ok(())
    }

    /// A runner process of `machine` that can hold a learner said so, or its newest process changed:
    /// the newest such process holds the machine's (it's told its member and the token its copies
    /// are fetched with), and every other one lets its go, so one process at a time has the
    /// replica's files.
    async fn cluster_announced(&self, machine: &str) {
        // The server's own runner, in its process, reads kiln.db as it is.
        let own = self.cfg.local_agent.enabled && machine == crate::pki::sanitize_name(&self.cfg.local_agent_name());
        let group = self.store.group().filter(|_| self.cfg.cluster.learners && !own);
        let (newest, mut others) = self.instances.lock().holder_of(machine, proto::CAP_CLUSTER);
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
        // One lock at a time: the machines first, then their runner processes.
        let machines: Vec<(String, u64)> = self
            .agents
            .lock()
            .values()
            .filter(|a| a.connected && !a.ephemeral && !a.labels.iter().any(|l| l == proto::OUTPOST_LABEL))
            .map(|a| (a.name.clone(), a.sample.as_ref().map_or(0, |s| s.disk_total.saturating_sub(s.disk_used))))
            .collect();
        let candidates: Vec<crate::convo::mirror::Candidate> = {
            let inst = self.instances.lock();
            machines
                .into_iter()
                .filter_map(|(machine, disk_free)| {
                    let peer = inst.newest_with(&machine, proto::CAP_CONVO)?;
                    let connected_at = inst.get(peer).map_or(0, |i| i.connected_at);
                    Some(crate::convo::mirror::Candidate { machine, peer, connected_at, disk_free })
                })
                .collect()
        };
        m.connected(candidates);
    }

    /// What a machine's runner said on the sync lane: of its copies (to the groups), of the git
    /// repositories it holds, or of its part of a task.
    fn sync_heard(&self, machine: &str, json: &str, raft: Vec<u8>) {
        match crate::sync::wire::decode(json) {
            Some(crate::sync::wire::Msg::Repos(r)) => {
                if self.places.heard(machine, r) {
                    self.emit(&ServerEvent::Places);
                }
                // Said when it connects, and when what it holds changes: what it shares.
                self.shares.arrange(self);
            }
            Some(crate::sync::wire::Msg::Shared(w)) => self.shares.shared(self, machine, *w),
            Some(crate::sync::wire::Msg::Task(news)) => self.tasks.heard(machine, news),
            Some(msg) => self.sync.from_runner(machine, msg, raft),
            None => {}
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
            self.events.send(Arc::from(e.text()));
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

    pub(crate) fn emit_agent(&self, a: &AgentLive) {
        self.emit(&ServerEvent::Agent { agent: Box::new(a.clone()) });
    }

    /// A message a wire couldn't read (`crate::wire::unread`): counted, kept for its sender's page
    /// and `on`'s, audited, and each page it changed told. Some: tell its sender now, of how many
    /// before it it wasn't told; None: not now.
    pub async fn note_unread(&self, u: crate::wire::unread::Unread, on: Option<crate::wire::unread::Page>) -> Option<u64> {
        let noted = self.refusals.lock().note(u, on);
        self.audit_unread(noted.rows).await;
        for page in &noted.changed {
            self.emit_refused(page);
        }
        noted.tell
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
        let kept = self.refusals.lock().kept(page).cloned();
        match page {
            crate::wire::unread::Page::Runner(name) => {
                let a = self.agents.lock().get(name).cloned();
                if let Some(mut a) = a {
                    a.refused = kept;
                    self.emit_agent(&a);
                }
            }
            crate::wire::unread::Page::Session(id) => self.harness.set_refused(self, id, kept),
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

    /// The recent messages of the AI session running as `pid` on a machine, as JSON (`None`: it
    /// has no transcript).
    pub async fn ai_transcript(&self, machine: &str, pid: u32) -> anyhow::Result<Option<String>> {
        self.ai_ask(machine, |req| ServerMsg::AiTranscript { req, pid }).await
    }

    /// The normalized view (events, state, changeset) of an AI session running on a machine.
    pub async fn ai_view(&self, machine: &str, pid: u32) -> anyhow::Result<Option<String>> {
        self.ai_ask(machine, |req| ServerMsg::AiView { req, pid }).await
    }

    /// The output of a task of an agent session running on a machine (JSON).
    pub async fn harness_task_output(&self, machine: &str, session: &str, task: &str, since: Option<u64>) -> anyhow::Result<Option<String>> {
        let (session, task) = (session.to_owned(), task.to_owned());
        self.ai_ask(machine, move |req| ServerMsg::HarnessTaskOutput { req, session, task, since }).await
    }

    /// The folders at `path` on a machine, as JSON [`crate::dirs::Listing`], read by its newest
    /// runner process. Only one that announced [`proto::CAP_DIRS`] is sent [`ServerMsg::DirList`]:
    /// `Ok(None)` when it is too old to list folders.
    pub async fn dir_list(&self, machine: &str, path: &str) -> anyhow::Result<Option<String>> {
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
        let (tx, rx) = futures::channel::oneshot::channel();
        self.ai_requests.lock().insert(req, tx);
        let sent = self.net.send(peer, LANE_TERM, Kind::Term, frame).await;
        let answer = if sent { crate::rt::timeout(crate::dirs::WAIT, rx).await.ok() } else { None };
        self.ai_requests.lock().remove(&req);
        match answer {
            Some(Ok(Some(json))) => Ok(Some(json)),
            Some(_) => anyhow::bail!("{machine} did not answer"),
            None if !sent => anyhow::bail!("{machine} is not connected"),
            None => anyhow::bail!("{machine} did not answer in time"),
        }
    }

    /// A piece of a file on a machine, for whoever views agent session `session` there
    /// (`crate::harness::files`): its answer's JSON and the bytes. Asked of the runner process
    /// holding the session; `Ok(None)` when that one doesn't read files ([`proto::CAP_FILES`]).
    pub async fn harness_file_read(&self, machine: &str, session: &str, path: &str, offset: u64, len: u32) -> anyhow::Result<Option<(String, Vec<u8>)>> {
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
            Some(Ok(a)) => Ok(Some(a)),
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
    /// [`AgentMsg::Answer`]. `Ok(None)`: that process doesn't know `cap` (its kiln is too old).
    pub async fn ask_routed(&self, machine: &str, cap: u32, wait: Duration, msg: impl FnOnce(u64) -> ServerMsg) -> anyhow::Result<Option<Option<String>>> {
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
        let (tx, rx) = futures::channel::oneshot::channel();
        self.ai_requests.lock().insert(req, tx);
        let sent = self.net.send(peer, LANE_TERM, Kind::Term, frame).await;
        let answer = if sent { crate::rt::timeout(wait, rx).await.ok() } else { None };
        self.ai_requests.lock().remove(&req);
        match answer {
            Some(Ok(json)) => Ok(Some(json)),
            Some(Err(_)) => anyhow::bail!("{machine} did not answer"),
            None if !sent => anyhow::bail!("{machine} is not connected"),
            None => anyhow::bail!("{machine} did not answer in time"),
        }
    }

    /// Ask machine `machine` something by its newest runner process announcing `cap` (`msg` given
    /// the question's number), and wait up to `wait` for its [`AgentMsg::Answer`]. `Ok(None)`: no
    /// process of it announces `cap` (its kiln is too old); `Ok(Some(json))`: what it answered.
    pub async fn ask_runner(&self, machine: &str, cap: u32, wait: Duration, msg: impl FnOnce(u64) -> ServerMsg) -> anyhow::Result<Option<Option<String>>> {
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
        let (tx, rx) = futures::channel::oneshot::channel();
        self.ai_requests.lock().insert(req, tx);
        let sent = self.net.send(peer, LANE_TERM, Kind::Term, frame).await;
        let answer = if sent { crate::rt::timeout(wait, rx).await.ok() } else { None };
        self.ai_requests.lock().remove(&req);
        match answer {
            Some(Ok(json)) => Ok(Some(json)),
            Some(Err(_)) => anyhow::bail!("{machine} did not answer"),
            None if !sent => anyhow::bail!("{machine} is not connected"),
            None => anyhow::bail!("{machine} did not answer in time"),
        }
    }

    /// A machine's past sessions (JSON `[PastSession]`).
    pub async fn harness_history(&self, machine: &str, kind: Option<String>, limit: u32) -> anyhow::Result<Option<String>> {
        self.ai_ask(machine, move |req| ServerMsg::HarnessHistory { req, kind, limit }).await
    }

    async fn ai_ask(&self, machine: &str, msg: impl FnOnce(u64) -> ServerMsg) -> anyhow::Result<Option<String>> {
        let req = self.ai_seq.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = futures::channel::oneshot::channel();
        self.ai_requests.lock().insert(req, tx);
        if !self.send_to_agent(machine, LANE_TERM, &msg(req)).await {
            self.ai_requests.lock().remove(&req);
            anyhow::bail!("machine {machine} is not connected");
        }
        let answer = crate::rt::timeout(Duration::from_secs(10), rx).await;
        self.ai_requests.lock().remove(&req);
        match answer {
            Ok(Ok(json)) => Ok(json),
            Ok(Err(_)) => anyhow::bail!("{machine} did not answer"),
            Err(_) => anyhow::bail!("{machine} did not answer in time"),
        }
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
                    let mut rx = self.cmds.lock().push(p, kind, body.clone(), Instant::now());
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

    /// The runners' commands due now: those not sent yet on their link, and those whose answer is
    /// overdue (lost below the channel).
    async fn flush_commands(&self) {
        let frames = self.cmds.lock().poll(Instant::now());
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

    pub(crate) async fn send_peer(&self, peer: PeerId, msg: &ServerMsg) {
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
        self.places.forget(agent);
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
        while let Ok(ev) = rx.recv().await {
            if let Err(e) = self.on_net(ev).await {
                tracing::warn!(error = %format!("{e:#}"), "agent message handling failed");
            }
        }
        anyhow::bail!("transport event stream ended")
    }

    async fn on_net(self: &Arc<Self>, ev: NetEvent) -> anyhow::Result<()> {
        match ev {
            NetEvent::Connected { peer, name, binding } => {
                self.tunnels.connected(peer, binding);
                if let Some(n) = name {
                    self.pending_peers.lock().insert(peer, n);
                }
            }
            NetEvent::Closed { peer, reason } | NetEvent::Unreachable { peer, reason } => {
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
                if let (Some(name), Some(m)) = (&name, &self.harness.copies) {
                    m.disconnected(name, peer);
                }
                if let Some(name) = name {
                    // Sync moves to another runner process of the machine that syncs, if any.
                    let syncing = self.instances.lock().newest_with(&name, proto::CAP_SYNC).is_some();
                    self.sync.machine(&name, syncing);
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
            NetEvent::Frame { peer, kind: Kind::Log, lane, data } if lane != proto::LANE_CONVO && lane != proto::LANE_FILES => {
                let chunk: proto::LogChunk = proto::decode(&data)?;
                if self.owns_job(peer, &chunk.job) {
                    self.logs.append(&chunk.job, chunk.idx, &chunk.data);
                }
            }
            NetEvent::Frame { peer, data, .. } => {
                let msg: AgentMsg = proto::decode(&data)?;
                self.on_agent_msg(peer, msg).await?;
            }
        }
        Ok(())
    }

    /// [`AgentMsg::Power`]: a machine's battery. Kept with the machine (the Runners page shows it);
    /// a machine on battery that gets low is said once, as it gets there, to whoever is watching
    /// (the UI) and to webhook subscribers (`machine.battery_low`), before it sleeps and its runner
    /// with it.
    async fn on_power(&self, agent: &str, json: &str) {
        let Ok(b) = serde_json::from_str::<crate::os::battery::Battery>(json) else { return };
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

    /// Whether `job` is one of the CI engine's legs: one it knows, or, while it rebuilds its runs
    /// after a restart (knowing none yet), one the store has (its message waits for it then).
    async fn ci_leg(&self, job: &str) -> bool {
        self.ci.is_leg(job) || (self.ci.enabled() && self.ci.restored().is_none() && self.store.ci_legs(vec![job.to_owned()]).await.is_ok_and(|l| l.contains(job)))
    }

    fn owns_job(&self, peer: PeerId, job: &str) -> bool {
        let Some(name) = self.agent_of(peer) else { return false };
        // A failed leg it keeps: a step rerun in place reports as the leg (W17-R04).
        self.agents.lock().get(&name).is_some_and(|a| a.running.iter().any(|j| j == job)) || self.ci.kept_by(job, &name)
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
        let jobs: Vec<String> = {
            let mut g = self.agents.lock();
            match g.get_mut(name) {
                Some(a) => {
                    a.busy = 0;
                    a.exclusive = false;
                    a.held.clear();
                    std::mem::take(&mut a.running)
                }
                None => vec![],
            }
        };
        if jobs.iter().any(|j| self.ci.is_leg(j)) {
            self.ci.send(crate::ci::dispatch::Msg::RunnerLost { agent: name.to_owned() });
        }
        for j in jobs.into_iter().filter(|j| !self.ci.is_leg(j)) {
            match self.store.requeue_job(j.clone(), format!("{reason} ({name})")).await {
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
    /// then answered: taken, or refused saying why. Never answered before it's applied.
    async fn on_wire(self: &Arc<Self>, peer: PeerId, agent: &str, frame: crate::wire::channel::Frame) {
        use crate::wire::channel::{Arrival, Frame, Refusal};
        match frame {
            Frame::Item { stream, seq, low, body } => {
                let answer = match self.wire.on_item(agent, &stream, seq, low, body) {
                    Arrival::Take(body) => {
                        let refusal = match proto::decode::<AgentMsg>(&body) {
                            Ok(AgentMsg::Wire(_)) => Some((Refusal::Malformed, "a channel frame inside a channel item".to_owned())),
                            Ok(inner) => Box::pin(self.on_agent_msg(peer, inner)).await.err().map(|e| (Refusal::Refused, format!("{e:#}"))),
                            Err(e) => Some((Refusal::Malformed, format!("{e:#}"))),
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
                self.cmds.lock().on_frame(peer, frame);
                self.flush_commands().await;
            }
        }
    }

    async fn on_agent_msg(self: &Arc<Self>, peer: PeerId, msg: AgentMsg) -> anyhow::Result<()> {
        if let AgentMsg::Hello(h) = msg {
            return self.on_hello(peer, h).await;
        }
        // Says which runner process of its machine this is; precedes Hello. Again later: a new
        // build that has taken over (blue/green) says it's the newest now.
        if let AgentMsg::Instance(ih) = msg {
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
        if matches!(msg, AgentMsg::Facts { .. } | AgentMsg::UpdatePrefs { .. }) && self.cmds.lock().is_coming(peer) {
            let waited = self.cmds.lock().not_wire(peer);
            self.send_waited(peer, waited).await;
        }
        match msg {
            AgentMsg::Hello(_) | AgentMsg::Instance(_) => {}
            AgentMsg::Heartbeat { .. } | AgentMsg::JobStarted { .. } => {}
            AgentMsg::Sample(s) => {
                if let Some(a) = self.agents.lock().get_mut(&agent) {
                    a.sample = Some(s.clone());
                }
                self.record_sample(&agent, s);
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
                        self.ci.send(crate::ci::dispatch::Msg::Finished { agent: agent.clone(), result: r });
                    } else {
                        self.job_done(&agent, r).await?;
                    }
                }
                // Heard (taken, or not this runner's to give any more): it needn't send it again.
                if self.instances.lock().get(peer).is_some_and(|i| i.caps & proto::CAP_RECEIVED != 0) {
                    self.send_peer(peer, &ServerMsg::Received { job }).await;
                }
            }
            AgentMsg::Reject { job, .. } if self.ci.is_leg(&job) => {
                if self.owns_job(peer, &job) {
                    self.ci.send(crate::ci::dispatch::Msg::Rejected { agent: agent.clone(), leg: job });
                }
            }
            AgentMsg::Reject { job, reason } => {
                if self.owns_job(peer, &job) {
                    self.release_slot(&agent, &job);
                    self.declined.lock().insert((job.clone(), agent.clone()), Instant::now());
                    self.store.unclaim_job(job.clone(), format!("declined by {agent}: {reason}")).await?;
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
            // One report per machine: its newest runner process's.
            AgentMsg::AiSessions { .. } if self.instances.lock().newest(&agent).is_some_and(|n| n != peer) => {}
            AgentMsg::AiSessions { json } => match serde_json::from_str::<Vec<crate::aisessions::AiSession>>(&json) {
                Ok(sessions) => {
                    let old = self.ai_sessions.lock().insert(agent.clone(), sessions.clone());
                    if old.as_ref() != Some(&sessions) {
                        self.emit(&ServerEvent::AiSessions { machine: agent.to_owned() });
                    }
                }
                Err(e) => tracing::debug!(agent = %agent, error = %e, "unreadable AI session report"),
            },
            AgentMsg::AiTranscript { req, json } => {
                if let Some(tx) = self.ai_requests.lock().remove(&req) {
                    let _ = tx.send(json);
                }
            }
            AgentMsg::AiView { req, json } | AgentMsg::HarnessTaskOutput { req, json } | AgentMsg::HarnessHistory { req, json } | AgentMsg::Answer { req, json } => {
                if let Some(tx) = self.ai_requests.lock().remove(&req) {
                    let _ = tx.send(json);
                }
            }
            AgentMsg::DirList { req, json } => {
                if let Some(tx) = self.ai_requests.lock().remove(&req) {
                    let _ = tx.send(Some(json));
                }
            }
            AgentMsg::HarnessFileData { req, json, data } => {
                if let Some(tx) = self.file_requests.lock().remove(&req) {
                    let _ = tx.send((json, data));
                }
            }
            // A streamed page's chunks and events (`crate::harness::streams`): to its viewers.
            AgentMsg::StreamFrame { stream, data } => self.harness.streams.chunk(&agent, &stream, data),
            AgentMsg::StreamEvent { stream, json } => {
                let after = self.harness.streams.event(&agent, &stream, json);
                // A recording that ended: its files kept in kiln's store of bytes.
                if let Some((session, rec)) = after.ended {
                    self.harness.streams.keep(self, session, rec);
                }
                // Opened: the size its viewer asked for while it was being set up, asked again.
                if let Some((live, again)) = after.again {
                    live.control(self, &again).await;
                }
            }
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
                if caps & proto::CAP_CLUSTER != 0 {
                    self.cluster_announced(&agent).await;
                }
                if caps & proto::CAP_CONVO != 0 {
                    self.mirrors_seen();
                    self.harness.restore_due(self, &agent);
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
            AgentMsg::Sync { json, raft } => self.sync_heard(&agent, &json, raft),
            AgentMsg::Cluster { json, raft } => match (self.store.group(), crate::store::learner::decode(&json)) {
                // Where kiln.db stands, for a read the runner answers once its replica holds it.
                (Some(g), Some(crate::store::learner::Msg::Ask { id })) => {
                    let at = crate::store::learner::Msg::At { id, seq: g.standing().0 };
                    if let Ok(b) = proto::encode(&ServerMsg::Cluster { json: crate::store::learner::encode(&at), raft: Vec::new() }) {
                        self.net.send(peer, proto::LANE_SYNC, Kind::Control, b).await;
                    }
                }
                (Some(g), Some(msg)) => self.cluster.heard(g, &agent, &msg, raft),
                _ => {}
            },
            AgentMsg::Convo { json, data } => {
                // About the server's own store: a mirror's answer. A mirror's copy sent back to
                // restore it from, when asked for. Else the runner's own store's feed.
                let msg = crate::convo::feed::decode(&json);
                match (&self.harness.copies, msg) {
                    // What a runner's store is doing, for the board.
                    (_, Some(crate::convo::feed::Msg::Stats { store, report })) => self.harness.board.heard(&agent, &store, *report, crate::util::now_ms()),
                    (_, Some(crate::convo::feed::Msg::Hello { store, restore: true })) => {
                        if self.harness.restore_accepts(&agent, &store)
                            && let Some(f) = &self.harness.feed
                        {
                            f.accept_restore(&store);
                            f.heard(peer, &agent, json, data);
                        }
                    }
                    (_, Some(crate::convo::feed::Msg::Restored { store, .. })) => self.harness.restore_done(self, &agent, &store),
                    (Some(m), Some(msg)) if msg.store() == m.store_id() => m.heard(&agent, msg),
                    _ => {
                        if let Some(f) = &self.harness.feed {
                            f.heard(peer, &agent, json, data);
                        }
                    }
                }
            }
            AgentMsg::FrontDoorStatus { json } => crate::frontdoor::control::on_status(self, peer, &agent, &json).await,
            // CI (docs/ci.md §2.5): the dispatcher's; facts are W13's.
            AgentMsg::JobEvent { job, json } => {
                if self.owns_job(peer, &job) {
                    self.ci.send(crate::ci::dispatch::Msg::Event { agent: agent.clone(), leg: job, json });
                }
            }
            // A pool's executor it was asked to start (docs/ci.md W13-R09).
            AgentMsg::Executor { json } => self.ci.send(crate::ci::dispatch::Msg::Executor { agent: agent.clone(), json }),
            // A preview's deployment it was asked to take down (docs/ci.md W15-R07).
            AgentMsg::Undeployed { json } => crate::ci::previews::undeployed(self, &agent, &json).await,
            // What it can do past Build's 32 bits. One that keeps the channel too is told what the
            // server has of its streams, and sends from there (docs/wire.md).
            AgentMsg::Caps { caps } => {
                self.instances.lock().set_wide(peer, caps);
                // One that takes hand-overs: its machine's older processes hand their sessions on.
                if caps & proto::CAP_HANDOVER != 0 {
                    self.check_handover().await;
                }
                self.wire.set_keeps(&agent, caps & proto::CAP_WIRE != 0);
                if caps & proto::CAP_WIRE != 0 {
                    self.cmds.lock().connected(peer);
                    self.send_peer(peer, &ServerMsg::Wire(self.wire.have(&agent))).await;
                } else {
                    let waited = self.cmds.lock().not_wire(peer);
                    self.send_waited(peer, waited).await;
                }
                // One that takes its agent limit from here is sent what's set for its machine.
                crate::harness::limit::on_caps(self, &agent, peer, caps).await;
            }
            AgentMsg::AgentLimit { json } => crate::harness::limit::on_held(self, peer, &agent, &json),
            AgentMsg::Wire(frame) => self.on_wire(peer, &agent, frame).await,
            // The kiln it runs (`crate::fleet`); the rest is W13's.
            AgentMsg::Facts { json } => match serde_json::from_str::<kiln_ci::facts::RunnerFacts>(&json) {
                Ok(f) => {
                    if let Some(k) = f.kiln.clone() {
                        // How far behind the server's kiln it is: to be upgraded, it takes no new work.
                        self.skew_check(&agent, peer, Some(k.commit.clone()));
                        crate::fleet::remember(self, &k.build, &k.target, &k.commit);
                        self.instances.lock().set_kiln(peer, k);
                        self.updates.late_by_commit(self, &agent);
                        self.updates.fleet.wake();
                    }
                    // A probe's whole report (a runner that only said its kiln has no CPU count):
                    // routing decides over it from now on (docs/ci.md W13-R01).
                    if f.cpu.threads > 0
                        && let Some(a) = self.agents.lock().get_mut(&agent)
                    {
                        a.facts = Some(Box::new(f));
                        self.ci.send(crate::ci::dispatch::Msg::RunnerFacts { agent: agent.clone() });
                    }
                }
                Err(e) => tracing::debug!(agent = %agent, error = %e, "unreadable runner facts"),
            },
            AgentMsg::ReachHello { json } => {
                let pod = self.agents.lock().get(&agent).is_none_or(|a| a.ephemeral);
                match serde_json::from_str::<crate::reach::relay::Announce>(&json) {
                    Ok(a) if !pod => {
                        tracing::info!(agent = %agent, fingerprint = %crate::reach::e2e::short(&a.fingerprint), accept = a.accept, "reach identity");
                        self.reach.lock().announce(&agent, a);
                    }
                    Ok(_) => {}
                    Err(e) => tracing::debug!(agent = %agent, error = %e, "unreadable reach announcement"),
                }
            }
            AgentMsg::Reach { to, link, open, data, close } => {
                // Job pods neither send nor receive reach links.
                if self.agents.lock().get(&agent).is_none_or(|a| a.ephemeral) {
                    return Ok(());
                }
                let out = {
                    let (agents, instances) = (&self.agents, &self.instances);
                    self.reach.lock().forward(&agent, peer, crate::reach::relay::Frame { to, link, open, data, close }, |n| {
                        if agents.lock().get(n).is_none_or(|a| a.ephemeral || !a.connected) {
                            return None;
                        }
                        instances.lock().newest_with(n, proto::CAP_REACH)
                    })
                };
                for (p, m) in out {
                    if let Ok(b) = proto::encode(&m) {
                        self.net.send(p, proto::LANE_REACH, Kind::Term, b).await;
                    }
                }
            }
            AgentMsg::ReachAsk { req, json } => {
                let ask = serde_json::from_str::<serde_json::Value>(&json).unwrap_or_default();
                match ask.get("op").and_then(serde_json::Value::as_str) {
                    Some("directory") => {
                        let answer = serde_json::json!({ "runners": self.reach_directory() });
                        self.send_peer(peer, &ServerMsg::ReachAnswer { req, json: answer.to_string() }).await;
                    }
                    // What the runner holds of other runners' trees and let go: kept, and answered
                    // with the rules it weighs them with (docs/reach.md §5.1).
                    Some("keep") => {
                        let (hub, runner) = (Arc::clone(self), agent.clone());
                        crate::rt::spawn(async move {
                            let answer = hub.copies.heard(&hub, &runner, ask.get("report")).await;
                            hub.send_peer(peer, &ServerMsg::ReachAnswer { req, json: answer.to_string() }).await;
                        });
                    }
                    // Which runner holds which object of the runners' caches (docs/ci.md W24-R27).
                    Some(op @ ("holds" | "holders")) => {
                        let s = |k: &str| ask.get(k).and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
                        let (repo, digest) = (s("repo"), s("digest"));
                        let answer = if op == "holds" {
                            crate::reach::directory::holds(&agent, &repo, &digest);
                            serde_json::json!({})
                        } else {
                            let up: Vec<String> = crate::reach::directory::holders(&repo, &digest, &agent).into_iter().filter(|r| self.agents.lock().get(r).is_some_and(|a| a.connected)).collect();
                            serde_json::json!({ "runners": up })
                        };
                        self.send_peer(peer, &ServerMsg::ReachAnswer { req, json: answer.to_string() }).await;
                    }
                    // An agent's MCP server on the runner asks for kiln's operations: answered on
                    // their own, so a slow one holds up nothing else the runner says.
                    Some("registry" | "call" | "page" | "stream") => {
                        let (hub, runner) = (Arc::clone(self), agent.clone());
                        crate::rt::spawn(async move {
                            let answer = crate::mcp::runner_ask(&hub, &runner, &ask).await;
                            hub.send_peer(peer, &ServerMsg::ReachAnswer { req, json: answer.to_string() }).await;
                        });
                    }
                    _ => {
                        let answer = serde_json::json!({ "error": "unknown reach question" });
                        self.send_peer(peer, &ServerMsg::ReachAnswer { req, json: answer.to_string() }).await;
                    }
                }
            }
            AgentMsg::Power { json } => self.on_power(&agent, &json).await,
            AgentMsg::NetDrops { dropped } => {
                let series = dropped.iter().map(|d| (vec![agent.clone(), d.lane.clone(), d.reason.clone()], i64::try_from(d.frames).unwrap_or(i64::MAX)));
                metrics::global().runner_net_dropped.replace_owned(&std::collections::BTreeSet::from([agent.clone()]), series);
                if let Some(a) = self.agents.lock().get_mut(&agent) {
                    a.dropped = dropped;
                }
            }
            // Hand-overs between a machine's runner processes (`crate::handovers`).
            AgentMsg::HarnessHandOver { session, offset, data, done } => self.handover_piece(&agent, peer, session, offset, data, done).await,
            AgentMsg::HarnessAdopted { session, error } => self.handover_adopted(&agent, peer, session, error).await,
            // What it holds warm, for placement (docs/ci.md W24-R18); a job's pod holds nothing.
            AgentMsg::WorkState { json } => match serde_json::from_str::<kiln_ci::work::slots::WorkState>(&json) {
                Ok(state) if !self.agents.lock().get(&agent).is_none_or(|a| a.ephemeral) => self.ci.send(crate::ci::dispatch::Msg::WorkState { agent: agent.clone(), state: Box::new(state) }),
                Ok(_) => {}
                Err(e) => tracing::debug!(agent = %agent, error = %e, "an unreadable report of warm workspaces"),
            },
            // An identity it asks for, for a key it made and keeps (docs/ci.md W27-R01).
            AgentMsg::Identity { json } => crate::identities::on_ask(self, peer, &agent, &json).await,
            AgentMsg::UpgradeLog { job, line } => self.updates.on_log(self, &job, line),
            AgentMsg::UpgradeDone { job, ok, error } => self.updates.on_done(self, &job, ok, error),
            AgentMsg::Harness { session, json } => match serde_json::from_str::<crate::harness::core::Update>(&json) {
                // A copy of a session that went on elsewhere since (an older epoch): its machine
                // lets go of it, and nothing it says of it counts.
                Ok(crate::harness::core::Update::Open { info, .. }) if self.harness.stale(&session, info.epoch) => {
                    tracing::info!(%agent, %session, epoch = info.epoch, "an agent session that went on elsewhere: this machine lets go of its copy");
                    // A runner from before letting go stops its copy instead (what it says of it
                    // doesn't count either way).
                    let lets_go = self.instances.lock().get(peer).is_some_and(|i| i.caps & proto::CAP_CONVO != 0);
                    let msg = if lets_go { ServerMsg::HarnessRelease { session } } else { ServerMsg::HarnessClose { session } };
                    self.command_peers(&agent, vec![peer], &msg).await.log_when_answered(LOG_WITHIN);
                }
                // An inbox's answer counts wherever it came from (it's for the message sent there),
                // and says nothing of where the session runs: whoever waits for it is told, and
                // the session's messages still go where it runs.
                Ok(u @ crate::harness::core::Update::Inbox { .. }) => {
                    self.harness.ingest_from(self, &crate::harness::registry::Location::Machine(agent.clone()), &session, u, false);
                }
                // The rest of what a copy left behind says (its machine was told to let go of it):
                // none of it counts, and it doesn't take the session's messages from where it runs.
                Ok(u) if !matches!(u, crate::harness::core::Update::Open { .. }) && !self.harness.runs_on(&session, &agent) => {}
                Ok(u) => {
                    let closed = matches!(u, crate::harness::core::Update::Closed);
                    // A runner process that sends its conversation store: the server keeps its
                    // sessions from that.
                    let shipped = self.instances.lock().get(peer).is_some_and(|i| i.caps & proto::CAP_CONVO != 0);
                    // Two processes of one machine never both run a session: of two that say they
                    // hold it, the newer keeps it and the older lets go (a hand-over whose word
                    // came late, `crate::handovers`).
                    let (holder, older) = {
                        let mut inst = self.instances.lock();
                        let what = crate::instances::Owned::Agent(session.clone());
                        let other = inst.owner(&what).filter(|q| *q != peer && inst.hands_over(*q) && inst.hands_over(peer));
                        match other.and_then(|q| inst.newer_of(q, peer).map(|newer| (q, newer))) {
                            Some((q, newer)) if newer == q => (None, Some(peer)),
                            pair => {
                                if closed {
                                    inst.disown(&what);
                                } else if inst.owner(&what) != Some(peer) {
                                    inst.own(what, peer);
                                }
                                (inst.get(peer).map(crate::instances::Instance::label).filter(|_| !closed), pair.map(|(q, _)| q))
                            }
                        }
                    };
                    // Not one a hand-over is taking there now: the newer says so once it took it.
                    if let Some(older) = older.filter(|_| !self.handovers.lock().going_to(&session, peer)) {
                        self.let_go_of(&agent, older, &session).await;
                    }
                    if older == Some(peer) {
                        return Ok(());
                    }
                    if let Some(label) = holder {
                        self.harness.held_by(&session, &label);
                    }
                    self.harness.ingest_from(self, &crate::harness::registry::Location::Machine(agent.clone()), &session, u, shipped);
                    if closed {
                        self.check_retire().await;
                        self.check_handover().await;
                    }
                }
                Err(e) => tracing::debug!(%agent, error = %e, "unreadable agent session update"),
            },
            AgentMsg::HarnessInventory { json } => match serde_json::from_str::<crate::harness::host::Inventory>(&json) {
                Ok(mut inv) => {
                    // A copy it kept of a session that went on elsewhere since isn't its own (it
                    // lets go of it once it announces it).
                    inv.sessions.retain(|id| self.harness.runs_on(id, &agent));
                    // This process's sessions are its own; another process of the machine keeps
                    // the ones it holds (the registry forgets sessions a report leaves out), and
                    // so does one that isn't connected now (`Registry::holder`).
                    let (me, present, others) = {
                        let mut inst = self.instances.lock();
                        for id in &inv.sessions {
                            inst.own(crate::instances::Owned::Agent(id.clone()), peer);
                        }
                        inst.reported(peer);
                        let others: Vec<String> = inst
                            .of(&agent)
                            .into_iter()
                            .filter(|p| *p != peer)
                            .flat_map(|p| inst.owned_by(p))
                            .filter_map(|o| match o {
                                crate::instances::Owned::Agent(id) => Some(id),
                                _ => None,
                            })
                            .collect();
                        let present: Vec<String> = inst.of(&agent).into_iter().filter_map(|p| inst.get(p).map(crate::instances::Instance::label)).collect();
                        (inst.get(peer).map(crate::instances::Instance::label), present, others)
                    };
                    if let Some(me) = &me {
                        for id in &inv.sessions {
                            self.harness.held_by(id, me);
                        }
                    }
                    let own = inv.sessions.clone();
                    inv.sessions.extend(others);
                    self.harness.inventory(self, &agent, me.as_deref(), &present, inv);
                    // Hand-overs dropped as it went away get their word now it's back.
                    self.handovers_dropped(&agent, peer, &own).await;
                    self.check_retire().await;
                }
                Err(e) => tracing::debug!(%agent, error = %e, "unreadable agent session inventory"),
            },
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
        for j in &db_running {
            if h.running.contains(&j.id) {
                keep.push(j.id.clone());
            } else if held_by_other(&j.id) {
                // Another runner process of this machine runs it.
            } else {
                self.store
                    .requeue_job(j.id.clone(), format!("agent {name} restarted without the job"))
                    .await?;
                self.emit_job(&j.id).await;
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
                self.launching.lock().remove(j);
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
        if let Some(j) = &h.dedicated_job {
            self.launching.lock().remove(j);
        }
        // It keeps the channel (docs/wire.md): said first, so a runner that keeps it too knows on
        // Welcome not to send its sessions whole. An older runner skips the frame.
        self.send_peer(peer, &ServerMsg::Caps { caps: proto::CAP_WIRE | proto::CAP_NET_DROPS | proto::CAP_HANDOVER | proto::CAP_WORK | proto::CAP_AGENT_LIMIT | proto::CAP_IDENTITY }).await;
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
    pub fn runner_caps(&self, name: &str) -> u32 {
        let inst = self.instances.lock();
        inst.newest(name).and_then(|p| inst.get(p)).map_or(0, |i| i.caps)
    }

    /// What a machine's newest runner can do in 64 bits: `Build`'s 32 and the rest
    /// ([`proto::AgentMsg::Caps`]: [`proto::CAP_WIRE`] ...).
    pub fn runner_wide_caps(&self, name: &str) -> u64 {
        let inst = self.instances.lock();
        inst.newest(name).and_then(|p| inst.get(p)).map_or(0, |i| u64::from(i.caps) | i.wide)
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
        self.release_slot(agent, &r.job);
        self.cancel_sent.lock().remove(&r.job);
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

    // ------------------------------------------------------------ scheduler

    /// Assign queued jobs to agents (or launch pods) until nothing more fits. Runs on every wake
    /// and once a second as a safety net.
    pub async fn scheduler_loop(self: Arc<Self>) -> anyhow::Result<()> {
        let mut tick = Interval::new(Duration::from_secs(1));
        let mut slow = Instant::now();
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
        }
    }

    /// The server's secrets, for the CI engine's trusted runs (docs/ci.md W09 narrows them).
    pub fn ci_secrets(&self) -> Vec<(String, String)> {
        self.secrets()
    }

    /// What a job's environment gets from the server: its secrets, and where each image kiln
    /// publishes lives (`KILN_IMAGE_<NAME>`, docs/cd-targets.md §7.1), set in the UI.
    fn job_secrets(&self) -> Vec<(String, String)> {
        let mut s = self.secrets();
        if let Some(cd) = crate::cd::get() {
            s.extend(cd.image_env());
        }
        s
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

    async fn schedule_once(&self) -> anyhow::Result<()> {
        let queued = self.store.queued_jobs(2000).await?;
        metrics::global().jobs_queued.set(i64::try_from(queued.len()).unwrap_or(i64::MAX));
        let running = i64::try_from(self.store.running_jobs().await?.len()).unwrap_or(0);
        metrics::global().jobs_running.set(running);
        if queued.is_empty() {
            return Ok(());
        }
        let mut runs = HashMap::new();
        let k8s = self.k8s.get().and_then(Option::as_ref);
        let mut pods_live = None;
        for job in queued {
            let Some(spec) = job.spec.clone() else { continue };
            if !runs.contains_key(&job.run_id) {
                let r = self.store.run(job.run_id.clone()).await?;
                runs.insert(job.run_id.clone(), r);
            }
            let Some(Some(run)) = runs.get(&job.run_id) else { continue };
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
                    let ephemeral = self.agents.lock().get(&agent).is_some_and(|a| a.ephemeral);
                    let executor = if job.labels.iter().any(|l| l == "k8s") && ephemeral { "k8s" } else { "baremetal" };
                    if !self.store.claim_job(job.id.clone(), agent.clone(), executor.into()).await? {
                        continue;
                    }
                    {
                        let mut g = self.agents.lock();
                        if let Some(a) = g.get_mut(&agent) {
                            a.running.push(job.id.clone());
                            a.busy = u32::try_from(a.running.len()).unwrap_or(u32::MAX);
                            hold_on(a, &job.id, v1_incoming(spec.exclusive, &spec.resources));
                        }
                    }
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
                        secrets: self.job_secrets(),
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
                    let launch = self.launching.lock().get(&job.id).copied();
                    match launch {
                        Some(Launch::Pod(t)) if t.elapsed() < timeout => continue,
                        Some(Launch::Retry { at, .. }) if Instant::now() < at => continue,
                        // After a server restart the launch table is empty, but the job's pod may
                        // already exist (Pending or about to connect): adopt it instead of duplicating.
                        None => {
                            if k.job_pod_alive(&job.id).await {
                                self.launching.lock().insert(job.id.clone(), Launch::Pod(Instant::now()));
                                continue;
                            }
                        }
                        Some(Launch::Pod(_)) => {
                            // Still Pending for capacity (or pulling a large image) is not a failure:
                            // keep waiting instead of churning pods.
                            if k.job_pod_alive(&job.id).await {
                                self.launching.lock().insert(job.id.clone(), Launch::Pod(Instant::now()));
                                continue;
                            }
                            tracing::warn!(job = %job.id, "pod agent did not connect in time; relaunching");
                            k.delete_job_pods(&job.id).await;
                            // Not again on the next pass if the launch below has to wait.
                            self.launching
                                .lock()
                                .insert(job.id.clone(), Launch::Retry { at: Instant::now(), check_pods: false });
                        }
                        Some(Launch::Retry { check_pods: true, .. }) => {
                            if k.job_pod_alive(&job.id).await {
                                self.launching.lock().insert(job.id.clone(), Launch::Pod(Instant::now()));
                                continue;
                            }
                            k.delete_job_pods(&job.id).await;
                        }
                        Some(Launch::Retry { check_pods: false, .. }) => {}
                    }
                    if pods_live.is_none() {
                        pods_live = Some(self.launching.lock().values().filter(|l| matches!(l, Launch::Pod(_))).count());
                    }
                    let live = pods_live.unwrap_or(0);
                    if u32::try_from(live).unwrap_or(u32::MAX) >= self.cfg.kubernetes.max_pods {
                        continue;
                    }
                    match k.launch(self, &job, run).await {
                        Ok(pod) => {
                            tracing::info!(job = %job.id, pod, "launched agent pod");
                            self.launching.lock().insert(job.id.clone(), Launch::Pod(Instant::now()));
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
                            self.launching.lock().insert(job.id.clone(), Launch::Retry { at, check_pods: !quota });
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
        let local = crate::pki::sanitize_name(&self.cfg.local_agent_name());
        if self.cfg.local_agent.enabled && agent == local {
            let port = self.cfg.server.http_addr.rsplit_once(':').map_or("7070", |(_, p)| p);
            return format!("http://127.0.0.1:{port}");
        }
        let ephemeral = self.agents.lock().get(agent).is_some_and(|a| a.ephemeral);
        if ephemeral
            && let Some(u) = &self.cfg.kubernetes.server_url {
                return u.clone();
            }
        self.cfg.server.public_url.clone()
    }

    fn pick_agent(&self, job: &Job) -> Option<String> {
        let declined = self.declined.lock().clone();
        let g = self.agents.lock();
        let incoming = job.spec.as_ref().map_or(kiln_ci::sched::state::Incoming::Ordinary, |s| v1_incoming(s.exclusive, &s.resources));
        g.values()
            .filter(|a| a.connected && !a.draining && !a.exclusive && a.skew.takes_work())
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
            // The one with the most room: by its idle cores, free memory and CI slots taken, as
            // reach measures it; then the most slots.
            .max_by(|a, b| {
                let (ra, rb) = (crate::reach::directory::room(strain_of(a), a.busy, a.slots), crate::reach::directory::room(strain_of(b), b.busy, b.slots));
                ra.total_cmp(&rb).then(a.slots.cmp(&b.slots)).then(b.name.cmp(&a.name))
            })
            .map(|a| a.name.clone())
    }

    async fn check_timeouts(&self) -> anyhow::Result<()> {
        let now = crate::util::now_unix();
        for job in self.store.running_jobs().await? {
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
                    if let Some(agent) = &job.agent {
                        self.release_slot(agent, &job.id);
                    }
                    self.cancel_sent.lock().remove(&job.id);
                    let (run_id, _) = self
                        .store
                        .finish_job(job.id.clone(), Status::TimedOut, None, Some(format!("exceeded {} min", spec.timeout_minutes)), None, None)
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
                self.release_slot(&name, &j);
                match self.store.requeue_job(j.clone(), format!("runner process did not reconnect ({name})")).await {
                    Ok(_) => self.emit_job(&j).await,
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
                    if !known.contains(&j.id) && j.agent.as_deref() != Some("-")
                        && let Ok(true) = self.store.requeue_job(j.id.clone(), "orphaned after restart".into()).await {
                            self.emit_job(&j.id).await;
                        }
                }
            }
        // Expire unused pod enrollment tokens.
        self.ephemeral_tokens.lock().retain(|_, (_, exp)| *exp > Instant::now());
        self.executor_tokens.lock().retain(|_, (_, _, exp)| *exp > Instant::now());
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
    let asked = kiln_ci::sched::pools::ResourceSpec { cpu: resources.cpu.clone(), memory: resources.memory.clone(), ..Default::default() }.resources().unwrap_or_default();
    kiln_ci::sched::state::Incoming::of(exclusive, &asked)
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

    /// A message a runner sent that couldn't be read (typed messages, step 1): its row is in the audit
    /// log, its runner's record carries it, and the runner's page is told.
    #[test]
    fn a_refusal_noted_is_audited_kept_on_its_runner_and_its_page_told() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = scratch_hub_on(Arc::new(kiln_seams::clock::SimClock::new()), |_| {}).await?;
            hub.agents.lock().insert("fedora".into(), live_runner("fedora", "linux", "x86_64"));
            let mut events = hub.events.subscribe();
            let fedora = crate::wire::unread::Sender::Runner { name: "fedora".into(), instance: "main".into() };
            anyhow::ensure!(hub.note_unread(unread(crate::wire::unread::Wire::Runner, fedora), None).await == Some(0), "its sender may be told");
            let row: (String, Option<String>, String, String) = hub
                .store
                .call(|c| Ok(c.query_row("SELECT principal, target, result, detail FROM audit2 WHERE action = 'security.wire_refused'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?))
                .await?;
            anyhow::ensure!(row == ("runner:fedora".into(), Some("runner".into()), "unknown".into(), "variant 51: AgentMsg has 51 variants".into()), "{row:?}");
            let kept = hub.agent_list().await.into_iter().find(|a| a.name == "fedora").and_then(|a| a.refused);
            anyhow::ensure!(kept.as_ref().map(crate::wire::unread::Kept::total) == Some(1), "GET /api/agents carries it: {kept:?}");
            let told: serde_json::Value = serde_json::from_str(&crate::rt::timeout(Duration::from_secs(5), events.recv()).await??)?;
            anyhow::ensure!(told.pointer("/type") == Some(&serde_json::json!("agent")) && told.pointer("/agent/refused/counts/0/n") == Some(&serde_json::json!(1)), "{told}");
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
            let _ = hub.note_unread(unread(crate::wire::unread::Wire::RunnerPayload, from), Some(crate::wire::unread::Page::Session("s1".into()))).await;
            let told: serde_json::Value = serde_json::from_str(&crate::rt::timeout(Duration::from_secs(5), events.recv()).await??)?;
            anyhow::ensure!(told.pointer("/type") == Some(&serde_json::json!("harness")) && told.pointer("/info/refused/counts/0/wire") == Some(&serde_json::json!("runner_payload")), "{told}");
            hub.harness.ingest(&hub, &there, "s1", open());
            let kept = hub.harness.list().into_iter().find(|i| i.id == "s1").and_then(|i| i.refused);
            anyhow::ensure!(kept.as_ref().map(crate::wire::unread::Kept::total) == Some(1), "kept across its runner's Open: {kept:?}");
            Ok(())
        })
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
            let spec = serde_json::json!({"kind": "claude", "cwd": "/w"}).to_string();
            let open = crate::harness::host::refusal("s1", &spec, "").into_iter().find(|u| matches!(u, crate::harness::core::Update::Open { .. })).ok_or_else(|| anyhow::anyhow!("no announcement"))?;
            hub.on_agent_msg(1, say(&open)?).await?;
            anyhow::ensure!(hub.harness.runs_on("s1", "box-a"), "it runs on box-a");
            let refused = crate::harness::inbox::Said { message: "m1".into(), answer: Err(crate::harness::inbox::Refusal::Moved { session: "s1".into(), epoch: 0 }) };
            hub.on_agent_msg(2, say(&crate::harness::core::Update::Inbox { said: Box::new(refused) })?).await?;
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
            hub.on_agent_msg(1, AgentMsg::UpdatePrefs { accept: true }).await?;
            anyhow::ensure!(!hub.cmds.lock().is_coming(1), "still waiting for box-a's Caps");
            anyhow::ensure!(matches!(old.try_recv(), Ok(Some(Answer::Sent | Answer::Unreachable { .. }))), "what waited for box-a didn't go its way");
            // box-b says its Caps: what waited for it goes on the channel.
            hub.on_agent_msg(2, AgentMsg::Caps { caps: proto::CAP_WIRE }).await?;
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
}
