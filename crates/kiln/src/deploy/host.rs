//! kiln's own git server (docs/deploy-api.md §7): bare repositories under the data directory,
//! written in process. A commit is made with git's plumbing against a scratch index, never a
//! working tree, and lands with a compare-and-swap of the branch: a push that came in between
//! makes it fail, never lost. Every call blocks; callers run them on the blocking pool.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, bail};

/// The id git gives "no commit": a ref update against it only creates the ref.
const NO_COMMIT: &str = "0000000000000000000000000000000000000000";

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

/// The repositories kiln hosts: its native repositories (docs/ci.md W14), whose bare repositories
/// live under one root as `<owner>/<name>.git`.
#[derive(Debug, Clone)]
pub struct Hosted {
    root: PathBuf,
}

impl Hosted {
    /// The repositories under `root` (`crate::ci::hosting::root`).
    pub fn new(root: &Path) -> Self {
        Self { root: root.to_owned() }
    }

    /// A repository's directory. A name is `owner/name`, each of letters, digits, `-`, `_` and
    /// `.`, never `.` or `..`: a name can't reach outside the hosted root.
    pub fn path(&self, repo: &str) -> anyhow::Result<PathBuf> {
        let repo = repo.trim_end_matches(".git");
        let seg = |s: &str| !s.is_empty() && s != "." && s != ".." && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        match repo.split_once('/') {
            Some((owner, name)) if seg(owner) && seg(name) => Ok(self.root.join(owner).join(format!("{name}.git"))),
            _ => bail!("{repo:?} isn't a repository name kiln hosts: `owner/name`, of letters, digits, '-', '_' and '.'"),
        }
    }

    /// Moves the repositories of an older root (`<data>/deploy/repos`, before release repositories
    /// were native ones) into this one, where this one has none by their name. What moved.
    pub fn adopt(&self, old: &Path) -> Vec<String> {
        let mut moved = Vec::new();
        let Ok(owners) = std::fs::read_dir(old) else { return moved };
        for owner in owners.flatten().filter(|e| e.path().is_dir()) {
            let Ok(repos) = std::fs::read_dir(owner.path()) else { continue };
            for r in repos.flatten() {
                let (from, name) = (r.path(), r.file_name().to_string_lossy().into_owned());
                let to = self.root.join(owner.file_name()).join(&name);
                if !name.ends_with(".git") || !from.join("HEAD").is_file() || to.exists() {
                    continue;
                }
                let renamed = std::fs::create_dir_all(self.root.join(owner.file_name())).and_then(|()| std::fs::rename(&from, &to));
                match renamed {
                    Ok(()) => moved.push(format!("{}/{}", owner.file_name().to_string_lossy(), name.trim_end_matches(".git"))),
                    Err(e) => tracing::warn!(from = %from.display(), to = %to.display(), error = %e, "deploy: a release repository couldn't move into kiln's hosted root"),
                }
            }
        }
        moved
    }

    /// The URL git reads it at on this machine (KilnCD's mirrors clone from it).
    pub fn url(&self, repo: &str) -> anyhow::Result<String> {
        Ok(format!("file://{}", self.path(repo)?.display()))
    }

