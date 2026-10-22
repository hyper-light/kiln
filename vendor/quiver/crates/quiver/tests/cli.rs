//! The `quiver` binary says what it is (kiln's CP-M1: the bar's first gated code), run as a process.

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

use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_quiver"))
        .args(args)
        .output()
        .expect("the binary runs")
}

#[test]
fn version_prints_the_name_version_and_commit() {
    let out = run(&["--version"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.starts_with(concat!("quiver ", env!("CARGO_PKG_VERSION"), " (")),
        "{text}"
    );
    assert!(text.ends_with(")\n"), "{text}");
}

#[test]
fn help_prints_what_it_is_and_the_usage() {
    let out = run(&["--help"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("quiver: Quiver"), "{text}");
    assert!(text.contains("usage: quiver"), "{text}");
}

#[test]
fn an_unknown_argument_is_refused_with_the_usage() {
    let out = run(&["--frobnicate"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let text = String::from_utf8(out.stderr).unwrap();
    assert!(text.contains("unknown arguments: --frobnicate"), "{text}");
    assert!(text.contains("usage: quiver"), "{text}");
}

#[test]
fn no_argument_prints_the_usage_and_fails() {
    let out = run(&[]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(
        String::from_utf8(out.stderr)
            .unwrap()
            .starts_with("usage: quiver")
    );
}

#[test]
fn an_argument_that_isnt_unicode_is_refused_not_a_panic() {
    assert_eq!(quiver::reply(&[None]).status, 2);
    assert!(quiver::reply(&[None]).text.contains("isn't Unicode"));
}
