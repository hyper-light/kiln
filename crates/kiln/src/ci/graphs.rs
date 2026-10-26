//! The package graph a run's `affected()` reads (docs/ci.md W24-R16, R17): its commit's manifests,
//! listed from the repository's mirror and read there with no checkout, then read as a graph by
//! `kiln_ci::work::graph`. A commit's graph never changes, so each is read once and kept (the newest
//! [`KEPT`] commits'); a run reads its commit's only when one of its workflows uses `affected()`.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, OnceLock};

use kiln_ci::wf::Workflow;
use kiln_ci::work::graph::{self, Graph};
use parking_lot::Mutex;

use crate::gitops::{self, LS_REMOTE_TIMEOUT};
use crate::hub::Hub;
use crate::store::Repo;

/// The commits whose graphs are kept, newest first.
const KEPT: usize = 64;
/// The most manifests one commit's graph reads, and the most bytes of them: past either, the
/// graph isn't read, and `affected()` says it couldn't be.
const MAX_MANIFESTS: usize = 20_000;
const MAX_BYTES: usize = 64 << 20;

/// (repository id, commit) → its graph, newest first.
fn kept() -> &'static Mutex<VecDeque<(String, String, Arc<Graph>)>> {
    static KEPT_GRAPHS: OnceLock<Mutex<VecDeque<(String, String, Arc<Graph>)>>> = OnceLock::new();
    KEPT_GRAPHS.get_or_init(Mutex::default)
}

/// Whether `wf`, or a workflow it calls, may use `affected()`: a run reads its commit's graph only
/// then. Function names are any case; a workflow that only mentions the word costs a read.
pub fn wanted(wf: &Workflow, called: &HashMap<String, Arc<Workflow>>) -> bool {
    std::iter::once(wf).chain(called.values().map(|w| &**w)).any(|w| serde_json::to_string(w).is_ok_and(|s| s.to_ascii_lowercase().contains("affected")))
}

/// The package graph of a run of `wf` at `sha`, when it [`wanted`] one.
pub async fn of_run(hub: &Hub, repo: &Repo, sha: &str, wf: &Workflow, called: &HashMap<String, Arc<Workflow>>) -> Option<Arc<Graph>> {
    if !wanted(wf, called) {
        return None;
    }
    at(hub, repo, sha).await
}

/// The package graph at `sha` (W24-R16): kept, else read from the mirror now. `None` when the
/// commit's files can't be listed or read; manifests that don't read are the graph's own problems,
/// which `affected()` reports.
pub async fn at(hub: &Hub, repo: &Repo, sha: &str) -> Option<Arc<Graph>> {
    if let Some(g) = kept().lock().iter().find(|(r, s, _)| *r == repo.id && s == sha).map(|(_, _, g)| Arc::clone(g)) {
        return Some(g);
    }
    match read(hub, repo, sha).await {
        Ok(g) => {
            let g = Arc::new(g);
            let mut k = kept().lock();
            if !k.iter().any(|(r, s, _)| *r == repo.id && s == sha) {
                k.push_front((repo.id.clone(), sha.to_owned(), Arc::clone(&g)));
                k.truncate(KEPT);
            }
            Some(g)
        }
        Err(e) => {
            tracing::warn!(repo = %repo.name, sha, error = %format!("{e:#}"), "the commit's package graph couldn't be read: affected() says so");
            None
        }
    }
}

/// The commit's manifests (`git ls-tree`), their texts (`git cat-file --batch`), and the graph
/// they make.
async fn read(hub: &Hub, repo: &Repo, sha: &str) -> anyhow::Result<Graph> {
    let mirror = gitops::mirror_path(hub, &repo.name).to_string_lossy().into_owned();
    let listing = gitops::run_git(hub, &["-C", &mirror, "ls-tree", "-r", "-z", "--full-tree", sha], LS_REMOTE_TIMEOUT).await?;
    let manifests = manifests(&listing);
    anyhow::ensure!(manifests.len() <= MAX_MANIFESTS, "the commit has {} manifests, more than the {MAX_MANIFESTS} a graph reads", manifests.len());
    let oids: Vec<String> = manifests.iter().map(|(_, oid)| oid.clone()).collect();
    let mut cmd = gitops::git(hub);
    cmd.args(["-C", &mirror, "cat-file", "--batch"]);
    let texts = crate::rt::blocking(move || batch(cmd, &oids)).await??;
    let by_path: HashMap<String, String> = manifests.into_iter().filter_map(|(path, oid)| texts.get(&oid).map(|t| (path, t.clone()))).collect();
    let paths: Vec<String> = by_path.keys().cloned().collect();
    crate::rt::blocking(move || graph::read(&|p| by_path.get(p).cloned(), &paths)).await
}

