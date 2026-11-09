//! W29-R25: fresh actions propose coherent locks and whole code evidence, never a new run.

use kiln_ci::lock::{kiln, regenerate};
use kiln_seams::dependencies::{SimDependencySources, SimRepository};

use super::*;

fn failure(e: crate::ci::Fail) -> anyhow::Error { anyhow::anyhow!("{} {}", e.0, e.1) }

#[test]
fn w29_r25_fresh_proposals_refresh_shared_pins_but_keep_unrelated_and_literal_pins() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let root = tempfile::tempdir()?;
        let hub = crate::hub::tests::scratch_hub().await?;
        let source = SimDependencySources::default();
        let a = "a".repeat(40);
        let m = "b".repeat(40);
        let full = "acme/tools@v1.2.3";
        let other = "acme/other@v1";
        for (name, owner_id, repo_id, reference) in [("acme/tools", 41, 42, "v1.2.3"), ("acme/other", 51, 52, "v1")] {
            source.put_repository(Repository { name: name.into(), native: false }, SimRepository {
                identity: Identity::Github { hostname: "github.com".into(), owner_id, repo_id }, default_branch: "main".into(),
                refs: [("refs/heads/main".into(), m.clone()), (format!("refs/tags/{reference}"), m.clone())].into(), commits: [(a.clone(), vec![]), (m.clone(), vec![a.clone()])].into(),
                files: [((a.clone(), "action.yml".into()), b"runs: {using: composite, steps: []}\n".to_vec()), ((m.clone(), "action.yml".into()), if name == "acme/tools" { b"runs: {using: composite, steps: [{run: echo changed, shell: sh}]}\n".to_vec() } else { b"invalid metadata must not be read\n".to_vec() }), ((a.clone(), "src/run.sh".into()), b"echo accepted\n".to_vec()), ((m.clone(), "src/run.sh".into()), b"echo changed\n".to_vec())].into(),
            })?;
        }
        let text = WORKFLOW.replace(USES, full);
        let mut locked = github::parse(&lock(PATH, "v1.2.3", &a)?).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let shared = ".github/workflows/shared.yml";
        let quiet = ".github/workflows/quiet.yml";
        let unlocked = ".github/workflows/unlocked.yml";
        locked.workflows.insert(shared.into(), vec![full.into()]);
        locked.workflows.insert(quiet.into(), vec![other.into()]);
        locked.dependencies.insert(other.into(), github::Action { git_ref: "v1".into(), commit: format!("sha1-{a}"), owner_id: 51, repo_id: 52, ..Default::default() });
        write(&root.path().join("caller"), vec![(shared.into(), text.clone()), (quiet.into(), WORKFLOW.replace(USES, other)), (unlocked.into(), WORKFLOW.replace(USES, "missing/unlocked@v1"))]).await?;
        let app = caller(&hub, root.path(), &text, Some(locked.write("github.com").map_err(|e| anyhow::anyhow!("{e:?}"))?)).await?;
        let proposal = super::super::generate::propose(&hub, &source, &app.repo, &app.commit, BTreeMap::new(), PATH).await.map_err(anyhow::Error::msg)?;
        let parsed = github::parse(proposal.files.github.as_deref().ok_or_else(|| anyhow::anyhow!("no proposed lock"))?).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        assert_eq!(parsed.dependencies.get(full).and_then(|p| github::digest(&p.commit)), Some(m.as_str()));
        assert_eq!(parsed.dependencies.get(other).and_then(|p| github::digest(&p.commit)), Some(a.as_str()));
        assert_eq!(parsed.workflows.len(), 3, "an unrelated unlocked workflow is not onboarded");
        assert_eq!(proposal.differences.len(), 1);
        assert!(proposal.differences.get(full).is_some_and(|d| d.files.iter().any(|f| f.path == "src/run.sh" && f.after.as_ref().is_some_and(|b| b.bytes == b"echo changed\n"))));
        let literal = text.replace(full, &format!("acme/tools@{a}"));
        let literal = super::super::generate::propose(&hub, &source, &app.repo, &app.commit, [(PATH.into(), Some(literal.into_bytes()))].into(), PATH).await.map_err(anyhow::Error::msg)?;
        assert!(literal.differences.is_empty(), "a full-SHA use retains that literal commit");
        let parsed = github::parse(literal.files.github.as_deref().ok_or_else(|| anyhow::anyhow!("no literal lock"))?).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        assert!(parsed.dependencies.values().all(|p| github::digest(&p.commit) == Some(a.as_str())));
        let counts: (i64, i64) = hub.store.call(|c| Ok((c.query_row("SELECT COUNT(*) FROM ci_pipeline_drafts", [], |r| r.get(0))?, c.query_row("SELECT COUNT(*) FROM ci_lock_refs", [], |r| r.get(0))?))).await?;
        assert_eq!(counts, (0, 0), "collecting a proposal is not a host write or acceptance");
        Ok(())
    })
}

