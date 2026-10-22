//! Release repositories on GitHub (docs/deploy-api.md §7), through kiln's GitHub App:
//! - **Made** in an organization the App is installed on, when one isn't there yet.
//! - **Read** through a mirror of kiln's, fetched with an installation token that reads that one
//!   repository; KilnCD reads the mirror, Argo CD reads GitHub with a token of its own that kiln
//!   replaces before it ends.
//! - **Written** by a push of a commit made in the mirror, with a lease on the head it was made
//!   on: a commit someone made beside it is never lost.
//! - **Pull requests**, where an environment says `via: pull-request`: a release is proposed from
//!   a branch of its own (`kiln/release-<id>-<when it was asked for>`); merged, its merge is
//!   delivered; closed unmerged, it's cancelled.
//! - **Deployments**: each release is a deployment of its commit to `<environment>/<app>`, its
//!   status following the release.
//! - **Webhooks**: a push has the mirror fetched at once, a pull request moves its release at once;
//!   without them kiln polls.
//!
//! Tokens go to git in its environment, never on its command line, in a file, or in what's said.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use base64::Engine as _;
use serde_json::Value;

use super::Deploy;
use super::argocd::{self, Argo};
use super::host::{Author, Change, CommitError, Committed, Hosted, LoggedCommit};
use super::model::{Environment, GITHUB_HOST, Release, RepoRef, State as ReleaseState};
use crate::githost::ChangeState;
use crate::githost::github::{App, Deployed};
use crate::hub::Hub;

/// How long a fetch or a push may take.
const NETWORK: Duration = Duration::from_secs(300);
/// How often a release repository is fetched when no webhook says it changed.
pub const POLL: Duration = Duration::from_secs(60);
/// How fresh a head must be that a commit is made on.
const FRESH: Duration = Duration::from_secs(2);
/// The branches releases proposed as pull requests come from.
pub const PROPOSAL_PREFIX: &str = "kiln/release-";

/// The branch a release is proposed from: its id, and when it was asked for, so a release of
/// another kiln (or of this one before its data was made again) with the same id is never taken
/// for it.
pub fn proposal_branch(r: &Release) -> String {
    format!("{PROPOSAL_PREFIX}{}-{}", r.id, r.created_at)
}

/// What the service keeps for GitHub's release repositories: when each mirror was fetched (none:
/// due now), one fetch or push at a time in each, and one report of a release at a time.
#[derive(Default)]
pub struct Mirrors {
    fetched: parking_lot::Mutex<HashMap<PathBuf, Instant>>,
    locks: parking_lot::Mutex<HashMap<PathBuf, Arc<async_lock::Mutex<()>>>>,
    reports: parking_lot::Mutex<HashMap<i64, Arc<async_lock::Mutex<()>>>>,
    /// A webhook said a release repository changed: the worker scans now.
    scan: std::sync::atomic::AtomicBool,
}

impl Mirrors {
    fn lock(&self, dir: &Path) -> Arc<async_lock::Mutex<()>> {
        Arc::clone(self.locks.lock().entry(dir.to_path_buf()).or_default())
    }

    /// Whether a webhook asked for a scan since the last time this was asked (and no longer).
    pub fn take_scan(&self) -> bool {
        self.scan.swap(false, std::sync::atomic::Ordering::AcqRel)
    }
}

/// A release repository on GitHub, as an environment names it.
pub struct Github {
    pub app: Arc<App>,
    /// `owner/name` on GitHub.
    pub name: String,
    pub branch: String,
    /// kiln's mirror: `<data>/deploy/github/<host>/<owner>/<name>.git`.
    mirror: Hosted,
    mirrors: Arc<Mirrors>,
}

/// The release repository `rr` names on GitHub, through kiln's GitHub App.
pub async fn open(d: &Deploy, rr: &RepoRef) -> Result<Github, String> {
    let app = crate::githost::github::app(&d.hub).await.ok_or("kiln has no GitHub App: make one in Settings (GitHub), or give `[ci.github]` its id and key")?;
    let root = d.hub.cfg.data("deploy").join("github").join(crate::githost::host_of(&app.web).replace(':', "_"));
    let mirror = Hosted::new(&root);
    name_ok(&rr.repo)?;
    Ok(Github { app, name: rr.repo.clone(), branch: rr.branch.clone(), mirror, mirrors: Arc::clone(&d.github) })
}

