//! Doc-truth (CLAUDE.md §5 and §6): a figure the design quotes from a run is the run's. Between a
//! `<!-- record: PATH -->` marker and its `<!-- /record -->` in docs/design/runtime-architecture.md, every number is
//! one the record PATH holds, so a measured figure can't drift from the run that measured it. Decision 4's (CP-T7.1,
//! goal 4 plan step 1: what a network namespace costs, and what a pool of them holds) are the first.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::path::{Path, PathBuf};

const DESIGN: &str = "docs/design/runtime-architecture.md";
const OPEN: &str = "<!-- record: ";
const CLOSE: &str = "<!-- /record -->";

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The numbers in `text`, as a figure is written: runs of digits with the commas and points between digits
/// (`6,127`, `0.571`, `6.19.10`), each with its commas dropped so `6,127` and `6127` are one figure.
fn numbers(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let mut n = String::new();
        while i < chars.len() {
            let c = chars[i];
            let joins =
                (c == ',' || c == '.') && chars.get(i + 1).is_some_and(char::is_ascii_digit);
            if c.is_ascii_digit() {
                n.push(c);
            } else if joins {
                if c == '.' {
                    n.push(c);
                }
            } else {
                break;
            }
            i += 1;
        }
        out.push(n);
    }
    out
}

/// `text` with every comma between two digits taken out, so its figures read as [`numbers`] gives them.
fn uncommaed(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    chars
        .iter()
        .enumerate()
        .filter(|&(i, &c)| {
            !(c == ','
                && i > 0
                && chars[i - 1].is_ascii_digit()
                && chars.get(i + 1).is_some_and(char::is_ascii_digit))
        })
        .map(|(_, &c)| c)
        .collect()
}

/// Whether `record` holds the figure `n` whole: not as a part of a longer run of digits.
fn holds(record: &str, n: &str) -> bool {
    let digit_at = |i: usize| record.as_bytes().get(i).is_some_and(u8::is_ascii_digit);
    record
        .match_indices(n)
        .any(|(at, _)| (at == 0 || !digit_at(at - 1)) && !digit_at(at + n.len()))
}

/// Each `(record path, quoted text)` the design marks.
fn marked(design: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = design;
    while let Some((_, after)) = rest.split_once(OPEN) {
        let (path, body) = after
            .split_once(" -->")
            .expect("a record marker ends with ` -->`");
        let (quoted, next) = body
            .split_once(CLOSE)
            .expect("a record marker is closed by `<!-- /record -->`");
        out.push((path.trim().to_owned(), quoted.to_owned()));
        rest = next;
    }
    out
}

/// Decision 5's record: the create breakdown's run on fedora (goal 5 plan step 1).
const CREATE_RECORD: &str = "bench/competitive/probes/create_breakdown.md";

/// How many figures each of `spans` quotes, in order, and each one the record its span names doesn't hold.
fn unheld(spans: &[(String, String)]) -> (Vec<usize>, Vec<String>) {
    let mut quoted = Vec::new();
    let mut missing = Vec::new();
    for (path, body) in spans {
        let record = std::fs::read_to_string(repo().join(path))
            .unwrap_or_else(|e| panic!("the record {path} the design names: {e}"));
        let record = uncommaed(&record);
        let figures = numbers(body);
        quoted.push(figures.len());
        for n in figures {
            if !holds(&record, &n) {
                missing.push(format!("{n} (quoted from {path})"));
            }
        }
    }
    (quoted, missing)
}

/// Non-vacuity, counted from the text rather than a picked floor (Ada's 2026-11-06 rule): every marker `text` opens
/// is read as a span and closed, and the spans that quote no figure, by record, so a span emptied or one the reading
/// missed is reported.
fn vacant(text: &str, spans: &[(String, String)], quoted: &[usize]) -> Result<(), String> {
    let (opens, closes) = (text.matches(OPEN).count(), text.matches(CLOSE).count());
    if (opens, closes) != (spans.len(), spans.len()) {
        return Err(format!(
            "{opens} record markers open and {closes} close, {} read as spans",
            spans.len()
        ));
    }
    let empty: Vec<&str> = spans
        .iter()
        .zip(quoted)
        .filter(|&(_, &n)| n == 0)
        .map(|((path, _), _)| path.as_str())
        .collect();
    if empty.is_empty() {
        Ok(())
    } else {
        Err(format!("marked spans that quote no figure: {empty:?}"))
    }
}

/// The design's marked spans that name `record`.
fn naming(record: &str) -> Vec<(String, String)> {
    let design = std::fs::read_to_string(repo().join(DESIGN)).expect("the design document");
    marked(&design)
        .into_iter()
        .filter(|(path, _)| path == record)
        .collect()
}

/// `body` with its first figure (as [`numbers`] reads one) written as `to`.
fn first_figure_as(body: &str, to: &str) -> String {
    let chars: Vec<char> = body.chars().collect();
    let start = chars
        .iter()
        .position(char::is_ascii_digit)
        .expect("a span that quotes a figure");
    let mut end = start;
    while let Some(&c) = chars.get(end) {
        let joins = (c == ',' || c == '.') && chars.get(end + 1).is_some_and(char::is_ascii_digit);
        if !(c.is_ascii_digit() || joins) {
            break;
        }
        end += 1;
    }
    let mut out: String = chars[..start].iter().collect();
    out.push_str(to);
    out.extend(&chars[end..]);
    out
}

