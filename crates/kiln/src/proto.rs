//! Agent ⇄ server protocol, carried over hyper-transport lanes (ordered, mutually authenticated
//! QUIC streams). Messages are postcard-encoded: compact, schema'd by Rust types, and decoding a
//! malformed frame is an error, never a panic.
//!
//! Lanes:
//! * [`LANE_CONTROL`] — control messages in both directions (class `Control`, highest priority).
//! * [`LANE_TERM`]    — interactive PTY traffic (class `Control`: keystrokes must not queue behind
//!   bulk logs).
//! * [`LANE_LOGS`]    — job output (class `Bulk`).

use serde::{Deserialize, Serialize};

use crate::metrics::HostSample;
use crate::pipeline::PlannedJob;
use crate::store::Status;

pub const LANE_CONTROL: u32 = 0;
pub const LANE_TERM: u32 = 1;
pub const LANE_LOGS: u32 = 2;
/// Repo sync's lane (`crate::sync`): a queue of its own, so a burst of sync never sheds a job's or a
/// terminal's frames.
pub const LANE_SYNC: u32 = 3;
/// Reach's lane (`crate::reach`): links between runners, relayed by the server. Interactive class:
/// a remote command's output must not queue behind job logs.
pub const LANE_REACH: u32 = 4;
/// Observe's lane (`crate::observe`): the observe group's messages and writes, a queue of their own
/// so replication never sheds a job's, a terminal's or sync's frames, nor waits behind them.
pub const LANE_OBSERVE: u32 = 5;
/// The conversation feed's lane (`crate::convo::feed`): a runner's store sent to the server's, in
/// bulk frames, never in the way of a job's or a terminal's (lane 5 is observe's).
pub const LANE_CONVO: u32 = 6;
/// Files read on a machine for the people viewing its agent sessions (`crate::harness::files`):
/// what a message mentions, shown in the conversation. Bulk frames of their own, so a video being
/// read never holds up a job's log, a terminal or the conversation feed. The last of a peer's eight.
pub const LANE_FILES: u32 = 7;
/// Pages streamed from a machine (`crate::browser::stream`): their picture and sound up, what their
/// viewers do down. Interactive frames of their own, so a stream never waits behind a log, a file or
/// the feed. Past the first eight lanes: a peer allows sixteen (`crate::net::limits`); only runners
/// announcing [`CAP_STREAM`] open it, and only to a server that asked them to stream.
pub const LANE_STREAM: u32 = 8;
/// Ports of agents' machines opened through the server for people's own devices
/// (`crate::browser::ports`, `crate::harness::opened`): the bytes of each connection both ways.
/// A lane of its own, so a page's files never wait behind a stream's picture or a terminal; only to
/// runners announcing [`CAP_PORTS`].
pub const LANE_PORTS: u32 = 9;

/// A lane's name, as metrics and runner pages say it.
pub fn lane_name(lane: u32) -> &'static str {
    match lane {
        LANE_CONTROL => "control",
        LANE_TERM => "term",
        LANE_LOGS => "logs",
        LANE_SYNC => "sync",
        LANE_REACH => "reach",
        5 => "observe",
        LANE_CONVO => "convo",
        LANE_FILES => "files",
        LANE_STREAM => "stream",
        LANE_PORTS => "ports",
        _ => "other",
    }
}

/// The lane a name says (as [`lane_name`] says it), or its number.
pub fn lane_named(name: &str) -> Option<u32> {
    (0..16).find(|l| lane_name(*l) == name).or_else(|| name.parse().ok())
}

/// Protocol version; bumped on incompatible changes. Agents with another version are refused.
pub const VERSION: u32 = 1;

/// Largest single log chunk an agent sends (fits comfortably inside one lane frame).
pub const LOG_CHUNK: usize = 32 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AgentMsg {
    Hello(Hello),
    Heartbeat { free_slots: u32, running: Vec<String> },
    Sample(HostSample),
    JobStarted { job: String },
    StepStarted { job: String, idx: u32 },
    StepFinished { job: String, idx: u32, status: Status, exit_code: Option<i32> },
    JobFinished(JobResult),
    JobSample { job: String, rss: u64, cpu_seconds: f64 },
    /// The agent could not take the job (e.g. no slot after a race); server requeues it.
    Reject { job: String, reason: String },
    TermOutput { session: String, data: Vec<u8> },
    TermExit { session: String, code: Option<i32>, error: Option<String> },
    /// The terminals still running on this machine (sent on every (re)connect, so a restarted
    /// server learns about sessions that outlived it).
    TermList { sessions: Vec<TermInfo> },
    /// Bytes that bring a fresh terminal to the session's current state (answer to TermAttach).
    TermReplay { session: String, data: Vec<u8> },
    /// The AI coding sessions running on this machine, as a JSON array of
    /// [`crate::aisessions::AiSession`] (JSON, so either side can add fields). Sent when they
    /// change. Appended last: postcard encodes variants by index.
    AiSessions { json: String },
    /// Answer to [`ServerMsg::AiTranscript`]: the messages as JSON, `None` when that session has
    /// no transcript (any more).
    AiTranscript { req: u64, json: Option<String> },
    /// One update of a kiln-native agent session running on this machine (JSON
    /// [`crate::harness::core::Update`]). Appended last: postcard encodes variants by index.
    Harness { session: String, json: String },
    /// The agent sessions this machine runs and the harnesses it offers (JSON
    /// [`crate::harness::host::Inventory`]); sent on every (re)connect, before their snapshots.
    HarnessInventory { json: String },
    /// Answer to [`ServerMsg::AiView`]: the session's normalized view (JSON: events, state,
    /// changes), `None` when there is none. Appended last: postcard encodes variants by index.
    AiView { req: u64, json: Option<String> },
    /// Which runner process of its machine this connection is, sent before [`Hello`]. A machine
    /// runs two while one replaces the other without a restart (the old one keeps its terminals
    /// until they end); runners from before this never send it and count as "legacy".
    /// Appended last: postcard encodes variants by index (an older server skips the frame).
    Instance(InstanceHello),
    /// The build this runner runs (a short hash of its binary) and what it can do ([`CAP_UPGRADE`]:
    /// it takes [`ServerMsg::Upgrade`]). Sent after Hello; runners from before this never send it.
    /// Appended last: postcard encodes variants by index (an older server skips the frame).
    Build { build: String, caps: u32 },
    /// A line of output from an upgrade this runner is doing ([`ServerMsg::Upgrade`]).
    UpgradeLog { job: String, line: String },
    /// That upgrade finished (`ok`), or failed with `error`.
    UpgradeDone { job: String, ok: bool, error: Option<String> },
    /// Answer to [`ServerMsg::HarnessTaskOutput`]: JSON `{data, offset, live, ...}` or `{error}`,
    /// `None` when this machine has no such session. Appended last: postcard encodes variants by
    /// index.
    HarnessTaskOutput { req: u64, json: Option<String> },
    /// Answer to [`ServerMsg::DirList`]: JSON [`crate::dirs::Listing`]. Appended last: postcard
    /// encodes variants by index.
    DirList { req: u64, json: String },
    /// Answer to [`ServerMsg::HarnessHistory`]: JSON `[`[`crate::aisessions::PastSession`]`]`,
    /// `None` when this machine does not look. Appended last: postcard encodes variants by index.
    HarnessHistory { req: u64, json: Option<String> },
    /// Repo sync (`crate::sync`, on [`LANE_SYNC`]): `json` a [`crate::sync::wire::Msg`], `raft` the
    /// bytes of the hyper-raft message it carries (empty when none). Only from runners announcing
    /// [`CAP_SYNC`]. Appended last: postcard encodes variants by index.
    Sync { json: String, raft: Vec<u8> },
    /// This machine's front door (JSON [`crate::frontdoor::Status`]): how its DNS, HTTPS and
    /// `.local` listeners and its certificate are doing, and a CSR when it needs a certificate from
    /// the kiln CA. Sent on every (re)connect and whenever it changes. Appended last: postcard
    /// encodes variants by index.
    FrontDoorStatus { json: String },
    /// CI (docs/ci.md §2.5): one event of a leg this runner runs from an [`ServerMsg::AssignV2`]
    /// (JSON `JobEventV2`: steps starting and finishing, annotations, groups, summaries, outputs,
    /// failure classes). Only from runners announcing [`CAP_CI2`]. Appended last: postcard encodes
    /// variants by index.
    JobEvent { job: String, json: String },
    /// CI: what this runner is (JSON `RunnerFacts`, docs/ci.md W13-R01): OS, architecture, CPUs,
    /// memory, disk, GPUs, executors, tools, declared facts. Sent on connect, every 10 minutes and
    /// when asked ([`ServerMsg::ProbeFacts`]); only from runners announcing [`CAP_FACTS`].
    /// Appended last: postcard encodes variants by index.
    Facts { json: String },
    /// Reach (`crate::reach`, on [`LANE_REACH`]): a frame of a link to runner `to`, for the server
    /// to relay. `data` is the link's ciphertext; `open` on a link's first frame; `close` ends the
    /// link (with why). Only from runners announcing [`CAP_REACH`]. Appended last: postcard
    /// encodes variants by index.
    Reach { to: String, link: String, open: bool, data: Vec<u8>, close: Option<String> },
    /// Reach: this runner's identity and whether it takes links (JSON
    /// [`crate::reach::relay::Announce`]). Sent on every (re)connect. Appended last: postcard
    /// encodes variants by index.
    ReachHello { json: String },
    /// Reach: a question for the server (JSON `{"op": "directory"}`), answered by
    /// [`ServerMsg::ReachAnswer`]. Appended last: postcard encodes variants by index.
    ReachAsk { req: u64, json: String },
    /// The conversation feed (`crate::convo::feed`, on [`LANE_CONVO`]): `json` a
    /// [`crate::convo::feed::Msg`], `data` a batch's bytes (empty for the rest). Only from runners
    /// announcing [`CAP_CONVO`]. Appended last: postcard encodes variants by index.
    Convo { json: String, data: Vec<u8> },
    /// Whether this machine takes upgrades from its server (its kiln config's `[updates] accept`):
    /// `false` and the server never upgrades it, whatever its policy there. Sent on every
    /// (re)connect, and when it changes. Appended last: postcard encodes variants by index.
    UpdatePrefs { accept: bool },
    /// This machine's battery (JSON [`crate::os::battery::Battery`]), when it has one: sent when it
    /// changes, and every 10 minutes. A machine on battery sleeps when it runs out, and its runner
    /// with it; the server says so first. Appended last: postcard encodes variants by index.
    Power { json: String },
    /// The answer to a question the server asked by `req` ([`ServerMsg::Diagnose`],
    /// [`ServerMsg::Repair`]): JSON, or `None` when there's nothing to say. Appended last: postcard
    /// encodes variants by index.
    Answer { req: u64, json: Option<String> },
    /// Answer to [`ServerMsg::HarnessFileRead`] (on [`LANE_FILES`]): `json` a
    /// [`crate::harness::files::Answer`] (the file's size, type and dimensions, or why it can't
    /// be read), `data` the bytes asked for. Only from runners announcing [`CAP_FILES`]. Appended
    /// last: postcard encodes variants by index.
    HarnessFileData { req: u64, json: String, data: Vec<u8> },
    /// A chunk of stream `stream` (on [`LANE_STREAM`]): `crate::browser::stream::frame`'s bytes.
    /// Only from runners announcing [`CAP_STREAM`]. Appended last: postcard encodes variants by
    /// index.
    StreamFrame { stream: String, data: Vec<u8> },
    /// What stream `stream` says (JSON: opened, navigated, loaded, focus, dialog, closed, error), on
    /// [`LANE_STREAM`]. Appended last: postcard encodes variants by index.
    StreamEvent { stream: String, json: String },
    /// Connection `conn` reached its port, or why not (`error`). On [`LANE_PORTS`].
    PortOpened { conn: u64, error: Option<String> },
    /// Bytes from connection `conn`'s port: at most `crate::browser::ports::WINDOW` the server
    /// hasn't acknowledged ([`ServerMsg::PortAck`]).
    PortData { conn: u64, data: Vec<u8> },
    /// The runner took `bytes` more of what the server sent connection `conn`.
    PortAck { conn: u64, bytes: u64 },
    /// Connection `conn`'s port ended it (or it failed: `error`).
    PortClose { conn: u64, error: Option<String> },
    /// CI: one of a pool's executors this runner was asked to start is ready, or failed (JSON
    /// `crate::runner::pool::Said`, docs/ci.md W13-R09). Only from runners announcing
    /// [`CAP_POOLS`]. Appended last: postcard encodes variants by index.
    Executor { json: String },
    /// CI: a deployment it was asked to take down ([`ServerMsg::Undeploy`]) is gone, or why it
    /// couldn't be (JSON `kiln_ci::wire::Undeployed`). Appended last: postcard encodes variants
    /// by index.
    Undeployed { json: String },
    /// What this runner can do, in 64 bits ([`AgentMsg::Build`]'s 32 ran out): the first 32 are
    /// `Build`'s, mirrored, and the rest new ([`CAP_WIRE`] on). Sent after `Build`; a runner from
    /// before this never sends it, and an older server skips it. Appended last: postcard encodes
    /// variants by index.
    Caps { caps: u64 },
    /// A frame of the channel between this runner and the server ([`crate::wire::channel`],
    /// docs/wire.md): numbered items carrying messages, their answers, and what each side has
    /// after a reconnect. Only once both sides announced [`CAP_WIRE`]. Appended last: postcard
    /// encodes variants by index.
    Wire(crate::wire::channel::Frame),
    /// The cluster group (docs/p2p.md, P2, on [`LANE_SYNC`]): `json` a
    /// `crate::store::learner::Msg`, `raft` the bytes of the hyper-raft message it carries (empty
    /// when none) from this runner's learner. Only from runners announcing [`CAP_CLUSTER`].
    /// Appended last: postcard encodes variants by index.
    Cluster { json: String, raft: Vec<u8> },
    /// What this runner's QUIC layer let go before it reached the wire since it started, by lane
    /// and reason (`crate::metrics::NetMetrics`): each supersedes the last. Sent with its samples
    /// once it changed, only to a server announcing [`CAP_NET_DROPS`]. Appended last: postcard
    /// encodes variants by index.
    NetDrops { dropped: Vec<NetDrop> },
    /// Agent session `session`, packed by this runner process for its machine's newer one (a
    /// take-over: [`ServerMsg::HarnessCmd`]'s local move), in pieces as
    /// [`ServerMsg::HarnessPackage`] carries them: the server passes them on to the newer process.
    /// On [`LANE_CONVO`]. Only to a server announcing [`CAP_HANDOVER`]. Appended last: postcard
    /// encodes variants by index.
    HarnessHandOver { session: String, offset: u64, data: Vec<u8>, done: bool },
    /// This runner process took over agent session `session` from an older one of its machine,
    /// or why it couldn't (`error`): the older one lets go of it, or goes on with it. Only to a
    /// server announcing [`CAP_HANDOVER`]. Appended last: postcard encodes variants by index.
    HarnessAdopted { session: String, error: Option<String> },
    /// CI: what this runner holds warm (JSON `kiln_ci::work::slots::WorkState`, docs/ci.md W24-R11,
    /// R18): each lane's workspaces and how long its legs took here, for placement. Sent once it
    /// changed, and every 10 minutes, each superseding the last; only to a server announcing
    /// [`CAP_WORK`]. Appended last: postcard encodes variants by index.
    WorkState { json: String },
    /// The most agent sessions this machine has at work at once, and where that comes from (JSON
    /// [`crate::harness::limit::Held`]: what a person set from the server, its config's, or kiln's
    /// default). Sent as it's welcomed and whenever it changes, each superseding the last; only to
    /// a server announcing [`CAP_AGENT_LIMIT`]. Appended last: postcard encodes variants by index.
    AgentLimit { json: String },

    /// An identity asked for (JSON [`IdentityAsk`], docs/ci.md W27-R01): a CSR for a key this
    /// runner process made and keeps (for itself, an agent session it holds at an epoch, a leg it
    /// runs); the server answers with [`ServerMsg::Identity`]. Only to a server announcing
    /// [`CAP_IDENTITY`]. Appended last: postcard encodes variants by index.
    Identity { json: String },
    /// Observe (`crate::observe`, on [`LANE_OBSERVE`]): `json` a [`crate::observe::wire::Msg`],
    /// `bytes` what it carries (a raft message's, or a write's entry; empty when none). Only from
    /// runners announcing [`CAP_OBSERVE`]. Appended last, after main's (deployed first): postcard
    /// encodes variants by index.
    Observe { json: String, bytes: Vec<u8> },
    /// This machine's headroom (JSON [`crate::headroom::Status`], docs/headroom.md): its targets'
    /// latencies, its window, what waits and what is paced. Sent on every (re)connect and when what
    /// it decides changes; only to a server announcing [`CAP_HEADROOM`]. Appended last: postcard
    /// encodes variants by index.
    HeadroomStatus { json: String },
    /// A frame of the server's this runner couldn't read (`crate::wire::unread`): on which wire,
    /// what it was (never what it said), why, how many more before it went untold (`folded`), and
    /// the agent session it was about, when that much of it could be read (`session`). The server
    /// notes it against this runner, and on that session's page too. Only to a server announcing
    /// [`CAP_UNREAD`]. Appended last, after identities', observe's and headroom's (`Identity`, `Observe`,
    /// `HeadroomStatus`): postcard encodes variants by index.
    Unread { wire: crate::wire::unread::Wire, what: String, code: crate::wire::unread::Code, why: String, folded: u64, session: Option<String> },
    /// Reach: a message on link `link` this runner couldn't read, from runner `from` (the link's
    /// other end): what it was (never what it said), a code and why. Its link ended. The server
    /// notes it against `from`, on this runner's page too. Only to a server announcing
    /// [`CAP_UNREAD`]. Appended last: postcard encodes variants by index.
    LinkUnread { from: String, link: String, what: String, code: crate::wire::unread::Code, why: String },
}