/// A repository's name on GitHub: `owner/name`, each of letters, digits, `-`, `_` and `.`.
pub fn name_ok(name: &str) -> Result<(), String> {
    let seg = |s: &str| !s.is_empty() && s != "." && s != ".." && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    match name.split_once('/') {
        Some((owner, repo)) if seg(owner) && seg(repo) && !repo.ends_with(".git") => Ok(()),
        _ => Err(format!("{name:?}: a repository on GitHub is `owner/name`")),
    }
}

impl Github {
    /// Where GitHub serves it (Argo CD reads it there).
    pub fn url(&self) -> String {
        format!("{}/{}.git", self.app.web, self.name)
    }

    /// Where KilnCD reads it: kiln's mirror, on this machine.
    pub fn mirror_url(&self) -> anyhow::Result<String> {
        self.mirror.url(&self.name)
    }

    fn dir(&self) -> anyhow::Result<PathBuf> {
        self.mirror.path(&self.name)
    }

    /// It as kiln's git host calls take a repository (never stored).
    pub fn row(&self) -> crate::store::Repo {
        crate::store::Repo {
            id: String::new(),
            name: self.name.clone(),
            clone_url: self.url(),
            branches: Vec::new(),
            poll_seconds: 0,
            enabled: true,
            created_at: 0,
            last_polled_at: None,
            last_error: None,
            mode: "upstream".into(),
            default_branch: Some(self.branch.clone()),
            mirror: None,
        }
    }

    /// git's settings for one call to GitHub with an installation token for this repository
    /// (`write`: one that pushes), set through its environment.
    async fn auth(&self, write: bool) -> anyhow::Result<[(String, String); 3]> {
        let perms: BTreeMap<String, String> = [("contents".to_owned(), (if write { "write" } else { "read" }).to_owned()), ("metadata".to_owned(), "read".to_owned())].into();
        let token = self.app.token(&self.name, &perms).await?;
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
        Ok([
            ("GIT_CONFIG_COUNT".into(), "1".into()),
            ("GIT_CONFIG_KEY_0".into(), format!("http.{}/.extraheader", self.app.web)),
            ("GIT_CONFIG_VALUE_0".into(), format!("Authorization: Basic {basic}")),
        ])
    }

    /// git in the mirror, talking to GitHub: what it said, whether it succeeded or not. It reads no
    /// global or system configuration: a URL rewrite or a credential helper there would have it
    /// fetch or push as whoever runs kiln rather than as the App.
    async fn remote(&self, args: &[&str], write: bool) -> anyhow::Result<std::process::Output> {
        let mut cmd = std::process::Command::new("git");
        crate::os::git_no_prompts(&mut cmd);
        cmd.env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1");
        cmd.arg("-C").arg(self.dir()?).args(args).stdin(std::process::Stdio::null());
        for (k, v) in self.auth(write).await? {
            cmd.env(k, v);
        }
        crate::proc::output(cmd, NETWORK).await.with_context(|| format!("git {}", args.first().copied().unwrap_or("")))
    }

    /// Brings the mirror to GitHub's branches, unless it was within `fresh` (and no webhook said
    /// it changed since).
    pub async fn fetch(&self, fresh: Duration) -> anyhow::Result<()> {
        let dir = self.dir()?;
        let lock = self.mirrors.lock(&dir);
        let _one = lock.lock().await;
        if self.mirrors.fetched.lock().get(&dir).is_some_and(|at| at.elapsed() < fresh) {
            return Ok(());
        }
        let (mirror, name, branch) = (self.mirror.clone(), self.name.clone(), self.branch.clone());
        crate::rt::blocking(move || mirror.ensure(&name, &branch)).await??;
        let started = Instant::now();
        let url = self.url();
        let out = self.remote(&["fetch", "--prune", "--quiet", &url, "+refs/heads/*:refs/heads/*"], false).await?;
        if !out.status.success() {
            bail!("git fetch from {url}: {}", said(&out));
        }
        self.mirrors.fetched.lock().insert(dir, started);
        Ok(())
    }

    /// The branch's head on GitHub, fetched within `fresh`.
    pub async fn head(&self, fresh: Duration) -> anyhow::Result<Option<String>> {
        self.fetch(fresh).await?;
        self.branch_head(&self.branch).await
    }

    /// A branch's head in the mirror, as fetched last.
    async fn branch_head(&self, branch: &str) -> anyhow::Result<Option<String>> {
        let (mirror, name, branch) = (self.mirror.clone(), self.name.clone(), branch.to_owned());
        crate::rt::blocking(move || mirror.head(&name, &branch)).await?
    }

