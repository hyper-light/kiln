//! W29-AC20/AC22 (plan 01a19a37's step 12): kiln beside the pinned `gh actions-lock`, on the same
//! trees and the same source facts. `tests/fixtures/locks-interop` holds each case's tree, its
//! recorded facts and what upstream wrote and found there (its ORIGIN.md says how); these replay
//! the facts through the dependency-source simulator and compare kiln's bytes, reading and verdicts.

use kiln_ci::lock::regenerate;
use kiln_seams::dependencies::{SimDependencySources, SimRepository, contract};
use serde_json::{Value, json};

use super::*;

#[derive(Clone, serde::Deserialize)]
struct Facts { repositories: BTreeMap<String, Source> }

#[derive(Clone, serde::Deserialize)]
struct Source { owner_id: i64, repo_id: i64, default_branch: String, refs: BTreeMap<String, String>, commits: BTreeMap<String, Vec<String>>, files: BTreeMap<String, BTreeMap<String, String>> }

#[derive(serde::Deserialize)]
struct Scenario { name: String, #[serde(default)] lock: String, changes: Vec<Change> }

#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct Change { workflow: Option<Edit>, #[serde(rename = "move")] moved: Option<Move>, identity: Option<Replaced> }

#[derive(serde::Deserialize)]
struct Edit { path: String, from: String, to: String }

#[derive(serde::Deserialize)]
struct Move { repository: String, #[serde(rename = "ref")] reference: String, to: String, parents: Vec<String>, files: BTreeMap<String, String> }

#[derive(serde::Deserialize)]
struct Replaced { repository: String, owner_id: i64, repo_id: i64 }

/// The facts' hosted repositories, Node's archive, and any image: job images are kiln's own
/// supplement (`.kiln/lock`), which GitHub's lock doesn't cover.
struct Simulated(SimDependencySources);

impl DependencySources for Simulated {
    fn resolve(&self, repository: Repository, reference: String) -> BoxFuture<'_, Result<Revision, Refusal>> { self.0.resolve(repository, reference) }
    fn contains(&self, revision: Revision, commit: String) -> BoxFuture<'_, Result<bool, Refusal>> { self.0.contains(revision, commit) }
    fn tags_at(&self, revision: Revision, commit: String) -> BoxFuture<'_, Result<Vec<String>, Refusal>> { self.0.tags_at(revision, commit) }
    fn file(&self, revision: Revision, commit: String, path: String) -> BoxFuture<'_, Result<Vec<u8>, Refusal>> { self.0.file(revision, commit, path) }
    fn manifest(&self, _reference: String) -> BoxFuture<'_, Result<Manifest, Refusal>> { Box::pin(async { Ok(contract::fixture_manifest()) }) }
    fn runtime(&self, runtime: Runtime) -> BoxFuture<'_, Result<Archive, Refusal>> { self.0.runtime(runtime) }
}

fn simulated(facts: &Facts) -> anyhow::Result<Simulated> {
    let source = SimDependencySources::default();
    for (name, s) in &facts.repositories {
        let files = s.files.iter().flat_map(|(commit, files)| files.iter().map(move |(path, text)| ((commit.clone(), path.clone()), text.as_bytes().to_vec()))).collect();
        let repository = SimRepository { identity: Identity::Github { hostname: "github.com".into(), owner_id: s.owner_id, repo_id: s.repo_id }, refs: s.refs.clone(), default_branch: s.default_branch.clone(), commits: s.commits.clone(), files };
        source.put_repository(Repository { name: name.clone(), native: false }, repository).map_err(|e| anyhow::anyhow!("{name}: {e:?}"))?;
    }
    // Node's archive, also by the version kiln's lock records for it.
    let archive = contract::fixture_archive();
    for runtime in [contract::fixture_runtime(), Runtime { version: Some(archive.version.clone()), ..contract::fixture_runtime() }] {
        source.put_runtime(runtime, archive.clone()).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    Ok(Simulated(source))
}

fn cases() -> std::path::PathBuf { Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/locks-interop/cases") }

fn read(path: &Path) -> anyhow::Result<String> { std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display())) }

/// A case's tree as committed files, less the lock upstream's own fixture carries.
fn tree(root: &Path) -> anyhow::Result<Vec<(String, String)>> {
    let (mut files, mut todo) = (Vec::new(), vec![root.to_path_buf()]);
    while let Some(dir) = todo.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() { todo.push(path); continue; }
            let rel = path.strip_prefix(root)?.to_string_lossy().replace('\\', "/");
            if rel != github::PATH { files.push((rel, read(&path)?)); }
        }
    }
    files.sort();
    Ok(files)
}

