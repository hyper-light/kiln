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

fn git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir);
    // Never a prompt: a question nobody sees would hang the call until its timeout.
    c.env("GIT_TERMINAL_PROMPT", "0");
    c
}

/// Runs `c`, its stdout trimmed; an error carries what git said.
async fn run(c: Command, timeout: Duration) -> anyhow::Result<String> {
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
    Ok(Facts { top, head, branch, remotes, default_branch })
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
/// they are in the working tree, on `parent` (`None`: a first commit), by `who`, saying `message`.
/// `Ok(None)` when that changes nothing. Ignored files among `paths` are left out, as git would
/// leave them out; files that are gone are deleted.
pub async fn commit(top: &Path, parent: Option<&str>, paths: Option<&[String]>, message: &str, who: &Ident) -> anyhow::Result<Option<Made>> {
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
    let mut c = git(top);
    c.args(["commit-tree", &tree, "-F", "-"]);
    if let Some(p) = parent {
        c.args(["-p", p]);
    }
    c.env("GIT_AUTHOR_NAME", &who.name).env("GIT_AUTHOR_EMAIL", &who.email).env("GIT_COMMITTER_NAME", &who.name).env("GIT_COMMITTER_EMAIL", &who.email);
    let sha = with_stdin(c, message.as_bytes()).await?;
    let mut c = git(top);
    match parent {
        Some(p) => c.args(["diff-tree", "--no-commit-id", "--name-only", "-r", "-z", p, &sha]),
        None => c.args(["diff-tree", "--no-commit-id", "--name-only", "-r", "-z", "--root", &sha]),
    };
    let files = run(c, GIT_TIMEOUT).await?.split('\0').filter(|s| !s.is_empty()).map(str::to_owned).collect();
    Ok(Some(Made { sha, parent: parent.map(str::to_owned), files }))
}

/// Runs `c` with `input` on its stdin; its stdout trimmed.
async fn with_stdin(mut c: Command, input: &[u8]) -> anyhow::Result<String> {
    use std::io::Write as _;
    use std::process::Stdio;
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
        anyhow::bail!("git commit-tree: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
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
            "git init -q -b main && git config user.name Ada && git config user.email ada@example.com && echo a > a && echo b > b && mkdir -p src && echo s > src/s.rs && printf 'target/\\n*.log\\n' > .gitignore && git add . && git commit -qm init && git remote add origin git@github.com:acme/app.git",
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
            let made = commit(Path::new(&f.top), Some(&head), Some(&paths), "The agent's work\n\nKiln-Agent: test", &who).await?.ok_or_else(|| anyhow::anyhow!("no commit"))?;
            let mut files = made.files.clone();
            files.sort();
            assert_eq!(files, vec!["a".to_owned(), "src/new.rs".into(), "src/s.rs".into()], "the person's b, the build output and the ignored log stay out");
            assert_eq!(made.parent.as_deref(), Some(head.as_str()));
            // The same again changes nothing.
            assert!(commit(Path::new(&f.top), Some(&made.sha), Some(&paths), "again", &who).await?.is_none(), "nothing more to commit");
            // Everything git sees, as `add -A` takes it (the person's b too; never the ignored).
            let all = commit(Path::new(&f.top), Some(&head), None, "all", &who).await?.ok_or_else(|| anyhow::anyhow!("no commit"))?;
            let mut files = all.files;
            files.sort();
            assert_eq!(files, vec!["a".to_owned(), "b".into(), "src/new.rs".into(), "src/s.rs".into()]);
            // A path outside the repository is refused.
            assert!(commit(Path::new(&f.top), Some(&head), Some(&["/etc/hostname".to_owned()]), "x", &who).await.is_err());
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
            let made = commit(&work, f.head.as_deref(), Some(&["z".to_owned()]), "z", &who).await?.ok_or_else(|| anyhow::anyhow!("no commit"))?;
            let url = bare.to_string_lossy().into_owned();
            push(&work, &url, &format!("+{}:refs/heads/agents/one", made.sha), None).await?;
            assert_eq!(out(&bare, &["rev-parse", "refs/heads/agents/one"])?, made.sha);
            let e = push(&work, "/nonexistent/remote.git", &format!("{}:refs/heads/x", made.sha), Some("kiln_agt_secret")).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(!e.is_empty() && !e.contains("kiln_agt_secret"), "{e}");
            anyhow::Ok(())
        })?;
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
}
