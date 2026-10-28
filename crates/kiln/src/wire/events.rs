//! What goes down the UI's events socket (`GET /api/events`, docs/wire.md, "The UI's sockets"):
//! [`ServerEvent`], one variant for each frame [`crate::hub::Hub::emit`] sends, tagged `type`.
//! The server builds every frame from it, and the UI reads it as the TypeScript `super::ts` makes
//! of it. A frame that only says something changed (`updates`, `places` ...) carries nothing: its
//! page fetches again.
//!
//! The socket's frames are also what kiln's own listeners read (attachments, notifications,
//! webhooks' subscribers), as the JSON they serialize to.

use serde::Serialize;
use serde_json::Value;

/// A frame of the events socket.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    /// The socket fell behind and dropped frames: fetch everything again.
    Resync,
    /// A run changed.
    Run { run: Box<crate::store::Run> },
    /// A leg changed (without its spec).
    Job { job: Box<crate::store::Job> },
    /// A runner changed.
    Agent { agent: Box<crate::hub::AgentLive> },
    /// A machine's host sample (`server` for the server's own).
    Sample { agent: String, sample: Box<crate::metrics::HostSample> },
    /// The queue: both shapes share the tag `queue`. The scheduler's counts ([`Queue::Counts`]), or
    /// where a land queue's entry is ([`Queue::Entry`]).
    Queue(Queue),
    /// The terminal sessions changed: refetch them.
    Terminals,
    /// System → Updates changed: refetch GET /api/updates.
    Updates,
    /// What machines hold, or a task moving a repository between them, changed: refetch GET /api/places.
    Places,
    /// The discovery table changed: refetch GET /api/discovery.
    Discovery,
    /// Join requests changed: refetch GET /api/join-requests.
    JoinRequests,
    /// The tailnet's DNS for the front doors changed: refetch GET /api/tailscale.
    Tailscale,
    /// Your notifications changed (a device, a mute, a delivery): refetch GET /api/push.
    Push,
    /// The peers changed: refetch them.
    Peers,
    /// The rules for the copies runners keep of other runners' trees, or what a runner holds of
    /// them, changed: refetch GET /api/reach/keep.
    Keep,
    /// A machine on battery ran low.
    #[serde(rename = "machine.battery_low")]
    MachineBatteryLow { machine: String, battery: crate::os::battery::Battery, text: String },
    /// A machine's AI sessions changed: refetch GET /api/ai-sessions.
    AiSessions { machine: String },
    /// An upgrade moved on (new output, or it finished).
    UpdateJob { job: Box<crate::updates::Job> },
    /// A harness account's sign-in moved on.
    AccountFlow { flow: Box<crate::accounts::Flow> },
    /// A run's plan stack changed.
    PlanStack { run: String },
    /// A comment on a diff was published.
    Comment(Box<Comment>),
    /// A review of a diff was given.
    Review(Box<Review>),
    /// A run of a diff's version moved on.
    DiffRun(Box<DiffRun>),
    /// An attachment changed, or went: both shapes share the tag `attachment`.
    Attachment(AttachmentChange),
    /// An incident changed.
    Incident { incident: Box<IncidentBrief> },
    /// A preview environment was taken down.
    #[serde(rename = "preview.removed")]
    PreviewRemoved { repo: String, environment: String, why: String },
    /// A land queue landed its batch on a trunk.
    Landed(Box<Landed>),
    /// A leg built an image into kiln's registry.
    CiImage { repo: String, name: String, digest: String, platforms: Vec<String> },
    /// An experiment changed.
    Experiment { repo: String, experiment: Box<ExperimentBrief> },
    /// A person's inbox changed.
    Inbox { person: String },
    /// A package was published to kiln's registry, or taken out.
    RegistryPackage { action: String, name: String, tag: String, digest: String, by: String },
    /// A release moved on.
    Deploy(Box<DeployRelease>),
    /// An app's registration in an environment changed, or went: both shapes share the tag
    /// `deploy_registration`.
    DeployRegistration(RegistrationChange),
    /// An environment's move between providers moved on.
    DeployMove {
        #[serde(rename = "move")]
        moving: Box<crate::deploy::moves::Move>,
    },
    /// A runner's front door changed: refetch it.
    FrontDoor { runner: String },
    /// How many agent sessions a runner has at work at once changed (set here, or what it said it
    /// holds): refetch it.
    AgentLimit { runner: String },
    /// An agent's change request opened or moved on.
    AgentChange { agent: String, change: Box<crate::workspace::change::Record> },
    /// An agent session kiln runs appeared or changed.
    Harness { info: Box<crate::harness::session::Info> },
    /// An agent session kiln ran is gone.
    HarnessClosed { id: String },
    /// Someone marked notices.
    Notices(Notices),
    /// A program changed.
    Program { program: String },
    /// A stack's deploy plan was made.
    CdPlan(CdPlan),
    /// A rollout of an app's object on a target moved on: refetch GET /api/cd/apps/:app/rollout.
    CdRollout { app: String, id: String },
    /// An app was deleted.
    CdAppDeleted { app: String },
    /// Apps' statuses changed (batched, at most every 300 ms).
    CdApps { apps: Vec<CdStatus> },
    /// A runner went away and hasn't come back.
    #[serde(rename = "runner.offline")]
    RunnerOffline { runner: OfflineRunner },
    /// A change's conflicts changed (W26-R12): its replay found some, one was resolved or taken
    /// back, or their resolution was committed.
    Conflict(Box<ConflictChange>),
    /// Something happened to a run or a diff that notification rules hear (`run.failed`,
    /// `mention` ...): the tag is its [`NoticeKind`].
    #[serde(untagged)]
    RunNotice(Box<RunNotice>),
    /// A deploy of kiln itself didn't simply take: the tag is its [`SelfDeployKind`].
    #[serde(untagged)]
    SelfDeploy(SelfDeployNotice),
    /// An agent needs you, finished, stopped or paused: the tag is its [`AgentNoticeKind`].
    #[serde(untagged)]
    AgentNotice(Box<AgentNotice>),
}