async fn publish(hub: &Arc<Hub>, root: &Path, name: &str, files: Vec<(String, String)>) -> anyhow::Result<(Repo, String)> {
    let dir = root.join(name);
    write(&dir, files).await?;
    git(hub, &dir, &["init", "-q", "-b", "main"]).await?;
    git(hub, &dir, &["add", "."]).await?;
    git(hub, &dir, &["commit", "-qm", "interoperability tree"]).await?;
    let commit = git(hub, &dir, &["rev-parse", "HEAD"]).await?;
    let repo = hub.store.upsert_repo(Repo {
        id: format!("interop-{name}"), name: format!("acme/{name}"), clone_url: format!("file://{}", dir.display()), branches: vec!["main".into()],
        poll_seconds: 60, enabled: true, created_at: 0, last_polled_at: None, last_error: None, mode: "upstream".into(), default_branch: Some("main".into()), mirror: None,
    }).await?;
    crate::gitops::sync_mirror(hub, &repo).await?;
    Ok((repo, commit))
}

/// kiln's reading of a lock is the official parser's: every field, and each workflow's closure.
fn reading(at: &str, lock: &str, official: &str) -> anyhow::Result<()> {
    let official: Value = serde_json::from_str(official)?;
    let file = github::parse(lock).map_err(|e| anyhow::anyhow!("{at}: kiln refuses a lock the official parser reads: {e:?}"))?;
    let dependencies: serde_json::Map<_, _> = file.dependencies.iter().map(|(key, a)| (key.clone(), json!({
        "hostname": a.hostname.clone().unwrap_or_default(), "ref": a.git_ref, "commit": a.commit, "owner_id": a.owner_id, "repo_id": a.repo_id, "uses": a.uses,
    }))).collect();
    let closure: serde_json::Map<_, _> = file.workflows.keys().map(|w| (w.clone(), json!(file.closure(w).unwrap_or_default()))).collect();
    let ours = json!({ "version": file.version, "workflows": file.workflows, "dependencies": dependencies, "closure": closure });
    assert_eq!(ours, official, "{at}: kiln reads the lock unlike the official parser");
    Ok(())
}

/// gh actions-lock's documented findings, as kiln's codes (docs/ci.md, W29-AC20).
fn code(category: &str) -> Option<&'static str> {
    Some(match category {
        "not-pinned" | "ref-changed" => "KCI4050",
        "unreachable-pin" => "KCI4051",
        "repo-replaced" => "KCI4052",
        "tag-commit-mismatch" => "KCI4053",
        "stale" => "KCI4055",
        "ref-moved" => "KCI4057",
        _ => return None,
    })
}

fn codes(text: &str) -> Vec<&str> {
    text.match_indices("KCI").filter_map(|(i, _)| text.get(i..i.saturating_add(7))).filter(|c| c.get(3..).is_some_and(|d| d.bytes().all(|b| b.is_ascii_digit()))).collect()
}

static NULL: Value = Value::Null;

fn field<'a>(value: &'a Value, key: &str) -> &'a Value { value.get(key).unwrap_or(&NULL) }

fn text<'a>(value: &'a Value, key: &str) -> &'a str { field(value, key).as_str().unwrap_or_default() }

