//! [`SimForge`]: each host simulated, in memory and deterministic, keeping its rules and limits:
//! GitHub's stacks merge bottom-up only, with a linear history, in the background; GitLab stacks
//! one author's merge requests, 20 at most, and rebases in the background; Bitbucket has neither
//! stacks nor a queue. Each repository holds a small commit graph (commits and their parents,
//! branches), so staleness, linear history, rebases and leases are real. Requests count against
//! the host's rate limit in a window of the simulator's own time ([`SimForge::advance`]).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use futures::future::BoxFuture;
use parking_lot::Mutex;

use super::suite::ForgeFixture;
use super::*;

/// The trunk every simulated repository starts with.
pub const TRUNK: &str = "main";
/// Most changes a simulated repository holds.
pub const SIM_CHANGES: usize = 10_000;
/// Most commits a simulated repository holds.
pub const SIM_COMMITS: usize = 100_000;
/// Most drafts a change holds.
pub const SIM_DRAFTS: usize = 1000;
/// Most background merges remembered (past it the oldest is let go: its id then reads NotFound,
/// as GitHub forgets a result after 24 hours).
pub const SIM_TICKETS: usize = 10_000;
/// Longest comment body, in bytes (GitHub's 65,536 characters, at most).
pub const BODY_MAX: usize = 65_536;
/// Longest title, in bytes.
pub const TITLE_MAX: usize = 256;
/// GitLab's most merge requests a stack.
pub const GITLAB_STACK_MOST: u32 = 20;
/// GitHub's requests an hour (a GitHub App installation's).
pub const GITHUB_HOURLY: u32 = 5000;
/// GitLab's requests an hour (Self-Managed's default, once its limit is on).
pub const GITLAB_HOURLY: u32 = 7200;
/// Bitbucket Cloud's requests an hour.
pub const BITBUCKET_HOURLY: u32 = 1000;
/// Where the simulator's clock starts (Unix seconds).
const EPOCH: u64 = 1_800_000_000;

struct Commit {
    parents: Vec<String>,
}

struct Ch {
    number: u64,
    author: String,
    title: String,
    head_ref: String,
    base_ref: String,
    state: State,
    rebasing: bool,
    version: u64,
    drafts: Vec<Comment>,
    comments: Vec<Comment>,
}

struct Ticket {
    number: u64,
    expected: String,
    method: Method,
    result: Option<MergeResult>,
}

struct Repo {
    /// Stacks are detected from target branches, as GitLab detects them, not made.
    detect: bool,
    commits: BTreeMap<String, Commit>,
    branches: BTreeMap<String, String>,
    changes: BTreeMap<u64, Ch>,
    stacks: BTreeMap<u64, Vec<u64>>,
    queue_on: bool,
    queue: Vec<(u64, String)>,
    tickets: BTreeMap<String, Ticket>,
    ticket_order: Vec<String>,
    version: u64,
    next_number: u64,
    next_stack: u64,
    next_id: u64,
    made: u64,
}

struct Engine {
    host: Host,
    repos: BTreeMap<String, Repo>,
    now: u64,
    limit: u32,
    window: u64,
    window_start: u64,
    used: u32,
    counted: u64,
    free: u64,
    /// Requests are being answered without counting (a fake that counted its own).
    uncounted: u32,
}

/// One host, simulated. Clones share it.
#[derive(Clone)]
pub struct SimForge {
    inner: Arc<Mutex<Engine>>,
}

fn kiln_does(what: &'static str, does: &str) -> Refusal {
    Refusal::Unsupported { what, kiln_does: does.to_owned() }
}

fn not_found(what: impl Into<String>) -> Refusal {
    Refusal::NotFound { what: what.into() }
}

fn ready<'a, T: Send + 'a>(v: T) -> BoxFuture<'a, T> {
    Box::pin(async move { v })
}

impl Repo {
    fn new(detect: bool) -> Self {
        let mut r = Self {
            detect,
            commits: BTreeMap::new(),
            branches: BTreeMap::new(),
            changes: BTreeMap::new(),
            stacks: BTreeMap::new(),
            queue_on: false,
            queue: Vec::new(),
            tickets: BTreeMap::new(),
            ticket_order: Vec::new(),
            version: 1,
            next_number: 1,
            next_stack: 1,
            next_id: 1,
            made: 0,
        };
        if let Ok(root) = r.commit(Vec::new(), "root") {
            r.branches.insert(TRUNK.to_owned(), root);
        }
        r
    }

    fn commit(&mut self, parents: Vec<String>, label: &str) -> Result<String, Refusal> {
        if self.commits.len() >= SIM_COMMITS {
            return Err(Refusal::Full { limit: SIM_COMMITS });
        }
        self.made = self.made.saturating_add(1);
        let digest = crate::sha256_hex(format!("{parents:?}\0{label}\0{}", self.made).as_bytes());
        let sha = digest.get(..40).unwrap_or(&digest).to_owned();
        self.commits.insert(sha.clone(), Commit { parents });
        Ok(sha)
    }

    fn tip(&self, branch: &str) -> Result<String, Refusal> {
        self.branches.get(branch).cloned().ok_or_else(|| not_found(format!("branch {branch}")))
    }

    /// The commit `from` names: a branch's head, or a commit.
    fn resolve(&self, from: &str) -> Option<String> {
        self.branches.get(from).cloned().or_else(|| self.commits.contains_key(from).then(|| from.to_owned()))
    }

    /// Whether `sha` is `tip` or in its history.
    fn contains(&self, tip: &str, sha: &str) -> bool {
        let mut seen = BTreeSet::new();
        let mut todo = vec![tip.to_owned()];
        while let Some(c) = todo.pop() {
            if c == sha {
                return true;
            }
            if !seen.insert(c.clone()) {
                continue;
            }
            if let Some(commit) = self.commits.get(&c) {
                todo.extend(commit.parents.iter().cloned());
            }
        }
        false
    }

    /// Whether `head` reaches `base` by single-parent commits alone: a linear history on it.
    fn linear(&self, base: &str, head: &str) -> bool {
        let mut c = head.to_owned();
        for _ in 0..=self.commits.len() {
            if c == base {
                return true;
            }
            match self.commits.get(&c).map(|k| k.parents.as_slice()) {
                Some([only]) => c = only.clone(),
                _ => return false,
            }
        }
        false
    }

    /// `head`'s commits that `onto` hasn't, replayed onto it one by one: the new head.
    fn rebase(&mut self, head: &str, onto: &str) -> Result<String, Refusal> {
        let mut chain = Vec::new();
        let mut c = head.to_owned();
        for _ in 0..=self.commits.len() {
            if self.contains(onto, &c) {
                break;
            }
            let first = self.commits.get(&c).and_then(|k| k.parents.first().cloned());
            chain.push(c);
            match first {
                Some(p) => c = p,
                None => break,
            }
        }
        let mut tip = onto.to_owned();
        for old in chain.iter().rev() {
            tip = self.commit(vec![tip], &format!("rebased {old}"))?;
        }
        Ok(tip)
    }