impl ServerEvent {
    /// The frame as its socket sends it.
    pub fn text(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// The frame as JSON, for kiln's own listeners that read it so (webhooks, rules).
    pub fn value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// The two frames tagged `queue`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(untagged)]
pub enum Queue {
    /// The scheduler's queued and running legs.
    Counts { queued: i64, running: i64 },
    /// Where a land queue's entry is.
    Entry(LandEntry),
}

/// A land queue's entry: `queued`, `testing`, `landed`, `ejected` or `cancelled`, with its diffs
/// and whose they are.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct LandEntry {
    pub entry: String,
    pub state: String,
    pub reason: Option<String>,
    pub strategy: String,
    pub diffs: Vec<LandDiff>,
    pub enqueued_by: String,
}

/// A diff of a land queue's entry.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct LandDiff {
    pub diff: String,
    pub repo: String,
    pub author: String,
    pub author_for: Option<String>,
}

/// A land queue's batch, landed.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Landed {
    /// The repository's id.
    pub repo: String,
    pub repo_name: String,
    pub trunk: String,
    pub tip: String,
    pub diffs: Vec<String>,
    /// Who queued each entry.
    pub by: Vec<String>,
}

/// A published comment on a diff.
#[derive(Debug, Clone, Default, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Comment {
    pub repo: String,
    pub diff: String,
    pub version: u32,
    pub thread: String,
    pub comment: String,
    pub author: String,
    pub body: String,
    pub file: Option<String>,
    pub side: String,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
    pub diff_author: String,
    pub diff_author_for: Option<String>,
}

/// A change's conflicts, changed (W26-R12): a replay of `version` found them (`state`
/// `conflicted`); one of them, `conflict`, was resolved, taking `take` (main, this, both, agent,
/// edit), or taken back (no `take`), by `by` (`agent` when an agent's, kept for a person to take);
/// or their resolution was committed (`state` `resolved`, `version` the revision it made).
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ConflictChange {
    pub repo: String,
    pub diff: String,
    pub replay: String,
    pub version: Option<u32>,
    pub state: Option<String>,
    pub conflict: Option<String>,
    pub take: Option<String>,
    pub by: Option<String>,
    pub agent: Option<bool>,
}

