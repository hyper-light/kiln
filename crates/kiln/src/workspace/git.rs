//! An agent's work as git sees it, on the machine where the agent works (docs/agentic.md, C1).
//!
//! The commit that goes to review is made beside the folder, never in it: a scratch index read
//! from the commit it builds on takes the agent's files as they are in the working tree, and the
//! commit is written from that index. The person's branch, index and working tree stay exactly as
//! they were; only git's object store gains the new objects, which `git gc` collects if nothing
//! keeps them.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How long one local git command may take.
const GIT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a push may take (a slow network, a large first push).
const PUSH_TIMEOUT: Duration = Duration::from_secs(180);
/// Paths given to one `update-index`, well under any system's argument limit.
const PATHS_PER_CALL: usize = 200;

/// A folder's repository, as git sees it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Facts {
    /// The working tree's top.
    pub top: String,
    /// The commit checked out (none in a repository with no commits).
    pub head: Option<String>,
    /// The branch checked out (none when detached).
    pub branch: Option<String>,
    /// The remotes, by name.
    pub remotes: Vec<Remote>,
    /// The branch the `origin` remote calls its default, when it says.
    pub default_branch: Option<String>,
    /// In a linked worktree (`git worktree add`, Claude Code's `EnterWorktree`): the branch its
    /// repository's own checkout has, the one the worktree's work is likely for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_branch: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub url: String,
}

/// Who a commit is by.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Ident {
    pub name: String,
    pub email: String,
}

/// A commit made for review.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Made {
    pub sha: String,
    /// The commit it builds on (none: the repository had no commits).
    pub parent: Option<String>,
    /// The files it changes, relative to the top.
    pub files: Vec<String>,
}

pub(super) fn git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir);
    // Never a prompt: a question nobody sees would hang the call until its timeout.
    c.env("GIT_TERMINAL_PROMPT", "0");
    c
}

/// Runs `c`, its stdout trimmed; an error carries what git said.
pub(super) async fn run(c: Command, timeout: Duration) -> anyhow::Result<String> {
    let what = format!("{:?}", c.get_args().skip(2).collect::<Vec<_>>());
    let out = crate::proc::output(c, timeout).await?;
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        let said = if said.is_empty() { String::from_utf8_lossy(&out.stdout).trim().to_owned() } else { said };
        anyhow::bail!("git {what}: {}", if said.is_empty() { format!("{}", out.status) } else { said });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Like [`run`], `None` when git says no (a ref that isn't there).
async fn ask(c: Command) -> Option<String> {
    let out = crate::proc::output(c, GIT_TIMEOUT).await.ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned()).filter(|s| !s.is_empty())
}

/// The repository `cwd` is in.
pub async fn facts(cwd: &Path) -> anyhow::Result<Facts> {
    let mut c = git(cwd);
    c.args(["rev-parse", "--show-toplevel"]);
    let top = run(c, GIT_TIMEOUT).await.map_err(|_| anyhow::anyhow!("{} isn't in a git repository", cwd.display()))?;
    let dir = PathBuf::from(&top);
    let mut c = git(&dir);
    c.args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]);
    let head = ask(c).await;
    let mut c = git(&dir);
    c.args(["symbolic-ref", "--quiet", "--short", "HEAD"]);
    let branch = ask(c).await;
    let mut c = git(&dir);
    c.args(["remote", "-v"]);
    let remotes = remotes_of(&ask(c).await.unwrap_or_default());
    let mut c = git(&dir);
    c.args(["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"]);
    let default_branch = ask(c).await.and_then(|r| r.strip_prefix("origin/").map(str::to_owned));
    let mut c = git(&dir);
    c.args(["worktree", "list", "--porcelain"]);
    let main_branch = main_checkout(&ask(c).await.unwrap_or_default()).filter(|(path, _)| Path::new(path) != dir).and_then(|(_, b)| b);
    Ok(Facts { top, head, branch, remotes, default_branch, main_branch })
}

/// The repository's own checkout, as `git worktree list --porcelain` lists it first: its path and
/// its branch (none detached).
fn main_checkout(porcelain: &str) -> Option<(String, Option<String>)> {
    let first = porcelain.split("\n\n").next()?;
    let path = first.lines().find_map(|l| l.strip_prefix("worktree "))?.to_owned();
    let branch = first.lines().find_map(|l| l.strip_prefix("branch refs/heads/")).map(str::to_owned);
    Some((path, branch))
}

/// Where a path is in git, read from the folders alone: the top of the working tree it's in, and
/// its repository's git folder, which the repository's linked worktrees share (`git worktree add`,
/// Claude Code's `EnterWorktree`). A checkout's top has a `.git` folder; a linked worktree's, a
/// `.git` file naming its folder under the repository's `.git/worktrees/`, whose `commondir`
/// names the shared one. A submodule is a repository of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub top: PathBuf,
    pub repo: PathBuf,
}

/// [`Place`] of `path`, a file or a folder; one that's gone, by the nearest folder above it left.
pub fn place(path: &Path) -> Option<Place> {
    let mut dir = path;
    while !dir.is_dir() {
        dir = dir.parent()?;
    }
    let canonical = |p: PathBuf| std::fs::canonicalize(&p).unwrap_or(p);
    loop {
        let dot = dir.join(".git");
        match std::fs::symlink_metadata(&dot) {
            Ok(m) if m.is_dir() => return Some(Place { top: dir.to_path_buf(), repo: canonical(dot) }),
            Ok(m) if m.is_file() => {
                let text = std::fs::read_to_string(&dot).ok()?;
                let own = dir.join(text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim());
                let repo = std::fs::read_to_string(own.join("commondir")).ok().map_or_else(|| own.clone(), |c| own.join(c.trim()));
                return Some(Place { top: dir.to_path_buf(), repo: canonical(repo) });
            }
            _ => dir = dir.parent()?,
        }
    }
}

/// The working tree file `abs` is in, when it's one of the repository of `home` (an agent's
/// folder): its top, and the file's path within it (as git names it, `/`-separated).
pub fn within(abs: &Path, home: &Path) -> Option<(PathBuf, String)> {
    let file = place(abs)?;
    if place(home)?.repo != file.repo {
        return None;
    }
    let rel = abs.strip_prefix(&file.top).ok()?.to_string_lossy().into_owned();
    let rel = if cfg!(windows) { rel.replace('\\', "/") } else { rel };
    (!rel.is_empty()).then_some((file.top, rel))
}

/// `git remote -v`'s lines, each remote once (by its fetch URL).
fn remotes_of(text: &str) -> Vec<Remote> {
    let mut out: Vec<Remote> = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(name), Some(url)) = (parts.next(), parts.next()) else { continue };
        if parts.next() == Some("(push)") && out.iter().any(|r| r.name == name) {
            continue;
        }
        if !out.iter().any(|r| r.name == name) {
            out.push(Remote { name: name.to_owned(), url: url.to_owned() });
        }
    }
    out
}

/// A scratch index, removed when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let mut lock = self.0.clone().into_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(lock);
    }
}

/// Commits `paths` (relative to `top`; `None`: every change git sees, as `git add -A` takes it) as
/// they are in the working tree, on `parent` (`None`: a first commit), by `who`, saying `message`;
/// signed as [`commit_tree`] signs. `Ok(None)` when that changes nothing. Ignored files among
/// `paths` are left out, as git would leave them out; files that are gone are deleted.
pub async fn commit(top: &Path, parent: Option<&str>, paths: Option<&[String]>, message: &str, who: &Ident, sign: Option<&str>) -> anyhow::Result<Option<Made>> {
    let scratch = Scratch(std::env::temp_dir().join(format!("kiln-change-{}.index", crate::util::new_id())));
    let with_index = |c: &mut Command| {
        c.env("GIT_INDEX_FILE", &scratch.0);
    };
    let mut c = git(top);
    with_index(&mut c);
    match parent {
        Some(p) => c.args(["read-tree", p]),
        None => c.args(["read-tree", "--empty"]),
    };
    run(c, GIT_TIMEOUT).await?;
    match paths {
        None => {
            let mut c = git(top);
            with_index(&mut c);
            c.args(["add", "-A"]);
            run(c, GIT_TIMEOUT).await?;
        }
        Some(paths) => {
            let wanted = taken(top, paths).await?;
            for chunk in wanted.chunks(PATHS_PER_CALL) {
                let mut c = git(top);
                with_index(&mut c);
                c.args(["update-index", "--add", "--remove", "--"]);
                c.args(chunk);
                run(c, GIT_TIMEOUT).await?;
            }
        }
    }
    let mut c = git(top);
    with_index(&mut c);
    c.arg("write-tree");
    let tree = run(c, GIT_TIMEOUT).await?;
    let before = match parent {
        Some(p) => {
            let mut c = git(top);
            c.args(["rev-parse", &format!("{p}^{{tree}}")]);
            Some(run(c, GIT_TIMEOUT).await?)
        }
        None => None,
    };
    if before.as_deref() == Some(tree.as_str()) {
        return Ok(None);
    }
    let sha = commit_tree(top, &tree, parent, message, (who, None), who, sign).await?;
    let files = changed(top, parent, &sha).await?;
    Ok(Some(Made { sha, parent: parent.map(str::to_owned), files }))
}