    /// Makes the repository if it doesn't exist yet; its directory.
    pub fn ensure(&self, repo: &str, branch: &str) -> anyhow::Result<PathBuf> {
        let dir = self.path(repo)?;
        if dir.join("HEAD").exists() {
            return Ok(dir);
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        git(&dir, &["init", "--quiet", "--bare", &format!("--initial-branch={branch}")], None, &[])?;
        Ok(dir)
    }

    /// The branch's head, if it has one.
    pub fn head(&self, repo: &str, branch: &str) -> anyhow::Result<Option<String>> {
        let dir = self.path(repo)?;
        if !dir.join("HEAD").exists() {
            return Ok(None);
        }
        let out = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}^{{commit}}")])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .context("run git")?;
        Ok(out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned()).filter(|s| !s.is_empty()))
    }

    /// A file at a commit (`None`: the commit has no such file).
    pub fn read(&self, repo: &str, commit: &str, path: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let dir = self.path(repo)?;
        let spec = format!("{commit}:{path}");
        let exists = Command::new("git").arg("-C").arg(&dir).args(["cat-file", "-e", &spec]).env("GIT_TERMINAL_PROMPT", "0").status().context("run git")?;
        if !exists.success() {
            return Ok(None);
        }
        Ok(Some(git(&dir, &["cat-file", "blob", &spec], None, &[])?))
    }

    /// Commits `changes` on `branch`, made on `expected` (its head as the caller read it; `None`:
    /// the branch must not exist yet). The message's trailers follow a blank line.
    pub fn commit(&self, repo: &str, branch: &str, expected: Option<&str>, changes: &[Change], message: &str, author: &Author) -> Result<Committed, CommitError> {
        self.commit_inner(repo, branch, expected, changes, message, author).map_err(|e| match e.downcast::<Moved>() {
            Ok(_) => CommitError::Moved,
            Err(e) => CommitError::Other(e),
        })
    }

    fn commit_inner(&self, repo: &str, branch: &str, expected: Option<&str>, changes: &[Change], message: &str, author: &Author) -> anyhow::Result<Committed> {
        let dir = self.ensure(repo, branch)?;
        if self.head(repo, branch)?.as_deref() != expected {
            return Err(Moved.into());
        }
        // A scratch index of its own (commits from several callers never share one), and an empty
        // work tree: git updates an index only beside one, and `--cacheinfo` never reads it.
        let scratch = crate::util::new_id();
        let index = dir.join(format!("kiln-index-{scratch}"));
        let work = dir.join(format!("kiln-work-{scratch}"));
        std::fs::create_dir_all(&work).with_context(|| format!("create {}", work.display()))?;
        let env = [("GIT_INDEX_FILE", index.as_os_str()), ("GIT_WORK_TREE", work.as_os_str())];
        let result = (|| -> anyhow::Result<Committed> {
            match expected {
                Some(parent) => git(&dir, &["read-tree", parent], None, &env)?,
                None => git(&dir, &["read-tree", "--empty"], None, &env)?,
            };
            for ch in changes {
                match &ch.content {
                    Some(bytes) => {
                        let blob = text(git(&dir, &["hash-object", "-w", "--stdin"], Some(bytes), &env)?);
                        git(&dir, &["update-index", "--add", "--cacheinfo", &format!("100644,{blob},{}", ch.path)], None, &env)?;
                    }
                    None => {
                        git(&dir, &["update-index", "--force-remove", "--", &ch.path], None, &env)?;
                    }
                }
            }
            let tree = text(git(&dir, &["write-tree"], None, &env)?);
            if let Some(parent) = expected
                && text(git(&dir, &["rev-parse", &format!("{parent}^{{tree}}")], None, &[])?) == tree
            {
                return Ok(Committed::Unchanged(parent.to_owned()));
            }
            let mut args = vec!["commit-tree".to_owned(), tree];
            if let Some(parent) = expected {
                args.extend(["-p".to_owned(), parent.to_owned()]);
            }
            let ident = [
                ("GIT_AUTHOR_NAME", author.name.as_str()),
                ("GIT_AUTHOR_EMAIL", author.email.as_str()),
                ("GIT_COMMITTER_NAME", "kiln"),
                ("GIT_COMMITTER_EMAIL", "kiln@localhost"),
            ];
            let ident: Vec<(&str, &std::ffi::OsStr)> = ident.iter().map(|(k, v)| (*k, std::ffi::OsStr::new(*v))).collect();
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let commit = text(git(&dir, &args, Some(message.as_bytes()), &ident)?);
            // The compare-and-swap: only if the branch still is where the commit was made.
            let swap = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["update-ref", &format!("refs/heads/{branch}"), &commit, expected.unwrap_or(NO_COMMIT)])
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()
                .context("run git")?;
            if !swap.status.success() {
                return Err(Moved.into());
            }
            Ok(Committed::New(commit))
        })();
        let _ = std::fs::remove_file(&index);
        let _ = std::fs::remove_dir_all(&work);
        result
    }

    /// The newest commit on `branch` whose message has the line `<key>: <value>`.
    pub fn find_trailer(&self, repo: &str, branch: &str, key: &str, value: &str) -> anyhow::Result<Option<String>> {
        let dir = self.path(repo)?;
        if self.head(repo, branch)?.is_none() {
            return Ok(None);
        }
        let grep = format!("--grep=^{key}: {}$", regex_escape(value));
        let out = text(git(&dir, &["log", "-n", "1", "--format=%H", &grep, &format!("refs/heads/{branch}")], None, &[])?);
        Ok(Some(out).filter(|s| !s.is_empty()))
    }

    /// The commits on `branch` after `since` (oldest first; all of them without `since`), each with
    /// the paths it changed and its message.
    pub fn commits_since(&self, repo: &str, branch: &str, since: Option<&str>) -> anyhow::Result<Vec<LoggedCommit>> {
        let dir = self.path(repo)?;
        let Some(head) = self.head(repo, branch)? else { return Ok(Vec::new()) };
        let range = match since {
            Some(s) if s != head => format!("{s}..{head}"),
            Some(_) => return Ok(Vec::new()),
            None => head,
        };
        // Each commit: a record separator, its sha, its message, a unit separator, then its paths.
        let out = text(git(&dir, &["log", "--reverse", "--format=%x1e%H%n%B%x1f", "--name-only", &range], None, &[])?);
        Ok(out
            .split('\u{1e}')
            .filter(|c| !c.trim().is_empty())
            .filter_map(|c| {
                let (head, paths) = c.split_once('\u{1f}')?;
                let (sha, message) = head.split_once('\n').unwrap_or((head, ""));
                Some(LoggedCommit {
                    sha: sha.trim().to_owned(),
                    message: message.trim().to_owned(),
                    paths: paths.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect(),
                })
            })
            .collect())
    }
}

/// A commit, as [`Hosted::commits_since`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggedCommit {
    pub sha: String,
    pub message: String,
    pub paths: Vec<String>,
}

