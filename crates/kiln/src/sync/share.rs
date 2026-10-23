//! Work kept in step between the machines that hold a repository, with no one starting it
//! (`docs/sync.md`, "Work between machines"). Each machine says what it holds ([`Msg::Repos`]);
//! the server tells each one the repositories others hold too ([`Msg::Shares`]), and each shares
//! its work in them whenever it changes: its branches, and each folder's work in progress as a
//! commit of its files as they are, on the branch checked out, with the commits those name that
//! the remote lacks in a bundle ([`Msg::Shared`]). The others take it in beside their own
//! ([`Msg::Take`]): `kiln/<machine>/<branch>` (`refs/remotes/kiln/…`), and its work in progress as
//! `refs/kiln/wip/<machine>/<branch>`. Nothing of their own moves, so it's quiet; and each folder
//! says how its branch stands against the others' ([`Peer`]: ahead, behind, or both, diverged), so
//! a conflict shows before anyone merges.
//!
//! A machine shares one repository of each it holds (its folders of the same repository, a
//! worktree's included, when it has several clones: the one git last worked in). A folder's work in
//! progress is left out when its files are too many or too big to carry; a bundle too big goes
//! without it, and too big still, the branches go alone (the others take the commits they hold).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use parking_lot::Mutex;
use sha2::{Digest as _, Sha256};

use super::git::{LONG, STATUS_TIME, run, run_with};
use super::wire::{Msg, Peer, ShareSlot, Work};
use crate::hub::Hub;

/// Where another machine's branches and work in progress are taken in.
const BRANCHES: &str = "refs/remotes/kiln/";
const WIP: &str = "refs/kiln/wip/";
/// Where the tips of a bundle being made are named.
const TIPS: &str = "refs/kiln-share/tip-";
/// A folder's work in progress is left out when its changed files are more, or weigh more, than this.
const WIP_FILES: usize = 5_000;
const WIP_BYTES: u64 = 64 << 20;
/// The biggest bundle of a machine's work.
pub const BUNDLE_MAX: u64 = 256 << 20;

/// A machine's name as a ref's part: letters, digits, `.`, `_` and `-`, never `..`, nor a `.` or
/// `.lock` at either end.
pub fn ref_name(machine: &str) -> String {
    let mut n: String = machine.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' }).collect();
    while n.contains("..") {
        n = n.replace("..", ".");
    }
    let n = n.trim_matches('.').trim_end_matches(".lock").to_owned();
    if n.is_empty() { "machine".to_owned() } else { n }
}

// ------------------------------------------------------------------------------ a runner's side

/// The repository a folder belongs to (its common git folder: a worktree's is its main one's).
pub async fn repository_of(folder: &Path) -> Option<PathBuf> {
    let dir = run(Some(folder), &["rev-parse", "--path-format=absolute", "--git-common-dir"], STATUS_TIME).await.ok()?;
    Some(PathBuf::from(dir.trim()))
}

/// What a machine's work in a repository is now, cheaply: it changes when a branch, a folder's
/// `HEAD` or its files do.
pub async fn fingerprint(folders: &[PathBuf]) -> String {
    let mut h = Sha256::new();
    if let Some(first) = folders.first()
        && let Ok(o) = run(Some(first), &["for-each-ref", "--format=%(refname) %(objectname)", "refs/heads"], STATUS_TIME).await
    {
        h.update(o.as_bytes());
    }
    for f in folders {
        h.update(f.to_string_lossy().as_bytes());
        if let Ok(o) = run(Some(f), &["rev-parse", "-q", "--verify", "HEAD"], STATUS_TIME).await {
            h.update(o.as_bytes());
        }
        if let Ok((listed, files)) = changes(f).await {
            h.update(listed.as_bytes());
            for (path, size, mtime) in files {
                h.update(path.as_bytes());
                h.update(size.to_le_bytes());
                h.update(mtime.to_le_bytes());
            }
        }
    }
    hex::encode(h.finalize())
}

