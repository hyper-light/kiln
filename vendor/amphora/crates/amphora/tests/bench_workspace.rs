//! The bench (`bench/`) is a workspace of its own, so the root's `[patch.crates-io]`, which points the hyper-raft crates
//! Amphora depends on at `vendor/hyper-raft`, does not reach it: without the same patches cargo looks for them on
//! crates.io and the bench does not build (bug c7af55d0, `amphora-bench` unbuildable at 29be700). AC-T1.2.1: the bench
//! suite runs against the product.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

/// The crates a manifest's `[patch.crates-io]` names, each with the path it gives, as seen from the repository's root
/// (`base` is the manifest's directory from there).
fn patches(manifest: &str, base: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut inside = false;
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            inside = line == "[patch.crates-io]";
            continue;
        }
        if !inside || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, rest) = line.split_once('=').unwrap();
        let path = rest
            .split("path = \"")
            .nth(1)
            .and_then(|path| path.split('"').next())
            .unwrap_or_else(|| panic!("{line}: a patch without a path"));
        found.push((name.trim().to_owned(), normalize(&format!("{base}{path}"))));
    }
    found.sort();
    found
}

/// `path` with each `dir/..` pair taken out.
fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            ".." if parts.last().is_some_and(|last| *last != "..") => {
                parts.pop();
            }
            "." | "" => {}
            part => parts.push(part),
        }
    }
    parts.join("/")
}

/// Do: read the root's and the bench's `[patch.crates-io]`. Expect: the bench patches every crate the root patches, to
/// the same vendored copy.
#[test]
fn the_bench_workspace_patches_every_crate_the_root_patches() {
    let root = patches(include_str!("../../../Cargo.toml"), "");
    let bench = patches(include_str!("../../../bench/Cargo.toml"), "bench/");
    assert!(!root.is_empty(), "the root patches nothing");
    let missing: Vec<_> = root.iter().filter(|patch| !bench.contains(patch)).collect();
    assert!(
        missing.is_empty(),
        "bench/Cargo.toml lacks the root's patches {missing:?}; it has {bench:?}"
    );
}

/// AC-T1.2.1. Do: count the `[patch.crates-io]` tables in the bench's manifest. Expect: exactly one. Cargo refuses a
/// manifest that names a table twice (a duplicate key), and the bench then doesn't build; merge 2472173 left two.
#[test]
fn the_bench_manifest_names_its_patches_once() {
    let tables = include_str!("../../../bench/Cargo.toml")
        .lines()
        .filter(|line| line.trim() == "[patch.crates-io]")
        .count();
    assert_eq!(
        tables, 1,
        "bench/Cargo.toml has {tables} [patch.crates-io] tables"
    );
}
