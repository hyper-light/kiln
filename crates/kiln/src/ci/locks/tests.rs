//! W29-R23: real Git commits, moved tags, workflow-owned locks and the saved run boundary.

use futures::future::BoxFuture;
use kiln_ci::expr::Dialect;
use kiln_ci::plan::{PlanAction, PlanSource, RunInput, RunPlan, Settings};
use kiln_seams::dependencies::{Archive, Manifest, Refusal, Runtime};
use parking_lot::Mutex;

use super::*;
use crate::ci::dependency_sources::tests::{Live, git};

const PATH: &str = ".github/workflows/ci.yml";
const USES: &str = "acme/tools@v1";
const WORKFLOW: &str = "on: push\njobs:\n  check:\n    runs-on: baremetal\n    steps:\n      - uses: acme/tools@v1\n";

struct Counted<S> {
    inner: S,
    reads: Arc<Mutex<Vec<(String, String)>>>,
}

impl<S: DependencySources> DependencySources for Counted<S> {
    fn resolve(&self, repository: Repository, reference: String) -> BoxFuture<'_, Result<Revision, Refusal>> { self.inner.resolve(repository, reference) }
    fn contains(&self, revision: Revision, commit: String) -> BoxFuture<'_, Result<bool, Refusal>> { self.inner.contains(revision, commit) }
    fn file(&self, revision: Revision, commit: String, path: String) -> BoxFuture<'_, Result<Vec<u8>, Refusal>> {
        self.reads.lock().push((commit.clone(), path.clone()));
        self.inner.file(revision, commit, path)
    }
    fn manifest(&self, reference: String) -> BoxFuture<'_, Result<Manifest, Refusal>> { self.inner.manifest(reference) }
    fn runtime(&self, runtime: Runtime) -> BoxFuture<'_, Result<Archive, Refusal>> { self.inner.runtime(runtime) }
}

struct Caller { repo: Repo, commit: String, model: Arc<Workflow> }

fn model(path: &str, text: &str) -> anyhow::Result<Arc<Workflow>> {
    let dialect = if path.starts_with(".kiln/") { Dialect::Kiln } else { Dialect::Gha };
    let (model, diagnostics) = wf::parse_workflow(path, text, dialect);
    anyhow::ensure!(!diagnostics.iter().any(|d| d.is_error()), "{diagnostics:?}");
    model.map(Arc::new).ok_or_else(|| anyhow::anyhow!("no workflow"))
}

fn lock(path: &str, reference: &str, commit: &str) -> anyhow::Result<String> {
    let pin = format!("acme/tools@{reference}");
    github::File {
        workflows: [(path.into(), vec![pin.clone()])].into(),
        dependencies: [(pin, github::Action { git_ref: reference.into(), commit: format!("sha1-{commit}"), owner_id: 41, repo_id: 42, ..Default::default() })].into(),
        ..Default::default()
    }.write("github.com").map_err(|e| anyhow::anyhow!("{e:?}"))
}

async fn write(dir: &Path, files: Vec<(String, String)>) -> anyhow::Result<()> {
    let dir = dir.to_path_buf();
    crate::rt::blocking(move || -> anyhow::Result<()> {
        for (path, text) in files {
            let path = dir.join(path);
            if let Some(p) = path.parent() { std::fs::create_dir_all(p)?; }
            std::fs::write(path, text)?;
        }
        Ok(())
    }).await?
}

async fn caller(hub: &Arc<Hub>, root: &Path, text: &str, locked: Option<String>) -> anyhow::Result<Caller> {
    let dir = root.join("caller");
    write(&dir, vec![(PATH.into(), text.into())]).await?;
    if let Some(lock) = locked { write(&dir, vec![(github::PATH.into(), lock)]).await?; }
    git(hub, &dir, &["init", "-q", "-b", "main"]).await?;
    git(hub, &dir, &["add", "."]).await?;
    git(hub, &dir, &["commit", "-qm", "caller"]).await?;
    let commit = git(hub, &dir, &["rev-parse", "HEAD"]).await?;
    let repo = hub.store.upsert_repo(Repo {
        id: "lock-caller".into(), name: "acme/app".into(), clone_url: format!("file://{}", dir.display()), branches: vec!["main".into()],
        poll_seconds: 60, enabled: true, created_at: 0, last_polled_at: None, last_error: None, mode: "upstream".into(), default_branch: Some("main".into()), mirror: None,
    }).await?;
    crate::gitops::sync_mirror(hub, &repo).await?;
    Ok(Caller { repo, commit, model: model(PATH, text)? })
}