/// The manifests of a `git ls-tree -r -z` listing (`<mode> <type> <oid>\t<path>`, each ended by a
/// NUL): (path, blob id) of each blob [`graph::read`] may read.
fn manifests(listing: &[u8]) -> Vec<(String, String)> {
    listing
        .split(|b| *b == 0)
        .filter_map(|entry| {
            let text = std::str::from_utf8(entry).ok()?;
            let (meta, path) = text.split_once('\t')?;
            let mut m = meta.split(' ');
            let (_, kind, oid) = (m.next()?, m.next()?, m.next()?);
            (kind == "blob" && graph::is_manifest(path)).then(|| (path.to_owned(), oid.to_owned()))
        })
        .collect()
}

/// Each object of `oids` as text, read in one `git cat-file --batch` (`cmd`): its answer is, per
/// object, `<oid> <type> <size>\n`, the bytes, `\n` (`<oid> missing\n` for one it hasn't).
fn batch(mut cmd: std::process::Command, oids: &[String]) -> anyhow::Result<HashMap<String, String>> {
    use std::io::{Read as _, Write as _};
    cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
    let mut child = cmd.spawn()?;
    let input: String = oids.iter().map(|o| format!("{o}\n")).collect();
    // Written beside the reading, so neither pipe fills while the other waits.
    let writer = child.stdin.take().map(|mut stdin| std::thread::spawn(move || stdin.write_all(input.as_bytes())));
    let mut out = Vec::new();
    // At most a little over the limit: past it, git is stopped rather than read to its end.
    let read = child.stdout.take().map(|mut s| s.by_ref().take(MAX_BYTES as u64 + 1).read_to_end(&mut out));
    // Ended (its stdin closed, it has said all), or stopped: either way the writer's done.
    let _ = child.kill();
    let _ = child.wait();
    let wrote = writer.map(|w| w.join());
    read.transpose()?;
    anyhow::ensure!(out.len() <= MAX_BYTES, "the commit's manifests are over {} MiB", MAX_BYTES >> 20);
    if let Some(Ok(Err(e))) = wrote {
        return Err(e.into());
    }
    Ok(parse_batch(&out))
}

