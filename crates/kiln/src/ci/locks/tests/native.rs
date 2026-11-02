use kiln_ci::lock::kiln as format;

use super::*;

const ROOT: &str = ".kiln/ci.yaml";
const TOP: &str = ".github/workflows/top.yml";
const CHILD: &str = ".github/workflows/child.yml";

struct Published { repo: Repo, commit: String, work: std::path::PathBuf, bare: std::path::PathBuf }

async fn publish(hub: &Arc<Hub>, root: &Path, name: &str, mut files: Vec<(String, String)>, share: &str) -> anyhow::Result<Published> {
    let work = root.join(name.replace('/', "__"));
    files.push((".kiln/config.yaml".into(), format!("share: {share}\n")));
    write(&work, files).await?;
    git(hub, &work, &["init", "-q", "-b", "main"]).await?;
    git(hub, &work, &["add", "."]).await?;
    git(hub, &work, &["commit", "-qm", "published fixture"]).await?;
    git(hub, &work, &["tag", "-am", "version one", "v1"]).await?;
    let commit = git(hub, &work, &["rev-parse", "HEAD"]).await?;
    let bare = crate::ci::hosting::path_of(hub, name).ok_or_else(|| anyhow::anyhow!("no native fixture path"))?;
    let parent = bare.parent().ok_or_else(|| anyhow::anyhow!("no native parent"))?.to_path_buf();
    crate::rt::blocking(move || std::fs::create_dir_all(parent)).await??;
    git(hub, root, &["clone", "-q", "--bare", &work.to_string_lossy(), &bare.to_string_lossy()]).await?;
    let repo = hub.store.upsert_repo(Repo {
        id: format!("native-{}", kiln_seams::sha256_hex(format!("{}:{name}", root.display()).as_bytes())), name: name.into(), clone_url: crate::ci::hosting::clone_url(&bare),
        branches: vec!["main".into()], poll_seconds: 60, enabled: true, created_at: 0, last_polled_at: None, last_error: None, mode: "native".into(), default_branch: Some("main".into()), mirror: None,
    }).await?;
    Ok(Published { repo, commit, work, bare })
}

async fn advance(hub: &Arc<Hub>, publisher: &Published, files: Vec<(String, String)>) -> anyhow::Result<String> {
    write(&publisher.work, files).await?;
    git(hub, &publisher.work, &["add", "."]).await?;
    git(hub, &publisher.work, &["commit", "-qm", "changed fixture"]).await?;
    let commit = git(hub, &publisher.work, &["rev-parse", "HEAD"]).await?;
    git(hub, &publisher.bare, &["fetch", "--quiet", &publisher.work.to_string_lossy(), "main:refs/heads/main"]).await?;
    git(hub, &publisher.bare, &["tag", "-fam", "moved partial version", "v1", &commit]).await?;
    Ok(commit)
}

fn pin(repository: &str, commit: &str, uses: Vec<String>) -> format::Dependency {
    format::Dependency::Git(format::Git { git_ref: "refs/tags/v1".into(), commit: format!("sha1-{commit}"), repo: repository.into(), uses, ..Default::default() })
}

fn native_lock(path: &str, roots: Vec<String>, entries: Vec<(String, format::Dependency)>) -> anyhow::Result<String> {
    format::File { workflows: [(path.into(), roots)].into(), dependencies: entries.into_iter().collect(), ..Default::default() }.write().map_err(|e| anyhow::anyhow!("{e:?}"))
}

async fn native_caller(hub: &Arc<Hub>, root: &Path, text: &str, locked: Option<String>) -> anyhow::Result<Caller> {
    let mut files = vec![(ROOT.into(), text.into()), (CHILD.into(), "on: workflow_call\njobs:\n  trap:\n    runs-on: baremetal\n    steps: [{run: echo caller trap}]\n".into())];
    if let Some(locked) = locked { files.push((format::PATH.into(), locked)); }
    let caller = publish(hub, root, "acme/app", files, "none").await?;
    crate::gitops::sync_mirror(hub, &caller.repo).await?;
    Ok(Caller { repo: caller.repo, commit: caller.commit, model: model(ROOT, text)? })
}

fn top() -> String { format!("on: workflow_call\njobs:\n  inner:\n    uses: ./{CHILD}\n") }
fn child(word: &str) -> String { format!("on: workflow_call\njobs:\n  check:\n    runs-on: baremetal\n    steps:\n      - run: echo {word}\n      - uses: {USES}\n") }