/// What git says changed in a folder (`status --porcelain -z`, untracked files too), and each
/// changed file there now with its size and time.
async fn changes(folder: &Path) -> anyhow::Result<(String, Vec<(String, u64, i128)>)> {
    let listed = run(Some(folder), &["--no-optional-locks", "status", "--porcelain", "-z", "--untracked-files=all"], STATUS_TIME).await?;
    let root = folder.to_path_buf();
    let names: Vec<String> = listed.split('\0').filter(|e| e.len() > 3).map(|e| e[3..].to_owned()).collect();
    let files = crate::rt::blocking(move || {
        names
            .into_iter()
            .filter_map(|n| {
                let m = std::fs::symlink_metadata(root.join(&n)).ok()?;
                let mtime = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| i128::try_from(d.as_nanos()).unwrap_or(i128::MAX));
                Some((n, m.len(), mtime))
            })
            .collect::<Vec<_>>()
    })
    .await?;
    Ok((listed, files))
}

/// A folder's work in progress: a commit of its files as they are (what git ignores left out),
/// on the commit checked out, named for the branch (`detached-<commit>` without one). None when
/// its files are its commit's, or are too many or too big to carry. Nothing in the folder changes:
/// the commit is made from an index of its own.
async fn wip_of(folder: &Path, me: &str) -> anyhow::Result<Option<(String, String)>> {
    let (listed, files) = changes(folder).await?;
    if listed.is_empty() || files.len() > WIP_FILES || files.iter().map(|(_, size, _)| size).sum::<u64>() > WIP_BYTES {
        return Ok(None);
    }
    let Ok(head) = run(Some(folder), &["rev-parse", "-q", "--verify", "HEAD"], STATUS_TIME).await else { return Ok(None) };
    let head = head.trim().to_owned();
    let branch = match run(Some(folder), &["symbolic-ref", "-q", "--short", "HEAD"], STATUS_TIME).await {
        Ok(b) if !b.trim().is_empty() => b.trim().to_owned(),
        _ => format!("detached-{}", head.get(..12).unwrap_or(&head)),
    };
    let index = run(Some(folder), &["rev-parse", "--path-format=absolute", "--git-path", "index"], STATUS_TIME).await?;
    let index = PathBuf::from(index.trim());
    let own = index.with_file_name(format!("kiln-wip-{}.index", crate::util::new_id()));
    // From the folder's index, so git hashes only what changed.
    let _ = std::fs::copy(&index, &own);
    let env = [("GIT_INDEX_FILE", own.as_os_str())];
    let tree = async {
        run_with(Some(folder), &["add", "--all"], &env, LONG).await?;
        run_with(Some(folder), &["write-tree"], &env, STATUS_TIME).await
    }
    .await;
    let _ = std::fs::remove_file(&own);
    let tree = tree?.trim().to_owned();
    if run(Some(folder), &["rev-parse", "HEAD^{tree}"], STATUS_TIME).await?.trim() == tree {
        return Ok(None);
    }
    let kiln = OsStr::new("kiln");
    let at = OsStr::new("kiln@localhost");
    let who = [("GIT_AUTHOR_NAME", kiln), ("GIT_AUTHOR_EMAIL", at), ("GIT_COMMITTER_NAME", kiln), ("GIT_COMMITTER_EMAIL", at)];
    let message = format!("Work in progress on {me}");
    let commit = run_with(Some(folder), &["commit-tree", &tree, "-p", &head, "-m", &message], &who, STATUS_TIME).await?;
    Ok(Some((branch, commit.trim().to_owned())))
}

