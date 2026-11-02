use super::*;

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const C: &str = "cccccccccccccccccccccccccccccccccccccccc";
const GOLDEN: &str = include_str!("fixtures/github-v003.lock");

fn sample() -> String {
    format!("version: v0.0.3\nworkflows:\n  .github/workflows/ci.yml: [ACME/Build@main]\n  .github/workflows/empty.yml: []\ndependencies:\n  ACME/Build@main:\n    ref: main\n    commit: sha1-{A}\n    owner_id: 1\n    repo_id: 2\n    uses: [Zeta/Leaf@v1, acme/leaf@don't]\n  Zeta/Leaf@v1:\n    ref: v1\n    commit: sha1-{B}\n    owner_id: 3\n    repo_id: 4\n  acme/leaf@don't:\n    ref: don't\n    commit: sha1-{C}\n    owner_id: 1\n    repo_id: 5\n")
}

#[test]
fn w29_r23_repository_keys_and_subactions() {
    assert_eq!(action_pin("Actions/Cache/save@V4"), Some("actions/cache@V4".into()));
    assert_eq!(canonical_pin("OWNER/repo@release@2026"), Some("owner/repo@release@2026".into()));
    assert_eq!(canonical_pin("owner/repo/sub@v1"), None);
    for uses in ["./own", "$/own", "docker://alpine:3", "kiln://acme/kit@v1", "kiln/checkout@v1", "acme/kit/.github/workflows/ci.yml@main", "owner/../sub@v1"] {
        assert_eq!(action_pin(uses), None, "{uses}");
    }
}

#[test]
fn w29_r23_nested_composite_and_exact_writer_bytes() -> Result<(), String> {
    let file = parse(&sample()).map_err(|d| format!("{d:?}"))?;
    assert_eq!(file.write("github.com").map_err(|d| format!("{d:?}"))?, GOLDEN);
    assert_eq!(file.closure(".github/workflows/ci.yml"), Some(vec!["acme/build@main".into(), "acme/leaf@don't".into(), "zeta/leaf@v1".into()]));
    assert_eq!(file.closure(".github/workflows/empty.yml"), Some(Vec::new()));
    assert_eq!(file.closure(".github/workflows/absent.yml"), None);
    let mut flat = file.clone();
    flat.workflows.insert(".github/workflows/ci.yml".into(), vec!["zeta/leaf@v1".into(), "acme/build@main".into()]);
    assert_eq!(flat.closure(".github/workflows/ci.yml"), file.closure(".github/workflows/ci.yml"));
    assert_eq!(parse(GOLDEN).map_err(|d| format!("{d:?}"))?.write("github.com").map_err(|d| format!("{d:?}"))?, GOLDEN);
    Ok(())
}

#[test]
fn w29_r23_legacy_versions_keep_their_dotcom_identity() -> Result<(), String> {
    let old = format!("version: v0.0.1\nworkflows:\n  .github/workflows/ci.yml: [Acme/Build@v4:sha1-{A}]\ndependencies:\n  Acme/Build@v4:sha1-{A}:\n    tag: v4\n    branch: main\n    commit: sha1-{A}\n    owner_id: 1\n    repo_id: 2\n");
    let file = parse(&old).map_err(|d| format!("{d:?}"))?;
    let action = file.dependencies.get("acme/build@v4").ok_or("missing canonical legacy pin")?;
    assert_eq!((file.version.as_str(), action.git_ref.as_str(), action.hostname.as_deref()), (VERSION, "v4", Some("github.com")));
    let tenant = file.write("acme.ghe.com").map_err(|d| format!("{d:?}"))?;
    assert!(tenant.contains("hostname: 'github.com'\n"), "{tenant}");
    let fallback = old.replace("    tag: v4\n    branch: main\n", "");
    assert_eq!(parse(&fallback).map_err(|d| format!("{d:?}"))?.dependencies.get("acme/build@v4").map(|a| a.git_ref.as_str()), Some("v4"));
    let v2 = sample().replace(VERSION, "v0.0.2");
    assert!(parse(&v2).map_err(|d| format!("{d:?}"))?.dependencies.values().all(|a| a.hostname.as_deref() == Some("github.com")));
    assert!(parse(&v2.replacen("    ref: main", "    hostname: github.com\n    ref: main", 1)).is_err());
    Ok(())
}