struct SavedSource<'a>(&'a Files);
impl PlanSource for SavedSource<'_> {
    fn workflow(&self, uses: &str) -> Result<Arc<Workflow>, String> { self.0.get(uses).cloned().ok_or_else(|| format!("no captured {uses}")) }
}

fn input(caller: &Caller) -> RunInput {
    input_for(caller, PATH)
}

fn input_for(caller: &Caller, path: &str) -> RunInput {
    RunInput { github: serde_json::json!({ "repository": caller.repo.name, "sha": caller.commit, "ref": "refs/heads/main", "workflow_ref": format!("{}/{path}@refs/heads/main", caller.repo.name) }), trusted: true, ..Default::default() }
}

#[test]
fn w29_r23_a_moved_tag_cannot_change_policy_metadata_or_the_saved_run() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let live = Live::for_locks().await?;
        let hub = live.hub();
        let old = live.commits.old.clone();
        let new = live.commits.new.clone();
        let origin = live.origin.clone();
        let caller = caller(&hub, live.root(), WORKFLOW, Some(lock(PATH, "v1", &old)?)).await?;
        let policy = serde_json::to_string(&kiln_ci::policy::Policy { allow: vec![format!("acme/tools@{old}")], require_sha_pins: true, ..Default::default() })?;
        let id = caller.repo.id.clone();
        hub.store.call(move |c| Ok(c.execute("INSERT INTO ci_action_policies (repo_id, policy, updated_at, updated_by) VALUES (?1, ?2, 0, 'lock-test')", [id, policy])?)).await?;
        // The partial ref has moved before preflight. A remains the code and policy subject.
        git(&hub, &origin, &["tag", "-fam", "moved", "v1", &new]).await?;
        let reads = Arc::new(Mutex::new(Vec::new()));
        let source = Counted { inner: live.sources, reads: Arc::clone(&reads) };
        let frozen = prepare_with(&hub, &source, &caller.repo, &caller.commit, PATH, Arc::clone(&caller.model)).await.map_err(anyhow::Error::msg)?;
        assert_eq!(frozen.root_actions().map_err(anyhow::Error::msg)?.get(USES).map(|a| &a.sha), Some(&old));
        assert!(!frozen.warnings.is_empty());
        assert!(reads.lock().iter().all(|(sha, _)| sha == &old));
        assert!(!reads.lock().is_empty());
        let saved = serde_json::json!({ "dependency_snapshot": frozen });
        let count = reads.lock().len();
        // Planning and a new reader of saved input use only the accepted snapshot.
        let restored = Frozen::saved(&saved, &caller.repo.name, PATH, &caller.commit).map_err(anyhow::Error::msg)?.ok_or_else(|| anyhow::anyhow!("snapshot missing"))?;
        let (_, acts) = RunPlan::start(Arc::clone(&caller.model), input(&caller), Settings::default(), &SavedSource(&restored.files)).map_err(anyhow::Error::msg)?;
        let leg = acts.iter().find_map(|a| if let PlanAction::Queue(l) = a { Some(l) } else { None }).ok_or_else(|| anyhow::anyhow!("no leg"))?;
        assert_eq!(restored.actions_for(&leg.defined_in).and_then(|a| a.get(USES)).map(|a| &a.sha), Some(&old));
        assert_eq!(reads.lock().len(), count);
        assert!(refresh_allowed(&hub, &caller.repo, &caller.commit, PATH, Some(&restored)).await.is_err());
        Ok(())
    })
}

