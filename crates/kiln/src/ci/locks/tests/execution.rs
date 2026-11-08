//! W29-R24: real immutable source files, with moved OCI and runtime metadata at the seam.

use kiln_ci::lock::kiln as format;
use kiln_seams::dependencies::{SimDependencySources, contract};

use super::*;

#[test]
fn w29_r24_images_nested_actions_and_exact_runtimes_survive_moved_tags() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let live = Live::for_locks().await?;
        let hub = live.hub();
        let source = SimDependencySources::default();
        let old = contract::fixture_manifest();
        let mut new = old.clone(); new.bytes.push(b' '); new.digest = format!("sha256:{}", crate::util::sha256_hex(&new.bytes));
        let image = "registry.test/acme/base:3";
        let pinned = format!("registry.test/acme/base@{}", old.digest);
        source.put_manifest(pinned.clone(), old.clone())?;
        source.put_manifest(image.into(), new)?;
        let archive = contract::fixture_archive();
        source.put_runtime(Runtime { version: Some(archive.version.clone()), ..contract::fixture_runtime() }, archive.clone())?;
        source.put_runtime(contract::fixture_runtime(), contract::fixture_new_archive())?;
        let image_key = format!("docker://{image}");
        let native = format::File {
            workflows: [(PATH.into(), vec![image_key.clone(), "node20".into()])].into(),
            dependencies: [
                (image_key, format::Dependency::Image(format::Image { digest: old.digest })),
                ("node20".into(), format::Dependency::Runtime(format::Runtime { archives: [("linux/amd64".into(), format::Archive { version: archive.version.clone(), url: archive.url.clone(), file: archive.file.clone(), sha256: archive.sha256.clone() })].into(), ..Default::default() })),
            ].into(), ..Default::default()
        }.write().map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let dockerfile = format!("FROM {image}\nRUN echo accepted\n");
        let text = format!("on: push\njobs:\n  check:\n    runs-on: baremetal\n    container: {image}\n    services:\n      db: {{image: {image}}}\n    steps:\n      - uses: ./composite\n  plain:\n    runs-on: baremetal\n    steps: [{{run: echo plain}}]\n");
        let dir = live.root().join("caller");
        write(&dir, vec![
            (format::PATH.into(), native),
            ("composite/action.yml".into(), format!("runs:\n  using: composite\n  steps:\n    - uses: ./js\n    - uses: ./build\n    - uses: docker://{image}\n")),
            ("js/action.yml".into(), "runs: {using: node20, main: main.js}\n".into()),
            ("build/action.yml".into(), "runs: {using: docker, image: Dockerfile}\n".into()),
            ("build/Dockerfile".into(), dockerfile.clone()),
        ]).await?;
        let caller = caller(&hub, live.root(), &text, None).await?;
        let policy = serde_json::to_string(&kiln_ci::policy::Policy { allow: vec![format!("docker://{pinned}")], ..Default::default() })?;
        let repo_id = caller.repo.id.clone();
        hub.store.call(move |c| Ok(c.execute("INSERT INTO ci_action_policies (repo_id, policy, updated_at, updated_by) VALUES (?1, ?2, 0, 'lock-test')", [repo_id, policy])?)).await?;
        let frozen = prepare_with(&hub, &source, &caller.repo, &caller.commit, PATH, Arc::clone(&caller.model)).await.map_err(anyhow::Error::msg)?;
        let scope = frozen.scopes.get(&scope_key(&caller.repo.name, PATH, &caller.commit)).ok_or_else(|| anyhow::anyhow!("no root scope"))?;
        let locks = scope.execution.get("check").ok_or_else(|| anyhow::anyhow!("no execution snapshot"))?;
        assert_eq!(locks.image(image).map_err(anyhow::Error::msg)?, pinned);
        assert_eq!(locks.runtime("node20", "linux/amd64").map_err(anyhow::Error::msg)?.version, archive.version);
        let derived = locks.dockerfiles.get("./build").ok_or_else(|| anyhow::anyhow!("no derived Dockerfile"))?;
        assert!(derived.locked.contains(&format!("FROM {pinned}\n")));
        assert_eq!(derived.source_sha256, crate::util::sha256_hex(dockerfile.as_bytes()));
        assert_eq!(std::fs::read_to_string(dir.join("build/Dockerfile"))?, dockerfile);
        assert!(scope.execution.get("plain").is_some_and(|l| l.is_empty()), "a shell-only job still enforces any executor routing gives it");
        let saved = serde_json::json!({ "dependency_snapshot": frozen });
        let restored = Frozen::saved(&saved, &caller.repo.name, PATH, &caller.commit).map_err(anyhow::Error::msg)?.ok_or_else(|| anyhow::anyhow!("no restored snapshot"))?;
        assert_eq!(restored.scopes.get(&scope_key(&caller.repo.name, PATH, &caller.commit)).map(|s| &s.execution), Some(&scope.execution));
        Ok(())
    })
}
