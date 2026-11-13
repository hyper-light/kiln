//! Every pipe the workspace makes is close-on-exec (decision 3: descriptors reach a program only on purpose;
//! CVE-2024-21626's class). A process holds a pipe made without it from its making until its close, and any program
//! another thread starts meanwhile inherits it: a test's own in-process pipe, in the suite's one process, was inherited
//! by the stand-in shim's bystander in `run::a_process_started_while_a_shim_starts_inherits_none_of_its_descriptors`,
//! which failed 2 of 3 in the full suite and passed alone (the lead, 2026-11-09). A pipe a program is to hold is made
//! close-on-exec and given to it in its child (`sys.rs`'s `dup2` onto its stdio), never left inheritable in the agent.
//! `std::io::pipe` makes its pipes close-on-exec: at once on Linux (`pipe2(O_CLOEXEC)`), and on macOS, which has no
//! pipe2, by pipe(2) and then fcntl, a gap a spawn that passes on every descriptor not marked (std's, whose posix_spawn
//! sets no `POSIX_SPAWN_CLOEXEC_DEFAULT`) hands to its program; so on macOS every program Hex starts is started with
//! `POSIX_SPAWN_CLOEXEC_DEFAULT` (`hex_runtime::macos::command` for std's `Command` calls). rustix's `pipe()` is
//! pipe(2), with none, and `pipe_with` is close-on-exec only when its flags say so. This reads every source in the
//! workspace and names each pipe made otherwise, by file and line.
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

/// How far past `pipe_with(` its flags are looked for: the call's argument, to its closing parenthesis.
fn flags_of(text: &str, at: usize) -> &str {
    let rest = text.get(at..).unwrap_or("");
    rest.find(')')
        .and_then(|end| rest.get(..end))
        .unwrap_or(rest)
}

/// The pipes `text` makes without close-on-exec, each by its line, numbered from 1: rustix's `pipe()`, libc's `pipe`,
/// and a `pipe_with` whose flags don't name `CLOEXEC`. A comment's mention makes none.
fn inheritable_in(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut at = 0;
    // Each line with its own end, `\n` or a checkout's `\r\n`, so its offset in `text` is its own.
    for (n, ended) in text.split_inclusive('\n').enumerate() {
        let line = ended
            .strip_suffix("\r\n")
            .or_else(|| ended.strip_suffix('\n'))
            .unwrap_or(ended);
        let code = line.trim_start();
        let start = at;
        at += ended.len();
        if code.starts_with("//") {
            continue;
        }
        let bare = code.contains("rustix::pipe::pipe()")
            || code.contains("libc::pipe(")
            || code.starts_with("use rustix::pipe::pipe;");
        let unflagged = line
            .match_indices("pipe_with(")
            .any(|(i, w)| !flags_of(text, start + i + w.len()).contains("CLOEXEC"));
        if bare || unflagged {
            out.push((n + 1, code.trim_end().to_owned()));
        }
    }
    out
}

/// This file, whose strings name the calls to look for them, and so isn't read.
const SELF: &str = "crates/hex-bench/tests/pipes.rs";

/// The pipes made without close-on-exec in `files`: `file:line: text`.
fn inheritable(files: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for file in files.iter().filter(|f| f.as_str() != SELF) {
        let text = std::fs::read_to_string(repo().join(file)).unwrap();
        for (n, line) in inheritable_in(&text) {
            out.push(format!("{file}:{n}: {line}"));
        }
    }
    out
}

/// Do: read every source in the workspace for the pipes it makes. Expect: each close-on-exec.
#[test]
fn every_pipe_the_workspace_makes_is_close_on_exec() {
    let found = inheritable(&sources());
    assert!(
        found.is_empty(),
        "{} pipes made without close-on-exec, held by any program started while they are open:\n{}",
        found.len(),
        found.join("\n")
    );
}

/// A source with rustix's `pipe()`, a `pipe_with` of other flags, one of `CLOEXEC` with them across lines, std's pipe,
/// and a comment naming `pipe()`: its first two lines alone make a pipe without close-on-exec.
const CALLS: &str = "\
let (r, w) = rustix::pipe::pipe().unwrap();
let (r, w) = rustix::pipe::pipe_with(PipeFlags::NONBLOCK).unwrap();
let (r, w) = rustix::pipe::pipe_with(
    PipeFlags::CLOEXEC | PipeFlags::NONBLOCK,
)
.unwrap();
let (r, w) = std::io::pipe().unwrap();
// rustix::pipe::pipe() would leave it inheritable
";

/// Non-vacuity: the reading finds a pipe made without close-on-exec, and only that. Do: read [`CALLS`]. Expect: the
/// first two named, by line, and none of the rest.
#[test]
fn the_reading_names_a_pipe_made_without_close_on_exec_alone() {
    let found: Vec<usize> = inheritable_in(CALLS).into_iter().map(|(n, _)| n).collect();
    assert_eq!(found, vec![1, 2]);
}

/// A checkout's lines may end `\r\n` (git's `core.autocrlf`, on by default on Windows), and a call's flags are read at
/// its own offset however many such lines come before it (AC-T7.4.1). Do: read [`CALLS`] with `\r\n` ends, ten times
/// over. Expect: the first two lines of each copy named, by line, and none of the rest.
#[test]
fn a_crlf_checkout_is_read_at_each_calls_own_offset() {
    let lines = CALLS.lines().count();
    let text = CALLS.replace('\n', "\r\n").repeat(10);
    let found: Vec<usize> = inheritable_in(&text).into_iter().map(|(n, _)| n).collect();
    let want: Vec<usize> = (0..10)
        .flat_map(|copy| [copy * lines + 1, copy * lines + 2])
        .collect();
    assert_eq!(found, want);
}