/// A review of a diff.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Review {
    pub repo: String,
    pub diff: String,
    pub version: u32,
    pub reviewer: String,
    pub action: String,
    pub body: String,
    /// How many drafted comments it published.
    pub published: usize,
    pub state: String,
    pub diff_author: String,
    pub diff_author_for: Option<String>,
}

/// A run of a diff's version.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "DiffRunEvent"))]
pub struct DiffRun {
    pub repo: String,
    pub diff: String,
    pub version: u32,
    pub run: String,
    pub workflow: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub failed: Vec<FailedLeg>,
    pub diff_author: String,
    pub diff_author_for: Option<String>,
}

/// A leg of a run that failed or timed out.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct FailedLeg {
    pub leg: String,
    pub name: String,
    pub status: String,
    /// Where its failure's log is.
    pub failure: String,
}

/// The two frames tagged `attachment`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(untagged)]
pub enum AttachmentChange {
    Saved { attachment: Box<crate::ci::attach::Attachment> },
    Removed { removed: String },
}

/// An incident as lists show it (without its bundle).
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct IncidentBrief {
    pub id: String,
    pub attachment: String,
    pub name: String,
    pub on: String,
    pub wake: String,
    pub title: String,
    pub repo: Option<String>,
    pub runner: Option<String>,
    pub state: String,
    pub agent: Option<String>,
    pub class: Option<String>,
    pub report: Option<String>,
    pub links: Vec<String>,
    pub why: Option<String>,
    /// How many times it happened.
    pub times: usize,
    pub last: Option<crate::ci::attach::Seen>,
    pub at: i64,
    pub updated_at: i64,
}

impl IncidentBrief {
    pub fn of(i: &crate::ci::attach::Incident) -> Self {
        Self {
            id: i.id.clone(),
            attachment: i.attachment.clone(),
            name: i.name.clone(),
            on: i.on.clone(),
            wake: i.wake.clone(),
            title: i.title.clone(),
            repo: i.repo.clone(),
            runner: i.runner.clone(),
            state: i.state.clone(),
            agent: i.agent.clone(),
            class: i.class.clone(),
            report: i.report.clone(),
            links: i.links.clone(),
            why: i.why.clone(),
            times: i.seen.len(),
            last: i.seen.last().cloned(),
            at: i.at,
            updated_at: i.updated_at,
        }
    }
}

/// An experiment as its frame says it: its state, and what changed with it.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ExperimentBrief {
    pub id: String,
    pub repo: String,
    pub state: String,
    /// Its runs that finished.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub measured: Option<usize>,
    /// Its runs planned.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub runs: Option<usize>,
    /// The variant chosen.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub decision: Option<crate::ci::experiments::Decision>,
}

/// A release that moved on; one waiting for approval says so (`needs`) and who may give it.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct DeployRelease {
    pub id: i64,
    pub app: String,
    pub environment: String,
    pub kind: crate::deploy::model::Kind,
    pub state: crate::deploy::model::State,
    pub by: String,
    pub release: crate::deploy::model::Release,
    /// `approval`, while it waits for one.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional, type = "\"approval\""))]
    pub needs: Option<String>,
    /// While it waits for approval: its approvals gate's `reviewers` evidence, as the gate
    /// recorded it (null when it has none).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional, type = "Array<string> | null"))]
    pub approvers: Option<Value>,
}

/// The two frames tagged `deploy_registration`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(untagged)]
pub enum RegistrationChange {
    Saved { registration: Box<crate::deploy::model::Registration> },
    Removed { removed: RegistrationRemoved },
}

/// A registration that went.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RegistrationRemoved {
    pub app: String,
    pub environment: String,
}

/// Notices someone marked (`principal`: whose; `tags`: agents whose shown notifications go).
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Notices {
    pub principal: String,
    pub keys: Vec<String>,
    #[cfg_attr(test, ts(type = "\"away\" | \"done\" | \"seen\""))]
    pub how: String,
    pub at: i64,
    pub tags: Vec<String>,
}

/// A stack's deploy plan.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "CdPlanMade"))]
pub struct CdPlan {
    pub repo: Option<String>,
    pub trunk: String,
    pub base: Option<String>,
    pub head: Option<String>,
    /// How many commits.
    pub commits: usize,
    /// How many apps it plans.
    pub apps: usize,
    pub ok: bool,
}

