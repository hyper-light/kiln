//! What a session's git repository holds besides its files, carried when the session moves to
//! another machine (`super::moving`). Repo sync keeps the working tree in step, never `.git`, so
//! this carries the rest: the branches (commits not pushed anywhere included), the tags, the stash,
//! what is staged, and where HEAD is. Only what the other machine lacks travels: a bundle of the
//! commits not reachable from its own tips, which it says before the move.
//!
//! - Taken ([`capture`]) on the machine the session leaves, once its folder's last changes are in
//!   the sync log, and refused while a merge, rebase, cherry-pick, revert or bisect is under way:
//!   that state lives in `.git` alone, and would be left behind.
//! - Put in place ([`restore`]) on the machine it goes to, before it goes on: the bundle's
//!   commits fetched, the refs set, the stash entries stored, the index read from what was staged,
//!   HEAD pointed where it was. No file of the working tree is touched (sync brings those), and
//!   nothing there is lost: a branch that has other commits there keeps them, beside the one that
//!   came (`<name>.before-move` for the branch HEAD is on, `<name>.from-<machine>` for others).
//!
//! Blocking: git does the work, and never prompts.

use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// The most a bundle may be: more doesn't move (push them first, and it goes).
const MAX_BUNDLE: u64 = 128 << 20;
/// Where the refs a bundle needs are made for a moment, and where one that came is fetched to.
const OUT: &str = "refs/kiln/move";
const IN: &str = "refs/kiln/moved";

/// A ref: its name (`main`, `v1.0`) and what it points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ref {
    pub name: String,
    pub sha: String,
}

/// Where HEAD is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "at", content = "to")]
pub enum Head {
    /// On a branch.
    Branch(String),
    /// On a branch with no commit yet.
    Unborn(String),
    /// At a commit, on no branch.
    Detached(String),
}

/// A repository's state besides its files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitState {
    pub head: Head,
    pub branches: Vec<Ref>,
    pub tags: Vec<Ref>,
    /// Newest first, as `git stash list` says them: each one's commit, and its message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stash: Vec<Ref>,
    /// What was staged, when it differed from HEAD: a commit whose tree is the index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<String>,
    /// The commits the other machine lacks, as a git bundle (base64); none when it lacked none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
}