/// A leg's event as a runner sends it ([`AgentMsg::JobEvent`]'s JSON): the event, and its place in
/// its attempt (`seq`, from runners that send a leg's events again after a reconnect).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(flatten)]
    pub event: kiln_ci::wire::JobEventV2,
}

/// A secret that became available to a running leg ([`ServerMsg::SecretsFor`]'s JSON). Never
/// logged, so no `Debug`.
#[derive(Clone, Serialize, Deserialize)]
pub struct SecretFor {
    pub name: String,
    pub value: String,
}

/// Frames a QUIC layer let go of one lane, for one reason ([`AgentMsg::NetDrops`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct NetDrop {
    pub lane: String,
    pub reason: String,
    pub frames: u64,
}

/// [`AgentMsg::Build`] capability: the runner takes [`ServerMsg::Upgrade`].
pub const CAP_UPGRADE: u32 = 1;
/// [`AgentMsg::Build`] capability: the runner waits for a taken-over session's process to end
/// before it resumes the conversation (`Spec::takeover_pid`). Without it, take-over would race.
pub const CAP_TAKEOVER: u32 = 2;
/// [`AgentMsg::Build`] capability: the runner answers [`ServerMsg::DirList`] (lists its folders).
pub const CAP_DIRS: u32 = 4;
/// [`AgentMsg::Build`] capability: the runner ends a taken-over session that runs outside kiln
/// itself (`Spec::takeover_stop`), then resumes the conversation. Without it, such a session has to
/// be quit by hand first.
pub const CAP_TAKEOVER_STOP: u32 = 8;
/// [`AgentMsg::Build`] capability: the runner syncs repos ([`ServerMsg::Sync`], `crate::sync`).
pub const CAP_SYNC: u32 = 64;
/// [`AgentMsg::Build`] capability: the runner replicates the observe group and writes into it
/// ([`ServerMsg::Observe`], `crate::observe`).
pub const CAP_OBSERVE: u32 = 128;
/// [`AgentMsg::Build`] capability: the runner runs a front door ([`ServerMsg::FrontDoor`]: DNS for
/// a domain, HTTPS in front of the server, a `.local` name) and reports it with
/// [`AgentMsg::FrontDoorStatus`].
pub const CAP_FRONT_DOOR: u32 = 16;
/// [`AgentMsg::Build`] capability: the runner says which git repositories its machine holds and
/// clones, bundles and deletes them for the server's tasks (`crate::sync::repos`), inside
/// [`AgentMsg::Sync`] / [`ServerMsg::Sync`]. Clear of CI's bits and of `CAP_REACH` (1 << 17).
pub const CAP_REPOS: u32 = 1 << 18;
/// [`AgentMsg::Build`] capability: an outpost, kiln run on a machine where it isn't installed
/// (`kiln outpost`). It takes no jobs and the server never upgrades it (nothing there is installed
/// to upgrade); the machine's terminals, AI sessions and agents work as a runner's do. It also
/// carries [`OUTPOST_LABEL`], so it is known as one while it's away too.
pub const CAP_OUTPOST: u32 = 32;
/// [`AgentMsg::Build`] capability: it reports whether its machine takes upgrades
/// ([`AgentMsg::UpdatePrefs`]) and takes the notice of a newer kiln ([`ServerMsg::UpdateAvailable`]).
pub const CAP_UPDATES: u32 = 256;
/// [`AgentMsg::Build`] capability: it upgrades blue/green (`crate::bluegreen`): the new build comes
/// up beside it as another instance, reports in, and takes over. An upgrade of it is done when it
/// says so ([`AgentMsg::UpgradeDone`]), not when the new build reports in.
pub const CAP_BLUE_GREEN: u32 = 512;
/// The label an outpost carries (see [`CAP_OUTPOST`]).
pub const OUTPOST_LABEL: &str = "outpost";

// CI's capability bits (docs/ci.md §2.5) start at 1 << 10, clear of the rest of kiln's.
/// The runner takes [`ServerMsg::AssignV2`], sends [`AgentMsg::JobEvent`], runs the GitHub-exact
/// step runtime (docs/ci.md W06).
pub const CAP_CI2: u32 = 1 << 10;
/// The runner runs `uses:` actions (docs/ci.md W07).
pub const CAP_ACTIONS: u32 = 1 << 11;
/// The runner has a working Docker or Podman (job containers, services, Docker actions).
pub const CAP_DOCKER: u32 = 1 << 12;
/// The runner can boot Firecracker microVMs.
pub const CAP_FIRECRACKER: u32 = 1 << 13;
/// The runner can run shards microVMs.
pub const CAP_SHARDS: u32 = 1 << 14;
/// The runner sends [`AgentMsg::Facts`] (docs/ci.md W13).
pub const CAP_FACTS: u32 = 1 << 15;
/// The runner opens terminals into legs ([`ServerMsg::JobTerminalOpen`], docs/ci.md W17).
pub const CAP_JOB_TTY: u32 = 1 << 16;
/// The runner's Docker or Podman runs rootless, so untrusted legs may use it (docs/ci.md W18-R07).
/// Clear of `CAP_REACH`, `CAP_REPOS` and `CAP_CONVO`.
pub const CAP_DOCKER_ROOTLESS: u32 = 1 << 20;
/// The runner takes reach links ([`ServerMsg::Reach`], `crate::reach`).
pub const CAP_REACH: u32 = 1 << 17;
/// [`AgentMsg::Build`] capability: the runner keeps its agent sessions' conversations in a store
/// and sends it to the server's ([`AgentMsg::Convo`], `crate::convo::feed`), so the server keeps
/// them from that, not from the sessions' updates. Clear of CI's bits, `CAP_REPOS` and the bits
/// observe and updates take (128, 256, 512).
pub const CAP_CONVO: u32 = 1 << 19;
/// [`AgentMsg::Build`] capability: the runner looks at its own machine and repairs it
/// ([`ServerMsg::Diagnose`], [`ServerMsg::Repair`], `crate::doctor`).
pub const CAP_DOCTOR: u32 = 1 << 21;
/// [`AgentMsg::Build`] capability: the runner reads files for the people viewing its agent
/// sessions ([`ServerMsg::HarnessFileRead`], answered on [`LANE_FILES`]).
pub const CAP_FILES: u32 = 1 << 22;
/// [`AgentMsg::Build`] capability: the runner streams pages for the people viewing its agent
/// sessions (`crate::browser::stream`, [`ServerMsg::StreamOpen`], on [`LANE_STREAM`]).
pub const CAP_STREAM: u32 = 1 << 23;
/// [`AgentMsg::Build`] capability: the runner says what a person may open live from its sessions
/// and starts their projects' apps ([`ServerMsg::Preview`], `crate::browser::apps`).
pub const CAP_PREVIEW: u32 = 1 << 24;
/// [`AgentMsg::Build`] capability: the runner connects the server to ports its machine's processes
/// listen on ([`ServerMsg::PortOpen`], on [`LANE_PORTS`], `crate::browser::ports`).
pub const CAP_PORTS: u32 = 1 << 25;
/// [`AgentMsg::Build`] capability: the runner streams pages in other engines (Firefox, WebKit) and
/// laid out as devices (`crate::browser::driven`, `crate::browser::devices`): a stream's spec's
/// `engine` and `device`.
pub const CAP_ENGINES: u32 = 1 << 26;
/// [`AgentMsg::Build`] capability: the runner answers questions about its agents' folders and
/// pushes their work for review ([`ServerMsg::Workspace`], `crate::workspace`).
pub const CAP_WORKSPACE: u32 = 1 << 27;
/// The runner starts a pool's executors when asked ([`ServerMsg::Executor`], docs/ci.md W13-R09).
/// Clear of every bit above (1 << 21 is `CAP_DOCTOR`, 1 << 22 `CAP_FILES`, 1 << 23 `CAP_STREAM`,
/// 1 << 24 `CAP_PREVIEW`, 1 << 25 `CAP_PORTS`, 1 << 26 `CAP_ENGINES`, 1 << 27 `CAP_WORKSPACE`).
pub const CAP_POOLS: u32 = 1 << 28;
/// [`AgentMsg::Build`] capability: the runner keeps a job's result and its events (each with its
/// `seq`) until the server says it has the result ([`ServerMsg::Received`]), and sends them again
/// after a reconnect, so what was in flight as the connection dropped isn't lost.
pub const CAP_RECEIVED: u32 = 1 << 29;
/// The runner takes down a deployment it made when its environment goes ([`ServerMsg::Undeploy`],
/// docs/ci.md W15-R07), and says so ([`AgentMsg::Undeployed`]).
pub const CAP_UNDEPLOY: u32 = 1 << 30;
/// The runner holds a learner replica of the cluster group, kiln's records (docs/p2p.md, P2;
/// [`AgentMsg::Cluster`], [`ServerMsg::Cluster`]). The last bit of the 32.
pub const CAP_CLUSTER: u32 = 1 << 31;