/// A locked native consumer's finished run, after its action's partial tag moved to new code.
struct Fresh { app: native::Published, model: Arc<Workflow>, source: crate::ci::dependency_sources::Sources<Arc<Hub>>, frozen: Frozen, saved: String, moved: String }

async fn fresh(hub: &Arc<Hub>, root: &Path) -> anyhow::Result<Fresh> {
    let publisher = native::publish(hub, root, "acme/tools", vec![("action.yml".into(), "runs: {using: composite, steps: []}\n".into()), ("src/run.sh".into(), "echo accepted\n".into())], "owner").await?;
    let text = WORKFLOW.replace(USES, "kiln://acme/tools@v1");
    let mut app = native::publish(hub, root, "acme/app", vec![(PATH.into(), text.clone())], "none").await?;
    crate::gitops::sync_mirror(hub, &app.repo).await?;
    let source = crate::ci::dependency_sources::Sources::new(Arc::clone(hub), app.repo.name.clone());
    let files = super::super::generate::collect(hub, &source, &app.repo, &app.commit, BTreeMap::new(), None, regenerate::Mode::Preserve).await.map_err(anyhow::Error::msg)?;
    app.commit = native::advance(hub, &app, vec![(github::PATH.into(), files.github.unwrap_or_default()), (kiln::PATH.into(), files.kiln.unwrap_or_default())]).await?;
    crate::gitops::sync_mirror(hub, &app.repo).await?;
    let model = model(PATH, &text)?;
    let frozen = capture_with(hub, &source, &app.repo, &app.commit, PATH, Arc::clone(&model), false).await.map_err(anyhow::Error::msg)?;
    let saved = serde_json::to_string(&serde_json::json!({"dependency_snapshot": frozen}))?;
    let (repo, sha, model_json, input_json) = (app.repo.id.clone(), app.commit.clone(), serde_json::to_string(&*model)?, saved.clone());
    hub.store.call(move |c| {
        c.execute("INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, status, created_at, finished_at, workflow_id) VALUES ('fresh-run', ?1, 1, 'ci', 'push', 'main', ?2, 'succeeded', 1, 2, 'ci')", rusqlite::params![repo, sha])?;
        c.execute("INSERT INTO ci_plans (run_id, workflow, input, done) VALUES ('fresh-run', ?1, ?2, 1)", rusqlite::params![model_json, input_json])?;
        Ok(())
    }).await?;
    let moved = native::advance(hub, &publisher, vec![("src/run.sh".into(), "echo changed code\n".into())]).await?;
    Ok(Fresh { app, model, source, frozen, saved, moved })
}

