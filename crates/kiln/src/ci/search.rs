//! The search across a change (docs/ci.md W26-R07), through kiln-seams' [`Search`] (vorpal,
//! `crates/kiln-search`):
//! - `GET /api/diffs/:id/versions/:n/search?q=&mode=text|code&lang=&limit=`: a kiln diff's (or
//!   change request's) files at a version;
//! - `GET /api/ci/repos/:repo/pulls/:n/search?…`: a host's pull request's files at its mirrored
//!   head, the same way.
//!
//! The index is the tree's (`git rev-parse <commit>^{tree}`), built at the first search of it from
//! the repository's blobs and kept under the server's cache (`<data>/cache/search`). Each hit has
//! its file, line, column, the definition it sits in and its line; hits on the lines the version
//! changes (its diff against its parent, or a pull request's against where its base branch meets
//! it) come first, then hits in the files it changes, then the rest of the tree. Indexes are
//! dropped with the change: when it lands or is abandoned (a pull request merged or closed), its
//! trees go, unless another open change still uses one; past the seam's bound, the tree searched
//! least recently goes.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use http::StatusCode;
use kiln_seams::search::{self as seam, File, Mode, Query, Refusal};
use parking_lot::Mutex;
use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Fail;
use super::diffs::{Diff, Version};
use crate::authz::{Caller, Scope};
use crate::gitops::{self, LS_REMOTE_TIMEOUT};
use crate::hub::Hub;

/// Hits a search answers when it isn't told.
const LIMIT: usize = 200;
/// Commits whose trees are remembered, and versions whose changed lines are.
const REMEMBERED: usize = 4096;
const CHANGES_REMEMBERED: usize = 64;

fn bad(why: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, why.into())
}