    /// Whether the mirror holds a commit, fetching once when it doesn't.
    async fn holds(&self, rev: &str) -> anyhow::Result<bool> {
        let has = |rev: String| {
            let (mirror, name) = (self.mirror.clone(), self.name.clone());
            async move { crate::rt::blocking(move || mirror.has(&name, &rev)).await? }
        };
        if has(rev.to_owned()).await? {
            return Ok(true);
        }
        self.fetch(Duration::ZERO).await?;
        has(rev.to_owned()).await
    }

    /// A file at a commit (`None`: the commit has no such file). A commit GitHub hasn't got is an
    /// error, not a file that isn't there.
    pub async fn read(&self, rev: &str, path: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let commit = rev.split(['^', '~']).next().unwrap_or(rev);
        if !self.holds(commit).await? {
            bail!("{} has no commit {commit}", self.url());
        }
        let (mirror, name, rev, path) = (self.mirror.clone(), self.name.clone(), rev.to_owned(), path.to_owned());
        crate::rt::blocking(move || mirror.read(&name, &rev, &path)).await?
    }

    /// Commits `changes` on `branch`, made on `parent` (`None`: on nothing) and pushed only while
    /// GitHub's `branch` is at `expected` (`None`: only if it isn't there yet).
    pub async fn commit_to(&self, branch: &str, parent: Option<&str>, expected: Option<&str>, changes: Vec<Change>, message: String, author: Author) -> Result<Committed, CommitError> {
        if let Some(p) = parent
            && !self.holds(p).await?
        {
            return Err(CommitError::Other(anyhow!("{} has no commit {p}", self.url())));
        }
        let (mirror, name, base, parent_owned) = (self.mirror.clone(), self.name.clone(), self.branch.clone(), parent.map(str::to_owned));
        let made = crate::rt::blocking(move || -> anyhow::Result<Committed> {
            mirror.ensure(&name, &base)?;
            mirror.make(&name, parent_owned.as_deref(), &changes, &message, &author)
        })
        .await
        .map_err(CommitError::Other)?
        .map_err(CommitError::Other)?;
        let Committed::New(sha) = &made else { return Ok(made) };
        let dir = self.dir()?;
        let lock = self.mirrors.lock(&dir);
        let _one = lock.lock().await;
        let url = self.url();
        let lease = format!("--force-with-lease=refs/heads/{branch}:{}", expected.unwrap_or(""));
        let out = self.remote(&["push", "--porcelain", &lease, &url, &format!("{sha}:refs/heads/{branch}")], true).await?;
        if !out.status.success() {
            let why = said(&out);
            // The lease: GitHub's branch isn't where the commit was made any more.
            if why.contains("stale info") || why.contains("fetch first") || why.contains("non-fast-forward") {
                self.mirrors.fetched.lock().remove(&dir);
                return Err(CommitError::Moved);
            }
            return Err(CommitError::Other(anyhow!("git push to {url}: {why}")));
        }
        // The mirror follows, so what's read next sees it without a fetch.
        let (mirror, name, branch, sha2) = (self.mirror.clone(), self.name.clone(), branch.to_owned(), sha.clone());
        crate::rt::blocking(move || mirror.set_head(&name, &branch, &sha2)).await.map_err(CommitError::Other)?.map_err(CommitError::Other)?;
        Ok(made)
    }

    /// The newest commit on the branch whose message has the line `<key>: <value>`.
    pub async fn find_trailer(&self, key: &str, value: &str) -> anyhow::Result<Option<String>> {
        self.fetch(FRESH).await?;
        let (mirror, name, branch, key, value) = (self.mirror.clone(), self.name.clone(), self.branch.clone(), key.to_owned(), value.to_owned());
        crate::rt::blocking(move || mirror.find_trailer(&name, &branch, &key, &value)).await?
    }

    /// The commits on the branch after `since`, fetched within `fresh`: its head, and them.
    pub async fn commits_since(&self, since: Option<&str>, fresh: Duration) -> anyhow::Result<(Option<String>, Vec<LoggedCommit>)> {
        self.fetch(fresh).await?;
        let (mirror, name, branch, since) = (self.mirror.clone(), self.name.clone(), self.branch.clone(), since.map(str::to_owned));
        crate::rt::blocking(move || -> anyhow::Result<(Option<String>, Vec<LoggedCommit>)> {
            let head = mirror.head(&name, &branch)?;
            // A commit the branch has lost (pushed over) can't be counted from: from its head on.
            let commits = match &since {
                Some(s) if mirror.has(&name, s)? => mirror.commits_since(&name, &branch, Some(s))?,
                Some(_) => Vec::new(),
                None => Vec::new(),
            };
            Ok((head, commits))
        })
        .await?
    }

