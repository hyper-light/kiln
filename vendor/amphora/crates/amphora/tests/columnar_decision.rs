//! CS.1, T-CS.1.1 (G11 step 1, kiln plan 01a16b0f477676d5a7ce168b80aa29f5): doc truth for the
//! columnar decision (CLAUDE.md sections 5 and 6). `docs/design/columnar.md` cites its claims as
//! `[cN]`; `docs/research/columnar.md` holds each claim's source, locator and quoted words, and a
//! graded table of the sources. Do: read both. Expect: every citation resolves to a claim and every
//! claim is cited; every claim's source is in the table with a locator and a grade from A to F, and
//! every source there is used; every paragraph, list item and table row of the decision that states
//! a number cites a claim, and each such number is in a quote it cites (in the comparison, a quote
//! its own cell cites); every paragraph of the sections that describe Amphora and the six designs,
//! and every cell of the comparison after its first column, cites a claim. That the quotes are the sources' own words, on the pages given, was
//! checked against the sources when the note was written (its "How to read this").

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::collections::{BTreeMap, BTreeSet};

const DESIGN: &str = include_str!("../../../docs/design/columnar.md");
const NOTE: &str = include_str!("../../../docs/research/columnar.md");

/// A number right after one of these words names a section, a step or a decision, not a claim.
const SKIP: [&str; 10] = [
    "step",
    "steps",
    "part",
    "parts",
    "decision",
    "decisions",
    "falsifier",
    "falsifiers",
    "stage",
    "§",
];

struct Claim {
    source: String,
    locator: String,
    quote: String,
}

/// The note's claim lines: `- **cN** · source · locator · "quote"`.
fn claims(note: &str) -> Result<BTreeMap<String, Claim>, String> {
    let mut out = BTreeMap::new();
    for line in note.lines().filter(|l| l.starts_with("- **c")) {
        let parts: Vec<&str> = line.splitn(4, " · ").collect();
        if parts.len() != 4 {
            return Err(format!("a claim line without four parts: {line}"));
        }
        let id = parts[0].trim_start_matches("- **").trim_end_matches("**");
        let Some(quote) = parts[3]
            .split_once('"')
            .and_then(|(_, after)| after.rsplit_once('"'))
            .map(|(quote, _)| quote)
        else {
            return Err(format!("{id} has no quote"));
        };
        if quote.is_empty() {
            return Err(format!("{id} has an empty quote"));
        }
        let claim = Claim {
            source: parts[1].trim().to_owned(),
            locator: parts[2].trim().to_owned(),
            quote: quote.to_owned(),
        };
        if out.insert(id.to_owned(), claim).is_some() {
            return Err(format!("{id} is defined twice"));
        }
    }
    Ok(out)
}

/// The note's sources table: key to (locator, grade).
fn sources(note: &str) -> BTreeMap<String, (String, String)> {
    let rest = note.split_once("## Sources").map_or("", |(_, rest)| rest);
    let section = rest
        .split_once("\n## ")
        .map_or(rest, |(section, _)| section);
    section
        .lines()
        .filter(|l| l.starts_with("| `"))
        .map(|l| {
            let cells: Vec<&str> = l.split('|').map(str::trim).collect();
            let cell = |i: usize| cells.get(i).copied().unwrap_or("").to_owned();
            (cell(1).trim_matches('`').to_owned(), (cell(3), cell(5)))
        })
        .collect()
}

/// The claim IDs a text cites, as `[cN]`.
fn cites(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some((_, after)) = rest.split_once("[c") {
        let tail = after.trim_start_matches(|ch: char| ch.is_ascii_digit());
        let digits = after.strip_suffix(tail).unwrap_or_default();
        if !digits.is_empty() && tail.starts_with(']') {
            out.push(format!("c{digits}"));
        }
        rest = after;
    }
    out
}

/// The numbers a text states, without thousands separators. Digits that are part of a name
/// (`G11`, `CP-T1.3`, `x86_64`, `p99`), dates, and numbers right after a word in [`SKIP`] are not
/// numbers here. With `groups`, digit groups split by a space (`300 000`) are one number, as
/// some papers print them.
fn numbers(text: &str, groups: bool) -> Vec<String> {
    let c: Vec<char> = text.chars().collect();
    let at = |i: usize| c.get(i).copied().unwrap_or(' ');
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        if !c[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let p = if i > 0 { at(i - 1) } else { ' ' };
        let pp = if i > 1 { at(i - 2) } else { ' ' };
        let before: String = c[..i].iter().collect();
        let word: String = before
            .split_whitespace()
            .last()
            .unwrap_or("")
            .to_lowercase()
            .chars()
            .filter(|ch| ch.is_ascii_lowercase() || *ch == '§')
            .collect();
        let named = p.is_alphanumeric()
            || p == '_'
            || p == '§'
            || ((p == '.' || p == '-') && pp.is_alphanumeric());
        if named || (p.is_whitespace() && SKIP.contains(&word.as_str())) {
            while i < c.len() && (c[i].is_alphanumeric() || matches!(c[i], '.' | '_' | '-')) {
                i += 1;
            }
            continue;
        }
        let mut j = i;
        loop {
            let ch = at(j);
            let next_digit = at(j + 1).is_ascii_digit();
            let grouped = groups
                && ch == ' '
                && (1..=3).all(|k| at(j + k).is_ascii_digit())
                && !at(j + 4).is_ascii_digit()
                && !(at(j + 4) == '.' && at(j + 5).is_ascii_digit());
            if ch.is_ascii_digit() || ((ch == ',' || ch == '.') && next_digit) || grouped {
                j += 1;
            } else {
                break;
            }
        }
        let token: String = c[i..j]
            .iter()
            .filter(|ch| **ch != ',' && **ch != ' ')
            .collect();
        if j - i == 4 && at(j) == '-' && at(j + 1).is_ascii_digit() {
            while j < c.len() && (c[j].is_ascii_digit() || c[j] == '-') {
                j += 1;
            }
        } else {
            out.push(token);
        }
        i = j;
    }
    out
}