/// Writes the commit of `tree` on `parent`, saying `message`, by `author` (at the time given, else
/// now), committed by `who`: an agent's work, so signed as the agent with the key this runner holds
/// in slot `sign` (its session's at its epoch, W27-R03), and never with a key the person's git
/// configuration names. A commit that can't be signed so is kept unsigned.
async fn commit_tree(top: &Path, tree: &str, parent: Option<&str>, message: &str, author: (&Ident, Option<&str>), who: &Ident, sign: Option<&str>) -> anyhow::Result<String> {
    let mut c = git(top);
    c.args(["commit-tree", "--no-gpg-sign", tree, "-F", "-"]);
    if let Some(p) = parent {
        c.args(["-p", p]);
    }
    c.env("GIT_AUTHOR_NAME", &author.0.name).env("GIT_AUTHOR_EMAIL", &author.0.email).env("GIT_COMMITTER_NAME", &who.name).env("GIT_COMMITTER_EMAIL", &who.email);
    if let Some(at) = author.1 {
        c.env("GIT_AUTHOR_DATE", at);
    }
    let sha = with_stdin(c, message.as_bytes()).await?;
    let Some(slot) = sign else { return Ok(sha) };
    match signed_as(top, &sha, slot).await {
        Ok(signed) => Ok(signed),
        Err(e) => {
            tracing::warn!(commit = %sha, error = %format!("{e:#}"), "an agent's commit is kept unsigned");
            Ok(sha)
        }
    }
}

/// Commit `sha` signed with the key this runner holds in `slot`, git's x509 way
/// (`crate::identities::signed_commit`): the signed commit, written beside it.
async fn signed_as(top: &Path, sha: &str, slot: &str) -> anyhow::Result<String> {
    let mut c = git(top);
    c.args(["cat-file", "commit", sha]);
    let out = crate::proc::output(c, GIT_TIMEOUT).await?;
    anyhow::ensure!(out.status.success(), "git cat-file: {}", String::from_utf8_lossy(&out.stderr).trim());
    let signed = crate::identities::signed_commit(&out.stdout, |m| {
        let (signature, pem) = crate::runner::identity::sign(slot, m)?;
        Ok((signature, crate::pki::der_of(&pem)?))
    })?;
    let mut c = git(top);
    c.args(["hash-object", "-t", "commit", "-w", "--stdin"]);
    with_stdin(c, &signed).await
}

/// The files commit `sha` changes over `parent` (`None`: all of them, a first commit).
async fn changed(top: &Path, parent: Option<&str>, sha: &str) -> anyhow::Result<Vec<String>> {
    let mut c = git(top);
    match parent {
        Some(p) => c.args(["diff-tree", "--no-commit-id", "--name-only", "-r", "-z", p, sha]),
        None => c.args(["diff-tree", "--no-commit-id", "--name-only", "-r", "-z", "--root", sha]),
    };
    Ok(run(c, GIT_TIMEOUT).await?.split('\0').filter(|s| !s.is_empty()).map(str::to_owned).collect())
}

/// Commit `sha`'s message, its author and when they wrote it.
async fn authored(top: &Path, sha: &str) -> anyhow::Result<(String, Ident, String)> {
    let mut c = git(top);
    c.args(["log", "-1", "--format=%an%x00%ae%x00%aD%x00%B", sha]);
    let out = crate::proc::output(c, GIT_TIMEOUT).await?;
    anyhow::ensure!(out.status.success(), "git log {sha}: {}", String::from_utf8_lossy(&out.stderr).trim());
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut parts = text.splitn(4, '\0');
    let (name, email, date, message) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default(), parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
    Ok((format!("{}\n", message.trim_end()), Ident { name: name.to_owned(), email: email.to_owned() }, date.to_owned()))
}

/// What commit `to` changes over commit `from`, as `git apply` takes it back.
async fn patch(top: &Path, from: &str, to: &str) -> anyhow::Result<Vec<u8>> {
    let mut c = git(top);
    c.args(["diff", "--binary", "--full-index", "--no-renames", "--no-color", "--no-ext-diff", "--no-textconv", from, to]);
    let out = crate::proc::output(c, GIT_TIMEOUT).await?;
    anyhow::ensure!(out.status.success(), "git diff {from} {to}: {}", String::from_utf8_lossy(&out.stderr).trim());
    Ok(out.stdout)
}

/// The tree `patch` makes of commit `onto`'s, made beside the folder (a scratch index, a three-way
/// merge where the patch's context has moved): `Err` with what git said when it doesn't go.
async fn applied(top: &Path, onto: &str, patch: &[u8]) -> anyhow::Result<String> {
    let id = crate::util::new_id();
    let scratch = Scratch(std::env::temp_dir().join(format!("kiln-restack-{id}.index")));
    let file = Scratch(std::env::temp_dir().join(format!("kiln-restack-{id}.diff")));
    std::fs::write(&file.0, patch)?;
    let mut c = git(top);
    c.env("GIT_INDEX_FILE", &scratch.0).args(["read-tree", onto]);
    run(c, GIT_TIMEOUT).await?;
    if !patch.is_empty() {
        let mut c = git(top);
        c.env("GIT_INDEX_FILE", &scratch.0).args(["apply", "--cached", "--3way", "--whitespace=nowarn"]).arg(&file.0);
        run(c, GIT_TIMEOUT).await?;
    }
    let mut c = git(top);
    c.env("GIT_INDEX_FILE", &scratch.0).arg("write-tree");
    run(c, GIT_TIMEOUT).await
}

async fn tree_of(top: &Path, sha: &str) -> anyhow::Result<String> {
    let mut c = git(top);
    c.args(["rev-parse", &format!("{sha}^{{tree}}")]);
    run(c, GIT_TIMEOUT).await
}

/// A new version of the first of `commits` (a stack's, bottom first, each on the one before) with
/// the agent's `paths` as the folder has them now, and the commits above it made again on it
/// (docs/agentic.md P4). The agent's change is taken against the stack's top (the folder holds the
/// whole stack's work), then put into the first commit; each commit above is its own change again,
/// on the new one below. `message` replaces the first's. Refused, saying why, when a change
/// doesn't go where it must (it changes what a commit above changed too), or when the new top
/// wouldn't be what the folder has.
pub async fn restack(top: &Path, commits: &[String], paths: &[String], message: Option<&str>, who: &Ident, sign: Option<&str>) -> anyhow::Result<Vec<Made>> {
    let (Some(first), Some(last)) = (commits.first(), commits.last()) else { anyhow::bail!("no commits to make again") };
    // A scratch commit to take the change from, never pushed: unsigned.
    let Some(change) = commit(top, Some(last), Some(paths), "kiln: an agent's change, before it is restacked", who, None).await? else {
        anyhow::bail!("nothing to add: its files are as the stack has them")
    };
    let mut c = git(top);
    c.args(["rev-parse", "--verify", "--quiet", &format!("{first}^1")]);
    let base = run(c, GIT_TIMEOUT).await.map_err(|_| anyhow::anyhow!("{first} has no commit below it"))?;
    let into_first = patch(top, last, &change.sha).await?;
    let mut made: Vec<Made> = Vec::new();
    let (mut old_below, mut new_below) = (base.clone(), base);
    for (i, old) in commits.iter().enumerate() {
        let what = if i == 0 { into_first.clone() } else { patch(top, &old_below, old).await? };
        let onto = if i == 0 { old.clone() } else { new_below.clone() };
        let tree = applied(top, &onto, &what).await.map_err(|e| {
            if i == 0 {
                anyhow::anyhow!("the change can't go into its own commit alone: it changes what a later one changed too ({e:#})")
            } else {
                anyhow::anyhow!("{} doesn't go on the new version below it ({e:#})", short_sha(old))
            }
        })?;
        let (said, author, date) = authored(top, old).await?;
        let message = if i == 0 { message.map_or(said, str::to_owned) } else { said };
        let sha = commit_tree(top, &tree, Some(&new_below), &message, (&author, Some(&date)), who, sign).await?;
        let files = changed(top, Some(&new_below), &sha).await?;
        made.push(Made { sha: sha.clone(), parent: Some(new_below.clone()), files });
        old_below.clone_from(old);
        new_below = sha;
    }
    anyhow::ensure!(tree_of(top, &new_below).await? == tree_of(top, &change.sha).await?, "made again, the commits don't come out as the folder has them");
    Ok(made)
}