    // ------------------------------------------------------------------ pull requests

    /// Proposes a release as a pull request: `changes` committed on the branch's head, on the
    /// release's own `branch` ([`proposal_branch`]), and a pull request from it. One proposed before
    /// (a restart between the two) is found, not made again.
    pub async fn propose(&self, branch: &str, changes: Vec<Change>, message: String, author: Author, body: &str) -> anyhow::Result<Proposal> {
        let branch = branch.to_owned();
        let row = self.row();
        if let Some((pr, state)) = self.app.find_change(&row, &branch).await? {
            return Ok(Proposal { number: pr.number, url: pr.url, branch, state });
        }
        let title = message.lines().next().unwrap_or_default().to_owned();
        let mut made = None;
        for _ in 0..5 {
            let head = self.head(Duration::ZERO).await?.ok_or_else(|| anyhow!("{} has no {} yet: kiln proposes a release on a branch with a commit", self.url(), self.branch))?;
            let expected = self.branch_head(&branch).await?;
            match self.commit_to(&branch, Some(&head), expected.as_deref(), changes.clone(), message.clone(), author.clone()).await {
                Ok(_) => {
                    made = Some(());
                    break;
                }
                Err(CommitError::Moved) => continue,
                Err(CommitError::Other(e)) => return Err(e),
            }
        }
        made.ok_or_else(|| anyhow!("{branch} kept moving: five tries"))?;
        let pr = self.app.open_change(&row, &branch, &self.branch, &title, body).await?;
        Ok(Proposal { number: pr.number, url: pr.url, branch, state: ChangeState::Open })
    }

    /// Where a release's pull request is.
    pub async fn proposal(&self, number: u64) -> anyhow::Result<ChangeState> {
        self.app.change(&self.row(), number).await
    }

    /// A release's pull request closed unmerged (saying why), its branch deleted.
    pub async fn withdraw(&self, number: u64, branch: &str, why: &str) -> anyhow::Result<()> {
        self.app.close_change(&self.row(), number, why).await?;
        self.drop_branch(branch).await;
        Ok(())
    }

    /// A release's branch, gone once its pull request is merged or closed.
    pub async fn drop_branch(&self, branch: &str) {
        if let Err(e) = self.app.delete_branch(&self.row(), branch).await {
            tracing::debug!(repo = %self.name, branch, error = %format!("{e:#}"), "deploy: a release's branch on GitHub stays");
        }
    }
}

/// A release proposed as a pull request.
#[derive(Debug, Clone)]
pub struct Proposal {
    pub number: u64,
    pub url: String,
    pub branch: String,
    pub state: ChangeState,
}

/// What git said, its token never in it (it's in the environment, not the command line or
/// output), at most a few lines.
fn said(out: &std::process::Output) -> String {
    let text = format!("{}\n{}", String::from_utf8_lossy(&out.stderr), String::from_utf8_lossy(&out.stdout));
    crate::util::truncate(text.trim(), 600).to_owned()
}

/// A release repository on GitHub that isn't there yet, made: private and empty, in its owner's
/// organization (docs/deploy-api.md §7). One that is there is left as it is.
pub async fn ensure(d: &Deploy, rr: &RepoRef) -> Result<(), String> {
    let gh = open(d, rr).await?;
    let e = |e: anyhow::Error| format!("{e:#}");
    if gh.app.repository(&rr.repo).await.map_err(e)?.is_some() {
        return Ok(());
    }
    gh.app.create_repository(&rr.repo, "kiln's release repository: what each environment runs, one release a commit").await.map_err(e)?;
    d.hub.store.audit("kiln".into(), "repo.create".into(), format!("{} on GitHub (a release repository, docs/deploy-api.md §6)", rr.repo)).await;
    Ok(())
}

// ------------------------------------------------------------------ deployments

/// GitHub's name for a registration's environment: `<environment>/<app>`, so each app's
/// deployments stand apart (GitHub marks one inactive when the next to its environment succeeds).
pub fn environment_name(app: &str, env: &str) -> String {
    format!("{env}/{app}")
}

/// What GitHub hears of a release in `state` (`None`: nothing; GitHub marks one superseded or
/// rolled back inactive itself).
pub fn deployment_state(state: ReleaseState) -> Option<&'static str> {
    match state {
        ReleaseState::Delivering => Some("in_progress"),
        ReleaseState::Succeeded => Some("success"),
        ReleaseState::Failed => Some("failure"),
        ReleaseState::Cancelled => Some("inactive"),
        _ => None,
    }
}

