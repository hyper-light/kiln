//! Test waits take their own record (CLAUDE.md §4 and §6; decision 9; the lead's rule of 2026-11-06 for test
//! patiences, and of 2026-11-08: every wait in the tests uses its own record's patience). A test's wait for a fact is
//! bounded by twice the slowest answer recorded for that fact, so a slow but healthy fact passes and only a stuck one
//! fails. The suites' program-end record, `hex_api::conformance::PATIENCE`, is twice the slowest end of a program on
//! its host class's record, from its start to its reported end (`HostClass::slowest_recorded_end`: on Linux the
//! first-run record, above the waits census's `lifecycle.exec` row). Bounding any other fact with it, a box's
//! emptying, a pool's refill, a turn's hold, a node's start, takes another fact's record, which under load fails a
//! test whose fact was within its own. So the program-end record may bound only a program's end: a `Proc`'s
//! `wait_for` and the cases' `exit_of`. This reads every test source in the workspace and names each other use, by
//! file and line.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::path::{Path, PathBuf};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The file the program-end record is defined in, whose cases may use it as the record of a program's end.
const CONFORMANCE: &str = "crates/hex-api/src/conformance.rs";

/// Every `.rs` file under the workspace's crates, walked with an explicit stack (CLAUDE.md §11), its path relative to
/// the repository as the repository names it: its components joined by `/` on every OS, where Windows walks `\`.
fn sources() -> Vec<String> {
    let root = repo();
    let mut out = Vec::new();
    let mut walk = vec![root.join("crates")];
    while let Some(dir) = walk.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let names: Vec<String> = path
                    .strip_prefix(&root)
                    .unwrap()
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect();
                out.push(names.join("/"));
            }
        }
    }
    out.sort();
    out
}

/// Whether the file `file`, whose text is `text`, brings the program-end record in by name, so a bare `PATIENCE` or
/// `PROGRAM_END` in it is the record: the file that defines it, one that imports it, or one that aliases it.
fn brings_it(text: &str, file: &str) -> bool {
    file == CONFORMANCE
        || text.lines().any(|l| {
            let l = l.trim_start();
            (l.starts_with("use hex_api::conformance")
                && (l.contains("PATIENCE") || l.contains("PROGRAM_END")))
                || l.contains("= hex_api::conformance::PATIENCE;")
                || l.contains("= hex_api::conformance::PROGRAM_END;")
        })
}

/// Whether `line` names the program-end record: by its path, or bare where the file brings it in (`brought`). A
/// mention inside a message, `{PATIENCE:?}`, bounds nothing and isn't counted.
fn names_it(line: &str, brought: bool) -> bool {
    let line = line.replace("{PATIENCE:?}", "");
    let words = |w: &str| {
        line.match_indices(w).any(|(at, _)| {
            let before = line.get(..at).and_then(|s| s.chars().next_back());
            let after = line.get(at + w.len()..).and_then(|s| s.chars().next());
            !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
                && !after.is_some_and(|c| c.is_alphanumeric() || c == '_')
        })
    };
    line.contains("conformance::PATIENCE")
        || line.contains("conformance::PROGRAM_END")
        || (brought && (words("PATIENCE") || words("PROGRAM_END")))
}

/// Whether the line uses the record as a program's end's bound, defines it or checks what it is, or only mentions it.
fn a_programs_end(line: &str) -> bool {
    let code = line.trim_start();
    code.starts_with("//")
        || code.starts_with("use ")
        || code.starts_with("pub use ")
        || code.contains(".wait_for(PATIENCE")
        || code.contains(".wait_for(conformance::PATIENCE")
        || code.contains(".wait_for(hex_api::conformance::PATIENCE")
        || code.contains("exit_of(")
        || code.starts_with("pub const PATIENCE")
        || code.starts_with("const PATIENCE: Duration = hex_api::conformance::PATIENCE;")
        || code.starts_with("assert_eq!(conformance::PATIENCE,")
}

/// The lines of `text`, the file `file`'s, that use the program-end record to bound another fact than a program's
/// end, numbered from 1.
fn misused_in(text: &str, file: &str) -> Vec<(usize, String)> {
    let brought = brings_it(text, file);
    text.lines()
        .enumerate()
        .filter(|(_, line)| names_it(line, brought) && !a_programs_end(line))
        .map(|(n, line)| (n + 1, line.trim().to_owned()))
        .collect()
}

/// This file, whose strings name the record to look for it, and so aren't read.
const SELF: &str = "crates/hex-bench/tests/test_waits.rs";

/// The uses of the program-end record in `files` that bound another fact than a program's end: `file:line: text`.
fn misused(files: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for file in files.iter().filter(|f| f.as_str() != SELF) {
        let text = std::fs::read_to_string(repo().join(file)).unwrap();
        for (n, line) in misused_in(&text, file) {
            out.push(format!("{file}:{n}: {line}"));
        }
    }
    out
}

/// Do: read every test source in the workspace for the program-end record. Expect: it bounds a program's end alone
/// (a `Proc`'s `wait_for`, the cases' `exit_of`), every other wait taking its own fact's record.
#[test]
fn the_program_end_record_bounds_a_programs_end_alone() {
    let found = misused(&sources());
    assert!(
        found.is_empty(),
        "{} waits bounded by the program-end record wait for another fact:\n{}",
        found.len(),
        found.join("\n")
    );
}

/// The walk names each source as the repository does, so the file defining the record, whose bare `PATIENCE` is the
/// record, and this one, which isn't read, are found by their names on every OS (AC-T7.4.1). Do: walk the workspace.
/// Expect: [`CONFORMANCE`] and [`SELF`] each named once.
#[test]
fn the_walk_names_the_record_and_this_file_as_the_repository_does() {
    let files = sources();
    for named in [CONFORMANCE, SELF] {
        assert_eq!(
            files.iter().filter(|f| f.as_str() == named).count(),
            1,
            "{named} among the {} sources walked",
            files.len()
        );
    }
}

/// Non-vacuity: the reading finds the record where it is used rightly and where it isn't. Do: read a test source
/// that brings the record in by name, a `Proc`'s wait, a cgroup's emptying and a deadline bounded by it, and a word
/// that only contains its name. Expect: the emptying and the deadline named, the program's wait and the word not.
#[test]
fn the_reading_tells_a_programs_end_from_another_fact() {
    let text = "use hex_api::conformance::{self, PATIENCE};\nfn t() {\n    p.wait_for(PATIENCE).unwrap();\n    \
                cg.wait_empty(conformance::PATIENCE);\n    let d = Instant::now().checked_add(PATIENCE);\n    \
                let x = MY_PATIENCES;\n    panic!(\"none in {PATIENCE:?}\");\n}\n";
    let named: Vec<usize> = misused_in(text, "crates/hex-x/tests/case.rs")
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(named, [4, 5]);
}
