//! W29-R25: real native draft versions, exact companion bytes and read-only verification.

use kiln_ci::lock::{kiln as format, regenerate};
use kiln_seams::dependencies::{SimDependencySources, SimRepository, contract};

use super::*;

fn fail(e: crate::ci::Fail) -> anyhow::Error { anyhow::anyhow!("{} {}", e.0, e.1) }
fn field<'a>(value: &'a serde_json::Value, at: &str) -> &'a str { value.pointer(at).and_then(serde_json::Value::as_str).unwrap_or_default() }

async fn draft_head(hub: &Arc<Hub>, repo: &Repo, value: &serde_json::Value) -> anyhow::Result<String> {
    let draft = crate::ci::pipeline_drafts::get(hub, field(value, "/draft/id")).await?.ok_or_else(|| anyhow::anyhow!("draft missing"))?;
    crate::ci::pipeline_drafts::head(hub, repo, &draft).await
}

#[test]
fn w29_r25_save_and_update_share_one_native_draft_and_retain_its_companions() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let root = tempfile::tempdir()?;
        let hub = crate::hub::tests::scratch_hub().await?;
        let publisher = native::publish(&hub, root.path(), "acme/tools", vec![("action.yml".into(), "runs: {using: composite, steps: []}\n".into())], "owner").await?;
        let text = WORKFLOW.replace(USES, "kiln://acme/tools@v1");
        let app = native::publish(&hub, root.path(), "acme/app", vec![(PATH.into(), text.clone())], "none").await?;
        crate::gitops::sync_mirror(&hub, &app.repo).await?;
        let author = crate::authz::Caller::owner();
        let overlay = ".kiln/overlays/ci.yaml";
        let first = crate::ci::workflows::edit(&hub, &author, &app.repo.name, PATH, &serde_json::json!({ "content": format!("{text}# first edit\n"), "draft": true, "files": [{"path": overlay, "text": "on: {diff: {}}\n"}] })).await.map_err(fail)?;
        let first_head = draft_head(&hub, &app.repo, &first).await?;
        let gh = crate::ci::load::blob(&hub, &app.repo, &first_head, github::PATH).await?;
        let kl = crate::ci::load::blob(&hub, &app.repo, &first_head, format::PATH).await?;
        let parsed = format::parse(&kl).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        assert_eq!(parsed.dependencies.get("kiln://acme/tools@v1").and_then(|d| if let format::Dependency::Git(g) = d { github::digest(&g.commit) } else { None }), Some(publisher.commit.as_str()));
        assert_eq!(crate::ci::load::blob(&hub, &app.repo, &first_head, overlay).await?, "on: {diff: {}}\n");
        let standalone = crate::ops::call(crate::ops::Cx { hub: Arc::clone(&hub), caller: author.clone() }, "ci.locks.update", serde_json::json!({"repo": app.repo.name})).await.map_err(fail)?;
        assert_eq!(field(&standalone, "/draft/id"), field(&first, "/draft/id"));
        let standalone_head = draft_head(&hub, &app.repo, &standalone).await?;
        assert_eq!(crate::ci::load::blob(&hub, &app.repo, &standalone_head, github::PATH).await?, gh);
        assert_eq!(crate::ci::load::blob(&hub, &app.repo, &standalone_head, format::PATH).await?, kl);
        native::advance(&hub, &publisher, vec![("action.yml".into(), "runs: {using: composite, steps: [{run: echo changed}]}\n".into())]).await?;
        let second = crate::ci::workflows::edit(&hub, &author, &app.repo.name, PATH, &serde_json::json!({ "content": format!("{text}# second edit\n"), "draft": true })).await.map_err(fail)?;
        assert_eq!((field(&second, "/draft/id"), field(&second, "/draft/change")), (field(&first, "/draft/id"), field(&first, "/draft/change")));
        let second_head = draft_head(&hub, &app.repo, &second).await?;
        assert_eq!(crate::ci::load::blob(&hub, &app.repo, &second_head, overlay).await?, "on: {diff: {}}\n", "saving the buffer retains its earlier overlay");
        assert_eq!(crate::ci::load::blob(&hub, &app.repo, &second_head, format::PATH).await?, kl, "a moved partial ref preserves the accepted pin");
        assert_eq!(git(&hub, &app.bare, &["rev-parse", "main"]).await?, app.commit, "only draft refs changed");
        let check = crate::ops::call(crate::ops::Cx { hub: Arc::clone(&hub), caller: author.clone() }, "ci.locks.verify", serde_json::json!({"repo": app.repo.name, "sha": second_head})).await.map_err(fail)?;
        assert_eq!(check.get("fresh"), Some(&serde_json::json!(true)), "{check}");
        let (drafts, accepted, notices): (i64, i64, i64) = hub.store.call(|c| Ok((c.query_row("SELECT COUNT(*) FROM ci_pipeline_drafts", [], |r| r.get(0))?, c.query_row("SELECT COUNT(*) FROM ci_lock_refs", [], |r| r.get(0))?, c.query_row("SELECT COUNT(*) FROM ci_inbox WHERE event = 'tag_moved'", [], |r| r.get(0))?))).await?;
        assert_eq!((drafts, accepted, notices), (1, 0, 0), "generation and verify cannot establish tag trust or send a notice");
        Ok(())
    })
}

