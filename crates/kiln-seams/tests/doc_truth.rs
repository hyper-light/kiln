//! docs/seams.md states each seam's contract: this test holds it to the code (CP-M2, AC-M2.5). For
//! every seam, the doc's "Methods:" are exactly its trait's methods (read from its source), as are
//! its "Methods of <trait>:" for the traits its trait hands out (a sandbox's `Run`); its clauses are
//! exactly its conformance cases: the same ids, in the same order, in the same words; and each bound
//! its simulator and its trait keep is said as the code has it. A section for a seam that isn't
//! one, or a seam without a section, fails it too.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use kiln_seams::conformance::Case;

/// One seam: its section's title, its module's file, its trait and the traits it hands out, its
/// cases (id, words), and its bounds as the doc says them (built from the code's numbers).
struct Seam {
    title: &'static str,
    file: &'static str,
    tr: &'static str,
    more: &'static [&'static str],
    cases: Vec<(&'static str, &'static str)>,
    bounds: Vec<String>,
}

/// A count as the doc says it: 1000, 10,000, a million.
fn count(n: u64) -> String {
    if n == 1_000_000 {
        return "a million".into();
    }
    let digits = n.to_string();
    if n < 10_000 {
        return digits;
    }
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A size as the doc says it: 64 KiB, 1 MiB, a GiB.
fn size(n: u64) -> String {
    match n {
        n if n == 1 << 30 => "a GiB".into(),
        n if n.is_multiple_of(1 << 30) => format!("{} GiB", n >> 30),
        n if n.is_multiple_of(1 << 20) => format!("{} MiB", n >> 20),
        n if n.is_multiple_of(1 << 10) => format!("{} KiB", n >> 10),
        n => format!("{n} bytes"),
    }
}

/// Days as the doc says them: a day, 7 days.
fn days(d: Duration) -> String {
    match d.as_secs() / 86_400 {
        1 => "a day".into(),
        n => format!("{n} days"),
    }
}

fn said<F>(cases: &[Case<F>]) -> Vec<(&'static str, &'static str)> {
    cases.iter().map(|c| (c.id, c.says)).collect()
}

fn seams() -> Vec<Seam> {
    use kiln_seams::*;
    let records = records::SimLimits::default();
    let blobs = blobs::SimLimits::default();
    let weights = sandbox::Limits::default();
    vec![
        Seam {
            title: "Clock",
            file: "clock.rs",
            tr: "Clock",
            more: &[],
            cases: said(&clock::cases::<clock::SimClockFixture>()),
            bounds: vec![format!("at most {} of them waiting", count(clock::SIM_SLEEPS_MAX as u64))],
        },
        Seam {
            title: "Spawn",
            file: "spawn.rs",
            tr: "Spawn",
            more: &[],
            cases: said(&spawn::cases::<spawn::SimSpawnFixture>()),
            bounds: vec![format!("at most {} polls a run", count(spawn::SIM_POLLS)), format!("at most {} tasks held", count(spawn::SIM_TASKS as u64))],
        },
        Seam {
            title: "Records",
            file: "records.rs",
            tr: "Records",
            more: &[],
            cases: said(&records::cases::<records::SimRecordsFixture>()),
            bounds: vec![
                format!("each key's last {} versions", count(records.versions as u64)),
                format!("the last {} deleted keys' histories", count(records.deleted as u64)),
                format!("a change feed of the last {} changes", count(records.changes as u64)),
                format!("{} rows", count(records.rows as u64)),
                format!("family names 1 to {}", records::FAMILY_MAX),
                format!("keys 1 to {} bytes", records::KEY_MAX),
                format!("values to {}", size(records::VALUE_MAX as u64)),
                format!("{} conditions and {} writes a transaction", count(records::TX_MAX as u64), count(records::TX_MAX as u64)),
                format!("{} rows a page or a change read", count(records::SCAN_MAX as u64)),
            ],
        },
        Seam {
            title: "Bus",
            file: "bus.rs",
            tr: "Bus",
            more: &[],
            cases: said(&bus::cases::<bus::SimBusFixture>()),
            bounds: vec![
                format!("{} topics", count(bus::SIM_TOPICS as u64)),
                format!("topic names 1 to {}", bus::NAME_MAX),
                format!("payloads to {}, keys to {} bytes", size(bus::PAYLOAD_MAX as u64), bus::KEY_MAX),
                format!("{} messages a read", count(bus::READ_MAX as u64)),
                format!("a topic retains 1 to {} messages", count(bus::RETAIN_MAX as u64)),
            ],
        },
        Seam {
            title: "Blobs",
            file: "blobs.rs",
            tr: "Blobs",
            more: &[],
            cases: said(&blobs::cases::<blobs::SimBlobsFixture>()),
            bounds: vec![
                format!("objects to {}, {} of them, {} in all", size(blobs.object), count(blobs.objects as u64), size(blobs.total)),
                format!("{} uploads under way", count(blobs.uploads as u64)),
                format!("keys 1 to {} bytes", blobs::KEY_MAX),
                format!("{} keys a listing", count(blobs::LIST_MAX as u64)),
                format!("part numbers 1 to {}", count(u64::from(blobs::PARTS_MAX))),
                format!("every part but the last at least {}", size(blobs::PART_MIN as u64)),
                format!("presigned URLs for up to {}", days(blobs::PRESIGN_MAX)),
            ],
        },
        Seam {
            title: "Secrets",
            file: "secrets.rs",
            tr: "Secrets",
            more: &[],
            cases: said(&secrets::cases::<secrets::SimSecretsFixture>()),
            bounds: vec![
                format!("{} secrets and leases", count(secrets::SIM_SECRETS as u64)),
                format!("names 1 to {}", secrets::NAME_MAX),
                format!("values to {}", size(secrets::VALUE_MAX as u64)),
                format!("{} names a listing", count(secrets::LIST_MAX as u64)),
                format!("{} versions kept a secret", count(secrets::VERSIONS_MAX as u64)),
                format!("deletes {} to {} out", secrets::WINDOW_MIN.as_secs() / 86_400, days(secrets::WINDOW_MAX)),
                format!("leases up to {}", days(secrets::LEASE_MAX)),
            ],
        },
        Seam {
            title: "Artifacts",
            file: "artifacts.rs",
            tr: "Artifacts",
            more: &[],
            cases: said(&artifacts::cases::<artifacts::SimArtifactsFixture>()),
            bounds: vec![
                format!("{} repositories, and {} blobs, manifests and tags in each, {} in all", count(artifacts::SIM_ITEMS as u64), count(artifacts::SIM_ITEMS as u64), size(artifacts::SIM_BYTES)),
                format!("manifests to {}", size(artifacts::MANIFEST_MAX as u64)),
                format!("names to {} bytes", artifacts::NAME_MAX),
                format!("{} tags a listing", count(artifacts::TAGS_MAX as u64)),
            ],
        },
        Seam {
            title: "Sandbox",
            file: "sandbox.rs",
            tr: "Sandbox",
            more: &["Run"],
            cases: said(&sandbox::cases::<sandbox::SimSandboxFixture>()),
            bounds: vec![
                format!("{} runs at once, {} outputs unread a run", count(sandbox::SIM_RUNS as u64), count(sandbox::SIM_PENDING as u64)),
                format!("weights ({} to {}, {} an even share", sandbox::WEIGHT_MIN, count(u64::from(sandbox::WEIGHT_MAX)), weights.cpu_weight),
            ],
        },
        Seam {
            title: "Placement",
            file: "placement.rs",
            tr: "Placement",
            more: &[],
            cases: said(&placement::cases::<placement::SimPlacementFixture>()),
            bounds: vec![format!("{} placements", count(placement::SIM_PLACED as u64))],
        },
        Seam {
            title: "HarnessPort",
            file: "harness.rs",
            tr: "HarnessPort",
            more: &[],
            cases: said(&harness::cases::<harness::SimHarnessFixture>()),
            bounds: vec![format!(
                "{} agents, what the {} closed last were sent, and {} commands an agent",
                count(harness::SIM_AGENTS as u64),
                count(harness::SIM_AGENTS as u64),
                count(harness::SIM_INBOX as u64)
            )],
        },
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
    methods_after(lines, "Methods:")
}

/// The names in the line that starts with `head`, after it.
fn methods_after(lines: &[String], head: &str) -> Option<BTreeSet<String>> {
    let rest = lines.iter().find_map(|l| l.strip_prefix(head))?;
    Some(rest.split('`').skip(1).step_by(2).map(str::to_owned).collect())
}

/// A section's prose as one line: its words, one space apart (the doc wraps its lines anywhere).
fn prose(lines: &[String]) -> String {
    lines.iter().flat_map(|l| l.split_whitespace()).collect::<Vec<_>>().join(" ")
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

/// CP-M2, AC-M2.5: docs/seams.md, held to the code.
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
        for tr in s.more {
            let code = trait_methods(&source, tr).ok_or_else(|| format!("{}: no `pub trait {tr}` in {}", s.title, s.file))?;
            match methods_after(lines, &format!("Methods of {tr}:")) {
                None => wrong.push(format!("{}: no \"Methods of {tr}:\" line", s.title)),
                Some(said) if said != code => wrong.push(format!("{}: the doc says {tr}'s methods are {said:?}, the trait has {code:?}", s.title)),
                Some(_) => {}
            }
        }
        let text = prose(lines);
        for bound in &s.bounds {
            if !text.contains(bound.as_str()) {
                wrong.push(format!("{}: the doc doesn't say {bound:?}, as the code has it", s.title));
            }
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

/// The checks themselves: a clause reworded on one side, or a method left out, is caught
/// (AC-M2.5).
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

/// A bound said otherwise than the code has it, or a handed-out trait's method left out, is caught
/// (AC-M2.5).
#[test]
fn a_bound_said_wrong_or_a_handed_out_trait_s_method_left_out_is_caught() {
    assert_eq!([count(1000), count(10_000), count(1 << 20), count(1_000_000)], ["1000", "10,000", "1,048,576", "a million"]);
    assert_eq!([size(64 << 10), size(1 << 20), size(1 << 30), size(1024 + 1)], ["64 KiB", "1 MiB", "a GiB", "1025 bytes"]);
    assert_eq!([days(Duration::from_secs(86_400)), days(Duration::from_secs(7 * 86_400))], ["a day", "7 days"]);
    let lines: Vec<String> = ["at most 100,000", "  agents, wrapped.", "Methods of Run: `stdin`, `kill`."].iter().map(|l| (*l).to_owned()).collect();
    assert!(prose(&lines).contains("at most 100,000 agents"));
    assert!(!prose(&lines).contains(&format!("at most {} agents", count(10_000))), "a bound changed in the code shows");
    assert_eq!(methods_after(&lines, "Methods of Run:"), Some(BTreeSet::from(["stdin".to_owned(), "kill".to_owned()])));
    assert_eq!(methods_after(&lines, "Methods of Lease:"), None);
}
