use super::*;

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const C: &str = "cccccccccccccccccccccccccccccccccccccccc";
const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const GH: &str = ".github/workflows/ci.yml";
const KL: &str = ".kiln/ci.yaml";

fn edit(path: &str, uses: &[&str]) -> Workflow { Workflow { path: path.into(), uses: Some(uses.iter().map(|s| (*s).into()).collect()) } }

fn git(key: &str, commit: &str, uses: &[&str]) -> Resolution {
    let reference = key.rsplit_once('@').map(|(_, r)| r).unwrap_or("v1");
    let native = key.starts_with("kiln://");
    Resolution { key: key.into(), dependency: Dependency::Git(Git {
        git_ref: format!("refs/tags/{reference}"), commit: format!("sha1-{commit}"), repo: if native { "kiln:0123456789abcdef".into() } else { "github:1/2".into() },
        uses: uses.iter().map(|s| (*s).into()).collect(), ..Git::default()
    }) }
}

fn image(key: &str, hash: &str) -> Resolution { Resolution { key: key.into(), dependency: Dependency::Image(kiln::Image { digest: format!("sha256:{hash}") }) } }

fn runtime(platform: &str, hash: &str) -> Resolution {
    let file = if platform == "macos/arm64" { "node-v24.1.0-darwin-arm64.tar.gz" } else { "node-v24.1.0-linux-x64.tar.gz" };
    Resolution { key: "node24".into(), dependency: Dependency::Runtime(kiln::Runtime { archives: [(platform.into(), kiln::Archive {
        version: "v24.1.0".into(), url: format!("https://nodejs.org/dist/v24.1.0/{file}"), file: file.into(), sha256: hash.into()
    })].into_iter().collect(), ..kiln::Runtime::default() }) }
}

fn generate(gh: Option<&github::File>, kl: Option<&kiln::File>, edits: &[Workflow], facts: &[Resolution], seeds: &[Seed], mode: Mode) -> Result<Files, Vec<Diag>> {
    regenerate(Input { github: gh, kiln: kl, workflows: edits, resolutions: facts, seeds, home_host: "github.com", mode })
}

fn parsed(files: &Files) -> Result<(github::File, kiln::File), String> {
    Ok((files.github.as_deref().map(github::parse).transpose().map_err(|d| format!("{d:?}"))?.unwrap_or_default(), files.kiln.as_deref().map(kiln::parse).transpose().map_err(|d| format!("{d:?}"))?.unwrap_or_default()))
}

