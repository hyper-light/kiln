//! `Forge`: what a git host does with changes (docs/ci.md W26-R20), for kiln's Review in Ship:
//! stacks (GitHub's Stacks API, GitLab's stacked merge requests; none on Bitbucket), a host's queue
//! (GitHub's merge queue, GitLab's merge trains), updating a stale change (GitHub's update-branch
//! against an expected head, GitLab's rebase, else a replayed commit pushed with a lease), batched
//! reviews (GitHub's pending review, GitLab's draft notes, held by kiln on Bitbucket), merging
//! (in the background on GitHub, polled), and what a repository has of each ([`Abilities`]). Where
//! a host lacks one, the seam refuses it [`Refusal::Unsupported`], naming what kiln does instead.
//!
//! Reads are conditional ([`Fresh`]): a read with the ETag of what hasn't changed is
//! `NotModified` and costs nothing against the host's rate limit, as GitHub's 304s don't; every
//! other request counts, and past the limit requests are refused [`Refusal::RateLimited`], never
//! dropped. githost (`kiln::githost::forge`) is the real implementation, for kiln's GitHub App,
//! GitLab and Bitbucket; [`SimForge`] simulates each host, keeping its limits (GitLab: one author
//! and 20 merge requests a stack; GitHub: stacks merged bottom-up, with a linear history).

use futures::future::BoxFuture;

mod sim;
mod suite;

pub use sim::{BITBUCKET_HOURLY, BODY_MAX, GITHUB_HOURLY, GITLAB_HOURLY, GITLAB_STACK_MOST, SIM_CHANGES, SIM_COMMITS, SIM_DRAFTS, SIM_TICKETS, SimForge, SimForgeFixture, TITLE_MAX, TRUNK};
pub use suite::{ForgeFixture, cases};

/// A git host the seam reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Host {
    Github,
    Gitlab,
    Bitbucket,
}

impl Host {
    pub fn name(self) -> &'static str {
        match self {
            Self::Github => "GitHub",
            Self::Gitlab => "GitLab",
            Self::Bitbucket => "Bitbucket",
        }
    }
}

/// Where a change is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Open,
    /// Merged: `commit` is its base branch's head once it was.
    Merged { commit: String },
    Closed,
}

/// A change's place in its stack: `position` 1 is the bottom, of `size` open changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Place {
    pub stack: u64,
    pub position: u32,
    pub size: u32,
}

/// A change: GitHub's and Bitbucket's pull request, GitLab's merge request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// GitHub's number, GitLab's iid, Bitbucket's id.
    pub number: u64,
    /// Who opened it (the host's login).
    pub author: String,
    pub title: String,
    pub head_ref: String,
    pub head_sha: String,
    /// The branch it merges into: the trunk, or in a stack the head branch of the change below.
    pub base_ref: String,
    pub state: State,
    pub stack: Option<Place>,
    /// The host is updating it (GitLab's rebase under way).
    pub updating: bool,
}

/// A change to open: `head` into `base`, by `by`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Open {
    pub head: String,
    pub base: String,
    pub title: String,
    pub by: String,
}

/// How a host's stacks merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackOrder {
    /// GitHub's: from the bottom, a change merging every unmerged one below it with it, together,
    /// and only with a linear history.
    BottomUp,
    /// GitLab's: each into the branch below it; merging the bottom one retargets the next onto
    /// the trunk.
    Retarget,
}

/// Whether a host stacks changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stacks {
    /// The host's own stacks: at most `most` changes (none: unbounded), all by one author when
    /// `one_author`.
    Native { most: Option<u32>, one_author: bool, order: StackOrder },
    /// None on the host: kiln's (`does` says how).
    Kiln { does: String },
}

/// A host's queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueKind {
    /// GitHub's merge queue.
    MergeQueue,
    /// GitLab's merge trains (Premium).
    MergeTrain,
}

/// How a host updates a stale change itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostUpdate {
    /// GitHub's update-branch: the base merged into the branch, against an expected head.
    UpdateBranch,
    /// GitLab's rebase: the branch rebased onto its base, in the background.
    Rebase,
}

/// How a host batches a review's comments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Batched {
    /// GitHub's pending review.
    PendingReview,
    /// GitLab's draft notes.
    DraftNotes,
    /// None on the host: kiln holds the drafts, then posts them together.
    Held,
}