fn failed(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn refused(r: Refusal) -> Fail {
    let status = match &r {
        Refusal::BadQuery { .. } | Refusal::Invalid { .. } => StatusCode::BAD_REQUEST,
        Refusal::TooBig { .. } => StatusCode::PAYLOAD_TOO_LARGE,
        Refusal::NotBuilt { .. } => StatusCode::CONFLICT,
        Refusal::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status, r.to_string())
}

/// What's asked: the query as the seam takes it (every hit it has, up to its bound, so the
/// changed lines can come first), and the hits to answer.
fn asked(q: &HashMap<String, String>) -> Result<(Query, usize), Fail> {
    let words = q.get("q").filter(|w| !w.trim().is_empty()).ok_or_else(|| bad("`q`: what to search for"))?;
    let mode = match q.get("mode").map(String::as_str) {
        None | Some("text") => Mode::Text,
        Some("code") => Mode::Code,
        Some(other) => return Err(bad(format!("`mode` is `text` or `code`, not `{other}`"))),
    };
    let limit = match q.get("limit") {
        None => LIMIT,
        Some(l) => l.parse::<usize>().ok().filter(|l| (1..=seam::HITS_MAX).contains(l)).ok_or_else(|| bad(format!("`limit`: 1 to {}", seam::HITS_MAX)))?,
    };
    let lang = q.get("lang").filter(|l| !l.trim().is_empty()).cloned();
    Ok((Query { mode, q: words.clone(), lang, limit: seam::HITS_MAX }, limit))
}

/// The tree `commit` has, in the repository at `dir` (a commit's tree never changes, so it's
/// remembered).
async fn tree_of(hub: &Hub, dir: &str, commit: &str) -> Result<String, Fail> {
    static TREES: OnceLock<Mutex<HashMap<(String, String), String>>> = OnceLock::new();
    let key = (dir.to_owned(), commit.to_owned());
    if let Some(t) = TREES.get_or_init(Default::default).lock().get(&key) {
        return Ok(t.clone());
    }
    let out = gitops::run_git(hub, &["-C", dir, "rev-parse", "--verify", "--quiet", &format!("{commit}^{{tree}}")], LS_REMOTE_TIMEOUT).await.map_err(|_| (StatusCode::NOT_FOUND, format!("no commit {commit} in the repository")))?;
    let tree = String::from_utf8_lossy(&out).trim().to_owned();
    seam::check_tree(&tree).map_err(refused)?;
    let mut trees = TREES.get_or_init(Default::default).lock();
    if trees.len() >= REMEMBERED {
        trees.clear();
    }
    trees.insert(key, tree.clone());
    Ok(tree)
}

/// `tree`'s files, read from the repository at `dir`, and how many were left out without being
/// read: links, submodules, and files past [`seam::FILE_MAX`].
async fn files_of(hub: &Hub, dir: &str, tree: &str) -> Result<(Vec<File>, u64), Fail> {
    let listed = gitops::run_git(hub, &["-C", dir, "ls-tree", "-r", "-z", "-l", "--full-tree", tree], LS_REMOTE_TIMEOUT).await.map_err(failed)?;
    let mut wanted: Vec<(String, String)> = Vec::new();
    let mut skipped = 0u64;
    let mut bytes = 0u64;
    for entry in listed.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let text = String::from_utf8_lossy(entry);
        let Some((meta, path)) = text.split_once('\t') else { continue };
        let mut m = meta.split_whitespace();
        let (Some(mode), Some(kind), Some(sha), Some(size)) = (m.next(), m.next(), m.next(), m.next()) else { continue };
        let size: u64 = size.parse().unwrap_or(u64::MAX);
        if kind != "blob" || !matches!(mode, "100644" | "100755") || size > seam::FILE_MAX as u64 || seam::check_path(path).is_err() {
            skipped = skipped.saturating_add(1);
            continue;
        }
        bytes = bytes.saturating_add(size);
        wanted.push((sha.to_owned(), path.to_owned()));
    }
    if wanted.len() > seam::TREE_FILES_MAX {
        return Err(refused(Refusal::TooBig { what: format!("a tree of {} files", wanted.len()), limit: seam::TREE_FILES_MAX as u64 }));
    }
    if bytes > seam::TREE_BYTES_MAX {
        return Err(refused(Refusal::TooBig { what: format!("a tree of {bytes} bytes"), limit: seam::TREE_BYTES_MAX }));
    }
    let input: Vec<u8> = wanted.iter().flat_map(|(sha, _)| sha.bytes().chain(std::iter::once(b'\n'))).collect();
    let out = super::diffs::git_with_input(hub, &["-C", dir, "cat-file", "--batch"], input).await.map_err(failed)?;
    let mut files = Vec::with_capacity(wanted.len());
    let mut rest: &[u8] = &out;
    for (sha, path) in wanted {
        let nl = rest.iter().position(|b| *b == b'\n').ok_or_else(|| failed(anyhow::anyhow!("git cat-file stopped short at {path}")))?;
        let header = String::from_utf8_lossy(rest.get(..nl).unwrap_or_default()).into_owned();
        let size: usize = match header.split_whitespace().collect::<Vec<_>>().as_slice() {
            [got, "blob", size] if *got == sha => size.parse().map_err(|_| failed(anyhow::anyhow!("git cat-file: {header}")))?,
            _ => return Err(failed(anyhow::anyhow!("git cat-file for {path}: {header}"))),
        };
        let body = rest.get(nl.saturating_add(1)..).unwrap_or_default();
        let content = body.get(..size).ok_or_else(|| failed(anyhow::anyhow!("git cat-file stopped short in {path}")))?;
        files.push(File { path, bytes: content.to_vec() });
        rest = body.get(size.saturating_add(1)..).unwrap_or_default();
    }
    Ok((files, skipped))
}

/// What a version changes: the lines it adds or rewrites (each file's, as inclusive ranges of its
/// new lines), and the files it touches.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Changed {
    pub lines: HashMap<String, Vec<(u32, u32)>>,
    pub files: HashSet<String>,
}

impl Changed {
    /// Whether any of lines `from` to `to` of `path` is one the version changes.
    fn touches(&self, path: &str, from: u32, to: u32) -> bool {
        self.lines.get(path).is_some_and(|ranges| ranges.iter().any(|(a, b)| *a <= to && from <= *b))
    }
}

