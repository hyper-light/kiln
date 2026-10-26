//! docs/design/topics.md states the `Topics` contract: this test holds it to the code (CLAUDE.md §6,
//! a doc-truth test). Its clauses are exactly the conformance suite's cases, the same ids in the
//! same order in the same words; its methods are exactly the traits' (read from their source); and
//! its bounds are the code's numbers.

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

use std::path::PathBuf;

use quiver_api::conformance::{SimFixture, cases};

fn read(path: &str) -> String {
    std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path)).unwrap()
}

/// The `fn` names of trait `name` in `source`, in order.
fn methods(source: &str, name: &str) -> Vec<String> {
    let start = source
        .find(&format!("pub trait {name}"))
        .unwrap_or_else(|| panic!("no trait {name}"));
    let body = source.get(start..).unwrap();
    let end = body.find("\n}").unwrap();
    body.get(..end)
        .unwrap()
        .lines()
        .filter_map(|l| {
            l.trim()
                .strip_prefix("fn ")
                .map(|rest| rest.split(['<', '(']).next().unwrap_or_default().to_owned())
        })
        .collect()
}

/// The names a doc line lists after `prefix`, in order.
fn listed(doc: &str, prefix: &str) -> Vec<String> {
    let line = doc
        .lines()
        .find(|l| l.starts_with(prefix))
        .unwrap_or_else(|| panic!("no line {prefix:?}"));
    line.get(prefix.len()..)
        .unwrap()
        .trim_end_matches('.')
        .split(',')
        .map(|m| m.trim().trim_matches('`').to_owned())
        .collect()
}

#[test]
fn the_clauses_are_the_suites_cases_in_order_and_words() {
    let doc = read("../../docs/design/topics.md");
    let said: Vec<(String, String)> = doc
        .lines()
        .filter_map(|l| l.strip_prefix("- **QUIVER-"))
        .map(|rest| {
            let (n, words) = rest.split_once("** ").unwrap();
            (format!("QUIVER-{n}"), words.to_owned())
        })
        .collect();
    let code: Vec<(String, String)> = cases::<SimFixture>()
        .iter()
        .map(|c| (c.id.to_owned(), c.says.to_owned()))
        .collect();
    assert_eq!(said, code);
}

#[test]
fn the_methods_are_the_traits() {
    let doc = read("../../docs/design/topics.md");
    let source = read("src/lib.rs");
    assert_eq!(
        listed(&doc, "Methods of `Topics`: "),
        methods(&source, "Topics")
    );
    assert_eq!(
        listed(&doc, "Methods of `Subscription`: "),
        methods(&source, "Subscription")
    );
}

#[test]
fn the_bounds_are_the_codes() {
    let doc = read("../../docs/design/topics.md").replace('\n', " ");
    assert_eq!(quiver_api::NAME_MAX, 128);
    assert!(
        doc.contains("Topic names are 1 to 128 of"),
        "the name bound"
    );
    assert_eq!(quiver_api::KEY_MAX, 1024);
    assert!(doc.contains("keys to 1024 bytes"));
    assert_eq!(quiver_api::PAYLOAD_MAX, 1 << 20);
    assert!(doc.contains("payloads to 1 MiB"));
    assert_eq!(quiver_api::READ_MAX, 1000);
    assert!(doc.contains("1000 messages a read"));
    assert_eq!(quiver_api::RETAIN_MESSAGES_MAX, 1_000_000);
    assert!(doc.contains("retains 1 to a million messages"));
    assert_eq!(quiver_api::RETAIN_BYTES_MAX, 4 << 30);
    assert!(doc.contains("1 byte to 4 GiB"));
}
