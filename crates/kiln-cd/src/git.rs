//! Git for deployments, through the `git` binary kiln already uses for CI.
//!
//! A source is read from a bare mirror ([`Mirror`]): kiln resolves its revision to a commit,
//! asks for the hashes of the paths its render reads (one `cat-file --batch-check` for any
//! number of them), and extracts exactly those paths when it renders. A path's hash is the
//! address of what is under it, so two commits that leave an app's paths alone give its render
//! the same key and the app is not rendered again.
//!
//! Everything here blocks; the server calls it on its blocking pool.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};

/// How to authenticate to a remote.
#[derive(Clone, Debug, Default)]
pub struct Auth {
    /// An `Authorization` header value (`Basic …`, `Bearer …`) for HTTPS remotes.
    pub header: Option<String>,
    /// A private key file for SSH remotes.
    pub ssh_key: Option<PathBuf>,
    /// Skip TLS verification (Argo CD's `insecure`).
    pub insecure: bool,
}

/// A bare mirror of a remote.
#[derive(Clone, Debug)]
pub struct Mirror {
    /// The mirror's directory.
    pub dir: PathBuf,
    /// The remote's URL.
    pub url: String,
    auth: Auth,
}

/// One entry of a tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Path relative to the repository root.
    pub path: String,
    /// Whether it is a directory.
    pub tree: bool,
    /// Whether it is a symbolic link (its blob is the link's target).
    pub link: bool,
    /// Its object hash.
    pub hash: String,
}

/// A commit as people read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    /// Its hash.
    pub sha: String,
    /// Its author's name.
    pub author: String,
    /// Its author's email.
    pub email: String,
    /// When it was authored, seconds since the epoch.
    pub at: i64,
    /// Its message's first line.
    pub subject: String,
    /// The rest of its message.
    pub body: String,
}

fn git(dir: Option<&Path>, auth: &Auth) -> Command {
    let mut c = Command::new("git");
    c.env("GIT_TERMINAL_PROMPT", "0").env("GIT_ASKPASS", "true").env("LC_ALL", "C");
    if let Some(d) = dir {
        c.arg("--git-dir").arg(d);
    }
    if let Some(h) = &auth.header {
        c.arg("-c").arg(format!("http.extraHeader=Authorization: {h}"));
    }
    if auth.insecure {
        c.arg("-c").arg("http.sslVerify=false");
    }
    if let Some(k) = &auth.ssh_key {
        c.env("GIT_SSH_COMMAND", format!("ssh -i {} -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new", k.display()));
    }
    c
}