    fn bump(&mut self, number: u64) {
        self.version = self.version.saturating_add(1);
        let v = self.version;
        if let Some(c) = self.changes.get_mut(&number) {
            c.version = v;
        }
    }

    fn bump_all(&mut self, numbers: &[u64]) {
        for n in numbers {
            self.bump(*n);
        }
    }

    fn ch(&self, number: u64) -> Result<&Ch, Refusal> {
        self.changes.get(&number).ok_or_else(|| not_found(format!("change {number}")))
    }

    fn open_ch(&self, number: u64) -> Result<&Ch, Refusal> {
        let c = self.ch(number)?;
        if c.state != State::Open {
            return Err(Refusal::Closed { number });
        }
        Ok(c)
    }

    fn head(&self, number: u64) -> Result<String, Refusal> {
        self.tip(&self.ch(number)?.head_ref)
    }

    fn is_open(&self, number: u64) -> bool {
        self.changes.get(&number).is_some_and(|c| c.state == State::Open)
    }

    fn stack_number_of(&self, number: u64) -> Option<u64> {
        if self.detect {
            return self.detected().into_iter().find(|(_, v)| v.contains(&number)).map(|(k, _)| k);
        }
        self.stacks.iter().find(|(_, v)| v.contains(&number)).map(|(k, _)| *k)
    }

    /// Stack `s`'s open changes, bottom to top.
    fn open_in(&self, s: u64) -> Vec<u64> {
        if self.detect {
            return self.detected().remove(&s).unwrap_or_default();
        }
        self.stacks.get(&s).map(|v| v.iter().copied().filter(|n| self.is_open(*n)).collect()).unwrap_or_default()
    }

    /// The open changes up from `bottom` (one into the trunk) by its author, each targeting the
    /// head branch of the one below, however many.
    fn chain_from(&self, bottom: u64) -> Vec<u64> {
        let Some(first) = self.changes.get(&bottom).filter(|c| c.state == State::Open && c.base_ref == TRUNK) else { return Vec::new() };
        let mut chain = vec![first];
        while let Some(last) = chain.last() {
            let next = self.changes.values().find(|o| o.state == State::Open && o.base_ref == last.head_ref && o.author == first.author && !chain.iter().any(|k| k.number == o.number));
            match next {
                Some(n) if chain.len() <= self.changes.len() => chain.push(n),
                _ => break,
            }
        }
        chain.into_iter().map(|c| c.number).collect()
    }

    /// GitLab's stacks as it detects them: from each open change into the trunk, one author's
    /// chain of 2 to 20, numbered by its bottom.
    fn detected(&self) -> BTreeMap<u64, Vec<u64>> {
        self.changes
            .values()
            .filter(|c| c.state == State::Open && c.base_ref == TRUNK)
            .map(|c| (c.number, self.chain_from(c.number)))
            .filter(|(_, v)| (2..=GITLAB_STACK_MOST as usize).contains(&v.len()))
            .collect()
    }

    fn stack_view(&self, s: u64) -> Stack {
        Stack { number: s, base: TRUNK.to_owned(), changes: self.open_in(s) }
    }

    fn view(&self, c: &Ch) -> Change {
        let stack = self.stack_number_of(c.number).and_then(|s| {
            let open = self.open_in(s);
            let at = open.iter().position(|n| *n == c.number)?;
            Some(Place { stack: s, position: u32::try_from(at).unwrap_or(u32::MAX).saturating_add(1), size: u32::try_from(open.len()).unwrap_or(u32::MAX) })
        });
        Change {
            number: c.number,
            author: c.author.clone(),
            title: c.title.clone(),
            head_ref: c.head_ref.clone(),
            head_sha: self.branches.get(&c.head_ref).cloned().unwrap_or_default(),
            base_ref: c.base_ref.clone(),
            state: c.state.clone(),
            stack,
            updating: c.rebasing,
        }
    }

    /// GitLab's rebases under way finish (as a later read finds them done).
    fn settle(&mut self) -> Result<(), Refusal> {
        let rebasing: Vec<u64> = self.changes.values().filter(|c| c.rebasing).map(|c| c.number).collect();
        for n in rebasing {
            let (head_ref, base_ref) = {
                let c = self.ch(n)?;
                (c.head_ref.clone(), c.base_ref.clone())
            };
            let (head, onto) = (self.tip(&head_ref)?, self.tip(&base_ref)?);
            let new = self.rebase(&head, &onto)?;
            self.branches.insert(head_ref, new);
            if let Some(c) = self.changes.get_mut(&n) {
                c.rebasing = false;
            }
            self.bump(n);
        }
        Ok(())
    }

    fn set_state(&mut self, number: u64, state: State) {
        if let Some(c) = self.changes.get_mut(&number) {
            c.state = state;
        }
        self.queue.retain(|(n, _)| *n != number);
        self.bump(number);
    }

    /// Changes based on `from` are based on `to` now (a merged change's branch is gone).
    fn retarget(&mut self, from: &str, to: &str) {
        let moved: Vec<u64> = self.changes.values().filter(|c| c.state == State::Open && c.base_ref == from).map(|c| c.number).collect();
        for n in &moved {
            if let Some(c) = self.changes.get_mut(n) {
                c.base_ref = to.to_owned();
            }
        }
        self.bump_all(&moved);
    }

    /// GitHub's bottom-up group for merging `number`: it and every unmerged change below it, if
    /// their history is linear.
    fn bottom_up_group(&self, number: u64) -> Result<Vec<u64>, Refusal> {
        let Some(s) = self.stack_number_of(number) else { return Ok(vec![number]) };
        let open = self.open_in(s);
        let at = open.iter().position(|n| *n == number).ok_or(Refusal::Closed { number })?;
        let group: Vec<u64> = open.into_iter().take(at.saturating_add(1)).collect();
        let mut base = self.tip(TRUNK)?;
        for n in &group {
            let head = self.head(*n)?;
            if !self.linear(&base, &head) {
                return Err(Refusal::NotLinear { why: format!("the stack isn't linear at change {n}: it needs rebasing (Rebase stack) before it merges") });
            }
            base = head;
        }
        Ok(group)
    }