#[test]
fn w29_r25_a_fresh_rerun_opens_a_draft_with_code_evidence_and_preserves_the_old_run() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let root = tempfile::tempdir()?;
        let hub = crate::hub::tests::scratch_hub().await?;
        let Fresh { app, model, source, frozen, saved, moved } = fresh(&hub, root.path()).await?;
        let denied = crate::authz::Caller::with(crate::authz::gate::Kind::Pat, "admin", Some(crate::authz::gate::Rights { grants: vec![crate::authz::Grant { what: "runs.rerun".into(), scope: crate::authz::Scope::Repo { repo: app.repo.name.clone() }, deny: false }], ..Default::default() }), None);
        assert!(fresh_draft(&hub, Some(&denied), "fresh-run").await.is_err_and(|e| e.0 == http::StatusCode::FORBIDDEN));
        assert!(fresh_draft(&hub, None, "fresh-run").await.is_err_and(|e| e.0 == http::StatusCode::FORBIDDEN));
        let result = crate::ops::call(crate::ops::Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() }, "ci.runs.rerun", serde_json::json!({"run": "fresh-run", "mode": "all", "fresh_actions": true})).await.map_err(failure)?;
        assert_eq!(result.get("proposal"), Some(&serde_json::json!("dependency_lock")));
        assert!(result.get("attempt").is_none(), "a lock proposal is not a new run attempt");
        let draft_id = result.pointer("/draft/id").and_then(serde_json::Value::as_str).ok_or_else(|| anyhow::anyhow!("no draft: {result}"))?;
        let draft = crate::ci::pipeline_drafts::get(&hub, draft_id).await?.ok_or_else(|| anyhow::anyhow!("no saved draft"))?;
        let head = crate::ci::pipeline_drafts::head(&hub, &app.repo, &draft).await?;
        let review = proposal_for(&hub, &app.repo, draft_id, &head).await.map_err(failure)?.ok_or_else(|| anyhow::anyhow!("no revision-bound evidence"))?;
        let diff = review.pointer("/differences/kiln:~1~1acme~1tools@v1");
        assert!(diff.is_some(), "{review}");
        let encoded: BTreeMap<String, kiln_seams::dependencies::Difference> = serde_json::from_value(review.get("differences").cloned().unwrap_or_default())?;
        assert!(encoded.values().any(|d| d.files.iter().any(|f| f.path == "src/run.sh" && f.after.as_ref().is_some_and(|b| b.bytes == b"echo changed code\n"))));
        let locks = crate::ci::load::blob(&hub, &app.repo, &head, kiln::PATH).await?;
        let parsed = kiln::parse(&locks).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        assert!(matches!(parsed.dependencies.get("kiln://acme/tools@v1"), Some(kiln::Dependency::Git(g)) if github::digest(&g.commit) == Some(moved.as_str())));
        let fresh = capture_with(&hub, &source, &app.repo, &head, PATH, Arc::clone(&model), false).await;
        assert!(fresh.is_err_and(|e| e.contains("KCI4055") && e.contains("review")), "Ready cannot bypass review even for a partial tag");
        let original = prepare_retry(&hub, "fresh-run", false).await.map_err(anyhow::Error::msg)?;
        assert_eq!(serde_json::to_value(original)?, serde_json::to_value(frozen)?);
        let (attempt, input, jobs, runs, accepted): (i64, String, i64, i64, i64) = hub.store.call(|c| Ok((c.query_row("SELECT attempt FROM runs WHERE id = 'fresh-run'", [], |r| r.get(0))?, c.query_row("SELECT input FROM ci_plans WHERE run_id = 'fresh-run'", [], |r| r.get(0))?, c.query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get(0))?, c.query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))?, c.query_row("SELECT COUNT(*) FROM ci_lock_refs", [], |r| r.get(0))?))).await?;
        assert_eq!((attempt, input, jobs, runs, accepted), (1, saved, 0, 1, 0));
        assert_eq!(git(&hub, &app.bare, &["rev-parse", "main"]).await?, app.commit);
        Ok(())
    })
}

/// The fresh rerun's lock-change draft: its id, exact head and evidence hash.
async fn open(hub: &Arc<Hub>, app: &native::Published) -> anyhow::Result<(String, String, String)> {
    let result = crate::ops::call(crate::ops::Cx { hub: Arc::clone(hub), caller: crate::authz::Caller::owner() }, "ci.runs.rerun", serde_json::json!({"run": "fresh-run", "mode": "all", "fresh_actions": true})).await.map_err(failure)?;
    let draft = result.pointer("/draft/id").and_then(serde_json::Value::as_str).ok_or_else(|| anyhow::anyhow!("no draft: {result}"))?.to_owned();
    let saved = crate::ci::pipeline_drafts::get(hub, &draft).await?.ok_or_else(|| anyhow::anyhow!("no saved draft"))?;
    let head = crate::ci::pipeline_drafts::head(hub, &app.repo, &saved).await?;
    let review = proposal_for(hub, &app.repo, &draft, &head).await.map_err(failure)?.ok_or_else(|| anyhow::anyhow!("no revision-bound evidence"))?;
    let hash = review.get("evidence_sha256").and_then(serde_json::Value::as_str).ok_or_else(|| anyhow::anyhow!("no evidence hash: {review}"))?.to_owned();
    Ok((draft, head, hash))
}

