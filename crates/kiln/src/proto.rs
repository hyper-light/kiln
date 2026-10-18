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
/// [`AgentMsg::Build`] capability: the runner runs a front door ([`ServerMsg::FrontDoor`]: DNS for
/// a domain, HTTPS in front of the server, a `.local` name) and reports it with
/// [`AgentMsg::FrontDoorStatus`].
pub const CAP_FRONT_DOOR: u32 = 16;
/// [`AgentMsg::Build`] capability: the runner says which git repositories its machine holds and
/// clones, bundles and deletes them for the server's tasks (`crate::sync::repos`), inside
/// [`AgentMsg::Sync`] / [`ServerMsg::Sync`]. Clear of CI's bits and of `CAP_REACH` (1 << 17).
pub const CAP_REPOS: u32 = 1 << 18;

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
/// The runner takes reach links ([`ServerMsg::Reach`], `crate::reach`).
pub const CAP_REACH: u32 = 1 << 17;

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

pub fn decode<'a, T: Deserialize<'a>>(b: &'a [u8]) -> anyhow::Result<T> {
    postcard::from_bytes(b).map_err(|e| anyhow::anyhow!("decode: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut seen = 0u32;
        for c in ci {
            assert!(c.is_power_of_two() && c & others == 0 && c & seen == 0, "{c} is a bit of its own");
            seen |= c;
        }
        Ok(())
    }
}