/// A merge method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Merge,
    Squash,
    Rebase,
    FastForward,
}

/// What a repository has of each, on its host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Abilities {
    pub host: Host,
    pub stacks: Stacks,
    /// The host's queue, where this repository has one on; none: kiln's merge queue (W28).
    pub queue: Option<QueueKind>,
    /// How the host updates a stale change; none: kiln replays it and pushes it with a lease.
    pub update: Option<HostUpdate>,
    pub review: Batched,
    /// Merges run in the background, polled with `merge_result` (GitHub's asynchronous merge).
    pub merge_async: bool,
    pub methods: Vec<Method>,
}

/// A conditional read's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fresh<T> {
    /// What's there now, and its ETag for the next read.
    Changed { value: T, etag: String },
    /// Unchanged since the ETag given: no request counted.
    NotModified,
}

impl<T> Fresh<T> {
    pub fn changed(self) -> Option<(T, String)> {
        match self {
            Self::Changed { value, etag } => Some((value, etag)),
            Self::NotModified => None,
        }
    }
}

/// A stack: its open changes, bottom to top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stack {
    pub number: u64,
    /// The branch it lands on.
    pub base: String,
    pub changes: Vec<u64>,
}

/// Bringing a stale change up to date, its branch at `expected`: by the host's own way, or with
/// `replayed` (a commit kiln replayed onto the base, already on the host) pushed with a lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub expected: String,
    pub replayed: Option<String>,
}

/// An update's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Updated {
    /// Done: the branch's head now.
    Done { head: String },
    /// Under way on the host: the change says `updating` until it's done.
    Started,
}

/// A merge, by `method`, of the change at `expected`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merge {
    pub method: Method,
    pub expected: String,
}

/// A merge's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merging {
    /// Merged: the changes (bottom first, for a stack) and the base's head after.
    Merged { merged: Vec<u64>, commit: String },
    /// Under way in the background: poll `merge_result` with `id`.
    Started { id: String },
}

/// A background merge's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeResult {
    Pending,
    Merged { merged: Vec<u64>, commit: String },
    Failed { why: String },
}

/// A line a comment is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub path: String,
    pub line: u32,
}

/// A comment to hold in a batched review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub body: String,
    pub line: Option<Line>,
}

/// What a review says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Approve,
    RequestChanges,
    Comment,
}

/// A comment: published, or a draft its author sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub id: u64,
    pub author: String,
    pub body: String,
    pub line: Option<Line>,
    /// The review it was published with, where the host keeps it.
    pub review: Option<u64>,
}

/// A published review: its verdict and its comments, published together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    pub id: u64,
    pub author: String,
    pub verdict: Verdict,
    pub comments: Vec<Comment>,
}