/// W29-R27: a code workflow's draft locks what its program uses, as a YAML workflow's does; a
/// program that throws isn't saved, the refusal naming its code and line.
#[test]
fn w29_r27_a_code_workflows_draft_locks_what_its_program_uses() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let root = tempfile::tempdir()?;
        let hub = crate::hub::tests::scratch_hub().await?;
        let publisher = native::publish(&hub, root.path(), "acme/tools", vec![("action.yml".into(), "runs: {using: composite, steps: []}\n".into())], "owner").await?;
        let program = "import { workflow, job, step } from \"@kiln/sdk\";\nexport default workflow({ on: \"push\", jobs: { build: job({ \"runs-on\": \"k8s\", steps: [step({ uses: \"kiln://acme/tools@v1\" })] }) } });\n";
        let app = native::publish(&hub, root.path(), "acme/app", vec![(".kiln/ci.ts".into(), program.to_owned())], "none").await?;
        crate::gitops::sync_mirror(&hub, &app.repo).await?;
        let author = crate::authz::Caller::owner();
        let saved = crate::ci::workflows::edit(&hub, &author, &app.repo.name, ".kiln/ci.ts", &serde_json::json!({ "content": format!("{program}// saved\n"), "draft": true })).await.map_err(fail)?;
        let head = draft_head(&hub, &app.repo, &saved).await?;
        let kl = format::parse(&crate::ci::load::blob(&hub, &app.repo, &head, format::PATH).await?).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let pinned = kl.dependencies.get("kiln://acme/tools@v1").and_then(|d| if let format::Dependency::Git(g) = d { github::digest(&g.commit) } else { None });
        assert_eq!(pinned, Some(publisher.commit.as_str()));
        let refused = crate::ci::workflows::edit(&hub, &author, &app.repo.name, ".kiln/ci.ts", &serde_json::json!({ "content": "const n: number = 1;\nthrow new Error(\"not yet\");\nexport default {};\n", "draft": true })).await;
        let Err((status, message)) = refused else { anyhow::bail!("a program that throws was saved") };
        assert!(status == http::StatusCode::UNPROCESSABLE_ENTITY && message.contains("KCI1030") && message.contains(":2:"), "{status} {message}");
        Ok(())
    })
}