/// [`Changed`] from `git diff --unified=0` (paths unquoted: `core.quotePath=false`).
pub(crate) fn changed_from_diff(diff: &str) -> Changed {
    let mut out = Changed::default();
    let mut path: Option<String> = None;
    for line in diff.lines() {
        if let Some(p) = line.strip_prefix("+++ ") {
            path = p.strip_prefix("b/").map(str::to_owned);
            if let Some(p) = &path {
                out.files.insert(p.clone());
            }
        } else if let (Some(hunk), Some(p)) = (line.strip_prefix("@@ "), path.as_ref()) {
            // `@@ -a[,b] +c[,d] @@`: d new lines from c (one when d is left out, none when 0).
            let Some(new) = hunk.split_whitespace().find_map(|w| w.strip_prefix('+')) else { continue };
            let (start, count) = new.split_once(',').map_or((new, "1"), |(s, c)| (s, c));
            let (Ok(start), Ok(count)) = (start.parse::<u32>(), count.parse::<u32>()) else { continue };
            if count > 0 {
                out.lines.entry(p.clone()).or_default().push((start, start.saturating_add(count).saturating_sub(1)));
            }
        }
    }
    out
}

/// What `commit` changes against `base` in the repository at `dir`, remembered; nothing when git
/// can't say (no base).
async fn changed(hub: &Hub, dir: &str, base: &str, commit: &str) -> Arc<Changed> {
    static CHANGED: OnceLock<Mutex<Vec<((String, String, String), Arc<Changed>)>>> = OnceLock::new();
    let key = (dir.to_owned(), base.to_owned(), commit.to_owned());
    if let Some((_, c)) = CHANGED.get_or_init(Default::default).lock().iter().find(|(k, _)| *k == key) {
        return Arc::clone(c);
    }
    let made = if base.is_empty() {
        Changed::default()
    } else {
        match gitops::run_git(hub, &["-c", "core.quotePath=false", "-C", dir, "diff", "--no-color", "--no-ext-diff", "--unified=0", "-M", base, commit], LS_REMOTE_TIMEOUT).await {
            Ok(out) => changed_from_diff(&String::from_utf8_lossy(&out)),
            Err(e) => {
                tracing::debug!(%base, %commit, error = %format!("{e:#}"), "a change's lines couldn't be read: its hits are in tree order");
                Changed::default()
            }
        }
    };
    let made = Arc::new(made);
    let mut all = CHANGED.get_or_init(Default::default).lock();
    if all.len() >= CHANGES_REMEMBERED {
        all.remove(0);
    }
    all.push((key, Arc::clone(&made)));
    made
}

/// What kiln keeps beside each index: the open changes it serves (`diff:<id>`,
/// `pull:<repo id>:<host>:<number>`) and the files it left out before the seam saw them.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Meta {
    trees: BTreeMap<String, TreeMeta>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct TreeMeta {
    owners: BTreeSet<String>,
    skipped: u64,
}

fn meta_path(hub: &Hub) -> PathBuf {
    hub.cfg.data("cache").join("search-meta.json")
}

/// The one writer of the meta file at a time.
fn meta_lock() -> &'static async_lock::Mutex<()> {
    static LOCK: OnceLock<async_lock::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| async_lock::Mutex::new(()))
}

/// `f` applied to the meta file, written back when it changed it (`f` says so).
async fn with_meta<R: Send + 'static>(hub: &Hub, f: impl FnOnce(&mut Meta) -> (bool, R) + Send + 'static) -> anyhow::Result<R> {
    let _g = meta_lock().lock().await;
    let path = meta_path(hub);
    crate::rt::blocking(move || -> anyhow::Result<R> {
        let mut meta: Meta = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let (dirty, out) = f(&mut meta);
        if dirty {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = path.with_extension(format!("json.{}", crate::util::new_id()));
            std::fs::write(&tmp, serde_json::to_vec(&meta)?)?;
            std::fs::rename(&tmp, &path)?;
        }
        Ok(out)
    })
    .await?
}

/// Where a search reads: a repository, a commit in it, what it's compared with, and the change it
/// is the search of.
struct Source<'a> {
    dir: &'a str,
    commit: &'a str,
    base: &'a str,
    owner: String,
}

