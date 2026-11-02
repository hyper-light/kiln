use super::*;

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const PATH: &str = ".kiln/ci.yaml";

fn entry(reference: &str) -> kiln::Git { kiln::Git { git_ref: reference.into(), commit: format!("sha1-{A}"), repo: "kiln:published-repository".into(), ..Default::default() } }

fn facts(reference: &str, current: &str) -> GitFacts {
    GitFacts { identity: kiln::Identity::Kiln { id: "published-repository".into() }, reference: reference.into(), current: current.into(), reachable: [A.into()].into(), previous: None }
}

fn check(key: &str, entry: &kiln::Git, facts: &GitFacts, policy: verify::Policy) -> Result<Verified, verify::Refused> {
    check_git(Check { workflow: PATH, key, entry, facts, home_host: "github.com", policy, accepted_moves: &BTreeMap::new() })
}

#[test]
fn w29_r26_editing_a_native_lock_to_a_known_moved_full_tag_requires_review() {
    let key = "kiln://acme/kit@v1.2.3";
    let mut pin = entry("refs/tags/v1.2.3");
    pin.commit = format!("sha1-{B}");
    let mut observed = facts("refs/tags/v1.2.3", B);
    observed.previous = Some(A.into());
    assert!(check(key, &pin, &observed, verify::Policy::default()).is_err_and(|e| e.diagnostics.iter().any(|d| d.code == "KCI4053")));
    let accepted = [(key.into(), B.into())].into();
    let approved = check_git(Check { workflow: PATH, key, entry: &pin, facts: &observed, home_host: "github.com", policy: verify::Policy::default(), accepted_moves: &accepted });
    assert!(approved.is_ok_and(|v| v.selection.commit == B));
}

#[test]
fn w29_r24_native_pins_keep_a_and_full_versions_refuse_movement() -> Result<(), String> {
    let verified = check("kiln://acme/kit@v1", &entry("refs/tags/v1"), &facts("refs/tags/v1", B), verify::Policy::default()).map_err(|e| format!("{:?}", e.diagnostics))?;
    assert_eq!(verified.selection.commit, A);
    assert!(verified.selection.locked);
    assert!(verified.warnings.iter().any(|d| d.code == "KCI4057"));
    let refused = check("kiln://acme/kit@v1.2.3", &entry("refs/tags/v1.2.3"), &facts("refs/tags/v1.2.3", B), verify::Policy::default()).err().ok_or("a moved full version was accepted")?;
    assert!(refused.diagnostics.iter().any(|d| d.code == "KCI4053"));
    let verified = check("kiln://acme/kit@v1.2.3", &entry("refs/heads/v1.2.3"), &facts("refs/heads/v1.2.3", B), verify::Policy::default()).map_err(|e| format!("{:?}", e.diagnostics))?;
    assert_eq!(verified.selection.commit, A);
    Ok(())
}

#[test]
fn w29_r24_uuid_numeric_ids_and_recorded_repository_history_are_bound() -> Result<(), String> {
    let source = entry("refs/tags/v1");
    let mut changed = facts("refs/tags/v1", B);
    changed.identity = kiln::Identity::Kiln { id: "reclaimed-name".into() };
    assert!(check("kiln://acme/kit@v1", &source, &changed, verify::Policy::default()).err().is_some_and(|r| r.diagnostics.iter().any(|d| d.code == "KCI4052")));
    for proof in [GitFacts { reachable: BTreeSet::new(), ..facts("refs/tags/v1", B) }, facts("refs/heads/other", B), facts(A, A), facts("refs/tags/v1", "bad")] {
        assert!(check("kiln://acme/kit@v1", &source, &proof, verify::Policy::default()).err().is_some_and(|r| r.diagnostics.iter().any(|d| d.code == "KCI4051")));
    }
    let source = kiln::Git { repo: "github:41/42".into(), ..source };
    let mut proof = facts("refs/tags/v1", A);
    proof.identity = kiln::Identity::Github { hostname: "github.com".into(), owner_id: 41, repo_id: 42 };
    assert!(check("acme/shared/.github/workflows/ci.yml@v1", &source, &proof, verify::Policy::default()).is_ok());
    proof.identity = kiln::Identity::Github { hostname: "other.ghe.com".into(), owner_id: 41, repo_id: 42 };
    assert!(check("acme/shared/.github/workflows/ci.yml@v1", &source, &proof, verify::Policy::default()).err().is_some_and(|r| r.diagnostics.iter().any(|d| d.code == "KCI4052")));
    Ok(())
}

#[test]
fn w29_r24_exact_subaction_and_workflow_paths_take_priority() -> Result<(), String> {
    let base = "kiln://acme/kit@v1";
    let sub = "kiln://acme/kit/sub@v1";
    let call = "kiln://acme/kit/.kiln/reusable.yaml@v1";
    let mut file = kiln::File::default();
    file.workflows.insert(PATH.into(), vec![base.into(), sub.into(), call.into()]);
    file.dependencies.insert(base.into(), kiln::Dependency::Git(entry("refs/tags/v1")));
    file.dependencies.insert(sub.into(), kiln::Dependency::Git(kiln::Git { commit: format!("sha1-{B}"), ..entry("refs/tags/v1") }));
    file.dependencies.insert(call.into(), kiln::Dependency::Git(entry("refs/tags/v1")));
    assert_eq!(pin_for(&file, PATH, sub).map(|(_, g)| g.commit.as_str()), Some(format!("sha1-{B}").as_str()));
    assert_eq!(pin_for(&file, PATH, "kiln://acme/kit/other@v1").map(|(k, _)| k), Some(base));
    assert!(pin_for(&file, PATH, "kiln://acme/kit/.kiln/missing.yaml@v1").is_none());
    assert_eq!(pin_for(&file, PATH, &format!("kiln://acme/kit/.kiln/reusable.yaml@{A}")).map(|(key, _)| key), Some(call));
    assert!(pin_for(&file, ".kiln/other.yaml", sub).is_none());
    Ok(())
}

#[test]
fn w29_r26_native_history_requires_reviewed_acceptance_when_enforced() -> Result<(), String> {
    let key = "kiln://acme/kit@v1.2.3";
    let source = entry("refs/tags/v1.2.3");
    let proof = GitFacts { previous: Some(B.into()), ..facts("refs/tags/v1.2.3", A) };
    let policy = verify::Policy { require: verify::Require::Enforce, ..Default::default() };
    assert!(check(key, &source, &proof, policy).err().is_some_and(|r| r.diagnostics.iter().any(|d| d.code == "KCI4056")));
    let accepted = check_git(Check { workflow: PATH, key, entry: &source, facts: &proof, home_host: "github.com", policy, accepted_moves: &[(key.into(), A.into())].into() }).map_err(|e| format!("{:?}", e.diagnostics))?;
    assert_eq!(accepted.selection.commit, A);
    assert_eq!(accepted.moved.len(), 1);
    Ok(())
}