#[test]
fn w29_r23_malformed_locks_are_positioned_errors() {
    let valid = sample();
    let cases = [
        String::new(), "[]".into(), "version: v0.0.4\n".into(), "version: v0.0.3\nunknown: true\n".into(),
        valid.replacen("    ref: main\n", "", 1), valid.replacen("    ref: main", "    ref: false", 1),
        valid.replacen("    ref: main", "    ref: other", 1), valid.replacen("    owner_id: 1", "    owner_id: 0", 1),
        valid.replacen("    repo_id: 2", "    repo_id: -1", 1), valid.replacen("    repo_id: 2", "    repo_id: 1.5", 1),
        valid.replacen(&format!("sha1-{A}"), "sha1-abc", 1),
        valid.replacen("    ref: main", "    hostname: GitHub.com\n    ref: main", 1),
        valid.replacen("    ref: main", "    hostname: evil.example\n    ref: main", 1),
        valid.replacen("    ref: main", "    hostname: ''\n    ref: main", 1),
        valid.replacen(".github/workflows/ci.yml:", "../outside:", 1),
        valid.replacen(".github/workflows/ci.yml:", "'C:\\outside':", 1),
        "version: v0.0.3\nversion: v0.0.3\n".into(), "version: v0.0.3\n---\nversion: v0.0.3\n".into(),
        "x".repeat(yaml::MAX_FILE_BYTES.saturating_add(1)),
    ];
    for source in cases {
        let result = parse(&source);
        assert!(result.is_err(), "accepted malformed lock: {source}");
        if let Err(found) = result {
            assert!(!found.is_empty());
            assert!(found.iter().all(|d| d.code == "KCI1022" && d.path == PATH));
        }
    }
    assert_eq!(parse("version: v0.0.3\nunknown: true\n").err().and_then(|d| d.first().map(|d| d.range.start.line)), Some(2));
}

#[test]
fn w29_r23_full_sha_key_cannot_hide_another_commit() {
    let source = format!("version: v0.0.3\ndependencies:\n  acme/build@{A}:\n    ref: v1\n    commit: sha1-{A}\n    owner_id: 1\n    repo_id: 2\n");
    assert!(parse(&source).is_ok());
    assert!(parse(&source.replace(&format!("commit: sha1-{A}"), &format!("commit: sha1-{B}"))).is_err());
    let sha256 = "f".repeat(64);
    assert!(parse(&source.replace(A, &sha256).replace("sha1-", "sha256-")).is_ok());
}

/// The official go/v0.0.6 parser's measured results, not YAML 1.2's scalar coercions.
#[test]
fn w29_r23_github_scalar_compatibility() -> Result<(), String> {
    for (value, expected) in [("true", "true"), ("1.0", "1.0")] {
        let source = sample().replace("@main", &format!("@{value}")).replacen("    ref: main", &format!("    ref: {value}"), 1);
        let file = parse(&source).map_err(|d| format!("{d:?}"))?;
        assert_eq!(file.dependencies.get(&format!("acme/build@{value}")).map(|a| a.git_ref.as_str()), Some(expected));
        let unicode = parse(&format!("# 🌱\n{source}")).map_err(|d| format!("{d:?}"))?;
        assert_eq!(unicode.dependencies.get(&format!("acme/build@{value}")).map(|a| a.git_ref.as_str()), Some(expected));
    }
    assert!(parse(&sample().replacen("owner_id: 1", "owner_id: '1'", 1)).is_err());
    assert!(parse(&sample().replacen("owner_id: 1", "owner_id: 0x1", 1)).is_err());
    let file = parse(&sample().replacen("owner_id: 1", "owner_id: 010", 1)).map_err(|d| format!("{d:?}"))?;
    assert_eq!(file.dependencies.get("acme/build@main").map(|a| a.owner_id), Some(8));
    let file = parse(&sample().replacen(".github/workflows/ci.yml:", "123:", 1)).map_err(|d| format!("{d:?}"))?;
    assert!(file.workflows.contains_key("123"));
    Ok(())
}