#[test]
fn w29_r24_caller_pins_choose_the_publisher_tree_and_survive_dispatcher_recovery() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let live = Live::for_locks().await?;
        let hub = live.hub();
        let publisher = publish(&hub, live.root(), "acme/shared", vec![(TOP.into(), top()), (CHILD.into(), child("publisher old")), (github::PATH.into(), lock(CHILD, "v1", &live.commits.new)?)], "owner").await?;
        let call = format!("kiln://acme/shared/{TOP}@v1");
        let text = format!("on: push\njobs:\n  outer:\n    uses: {call}\n");
        let locked = native_lock(ROOT, vec![call.clone()], vec![(call.clone(), pin(&format!("kiln:{}", publisher.repo.id), &publisher.commit, vec![USES.into()])), (USES.into(), pin("github:41/42", &live.commits.old, vec![]))])?;
        let caller = native_caller(&hub, live.root(), &text, Some(locked)).await?;
        advance(&hub, &publisher, vec![(CHILD.into(), child("publisher changed"))]).await?;
        git(&hub, &live.origin, &["tag", "-fam", "action moved", "v1", &live.commits.new]).await?;
        let policy = serde_json::to_string(&kiln_ci::policy::Policy { allow: vec![format!("acme/shared/**@{}", publisher.commit), format!("acme/tools@{}", live.commits.old)], require_sha_pins: true, ..Default::default() })?;
        let id = caller.repo.id.clone();
        hub.store.call(move |c| Ok(c.execute("INSERT INTO ci_action_policies (repo_id, policy, updated_at, updated_by) VALUES (?1, ?2, 0, 'lock-test')", [id, policy])?)).await?;
        let reads = Arc::new(Mutex::new(Vec::new()));
        let source = Counted { inner: live.sources, reads: Arc::clone(&reads) };
        let frozen = prepare_with(&hub, &source, &caller.repo, &caller.commit, ROOT, Arc::clone(&caller.model)).await.map_err(anyhow::Error::msg)?;
        assert!(frozen.root_locked);
        let (_, planned) = RunPlan::start(Arc::clone(&caller.model), input_for(&caller, ROOT), Settings::default(), &SavedSource(&frozen.files)).map_err(anyhow::Error::msg)?;
        let spec = planned.iter().find_map(|a| if let PlanAction::Queue(l) = a { Some(l) } else { None }).ok_or_else(|| anyhow::anyhow!("no nested leaf"))?;
        assert_eq!(spec.defined_in.workflow_repository, publisher.repo.name);
        assert_eq!(spec.defined_in.workflow_file_path, CHILD);
        assert_eq!(spec.defined_in.workflow_sha, publisher.commit);
        assert_eq!(frozen.actions_for(&spec.defined_in).and_then(|a| a.get(USES)).map(|a| &a.sha), Some(&live.commits.old));
        assert_eq!(spec.def().and_then(|j| j.steps.first()).and_then(|s| s.run.as_ref()).map(|t| t.src.as_str()), Some("echo publisher old"));
        assert!(reads.lock().iter().filter(|(_, path)| path == TOP || path == CHILD).all(|(sha, _)| sha == &publisher.commit));
        assert!(reads.lock().iter().filter(|(_, path)| path == "action.yml").all(|(sha, _)| sha == &live.commits.old));
        let count = reads.lock().len();
        let saved = serde_json::json!({ "dependency_snapshot": frozen, "github": input_for(&caller, ROOT).github, "inputs": {}, "vars": {}, "trusted": true, "graph": kiln_ci::work::graph::Graph::default() });
        let restored = Frozen::saved(&saved, &caller.repo.name, ROOT, &caller.commit).map_err(anyhow::Error::msg)?.ok_or_else(|| anyhow::anyhow!("snapshot missing"))?;
        assert_eq!(restored.actions_for(&spec.defined_in).and_then(|a| a.get(USES)).map(|a| &a.sha), Some(&live.commits.old));
        // The server stopped after persisting a queued leg. A new dispatcher rebuilds it from
        // SQLite, after the callee and action refs changed, without a source re-resolution.
        crate::ci::runs::create(&hub, &caller.repo, crate::ci::runs::NewRun { id: "native-lock-recovery".into(), workflow: ROOT.into(), name: "native lock recovery".into(), event: "push".into(), branch: "main".into(), sha: caller.commit.clone(), trusted: true, model: serde_json::to_string(&caller.model)?, input: saved.to_string(), ..Default::default() }).await?;
        crate::ci::runs::insert_leg(&hub, "native-lock-recovery", "native-lock-leaf", spec, crate::store::Status::Queued, None).await?;
        // After acceptance, neither tag even reaches A anymore. New preflight would refuse;
        // recovery must consume the persisted decision instead of examining mutable refs.
        let unrelated = git(&hub, &publisher.bare, &["commit-tree", &format!("{}^{{tree}}", publisher.commit), "-m", "unrelated callee after capture"]).await?;
        git(&hub, &publisher.bare, &["tag", "-fam", "unrelated after capture", "v1", &unrelated]).await?;
        git(&hub, &live.origin, &["tag", "-fam", "unrelated after capture", "v1", &live.commits.foreign]).await?;
        crate::ci::dispatch::spawn(Arc::clone(&hub));
        struct Stop(Arc<Hub>);
        impl Drop for Stop { fn drop(&mut self) { self.0.shutdown.cancel(); } }
        let _stop = Stop(Arc::clone(&hub));
        let until = std::time::Instant::now() + Duration::from_secs(30);
        let recovered = loop {
            if let Some(recovered) = hub.ci.restored() { break recovered; }
            anyhow::ensure!(std::time::Instant::now() < until, "dispatcher didn't restore");
            crate::rt::sleep(Duration::from_millis(10)).await;
        };
        assert_eq!((recovered.runs, recovered.legs), (1, 1));
        assert!(hub.ci.is_leg("native-lock-leaf"));
        let route = crate::ci::dispatch::stored_route(&hub, "native-lock-leaf").await?.ok_or_else(|| anyhow::anyhow!("no restored route"))?;
        assert_ne!(route.caps & crate::proto::CAP_DEPENDENCY_LOCKS, 0, "recovery retains the locked job's enforcement requirement");
        assert!(hub.store.run("native-lock-recovery".into()).await?.is_some_and(|r| r.error.is_none()));
        assert_eq!(reads.lock().len(), count);
        assert!(refresh_allowed(&hub, &caller.repo, &caller.commit, ROOT, None).await.is_err());
        Ok(())
    })
}