    /// Merges `number` into its base now, by `method` (a stacked GitHub change: its bottom-up
    /// group, fast-forwarded): the changes merged and the base's head after.
    fn merge_now(&mut self, host: Host, number: u64, method: Method) -> Result<(Vec<u64>, String), Refusal> {
        if host == Host::Github && self.stack_number_of(number).is_some() {
            let group = self.bottom_up_group(number)?;
            let mut top = String::new();
            for n in &group {
                top = self.head(*n)?;
            }
            self.branches.insert(TRUNK.to_owned(), top.clone());
            for n in &group {
                self.set_state(*n, State::Merged { commit: top.clone() });
                let head_ref = self.ch(*n)?.head_ref.clone();
                self.retarget(&head_ref, TRUNK);
            }
            return Ok((group, top));
        }
        let (head_ref, base_ref) = {
            let c = self.open_ch(number)?;
            (c.head_ref.clone(), c.base_ref.clone())
        };
        let (head, base) = (self.tip(&head_ref)?, self.tip(&base_ref)?);
        let new = match method {
            Method::Merge => self.commit(vec![base, head], &format!("merge change {number}"))?,
            Method::Squash => self.commit(vec![base], &format!("squashed change {number}"))?,
            Method::Rebase => self.rebase(&head, &base)?,
            Method::FastForward if self.contains(&head, &base) => head,
            Method::FastForward => return Err(Refusal::NotLinear { why: format!("change {number} can't fast-forward: it doesn't have its base's head") }),
        };
        self.branches.insert(base_ref.clone(), new.clone());
        self.set_state(number, State::Merged { commit: new.clone() });
        if let Some(s) = self.stack_number_of(number)
            && let Some(v) = self.stacks.get_mut(&s)
        {
            v.retain(|n| *n != number);
        }
        self.retarget(&head_ref, &base_ref);
        Ok((vec![number], new))
    }
}

impl Engine {
    /// Counts one request against the rate limit, or refuses it past the limit.
    fn charge(&mut self) -> Result<(), Refusal> {
        if self.uncounted > 0 {
            return Ok(());
        }
        if self.now >= self.window_start.saturating_add(self.window) {
            self.window_start = self.now;
            self.used = 0;
        }
        if self.used >= self.limit {
            return Err(Refusal::RateLimited { resets_at: self.window_start.saturating_add(self.window) });
        }
        self.used = self.used.saturating_add(1);
        self.counted = self.counted.saturating_add(1);
        Ok(())
    }

    fn repo(&mut self, repo: &str) -> Result<&mut Repo, Refusal> {
        self.repos.get_mut(repo).ok_or_else(|| not_found(format!("repository {repo}")))
    }

    /// Charges one request, then runs `f` on `repo`.
    fn on<T>(&mut self, repo: &str, f: impl FnOnce(Host, &mut Repo) -> Result<T, Refusal>) -> Result<T, Refusal> {
        if !self.repos.contains_key(repo) {
            return Err(not_found(format!("repository {repo}")));
        }
        self.charge()?;
        let host = self.host;
        f(host, self.repo(repo)?)
    }

    fn abilities(&self, repo: &Repo) -> Abilities {
        match self.host {
            Host::Github => Abilities {
                host: self.host,
                stacks: Stacks::Native { most: None, one_author: false, order: StackOrder::BottomUp },
                queue: repo.queue_on.then_some(QueueKind::MergeQueue),
                update: Some(HostUpdate::UpdateBranch),
                review: Batched::PendingReview,
                merge_async: true,
                methods: vec![Method::Merge, Method::Squash, Method::Rebase],
            },
            Host::Gitlab => Abilities {
                host: self.host,
                stacks: Stacks::Native { most: Some(GITLAB_STACK_MOST), one_author: true, order: StackOrder::Retarget },
                queue: repo.queue_on.then_some(QueueKind::MergeTrain),
                update: Some(HostUpdate::Rebase),
                review: Batched::DraftNotes,
                merge_async: false,
                methods: vec![Method::Merge, Method::Squash, Method::FastForward],
            },
            Host::Bitbucket => Abilities {
                host: self.host,
                stacks: Stacks::Kiln { does: BITBUCKET_STACKS.to_owned() },
                queue: None,
                update: None,
                review: Batched::Held,
                merge_async: false,
                methods: vec![Method::Merge, Method::Squash, Method::FastForward],
            },
        }
    }
}

/// What kiln does for stacks on Bitbucket.
const BITBUCKET_STACKS: &str = "chains pull requests by their bases and holds their order with a commit status";
/// What kiln does where a repository has no queue of its host's.
const KILN_QUEUE: &str = "lands it through kiln's merge queue (W28)";
/// What kiln does where a host can't update a change.
const KILN_UPDATE: &str = "replays it in its mirror and pushes it with a lease";

impl SimForge {
    fn with(host: Host, limit: u32, window: u64) -> Self {
        let mut repos = BTreeMap::new();
        repos.insert(SimForgeFixture::REPO.to_owned(), Repo::new(host == Host::Gitlab));
        Self { inner: Arc::new(Mutex::new(Engine { host, repos, now: EPOCH, limit, window, window_start: EPOCH, used: 0, counted: 0, free: 0, uncounted: 0 })) }
    }

    /// GitHub: a GitHub App installation's 5,000 requests an hour.
    pub fn github() -> Self {
        Self::with(Host::Github, GITHUB_HOURLY, 3600)
    }

    /// GitLab: 7,200 requests an hour (Self-Managed's default once its limit is on).
    pub fn gitlab() -> Self {
        Self::with(Host::Gitlab, GITLAB_HOURLY, 3600)
    }

    /// Bitbucket Cloud: 1,000 requests an hour.
    pub fn bitbucket() -> Self {
        Self::with(Host::Bitbucket, BITBUCKET_HOURLY, 3600)
    }

    /// `host`'s simulator.
    pub fn of(host: Host) -> Self {
        match host {
            Host::Github => Self::github(),
            Host::Gitlab => Self::gitlab(),
            Host::Bitbucket => Self::bitbucket(),
        }
    }

    pub fn host(&self) -> Host {
        self.inner.lock().host
    }

    /// Adds an empty repository (its trunk one commit).
    pub fn add_repo(&self, name: &str) {
        let mut e = self.inner.lock();
        let detect = e.host == Host::Gitlab;
        e.repos.entry(name.to_owned()).or_insert_with(|| Repo::new(detect));
    }

    /// A push: a commit labelled `label` on top of `from` (a branch or a commit), `branch` moved
    /// to it (made if new). Its sha.
    pub fn push(&self, repo: &str, branch: &str, from: &str, label: &str) -> Result<String, Refusal> {
        let mut e = self.inner.lock();
        let r = e.repo(repo)?;
        let parent = r.resolve(from).ok_or_else(|| not_found(format!("branch or commit {from}")))?;
        let sha = r.commit(vec![parent], label)?;
        r.branches.insert(branch.to_owned(), sha.clone());
        let touched: Vec<u64> = r.changes.values().filter(|c| c.head_ref == branch && c.state == State::Open).map(|c| c.number).collect();
        r.bump_all(&touched);
        r.version = r.version.saturating_add(1);
        Ok(sha)
    }

