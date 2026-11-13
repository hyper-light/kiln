//! Configuration: a TOML file plus environment overrides. Every field has a default so kiln starts
//! with no config at all (bare-metal quick start).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

pub mod ci;
pub mod sso;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct Config {
    pub server: Server,
    pub gitops: GitOps,
    pub repos: Vec<RepoConfig>,
    pub local_agent: LocalAgent,
    pub kubernetes: Kubernetes,
    pub terminal: Terminal,
    pub retention: Retention,
    pub telemetry: Telemetry,
    /// Speech to text for the console's microphone where the browser can't do it itself.
    pub voice: crate::voice::Voice,
    /// Upgrades: where kiln's own binaries for every platform come from.
    pub updates: Updates,
    pub discovery: Discovery,
    /// Deployments (docs/cd.md).
    pub cd: Cd,
    /// CI (docs/ci.md).
    pub ci: ci::Ci,
    /// kiln's own metrics, logs and events (docs/observe.md).
    pub observe: Observe,
    /// The engine that places work (docs/needs.md §21): `[sched.classes.<class>]`.
    pub sched: Sched,
    /// Signing in with GitHub, GitLab and Bitbucket (docs/ci.md W23-R08).
    pub sso: sso::Sso,
    /// Conversations (docs/conversations.md): the runners that keep a copy of the server's store.
    pub conversations: Conversations,
    /// Notifications to people's devices (docs/notifications.md).
    pub push: crate::push::Push,
    /// Agents that start agents (docs/agents.md, "Agents that run agents").
    pub agents: Agents,
    /// Where kiln keeps its bytes (docs/blobs.md): the server's data directory, or any S3.
    pub blobs: crate::blobs::Config,
    /// The bus kiln's events ride (docs/bus.md): Quiver, in the server's process.
    pub bus: crate::bus::Config,
    /// What a runner's administrator declares of it (docs/ci.md W13-R02): `[agent.facts]` and
    /// `[agent.probes]`. The server's own runner reads them here; `kiln agent` from its config file.
    pub agent: crate::runner::probe::Declared,
    /// Environment variables injected into every job (e.g. registry mirrors). Values starting with
    /// `file:` are read from that path at job dispatch time and masked in logs.
    pub secrets: BTreeMap<String, String>,
    /// kiln's records through the cluster group (docs/p2p.md).
    pub cluster: Cluster,
    /// What reach decides on its own about runs on other machines (docs/reach.md, "Sizes from
    /// history"): each off until declared.
    pub reach: Reach,
    /// What this machine's runner does on its own (docs/runners.md "Pace").
    pub runner: RunnerSettings,
    /// The machine's capacity keeper this runner holds its runs' room through (docs/lb.md "A
    /// machine's keeper"): none until declared.
    pub capacity: Capacity,
}

/// `[capacity]` (docs/lb.md "A machine's keeper"; Hex's capacity-view.md; kiln plan
/// 01a1cd8e862e7169a074db3d5fa33f3d, step 13): the machine's capacity keeper, Hex's node, which
/// keeps one ledger of the machine's room for Hex's boxes and kiln's runs. Declared, each run this
/// runner starts reserves its room through the keeper and starts only on the keeper's grant, and
/// the runner takes nothing new while it isn't synced with it. Off unless both are declared.
///
/// ```toml
/// [capacity]
/// keeper = "10.0.0.7:4433"   # where the keeper's capacity port is reached, host:port
/// keeper_name = "node-7"     # the name its certificate carries
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Capacity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keeper: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keeper_name: Option<String>,
}

impl Capacity {
    /// The keeper this runner holds through, where it's reached and the name its certificate
    /// carries: none unless both are declared; why not, where one is declared without the other.
    pub fn keeper(&self) -> Result<Option<(String, String)>, String> {
        let said = |v: &Option<String>| v.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
        match (said(&self.keeper), said(&self.keeper_name)) {
            (Some(at), Some(name)) => Ok(Some((at, name))),
            (None, None) => Ok(None),
            (Some(_), None) => Err("`[capacity] keeper` is declared without `keeper_name`, the name the keeper's certificate carries: no keeper is held through".to_owned()),
            (None, Some(_)) => Err("`[capacity] keeper_name` is declared without `keeper`, where the keeper is reached (host:port): no keeper is held through".to_owned()),
        }
    }
}

/// `[runner]` (docs/runners.md "Pace", W05-R26).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunnerSettings {
    /// On battery: `"drain"` (the default: it takes nothing new until it's on its charger, as
    /// volunteer computing and Windows' Task Scheduler do; `kiln_ci::pace::power` says why) or
    /// `"take"` (as on its charger).
    pub on_battery: kiln_ci::pace::power::OnBattery,
}