/// [`AgentMsg::Caps`] and [`ServerMsg::Caps`] capability, the first past `Build`'s 32 bits: the
/// peer keeps the channel (`crate::wire::channel`, docs/wire.md). What must arrive goes as
/// numbered items, answered by the process that applied them and sent again after a reconnect.
pub const CAP_WIRE: u64 = 1 << 32;

/// [`ServerMsg::Caps`] capability: the server takes [`AgentMsg::NetDrops`], what a runner's QUIC
/// layer let go, for its page.
pub const CAP_NET_DROPS: u64 = 1 << 33;
/// [`AgentMsg::Caps`] capability: the runner's sessions have inboxes (CP-T3.2): it takes a message
/// as [`ServerMsg::HarnessSend`], for an epoch, and answers what became of it
/// (`crate::harness::core::Update::Inbox`). Past `Build`'s 32 bits, which ran out; a bit of its
/// own (1 << 33 is the server's `CAP_NET_DROPS`).
pub const CAP_INBOX: u64 = 1 << 34;

/// [`AgentMsg::Caps`] and [`ServerMsg::Caps`] capability: hand-overs between a machine's runner
/// processes. A runner hands an agent session to its machine's newer process when told to
/// ([`AgentMsg::HarnessHandOver`]) and says whether it took one over ([`AgentMsg::HarnessAdopted`]);
/// the server passes the session on and says how it went (`Command::HandedOver`, in a
/// [`ServerMsg::HarnessCmd`]). Bit 34 is T3's `CAP_INBOX` (docs/wire.md).
pub const CAP_HANDOVER: u64 = 1 << 35;

/// [`AgentMsg::Caps`] and [`ServerMsg::Caps`] capability: warm workspaces, for placement
/// (docs/ci.md W24-R18). A runner that says it tells the server what it holds warm
/// ([`AgentMsg::WorkState`]); a server that says it takes that.
pub const CAP_WORK: u64 = 1 << 36;

/// [`AgentMsg::Caps`] and [`ServerMsg::Caps`] capability: the most agent sessions a machine has at
/// work at once, set from the server (docs/agents.md, "How many agents a machine runs"). A runner that
/// says it takes [`ServerMsg::AgentLimit`] and says what it holds ([`AgentMsg::AgentLimit`]); a
/// server that says it takes that.
pub const CAP_AGENT_LIMIT: u64 = 1 << 37;

/// [`AgentMsg::Caps`] and [`ServerMsg::Caps`] capability: identities (docs/ci.md W27-R01). A runner
/// that says it asks for certificates for keys it makes and keeps ([`AgentMsg::Identity`]); a
/// server that says it issues them ([`ServerMsg::Identity`]).
pub const CAP_IDENTITY: u64 = 1 << 38;

/// [`AgentMsg::Caps`] and [`ServerMsg::Caps`] capability: headroom (docs/headroom.md). A runner
/// that says it keeps its machine's headroom from settings the server sends
/// ([`ServerMsg::Headroom`]) and reports it ([`AgentMsg::HeadroomStatus`]); a server that says it
/// takes that.
pub const CAP_HEADROOM: u64 = 1 << 39;

/// [`ServerMsg::Caps`] capability: the server takes a session's changeset as what changed in it
/// (`crate::harness::core::Update::ChangesPatch`), merged into its copy, so a runner sends the files
/// an agent touched rather than the whole changeset each time. Without it, a runner sends it whole.
pub const CAP_CHANGES_PATCH: u64 = 1 << 40;

/// What an [`AgentMsg::Identity`] asks for: a certificate for the key of `csr`, as `kind` says.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct IdentityAsk {
    /// `runner` (this process), `agent` (a session it holds, at `epoch`) or `leg` (one it runs).
    pub kind: String,
    pub csr: String,
    /// `agent`: the session; `leg`: the leg's id.
    pub of: Option<String>,
    /// `agent`: the epoch the key is for.
    pub epoch: Option<u64>,
}

/// The answer to an [`IdentityAsk`]: its kind and subject, and the certificate (PEM) or why not.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct IdentityGiven {
    pub kind: String,
    pub of: Option<String>,
    pub epoch: Option<u64>,
    /// The identity's kiln:// URI.
    pub uri: String,
    pub cert_pem: Option<String>,
    pub error: Option<String>,
}

/// [`AgentMsg::Caps`] and [`ServerMsg::Caps`] capability: the peer takes word that a frame of its
/// couldn't be read ([`AgentMsg::Unread`], [`ServerMsg::Unread`]; `crate::wire::unread`, docs/wire.md
/// "What can't be read"). One without it is told nothing new; what it sends that can't be read is
/// noted all the same. Bit 37 is the agent limit's [`CAP_AGENT_LIMIT`], 38 identities'
/// [`CAP_IDENTITY`], 39 headroom's [`CAP_HEADROOM`], 40 changesets' patches'
/// [`CAP_CHANGES_PATCH`].
pub const CAP_UNREAD: u64 = 1 << 41;

/// See [`AgentMsg::Instance`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InstanceHello {
    /// `main` for a machine's usual runner; a uuid for one set up beside it.
    pub id: String,
    /// When this instance was set up (unix seconds; 0 for `main`). The newest gets new work.
    pub created_at: i64,
    pub pid: u32,
}