struct Unit {
    section: String,
    text: String,
    row: bool,
}

/// The design's paragraphs, list items (each with its continuation lines) and table rows.
fn units(design: &str) -> Vec<Unit> {
    let mut out = Vec::new();
    let mut section = String::new();
    let mut current: Option<String> = None;
    for line in design.lines() {
        let trimmed = line.trim_start();
        let item = trimmed.starts_with("- ")
            || trimmed
                .split_once(". ")
                .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|ch| ch.is_ascii_digit()));
        if (line.starts_with('#') || line.trim().is_empty() || line.starts_with('|') || item)
            && let Some(text) = current.take()
        {
            out.push(Unit {
                section: section.clone(),
                text,
                row: false,
            });
        }
        if line.starts_with('#') {
            section = line.to_owned();
        } else if line.starts_with('|') {
            if !line.starts_with("|---") {
                out.push(Unit {
                    section: section.clone(),
                    text: line.to_owned(),
                    row: true,
                });
            }
        } else if item {
            current = Some(line.to_owned());
        } else if !line.trim().is_empty() {
            current = Some(match current.take() {
                Some(text) => format!("{text}\n{line}"),
                None => line.to_owned(),
            });
        }
    }
    if let Some(text) = current {
        out.push(Unit {
            section,
            text,
            row: false,
        });
    }
    out
}

/// What a text states: citations and code spans taken out, and a list item's marker.
fn stated(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    loop {
        let cite = rest.find("[c");
        let code = rest.find('`');
        let Some(at) = [cite, code].into_iter().flatten().min() else {
            out.push_str(rest);
            break;
        };
        let (before, from) = rest.split_at_checked(at).unwrap_or((rest, ""));
        out.push_str(before);
        let close = if Some(at) == code { '`' } else { ']' };
        // Past the opening '[' or '`' (one byte), then past its close.
        rest = from
            .get(1..)
            .and_then(|inner| inner.split_once(close))
            .map_or("", |(_, after)| after);
    }
    let t = out.trim_start();
    let marker = if let Some(stripped) = t.strip_prefix("- ") {
        Some(stripped)
    } else {
        t.split_once(". ").and_then(|(n, after)| {
            if !n.is_empty() && n.chars().all(|ch| ch.is_ascii_digit()) {
                Some(after)
            } else {
                None
            }
        })
    };
    marker.map_or(out.clone(), str::to_owned)
}

/// Every way the decision's text fails its sources, in words.
fn faults(design: &str, note: &str) -> Vec<String> {
    let mut faults = Vec::new();
    let claims = match claims(note) {
        Ok(claims) => claims,
        Err(e) => return vec![e],
    };
    if claims.is_empty() {
        faults.push("the note has no claim lines".to_owned());
    }
    let sources = sources(note);
    for (id, claim) in &claims {
        if !sources.contains_key(&claim.source) {
            faults.push(format!(
                "{id} names source {}, not in the table",
                claim.source
            ));
        }
        if claim.locator.is_empty() {
            faults.push(format!("{id} has no locator"));
        }
    }
    for (key, (locator, grade)) in &sources {
        if locator.is_empty() || !matches!(grade.as_str(), "A" | "B" | "C" | "D" | "E" | "F") {
            faults.push(format!("source {key} lacks a locator or a grade A to F"));
        }
        if !claims.values().any(|c| &c.source == key) {
            faults.push(format!("source {key} backs no claim"));
        }
    }
    let cited: BTreeSet<String> = cites(design).into_iter().collect();
    for id in cited.iter().filter(|id| !claims.contains_key(*id)) {
        faults.push(format!("{id} is cited but has no claim line"));
    }
    for id in claims.keys().filter(|id| !cited.contains(*id)) {
        faults.push(format!("{id} is in the note but cited nowhere"));
    }
    for unit in units(design) {
        if unit.row && unit.text.starts_with("| design") {
            continue;
        }
        let ids = cites(&unit.text);
        let comparison = unit.section.starts_with("## The comparison") && unit.row;
        // A comparison cell's numbers are held to the quotes that cell cites: a row's other cells
        // cite other claims, whose quotes may hold the same number by chance.
        let cells: Vec<&str> = unit.text.split('|').collect();
        let pieces: Vec<&str> = if comparison {
            cells
                .iter()
                .skip(1)
                .take(cells.len().saturating_sub(2))
                .copied()
                .collect()
        } else {
            vec![unit.text.as_str()]
        };
        for piece in pieces {
            let held: BTreeSet<String> = cites(piece)
                .iter()
                .filter_map(|id| claims.get(id))
                .flat_map(|c| numbers(&c.quote, true))
                .collect();
            let head: String = piece.trim().chars().take(70).collect();
            for n in numbers(&stated(piece), false)
                .iter()
                .filter(|n| !held.contains(*n))
            {
                faults.push(format!("{n} is in no quote cited by: {head}"));
            }
        }
        let head: String = unit.text.chars().take(70).collect();
        let described = unit.section.starts_with("## What a design must fit")
            || unit.section.starts_with("## The six designs");
        if described && ids.is_empty() {
            faults.push(format!("uncited: {head}"));
        }
        if comparison {
            for cell in cells.iter().skip(2).take(cells.len().saturating_sub(3)) {
                if cites(cell).is_empty() {
                    faults.push(format!(
                        "an uncited cell of the comparison: {}",
                        cell.trim()
                    ));
                }
            }
        }
    }
    faults
}