/// `[reach]` (docs/reach.md, "Sizes from history" and §2.3; docs/needs.md E6 and §21): what kiln
/// decides on its own about reach runs and the runs this runner admits, each as kiln has it until
/// the person declares it (AGENTS.md: opt-in automation). Every run is measured and kept either way.
///
/// ```toml
/// [reach]
/// overcommit_cpu = 1.5     # CPUs offered: half again the room's
/// backfill = true          # room kept for the first in line that fits nowhere yet
/// predicted_order = 4      # a tenant's own runs by predicted time, within γ = 4
/// tokens = true            # owners told what they hold here, each its own
/// learned_weights = true   # probes drawn by learned wait weights
/// auction = true           # runs arriving together placed by the auction
/// good_places = false      # among equals runs take turns, not back to where their kind went well
/// ```
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Reach {
    /// What sizes a run for placement: `"ask"` (what it asks: the default) or `"history"` (what its
    /// command used before on that platform, its ask the ceiling).
    pub sizes: kiln_ci::sizes::Source,
    /// A run killed for memory that ran sized from history below its ask runs once more at its
    /// ask (not a detached run, nor one fed input: docs/reach.md says why).
    pub retry_oom: bool,
    /// A run the placement kernel can't start now is probed at runners drawn in proportion to one
    /// learned weight each (`place::Settings::learned_weights`; docs/lb.md "Learned wait weights";
    /// docs/needs.md §21; LB08, W05-R23): its CPUs until learned, halved where the runs probed
    /// loaded it past twice the optimum's makespan in its phase (Lattanzi et al., SODA 2020,
    /// Algorithm 1; that level doubled as the runs show it's past it, Aspnes et al., J. ACM 1997),
    /// and at random again once the weights aren't helping, saying so. Off: two drawn at random,
    /// as before.
    pub learned_weights: bool,
    /// Ordinary runs that arrive in one step (a tenant's budget letting several through, a CI run's
    /// legs waiting together at the server) go where the integer auction puts them, at the least
    /// total expected finish (`place::Settings::auction`; docs/lb.md "The auction"; docs/needs.md
    /// §21; LB08, W05-R26), where the runs' times and the runners' measured speeds tell them apart
    /// and the kernel can start them now; the rest as before. Off: one at a time.
    pub auction: bool,
    /// This runner's admission offers its CPUs times this, beside what's held (docs/needs.md §3:
    /// CPUs are shared, never memory, disk or GPUs): `1.5` is half again. 1, the room itself, until
    /// declared.
    #[serde(skip_serializing_if = "is_one")]
    pub overcommit_cpu: f64,
    /// Room kept for the first in this runner's line that fits nowhere yet, from when its claims are
    /// predicted to leave it room (EASY backfilling, docs/ci.md W05-R39): only what fits beside that
    /// room, or ends by then, starts ahead of it. Off until declared.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub backfill: bool,
    /// How far a tenant's own runs here go by their predicted times (docs/ci.md W05-R09,
    /// docs/needs.md §5): the robustness target γ, from which the weight on predictions follows as
    /// Purohit, Svitkina and Kumar derive it (NeurIPS 2018: λ = 1 − 2/γ). Unset, or 2 or less:
    /// arrival order, as before.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub predicted_order: Option<f64>,
    /// Room tokens (docs/lb.md "Room tokens"; LB08): each owner whose link named `place.tokens` is
    /// told what it holds of this runner's room, its own, on placement's channel, the room its runs
    /// free going back to it; the others hear this runner's hints, as before. Off until declared.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub tokens: bool,
    /// A run the placement kernel places goes back to the runner its kind of work (the same
    /// command, leg or session) last went well on, when that runner is as free and its cores as
    /// fast as any, and past the runner that last let that kind down (docs/lb.md "Good places";
    /// W05-R04, W05-R23). What it left there is warm, and going back is the decision that takes
    /// least. A run's answer says when that was why it went there. On by default, the user's
    /// exception to opt-in automation (2026-11-11, docs/ci.md §0.3); `false`: among equals runs
    /// take turns, as before.
    #[serde(skip_serializing_if = "is_true")]
    pub good_places: bool,
}

fn is_one(x: &f64) -> bool {
    *x == 1.0
}

fn is_true(x: &bool) -> bool {
    *x
}

impl Default for Reach {
    fn default() -> Self {
        Reach { sizes: kiln_ci::sizes::Source::default(), retry_oom: false, learned_weights: false, auction: false, overcommit_cpu: 1.0, backfill: false, predicted_order: None, tokens: false, good_places: true }
    }
}

impl Reach {
    pub fn policy(&self) -> kiln_ci::sizes::Policy {
        kiln_ci::sizes::Policy { sizes: self.sizes, retry_oom: self.retry_oom }
    }

    /// What this runner's placement decides by (`kiln_ci::place::runner::Settings`, docs/reach.md
    /// §2.3): kiln's own, with what's declared here over them; why not, for a value that doesn't
    /// read.
    pub fn place(&self) -> Result<kiln_ci::place::runner::Settings, String> {
        if !self.overcommit_cpu.is_finite() || self.overcommit_cpu < 1.0 {
            return Err(format!("`[reach] overcommit_cpu` is a factor of 1 or more (`1.5`), not {}", self.overcommit_cpu));
        }
        let predicted_order = match self.predicted_order {
            None => None,
            Some(g) if g.is_finite() && g > 0.0 => Some(kiln_ci::sched::units::thousandths_of(g)),
            Some(g) => return Err(format!("`[reach] predicted_order` is a robustness target above 0 (`4`), not {g}")),
        };
        Ok(kiln_ci::place::runner::Settings {
            overcommit_cpu: kiln_ci::sched::state::per_mille(self.overcommit_cpu),
            backfill: self.backfill,
            predicted_order,
            tokens: self.tokens,
            learned_weights: self.learned_weights,
            auction: self.auction,
            good_places: self.good_places,
            // The rest kiln's own.
            ..kiln_ci::place::runner::Settings::default()
        })
    }
}

/// `[cluster]` (docs/p2p.md).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Cluster {
    /// The store's writes go through the cluster group (P1): one hyper-raft group with kiln.db as
    /// its state machine, its log in `<data>/cluster`. Off (or `KILN_CLUSTER_GROUP=0`), writes go
    /// straight to kiln.db, as before it; the database is whole either way.
    pub group: bool,
    /// The runners hold learner replicas of the group (P2), each fed by the server over its link,
    /// from a copy of kiln.db. Off (or `KILN_CLUSTER_LEARNERS=0`), none is added; the learners the
    /// group has stay in it, lagging.
    pub learners: bool,
}

