use super::*;
use format::{Action, File};

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const M: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const WORKFLOW: &str = ".github/workflows/ci.yml";

fn fixture(reference: &str, current: &str, reaches: bool) -> (File, BTreeMap<String, GitFacts>, Vec<String>) {
    let key = format!("acme/action@{reference}");
    let file = File {
        workflows: [(WORKFLOW.into(), vec![key.clone()])].into(),
        dependencies: [(key.clone(), Action { git_ref: reference.into(), commit: format!("sha1-{A}"), owner_id: 1, repo_id: 2, ..Action::default() })].into(),
        ..File::default()
    };
    let facts = [(key.clone(), GitFacts {
        hostname: "github.com".into(), owner_id: 1, repo_id: 2, reference: format!("refs/tags/{reference}"), current: current.into(),
        reachable: if reaches { [A.to_owned()].into() } else { BTreeSet::new() }, previous: None, requested_ref: String::new(),
    })].into();
    (file, facts, vec![key])
}

fn run(file: Option<&File>, facts: &BTreeMap<String, GitFacts>, uses: &[String], policy: Policy) -> Result<Verified, Refused> {
    check_github(GithubCheck { workflow: WORKFLOW, uses, file, facts, home_host: "github.com", policy, accepted_moves: &BTreeMap::new() })
}

fn codes(result: &Result<Verified, Refused>) -> Vec<&'static str> {
    match result { Ok(ok) => ok.warnings.iter().map(|d| d.code).collect(), Err(no) => no.diagnostics.iter().map(|d| d.code).collect() }
}

#[test]
fn w29_r23_moved_partial_ref_keeps_a_and_full_version_refuses() -> Result<(), String> {
    let (file, facts, uses) = fixture("v45", M, true);
    let ok = run(Some(&file), &facts, &uses, Policy::default()).map_err(|e| format!("{e:?}"))?;
    assert_eq!(ok.pins.get("acme/action@v45").map(|p| (p.commit.as_str(), p.locked)), Some((A, true)));
    assert_eq!(ok.warnings.iter().map(|d| d.code).collect::<Vec<_>>(), ["KCI4057"]);
    let (file, facts, uses) = fixture("v45.0.7", M, true);
    let result = run(Some(&file), &facts, &uses, Policy::default());
    assert!(result.is_err());
    assert_eq!(codes(&result), ["KCI4053"]);
    Ok(())
}

#[test]
fn w29_r23_a_version_shaped_branch_is_still_a_branch() -> Result<(), String> {
    let (file, mut facts, uses) = fixture("v1.2.3", M, true);
    if let Some(f) = facts.get_mut("acme/action@v1.2.3") { f.reference = "refs/heads/v1.2.3".into(); }
    let ok = run(Some(&file), &facts, &uses, Policy::default()).map_err(|e| format!("{e:?}"))?;
    assert_eq!(ok.pins.values().next().map(|p| p.commit.as_str()), Some(A));
    assert_eq!(ok.warnings.iter().map(|d| d.code).collect::<Vec<_>>(), ["KCI4057"]);
    Ok(())
}

#[test]
fn w29_r23_head_keeps_its_real_default_branch_and_requires_the_actual_request() -> Result<(), String> {
    let (file, mut facts, uses) = fixture("HEAD", M, true);
    for asked in ["", "other"] {
        if let Some(f) = facts.get_mut("acme/action@HEAD") { f.reference = "refs/heads/trunk".into(); f.requested_ref = asked.into(); }
        assert_eq!(codes(&run(Some(&file), &facts, &uses, Policy::default())), ["KCI4051"]);
    }
    if let Some(f) = facts.get_mut("acme/action@HEAD") { f.requested_ref = "HEAD".into(); }
    let ok = run(Some(&file), &facts, &uses, Policy::default()).map_err(|e| format!("{e:?}"))?;
    assert_eq!(ok.pins.values().next().map(|p| p.commit.as_str()), Some(A));
    if let Some(f) = facts.get_mut("acme/action@HEAD") { f.reference = M.into(); }
    assert_eq!(codes(&run(Some(&file), &facts, &uses, Policy::default())), ["KCI4051"]);
    Ok(())
}