    /// `branch`'s head.
    pub fn tip(&self, repo: &str, branch: &str) -> Option<String> {
        self.inner.lock().repos.get(repo).and_then(|r| r.branches.get(branch).cloned())
    }

    /// Whether `sha` is in `branch`'s history.
    pub fn contains(&self, repo: &str, branch: &str, sha: &str) -> bool {
        let e = self.inner.lock();
        e.repos.get(repo).and_then(|r| r.branches.get(branch).map(|tip| r.contains(tip, sha))).unwrap_or(false)
    }

    /// Whether `branch` reaches `base` by single-parent commits alone.
    pub fn linear(&self, repo: &str, base: &str, branch: &str) -> bool {
        let e = self.inner.lock();
        e.repos.get(repo).and_then(|r| r.branches.get(branch).map(|tip| r.linear(base, tip))).unwrap_or(false)
    }

    /// Turns `repo`'s queue (GitHub's merge queue, GitLab's merge train) on or off; Bitbucket has
    /// none.
    pub fn set_queue(&self, repo: &str, on: bool) {
        let mut e = self.inner.lock();
        let host = e.host;
        if let Ok(r) = e.repo(repo) {
            r.queue_on = on && host != Host::Bitbucket;
        }
    }

    /// The queue's checks pass: it lands its changes, first first; one whose head moved is ejected.
    /// Those landed.
    pub fn land_queue(&self, repo: &str) -> Result<Vec<u64>, Refusal> {
        let mut e = self.inner.lock();
        let host = e.host;
        let r = e.repo(repo)?;
        let mut landed = Vec::new();
        while let Some((n, expected)) = (!r.queue.is_empty()).then(|| r.queue.remove(0)) {
            if !r.is_open(n) || r.head(n)? != expected {
                continue;
            }
            let method = if host == Host::Github { Method::Rebase } else { Method::Merge };
            if let Ok((merged, _)) = r.merge_now(host, n, method) {
                landed.extend(merged);
            }
        }
        Ok(landed)
    }

    /// Counts one request against the rate limit, as the host would (a fake of the host's HTTP
    /// API counts each of its requests so, then answers it [`SimForge::uncounted`]).
    pub fn charge(&self) -> Result<(), Refusal> {
        self.inner.lock().charge()
    }

    /// Runs `f`, the requests it makes not counted.
    pub fn uncounted<T>(&self, f: impl FnOnce(&SimForge) -> T) -> T {
        {
            let mut e = self.inner.lock();
            e.uncounted = e.uncounted.saturating_add(1);
        }
        let out = f(self);
        let mut e = self.inner.lock();
        e.uncounted = e.uncounted.saturating_sub(1);
        out
    }

    /// A push with a lease: `branch` moved to `sha` (a commit already there) if it's at
    /// `expected`, else refused Moved.
    pub fn push_lease(&self, repo: &str, branch: &str, sha: &str, expected: &str) -> Result<(), Refusal> {
        let mut e = self.inner.lock();
        let r = e.repo(repo)?;
        if !r.commits.contains_key(sha) {
            return Err(not_found(format!("commit {sha}")));
        }
        let head = r.tip(branch)?;
        if head != expected {
            return Err(Refusal::Moved { head });
        }
        r.branches.insert(branch.to_owned(), sha.to_owned());
        let touched: Vec<u64> = r.changes.values().filter(|c| c.head_ref == branch && c.state == State::Open).map(|c| c.number).collect();
        r.bump_all(&touched);
        r.version = r.version.saturating_add(1);
        Ok(())
    }

    /// Moves the simulator's clock on.
    pub fn advance(&self, secs: u64) {
        let mut e = self.inner.lock();
        e.now = e.now.saturating_add(secs);
    }

    /// The simulator's clock (Unix seconds).
    pub fn now(&self) -> u64 {
        self.inner.lock().now
    }

    /// Requests counted against the rate limit so far.
    pub fn counted(&self) -> u64 {
        self.inner.lock().counted
    }

    /// Reads answered NotModified so far (free).
    pub fn free(&self) -> u64 {
        self.inner.lock().free
    }

    fn run<T: Send + 'static>(&self, repo: &str, f: impl FnOnce(Host, &mut Repo) -> Result<T, Refusal>) -> BoxFuture<'static, Result<T, Refusal>> {
        let out = self.inner.lock().on(repo, f);
        ready(out)
    }
}

fn check_body(body: &str) -> Result<(), Refusal> {
    if body.trim().is_empty() || body.len() > BODY_MAX {
        return Err(Refusal::Invalid { what: "a comment's body", why: format!("1 to {BODY_MAX} bytes, not blank ({})", body.len()) });
    }
    Ok(())
}

/// Checks `changes` chain onto `below` (the head branch they stack on, the trunk for a new stack)
/// and keep the host's bounds, `size` already in the stack: the stack's author, if one.
fn check_chain(host: Host, r: &Repo, mut below: String, changes: &[u64], mut author: Option<String>, size: usize) -> Result<(), Refusal> {
    if changes.is_empty() {
        return Err(Refusal::Invalid { what: "a stack", why: "no changes".into() });
    }
    let mut seen = BTreeSet::new();
    for (i, n) in changes.iter().enumerate() {
        let c = r.open_ch(*n)?;
        if !seen.insert(*n) {
            return Err(Refusal::Invalid { what: "a stack", why: format!("change {n} is in it twice") });
        }
        if let Some(s) = r.stack_number_of(*n).filter(|_| !r.detect) {
            return Err(Refusal::Invalid { what: "a stack", why: format!("change {n} is in stack {s} already") });
        }
        if c.base_ref != below {
            return Err(Refusal::Invalid { what: "a stack", why: format!("change {n} is based on {}, not {below}", c.base_ref) });
        }
        if host == Host::Gitlab {
            let first = author.get_or_insert_with(|| c.author.clone());
            if *first != c.author {
                return Err(Refusal::StackLimit { why: format!("merge request {n} is by {}, not {first}: GitLab stacks one author's merge requests", c.author) });
            }
            let at = size.saturating_add(i).saturating_add(1);
            if at > GITLAB_STACK_MOST as usize {
                return Err(Refusal::StackLimit { why: format!("merge request {n} would be the stack's {at}th: GitLab stacks {GITLAB_STACK_MOST} merge requests at most") });
            }
        }
        below = c.head_ref.clone();
    }
    Ok(())
}

impl Forge for SimForge {
    fn abilities<'a>(&'a self, repo: &'a str) -> BoxFuture<'a, Result<Abilities, Refusal>> {
        let mut e = self.inner.lock();
        let out = (|| {
            if !e.repos.contains_key(repo) {
                return Err(not_found(format!("repository {repo}")));
            }
            e.charge()?;
            let r = e.repos.get(repo).ok_or_else(|| not_found(format!("repository {repo}")))?;
            Ok(e.abilities(r))
        })();
        ready(out)
    }