/// Runs git in `dir`: its output, trimmed; an error says what git said.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "kiln")
        .env("GIT_AUTHOR_EMAIL", "kiln@localhost")
        .env("GIT_COMMITTER_NAME", "kiln")
        .env("GIT_COMMITTER_EMAIL", "kiln@localhost")
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_owned())
    } else {
        Err(format!("git {}: {}", args.first().copied().unwrap_or(""), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// `git` that may fail: whether it did what it was asked.
fn git_ok(dir: &Path, args: &[&str]) -> bool {
    git(dir, args).is_ok()
}

/// The repository `cwd` is in: its top folder, or none.
pub fn root_of(cwd: &Path) -> Option<PathBuf> {
    git(cwd, &["rev-parse", "--show-toplevel"]).ok().filter(|r| !r.is_empty()).map(PathBuf::from)
}

/// What's under way in `root` that lives in `.git` alone, if anything.
pub fn under_way(root: &Path) -> Option<&'static str> {
    let dir = PathBuf::from(git(root, &["rev-parse", "--absolute-git-dir"]).ok()?);
    let common = git(root, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).map(PathBuf::from).unwrap_or_else(|_| dir.clone());
    [
        (dir.join("MERGE_HEAD"), "a merge"),
        (dir.join("rebase-merge"), "a rebase"),
        (dir.join("rebase-apply"), "a rebase"),
        (dir.join("CHERRY_PICK_HEAD"), "a cherry-pick"),
        (dir.join("REVERT_HEAD"), "a revert"),
        (dir.join("BISECT_LOG"), "a bisect"),
        (common.join("BISECT_LOG"), "a bisect"),
    ]
    .into_iter()
    .find(|(p, _)| p.exists())
    .map(|(_, what)| what)
}

fn refs(root: &Path, prefix: &str) -> Vec<Ref> {
    let out = git(root, &["for-each-ref", "--format=%(refname) %(objectname)", prefix]).unwrap_or_default();
    let strip = format!("{prefix}/");
    out.lines().filter_map(|l| l.split_once(' ')).filter_map(|(r, sha)| Some(Ref { name: r.strip_prefix(&strip)?.to_owned(), sha: sha.to_owned() })).collect()
}

/// The commits and tags `root` holds at the tips of its refs, and HEAD: what a machine says it
/// has before a session moves to it.
pub fn tips(root: &Path) -> Vec<String> {
    let mut out: Vec<String> = git(root, &["for-each-ref", "--format=%(objectname)"]).unwrap_or_default().lines().map(str::to_owned).collect();
    if let Ok(h) = git(root, &["rev-parse", "-q", "--verify", "HEAD"]) {
        out.push(h);
    }
    out.sort();
    out.dedup();
    out
}

/// The tips of `root`'s remote-tracking branches: what the other machine likely has, when it can't
/// say (a copy of the repository has what was pushed).
pub fn pushed(root: &Path) -> Vec<String> {
    git(root, &["for-each-ref", "--format=%(objectname)", "refs/remotes"]).unwrap_or_default().lines().map(str::to_owned).collect()
}

/// The repository at `root` (the session's folder there) besides its files, with what `theirs`
/// (the other machine's tips, [`tips`]) lacks as a bundle. An error says why it can't be carried.
pub fn capture(root: &Path, theirs: &[String]) -> Result<GitState, String> {
    if let Some(what) = under_way(root) {
        return Err(format!("{what} is under way in {} (git keeps it in .git, which doesn't move): finish it or abort it there, and it moves", root.display()));
    }
    let head = match git(root, &["symbolic-ref", "-q", "HEAD"]) {
        Ok(r) => {
            let b = r.strip_prefix("refs/heads/").unwrap_or(&r).to_owned();
            if git_ok(root, &["rev-parse", "-q", "--verify", "HEAD"]) { Head::Branch(b) } else { Head::Unborn(b) }
        }
        Err(_) => Head::Detached(git(root, &["rev-parse", "HEAD"])?),
    };
    let branches = refs(root, "refs/heads");
    let tags = refs(root, "refs/tags");
    let stash: Vec<Ref> = git(root, &["stash", "list", "--format=%H%x09%gs"])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(sha, msg)| Ref { name: msg.to_owned(), sha: sha.to_owned() })
        .collect();
    // What's staged, when it differs from HEAD's tree: kept as a commit, so it can travel.
    let born = !matches!(head, Head::Unborn(_));
    let staged = if born { !git_ok(root, &["diff", "--cached", "--quiet"]) } else { !git(root, &["ls-files", "--cached"]).unwrap_or_default().is_empty() };
    let index = if staged {
        let tree = git(root, &["write-tree"])?;
        let mut args = vec!["commit-tree", tree.as_str(), "-m", "kiln: what was staged when the session moved"];
        if born {
            args.extend(["-p", "HEAD"]);
        }
        Some(git(root, &args)?)
    } else {
        None
    };
    // The bundle: every branch, tag, stash entry and the staged commit, less what they have.
    let mut wanted: Vec<(String, String)> = Vec::new();
    for (i, s) in stash.iter().enumerate() {
        wanted.push((format!("{OUT}/stash/{i}"), s.sha.clone()));
    }
    if let Some(c) = &index {
        wanted.push((format!("{OUT}/index"), c.clone()));
    }
    for (r, sha) in &wanted {
        git(root, &["update-ref", r, sha])?;
    }
    let bundle = bundle(root, theirs);
    // The refs made for it, gone again: the repository is left as it was.
    for (r, _) in &wanted {
        let _ = git(root, &["update-ref", "-d", r]);
    }
    Ok(GitState { head, branches, tags, stash, index, bundle: bundle? })
}

/// A bundle of every ref here less what `theirs` reaches (those of them this repository knows).
fn bundle(root: &Path, theirs: &[String]) -> Result<Option<String>, String> {
    let known: Vec<&String> = theirs.iter().filter(|t| git_ok(root, &["cat-file", "-e", &format!("{t}^{{commit}}")])).collect();
    let tmp = std::env::temp_dir().join(format!("kiln-move-{}.bundle", crate::util::new_id()));
    let mut args: Vec<String> = vec!["bundle".into(), "create".into(), tmp.to_string_lossy().into_owned(), "--branches".into(), "--tags".into(), format!("--glob={OUT}/*")];
    if !known.is_empty() {
        args.push("--not".into());
        args.extend(known.into_iter().cloned());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let made = git(root, &refs);
    let out = match made {
        Ok(_) => {
            let size = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
            if size > MAX_BUNDLE {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!("its commits the other machine lacks come to {} MB, more than a move carries ({} MB): push them, and it moves", size >> 20, MAX_BUNDLE >> 20));
            }
            let bytes = std::fs::read(&tmp).map_err(|e| format!("reading the bundle: {e}"))?;
            Some(base64::engine::general_purpose::STANDARD.encode(bytes))
        }
        // They have every commit already.
        Err(e) if e.contains("empty bundle") => None,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    let _ = std::fs::remove_file(&tmp);
    Ok(out)
}

/// Puts `st` (from machine `from`) in place in the repository at `root`, its working tree as sync
/// brings it. What it did, in words, for the session's timeline; what couldn't be is said too.
pub fn restore(root: &Path, st: &GitState, from: &str) -> Vec<String> {
    let mut said: Vec<String> = Vec::new();
    let mut lacking: Vec<String> = Vec::new();
    if let Some(b) = &st.bundle {
        match fetch_bundle(root, b, from) {
            Ok(()) => {}
            Err(e) => said.push(format!("Its commits couldn't be added here ({e}); they stay on {from}")),
        }
    }
    let have = |sha: &str| git_ok(root, &["cat-file", "-e", sha]);
    let head_branch = match &st.head {
        Head::Branch(b) | Head::Unborn(b) => Some(b.as_str()),
        Head::Detached(_) => None,
    };
    // Branches: new ones made, ones behind moved on, ones with other commits here kept.
    let mut came = Vec::new();
    for b in &st.branches {
        if !have(&b.sha) {
            lacking.push(b.name.clone());
            continue;
        }
        let r = format!("refs/heads/{}", b.name);
        let here = git(root, &["rev-parse", "-q", "--verify", &r]).ok();
        match here.as_deref() {
            None => {
                if git_ok(root, &["update-ref", &r, &b.sha]) {
                    came.push(b.name.clone());
                }
            }
            Some(h) if h == b.sha => {}
            Some(h) if git_ok(root, &["merge-base", "--is-ancestor", h, &b.sha]) => {
                if git_ok(root, &["update-ref", &r, &b.sha, h]) {
                    came.push(b.name.clone());
                }
            }
            Some(h) if Some(b.name.as_str()) == head_branch => {
                // The session goes on from where it was: its branch moves there, and what this
                // machine had on it stays, beside it.
                let kept = format!("{}.before-move", b.name);
                if git_ok(root, &["update-ref", &format!("refs/heads/{kept}"), h]) && git_ok(root, &["update-ref", &r, &b.sha, h]) {
                    came.push(b.name.clone());
                    said.push(format!("Its branch {} here had other commits: they're kept as {kept}", b.name));
                }
            }
            Some(_) => {
                let beside = format!("{}.from-{from}", b.name);
                if git_ok(root, &["update-ref", &format!("refs/heads/{beside}"), &b.sha]) {
                    said.push(format!("Its branch {} differs here: {from}'s is {beside}", b.name));
                }
            }
        }
    }
    for t in &st.tags {
        let r = format!("refs/tags/{}", t.name);
        match git(root, &["rev-parse", "-q", "--verify", &r]).ok() {
            None if have(&t.sha) => {
                let _ = git(root, &["update-ref", &r, &t.sha]);
            }
            None => lacking.push(format!("tag {}", t.name)),
            Some(h) if h == t.sha => {}
            Some(_) => said.push(format!("Its tag {} differs here, so this machine's stays", t.name)),
        }
    }
    // The stash: entries it hasn't, oldest first, so the newest ends on top.
    let here: Vec<String> = git(root, &["stash", "list", "--format=%H"]).unwrap_or_default().lines().map(str::to_owned).collect();
    let mut stashed = 0;
    for s in st.stash.iter().rev() {
        if here.contains(&s.sha) {
            continue;
        }
        if have(&s.sha) && git_ok(root, &["stash", "store", "-m", &s.name, &s.sha]) {
            stashed += 1;
        } else {
            lacking.push(format!("stash entry {:?}", s.name));
        }
    }
    // HEAD, where it was; then the index, as staged there (no file is touched).
    let headed = match &st.head {
        Head::Branch(b) | Head::Unborn(b) => git_ok(root, &["symbolic-ref", "HEAD", &format!("refs/heads/{b}")]),
        Head::Detached(sha) => have(sha) && git_ok(root, &["update-ref", "--no-deref", "HEAD", sha]),
    };
    if !headed {
        said.push(format!("HEAD couldn't be put where it was on {from}"));
    }
    let indexed = match (&st.index, &st.head) {
        (Some(c), _) if have(c) => git_ok(root, &["read-tree", &format!("{c}^{{tree}}")]),
        (Some(_), _) => {
            lacking.push("what was staged".into());
            false
        }
        (None, Head::Unborn(_)) => git_ok(root, &["read-tree", "--empty"]),
        (None, _) => git_ok(root, &["read-tree", "HEAD"]),
    };
    // What was fetched for this, gone again.
    if let Ok(del) = git(root, &["for-each-ref", "--format=%(refname)", &format!("{IN}/{}", safe(from))]) {
        for r in del.lines() {
            let _ = git(root, &["update-ref", "-d", r]);
        }
    }
    let mut what = vec![match &st.head {
        Head::Branch(b) | Head::Unborn(b) => format!("on branch {b}"),
        Head::Detached(sha) => format!("at {}", sha.get(..10).unwrap_or(sha)),
    }];
    if !came.is_empty() {
        what.push(format!("{} from there ({})", if came.len() == 1 { "a branch" } else { "branches" }, came.join(", ")));
    }
    if stashed > 0 {
        what.push(format!("{stashed} stash {}", if stashed == 1 { "entry" } else { "entries" }));
    }
    if st.index.is_some() && indexed {
        what.push("what was staged".into());
    }
    said.insert(0, format!("Its repository here is as it was on {from}: {}", what.join(", ")));
    if !lacking.is_empty() {
        said.push(format!("These couldn't come, their commits not being here: {}; they stay on {from}", lacking.join(", ")));
    }
    said
}

/// A machine's name as a ref's part.
fn safe(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect()
}

fn fetch_bundle(root: &Path, b64: &str, from: &str) -> Result<(), String> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).map_err(|e| format!("the bundle is unreadable: {e}"))?;
    let tmp = std::env::temp_dir().join(format!("kiln-moved-{}.bundle", crate::util::new_id()));
    std::fs::write(&tmp, bytes).map_err(|e| format!("writing the bundle: {e}"))?;
    let to = format!("{IN}/{}", safe(from));
    let path = tmp.to_string_lossy().into_owned();
    let got = git(root, &["fetch", "--quiet", "--no-tags", &path, &format!("+refs/*:{to}/*")]);
    let _ = std::fs::remove_file(&tmp);
    got.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
        git(dir, args).map_err(anyhow::Error::msg)
    }

    fn write(dir: &Path, name: &str, text: &str) -> anyhow::Result<()> {
        std::fs::write(dir.join(name), text)?;
        Ok(())
    }

    /// A repository and a clone of it, as a session's folder and its copy on another machine.
    fn pair() -> anyhow::Result<(tempfile::TempDir, PathBuf, PathBuf)> {
        let t = tempfile::tempdir()?;
        let a = t.path().join("a");
        std::fs::create_dir_all(&a)?;
        sh(&a, &["init", "-q", "-b", "main"])?;
        write(&a, "README", "one\n")?;
        sh(&a, &["add", "README"])?;
        sh(&a, &["commit", "-qm", "first"])?;
        let b = t.path().join("b");
        let out = Command::new("git").args(["clone", "-q"]).arg(&a).arg(&b).output()?;
        anyhow::ensure!(out.status.success(), "clone: {}", String::from_utf8_lossy(&out.stderr));
        Ok((t, a, b))
    }

    /// Its folder's files, as sync brings them: the working tree of `a` over `b`'s.
    fn sync_files(a: &Path, b: &Path) -> anyhow::Result<()> {
        for name in ["README", "staged.txt", "loose.txt"] {
            match std::fs::read(a.join(name)) {
                Ok(bytes) => std::fs::write(b.join(name), bytes)?,
                Err(_) => {
                    let _ = std::fs::remove_file(b.join(name));
                }
            }
        }
        Ok(())
    }

    /// Everything a session left in its repository arrives on the other machine as it was:
    /// unpushed commits on its branch and another, a stash, what was staged and what wasn't.
    #[test]
    fn a_repository_moves_as_it_was() -> anyhow::Result<()> {
        let (_t, a, b) = pair()?;
        // On a: two commits nobody else has, a branch of its own, a stash, a staged file, an
        // unstaged change.
        write(&a, "README", "two\n")?;
        sh(&a, &["commit", "-qam", "second"])?;
        sh(&a, &["branch", "spike"])?;
        write(&a, "README", "three\n")?;
        sh(&a, &["commit", "-qam", "third"])?;
        write(&a, "README", "stashed\n")?;
        sh(&a, &["stash", "push", "-qm", "an idea"])?;
        write(&a, "staged.txt", "staged\n")?;
        sh(&a, &["add", "staged.txt"])?;
        write(&a, "README", "unstaged\n")?;
        let status_a = sh(&a, &["status", "--porcelain"])?;
        let st = capture(&a, &tips(&b)).map_err(anyhow::Error::msg)?;
        assert!(st.bundle.is_some(), "b lacks the new commits");
        assert_eq!(st.head, Head::Branch("main".into()));
        assert!(sh(&a, &["for-each-ref", OUT]).map(|o| o.is_empty())?, "a is left as it was");
        // The package travels as JSON.
        let st: GitState = serde_json::from_str(&serde_json::to_string(&st)?)?;
        sync_files(&a, &b)?;
        let said = restore(&b, &st, "lab-a");
        assert_eq!(sh(&b, &["log", "--format=%s", "main"])?, "third\nsecond\nfirst", "{said:?}");
        assert_eq!(sh(&b, &["rev-parse", "spike"])?, sh(&a, &["rev-parse", "spike"])?);
        assert_eq!(sh(&b, &["stash", "list", "--format=%gs"])?, "On main: an idea");
        assert_eq!(sh(&b, &["status", "--porcelain"])?, status_a, "staged and unstaged as they were");
        assert_eq!(sh(&b, &["symbolic-ref", "HEAD"])?, "refs/heads/main");
        assert!(sh(&b, &["for-each-ref", IN]).map(|o| o.is_empty())?, "what was fetched for it is gone");
        assert!(said.first().is_some_and(|s| s.contains("on branch main") && s.contains("1 stash entry") && s.contains("what was staged")), "{said:?}");
        Ok(())
    }

    /// Nothing the other machine has is lost: its branch with other commits is kept beside.
    #[test]
    fn nothing_there_is_lost() -> anyhow::Result<()> {
        let (_t, a, b) = pair()?;
        write(&a, "README", "from a\n")?;
        sh(&a, &["commit", "-qam", "on a"])?;
        sh(&a, &["branch", "other"])?;
        // b went its own way on main and on other.
        write(&b, "README", "from b\n")?;
        sh(&b, &["commit", "-qam", "on b"])?;
        sh(&b, &["branch", "other"])?;
        let b_main = sh(&b, &["rev-parse", "main"])?;
        let st = capture(&a, &tips(&b)).map_err(anyhow::Error::msg)?;
        sync_files(&a, &b)?;
        let said = restore(&b, &st, "lab-a");
        assert_eq!(sh(&b, &["rev-parse", "main"])?, sh(&a, &["rev-parse", "main"])?, "the session goes on from where it was");
        assert_eq!(sh(&b, &["rev-parse", "main.before-move"])?, b_main, "what b had on main is kept");
        assert_eq!(sh(&b, &["rev-parse", "other"])?, b_main, "another branch stays b's own");
        assert_eq!(sh(&b, &["rev-parse", "other.from-lab-a"])?, sh(&a, &["rev-parse", "main"])?);
        assert!(said.iter().any(|s| s.contains("main.before-move")) && said.iter().any(|s| s.contains("other.from-lab-a")), "{said:?}");
        Ok(())
    }

    /// When the other machine has every commit, nothing but the refs travels.
    #[test]
    fn only_what_they_lack_travels() -> anyhow::Result<()> {
        let (_t, a, b) = pair()?;
        let st = capture(&a, &tips(&b)).map_err(anyhow::Error::msg)?;
        assert_eq!(st.bundle, None);
        // Without knowing their tips, the whole history goes.
        let st = capture(&a, &[]).map_err(anyhow::Error::msg)?;
        assert!(st.bundle.is_some());
        Ok(())
    }

    /// A merge under way lives in .git alone: the session doesn't move until it's done.
    #[test]
    fn a_merge_under_way_holds_it() -> anyhow::Result<()> {
        let (_t, a, _b) = pair()?;
        sh(&a, &["checkout", "-qb", "side"])?;
        write(&a, "README", "side\n")?;
        sh(&a, &["commit", "-qam", "side"])?;
        sh(&a, &["checkout", "-q", "main"])?;
        write(&a, "README", "main\n")?;
        sh(&a, &["commit", "-qam", "main"])?;
        let _ = git(&a, &["merge", "-q", "side"]);
        assert_eq!(under_way(&a), Some("a merge"));
        assert!(capture(&a, &[]).is_err_and(|e| e.contains("a merge is under way")));
        Ok(())
    }

    /// A repository with no commit yet, on a branch: its staged files still come.
    #[test]
    fn a_repository_with_no_commit_yet() -> anyhow::Result<()> {
        let t = tempfile::tempdir()?;
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        for d in [&a, &b] {
            std::fs::create_dir_all(d)?;
            sh(d, &["init", "-q", "-b", "main"])?;
        }
        write(&a, "staged.txt", "new\n")?;
        sh(&a, &["add", "staged.txt"])?;
        let st = capture(&a, &tips(&b)).map_err(anyhow::Error::msg)?;
        assert_eq!(st.head, Head::Unborn("main".into()));
        sync_files(&a, &b)?;
        let _ = restore(&b, &st, "lab-a");
        assert_eq!(sh(&b, &["status", "--porcelain"])?, "A  staged.txt");
        Ok(())
    }
}