/// One per tree while it's built: a second search of a tree being built waits for it.
fn building(tree: &str) -> Arc<async_lock::Mutex<()>> {
    static BUILDING: OnceLock<Mutex<HashMap<String, Arc<async_lock::Mutex<()>>>>> = OnceLock::new();
    let mut all = BUILDING.get_or_init(Default::default).lock();
    all.retain(|_, l| Arc::strong_count(l) > 1);
    Arc::clone(all.entry(tree.to_owned()).or_default())
}

/// Searches what `src` names: its tree's index built first when it isn't.
async fn search_at(hub: &Hub, src: Source<'_>, q: &HashMap<String, String>) -> Result<serde_json::Map<String, Value>, Fail> {
    let (query, limit) = asked(q)?;
    let tree = tree_of(hub, src.dir, src.commit).await?;
    let search = &hub.search;
    let mut index = "ready";
    let mut built = search.built(&tree).await.map_err(refused)?;
    if built.is_none() {
        let lock = building(&tree);
        let _g = lock.lock().await;
        built = search.built(&tree).await.map_err(refused)?;
        if built.is_none() {
            let (files, skipped) = files_of(hub, src.dir, &tree).await?;
            built = Some(search.build(&tree, files).await.map_err(refused)?);
            index = "built";
            let (t, owner) = (tree.clone(), src.owner.clone());
            with_meta(hub, move |m| {
                m.trees.insert(t, TreeMeta { owners: BTreeSet::from([owner]), skipped });
                (true, ())
            })
            .await
            .map_err(failed)?;
            sweep(hub).await;
        }
    }
    let (t, owner) = (tree.clone(), src.owner.clone());
    let kept = with_meta(hub, move |m| {
        let e = m.trees.entry(t).or_default();
        let fresh = e.owners.insert(owner);
        (fresh, e.skipped)
    })
    .await
    .map_err(failed)?;
    let found = search.search(&tree, query.clone()).await.map_err(refused)?;
    let changed = changed(hub, src.dir, src.base, src.commit).await;
    let truncated = found.truncated();
    let mut hits: Vec<(u8, Value)> = found
        .hits
        .into_iter()
        .map(|h| {
            let on_line = changed.touches(&h.path, h.line, h.end_line);
            let in_change = changed.files.contains(&h.path);
            let group = if on_line { 0 } else if in_change { 1 } else { 2 };
            let (symbol, kind) = h.symbol.map_or((Value::Null, Value::Null), |s| (json!(s.name), json!(s.kind)));
            (group, json!({ "path": h.path, "line": h.line, "column": h.column, "end_line": h.end_line, "text": h.text, "symbol": symbol, "kind": kind, "changed": on_line, "in_change": in_change }))
        })
        .collect();
    hits.sort_by_key(|(g, _)| *g);
    let shown = hits.len().min(limit);
    let hits: Vec<Value> = hits.into_iter().take(limit).map(|(_, v)| v).collect();
    let built = built.unwrap_or(seam::Built { files: found.files, skipped: 0, bytes: 0 });
    let mut out = serde_json::Map::new();
    for (k, v) in [
        ("tree", json!(tree)),
        ("commit", json!(src.commit)),
        ("base", json!(src.base)),
        ("q", json!(query.q)),
        ("mode", json!(query.mode)),
        ("lang", json!(query.lang)),
        ("index", json!(index)),
        ("files", json!(built.files)),
        ("skipped", json!(built.skipped.saturating_add(kept))),
        ("total", json!(found.total)),
        ("matched_files", json!(found.matched_files)),
        ("truncated", json!(truncated || (shown as u64) < found.total)),
        ("hits", json!(hits)),
    ] {
        out.insert(k.into(), v);
    }
    Ok(out)
}

/// `GET /api/diffs/:id/versions/:n/search` (W26-R07): a diff's files at version `v`, the lines it
/// changes against its parent first.
pub async fn version(hub: &Hub, d: &Diff, v: &Version, q: &HashMap<String, String>) -> Result<Value, Fail> {
    let (_, dir) = super::diffs::home(hub, d).await?;
    let mut out = search_at(hub, Source { dir: &dir, commit: &v.commit, base: &v.parent, owner: format!("diff:{}", d.id) }, q).await?;
    out.insert("diff".into(), json!(d.name));
    out.insert("version".into(), json!(v.n));
    Ok(Value::Object(out))
}