#[test]
fn w29_r23_full_tag_movement_refuses_before_any_dependency_metadata() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let live = Live::for_locks().await?;
        let hub = live.hub();
        git(&hub, &live.origin, &["tag", "-am", "version", "v1.2.3", &live.commits.new]).await?;
        let text = WORKFLOW.replace("@v1", "@v1.2.3");
        let caller = caller(&hub, live.root(), &text, Some(lock(PATH, "v1.2.3", &live.commits.old)?)).await?;
        let reads = Arc::new(Mutex::new(Vec::new()));
        let source = Counted { inner: live.sources, reads: Arc::clone(&reads) };
        let result = prepare_with(&hub, &source, &caller.repo, &caller.commit, PATH, caller.model).await;
        assert!(result.err().is_some_and(|e| e.contains("KCI4053")));
        assert!(reads.lock().is_empty());
        Ok(())
    })
}

#[test]
fn w29_r23_absent_snapshots_keep_old_runs_compatible_and_present_bad_ones_refuse() -> anyhow::Result<()> {
    let sha = "a".repeat(40);
    assert!(Frozen::saved(&serde_json::json!({}), "acme/app", PATH, &sha).map_err(anyhow::Error::msg)?.is_none());
    for raw in [serde_json::json!({}), serde_json::json!({ "version": 2 }), serde_json::json!("bad")] {
        assert!(Frozen::saved(&serde_json::json!({ "dependency_snapshot": raw }), "acme/app", PATH, &sha).is_err());
    }
    Ok(())
}

#[test]
fn w29_r23_each_callee_keeps_its_own_lock_and_source_through_recovery() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let live = Live::for_locks().await?;
        let hub = live.hub();
        let one = ".github/workflows/one.yml";
        let two = ".github/workflows/two.yml";
        let reusable = |word: &str| format!("on: workflow_call\njobs:\n  test:\n    runs-on: baremetal\n    steps:\n      - run: echo {word}\n      - uses: {USES}\n");
        let shared = live.root().join("shared");
        write(&shared, vec![(one.into(), reusable("old")), (github::PATH.into(), lock(one, "v1", &live.commits.old)?)]).await?;
        git(&hub, &shared, &["init", "-q", "-b", "main"]).await?;
        git(&hub, &shared, &["add", "."]).await?;
        git(&hub, &shared, &["commit", "-qm", "old callee"]).await?;
        let first = git(&hub, &shared, &["rev-parse", "HEAD"]).await?;
        git(&hub, &shared, &["tag", "-am", "old", "v1"]).await?;
        write(&shared, vec![(one.into(), reusable("changed")), (two.into(), reusable("new")), (github::PATH.into(), lock(two, "v1", &live.commits.new)?)]).await?;
        git(&hub, &shared, &["add", "."]).await?;
        git(&hub, &shared, &["commit", "-qm", "new callee"]).await?;
        let second = git(&hub, &shared, &["rev-parse", "HEAD"]).await?;
        git(&hub, &shared, &["tag", "-am", "new", "v2"]).await?;
        let shared_origin = live.root().join("shared.git");
        git(&hub, live.root(), &["clone", "-q", "--bare", &shared.to_string_lossy(), &shared_origin.to_string_lossy()]).await?;
        git(&hub, &live.origin, &["tag", "-fam", "leaf moved", "v1", &live.commits.new]).await?;
        let text = format!("on: push\njobs:\n  one:\n    uses: acme/shared/{one}@v1\n  two:\n    uses: acme/shared/{two}@v2\n");
        let caller = caller(&hub, live.root(), &text, None).await?;
        let frozen = prepare_with(&hub, &live.sources, &caller.repo, &caller.commit, PATH, Arc::clone(&caller.model)).await.map_err(anyhow::Error::msg)?;
        let defined = |path: &str, sha: &str| kiln_ci::plan::DefinedIn { workflow_repository: "acme/shared".into(), workflow_file_path: path.into(), workflow_sha: sha.into(), ..Default::default() };
        assert_eq!(frozen.actions_for(&defined(one, &first)).and_then(|a| a.get(USES)).map(|a| &a.sha), Some(&live.commits.old));
        assert_eq!(frozen.actions_for(&defined(two, &second)).and_then(|a| a.get(USES)).map(|a| &a.sha), Some(&live.commits.new));
        let saved = serde_json::json!({ "dependency_snapshot": frozen });
        // A mutable callee ref changes after acceptance, before planning/recovery.
        git(&hub, &shared_origin, &["tag", "-fam", "moved callee", "v1", &second]).await?;
        let restored = Frozen::saved(&saved, &caller.repo.name, PATH, &caller.commit).map_err(anyhow::Error::msg)?.ok_or_else(|| anyhow::anyhow!("no snapshot"))?;
        let old = restored.files.get(&format!("acme/shared/{one}@v1")).ok_or_else(|| anyhow::anyhow!("no callee"))?;
        assert_eq!(old.origin.as_ref().map(|o| &o.sha), Some(&first));
        assert_eq!(old.jobs.first().and_then(|j| j.steps.first()).and_then(|s| s.run.as_ref()).map(|t| t.src.as_str()), Some("echo old"));
        let (_, acts) = RunPlan::start(Arc::clone(&caller.model), input(&caller), Settings::default(), &SavedSource(&restored.files)).map_err(anyhow::Error::msg)?;
        let selected: Vec<&String> = acts.iter().filter_map(|a| match a {
            PlanAction::Queue(l) => restored.actions_for(&l.defined_in).and_then(|a| a.get(USES)).map(|a| &a.sha), _ => None,
        }).collect();
        assert_eq!(selected.len(), 2);
        assert!(selected.contains(&&live.commits.old) && selected.contains(&&live.commits.new));
        Ok(())
    })
}