#[test]
fn cs_1_every_claim_in_the_columnar_decision_is_cited() {
    let faults = faults(DESIGN, NOTE);
    assert!(faults.is_empty(), "{}", faults.join("\n"));
    // Non-vacuity: the check saw the decision's numbers and its comparison.
    let rows = units(DESIGN)
        .iter()
        .filter(|u| u.row && u.section.starts_with("## The comparison"))
        .count();
    assert!(rows >= 7, "the comparison has {rows} rows");
    assert!(claims(NOTE).unwrap().len() >= 90);
}

#[test]
fn cs_1_a_number_no_cited_quote_holds_is_a_fault() {
    // 36.8 is table_count's rate at one client [c12]; 36.9 is in no quote.
    let changed = DESIGN.replacen("36.8 a second", "36.9 a second", 1);
    assert_ne!(changed, DESIGN);
    let faults = faults(&changed, NOTE);
    assert!(faults.iter().any(|f| f.starts_with("36.9 ")), "{faults:?}");
}

#[test]
fn cs_1_an_uncited_paragraph_cell_or_claim_is_a_fault() {
    // A paragraph among the designs that cites nothing.
    let changed = DESIGN.replacen(
        "\n## The comparison",
        "\nAn uncited paragraph.\n\n## The comparison",
        1,
    );
    let f = faults(&changed, NOTE);
    assert!(
        f.iter().any(|f| f == "uncited: An uncited paragraph."),
        "{f:?}"
    );
    // A claim the design no longer cites.
    let changed = DESIGN.replace("[c89]", "");
    let f = faults(&changed, NOTE);
    assert!(
        f.iter()
            .any(|f| f == "c89 is in the note but cited nowhere"),
        "{f:?}"
    );
    // A comparison cell without its citation.
    let changed = DESIGN.replacen(
        "[c55]; at most 10% from analytics elsewhere [c63]",
        "; at most 10% from analytics elsewhere",
        1,
    );
    assert_ne!(changed, DESIGN);
    let f = faults(&changed, NOTE);
    assert!(
        f.iter().any(|f| f.starts_with("10 is in no quote")),
        "{f:?}"
    );
    // A citation the note has no line for.
    let changed = format!("{DESIGN}\nA claim [c9999].\n");
    let f = faults(&changed, NOTE);
    assert!(
        f.iter()
            .any(|f| f == "c9999 is cited but has no claim line"),
        "{f:?}"
    );
}

#[test]
fn cs_1_an_ungraded_or_missing_source_is_a_fault() {
    let ungraded = NOTE.replacen("| paper | C |", "| paper | ? |", 1);
    assert_ne!(ungraded, NOTE);
    let f = faults(DESIGN, &ungraded);
    assert!(
        f.iter().any(|f| f.contains("lacks a locator or a grade")),
        "{f:?}"
    );
    let missing = NOTE.replacen(
        "· sqlite3he · p. 1, abstract",
        "· nowhere · p. 1, abstract",
        1,
    );
    assert_ne!(missing, NOTE);
    let f = faults(DESIGN, &missing);
    assert!(
        f.iter().any(|f| f.contains("names source nowhere")),
        "{f:?}"
    );
}

#[test]
fn cs_1_numbers_skip_names_dates_and_references() {
    assert_eq!(
        numbers(
            "CP-T1.3 at G11 step 2, on 2026-11-21, x86_64 p99.9 §7",
            false
        ),
        Vec::<String>::new()
    );
    assert_eq!(
        numbers("from 3.5 to 123.6, 2,197 and 14,127; 11.9% 2.5x", false),
        vec!["3.5", "123.6", "2197", "14127", "11.9", "2.5"]
    );
    assert_eq!(
        numbers("processes 300 000 Tx/s at 109.5 and 180 99.1", true),
        vec!["300000", "109.5", "180", "99.1"]
    );
}