#[test]
fn w29_r23_forks_unknown_history_and_different_refs_never_prove_a_pin() {
    for reference in ["refs/tags/v45", "refs/heads/other", A, ""] {
        let (file, mut facts, uses) = fixture("v45", M, false);
        if let Some(f) = facts.get_mut("acme/action@v45") { f.reference = reference.into(); }
        let result = run(Some(&file), &facts, &uses, Policy::default());
        assert!(result.is_err(), "{reference}");
        assert_eq!(codes(&result), ["KCI4051"]);
    }
    // Even a matching bare SHA is not evidence of membership in the source's own history.
    let (mut file, mut facts, uses) = fixture(A, A, true);
    if let Some(a) = file.dependencies.get_mut(&format!("acme/action@{A}")) { a.git_ref = A.into(); }
    if let Some(f) = facts.get_mut(&format!("acme/action@{A}")) { f.reference = A.into(); }
    assert_eq!(codes(&run(Some(&file), &facts, &uses, Policy::default())), ["KCI4051"]);
}

#[test]
fn w29_r23_owner_repository_and_host_identities_are_all_bound() {
    for (owner_id, repo_id, host) in [(9, 2, "github.com"), (1, 9, "github.com"), (1, 2, "evil.ghe.com"), (0, 0, "github.com")] {
        let (file, mut facts, uses) = fixture("v1", A, true);
        if let Some(f) = facts.get_mut("acme/action@v1") { f.owner_id = owner_id; f.repo_id = repo_id; f.hostname = host.into(); }
        let result = run(Some(&file), &facts, &uses, Policy::default());
        assert!(result.is_err());
        assert_eq!(codes(&result), ["KCI4052"]);
    }
}

#[test]
fn w29_r23_workflow_scope_and_transitive_coverage() {
    let (mut file, mut facts, mut uses) = fixture("v1", A, true);
    let child = "acme/child@v2".to_owned();
    uses.push(child.clone());
    let result = run(Some(&file), &facts, &uses, Policy::default());
    assert_eq!(codes(&result), ["KCI4050"]);
    if let Some(a) = file.dependencies.get_mut("acme/action@v1") { a.uses.push(child.clone()); }
    let result = run(Some(&file), &facts, &uses, Policy::default());
    assert_eq!(codes(&result), ["KCI4050"]);
    let (_, child_facts, _) = fixture("v2", A, true);
    file.dependencies.insert(child.clone(), Action { git_ref: "v2".into(), commit: format!("sha1-{A}"), owner_id: 1, repo_id: 2, ..Action::default() });
    if let Some(f) = child_facts.values().next() { facts.insert(child.clone(), f.clone()); }
    assert!(run(Some(&file), &facts, &uses, Policy::default()).is_ok());
    // A pin present only under another workflow grants no coverage to this one.
    if let Some(a) = file.dependencies.get_mut("acme/action@v1") { a.uses.clear(); }
    file.workflows.insert(".github/workflows/other.yml".into(), vec![child]);
    assert_eq!(codes(&run(Some(&file), &facts, &uses, Policy::default())), ["KCI4050"]);
}

#[test]
fn w29_r23_a_full_sha_use_can_reuse_a_verified_pin_in_this_workflow() -> Result<(), String> {
    let (file, facts, _) = fixture("v1", A, true);
    let uses = vec![format!("acme/action/sub@{A}")];
    let ok = run(Some(&file), &facts, &uses, Policy::default()).map_err(|e| format!("{e:?}"))?;
    assert_eq!(ok.pins.get(&format!("acme/action@{A}")).map(|p| (p.pin.as_str(), p.commit.as_str())), Some(("acme/action@v1", A)));
    let wrong = vec![format!("acme/action@{M}")];
    assert_eq!(codes(&run(Some(&file), &facts, &wrong, Policy::default())), ["KCI4050"]);
    Ok(())
}

