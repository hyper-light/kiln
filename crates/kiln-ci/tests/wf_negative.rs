//! docs/ci.md W02-AC02: each file of F-04 (one per diagnostic code) yields its code at the line
//! and column `expect.txt` gives.

use std::path::Path;

use kiln_ci::expr::Dialect;
use kiln_ci::wf::{Diag, apply_overlay, parse_action, parse_overlay, parse_workflow};

fn diags(name: &str, src: &str) -> Vec<Diag> {
    if name.ends_with(".action.yml") {
        return parse_action(src).1;
    }
    if name.ends_with(".overlay.yaml") {
        let (ov, mut d) = parse_overlay(src);
        let (wf, _) = parse_workflow(".github/workflows/x.yml", "on: push\njobs:\n  a:\n    runs-on: x\n    steps: [{run: echo}]\n", Dialect::Gha);
        if let (Some(mut wf), Some(ov)) = (wf, ov) {
            d.extend(apply_overlay(&mut wf, &ov));
        }
        return d;
    }
    let dialect = if name.contains(".kiln.") { Dialect::Kiln } else { Dialect::Gha };
    parse_workflow(&format!(".github/workflows/{name}"), src, dialect).1
}

#[test]
fn w02_ac02_each_diagnostic_at_its_place() -> Result<(), String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/negative");
    let expect = std::fs::read_to_string(dir.join("expect.txt")).map_err(|e| e.to_string())?;
    let mut failures = Vec::new();
    let mut n = 0;
    for line in expect.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#')) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        let [file, code, pos] = parts.as_slice() else { return Err(format!("bad line: {line}")) };
        n += 1;
        let src = std::fs::read_to_string(dir.join(file)).map_err(|e| format!("{file}: {e}"))?;
        let d = diags(file, &src);
        match d.iter().find(|x| x.code == *code) {
            Some(x) => {
                let got = format!("{}:{}", x.range.start.line, x.range.start.col);
                if got != *pos {
                    failures.push(format!("{file}: {code} at {got}, expected {pos} ({})", x.message));
                }
            }
            None => failures.push(format!("{file}: no {code}; got {:?}", d.iter().map(|x| format!("{} {}:{}", x.code, x.range.start.line, x.range.start.col)).collect::<Vec<_>>())),
        }
    }
    let files = std::fs::read_dir(&dir).map_err(|e| e.to_string())?.filter(|e| e.as_ref().is_ok_and(|e| e.file_name().to_string_lossy().starts_with("KCI"))).count();
    assert_eq!(n, files, "every fixture has an expectation");
    if failures.is_empty() { Ok(()) } else { Err(failures.join("\n")) }
}
