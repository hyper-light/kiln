//! docs/seams.md states each seam's contract: this test holds it to the code (CP-M2). For every
//! seam, the doc's "Methods:" are exactly its trait's methods (read from its source), and its
//! clauses are exactly its conformance cases: the same ids, in the same order, in the same words.
//! A section for a seam that isn't one, or a seam without a section, fails it too.

use std::collections::BTreeSet;
use std::path::PathBuf;

use kiln_seams::conformance::Case;

/// One seam: its section's title, its module's file, its trait, and its cases (id, words).
struct Seam {
    title: &'static str,
    file: &'static str,
    tr: &'static str,
    cases: Vec<(&'static str, &'static str)>,
}

fn said<F>(cases: &[Case<F>]) -> Vec<(&'static str, &'static str)> {
    cases.iter().map(|c| (c.id, c.says)).collect()
}

fn seams() -> Vec<Seam> {
    use kiln_seams::*;
    vec![
        Seam { title: "Clock", file: "clock.rs", tr: "Clock", cases: said(&clock::cases::<clock::SimClockFixture>()) },
        Seam { title: "Spawn", file: "spawn.rs", tr: "Spawn", cases: said(&spawn::cases::<spawn::SimSpawnFixture>()) },
        Seam { title: "Records", file: "records.rs", tr: "Records", cases: said(&records::cases::<records::SimRecordsFixture>()) },
        Seam { title: "Bus", file: "bus.rs", tr: "Bus", cases: said(&bus::cases::<bus::SimBusFixture>()) },
        Seam { title: "Blobs", file: "blobs.rs", tr: "Blobs", cases: said(&blobs::cases::<blobs::SimBlobsFixture>()) },
        Seam { title: "Secrets", file: "secrets.rs", tr: "Secrets", cases: said(&secrets::cases::<secrets::SimSecretsFixture>()) },
        Seam { title: "Artifacts", file: "artifacts.rs", tr: "Artifacts", cases: said(&artifacts::cases::<artifacts::SimArtifactsFixture>()) },
        Seam { title: "Sandbox", file: "sandbox.rs", tr: "Sandbox", cases: said(&sandbox::cases::<sandbox::SimSandboxFixture>()) },
        Seam { title: "Placement", file: "placement.rs", tr: "Placement", cases: said(&placement::cases::<placement::SimPlacementFixture>()) },
        Seam { title: "HarnessPort", file: "harness.rs", tr: "HarnessPort", cases: said(&harness::cases::<harness::SimHarnessFixture>()) },
    ]
}

fn here() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The doc's sections: each title with its lines.
fn sections(doc: &str) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for line in doc.lines() {
        if let Some(title) = line.strip_prefix("## ") {
            out.push((title.trim().to_owned(), Vec::new()));
        } else if let Some((_, lines)) = out.last_mut() {
            lines.push(line.to_owned());
        }
    }
    out
}

/// The names in a "Methods: `a`, `b`." line.
fn methods_said(lines: &[String]) -> Option<BTreeSet<String>> {
    let line = lines.iter().find(|l| l.starts_with("Methods:"))?;
    Some(line.split('`').skip(1).step_by(2).map(str::to_owned).collect())
}