impl Default for Cluster {
    fn default() -> Self {
        Self { group: true, learners: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Server {
    pub http_addr: String,
    pub quic_addr: String,
    /// Base URL agents and humans use to reach the HTTP API.
    pub public_url: String,
    /// `host:port` agents dial for QUIC.
    pub quic_advertise: String,
    pub data_dir: PathBuf,
    pub admin_token_file: Option<PathBuf>,
    pub metrics_public: bool,
    /// Extra DNS names / IPs for the server certificate.
    pub tls_names: Vec<String>,
    /// Also serve HTTPS on `http_addr` (plain HTTP keeps working there: each connection is
    /// answered in what it starts with). The certificate is `https_cert`/`https_key` when given,
    /// else one from the kiln CA, which devices trust once they install `/ca.pem` (or
    /// `/ca.mobileconfig`). See docs/front-door.md for HTTPS on a runner instead.
    pub https: bool,
    /// A PEM certificate chain (leaf first) for `https`, instead of the kiln CA's.
    pub https_cert: Option<PathBuf>,
    /// Its PEM private key.
    pub https_key: Option<PathBuf>,
    /// Where pages agents show are served (docs/previews.md), as `https://kiln-pages.example`: a
    /// site of its own, so browsers keep pages apart from kiln. Unset: kiln's own address serves
    /// them, each sandboxed in an origin of its own.
    pub pages_url: Option<String>,
    /// WebRTC to streams' viewers (docs/previews.md, "Streams"): the UDP address it listens on, as
    /// `0.0.0.0:7072`. Unset, viewers take streams over their WebSocket (WebCodecs) alone.
    pub webrtc_addr: Option<String>,
    /// The `host:port`s viewers reach it at (its ICE candidates), when they aren't `webrtc_addr`
    /// itself: a forwarded or NATed port, a host's Tailscale address. Empty: `webrtc_addr`, or (on
    /// every address) each LAN address of this machine at its port.
    pub webrtc_advertise: Vec<String>,
    /// The HTTP/3 WebTransport door (goal 4, docs/http.md "HTTP/3"): the UDP address it listens
    /// on, as `0.0.0.0:7443`. Unset (the default), there is no door and no socket is bound.
    pub h3_addr: Option<String>,
    /// The `host:port` pages reach the door at, when it isn't `public_url`'s host at the bound
    /// port.
    pub h3_advertise: Option<String>,
    /// A publicly trusted PEM certificate chain for the door. Unset, the door serves self-signed
    /// ECDSA P-256 certificates that pages pin by hash, rotated weekly.
    pub h3_cert: Option<PathBuf>,
    /// Its PEM private key.
    pub h3_key: Option<PathBuf>,
    /// A shard whose queue stands past its calm (most of a tolerance, 1 s, past it while the shard runs
    /// most of the time), or a pool's line that work keeps standing past it, answers Background requests
    /// 503 with `Retry-After`, and Normal ones too once it has stood another tolerance; Critical ones
    /// (health, sign-in, a person's pages and actions) never (`crate::overload`, kiln's goal 6). Off until declared, as what kiln decides
    /// on its own is (docs/ci.md §0.3, "Opt-in automation"): the queues are measured and shown on
    /// `/metrics` and `/readyz` either way, and off, nothing is refused. `KILN_SHED` overrides it.
    pub shed: bool,
    /// Runners reconnecting are taken back only as fast as the server can (docs/resilience.md R5,
    /// `crate::hub::admission`): admissions in progress are held to what the control loop takes at
    /// once by Little's law, a runner past that is told its turn and dials again then, and on its way
    /// down the server tells its runners when it expects to answer again. Off until declared, as what
    /// kiln decides on its own is (docs/ci.md §0.3, "Opt-in automation"): admissions are measured and
    /// what they teach is kept either way, and off, every runner is taken as it comes.
    pub admission: bool,
    /// Cores of the server's own (docs/priority.md, "Cores of its own"; goal 8, plan 01a1a59c2b29
    /// step 6), as a CPU list (`8-15`, `2,4-7`): on Linux `kiln install` gives its `kiln.slice`
    /// these alone (`AllowedCPUs=`, where the service manager has the cpuset controller) and,
    /// installed system-wide, makes them a cpuset partition that nothing outside the slice runs on
    /// (given back without them, and by `kiln uninstall`); the server fixes its shards to them, says
    /// whether they're a partition's (its log, `/healthz/server`); and kiln's own legs and agents keep
    /// off them, those its own runner starts (off its shards' cores, where the slice holds the runner on
    /// them) and those of every runner whose config this is (step 7; goal 9, plan 01a1a87b2cec step 4).
    /// Unset (the default), the server shares every core with what runs beside it, first by its weight.
    pub cores: Option<String>,
    /// The server's shards in Linux's round-robin real-time class at its lowest priority
    /// (docs/priority.md, "A real-time class for the shards"; goal 8, plan 01a1a59c2b29 step 7; goal 9,
    /// plan 01a1a87b2cec step 4), where the system grants it (`kiln install` writes the grant,
    /// `LimitRTPRIO=`): a shard waking takes its CPU at once, whatever runs there, under every kernel
    /// preemption mode. Each takes it on a CPU of its own, the server's other threads kept off them:
    /// those of `cores`, else the machine's fastest first, one thread of each core (`crate::rt::class_cpus`);
    /// with no such CPU for each shard, none asks (2026-11-06). Refused or held back, the shards run as
    /// without it and `/healthz/server` and the log say why. Off until declared (docs/ci.md §0.3,
    /// "Opt-in automation"): on bare metal, `kiln install` declares it in a new server's config wherever
    /// the systemd unit it writes grants it, saying why (docs/priority.md, "The bare-metal default").
    /// `KILN_RT_REALTIME` overrides it.
    pub realtime: bool,
    /// How the server's shards nudge their CPUs while they wait (docs/priority.md, "A nudge for the
    /// shards"; goal 8, plan 01a1a59c2b29 step 7; goal 6, plan 01a1a59c2abe step 3): a nudged shard's waits
    /// end at least every step quantum less the timers' tick (166.7 µs; goal 9, plan 01a1a87b2cec step 4), so
    /// on a Linux kernel that preempts lazily a shard woken behind a thread busy in user space runs within a
    /// quantum, the timer slack added (266.7 µs), not at the kernel's next tick (up to 1 ms at 1000 Hz).
    /// `"off"`, the default: never; it's off until declared (docs/ci.md §0.3, "Opt-in automation").
    /// `"auto"`, the mode a config declares, nudges a shard only while its host holds its wakes, as its
    /// probe reads each period from the kernel's count of its wait for a CPU (`crate::nudge`), so a calm
    /// server pays nothing and says when and why a shard nudged; `"on"` nudges every wait, a wake of each
    /// waiting shard every period. Needs no privilege; in the real-time class's place where that's refused or
    /// not asked for (a shard in it needs no nudge); macOS and Windows refuse it, having no lazy preemption.
    /// `KILN_RT_NUDGE` overrides it (`auto`, `on`, `off`, or a flag's word: `1`, `0`), and `KILN_RT_NUDGE_US`
    /// sets its period for a lab's A/B.
    pub nudge: crate::nudge::Mode,
    /// The browser protocol over one WebSocket (`GET /api/live`, `crate::live`; goal 3 of the browser
    /// protocol program, docs/browser-protocol.md): served beside today's sockets when true. Off until
    /// declared, as the plan's open call leaves it until goal 9 moves the UI onto it. `KILN_LIVE`
    /// overrides it.
    pub live: bool,
    /// The TCP congestion control the protocol's sockets ask for (`bbr`, say), on Linux, where the host
    /// allows it: loaded, and listed in `net.ipv4.tcp_allowed_congestion_control` (the host's
    /// administrator's step). Each socket says what it got, or why it was refused, on `/metrics` and
    /// `GET /api/live/sockets`; nothing falls back silently. Unset (the default): the host's own.
    /// `KILN_BROWSER_CONGESTION` overrides it.
    pub browser_congestion: Option<String>,
    /// Whether the protocol's sockets keep to their end-to-end window (`kiln_bp::window`: no more bytes
    /// the page hasn't said it has than the path's rate times its round trip allows). On (the default);
    /// off leaves the kernel's mark and the page's credit the only bounds below the scheduler: the
    /// lab's window-on-against-off A/B (plan 01a1a832eb2f711f9c6f51d88effe89d step 7), and a test of
    /// the mark alone. `KILN_BROWSER_WINDOW` overrides it.
    pub browser_window: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitOps {
    pub orgs: Vec<String>,
    pub poll_seconds: u32,
    pub discover_seconds: u32,
    /// Only auto-register discovered repos that are not archived / forks.
    pub skip_forks: bool,
    pub webhook_secret_file: Option<PathBuf>,
    /// Optional token for commit statuses and higher API limits (never required).
    pub github_token_file: Option<PathBuf>,
    /// Concurrent git operations (fetches) across all repos.
    pub git_concurrency: usize,
    /// Build pull requests from refs/pull/*/head.
    pub pull_requests: bool,
    /// Synthesize a build + test pipeline (from Cargo.toml, go.mod, pyproject.toml, package.json,
    /// rebar.config) for repositories that have no `.kiln/` directory yet.
    pub auto_pipelines: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct RepoConfig {
    pub name: String,
    pub clone_url: Option<String>,
    pub branches: Vec<String>,
    pub poll_seconds: Option<u32>,
    /// `native`: kiln hosts it (docs/ci.md W14-R01), made empty or from `import`; `upstream`
    /// (the default): kiln follows `clone_url`.
    pub mode: Option<String>,
    /// A native repository's history, copied once from this URL.
    pub import: Option<String>,
    /// A new, empty native repository's trunk (`main`).
    pub default_branch: Option<String>,
    /// A native repository's upstream: its trunk is pushed there after each land.
    pub mirror: Option<String>,
}


#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalAgent {
    pub enabled: bool,
    pub name: Option<String>,
    pub labels: Vec<String>,
    pub slots: u32,
    pub work_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Kubernetes {
    pub enabled: bool,
    pub namespace: String,
    pub agent_image: String,
    pub service_account: String,
    pub cache_pvc: Option<String>,
    pub max_pods: u32,
    /// Seconds to wait for a launched pod's agent to connect before retrying.
    pub start_timeout_seconds: u32,
    pub node_selector: BTreeMap<String, String>,
    /// Job and executor pods only on worker nodes (no `node-role.kubernetes.io/control-plane`
    /// label), not just preferring them: the control plane's etcd and API server keep their room
    /// (W13-R07). Off by default, as a cluster of control-plane nodes alone would run no job.
    pub workers_only: bool,
    pub image_pull_policy: String,
    /// Default requests when a job doesn't specify resources.
    pub default_cpu: String,
    pub default_memory: String,
    /// URL / address pods use to reach the server (defaults to public_url / quic_advertise).
    pub server_url: Option<String>,
    pub server_quic: Option<String>,
    /// PriorityClass for job pods (low, so CI never starves the control plane).
    pub priority_class: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Terminal {
    pub enabled: bool,
    pub shell: Option<String>,
    pub max_sessions: usize,
    /// Close a terminal session after this long with nobody attached.
    pub idle_minutes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Retention {
    pub days: u32,
    /// Hard cap on a single job's log size.
    pub max_job_log_mb: u64,
    /// How long the audit log keeps its rows (docs/ci.md W19-R05).
    pub audit_days: u32,
}

/// `[conversations]` (docs/conversations.md, "Mirrors").
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Conversations {
    /// How many runners keep a copy of the server's conversation store. 0: none.
    pub mirrors: usize,
    /// Runners to pick first, by name (each still has to keep a store of its own).
    pub mirror_on: Vec<String>,
    /// Days the server's store keeps an ended agent's transcript (its events stay). 0: for good.
    /// Runners and mirrors keep them the default 30.
    pub keep_closed_days: u32,
    /// Days kiln's store of bytes keeps an ended agent's archived transcript after its last close
    /// (docs/blobs.md, `transcripts/`). 0: for good.
    pub keep_archived_days: u32,
    /// Make the store's file compact once a third of it is free: a copy written a batch at a time
    /// while the store has nothing else to do, then put in its place. Runners and mirrors compact
    /// theirs.
    pub compact: bool,
    /// No conversation store for this server: tests that don't use one (`KILN_CONVERSATIONS=off`
    /// does it for a whole process). Never read from a config file.
    #[serde(skip)]
    pub off: bool,
}

/// `[sched]`: the engine that places work (docs/needs.md §21).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Sched {
    /// Each class's floor and bound (docs/needs.md §7, docs/ci.md W05-R31), by its name:
    /// `critical`, `land`, `interactive`, `batch` or `background`. What a class leaves out stays
    /// as kiln has it: a 15 % floor and a 30-minute bound for `batch`, 5 % and an hour for
    /// `background`, neither for the rest.
    ///
    /// ```toml
    /// [sched.classes.batch]
    /// floor = 20      # percent of the fleet, at its largest dimension
    /// bound = "45m"   # promoted a class after waiting this long, its class passed over
    /// ```
    pub classes: BTreeMap<kiln_ci::sched::Class, ClassSettings>,
    /// How long a grant lives unless its runner renews it (docs/needs.md §8, docs/ci.md W05-R34):
    /// one not renewed by then is lost, and its item's next attempt queued. Runners renew three
    /// times a lease, at most every 10 s. 60 s by default (`90s`, `2m`), at least 3 s.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease: Option<String>,
    /// How long past a lost grant's expiry an item that stops on losing its lease waits before its
    /// next attempt (W05-R34): its runner has stopped it by then. 15 s by default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_delay: Option<String>,
    /// Reservations for big items (docs/needs.md §5, docs/ci.md W05-R39): the first of each class
    /// that fits no runner keeps room on the runner where it frees first, from its grants' expected
    /// ends, and only what ends before then, or fits beside it, is granted there. Without it a big
    /// item may wait as long as smaller ones keep coming. Off until declared (docs/ci.md §0.3).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub backfill: bool,
}

/// `[sched.classes.<class>]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClassSettings {
    /// Its floor share of the fleet, in percent (0 to 100).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub floor: Option<f64>,
    /// How long one of its items waits, its class starting nothing where it would run, before it's
    /// promoted a class (`90s`, `30m`, `6h`), or `never`: a person's, none by default. Each item
    /// has a measured bound besides, the expected end of the higher-class work ahead of it (docs/ci.md
    /// W05-R31), which `never` doesn't turn off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bound: Option<String>,
}

impl Sched {
    /// The classes' floors and bounds as the engine takes them: kiln's defaults, with what's set
    /// here over them.
    pub fn classes(&self) -> anyhow::Result<BTreeMap<kiln_ci::sched::Class, kiln_ci::sched::ClassConfig>> {
        let mut out = kiln_ci::sched::state::default_classes();
        for (class, set) in &self.classes {
            let c = out.entry(*class).or_default();
            if let Some(floor) = set.floor {
                anyhow::ensure!((0.0..=100.0).contains(&floor), "[sched.classes.{}] floor = {floor}: a percent, 0 to 100", class.as_str());
                c.floor = kiln_ci::sched::units::thousandths_of(floor / 100.0);
            }
            if let Some(bound) = set.bound.as_deref() {
                c.bound = match bound.trim() {
                    "never" => None,
                    b => Some(kiln_ci::work::reap::parse_duration(b).map_err(|e| anyhow::anyhow!("[sched.classes.{}] bound: {e}", class.as_str()))?.saturating_mul(1000)),
                };
            }
        }
        Ok(out)
    }

    /// The lease and the lock-delay, in milliseconds, as the engine takes them (W05-R34): kiln's
    /// 60 s and 15 s, or what's set here.
    pub fn leases(&self) -> anyhow::Result<(u64, u64)> {
        let read = |name: &str, set: Option<&str>, default: u64| -> anyhow::Result<u64> {
            set.map_or(Ok(default), |v| Ok(kiln_ci::work::reap::parse_duration(v).map_err(|e| anyhow::anyhow!("[sched] {name}: {e}"))?.saturating_mul(1000)))
        };
        let lease = read("lease", self.lease.as_deref(), 60_000)?;
        anyhow::ensure!(lease >= 3_000, "[sched] lease: at least 3s, as runners renew three times a lease");
        Ok((lease, read("lock_delay", self.lock_delay.as_deref(), 15_000)?))
    }
}

/// `[agents]`: how far agents may start agents (docs/agents.md, "Agents that run agents").
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Agents {
    /// How many agents deep a chain may go below the one a person started (that one is 0).
    pub max_depth: u32,
    /// The most subagents at work one agent may have at once, whatever its own Subagents setting
    /// says. None (the default): as many as that setting says, and with none, no limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_children: Option<u32>,
}

impl Default for Agents {
    fn default() -> Self {
        Self { max_depth: 3, max_children: None }
    }
}

impl Conversations {
    /// How long ended agents' transcripts are kept (None: for good).
    pub fn keep_closed(&self) -> Option<std::time::Duration> {
        (self.keep_closed_days > 0).then(|| std::time::Duration::from_secs(u64::from(self.keep_closed_days) * 86_400))
    }

    /// How a store looks after its file, as this says.
    pub fn upkeep(&self) -> crate::convo::store::Upkeep {
        crate::convo::store::Upkeep { keep_closed: self.keep_closed(), compact: self.compact.then_some(crate::convo::store::COMPACT_MIN_FREE) }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Telemetry {
    pub sample_seconds: u32,
    pub log_format: String,
}

/// Finding other kiln instances (multicast DNS `_kiln._tcp.local.` or the OS's own service
/// discovery, unicast DNS-SD, probes of addresses, Tailscale peers) and letting machines ask to join
/// this server as agents. Joining always needs an admin's approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Discovery {
    /// Master switch for announcing and browsing.
    pub enabled: bool,
    /// Advertise this server on the local network (UDP 5353 must be open to the LAN).
    pub announce: bool,
    /// Keep a table of other kiln instances seen on the network (`GET /api/discovery`).
    pub browse: bool,
    /// Accept join requests (`POST /api/join-requests`) from machines running `kiln join`.
    pub join_requests: bool,
    /// Network interfaces to use (e.g. `["enp5s0"]`). Empty: every multicast-capable LAN
    /// interface except loopback, point-to-point links and container bridges.
    pub interfaces: Vec<String>,
    /// Use the system's service discovery where there is one (Bonjour on macOS, DNS-SD on
    /// Windows 10 1809+) instead of kiln's own multicast DNS. Linux always uses kiln's own.
    pub native: bool,
    /// Unicast DNS-SD domains to browse for `_kiln._tcp.<domain>`, besides the system's search
    /// domains and the browse domains they list (`b._dns-sd._udp.<domain>`).
    pub domains: Vec<String>,
    /// Name servers for unicast DNS-SD (`"192.168.1.1"`, `"[fd00::1]:53"`). Empty: those of
    /// `/etc/resolv.conf`, or of the network adapters on Windows.
    pub dns_servers: Vec<String>,
    /// Who may query this instance's multicast DNS responder by unicast from outside its own
    /// subnets (scans and probes from other networks): `"private"` (RFC 1918, 100.64.0.0/10,
    /// fc00::/7, link-local), `"any"`, or addresses and CIDRs. Empty: the local subnets only.
    pub answer_from: Vec<String>,
    /// Probe the peers `tailscale status` lists, every few minutes.
    pub tailscale: bool,
    /// Addresses or host names (optionally `:<http port>`) probed every minute.
    pub seeds: Vec<String>,
    /// Publish this instance in a unicast DNS zone with DNS UPDATE signed with TSIG.
    pub publish: Option<Publish>,
}

/// `[discovery.publish]`: the zone kiln adds its DNS-SD records to (RFC 2136 + RFC 8945).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Publish {
    /// The primary name server of the zone, `host[:port]`.
    pub server: String,
    /// The zone, e.g. `home.example.com`.
    pub zone: String,
    /// The TSIG key's name.
    pub key_name: String,
    /// A file holding the key's base64 secret (alone, or as `tsig-keygen` writes it).
    pub key_secret_file: PathBuf,
    /// `hmac-sha256` (default), `hmac-sha384` or `hmac-sha512`.
    pub algorithm: String,
}

fn hostname() -> String {
    crate::os::hostname()
}

/// `/var/lib/kiln` as root; otherwise per user: `~/.local/share/kiln` (`$XDG_DATA_HOME`) on Unix,
/// `%LOCALAPPDATA%\kiln` on Windows.
/// Deployments.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Cd {
    pub enabled: bool,
    /// How often sources are checked for new commits (webhooks check at once).
    pub poll_seconds: u64,
    /// Outside Kubernetes, the kubeconfig context that stands in for Argo CD's in-cluster
    /// destination (`https://kubernetes.default.svc`).
    pub in_cluster_context: String,
    /// The kubeconfig file contexts are read from (the default one when empty).
    pub kubeconfig: String,
    pub helm: String,
    pub kubectl: String,
    /// The kustomize binary; `kubectl kustomize` when empty.
    pub kustomize: String,
    /// Extra `kustomize build` options (Argo CD's `kustomize.buildOptions`).
    pub kustomize_options: Vec<String>,
    pub render_timeout_seconds: u64,
    /// Requests a second kiln sends a cluster, unless the cluster says otherwise.
    pub qps: f64,
    /// Wait between sync waves (Argo CD's `ARGOCD_SYNC_WAVE_DELAY`).
    pub wave_delay_ms: u64,
    /// Whether agents may sync, roll back, take over and delete apps outside any environment.
    /// An environment says so itself (`agents: { deploy: true }`). Off: agents plan and ask.
    pub agents_deploy: bool,
}

impl Default for Cd {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_seconds: 60,
            in_cluster_context: String::new(),
            kubeconfig: String::new(),
            helm: "helm".into(),
            kubectl: "kubectl".into(),
            kustomize: String::new(),
            kustomize_options: Vec::new(),
            render_timeout_seconds: 90,
            qps: 50.0,
            wave_delay_ms: 2_000,
            agents_deploy: false,
        }
    }
}

pub fn default_data_dir() -> PathBuf {
    if crate::os::is_root() {
        return crate::os::system_dirs().0;
    }
    crate::os::data_home().join("kiln")
}

/// `$KILN_CONFIG`; `/etc/kiln/kiln.toml` as root; otherwise per user: `~/.config/kiln/kiln.toml`
/// (`$XDG_CONFIG_HOME`) on Unix, `%APPDATA%\kiln\kiln.toml` on Windows.
pub fn default_config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("KILN_CONFIG") {
        return PathBuf::from(p);
    }
    if crate::os::is_root() {
        return crate::os::system_dirs().1;
    }
    crate::os::config_home().join("kiln").join("kiln.toml")
}

impl Default for Server {
    fn default() -> Self {
        let host = hostname();
        Self {
            http_addr: "0.0.0.0:7070".into(),
            quic_addr: "0.0.0.0:7071".into(),
            public_url: format!("http://{host}:7070"),
            quic_advertise: format!("{host}:7071"),
            data_dir: default_data_dir(),
            admin_token_file: None,
            metrics_public: true,
            tls_names: Vec::new(),
            https: false,
            https_cert: None,
            https_key: None,
            pages_url: None,
            webrtc_addr: None,
            webrtc_advertise: Vec::new(),
            h3_addr: None,
            h3_advertise: None,
            h3_cert: None,
            h3_key: None,
            shed: false,
            admission: false,
            cores: None,
            realtime: false,
            nudge: crate::nudge::Mode::Off,
            live: false,
            browser_congestion: None,
            browser_window: true,
        }
    }
}

impl Default for GitOps {
    fn default() -> Self {
        Self {
            orgs: Vec::new(),
            poll_seconds: 60,
            discover_seconds: 900,
            skip_forks: true,
            webhook_secret_file: None,
            github_token_file: None,
            git_concurrency: 8,
            pull_requests: true,
            auto_pipelines: true,
        }
    }
}

impl Default for LocalAgent {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        Self {
            enabled: true,
            name: None,
            labels: vec!["baremetal".into(), "local".into()],
            slots: u32::try_from((cpus / 4).max(1)).unwrap_or(4),
            work_dir: None,
        }
    }
}

