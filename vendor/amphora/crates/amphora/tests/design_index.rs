//! AC-T1.2.3, T-T1.13.2 (G4 step 6, G15 step 3; kiln plans 01a16b0f467274bd8b9e7b113c1a393a and
//! 01a16b0f47f4770490fd4f6793cfef63): doc truth for `docs/design.md`, the index of every T1
//! decision (CLAUDE.md sections 5 and 6), and for the client protocol's research.
//!
//! Do: read `docs/design.md` and every record in `docs/design/` (README.md, the target, aside).
//! Expect: every level-two section of a record is indexed exactly once, as a decision line or as a
//! section that is not a decision; a record without level-two sections is indexed by its file; every
//! link into `docs/design/` resolves to a record and a section that exist; a decision line's area is
//! its record's and its number the section's own. And for the client protocol: every `[pN]` that
//! `docs/design.md` or `docs/design/client-protocol.md` cites is a claim line of
//! `docs/research/client-protocol.md`, every claim is cited, every claim's source is in the note's
//! table with a locator and a grade from A to F and backs a claim, every row and paragraph of the
//! comparison cites a claim, and every number either states is in a quote it cites.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::string_slice
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

const INDEX: &str = include_str!("../../../docs/design.md");
const NOTE: &str = include_str!("../../../docs/research/client-protocol.md");
/// The section of `docs/design.md` holding the comparison.
const COMPARISON: &str = "## The client protocol, researched";
/// The record of the client-protocol decision, whose citations the note also backs.
const PROTOCOL_RECORD: &str = "client-protocol";

/// Every record, area (file stem) to text, read from the tree so that a record added by anyone is
/// held to the index without a change here.
fn records() -> BTreeMap<String, String> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/design");
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some(area) = name.strip_suffix(".md")
            && name != "README.md"
        {
            out.insert(area.to_owned(), std::fs::read_to_string(&path).unwrap());
        }
    }
    out
}

/// A heading's anchor as GitHub makes it: lower case, characters other than letters, digits, `_`,
/// `-` and spaces dropped, spaces as `-`.
fn slug(heading: &str) -> String {
    heading
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | ' '))
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

struct Section {
    anchor: String,
    /// The number the record gives it (`## 3. ...`), or `–`.
    number: String,
}

/// A record's level-two sections, each with its anchor (repeats of a heading at any level, in
/// order, get `-1`, `-2`, as GitHub numbers them); headings inside code fences are not headings.
fn sections(text: &str) -> Vec<Section> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut out = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        let hashes = line.chars().take_while(|c| *c == '#').count();
        if fenced || hashes == 0 || !line[hashes..].starts_with(' ') {
            continue;
        }
        let heading = line[hashes..].trim();
        let base = slug(heading);
        let count = seen.entry(base.clone()).or_insert(0);
        let anchor = if *count == 0 {
            base
        } else {
            format!("{base}-{count}")
        };
        *count += 1;
        if hashes == 2 {
            let number = heading
                .split_once(". ")
                .filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
                .map_or("–".to_owned(), |(n, _)| n.to_owned());
            out.push(Section { anchor, number });
        }
    }
    out
}

/// The links into `docs/design/` a text holds: (file stem, anchor), the anchor empty for a link
/// to the whole record.
fn links(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some((_, after)) = rest.split_once("](design/") {
        let target = after.split_once(')').map_or(after, |(t, _)| t);
        let (file, anchor) = target.split_once('#').unwrap_or((target, ""));
        if let Some(area) = file.strip_suffix(".md") {
            out.push((area.to_owned(), anchor.to_owned()));
        }
        rest = after;
    }
    out
}

/// The text of one `## ` section of a markdown file, without its heading.
fn section<'a>(text: &'a str, heading: &str) -> &'a str {
    let rest = text
        .split_once(&format!("{heading}\n"))
        .map_or("", |(_, r)| r);
    rest.split_once("\n## ").map_or(rest, |(s, _)| s)
}