/// A host's rate limit as it stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    pub limit: u32,
    pub remaining: u32,
    /// When it resets (Unix seconds).
    pub resets_at: u64,
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("no {what}")]
    NotFound { what: String },
    /// The change's head isn't the one expected: it's at `head`.
    #[error("the change moved: its head is {head}")]
    Moved { head: String },
    #[error("{what} isn't something this host does: kiln {kiln_does}")]
    Unsupported { what: &'static str, kiln_does: String },
    /// Past a host's bound on its stacks.
    #[error("{why}")]
    StackLimit { why: String },
    /// A stack (or a fast-forward) whose history isn't linear: it needs updating first.
    #[error("{why}")]
    NotLinear { why: String },
    #[error("change {number} is merged or closed")]
    Closed { number: u64 },
    /// Another request is changing it.
    #[error("{why}")]
    Conflict { why: String },
    #[error("past the host's rate limit until {resets_at}")]
    RateLimited { resets_at: u64 },
    #[error("{what} isn't valid: {why}")]
    Invalid { what: &'static str, why: String },
    #[error("the simulator holds its most ({limit})")]
    Full { limit: usize },
    #[error("the host is unavailable: {0}")]
    Unavailable(String),
}

/// What a git host does with changes, per repository (`owner/name` on the host).
pub trait Forge: Send + Sync + 'static {
    /// What `repo` has of stacks, a queue, updating, batched reviews and merging.
    fn abilities<'a>(&'a self, repo: &'a str) -> BoxFuture<'a, Result<Abilities, Refusal>>;
    /// Opens a change from `open.head` into `open.base`, by `open.by`.
    fn open<'a>(&'a self, repo: &'a str, open: Open) -> BoxFuture<'a, Result<Change, Refusal>>;
    /// Change `number`, unless it's as it was when read with `etag`.
    fn change<'a>(&'a self, repo: &'a str, number: u64, etag: Option<&'a str>) -> BoxFuture<'a, Result<Fresh<Change>, Refusal>>;
    /// The open changes, by number, unless they're as they were when read with `etag`.
    fn changes<'a>(&'a self, repo: &'a str, etag: Option<&'a str>) -> BoxFuture<'a, Result<Fresh<Vec<Change>>, Refusal>>;
    /// Brings change `number` up to date with its base, if its head is still `update.expected`.
    fn update<'a>(&'a self, repo: &'a str, number: u64, update: Update) -> BoxFuture<'a, Result<Updated, Refusal>>;
    /// Makes a stack of `changes`, bottom to top, each based on the head of the one below.
    fn stack<'a>(&'a self, repo: &'a str, changes: Vec<u64>) -> BoxFuture<'a, Result<Stack, Refusal>>;
    /// Puts `changes` on top of stack `stack`, in order.
    fn stack_add<'a>(&'a self, repo: &'a str, stack: u64, changes: Vec<u64>) -> BoxFuture<'a, Result<Stack, Refusal>>;
    /// The stack change `number` is in.
    fn stack_of<'a>(&'a self, repo: &'a str, number: u64) -> BoxFuture<'a, Result<Option<Stack>, Refusal>>;
    /// Takes stack `stack`'s unmerged changes out of it, those that can't be (queued) left in
    /// place; none left, the stack is dissolved (`None`).
    fn unstack<'a>(&'a self, repo: &'a str, stack: u64) -> BoxFuture<'a, Result<Option<Stack>, Refusal>>;
    /// Merges change `number` (in a bottom-up stack, every unmerged change below it with it).
    fn merge<'a>(&'a self, repo: &'a str, number: u64, merge: Merge) -> BoxFuture<'a, Result<Merging, Refusal>>;
    /// The result of the background merge `id` of change `number`.
    fn merge_result<'a>(&'a self, repo: &'a str, number: u64, id: &'a str) -> BoxFuture<'a, Result<MergeResult, Refusal>>;
    /// Puts change `number` (at `expected`) in the host's queue (in a bottom-up stack, with the
    /// unmerged changes below it): the queue after, first to land first.
    fn enqueue<'a>(&'a self, repo: &'a str, number: u64, expected: &'a str) -> BoxFuture<'a, Result<Vec<u64>, Refusal>>;
    /// Takes change `number` out of the host's queue, with the changes stacked above it: those
    /// taken out.
    fn dequeue<'a>(&'a self, repo: &'a str, number: u64) -> BoxFuture<'a, Result<Vec<u64>, Refusal>>;
    /// The host's queue, first to land first.
    fn queued<'a>(&'a self, repo: &'a str) -> BoxFuture<'a, Result<Vec<u64>, Refusal>>;
    /// Holds a comment in `by`'s batched review of change `number`: its id.
    fn draft<'a>(&'a self, repo: &'a str, number: u64, by: &'a str, draft: Draft) -> BoxFuture<'a, Result<u64, Refusal>>;
    /// `by`'s held comments on change `number` (only `by` sees them).
    fn drafts<'a>(&'a self, repo: &'a str, number: u64, by: &'a str) -> BoxFuture<'a, Result<Vec<Comment>, Refusal>>;
    /// Publishes `by`'s held comments together, with `verdict`.
    fn publish<'a>(&'a self, repo: &'a str, number: u64, by: &'a str, verdict: Verdict) -> BoxFuture<'a, Result<Review, Refusal>>;
    /// Change `number`'s published comments, oldest first.
    fn comments<'a>(&'a self, repo: &'a str, number: u64) -> BoxFuture<'a, Result<Vec<Comment>, Refusal>>;
    /// The host's rate limit as it stands: free where the host has a free way to ask (GitHub's),
    /// else as its last answer said (asked once if it hasn't answered yet).
    fn rate<'a>(&'a self, repo: &'a str) -> BoxFuture<'a, Result<Rate, Refusal>>;
}