impl Default for Kubernetes {
    fn default() -> Self {
        Self {
            enabled: false,
            namespace: "kiln-jobs".into(),
            agent_image: "kiln:dev".into(),
            service_account: "kiln-job".into(),
            cache_pvc: None,
            max_pods: 64,
            start_timeout_seconds: 300,
            node_selector: BTreeMap::new(),
            workers_only: false,
            image_pull_policy: "IfNotPresent".into(),
            default_cpu: "1".into(),
            default_memory: "2Gi".into(),
            server_url: None,
            server_quic: None,
            priority_class: None,
        }
    }
}

impl Default for Discovery {
    fn default() -> Self {
        Self {
            enabled: true,
            announce: true,
            browse: true,
            join_requests: true,
            interfaces: Vec::new(),
            native: true,
            domains: Vec::new(),
            dns_servers: Vec::new(),
            answer_from: vec!["private".into()],
            tailscale: true,
            seeds: Vec::new(),
            publish: None,
        }
    }
}

impl Default for Terminal {
    fn default() -> Self {
        Self { enabled: true, shell: None, max_sessions: 32, idle_minutes: 720 }
    }
}

/// Observe (docs/observe.md): the group the server leads and runners replicate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Observe {
    pub enabled: bool,
    /// How long samples are kept: Datadog's, 15 months at full resolution ("Data retention
    /// periods", docs.datadoghq.com), as days.
    pub metrics_retention_days: u32,
    /// How long log records are kept: Datadog's default index retention, 15 days.
    pub logs_retention_days: u32,
    /// How long spans are kept: Datadog's for indexed spans, 15 days ("Trace Retention",
    /// docs.datadoghq.com).
    pub spans_retention_days: u32,
    /// How long profilers' uploads are kept: Datadog's for profiles, 8 days ("Continuous Profiler",
    /// docs.datadoghq.com).
    pub profiles_retention_days: u32,
    /// The most GiB profiles take on the server's disk; unset, a quarter of that disk.
    pub profiles_max_gb: Option<u32>,
    /// The most series the group holds (0: no bound); past it, new series are refused and counted.
    pub max_series: u64,
    /// How often the server looks at its own host, container and process (`crate::observe::host`;
    /// 0: never). Runners look as often as `KILN_OBSERVE_HOST_SECONDS` says, 15 unless it does.
    pub host_seconds: u32,
    /// Threads queries may read series on beside their own, shared by every query at once (0: each
    /// reads on its own thread alone), at a lower priority than the server's other work. Unset: a
    /// quarter of the cores the server may run on, less one (docs/observe.md §12.19).
    pub query_helpers: Option<u32>,
}