#[test]
fn w29_r24_a_locked_private_callee_is_refused_before_reading_it() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let live = Live::for_locks().await?;
        let hub = live.hub();
        let publisher = publish(&hub, live.root(), "acme/private", vec![(TOP.into(), "on: workflow_call\njobs:\n  check:\n    runs-on: baremetal\n    steps: [{run: echo private}]\n".into())], "none").await?;
        let call = format!("kiln://acme/private/{TOP}@v1");
        let text = format!("on: push\njobs:\n  check:\n    uses: {call}\n");
        let locked = native_lock(ROOT, vec![call.clone()], vec![(call, pin(&format!("kiln:{}", publisher.repo.id), &publisher.commit, vec![]))])?;
        let caller = native_caller(&hub, live.root(), &text, Some(locked)).await?;
        let reads = Arc::new(Mutex::new(Vec::new()));
        let source = Counted { inner: live.sources, reads: Arc::clone(&reads) };
        assert!(prepare_with(&hub, &source, &caller.repo, &caller.commit, ROOT, caller.model).await.err().is_some_and(|e| e.contains("KCI4022")));
        assert!(reads.lock().is_empty());
        Ok(())
    })
}

#[test]
fn w29_r24_an_invalid_callee_lock_fails_before_any_action_metadata() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let live = Live::for_locks().await?;
        let hub = live.hub();
        let publisher = publish(&hub, live.root(), "acme/shared", vec![(TOP.into(), child("must not run")), (format::PATH.into(), "version: 999\n".into())], "owner").await?;
        let call = format!("kiln://acme/shared/{TOP}@v1");
        let text = format!("on: push\njobs:\n  check:\n    uses: {call}\n");
        let locked = native_lock(ROOT, vec![call.clone()], vec![(call, pin(&format!("kiln:{}", publisher.repo.id), &publisher.commit, vec![]))])?;
        let caller = native_caller(&hub, live.root(), &text, Some(locked)).await?;
        let reads = Arc::new(Mutex::new(Vec::new()));
        let source = Counted { inner: live.sources, reads: Arc::clone(&reads) };
        assert!(prepare_with(&hub, &source, &caller.repo, &caller.commit, ROOT, caller.model).await.err().is_some_and(|e| e.contains("KCI1022")));
        assert!(reads.lock().iter().all(|(_, path)| path != "action.yml"));
        Ok(())
    })
}