/// An app's status.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct CdStatus {
    /// `<project>/<name>`.
    pub app: String,
    pub sync: kiln_cd::model::Sync,
    pub health: kiln_cd::model::Health,
    pub revision: Option<String>,
    pub resources: u32,
    pub out_of_sync: u32,
    pub shared: u32,
    pub missing: u32,
    pub prune: u32,
    pub unmarked: u32,
    pub message: Option<String>,
    pub operation: Option<CdOperation>,
}

/// An app's sync: the one under way, or the last.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct CdOperation {
    pub id: u64,
    pub phase: kiln_cd::engine::OpPhase,
    pub trigger: kiln_cd::engine::Trigger,
    pub attempt: u32,
    pub message: String,
    pub by: Option<String>,
    pub started_ms: u64,
    pub finished_ms: Option<u64>,
}

impl CdStatus {
    pub fn of(s: &kiln_cd::engine::Summary) -> Self {
        Self {
            app: s.app.as_ref().to_owned(),
            sync: s.sync,
            health: s.health,
            revision: s.revision.as_deref().map(str::to_owned),
            resources: s.resources,
            out_of_sync: s.out_of_sync,
            shared: s.shared,
            missing: s.missing,
            prune: s.prune,
            unmarked: s.unmarked,
            message: s.message.clone(),
            operation: s.operation.as_ref().map(|o| CdOperation {
                id: o.id,
                phase: o.phase,
                trigger: o.trigger,
                attempt: o.attempt,
                message: o.message.clone(),
                by: o.by.clone(),
                started_ms: o.started_ms,
                finished_ms: o.finished_ms,
            }),
        }
    }
}

/// A runner that went away: its `name`, `labels`, and `gone_for` seconds.
#[derive(Debug, Clone, Default, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct OfflineRunner {
    pub name: String,
    pub labels: Vec<String>,
    pub gone_for: u64,
}

/// What notification rules hear of a run or a diff.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub enum NoticeKind {
    #[serde(rename = "run.succeeded")]
    RunSucceeded,
    #[serde(rename = "run.fixed")]
    RunFixed,
    #[serde(rename = "run.cancelled")]
    RunCancelled,
    #[serde(rename = "run.failed")]
    RunFailed,
    #[serde(rename = "leg.flaky")]
    LegFlaky,
    #[serde(rename = "deploy.succeeded")]
    DeploySucceeded,
    #[serde(rename = "deploy.failed")]
    DeployFailed,
    #[serde(rename = "land.ejected")]
    LandEjected,
    #[serde(rename = "diff.landed")]
    DiffLanded,
    #[serde(rename = "test.quarantined")]
    TestQuarantined,
    #[serde(rename = "quota.near")]
    QuotaNear,
    #[serde(rename = "secret.unused")]
    SecretUnused,
    #[serde(rename = "approval.waiting")]
    ApprovalWaiting,
    #[serde(rename = "review.requested")]
    ReviewRequested,
    #[serde(rename = "comment.reply")]
    CommentReply,
    #[serde(rename = "mention")]
    Mention,
}

impl NoticeKind {
    pub const ALL: [NoticeKind; 16] = [
        Self::RunSucceeded,
        Self::RunFixed,
        Self::RunCancelled,
        Self::RunFailed,
        Self::LegFlaky,
        Self::DeploySucceeded,
        Self::DeployFailed,
        Self::LandEjected,
        Self::DiffLanded,
        Self::TestQuarantined,
        Self::QuotaNear,
        Self::SecretUnused,
        Self::ApprovalWaiting,
        Self::ReviewRequested,
        Self::CommentReply,
        Self::Mention,
    ];

    /// Its name, as rules say it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RunSucceeded => "run.succeeded",
            Self::RunFixed => "run.fixed",
            Self::RunCancelled => "run.cancelled",
            Self::RunFailed => "run.failed",
            Self::LegFlaky => "leg.flaky",
            Self::DeploySucceeded => "deploy.succeeded",
            Self::DeployFailed => "deploy.failed",
            Self::LandEjected => "land.ejected",
            Self::DiffLanded => "diff.landed",
            Self::TestQuarantined => "test.quarantined",
            Self::QuotaNear => "quota.near",
            Self::SecretUnused => "secret.unused",
            Self::ApprovalWaiting => "approval.waiting",
            Self::ReviewRequested => "review.requested",
            Self::CommentReply => "comment.reply",
            Self::Mention => "mention",
        }
    }

    /// The kind named `event`.
    pub fn of(event: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == event)
    }
}