#[test]
fn w29_r25_verify_reports_a_moved_full_tag_without_acceptance_or_delivery() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let root = tempfile::tempdir()?;
        let hub = crate::hub::tests::scratch_hub().await?;
        let (source, repo) = policy::simulated()?;
        let a = "a".repeat(40);
        let m = "b".repeat(40);
        let text = WORKFLOW.replace("@v1", "@v1.2.3");
        let app = caller(&hub, root.path(), &text, Some(lock(PATH, "v1.2.3", &a)?)).await?;
        source.move_ref(&repo, "refs/tags/v1.2.3", &m)?;
        let reads = Arc::new(Mutex::new(Vec::new()));
        let counted = Counted { inner: source.clone(), reads: Arc::clone(&reads) };
        let result = super::super::api::verify_with(&hub, Arc::new(counted), &app.repo, &app.commit).await.map_err(fail)?;
        assert_eq!(result.get("fresh"), Some(&serde_json::json!(false)), "{result}");
        assert_eq!((field(&result, "/stale/0/workflow"), field(&result, "/stale/0/uses"), field(&result, "/stale/0/locked"), field(&result, "/stale/0/now")), (PATH, "acme/tools@v1.2.3", a.as_str(), m.as_str()), "{result}");
        assert!(field(&result, "/stale/0/error").contains("KCI4053"));
        assert!(reads.lock().is_empty(), "the moved full tag is refused before dependency metadata");
        let counts: (i64, i64, i64) = hub.store.call(|c| Ok((c.query_row("SELECT COUNT(*) FROM ci_lock_refs", [], |r| r.get(0))?, c.query_row("SELECT COUNT(*) FROM ci_lock_moves", [], |r| r.get(0))?, c.query_row("SELECT COUNT(*) FROM ci_pipeline_drafts", [], |r| r.get(0))?))).await?;
        assert_eq!(counts, (0, 0, 0));
        let reader = crate::authz::Caller::with(crate::authz::gate::Kind::Pat, "lock-reader", Some(crate::authz::gate::Rights { grants: vec![crate::authz::Grant { what: "runs.read".into(), scope: crate::authz::Scope::Repo { repo: app.repo.name.clone() }, deny: false }], ..Default::default() }), None);
        assert!(update(&hub, &reader, &app.repo.name, &serde_json::json!({})).await.is_err_and(|e| e.0 == http::StatusCode::FORBIDDEN));
        assert_eq!(std::fs::read_to_string(root.path().join("caller").join(github::PATH))?, lock(PATH, "v1.2.3", &a)?);
        Ok(())
    })
}

#[test]
fn w29_r25_corrupt_future_locks_refuse_the_native_writer_before_a_draft_exists() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let root = tempfile::tempdir()?;
        let hub = crate::hub::tests::scratch_hub().await?;
        let corrupt = "version: v9.0.0\nworkflows: {}\ndependencies: {}\n";
        let app = native::publish(&hub, root.path(), "acme/app", vec![(PATH.into(), "on: push\njobs: {check: {runs-on: baremetal, steps: [{run: echo original}]}}\n".into()), (github::PATH.into(), corrupt.into())], "none").await?;
        crate::gitops::sync_mirror(&hub, &app.repo).await?;
        let owner = crate::authz::Caller::owner();
        let result = update(&hub, &owner, &app.repo.name, &serde_json::json!({})).await;
        assert!(result.is_err_and(|e| e.0 == http::StatusCode::UNPROCESSABLE_ENTITY && e.1.contains("KCI1022")));
        let count: i64 = hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM ci_pipeline_drafts", [], |r| r.get(0))?)).await?;
        assert_eq!(count, 0);
        assert_eq!(git(&hub, &app.bare, &["show", &format!("main:{}", github::PATH)]).await?, corrupt.trim_end());
        Ok(())
    })
}