#[test]
fn w29_r23_case_aliases_agree_or_fail_without_losing_pins() {
    let source = format!("version: v0.0.3\ndependencies:\n  ACME/Build@v1: {{ ref: v1, commit: sha1-{A}, owner_id: 1, repo_id: 2 }}\n  acme/build@v1: {{ ref: v1, commit: sha1-{A}, owner_id: 1, repo_id: 2 }}\n");
    assert_eq!(parse(&source).ok().map(|f| f.dependencies.len()), Some(1));
    let conflicting = source.replacen("  acme/build@v1: { ref: v1, commit:", "  acme/build@v1: { ref: v1, extra: true, commit:", 1);
    assert!(parse(&conflicting).is_err());
    let conflicting = source.replacen(&format!("  acme/build@v1: {{ ref: v1, commit: sha1-{A}"), &format!("  acme/build@v1: {{ ref: v1, commit: sha1-{B}"), 1);
    assert!(parse(&conflicting).is_err());
}

#[test]
fn w29_r23_graphs_are_iterative_and_cycles_fail() -> Result<(), String> {
    let mut file = File::default();
    file.workflows.insert(".github/workflows/ci.yml".into(), vec!["acme/build@v0".into()]);
    for n in 0_u32..2000 {
        let uses = if n == 1999 { vec![] } else { vec![format!("acme/build@v{}", n.saturating_add(1))] };
        file.dependencies.insert(format!("acme/build@v{n}"), Action { git_ref: format!("v{n}"), commit: format!("sha1-{A}"), owner_id: 1, repo_id: 2, uses, ..Action::default() });
    }
    let source = file.write("github.com").map_err(|d| format!("{d:?}"))?;
    assert_eq!(parse(&source).map_err(|d| format!("{d:?}"))?.closure(".github/workflows/ci.yml").map(|v| v.len()), Some(2000));
    if let Some(last) = file.dependencies.get_mut("acme/build@v1999") { last.uses.push("acme/build@v0".into()); }
    assert!(file.write("github.com").is_err(), "the cycle is refused before bytes are returned");
    Ok(())
}

#[test]
fn w29_r23_missing_pins_remain_for_the_provenance_check() -> Result<(), String> {
    let source = sample().replace("Zeta/Leaf@v1,", "missing/action@v1,");
    let file = parse(&source).map_err(|d| format!("{d:?}"))?;
    assert!(file.closure(".github/workflows/ci.yml").is_some_and(|pins| pins.contains(&"missing/action@v1".into())));
    Ok(())
}

#[test]
fn w29_r23_writing_cannot_rebind_hosts_or_fold_unsafe_strings() -> Result<(), String> {
    let mut file = parse(&sample()).map_err(|d| format!("{d:?}"))?;
    if let Some(a) = file.dependencies.get_mut("acme/build@main") { a.hostname = Some("acme.ghe.com".into()); }
    assert!(file.write("github.com").is_err());
    let mut file = parse(&sample()).map_err(|d| format!("{d:?}"))?;
    file.workflows.insert("../outside".into(), vec![]);
    assert!(file.write("github.com").is_err());
    file.workflows.remove("../outside");
    file.workflows.insert(".github/workflows/evil.yml".into(), vec!["owner/repo@a\nb".into()]);
    assert!(file.write("github.com").is_err());
    Ok(())
}