/// Every figure the design marks, decision 4's and decision 5's among them, is in the record its span names.
#[test]
fn every_figure_the_design_marks_is_in_its_record() {
    let design = std::fs::read_to_string(repo().join(DESIGN)).expect("the design document");
    let spans = marked(&design);
    let netns = naming("bench/competitive/probes/netns_pool.md");
    assert!(
        netns.len() >= 4,
        "decision 4 marks its four tables of record, found {}",
        netns.len()
    );
    let (quoted, missing) = unheld(&spans);
    // Non-vacuity: every marker is read and every span quotes a figure.
    if let Err(e) = vacant(&design, &spans, &quoted) {
        panic!("{e}");
    }
    assert!(
        missing.is_empty(),
        "figures the design quotes that their record doesn't hold: {missing:?}"
    );
}

/// Decision 5's quoted figures (CP-T7.1, AC-T7.1.3: a container's start, of which create is the most; goal 5 plan
/// step 1: the parts of a seam create on fedora, what each lever takes and what's left) are each in the create
/// breakdown's record, which names the run they came from.
#[test]
fn decision_5s_figures_are_the_create_breakdowns() {
    let spans = naming(CREATE_RECORD);
    assert!(
        spans.len() >= 3,
        "decision 5 marks its three tables of record (the parts by group, the levers, what's left), found {}",
        spans.len()
    );
    let (quoted, missing) = unheld(&spans);
    // Non-vacuity: each of decision 5's spans quotes a figure, so an emptied table fails here.
    assert!(
        quoted.iter().all(|&n| n > 0),
        "figures quoted by each of decision 5's marked spans: {quoted:?}"
    );
    assert!(
        missing.is_empty(),
        "figures decision 5 quotes that the create breakdown's record doesn't hold: {missing:?}"
    );
}

/// The check fails when a figure drifts (CP-T7.1, goal 5 plan step 1's test): decision 5's first quoted figure,
/// written as one its record doesn't hold, is reported, named with the record, and nothing else is.
#[test]
fn a_figure_that_drifts_from_its_record_is_reported() {
    let mut spans = naming(CREATE_RECORD);
    let drift = "987654.321";
    let record =
        uncommaed(&std::fs::read_to_string(repo().join(CREATE_RECORD)).expect("the record"));
    assert!(
        !holds(&record, drift),
        "the drifted figure is one the record doesn't hold"
    );
    let (_, body) = spans
        .first_mut()
        .expect("decision 5 marks a table of record");
    *body = first_figure_as(body, drift);
    let (_, missing) = unheld(&spans);
    assert_eq!(missing, [format!("{drift} (quoted from {CREATE_RECORD})")]);
}

/// The non-vacuity check fails on what it guards (CLAUDE.md §6): a marked span emptied of its figures is reported by
/// its record, and so is a marker the reading misses; the design as written passes.
#[test]
fn an_emptied_span_or_an_unread_marker_is_reported() {
    let design = std::fs::read_to_string(repo().join(DESIGN)).expect("the design document");
    let mut spans = marked(&design);
    let (mut quoted, _) = unheld(&spans);
    assert_eq!(vacant(&design, &spans, &quoted), Ok(()));
    quoted[0] = 0;
    let err = vacant(&design, &spans, &quoted).unwrap_err();
    assert!(err.contains(&spans[0].0), "{err}");
    quoted[0] = 1;
    spans.pop();
    assert!(vacant(&design, &spans, &quoted).is_err());
}

#[test]
fn a_spans_first_figure_is_rewritten_whole() {
    assert_eq!(first_figure_as("| a | 6,127.5 ms |", "1"), "| a | 1 ms |");
    assert_eq!(first_figure_as("x 0.571, 2", "9.9"), "x 9.9, 2");
    assert_eq!(first_figure_as("p50 of r1", "7"), "p7 of r1");
}

#[test]
fn a_figure_is_read_whole_and_its_commas_dropped() {
    assert_eq!(numbers("6,127, 5,260/s"), ["6127", "5260"]);
    assert_eq!(numbers("+0.571 (1.04x)"), ["0.571", "1.04"]);
    assert_eq!(
        numbers("Linux 6.19.10, i9-14900HX"),
        ["6.19.10", "9", "14900"]
    );
    assert_eq!(
        uncommaed("1,029 against 1,079/s, and"),
        "1029 against 1079/s, and"
    );
}

#[test]
fn a_figure_held_inside_a_longer_one_is_not_held() {
    assert!(holds("one thread 6127/s", "6127"));
    assert!(!holds("one thread 16127/s", "6127"));
    assert!(!holds("0.5710 ms", "0.571"));
    assert!(holds("Linux 6.18.54.1", "6.18"));
}

#[test]
fn a_marker_names_its_record_and_closes() {
    let text = "a <!-- record: x.md -->\n| 1.5 |\n<!-- /record --> b <!-- record: y.md --> 2 <!-- /record -->";
    let spans = marked(text);
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0].0, "x.md");
    assert!(spans[0].1.contains("1.5"));
    assert_eq!(spans[1].0, "y.md");
}