/// Every way the index fails the records, in words.
fn index_faults(index: &str, records: &BTreeMap<String, String>) -> Vec<String> {
    let mut faults = Vec::new();
    // Each indexed (area, anchor) and how often; decision lines checked for area and number.
    let mut indexed: BTreeMap<(String, String), usize> = BTreeMap::new();
    for line in section(index, "## The decisions")
        .lines()
        .filter(|l| l.starts_with("| ") && !l.starts_with("| Area "))
    {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        if cells.len() != 6 {
            faults.push(format!("a decision line without four cells: {line}"));
            continue;
        }
        let (area, number, words) = (cells[1], cells[2], cells[3]);
        let found = links(cells[4]);
        let [(file, anchor)] = found.as_slice() else {
            faults.push(format!("a decision line without one link: {line}"));
            continue;
        };
        if words.is_empty() {
            faults.push(format!("a decision line without its decision: {line}"));
        }
        if area != file {
            faults.push(format!(
                "a line in area {area} points into {file}.md: {line}"
            ));
        }
        if !anchor.is_empty()
            && let Some(text) = records.get(file)
            && let Some(s) = sections(text).iter().find(|s| &s.anchor == anchor)
            && s.number != number
        {
            faults.push(format!(
                "{file}.md#{anchor} is number {}, its line says {number}",
                s.number
            ));
        }
        *indexed.entry((file.clone(), anchor.clone())).or_insert(0) += 1;
    }
    for (file, anchor) in links(section(index, "## Sections that are not decisions")) {
        *indexed.entry((file, anchor)).or_insert(0) += 1;
    }
    // Every link anywhere in the index points at a record and a section that exist.
    for (file, anchor) in links(index) {
        let Some(text) = records.get(&file) else {
            if file != "README" {
                faults.push(format!("design/{file}.md is no record"));
            }
            continue;
        };
        if !anchor.is_empty() && !sections(text).iter().any(|s| s.anchor == anchor) {
            faults.push(format!("design/{file}.md#{anchor} is no section of it"));
        }
    }
    for (area, text) in records {
        let found = sections(text);
        if found.is_empty() && !indexed.contains_key(&(area.clone(), String::new())) {
            faults.push(format!("{area}.md has no line in the index"));
        }
        for s in found {
            match indexed.get(&(area.clone(), s.anchor.clone())) {
                None => faults.push(format!("{area}.md#{} has no line in the index", s.anchor)),
                Some(1) => {}
                Some(n) => faults.push(format!("{area}.md#{} is indexed {n} times", s.anchor)),
            }
        }
    }
    faults
}

/// The note's claim lines, `- **pN** · source · locator · "quote"`: id to (source, locator, quote).
fn claims(note: &str) -> Result<BTreeMap<String, (String, String, String)>, String> {
    let mut out = BTreeMap::new();
    for line in note.lines().filter(|l| l.starts_with("- **p")) {
        let parts: Vec<&str> = line.splitn(4, " · ").collect();
        let [id, source, locator, rest] = parts.as_slice() else {
            return Err(format!("a claim line without four parts: {line}"));
        };
        let id = id.trim_start_matches("- **").trim_end_matches("**");
        let quote = rest
            .split_once('"')
            .and_then(|(_, after)| after.rsplit_once('"'))
            .map_or("", |(q, _)| q);
        if quote.is_empty() {
            return Err(format!("{id} has no quote"));
        }
        let claim = (
            source.trim().to_owned(),
            locator.trim().to_owned(),
            quote.to_owned(),
        );
        if out.insert(id.to_owned(), claim).is_some() {
            return Err(format!("{id} is defined twice"));
        }
    }
    Ok(out)
}

/// The note's sources table: key to (locator, grade).
fn sources(note: &str) -> BTreeMap<String, (String, String)> {
    section(note, "## Sources")
        .lines()
        .filter(|l| l.starts_with("| `"))
        .map(|l| {
            let cells: Vec<&str> = l.split('|').map(str::trim).collect();
            let cell = |i: usize| cells.get(i).copied().unwrap_or("").to_owned();
            (cell(1).trim_matches('`').to_owned(), (cell(3), cell(5)))
        })
        .collect()
}