impl Default for Observe {
    fn default() -> Self {
        Self {
            enabled: true,
            metrics_retention_days: 455,
            logs_retention_days: 15,
            spans_retention_days: 15,
            profiles_retention_days: 8,
            profiles_max_gb: None,
            max_series: 0,
            host_seconds: 15,
            query_helpers: None,
        }
    }
}

impl Default for Conversations {
    fn default() -> Self {
        Self { mirrors: 1, mirror_on: Vec::new(), keep_closed_days: 30, keep_archived_days: 365, compact: true, off: false }
    }
}

impl Default for Retention {
    fn default() -> Self {
        Self { days: 30, max_job_log_mb: 256, audit_days: 400 }
    }
}

impl Default for Telemetry {
    fn default() -> Self {
        Self { sample_seconds: 5, log_format: "text".into() }
    }
}


impl Config {
    /// Load from `path` if it exists (a missing file is fine), then apply env overrides.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut cfg = match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str::<Config>(&s).with_context(|| format!("parse {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        if let Some(v) = std::env::var_os("KILN_DATA_DIR") {
            cfg.server.data_dir = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("KILN_HTTP_ADDR") {
            cfg.server.http_addr = v;
        }
        if let Ok(v) = std::env::var("KILN_QUIC_ADDR") {
            cfg.server.quic_addr = v;
        }
        if let Ok(v) = std::env::var("KILN_PUBLIC_URL") {
            cfg.server.public_url = v;
        }
        if let Ok(v) = std::env::var("KILN_CLUSTER_GROUP") {
            cfg.cluster.group = !matches!(v.trim(), "0" | "false" | "off" | "no");
        }
        if let Ok(v) = std::env::var("KILN_CLUSTER_LEARNERS") {
            cfg.cluster.learners = !matches!(v.trim(), "0" | "false" | "off" | "no");
        }
        if let Ok(v) = std::env::var("KILN_QUIC_ADVERTISE") {
            cfg.server.quic_advertise = v;
        }
        if let Ok(v) = std::env::var("KILN_WEBRTC_ADDR") {
            cfg.server.webrtc_addr = Some(v).filter(|v| !v.trim().is_empty());
        }
        if let Ok(v) = std::env::var("KILN_H3_ADDR") {
            cfg.server.h3_addr = Some(v).filter(|v| !v.trim().is_empty());
        }
        if let Ok(v) = std::env::var("KILN_H3_ADVERTISE") {
            cfg.server.h3_advertise = Some(v).filter(|v| !v.trim().is_empty());
        }
        if let Ok(v) = std::env::var("KILN_WEBRTC_ADVERTISE") {
            cfg.server.webrtc_advertise = v.split(',').map(str::trim).filter(|a| !a.is_empty()).map(str::to_owned).collect();
        }
        // `[server] shed`, as a lab's server takes its settings (`kiln-lab ab --env-b KILN_SHED=1`).
        if let Ok(v) = std::env::var("KILN_SHED") {
            cfg.server.shed = !matches!(v.trim(), "0" | "false" | "off" | "no");
        }
        // `[server] realtime`, as a lab's server takes its settings (`kiln-lab ab --env-b KILN_RT_REALTIME=1`).
        if let Ok(v) = std::env::var("KILN_RT_REALTIME") {
            cfg.server.realtime = !matches!(v.trim(), "0" | "false" | "off" | "no");
        }
        // `[server] nudge`, as a lab's server takes its settings (`kiln-lab ab --env-b KILN_RT_NUDGE=1`, `=auto`, `=off`).
        if let Ok(v) = std::env::var("KILN_RT_NUDGE") {
            cfg.server.nudge = crate::nudge::Mode::from_env(&v).ok_or_else(|| anyhow::anyhow!("KILN_RT_NUDGE={v}: auto, on or off"))?;
        }
        // `[server] live`, `browser_congestion` and `browser_window`, as a lab's server takes its settings.
        if let Ok(v) = std::env::var("KILN_LIVE") {
            cfg.server.live = !matches!(v.trim(), "0" | "false" | "off" | "no" | "");
        }
        if let Ok(v) = std::env::var("KILN_BROWSER_CONGESTION") {
            cfg.server.browser_congestion = Some(v.trim().to_owned()).filter(|v| !v.is_empty());
        }
        if let Ok(v) = std::env::var("KILN_BROWSER_WINDOW") {
            cfg.server.browser_window = !matches!(v.trim(), "0" | "false" | "off" | "no");
        }
        Ok(cfg)
    }

    pub fn data(&self, sub: &str) -> PathBuf {
        self.server.data_dir.join(sub)
    }

    pub fn local_agent_name(&self) -> String {
        self.local_agent.name.clone().unwrap_or_else(hostname)
    }

    /// What the server's own runner (`[local_agent]`) is known by (docs/machines.md): its machine's
    /// id ([`crate::os::machine_id`]), else its name. Its name ([`Self::local_agent_name`]) is its
    /// label.
    pub fn local_agent_key(&self) -> String {
        crate::os::machine_id().unwrap_or_else(|| crate::pki::sanitize_name(&self.local_agent_name()))
    }

    pub fn hostname() -> String {
        hostname()
    }

    pub fn render_default() -> String {
        toml::to_string_pretty(&Config::default()).unwrap_or_default()
    }
}