fn run(mut c: Command) -> Result<Vec<u8>> {
    let out = c.stdin(Stdio::null()).output().context("running git")?;
    if !out.status.success() {
        bail!("git failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).trim().to_owned()
}

/// The object path git takes for `path` at `commit` (`<commit>^{tree}` for the root).
fn spec(commit: &str, path: &str) -> String {
    let p = path.trim_matches('/');
    if p.is_empty() || p == "." { format!("{commit}^{{tree}}") } else { format!("{commit}:{p}") }
}

impl Mirror {
    /// The mirror of `url` in `dir`, cloned if it isn't there yet.
    pub fn open(dir: &Path, url: &str, auth: Auth) -> Result<Mirror> {
        let m = Mirror { dir: dir.to_owned(), url: url.to_owned(), auth };
        if !dir.join("HEAD").exists() {
            if let Some(parent) = dir.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut c = git(None, &m.auth);
            c.args(["clone", "--mirror", "--quiet", url]).arg(dir);
            run(c).with_context(|| format!("cloning {url}"))?;
        }
        Ok(m)
    }

    /// Fetches everything the remote has.
    pub fn fetch(&self) -> Result<()> {
        let mut c = git(Some(&self.dir), &self.auth);
        c.args(["fetch", "--prune", "--quiet", "--force", "origin", "+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"]);
        run(c).with_context(|| format!("fetching {}", self.url)).map(|_| ())
    }

    /// The commit a revision names: a branch, a tag, `HEAD`, or a (short) commit hash.
    pub fn resolve(&self, revision: &str) -> Result<String> {
        let rev = if revision.is_empty() { "HEAD" } else { revision };
        for candidate in [format!("refs/heads/{rev}"), format!("refs/tags/{rev}"), rev.to_owned()] {
            let mut c = git(Some(&self.dir), &self.auth);
            c.args(["rev-parse", "--verify", "--quiet", &format!("{candidate}^{{commit}}")]);
            if let Ok(out) = run(c) {
                let sha = text(out);
                if !sha.is_empty() {
                    return Ok(sha);
                }
            }
        }
        bail!("{} has no revision {rev:?}", self.url)
    }

    /// Who wrote a commit, when, and what it says.
    pub fn commit(&self, commit: &str) -> Result<Commit> {
        let mut c = git(Some(&self.dir), &self.auth);
        c.args(["show", "--no-patch", "--format=%H%x00%an%x00%ae%x00%at%x00%s%x00%b", &format!("{commit}^{{commit}}"), "--"]);
        let out = String::from_utf8_lossy(&run(c)?).into_owned();
        let mut f = out.splitn(6, '\0');
        let mut next = || f.next().unwrap_or("").to_owned();
        let (sha, author, email, at, subject, body) = (next(), next(), next(), next(), next(), next());
        if sha.trim().is_empty() {
            bail!("{} has no commit {commit}", self.url);
        }
        Ok(Commit { sha: sha.trim().to_owned(), author, email, at: at.trim().parse().unwrap_or(0), subject, body: body.trim().to_owned() })
    }

    /// The hash of each path at `commit` (`None` where nothing is there), in one call.
    pub fn hashes(&self, commit: &str, paths: &[&str]) -> Result<Vec<Option<String>>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut c = git(Some(&self.dir), &self.auth);
        c.args(["cat-file", "--batch-check=%(objectname) %(objecttype)"]);
        let mut child = c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().context("running git")?;
        let mut input = String::new();
        for p in paths {
            input.push_str(&spec(commit, p));
            input.push('\n');
        }
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input.as_bytes())?;
        }
        let mut out = String::new();
        if let Some(mut stdout) = child.stdout.take() {
            stdout.read_to_string(&mut out)?;
        }
        child.wait()?;
        Ok(out
            .lines()
            .map(|l| {
                let mut it = l.split_whitespace();
                match (it.next(), it.next()) {
                    (Some(hash), Some("tree" | "blob")) => Some(hash.to_owned()),
                    _ => None,
                }
            })
            .collect())
    }

    /// A file's contents at `commit`.
    pub fn read(&self, commit: &str, path: &str) -> Result<Option<Vec<u8>>> {
        let mut c = git(Some(&self.dir), &self.auth);
        c.args(["cat-file", "blob", &spec(commit, path)]);
        let out = c.stdin(Stdio::null()).output().context("running git")?;
        Ok(out.status.success().then_some(out.stdout))
    }

    /// The entries under `dir` at `commit` (recursively when `recurse`).
    pub fn list(&self, commit: &str, dir: &str, recurse: bool) -> Result<Vec<Entry>> {
        let mut c = git(Some(&self.dir), &self.auth);
        c.args(["ls-tree", "-z", "--full-tree"]);
        if recurse {
            c.arg("-r");
        }
        c.arg(commit);
        let d = dir.trim_matches('/');
        if !d.is_empty() && d != "." {
            c.arg("--").arg(format!("{d}/"));
        }
        let out = run(c)?;
        Ok(out
            .split(|b| *b == 0)
            .filter(|r| !r.is_empty())
            .filter_map(|record| {
                let line = String::from_utf8_lossy(record);
                let (meta, path) = line.split_once('\t')?;
                let mut it = meta.split_whitespace();
                let (mode, kind, hash) = (it.next()?, it.next()?, it.next()?);
                Some(Entry { path: path.to_owned(), tree: kind == "tree", link: mode == "120000", hash: hash.to_owned() })
            })
            .collect())
    }

    /// `paths` plus the targets of the symbolic links under them, followed until nothing new
    /// is reached: a render that reads through a link reads its target, so the target is part of
    /// what the render's key covers. A link out of the repository is refused, as Argo CD refuses
    /// it.
    pub fn with_links(&self, commit: &str, paths: &[String]) -> Result<Vec<String>> {
        let mut all: Vec<String> = paths.to_vec();
        let mut todo: Vec<String> = paths.to_vec();
        let mut rounds = 0;
        while let Some(p) = todo.pop() {
            rounds += 1;
            if rounds > 4096 {
                bail!("too many symbolic links under {}", paths.join(", "));
            }
            let entries = self.list_path(commit, &p)?;
            for e in entries.into_iter().filter(|e| e.link) {
                let target = self.read(commit, &e.path)?.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
                let dir = e.path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                let Some(resolved) = crate::render::join(dir, target.trim()) else {
                    bail!("{} links outside the repository ({target})", e.path);
                };
                if !all.iter().any(|a| covers(a, &resolved)) {
                    all.push(resolved.clone());
                    todo.push(resolved);
                }
            }
        }
        all.sort();
        all.dedup();
        Ok(all)
    }

    /// The entries at or under `path`: the path itself if it is a file or link, else everything
    /// beneath it.
    fn list_path(&self, commit: &str, path: &str) -> Result<Vec<Entry>> {
        let p = path.trim_matches('/');
        let mut c = git(Some(&self.dir), &self.auth);
        c.args(["ls-tree", "-r", "-z", "--full-tree", commit]);
        if !p.is_empty() && p != "." {
            c.arg("--").arg(p);
        }
        let out = run(c)?;
        Ok(out
            .split(|b| *b == 0)
            .filter(|r| !r.is_empty())
            .filter_map(|record| {
                let line = String::from_utf8_lossy(record);
                let (meta, path) = line.split_once('\t')?;
                let mut it = meta.split_whitespace();
                let (mode, kind, hash) = (it.next()?, it.next()?, it.next()?);
                Some(Entry { path: path.to_owned(), tree: kind == "tree", link: mode == "120000", hash: hash.to_owned() })
            })
            .collect())
    }

    /// The paths that differ between two commits.
    pub fn changed(&self, from: &str, to: &str) -> Result<Vec<String>> {
        let mut c = git(Some(&self.dir), &self.auth);
        c.args(["diff-tree", "-r", "-z", "--name-only", "--no-commit-id", from, to]);
        let out = run(c)?;
        Ok(out.split(|b| *b == 0).filter(|r| !r.is_empty()).map(|r| String::from_utf8_lossy(r).into_owned()).collect())
    }

    /// Writes the files under `paths` at `commit` into `dest`, keeping their repository layout.
    /// Paths that don't exist are skipped.
    pub fn extract(&self, commit: &str, paths: &[&str], dest: &Path) -> Result<()> {
        let hashes = self.hashes(commit, paths)?;
        let present: Vec<&str> = paths.iter().zip(&hashes).filter(|(_, h)| h.is_some()).map(|(p, _)| *p).collect();
        std::fs::create_dir_all(dest)?;
        if present.is_empty() {
            return Ok(());
        }
        let mut c = git(Some(&self.dir), &self.auth);
        c.args(["archive", "--format=tar", commit]);
        if !present.iter().any(|p| p.trim_matches('/').is_empty() || *p == ".") {
            c.arg("--");
            for p in &present {
                c.arg(p.trim_matches('/'));
            }
        }
        let mut child = c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("running git archive")?;
        let stdout = child.stdout.take().context("git archive has no output")?;
        let mut archive = tar::Archive::new(stdout);
        archive.set_preserve_permissions(false);
        archive.unpack(dest).context("unpacking git archive")?;
        let status = child.wait()?;
        if !status.success() {
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                e.read_to_string(&mut err).ok();
            }
            bail!("git archive failed: {}", err.trim());
        }
        Ok(())
    }
}