#[test]
fn w29_r25_generation_reads_accepted_composite_metadata_images_and_runtime_archives() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let root = tempfile::tempdir()?;
        let hub = crate::hub::tests::scratch_hub().await?;
        let source = SimDependencySources::default();
        let tools = Repository { name: "acme/tools".into(), native: false };
        let inner = Repository { name: "acme/inner".into(), native: false };
        let (a, m, c) = ("a".repeat(40), "b".repeat(40), "c".repeat(40));
        source.put_repository(tools.clone(), SimRepository { identity: Identity::Github { hostname: "github.com".into(), owner_id: 41, repo_id: 42 }, default_branch: "main".into(), refs: [("refs/heads/main".into(), m.clone()), ("refs/tags/v1".into(), a.clone())].into(), commits: [(a.clone(), vec![]), (m.clone(), vec![a.clone()])].into(), files: [((a.clone(), "action.yml".into()), b"runs: {using: composite, steps: [{uses: acme/inner@v2}]}\n".to_vec()), ((m.clone(), "action.yml".into()), b"runs: {using: node24, main: evil.js}\n".to_vec())].into() })?;
        source.put_repository(inner, SimRepository { identity: Identity::Github { hostname: "github.com".into(), owner_id: 41, repo_id: 43 }, default_branch: "main".into(), refs: [("refs/heads/main".into(), c.clone()), ("refs/tags/v2".into(), c.clone())].into(), commits: [(c.clone(), vec![])].into(), files: [((c.clone(), "action.yml".into()), b"runs: {using: node20, main: main.js}\n".to_vec())].into() })?;
        let archive = contract::fixture_archive();
        source.put_runtime(contract::fixture_runtime(), archive.clone())?;
        source.put_runtime(Runtime { version: Some(archive.version.clone()), ..contract::fixture_runtime() }, archive.clone())?;
        let manifest = contract::fixture_manifest();
        let image = "registry.test/acme/base:3";
        source.put_manifest(image.into(), manifest.clone())?;
        source.put_manifest(format!("registry.test/acme/base@{}", manifest.digest), manifest.clone())?;
        for image in ["registry.test/acme/a:1", "registry.test/acme/b:2"] {
            source.put_manifest(image.into(), manifest.clone())?;
            let pinned = kiln_ci::lock::execution::pin_image(image, &manifest.digest).map_err(anyhow::Error::msg)?;
            source.put_manifest(pinned, manifest.clone())?;
        }
        let dir = root.path().join("caller");
        write(&dir, vec![("build/action.yml".into(), "runs: {using: docker, image: Dockerfile}\n".into()), ("build/Dockerfile".into(), format!("FROM {image}\nRUN echo original\n"))]).await?;
        let matrix = WORKFLOW.replace("    steps:", "    strategy:\n      matrix:\n        image: [registry.test/acme/a:1, registry.test/acme/b:2]\n    container: ${{ matrix.image }}\n    steps:");
        let text = format!("{matrix}      - uses: ./build\n");
        let mut app = caller(&hub, root.path(), &text, None).await?;
        let reads = Arc::new(Mutex::new(Vec::new()));
        let counted = Counted { inner: source.clone(), reads: Arc::clone(&reads) };
        let generated = super::super::generate::collect(&hub, &counted, &app.repo, &app.commit, BTreeMap::new(), None, regenerate::Mode::Preserve).await.map_err(anyhow::Error::msg)?;
        let gh = generated.github.clone().ok_or_else(|| anyhow::anyhow!("no GitHub file"))?;
        let kl = generated.kiln.clone().ok_or_else(|| anyhow::anyhow!("no native supplement"))?;
        let parsed = github::parse(&gh).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        assert_eq!(parsed.workflows.get(PATH), Some(&vec![USES.into()]));
        assert_eq!(parsed.dependencies.get(USES).map(|a| &a.uses), Some(&vec!["acme/inner@v2".into()]));
        let native = format::parse(&kl).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        assert!(matches!(native.dependencies.get("node20"), Some(format::Dependency::Runtime(r)) if r.archives.get("linux/amd64").is_some_and(|a| a.sha256 == archive.sha256)));
        write(&dir, vec![(github::PATH.into(), gh), (format::PATH.into(), kl)]).await?;
        git(&hub, &dir, &["add", "."]).await?;
        git(&hub, &dir, &["commit", "-qm", "accepted locks"]).await?;
        app.commit = git(&hub, &dir, &["rev-parse", "HEAD"]).await?;
        crate::gitops::sync_mirror(&hub, &app.repo).await?;
        let frozen = capture_with(&hub, &counted, &app.repo, &app.commit, PATH, model(PATH, &text)?, false).await.map_err(anyhow::Error::msg)?;
        let execution = frozen.scopes.values().find_map(|s| s.execution.get("check")).ok_or_else(|| anyhow::anyhow!("no execution image snapshot"))?;
        for image in ["registry.test/acme/a:1", "registry.test/acme/b:2"] {
            assert_eq!(execution.image(image).map_err(anyhow::Error::msg)?, kiln_ci::lock::execution::pin_image(image, &manifest.digest).map_err(anyhow::Error::msg)?);
        }
        source.move_ref(&tools, "refs/tags/v1", &m)?;
        source.put_runtime(contract::fixture_runtime(), contract::fixture_new_archive())?;
        reads.lock().clear();
        let again = super::super::generate::collect(&hub, &counted, &app.repo, &app.commit, BTreeMap::new(), None, regenerate::Mode::Preserve).await.map_err(anyhow::Error::msg)?;
        assert_eq!(again, generated, "ordinary saves keep both lockfiles byte-identical");
        assert!(reads.lock().iter().all(|(sha, _)| sha == &a || sha == &c), "metadata at M is never read");
        assert_eq!(std::fs::read_to_string(dir.join("build/Dockerfile"))?, format!("FROM {image}\nRUN echo original\n"));
        let mut shorthand = native;
        if let Some(format::Dependency::Runtime(runtime)) = shorthand.dependencies.get_mut("node20") { runtime.sha256 = Some(archive.sha256.clone()); runtime.archives.clear(); }
        let old = shorthand.write().map_err(|e| anyhow::anyhow!("{e:?}"))?;
        write(&dir, vec![(format::PATH.into(), old.clone())]).await?;
        git(&hub, &dir, &["add", "."]).await?;
        git(&hub, &dir, &["commit", "-qm", "legacy runtime checksum"]).await?;
        app.commit = git(&hub, &dir, &["rev-parse", "HEAD"]).await?;
        crate::gitops::sync_mirror(&hub, &app.repo).await?;
        let refused = super::super::generate::collect(&hub, &counted, &app.repo, &app.commit, BTreeMap::new(), None, regenerate::Mode::Preserve).await;
        assert!(refused.is_err_and(|why| why.contains("KCI4050") && why.contains("checksum")), "a checksum cannot acquire the newer archive's location");
        assert_eq!(std::fs::read_to_string(dir.join(format::PATH))?, old);
        source.put_runtime(contract::fixture_runtime(), archive.clone())?;
        let recovered = super::super::generate::collect(&hub, &counted, &app.repo, &app.commit, BTreeMap::new(), None, regenerate::Mode::Preserve).await.map_err(anyhow::Error::msg)?;
        let recovered = format::parse(recovered.kiln.as_deref().ok_or_else(|| anyhow::anyhow!("runtime supplement missing"))?).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        assert!(matches!(recovered.dependencies.get("node20"), Some(format::Dependency::Runtime(r)) if r.sha256.is_none() && r.archives.get("linux/amd64").is_some_and(|a| a.sha256 == archive.sha256)));
        Ok(())
    })
}