fn short_sha(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// Runs `c` with `input` on its stdin; its stdout trimmed.
async fn with_stdin(mut c: Command, input: &[u8]) -> anyhow::Result<String> {
    use std::io::Write as _;
    use std::process::Stdio;
    let what = c.get_args().nth(2).map(|a| a.to_string_lossy().into_owned()).unwrap_or_default();
    let input = input.to_vec();
    c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = crate::rt::blocking(move || -> anyhow::Result<std::process::Output> {
        let mut child = c.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(&input)?;
        }
        Ok(child.wait_with_output()?)
    })
    .await??;
    if !out.status.success() {
        anyhow::bail!("git {what}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Commits `paths` as the working tree has them on `on` (`None`: the folder's last commit), as
/// [`commit`] does: one step of a plan run's stack. `Ok(None)` when that changes nothing.
pub async fn commit_on(cwd: &Path, on: Option<&str>, paths: &[String], message: &str, fallback: &Ident, sign: Option<&str>) -> anyhow::Result<Option<Made>> {
    let facts = facts(cwd).await?;
    let top = Path::new(&facts.top);
    let who = identity(top, fallback).await;
    let parent = on.or(facts.head.as_deref());
    if let Some(p) = on {
        let mut c = git(top);
        c.args(["cat-file", "-e", &format!("{p}^{{commit}}")]);
        run(c, GIT_TIMEOUT).await.map_err(|_| anyhow::anyhow!("the commit of the step before ({}) isn't in {} any more", short_sha(p), facts.top))?;
    }
    commit(top, parent, Some(paths), message, &who, sign).await
}

/// Of `paths`, those a commit takes: inside the repository, and not ignored (unless git tracks them).
async fn taken(top: &Path, paths: &[String]) -> anyhow::Result<Vec<String>> {
    let mut inside: Vec<String> = Vec::new();
    for p in paths {
        let rel = relative(top, p).ok_or_else(|| anyhow::anyhow!("{p} isn't in the repository at {}", top.display()))?;
        if !rel.is_empty() && !inside.contains(&rel) {
            inside.push(rel);
        }
    }
    let mut ignored: Vec<String> = Vec::new();
    for chunk in inside.chunks(PATHS_PER_CALL) {
        let mut c = git(top);
        c.args(["check-ignore", "--"]);
        c.args(chunk);
        // Exit 1 is "none of them": no error.
        let out = crate::proc::output(c, GIT_TIMEOUT).await?;
        ignored.extend(String::from_utf8_lossy(&out.stdout).lines().map(str::to_owned));
    }
    Ok(inside.into_iter().filter(|p| !ignored.contains(p)).collect())
}

/// `path` (absolute, or relative to `top`) relative to `top`, with `/` between parts; `None` when
/// it is outside.
fn relative(top: &Path, path: &str) -> Option<String> {
    let p = Path::new(path);
    let rel = if p.is_absolute() {
        // A top reached through a symlink (macOS's /var → /private/var) still matches.
        p.strip_prefix(top).ok().map(Path::to_path_buf).or_else(|| {
            let real_top = std::fs::canonicalize(top).ok()?;
            let real = p.parent().and_then(|d| std::fs::canonicalize(d).ok()).map(|d| d.join(p.file_name().unwrap_or_default()))?;
            real.strip_prefix(&real_top).ok().map(Path::to_path_buf)
        })?
    } else {
        p.to_path_buf()
    };
    let mut parts: Vec<String> = Vec::new();
    for c in rel.components() {
        match c {
            std::path::Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

/// Pushes `refspec` to `to` (a remote's name or a URL) from `top`, with `bearer` as the
/// `Authorization` a kiln server takes when given. What git and the remote said.
pub async fn push(top: &Path, to: &str, refspec: &str, bearer: Option<&str>) -> anyhow::Result<String> {
    let mut c = git(top);
    if let Some(t) = bearer {
        c.arg("-c").arg(format!("http.extraHeader=Authorization: Bearer {t}"));
    }
    c.args(["push", "--porcelain", to, refspec]);
    // A key that needs its passphrase typed fails at once rather than waiting for it.
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        c.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    let out = crate::proc::output(c, PUSH_TIMEOUT).await?;
    let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        let said = said.trim();
        // A token never shows up in what's said back.
        let said = bearer.map_or_else(|| said.to_owned(), |t| said.replace(t, "…"));
        anyhow::bail!("{}", if said.is_empty() { format!("the push failed ({})", out.status) } else { said });
    }
    Ok(said)
}

/// Who commits on this machine (git's own `user.name` and `user.email`), else `fallback`.
pub async fn identity(top: &Path, fallback: &Ident) -> Ident {
    let mut c = git(top);
    c.args(["config", "user.name"]);
    let name = ask(c).await;
    let mut c = git(top);
    c.args(["config", "user.email"]);
    let email = ask(c).await;
    match (name, email) {
        (Some(name), Some(email)) => Ident { name, email },
        _ => fallback.clone(),
    }
}

/// The commit `cwd`'s repository has checked out, if any: where a session began, its commits after
/// it its own (the Changes tab's Commits).
pub async fn head(cwd: &Path) -> Option<String> {
    let mut c = git(cwd);
    c.args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]);
    ask(c).await
}

/// Most commits [`log`] lists.
const LOG_MOST: usize = 200;
/// Commits [`log`] lists of a branch with none of its own since its trunk: its latest.
const LOG_LATEST: usize = 50;

/// One commit as the Changes tab's Commits lists it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "LoggedCommit"))]
pub struct Logged {
    pub sha: String,
    pub parents: Vec<String>,
    pub subject: String,
    pub author: String,
    /// When it was committed (unix ms).
    pub at: i64,
    /// What it changes over its first parent: files, and lines added and removed.
    pub files: u32,
    pub added: u32,
    pub removed: u32,
    /// The session's own: made after the commit its folder had as it began.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub session: bool,
}

/// What isn't committed yet, as `git add -A` would take it: how many files, and lines.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "UncommittedCounts"))]
pub struct Counts {
    pub files: u32,
    pub added: u32,
    pub removed: u32,
}

/// A branch's commits since its trunk ([`log`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "CommitLog"))]
pub struct Log {
    /// The working tree's top.
    pub top: String,
    pub head: Option<String>,
    pub branch: Option<String>,
    /// The trunk it's measured from (`origin/main`), and the commit where they split; none when
    /// it has none, or nothing of its own since (then its latest commits are listed).
    pub trunk: Option<String>,
    pub base: Option<String>,
    /// Not committed yet; none when everything is.
    pub dirty: Option<Counts>,
    /// Newest first, along its first parents.
    pub commits: Vec<Logged>,
    /// There are more than are listed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub more: bool,
    /// How its session's commits were told: `head` (after the commit it began at), `time` (made
    /// after it began: a session from before kiln kept that commit, or one whose branch was
    /// rewritten since), or `none`.
    pub told: String,
    /// Its repository's branches, the latest committed to first (a dozen at most): what Commits
    /// may list instead.
    pub branches: Vec<String>,
    /// The branch listed, when it isn't the one checked out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub listed: Option<String>,
}

/// `cwd`'s branch's commits (or `branch`'s) since `trunk` (none: `origin`'s default branch, else
/// `main` or `master`), newest first along its first parents; or its latest, when it has none of
/// its own. A session's own are marked: those after `since_head` (the commit it began at), else
/// those made after `since_at` (unix ms). What isn't committed yet is the checked-out branch's.
pub async fn log(cwd: &Path, branch: Option<&str>, trunk: Option<&str>, since_head: Option<&str>, since_at: Option<i64>) -> anyhow::Result<Log> {
    let f = facts(cwd).await?;
    let top = PathBuf::from(&f.top);
    let mut out = Log { top: f.top.clone(), head: f.head.clone(), branch: f.branch.clone(), told: "none".into(), ..Log::default() };
    let mut c = git(&top);
    c.args(["for-each-ref", "--sort=-committerdate", "--count=12", "--format=%(refname:short)", "refs/heads"]);
    out.branches = ask(c).await.unwrap_or_default().lines().map(str::to_owned).collect();
    let Some(checked_out) = f.head.clone() else {
        out.dirty = counts_of(&top, None).await?;
        return Ok(out);
    };
    // Another branch than the one checked out: its commits, and nothing uncommitted of its.
    let other = branch.filter(|b| f.branch.as_deref() != Some(*b));
    let head = match other {
        Some(b) => {
            let mut c = git(&top);
            c.args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{b}^{{commit}}")]);
            out.listed = Some(b.to_owned());
            ask(c).await.ok_or_else(|| anyhow::anyhow!("there's no branch {b} here"))?
        }
        None => checked_out.clone(),
    };
    let wanted: Vec<String> = match trunk {
        Some(t) => vec![t.to_owned()],
        None => f.default_branch.iter().map(|b| format!("origin/{b}")).chain(["origin/main", "origin/master", "main", "master"].map(str::to_owned)).collect(),
    };
    for t in wanted {
        let mut c = git(&top);
        c.args(["rev-parse", "--verify", "--quiet", &format!("{t}^{{commit}}")]);
        if ask(c).await.is_none() {
            continue;
        }
        let mut c = git(&top);
        c.args(["merge-base", &head, &t]);
        if let Some(base) = ask(c).await.filter(|b| *b != head) {
            out.trunk = Some(t);
            out.base = Some(base);
        }
        break;
    }
    let mut c = git(&top);
    c.args(["log", "--first-parent", "--diff-merges=first-parent", "--no-renames", "--numstat", "--format=%x1e%H%x1f%P%x1f%an%x1f%ct%x1f%s"]);
    match &out.base {
        Some(base) => c.arg(format!("-n{}", LOG_MOST + 1)).arg(format!("{base}..{head}")),
        None => c.arg(format!("-n{}", LOG_LATEST + 1)).arg(&head),
    };
    let text = run(c, GIT_TIMEOUT).await?;
    let mut commits = parse_log(&text);
    let most = if out.base.is_some() { LOG_MOST } else { LOG_LATEST };
    out.more = commits.len() > most;
    commits.truncate(most);
    // Its session's own: after the commit it began at, while that's still in its history.
    let mut ours: Option<Vec<String>> = None;
    if let Some(h) = since_head.filter(|h| !h.is_empty()) {
        let mut c = git(&top);
        c.args(["merge-base", "--is-ancestor", h, &head]);
        if crate::proc::output(c, GIT_TIMEOUT).await.is_ok_and(|o| o.status.success()) {
            let mut c = git(&top);
            c.args(["rev-list", "--first-parent", &format!("{h}..{head}")]);
            ours = Some(run(c, GIT_TIMEOUT).await?.lines().map(str::to_owned).collect());
        }
    }
    match (ours, since_at) {
        (Some(ours), _) => {
            out.told = "head".into();
            for c in &mut commits {
                c.session = ours.contains(&c.sha);
            }
        }
        (None, Some(at)) => {
            out.told = "time".into();
            for c in &mut commits {
                c.session = c.at >= at;
            }
        }
        (None, None) => {}
    }
    out.commits = commits;
    if other.is_none() {
        out.dirty = counts_of(&top, Some(&checked_out)).await?;
    }
    Ok(out)
}

/// Commits from `git log --numstat` in [`log`]'s format: each record a line of its fields, then a
/// line a file it changes.
fn parse_log(text: &str) -> Vec<Logged> {
    let mut out = Vec::new();
    for record in text.split('\u{1e}').filter(|r| !r.trim().is_empty()) {
        let mut lines = record.lines();
        let mut fields = lines.next().unwrap_or_default().split('\u{1f}');
        let mut field = || fields.next().unwrap_or_default().to_owned();
        let (sha, parents, author, at, subject) = (field(), field(), field(), field(), field());
        let mut l = Logged { sha, parents: parents.split_whitespace().map(str::to_owned).collect(), subject, author, at: at.parse::<i64>().unwrap_or(0).saturating_mul(1000), ..Logged::default() };
        for line in lines.filter(|l| !l.trim().is_empty()) {
            let mut cols = line.split('\t');
            let (add, del) = (cols.next().unwrap_or_default(), cols.next().unwrap_or_default());
            l.files += 1;
            // A binary file's counts are `-`.
            l.added += add.parse::<u32>().unwrap_or(0);
            l.removed += del.parse::<u32>().unwrap_or(0);
        }
        out.push(l);
    }
    out
}

/// The tree of `top`'s working tree as `git add -A` would take it onto `head` (none: no commits
/// yet), made beside the folder in a scratch index: its untracked files too, never its ignored.
async fn now_tree(top: &Path, head: Option<&str>) -> anyhow::Result<String> {
    let scratch = Scratch(std::env::temp_dir().join(format!("kiln-now-{}.index", crate::util::new_id())));
    let mut c = git(top);
    c.env("GIT_INDEX_FILE", &scratch.0);
    match head {
        Some(h) => c.args(["read-tree", h]),
        None => c.args(["read-tree", "--empty"]),
    };
    run(c, GIT_TIMEOUT).await?;
    let mut c = git(top);
    c.env("GIT_INDEX_FILE", &scratch.0).args(["add", "-A"]);
    run(c, GIT_TIMEOUT).await?;
    let mut c = git(top);
    c.env("GIT_INDEX_FILE", &scratch.0).arg("write-tree");
    run(c, GIT_TIMEOUT).await
}

/// The empty tree's id: what a repository without commits is compared from.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// What isn't committed yet over `head`: none when nothing.
async fn counts_of(top: &Path, head: Option<&str>) -> anyhow::Result<Option<Counts>> {
    let now = now_tree(top, head).await?;
    let mut c = git(top);
    c.args(["diff", "--no-renames", "--numstat", head.unwrap_or(EMPTY_TREE), &now]);
    let text = run(c, GIT_TIMEOUT).await?;
    let mut n = Counts::default();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let mut cols = line.split('\t');
        n.files += 1;
        n.added += cols.next().unwrap_or_default().parse::<u32>().unwrap_or(0);
        n.removed += cols.next().unwrap_or_default().parse::<u32>().unwrap_or(0);
    }
    Ok((n.files > 0).then_some(n))
}

/// What isn't committed yet, as a pick of [`picked`] names it.
pub const NOW: &str = "now";

/// The picked commits' change, together ([`picked`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "PickedCommits"))]
pub struct Picked {
    /// What it's measured from: the first pick's parent.
    pub from: String,
    pub files: Vec<crate::harness::model::FileChange>,
    /// A pick that doesn't go on the ones before it without a commit that wasn't picked: the
    /// pick, and the commit it needs (none: no single one makes it go).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub needs: Option<Needs>,
}

/// A pick that doesn't go without a commit left out ([`Picked::needs`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "PickNeeds"))]
pub struct Needs {
    pub pick: String,
    pub commit: Option<String>,
    pub subject: Option<String>,
}

/// What `picks` (commits of `cwd`'s branch, or `branch`'s; [`NOW`] for what isn't committed yet
/// on the one checked out) change together: a run of them, from the first one's parent to the
/// last; with gaps, each applied in order onto the first one's parent in a scratch index, the
/// folder untouched. One that doesn't go without a commit left out says which.
pub async fn picked(cwd: &Path, branch: Option<&str>, picks: &[String]) -> anyhow::Result<Picked> {
    let f = facts(cwd).await?;
    let top = PathBuf::from(&f.top);
    let head = f.head.clone();
    let other = branch.filter(|b| f.branch.as_deref() != Some(*b));
    // The branch's first-parent line, oldest first, what isn't committed yet after the one checked out's.
    let tip = match other {
        Some(b) => Some(format!("refs/heads/{b}")),
        None => head.clone(),
    };
    let mut line: Vec<String> = match &tip {
        Some(t) => {
            let mut c = git(&top);
            c.args(["rev-list", "--first-parent", "--reverse", t]);
            run(c, GIT_TIMEOUT).await?.lines().map(str::to_owned).collect()
        }
        None => Vec::new(),
    };
    if other.is_none() {
        line.push(NOW.to_owned());
    }
    let mut at: Vec<usize> = Vec::new();
    for p in picks {
        let i = line.iter().position(|s| s == p || (p.len() >= 7 && s.starts_with(p.as_str()))).ok_or_else(|| anyhow::anyhow!("{} isn't on this branch's line of commits", short_sha(p)))?;
        if !at.contains(&i) {
            at.push(i);
        }
    }
    at.sort_unstable();
    let (Some(&first), Some(&last)) = (at.first(), at.last()) else { anyhow::bail!("no commits picked") };
    // A commit's tree, what isn't committed yet as a tree, and what came before each.
    let now = if line.get(last).is_some_and(|s| s == NOW) { Some(now_tree(&top, head.as_deref()).await?) } else { None };
    let tree_at = |i: usize| -> String { if line.get(i).is_some_and(|s| s == NOW) { now.clone().unwrap_or_default() } else { line.get(i).cloned().unwrap_or_default() } };
    let before = |i: usize| -> String { i.checked_sub(1).and_then(|j| line.get(j).cloned()).unwrap_or_else(|| EMPTY_TREE.to_owned()) };
    let from = before(first);
    let run_of_them = at.windows(2).all(|w| matches!(w, [a, b] if b - a == 1));
    let mut needs = None;
    let to = if run_of_them {
        tree_at(last)
    } else {
        let mut acc = from.clone();
        let mut prev: Option<usize> = None;
        for &i in &at {
            let what = patch(&top, &before(i), &tree_at(i)).await?;
            match applied(&top, &acc, &what).await {
                Ok(t) => acc = t,
                Err(_) => {
                    // The commit left out between the last pick and this one that it needs: the
                    // first that, put in, lets it go.
                    let mut found = None;
                    for j in prev.map_or(first, |p| p + 1)..i {
                        let Ok(with) = applied(&top, &acc, &patch(&top, &before(j), &tree_at(j)).await?).await else { continue };
                        if applied(&top, &with, &what).await.is_ok() {
                            found = line.get(j).cloned();
                            break;
                        }
                    }
                    let subject = match &found {
                        Some(sha) => {
                            let mut c = git(&top);
                            c.args(["log", "-1", "--format=%s", sha]);
                            ask(c).await
                        }
                        None => None,
                    };
                    needs = Some(Needs { pick: line.get(i).cloned().unwrap_or_default(), commit: found, subject });
                    break;
                }
            }
            prev = Some(i);
        }
        acc
    };
    let files = if needs.is_some() {
        Vec::new()
    } else {
        let mut c = git(&top);
        c.args(["diff", "--no-renames", "--no-color", "--no-ext-diff", "--no-textconv", "-U3", &from, &to]);
        crate::harness::diff::git_changes(&run(c, GIT_TIMEOUT).await?, &f.top, crate::util::now_ms())
    };
    Ok(Picked { from, files, needs })
}

/// A point Compare's From or To picks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Point {
    /// The folder as it is now, untracked files too (as `git add -A` would take it).
    Now,
    /// As its session began: the commit its folder had checked out then.
    Began,
    /// A branch, a tag or a commit.
    Ref { name: String },
    /// One who changed its files (`main`, the agent itself, or a subagent run): the folder as its
    /// session began, with the files it changed as it left them.
    Agent { by: String },
}

/// One who changed files' version of one: as they left it.
#[derive(Debug, Clone, PartialEq)]
pub enum Left {
    Text(std::sync::Arc<str>),
    Absent,
    /// As it is now: the latest to change it, or how they left it isn't known.
    Now,
}

/// A file one who changed files left: where it is in its repository, where on disk (in whichever
/// of its working trees), and how they left it.
#[derive(Debug, Clone, PartialEq)]
pub struct LeftFile {
    pub path: String,
    pub abs: String,
    pub left: Left,
}

/// Each one who changed files' version of them, by who (`super::super::harness::model::FileChange::by`'s names).
pub type Versions = std::collections::HashMap<String, Vec<LeftFile>>;

/// What Compare's pickers offer of a repository besides the session's own points ([`refs`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "CompareRefs"))]
pub struct Refs {
    /// Its branches, the latest committed to first; its remote's after its own.
    pub branches: Vec<String>,
    pub tags: Vec<String>,
    /// The latest commits on the branch checked out.
    pub commits: Vec<Logged>,
    /// Its trunk (`origin/main`), when it has one: From at first.
    pub trunk: Option<String>,
}

/// Most of each kind [`refs`] offers.
const REFS_MOST: usize = 50;

/// `cwd`'s repository's branches (its own, then its remote's), tags and latest commits, for
/// Compare's pickers.
pub async fn refs(cwd: &Path) -> anyhow::Result<Refs> {
    let f = facts(cwd).await?;
    let top = PathBuf::from(&f.top);
    let list = |pattern: &'static str, sort: &'static str| {
        let mut c = git(&top);
        c.args(["for-each-ref", sort, &format!("--count={REFS_MOST}"), "--format=%(refname:short)", pattern]);
        c
    };
    let mut branches: Vec<String> = ask(list("refs/heads", "--sort=-committerdate")).await.unwrap_or_default().lines().map(str::to_owned).collect();
    branches.extend(ask(list("refs/remotes", "--sort=-committerdate")).await.unwrap_or_default().lines().filter(|b| !b.ends_with("/HEAD")).map(str::to_owned));
    let tags = ask(list("refs/tags", "--sort=-creatordate")).await.unwrap_or_default().lines().map(str::to_owned).collect();
    let commits = match &f.head {
        Some(_) => {
            let mut c = git(&top);
            c.args(["log", "--first-parent", "--no-renames", "--numstat", "--format=%x1e%H%x1f%P%x1f%an%x1f%ct%x1f%s", &format!("-n{REFS_MOST}"), "HEAD"]);
            parse_log(&run(c, GIT_TIMEOUT).await?)
        }
        None => Vec::new(),
    };
    let trunk = f.default_branch.map(|b| format!("origin/{b}")).filter(|t| branches.contains(t)).or_else(|| ["origin/main", "origin/master", "main", "master"].into_iter().map(str::to_owned).find(|t| branches.contains(t)));
    Ok(Refs { branches, tags, commits, trunk })
}

/// Two points compared ([`compare`]): what changes from one to the other.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "Compared"))]
pub struct Compared {
    pub files: Vec<crate::harness::model::FileChange>,
    /// Too many to send whole: their counts only; a file's lines come when it's asked for alone.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub counts_only: bool,
    /// Compared from where they split (git's three-dot), as asked; off where one of them isn't a
    /// commit's (two agents' versions).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub split: bool,
    /// For two agents' versions: of each file (by its path), which of them changed it.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub changed_by: std::collections::BTreeMap<String, Vec<String>>,
}

/// Most files, and lines, a compare sends whole; past them, counts first.
const COMPARE_FILES: usize = 300;
const COMPARE_LINES: usize = 20_000;

/// A point as a tree, and the commit it stands on (for where two split).
struct Resolved {
    tree: String,
    commit: Option<String>,
    /// An agent's: the paths it changed.
    touched: Option<Vec<String>>,
}

/// `point` in `top`'s repository: `began` the commit its session began at (else the one checked
/// out), `versions` who changed files' versions of them.
async fn resolve(top: &Path, head: Option<&str>, began: Option<&str>, point: &Point, versions: &Versions) -> anyhow::Result<Resolved> {
    let began = began.or(head);
    match point {
        Point::Now => Ok(Resolved { tree: now_tree(top, head).await?, commit: head.map(str::to_owned), touched: None }),
        Point::Began => {
            let b = began.ok_or_else(|| anyhow::anyhow!("its folder has no commit to say how it was as it began"))?;
            Ok(Resolved { tree: tree_of(top, b).await?, commit: Some(b.to_owned()), touched: None })
        }
        Point::Ref { name } => {
            let mut c = git(top);
            c.args(["rev-parse", "--verify", "--quiet", &format!("{name}^{{commit}}")]);
            let sha = ask(c).await.ok_or_else(|| anyhow::anyhow!("there's no branch, tag or commit {name} here"))?;
            Ok(Resolved { tree: tree_of(top, &sha).await?, commit: Some(sha), touched: None })
        }
        Point::Agent { by } => {
            let files = versions.get(by).ok_or_else(|| anyhow::anyhow!("how {by} left its files is known only on the machine it works on"))?;
            let scratch = Scratch(std::env::temp_dir().join(format!("kiln-version-{}.index", crate::util::new_id())));
            let mut c = git(top);
            c.env("GIT_INDEX_FILE", &scratch.0);
            match began {
                Some(b) => c.args(["read-tree", b]),
                None => c.args(["read-tree", "--empty"]),
            };
            run(c, GIT_TIMEOUT).await?;
            for f in files {
                match &f.left {
                    Left::Text(t) => {
                        let mut c = git(top);
                        c.args(["hash-object", "-w", "--stdin"]);
                        let blob = with_stdin(c, t.as_bytes()).await?;
                        let mut c = git(top);
                        c.env("GIT_INDEX_FILE", &scratch.0).args(["update-index", "--add", "--cacheinfo", &format!("100644,{blob},{}", f.path)]);
                        run(c, GIT_TIMEOUT).await?;
                    }
                    Left::Absent => {
                        let mut c = git(top);
                        c.env("GIT_INDEX_FILE", &scratch.0).args(["update-index", "--force-remove", "--", &f.path]);
                        run(c, GIT_TIMEOUT).await?;
                    }
                    Left::Now => {
                        // As it is on disk, in whichever working tree it's in; gone, it's gone.
                        let mut c = git(top);
                        c.args(["hash-object", "-w", "--no-filters", "--", &f.abs]);
                        let mut c2 = git(top);
                        match run(c, GIT_TIMEOUT).await {
                            Ok(blob) => c2.env("GIT_INDEX_FILE", &scratch.0).args(["update-index", "--add", "--cacheinfo", &format!("100644,{blob},{}", f.path)]),
                            Err(_) => c2.env("GIT_INDEX_FILE", &scratch.0).args(["update-index", "--force-remove", "--", &f.path]),
                        };
                        run(c2, GIT_TIMEOUT).await?;
                    }
                }
            }
            let mut c = git(top);
            c.env("GIT_INDEX_FILE", &scratch.0).arg("write-tree");
            let tree = run(c, GIT_TIMEOUT).await?;
            Ok(Resolved { tree, commit: began.map(str::to_owned), touched: Some(files.iter().map(|f| f.path.clone()).collect()) })
        }
    }
}

/// What changes from `from` to `to` in `cwd`'s repository: with `split`, from where they split
/// (git's three-dot), where both stand on commits, never between two agents' versions; `path`:
/// that file alone, its lines whatever the compare's size. `began`: the commit its session began
/// at; `versions`: who changed files' versions of them.
pub async fn compare(cwd: &Path, from: &Point, to: &Point, split: bool, path: Option<&str>, began: Option<&str>, versions: &Versions) -> anyhow::Result<Compared> {
    let f = facts(cwd).await?;
    let top = PathBuf::from(&f.top);
    let head = f.head.as_deref();
    let a = resolve(&top, head, began, from, versions).await?;
    let b = resolve(&top, head, began, to, versions).await?;
    let agents = a.touched.is_some() && b.touched.is_some();
    let mut out = Compared::default();
    let mut base = a.tree.clone();
    if split && !agents
        && let (Some(x), Some(y)) = (&a.commit, &b.commit)
    {
        let mut c = git(&top);
        c.args(["merge-base", x, y]);
        if let Some(m) = ask(c).await {
            base = tree_of(&top, &m).await?;
            out.split = true;
        }
    }
    let mut c = git(&top);
    c.args(["diff", "--no-renames", "--numstat", &base, &b.tree]);
    let numstat = run(c, GIT_TIMEOUT).await?;
    let (mut files, mut lines) = (0usize, 0usize);
    for l in numstat.lines().filter(|l| !l.trim().is_empty()) {
        let mut cols = l.split('\t');
        files += 1;
        lines += cols.next().unwrap_or_default().parse::<usize>().unwrap_or(0) + cols.next().unwrap_or_default().parse::<usize>().unwrap_or(0);
    }
    let whole = path.is_some() || (files <= COMPARE_FILES && lines <= COMPARE_LINES);
    let mut c = git(&top);
    c.args(["diff", "--no-renames", "--no-color", "--no-ext-diff", "--no-textconv", "-U3", &base, &b.tree]);
    if let Some(p) = path {
        c.args(["--", p]);
    }
    if whole {
        out.files = crate::harness::diff::git_changes(&run(c, GIT_TIMEOUT).await?, &f.top, crate::util::now_ms());
    } else {
        // Counts first: each file as numstat says, its lines when asked for alone.
        out.counts_only = true;
        let mut c = git(&top);
        c.args(["diff", "--no-renames", "--numstat", "--summary", &base, &b.tree]);
        let text = run(c, GIT_TIMEOUT).await?;
        let created: Vec<&str> = text.lines().filter_map(|l| l.trim().strip_prefix("create mode ")).filter_map(|l| l.split_once(' ').map(|(_, p)| p)).collect();
        let deleted: Vec<&str> = text.lines().filter_map(|l| l.trim().strip_prefix("delete mode ")).filter_map(|l| l.split_once(' ').map(|(_, p)| p)).collect();
        for l in text.lines().filter(|l| l.contains('\t')) {
            let mut cols = l.split('\t');
            let (add, del, p) = (cols.next().unwrap_or_default(), cols.next().unwrap_or_default(), cols.next().unwrap_or_default());
            out.files.push(crate::harness::model::FileChange {
                path: p.to_owned(),
                tree: Some(f.top.clone()),
                created: created.contains(&p),
                deleted: deleted.contains(&p),
                added: add.parse().unwrap_or(0),
                removed: del.parse().unwrap_or(0),
                hunks: Vec::new(),
                truncated: true,
                at: crate::util::now_ms(),
                by: Vec::new(),
                place: Some(crate::harness::model::FilePlace::Project),
            });
        }
    }
    // Two agents' versions: which of them changed each file.
    if let (Point::Agent { by: x }, Point::Agent { by: y }, Some(ta), Some(tb)) = (from, to, &a.touched, &b.touched) {
        for file in &out.files {
            let who: Vec<String> = [(x, ta), (y, tb)].into_iter().filter(|(_, t)| t.contains(&file.path)).map(|(w, _)| w.clone()).collect();
            out.changed_by.insert(file.path.clone(), who);
        }
    }
    Ok(out)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(dir: &Path, script: &str) -> anyhow::Result<()> {
        let ok = Command::new("sh").arg("-c").arg(script).current_dir(dir).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").status()?.success();
        anyhow::ensure!(ok, "{script}");
        Ok(())
    }

    fn out(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
        Ok(String::from_utf8_lossy(&Command::new("git").args(args).current_dir(dir).output()?.stdout).trim().to_owned())
    }

    fn repo() -> anyhow::Result<tempfile::TempDir> {
        let d = tempfile::tempdir()?;
        sh(
            d.path(),
            "git init -q -b main && git config user.name Ada && git config user.email ada@example.com && git config commit.gpgsign false && git config tag.gpgsign false && echo a > a && echo b > b && mkdir -p src && echo s > src/s.rs && printf 'target/\\n*.log\\n' > .gitignore && git add . && git commit -qm init && git remote add origin git@github.com:acme/app.git",
        )?;
        Ok(d)
    }

    #[test]
    fn a_commit_takes_only_the_agent_s_files_and_leaves_the_folder_as_it_was() -> anyhow::Result<()> {
        let d = repo()?;
        let top = d.path().to_path_buf();
        // The person's own edit (b), the agent's (a, a new src/new.rs, src/s.rs deleted), a build
        // output and a log the agent wrote.
        sh(&top, "echo mine > b && echo a2 > a && echo n > src/new.rs && rm src/s.rs && mkdir -p target && echo o > target/out && echo l > x.log")?;
        let status_before = out(&top, &["status", "--porcelain"])?;
        let here = top.clone();
        crate::rt::block_on(async move {
            let top = here;
            let f = facts(&top.join("src")).await?;
            assert_eq!(f.branch.as_deref(), Some("main"));
            assert_eq!(f.remotes, vec![Remote { name: "origin".into(), url: "git@github.com:acme/app.git".into() }]);
            let head = f.head.clone().unwrap_or_default();
            let paths: Vec<String> = vec![top.join("a").to_string_lossy().into_owned(), "src/new.rs".into(), top.join("src/s.rs").to_string_lossy().into_owned(), "target/out".into(), "x.log".into()];
            let who = Ident { name: "Ada".into(), email: "ada@example.com".into() };
            let made = commit(Path::new(&f.top), Some(&head), Some(&paths), "The agent's work\n\nKiln-Agent: test", &who, None).await?.ok_or_else(|| anyhow::anyhow!("no commit"))?;
            let mut files = made.files.clone();
            files.sort();
            assert_eq!(files, vec!["a".to_owned(), "src/new.rs".into(), "src/s.rs".into()], "the person's b, the build output and the ignored log stay out");
            assert_eq!(made.parent.as_deref(), Some(head.as_str()));
            // The same again changes nothing.
            assert!(commit(Path::new(&f.top), Some(&made.sha), Some(&paths), "again", &who, None).await?.is_none(), "nothing more to commit");
            // Everything git sees, as `add -A` takes it (the person's b too; never the ignored).
            let all = commit(Path::new(&f.top), Some(&head), None, "all", &who, None).await?.ok_or_else(|| anyhow::anyhow!("no commit"))?;
            let mut files = all.files;
            files.sort();
            assert_eq!(files, vec!["a".to_owned(), "b".into(), "src/new.rs".into(), "src/s.rs".into()]);
            // A path outside the repository is refused.
            assert!(commit(Path::new(&f.top), Some(&head), Some(&["/etc/hostname".to_owned()]), "x", &who, None).await.is_err());
            anyhow::Ok(())
        })?;
        assert_eq!(status_before, out(&top, &["status", "--porcelain"])?, "the folder's index and working tree are as they were");
        assert_eq!(out(&top, &["log", "--format=%s", "-1"])?, "init", "the branch didn't move");
        Ok(())
    }

    #[test]
    fn a_push_reaches_the_remote_and_says_why_when_it_fails() -> anyhow::Result<()> {
        let d = repo()?;
        let remote = tempfile::tempdir()?;
        sh(remote.path(), "git init -q --bare")?;
        sh(d.path(), "echo z > z")?;
        let (work, bare) = (d.path().to_path_buf(), remote.path().to_path_buf());
        crate::rt::block_on(async move {
            let f = facts(&work).await?;
            let who = Ident { name: "Ada".into(), email: "ada@example.com".into() };
            let made = commit(&work, f.head.as_deref(), Some(&["z".to_owned()]), "z", &who, None).await?.ok_or_else(|| anyhow::anyhow!("no commit"))?;
            let url = bare.to_string_lossy().into_owned();
            push(&work, &url, &format!("+{}:refs/heads/agents/one", made.sha), None).await?;
            assert_eq!(out(&bare, &["rev-parse", "refs/heads/agents/one"])?, made.sha);
            let e = push(&work, "/nonexistent/remote.git", &format!("{}:refs/heads/x", made.sha), Some("kiln_agt_secret")).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(!e.is_empty() && !e.contains("kiln_agt_secret"), "{e}");
            anyhow::Ok(())
        })?;
        Ok(())
    }

    /// An agent in a worktree of its folder's repository (`git worktree add`, Claude Code's
    /// `EnterWorktree`): its files are placed there, by their paths in it, and its facts say the
    /// checkout's branch beside its own.
    #[test]
    fn a_worktree_s_files_are_placed_in_it_and_another_repository_s_are_not() -> anyhow::Result<()> {
        let d = repo()?;
        let top = d.path().to_path_buf();
        sh(&top, "git worktree add -q -b feature .claude/worktrees/feature && echo x > .claude/worktrees/feature/a")?;
        let wt = top.join(".claude/worktrees/feature");
        let other = repo()?;
        assert_eq!(within(&wt.join("a"), &top), Some((wt.clone(), "a".to_owned())), "from the checkout, the worktree's own file");
        assert_eq!(within(&top.join("src/s.rs"), &wt), Some((top.clone(), "src/s.rs".to_owned())), "from the worktree, the checkout's: one repository");
        assert_eq!(within(&wt.join("src/gone.rs"), &top), Some((wt.clone(), "src/gone.rs".to_owned())), "a file that's gone, by its folder");
        assert_eq!(within(&other.path().join("a"), &top), None, "another repository's isn't the agent's");
        assert_eq!(place(&wt).map(|p| p.repo), place(&top).map(|p| p.repo));
        let (w, t) = (wt.clone(), top.clone());
        let (worktree, checkout) = crate::rt::block_on(async move { anyhow::Ok((facts(&w).await?, facts(&t).await?)) })?;
        assert_eq!((worktree.branch.as_deref(), worktree.main_branch.as_deref()), (Some("feature"), Some("main")));
        assert_eq!(checkout.main_branch, None, "the checkout itself is no worktree");
        assert_eq!(
            main_checkout("worktree /w/app\nHEAD abc\nbranch refs/heads/main\n\nworktree /w/app/.claude/worktrees/f\nHEAD def\nbranch refs/heads/f\n"),
            Some(("/w/app".to_owned(), Some("main".to_owned())))
        );
        assert_eq!(main_checkout("worktree /w/app\nHEAD abc\ndetached\n"), Some(("/w/app".to_owned(), None)));
        Ok(())
    }

    #[test]
    fn paths_are_taken_relative_to_the_top() {
        let top = Path::new("/w/app");
        assert_eq!(relative(top, "/w/app/src/a.rs").as_deref(), Some("src/a.rs"));
        assert_eq!(relative(top, "src/a.rs").as_deref(), Some("src/a.rs"));
        assert_eq!(relative(top, "./src/a.rs").as_deref(), Some("src/a.rs"));
        assert_eq!(relative(top, "../b/a.rs"), None);
        assert_eq!(relative(top, "/elsewhere/a.rs"), None);
        assert_eq!(remotes_of("origin\tgit@x:a/b.git (fetch)\norigin\tgit@x:a/b.git (push)\nup\thttps://h/u.git (fetch)\n"), vec![Remote { name: "origin".into(), url: "git@x:a/b.git".into() }, Remote { name: "up".into(), url: "https://h/u.git".into() }]);
    }

    /// A plan run's stack (docs/agentic.md P4): steps' commits each on the one before, then a
    /// step's new version with the steps above it made again; the folder as it was throughout.
    #[test]
    fn a_step_s_new_version_goes_into_its_own_commit_and_the_ones_above_are_made_again() -> anyhow::Result<()> {
        let d = repo()?;
        let top = d.path().to_path_buf();
        let here = top.clone();
        let status = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = std::sync::Arc::clone(&status);
        let look = move |dir: &Path| -> anyhow::Result<()> {
            seen.lock().map_err(|_| anyhow::anyhow!("poisoned"))?.push((out(dir, &["rev-parse", "HEAD"])?, out(dir, &["diff", "--cached", "--name-only"])?));
            Ok(())
        };
        crate::rt::block_on(async move {
            let top = here;
            let who = Ident { name: "Ada".into(), email: "ada@example.com".into() };
            let show = |sha: &str, file: &str| out(&top, &["show", &format!("{sha}:{file}")]);
            let one = |s: &str| std::fs::write(top.join("one.txt"), s);
            // Step 1 writes one.txt, step 2 two.txt, step 3 changes one.txt's last line.
            one("1\n2\n3\n4\n5\n6\n7\n8\n")?;
            let s1 = commit_on(&top, None, &["one.txt".into()], "Step one\n\nChange-Id: I1111111111111111111111111111111111111111\n", &who, None).await?.ok_or_else(|| anyhow::anyhow!("no commit"))?;
            std::fs::write(top.join("two.txt"), "two\n")?;
            let s2 = commit_on(&top, Some(&s1.sha), &["two.txt".into()], "Step two\n", &who, None).await?.ok_or_else(|| anyhow::anyhow!("no commit"))?;
            one("1\n2\n3\n4\n5\n6\n7\nEIGHT\n")?;
            let s3 = commit_on(&top, Some(&s2.sha), &[top.join("one.txt").to_string_lossy().into_owned()], "Step three\n", &who, None).await?.ok_or_else(|| anyhow::anyhow!("no commit"))?;
            assert_eq!((s2.parent.as_deref(), s3.parent.as_deref(), s3.files.clone()), (Some(s1.sha.as_str()), Some(s2.sha.as_str()), vec!["one.txt".to_owned()]));
            assert!(commit_on(&top, Some(&s3.sha), &["one.txt".into()], "again", &who, None).await?.is_none(), "nothing new: no commit");
            assert!(commit_on(&top, Some("0123456789012345678901234567890123456789"), &["one.txt".into()], "x", &who, None).await.is_err(), "a step before that isn't there");
            look(&top)?;
            // Step 1's agent fixes its first line in the folder, which has the whole stack's work.
            one("ONE\n2\n3\n4\n5\n6\n7\nEIGHT\n")?;
            let stack = [s1.sha.clone(), s2.sha.clone(), s3.sha.clone()];
            let made = restack(&top, &stack, &["one.txt".into()], None, &who, None).await?;
            let [m1, m2, m3] = made.as_slice() else { anyhow::bail!("{made:?}") };
            assert_eq!(show(&m1.sha, "one.txt")?, "ONE\n2\n3\n4\n5\n6\n7\n8", "the fix, in step 1's own commit");
            assert_eq!(m1.parent, s1.parent, "on what step 1 was on");
            assert_eq!(out(&top, &["log", "-1", "--format=%B", &m1.sha])?, "Step one\n\nChange-Id: I1111111111111111111111111111111111111111", "its message and Change-Id kept");
            assert_eq!((show(&m2.sha, "two.txt")?, m2.parent.as_deref(), m2.files.clone()), ("two".to_owned(), Some(m1.sha.as_str()), vec!["two.txt".to_owned()]));
            assert_eq!(show(&m3.sha, "one.txt")?, "ONE\n2\n3\n4\n5\n6\n7\nEIGHT", "step 3 on it, the fix kept");
            assert_eq!(out(&top, &["log", "-1", "--format=%s", &m3.sha])?, "Step three");
            // A new message for it.
            let titled = restack(&top, std::slice::from_ref(&m3.sha), &["one.txt".into()], Some("Step three, renamed\n"), &who, None).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(titled.contains("nothing to add"), "{titled}");
            // A fix to what a later step changed can't go into step 1 alone: refused, saying so.
            one("ONE\n2\n3\n4\n5\n6\n7\nEIGHT!\n")?;
            let e = restack(&top, &[m1.sha.clone(), m2.sha.clone(), m3.sha.clone()], &["one.txt".into()], None, &who, None).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(e.contains("changes what a later one changed too"), "{e}");
            // At the top, it goes.
            let top_only = restack(&top, std::slice::from_ref(&m3.sha), &["one.txt".into()], Some("Step three, again\n"), &who, None).await?;
            assert_eq!(top_only.first().map(|m| show(&m.sha, "one.txt")).transpose()?.as_deref(), Some("ONE\n2\n3\n4\n5\n6\n7\nEIGHT!"));
            look(&top)?;
            anyhow::Ok(())
        })?;
        let seen = status.lock().map_err(|_| anyhow::anyhow!("poisoned"))?.clone();
        assert!(seen.len() == 2 && seen.first() == seen.last(), "the folder's branch and index didn't move: {seen:?}");
        assert_eq!(out(&top, &["log", "--format=%s", "-1"])?, "init");
        Ok(())
    }

    /// W27-R03: what a runner commits of an agent's work (its change request, a plan step's
    /// commit, a restack) is signed as the agent, with the key the runner holds for its session at
    /// its epoch; never with a key the person's git names (here a signing program that fails, so
    /// a commit it signed couldn't be made). With no key here (the server's own sessions), it's
    /// unsigned.
    #[test]
    fn w27_r03_an_agents_commit_the_runner_makes_is_signed_as_the_agent() -> anyhow::Result<()> {
        use kiln_ci::identity::chain::{Actor, Identity};
        use kiln_ci::identity::gitsig;
        let d = repo()?;
        let top = d.path().to_path_buf();
        sh(&top, "git config commit.gpgsign true && git config gpg.program false && echo a2 > a")?;
        let keys = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open_scratch(&keys.path().join("pki"))?;
        let session = format!("w27-r03-runner-commit-{}", std::process::id());
        let asked: crate::proto::IdentityAsk = serde_json::from_str(&crate::runner::identity::ask("agent", Some(&session), Some(2))?)?;
        let identity = Identity::new(pki.organization(), Actor::Agent { person: "sam".into(), agent: session.clone(), epoch: 2, held_by: None });
        let pem = pki.issue_identity(&asked.csr, &identity, &[], time::Duration::hours(1))?;
        let given = crate::proto::IdentityGiven { kind: asked.kind, of: asked.of, epoch: asked.epoch, uri: identity.uri()?, cert_pem: Some(pem), error: None, delegation: None };
        crate::runner::identity::given(given, keys.path(), "main")?;
        let slot = crate::runner::identity::slot("agent", Some(&session), Some(2));
        let here = top.clone();
        let (signed, plain) = crate::rt::block_on(async move {
            let f = facts(&here).await?;
            let who = Ident { name: "Ada".into(), email: "ada@example.com".into() };
            let signed = commit(Path::new(&f.top), f.head.as_deref(), Some(&["a".to_owned()]), "The agent's", &who, Some(&slot)).await?.ok_or_else(|| anyhow::anyhow!("no commit"))?;
            let plain = commit(Path::new(&f.top), f.head.as_deref(), None, "All of it", &who, None).await?.ok_or_else(|| anyhow::anyhow!("no commit"))?;
            crate::runner::identity::forget(&slot);
            anyhow::Ok((signed, plain))
        })?;
        let raw = Command::new("git").args(["cat-file", "commit", &signed.sha]).current_dir(&top).output()?.stdout;
        let sig = gitsig::signature_of(&raw).ok_or_else(|| anyhow::anyhow!("not signed: {}", String::from_utf8_lossy(&raw)))?;
        let v = gitsig::verify(&gitsig::payload(&raw), &sig, &pki.ca_der, &[], crate::util::now_unix())?;
        assert_eq!(v.identity, identity, "as the agent, at its epoch");
        let raw = Command::new("git").args(["cat-file", "commit", &plain.sha]).current_dir(&top).output()?.stdout;
        assert!(gitsig::signature_of(&raw).is_none(), "no key here: unsigned, and not with the person's");
        Ok(())
    }

    /// A commit of `file` saying `text`, made at `at` (unix s), its id.
    fn commit_at(top: &Path, file: &str, text: &str, subject: &str, at: i64) -> anyhow::Result<String> {
        sh(top, &format!("printf '{text}' > {file} && git add {file} && GIT_AUTHOR_DATE='@{at} +0000' GIT_COMMITTER_DATE='@{at} +0000' git commit -qm '{subject}'"))?;
        out(top, &["rev-parse", "HEAD"])
    }

    /// The Changes tab's Commits (the Changes panel plan 01a194444b1d, step 4): a branch with commits
    /// before and after its session began marks only the session's as its own, by the commit it began
    /// at, else by when it began; counted from its trunk, only the branch's own are listed.
    #[test]
    fn log_marks_only_the_sessions_commits_as_its_own() -> anyhow::Result<()> {
        let d = repo()?;
        let top = d.path().to_path_buf();
        let before = commit_at(&top, "a", "a2\\n", "before it began", 2_000)?;
        let began = out(&top, &["rev-parse", "HEAD"])?;
        let s1 = commit_at(&top, "b", "b2\\n", "its first", 3_000)?;
        let s2 = commit_at(&top, "src/s.rs", "s2\\n", "its second", 4_000)?;
        sh(&top, "echo new > new.txt && echo a3 > a")?;
        crate::rt::block_on(async move {
            let marked = |l: &Log| l.commits.iter().map(|c| (c.sha.clone(), c.session)).collect::<Vec<_>>();
            // By the commit it began at; on its trunk itself, its latest are listed.
            let l = log(&top, None, None, Some(&began), None).await?;
            assert_eq!((l.told.as_str(), l.trunk.as_deref()), ("head", None));
            assert_eq!(marked(&l).get(..3).map(<[_]>::to_vec), Some(vec![(s2.clone(), true), (s1.clone(), true), (before.clone(), false)]));
            assert_eq!(l.commits.first().map(|c| (c.subject.as_str(), c.files, c.added, c.removed, c.at)), Some(("its second", 1, 1, 1, 4_000_000)));
            // What isn't committed yet: a file changed and one new (untracked).
            assert_eq!(l.dirty, Some(Counts { files: 2, added: 2, removed: 1 }));
            // Without that commit, by when it began.
            let l = log(&top, None, None, None, Some(2_500_000)).await?;
            assert_eq!(l.told, "time");
            assert_eq!(marked(&l).get(..3).map(<[_]>::to_vec), Some(vec![(s2.clone(), true), (s1.clone(), true), (before.clone(), false)]));
            // A branch counted from its trunk: only its own.
            sh(&top, "git stash -u -q && git checkout -q -b feature")?;
            let f1 = commit_at(&top, "c", "c\\n", "on the branch", 5_000)?;
            let l = log(&top, None, Some("main"), Some(&s2), None).await?;
            assert_eq!((l.trunk.as_deref(), l.base.as_deref(), marked(&l)), (Some("main"), Some(s2.as_str()), vec![(f1, true)]));
            assert_eq!(l.dirty, None);
            anyhow::Ok(())
        })
    }

    /// Picked commits, together: a run of them is `git diff <first>^ <last>`; with one left out
    /// between, each goes on in order; one that needs the one left out says which.
    #[test]
    fn picked_commits_go_together_in_order_or_name_the_commit_they_need() -> anyhow::Result<()> {
        let d = repo()?;
        let top = d.path().to_path_buf();
        let one = commit_at(&top, "x.txt", "1\\n2\\n3\\n", "one", 2_000)?;
        let two = commit_at(&top, "y.txt", "y\\n", "two", 3_000)?;
        let three = commit_at(&top, "x.txt", "1\\n2\\nTHREE\\n", "three", 4_000)?;
        let four = commit_at(&top, "y.txt", "Y\\n", "four", 5_000)?;
        let numstat = |from: &str, to: &str| -> anyhow::Result<Vec<(String, u32, u32)>> {
            Ok(out(&top, &["diff", "--numstat", from, to])?.lines().filter_map(|l| {
                let mut c = l.split('\t');
                let (a, r, p) = (c.next()?.parse().ok()?, c.next()?.parse().ok()?, c.next()?.to_owned());
                Some((p, a, r))
            }).collect())
        };
        let want_run = numstat(&format!("{two}^"), &three)?;
        let here = top.clone();
        let (one2, two2, three2, four2) = (one.clone(), two.clone(), three.clone(), four.clone());
        crate::rt::block_on(async move {
            let top = here;
            let counted = |p: &Picked| p.files.iter().map(|f| (f.path.clone(), f.added, f.removed)).collect::<Vec<_>>();
            // A run: what git diff says from the first's parent to the last.
            let run = picked(&top, None, &[three2.clone(), two2.clone()]).await?;
            assert_eq!((run.from.clone(), counted(&run), run.needs.clone()), (one2.clone(), want_run, None));
            // One and three, two left out: each goes on in order.
            let gap = picked(&top, None, &[one2.clone(), three2.clone()]).await?;
            assert_eq!((counted(&gap), gap.needs.clone()), (vec![("x.txt".to_owned(), 3, 0)], None));
            // Four needs two, which isn't picked: it says so.
            let needs = picked(&top, None, &[one2.clone(), four2.clone()]).await?;
            assert_eq!(needs.needs.map(|n| (n.pick, n.commit, n.subject)), Some((four2.clone(), Some(two2.clone()), Some("two".to_owned()))));
            // What isn't committed yet, after the last commit.
            sh(&top, "printf 'Y\\nmore\\n' > y.txt")?;
            let now = picked(&top, None, &[four2, NOW.to_owned()]).await?;
            assert_eq!(counted(&now), vec![("y.txt".to_owned(), 2, 1)]);
            // A commit not on the branch's line is refused.
            assert!(picked(&top, None, &["0123456789abcdef".to_owned()]).await.is_err());
            anyhow::Ok(())
        })
    }

    /// Compare (the Changes panel plan 01a194444b1d, step 5): between branches that both moved since
    /// they split, from where they split only what To adds; without, every difference.
    #[test]
    fn compare_between_branches_that_both_moved_splits_or_shows_every_difference() -> anyhow::Result<()> {
        let d = repo()?;
        let top = d.path().to_path_buf();
        sh(&top, "git checkout -q -b feature")?;
        commit_at(&top, "f.txt", "f\\n", "on the branch", 2_000)?;
        sh(&top, "git checkout -q main")?;
        commit_at(&top, "m.txt", "m\\n", "on main", 3_000)?;
        crate::rt::block_on(async move {
            let name = |n: &str| Point::Ref { name: n.to_owned() };
            let files = |c: &Compared| c.files.iter().map(|f| (f.path.clone(), f.created, f.deleted)).collect::<Vec<_>>();
            let only = compare(&top, &name("main"), &name("feature"), true, None, None, &Versions::new()).await?;
            assert_eq!((files(&only), only.split), (vec![("f.txt".to_owned(), true, false)], true));
            let every = compare(&top, &name("main"), &name("feature"), false, None, None, &Versions::new()).await?;
            assert_eq!((files(&every), every.split), (vec![("f.txt".to_owned(), true, false), ("m.txt".to_owned(), false, true)], false));
            // Now: the folder as it is, a new untracked file too.
            sh(&top, "echo n > n.txt")?;
            let now = compare(&top, &name("main"), &Point::Now, true, None, None, &Versions::new()).await?;
            assert_eq!(files(&now), vec![("n.txt".to_owned(), true, false)]);
            anyhow::Ok(())
        })
    }

    /// Two agents' versions, each its files as it left them over how the folder was as the session
    /// began: compared From → To plainly, each file saying which of them changed it; turned around,
    /// what one added reads as deleted.
    #[test]
    fn two_agents_versions_compare_and_name_who_changed_each_file() -> anyhow::Result<()> {
        let d = repo()?;
        let top = d.path().to_path_buf();
        let began = out(&top, &["rev-parse", "HEAD"])?;
        let at = |p: &str| top.join(p).to_string_lossy().into_owned();
        let mut versions = Versions::new();
        versions.insert("toolu_x".into(), vec![LeftFile { path: "a".into(), abs: at("a"), left: Left::Text("x's a\n".into()) }, LeftFile { path: "new.txt".into(), abs: at("new.txt"), left: Left::Text("x's own\n".into()) }]);
        versions.insert("toolu_y".into(), vec![LeftFile { path: "a".into(), abs: at("a"), left: Left::Text("y's a\n".into()) }, LeftFile { path: "b".into(), abs: at("b"), left: Left::Absent }]);
        crate::rt::block_on(async move {
            let (x, y) = (Point::Agent { by: "toolu_x".into() }, Point::Agent { by: "toolu_y".into() });
            let c = compare(&top, &x, &y, true, None, Some(&began), &versions).await?;
            let seen = |c: &Compared| c.files.iter().map(|f| (f.path.clone(), f.created, f.deleted, c.changed_by.get(&f.path).cloned().unwrap_or_default())).collect::<Vec<_>>();
            assert!(!c.split, "two agents' versions compare plainly");
            assert_eq!(
                seen(&c),
                vec![
                    ("a".to_owned(), false, false, vec!["toolu_x".to_owned(), "toolu_y".to_owned()]),
                    ("b".to_owned(), false, true, vec!["toolu_y".to_owned()]),
                    ("new.txt".to_owned(), false, true, vec!["toolu_x".to_owned()]),
                ]
            );
            // Swapped: the file only x added reads as added now, b as y's deletion undone.
            let back = compare(&top, &y, &x, true, None, Some(&began), &versions).await?;
            assert_eq!(back.files.iter().map(|f| (f.path.clone(), f.created, f.deleted)).collect::<Vec<_>>(), vec![("a".to_owned(), false, false), ("b".to_owned(), true, false), ("new.txt".to_owned(), true, false)]);
            // An agent whose versions this machine doesn't keep is refused, saying why.
            assert!(compare(&top, &Point::Agent { by: "toolu_z".into() }, &Point::Now, true, None, Some(&began), &versions).await.is_err());
            anyhow::Ok(())
        })
    }
}
