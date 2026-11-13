//! The census of inputs (`hex_sim::census`) is the committed traces' own: a doc-truth test.
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

use hex_sim::census;
use hex_sim::packing::minimal_shapes;
use hex_sim::replay::{Limits, parse};

/// What one trace holds, by the census's measures.
#[derive(Debug, Default, Clone, Copy)]
struct Holds {
    bytes: usize,
    nodes: usize,
    legs: usize,
    owners: usize,
    groups: usize,
    overheads: usize,
    shapes: usize,
}

/// Every trace committed in bench/sched, with what it holds: rows counted from its text, and its
/// minimal shapes from its parse with no bound but the text's own.
fn committed() -> Vec<(String, Holds)> {
    let unbounded = Limits {
        max_bytes: usize::MAX,
        max_nodes: usize::MAX,
        max_legs: usize::MAX,
        max_owners: usize::MAX,
        max_groups: usize::MAX,
        max_overheads: usize::MAX,
    };
    let mut out = Vec::new();
    for entry in std::fs::read_dir("../../bench/sched").unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "trace") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let rows = |kind: &str| {
            text.lines()
                .filter(|l| l.trim().split(',').next() == Some(kind))
                .count()
        };
        let cluster = parse(&text, &unbounded).unwrap();
        let holds = Holds {
            bytes: text.len(),
            nodes: rows("node"),
            legs: rows("leg"),
            owners: rows("owner"),
            groups: rows("group"),
            overheads: rows("overhead"),
            shapes: minimal_shapes(&cluster).len(),
        };
        out.push((path.display().to_string(), holds));
    }
    out
}

/// The input-bound rule (the lead, 2026-11-06). Do: read every trace committed in bench/sched and
/// count what each holds. Expect each census figure to be the most any of them holds, so each bound
/// derived from it (`replay::Limits::default`, `index::Shapes::MAX`) is twice the recorded inputs'
/// most; a trace that holds more names the figure to raise.
#[test]
fn the_census_is_the_most_the_committed_traces_hold() {
    let traces = committed();
    assert!(
        traces.len() >= 4,
        "the census reads the committed traces: {traces:?}"
    );
    let most = |measure: fn(&Holds) -> usize| {
        traces
            .iter()
            .map(|(name, h)| (measure(h), name.clone()))
            .max()
            .unwrap()
    };
    for (what, figure, (held, by)) in [
        ("BYTES", census::BYTES, most(|h| h.bytes)),
        ("NODES", census::NODES, most(|h| h.nodes)),
        ("LEGS", census::LEGS, most(|h| h.legs)),
        ("OWNERS", census::OWNERS, most(|h| h.owners)),
        ("GROUPS", census::GROUPS, most(|h| h.groups)),
        ("OVERHEADS", census::OVERHEADS, most(|h| h.overheads)),
        ("SHAPES", census::SHAPES, most(|h| h.shapes)),
    ] {
        assert_eq!(
            figure, held,
            "census::{what} is {figure}; {by} holds {held}"
        );
    }
}

/// Do: take the default limits and the shapes' bound. Expect each to be the census's figure times the
/// rule's margin, and every committed trace to parse within them.
#[test]
fn the_default_bounds_are_twice_the_census() {
    let m = census::INPUT_MARGIN;
    assert_eq!(
        Limits::default(),
        Limits {
            max_bytes: census::BYTES * m,
            max_nodes: census::NODES * m,
            max_legs: census::LEGS * m,
            max_owners: census::OWNERS * m,
            max_groups: census::GROUPS * m,
            max_overheads: census::OVERHEADS * m,
        }
    );
    assert_eq!(hex_sim::index::Shapes::MAX, census::SHAPES * m);
    for (name, _) in committed() {
        let text = std::fs::read_to_string(&name).unwrap();
        parse(&text, &Limits::default()).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}