    fn open<'a>(&'a self, repo: &'a str, open: Open) -> BoxFuture<'a, Result<Change, Refusal>> {
        self.run(repo, move |_, r| {
            if open.by.trim().is_empty() {
                return Err(Refusal::Invalid { what: "a change's author", why: "empty".into() });
            }
            if open.title.trim().is_empty() || open.title.len() > TITLE_MAX {
                return Err(Refusal::Invalid { what: "a change's title", why: format!("1 to {TITLE_MAX} bytes, not blank ({})", open.title.len()) });
            }
            if open.head == open.base {
                return Err(Refusal::Invalid { what: "a change's branches", why: format!("{} into itself", open.head) });
            }
            for b in [&open.head, &open.base] {
                if !r.branches.contains_key(b.as_str()) {
                    return Err(Refusal::Invalid { what: "a change's branches", why: format!("no branch {b}") });
                }
            }
            if let Some(c) = r.changes.values().find(|c| c.state == State::Open && c.head_ref == open.head) {
                return Err(Refusal::Invalid { what: "a change's branches", why: format!("change {} is open from {} already", c.number, open.head) });
            }
            if r.changes.len() >= SIM_CHANGES {
                return Err(Refusal::Full { limit: SIM_CHANGES });
            }
            let number = r.next_number;
            r.next_number = number.saturating_add(1);
            r.changes.insert(
                number,
                Ch { number, author: open.by, title: open.title, head_ref: open.head, base_ref: open.base, state: State::Open, rebasing: false, version: 0, drafts: Vec::new(), comments: Vec::new() },
            );
            r.bump(number);
            if r.detect {
                // A stack GitLab detects may have grown: every open change's place may have moved.
                let open: Vec<u64> = r.changes.values().filter(|c| c.state == State::Open).map(|c| c.number).collect();
                r.bump_all(&open);
            }
            let c = r.ch(number)?;
            Ok(r.view(c))
        })
    }