/// Something notification rules hear of a run or a diff (`run` is the diff, for a diff's).
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RunNotice {
    #[serde(rename = "type")]
    pub kind: NoticeKind,
    pub repo: RepoRef,
    pub run: NoticeRun,
    /// For approvals and deploys: which environments.
    pub environments: Vec<String>,
}

/// A repository, by id and name.
#[derive(Debug, Clone, Default, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RepoRef {
    pub id: String,
    pub name: String,
}

/// The run a notice is about, as rules read it (`run`).
#[derive(Debug, Clone, Default, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct NoticeRun {
    pub repo: String,
    pub id: String,
    pub number: i64,
    pub workflow: String,
    pub name: String,
    pub branch: String,
    pub sha: String,
    pub message: String,
    pub actor: String,
    pub conclusion: String,
    pub url: String,
}

/// What a deploy of kiln itself came to.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub enum SelfDeployKind {
    /// A check that holds failed; it was kept.
    #[serde(rename = "deploy.held")]
    Held,
    #[serde(rename = "deploy.rolled_back")]
    RolledBack,
    /// The version put back couldn't start.
    #[serde(rename = "deploy.rollback_failed")]
    RollbackFailed,
}

impl SelfDeployKind {
    /// Its name, as rules say it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Held => "deploy.held",
            Self::RolledBack => "deploy.rolled_back",
            Self::RollbackFailed => "deploy.rollback_failed",
        }
    }
}

/// A deploy of kiln itself that didn't simply take.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SelfDeployNotice {
    #[serde(rename = "type")]
    pub kind: SelfDeployKind,
    pub deploy: SelfDeploy,
    pub message: String,
}

/// Kiln's own deploy: its app, the images it went `from` and `to`, and its revision.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SelfDeploy {
    pub app: String,
    pub from: String,
    pub to: String,
    pub revision: Option<String>,
}

/// What an agent's note says of it.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub enum AgentNoticeKind {
    #[serde(rename = "agent.needs_you")]
    NeedsYou,
    #[serde(rename = "agent.finished")]
    Finished,
    #[serde(rename = "agent.stopped")]
    Stopped,
    #[serde(rename = "agent.paused")]
    Paused,
}

/// An agent needs you, finished, stopped or paused.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct AgentNotice {
    #[serde(rename = "type")]
    pub kind: AgentNoticeKind,
    pub agent: NoticeAgent,
    pub title: String,
    pub text: String,
    pub url: String,
}