/// `GET /api/ci/repos/:repo/pulls/:n/search` (W26-R07): a host's pull request at its mirrored head,
/// the lines it changes against where its base branch meets it first.
pub async fn pull(hub: &Hub, repo_key: &str, n: &str, q: &HashMap<String, String>, caller: Option<&Caller>) -> Result<Value, Fail> {
    let repo = hub.store.repos().await.map_err(failed)?.into_iter().find(|r| r.name == repo_key || r.id == repo_key).ok_or((StatusCode::NOT_FOUND, "no such repository".to_owned()))?;
    if caller.is_some_and(|c| !c.can("diffs.read", &Scope::Repo { repo: repo.name.clone() })) {
        return Err((StatusCode::NOT_FOUND, "no such repository".to_owned()));
    }
    let number: i64 = n.parse().map_err(|_| (StatusCode::NOT_FOUND, format!("`{n}` isn't a pull request's number")))?;
    let repo_id = repo.id.clone();
    let row: Option<(String, String, String)> = hub
        .store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT p.host, p.base, h.sha FROM host_pulls p JOIN host_pull_heads h ON h.repo_id = p.repo_id AND h.host = p.host AND h.number = p.number WHERE p.repo_id = ?1 AND p.number = ?2 ORDER BY h.n DESC LIMIT 1",
                params![repo_id, number],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
        })
        .await
        .map_err(failed)?;
    let (host, base_branch, head) = row.ok_or_else(|| (StatusCode::NOT_FOUND, format!("no pull request #{number} mirrored for {}", repo.name)))?;
    let mirror = gitops::ensure_commit(hub, &repo, &head).await.map_err(|e| (StatusCode::BAD_GATEWAY, format!("its head {head} couldn't be fetched: {e:#}")))?;
    let dir = mirror.to_string_lossy().into_owned();
    let base = match gitops::run_git(hub, &["-C", &dir, "merge-base", &head, &format!("refs/heads/{base_branch}")], LS_REMOTE_TIMEOUT).await {
        Ok(out) if !base_branch.is_empty() => String::from_utf8_lossy(&out).trim().to_owned(),
        _ => String::new(),
    };
    let mut out = search_at(hub, Source { dir: &dir, commit: &head, base: &base, owner: format!("pull:{}:{host}:{number}", repo.id) }, q).await?;
    out.insert("repo".into(), json!(repo.name));
    out.insert("pull".into(), json!(number));
    out.insert("head".into(), json!(head));
    Ok(Value::Object(out))
}

/// Whether the change an owner names is still open (`diff:<id>`, `pull:<repo id>:<host>:<n>`).
async fn open(hub: &Hub, owner: &str) -> bool {
    let owner = owner.to_owned();
    hub.store
        .call(move |c| {
            let state: Option<String> = if let Some(id) = owner.strip_prefix("diff:") {
                c.query_row("SELECT state FROM diffs WHERE id = ?1", [id], |r| r.get(0)).optional()?
            } else if let Some(rest) = owner.strip_prefix("pull:") {
                let mut parts = rest.rsplitn(3, ':');
                let (Some(n), Some(host), Some(repo)) = (parts.next(), parts.next(), parts.next()) else { return Ok(false) };
                c.query_row("SELECT state FROM host_pulls WHERE repo_id = ?1 AND host = ?2 AND number = ?3", params![repo, host, n.parse::<i64>().unwrap_or(-1)], |r| r.get(0)).optional()?
            } else {
                None
            };
            Ok(state.is_some_and(|s| !matches!(s.as_str(), "landed" | "abandoned" | "merged" | "closed")))
        })
        .await
        .unwrap_or(true)
}