    fn change<'a>(&'a self, repo: &'a str, number: u64, etag: Option<&'a str>) -> BoxFuture<'a, Result<Fresh<Change>, Refusal>> {
        let mut e = self.inner.lock();
        let out = (|| {
            let r = e.repo(repo)?;
            r.settle()?;
            let c = r.ch(number)?;
            let tag = format!("\"c{number}-v{}\"", c.version);
            if etag == Some(tag.as_str()) {
                e.free = e.free.saturating_add(1);
                return Ok(Fresh::NotModified);
            }
            e.charge()?;
            let r = e.repo(repo)?;
            let value = r.view(r.ch(number)?);
            Ok(Fresh::Changed { value, etag: tag })
        })();
        ready(out)
    }

    fn changes<'a>(&'a self, repo: &'a str, etag: Option<&'a str>) -> BoxFuture<'a, Result<Fresh<Vec<Change>>, Refusal>> {
        let mut e = self.inner.lock();
        let out = (|| {
            let r = e.repo(repo)?;
            r.settle()?;
            let tag = format!("\"l-v{}\"", r.version);
            if etag == Some(tag.as_str()) {
                e.free = e.free.saturating_add(1);
                return Ok(Fresh::NotModified);
            }
            e.charge()?;
            let r = e.repo(repo)?;
            let value = r.changes.values().filter(|c| c.state == State::Open).map(|c| r.view(c)).collect();
            Ok(Fresh::Changed { value, etag: tag })
        })();
        ready(out)
    }

    fn update<'a>(&'a self, repo: &'a str, number: u64, update: Update) -> BoxFuture<'a, Result<Updated, Refusal>> {
        self.run(repo, move |host, r| {
            r.settle()?;
            let (head_ref, base_ref, rebasing) = {
                let c = r.open_ch(number)?;
                (c.head_ref.clone(), c.base_ref.clone(), c.rebasing)
            };
            if rebasing {
                return Err(Refusal::Conflict { why: format!("change {number} is being rebased") });
            }
            let head = r.tip(&head_ref)?;
            if head != update.expected {
                return Err(Refusal::Moved { head });
            }
            if let Some(sha) = update.replayed {
                if !r.commits.contains_key(&sha) {
                    return Err(not_found(format!("commit {sha}")));
                }
                r.branches.insert(head_ref, sha.clone());
                r.bump(number);
                return Ok(Updated::Done { head: sha });
            }
            let base = r.tip(&base_ref)?;
            match host {
                Host::Github if r.contains(&head, &base) => Ok(Updated::Done { head }),
                Host::Github => {
                    let new = r.commit(vec![head, base], &format!("merge {base_ref} into {head_ref}"))?;
                    r.branches.insert(head_ref, new.clone());
                    r.bump(number);
                    Ok(Updated::Done { head: new })
                }
                Host::Gitlab => {
                    if let Some(c) = r.changes.get_mut(&number) {
                        c.rebasing = true;
                    }
                    r.bump(number);
                    Ok(Updated::Started)
                }
                Host::Bitbucket => Err(kiln_does("updating a change on the host", KILN_UPDATE)),
            }
        })
    }

    fn stack<'a>(&'a self, repo: &'a str, changes: Vec<u64>) -> BoxFuture<'a, Result<Stack, Refusal>> {
        self.run(repo, move |host, r| {
            if host == Host::Bitbucket {
                return Err(kiln_does("stacks", BITBUCKET_STACKS));
            }
            check_chain(host, r, TRUNK.to_owned(), &changes, None, 0)?;
            if r.detect {
                let bottom = changes.first().copied().unwrap_or_default();
                let found = r.open_in(bottom);
                return Ok(Stack { number: bottom, base: TRUNK.to_owned(), changes: if found.is_empty() { changes } else { found } });
            }
            let s = r.next_stack;
            r.next_stack = s.saturating_add(1);
            r.stacks.insert(s, changes.clone());
            r.bump_all(&changes);
            Ok(r.stack_view(s))
        })
    }

    fn stack_add<'a>(&'a self, repo: &'a str, stack: u64, changes: Vec<u64>) -> BoxFuture<'a, Result<Stack, Refusal>> {
        self.run(repo, move |host, r| {
            if host == Host::Bitbucket {
                return Err(kiln_does("stacks", BITBUCKET_STACKS));
            }
            let mut open = if r.detect { r.chain_from(stack) } else { r.open_in(stack) };
            if let Some(at) = open.iter().position(|n| changes.contains(n)).filter(|_| r.detect) {
                open.truncate(at);
            }
            let Some(top) = open.last() else { return Err(not_found(format!("stack {stack}"))) };
            let top = r.ch(*top)?;
            check_chain(host, r, top.head_ref.clone(), &changes, Some(top.author.clone()), open.len())?;
            if let Some(v) = r.stacks.get_mut(&stack) {
                v.extend(changes.iter().copied());
            }
            r.bump_all(&open);
            r.bump_all(&changes);
            if r.detect && r.open_in(stack).is_empty() {
                return Ok(Stack { number: stack, base: TRUNK.to_owned(), changes: open.into_iter().chain(changes).collect() });
            }
            Ok(r.stack_view(stack))
        })
    }

    fn stack_of<'a>(&'a self, repo: &'a str, number: u64) -> BoxFuture<'a, Result<Option<Stack>, Refusal>> {
        self.run(repo, move |_, r| {
            r.ch(number)?;
            Ok(r.stack_number_of(number).map(|s| r.stack_view(s)))
        })
    }

    fn unstack<'a>(&'a self, repo: &'a str, stack: u64) -> BoxFuture<'a, Result<Option<Stack>, Refusal>> {
        self.run(repo, move |host, r| {
            match host {
                Host::Bitbucket => return Err(kiln_does("stacks", BITBUCKET_STACKS)),
                Host::Gitlab => return Err(kiln_does("unstacking", "retargets each merge request onto the default branch")),
                Host::Github => {}
            }
            let open = r.open_in(stack);
            if !r.stacks.contains_key(&stack) || open.is_empty() {
                return Err(not_found(format!("stack {stack}")));
            }
            let queued: BTreeSet<u64> = r.queue.iter().map(|(n, _)| *n).collect();
            let kept: Vec<u64> = open.iter().copied().filter(|n| queued.contains(n)).collect();
            if let Some(v) = r.stacks.get_mut(&stack) {
                v.retain(|n| kept.contains(n));
            }
            r.bump_all(&open);
            if kept.is_empty() {
                r.stacks.remove(&stack);
                return Ok(None);
            }
            Ok(Some(r.stack_view(stack)))
        })
    }

    fn merge<'a>(&'a self, repo: &'a str, number: u64, merge: Merge) -> BoxFuture<'a, Result<Merging, Refusal>> {
        self.run(repo, move |host, r| {
            r.settle()?;
            r.open_ch(number)?;
            let allowed = match host {
                Host::Github => matches!(merge.method, Method::Merge | Method::Squash | Method::Rebase),
                Host::Gitlab | Host::Bitbucket => matches!(merge.method, Method::Merge | Method::Squash | Method::FastForward),
            };
            if !allowed {
                return Err(Refusal::Invalid { what: "a merge method", why: format!("{} doesn't merge by {:?}", host.name(), merge.method) });
            }
            if host != Host::Github {
                let head = r.head(number)?;
                if head != merge.expected {
                    return Err(Refusal::Moved { head });
                }
                let (merged, commit) = r.merge_now(host, number, merge.method)?;
                return Ok(Merging::Merged { merged, commit });
            }
            if r.stack_number_of(number).is_some() {
                r.bottom_up_group(number)?;
            }
            if r.tickets.values().any(|t| t.number == number && t.result.is_none()) {
                return Err(Refusal::Conflict { why: format!("a merge of change {number} is under way already") });
            }
            if r.tickets.len() >= SIM_TICKETS && !r.ticket_order.is_empty() {
                let oldest = r.ticket_order.remove(0);
                r.tickets.remove(&oldest);
            }
            let id = format!("{:08x}-sim-merge-{number}-{}", r.next_id, r.next_id);
            r.next_id = r.next_id.saturating_add(1);
            r.tickets.insert(id.clone(), Ticket { number, expected: merge.expected, method: merge.method, result: None });
            r.ticket_order.push(id.clone());
            Ok(Merging::Started { id })
        })
    }

    fn merge_result<'a>(&'a self, repo: &'a str, number: u64, id: &'a str) -> BoxFuture<'a, Result<MergeResult, Refusal>> {
        self.run(repo, move |host, r| {
            let (expected, method, done) = match r.tickets.get(id) {
                Some(t) if t.number == number => (t.expected.clone(), t.method, t.result.clone()),
                _ => return Err(not_found(format!("merge {id} of change {number}"))),
            };
            if let Some(done) = done {
                return Ok(done);
            }
            let result = match r.head(number) {
                Ok(head) if !r.is_open(number) => MergeResult::Failed { why: format!("change {number} isn't open (its head {head})") },
                Ok(head) if head != expected => MergeResult::Failed { why: format!("change {number} was pushed to after the merge was asked ({head}, not {expected}): cancelled") },
                Ok(_) => match r.merge_now(host, number, method) {
                    Ok((merged, commit)) => MergeResult::Merged { merged, commit },
                    Err(e) => MergeResult::Failed { why: e.to_string() },
                },
                Err(e) => MergeResult::Failed { why: e.to_string() },
            };
            if let Some(t) = r.tickets.get_mut(id) {
                t.result = Some(result.clone());
            }
            Ok(result)
        })
    }

    fn enqueue<'a>(&'a self, repo: &'a str, number: u64, expected: &'a str) -> BoxFuture<'a, Result<Vec<u64>, Refusal>> {
        self.run(repo, move |host, r| {
            if !r.queue_on {
                return Err(kiln_does("a host's queue", KILN_QUEUE));
            }
            let base_ref = r.open_ch(number)?.base_ref.clone();
            let head = r.head(number)?;
            if head != expected {
                return Err(Refusal::Moved { head });
            }
            let group = match host {
                Host::Github => r.bottom_up_group(number)?,
                _ if base_ref != TRUNK => return Err(Refusal::Invalid { what: "a merge train's merge request", why: format!("it merges into {base_ref}: a train takes merge requests into {TRUNK}") }),
                _ => vec![number],
            };
            for n in group {
                if !r.queue.iter().any(|(q, _)| *q == n) {
                    let at = r.head(n)?;
                    r.queue.push((n, at));
                    r.bump(n);
                }
            }
            Ok(r.queue.iter().map(|(n, _)| *n).collect())
        })
    }

    fn dequeue<'a>(&'a self, repo: &'a str, number: u64) -> BoxFuture<'a, Result<Vec<u64>, Refusal>> {
        self.run(repo, move |host, r| {
            if !r.queue_on {
                return Err(kiln_does("a host's queue", KILN_QUEUE));
            }
            r.ch(number)?;
            if !r.queue.iter().any(|(n, _)| *n == number) {
                return Ok(Vec::new());
            }
            let mut out = vec![number];
            if host == Host::Github
                && let Some(s) = r.stack_number_of(number)
            {
                let open = r.open_in(s);
                let above = open.iter().skip_while(|n| **n != number).skip(1);
                out.extend(above.filter(|n| r.queue.iter().any(|(q, _)| q == *n)));
            }
            r.queue.retain(|(n, _)| !out.contains(n));
            r.bump_all(&out);
            Ok(out)
        })
    }

    fn queued<'a>(&'a self, repo: &'a str) -> BoxFuture<'a, Result<Vec<u64>, Refusal>> {
        self.run(repo, |_, r| if r.queue_on { Ok(r.queue.iter().map(|(n, _)| *n).collect()) } else { Err(kiln_does("a host's queue", KILN_QUEUE)) })
    }

    fn draft<'a>(&'a self, repo: &'a str, number: u64, by: &'a str, draft: Draft) -> BoxFuture<'a, Result<u64, Refusal>> {
        let mut e = self.inner.lock();
        let out = (|| {
            // Bitbucket's drafts are kiln's to hold: no request reaches the host.
            if e.host != Host::Bitbucket {
                e.repo(repo)?;
                e.charge()?;
            }
            let r = e.repo(repo)?;
            r.open_ch(number)?;
            check_body(&draft.body)?;
            if by.trim().is_empty() {
                return Err(Refusal::Invalid { what: "a draft's author", why: "empty".into() });
            }
            let id = r.next_id;
            let c = r.changes.get_mut(&number).ok_or_else(|| not_found(format!("change {number}")))?;
            if c.drafts.len() >= SIM_DRAFTS {
                return Err(Refusal::Full { limit: SIM_DRAFTS });
            }
            c.drafts.push(Comment { id, author: by.to_owned(), body: draft.body, line: draft.line, review: None });
            r.next_id = id.saturating_add(1);
            Ok(id)
        })();
        ready(out)
    }

    fn drafts<'a>(&'a self, repo: &'a str, number: u64, by: &'a str) -> BoxFuture<'a, Result<Vec<Comment>, Refusal>> {
        let mut e = self.inner.lock();
        let out = (|| {
            if e.host != Host::Bitbucket {
                e.repo(repo)?;
                e.charge()?;
            }
            let r = e.repo(repo)?;
            Ok(r.ch(number)?.drafts.iter().filter(|d| d.author == by).cloned().collect())
        })();
        ready(out)
    }

    fn publish<'a>(&'a self, repo: &'a str, number: u64, by: &'a str, verdict: Verdict) -> BoxFuture<'a, Result<Review, Refusal>> {
        let mut e = self.inner.lock();
        let out = (|| {
            let host = e.host;
            let r = e.repo(repo)?;
            r.open_ch(number)?;
            let held = r.ch(number)?.drafts.iter().filter(|d| d.author == by).count();
            // GitHub submits its pending review, GitLab publishes its draft notes and approves:
            // a request or two; kiln posts Bitbucket's held comments one by one, then the verdict.
            let requests = match host {
                Host::Github => 1,
                Host::Gitlab => 2,
                Host::Bitbucket => held.saturating_add(1),
            };
            let remaining = usize::try_from(e.limit.saturating_sub(e.used)).unwrap_or(usize::MAX);
            if e.now < e.window_start.saturating_add(e.window) && remaining < requests {
                return Err(Refusal::RateLimited { resets_at: e.window_start.saturating_add(e.window) });
            }
            for _ in 0..requests {
                e.charge()?;
            }
            let r = e.repo(repo)?;
            let review = r.next_id;
            r.next_id = review.saturating_add(1);
            let c = r.changes.get_mut(&number).ok_or_else(|| not_found(format!("change {number}")))?;
            let (mine, rest): (Vec<Comment>, Vec<Comment>) = std::mem::take(&mut c.drafts).into_iter().partition(|d| d.author == by);
            c.drafts = rest;
            let comments: Vec<Comment> = mine.into_iter().map(|d| Comment { review: Some(review), ..d }).collect();
            c.comments.extend(comments.iter().cloned());
            r.bump(number);
            Ok(Review { id: review, author: by.to_owned(), verdict, comments })
        })();
        ready(out)
    }

    fn comments<'a>(&'a self, repo: &'a str, number: u64) -> BoxFuture<'a, Result<Vec<Comment>, Refusal>> {
        self.run(repo, move |_, r| Ok(r.ch(number)?.comments.clone()))
    }

    fn rate<'a>(&'a self, repo: &'a str) -> BoxFuture<'a, Result<Rate, Refusal>> {
        let e = self.inner.lock();
        let out = if e.repos.contains_key(repo) {
            let fresh = e.now >= e.window_start.saturating_add(e.window);
            let used = if fresh { 0 } else { e.used };
            let resets_at = if fresh { e.now.saturating_add(e.window) } else { e.window_start.saturating_add(e.window) };
            Ok(Rate { limit: e.limit, remaining: e.limit.saturating_sub(used), resets_at })
        } else {
            Err(not_found(format!("repository {repo}")))
        };
        ready(out)
    }
}