/// The agent a notice is about.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct NoticeAgent {
    pub id: String,
    pub name: String,
    pub harness: String,
    pub harness_name: String,
    /// `server`, or the runner it's on.
    pub machine: String,
    pub cwd: String,
    /// Whom it acts for (`owner` or `user:<id>`).
    pub opener: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The frames serialize to what their `json!`s did before they were typed.
    #[test]
    fn frames_are_what_they_were() -> anyhow::Result<()> {
        let same = |e: ServerEvent, was: Value| -> anyhow::Result<()> {
            let now: Value = serde_json::from_str(&e.text())?;
            anyhow::ensure!(now == was, "{now} isn't {was}");
            Ok(())
        };
        same(ServerEvent::Resync, json!({"type": "resync"}))?;
        same(ServerEvent::Updates, json!({"type": "updates"}))?;
        same(ServerEvent::JoinRequests, json!({"type": "join_requests"}))?;
        same(ServerEvent::Keep, json!({"type": "keep"}))?;
        // Both of `queue`'s shapes.
        same(ServerEvent::Queue(Queue::Counts { queued: 3, running: 1 }), json!({"type": "queue", "queued": 3, "running": 1}))?;
        let entry = LandEntry {
            entry: "e1".into(),
            state: "landed".into(),
            reason: None,
            strategy: "rebase".into(),
            diffs: vec![LandDiff { diff: "D1".into(), repo: "acme/app".into(), author: "agent:a1".into(), author_for: Some("user:ada".into()) }],
            enqueued_by: "user:ada".into(),
        };
        same(
            ServerEvent::Queue(Queue::Entry(entry)),
            json!({ "type": "queue", "entry": "e1", "state": "landed", "reason": null, "strategy": "rebase", "diffs": [{ "diff": "D1", "repo": "acme/app", "author": "agent:a1", "author_for": "user:ada" }], "enqueued_by": "user:ada" }),
        )?;
        let battery = crate::os::battery::Battery { percent: 12, ac: false, charging: false, minutes: Some(30) };
        same(ServerEvent::MachineBatteryLow { machine: "mac".into(), battery: battery.clone(), text: "low".into() }, json!({"type": "machine.battery_low", "machine": "mac", "battery": battery, "text": "low"}))?;
        same(ServerEvent::RunnerOffline { runner: OfflineRunner { name: "metal-1".into(), labels: vec!["linux".into()], gone_for: 180 } }, json!({"type": "runner.offline", "runner": {"name": "metal-1", "labels": ["linux"], "gone_for": 180}}))?;
        same(ServerEvent::Attachment(AttachmentChange::Removed { removed: "a1".into() }), json!({"type": "attachment", "removed": "a1"}))?;
        same(
            ServerEvent::DeployRegistration(RegistrationChange::Removed { removed: RegistrationRemoved { app: "web".into(), environment: "prod".into() } }),
            json!({ "type": "deploy_registration", "removed": { "app": "web", "environment": "prod" } }),
        )?;
        same(
            ServerEvent::Experiment { repo: "acme/app".into(), experiment: Box::new(ExperimentBrief { id: "E1".into(), repo: "acme/app".into(), state: "measuring".into(), measured: Some(2), runs: Some(6), decision: None }) },
            json!({ "type": "experiment", "repo": "acme/app", "experiment": { "id": "E1", "repo": "acme/app", "state": "measuring", "measured": 2, "runs": 6 } }),
        )?;
        let run = NoticeRun { repo: "acme/app".into(), id: "r1".into(), number: 7, workflow: "ci.yml".into(), name: "CI".into(), branch: "main".into(), sha: "abc".into(), message: "m".into(), actor: "ada".into(), conclusion: "failure".into(), url: "u".into() };
        same(
            ServerEvent::RunNotice(Box::new(RunNotice { kind: NoticeKind::RunFailed, repo: RepoRef { id: "1".into(), name: "acme/app".into() }, run, environments: vec![] })),
            json!({ "type": "run.failed", "repo": { "id": "1", "name": "acme/app" }, "run": { "repo": "acme/app", "id": "r1", "number": 7, "workflow": "ci.yml", "name": "CI", "branch": "main", "sha": "abc", "message": "m", "actor": "ada", "conclusion": "failure", "url": "u" }, "environments": [] }),
        )?;
        same(
            ServerEvent::SelfDeploy(SelfDeployNotice { kind: SelfDeployKind::RolledBack, deploy: SelfDeploy { app: "kiln".into(), from: "a".into(), to: "b".into(), revision: None }, message: "m".into() }),
            json!({ "type": "deploy.rolled_back", "deploy": { "app": "kiln", "from": "a", "to": "b", "revision": null }, "message": "m" }),
        )?;
        same(
            ServerEvent::Notices(Notices { principal: "owner".into(), keys: vec!["k".into()], how: "seen".into(), at: 5, tags: vec![] }),
            json!({ "type": "notices", "principal": "owner", "keys": ["k"], "how": "seen", "at": 5, "tags": [] }),
        )?;
        Ok(())
    }

    /// Every notice name rules hear is a frame's tag.
    #[test]
    fn notice_kinds_are_their_names() -> anyhow::Result<()> {
        for k in NoticeKind::ALL {
            anyhow::ensure!(serde_json::to_value(k)? == json!(k.as_str()) && NoticeKind::of(k.as_str()) == Some(k), "{k:?}");
        }
        for k in [SelfDeployKind::Held, SelfDeployKind::RolledBack, SelfDeployKind::RollbackFailed] {
            anyhow::ensure!(serde_json::to_value(k)? == json!(k.as_str()), "{k:?}");
        }
        Ok(())
    }
}
