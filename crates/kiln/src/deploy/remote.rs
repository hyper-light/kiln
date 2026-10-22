//! Release repositories on another git host (docs/deploy-api.md §7): GitHub, through kiln's GitHub
//! App; a GitLab `[[ci.gitlab]]` names, with its token; or Bitbucket, with `[ci.bitbucket]`'s:
//! - **Made** in an organization, group or workspace when one isn't there yet.
//! - **Read** through a mirror of kiln's, fetched with credentials that read that repository;
//!   KilnCD reads the mirror, Argo CD reads the host with credentials of its own that read that one
//!   repository (on GitHub an installation token kiln replaces before it ends, on GitLab a deploy
//!   token, on Bitbucket a deploy key over SSH).
//! - **Written** by a push of a commit made in the mirror, with a lease on the head it was made
//!   on: a commit someone made beside it is never lost.
//! - **Pull requests** (GitLab's merge requests), where an environment says `via: pull-request`: a
//!   release is proposed from a branch of its own (`kiln/release-<id>-<when it was asked for>`);
//!   merged, its merge is delivered; closed unmerged, it's cancelled.
//! - **Deployments**: each release is a deployment of its commit to `<environment>/<app>`, its
//!   status following the release (on Bitbucket, a build status of its commit).
//! - **Webhooks**: a push has the mirror fetched at once, a pull or merge request moves its release
//!   at once; without them kiln polls.
//!
//! Credentials go to git in its environment, never on its command line, in a file, or in what's
//! said.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use base64::Engine as _;
use serde_json::{Value, json};

use super::Deploy;
use super::argocd::{self, Argo};
use super::host::{Author, Change, CommitError, Committed, Hosted, LoggedCommit};
use super::model::{BITBUCKET_HOST, Environment, GITHUB_HOST, GITLAB_HOST, Release, RepoRef, State as ReleaseState};
use crate::githost::bitbucket::Bitbucket;
use crate::githost::github::{App, Deployed};
use crate::githost::gitlab::Gitlab;
use crate::githost::{ChangeRef, ChangeState};
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

/// What the service keeps for release repositories on other hosts: when each mirror was fetched
/// (none: due now), one fetch or push at a time in each, and one report of a release at a time.
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

/// How a host calls a change proposed to a branch, and its number: GitHub's and Bitbucket's pull
/// request #3, GitLab's merge request !3 (`kind`: a release repository's `host`).
pub fn change_name(kind: &str, number: u64) -> String {
    if kind == GITLAB_HOST { format!("merge request !{number}") } else { format!("pull request #{number}") }
}

/// A host's name as people call it (`kind`: a release repository's `host`).
pub fn host_label(kind: &str) -> &'static str {
    match kind {
        GITLAB_HOST => "GitLab",
        GITHUB_HOST => "GitHub",
        BITBUCKET_HOST => "Bitbucket",
        _ => "its host",
    }
}

/// The host a release repository is on.
#[derive(Clone)]
pub enum Host {
    Github(Arc<App>),
    Gitlab(Arc<Gitlab>),
    Bitbucket(Arc<Bitbucket>),
}

impl Host {
    /// Its web origin.
    pub fn web(&self) -> &str {
        match self {
            Host::Github(a) => &a.web,
            Host::Gitlab(g) => &g.web,
            Host::Bitbucket(b) => &b.web,
        }
    }

    /// Its name, as people call it.
    pub fn label(&self) -> &'static str {
        match self {
            Host::Github(_) => "GitHub",
            Host::Gitlab(_) => "GitLab",
            Host::Bitbucket(_) => "Bitbucket",
        }
    }

    /// Its name in a release repository (`release_repo.host`).
    pub fn kind(&self) -> &'static str {
        match self {
            Host::Github(_) => GITHUB_HOST,
            Host::Gitlab(_) => GITLAB_HOST,
            Host::Bitbucket(_) => BITBUCKET_HOST,
        }
    }
}

/// A release repository on another host, as an environment names it.
pub struct Remote {
    pub host: Host,
    /// `owner/name` (GitLab's `group/…/name`, Bitbucket's `workspace/name`) on the host.
    pub name: String,
    pub branch: String,
    /// kiln's mirror: `<data>/deploy/<host kind>/<host>/<owner>/<name>.git` (a GitLab subgroup's
    /// path folded into its owner, `group__sub`).
    mirror: Hosted,
    mirrors: Arc<Mirrors>,
}