/// Forgets `owner`'s hold on each of `trees`, and every tree no open change holds then.
async fn release(hub: &Hub, owner: &str, trees: Vec<String>) {
    let who = owner.to_owned();
    let gone = with_meta(hub, move |m| {
        let mut gone = Vec::new();
        for t in trees {
            if let Some(e) = m.trees.get_mut(&t) {
                e.owners.remove(&who);
                if e.owners.is_empty() {
                    m.trees.remove(&t);
                    gone.push(t);
                }
            } else {
                gone.push(t);
            }
        }
        (true, gone)
    })
    .await
    .unwrap_or_default();
    for t in gone {
        if let Err(e) = hub.search.forget(&t).await {
            tracing::warn!(tree = %t, error = %e, "a closed change's search index stays");
        }
    }
}

/// A diff landed or was abandoned (W26-R07): its versions' indexes go with it.
pub async fn closed(hub: &Hub, d: &Diff) {
    let Ok((_, dir)) = super::diffs::home(hub, d).await else { return };
    let mut trees = Vec::new();
    for v in super::diffs::versions(hub, d).await.unwrap_or_default() {
        if let Ok(t) = tree_of(hub, &dir, &v.commit).await {
            trees.push(t);
        }
    }
    release(hub, &format!("diff:{}", d.id), trees).await;
}