#[test]
fn w29_r25_unresolved_sources_and_invalid_update_requests_write_no_draft() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let root = tempfile::tempdir()?;
        let hub = crate::hub::tests::scratch_hub().await?;
        let text = WORKFLOW.replace(USES, "kiln://acme/missing@v1");
        let app = native::publish(&hub, root.path(), "acme/app", vec![(PATH.into(), text.clone())], "none").await?;
        let owner = crate::authz::Caller::owner();
        let bad = update(&hub, &owner, &app.repo.name, &serde_json::json!({"ref": 7})).await;
        assert!(bad.is_err_and(|e| e.0 == http::StatusCode::BAD_REQUEST));
        let failed = crate::ci::workflows::edit(&hub, &owner, &app.repo.name, PATH, &serde_json::json!({"content": format!("{text}# edit\n"), "draft": true})).await;
        assert!(failed.is_err_and(|e| e.0 == http::StatusCode::UNPROCESSABLE_ENTITY));
        let count: i64 = hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM ci_pipeline_drafts", [], |r| r.get(0))?)).await?;
        assert_eq!(count, 0);
        assert_eq!(git(&hub, &app.bare, &["rev-parse", "main"]).await?, app.commit);
        Ok(())
    })
}

#[test]
fn w29_r25_verify_reports_unused_roots_and_removed_workflows_without_fetching_them() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let root = tempfile::tempdir()?;
        let hub = crate::hub::tests::scratch_hub().await?;
        let mut file = github::parse(&lock(PATH, "v1", &"a".repeat(40))?).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        file.workflows.insert(".github/workflows/removed.yml".into(), Vec::new());
        let text = WORKFLOW.replace(&format!("uses: {USES}"), "run: echo local");
        let app = caller(&hub, root.path(), &text, Some(file.write("github.com").map_err(|e| anyhow::anyhow!("{e:?}"))?)).await?;
        let result = super::super::api::verify_with(&hub, Arc::new(SimDependencySources::default()), &app.repo, &app.commit).await.map_err(fail)?;
        assert_eq!(result.get("fresh"), Some(&serde_json::json!(false)), "{result}");
        let stale = result.get("stale").and_then(serde_json::Value::as_array).ok_or_else(|| anyhow::anyhow!("no stale rows"))?;
        assert_eq!(stale.len(), 2, "{result}");
        assert!(stale.iter().any(|r| field(r, "/workflow") == ".github/workflows/removed.yml" && field(r, "/error").contains("KCI4055")));
        assert!(stale.iter().any(|r| field(r, "/uses") == USES && field(r, "/locked") == "a".repeat(40) && r.get("now").is_some_and(serde_json::Value::is_null)));
        let count: i64 = hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM ci_lock_refs", [], |r| r.get(0))?)).await?;
        assert_eq!(count, 0);
        Ok(())
    })
}

