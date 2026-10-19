//! Git hosts (docs/deploy-api.md §7, docs/ci.md W23-P5): kiln's own git server, GitHub, GitLab
//! and Bitbucket behind one interface, for CD's release repositories and for CI's providers:
//! read a repository (a mirror URL and credentials), commit files against an expected head, push
//! a commit made locally, open a change (a pull or merge request; kiln's: a change request) and
//! follow it, report a commit or deployment status, read a push webhook, and create a repository
//! from a template.

pub mod github;
pub mod web;

use std::sync::Arc;

use serde_json::Value;

use crate::hub::Hub;
use crate::store::Repo;

/// The id git gives "no commit": a ref update against it only creates the ref.
pub const NO_COMMIT: &str = "0000000000000000000000000000000000000000";

/// One file a commit writes (`Some`) or removes (`None`).
#[derive(Debug, Clone)]
pub struct Change {
    pub path: String,
    pub content: Option<Vec<u8>>,
}

/// Who a commit is by.
#[derive(Debug, Clone)]
pub struct Author {
    pub name: String,
    pub email: String,
}

impl Default for Author {
    fn default() -> Self {
        Self { name: "kiln".to_owned(), email: "kiln@localhost".to_owned() }
    }
}

/// What a commit did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Committed {
    /// A new commit, now the branch's head.
    New(String),
    /// Nothing to change: the branch already holds every file as asked; its head.
    Unchanged(String),
}

impl Committed {
    pub fn sha(&self) -> &str {
        match self {
            Self::New(s) | Self::Unchanged(s) => s,
        }
    }
}

/// Why a commit didn't land.
#[derive(Debug)]
pub enum CommitError {
    /// The branch moved from the head the commit was made on.
    Moved,
    Other(anyhow::Error),
}

impl std::fmt::Display for CommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Moved => write!(f, "the branch moved while committing"),
            Self::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl From<anyhow::Error> for CommitError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(e)
    }
}

/// How to read a repository: a URL git fetches, and the header it needs (none: open, or a path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reader {
    pub url: String,
    pub header: Option<(String, String)>,
}

/// A change opened on a host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRef {
    pub number: u64,
    pub url: String,
}

/// Where a change is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeState {
    Open,
    Merged { commit: Option<String> },
    Closed,
}

/// A commit status: GitHub's and Bitbucket's states (`pending`, `success`, `failure`, `error`),
/// GitLab's from them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitStatus {
    pub context: String,
    pub state: String,
    pub description: String,
    pub url: Option<String>,
}

/// A push a webhook says happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Push {
    /// `owner/name` on the host.
    pub repo: String,
    pub refname: String,
    pub before: String,
    pub after: String,
}

/// A git host.
#[derive(Clone)]
pub enum GitHost {
    /// kiln's own git server ([`crate::ci::hosting`]).
    Kiln,
    Github(Arc<github::App>),
}

/// The host of `repo`: kiln for repositories it hosts, GitHub for one on the App's GitHub.
pub async fn for_repo(hub: &Hub, repo: &Repo) -> Option<GitHost> {
    if crate::ci::hosting::native_path(hub, repo).is_some() {
        return Some(GitHost::Kiln);
    }
    let app = github::app(hub).await?;
    app.name_of(repo).map(|_| GitHost::Github(app))
}

/// `host[:port]` and the path of a clone URL (`https://h/o/r.git`, `git@h:o/r.git`,
/// `ssh://git@h:22/o/r`, `git://h:9418/o/r.git`).
pub fn split_url(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    let (host, path) = match url.split_once("://") {
        Some((_, rest)) => {
            let (auth_and_host, path) = rest.split_once('/')?;
            (auth_and_host.rsplit_once('@').map_or(auth_and_host, |(_, h)| h), path)
        }
        None => {
            let (who, path) = url.split_once(':')?;
            (who.rsplit_once('@').map_or(who, |(_, h)| h), path)
        }
    };
    let path = path.trim_matches('/').trim_end_matches(".git").to_owned();
    (!host.is_empty() && !path.is_empty()).then(|| (host.to_ascii_lowercase(), path))
}

/// The host part (`host[:port]`) of a web origin.
pub fn host_of(origin: &str) -> String {
    let rest = origin.split_once("://").map_or(origin, |(_, r)| r);
    rest.split('/').next().unwrap_or(rest).to_ascii_lowercase()
}