#[test]
fn w29_r23_lock_failure_is_a_startup_failure_with_zero_jobs() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let live = Live::for_locks().await?;
        let hub = live.hub();
        let missing = github::File { workflows: [(PATH.into(), Vec::new())].into(), ..Default::default() }.write("github.com").map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let caller = caller(&hub, live.root(), WORKFLOW, Some(missing)).await?;
        super::super::dispatch::spawn(Arc::clone(&hub));
        struct Stop(Arc<Hub>);
        impl Drop for Stop { fn drop(&mut self) { self.0.shutdown.cancel(); } }
        let _stop = Stop(Arc::clone(&hub));
        let (tx, rx) = async_channel::bounded(1);
        let event = kiln_ci::trigger::Event { name: "push".into(), repo: caller.repo.name.clone(), git_ref: "refs/heads/main".into(), sha: caller.commit.clone(), actor: "lock-test".into(), trusted: true, ..Default::default() };
        let loaded = super::super::load::Loaded { path: PATH.into(), dialect: Dialect::Gha, runs: true, workflow: Some((*caller.model).clone()), diagnostics: Vec::new() };
        super::super::events::start_given(&hub, &caller.repo, &event, &loaded, ("lock test", "tester"), serde_json::Value::Null, super::super::dispatch::Given { reply: Some(tx), ..Default::default() }).await?;
        let id = crate::rt::timeout(Duration::from_secs(30), rx.recv()).await??;
        let until = std::time::Instant::now() + Duration::from_secs(30);
        let run = loop {
            let run = hub.store.run(id.clone()).await?.ok_or_else(|| anyhow::anyhow!("run missing"))?;
            if run.status.is_terminal() { break run; }
            anyhow::ensure!(std::time::Instant::now() < until, "startup didn't finish");
            crate::rt::sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(run.status, crate::store::Status::Failed);
        assert!(run.error.as_deref().is_some_and(|e| e.contains("KCI4050")), "{:?}", run.error);
        assert!(hub.store.jobs(id.clone(), false).await?.is_empty());
        assert!(retry_needs_preflight(&hub, &id).await.map_err(anyhow::Error::msg)?);
        let (reply, result) = async_channel::bounded(1);
        hub.ci.send(super::super::dispatch::Msg::Rerun { run: id.clone(), mode: super::super::dispatch::RerunMode::All, debug: false, keep_failed: None, fresh_actions: false, no_reuse: false, cold: false, placed: HashMap::new(), by: "lock-test".into(), reply });
        let repeated = crate::rt::timeout(Duration::from_secs(30), result.recv()).await??;
        assert!(repeated.err().is_some_and(|e| e.contains("KCI4050")));
        assert!(hub.store.jobs(id, false).await?.is_empty());
        Ok(())
    })
}

mod native;
mod execution;
mod policy;