/// A consumer commit with `files` and the draft's lock files, on the trunk.
async fn with_locks(hub: &Arc<Hub>, app: &native::Published, head: &str, mut files: Vec<(String, String)>) -> anyhow::Result<String> {
    for path in [github::PATH, kiln::PATH] { files.push((path.into(), crate::ci::load::blob(hub, &app.repo, head, path).await?)); }
    let commit = native::advance(hub, app, files).await?;
    crate::gitops::sync_mirror(hub, &app.repo).await?;
    Ok(commit)
}

fn with_rights(name: &str, repo: &str, what: &[&str]) -> crate::authz::Caller {
    let grants = what.iter().map(|w| crate::authz::Grant { what: (*w).into(), scope: crate::authz::Scope::Repo { repo: repo.into() }, deny: false }).collect();
    crate::authz::Caller::with(crate::authz::gate::Kind::User, name, Some(crate::authz::gate::Rights { grants, ..Default::default() }), None)
}

#[test]
fn w29_r25_a_persons_word_on_the_exact_evidence_lets_only_that_code_run_in_this_consumer() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let root = tempfile::tempdir()?;
        let hub = crate::hub::tests::scratch_hub().await?;
        let Fresh { app, model, source, moved, .. } = fresh(&hub, root.path()).await?;
        let (draft_id, head, hash) = open(&hub, &app).await?;
        let owner = crate::authz::Caller::owner();
        let name = app.repo.name.clone();
        let mut draft = crate::ci::pipeline_drafts::get(&hub, &draft_id).await?.ok_or_else(|| anyhow::anyhow!("no saved draft"))?;
        let early = crate::ci::pipeline_drafts::ready(&hub, &owner, &app.repo, &mut draft).await;
        assert!(early.is_err_and(|e| e.0 == http::StatusCode::CONFLICT && e.1.contains("accept")), "Ready waits for the person's word on the upstream code");
        let ask = |head: &str, hash: &str| serde_json::json!({"draft": draft_id, "head": head, "evidence_sha256": hash});
        // Agents and tokens are not a person, whatever rights they hold.
        let mut agent = crate::authz::Caller::owner();
        agent.agent = Some("agent-session".into());
        let mut token = crate::authz::Caller::owner();
        token.token = Some("pat-1".into());
        for caller in [&agent, &token] {
            assert!(crate::ci::locks::accept(&hub, caller, &name, &ask(&head, &hash)).await.is_err_and(|e| e.0 == http::StatusCode::FORBIDDEN));
        }
        let unrighted = with_rights("user:kim", &name, &["workflows.edit", "diffs.submit"]);
        assert!(crate::ci::locks::accept(&hub, &unrighted, &name, &ask(&head, &hash)).await.is_err_and(|e| e.0 == http::StatusCode::FORBIDDEN));
        // Words about another revision or other evidence are no acceptance of this one.
        assert!(crate::ci::locks::accept(&hub, &owner, &name, &ask(&app.commit, &hash)).await.is_err_and(|e| e.0 == http::StatusCode::CONFLICT));
        assert!(crate::ci::locks::accept(&hub, &owner, &name, &ask(&head, &"0".repeat(64))).await.is_err_and(|e| e.0 == http::StatusCode::CONFLICT));
        let refused = capture_with(&hub, &source, &app.repo, &head, PATH, Arc::clone(&model), false).await;
        assert!(refused.is_err_and(|e| e.contains("KCI4055")));
        let accepted = crate::ops::call(crate::ops::Cx { hub: Arc::clone(&hub), caller: owner.clone() }, "ci.locks.accept", serde_json::json!({"repo": name, "draft": draft_id, "head": head, "evidence_sha256": hash})).await.map_err(failure)?;
        assert_eq!(accepted.get("accepted"), Some(&serde_json::json!(true)), "{accepted}");
        let runs = capture_with(&hub, &source, &app.repo, &head, PATH, Arc::clone(&model), true).await.map_err(anyhow::Error::msg)?;
        assert_eq!(runs.root_actions().map_err(anyhow::Error::msg)?.get("kiln://acme/tools@v1").map(|a| &a.sha), Some(&moved));
        crate::ci::pipeline_drafts::ready(&hub, &owner, &app.repo, &mut draft).await.map_err(failure)?;
        // Another tree of this consumer still waits, until the land queue registers its candidate.
        let candidate = with_locks(&hub, &app, &head, vec![("README.md".into(), "landed with others\n".into())]).await?;
        assert!(capture_with(&hub, &source, &app.repo, &candidate, PATH, Arc::clone(&model), false).await.is_err_and(|e| e.contains("KCI4055")));
        crate::ci::locks::lock_candidate(&hub, &app.repo, &candidate, std::slice::from_ref(&head)).await.map_err(anyhow::Error::msg)?;
        let tested = capture_with(&hub, &source, &app.repo, &candidate, PATH, Arc::clone(&model), false).await.map_err(anyhow::Error::msg)?;
        assert_eq!(tested.root_actions().map_err(anyhow::Error::msg)?.get("kiln://acme/tools@v1").map(|a| &a.sha), Some(&moved));
        // Landed, the pin is this consumer's: later trunk commits run it.
        crate::ci::locks::lock_landed(&hub, &app.repo, &candidate, &candidate).await.map_err(anyhow::Error::msg)?;
        let later = with_locks(&hub, &app, &head, vec![("README.md".into(), "and after\n".into())]).await?;
        let after = capture_with(&hub, &source, &app.repo, &later, PATH, Arc::clone(&model), false).await.map_err(anyhow::Error::msg)?;
        assert_eq!(after.root_actions().map_err(anyhow::Error::msg)?.get("kiln://acme/tools@v1").map(|a| &a.sha), Some(&moved));
        // Nothing of it became a global acceptance another consumer could borrow.
        let global: i64 = hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM ci_lock_refs", [], |r| r.get(0))?)).await?;
        assert_eq!(global, 0);
        Ok(())
    })
}

