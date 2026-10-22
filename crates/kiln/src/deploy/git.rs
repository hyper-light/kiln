//! An environment's release repository, wherever it is (docs/deploy-api.md §7): one of kiln's own
//! ([`super::host`]), or one on GitHub through kiln's GitHub App ([`super::github`]). What a
//! release reads and writes there, the same either way.

use std::time::Duration;

use super::Deploy;
use super::github::{self, Github};
use super::host::{Author, Change, CommitError, Committed, Hosted, LoggedCommit};
use super::model::{GITHUB_HOST, KILN_HOST, RepoRef};

/// A release repository and the branch releases go to.
pub enum Repo {
    Kiln { hosted: Hosted, repo: String, branch: String },
    Github(Box<Github>),
}

/// The release repository `rr` names.
pub async fn open(d: &Deploy, rr: &RepoRef) -> Result<Repo, String> {
    match rr.host.as_str() {
        KILN_HOST => Ok(Repo::Kiln { hosted: d.hosted.clone(), repo: rr.repo.clone(), branch: rr.branch.clone() }),
        GITHUB_HOST => Ok(Repo::Github(Box::new(github::open(d, rr).await?))),
        other => Err(format!("{other}: a release repository is on kiln's own git server (`kiln`) or on GitHub (`github`)")),
    }
}

impl Repo {
    /// The branch's head now (`None`: it has no commit yet).
    pub async fn head(&self) -> anyhow::Result<Option<String>> {
        match self {
            Repo::Kiln { hosted, repo, branch } => {
                let (h, r, b) = (hosted.clone(), repo.clone(), branch.clone());
                crate::rt::blocking(move || h.head(&r, &b)).await?
            }
            Repo::Github(g) => g.head(Duration::from_secs(2)).await,
        }
    }

    /// A file at a commit, or a revision of one (`<sha>^`). `None`: it has no such file.
    pub async fn read(&self, rev: &str, path: &str) -> anyhow::Result<Option<Vec<u8>>> {
        match self {
            Repo::Kiln { hosted, repo, .. } => {
                let (h, r, rev, path) = (hosted.clone(), repo.clone(), rev.to_owned(), path.to_owned());
                crate::rt::blocking(move || h.read(&r, &rev, &path)).await?
            }
            Repo::Github(g) => g.read(rev, path).await,
        }
    }

    /// Commits `changes` on the branch, made on `expected`, its head as the caller read it (`None`:
    /// the branch has no commit yet). `Moved` when the branch isn't there any more.
    pub async fn commit(&self, expected: Option<&str>, changes: Vec<Change>, message: String, author: Author) -> Result<Committed, CommitError> {
        match self {
            Repo::Kiln { hosted, repo, branch } => {
                let (h, r, b, e) = (hosted.clone(), repo.clone(), branch.clone(), expected.map(str::to_owned));
                crate::rt::blocking(move || h.commit(&r, &b, e.as_deref(), &changes, &message, &author)).await.map_err(CommitError::Other)?
            }
            Repo::Github(g) => g.commit_to(&g.branch, expected, expected, changes, message, author).await,
        }
    }

    /// The newest commit on the branch whose message has the line `<key>: <value>`.
    pub async fn find_trailer(&self, key: &str, value: &str) -> anyhow::Result<Option<String>> {
        match self {
            Repo::Kiln { hosted, repo, branch } => {
                let (h, r, b, k, v) = (hosted.clone(), repo.clone(), branch.clone(), key.to_owned(), value.to_owned());
                crate::rt::blocking(move || h.find_trailer(&r, &b, &k, &v)).await?
            }
            Repo::Github(g) => g.find_trailer(key, value).await,
        }
    }

    /// The branch's head and its commits after `since` (oldest first; none without `since`), for a
    /// scan: GitHub's looked at again once a minute, or when a webhook says it changed.
    pub async fn scan(&self, since: Option<&str>) -> anyhow::Result<(Option<String>, Vec<LoggedCommit>)> {
        match self {
            Repo::Kiln { hosted, repo, branch } => {
                let (h, r, b, since) = (hosted.clone(), repo.clone(), branch.clone(), since.map(str::to_owned));
                crate::rt::blocking(move || -> anyhow::Result<(Option<String>, Vec<LoggedCommit>)> {
                    let head = h.head(&r, &b)?;
                    let commits = match &since {
                        Some(s) => h.commits_since(&r, &b, Some(s))?,
                        None => Vec::new(),
                    };
                    Ok((head, commits))
                })
                .await?
            }
            Repo::Github(g) => g.commits_since(since, github::POLL).await,
        }
    }

    /// Where KilnCD reads it on this machine: kiln's repository, or kiln's mirror of GitHub's.
    pub fn url(&self) -> anyhow::Result<String> {
        match self {
            Repo::Kiln { hosted, repo, .. } => hosted.url(repo),
            Repo::Github(g) => g.mirror_url(),
        }
    }

    /// The repository on GitHub, where it is one.
    pub fn github(&self) -> Option<&Github> {
        match self {
            Repo::Github(g) => Some(g),
            Repo::Kiln { .. } => None,
        }
    }
}