/// `git cat-file --batch`'s answer, by object id; objects missing, or not text, are left out.
fn parse_batch(mut out: &[u8]) -> HashMap<String, String> {
    let mut texts = HashMap::new();
    while let Some(nl) = out.iter().position(|b| *b == b'\n') {
        let header = String::from_utf8_lossy(out.get(..nl).unwrap_or_default()).into_owned();
        out = out.get(nl + 1..).unwrap_or_default();
        let mut words = header.split(' ');
        let (Some(oid), Some(_kind), Some(size)) = (words.next(), words.next(), words.next().and_then(|s| s.parse::<usize>().ok())) else { continue };
        let Some(body) = out.get(..size) else { break };
        if let Ok(text) = std::str::from_utf8(body) {
            texts.insert(oid.to_owned(), text.to_owned());
        }
        out = out.get(size + 1..).unwrap_or_default();
    }
    texts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w24_r16_manifests_of_a_listing() {
        let entries = ["100644 blob aaa\tCargo.toml", "100644 blob bbb\tcrates/a/Cargo.toml", "100644 blob ccc\tcrates/a/src/lib.rs", "040000 tree ddd\tcrates/Cargo.toml", "100644 blob eee\tui/node_modules/x/package.json", "160000 commit fff\tvendor/sub"];
        let listing: Vec<u8> = entries.iter().flat_map(|e| e.bytes().chain([0])).collect();
        let got = manifests(&listing);
        assert_eq!(got, [("Cargo.toml".to_owned(), "aaa".to_owned()), ("crates/a/Cargo.toml".to_owned(), "bbb".to_owned()), ("ui/node_modules/x/package.json".to_owned(), "eee".to_owned())], "blobs named as manifests; graph::read leaves node_modules out");
    }

    #[test]
    fn w24_r16_a_batch_answer() {
        let out = b"aaa blob 5\nhello\nbbb missing\nccc blob 0\n\nddd blob 3\n\xff\xfe\x00\neee blob 2\nok\n";
        let got = parse_batch(out);
        assert_eq!(got.get("aaa").map(String::as_str), Some("hello"));
        assert_eq!(got.get("ccc").map(String::as_str), Some(""));
        assert_eq!(got.get("eee").map(String::as_str), Some("ok"), "after a missing object and one that isn't text");
        assert!(!got.contains_key("bbb") && !got.contains_key("ddd"));
        assert!(parse_batch(b"aaa blob 99\nshort").is_empty(), "a body cut short is left out, never read past");
    }

    /// W24-R16: a commit's graph, read from the mirror as a run reads it, and kept for the next.
    #[test]
    fn w24_r16_a_commits_graph_from_its_mirror() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let repo = Repo { id: "r1".into(), name: "acme/ws".into(), clone_url: String::new(), branches: Vec::new(), poll_seconds: 0, enabled: true, created_at: 0, last_polled_at: None, last_error: None, mode: "native".into(), default_branch: None, mirror: None };
            let mirror = gitops::mirror_path(&hub, &repo.name);
            let work = tempfile::tempdir()?;
            let git = |dir: &std::path::Path, args: &[&str]| -> anyhow::Result<String> {
                let o = std::process::Command::new("git").args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "init.defaultBranch=main"]).args(args).current_dir(dir).env("GIT_CONFIG_NOSYSTEM", "1").output()?;
                anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
                Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
            };
            let files = [
                ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
                ("crates/a/Cargo.toml", "[package]\nname = \"a\"\n"),
                ("crates/a/src/lib.rs", ""),
                ("crates/b/Cargo.toml", "[package]\nname = \"b\"\n\n[dependencies]\na = { path = \"../a\" }\n"),
                ("crates/b/src/lib.rs", ""),
            ];
            for (p, text) in files {
                let at = work.path().join(p);
                if let Some(d) = at.parent() {
                    std::fs::create_dir_all(d)?;
                }
                std::fs::write(at, text)?;
            }
            git(work.path(), &["init", "-q"])?;
            git(work.path(), &["add", "."])?;
            git(work.path(), &["commit", "-qm", "two crates"])?;
            let sha = git(work.path(), &["rev-parse", "HEAD"])?;
            std::fs::create_dir_all(mirror.parent().unwrap_or(work.path()))?;
            git(work.path(), &["clone", "-q", "--bare", ".", &mirror.to_string_lossy()])?;
            let g = at(&hub, &repo, &sha).await.ok_or_else(|| anyhow::anyhow!("no graph"))?;
            anyhow::ensure!(g.problems.is_empty(), "{:?}", g.problems);
            let eco = graph::Ecosystem::Cargo;
            let names: Vec<&str> = g.of(eco).map(|p| p.name.as_str()).collect();
            anyhow::ensure!(names == ["a", "b"], "{names:?}");
            let hit: Vec<String> = graph::affected(&g, eco, &["crates/a/src/lib.rs".to_owned()]).into_iter().collect();
            anyhow::ensure!(hit == ["a", "b"], "a, and b, which depends on it: {hit:?}");
            // Kept: the same graph again, though the mirror is gone.
            std::fs::remove_dir_all(&mirror)?;
            let again = at(&hub, &repo, &sha).await.ok_or_else(|| anyhow::anyhow!("not kept"))?;
            anyhow::ensure!(Arc::ptr_eq(&g, &again));
            anyhow::ensure!(at(&hub, &repo, "0000000000000000000000000000000000000000").await.is_none(), "a commit the mirror hasn't: no graph");
            Ok(())
        })
    }
}