/// `[updates]`: the pipeline that builds kiln for every platform (`.kiln/release.yaml`). When its
/// job succeeds, the binaries it leaves (`dist/kiln-<target>`) are what that platform's runners
/// upgrade to: no token, no upload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Updates {
    /// The repository, as kiln names it.
    pub release_repo: String,
    /// Its pipeline.
    pub release_pipeline: String,
    /// The branch "Build every platform" builds.
    pub release_branch: String,
    /// Keep every platform in step with the server: whenever the other platforms' binaries
    /// aren't built from the commit the server runs, run the pipeline on that commit
    /// (`crate::updates::in_step_loop`), so every runner can be upgraded to what the server runs.
    /// Needs a server built knowing its commit (`KILN_COMMIT`).
    pub in_step: bool,
    /// Runners whose update policy is Automatic follow the newest kiln (`crate::fleet`): when the
    /// server or a runner runs a newer commit of the release branch, they are brought to it, by a
    /// binary of it, or by building it itself on a machine with a kiln checkout. `false`: none is,
    /// whatever its policy.
    pub follow: bool,
    /// On a runner's machine: it takes upgrades from its server. `false`: its runner refuses every
    /// one, whatever the server's policy for it (`kiln config set updates.accept false`); the
    /// server shows it so.
    pub accept: bool,
    /// Minutes a runner has to switch to its new build and report in, counted once it has no
    /// jobs running. System → Updates can change it, and the two below.
    pub upgrade_minutes: u64,
    /// Minutes kiln keeps an upgrade open while the runner finishes the jobs it is running (it
    /// switches only once they are done).
    pub wait_for_jobs_minutes: u64,
    /// Minutes a machine has to build kiln itself, install included.
    pub build_minutes: u64,
    /// Version skew (`crate::skew`): a runner on the server's commit or one of this many release
    /// branch commits before it (its first parents) does everything. System → Updates can change
    /// it, and the one below.
    pub skew_full: u32,
    /// Version skew: a runner on one of this many commits before the server's, past `skew_full`,
    /// connects to be upgraded and takes no new work until it is; further behind, it's refused.
    /// Never less than `skew_full`.
    pub skew_upgrade: u32,
}