#[test]
fn w29_r25_github_only_generation_matches_the_cli_and_omits_an_unneeded_kiln_file() -> Result<(), String> {
    let files = generate(None, None, &[edit(GH, &["ACME/Build@v1"])], &[git("acme/build@v1", A, &["acme/leaf@v2"]), git("acme/leaf@v2", B, &[])], &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?;
    let header = github::HEADER;
    let expected = format!("{header}version: 'v0.0.3'\nworkflows:\n    '{GH}':\n        - 'acme/build@v1'\ndependencies:\n    'acme/build@v1':\n        ref: 'v1'\n        commit: 'sha1-{A}'\n        owner_id: 1\n        repo_id: 2\n        uses:\n            - 'acme/leaf@v2'\n    'acme/leaf@v2':\n        ref: 'v2'\n        commit: 'sha1-{B}'\n        owner_id: 1\n        repo_id: 2\n");
    assert_eq!(files.github, Some(expected));
    assert_eq!(files.kiln, None);
    Ok(())
}

#[test]
fn w29_r24_github_supplement_keeps_runtimes_images_and_hosted_sources_without_hosted_ids_in_github() -> Result<(), String> {
    let facts = vec![git("acme/build@v1", A, &["node24", "docker://alpine:3"]), runtime("linux/amd64", HASH), runtime("macos/arm64", HASH), image("docker://alpine:3", HASH), git("kiln://acme/kit/setup@v2", B, &[])];
    let edits = [edit(GH, &["acme/build@v1", "kiln://acme/kit/setup@v2"]), edit(KL, &["acme/build@v1"])];
    let files = generate(None, None, &edits, &facts, &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?;
    let (gh, kl) = parsed(&files)?;
    assert_eq!(gh.workflows.get(GH), Some(&vec!["acme/build@v1".into()]));
    assert!(gh.dependencies.keys().all(|k| !k.starts_with("kiln://")));
    assert_eq!(kl.workflows.get(GH), Some(&vec!["acme/build@v1".into(), "kiln://acme/kit/setup@v2".into()]));
    assert_eq!(kl.closure(KL).map(|v| v.len()), Some(3));
    assert!(matches!(kl.dependencies.get("node24"), Some(Dependency::Runtime(r)) if r.archives.len() == 2));
    assert!(compare(&gh, &kl, "github.com").is_ok());
    Ok(())
}

#[test]
fn w29_r24_reusable_workflow_dependencies_stay_in_the_supplement() -> Result<(), String> {
    let call = "acme/shared/.github/workflows/release.yml@v3";
    let nested = "kiln://acme/shared/.kiln/build.yaml@v3";
    let external_kiln = "acme/external/.kiln/release.yaml@v2";
    let facts = [git(call, A, &[nested, "acme/build@v1"]), git(nested, B, &["executor://ubuntu:24.04", external_kiln]), git(external_kiln, A, &[]), git("acme/build@v1", C, &[]), image("executor://ubuntu:24.04", HASH)];
    let files = generate(None, None, &[edit(GH, &[call])], &facts, &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?;
    let (gh, kl) = parsed(&files)?;
    assert_eq!(gh.workflows.get(GH), Some(&vec![]));
    assert!(gh.dependencies.is_empty());
    assert_eq!(kl.closure(GH).map(|v| v.len()), Some(5));
    assert!(kl.dependencies.contains_key(call));
    Ok(())
}

#[test]
fn w29_r25_ordinary_regeneration_never_borrows_a_moved_refs_commit_or_edges() -> Result<(), String> {
    let facts = [git("acme/build@v1", A, &["acme/leaf@v2", "node24"]), git("acme/leaf@v2", B, &[]), runtime("linux/amd64", HASH)];
    let initial = generate(None, None, &[edit(GH, &["acme/build@v1"])], &facts, &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?;
    let (gh, kl) = parsed(&initial)?;
    let moved = [git("acme/build@v1", C, &["attacker/new@v1"]), git("attacker/new@v1", C, &[])];
    let regenerated = generate(Some(&gh), Some(&kl), &[edit(GH, &["acme/build@v1"])], &moved, &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?;
    assert_eq!(regenerated, initial);
    assert!(!format!("{regenerated:?}").contains("attacker"));
    let proposed = generate(Some(&gh), Some(&kl), &[edit(GH, &["acme/build@v1"])], &moved, &[], Mode::Propose).map_err(|d| format!("{d:?}"))?;
    let (proposed, _) = parsed(&proposed)?;
    assert_eq!(proposed.dependencies.get("acme/build@v1").map(|a| a.commit.as_str()), Some(format!("sha1-{C}").as_str()));
    Ok(())
}

#[test]
fn w29_r25_untouched_workflows_and_shared_pins_survive_pruning_and_proposals() -> Result<(), String> {
    let initial = generate(None, None, &[edit(GH, &["acme/build@v1"]), edit(KL, &["acme/build@v1", "image://alpine:3"])], &[git("acme/build@v1", A, &[]), image("image://alpine:3", HASH)], &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?;
    let (mut gh, kl) = parsed(&initial)?;
    gh.dependencies.insert("orphan/old@v1".into(), github::Action { git_ref: "v1".into(), commit: format!("sha1-{B}"), owner_id: 3, repo_id: 4, ..github::Action::default() });
    let output = generate(Some(&gh), Some(&kl), &[edit(GH, &["acme/build@v1"])], &[git("acme/build@v1", B, &[])], &[], Mode::Propose).map_err(|d| format!("{d:?}"))?;
    let (proposed_gh, untouched_kl) = parsed(&output)?;
    assert_eq!(untouched_kl, kl);
    assert_eq!(proposed_gh.dependencies.get("acme/build@v1").map(|a| a.commit.as_str()), Some(format!("sha1-{B}").as_str()));
    let output = generate(Some(&gh), Some(&kl), &[Workflow { path: GH.into(), uses: None }], &[], &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?;
    let (new_gh, new_kl) = parsed(&output)?;
    assert!(new_gh.workflows.is_empty());
    assert_eq!(new_kl, kl);
    let output = generate(Some(&gh), Some(&kl), &[Workflow { path: GH.into(), uses: None }, Workflow { path: KL.into(), uses: None }], &[], &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?;
    assert_eq!(output, Files { github: None, kiln: None });
    Ok(())
}

#[test]
fn w29_r24_caller_overrides_callee_seeds_without_borrowing_their_metadata() -> Result<(), String> {
    let initial = generate(None, None, &[edit(KL, &["acme/build@v1"])], &[git("acme/build@v1", A, &[])], &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?;
    let (_, kl) = parsed(&initial)?;
    let seeds = [Seed { source: "callee-a@locked-A".into(), pins: vec![git("acme/build@v1", B, &["missing/other@v1"])] }, Seed { source: "callee-b@locked-B".into(), pins: vec![git("acme/build@v1", C, &[])] }];
    assert_eq!(generate(None, Some(&kl), &[edit(KL, &["acme/build@v1"])], &[], &seeds, Mode::Preserve).map_err(|d| format!("{d:?}"))?, initial);
    let refused = generate(None, None, &[edit(KL, &["acme/build@v1"])], &[], &seeds, Mode::Preserve).err().ok_or("competing seeds accepted without a caller override")?;
    assert!(refused.iter().any(|d| d.code == "KCI4054" && d.message.contains("callee-a") && d.message.contains("callee-b")));
    Ok(())
}

#[test]
fn w29_r24_overlapping_files_show_both_conflicting_entries() -> Result<(), String> {
    let first = generate(None, None, &[edit(GH, &["acme/build@v1"]), edit(KL, &["acme/build@v1"])], &[git("acme/build@v1", A, &[])], &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?;
    let (gh, mut kl) = parsed(&first)?;
    kl.workflows.insert(GH.into(), vec!["acme/build/sub@v1".into()]);
    kl.dependencies.insert("acme/build/sub@v1".into(), git("acme/build/sub@v1", B, &[]).dependency);
    let refused = compare(&gh, &kl, "github.com").err().ok_or("conflicting locks accepted")?;
    assert!(refused.iter().any(|d| d.code == "KCI4054" && d.path == GH && d.message.contains(github::PATH) && d.message.contains(kiln::PATH) && d.message.contains(A) && d.message.contains(B)));
    let refused = generate(Some(&gh), Some(&kl), &[], &[], &[], Mode::Preserve).err().ok_or("regeneration hid a conflict")?;
    assert!(refused.iter().any(|d| d.code == "KCI4054"));
    kl.workflows.insert(GH.into(), vec!["acme/build@refs/tags/v1".into()]);
    let mut qualified = git("acme/build@v1", B, &[]).dependency;
    if let Dependency::Git(g) = &mut qualified { g.git_ref = "refs/tags/v1".into(); }
    kl.dependencies.insert("acme/build@refs/tags/v1".into(), qualified);
    assert!(compare(&gh, &kl, "github.com").err().is_some_and(|d| d.iter().any(|d| d.code == "KCI4054")), "qualified refs cannot hide the conflict");
    Ok(())
}

#[test]
fn w29_r25_refusals_are_atomic_for_missing_facts_expressions_cycles_and_changed_identities() -> Result<(), String> {
    for edits in [vec![edit(GH, &["missing/build@v1"])], vec![edit(KL, &["acme/build@${{env.TAG}}"])] ] {
        let errors = generate(None, None, &edits, &[], &[], Mode::Preserve).err().ok_or("missing resolution produced lockfiles")?;
        assert!(errors.iter().any(|d| d.code == "KCI4050"));
    }
    let cycle = [git("acme/build@v1", A, &["acme/leaf@v2"]), git("acme/leaf@v2", B, &["acme/build@v1"])];
    assert!(generate(None, None, &[edit(GH, &["acme/build@v1"])], &cycle, &[], Mode::Preserve).err().is_some_and(|d| d.iter().any(|d| d.code == "KCI1022")));
    let initial = generate(None, None, &[edit(GH, &["acme/build@v1"])], &[git("acme/build@v1", A, &[])], &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?;
    let (gh, _) = parsed(&initial)?;
    let mut changed = git("acme/build@v1", B, &[]);
    if let Dependency::Git(g) = &mut changed.dependency { g.repo = "github:3/4".into(); }
    for mode in [Mode::Preserve, Mode::Propose] {
        assert!(generate(Some(&gh), None, &[edit(GH, &["acme/build@v1"])], &[changed.clone()], &[], mode).err().is_some_and(|d| d.iter().any(|d| d.code == "KCI4052")));
    }
    Ok(())
}

#[test]
fn w29_r24_new_platforms_do_not_replace_an_accepted_runtime_archive() -> Result<(), String> {
    let initial = generate(None, None, &[edit(KL, &["node24"])], &[runtime("linux/amd64", HASH)], &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?;
    let (_, kl) = parsed(&initial)?;
    let facts = [runtime("linux/amd64", &"b".repeat(64)), runtime("macos/arm64", HASH)];
    let output = generate(None, Some(&kl), &[edit(KL, &["node24"])], &facts, &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?;
    let (_, kl) = parsed(&output)?;
    assert!(matches!(kl.dependencies.get("node24"), Some(Dependency::Runtime(r)) if r.archives.len() == 2 && r.archives.get("linux/amd64").is_some_and(|a| a.sha256 == HASH)));
    Ok(())
}

#[test]
fn w29_r25_permutations_and_repeated_generation_produce_identical_files() -> Result<(), String> {
    let facts = vec![git("acme/build@v1", A, &["node24", "image://alpine:3"]), git("kiln://acme/kit/setup@v2", B, &[]), image("image://alpine:3", HASH), runtime("linux/amd64", HASH), runtime("macos/arm64", HASH)];
    let edits = vec![edit(GH, &["acme/build@v1", "kiln://acme/kit/setup@v2"]), edit(KL, &["image://alpine:3", "acme/build@v1"])];
    let expected = generate(None, None, &edits, &facts, &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?;
    for shift in 0..facts.len() {
        let mut facts = facts.clone();
        facts.rotate_left(shift);
        for reverse in [false, true] {
            let mut edits = edits.clone();
            if reverse { facts.reverse(); edits.reverse(); for e in &mut edits { if let Some(uses) = &mut e.uses { uses.reverse(); } } }
            assert_eq!(generate(None, None, &edits, &facts, &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?, expected);
        }
    }
    let (gh, kl) = parsed(&expected)?;
    assert_eq!(generate(Some(&gh), Some(&kl), &edits, &facts, &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?, expected);
    Ok(())
}

#[test]
fn w29_r25_builtin_and_local_steps_keep_an_empty_onboarded_workflow() -> Result<(), String> {
    let files = generate(None, None, &[edit(GH, &["./actions/setup", "$/actions/other", "kiln/checkout@v1"])], &[], &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?;
    let (gh, kl) = parsed(&files)?;
    assert_eq!(gh.workflows.get(GH), Some(&vec![]));
    assert!(kl.workflows.is_empty());
    assert!(files.github.is_some());
    assert!(files.kiln.is_none());
    Ok(())
}

#[test]
fn w29_r24_unrelated_workflows_keep_their_own_file_pins() -> Result<(), String> {
    let gh_only = generate(None, None, &[edit(GH, &["acme/build@v1"])], &[git("acme/build@v1", A, &[])], &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?;
    let kl_only = generate(None, None, &[edit(KL, &["acme/build@v1"])], &[git("acme/build@v1", B, &[])], &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?;
    let (gh, _) = parsed(&gh_only)?;
    let (_, kl) = parsed(&kl_only)?;
    assert_eq!(generate(Some(&gh), Some(&kl), &[], &[], &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?, Files { github: gh_only.github, kiln: kl_only.kiln });
    Ok(())
}

#[test]
fn w29_r25_one_file_cannot_change_an_untouched_workflows_shared_pin() -> Result<(), String> {
    let other = ".github/workflows/other.yml";
    let initial = generate(None, None, &[edit(GH, &["acme/build@v1"]), edit(other, &["acme/build@v1"])], &[git("acme/build@v1", A, &[])], &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?;
    let (gh, _) = parsed(&initial)?;
    let refused = generate(Some(&gh), None, &[edit(GH, &["acme/build@v1"])], &[git("acme/build@v1", B, &[])], &[], Mode::Propose).err().ok_or("a shared pin changed for an untouched workflow")?;
    assert!(refused.iter().any(|d| d.code == "KCI4054" && d.message.contains(GH) && d.message.contains(other)));
    Ok(())
}

#[test]
fn w29_r24_subactions_share_a_github_tree_and_a_full_sha_can_reuse_an_accepted_pin() -> Result<(), String> {
    let initial = generate(None, None, &[edit(GH, &["acme/build/sub@v1", "acme/build/other@v1"])], &[git("acme/build/sub@v1", A, &[]), git("acme/build/other@v1", A, &[])], &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?;
    let (gh, _) = parsed(&initial)?;
    assert_eq!(gh.workflows.get(GH), Some(&vec!["acme/build@v1".into()]));
    assert_eq!(gh.dependencies.len(), 1);
    let sha_use = format!("acme/build@{A}");
    let output = generate(Some(&gh), None, &[edit(GH, &[&sha_use])], &[], &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?;
    let (gh, _) = parsed(&output)?;
    assert_eq!(gh.dependencies.get(&sha_use).map(|a| a.commit.as_str()), Some(format!("sha1-{A}").as_str()));
    let refused = generate(None, None, &[edit(GH, &["acme/build/sub@v1", "acme/build/other@v1"])], &[git("acme/build/sub@v1", A, &[]), git("acme/build/other@v1", B, &[])], &[], Mode::Preserve).err().ok_or("two subactions selected different trees for one GitHub pin")?;
    assert!(refused.iter().any(|d| d.code == "KCI4054"));
    Ok(())
}

#[test]
fn w29_r25_conflicting_facts_fail_independent_of_order_and_large_graphs_are_refused() -> Result<(), String> {
    let facts = [git("acme/build@v1", A, &[]), git("acme/build@v1", B, &[])];
    let first = generate(None, None, &[edit(GH, &["acme/build@v1"])], &facts, &[], Mode::Preserve).err().ok_or("conflicting facts were accepted")?;
    let second = generate(None, None, &[edit(GH, &["acme/build@v1"])], &[facts.get(1).ok_or("missing fact")?.clone(), facts.first().ok_or("missing fact")?.clone()], &[], Mode::Preserve).err().ok_or("reversed conflicting facts were accepted")?;
    assert_eq!(first, second);
    assert!(first.iter().all(|d| d.code == "KCI4054"));
    let mut large = git("acme/build@v1", A, &[]);
    if let Dependency::Git(g) = &mut large.dependency { g.uses = vec!["acme/leaf@v1".into(); kiln::MAX_ITEMS]; }
    assert!(generate(None, None, &[edit(KL, &["acme/build@v1"])], &[large], &[], Mode::Preserve).err().is_some_and(|d| d.iter().any(|d| d.code == "KCI1022")));
    Ok(())
}

#[test]
fn w29_r24_head_keeps_the_recorded_branch_beside_githubs_head_spelling() -> Result<(), String> {
    let key = "acme/build@HEAD";
    let mut root = git(key, A, &["node24"]);
    if let Dependency::Git(g) = &mut root.dependency { g.git_ref = "refs/heads/main".into(); }
    let facts = [root, runtime("linux/amd64", HASH)];
    let files = generate(None, None, &[edit(GH, &[key])], &facts, &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?;
    let (gh, kl) = parsed(&files)?;
    assert_eq!(gh.dependencies.get(key).map(|a| a.git_ref.as_str()), Some("HEAD"));
    assert!(matches!(kl.dependencies.get(key), Some(Dependency::Git(g)) if g.git_ref == "refs/heads/main"));
    assert_eq!(generate(Some(&gh), Some(&kl), &[edit(GH, &[key])], &facts, &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?, files);
    Ok(())
}

#[test]
fn w29_r24_a_legacy_runtime_checksum_can_gain_only_an_identical_archives_location() -> Result<(), String> {
    let accepted = kiln::File {
        workflows: [(KL.into(), vec!["node24".into()])].into(),
        dependencies: [("node24".into(), Dependency::Runtime(kiln::Runtime { sha256: Some(HASH.into()), archives: BTreeMap::new() }))].into(),
        ..Default::default()
    };
    let good = [runtime("linux/amd64", HASH)];
    let files = generate(None, Some(&accepted), &[edit(KL, &["node24"])], &good, &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?;
    let (_, promoted) = parsed(&files)?;
    assert!(matches!(promoted.dependencies.get("node24"), Some(Dependency::Runtime(r)) if r.sha256.is_none() && r.archives.get("linux/amd64").is_some_and(|a| a.sha256 == HASH)));
    let other = [runtime("linux/amd64", &"b".repeat(64))];
    let files = generate(None, Some(&accepted), &[edit(KL, &["node24"])], &other, &[], Mode::Preserve).map_err(|d| format!("{d:?}"))?;
    let (_, preserved) = parsed(&files)?;
    assert_eq!(preserved, accepted, "another archive cannot acquire the accepted checksum");
    Ok(())
}