/// The fixture for a [`SimForge`]: a fresh host with the repository [`SimForgeFixture::REPO`].
pub struct SimForgeFixture(pub SimForge);

impl SimForgeFixture {
    /// The repository every case works in.
    pub const REPO: &'static str = "kiln/conformance";

    pub fn new(host: Host) -> Self {
        Self(SimForge::of(host))
    }
}

impl ForgeFixture for SimForgeFixture {
    fn forge(&self) -> &dyn Forge {
        &self.0
    }
    fn host(&self) -> Host {
        self.0.host()
    }
    fn repo(&self) -> &str {
        Self::REPO
    }
    fn git(&self) -> &SimForge {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: &str = SimForgeFixture::REPO;

    fn block<T>(f: BoxFuture<'_, T>) -> T {
        futures::executor::block_on(f)
    }

    fn at<T: Clone>(v: &[T], i: usize) -> Result<T, Refusal> {
        v.get(i).cloned().ok_or_else(|| not_found(format!("item {i}")))
    }

    /// `n` changes by `by`, each on the branch of the one before: their numbers.
    fn chain(s: &SimForge, prefix: &str, n: u32, by: &str) -> Result<Vec<u64>, Refusal> {
        let mut out = Vec::new();
        let mut below = TRUNK.to_owned();
        for i in 1..=n {
            let branch = format!("{prefix}-{i}");
            s.push(R, &branch, &below, &branch)?;
            out.push(block(s.open(R, Open { head: branch.clone(), base: below.clone(), title: branch.clone(), by: by.to_owned() }))?.number);
            below = branch;
        }
        Ok(out)
    }

    fn head(s: &SimForge, n: u64) -> Result<String, Refusal> {
        block(s.change(R, n, None))?.changed().map(|(c, _)| c.head_sha).ok_or_else(|| not_found("a fresh read"))
    }

    fn started(m: Merging) -> Result<String, Refusal> {
        match m {
            Merging::Started { id } => Ok(id),
            Merging::Merged { .. } => Err(Refusal::Unavailable("GitHub merges in the background".into())),
        }
    }

    /// W26-R20: GitLab's simulator refuses a stack's 21st merge request, and one by another
    /// author, as GitLab stacks one author's merge requests, 20 at most.
    #[test]
    fn w26_r20_gitlab_refuses_a_21st_stacked_merge_request_and_another_author_s() -> Result<(), Refusal> {
        let s = SimForge::gitlab();
        let mrs = chain(&s, "s", 21, "alice")?;
        let (twenty, last) = mrs.split_at(20);
        let refused = block(s.stack(R, mrs.clone()));
        assert!(matches!(&refused, Err(Refusal::StackLimit { why }) if why.contains("21th") && why.contains("20 merge requests at most")), "{refused:?}");
        let stack = block(s.stack(R, twenty.to_vec()))?;
        assert_eq!(stack.changes.len(), 20);
        assert!(matches!(block(s.stack_add(R, stack.number, last.to_vec())), Err(Refusal::StackLimit { .. })));
        let mine = at(&chain(&s, "t", 1, "alice")?, 0)?;
        s.push(R, "theirs", "t-1", "theirs")?;
        let theirs = block(s.open(R, Open { head: "theirs".into(), base: "t-1".into(), title: "theirs".into(), by: "bob".into() }))?;
        let refused = block(s.stack(R, vec![mine, theirs.number]));
        assert!(matches!(&refused, Err(Refusal::StackLimit { why }) if why.contains("bob") && why.contains("one author")), "{refused:?}");
        Ok(())
    }

    /// W26-R20: GitHub's simulator merges a stack bottom-up only: merging the third of three
    /// merges the first, second and third, in that order, in one linear history.
    #[test]
    fn w26_r20_github_merges_a_stack_bottom_up_only_in_a_linear_history() -> Result<(), Refusal> {
        let s = SimForge::github();
        let root = s.tip(R, TRUNK).ok_or_else(|| not_found("trunk"))?;
        let stack = chain(&s, "s", 3, "alice")?;
        block(s.stack(R, stack.clone()))?;
        let heads: Vec<String> = stack.iter().map(|n| head(&s, *n)).collect::<Result<_, _>>()?;
        let (top, top_head) = (at(&stack, 2)?, at(&heads, 2)?);
        let id = started(block(s.merge(R, top, Merge { method: Method::Squash, expected: top_head.clone() }))?)?;
        assert_eq!(block(s.merge_result(R, top, &id))?, MergeResult::Merged { merged: stack.clone(), commit: top_head.clone() });
        // Bottom first: the trunk's history is each change's head in stack order, on the root.
        let mut log = vec![top_head];
        {
            let e = s.inner.lock();
            let r = e.repos.get(R).ok_or_else(|| not_found("repo"))?;
            while let Some([p]) = log.last().and_then(|c| r.commits.get(c)).map(|c| c.parents.as_slice()) {
                log.push(p.clone());
            }
        }
        assert_eq!(log, vec![at(&heads, 2)?, at(&heads, 1)?, at(&heads, 0)?, root]);
        Ok(())
    }

    /// W26-R20: a GitHub stack that isn't linear is refused NotLinear until it's updated, and no
    /// change merges above an unmerged one.
    #[test]
    fn w26_r20_github_refuses_a_stack_that_isnt_linear_until_it_is_updated() -> Result<(), Refusal> {
        let s = SimForge::github();
        let stack = chain(&s, "s", 2, "alice")?;
        let (one, two) = (at(&stack, 0)?, at(&stack, 1)?);
        block(s.stack(R, stack.clone()))?;
        let main = s.push(R, TRUNK, TRUNK, "trunk moves")?;
        let h2 = head(&s, two)?;
        assert!(matches!(block(s.merge(R, two, Merge { method: Method::Merge, expected: h2.clone() })), Err(Refusal::NotLinear { .. })));
        assert_eq!(s.tip(R, TRUNK), Some(main));
        // update-branch merges the base in: still not linear.
        block(s.update(R, one, Update { expected: head(&s, one)?, replayed: None }))?;
        assert!(matches!(block(s.merge(R, one, Merge { method: Method::Merge, expected: head(&s, one)? })), Err(Refusal::NotLinear { .. })));
        // Rebased (kiln's replays, pushed with a lease): it merges.
        let r1 = s.push(R, "replay-1", TRUNK, "s-1 again")?;
        block(s.update(R, one, Update { expected: head(&s, one)?, replayed: Some(r1) }))?;
        let r2 = s.push(R, "replay-2", "s-1", "s-2 again")?;
        block(s.update(R, two, Update { expected: h2, replayed: Some(r2.clone()) }))?;
        let id = started(block(s.merge(R, two, Merge { method: Method::Merge, expected: r2.clone() }))?)?;
        assert_eq!(block(s.merge_result(R, two, &id))?, MergeResult::Merged { merged: stack, commit: r2 });
        Ok(())
    }

    /// W26-R20: Bitbucket's simulator has no stacks and no queue: each refused Unsupported,
    /// naming what kiln does instead; its drafts are kiln's, costing the host nothing.
    #[test]
    fn w26_r20_bitbucket_refuses_stacks_and_a_queue_naming_what_kiln_does() -> Result<(), Refusal> {
        let s = SimForge::bitbucket();
        s.set_queue(R, true);
        let prs = chain(&s, "s", 2, "alice")?;
        let first = at(&prs, 0)?;
        let h = head(&s, first)?;
        for refused in [block(s.stack(R, prs.clone())).map(|_| ()), block(s.enqueue(R, first, &h)).map(|_| ()), block(s.update(R, first, Update { expected: h.clone(), replayed: None })).map(|_| ())] {
            assert!(matches!(&refused, Err(Refusal::Unsupported { kiln_does, .. }) if !kiln_does.is_empty()), "{refused:?}");
        }
        let before = s.counted();
        block(s.draft(R, first, "alice", Draft { body: "held".into(), line: None }))?;
        assert_eq!(s.counted(), before, "a held draft reached the host");
        Ok(())
    }

    /// W26-R20: a read with an unchanged ETag is free; requests past the hour's limit are
    /// refused with when it resets.
    #[test]
    fn w26_r20_not_modified_reads_are_free_and_the_limit_holds() -> Result<(), Refusal> {
        let s = SimForge::bitbucket();
        let n = at(&chain(&s, "s", 1, "alice")?, 0)?;
        let etag = block(s.change(R, n, None))?.changed().map(|(_, e)| e).ok_or_else(|| not_found("a first read"))?;
        let counted = s.counted();
        for _ in 0..5000 {
            assert_eq!(block(s.change(R, n, Some(&etag)))?, Fresh::NotModified);
        }
        assert_eq!((s.counted(), s.free()), (counted, 5000));
        while block(s.abilities(R)).is_ok() {}
        assert_eq!(s.counted(), 1000);
        assert!(matches!(block(s.abilities(R)), Err(Refusal::RateLimited { resets_at }) if resets_at == EPOCH + 3600));
        Ok(())
    }
}
