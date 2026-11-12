//! W29-R26: trusted trunk policy, real persistent history, and captured opt-in delivery.

use kiln_seams::dependencies::{SimDependencySources, SimRepository};

use super::*;

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const M: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const U: &str = "1111111111111111111111111111111111111111";
const FULL: &str = "refs/tags/v1.2.3";

pub(super) fn simulated() -> anyhow::Result<(SimDependencySources, Repository)> {
    let source = SimDependencySources::default();
    let repository = Repository { name: "acme/tools".into(), native: false };
    source.put_repository(repository.clone(), SimRepository {
        identity: Identity::Github { hostname: "github.com".into(), owner_id: 41, repo_id: 42 },
        refs: [("refs/heads/main".into(), M.into()), (FULL.into(), A.into())].into(), default_branch: "main".into(),
        commits: [(A.into(), vec![]), (M.into(), vec![A.into()]), (U.into(), vec![])].into(),
        files: [((A.into(), "action.yml".into()), b"name: accepted\nruns: {using: composite, steps: []}\n".to_vec()), ((M.into(), "action.yml".into()), b"name: moved\nruns: {using: composite, steps: []}\n".to_vec())].into(),
    })?;
    Ok((source, repository))
}

async fn change_trunk(hub: &Arc<Hub>, caller: &Caller, root: &Path, config: &str) -> anyhow::Result<()> {
    let dir = root.join("caller");
    git(hub, &dir, &["checkout", "-q", "main"]).await?;
    write(&dir, vec![(".kiln/config.yaml".into(), config.into())]).await?;
    git(hub, &dir, &["add", "."]).await?;
    git(hub, &dir, &["commit", "-qm", "trusted lock policy"]).await?;
    crate::gitops::sync_mirror(hub, &caller.repo).await?;
    Ok(())
}

#[test]
fn w29_r26_a_feature_cannot_relax_trunk_and_report_is_distinct_from_enforce() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let root = tempfile::tempdir()?;
        let hub = crate::hub::tests::scratch_hub().await?;
        let text = WORKFLOW.replace("@v1", "@v1.2.3");
        write(&root.path().join("caller"), vec![(".kiln/config.yaml".into(), "require_lock: enforce\n".into())]).await?;
        let mut caller = caller(&hub, root.path(), &text, None).await?;
        let dir = root.path().join("caller");
        git(&hub, &dir, &["checkout", "-qb", "feature"]).await?;
        write(&dir, vec![(".kiln/config.yaml".into(), "require_lock: off\nmoved_tags: flag\n".into())]).await?;
        git(&hub, &dir, &["add", "."]).await?;
        git(&hub, &dir, &["commit", "-qm", "feature cannot change policy"]).await?;
        caller.commit = git(&hub, &dir, &["rev-parse", "HEAD"]).await?;
        crate::gitops::sync_mirror(&hub, &caller.repo).await?;
        let (source, _) = simulated()?;
        let reads = Arc::new(Mutex::new(Vec::new()));
        let source = Counted { inner: source, reads: Arc::clone(&reads) };
        let refused = prepare_with(&hub, &source, &caller.repo, &caller.commit, PATH, Arc::clone(&caller.model)).await;
        assert!(refused.is_err_and(|why| why.contains("KCI4055")));
        assert!(reads.lock().is_empty(), "enforcement precedes dependency metadata");
        change_trunk(&hub, &caller, root.path(), "require_lock: report\n").await?;
        let report = prepare_with(&hub, &source, &caller.repo, &caller.commit, PATH, Arc::clone(&caller.model)).await.map_err(anyhow::Error::msg)?;
        assert!(!report.root_locked);
        assert!(report.warnings.iter().any(|w| w.contains("KCI4055")));
        assert_eq!(report.root_actions().map_err(anyhow::Error::msg)?.get("acme/tools@v1.2.3").map(|a| a.sha.as_str()), Some(A));
        change_trunk(&hub, &caller, root.path(), "require_lock: typo\n").await?;
        assert!(prepare_with(&hub, &source, &caller.repo, &caller.commit, PATH, caller.model).await.is_err_and(|e| e.contains("KCI4055")), "an unreadable trusted policy cannot fall back to off");
        Ok(())
    })
}

fn notification(config: &mut crate::config::Config) {
    config.ci.notify.push(crate::ci::notify::Rule { on: vec!["tag_moved".into()], cond: None, to: vec!["inbox:lock-observer".into()], throttle: None });
}

/// Starts the relay of kiln's events and the notification rules' reader, and waits until the reader
/// stands on kiln.events: a moved tag's notice is delivered from there (crate::events), to what
/// happens after the reader stands.
pub(super) async fn told(hub: &Arc<Hub>) -> anyhow::Result<()> {
    crate::events::spawn(hub);
    crate::ci::notify::spawn(hub);
    for _ in 0..3000 {
        if hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM event_positions WHERE reader = 'notify'", [], |r| r.get::<_, i64>(0))?)).await? == 1 {
            return Ok(());
        }
        crate::rt::sleep(std::time::Duration::from_millis(10)).await;
    }
    anyhow::bail!("the notification rules' reader never stood on kiln.events")
}