impl Default for Updates {
    fn default() -> Self {
        Self {
            release_repo: "hyper-light/kiln".to_owned(),
            release_pipeline: "release".to_owned(),
            release_branch: "main".to_owned(),
            in_step: true,
            follow: true,
            accept: true,
            upgrade_minutes: 15,
            wait_for_jobs_minutes: 6 * 60,
            build_minutes: 60,
            // About three days of main, and about a month: a runner a day behind does everything.
            skew_full: 100,
            skew_upgrade: 1000,
        }
    }
}

#[cfg(test)]
mod reach_tests {
    use super::*;

    /// W05-R09, W05-R39, LB08: `[reach]`'s placement settings as this runner's placement takes them:
    /// kiln's own until declared, each declared one read, and one that doesn't read refused saying
    /// which.
    #[test]
    fn lb08_reach_placement_settings_are_read_and_default_as_before() -> anyhow::Result<()> {
        let none: Config = toml::from_str("")?;
        anyhow::ensure!(none.reach.place() == Ok(kiln_ci::place::runner::Settings::default()), "undeclared, kiln's own: {:?}", none.reach.place());
        let set: Config = toml::from_str("[reach]\novercommit_cpu = 1.5\nbackfill = true\npredicted_order = 4\ntokens = true\n")?;
        let p = set.reach.place().map_err(|e| anyhow::anyhow!(e))?;
        anyhow::ensure!((p.overcommit_cpu, p.backfill, p.predicted_order, p.tokens) == (1500, true, Some(4000), true), "{p:?}");
        anyhow::ensure!(p == kiln_ci::place::runner::Settings { overcommit_cpu: 1500, backfill: true, predicted_order: Some(4000), tokens: true, ..kiln_ci::place::runner::Settings::default() }, "the rest kiln's own");
        for (bad, says) in [("overcommit_cpu = 0.5", "overcommit_cpu"), ("predicted_order = -1", "predicted_order")] {
            let c: Config = toml::from_str(&format!("[reach]\n{bad}\n"))?;
            anyhow::ensure!(c.reach.place().is_err_and(|e| e.contains(says)), "{bad}");
        }
        anyhow::ensure!(toml::from_str::<Config>("[reach]\ntoken = true\n").is_err(), "an unknown key is an error");
        // Good places: on unless turned off (the user's exception to opt-in, docs/ci.md §0.3).
        anyhow::ensure!(none.reach.place().is_ok_and(|p| p.good_places), "good places are on by default");
        let off: Config = toml::from_str("[reach]\ngood_places = false\n")?;
        anyhow::ensure!(off.reach.place().is_ok_and(|p| !p.good_places), "good_places = false turns them off");
        // Said back as written: nothing undeclared is.
        let said = toml::to_string(&Reach::default())?;
        anyhow::ensure!(!said.contains("overcommit_cpu") && !said.contains("tokens") && !said.contains("backfill") && !said.contains("predicted_order") && !said.contains("good_places"), "{said}");
        Ok(())
    }