/// The claim ids a text cites, as `[pN]`.
fn cites(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some((_, after)) = rest.split_once("[p") {
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty() && after[digits.len()..].starts_with(']') {
            out.push(format!("p{digits}"));
        }
        rest = after;
    }
    out
}

/// What a text states: citations, links' targets and code spans taken out.
fn stated(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    loop {
        let cut = [rest.find("[p"), rest.find('`'), rest.find("](")]
            .into_iter()
            .flatten()
            .min();
        let Some(at) = cut else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..at]);
        let from = &rest[at..];
        let close = if from.starts_with('`') {
            '`'
        } else if from.starts_with("](") {
            ')'
        } else {
            ']'
        };
        rest = from[1..].split_once(close).map_or("", |(_, after)| after);
    }
    out
}

/// The numbers a text states. Digits that are part of a name (`CP-T1.2`, `p99`, `x86_64`, `§5`) and
/// dates are not numbers here.
fn numbers(text: &str) -> Vec<String> {
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
        let named = p.is_alphanumeric()
            || p == '_'
            || p == '§'
            || ((p == '.' || p == '-') && pp.is_alphanumeric());
        let mut j = i;
        while at(j).is_ascii_digit() || (matches!(at(j), '.' | ',') && at(j + 1).is_ascii_digit()) {
            j += 1;
        }
        let date = j - i == 4 && at(j) == '-' && at(j + 1).is_ascii_digit();
        if named || date {
            while j < c.len() && (c[j].is_alphanumeric() || matches!(c[j], '.' | '_' | '-')) {
                j += 1;
            }
        } else {
            out.push(c[i..j].iter().filter(|ch| **ch != ',').collect());
        }
        i = j;
    }
    out
}

/// A text's paragraphs, list items and table rows (headings, separator rows and blank lines out).
fn units(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let item = line.trim_start().starts_with("- ");
        let breaks =
            line.starts_with('#') || line.trim().is_empty() || line.starts_with('|') || item;
        if breaks && let Some(t) = current.take() {
            out.push(t);
        }
        if line.starts_with('|') {
            if !line.starts_with("|---") {
                out.push(line.to_owned());
            }
        } else if item || (!breaks && current.is_none()) {
            current = Some(line.to_owned());
        } else if !breaks && let Some(t) = current.as_mut() {
            t.push('\n');
            t.push_str(line);
        }
    }
    out.extend(current);
    out
}

/// Every way the client protocol's research fails its sources, in words.
fn protocol_faults(index: &str, record: &str, note: &str) -> Vec<String> {
    let mut faults = Vec::new();
    let claims = match claims(note) {
        Ok(c) => c,
        Err(e) => return vec![e],
    };
    let sources = sources(note);
    for (id, (source, locator, _)) in &claims {
        if !sources.contains_key(source) {
            faults.push(format!("{id} names source {source}, not in the table"));
        }
        if locator.is_empty() {
            faults.push(format!("{id} has no locator"));
        }
    }
    for (key, (locator, grade)) in &sources {
        if locator.is_empty() || !matches!(grade.as_str(), "A" | "B" | "C" | "D" | "E" | "F") {
            faults.push(format!("source {key} lacks a locator or a grade A to F"));
        }
        if !claims.values().any(|(s, _, _)| s == key) {
            faults.push(format!("source {key} backs no claim"));
        }
    }
    let comparison = section(index, COMPARISON);
    let cited: BTreeSet<String> = cites(index).into_iter().chain(cites(record)).collect();
    for id in cited.iter().filter(|id| !claims.contains_key(*id)) {
        faults.push(format!("{id} is cited but has no claim line"));
    }
    for id in claims.keys().filter(|id| !cited.contains(*id)) {
        faults.push(format!("{id} is in the note but cited nowhere"));
    }
    let header = |u: &str| u.starts_with("| |");
    for unit in units(comparison).iter().filter(|u| !header(u)) {
        if cites(unit).is_empty() {
            faults.push(format!(
                "uncited in the comparison: {}",
                unit.chars().take(70).collect::<String>()
            ));
        }
    }
    // Numbers are held to the quotes their own cell (in a row) or paragraph cites.
    for unit in units(comparison).iter().chain(units(record).iter()) {
        let pieces: Vec<&str> = if unit.starts_with('|') {
            unit.split('|').collect()
        } else {
            vec![unit.as_str()]
        };
        for piece in pieces {
            let held: BTreeSet<String> = cites(piece)
                .iter()
                .filter_map(|id| claims.get(id))
                .flat_map(|(_, _, quote)| numbers(quote))
                .collect();
            for n in numbers(&stated(piece))
                .iter()
                .filter(|n| !held.contains(*n))
            {
                faults.push(format!(
                    "{n} is in no quote cited by: {}",
                    piece.trim().chars().take(70).collect::<String>()
                ));
            }
        }
    }
    faults
}