/// [`counts`] once the notices told by then are delivered: as `want` within 30 s, else as they are.
async fn counts_settled(hub: &Hub, want: (&str, &str, &str, i64, i64)) -> anyhow::Result<(String, String, String, i64, i64)> {
    let mut now = counts(hub).await?;
    for _ in 0..3000 {
        if (now.0.as_str(), now.1.as_str(), now.2.as_str(), now.3, now.4) == want {
            break;
        }
        crate::rt::sleep(std::time::Duration::from_millis(10)).await;
        now = counts(hub).await?;
    }
    Ok(now)
}

async fn counts(hub: &Hub) -> anyhow::Result<(String, String, String, i64, i64)> {
    hub.store.call(|c| {
        let (first, accepted, last) = c.query_row("SELECT first_commit, accepted_commit, last_commit FROM ci_lock_refs WHERE git_ref = 'refs/tags/v1.2.3'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        let moves = c.query_row("SELECT COUNT(*) FROM ci_lock_moves", [], |r| r.get(0))?;
        let notices = c.query_row("SELECT COUNT(*) FROM ci_inbox WHERE event = 'tag_moved' AND person = 'user:lock-observer'", [], |r| r.get(0))?;
        Ok((first, accepted, last, moves, notices))
    }).await
}

#[test]
fn w29_r26_moved_tags_are_not_blessed_or_notified_again_after_reopening_the_store() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let root = tempfile::tempdir()?;
        let database = root.path().join("history.db");
        let hub = crate::hub::tests::scratch_hub_persistent(database.clone(), notification).await?;
        told(&hub).await?;
        let text = WORKFLOW.replace("@v1", "@v1.2.3");
        let caller = caller(&hub, root.path(), &text, None).await?;
        let (source, repository) = simulated()?;
        let first = prepare_with(&hub, &source, &caller.repo, &caller.commit, PATH, Arc::clone(&caller.model)).await.map_err(anyhow::Error::msg)?;
        assert!(first.warnings.is_empty());
        source.move_ref(&repository, FULL, M)?;
        for _ in 0..2 {
            let next = prepare_with(&hub, &source, &caller.repo, &caller.commit, PATH, Arc::clone(&caller.model)).await.map_err(anyhow::Error::msg)?;
            assert!(next.warnings.iter().any(|w| w.contains("KCI4056")));
            assert_eq!(next.root_actions().map_err(anyhow::Error::msg)?.get("acme/tools@v1.2.3").map(|a| a.sha.as_str()), Some(M));
        }
        assert_eq!(counts_settled(&hub, (A, A, M, 1, 1)).await?, (A.into(), A.into(), M.into(), 1, 1));
        let moved = moved_tags(&hub, "acme/tools", false).await?;
        assert_eq!(moved.len(), 1);
        assert!(moved_tags(&hub, "acme/tools", true).await?.is_empty(), "a native repository with the same name has its own history");
        assert_eq!(moved.first().and_then(|m| m.get("locked")).and_then(serde_json::Value::as_str), Some(A));
        assert_eq!(moved.first().and_then(|m| m.get("now")).and_then(serde_json::Value::as_str), Some(M));
        drop(hub);
        let hub = crate::hub::tests::scratch_hub_persistent(database, |config| { notification(config); config.ci.locks.moved_tags = verify::MovedTags::Refuse; }).await?;
        told(&hub).await?;
        crate::gitops::sync_mirror(&hub, &caller.repo).await?;
        notify_moves(&hub).await?;
        notify_moves(&hub).await?;
        let reads = Arc::new(Mutex::new(Vec::new()));
        let traced = Counted { inner: source.clone(), reads: Arc::clone(&reads) };
        let refused = prepare_with(&hub, &traced, &caller.repo, &caller.commit, PATH, Arc::clone(&caller.model)).await;
        assert!(refused.is_err_and(|e| e.contains("KCI4056")));
        assert!(reads.lock().is_empty());
        assert_eq!(counts_settled(&hub, (A, A, M, 1, 1)).await?, (A.into(), A.into(), M.into(), 1, 1));
        source.replace_identity(&repository, Identity::Github { hostname: "github.com".into(), owner_id: 99, repo_id: 100 })?;
        assert!(prepare_with(&hub, &traced, &caller.repo, &caller.commit, PATH, Arc::clone(&caller.model)).await.is_err_and(|e| e.contains("KCI4052")));
        assert!(reads.lock().is_empty());
        source.replace_identity(&repository, Identity::Github { hostname: "github.com".into(), owner_id: 41, repo_id: 42 })?;
        source.move_ref(&repository, FULL, U)?;
        assert!(prepare_with(&hub, &traced, &caller.repo, &caller.commit, PATH, caller.model).await.is_err_and(|e| e.contains("KCI4056")), "an unrelated new commit is still a moved tag");
        let (first, accepted, last, _, _) = counts(&hub).await?;
        assert_eq!((first.as_str(), accepted.as_str(), last.as_str()), (A, A, U));
        hub.store.call(|c| Ok(c.execute("UPDATE ci_lock_moves SET observed_at = 1", [])?)).await?;
        let moved = moved_tags(&hub, "acme/tools", false).await?;
        assert_eq!(moved.last().and_then(|m| m.get("now")).and_then(serde_json::Value::as_str), Some(U), "equal observation times retain the newest catalog flag");
        Ok(())
    })
}