/// The release repository `rr` names on its host: GitHub through kiln's GitHub App, the GitLab
/// whose token covers it, or Bitbucket.
pub async fn open(d: &Deploy, rr: &RepoRef) -> Result<Remote, String> {
    name_ok(&rr.host, &rr.repo)?;
    let host = match rr.host.as_str() {
        GITHUB_HOST => Host::Github(crate::githost::github::app(&d.hub).await.ok_or("kiln has no GitHub App: make one in Settings (GitHub), or give `[ci.github]` its id and key")?),
        GITLAB_HOST => Host::Gitlab(gitlab_for(&d.hub, &rr.repo).await?),
        BITBUCKET_HOST => Host::Bitbucket(bitbucket_for(&d.hub, &rr.repo).await?),
        other => return Err(format!("{other}: kiln reaches release repositories on github, gitlab and bitbucket")),
    };
    let root = d.hub.cfg.data("deploy").join(host.kind()).join(crate::githost::host_of(host.web()).replace([':', '/'], "_"));
    Ok(Remote { host, name: rr.repo.clone(), branch: rr.branch.clone(), mirror: Hosted::new(&root), mirrors: Arc::clone(&d.remote) })
}

/// The GitLab `[[ci.gitlab]]` names whose token covers the project `path`.
async fn gitlab_for(hub: &Hub, path: &str) -> Result<Arc<Gitlab>, String> {
    let hosts = crate::githost::gitlab::hosts(hub).await;
    if hosts.is_empty() {
        return Err("kiln knows no GitLab: give `[[ci.gitlab]]` its URL and a token".into());
    }
    hosts
        .into_iter()
        .find(|g| g.name_of(&row_of(&g.web, path, None)).as_deref() == Some(path))
        .ok_or_else(|| format!("{path}: no GitLab of kiln's (`[[ci.gitlab]]`) has a token for its group"))
}

/// Bitbucket, where `[ci.bitbucket]`'s credentials cover the repository `path`.
async fn bitbucket_for(hub: &Hub, path: &str) -> Result<Arc<Bitbucket>, String> {
    let b = crate::githost::bitbucket::host(hub).await.ok_or("kiln knows no Bitbucket: give `[ci.bitbucket]` an access token, or a user and an app password")?;
    if b.name_of(&row_of(&b.web, path, None)).as_deref() != Some(path) {
        return Err(format!("{path}: `[ci.bitbucket]`'s credentials don't cover its workspace"));
    }
    Ok(b)
}

/// A repository's name on its host: `owner/name` on GitHub, `group/…/name` on GitLab,
/// `workspace/name` on Bitbucket, each part of letters, digits, `-`, `_` and `.`.
pub fn name_ok(host: &str, name: &str) -> Result<(), String> {
    let seg = |s: &str| !s.is_empty() && s != "." && s != ".." && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    let parts: Vec<&str> = name.split('/').collect();
    let deep = if host == GITLAB_HOST { parts.len() >= 2 } else { parts.len() == 2 };
    if deep && parts.iter().all(|p| seg(p)) && !name.ends_with(".git") {
        Ok(())
    } else if host == GITLAB_HOST {
        Err(format!("{name:?}: a project on GitLab is `group/name` (or `group/subgroup/name`)"))
    } else if host == BITBUCKET_HOST {
        Err(format!("{name:?}: a repository on Bitbucket is `workspace/name`"))
    } else {
        Err(format!("{name:?}: a repository on GitHub is `owner/name`"))
    }
}

/// A repository as kiln's git host calls take one (never stored).
fn row_of(web: &str, name: &str, branch: Option<&str>) -> crate::store::Repo {
    crate::store::Repo {
        id: String::new(),
        name: name.to_owned(),
        clone_url: format!("{web}/{name}.git"),
        branches: Vec::new(),
        poll_seconds: 0,
        enabled: true,
        created_at: 0,
        last_polled_at: None,
        last_error: None,
        mode: "upstream".into(),
        default_branch: branch.map(str::to_owned),
        mirror: None,
    }
}

impl Remote {
    /// Where the host serves it over HTTPS (kiln fetches and pushes there).
    pub fn url(&self) -> String {
        format!("{}/{}.git", self.host.web(), self.name)
    }

    /// Where Argo CD reads it: there, or on Bitbucket over SSH (its deploy key reads there).
    pub fn reader_url(&self) -> String {
        match &self.host {
            Host::Bitbucket(b) => b.ssh_url(&self.name),
            _ => self.url(),
        }
    }

    /// Where KilnCD reads it: kiln's mirror, on this machine.
    pub fn mirror_url(&self) -> anyhow::Result<String> {
        self.mirror.url(&self.mirror_name())
    }

    /// Its name in the mirror's root: a GitLab subgroup's path folded into its owner.
    fn mirror_name(&self) -> String {
        match self.name.rsplit_once('/') {
            Some((owner, name)) => format!("{}/{name}", owner.replace('/', "__")),
            None => self.name.clone(),
        }
    }

    fn dir(&self) -> anyhow::Result<PathBuf> {
        self.mirror.path(&self.mirror_name())
    }

