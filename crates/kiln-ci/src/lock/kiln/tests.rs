use super::*;

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn git() -> Git { Git { git_ref: "refs/tags/v3".into(), commit: format!("sha1-{A}"), repo: "github:1/2".into(), ..Git::default() } }

fn archive(platform: &str) -> Archive {
    let suffix = match platform { "linux/amd64" => "linux-x64", "linux/amd64/musl" => "alpine-x64", "macos/arm64" => "darwin-arm64", _ => "linux-arm64" };
    let file = format!("node-v24.1.0-{suffix}.tar.gz");
    Archive { version: "v24.1.0".into(), url: format!("https://nodejs.org/dist/v24.1.0/{file}"), file, sha256: HASH.into() }
}

fn sample() -> File {
    let mut file = File::default();
    let root = "acme/shared/.github/workflows/release.yml@v3";
    file.workflows.insert(".kiln/ci.yaml".into(), vec![root.into(), "docker://ghcr.io/acme/builder:1.4".into()]);
    file.workflows.insert(".kiln/empty.yaml".into(), vec![]);
    let mut source = git();
    source.uses = vec!["kiln://acme/kit/actions/setup@v3".into(), "node24".into()];
    file.dependencies.insert(root.into(), Dependency::Git(source));
    file.dependencies.insert("kiln://acme/kit/actions/setup@v3".into(), Dependency::Git(Git { repo: "kiln:01a19bb96db2714ea9fbc17b7d56946f".into(), ..git() }));
    for key in ["docker://ghcr.io/acme/builder:1.4", "image://alpine:3", "microvm://ghcr.io/acme/vm:v1", "executor://ubuntu:24.04"] {
        file.dependencies.insert(key.into(), Dependency::Image(Image { digest: format!("sha256:{HASH}") }));
    }
    file.dependencies.insert("node24".into(), Dependency::Runtime(Runtime { archives: ["linux/amd64", "linux/amd64/musl", "macos/arm64"].into_iter().map(|p| (p.into(), archive(p))).collect(), ..Runtime::default() }));
    file.dependencies.insert("node20".into(), Dependency::Runtime(Runtime { sha256: Some(HASH.into()), ..Runtime::default() }));
    file
}

#[test]
fn w29_r24_all_dependency_kinds_roundtrip_and_have_stable_bytes() -> Result<(), String> {
    let file = sample();
    let text = file.write().map_err(|d| format!("{d:?}"))?;
    assert!(text.starts_with(&format!("{HEADER}version: 1\nworkflows:\n  '.kiln/ci.yaml':\n")));
    assert!(text.contains("  '.kiln/empty.yaml': []\n"));
    assert!(text.contains("      'linux/amd64/musl':\n        version: 'v24.1.0'\n"));
    let parsed = parse(&text).map_err(|d| format!("{d:?}"))?;
    assert_eq!(parsed, file);
    assert_eq!(parsed.write().map_err(|d| format!("{d:?}"))?, text);
    assert_eq!(file.closure(".kiln/ci.yaml").map(|v| v.len()), Some(4));
    assert_eq!(file.closure(".kiln/empty.yaml"), Some(vec![]));
    assert_eq!(file.closure(".kiln/missing.yaml"), None);
    Ok(())
}

#[test]
fn w29_r24_minimal_format_is_exact_and_sorts_edges() -> Result<(), String> {
    let mut file = File::default();
    file.workflows.insert(".kiln/ci.yaml".into(), vec!["acme/kit@v3".into(), "acme/kit@v3".into()]);
    file.dependencies.insert("acme/kit@v3".into(), Dependency::Git(git()));
    let golden = format!("{HEADER}version: 1\nworkflows:\n  '.kiln/ci.yaml':\n    - 'acme/kit@v3'\ndependencies:\n  'acme/kit@v3':\n    ref: 'refs/tags/v3'\n    commit: 'sha1-{A}'\n    repo: 'github:1/2'\n");
    assert_eq!(file.write().map_err(|d| format!("{d:?}"))?, golden);
    assert_eq!(parse(&golden.replace("acme/kit@v3", "ACME/Kit@v3")).map_err(|d| format!("{d:?}"))?.write().map_err(|d| format!("{d:?}"))?, golden);
    Ok(())
}