/// A terminal session kept by an agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TermInfo {
    pub session: String,
    pub cols: u16,
    pub rows: u16,
    pub command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hello {
    pub protocol: u32,
    pub name: String,
    pub labels: Vec<String>,
    pub slots: u32,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub version: String,
    pub ephemeral: bool,
    /// Pod agents are launched for exactly one job.
    pub dedicated_job: Option<String>,
    /// Jobs the agent is still executing (after a reconnect).
    pub running: Vec<String>,
    pub terminal: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobResult {
    pub job: String,
    pub status: Status,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    pub peak_rss: Option<u64>,
    pub cpu_seconds: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ServerMsg {
    Welcome { server_version: String, sample_seconds: u32 },
    Refused { reason: String },
    Assign(Box<Assignment>),
    Cancel { job: String, reason: String },
    /// Stop accepting jobs and exit once idle (ephemeral agents, upgrades).
    Drain,
    TermOpen { session: String, cols: u16, rows: u16, command: Option<String> },
    TermInput { session: String, data: Vec<u8> },
    TermResize { session: String, cols: u16, rows: u16 },
    TermClose { session: String },
    /// Send the session's replay (TermReplay); live output keeps flowing as TermOutput.
    TermAttach { session: String },
    /// Send the recent messages of the AI session running as `pid` (answered by AiTranscript).
    /// Appended last: postcard encodes variants by index.
    AiTranscript { req: u64, pid: u32 },
    /// Start a kiln-native agent session under this id (JSON [`crate::harness::session::Spec`]).
    /// Appended last: postcard encodes variants by index.
    HarnessOpen { session: String, json: String },
    /// A command for an agent session (JSON [`crate::harness::model::Command`]).
    HarnessCmd { session: String, json: String },
    /// Stop an agent session for good.
    HarnessClose { session: String },
    /// Send the normalized view of the AI session running as `pid` (answered by AiView).
    /// Appended last: postcard encodes variants by index.
    AiView { req: u64, pid: u32 },
    /// A piece of a file attached to a message for an agent session (frames are bounded, files
    /// are not): written at `offset`; `done` on the last piece. The message refers to it by `file`.
    /// Appended last: postcard encodes variants by index.
    HarnessFile { session: String, file: String, offset: u64, data: Vec<u8>, done: bool },
    /// Sent to a machine's newest runner process: the older one, `instance` ("legacy" for one
    /// that never announced itself), has nothing left to run (no terminals, agent sessions or
    /// jobs) and can be stopped. Appended last: postcard encodes variants by index.
    Retire { instance: String },
    /// Upgrade this machine's runner without a restart: fetch the binary at `path` on the server
    /// (it must match `sha256` and `size`), then run its `install --agent --upgrade`, reporting
    /// with UpgradeLog / UpgradeDone under `job`. Only sent to runners that announced
    /// [`CAP_UPGRADE`]. Appended last: postcard encodes variants by index.
    Upgrade { job: String, path: String, sha256: String, size: u64 },
    /// Read the output of one of an agent session's tasks after byte `since` (its tail without
    /// one); answered by [`AgentMsg::HarnessTaskOutput`]. Appended last: postcard encodes
    /// variants by index.
    HarnessTaskOutput { req: u64, session: String, task: String, since: Option<u64> },
    /// List the folders at `path` on this machine (see `crate::dirs`); answered by
    /// [`AgentMsg::DirList`]. Only sent to runners that announced [`CAP_DIRS`]. Appended last:
    /// postcard encodes variants by index.
    DirList { req: u64, path: String },
    /// List this machine's past sessions (of one `kind`, or all), newest first, at most `limit`;
    /// answered by [`AgentMsg::HarnessHistory`]. Appended last: postcard encodes variants by
    /// index.
    HarnessHistory { req: u64, kind: Option<String>, limit: u32 },
    /// Repo sync (`crate::sync`, on [`LANE_SYNC`]): as [`AgentMsg::Sync`]. Only to runners that
    /// announced [`CAP_SYNC`]. Appended last: postcard encodes variants by index.
    Sync { json: String, raft: Vec<u8> },
    /// This machine's front door (JSON [`crate::frontdoor::Push`]): its settings, and a
    /// certificate the kiln CA issued for it or one uploaded for it. Sent when it connects and
    /// whenever that changes; only to runners that announced [`CAP_FRONT_DOOR`]. Appended last:
    /// postcard encodes variants by index.
    FrontDoor { json: String },
    /// CI (docs/ci.md §2.5): run this leg (JSON `AssignmentV2`). Only to runners announcing
    /// [`CAP_CI2`]; others keep getting [`ServerMsg::Assign`]. Appended last: postcard encodes
    /// variants by index.
    AssignV2 { json: String },
    /// CI: secrets that became available to a running leg (an environment's, once approved; JSON
    /// `[{name, value}]`). Appended last: postcard encodes variants by index.
    SecretsFor { job: String, json: String },
    /// CI: open a terminal inside a running or kept leg's environment (docs/ci.md W17); output
    /// flows as terminal output for `session`. Only to runners announcing [`CAP_JOB_TTY`].
    /// Appended last: postcard encodes variants by index.
    JobTerminalOpen { job: String, session: String, cols: u16, rows: u16 },
    /// CI: a leg kept after failing (docs/ci.md W17-R02) may be cleaned up now. Appended last:
    /// postcard encodes variants by index.
    KeepRelease { job: String },
    /// CI: send [`AgentMsg::Facts`] now. Only to runners announcing [`CAP_FACTS`]. Appended last:
    /// postcard encodes variants by index.
    ProbeFacts,
    /// Reach (`crate::reach`, on [`LANE_REACH`]): a frame of a link from runner `from` (the name
    /// its certificate carries), as [`AgentMsg::Reach`]. Only to runners announcing [`CAP_REACH`].
    /// Appended last: postcard encodes variants by index.
    Reach { from: String, link: String, open: bool, data: Vec<u8>, close: Option<String> },
    /// Reach: the answer to [`AgentMsg::ReachAsk`] (JSON). Appended last: postcard encodes
    /// variants by index.
    ReachAnswer { req: u64, json: String },
    /// CI: keep a kept leg `minutes` from now (docs/ci.md W17-R02, "Extend"). Only to runners
    /// announcing [`CAP_JOB_TTY`]. Appended last: postcard encodes variants by index.
    KeepExtend { job: String, minutes: u32 },
    /// CI: run step `step` of kept leg `job` again in place, as step `idx` of its log (docs/ci.md
    /// W17-R04). Only to runners announcing [`CAP_JOB_TTY`]. Appended last: postcard encodes
    /// variants by index.
    JobRerunStep { job: String, step: u32, idx: u32 },
    /// CI: run `command` in running or kept leg `job`'s shell (its workspace and environment, or
    /// its container), its output on terminal session `session` (docs/ci.md W17-R05). Only to
    /// runners announcing [`CAP_JOB_TTY`]. Appended last: postcard encodes variants by index.
    JobExec { job: String, session: String, command: String },
    /// The conversation feed (`crate::convo::feed`, on [`LANE_CONVO`]): as [`AgentMsg::Convo`].
    /// Only to runners announcing [`CAP_CONVO`]. Appended last: postcard encodes variants by
    /// index.
    Convo { json: String, data: Vec<u8> },
    /// A piece of a session's package (`crate::harness::moving::Package`, JSON) for it to go on on
    /// this machine, made from the server's conversation store (`crate::convo::resume`): its
    /// machine is gone. Written at `offset`; on the last piece (`done`) the runner takes the
    /// session over. On [`LANE_CONVO`]; only to runners announcing [`CAP_CONVO`]. Appended last:
    /// postcard encodes variants by index.
    HarnessPackage { session: String, offset: u64, data: Vec<u8>, done: bool },
    /// Let go of this session: it went on on another machine since (its copy here is from an
    /// earlier epoch). Its harness stops; it isn't closed. Only to runners announcing
    /// [`CAP_CONVO`]. Appended last: postcard encodes variants by index.
    HarnessRelease { session: String },
    /// A newer kiln is available for this machine's runner: `commit` (`build`, its binary for this
    /// platform, when there is one) and where it comes from. Said whatever its update policy, once a
    /// commit; only to runners announcing [`CAP_UPDATES`]. Appended last: postcard encodes variants
    /// by index.
    UpdateAvailable { commit: String, build: String, from: String },
    /// Look at this machine and say what's wrong (JSON `crate::doctor::Machine`), answered by
    /// [`AgentMsg::Answer`]. Only to runners announcing [`CAP_DOCTOR`]. Appended last: postcard
    /// encodes variants by index.
    Diagnose { req: u64 },
    /// Make repair `action` on this machine (`crate::doctor::RUNNER_ACTIONS`), answered by
    /// [`AgentMsg::Answer`] (`{message}` or `{error}`). Only to runners announcing [`CAP_DOCTOR`].
    /// Appended last: postcard encodes variants by index.
    Repair { req: u64, action: String },
    /// Read up to `len` bytes of `path` from `offset`, for whoever views agent session `session`
    /// (a file one of its messages mentions); answered by [`AgentMsg::HarnessFileData`]. Only to
    /// runners announcing [`CAP_FILES`]. Appended last: postcard encodes variants by index.
    HarnessFileRead { req: u64, session: String, path: String, offset: u64, len: u32 },
    /// Stream a page for whoever views agent session `session` (`json` a
    /// `crate::browser::stream::Spec`), as `stream`. Only to runners announcing [`CAP_STREAM`].
    /// Appended last: postcard encodes variants by index.
    StreamOpen { stream: String, session: String, json: String },
    /// What a viewer asks of stream `stream` (`crate::browser::stream::Stream::control`). Appended
    /// last: postcard encodes variants by index.
    StreamControl { stream: String, session: String, json: String },
    /// What a viewer did in stream `stream`: `crate::browser::stream::Input`s, in order, as JSON.
    /// Appended last: postcard encodes variants by index.
    StreamInput { stream: String, session: String, json: String },
    /// Close stream `stream`. Appended last: postcard encodes variants by index.
    StreamClose { stream: String, session: String },
    /// What a person opens live from agent session `session` (`crate::browser::apps`): `json` is
    /// `{"op": "look"}` (the ports the machine serves, the project's app), `{"op": "start"}` (its
    /// app, as its `.kiln/preview.yaml` says) or `{"op": "stop"}`; answered by
    /// [`AgentMsg::Answer`]. Only to runners announcing [`CAP_PREVIEW`]. Appended last: postcard
    /// encodes variants by index.
    Preview { req: u64, session: String, json: String },
    /// Connect connection `conn` to port `port` on agent session `session`'s machine (on loopback,
    /// a port its processes listen on: `crate::browser::ports`); answered by
    /// [`AgentMsg::PortOpened`]. Only to runners announcing [`CAP_PORTS`], on [`LANE_PORTS`].
    PortOpen { conn: u64, session: String, port: u16 },
    /// Bytes for connection `conn`'s port.
    PortData { conn: u64, session: String, data: Vec<u8> },
    /// The server took `bytes` more of what connection `conn`'s runner sent: it may send that many
    /// more (`crate::browser::ports::WINDOW`).
    PortAck { conn: u64, session: String, bytes: u64 },
    /// Connection `conn` is done with.
    PortClose { conn: u64, session: String },
    /// A question about agent session `session`'s folder (`json` a [`crate::workspace::Ask`]: its
    /// repository, or a commit of the agent's work pushed for review), answered by
    /// [`AgentMsg::Answer`]. Only to runners announcing [`CAP_WORKSPACE`]. Appended last: postcard
    /// encodes variants by index.
    Workspace { req: u64, session: String, json: String },
    /// CI: start or stop one of a pool's executors (JSON `crate::runner::pool::Ask`, docs/ci.md
    /// W13-R09). Only to runners announcing [`CAP_POOLS`]. Appended last: postcard encodes
    /// variants by index.
    Executor { json: String },
    /// The server has job `job`'s result ([`AgentMsg::JobFinished`]): the runner forgets it and
    /// the job's events. With no `job`, said as the server hears the runner's capabilities: it
    /// takes each event once (by its `seq`), so the runner may send them again after a reconnect.
    /// Only to runners announcing [`CAP_RECEIVED`]. Appended last: postcard encodes variants by
    /// index.
    Received { job: String },
    /// CI: take down a deployment this runner made (JSON `kiln_ci::wire::Undeploy`, docs/ci.md
    /// W15-R07): its environment, a preview, went. Only to runners announcing [`CAP_UNDEPLOY`].
    /// Appended last: postcard encodes variants by index.
    Undeploy { json: String },
    /// What this server can do, in 64 bits, as [`AgentMsg::Caps`]: sent after `Welcome` to a
    /// runner that announced [`CAP_WIRE`]. Appended last: postcard encodes variants by index.
    Caps { caps: u64 },
    /// A frame of the channel between the server and this runner, as [`AgentMsg::Wire`]. Appended
    /// last: postcard encodes variants by index.
    Wire(crate::wire::channel::Frame),
    /// The cluster group (docs/p2p.md, P2, on [`LANE_SYNC`]): as [`AgentMsg::Cluster`], to the
    /// runner's learner. Only to runners announcing [`CAP_CLUSTER`]. Appended last: postcard
    /// encodes variants by index.
    Cluster { json: String, raft: Vec<u8> },
    /// What the server's copy of each of this runner's agent sessions has: sent before `Welcome`
    /// to a runner that keeps the channel (as many of these as it takes), so a stream it starts
    /// (it started again, or the server lost its streams) sends only what the copy lacks
    /// (docs/wire.md, "Restarts"). Appended last: postcard encodes variants by index.
    Kept { sessions: Vec<KeptSession> },
    /// A message for agent session `session` (JSON [`crate::harness::model::Command`], a send or a
    /// skill run with its id), sent for the session's epoch `epoch` as the server last heard it: the
    /// session's inbox takes it once, refuses it if the session has left that epoch, and answers
    /// either way (CP-T3.2). Only to runners announcing [`CAP_INBOX`]. Appended last, after the
    /// wire's, the cluster group's and the restarts' `Kept` (CP-T3.2 landed after them): postcard
    /// encodes variants by index.
    HarnessSend { session: String, epoch: u64, json: String },
    /// What a person set from the server for the most agent sessions this machine has at work at
    /// once (JSON [`crate::harness::limit::Asked`]), over its config's; none set, its config's (or
    /// kiln's default) holds. Sent as the runner says [`CAP_AGENT_LIMIT`] and whenever it changes;
    /// the runner keeps it for its restarts. Appended last: postcard encodes variants by index.
    AgentLimit { json: String },

    /// The certificate for an [`AgentMsg::Identity`], or why there's none (JSON [`IdentityGiven`],
    /// docs/ci.md W27-R01). Only to runners announcing [`CAP_IDENTITY`]. Appended last: postcard
    /// encodes variants by index.
    Identity { json: String },
    /// Observe (`crate::observe`, on [`LANE_OBSERVE`]): as [`AgentMsg::Observe`]. Only to runners
    /// announcing [`CAP_OBSERVE`]. Appended last, after main's (deployed first): postcard encodes
    /// variants by index.
    Observe { json: String, bytes: Vec<u8> },
    /// This machine's headroom settings (JSON [`crate::headroom::Push`]): applied at once and kept
    /// for the runner's next start. Only to runners announcing [`CAP_HEADROOM`]. Appended last:
    /// postcard encodes variants by index.
    Headroom { json: String },
    /// A frame of this runner's the server couldn't read (`crate::wire::unread`): on which wire,
    /// what it was (never what it said), why, and how many more before it went untold (`folded`).
    /// Only to runners announcing [`CAP_UNREAD`]. Appended last, after identities', observe's and headroom's
    /// (`Identity`, `Observe`, `Headroom`): postcard encodes variants by index.
    Unread { wire: crate::wire::unread::Wire, what: String, code: crate::wire::unread::Code, why: String, folded: u64 },
    /// Reach (on [`LANE_REACH`]): a frame of this runner's the relay refused, about link `link` to
    /// runner `from`: why in a code from a closed set (`crate::reach::relay::Refusal`) and in words.
    /// To runners announcing [`CAP_UNREAD`], in place of [`ServerMsg::Reach`]'s `close`. Appended
    /// last: postcard encodes variants by index.
    ReachRefused { from: String, link: String, code: crate::reach::relay::Refusal, why: String },
}

/// What the server's copy of one agent session has of its events, said in digests
/// ([`ServerMsg::Kept`], `crate::wire::sums`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeptSession {
    pub session: String,
    /// The epoch its copy is of: a session that went on elsewhere since starts over whole.
    pub epoch: u64,
    /// Its newest event's number in the copy (0: it has none).
    pub seq: u64,
    /// Where its first run begins: its oldest event's number in the copy, or before it where a run
    /// of `run` begins (1, 1 + `run` …; an older server says its oldest event's number, and the
    /// runs begin there).
    pub from: u64,
    /// How many events a digest in `runs` covers.
    pub run: u64,
    /// A digest of each run of `run` events from `from` on (each of the copy's events numbered
    /// within it), up to the events `last` has one of each.
    pub runs: Vec<u64>,
    /// A digest of each of its newest events, through `seq` (0: the copy lacks that one).
    pub last: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Assignment {
    pub job_id: String,
    pub run_id: String,
    pub repo: String,
    pub clone_url: String,
    pub sha: String,
    pub branch: String,
    pub event: String,
    pub run_number: i64,
    pub attempt: u32,
    pub spec: PlannedJob,
    /// Already resolved secrets; masked from logs by the agent.
    pub secrets: Vec<(String, String)>,
    /// Bearer token scoped to this job for the source tarball and artifact upload endpoints.
    pub job_token: String,
    /// Base URL of the server HTTP API.
    pub server_url: String,
}

/// One chunk of output for a job's step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogChunk {
    pub job: String,
    pub idx: u32,
    pub data: Vec<u8>,
}

pub fn encode<T: Serialize>(v: &T) -> anyhow::Result<Vec<u8>> {
    postcard::to_allocvec(v).map_err(|e| anyhow::anyhow!("encode: {e}"))
}

/// Why a frame couldn't be read ([`read`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unreadable {
    /// Its variant's index is past the `known` variants this build has: a newer peer's.
    Unknown { index: u64, known: usize },
    /// It isn't one: cut short, bytes left over, or otherwise unreadable. `variant`: its variant's
    /// index, when that much could be read.
    Malformed { variant: Option<u64>, why: String },
}

impl Unreadable {
    pub fn code(&self) -> crate::wire::unread::Code {
        match self {
            Unreadable::Unknown { .. } => crate::wire::unread::Code::Unknown,
            Unreadable::Malformed { .. } => crate::wire::unread::Code::Malformed,
        }
    }

    /// What it was, as `crate::wire::unread::Unread` says it: its variant, never what it said.
    pub fn what(&self) -> String {
        match self {
            Unreadable::Unknown { index, .. } | Unreadable::Malformed { variant: Some(index), .. } => format!("variant {index}"),
            Unreadable::Malformed { variant: None, .. } => "a frame".to_owned(),
        }
    }

    /// Why, of a frame that was to be an `of`.
    pub fn why(&self, of: &str) -> String {
        match self {
            Unreadable::Unknown { known, .. } => format!("{of} has {known} variants"),
            Unreadable::Malformed { why, .. } => why.clone(),
        }
    }
}

/// A frame read whole into `T`, an enum of `variants()` variants (postcard), or why it couldn't
/// be: a variant this build doesn't know (its index past them), or bytes that aren't one, or that
/// are one with more after it.
pub fn read<'a, T: Deserialize<'a>>(b: &'a [u8], variants: impl FnOnce() -> usize) -> Result<T, Unreadable> {
    let variant = varint(b);
    match postcard::take_from_bytes::<T>(b) {
        Ok((m, [])) => Ok(m),
        Ok((_, rest)) => Err(Unreadable::Malformed { variant, why: format!("{} bytes left over", rest.len()) }),
        Err(e) => {
            let known = variants();
            match variant {
                Some(index) if usize::try_from(index).ok().is_none_or(|i| i >= known) => Err(Unreadable::Unknown { index, known }),
                _ => Err(Unreadable::Malformed { variant, why: unreadable(&e) }),
            }
        }
    }
}

/// A frame read whole into `T`, a message that isn't an enum ([`LogChunk`]), or why it couldn't be.
pub fn read_whole<'a, T: Deserialize<'a>>(b: &'a [u8]) -> Result<T, Unreadable> {
    match postcard::take_from_bytes::<T>(b) {
        Ok((m, [])) => Ok(m),
        Ok((_, rest)) => Err(Unreadable::Malformed { variant: None, why: format!("{} bytes left over", rest.len()) }),
        Err(e) => Err(Unreadable::Malformed { variant: None, why: unreadable(&e) }),
    }
}