#[test]
fn w29_r24_enforce_requires_runtime_and_matrix_image_supplements_even_with_a_github_lock() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        for node in [false, true] {
            let root = tempfile::tempdir()?;
            let hub = crate::hub::tests::scratch_hub().await?;
            let source = SimDependencySources::default();
            let a = "a".repeat(40);
            source.put_repository(Repository { name: "acme/tools".into(), native: false }, SimRepository {
                identity: Identity::Github { hostname: "github.com".into(), owner_id: 41, repo_id: 42 }, default_branch: "main".into(),
                refs: [("refs/heads/main".into(), a.clone()), ("refs/tags/v1".into(), a.clone())].into(), commits: [(a.clone(), vec![])].into(),
                files: [((a.clone(), "action.yml".into()), b"runs: {using: node20, main: main.js}\n".to_vec())].into(),
            })?;
            write(&root.path().join("caller"), vec![(".kiln/config.yaml".into(), "require_lock: enforce\n".into())]).await?;
            let (text, gh) = if node { (WORKFLOW.to_owned(), lock(PATH, "v1", &a)?) } else {
                let text = "on: push\njobs:\n  check:\n    runs-on: baremetal\n    strategy:\n      matrix:\n        image: [acme/a:1, acme/b:2]\n    container: ${{ matrix.image }}\n    steps: [{run: echo local}]\n";
                let gh = github::File { workflows: [(PATH.into(), Vec::new())].into(), ..Default::default() }.write("github.com").map_err(|e| anyhow::anyhow!("{e:?}"))?;
                (text.to_owned(), gh)
            };
            let app = caller(&hub, root.path(), &text, Some(gh)).await?;
            let result = capture_with(&hub, &source, &app.repo, &app.commit, PATH, app.model, false).await;
            assert!(result.is_err_and(|why| why.contains("KCI4055") && why.contains("supplemental")), "Git-only locks do not authorize mutable runtime/image inputs (node={node})");
        }
        Ok(())
    })
}
