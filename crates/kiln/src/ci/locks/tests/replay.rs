//! W29-R25: real Git conflicts in generated files, with no branch or accepted-history write.

use kiln_ci::lock::{kiln, regenerate};

use super::*;

async fn branch(hub: &Arc<Hub>, app: &native::Published, parent: &str, name: &str, files: Vec<(String, String)>) -> anyhow::Result<String> {
    git(hub, &app.work, &["checkout", "-qb", name, parent]).await?;
    if !files.is_empty() {
        write(&app.work, files).await?;
        git(hub, &app.work, &["add", "."]).await?;
        git(hub, &app.work, &["commit", "-qm", "workflow change"]).await?;
    }
    let reference = format!("HEAD:refs/heads/{name}");
    git(hub, &app.bare, &["fetch", "--quiet", &app.work.to_string_lossy(), &reference]).await?;
    crate::gitops::sync_mirror(hub, &app.repo).await?;
    let head = git(hub, &app.work, &["rev-parse", "HEAD"]).await?;
    let source = crate::ci::dependency_sources::Sources::borrowed(hub, app.repo.name.clone());
    let locks = super::super::generate::collect(hub, &source, &app.repo, &head, BTreeMap::new(), None, regenerate::Mode::Preserve).await.map_err(anyhow::Error::msg)?;
    let gh = locks.github.ok_or_else(|| anyhow::anyhow!("no GitHub workflow lock"))?;
    let kl = locks.kiln.ok_or_else(|| anyhow::anyhow!("no native action lock"))?;
    write(&app.work, vec![(github::PATH.into(), gh), (kiln::PATH.into(), kl)]).await?;
    git(hub, &app.work, &["add", "."]).await?;
    if !git(hub, &app.work, &["diff", "--cached", "--name-only"]).await?.is_empty() { git(hub, &app.work, &["commit", "-qm", "generated locks"]).await?; }
    git(hub, &app.bare, &["fetch", "--quiet", &app.work.to_string_lossy(), &reference]).await?;
    git(hub, &app.work, &["rev-parse", "HEAD"]).await
}

#[test]
fn w29_r25_replay_regenerates_both_conflicted_locks_and_retains_the_accepted_commit() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let root = tempfile::tempdir()?;
        let hub = crate::hub::tests::scratch_hub().await?;
        let tools = native::publish(&hub, root.path(), "acme/tools", vec![("action.yml".into(), "runs: {using: composite, steps: []}\n".into())], "owner").await?;
        let workflow = WORKFLOW.replace(USES, "kiln://acme/tools@v1");
        let app = native::publish(&hub, root.path(), "acme/app", vec![(PATH.into(), workflow.clone())], "none").await?;
        let base = branch(&hub, &app, &app.commit, "base-locks", Vec::new()).await?;
        let onto = branch(&hub, &app, &base, "onto", vec![(".github/workflows/alpha.yml".into(), workflow.clone())]).await?;
        let m = native::advance(&hub, &tools, vec![("action.yml".into(), "runs: {using: composite, steps: [{uses: 'kiln://acme/missing@v1'}]}\n".into())]).await?;
        assert_ne!(tools.commit, m);
        let change = branch(&hub, &app, &base, "change", vec![(".github/workflows/beta.yml".into(), workflow)]).await?;
        let path = app.bare.to_string_lossy();
        let merged = crate::ci::diffs::git_run(&hub, &["-C", &path, "merge-tree", "--write-tree", "--no-messages", "--name-only", &format!("--merge-base={base}"), &onto, &change], &[], Vec::new()).await?;
        assert!(!merged.ok, "the two generated files conflict as text");
        let text = String::from_utf8(merged.out)?;
        let mut lines = text.lines();
        let tree = lines.next().ok_or_else(|| anyhow::anyhow!("no merged tree"))?;
        let conflicts: BTreeSet<_> = lines.collect();
        assert_eq!(conflicts, [github::PATH, kiln::PATH].into(), "{text}");
        let (first, second) = futures::try_join!(super::super::regenerate_tree(&hub, &path, &base, &onto, &change, tree), super::super::regenerate_tree(&hub, &path, &base, &onto, &change, tree)).map_err(anyhow::Error::msg)?;
        assert_eq!(first, second, "the same merged workflow tree gives the same two files");
        for merge in [false, true] {
            let landed = crate::ci::land::tests::replayed_tree(&hub, &path, &base, &onto, &change, merge).await.map_err(anyhow::Error::msg)?;
            assert_eq!(landed, first, "both landing strategies regenerate the same tree");
        }
        crate::ci::land::tests::fast_forward_locks(&hub, &path, &base, &base, &change).await.map_err(anyhow::Error::msg)?;
        let gh = git(&hub, &app.bare, &["show", &format!("{first}:{}", github::PATH)]).await?;
        let gh = github::parse(&gh).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        assert_eq!(gh.workflows.len(), 3);
        let kl = git(&hub, &app.bare, &["show", &format!("{first}:{}", kiln::PATH)]).await?;
        let kl = kiln::parse(&kl).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        assert_eq!(kl.dependencies.len(), 1, "M's metadata would add the missing repository");
        assert!(matches!(kl.dependencies.get("kiln://acme/tools@v1"), Some(kiln::Dependency::Git(g)) if github::digest(&g.commit) == Some(tools.commit.as_str())));
        for (reference, commit) in [("main", &app.commit), ("onto", &onto), ("change", &change)] { assert_eq!(git(&hub, &app.bare, &["rev-parse", reference]).await?, *commit); }
        git(&hub, &app.work, &["checkout", "-qb", "stale", &change]).await?;
        write(&app.work, vec![(".github/workflows/gamma.yml".into(), WORKFLOW.replace(USES, "kiln://acme/tools@v1"))]).await?;
        git(&hub, &app.work, &["add", "."]).await?;
        git(&hub, &app.work, &["commit", "-qm", "workflow without refreshed locks"]).await?;
        git(&hub, &app.bare, &["fetch", "--quiet", &app.work.to_string_lossy(), "HEAD:refs/heads/stale"]).await?;
        let stale = git(&hub, &app.work, &["rev-parse", "HEAD"]).await?;
        assert!(crate::ci::land::tests::fast_forward_locks(&hub, &path, &base, &base, &stale).await.is_err_and(|why| why.starts_with("ff-only:") && why.contains("refresh")));
        let workflow = WORKFLOW.replace(USES, "kiln://acme/tools@v1");
        let left = branch(&hub, &app, &base, "source-left", vec![(PATH.into(), workflow.replace("runs-on: baremetal", "runs-on: [baremetal, left]")), (".github/workflows/left.yml".into(), workflow.clone())]).await?;
        let right = branch(&hub, &app, &base, "source-right", vec![(PATH.into(), workflow.replace("runs-on: baremetal", "runs-on: [baremetal, right]")), (".github/workflows/right.yml".into(), workflow)]).await?;
        for merge in [false, true] {
            let error = crate::ci::land::tests::replayed_tree(&hub, &path, &base, &left, &right, merge).await.err().ok_or_else(|| anyhow::anyhow!("workflow conflict was hidden"))?;
            assert!(error.starts_with("source conflicts:") && error.contains(PATH) && !error.contains(github::PATH) && !error.contains(kiln::PATH), "only the workflow needs a person's resolution: {error}");
        }
        let count: i64 = hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM ci_lock_refs", [], |r| r.get(0))?)).await?;
        assert_eq!(count, 0, "replaying cannot accept a mutable tag");
        Ok(())
    })
}