fn settled(state: &str) -> bool {
    matches!(state, "success" | "failure" | "inactive")
}

/// Reports a release that moved to `r.state` to GitHub, as a deployment of its commit, where its
/// environment's release repository is on GitHub: in the background, one report of a release at a
/// time, never one that took it back from where it settled; tried again for a few minutes while
/// GitHub doesn't take it.
pub fn report(d: &Deploy, r: &Release) {
    let (Some(state), Some(_)) = (deployment_state(r.state), r.commit.as_ref()) else { return };
    let (hub, mirrors, id, environment, app, at) = (Arc::clone(&d.hub), Arc::clone(&d.github), r.id, r.environment.clone(), r.app.clone(), r.state);
    crate::rt::spawn(async move {
        for attempt in 0..5u32 {
            match report_now(&hub, &mirrors, id, &environment, &app, at, state).await {
                Ok(()) => return,
                Err(e) if attempt == 4 => tracing::warn!(release = id, error = %format!("{e:#}"), "deploy: GitHub didn't take a release's deployment status"),
                Err(e) => tracing::debug!(release = id, error = %format!("{e:#}"), "deploy: GitHub didn't take a release's deployment status yet"),
            }
            crate::rt::sleep(Duration::from_secs(30 * u64::from(attempt + 1))).await;
        }
    });
}

async fn report_now(hub: &Arc<Hub>, mirrors: &Mirrors, id: i64, environment: &str, app: &str, at: ReleaseState, state: &str) -> anyhow::Result<()> {
    let st = &hub.store;
    let Some(env) = super::store::environment(st, environment).await? else { return Ok(()) };
    if env.release_repo.host != GITHUB_HOST {
        return Ok(());
    }
    let lock = Arc::clone(mirrors.reports.lock().entry(id).or_default());
    let _one = lock.lock().await;
    let Some(r) = super::store::release(st, id).await? else { return Ok(()) };
    let Some(commit) = r.commit.clone() else { return Ok(()) };
    let key = format!("deploy/github/deployment/{id}");
    let last = st.kv_get(key.clone()).await?;
    // Settled once, it stays: a report that comes late never takes it back.
    if last.as_deref() == Some(state) || last.as_deref().is_some_and(settled) {
        return Ok(());
    }
    let app_obj = crate::githost::github::app(hub).await.ok_or_else(|| anyhow!("kiln has no GitHub App any more"))?;
    let repo = crate::store::Repo { clone_url: format!("{}/{}.git", app_obj.web, env.release_repo.repo), name: env.release_repo.repo.clone(), ..empty_row() };
    let base = hub.cfg.server.public_url.trim_end_matches('/');
    let page = format!("{base}/deployments/releases/{id}");
    let what = format!("release {id}: {} {}{}", r.kind.as_str(), r.version.as_deref().unwrap_or(""), r.message.as_deref().filter(|_| at != ReleaseState::Delivering).map(|m| format!(" · {m}")).unwrap_or_default());
    let s = Deployed { state, description: what.trim(), environment_url: None, log_url: Some(&page) };
    app_obj.deployment_status(&repo, &commit, &environment_name(app, environment), &s).await?;
    st.kv_set(key, state.to_owned()).await?;
    Ok(())
}

fn empty_row() -> crate::store::Repo {
    crate::store::Repo {
        id: String::new(),
        name: String::new(),
        clone_url: String::new(),
        branches: Vec::new(),
        poll_seconds: 0,
        enabled: true,
        created_at: 0,
        last_polled_at: None,
        last_error: None,
        mode: "upstream".into(),
        default_branch: None,
        mirror: None,
    }
}

// ------------------------------------------------------------------ webhooks

