//! No production wait is seeded from a record (decision 9 of docs/design/runtime-architecture.md; AC-T7.1.1,
//! AC-T7.1.3). The one seeding constructor, `Patience::seeded`, compiles only under `cfg(test)` or hex-api's
//! `test-seeds` feature, which hex-core's feature of the name turns on (for `Seeds::seeded`), and the manifests that
//! may turn them on are hex-testkit's, which every other crate may take only as a dev-dependency, and hex-core's own
//! dev-dependency on hex-api, for hex-core's tests. Cargo's feature resolver 2, which resolver 3 keeps, unifies a dev-dependency's
//! features into a build only where that build compiles the dev-dependencies, its tests, examples and benches (the
//! Cargo Book, "Features", "Feature resolver version 2"), so no production build has it. This reads every manifest in
//! the repository, the workspace's and the bench probes' outside it, and names each other use, by file and line.
//! Cargo reaches a package under another key only through its `package` field (the Cargo Book, "Specifying
//! Dependencies", "Renaming dependencies in Cargo.toml"), so a rename of hex-testkit is refused wherever it is, and
//! its own key is the one name to hold.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::path::{Path, PathBuf};

/// The repository's root: hex-core is its `crates/hex-core`.
fn root() -> PathBuf {
    std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap()
}

/// Every `Cargo.toml` under `root` but build output's, vendored projects' and hidden directories' (git's, and other
/// worktrees' under `.claude`), walked with a stack of directories (CLAUDE.md §11).
fn manifests(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if entry.file_type().unwrap().is_dir() {
                if !(name.starts_with('.') || name == "target" || name == "vendor") {
                    dirs.push(entry.path());
                }
            } else if name == "Cargo.toml" {
                found.push(entry.path());
            }
        }
    }
    found.sort();
    found
}

/// A manifest line without its comment: the text before the first `#` outside a string.
fn code(line: &str) -> &str {
    let mut quoted = false;
    for (at, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '#' if !quoted => return line.get(..at).unwrap_or(line),
            _ => {}
        }
    }
    line
}

/// Do: read every manifest in the repository. Expect: `test-seeds` defined in hex-api's features and in hex-core's (which
/// turns hex-api's on), and turned on in hex-testkit's dependencies (hex-core's) and hex-core's dev-dependencies
/// (hex-api's), nowhere else; hex-testkit taken by every other crate as a dev-dependency alone, its workspace entry the
/// definition, and never renamed; and, so the read is no vacuous pass, each definition and each use found once and at
/// least one crate taking hex-testkit.
#[test]
fn only_a_dev_dependency_turns_the_test_seeds_on() {
    let root = root();
    let (mut wrong, mut defined, mut enabled, mut dev_uses) = (Vec::new(), 0, 0, 0);
    for manifest in manifests(&root) {
        let text = std::fs::read_to_string(&manifest).unwrap();
        let file = manifest.strip_prefix(&root).unwrap();
        let dir = file.parent().unwrap();
        let mut section = String::new();
        for (n, line) in text.lines().enumerate() {
            let line = code(line).trim();
            let at = |section: &str| format!("{}:{} {section} {line}", file.display(), n + 1);
            if line.starts_with('[') {
                section = line.to_owned();
                if section.contains("hex-testkit")
                    && !section.contains("dev-dependencies")
                    && section != "[workspace.dependencies.hex-testkit]"
                {
                    wrong.push(at(&section));
                }
                continue;
            }
            let spaceless = line
                .split_whitespace()
                .collect::<String>()
                .replace('\'', "\"");
            if spaceless.contains("package=\"hex-testkit\"") {
                wrong.push(at(&section));
            }
            let key = line.split(['=', '.', ' ']).next().unwrap_or_default();
            if key == "hex-testkit" {
                if section.ends_with("dev-dependencies]") {
                    dev_uses += 1;
                } else if section != "[workspace.dependencies]" {
                    wrong.push(at(&section));
                }
            }
            if line.contains("test-seeds") {
                let defines = section == "[features]"
                    && key == "test-seeds"
                    && (dir == Path::new("crates/hex-api") || dir == Path::new("crates/hex-core"));
                let enables = (dir == Path::new("crates/hex-testkit")
                    && section == "[dependencies]"
                    && key == "hex-core")
                    || (dir == Path::new("crates/hex-core")
                        && section == "[dev-dependencies]"
                        && key == "hex-api");
                if defines {
                    defined += 1;
                } else if enables {
                    enabled += 1;
                } else {
                    wrong.push(at(&section));
                }
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "test-seeds or hex-testkit outside a dev-dependency: {wrong:#?}"
    );
    assert_eq!(
        (defined, enabled),
        (2, 2),
        "hex-api's and hex-core's features, and hex-testkit's and hex-core's dev uses, each found once"
    );
    assert!(
        dev_uses > 0,
        "no crate takes hex-testkit as a dev-dependency: the read found nothing to hold"
    );
}