#[test]
fn ac_t1_2_3_every_decision_has_one_index_line_and_every_line_a_decision() {
    let records = records();
    let faults = index_faults(INDEX, &records);
    assert!(faults.is_empty(), "{}", faults.join("\n"));
    // Non-vacuity: the check saw the records' decisions and the index's lines.
    let decisions: usize = records.values().map(|t| sections(t).len()).sum();
    assert!(decisions >= 100, "{decisions} sections in the records");
    assert!(links(section(INDEX, "## The decisions")).len() >= 100);
    assert!(records.contains_key(PROTOCOL_RECORD));
}

#[test]
fn ac_t1_2_3_a_decision_without_a_line_is_a_fault() {
    let mut records = records();
    let semantics = records.get_mut("semantics").unwrap();
    semantics.push_str("\n## 3. A new decision\n\nIts words.\n");
    let faults = index_faults(INDEX, &records);
    assert!(
        faults.contains(&"semantics.md#3-a-new-decision has no line in the index".to_owned()),
        "{faults:?}"
    );
    // A new record without sections of its own is one decision.
    records.insert(
        "new-area".to_owned(),
        "# A new area\n\nIts words.\n".to_owned(),
    );
    let faults = index_faults(INDEX, &records);
    assert!(
        faults.contains(&"new-area.md has no line in the index".to_owned()),
        "{faults:?}"
    );
}

#[test]
fn ac_t1_2_3_adding_a_decision_is_one_line() {
    let mut records = records();
    let semantics = records.get_mut("semantics").unwrap();
    semantics.push_str("\n## 3. A new decision\n\nIts words.\n");
    let line =
        "| semantics | 3 | A new decision | [semantics.md](design/semantics.md#3-a-new-decision) |";
    // The one line, after the table's first row.
    let first = "| Area | No. | Decision | Record |\n|---|---|---|---|\n";
    let fixed = INDEX.replacen(first, &format!("{first}{line}\n"), 1);
    assert_ne!(fixed, INDEX);
    let faults = index_faults(&fixed, &records);
    assert!(faults.is_empty(), "{}", faults.join("\n"));
}

#[test]
fn ac_t1_2_3_a_line_that_points_nowhere_is_a_fault() {
    let records = records();
    let gone = INDEX.replacen(
        "design/semantics.md#1-text-is-well-formed-utf-8",
        "design/semantics.md#1-text-is-anything",
        1,
    );
    assert_ne!(gone, INDEX);
    let faults = index_faults(&gone, &records);
    assert!(
        faults.contains(&"design/semantics.md#1-text-is-anything is no section of it".to_owned()),
        "{faults:?}"
    );
    assert!(
        faults.contains(
            &"semantics.md#1-text-is-well-formed-utf-8 has no line in the index".to_owned()
        ),
        "{faults:?}"
    );
    let no_record = INDEX.replacen("](design/semantics.md#1-", "](design/semantic.md#1-", 1);
    let faults = index_faults(&no_record, &records);
    assert!(
        faults.contains(&"design/semantic.md is no record".to_owned()),
        "{faults:?}"
    );
    let renumbered = INDEX.replacen("| semantics | 1 |", "| semantics | 7 |", 1);
    assert_ne!(renumbered, INDEX);
    let faults = index_faults(&renumbered, &records);
    assert!(
        faults.contains(
            &"semantics.md#1-text-is-well-formed-utf-8 is number 1, its line says 7".to_owned()
        ),
        "{faults:?}"
    );
}