    /// docs/lb.md "A machine's keeper": `[capacity]` names the machine's keeper, where it's reached
    /// and the name its certificate carries. None is held through until both are declared; one
    /// without the other is refused saying which is missing; an unknown key is an error; nothing
    /// undeclared is said back.
    #[test]
    fn capacity_names_its_keeper_only_when_both_are_declared() -> anyhow::Result<()> {
        let none: Config = toml::from_str("")?;
        anyhow::ensure!(none.capacity.keeper() == Ok(None), "off until declared: {:?}", none.capacity.keeper());
        let both: Config = toml::from_str("[capacity]\nkeeper = \"10.0.0.7:4433\"\nkeeper_name = \"node-7\"\n")?;
        anyhow::ensure!(both.capacity.keeper() == Ok(Some(("10.0.0.7:4433".to_owned(), "node-7".to_owned()))), "{:?}", both.capacity.keeper());
        let at: Config = toml::from_str("[capacity]\nkeeper = \"node-7.lan:4433\"\n")?;
        anyhow::ensure!(at.capacity.keeper().is_err_and(|e| e.contains("keeper_name")), "{:?}", at.capacity.keeper());
        let name: Config = toml::from_str("[capacity]\nkeeper_name = \"node-7\"\nkeeper = \" \"\n")?;
        anyhow::ensure!(name.capacity.keeper().is_err_and(|e| e.contains("host:port")), "{:?}", name.capacity.keeper());
        anyhow::ensure!(toml::from_str::<Config>("[capacity]\nkeeper_port = 4433\n").is_err(), "an unknown key is an error");
        let said = toml::to_string(&Capacity::default())?;
        anyhow::ensure!(said.trim().is_empty(), "{said}");
        Ok(())
    }
}