#[test]
fn w29_r26_a_preview_neither_accepts_a_first_resolution_nor_records_a_move() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let root = tempfile::tempdir()?;
        let hub = crate::hub::tests::persisted_hub_on(crate::rt::clock(), notification).await?;
        told(&hub).await?;
        let text = WORKFLOW.replace("@v1", "@v1.2.3");
        let caller = caller(&hub, root.path(), &text, None).await?;
        let (source, repository) = simulated()?;
        let preview = capture_with(&hub, &source, &caller.repo, &caller.commit, PATH, Arc::clone(&caller.model), false).await.map_err(anyhow::Error::msg)?;
        assert!(preview.warnings.is_empty());
        let accepted: i64 = hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM ci_lock_refs", [], |r| r.get(0))?)).await?;
        assert_eq!(accepted, 0, "merely inspecting a workflow cannot establish trust");
        prepare_with(&hub, &source, &caller.repo, &caller.commit, PATH, Arc::clone(&caller.model)).await.map_err(anyhow::Error::msg)?;
        source.move_ref(&repository, FULL, M)?;
        let preview = capture_with(&hub, &source, &caller.repo, &caller.commit, PATH, Arc::clone(&caller.model), false).await.map_err(anyhow::Error::msg)?;
        assert!(preview.warnings.iter().any(|w| w.contains("KCI4056")));
        assert_eq!(counts(&hub).await?, (A.into(), A.into(), A.into(), 0, 0));
        assert!(moved_tags(&hub, "acme/tools", false).await?.is_empty());
        // A real start records the same observation and reaches only declared channels.
        prepare_with(&hub, &source, &caller.repo, &caller.commit, PATH, caller.model).await.map_err(anyhow::Error::msg)?;
        assert_eq!(counts_settled(&hub, (A, A, M, 1, 1)).await?, (A.into(), A.into(), M.into(), 1, 1));
        Ok(())
    })
}

#[test]
fn w29_r26_required_stale_coverage_reports_both_the_missing_pin_and_policy() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let root = tempfile::tempdir()?;
        let hub = crate::hub::tests::scratch_hub().await?;
        write(&root.path().join("caller"), vec![(".kiln/config.yaml".into(), "require_lock: enforce\n".into())]).await?;
        let empty = github::File { workflows: [(PATH.into(), Vec::new())].into(), ..Default::default() }.write("github.com").map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let caller = caller(&hub, root.path(), WORKFLOW, Some(empty)).await?;
        let (source, _) = simulated()?;
        let reads = Arc::new(Mutex::new(Vec::new()));
        let source = Counted { inner: source, reads: Arc::clone(&reads) };
        let why = prepare_with(&hub, &source, &caller.repo, &caller.commit, PATH, caller.model).await.err().ok_or_else(|| anyhow::anyhow!("stale lock accepted"))?;
        assert!(why.contains("KCI4050") && why.contains("KCI4055"), "{why}");
        assert!(reads.lock().is_empty());
        Ok(())
    })
}

#[test]
fn w29_r26_two_preflights_cannot_both_establish_different_first_acceptances() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let root = tempfile::tempdir()?;
        let hub = crate::hub::tests::scratch_hub().await?;
        let caller = caller(&hub, root.path(), WORKFLOW, None).await?;
        let (source, repository) = simulated()?;
        let remote = Remote::parse("acme/tools@v1.2.3").ok_or_else(|| anyhow::anyhow!("no remote"))?;
        let revision_a = source.resolve(repository.clone(), "v1.2.3".into()).await?;
        let first = super::super::history::Seen::read(&hub, &source, &remote, &revision_a).await.map_err(anyhow::Error::msg)?;
        source.move_ref(&repository, FULL, M)?;
        let revision_m = source.resolve(repository, "v1.2.3".into()).await?;
        let second = super::super::history::Seen::read(&hub, &source, &remote, &revision_m).await.map_err(anyhow::Error::msg)?;
        assert!(first.previous.is_none() && second.previous.is_none());
        first.record(&hub, Some(&caller.repo), PATH, true).await.map_err(anyhow::Error::msg)?;
        let effective = second.record(&hub, Some(&caller.repo), PATH, true).await.map_err(anyhow::Error::msg)?;
        assert_eq!(effective.previous.as_deref(), Some(A));
        let (first, accepted, last, moves, _) = counts(&hub).await?;
        assert_eq!((first.as_str(), accepted.as_str(), last.as_str(), moves), (A, A, M, 1));
        Ok(())
    })
}