#[test]
fn w29_r24_malformed_input_is_bounded_positioned_and_refuses_unknown_fields() -> Result<(), String> {
    let text = sample().write().map_err(|d| format!("{d:?}"))?;
    let invalid = [String::new(), "[]".into(), "version: 2\n".into(), "version: '1'\n".into(), "version: 1\nunknown: true\n".into(),
        "version: 1\nversion: 1\n".into(), "version: 1\n---\nversion: 1\n".into(), text.replace("ref: 'refs/tags/v3'", "ref: 'refs/tags/v4'"),
        text.replace("repo: 'github:1/2'", "repo: 'github:0/2'"), text.replace("repo: 'github:1/2'", "repo: 'github:one/2'"),
        text.replace("repo: 'github:1/2'", "hostname: evil.example\n    repo: 'github:1/2'"), text.replace("sha1-", "sha2-"),
        text.replace("'.kiln/ci.yaml'", "'../ci.yaml'"), text.replace("digest: 'sha256:", "digest: 'sha512:"),
        text.replace("ref: 'refs/tags/v3'", "ref: 3"), text.replace("refs/tags/v3", "refs/tags/../v3"),
        text.replace("file: 'node-v24.1.0-linux-x64.tar.gz'", "file: '../node-v24.1.0-linux-x64.tar.gz'"),
        text.replace("'linux/amd64/musl'", "'macos/amd64/musl'"), text.replace("https://nodejs.org", "https://token@nodejs.org"),
        text.replace("/dist/v24.1.0/", "/dist/latest-v24.x/"), text.replace("version: 'v24.1.0'", "version: 'v20.1.0'"),
        text.replace("'node24'", "'node25'"), "x".repeat(yaml::MAX_FILE_BYTES.saturating_add(1))];
    for source in invalid {
        let found = parse(&source).err().ok_or_else(|| format!("accepted malformed input: {source}"))?;
        assert!(!found.is_empty());
        assert!(found.iter().all(|d| d.code == "KCI1022" && d.path == PATH && d.range.start.line > 0), "{found:?}");
    }
    assert_eq!(parse("version: 1\nunknown: true\n").err().and_then(|d| d.first().map(|d| d.range.start.line)), Some(2));
    Ok(())
}

#[test]
fn w29_r24_full_commit_keys_and_source_identities_cannot_disagree() {
    let mut source = git();
    assert!(Dependency::Git(source.clone()).validate(&format!("acme/kit@{A}"), "github.com").is_ok());
    source.commit = format!("sha1-{}", "b".repeat(40));
    assert!(Dependency::Git(source.clone()).validate(&format!("acme/kit@{A}"), "github.com").is_err());
    assert!(Dependency::Git(source).validate("kiln://acme/kit@v3", "github.com").is_err());
    for key in ["acme/../kit@v3", "acme/kit@../v3", "acme/kit@${{env.TAG}}", "acme/kit@-v3", "acme/kit@refs/heads/.hidden"] { assert_eq!(canonical_key(key), None); }
    assert_eq!(canonical_key("ACME/Kit@release@2026"), Some("acme/kit@release@2026".into()));
    let image = Dependency::Image(Image { digest: format!("sha256:{HASH}") });
    for key in ["docker://", "docker://../repo:v1", "docker://x@sha256:bad", "acme/kit@v3"] { assert!(image.validate(key, "github.com").is_err(), "{key}"); }
}

#[test]
fn w29_r24_unknown_pins_remain_visible_and_cycles_are_iterative() -> Result<(), String> {
    let mut file = File::default();
    file.workflows.insert(".kiln/ci.yaml".into(), vec!["acme/kit@v3".into()]);
    let mut source = git();
    source.uses.push("missing/kit@v1".into());
    file.dependencies.insert("acme/kit@v3".into(), Dependency::Git(source));
    assert_eq!(parse(&file.write().map_err(|d| format!("{d:?}"))?).map_err(|d| format!("{d:?}"))?.closure(".kiln/ci.yaml"), Some(vec!["acme/kit@v3".into(), "missing/kit@v1".into()]));
    file.dependencies.insert("missing/kit@v1".into(), Dependency::Git(Git { git_ref: "refs/tags/v1".into(), uses: vec!["acme/kit@v3".into()], ..git() }));
    assert!(file.write().is_err());
    let mut large = File::default();
    large.workflows.insert(".kiln/ci.yaml".into(), vec!["acme/kit@v0".into()]);
    for n in 0..1500_u32 {
        large.dependencies.insert(format!("acme/kit@v{n}"), Dependency::Git(Git { git_ref: format!("refs/tags/v{n}"), uses: if n == 1499 { vec![] } else { vec![format!("acme/kit@v{}", n.saturating_add(1))] }, ..git() }));
    }
    assert_eq!(parse(&large.write().map_err(|d| format!("{d:?}"))?).map_err(|d| format!("{d:?}"))?.closure(".kiln/ci.yaml").map(|v| v.len()), Some(1500));
    large.workflows.insert(".kiln/many.yaml".into(), vec!["acme/kit@v0".into(); MAX_ITEMS]);
    assert!(large.write().is_err());
    Ok(())
}