#[test]
fn ac_t1_2_3_a_section_indexed_twice_is_a_fault() {
    let line =
        "- semantics: TEXT ([semantics.md](design/semantics.md#1-text-is-well-formed-utf-8))";
    let twice = format!("{INDEX}\n{line}\n");
    let faults = index_faults(&twice, &records());
    assert!(
        faults.contains(&"semantics.md#1-text-is-well-formed-utf-8 is indexed 2 times".to_owned()),
        "{faults:?}"
    );
}

#[test]
fn ac_t1_2_3_anchors_are_githubs() {
    assert_eq!(
        slug("2. `IN (SELECT ...)` answers a row"),
        "2-in-select--answers-a-row"
    );
    assert_eq!(
        slug("The owner's time (2026-10-31)"),
        "the-owners-time-2026-10-31"
    );
    let found = sections("# T\n## A\n### A\n```\n## not a heading\n```\n## A\n## 4. B\n");
    let anchors: Vec<&str> = found.iter().map(|s| s.anchor.as_str()).collect();
    assert_eq!(anchors, ["a", "a-2", "4-b"]);
    assert_eq!(found[2].number, "4");
    assert_eq!(found[0].number, "–");
}

fn protocol_record() -> String {
    records().remove(PROTOCOL_RECORD).unwrap()
}

#[test]
fn ac_t1_2_3_the_client_protocol_is_held_to_its_sources() {
    let record = protocol_record();
    let faults = protocol_faults(INDEX, &record, NOTE);
    assert!(faults.is_empty(), "{}", faults.join("\n"));
    // Non-vacuity: the comparison's rows and the note's claims were seen.
    let rows = units(section(INDEX, COMPARISON))
        .iter()
        .filter(|u| u.starts_with('|'))
        .count();
    assert!(rows >= 7, "the comparison has {rows} rows");
    assert!(claims(NOTE).unwrap().len() >= 25);
}

#[test]
fn ac_t1_2_3_an_uncited_row_number_or_claim_is_a_fault() {
    let record = protocol_record();
    // 16 is MySQL's packet bound [p8]; 17 is in no quote.
    let changed = INDEX.replacen("at most 16 MByte", "at most 17 MByte", 1);
    assert_ne!(changed, INDEX);
    let f = protocol_faults(&changed, &record, NOTE);
    assert!(
        f.iter().any(|f| f.starts_with("17 is in no quote")),
        "{f:?}"
    );
    // A row of the comparison that cites nothing.
    let changed = INDEX.replacen(
        "\n\n## The decisions",
        "\n| Uncited | a | b | c | d | e |\n\n## The decisions",
        1,
    );
    let f = protocol_faults(&changed, &record, NOTE);
    assert!(
        f.iter()
            .any(|f| f.starts_with("uncited in the comparison: | Uncited")),
        "{f:?}"
    );
    // A claim nobody cites, and a citation without a claim.
    let changed = INDEX.replace("[p20]", "[p999]");
    let f = protocol_faults(&changed, &record, NOTE);
    assert!(
        f.contains(&"p20 is in the note but cited nowhere".to_owned()),
        "{f:?}"
    );
    assert!(
        f.contains(&"p999 is cited but has no claim line".to_owned()),
        "{f:?}"
    );
    // A source without a grade.
    let ungraded = NOTE.replacen(
        "| specification | A | Framing",
        "| specification | ? | Framing",
        1,
    );
    assert_ne!(ungraded, NOTE);
    let f = protocol_faults(INDEX, &record, &ungraded);
    assert!(
        f.iter().any(|f| f.contains("lacks a locator or a grade")),
        "{f:?}"
    );
}

#[test]
fn ac_t1_2_3_numbers_skip_names_and_dates() {
    assert_eq!(
        numbers("CP-T1.2 on 2026-11-21, x86_64 p99 §5"),
        Vec::<String>::new()
    );
    assert_eq!(
        numbers("at most 16 MByte, 2,197 and 3.5"),
        ["16", "2197", "3.5"]
    );
}