impl GitHost {
    /// How to read `repo`.
    pub async fn reader(&self, hub: &Hub, repo: &Repo) -> anyhow::Result<Reader> {
        match self {
            GitHost::Kiln => {
                let path = crate::ci::hosting::native_path(hub, repo).ok_or_else(|| anyhow::anyhow!("{} isn't hosted here", repo.name))?;
                Ok(Reader { url: crate::ci::hosting::clone_url(&path), header: None })
            }
            GitHost::Github(app) => app.reader(repo).await,
        }
    }

    /// Commits `files` onto `branch`, made on `expected` (`None`: the branch is new). `Moved` when
    /// the branch isn't at `expected` any more; `Unchanged` when it already holds them.
    #[allow(clippy::too_many_arguments)]
    pub async fn commit(&self, hub: &Hub, repo: &Repo, branch: &str, expected: Option<&str>, files: &[Change], message: &str, author: &Author) -> Result<Committed, CommitError> {
        match self {
            GitHost::Kiln => kiln_commit(hub, repo, branch, expected, files, message, author).await,
            GitHost::Github(app) => app.commit(repo, branch, expected, files, message, author).await,
        }
    }

    /// Pushes `sha` (made in the repository at `local`) to `branch`, if it's still at `expected`.
    pub async fn push(&self, hub: &Hub, repo: &Repo, local: &str, sha: &str, branch: &str, expected: Option<&str>) -> Result<(), CommitError> {
        let target = match self {
            GitHost::Kiln => {
                let path = crate::ci::hosting::native_path(hub, repo).ok_or_else(|| anyhow::anyhow!("{} isn't hosted here", repo.name))?;
                // In process: the objects, then the branch by compare-and-swap.
                let p = path.to_string_lossy().into_owned();
                crate::gitops::run_git(hub, &["-C", local, "push", "--quiet", &p, &format!("{sha}:refs/kiln/incoming/{sha}")], crate::gitops::LS_REMOTE_TIMEOUT).await?;
                let moved = crate::gitops::run_git(hub, &["-C", &p, "update-ref", &format!("refs/heads/{branch}"), sha, expected.unwrap_or(NO_COMMIT)], crate::gitops::LS_REMOTE_TIMEOUT).await;
                let _ = crate::gitops::run_git(hub, &["-C", &p, "update-ref", "-d", &format!("refs/kiln/incoming/{sha}")], crate::gitops::LS_REMOTE_TIMEOUT).await;
                return moved.map(|_| ()).map_err(|_| CommitError::Moved);
            }
            GitHost::Github(app) => app.push_url(repo).await?,
        };
        let lease = format!("--force-with-lease=refs/heads/{branch}:{}", expected.unwrap_or(""));
        match crate::gitops::run_git(hub, &["-C", local, "push", "--quiet", &lease, &target, &format!("{sha}:refs/heads/{branch}")], std::time::Duration::from_secs(600)).await {
            Ok(_) => Ok(()),
            Err(e) if format!("{e:#}").contains("stale info") || format!("{e:#}").contains("rejected") => Err(CommitError::Moved),
            Err(e) => Err(CommitError::Other(e)),
        }
    }

    /// Opens a change from `head` into `base`.
    pub async fn open_change(&self, hub: &Arc<Hub>, repo: &Repo, head: &str, base: &str, title: &str, body: &str) -> anyhow::Result<ChangeRef> {
        match self {
            GitHost::Kiln => {
                let path = crate::ci::hosting::native_path(hub, repo).ok_or_else(|| anyhow::anyhow!("{} isn't hosted here", repo.name))?;
                let p = path.to_string_lossy().into_owned();
                let sha = crate::gitops::run_git(hub, &["-C", &p, "rev-parse", "--verify", &format!("refs/heads/{head}^{{commit}}")], crate::gitops::LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned())?;
                let by = crate::ci::cr::By { pushed_by: "kiln".into(), author: ("kiln".into(), "agent") };
                let options = kiln_ci::review::Options { title: Some(title.to_owned()), message: (!body.is_empty()).then(|| body.to_owned()), ..kiln_ci::review::Options::default() };
                let o = crate::ci::cr::submit(hub, repo, &p, base, head, &sha, &by, &options).await.map_err(anyhow::Error::msg)?;
                Ok(ChangeRef { number: u64::from(o.number), url: o.url })
            }
            GitHost::Github(app) => app.open_change(repo, head, base, title, body).await,
        }
    }