#[test]
fn w29_r26_defaults_report_enforce_and_stricter_policy() -> Result<(), String> {
    assert_eq!(Policy::default(), Policy { require: Require::Off, moved_tags: MovedTags::Flag });
    let (file, facts, uses) = fixture("v1", A, true);
    assert!(run(None, &facts, &uses, Policy::default()).is_ok());
    let report = Policy { require: Require::Report, ..Policy::default() };
    let ok = run(None, &facts, &uses, report).map_err(|e| format!("{e:?}"))?;
    assert!(!ok.onboarded);
    assert_eq!(ok.warnings.iter().map(|d| d.code).collect::<Vec<_>>(), ["KCI4055"]);
    let enforce = Policy { require: Require::Enforce, ..Policy::default() };
    assert_eq!(enforce.stricter(Policy::default()).moved_tags, MovedTags::Refuse);
    assert_eq!(codes(&run(None, &facts, &uses, enforce)), ["KCI4055"]);
    assert!(run(Some(&file), &facts, &uses, enforce).is_ok());
    for server in [Require::Off, Require::Report, Require::Enforce] {
        for repo in [Require::Off, Require::Report, Require::Enforce] {
            let a = Policy { require: server, ..Policy::default() };
            let b = Policy { require: repo, ..Policy::default() };
            assert_eq!(a.stricter(b), b.stricter(a));
            assert_eq!(a.stricter(b).require, server.max(repo));
        }
    }
    Ok(())
}

#[test]
fn w29_r26_full_version_history_is_flagged_or_refused_until_reviewed() -> Result<(), String> {
    let (mut file, mut facts, uses) = fixture("v1.2.3", M, true);
    if let Some(f) = facts.get_mut("acme/action@v1.2.3") { f.previous = Some(A.into()); }
    let ok = run(None, &facts, &uses, Policy::default()).map_err(|e| format!("{e:?}"))?;
    assert_eq!(ok.moved, [MovedRef { pin: "acme/action@v1.2.3".into(), before: A.into(), now: M.into() }]);
    let refuse = Policy { moved_tags: MovedTags::Refuse, ..Policy::default() };
    assert_eq!(codes(&run(None, &facts, &uses, refuse)), ["KCI4056"]);
    let enforced = Policy { require: Require::Enforce, ..Policy::default() };
    assert_eq!(codes(&run(None, &facts, &uses, enforced)), ["KCI4055", "KCI4056"]);
    // Merely editing the lock to M does not accept the observed move.
    if let Some(a) = file.dependencies.get_mut("acme/action@v1.2.3") { a.commit = format!("sha1-{M}"); }
    assert_eq!(codes(&run(Some(&file), &facts, &uses, refuse)), ["KCI4056"]);
    let accepted = [("acme/action@v1.2.3".into(), M.into())].into();
    let ok = check_github(GithubCheck { workflow: WORKFLOW, uses: &uses, file: Some(&file), facts: &facts, home_host: "github.com", policy: refuse, accepted_moves: &accepted }).map_err(|e| format!("{e:?}"))?;
    assert_eq!(ok.pins.values().next().map(|p| p.commit.as_str()), Some(M));
    Ok(())
}

#[test]
fn w29_r23_missing_facts_and_mutated_typed_data_fail_closed() {
    let (mut file, mut facts, uses) = fixture("v1", A, true);
    assert_eq!(codes(&run(Some(&file), &BTreeMap::new(), &uses, Policy::default())), ["KCI4051"]);
    if let Some(f) = facts.get_mut("acme/action@v1") { f.current.clear(); }
    assert_eq!(codes(&run(Some(&file), &facts, &uses, Policy::default())), ["KCI4051"]);
    if let Some(a) = file.dependencies.get_mut("acme/action@v1") { a.commit = "sha1-short".into(); }
    assert_eq!(codes(&run(Some(&file), &facts, &uses, Policy::default())), ["KCI1022"]);
    file.version = "v0.0.4".into();
    assert_eq!(codes(&run(Some(&file), &facts, &uses, Policy::default())), ["KCI1022"]);
}