/// A machine's work in a repository (`folders`, all of one repository): its branches, each
/// folder's work in progress, and in `out` a bundle of the commits they name that its remote
/// lacks (true when one was made).
pub async fn work(folders: &[PathBuf], me: &str, out: &Path) -> anyhow::Result<(Work, bool)> {
    let first = folders.first().context("no folder")?;
    let mut w = Work::default();
    for line in run(Some(first), &["for-each-ref", "--format=%(refname:short) %(objectname)", "refs/heads"], STATUS_TIME).await?.lines() {
        if let Some((branch, commit)) = line.split_once(' ') {
            w.branches.insert(branch.to_owned(), commit.trim().to_owned());
        }
    }
    for f in folders {
        match wip_of(f, me).await {
            Ok(Some((branch, commit))) => {
                w.wip.insert(branch, commit);
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(target: "kiln.sync", folder = %f.display(), error = %format!("{e:#}"), "a folder's work in progress couldn't be made"),
        }
    }
    let tips: Vec<String> = w.branches.values().chain(w.wip.values()).cloned().collect();
    let mut made = bundle(first, &tips, out).await?;
    if made && size_of(out) > BUNDLE_MAX && !w.wip.is_empty() {
        // Too big with the work in progress: the branches alone.
        w.wip.clear();
        let tips: Vec<String> = w.branches.values().cloned().collect();
        made = bundle(first, &tips, out).await?;
    }
    if made && size_of(out) > BUNDLE_MAX {
        let _ = std::fs::remove_file(out);
        made = false;
    }
    Ok((w, made))
}

fn size_of(p: &Path) -> u64 {
    std::fs::metadata(p).map_or(0, |m| m.len())
}

/// A bundle in `out` of what the commits `tips` reach that the repository's remote doesn't: false,
/// and none, when it reaches them all.
async fn bundle(repo: &Path, tips: &[String], out: &Path) -> anyhow::Result<bool> {
    if tips.is_empty() {
        return Ok(false);
    }
    // A bundle names refs, not commits: each tip is one while it is made.
    let names: Vec<String> = (0..tips.len()).map(|n| format!("{TIPS}{n}")).collect();
    let made = async {
        for (name, tip) in names.iter().zip(tips) {
            run(Some(repo), &["update-ref", name, tip], STATUS_TIME).await?;
        }
        let mut args: Vec<String> = vec!["bundle".into(), "create".into(), "--quiet".into(), out.to_string_lossy().into_owned()];
        args.extend(names.iter().cloned());
        if run(Some(repo), &["config", "--get", "remote.origin.url"], STATUS_TIME).await.is_ok() {
            args.extend(["--not".into(), "--remotes=origin".into()]);
        }
        match run(Some(repo), &args, LONG).await {
            Ok(_) => Ok(true),
            Err(e) if format!("{e:#}").contains("empty bundle") => Ok(false),
            Err(e) => Err(e),
        }
    }
    .await;
    for name in &names {
        let _ = run(Some(repo), &["update-ref", "-d", name], STATUS_TIME).await;
    }
    made
}

/// Runs git in `repo` with `input` on its standard input: what it printed.
async fn with_input(repo: &Path, args: &[&str], input: String) -> anyhow::Result<String> {
    let mut c = super::git::git(Some(repo));
    c.args(args).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let what = args.first().copied().unwrap_or("").to_owned();
    crate::rt::blocking(move || -> anyhow::Result<String> {
        use std::io::Write as _;
        let mut child = c.spawn().with_context(|| format!("git {what}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        anyhow::ensure!(out.status.success(), "git {what}: {}", String::from_utf8_lossy(&out.stderr).trim());
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    })
    .await?
}

/// The commits of `ids` the repository holds.
async fn held(repo: &Path, ids: &BTreeSet<&str>) -> HashSet<String> {
    let input: String = ids.iter().map(|id| format!("{id}\n")).collect();
    let Ok(out) = with_input(repo, &["cat-file", "--batch-check=%(objectname) %(objecttype)"], input).await else { return HashSet::new() };
    out.lines().filter_map(|l| l.split_once(' ')).filter(|(_, kind)| *kind == "commit").map(|(id, _)| id.to_owned()).collect()
}

/// Takes another machine's work (`w`, its bundle in `bundle` when there's one) into the
/// repository `repo`: its branches as `kiln/<machine>/<branch>`, its work in progress as
/// `refs/kiln/wip/<machine>/<branch>`, and what it no longer has goes. Nothing else moves.
pub async fn take(repo: &Path, w: &Work, bundle: Option<&Path>) -> anyhow::Result<()> {
    let ids: BTreeSet<&str> = w.branches.values().chain(w.wip.values()).map(String::as_str).collect();
    let mut have = held(repo, &ids).await;
    if have.len() < ids.len()
        && let Some(file) = bundle
    {
        let file = file.to_string_lossy().into_owned();
        if run(Some(repo), &["bundle", "unbundle", &file], LONG).await.is_err() {
            // Built on commits its remote has and this repository hasn't fetched yet.
            let _ = run(Some(repo), &["fetch", "--quiet", "origin"], LONG).await;
            let _ = run(Some(repo), &["bundle", "unbundle", &file], LONG).await;
        }
        have = held(repo, &ids).await;
    }
    let m = ref_name(&w.from);
    let wanted: BTreeMap<String, &String> = w
        .branches
        .iter()
        .map(|(b, c)| (format!("{BRANCHES}{m}/{b}"), c))
        .chain(w.wip.iter().map(|(b, c)| (format!("{WIP}{m}/{b}"), c)))
        .filter(|(r, c)| have.contains(c.as_str()) && super::refs::valid_name(r))
        .collect();
    let there = run(Some(repo), &["for-each-ref", "--format=%(refname)", &format!("{BRANCHES}{m}/"), &format!("{WIP}{m}/")], STATUS_TIME).await?;
    let mut lines = String::new();
    for (r, c) in &wanted {
        lines.push_str(&format!("update {r} {c}\n"));
    }
    for r in there.lines().filter(|r| !r.is_empty() && !wanted.contains_key(*r)) {
        lines.push_str(&format!("delete {r}\n"));
    }
    if !lines.is_empty() {
        with_input(repo, &["update-ref", "--stdin"], lines).await?;
    }
    Ok(())
}

/// How the branch `branch` checked out in `folder` stands against the same branch of the other
/// machines whose work it took in.
pub async fn peers(folder: &Path, branch: &str) -> Vec<Peer> {
    let Ok(listed) = run(Some(folder), &["for-each-ref", "--format=%(refname)", BRANCHES, WIP], STATUS_TIME).await else { return Vec::new() };
    let mut by: BTreeMap<String, Peer> = BTreeMap::new();
    for r in listed.lines() {
        if let Some((m, b)) = r.strip_prefix(BRANCHES).and_then(|rest| rest.split_once('/'))
            && b == branch
        {
            let counts = run(Some(folder), &["rev-list", "--left-right", "--count", &format!("HEAD...{r}")], STATUS_TIME).await.unwrap_or_default();
            let mut n = counts.split_whitespace().map(|x| x.parse::<u32>().unwrap_or(0));
            let p = by.entry(m.to_owned()).or_insert_with(|| Peer { machine: m.to_owned(), ..Peer::default() });
            (p.ahead, p.behind) = (n.next().unwrap_or(0), n.next().unwrap_or(0));
        } else if let Some((m, b)) = r.strip_prefix(WIP).and_then(|rest| rest.split_once('/'))
            && b == branch
        {
            by.entry(m.to_owned()).or_insert_with(|| Peer { machine: m.to_owned(), ..Peer::default() }).wip = true;
        }
    }
    by.into_values().collect()
}

// ------------------------------------------------------------------------------ the server's side

/// Who shares what: each machine's latest work in each repository, and the batch it sends its
/// next to.
#[derive(Default)]
pub struct Shares {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// (repository, machine) → its latest work.
    latest: HashMap<(String, String), Work>,
    /// (repository, machine) → the batch its next work goes to.
    slots: HashMap<(String, String), String>,
    /// (machine, repository, from) → the batch of `from`'s work it was given last.
    given: HashMap<(String, String, String), String>,
}

impl Shares {
    /// What machines hold changed (or one connected): each is told the repositories it holds that
    /// others hold too, with a batch for its work in each, and given the others' work it lacks.
    pub fn arrange(&self, hub: &Hub) {
        let held = hub.places.keys();
        let holders = |key: &str| -> BTreeSet<String> { held.iter().filter(|(_, keys)| keys.contains(key)).map(|(m, _)| m.clone()).collect() };
        let mut out: Vec<(String, Msg)> = Vec::new();
        {
            let mut inner = self.inner.lock();
            for (machine, keys) in &held {
                let mut slots = Vec::new();
                for key in keys {
                    let others: HashSet<String> = holders(key).into_iter().filter(|m| m != machine).collect();
                    if others.is_empty() {
                        continue;
                    }
                    let id = (key.clone(), machine.clone());
                    let batch = inner.slots.entry(id.clone()).or_insert_with(crate::util::new_id).clone();
                    hub.tasks.open_bundle(&batch, machine, others.clone());
                    slots.push(ShareSlot { key: key.clone(), batch, fresh: !inner.latest.contains_key(&id) });
                    // The others' latest work it hasn't been given.
                    for from in &others {
                        let Some(w) = inner.latest.get(&(key.clone(), from.clone())).cloned() else { continue };
                        hub.tasks.allow(&w.batch, machine);
                        let given = (machine.clone(), key.clone(), from.clone());
                        if inner.given.get(&given) != Some(&w.batch) {
                            inner.given.insert(given, w.batch.clone());
                            out.push((machine.clone(), Msg::Take(Box::new(w))));
                        }
                    }
                }
                out.push((machine.clone(), Msg::Shares { shares: slots }));
            }
        }
        for (machine, msg) in out {
            hub.sync.send(&machine, msg);
        }
    }

    /// `machine` shared its work: kept as its latest (its last bundle goes), given to the others
    /// that hold the repository, and the machine told where its next goes.
    pub fn shared(&self, hub: &Hub, machine: &str, mut w: Work) {
        let id = (w.key.clone(), machine.to_owned());
        let wanted = self.inner.lock().slots.get(&id).is_some_and(|b| *b == w.batch);
        if !wanted {
            return;
        }
        // The bundle is the one the server took for the batch, or none.
        match hub.tasks.bundled(&w.batch) {
            Some((hash, size)) => (w.hash, w.size) = (Some(hash), size),
            None => (w.hash, w.size) = (None, 0),
        }
        w.from = machine.to_owned();
        let old = {
            let mut inner = self.inner.lock();
            inner.slots.remove(&id);
            inner.latest.insert(id, w.clone())
        };
        if let Some(old) = old
            && old.batch != w.batch
        {
            hub.tasks.drop_bundle(&old.batch);
        }
        tracing::info!(target: "kiln.sync", event = "sync.shared", repo = %w.key, machine = %machine, branches = w.branches.len(), wip = w.wip.len(), bytes = w.size, "a machine shared its work");
        self.arrange(hub);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs git in `dir`: what it printed; a failure fails the test.
    fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
            .args(args)
            .output()?;
        anyhow::ensure!(out.status.success(), "git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr));
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    #[test]
    fn a_machine_s_work_goes_beside_another_s_own_and_says_how_they_stand() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (origin, a, b) = (dir.path().join("origin"), dir.path().join("a"), dir.path().join("b"));
        for d in [&origin, &a, &b] {
            std::fs::create_dir_all(d)?;
        }
        git(&origin, &["init", "-q", "-b", "main"])?;
        std::fs::write(origin.join("README"), "hello\n")?;
        git(&origin, &["add", "README"])?;
        git(&origin, &["commit", "-q", "-m", "first"])?;
        let from = origin.to_string_lossy().into_owned();
        git(&a, &["clone", "-q", &from, "."])?;
        git(&b, &["clone", "-q", &from, "."])?;
        let start = git(&b, &["rev-parse", "main"])?;
        // a commits, and has work in progress: an edit and a new file.
        std::fs::write(a.join("lib.rs"), "fn main() {}\n")?;
        git(&a, &["add", "lib.rs"])?;
        git(&a, &["commit", "-q", "-m", "lib"])?;
        let committed = git(&a, &["rev-parse", "main"])?;
        std::fs::write(a.join("README"), "hello again\n")?;
        std::fs::write(a.join("notes.txt"), "half done\n")?;
        let quiet = git(&a, &["status", "--porcelain"])?;
        let bundle = dir.path().join("work.bundle");
        let (aa, bb, file) = (a.clone(), b.clone(), bundle.clone());
        let (w, made, print) = crate::rt::block_on(async move {
            let print = fingerprint(std::slice::from_ref(&aa)).await;
            let (mut w, made) = work(std::slice::from_ref(&aa), "a", &file).await?;
            w.from = "a".into();
            take(&bb, &w, made.then_some(file.as_path())).await?;
            anyhow::Ok((w, made, print))
        })?;
        assert!(made, "a's commit isn't on the remote: it goes in the bundle");
        assert_eq!(w.branches.get("main"), Some(&committed));
        assert!(w.wip.contains_key("main"));
        assert_eq!(git(&a, &["status", "--porcelain"])?, quiet, "making the work in progress changed nothing in a's folder");
        // b has a's work beside its own: its branch, and its work in progress, files and all.
        assert_eq!(git(&b, &["rev-parse", "refs/remotes/kiln/a/main"])?, committed);
        assert_eq!(git(&b, &["show", "refs/kiln/wip/a/main:notes.txt"])?, "half done");
        assert_eq!(git(&b, &["show", "refs/kiln/wip/a/main:README"])?, "hello again");
        assert_eq!((git(&b, &["rev-parse", "main"])?, std::fs::read_to_string(b.join("README"))?), (start, "hello\n".to_owned()), "nothing of b's own moved");
        assert_eq!(git(&b, &["status", "--porcelain"])?, "");
        let bb = b.clone();
        let stands = crate::rt::block_on(async move { anyhow::Ok(peers(&bb, "main").await) })?;
        assert_eq!(stands, vec![Peer { machine: "a".into(), ahead: 0, behind: 1, wip: true }]);
        // a's work in progress goes (committed): b's goes too; a's look says nothing changed twice.
        git(&a, &["add", "--all"])?;
        git(&a, &["commit", "-q", "-m", "done"])?;
        let (aa, bb, file) = (a.clone(), b.clone(), bundle.clone());
        let (again, same) = crate::rt::block_on(async move {
            let again = fingerprint(std::slice::from_ref(&aa)).await;
            let same = fingerprint(std::slice::from_ref(&aa)).await;
            let (mut w, made) = work(std::slice::from_ref(&aa), "a", &file).await?;
            w.from = "a".into();
            take(&bb, &w, made.then_some(file.as_path())).await?;
            anyhow::Ok((again, same))
        })?;
        assert!(again != print && again == same, "the fingerprint follows the work, and only it");
        assert_eq!(git(&b, &["rev-parse", "refs/remotes/kiln/a/main"])?, git(&a, &["rev-parse", "main"])?);
        assert_eq!(git(&b, &["for-each-ref", "refs/kiln/wip/"])?, "", "a's work in progress is gone from b");
        Ok(())
    }

    #[test]
    fn a_machine_s_name_is_a_ref_s_part() {
        assert_eq!(ref_name("fedora.attlocal.net"), "fedora.attlocal.net");
        assert_eq!(ref_name("Ada's Mac"), "Ada-s-Mac");
        assert_eq!(ref_name("..x..lock"), "x");
        assert_eq!(ref_name(""), "machine");
    }
}