    /// Where change `number` is.
    pub async fn change(&self, hub: &Hub, repo: &Repo, number: u64) -> anyhow::Result<ChangeState> {
        match self {
            GitHost::Kiln => {
                let d = crate::ci::diffs::find(hub, &format!("CR{number}")).await?.filter(|d| d.repo == repo.name).ok_or_else(|| anyhow::anyhow!("no change request {number}"))?;
                let id = d.id.clone();
                let landed: Option<String> = hub.store.call(move |c| Ok(c.query_row("SELECT landed_commit FROM diffs WHERE id = ?1", [&id], |r| r.get(0))?)).await?;
                Ok(match d.state.as_str() {
                    "landed" => ChangeState::Merged { commit: landed },
                    "abandoned" => ChangeState::Closed,
                    _ => ChangeState::Open,
                })
            }
            GitHost::Github(app) => app.change(repo, number).await,
        }
    }

    /// Reports a status on `sha`.
    pub async fn status(&self, hub: &Hub, repo: &Repo, sha: &str, status: &CommitStatus) -> anyhow::Result<()> {
        match self {
            // kiln shows its own runs and releases.
            GitHost::Kiln => {
                let _ = (hub, repo, sha, status);
                Ok(())
            }
            GitHost::Github(app) => app.status(repo, sha, status).await,
        }
    }

    /// Records a deployment of `sha` to `environment` in `state` (`queued`, `in_progress`,
    /// `success`, `failure`, `error`, `inactive`).
    pub async fn deployment(&self, hub: &Hub, repo: &Repo, sha: &str, environment: &str, state: &str, url: Option<&str>) -> anyhow::Result<()> {
        match self {
            GitHost::Kiln => {
                let _ = (hub, repo, sha, environment, state, url);
                Ok(())
            }
            GitHost::Github(app) => app.deployment(repo, sha, environment, state, url).await,
        }
    }

    /// The push a webhook delivery is, if it's one from this host.
    pub fn push_event(&self, headers: &http::HeaderMap, body: &[u8]) -> Option<Push> {
        match self {
            GitHost::Kiln => None,
            GitHost::Github(_) => github::push_event(headers, body),
        }
    }

    /// Makes `name` (`owner/name`) from the repository `template`.
    pub async fn create_from_template(&self, hub: &Hub, template: &str, name: &str, private: bool) -> anyhow::Result<()> {
        match self {
            GitHost::Kiln => {
                let _ = private;
                let repos = hub.store.repos().await?;
                let from = repos.iter().find(|r| r.name == template).and_then(|r| crate::ci::hosting::native_path(hub, r)).ok_or_else(|| anyhow::anyhow!("no template {template} here"))?;
                crate::ci::hosting::create_from(hub, name, &from).await.map(|_| ())
            }
            GitHost::Github(app) => app.create_from_template(template, name, private).await,
        }
    }
}