/// A clause's id: capitals, a dash, digits (`CLOCK-3`).
fn is_clause_id(id: &str) -> bool {
    id.split_once('-').is_some_and(|(seam, n)| !seam.is_empty() && seam.bytes().all(|b| b.is_ascii_uppercase()) && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The clauses: `- **ID** words` lines, in order (a bold bullet that isn't an id, such as
/// `**Implementations:**`, isn't one).
fn clauses_said(lines: &[String]) -> Vec<(String, String)> {
    lines
        .iter()
        .filter_map(|l| {
            let rest = l.strip_prefix("- **")?;
            let (id, words) = rest.split_once("** ")?;
            is_clause_id(id).then(|| (id.to_owned(), words.trim().to_owned()))
        })
        .collect()
}

/// The methods of `pub trait <name>` in `source`: each `fn` directly in its body.
fn trait_methods(source: &str, name: &str) -> Option<BTreeSet<String>> {
    let head = format!("pub trait {name}");
    let mut lines = source.lines().skip_while(|l| !l.trim_start().starts_with(&head));
    let first = lines.next()?;
    let mut depth: i64 = first.matches('{').count() as i64 - first.matches('}').count() as i64;
    let mut out = BTreeSet::new();
    for line in lines {
        if depth <= 0 {
            break;
        }
        let code = line.split("//").next().unwrap_or_default();
        if depth == 1
            && let Some(rest) = code.trim_start().strip_prefix("fn ")
        {
            let ident: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            out.insert(ident);
        }
        depth += code.matches('{').count() as i64 - code.matches('}').count() as i64;
    }
    Some(out)
}

#[test]
fn docs_seams_md_states_each_contract_as_the_code_has_it() -> Result<(), String> {
    let doc_path = here().join("../../docs/seams.md");
    let doc = std::fs::read_to_string(&doc_path).map_err(|e| format!("{}: {e}", doc_path.display()))?;
    let sections = sections(&doc);
    let seams = seams();
    let mut wrong = Vec::new();
    for s in &seams {
        let Some((_, lines)) = sections.iter().find(|(t, _)| t == s.title) else {
            wrong.push(format!("{}: no section", s.title));
            continue;
        };
        let source_path = here().join("src").join(s.file);
        let source = std::fs::read_to_string(&source_path).map_err(|e| format!("{}: {e}", source_path.display()))?;
        let code_methods = trait_methods(&source, s.tr).ok_or_else(|| format!("{}: no `pub trait {}` in {}", s.title, s.tr, s.file))?;
        match methods_said(lines) {
            None => wrong.push(format!("{}: no Methods: line", s.title)),
            Some(said) if said != code_methods => wrong.push(format!("{}: the doc says the methods are {said:?}, the trait has {code_methods:?}", s.title)),
            Some(_) => {}
        }
        let doc_clauses = clauses_said(lines);
        let code_clauses: Vec<(String, String)> = s.cases.iter().map(|(id, says)| ((*id).to_owned(), (*says).to_owned())).collect();
        if doc_clauses != code_clauses {
            for (i, (d, c)) in doc_clauses.iter().zip(&code_clauses).enumerate() {
                if d != c {
                    wrong.push(format!("{}: clause {}: the doc says {d:?}, the code {c:?}", s.title, i + 1));
                }
            }
            if doc_clauses.len() != code_clauses.len() {
                wrong.push(format!("{}: the doc has {} clauses, the code {}", s.title, doc_clauses.len(), code_clauses.len()));
            }
        }
        if code_clauses.is_empty() {
            wrong.push(format!("{}: no cases", s.title));
        }
    }
    for (title, _) in &sections {
        if !seams.iter().any(|s| s.title == title) {
            wrong.push(format!("a section {title:?} for no seam"));
        }
    }
    if wrong.is_empty() { Ok(()) } else { Err(format!("docs/seams.md and the code part:\n{}", wrong.join("\n"))) }
}

/// The checks themselves: a clause reworded on one side, or a method left out, is caught.
#[test]
fn a_reworded_clause_or_a_missing_method_is_caught() {
    let doc = "## Clock\n\n- **Implementations:** not a clause\n\nMethods: `now`, `sleep`.\n\n- **CLOCK-1** now() never goes back\n";
    let secs = sections(doc);
    let lines = secs.first().map(|(_, l)| l.clone()).unwrap_or_default();
    assert_eq!(methods_said(&lines), Some(BTreeSet::from(["now".to_owned(), "sleep".to_owned()])));
    assert_eq!(clauses_said(&lines), vec![("CLOCK-1".to_owned(), "now() never goes back".to_owned())]);
    let source = "pub trait Clock: Send {\n    fn now(&self) -> u64;\n    /// a { brace } in a comment\n    fn sleep(&self) -> u64 {\n        { 1 }\n    }\n}\nfn outside() {}\n";
    assert_eq!(trait_methods(source, "Clock"), Some(BTreeSet::from(["now".to_owned(), "sleep".to_owned()])));
    assert_ne!(trait_methods(source, "Clock"), Some(BTreeSet::from(["now".to_owned()])), "a method the doc leaves out shows");
}