#[test]
fn w29_r24_draft_only_callees_remain_unusable_with_and_without_a_lock() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        for locked in [false, true] {
            let live = Live::for_locks().await?;
            let hub = live.hub();
            let publisher = publish(&hub, live.root(), "acme/shared", vec![(TOP.into(), child("unpublished"))], "owner").await?;
            let foreign = git(&hub, &publisher.bare, &["commit-tree", &format!("{}^{{tree}}", publisher.commit), "-m", "only a draft reaches this"]).await?;
            let draft = format!("refs/heads/{}hidden", kiln_ci::drafts::DRAFT_PREFIX);
            git(&hub, &publisher.bare, &["update-ref", &draft, &foreign]).await?;
            let call = format!("kiln://acme/shared/{TOP}@{foreign}");
            let text = format!("on: push\njobs:\n  check:\n    uses: {call}\n");
            let lock = if locked {
                Some(native_lock(ROOT, vec![call.clone()], vec![(call, format::Dependency::Git(format::Git { git_ref: draft, commit: format!("sha1-{foreign}"), repo: format!("kiln:{}", publisher.repo.id), ..Default::default() }))])?)
            } else { None };
            let caller = native_caller(&hub, live.root(), &text, lock).await?;
            let reads = Arc::new(Mutex::new(Vec::new()));
            let source = Counted { inner: live.sources, reads: Arc::clone(&reads) };
            let error = prepare_with(&hub, &source, &caller.repo, &caller.commit, ROOT, caller.model).await.err().ok_or_else(|| anyhow::anyhow!("a draft-only callee was accepted"))?;
            assert!(error.contains("KCI4023") || (locked && error.contains("KCI4051")), "{error}");
            assert!(reads.lock().is_empty());
        }
        Ok(())
    })
}

#[test]
fn w29_r24_disagreeing_files_refuse_with_both_entries_before_source_reads() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let live = Live::for_locks().await?;
        let hub = live.hub();
        let mut caller = caller(&hub, live.root(), WORKFLOW, Some(lock(PATH, "v1", &live.commits.old)?)).await?;
        let dir = live.root().join("caller");
        let lock = native_lock(PATH, vec![USES.into()], vec![(USES.into(), pin("github:41/42", &live.commits.new, vec![]))])?;
        write(&dir, vec![(format::PATH.into(), lock)]).await?;
        git(&hub, &dir, &["add", "."]).await?;
        git(&hub, &dir, &["commit", "-qm", "conflicting lock"]).await?;
        caller.commit = git(&hub, &dir, &["rev-parse", "HEAD"]).await?;
        crate::gitops::sync_mirror(&hub, &caller.repo).await?;
        let reads = Arc::new(Mutex::new(Vec::new()));
        let source = Counted { inner: live.sources, reads: Arc::clone(&reads) };
        let error = prepare_with(&hub, &source, &caller.repo, &caller.commit, PATH, caller.model).await.err().ok_or_else(|| anyhow::anyhow!("disagreeing locks were accepted"))?;
        assert!(error.contains("KCI4054") && error.contains(github::PATH) && error.contains(format::PATH), "{error}");
        assert!(reads.lock().is_empty());
        Ok(())
    })
}

#[test]
fn w29_r24_hosted_actions_in_github_workflows_use_kiln_ids_and_satisfy_sha_policy() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let dir = tempfile::tempdir()?;
        let hub = crate::hub::tests::scratch_hub_with(|cfg| {
            cfg.ci.enabled = true;
            cfg.ci.actions.sources.push(crate::config::ci::ActionSource { owners: vec!["acme".into()], url: None, native: true });
        }).await?;
        let publisher = publish(&hub, dir.path(), "acme/shared", vec![("action.yml".into(), "name: hosted\nruns: {using: composite, steps: []}\n".into())], "owner").await?;
        let uses = "acme/shared@v1";
        let text = WORKFLOW.replace(USES, uses);
        let empty = github::File { workflows: [(PATH.into(), vec![])].into(), ..Default::default() }.write("github.com").map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let mut caller = caller(&hub, dir.path(), &text, Some(empty)).await?;
        let caller_dir = dir.path().join("caller");
        let lock = native_lock(PATH, vec![uses.into()], vec![(uses.into(), pin(&format!("kiln:{}", publisher.repo.id), &publisher.commit, vec![]))])?;
        write(&caller_dir, vec![(format::PATH.into(), lock)]).await?;
        git(&hub, &caller_dir, &["add", "."]).await?;
        git(&hub, &caller_dir, &["commit", "-qm", "hosted dependency lock"]).await?;
        caller.commit = git(&hub, &caller_dir, &["rev-parse", "HEAD"]).await?;
        crate::gitops::sync_mirror(&hub, &caller.repo).await?;
        let policy = serde_json::to_string(&kiln_ci::policy::Policy { allow: vec![format!("acme/shared@{}", publisher.commit)], require_sha_pins: true, ..Default::default() })?;
        let id = caller.repo.id.clone();
        hub.store.call(move |c| Ok(c.execute("INSERT INTO ci_action_policies (repo_id, policy, updated_at, updated_by) VALUES (?1, ?2, 0, 'lock-test')", [id, policy])?)).await?;
        let source = crate::ci::dependency_sources::Sources::new(Arc::clone(&hub), caller.repo.name.clone());
        let frozen = prepare_with(&hub, &source, &caller.repo, &caller.commit, PATH, caller.model).await.map_err(anyhow::Error::msg)?;
        assert_eq!(frozen.root_actions().map_err(anyhow::Error::msg)?.get(uses).map(|a| &a.sha), Some(&publisher.commit));
        assert!(frozen.root_locked);
        Ok(())
    })
}