/// Lets go of the changes that closed while no one was looking (a pull request merged on its host,
/// a diff closed before a server restart): each index whose changes are all closed goes.
pub async fn sweep(hub: &Hub) {
    let owners: Vec<(String, Vec<String>)> = with_meta(hub, |m| (false, m.trees.iter().map(|(t, e)| (t.clone(), e.owners.iter().cloned().collect())).collect())).await.unwrap_or_default();
    let mut closed: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (tree, list) in owners {
        for owner in list {
            if !open(hub, &owner).await {
                closed.entry(owner).or_default().push(tree.clone());
            }
        }
    }
    for (owner, trees) in closed {
        release(hub, &owner, trees).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W26-R07: the lines a version changes, read from its diff.
    #[test]
    fn w26_r07_changed_lines_from_a_diff() {
        let diff = "diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -3,0 +4,2 @@ fn f() {\n+a\n+b\n@@ -10 +12 @@\n-x\n+y\n@@ -20,3 +22,0 @@\n-gone\ndiff --git a/old.rs b/old.rs\ndeleted file mode 100644\n--- a/old.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-z\ndiff --git a/new dir/n.txt b/new dir/n.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new dir/n.txt\n@@ -0,0 +1 @@\n+hi\n";
        let c = changed_from_diff(diff);
        assert_eq!(c.lines.get("src/a.rs"), Some(&vec![(4, 5), (12, 12)]));
        assert_eq!(c.lines.get("new dir/n.txt"), Some(&vec![(1, 1)]));
        assert!(c.files.contains("src/a.rs") && c.files.contains("new dir/n.txt") && !c.files.contains("old.rs"));
        assert!(c.touches("src/a.rs", 5, 5) && c.touches("src/a.rs", 1, 4) && !c.touches("src/a.rs", 6, 11) && !c.touches("src/b.rs", 4, 4));
    }

    fn run_git(dir: &std::path::Path, args: &[&str]) -> anyhow::Result<String> {
        let o = std::process::Command::new("git")
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main"])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()?;
        anyhow::ensure!(o.status.success(), "git {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr));
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
    }

    /// W26-R07: a repository kiln doesn't host searches a pull request's mirrored head the same
    /// way: its changed lines first, and its index let go once the host says it merged.
    #[test]
    fn w26_r07_a_hosts_pull_request_at_its_mirrored_head() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let repo = crate::store::Repo { id: "up1".into(), name: "acme/up".into(), clone_url: "https://github.com/acme/up.git".into(), branches: vec!["main".into()], poll_seconds: 60, enabled: true, created_at: 0, last_polled_at: None, last_error: None, mode: "upstream".into(), default_branch: Some("main".into()), mirror: None };
            let repo = hub.store.upsert_repo(repo).await?;
            // The mirror: main, and the pull request's head on top of it.
            let work = tempfile::tempdir()?;
            let w = work.path();
            run_git(w, &["init", "-q"])?;
            std::fs::write(w.join("lib.rs"), "pub fn grid() -> usize {\n    // an empty matrix is a grid too\n    0\n}\n")?;
            run_git(w, &["add", "."])?;
            run_git(w, &["commit", "-qm", "main"])?;
            std::fs::write(w.join("expand.py"), "def expand(m):\n    # an empty matrix expands to nothing\n    return []\n")?;
            run_git(w, &["add", "."])?;
            run_git(w, &["commit", "-qm", "the pull request"])?;
            let head = run_git(w, &["rev-parse", "HEAD"])?;
            let mirror = gitops::mirror_path(&hub, &repo.name);
            if let Some(parent) = mirror.parent() {
                std::fs::create_dir_all(parent)?;
            }
            run_git(w, &["clone", "-q", "--bare", ".", &mirror.to_string_lossy()])?;
            run_git(&mirror, &["update-ref", "refs/heads/main", "HEAD~1"])?;
            let (id, h) = (repo.id.clone(), head.clone());
            hub.store
                .call(move |c| {
                    c.execute("INSERT INTO host_pulls (repo_id, host, number, title, author, state, base, first_seen) VALUES (?1, 'github', 7, 'expand', 'github:ada', 'open', 'main', 0)", [&id])?;
                    c.execute("INSERT INTO host_pull_heads (repo_id, host, number, n, sha, seen_at) VALUES (?1, 'github', 7, 1, ?2, 0)", params![id, h])?;
                    Ok(())
                })
                .await?;
            let q = HashMap::from([("q".to_owned(), "empty matrix".to_owned())]);
            let v = pull(&hub, "acme/up", "7", &q, None).await.map_err(|(s, m)| anyhow::anyhow!("{s}: {m}"))?;
            let got: Vec<(String, u64, String, bool)> = v
                .get("hits")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|h| {
                    let s = |k: &str| h.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
                    (s("path"), h.get("line").and_then(Value::as_u64).unwrap_or(0), s("symbol"), h.get("changed").and_then(Value::as_bool).unwrap_or(false))
                })
                .collect();
            assert_eq!(got, vec![("expand.py".to_owned(), 2, "expand".to_owned(), true), ("lib.rs".to_owned(), 2, "grid".to_owned(), false)], "{v}");
            assert_eq!((v.get("head").and_then(Value::as_str), v.get("pull").and_then(Value::as_i64), v.get("index").and_then(Value::as_str)), (Some(head.as_str()), Some(7), Some("built")));
            let tree = v.get("tree").and_then(Value::as_str).unwrap_or_default().to_owned();
            assert!(hub.search.built(&tree).await.map_err(|e| anyhow::anyhow!("{e}"))?.is_some());
            // Merged on its host: the next sweep lets its index go.
            let id = repo.id.clone();
            hub.store.call(move |c| Ok(c.execute("UPDATE host_pulls SET state = 'merged' WHERE repo_id = ?1 AND number = 7", [&id])?)).await?;
            sweep(&hub).await;
            assert!(hub.search.built(&tree).await.map_err(|e| anyhow::anyhow!("{e}"))?.is_none(), "a merged pull request's index goes");
            Ok(())
        })
    }

    /// W26-R07: what a search asks, and what it refuses.
    #[test]
    fn w26_r07_what_a_search_asks() {
        let q = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect::<HashMap<_, _>>();
        let (query, limit) = asked(&q(&[("q", "empty matrix")])).unwrap_or_else(|_| (Query { mode: Mode::Code, q: String::new(), lang: None, limit: 0 }, 0));
        assert_eq!((query.mode, query.q.as_str(), limit), (Mode::Text, "empty matrix", LIMIT));
        assert!(asked(&q(&[("q", "expand($A)"), ("mode", "code"), ("lang", "rust"), ("limit", "5")])).is_ok_and(|(qq, l)| qq.mode == Mode::Code && qq.lang.as_deref() == Some("rust") && l == 5));
        for bad_one in [q(&[]), q(&[("q", "  ")]), q(&[("q", "x"), ("mode", "fuzzy")]), q(&[("q", "x"), ("limit", "0")]), q(&[("q", "x"), ("limit", "1001")])] {
            assert!(asked(&bad_one).is_err_and(|(s, _)| s == StatusCode::BAD_REQUEST), "{bad_one:?}");
        }
    }
}