/// A commit in kiln's own repository: the files written in a scratch index on `expected`'s tree,
/// the branch moved by compare-and-swap.
async fn kiln_commit(hub: &Hub, repo: &Repo, branch: &str, expected: Option<&str>, files: &[Change], message: &str, author: &Author) -> Result<Committed, CommitError> {
    let path = crate::ci::hosting::native_path(hub, repo).ok_or_else(|| anyhow::anyhow!("{} isn't hosted here", repo.name))?;
    let p = path.to_string_lossy().into_owned();
    let run = |args: Vec<String>, env: Vec<(String, String)>, input: Vec<u8>| {
        let p = p.clone();
        async move {
            let mut a: Vec<&str> = vec!["-C", &p];
            a.extend(args.iter().map(String::as_str));
            let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            crate::ci::diffs::git_run(hub, &a, &env, input).await
        }
    };
    let index = std::env::temp_dir().join(format!("kiln-index-{}", crate::util::new_id()));
    let idx = index.to_string_lossy().into_owned();
    let ienv = vec![("GIT_INDEX_FILE".to_owned(), idx.clone())];
    let result = async {
        let base_tree = match expected {
            Some(e) => {
                let r = run(vec!["rev-parse".into(), format!("{e}^{{tree}}")], vec![], vec![]).await?;
                anyhow::ensure!(r.ok, "{e} isn't a commit here");
                let t = String::from_utf8_lossy(&r.out).trim().to_owned();
                let r = run(vec!["read-tree".into(), t.clone()], ienv.clone(), vec![]).await?;
                anyhow::ensure!(r.ok, "the base tree: {}", String::from_utf8_lossy(&r.err).trim());
                Some(t)
            }
            None => {
                let r = run(vec!["read-tree".into(), "--empty".into()], ienv.clone(), vec![]).await?;
                anyhow::ensure!(r.ok, "an empty index: {}", String::from_utf8_lossy(&r.err).trim());
                None
            }
        };
        for f in files {
            match &f.content {
                Some(bytes) => {
                    let blob = crate::ci::diffs::git_with_input(hub, &["-C", &p, "hash-object", "-w", "--stdin"], bytes.clone()).await?;
                    let info = format!("100644,{},{}", String::from_utf8_lossy(&blob).trim(), f.path);
                    let r = run(vec!["update-index".into(), "--add".into(), "--cacheinfo".into(), info], ienv.clone(), vec![]).await?;
                    anyhow::ensure!(r.ok, "{}: {}", f.path, String::from_utf8_lossy(&r.err).trim());
                }
                None => {
                    let r = run(vec!["update-index".into(), "--force-remove".into(), "--".into(), f.path.clone()], ienv.clone(), vec![]).await?;
                    anyhow::ensure!(r.ok, "{}: {}", f.path, String::from_utf8_lossy(&r.err).trim());
                }
            }
        }
        let r = run(vec!["write-tree".into()], ienv.clone(), vec![]).await?;
        anyhow::ensure!(r.ok, "the tree: {}", String::from_utf8_lossy(&r.err).trim());
        anyhow::Ok((base_tree, String::from_utf8_lossy(&r.out).trim().to_owned()))
    }
    .await;
    let _ = std::fs::remove_file(&index);
    let (base_tree, tree) = result?;
    if let (Some(e), Some(b)) = (expected, base_tree)
        && b == tree
    {
        return Ok(Committed::Unchanged(e.to_owned()));
    }
    let mut args = vec!["commit-tree".to_owned(), tree];
    if let Some(e) = expected {
        args.extend(["-p".to_owned(), e.to_owned()]);
    }
    args.extend(["-F".into(), "-".into()]);
    let env = vec![
        ("GIT_AUTHOR_NAME".to_owned(), author.name.clone()),
        ("GIT_AUTHOR_EMAIL".to_owned(), author.email.clone()),
        ("GIT_COMMITTER_NAME".to_owned(), author.name.clone()),
        ("GIT_COMMITTER_EMAIL".to_owned(), author.email.clone()),
    ];
    let r = run(args, env, message.as_bytes().to_vec()).await?;
    if !r.ok {
        return Err(CommitError::Other(anyhow::anyhow!("git commit-tree: {}", String::from_utf8_lossy(&r.err).trim())));
    }
    let sha = String::from_utf8_lossy(&r.out).trim().to_owned();
    let r = run(vec!["update-ref".into(), format!("refs/heads/{branch}"), sha.clone(), expected.unwrap_or(NO_COMMIT).to_owned()], vec![], vec![]).await?;
    if !r.ok {
        return Err(CommitError::Moved);
    }
    Ok(Committed::New(sha))
}

/// A webhook body's JSON, when it is JSON.
pub(crate) fn json_body(body: &[u8]) -> Option<Value> {
    serde_json::from_slice(body).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w23_urls() {
        let s = |u: &str| split_url(u);
        assert_eq!(s("https://github.com/acme/app.git"), Some(("github.com".into(), "acme/app".into())));
        assert_eq!(s("https://x:tok@github.com/acme/app"), Some(("github.com".into(), "acme/app".into())));
        assert_eq!(s("git@github.com:acme/app.git"), Some(("github.com".into(), "acme/app".into())));
        assert_eq!(s("ssh://git@ghe.corp:2222/acme/app"), Some(("ghe.corp:2222".into(), "acme/app".into())));
        assert_eq!(s("git://127.0.0.1:9418/acme/app.git"), Some(("127.0.0.1:9418".into(), "acme/app".into())));
        assert_eq!(s("nope"), None);
        assert_eq!(host_of("https://GitHub.com/"), "github.com");
    }
}
