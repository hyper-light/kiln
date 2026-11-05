//! W29-R27's erasure against Node 24's own type stripping (`tests/fixtures/erase`): the same bytes
//! where Node erases, `KCI1033` where it refuses, at the places each refused case names.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::*;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/erase")
}

#[derive(serde::Deserialize)]
struct Recorded {
    node: String,
    cases: BTreeMap<String, Case>,
}

#[derive(serde::Deserialize)]
struct Case {
    js: Option<String>,
    refused: Option<String>,
}

/// The places a refused case's first line names: `// refused at 2:1, 3:8`.
fn named(ts: &str) -> Vec<(u32, u32)> {
    let Some(list) = ts.lines().next().and_then(|l| l.strip_prefix("// refused at ")) else { return Vec::new() };
    list.split(", ").filter_map(|p| p.split_once(':')).filter_map(|(l, c)| Some((l.parse().ok()?, c.parse().ok()?))).collect()
}

#[test]
fn w29_r27_erasure_is_node_24s_byte_for_byte() -> anyhow::Result<()> {
    let recorded: Recorded = serde_json::from_str(&std::fs::read_to_string(fixtures().join("node.json"))?)?;
    assert!(recorded.node.starts_with("v24."), "recorded with Node {}", recorded.node);
    assert!(recorded.cases.len() >= 40, "{} cases", recorded.cases.len());
    let mut wrong = Vec::new();
    for (name, case) in &recorded.cases {
        let ts = std::fs::read_to_string(fixtures().join("cases").join(name))?;
        match (&case.js, &case.refused, erase(&ts)) {
            (Some(js), _, Ok(ours)) if *js == ours => {}
            (None, Some(code), Err(diags)) if code == "ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX" => {
                let at: Vec<(u32, u32)> = diags.iter().map(|d| (d.range.start.line, d.range.start.col)).collect();
                if diags.iter().any(|d| d.code != "KCI1033") || at != named(&ts) || at.is_empty() {
                    wrong.push(format!("{name}: refused at {at:?}, the case names {:?}: {diags:?}", named(&ts)));
                }
            }
            (js, refused, ours) => wrong.push(format!("{name}:\n  node {js:?} {refused:?}\n  kiln {ours:?}")),
        }
    }
    assert!(wrong.is_empty(), "{} of {} cases differ from Node's:\n{}", wrong.len(), recorded.cases.len(), wrong.join("\n"));
    Ok(())
}

#[test]
fn w29_r27_erasure_keeps_every_offset() -> anyhow::Result<()> {
    for entry in std::fs::read_dir(fixtures().join("cases"))? {
        let ts = std::fs::read_to_string(entry?.path())?;
        if let Ok(js) = erase(&ts) {
            assert_eq!(js.len(), ts.len(), "{ts:?}");
            assert_eq!(js.lines().count(), ts.lines().count(), "{ts:?}");
            assert_eq!(js.encode_utf16().count(), ts.encode_utf16().count(), "{ts:?}");
        }
    }
    Ok(())
}

#[test]
fn w29_r27_what_the_grammar_cant_read_is_left_for_quickjs() {
    let ts = "const a: number = 1;\nconst b = )(;\n";
    let js = erase(ts).unwrap_or_default();
    assert!(js.contains(")(;"), "{js:?}");
}

#[test]
fn w29_r27_a_template_after_an_erased_statement_starts_its_own() {
    // Node 24 adds no `;` before a template holding `${`, so its JavaScript calls `b` as the
    // template's tag; kiln's keeps the two statements apart.
    assert_eq!(erase("let a = b\ninterface I {}\n`${f()}`\n").ok().as_deref(), Some("let a = b\n;             \n`${f()}`\n"));
}

#[test]
fn w29_r27_a_type_the_grammar_misreads_is_left_for_quickjs() {
    let ts = "interface I { a: = }\nconst b: number = 1;\n";
    assert_eq!(erase(ts).ok().as_deref(), Some("interface I { a: = }\nconst b         = 1;\n"));
}

#[test]
fn w29_r27_imports_are_found_where_they_are_named() {
    let ts = "import { a } from \"@kiln/sdk\";\nimport \"node:fs\";\nexport * from \"x\";\nconst m = await import(\"z\");\nconst n = import(name);\nexport default { on: \"push\" };\n";
    let all = imports(ts);
    let found: Vec<(Option<&str>, u32, u32)> = all.iter().map(|(n, r)| (n.as_deref(), r.start.line, r.start.col)).collect();
    assert_eq!(found, vec![(Some("@kiln/sdk"), 1, 1), (Some("node:fs"), 2, 1), (Some("x"), 3, 1), (Some("z"), 4, 17), (None, 5, 11)]);
    assert_eq!(default_export(ts).map(|p| (p.line, p.col)), Some((6, 1)));
    assert_eq!(default_export("const a = 1;\n"), None);
}

#[test]
fn w29_r27_what_a_program_makes_reads_as_yaml_in_its_order() {
    let gha = r#"{"name":"ci","on":{"push":{"branches":["main"]}},"jobs":{"b":{"runs-on":"ubuntu-latest","steps":[{"run":"make"}]}}}"#;
    let (yaml, github) = emitted(gha).unwrap_or_default();
    let top: Vec<&str> = yaml.lines().filter(|l| !l.starts_with([' ', '-'])).filter_map(|l| l.split(':').next()).map(|k| k.trim_matches('"')).collect();
    assert_eq!(top, vec!["name", "on", "jobs"], "{yaml}");
    assert!(github, "{yaml}");
    let kiln = r#"{"name":"ci","on":"diff","jobs":{"b":{"runs-on":"linux","steps":[{"run":"make"}]}}}"#;
    let (_, github) = emitted(kiln).unwrap_or_default();
    assert!(!github, "kiln's keys aren't GitHub's");
}