/// The App's webhook, for release repositories: a push to a branch an environment releases from
/// has its mirror fetched now (and a scan run); a pull request of a release (opened, merged,
/// closed) moves that release now.
pub async fn on_event(d: &Deploy, event: &str, v: &Value) {
    let full = v.pointer("/repository/full_name").and_then(Value::as_str).unwrap_or_default();
    let Ok(envs) = super::store::environments(&d.hub.store).await else { return };
    let ours: Vec<&Environment> = envs.iter().filter(|e| e.release_repo.host == GITHUB_HOST && e.release_repo.repo.eq_ignore_ascii_case(full)).collect();
    if ours.is_empty() {
        return;
    }
    let moved = match event {
        "push" => v.get("ref").and_then(Value::as_str).is_some_and(|r| ours.iter().any(|e| r == format!("refs/heads/{}", e.release_repo.branch))),
        // A merge moves the base branch too.
        "pull_request" => v.pointer("/pull_request/merged").and_then(Value::as_bool) == Some(true),
        _ => false,
    };
    if moved && let Ok(gh) = open(d, &ours.first().map(|e| e.release_repo.clone()).unwrap_or_default()).await {
        if let Ok(dir) = gh.dir() {
            d.github.fetched.lock().remove(&dir);
        }
        d.github.scan.store(true, std::sync::atomic::Ordering::Release);
    }
    if event == "pull_request"
        && let Some(head) = v.pointer("/pull_request/head/ref").and_then(Value::as_str)
        && let Some(id) = head.strip_prefix(PROPOSAL_PREFIX).and_then(|rest| rest.split('-').next()).and_then(|n| n.parse::<i64>().ok())
        && let Ok(Some(r)) = super::store::release(&d.hub.store, id).await
        && proposal_branch(&r) == head
        && ours.iter().any(|e| e.name == r.environment)
    {
        let _ = super::store::schedule(&d.hub.store, id, crate::util::now_unix()).await;
    }
    d.wake.notify();
}

// ------------------------------------------------------------------ Argo CD

/// How long Argo CD's token for a release repository on GitHub is used before kiln puts a new one
/// in its place (GitHub's last an hour), and how long a new one lasts at least.
const READER_SECS: i64 = 30 * 60;
const READER_LASTS: i64 = 45 * 60;

/// Argo CD reads a release repository on GitHub with an installation token that reads that one
/// repository, put in its place every half hour, or at once when Argo CD can't read with the one
/// it has.
pub async fn argo_reader(d: &Deploy, argo: &Argo, env: &Environment, url: &str) -> Result<(), String> {
    let key = format!("deploy/github/argo-reader/{}/{}", argo.integration, env.release_repo.repo);
    let now = crate::util::now_unix();
    let put = d.hub.store.kv_get(key.clone()).await.ok().flatten().and_then(|s| s.parse::<i64>().ok());
    if put.is_some_and(|at| now - at < READER_SECS) {
        let reads = argo.repository(url).await?.is_some_and(|r| r.pointer("/connectionState/status").and_then(Value::as_str) != Some("Failed"));
        if reads {
            return Ok(());
        }
    }
    let gh = open(d, &env.release_repo).await?;
    let perms: BTreeMap<String, String> = [("contents".to_owned(), "read".to_owned()), ("metadata".to_owned(), "read".to_owned())].into();
    let token = gh.app.token_lasting(&gh.name, &perms, READER_LASTS).await.map_err(|e| format!("{e:#}"))?;
    argo.put_repository(url, &argocd::release_repo_name(&env.release_repo.repo), "x-access-token", &token).await.map_err(|why| format!("Argo CD can't read the release repository at {url}: {why}"))?;
    d.hub.store.kv_set(key, now.to_string()).await.map_err(|e| format!("{e:#}"))?;
    Ok(())
}

/// Every Argo CD environment's token for its release repository on GitHub, put again where it's
/// due (the worker, every few minutes).
pub async fn refresh_readers(d: &Deploy) {
    let Ok(envs) = super::store::environments(&d.hub.store).await else { return };
    for env in envs.iter().filter(|e| e.release_repo.host == GITHUB_HOST) {
        let Some(integration) = super::appset::integration_of(env) else { continue };
        let done = match argocd::Argo::of(&d.hub, integration).await {
            Ok(argo) => {
                let url = super::appset::release_url(d, &argo, env).await;
                argo_reader(d, &argo, env, &url).await
            }
            Err(e) => Err(e),
        };
        if let Err(why) = done {
            tracing::warn!(environment = %env.name, error = %why, "deploy: Argo CD's token for a release repository on GitHub wasn't put in place");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_states() {
        assert!(name_ok("acme/releases").is_ok());
        for bad in ["releases", "acme/", "/r", "a/b/c", "acme/r.git", "../x", "a b/c"] {
            assert!(name_ok(bad).is_err(), "{bad}");
        }
        assert_eq!(environment_name("web", "production"), "production/web");
        assert_eq!(deployment_state(ReleaseState::Delivering), Some("in_progress"));
        assert_eq!(deployment_state(ReleaseState::Superseded), None);
        assert!(settled("failure") && !settled("in_progress"));
    }
}