/// A runner's frame, read ([`read`]).
pub fn read_agent(b: &[u8]) -> Result<AgentMsg, Unreadable> {
    read(b, || crate::wire::agent_classes().len())
}

/// The server's frame, read ([`read`]).
pub fn read_server(b: &[u8]) -> Result<ServerMsg, Unreadable> {
    read(b, || crate::wire::server_classes().len())
}

/// Why postcard couldn't read a frame, as an `Unread` says it.
fn unreadable(e: &postcard::Error) -> String {
    match e {
        postcard::Error::DeserializeUnexpectedEnd => "cut short".to_owned(),
        e => e.to_string(),
    }
}

/// The varint `b` starts with: a postcard enum's variant index, when it has one.
fn varint(b: &[u8]) -> Option<u64> {
    let mut v: u64 = 0;
    for (i, byte) in b.iter().take(10).enumerate() {
        v |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame as postcard reads it, bytes left over or not: what these tests compare with what was
    /// encoded. What the server and a runner read goes through `read` (`read_agent`, `read_server`).
    fn decode<'a, T: Deserialize<'a>>(b: &'a [u8]) -> anyhow::Result<T> {
        postcard::from_bytes(b).map_err(|e| anyhow::anyhow!("decode: {e}"))
    }

    #[test]
    fn roundtrip() -> anyhow::Result<()> {
        let m = AgentMsg::StepFinished {
            job: "j".into(),
            idx: 3,
            status: Status::Failed,
            exit_code: Some(2),
        };
        let b = encode(&m)?;
        assert_eq!(decode::<AgentMsg>(&b)?, m);
        assert!(decode::<AgentMsg>(&[0xff, 0xff, 0xff]).is_err());
        Ok(())
    }

    #[test]
    fn w05_r22_an_assignment_reaches_its_runner_whether_its_job_requires_anything_or_not() -> anyhow::Result<()> {
        // A job that requires nothing went out with its `requires` left out, and every runner
        // refused it ("Found an Option discriminant that wasn't 0 or 1"): no image built for hours.
        let p = crate::pipeline::parse("name: r\njobs:\n  any:\n    steps: [{run: x}]\n  linux:\n    requires: [\"os == linux\"]\n    steps: [{run: y}]\n")?;
        let jobs = crate::pipeline::plan(&p, &crate::pipeline::Context::default())?;
        assert_eq!(jobs.len(), 2);
        let assign = |spec: PlannedJob| {
            ServerMsg::Assign(Box::new(Assignment {
                job_id: "j".into(),
                run_id: "r".into(),
                repo: "o/r".into(),
                clone_url: String::new(),
                sha: "0".repeat(40),
                branch: "main".into(),
                event: "push".into(),
                run_number: 7,
                attempt: 0,
                spec,
                secrets: vec![("K".into(), "v".into())],
                job_token: "t".into(),
                server_url: "http://kiln".into(),
            }))
        };
        for spec in &jobs {
            let m = assign(spec.clone().for_runner(CAP_CI2));
            assert_eq!(decode::<ServerMsg>(&encode(&m)?)?, m);
            // The store keeps the job as JSON, and reads one kept before `requires` was.
            assert_eq!(&serde_json::from_str::<PlannedJob>(&serde_json::to_string(spec)?)?, spec);
            let mut before = serde_json::to_value(spec)?;
            if let Some(o) = before.as_object_mut() {
                o.remove("requires");
            }
            assert!(serde_json::from_value::<PlannedJob>(before)?.requires.is_empty());
        }
        // A runner from before the CI work (W22-AC09's mixed fleet, a kiln of edc9ae8) reads a job
        // that requires nothing as it always did: every field but `requires`, nothing left over.
        #[derive(Deserialize)]
        #[allow(dead_code, reason = "read only for its layout")]
        struct OldJob {
            key: String,
            spec_id: String,
            display: String,
            needs: Vec<String>,
            labels: Vec<String>,
            image: Option<String>,
            toolchain: Option<String>,
            env: std::collections::BTreeMap<String, String>,
            timeout_minutes: u32,
            retries: u32,
            exclusive: bool,
            resources: crate::pipeline::Resources,
            artifacts: Vec<String>,
            cache: Vec<String>,
            condition: Option<String>,
            steps: Vec<crate::pipeline::StepSpec>,
        }
        #[derive(Deserialize)]
        #[allow(dead_code, reason = "read only for its layout")]
        struct OldAssignment {
            job_id: String,
            run_id: String,
            repo: String,
            clone_url: String,
            sha: String,
            branch: String,
            event: String,
            run_number: i64,
            attempt: u32,
            spec: OldJob,
            secrets: Vec<(String, String)>,
            job_token: String,
            server_url: String,
        }
        let any = jobs.iter().find(|j| j.requires.is_empty()).cloned().ok_or_else(|| anyhow::anyhow!("no job that requires nothing"))?;
        let (old, new) = (encode(&assign(any.clone().for_runner(0)))?, encode(&assign(any.for_runner(CAP_CI2)))?);
        assert_eq!(old.first(), new.first(), "the same variant");
        let (a, rest) = postcard::take_from_bytes::<OldAssignment>(old.get(1..).unwrap_or_default())?;
        assert!(rest.is_empty(), "{} bytes past the assignment", rest.len());
        assert_eq!((a.spec.key.as_str(), a.spec.steps.len(), a.secrets.len(), a.server_url.as_str()), ("any", 1, 1, "http://kiln"));
        Ok(())
    }

    /// W05-R22, found by W28 step 3's replay: a job pod (`kiln agent --ephemeral`, v1's `[k8s]`
    /// jobs) never says what it can do, so it was written every job as for a runner from before the
    /// CI work, without `requires`, and read it as this build does: `rust` refused as malformed
    /// ("Found an Option discriminant that wasn't 0 or 1"), `ui` read with no steps and passed
    /// without running one. A pod reads its job whole; a bare-metal runner that hasn't said keeps
    /// the old layout (W22-AC09).
    #[test]
    fn w05_r22_a_job_pod_reads_its_assignment_whole() -> anyhow::Result<()> {
        let p = crate::pipeline::parse(concat!(
            "name: ci\njobs:\n",
            "  rust:\n    runs-on: [k8s]\n    toolchain: rust\n    resources: { cpu: \"2\", memory: 16Gi }\n",
            "    steps:\n      - { name: clippy, run: cargo clippy }\n      - { name: test, run: cargo test }\n",
            "  ui:\n    runs-on: [k8s]\n    toolchain: node\n    steps: [{run: npm ci}, {run: npm run lint}]\n",
            "  linux:\n    runs-on: [k8s]\n    requires: [\"os == linux\"]\n    steps: [{run: uname}]\n",
        ))?;
        let jobs = crate::pipeline::plan(&p, &crate::pipeline::Context::default())?;
        assert_eq!(jobs.len(), 3);
        let assign = |spec: PlannedJob| {
            ServerMsg::Assign(Box::new(Assignment {
                job_id: "j".into(),
                run_id: "r".into(),
                repo: "o/r".into(),
                clone_url: String::new(),
                sha: "0".repeat(40),
                branch: "main".into(),
                event: "push".into(),
                run_number: 7,
                attempt: 0,
                spec,
                secrets: vec![("K".into(), "v".into())],
                job_token: "t".into(),
                server_url: "http://kiln".into(),
            }))
        };
        for spec in &jobs {
            // What a pod's own decoder (this build's) reads of what the server writes it.
            let read = decode::<ServerMsg>(&encode(&assign(spec.clone().for_assignee(0, true)))?)?;
            let ServerMsg::Assign(a) = read else { anyhow::bail!("{} came back as another message", spec.key) };
            assert_eq!((&a.spec.key, &a.spec.steps, &a.spec.requires), (&spec.key, &spec.steps, &spec.requires), "{} as its pod reads it", spec.key);
            assert_eq!((a.secrets.len(), a.server_url.as_str()), (1, "http://kiln"), "{}: the fields after the job", spec.key);
            // What the pod got before: the layout of a runner from before the CI work.
            let before = decode::<ServerMsg>(&encode(&assign(spec.clone().for_runner(0)))?);
            assert!(!matches!(&before, Ok(m) if m == &assign(spec.clone())), "{}: read whole without `requires`", spec.key);
        }
        // A bare-metal runner that hasn't said what it can do is still written the old layout.
        let any = jobs.iter().find(|j| j.key == "ui").cloned().ok_or_else(|| anyhow::anyhow!("no ui"))?;
        assert_eq!(encode(&assign(any.clone().for_assignee(0, false)))?, encode(&assign(any.clone().for_runner(0)))?);
        assert_eq!(encode(&assign(any.clone().for_assignee(CAP_CI2, false)))?, encode(&assign(any.for_runner(CAP_CI2)))?);
        Ok(())
    }

    #[test]
    fn appended_variants_keep_their_indexes() -> anyhow::Result<()> {
        // Older peers decode by variant index: the new ones must come after every existing one.
        let i = encode(&AgentMsg::Instance(InstanceHello { id: "main".into(), created_at: 0, pid: 7 }))?;
        let h = encode(&AgentMsg::HarnessInventory { json: String::new() })?;
        assert!(i.first() > h.first(), "Instance must be appended after HarnessInventory");
        let r = encode(&ServerMsg::Retire { instance: "legacy".into() })?;
        let c = encode(&ServerMsg::HarnessClose { session: String::new() })?;
        assert!(r.first() > c.first(), "Retire must be appended after HarnessClose");
        assert_eq!(decode::<ServerMsg>(&r)?, ServerMsg::Retire { instance: "legacy".into() });
        let b = encode(&AgentMsg::Build { build: "abc".into(), caps: CAP_UPGRADE })?;
        assert!(b.first() > i.first(), "Build must be appended after Instance");
        let d = encode(&AgentMsg::UpgradeDone { job: "j".into(), ok: true, error: None })?;
        assert!(d.first() > b.first());
        let u = encode(&ServerMsg::Upgrade { job: "j".into(), path: "/dl/kiln/x".into(), sha256: "00".into(), size: 1 })?;
        assert!(u.first() > r.first(), "Upgrade must be appended after Retire");
        Ok(())
    }

    #[test]
    fn the_channel_is_appended_after_every_older_variant() -> anyhow::Result<()> {
        use crate::wire::channel::{Frame, Has, Nack, Refusal};
        // AgentMsg had 45 variants (Undeployed last, 44) and ServerMsg 53 (Undeploy last, 52):
        // an older peer skips what comes after.
        let undeployed = encode(&AgentMsg::Undeployed { json: String::new() })?;
        let undeploy = encode(&ServerMsg::Undeploy { json: String::new() })?;
        assert_eq!((undeployed.first(), undeploy.first()), (Some(&44), Some(&52)));
        let frames = [
            Frame::Item { stream: "cmd".into(), seq: 3, low: 2, body: vec![1, 2, 3] },
            Frame::Ack(Has { stream: "cmd".into(), upto: 3, nacks: vec![Nack { seq: 2, code: Refusal::Unknown, why: "gone".into() }] }),
            Frame::Want { stream: "cmd".into(), from: 2 },
            Frame::Have(vec![Has { stream: "s:x#1".into(), upto: 9, nacks: Vec::new() }]),
        ];
        let agent = [(AgentMsg::Caps { caps: CAP_WIRE | u64::from(CAP_UPGRADE) }, 45u8)].into_iter().chain(frames.iter().cloned().map(|f| (AgentMsg::Wire(f), 46)));
        for (m, idx) in agent {
            let b = encode(&m)?;
            assert_eq!(b.first(), Some(&idx), "{m:?} is AgentMsg variant {idx} on the wire");
            assert_eq!(decode::<AgentMsg>(&b)?, m);
            assert!(decode::<AgentMsg>(b.get(..b.len() - 1).unwrap_or_default()).is_err(), "a cut-off frame is an error");
        }
        let kept = ServerMsg::Kept { sessions: vec![KeptSession { session: "s".into(), epoch: 2, seq: 40, from: 1, run: 64, runs: Vec::new(), last: vec![7; 40] }] };
        let server = [(ServerMsg::Caps { caps: CAP_WIRE }, 53u8)].into_iter().chain(frames.into_iter().map(|f| (ServerMsg::Wire(f), 54))).chain([(kept, 56)]);
        for (m, idx) in server {
            let b = encode(&m)?;
            assert_eq!(b.first(), Some(&idx), "{m:?} is ServerMsg variant {idx} on the wire");
            assert_eq!(decode::<ServerMsg>(&b)?, m);
        }
        assert!(CAP_WIRE.is_power_of_two() && CAP_WIRE > u64::from(u32::MAX), "CAP_WIRE is past Build's 32 bits");
        Ok(())
    }

    #[test]
    fn hand_overs_are_appended_after_every_older_variant() -> anyhow::Result<()> {
        // AgentMsg ended with NetDrops (48): an older server never gets these (they go only to a
        // server announcing CAP_HANDOVER), and decodes the rest as before. ServerMsg gains nothing:
        // the slot after Kept is T3's HarnessSend (docs/wire.md).
        let drops = encode(&AgentMsg::NetDrops { dropped: Vec::new() })?;
        assert_eq!(drops.first(), Some(&48));
        let agent = [
            (AgentMsg::HarnessHandOver { session: "s".into(), offset: 3, data: vec![1, 2], done: true }, 49u8),
            (AgentMsg::HarnessAdopted { session: "s".into(), error: Some("no room".into()) }, 50),
        ];
        for (m, idx) in agent {
            let b = encode(&m)?;
            assert_eq!(b.first(), Some(&idx), "{m:?} is AgentMsg variant {idx} on the wire");
            assert_eq!(decode::<AgentMsg>(&b)?, m);
        }
        // Bit 34 is T3's CAP_INBOX.
        assert!(CAP_HANDOVER.is_power_of_two() && CAP_HANDOVER == 1 << 35 && CAP_HANDOVER & (CAP_WIRE | CAP_NET_DROPS) == 0);
        Ok(())
    }

    /// W24-R18: a runner's warm state comes after every older variant, and its bit is a new one.
    #[test]
    fn work_state_is_appended_after_the_hand_overs() -> anyhow::Result<()> {
        let adopted = encode(&AgentMsg::HarnessAdopted { session: "s".into(), error: None })?;
        let m = AgentMsg::WorkState { json: r#"{"lanes":[]}"#.into() };
        let b = encode(&m)?;
        assert_eq!(b.first().copied(), adopted.first().map(|i| i + 1), "AgentMsg::WorkState comes right after HarnessAdopted");
        assert_eq!(decode::<AgentMsg>(&b)?, m);
        // Bit 34 is T3's CAP_INBOX.
        assert!(CAP_WORK.is_power_of_two() && CAP_WORK == 1 << 36 && CAP_WORK & (CAP_WIRE | CAP_NET_DROPS | 1 << 34 | CAP_HANDOVER) == 0);
        Ok(())
    }

    /// A machine's agent limit comes after every older variant both ways (so a peer from before
    /// reads every older frame as it did), and its bit is a new one.
    #[test]
    fn agent_limits_are_appended_after_every_older_variant() -> anyhow::Result<()> {
        let work = encode(&AgentMsg::WorkState { json: "{}".into() })?;
        let held = AgentMsg::AgentLimit { json: r#"{"max_sessions":128,"from":"server","config":100}"#.into() };
        let b = encode(&held)?;
        assert_eq!(b.first().copied(), work.first().map(|i| i + 1), "AgentMsg::AgentLimit comes right after WorkState");
        assert_eq!(b.first(), Some(&52));
        assert_eq!(decode::<AgentMsg>(&b)?, held);
        let send = encode(&ServerMsg::HarnessSend { session: "s".into(), epoch: 1, json: "{}".into() })?;
        let asked = ServerMsg::AgentLimit { json: r#"{"max_sessions":128}"#.into() };
        let b = encode(&asked)?;
        assert_eq!(b.first().copied(), send.first().map(|i| i + 1), "ServerMsg::AgentLimit comes right after HarnessSend");
        assert_eq!(b.first(), Some(&58));
        assert_eq!(decode::<ServerMsg>(&b)?, asked);
        assert!(CAP_AGENT_LIMIT.is_power_of_two() && CAP_AGENT_LIMIT == 1 << 37 && CAP_AGENT_LIMIT & (CAP_WIRE | CAP_NET_DROPS | CAP_INBOX | CAP_HANDOVER | CAP_WORK) == 0);
        Ok(())
    }

    /// W27-R01: identities come after every older variant, and their bit is a new one.
    #[test]
    fn w27_r01_identities_are_appended_last() -> anyhow::Result<()> {
        let limit = encode(&AgentMsg::AgentLimit { json: String::new() })?;
        let asked = encode(&ServerMsg::AgentLimit { json: String::new() })?;
        let (a, s) = (AgentMsg::Identity { json: "{}".into() }, ServerMsg::Identity { json: "{}".into() });
        let (ab, sb) = (encode(&a)?, encode(&s)?);
        assert_eq!(ab.first().copied(), limit.first().map(|i| i + 1), "AgentMsg::Identity comes right after AgentLimit");
        assert_eq!(sb.first().copied(), asked.first().map(|i| i + 1), "ServerMsg::Identity comes right after AgentLimit");
        assert_eq!((decode::<AgentMsg>(&ab)?, decode::<ServerMsg>(&sb)?), (a, s));
        assert!(CAP_IDENTITY.is_power_of_two() && CAP_IDENTITY == 1 << 38 && CAP_IDENTITY & (CAP_WIRE | CAP_NET_DROPS | CAP_INBOX | CAP_HANDOVER | CAP_WORK | CAP_AGENT_LIMIT) == 0);
        Ok(())
    }

    /// Headroom (docs/headroom.md) comes after identities and observe, appended before it, and its
    /// bit is a new one: none reads another's messages as its own.
    #[test]
    fn headroom_is_appended_after_observe() -> anyhow::Result<()> {
        let agent = encode(&AgentMsg::Observe { json: String::new(), bytes: Vec::new() })?;
        let server = encode(&ServerMsg::Observe { json: String::new(), bytes: Vec::new() })?;
        let (a, s) = (AgentMsg::HeadroomStatus { json: "{}".into() }, ServerMsg::Headroom { json: "{}".into() });
        let (ab, sb) = (encode(&a)?, encode(&s)?);
        assert_eq!(ab.first().copied(), agent.first().map(|i| i + 1), "AgentMsg::HeadroomStatus comes right after Observe");
        assert_eq!(sb.first().copied(), server.first().map(|i| i + 1), "ServerMsg::Headroom comes right after Observe");
        assert_eq!((decode::<AgentMsg>(&ab)?, decode::<ServerMsg>(&sb)?), (a, s));
        assert!(CAP_HEADROOM.is_power_of_two() && CAP_HEADROOM == 1 << 39 && CAP_HEADROOM & (CAP_WIRE | CAP_NET_DROPS | CAP_INBOX | CAP_HANDOVER | CAP_WORK | CAP_AGENT_LIMIT | CAP_IDENTITY) == 0);
        Ok(())
    }

    /// A server that takes changesets as patches says so with a bit of its own: a runner never
    /// takes another capability for it and sends a server patches it can't read.
    #[test]
    fn the_changes_patch_bit_is_a_new_one() {
        let before = CAP_WIRE | CAP_NET_DROPS | CAP_INBOX | CAP_HANDOVER | CAP_WORK | CAP_AGENT_LIMIT | CAP_IDENTITY | CAP_HEADROOM;
        assert!(CAP_CHANGES_PATCH.is_power_of_two() && CAP_CHANGES_PATCH == 1 << 40 && CAP_CHANGES_PATCH & before == 0);
    }

    /// Typed messages, step 3: word that a frame couldn't be read is appended last in both, after
    /// the hand-overs', T3's, W24's, the agent limit's, identities', observe's and headroom's, behind
    /// a bit of its own.
    #[test]
    fn word_of_what_couldnt_be_read_is_appended_after_every_older_variant() -> anyhow::Result<()> {
        use crate::wire::unread::{Code, Wire};
        let up = AgentMsg::Unread { wire: Wire::Server, what: "variant 60".into(), code: Code::Unknown, why: "ServerMsg has 60 variants".into(), folded: 0, session: None };
        let down = ServerMsg::Unread { wire: Wire::Runner, what: "variant 54".into(), code: Code::Malformed, why: "cut short".into(), folded: 3 };
        let headroom = encode(&AgentMsg::HeadroomStatus { json: "{}".into() })?;
        assert_eq!(encode(&up)?.first().copied(), headroom.first().map(|i| i + 1), "AgentMsg::Unread comes right after HeadroomStatus");
        assert_eq!(encode(&up)?.first(), Some(&56));
        let headroom = encode(&ServerMsg::Headroom { json: "{}".into() })?;
        assert_eq!(encode(&down)?.first().copied(), headroom.first().map(|i| i + 1), "ServerMsg::Unread comes right after Headroom");
        assert_eq!(encode(&down)?.first(), Some(&62));
        assert_eq!(decode::<AgentMsg>(&encode(&up)?)?, up);
        assert_eq!(decode::<ServerMsg>(&encode(&down)?)?, down);
        // Step 5 appended `LinkUnread` (57) and `ReachRefused` (63) after them.
        assert_eq!((crate::wire::agent_classes().len(), crate::wire::server_classes().len()), (58, 64), "each table has every variant once");
        assert!(CAP_UNREAD.is_power_of_two() && CAP_UNREAD == 1 << 41 && CAP_UNREAD & (CAP_WIRE | CAP_NET_DROPS | CAP_INBOX | CAP_HANDOVER | CAP_WORK | CAP_AGENT_LIMIT | CAP_IDENTITY | CAP_HEADROOM | CAP_CHANGES_PATCH) == 0);
        Ok(())
    }

    /// Typed messages, step 5: what reach's links couldn't read, and the relay's typed refusals,
    /// are appended last, after step 3's `Unread`s.
    #[test]
    fn reachs_word_of_what_couldnt_be_read_is_appended_after_every_older_variant() -> anyhow::Result<()> {
        use crate::wire::unread::Code;
        let up = AgentMsg::LinkUnread { from: "mac".into(), link: "L1".into(), what: "variant 99".into(), code: Code::Unknown, why: "Ask has 12 variants".into() };
        let down = ServerMsg::ReachRefused { from: "linux".into(), link: "L1".into(), code: crate::reach::relay::Refusal::NotAccepted, why: "linux does not accept commands from other runners".into() };
        assert_eq!((encode(&up)?.first().copied(), encode(&down)?.first().copied()), (Some(57), Some(63)));
        assert_eq!(decode::<AgentMsg>(&encode(&up)?)?, up);
        assert_eq!(decode::<ServerMsg>(&encode(&down)?)?, down);
        Ok(())
    }

    /// A frame is read whole into its message, or said why not: a variant past the enum's is
    /// unknown (a newer peer's); cut short, or with bytes left over, it's malformed.
    #[test]
    fn a_frame_is_read_whole_or_said_why_not() -> anyhow::Result<()> {
        use crate::wire::unread::Code;
        let hello = encode(&AgentMsg::HarnessAdopted { session: "s".into(), error: None })?;
        assert!(read_agent(&hello).is_ok());
        // A newer runner's variant 200 (two bytes of varint), and one just past.
        let mut newer = vec![0xc8, 0x01];
        newer.extend_from_slice(&[1, 2, 3]);
        let (agents, servers) = (crate::wire::agent_classes().len(), crate::wire::server_classes().len());
        let e = read_agent(&newer).err().ok_or_else(|| anyhow::anyhow!("read"))?;
        assert_eq!((e.code(), e.what(), e.why("AgentMsg")), (Code::Unknown, "variant 200".to_owned(), format!("AgentMsg has {agents} variants")));
        assert_eq!(read_agent(&[u8::try_from(agents)?]).err().map(|e| e.code()), Some(Code::Unknown));
        assert_eq!(read_server(&[u8::try_from(servers)?, 0]).err().map(|e| e.code()), Some(Code::Unknown));
        // Cut short: a known variant whose fields aren't all there.
        let cut = hello.get(..hello.len().saturating_sub(1)).unwrap_or_default();
        let e = read_agent(cut).err().ok_or_else(|| anyhow::anyhow!("read"))?;
        assert_eq!((e.code(), e.why("AgentMsg")), (Code::Malformed, "cut short".to_owned()));
        // Bytes left over after a whole message.
        let mut more = hello.clone();
        more.extend_from_slice(&[7, 7]);
        let e = read_agent(&more).err().ok_or_else(|| anyhow::anyhow!("read"))?;
        assert_eq!((e.code(), e.why("AgentMsg")), (Code::Malformed, "2 bytes left over".to_owned()));
        // Nothing at all, and a message that isn't an enum.
        assert_eq!(read_agent(&[]).err().map(|e| (e.code(), e.what())), Some((Code::Malformed, "a frame".to_owned())));
        let chunk = encode(&LogChunk { job: "j".into(), idx: 1, data: vec![1] })?;
        assert!(read_whole::<LogChunk>(&chunk).is_ok());
        let mut over = chunk.clone();
        over.push(0);
        assert!(read_whole::<LogChunk>(&over).is_err_and(|e| e.code() == Code::Malformed));
        Ok(())
    }

    /// Whatever bytes come, reading them never panics: always a message, or why it isn't one.
    #[test]
    fn any_bytes_read_as_a_message_or_say_why_not() {
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let (mut read, mut refused) = (0u32, 0u32);
        for _ in 0..20_000 {
            let len = usize::try_from(next() % 64).unwrap_or(0);
            let bytes: Vec<u8> = (0..len).map(|_| (next() & 0xff) as u8).collect();
            for ok in [read_agent(&bytes).is_ok(), read_server(&bytes).is_ok()] {
                if ok { read += 1 } else { refused += 1 }
            }
        }
        assert!(refused > 0 && read + refused == 40_000);
    }

    #[test]
    fn the_cluster_group_is_appended_after_the_channel() -> anyhow::Result<()> {
        // After `Wire` in both (AgentMsg 46, ServerMsg 54), so a peer from before P2 skips it.
        let agent = AgentMsg::Cluster { json: r#"{"kind":"leave"}"#.into(), raft: vec![9, 8, 7] };
        let server = ServerMsg::Cluster { json: r#"{"kind":"raft"}"#.into(), raft: Vec::new() };
        let (a, s) = (encode(&agent)?, encode(&server)?);
        assert_eq!((a.first(), s.first()), (Some(&47), Some(&55)));
        assert_eq!((decode::<AgentMsg>(&a)?, decode::<ServerMsg>(&s)?), (agent, server));
        assert_eq!(CAP_CLUSTER.count_ones(), 1);
        assert_eq!(CAP_CLUSTER & CAP_UNDEPLOY, 0);
        assert_eq!(u64::from(CAP_CLUSTER) & CAP_WIRE, 0);
        Ok(())
    }

    #[test]
    fn what_a_runner_dropped_is_appended_after_the_cluster_group() -> anyhow::Result<()> {
        // After `Cluster` (47), so a server from before skips it; and only to one that takes it.
        let m = AgentMsg::NetDrops { dropped: vec![NetDrop { lane: lane_name(LANE_TERM).into(), reason: "shed".into(), frames: 3 }] };
        let b = encode(&m)?;
        assert_eq!(b.first(), Some(&48));
        assert_eq!(decode::<AgentMsg>(&b)?, m);
        assert!(CAP_NET_DROPS.is_power_of_two() && CAP_NET_DROPS & CAP_WIRE == 0 && CAP_NET_DROPS > u64::from(u32::MAX));
        assert_eq!((lane_name(LANE_CONTROL), lane_name(LANE_PORTS), lane_name(99)), ("control", "ports", "other"));
        Ok(())
    }

    #[test]
    fn dir_list_round_trips_after_every_older_variant() -> anyhow::Result<()> {
        let ask = ServerMsg::DirList { req: 7, path: "~/Projects".into() };
        let a = encode(&ask)?;
        assert_eq!(decode::<ServerMsg>(&a)?, ask);
        let t = encode(&ServerMsg::HarnessTaskOutput { req: 1, session: "s".into(), task: "t".into(), since: None })?;
        assert!(a.first() > t.first(), "ServerMsg::DirList must be appended after HarnessTaskOutput");
        assert_eq!(a.first(), Some(&19), "ServerMsg::DirList is variant 19 on the wire");

        let answer = AgentMsg::DirList { req: 7, json: r#"{"path":"~/Projects"}"#.into() };
        let b = encode(&answer)?;
        assert_eq!(decode::<AgentMsg>(&b)?, answer);
        let o = encode(&AgentMsg::HarnessTaskOutput { req: 1, json: None })?;
        assert!(b.first() > o.first(), "AgentMsg::DirList must be appended after HarnessTaskOutput");
        assert_eq!(b.first(), Some(&23), "AgentMsg::DirList is variant 23 on the wire");

        // A cut-off frame is an error, never a panic.
        assert!(decode::<ServerMsg>(a.get(..a.len() - 1).unwrap_or_default()).is_err());
        assert!(CAP_DIRS.is_power_of_two() && CAP_DIRS & (CAP_UPGRADE | CAP_TAKEOVER) == 0, "CAP_DIRS is a bit of its own");
        assert!(CAP_TAKEOVER_STOP.is_power_of_two() && CAP_TAKEOVER_STOP & (CAP_UPGRADE | CAP_TAKEOVER | CAP_DIRS) == 0, "CAP_TAKEOVER_STOP is a bit of its own");
        Ok(())
    }

    #[test]
    fn front_door_messages_are_appended_after_every_older_variant() -> anyhow::Result<()> {
        // ServerMsg::FrontDoor comes after Sync (variant 21, repo sync): an older runner skips it.
        let push = ServerMsg::FrontDoor { json: r#"{"settings":{"domain":"kiln.home"}}"#.into() };
        let p = encode(&push)?;
        assert_eq!(decode::<ServerMsg>(&p)?, push);
        let h = encode(&ServerMsg::HarnessHistory { req: 1, kind: None, limit: 5 })?;
        let sync = encode(&ServerMsg::Sync { json: String::new(), raft: Vec::new() })?;
        assert_eq!(h.first(), Some(&20), "ServerMsg::HarnessHistory is variant 20 on the wire");
        assert_eq!(sync.first(), Some(&21), "ServerMsg::Sync is variant 21 on the wire");
        assert_eq!(p.first(), Some(&22), "ServerMsg::FrontDoor is variant 22 on the wire, after Sync");

        // AgentMsg::FrontDoorStatus comes after Sync (variant 25): an older server skips it.
        let status = AgentMsg::FrontDoorStatus { json: r#"{"dns":{"state":"on"}}"#.into() };
        let s = encode(&status)?;
        assert_eq!(decode::<AgentMsg>(&s)?, status);
        let ah = encode(&AgentMsg::HarnessHistory { req: 1, json: None })?;
        let async_ = encode(&AgentMsg::Sync { json: String::new(), raft: Vec::new() })?;
        assert_eq!(ah.first(), Some(&24), "AgentMsg::HarnessHistory is variant 24 on the wire");
        assert_eq!(async_.first(), Some(&25), "AgentMsg::Sync is variant 25 on the wire");
        assert_eq!(s.first(), Some(&26), "AgentMsg::FrontDoorStatus is variant 26 on the wire, after Sync");

        // A cut-off frame is an error, never a panic.
        assert!(decode::<AgentMsg>(s.get(..s.len() - 1).unwrap_or_default()).is_err());
        assert!(
            CAP_FRONT_DOOR.is_power_of_two() && CAP_FRONT_DOOR & (CAP_UPGRADE | CAP_TAKEOVER | CAP_DIRS | CAP_TAKEOVER_STOP | CAP_SYNC) == 0,
            "CAP_FRONT_DOOR is a bit of its own"
        );
        assert_eq!(CAP_FRONT_DOOR, 16);
        Ok(())
    }

    #[test]
    fn ci_messages_are_appended_after_every_older_variant() -> anyhow::Result<()> {
        let server = [
            (ServerMsg::AssignV2 { json: "{}".into() }, 23u8),
            (ServerMsg::SecretsFor { job: "j".into(), json: "[]".into() }, 24),
            (ServerMsg::JobTerminalOpen { job: "j".into(), session: "s".into(), cols: 80, rows: 24 }, 25),
            (ServerMsg::KeepRelease { job: "j".into() }, 26),
            (ServerMsg::ProbeFacts, 27),
        ];
        for (m, idx) in server {
            let b = encode(&m)?;
            assert_eq!(b.first(), Some(&idx), "{m:?} is ServerMsg variant {idx} on the wire");
            assert_eq!(decode::<ServerMsg>(&b)?, m);
        }
        let agent = [(AgentMsg::JobEvent { job: "j".into(), json: "{}".into() }, 27u8), (AgentMsg::Facts { json: "{}".into() }, 28)];
        for (m, idx) in agent {
            let b = encode(&m)?;
            assert_eq!(b.first(), Some(&idx), "{m:?} is AgentMsg variant {idx} on the wire");
            assert_eq!(decode::<AgentMsg>(&b)?, m);
        }
        let ci = [CAP_CI2, CAP_ACTIONS, CAP_DOCKER, CAP_FIRECRACKER, CAP_SHARDS, CAP_FACTS, CAP_JOB_TTY];
        let others = CAP_UPGRADE | CAP_TAKEOVER | CAP_DIRS | CAP_TAKEOVER_STOP | CAP_SYNC | CAP_FRONT_DOOR | CAP_REPOS;
        assert!(CAP_REPOS.is_power_of_two() && CAP_REPOS & (CAP_UPGRADE | CAP_TAKEOVER | CAP_DIRS | CAP_TAKEOVER_STOP | CAP_SYNC | CAP_FRONT_DOOR) == 0);
        assert!(
            CAP_OUTPOST.is_power_of_two() && CAP_OUTPOST & (CAP_UPGRADE | CAP_TAKEOVER | CAP_DIRS | CAP_TAKEOVER_STOP | CAP_SYNC | CAP_FRONT_DOOR | CAP_REPOS) == 0,
            "CAP_OUTPOST is a bit of its own"
        );
        let mut seen = 0u32;
        for c in ci {
            assert!(c.is_power_of_two() && c & others == 0 && c & seen == 0, "{c} is a bit of its own");
            seen |= c;
        }
        Ok(())
    }

    #[test]
    fn a_diagnosis_is_appended_after_every_older_variant() -> anyhow::Result<()> {
        let last_server = encode(&ServerMsg::UpdateAvailable { commit: "c".into(), build: "b".into(), from: "f".into() })?;
        for (i, m) in [ServerMsg::Diagnose { req: 7 }, ServerMsg::Repair { req: 8, action: "prune-versions".into() }].into_iter().enumerate() {
            let b = encode(&m)?;
            assert_eq!(b.first().copied(), last_server.first().map(|n| n + 1 + i as u8), "{m:?} comes right after the ones before it");
            assert_eq!(decode::<ServerMsg>(&b)?, m);
        }
        let last_agent = encode(&AgentMsg::Power { json: "{}".into() })?;
        let answer = AgentMsg::Answer { req: 7, json: Some("{}".into()) };
        let b = encode(&answer)?;
        assert_eq!(b.first().copied(), last_agent.first().map(|i| i + 1), "AgentMsg::Answer comes right after Power");
        assert_eq!(decode::<AgentMsg>(&b)?, answer);
        Ok(())
    }

    #[test]
    fn reading_files_is_appended_after_every_older_variant() -> anyhow::Result<()> {
        let last_server = encode(&ServerMsg::UpdateAvailable { commit: "c".into(), build: "b".into(), from: "f".into() })?;
        let ask = ServerMsg::HarnessFileRead { req: 9, session: "s".into(), path: "/tmp/a.png".into(), offset: 1 << 20, len: 1 << 20 };
        let b = encode(&ask)?;
        assert_eq!(b.first().copied(), last_server.first().map(|n| n + 3), "HarnessFileRead comes right after Repair");
        assert_eq!(decode::<ServerMsg>(&b)?, ask);
        let last_agent = encode(&AgentMsg::Power { json: "{}".into() })?;
        let answer = AgentMsg::HarnessFileData { req: 9, json: "{}".into(), data: vec![0x89, b'P', b'N', b'G'] };
        let b = encode(&answer)?;
        assert_eq!(b.first().copied(), last_agent.first().map(|n| n + 2), "HarnessFileData comes right after Answer");
        assert_eq!(decode::<AgentMsg>(&b)?, answer);
        let taken = CAP_UPGRADE
            | CAP_TAKEOVER
            | CAP_DIRS
            | CAP_TAKEOVER_STOP
            | CAP_SYNC
            | CAP_FRONT_DOOR
            | CAP_REPOS
            | CAP_OUTPOST
            | CAP_UPDATES
            | CAP_BLUE_GREEN
            | CAP_CI2
            | CAP_ACTIONS
            | CAP_DOCKER
            | CAP_FIRECRACKER
            | CAP_SHARDS
            | CAP_FACTS
            | CAP_JOB_TTY
            | CAP_DOCKER_ROOTLESS
            | CAP_REACH
            | CAP_CONVO
            | CAP_DOCTOR
            | 128;
        assert!(CAP_FILES.is_power_of_two() && CAP_FILES & taken == 0, "CAP_FILES is a bit of its own");
        let lanes = [LANE_CONTROL, LANE_TERM, LANE_LOGS, LANE_SYNC, LANE_REACH, 5, LANE_CONVO];
        assert!(LANE_FILES < 8 && !lanes.contains(&LANE_FILES), "a lane of its own, within the eight a peer has");
        Ok(())
    }

    #[test]
    fn ports_are_appended_after_previews() -> anyhow::Result<()> {
        let last_server = encode(&ServerMsg::Preview { req: 3, session: "s".into(), json: "{}".into() })?;
        let server = [
            ServerMsg::PortOpen { conn: 1, session: "s".into(), port: 5173 },
            ServerMsg::PortData { conn: 1, session: "s".into(), data: b"GET / HTTP/1.1\r\n".to_vec() },
            ServerMsg::PortAck { conn: 1, session: "s".into(), bytes: 65536 },
            ServerMsg::PortClose { conn: 1, session: "s".into() },
        ];
        for (i, m) in server.into_iter().enumerate() {
            let b = encode(&m)?;
            assert_eq!(b.first().copied(), last_server.first().map(|n| n + 1 + u8::try_from(i).unwrap_or(0)), "{m:?} comes right after the ones before it");
            assert_eq!(decode::<ServerMsg>(&b)?, m);
        }
        let last_agent = encode(&AgentMsg::StreamEvent { stream: "v".into(), json: "{}".into() })?;
        let agent = [
            AgentMsg::PortOpened { conn: 1, error: None },
            AgentMsg::PortData { conn: 1, data: b"HTTP/1.1 200 OK\r\n".to_vec() },
            AgentMsg::PortAck { conn: 1, bytes: 9 },
            AgentMsg::PortClose { conn: 1, error: Some("reset".into()) },
        ];
        for (i, m) in agent.into_iter().enumerate() {
            let b = encode(&m)?;
            assert_eq!(b.first().copied(), last_agent.first().map(|n| n + 1 + u8::try_from(i).unwrap_or(0)), "{m:?} comes right after the ones before it");
            assert_eq!(decode::<AgentMsg>(&b)?, m);
        }
        assert!(CAP_PORTS.is_power_of_two() && CAP_PORTS > CAP_PREVIEW);
        assert!(CAP_ENGINES.is_power_of_two() && CAP_ENGINES > CAP_PORTS);
        assert!(LANE_PORTS == LANE_STREAM + 1 && LANE_PORTS < crate::net::limits(true).lanes_per_peer, "a lane of its own, within what a peer allows");
        Ok(())
    }

    /// CP-T3.2: a message for an epoch goes on the wire after everything there was before it, so a
    /// runner from before reads every older frame as it did.
    #[test]
    fn messages_for_an_epoch_are_appended_after_the_wires() -> anyhow::Result<()> {
        // After the wire's `Caps` (53) and `Wire` (54), the cluster group's `Cluster` (55) and the
        // restarts' `Kept` (56), which landed first.
        let kept = encode(&ServerMsg::Kept { sessions: Vec::new() })?;
        let m = ServerMsg::HarnessSend { session: "s".into(), epoch: 3, json: r#"{"type":"send","id":"m1","text":"hi"}"#.into() };
        let b = encode(&m)?;
        assert_eq!(b.first().copied(), kept.first().map(|n| n + 1), "it comes right after the ones before it");
        assert_eq!(b.first().copied(), Some(57));
        assert_eq!(decode::<ServerMsg>(&b)?, m);
        // Past `Build`'s 32 bits (the last of which is `CAP_CLUSTER`), clear of `CAP_WIRE`.
        assert!(CAP_INBOX.is_power_of_two() && CAP_INBOX > u64::from(u32::MAX) && CAP_INBOX & (CAP_WIRE | CAP_NET_DROPS) == 0);
        Ok(())
    }

    #[test]
    fn workspaces_are_appended_after_ports() -> anyhow::Result<()> {
        let last = encode(&ServerMsg::PortClose { conn: 1, session: "s".into() })?;
        let m = ServerMsg::Workspace { req: 4, session: "s".into(), json: r#"{"op":"git"}"#.into() };
        let b = encode(&m)?;
        assert_eq!(b.first().copied(), last.first().map(|n| n + 1), "it comes right after the ones before it");
        assert_eq!(decode::<ServerMsg>(&b)?, m);
        assert!(CAP_WORKSPACE.is_power_of_two() && CAP_WORKSPACE > CAP_ENGINES && CAP_ENGINES > CAP_PORTS);
        Ok(())
    }

    #[test]
    fn previews_are_appended_after_streams() -> anyhow::Result<()> {
        let last = encode(&ServerMsg::StreamClose { stream: "v".into(), session: "s".into() })?;
        let m = ServerMsg::Preview { req: 3, session: "s".into(), json: "{\"op\":\"look\"}".into() };
        let b = encode(&m)?;
        assert_eq!(b.first().copied(), last.first().map(|n| n + 1), "Preview comes right after StreamClose");
        assert_eq!(decode::<ServerMsg>(&b)?, m);
        assert!(CAP_PREVIEW.is_power_of_two() && CAP_PREVIEW > CAP_STREAM);
        Ok(())
    }

    #[test]
    fn streams_are_appended_after_every_older_variant() -> anyhow::Result<()> {
        let last_server = encode(&ServerMsg::HarnessFileRead { req: 1, session: "s".into(), path: "/p".into(), offset: 0, len: 1 })?;
        let after = [
            ServerMsg::StreamOpen { stream: "v".into(), session: "s".into(), json: "{}".into() },
            ServerMsg::StreamControl { stream: "v".into(), session: "s".into(), json: "{}".into() },
            ServerMsg::StreamInput { stream: "v".into(), session: "s".into(), json: "[]".into() },
            ServerMsg::StreamClose { stream: "v".into(), session: "s".into() },
        ];
        for (i, m) in after.into_iter().enumerate() {
            let b = encode(&m)?;
            assert_eq!(b.first().copied(), last_server.first().map(|n| n + 1 + u8::try_from(i).unwrap_or(0)), "{m:?} comes right after the ones before it");
            assert_eq!(decode::<ServerMsg>(&b)?, m);
        }
        let last_agent = encode(&AgentMsg::HarnessFileData { req: 1, json: "{}".into(), data: vec![] })?;
        for (i, m) in [AgentMsg::StreamFrame { stream: "v".into(), data: vec![b'v', b'k'] }, AgentMsg::StreamEvent { stream: "v".into(), json: "{}".into() }].into_iter().enumerate() {
            let b = encode(&m)?;
            assert_eq!(b.first().copied(), last_agent.first().map(|n| n + 1 + u8::try_from(i).unwrap_or(0)), "{m:?} comes right after the ones before it");
            assert_eq!(decode::<AgentMsg>(&b)?, m);
        }
        assert!(CAP_STREAM.is_power_of_two() && CAP_STREAM > CAP_FILES && CAP_STREAM < 1 << 24);
        assert!(LANE_STREAM == 8 && LANE_STREAM < crate::net::limits(true).lanes_per_peer, "a lane past the first eight, within what a peer allows");
        Ok(())
    }

    #[test]
    fn pool_executors_are_appended_after_every_older_variant() -> anyhow::Result<()> {
        let last_server = encode(&ServerMsg::Workspace { req: 1, session: "s".into(), json: "{}".into() })?;
        let start = ServerMsg::Executor { json: "{}".into() };
        let b = encode(&start)?;
        assert_eq!(b.first().copied(), last_server.first().map(|n| n + 1), "ServerMsg::Executor comes right after Workspace");
        assert_eq!(decode::<ServerMsg>(&b)?, start);
        let last_agent = encode(&AgentMsg::PortClose { conn: 1, error: None })?;
        let said = AgentMsg::Executor { json: "{}".into() };
        let b = encode(&said)?;
        assert_eq!(b.first().copied(), last_agent.first().map(|n| n + 1), "AgentMsg::Executor comes right after PortClose");
        assert_eq!(decode::<AgentMsg>(&b)?, said);
        let others = CAP_WORKSPACE | CAP_ENGINES | CAP_PORTS | CAP_PREVIEW | CAP_STREAM | CAP_FILES | CAP_DOCTOR | CAP_CONVO | CAP_DOCKER_ROOTLESS | CAP_REACH | CAP_REPOS | CAP_JOB_TTY | CAP_FACTS | CAP_SHARDS | CAP_FIRECRACKER | CAP_DOCKER | CAP_ACTIONS | CAP_CI2;
        assert!(CAP_POOLS.is_power_of_two() && CAP_POOLS & others == 0, "CAP_POOLS is a bit of its own");
        Ok(())
    }

    #[test]
    fn receipts_are_appended_after_pool_executors() -> anyhow::Result<()> {
        let last = encode(&ServerMsg::Executor { json: "{}".into() })?;
        let got = ServerMsg::Received { job: "j".into() };
        let b = encode(&got)?;
        assert_eq!(b.first().copied(), last.first().map(|n| n + 1), "ServerMsg::Received comes right after Executor");
        assert_eq!(decode::<ServerMsg>(&b)?, got);
        assert!(CAP_RECEIVED.is_power_of_two() && CAP_RECEIVED > CAP_POOLS);
        Ok(())
    }

    #[test]
    fn undeploys_are_appended_after_receipts_and_executors() -> anyhow::Result<()> {
        let last_server = encode(&ServerMsg::Received { job: "j".into() })?;
        let ask = ServerMsg::Undeploy { json: "{}".into() };
        let b = encode(&ask)?;
        assert_eq!(b.first().copied(), last_server.first().map(|n| n + 1), "ServerMsg::Undeploy comes right after Received");
        assert_eq!(decode::<ServerMsg>(&b)?, ask);
        let last_agent = encode(&AgentMsg::Executor { json: "{}".into() })?;
        let said = AgentMsg::Undeployed { json: "{}".into() };
        let b = encode(&said)?;
        assert_eq!(b.first().copied(), last_agent.first().map(|n| n + 1), "AgentMsg::Undeployed comes right after Executor");
        assert_eq!(decode::<AgentMsg>(&b)?, said);
        assert!(CAP_UNDEPLOY.is_power_of_two() && CAP_UNDEPLOY > CAP_RECEIVED);
        Ok(())
    }

    #[test]
    fn observe_messages_are_appended_after_every_older_variant() -> anyhow::Result<()> {
        // Right after main's last (deployed first): a machine's limit on agents at work, each way.
        let last_server = encode(&ServerMsg::Identity { json: "{}".into() })?;
        let s = ServerMsg::Observe { json: "{}".into(), bytes: vec![1, 2] };
        let b = encode(&s)?;
        assert_eq!(b.first().copied(), last_server.first().map(|n| n + 1), "ServerMsg::Observe comes right after Identity");
        assert_eq!(decode::<ServerMsg>(&b)?, s);
        let last_agent = encode(&AgentMsg::Identity { json: "{}".into() })?;
        let a = AgentMsg::Observe { json: "{}".into(), bytes: vec![3] };
        let b = encode(&a)?;
        assert_eq!(b.first().copied(), last_agent.first().map(|n| n + 1), "AgentMsg::Observe comes right after Identity");
        assert_eq!(decode::<AgentMsg>(&b)?, a);
        let every = [
            CAP_UPGRADE,
            CAP_TAKEOVER,
            CAP_DIRS,
            CAP_TAKEOVER_STOP,
            CAP_SYNC,
            CAP_FRONT_DOOR,
            CAP_OUTPOST,
            CAP_REPOS,
            CAP_UPDATES,
            CAP_BLUE_GREEN,
            CAP_CI2,
            CAP_ACTIONS,
            CAP_DOCKER,
            CAP_FIRECRACKER,
            CAP_SHARDS,
            CAP_FACTS,
            CAP_JOB_TTY,
            CAP_DOCKER_ROOTLESS,
            CAP_REACH,
            CAP_CONVO,
            CAP_DOCTOR,
            CAP_FILES,
            CAP_STREAM,
            CAP_PREVIEW,
            CAP_PORTS,
            CAP_ENGINES,
            CAP_WORKSPACE,
            CAP_POOLS,
            CAP_RECEIVED,
            CAP_UNDEPLOY,
            CAP_CLUSTER,
        ];
        assert!(CAP_OBSERVE.is_power_of_two() && every.iter().all(|c| c & CAP_OBSERVE == 0), "CAP_OBSERVE is a bit of its own");
        const { assert!(LANE_OBSERVE != LANE_CONVO && LANE_OBSERVE != LANE_FILES && LANE_OBSERVE != LANE_STREAM && LANE_OBSERVE != LANE_PORTS) };
        Ok(())
    }

    #[test]
    fn the_conversation_feed_is_appended_after_every_older_variant() -> anyhow::Result<()> {
        let last_server = encode(&ServerMsg::JobExec { job: "j".into(), session: "s".into(), command: "c".into() })?;
        let after = [
            ServerMsg::Convo { json: "{}".into(), data: vec![1, 2] },
            ServerMsg::HarnessPackage { session: "s".into(), offset: 3, data: vec![4], done: true },
            ServerMsg::HarnessRelease { session: "s".into() },
        ];
        for (i, m) in after.into_iter().enumerate() {
            let b = encode(&m)?;
            assert_eq!(b.first().copied(), last_server.first().map(|n| n + 1 + i as u8), "{m:?} comes right after the ones before it");
            assert_eq!(decode::<ServerMsg>(&b)?, m);
        }
        let last_agent = encode(&AgentMsg::ReachAsk { req: 1, json: "{}".into() })?;
        let convo = AgentMsg::Convo { json: "{}".into(), data: Vec::new() };
        let b = encode(&convo)?;
        assert_eq!(b.first().copied(), last_agent.first().map(|i| i + 1), "AgentMsg::Convo comes right after ReachAsk");
        assert_eq!(decode::<AgentMsg>(&b)?, convo);
        let all = [CAP_UPGRADE, CAP_TAKEOVER, CAP_DIRS, CAP_TAKEOVER_STOP, CAP_SYNC, CAP_FRONT_DOOR, CAP_REPOS, CAP_OUTPOST, CAP_CI2, CAP_ACTIONS, CAP_DOCKER, CAP_FIRECRACKER, CAP_SHARDS, CAP_FACTS, CAP_JOB_TTY, CAP_REACH];
        assert!(CAP_CONVO.is_power_of_two() && all.iter().all(|c| c & CAP_CONVO == 0), "CAP_CONVO is a bit of its own");
        assert!(CAP_DOCKER_ROOTLESS.is_power_of_two() && all.iter().chain([&CAP_CONVO]).all(|c| c & CAP_DOCKER_ROOTLESS == 0), "CAP_DOCKER_ROOTLESS is a bit of its own");
        // Observe's and updates' bits, taken elsewhere; a lane of its own, within the eight a peer has.
        const { assert!(CAP_CONVO & (128 | 256 | 512) == 0) };
        const { assert!(LANE_CONVO > LANE_REACH && LANE_CONVO != 5 && LANE_CONVO < 8) };
        Ok(())
    }
}