/// kiln's verify answers as `--verify` does, refusing each documented case with its code; a
/// moved partial ref warns as a read-only relock does. kiln names a workflow's first refusal,
/// so every row it gives must be one upstream gives too.
fn agree(at: &str, scenario: &str, upstream: &Value, ours: &Value) {
    let verify = field(upstream, "verify");
    assert_eq!(ours.get("fresh").and_then(Value::as_bool), verify.get("valid").and_then(Value::as_bool), "{at}: kiln's fresh isn't --verify's valid\n{ours}\n{verify}");
    let findings = |mode: &Value, severity: &str| -> Vec<(String, String, &'static str)> {
        field(mode, "findings").as_array().into_iter().flatten().filter(|f| text(f, "severity") == severity)
            .filter_map(|f| Some((text(f, "workflow").to_owned(), text(f, "dependency").to_owned(), code(text(f, "category"))?))).collect()
    };
    let errors = findings(verify, "error");
    let rows = field(ours, "stale").as_array().cloned().unwrap_or_default();
    for row in &rows {
        let (workflow, uses, error) = (text(row, "workflow"), text(row, "uses"), text(row, "error"));
        assert!(errors.iter().any(|(w, d, c)| w == workflow && d == uses && codes(error).contains(c)), "{at}: upstream doesn't find kiln's {row}\n{verify}");
    }
    assert_eq!(rows.is_empty(), errors.is_empty(), "{at}: one refuses and the other doesn't\n{ours}\n{verify}");
    if let Some(wanted) = code(scenario).filter(|c| *c != "KCI4057") {
        assert!(errors.iter().any(|(_, _, c)| *c == wanted), "{at}: upstream doesn't give its documented {scenario}\n{verify}");
        assert!(rows.iter().any(|r| codes(text(r, "error")).contains(&wanted)), "{at}: kiln doesn't refuse with {wanted}\n{ours}");
    }
    let moved = findings(field(upstream, "check"), "warning");
    let warned: Vec<&str> = field(ours, "warnings").as_array().into_iter().flatten().filter_map(Value::as_str).filter(|w| codes(w).contains(&"KCI4057")).collect();
    for w in &warned { assert!(moved.iter().any(|(_, d, _)| w.contains(d.as_str())), "{at}: upstream doesn't warn of kiln's {w}"); }
    if scenario == "ref-moved" {
        assert!(!moved.is_empty(), "{at}: upstream doesn't warn of the moved partial ref");
        for (_, d, _) in &moved { assert!(warned.iter().any(|w| w.contains(d.as_str())), "{at}: kiln doesn't warn (KCI4057) of {d}\n{ours}"); }
    }
}