#[test]
fn w29_r25_a_reviewers_accept_of_the_exact_version_counts_until_withdrawn_and_a_discard_ends_it() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let root = tempfile::tempdir()?;
        let hub = crate::hub::tests::scratch_hub().await?;
        let Fresh { app, model, source, moved, .. } = fresh(&hub, root.path()).await?;
        let (draft_id, head, hash) = open(&hub, &app).await?;
        let name = app.repo.name.clone();
        let mut draft = crate::ci::pipeline_drafts::get(&hub, &draft_id).await?.ok_or_else(|| anyhow::anyhow!("no saved draft"))?;
        let diff = crate::ci::diffs::find(&hub, draft.name.as_deref().unwrap_or_default()).await?.ok_or_else(|| anyhow::anyhow!("no diff"))?;
        let run_at_head = || capture_with(&hub, &source, &app.repo, &head, PATH, Arc::clone(&model), false);
        // An agent session's accept is no person's word, though the review itself stands.
        let mut bot = with_rights("user:bot", &name, &["diffs.review"]);
        bot.agent = Some("agent-session".into());
        crate::ci::review::review(&hub, &diff, &bot, &serde_json::json!({"action": "accept"})).await.map_err(failure)?;
        assert!(run_at_head().await.is_err_and(|e| e.contains("KCI4055")));
        let kim = with_rights("user:kim", &name, &["diffs.review"]);
        crate::ci::review::review(&hub, &diff, &kim, &serde_json::json!({"action": "accept"})).await.map_err(failure)?;
        let runs = run_at_head().await.map_err(anyhow::Error::msg)?;
        assert_eq!(runs.root_actions().map_err(anyhow::Error::msg)?.get("kiln://acme/tools@v1").map(|a| &a.sha), Some(&moved));
        // A comment keeps it; asking for changes withdraws it.
        crate::ci::review::review(&hub, &diff, &kim, &serde_json::json!({"action": "comment", "body": "reading the rest"})).await.map_err(failure)?;
        assert!(run_at_head().await.is_ok());
        crate::ci::review::review(&hub, &diff, &kim, &serde_json::json!({"action": "request_changes", "body": "not this code"})).await.map_err(failure)?;
        assert!(run_at_head().await.is_err_and(|e| e.contains("KCI4055")));
        // The owner's own word holds until the draft is discarded.
        crate::ci::locks::accept(&hub, &crate::authz::Caller::owner(), &name, &serde_json::json!({"draft": draft_id, "head": head, "evidence_sha256": hash})).await.map_err(failure)?;
        assert!(run_at_head().await.is_ok());
        crate::ci::pipeline_drafts::discard(&hub, &crate::authz::Caller::owner(), &app.repo, &mut draft).await.map_err(failure)?;
        assert!(run_at_head().await.is_err_and(|e| e.contains("KCI4055")));
        Ok(())
    })
}