impl LoggedCommit {
    /// The value of a trailer (`Kiln-Release`) in its message.
    pub fn trailer(&self, key: &str) -> Option<&str> {
        self.message.lines().rev().find_map(|l| l.strip_prefix(key)?.strip_prefix(':').map(str::trim))
    }
}

/// The branch moved while committing.
#[derive(Debug)]
struct Moved;

impl std::fmt::Display for Moved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the branch moved")
    }
}

impl std::error::Error for Moved {}

/// `s` as a basic regular expression that matches only itself.
fn regex_escape(s: &str) -> String {
    s.chars().flat_map(|c| if "\\.[]*^$".contains(c) { vec!['\\', c] } else { vec![c] }).collect()
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).trim().to_owned()
}

/// Runs git in `dir`, feeding `stdin` when given; its standard output, or why it failed.
fn git(dir: &Path, args: &[&str], stdin: Option<&[u8]>, env: &[(&str, &std::ffi::OsStr)]) -> anyhow::Result<Vec<u8>> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args).env("GIT_TERMINAL_PROMPT", "0").stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = cmd.spawn().context("run git")?;
    if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(bytes).context("write to git")?;
    }
    let out = child.wait_with_output().context("wait for git")?;
    if !out.status.success() {
        bail!("git {}: {}", args.first().copied().unwrap_or(""), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, content: &str) -> Change {
        Change { path: path.to_owned(), content: Some(content.as_bytes().to_vec()) }
    }

    #[test]
    fn commits_land_on_the_head_they_were_made_on_and_nowhere_else() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let h = Hosted::new(tmp.path());
        let who = Author::default();
        let first = h.commit("kiln/releases", "main", None, &[file("shop/web/staging/release.json", "{}")], "register web\n\nKiln-Release: 0", &who).map_err(|e| anyhow::anyhow!("{e}"))?;
        let Committed::New(first) = first else { anyhow::bail!("the first commit is new") };
        assert_eq!(h.head("kiln/releases", "main")?.as_deref(), Some(first.as_str()));
        assert_eq!(h.read("kiln/releases", &first, "shop/web/staging/release.json")?.as_deref(), Some(&b"{}"[..]));
        assert_eq!(h.read("kiln/releases", &first, "nothing/here")?, None);
        // Made on a head that has moved: refused, and nothing changes.
        assert!(matches!(h.commit("kiln/releases", "main", None, &[file("x", "1")], "late", &who), Err(CommitError::Moved)));
        let second = h.commit("kiln/releases", "main", Some(&first), &[file("shop/web/staging/values.yaml", "a: 1\n")], "config", &who).map_err(|e| anyhow::anyhow!("{e}"))?;
        assert!(matches!(second, Committed::New(_)));
        assert!(h.read("kiln/releases", second.sha(), "shop/web/staging/release.json")?.is_some(), "what the commit didn't touch is kept");
        // The same files again: nothing to commit.
        let same = h.commit("kiln/releases", "main", Some(second.sha()), &[file("shop/web/staging/values.yaml", "a: 1\n")], "again", &who).map_err(|e| anyhow::anyhow!("{e}"))?;
        assert_eq!(same, Committed::Unchanged(second.sha().to_owned()));
        // Removing a file.
        let third = h.commit("kiln/releases", "main", Some(second.sha()), &[Change { path: "shop/web/staging/values.yaml".into(), content: None }], "drop", &who).map_err(|e| anyhow::anyhow!("{e}"))?;
        assert_eq!(h.read("kiln/releases", third.sha(), "shop/web/staging/values.yaml")?, None);
        let log = h.commits_since("kiln/releases", "main", Some(&first))?;
        assert_eq!(log.iter().map(|c| c.message.lines().next().unwrap_or("")).collect::<Vec<_>>(), ["config", "drop"]);
        assert_eq!(log.first().map(|c| c.paths.clone()), Some(vec!["shop/web/staging/values.yaml".to_owned()]));
        let all = h.commits_since("kiln/releases", "main", None)?;
        assert_eq!(all.first().and_then(|c| c.trailer("Kiln-Release")), Some("0"));
        Ok(())
    }

    #[test]
    fn names_stay_inside_the_hosted_root() -> anyhow::Result<()> {
        let h = Hosted::new(Path::new("/data/hosted"));
        assert_eq!(h.path("team/releases.git")?, Path::new("/data/hosted/team/releases.git"));
        for bad in ["", "releases", "../etc", "a/../b", "a//b", "a b", "/abs", "a/b/c", "./b"] {
            assert!(h.path(bad).is_err(), "{bad:?}");
        }
        Ok(())
    }

    #[test]
    fn release_repositories_move_into_the_hosted_root_once() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let (old, root) = (tmp.path().join("deploy/repos"), tmp.path().join("hosted"));
        Hosted::new(&old).commit("kiln/releases", "main", None, &[file("a", "1")], "first", &Author::default()).map_err(|e| anyhow::anyhow!("{e}"))?;
        let h = Hosted::new(&root);
        assert_eq!(h.adopt(&old), ["kiln/releases"]);
        assert!(h.head("kiln/releases", "main")?.is_some(), "its history came along");
        assert!(h.adopt(&old).is_empty(), "nothing moves twice");
        Ok(())
    }
}