fn case(name: &'static str) -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let dir = cases().join(name);
        let (expected, given) = (dir.join("expected"), dir.join("tree").join(github::PATH));
        let facts: Facts = serde_json::from_str(&read(&dir.join("facts.json"))?)?;
        let scenarios: Vec<Scenario> = serde_json::from_str(&read(&dir.join("scenarios.json"))?)?;
        let files = tree(&dir.join("tree"))?;
        let root = tempfile::tempdir()?;
        let hub = crate::hub::tests::scratch_hub().await?;

        // The writer: for the bare tree, kiln's actions.lock is upstream's, byte for byte.
        let (repo, commit) = publish(&hub, root.path(), name, files.clone()).await?;
        let generated = super::super::generate::collect(&hub, &simulated(&facts)?, &repo, &commit, BTreeMap::new(), None, regenerate::Mode::Preserve).await.map_err(anyhow::Error::msg)?;
        let golden = read(&expected.join("actions.lock"))?;
        assert_eq!(generated.github.as_deref(), Some(golden.as_str()), "{name}: kiln's actions.lock isn't gh actions-lock's");

        reading(name, &golden, &read(&expected.join("read.json"))?)?;
        if given.exists() { reading(&format!("{name} (upstream's lock)"), &read(&given)?, &read(&expected.join("read-given.json"))?)?; }

        let verdicts: Value = serde_json::from_str(&read(&expected.join("verify.json"))?)?;
        for scenario in &scenarios {
            let at = format!("{name}/{}", scenario.name);
            let mut files = files.clone();
            files.push((github::PATH.into(), if scenario.lock == "given" { read(&given)? } else { golden.clone() }));
            files.extend(generated.kiln.clone().map(|k| (kiln_ci::lock::kiln::PATH.to_owned(), k)));
            let mut facts = facts.clone();
            for change in &scenario.changes {
                if let Some(edit) = &change.workflow {
                    let file = files.iter_mut().find(|(p, _)| *p == edit.path).ok_or_else(|| anyhow::anyhow!("{at}: no {}", edit.path))?;
                    anyhow::ensure!(file.1.contains(&edit.from), "{at}: {} lacks {:?}", edit.path, edit.from);
                    file.1 = file.1.replacen(&edit.from, &edit.to, 1);
                }
                if let Some(m) = &change.moved {
                    let source = facts.repositories.get_mut(&m.repository).ok_or_else(|| anyhow::anyhow!("{at}: no {}", m.repository))?;
                    source.commits.insert(m.to.clone(), m.parents.clone());
                    source.files.insert(m.to.clone(), m.files.clone());
                    source.refs.insert(m.reference.clone(), m.to.clone());
                }
                if let Some(r) = &change.identity {
                    let source = facts.repositories.get_mut(&r.repository).ok_or_else(|| anyhow::anyhow!("{at}: no {}", r.repository))?;
                    (source.owner_id, source.repo_id) = (r.owner_id, r.repo_id);
                }
            }
            let (repo, commit) = publish(&hub, root.path(), &format!("{name}-{}", scenario.name), files).await?;
            let ours = super::super::api::verify_with(&hub, Arc::new(simulated(&facts)?), &repo, &commit).await.map_err(|e| anyhow::anyhow!("{at}: {} {}", e.0, e.1))?;
            agree(&at, &scenario.name, field(&verdicts, &scenario.name), &ours);
        }
        Ok(())
    })
}

#[test]
fn w29_ac20_upstreams_own_fixture_reads_writes_and_verifies_alike() -> anyhow::Result<()> { case("upstream-golden-json") }

#[test]
fn w29_ac20_a_nested_composite_and_a_subdirectory_action_write_alike() -> anyhow::Result<()> { case("nested-composite") }

#[test]
fn w29_ac20_f02_mantle_writes_and_verifies_alike() -> anyhow::Result<()> { case("f02-mantle") }

#[test]
fn w29_ac20_f02_slates_branches_and_a_local_reusable_workflow_write_alike() -> anyhow::Result<()> { case("f02-slates") }

#[test]
fn w29_ac20_f02_kilns_full_commit_pins_record_their_tags_alike() -> anyhow::Result<()> { case("f02-kiln") }

/// focal's release workflow takes its matrix from a job's outputs, so the images its jobs run on
/// aren't known until it runs. kiln writes neither lock then (W29-R24: no partial pair), where
/// gh actions-lock, which locks no image, writes its file; kiln still reads that file alike.
#[test]
fn w29_ac20_f02_focal_reads_alike_and_kiln_refuses_to_lock_run_time_images() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let dir = cases().join("f02-focal");
        let expected = dir.join("expected");
        reading("f02-focal", &read(&expected.join("actions.lock"))?, &read(&expected.join("read.json"))?)?;
        let facts: Facts = serde_json::from_str(&read(&dir.join("facts.json"))?)?;
        let root = tempfile::tempdir()?;
        let hub = crate::hub::tests::scratch_hub().await?;
        let (repo, commit) = publish(&hub, root.path(), "f02-focal", tree(&dir.join("tree"))?).await?;
        let refused = super::super::generate::collect(&hub, &simulated(&facts)?, &repo, &commit, BTreeMap::new(), None, regenerate::Mode::Preserve).await;
        assert!(refused.as_ref().is_err_and(|why| why.contains("KCI4050") && why.contains("release.yml") && why.contains("data unavailable when locks are written")), "{refused:?}");
        Ok(())
    })
}

#[test]
fn w29_ac20_ac22_each_documented_case_of_f02_hyper_raft_gets_its_code() -> anyhow::Result<()> { case("f02-hyper-raft") }