#[test]
fn w29_r24_one_saved_scope_cannot_silently_take_the_first_callers_pin() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let live = Live::for_locks().await?;
        let hub = live.hub();
        let shared = publish(&hub, live.root(), "acme/shared", vec![(CHILD.into(), child("shared"))], "owner").await?;
        let call = format!("kiln://acme/shared/{CHILD}@v1");
        for (name, commit) in [("acme/parent-one", &live.commits.old), ("acme/parent-two", &live.commits.new)] {
            let parent = format!("on: workflow_call\njobs:\n  inner:\n    uses: {call}\n");
            let locked = native_lock(TOP, vec![call.clone()], vec![(call.clone(), pin(&format!("kiln:{}", shared.repo.id), &shared.commit, vec![USES.into()])), (USES.into(), pin("github:41/42", commit, vec![]))])?;
            publish(&hub, live.root(), name, vec![(TOP.into(), parent), (format::PATH.into(), locked)], "owner").await?;
        }
        git(&hub, &live.origin, &["tag", "-fam", "action moved", "v1", &live.commits.new]).await?;
        let text = format!("on: push\njobs:\n  one:\n    uses: kiln://acme/parent-one/{TOP}@v1\n  two:\n    uses: kiln://acme/parent-two/{TOP}@v1\n");
        let caller = native_caller(&hub, live.root(), &text, None).await?;
        let error = prepare_with(&hub, &live.sources, &caller.repo, &caller.commit, ROOT, caller.model).await.err().ok_or_else(|| anyhow::anyhow!("two caller contexts silently shared one scope"))?;
        assert!(error.contains("KCI4054") && error.contains(&live.commits.old) && error.contains(&live.commits.new), "{error}");
        Ok(())
    })
}

#[test]
fn w29_r24_supplemental_provenance_cannot_bypass_githubs_moved_full_tag_check() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let live = Live::for_locks().await?;
        let hub = live.hub();
        git(&hub, &live.origin, &["tag", "-am", "full version moved", "v1.2.3", &live.commits.new]).await?;
        git(&hub, &live.origin, &["update-ref", "refs/heads/v1.2.3", &live.commits.old]).await?;
        let text = WORKFLOW.replace("@v1", "@v1.2.3");
        let mut caller = caller(&hub, live.root(), &text, Some(lock(PATH, "v1.2.3", &live.commits.old)?)).await?;
        let key = "acme/tools@v1.2.3";
        let native = format::Dependency::Git(format::Git { git_ref: "refs/heads/v1.2.3".into(), commit: format!("sha1-{}", live.commits.old), repo: "github:41/42".into(), ..Default::default() });
        let supplemental = native_lock(PATH, vec![key.into()], vec![(key.into(), native)])?;
        let dir = live.root().join("caller");
        write(&dir, vec![(format::PATH.into(), supplemental)]).await?;
        git(&hub, &dir, &["add", "."]).await?;
        git(&hub, &dir, &["commit", "-qm", "same tree through a different recorded ref"]).await?;
        caller.commit = git(&hub, &dir, &["rev-parse", "HEAD"]).await?;
        crate::gitops::sync_mirror(&hub, &caller.repo).await?;
        let reads = Arc::new(Mutex::new(Vec::new()));
        let source = Counted { inner: live.sources, reads: Arc::clone(&reads) };
        let error = prepare_with(&hub, &source, &caller.repo, &caller.commit, PATH, caller.model).await.err().ok_or_else(|| anyhow::anyhow!("the supplemental file bypassed the moved full tag"))?;
        assert!(error.contains("KCI4053"), "{error}");
        assert!(reads.lock().is_empty());
        Ok(())
    })
}