    /// It as kiln's git host calls take a repository.
    pub fn row(&self) -> crate::store::Repo {
        row_of(self.host.web(), &self.name, Some(&self.branch))
    }

    /// git's settings for one call to the host with credentials for this repository (`write`: one
    /// that pushes), set through its environment.
    async fn auth(&self, write: bool) -> anyhow::Result<[(String, String); 3]> {
        let header = match &self.host {
            Host::Github(app) => {
                let perms: BTreeMap<String, String> = [("contents".to_owned(), (if write { "write" } else { "read" }).to_owned()), ("metadata".to_owned(), "read".to_owned())].into();
                let token = app.token(&self.name, &perms).await?;
                format!("Authorization: Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}")))
            }
            Host::Gitlab(g) => {
                let reader = g.reader(&self.row())?;
                let (k, v) = reader.header.ok_or_else(|| anyhow!("GitLab's reader has no credentials"))?;
                format!("{k}: {v}")
            }
            Host::Bitbucket(b) => {
                let reader = b.reader(&self.row())?;
                let (k, v) = reader.header.ok_or_else(|| anyhow!("Bitbucket's reader has no credentials"))?;
                format!("{k}: {v}")
            }
        };
        Ok([
            ("GIT_CONFIG_COUNT".into(), "1".into()),
            ("GIT_CONFIG_KEY_0".into(), format!("http.{}/.extraheader", self.host.web())),
            ("GIT_CONFIG_VALUE_0".into(), header),
        ])
    }

    /// git in the mirror, talking to the host: what it said, whether it succeeded or not. It reads
    /// no global or system configuration: a URL rewrite or a credential helper there would have it
    /// fetch or push as whoever runs kiln rather than with kiln's credentials.
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

    /// Brings the mirror to the host's branches, unless it was within `fresh` (and no webhook said
    /// it changed since).
    pub async fn fetch(&self, fresh: Duration) -> anyhow::Result<()> {
        let dir = self.dir()?;
        let lock = self.mirrors.lock(&dir);
        let _one = lock.lock().await;
        if self.mirrors.fetched.lock().get(&dir).is_some_and(|at| at.elapsed() < fresh) {
            return Ok(());
        }
        let (mirror, name, branch) = (self.mirror.clone(), self.mirror_name(), self.branch.clone());
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

    /// The branch's head on the host, fetched within `fresh`.
    pub async fn head(&self, fresh: Duration) -> anyhow::Result<Option<String>> {
        self.fetch(fresh).await?;
        self.branch_head(&self.branch).await
    }

    /// A branch's head in the mirror, as fetched last.
    async fn branch_head(&self, branch: &str) -> anyhow::Result<Option<String>> {
        let (mirror, name, branch) = (self.mirror.clone(), self.mirror_name(), branch.to_owned());
        crate::rt::blocking(move || mirror.head(&name, &branch)).await?
    }

    /// Whether the mirror holds a commit, fetching once when it doesn't.
    async fn holds(&self, rev: &str) -> anyhow::Result<bool> {
        let has = |rev: String| {
            let (mirror, name) = (self.mirror.clone(), self.mirror_name());
            async move { crate::rt::blocking(move || mirror.has(&name, &rev)).await? }
        };
        if has(rev.to_owned()).await? {
            return Ok(true);
        }
        self.fetch(Duration::ZERO).await?;
        has(rev.to_owned()).await
    }

    /// A file at a commit (`None`: the commit has no such file). A commit the host hasn't got is an
    /// error, not a file that isn't there.
    pub async fn read(&self, rev: &str, path: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let commit = rev.split(['^', '~']).next().unwrap_or(rev);
        if !self.holds(commit).await? {
            bail!("{} has no commit {commit}", self.url());
        }
        let (mirror, name, rev, path) = (self.mirror.clone(), self.mirror_name(), rev.to_owned(), path.to_owned());
        crate::rt::blocking(move || mirror.read(&name, &rev, &path)).await?
    }

    /// Commits `changes` on `branch`, made on `parent` (`None`: on nothing) and pushed only while
    /// the host's `branch` is at `expected` (`None`: only if it isn't there yet).
    pub async fn commit_to(&self, branch: &str, parent: Option<&str>, expected: Option<&str>, changes: Vec<Change>, message: String, author: Author) -> Result<Committed, CommitError> {
        if let Some(p) = parent
            && !self.holds(p).await?
        {
            return Err(CommitError::Other(anyhow!("{} has no commit {p}", self.url())));
        }
        let (mirror, name, base, parent_owned) = (self.mirror.clone(), self.mirror_name(), self.branch.clone(), parent.map(str::to_owned));
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
            // The lease: the host's branch isn't where the commit was made any more.
            if why.contains("stale info") || why.contains("fetch first") || why.contains("non-fast-forward") {
                self.mirrors.fetched.lock().remove(&dir);
                return Err(CommitError::Moved);
            }
            return Err(CommitError::Other(anyhow!("git push to {url}: {why}")));
        }
        // The mirror follows, so what's read next sees it without a fetch.
        let (mirror, name, branch, sha2) = (self.mirror.clone(), self.mirror_name(), branch.to_owned(), sha.clone());
        crate::rt::blocking(move || mirror.set_head(&name, &branch, &sha2)).await.map_err(CommitError::Other)?.map_err(CommitError::Other)?;
        Ok(made)
    }

    /// The newest commit on the branch whose message has the line `<key>: <value>`.
    pub async fn find_trailer(&self, key: &str, value: &str) -> anyhow::Result<Option<String>> {
        self.fetch(FRESH).await?;
        let (mirror, name, branch, key, value) = (self.mirror.clone(), self.mirror_name(), self.branch.clone(), key.to_owned(), value.to_owned());
        crate::rt::blocking(move || mirror.find_trailer(&name, &branch, &key, &value)).await?
    }

    /// The commits on the branch after `since`, fetched within `fresh`: its head, and them.
    pub async fn commits_since(&self, since: Option<&str>, fresh: Duration) -> anyhow::Result<(Option<String>, Vec<LoggedCommit>)> {
        self.fetch(fresh).await?;
        let (mirror, name, branch, since) = (self.mirror.clone(), self.mirror_name(), self.branch.clone(), since.map(str::to_owned));
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

    /// Proposes a release as a pull request (a merge request on GitLab): `changes` committed on the
    /// branch's head, on the release's own `branch` ([`proposal_branch`]), and a pull request from
    /// it. One proposed before (a restart between the two) is found, not made again.
    pub async fn propose(&self, branch: &str, changes: Vec<Change>, message: String, author: Author, body: &str) -> anyhow::Result<Proposal> {
        let branch = branch.to_owned();
        let row = self.row();
        let found = match &self.host {
            Host::Github(app) => app.find_change(&row, &branch).await?,
            Host::Gitlab(g) => g.find_change(&row, &branch).await?,
            Host::Bitbucket(b) => b.find_change(&row, &branch).await?,
        };
        if let Some((pr, state)) = found {
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
        let pr: ChangeRef = match &self.host {
            Host::Github(app) => app.open_change(&row, &branch, &self.branch, &title, body).await?,
            Host::Gitlab(g) => g.open_change(&row, &branch, &self.branch, &title, body).await?,
            Host::Bitbucket(b) => b.open_change(&row, &branch, &self.branch, &title, body).await?,
        };
        Ok(Proposal { number: pr.number, url: pr.url, branch, state: ChangeState::Open })
    }

    /// Where a release's pull request is.
    pub async fn proposal(&self, number: u64) -> anyhow::Result<ChangeState> {
        match &self.host {
            Host::Github(app) => app.change(&self.row(), number).await,
            Host::Gitlab(g) => g.change(&self.row(), number).await,
            Host::Bitbucket(b) => b.change(&self.row(), number).await,
        }
    }

    /// A release's pull request closed unmerged (saying why), its branch deleted.
    pub async fn withdraw(&self, number: u64, branch: &str, why: &str) -> anyhow::Result<()> {
        match &self.host {
            Host::Github(app) => app.close_change(&self.row(), number, why).await?,
            Host::Gitlab(g) => g.close_change(&self.row(), number, why).await?,
            Host::Bitbucket(b) => b.close_change(&self.row(), number, why).await?,
        }
        self.drop_branch(branch).await;
        Ok(())
    }

    // ------------------------------------------------------------------ the repository itself

    /// Whether the host has it, as it shows it to kiln.
    pub async fn exists(&self) -> anyhow::Result<bool> {
        Ok(match &self.host {
            Host::Github(app) => app.repository(&self.name).await?.is_some(),
            Host::Gitlab(g) => g.project(&self.name).await?.is_some(),
            Host::Bitbucket(b) => b.repository(&self.name).await?.is_some(),
        })
    }

    /// Makes it there, private and empty.
    pub async fn make(&self, description: &str) -> anyhow::Result<()> {
        match &self.host {
            Host::Github(app) => app.create_repository(&self.name, description).await,
            Host::Gitlab(g) => g.create_project(&self.name, description).await,
            Host::Bitbucket(b) => b.create_repository(&self.name, description).await,
        }
    }

    /// Deletes it there: one kiln made and couldn't fill.
    pub async fn delete(&self) -> anyhow::Result<()> {
        match &self.host {
            Host::Github(app) => app.delete_repository(&self.name).await,
            Host::Gitlab(g) => g.delete_project(&self.name).await,
            Host::Bitbucket(b) => b.delete_repository(&self.name).await,
        }
    }

    /// Pushes `refspec` of the git repository at `dir` to it, with credentials that write it (in
    /// git's environment, as every call to the host is).
    pub async fn push_from(&self, dir: &Path, refspec: &str) -> anyhow::Result<()> {
        let mut cmd = std::process::Command::new("git");
        crate::os::git_no_prompts(&mut cmd);
        cmd.env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1");
        let url = self.url();
        cmd.arg("-C").arg(dir).args(["push", "--quiet", "--", &url, refspec]).stdin(std::process::Stdio::null());
        for (k, v) in self.auth(true).await? {
            cmd.env(k, v);
        }
        let out = crate::proc::output(cmd, NETWORK).await.context("git push")?;
        anyhow::ensure!(out.status.success(), "git push to {url}: {}", said(&out));
        Ok(())
    }

    /// A release's branch, gone once its pull request is merged or closed.
    pub async fn drop_branch(&self, branch: &str) {
        let gone = match &self.host {
            Host::Github(app) => app.delete_branch(&self.row(), branch).await,
            Host::Gitlab(g) => g.delete_branch(&self.row(), branch).await,
            Host::Bitbucket(b) => b.delete_branch(&self.row(), branch).await,
        };
        if let Err(e) = gone {
            tracing::debug!(repo = %self.name, branch, error = %format!("{e:#}"), "deploy: a release's branch stays");
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

/// What git said, its credentials never in it (they're in the environment, not the command line or
/// output), at most a few lines.
fn said(out: &std::process::Output) -> String {
    let text = format!("{}\n{}", String::from_utf8_lossy(&out.stderr), String::from_utf8_lossy(&out.stdout));
    crate::util::truncate(text.trim(), 600).to_owned()
}

/// A release repository that isn't there yet on its host, made: private and empty, in its owner's
/// organization, group or workspace (docs/deploy-api.md §7). One that is there is left as it is.
pub async fn ensure(d: &Deploy, rr: &RepoRef) -> Result<(), String> {
    let r = open(d, rr).await?;
    let e = |e: anyhow::Error| format!("{e:#}");
    if r.exists().await.map_err(e)? {
        return Ok(());
    }
    r.make("kiln's release repository: what each environment runs, one release a commit").await.map_err(e)?;
    d.hub.store.audit("kiln".into(), "repo.create".into(), format!("{} on {} (a release repository, docs/deploy-api.md §6)", rr.repo, r.host.label())).await;
    Ok(())
}

// ------------------------------------------------------------------ deployments

/// The host's name for a registration's environment: `<environment>/<app>`, so each app's
/// deployments stand apart (a host marks one inactive when the next to its environment succeeds).
pub fn environment_name(app: &str, env: &str) -> String {
    format!("{env}/{app}")
}

/// What the host hears of a release in `state` (GitHub's words, GitLab's and Bitbucket's from
/// them; `None`: nothing, the host retires one superseded or rolled back itself).
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

/// Reports a release that moved to `r.state` to its release repository's host, as a deployment of
/// its commit (Bitbucket's: a build status of it), where that host is GitHub, a GitLab or
/// Bitbucket: in the background, one report of a release at a time, never one that took it back
/// from where it settled; tried again for a few minutes while the host doesn't take it.
pub fn report(d: &Deploy, r: &Release) {
    let (Some(state), Some(_)) = (deployment_state(r.state), r.commit.as_ref()) else { return };
    let (deploy, id, environment, app, at) = (DeployRef { hub: Arc::clone(&d.hub), mirrors: Arc::clone(&d.remote) }, r.id, r.environment.clone(), r.app.clone(), r.state);
    crate::rt::spawn(async move {
        for attempt in 0..5u32 {
            match report_now(&deploy, id, &environment, &app, at, state).await {
                Ok(()) => return,
                Err(e) if attempt == 4 => tracing::warn!(release = id, error = %format!("{e:#}"), "deploy: the release repository's host didn't take a release's deployment status"),
                Err(e) => tracing::debug!(release = id, error = %format!("{e:#}"), "deploy: the release repository's host didn't take a release's deployment status yet"),
            }
            crate::rt::sleep(Duration::from_secs(30 * u64::from(attempt + 1))).await;
        }
    });
}

/// What a report in the background needs of the service.
struct DeployRef {
    hub: Arc<Hub>,
    mirrors: Arc<Mirrors>,
}

async fn report_now(d: &DeployRef, id: i64, environment: &str, app: &str, at: ReleaseState, state: &str) -> anyhow::Result<()> {
    let st = &d.hub.store;
    let Some(env) = super::store::environment(st, environment).await? else { return Ok(()) };
    let rr = env.release_repo.clone();
    let host = match rr.host.as_str() {
        GITHUB_HOST => Host::Github(crate::githost::github::app(&d.hub).await.ok_or_else(|| anyhow!("kiln has no GitHub App any more"))?),
        GITLAB_HOST => Host::Gitlab(gitlab_for(&d.hub, &rr.repo).await.map_err(anyhow::Error::msg)?),
        BITBUCKET_HOST => Host::Bitbucket(bitbucket_for(&d.hub, &rr.repo).await.map_err(anyhow::Error::msg)?),
        _ => return Ok(()),
    };
    let lock = Arc::clone(d.mirrors.reports.lock().entry(id).or_default());
    let _one = lock.lock().await;
    let Some(r) = super::store::release(st, id).await? else { return Ok(()) };
    let Some(commit) = r.commit.clone() else { return Ok(()) };
    // Its key from before GitLab: kept, so a release reported then isn't reported again.
    let key = format!("deploy/github/deployment/{id}");
    let last = st.kv_get(key.clone()).await?;
    // Settled once, it stays: a report that comes late never takes it back.
    if last.as_deref() == Some(state) || last.as_deref().is_some_and(settled) {
        return Ok(());
    }
    let repo = row_of(host.web(), &rr.repo, Some(&rr.branch));
    let base = d.hub.cfg.server.public_url.trim_end_matches('/');
    let page = format!("{base}/deployments/releases/{id}");
    let environment_name = environment_name(app, environment);
    let what = format!("release {id}: {} {}{}", r.kind.as_str(), r.version.as_deref().unwrap_or(""), r.message.as_deref().filter(|_| at != ReleaseState::Delivering).map(|m| format!(" · {m}")).unwrap_or_default());
    match &host {
        Host::Github(app_obj) => {
            let s = Deployed { state, description: what.trim(), environment_url: None, log_url: Some(&page) };
            app_obj.deployment_status(&repo, &commit, &environment_name, &s).await?;
        }
        // GitLab's deployments say no more than their status; one cancelled is `canceled`.
        Host::Gitlab(g) => g.deployment(&repo, &commit, &environment_name, if state == "inactive" { "cancelled" } else { state }, Some(&page)).await?,
        // Bitbucket's is a build status of the commit, `deploy/<environment>/<app>`.
        Host::Bitbucket(b) => {
            let s = crate::githost::CommitStatus { context: format!("deploy/{environment_name}"), state: state.to_owned(), description: what.trim().to_owned(), url: Some(page.clone()) };
            b.status(&d.hub, &repo, &commit, &s).await?;
        }
    }
    st.kv_set(key, state.to_owned()).await?;
    Ok(())
}

// ------------------------------------------------------------------ webhooks

/// A host's webhook, for release repositories: a push to a branch an environment releases from
/// has its mirror fetched now (and a scan run); a pull or merge request of a release (opened,
/// merged, closed) moves that release now. `full`: the repository's name on the host; `pushed`:
/// the branch a push moved; `proposal`: a pull or merge request's branch, and whether it's merged.
async fn on_event(d: &Deploy, kind: &str, full: &str, pushed: Option<&str>, proposal: Option<(&str, bool)>) {
    let Ok(envs) = super::store::environments(&d.hub.store).await else { return };
    let ours: Vec<&Environment> = envs.iter().filter(|e| e.release_repo.host == kind && e.release_repo.repo.eq_ignore_ascii_case(full)).collect();
    if ours.is_empty() {
        return;
    }
    // A merge moves the base branch too.
    let moved = pushed.is_some_and(|b| ours.iter().any(|e| e.release_repo.branch == b)) || proposal.is_some_and(|(_, merged)| merged);
    if moved && let Some(e) = ours.first() {
        if let Ok(r) = open(d, &e.release_repo).await
            && let Ok(dir) = r.dir()
        {
            d.remote.fetched.lock().remove(&dir);
        }
        d.remote.scan.store(true, std::sync::atomic::Ordering::Release);
    }
    if let Some((head, _)) = proposal
        && let Some(id) = head.strip_prefix(PROPOSAL_PREFIX).and_then(|rest| rest.split('-').next()).and_then(|n| n.parse::<i64>().ok())
        && let Ok(Some(r)) = super::store::release(&d.hub.store, id).await
        && proposal_branch(&r) == head
        && ours.iter().any(|e| e.name == r.environment)
    {
        let _ = super::store::schedule(&d.hub.store, id, crate::util::now_unix()).await;
    }
    d.wake.notify();
}

/// The GitHub App's `push` and `pull_request` events.
pub async fn on_github_event(d: &Deploy, event: &str, v: &Value) {
    let full = v.pointer("/repository/full_name").and_then(Value::as_str).unwrap_or_default();
    match event {
        "push" => on_event(d, GITHUB_HOST, full, v.get("ref").and_then(Value::as_str).and_then(|r| r.strip_prefix("refs/heads/")), None).await,
        "pull_request" => {
            let head = v.pointer("/pull_request/head/ref").and_then(Value::as_str).unwrap_or_default();
            on_event(d, GITHUB_HOST, full, None, Some((head, v.pointer("/pull_request/merged").and_then(Value::as_bool) == Some(true)))).await;
        }
        _ => {}
    }
}

/// Bitbucket's `repo:push` and `pullrequest:*` events (created, updated, fulfilled: merged,
/// rejected: declined).
pub async fn on_bitbucket_event(d: &Deploy, event: &str, v: &Value) {
    let full = v.pointer("/repository/full_name").and_then(Value::as_str).unwrap_or_default();
    match event {
        "repo:push" => {
            let changes = v.pointer("/push/changes").and_then(Value::as_array).cloned().unwrap_or_default();
            let branches = changes.iter().filter_map(|c| c.get("new").filter(|n| n.get("type").and_then(Value::as_str) == Some("branch")).and_then(|n| n.get("name")).and_then(Value::as_str));
            for branch in branches {
                on_event(d, BITBUCKET_HOST, full, Some(branch), None).await;
            }
        }
        "pullrequest:created" | "pullrequest:updated" | "pullrequest:fulfilled" | "pullrequest:rejected" => {
            let head = v.pointer("/pullrequest/source/branch/name").and_then(Value::as_str).unwrap_or_default();
            let merged = event == "pullrequest:fulfilled" || v.pointer("/pullrequest/state").and_then(Value::as_str) == Some("MERGED");
            on_event(d, BITBUCKET_HOST, full, None, Some((head, merged))).await;
        }
        _ => {}
    }
}

/// A GitLab's `Push Hook` and `Merge Request Hook` events.
pub async fn on_gitlab_event(d: &Deploy, event: &str, v: &Value) {
    let full = v.pointer("/project/path_with_namespace").and_then(Value::as_str).unwrap_or_default();
    match event {
        "Push Hook" => on_event(d, GITLAB_HOST, full, v.get("ref").and_then(Value::as_str).and_then(|r| r.strip_prefix("refs/heads/")), None).await,
        "Merge Request Hook" => {
            let head = v.pointer("/object_attributes/source_branch").and_then(Value::as_str).unwrap_or_default();
            let merged = v.pointer("/object_attributes/state").and_then(Value::as_str) == Some("merged") || v.pointer("/object_attributes/action").and_then(Value::as_str) == Some("merge");
            on_event(d, GITLAB_HOST, full, None, Some((head, merged))).await;
        }
        _ => {}
    }
}

// ------------------------------------------------------------------ Argo CD

/// How long Argo CD's token for a release repository on GitHub is used before kiln puts a new one
/// in its place (GitHub's last an hour), and how long a new one lasts at least.
const READER_SECS: i64 = 30 * 60;
const READER_LASTS: i64 = 45 * 60;

/// Argo CD reads a release repository on its host with credentials that read that one repository:
/// on GitHub an installation token, put in its place every half hour; on GitLab a deploy token of
/// the project, and on Bitbucket a deploy key (over SSH: its API makes no token that only reads),
/// each made again (the one before revoked) only when Argo CD can't read with it. Any is put in
/// place at once when Argo CD can't read.
pub async fn argo_reader(d: &Deploy, argo: &Argo, env: &Environment, url: &str) -> Result<(), String> {
    let key = format!("deploy/{}/argo-reader/{}/{}", env.release_repo.host, argo.integration, env.release_repo.repo);
    let now = crate::util::now_unix();
    let kept: Value = d.hub.store.kv_get(key.clone()).await.ok().flatten().map(|s| serde_json::from_str(&s).unwrap_or_else(|_| json!({ "at": s.parse::<i64>().unwrap_or(0) }))).unwrap_or(Value::Null);
    let put = kept.get("at").and_then(Value::as_i64);
    let r = open(d, &env.release_repo).await?;
    let due = match &r.host {
        Host::Github(_) => put.is_none_or(|at| now - at >= READER_SECS),
        Host::Gitlab(_) | Host::Bitbucket(_) => put.is_none(),
    };
    if !due {
        let reads = argo.repository(url).await?.is_some_and(|r| r.pointer("/connectionState/status").and_then(Value::as_str) != Some("Failed"));
        if reads {
            return Ok(());
        }
    }
    let e = |e: anyhow::Error| format!("{e:#}");
    let label = format!("kiln: Argo CD {}", argo.integration);
    let (reads, made) = match &r.host {
        Host::Github(app) => {
            let perms: BTreeMap<String, String> = [("contents".to_owned(), "read".to_owned()), ("metadata".to_owned(), "read".to_owned())].into();
            (Reads::Login("x-access-token".to_owned(), app.token_lasting(&r.name, &perms, READER_LASTS).await.map_err(e)?), None)
        }
        Host::Gitlab(g) => {
            let (id, user, token) = g.deploy_token(&r.name, &label).await.map_err(e)?;
            (Reads::Login(user, token), Some(id))
        }
        Host::Bitbucket(b) => {
            let (id, key) = b.deploy_key(&r.name, &label).await.map_err(e)?;
            (Reads::Key(key), Some(id))
        }
    };
    let name = argocd::release_repo_name(&env.release_repo.repo);
    let put = match &reads {
        Reads::Login(user, token) => argo.put_repository(url, &name, user, token).await,
        Reads::Key(key) => argo.put_repository_ssh(url, &name, key).await,
    };
    if let Err(why) = put {
        // The credential Argo CD didn't take goes, not to be left behind on the host.
        if let Some(id) = made {
            revoke_reader(&r, id).await;
        }
        let known = if matches!(reads, Reads::Key(_)) { " (Argo CD checks the server's SSH host key against the ones it knows: argocd-ssh-known-hosts-cm)" } else { "" };
        return Err(format!("Argo CD can't read the release repository at {url}: {why}{known}"));
    }
    // The deploy token or key it replaces, revoked (never the one just made, should the host
    // give it the same id).
    if let Some(old) = kept.get("token_id").and_then(Value::as_u64)
        && Some(old) != made
    {
        revoke_reader(&r, old).await;
    }
    d.hub.store.kv_set(key, json!({ "at": now, "token_id": made }).to_string()).await.map_err(e)?;
    Ok(())
}

/// What Argo CD reads a release repository with.
enum Reads {
    /// A user and a token, over HTTPS.
    Login(String, String),
    /// An SSH private key.
    Key(String),
}

/// A deploy token (GitLab) or deploy key (Bitbucket) of Argo CD's, revoked: one that's been
/// replaced, or one Argo CD didn't take.
async fn revoke_reader(r: &Remote, id: u64) {
    let gone = match &r.host {
        Host::Github(_) => Ok(()),
        Host::Gitlab(g) => g.revoke_deploy_token(&r.name, id).await,
        Host::Bitbucket(b) => b.revoke_deploy_key(&r.name, id).await,
    };
    if let Err(e) = gone {
        tracing::warn!(repo = %r.name, host = r.host.label(), error = %format!("{e:#}"), "deploy: a credential of Argo CD's stays on the release repository's host");
    }
}

/// Every Argo CD environment's credentials for its release repository on another host, put again
/// where they're due (the worker, every few minutes).
pub async fn refresh_readers(d: &Deploy) {
    let Ok(envs) = super::store::environments(&d.hub.store).await else { return };
    for env in envs.iter().filter(|e| super::model::remote_host(&e.release_repo.host)) {
        let Some(integration) = super::appset::integration_of(env) else { continue };
        let done = match argocd::Argo::of(&d.hub, integration).await {
            Ok(argo) => {
                let url = super::appset::release_url(d, &argo, env).await;
                argo_reader(d, &argo, env, &url).await
            }
            Err(e) => Err(e),
        };
        if let Err(why) = done {
            tracing::warn!(environment = %env.name, error = %why, "deploy: Argo CD's credentials for a release repository weren't put in place");
        }
    }
}

/// Where Argo CD reads a release repository on another host ([`Remote::reader_url`]).
pub async fn release_url(d: &Deploy, rr: &RepoRef) -> Option<String> {
    open(d, rr).await.ok().map(|r| r.reader_url())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_states() {
        assert!(name_ok(GITHUB_HOST, "acme/releases").is_ok());
        for bad in ["releases", "acme/", "/r", "a/b/c", "acme/r.git", "../x", "a b/c"] {
            assert!(name_ok(GITHUB_HOST, bad).is_err(), "{bad}");
        }
        assert!(name_ok(GITLAB_HOST, "acme/platform/releases").is_ok(), "a subgroup's project");
        assert!(name_ok(GITLAB_HOST, "acme/releases").is_ok());
        for bad in ["releases", "acme/../x", "acme//r", "acme/r.git"] {
            assert!(name_ok(GITLAB_HOST, bad).is_err(), "{bad}");
        }
        assert!(name_ok(BITBUCKET_HOST, "acme/releases").is_ok());
        for bad in ["releases", "acme/platform/releases", "acme/r.git"] {
            assert!(name_ok(BITBUCKET_HOST, bad).is_err(), "{bad}");
        }
        assert_eq!(host_label(BITBUCKET_HOST), "Bitbucket");
        assert_eq!(change_name(BITBUCKET_HOST, 3), "pull request #3");
        assert_eq!(environment_name("web", "production"), "production/web");
        assert_eq!(deployment_state(ReleaseState::Delivering), Some("in_progress"));
        assert_eq!(deployment_state(ReleaseState::Superseded), None);
        assert!(settled("failure") && !settled("in_progress"));
    }
}