/// Whether `dir` is `path` or one of its parents.
fn covers(dir: &str, path: &str) -> bool {
    let d = dir.trim_matches('/');
    d.is_empty() || path == d || path.strip_prefix(d).is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) -> Result<String> {
        let out = Command::new("git").current_dir(dir).args(args).env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").output()?;
        if !out.status.success() {
            bail!("{}", String::from_utf8_lossy(&out.stderr));
        }
        Ok(text(out.stdout))
    }

    #[test]
    fn a_commit_says_who_when_and_what() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work)?;
        sh(&work, &["init", "-q", "-b", "main"])?;
        std::fs::write(work.join("a.yaml"), "kind: ConfigMap\n")?;
        sh(&work, &["add", "."])?;
        sh(&work, &["commit", "-qm", "Fix the probe\n\nIt timed out on cold starts."])?;
        let head = sh(&work, &["rev-parse", "HEAD"])?;
        let m = Mirror::open(&tmp.path().join("mirror.git"), work.to_str().unwrap_or(""), Auth::default())?;
        let c = m.commit("main")?;
        assert_eq!(c.sha, head);
        assert_eq!((c.author.as_str(), c.email.as_str()), ("t", "t@t"));
        assert_eq!(c.subject, "Fix the probe");
        assert_eq!(c.body, "It timed out on cold starts.");
        assert!(c.at > 1_600_000_000);
        assert!(m.commit("0000000000000000000000000000000000000000").is_err());
        Ok(())
    }

    #[test]
    fn paths_hash_by_content_and_extract_with_their_layout() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let work = tmp.path().join("work");
        std::fs::create_dir_all(work.join("deploy/web"))?;
        std::fs::create_dir_all(work.join("deploy/api"))?;
        sh(&work, &["init", "-q", "-b", "main"])?;
        std::fs::write(work.join("deploy/web/cm.yaml"), "kind: ConfigMap\n")?;
        std::fs::write(work.join("deploy/api/cm.yaml"), "kind: ConfigMap\n")?;
        sh(&work, &["add", "."])?;
        sh(&work, &["commit", "-qm", "one"])?;
        let one = sh(&work, &["rev-parse", "HEAD"])?;
        std::fs::write(work.join("deploy/api/cm.yaml"), "kind: Secret\n")?;
        sh(&work, &["commit", "-qam", "two"])?;
        let two = sh(&work, &["rev-parse", "HEAD"])?;

        let m = Mirror::open(&tmp.path().join("mirror.git"), work.to_str().unwrap_or(""), Auth::default())?;
        assert_eq!(m.resolve("main")?, two);
        let h1 = m.hashes(&one, &["deploy/web", "deploy/api", "nope"])?;
        let h2 = m.hashes(&two, &["deploy/web", "deploy/api", "nope"])?;
        assert_eq!(h1.first(), h2.first(), "web didn't change");
        assert_ne!(h1.get(1), h2.get(1), "api did");
        assert_eq!(h1.get(2), Some(&None));
        assert_eq!(m.changed(&one, &two)?, ["deploy/api/cm.yaml"]);
        assert_eq!(m.read(&two, "deploy/api/cm.yaml")?.as_deref(), Some(&b"kind: Secret\n"[..]));
        let out = tmp.path().join("out");
        m.extract(&two, &["deploy/web", "missing"], &out)?;
        assert!(out.join("deploy/web/cm.yaml").exists());
        assert!(!out.join("deploy/api").exists());
        let entries = m.list(&two, "deploy", true)?;
        assert_eq!(entries.iter().filter(|e| !e.tree).count(), 2);
        Ok(())
    }
}
