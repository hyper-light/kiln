//! GitHub's own expression test vectors (docs/ci.md W01-AC01), run against `kiln_ci::expr`.
//!
//! Each case parses with exactly the contexts the case defines (and no extension functions), as
//! GitHub's harness does (`xlang.test.ts`), then evaluates; an expected error must contain
//! GitHub's message, an expected result must have the same kind and the same JSON.

use std::path::Path;

use kiln_ci::expr::{Budget, Dialect, Expr, MapContexts, ParseOptions, Value, eval, json_parse, json_stringify};

fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(o) => o.get(key),
        _ => None,
    }
}

fn text(v: Option<&Value>) -> String {
    v.map(Value::coerce_string).unwrap_or_default()
}

#[test]
fn w01_ac01_github_expression_vectors() -> Result<(), String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/oracle/testdata");
    let mut files: Vec<_> = std::fs::read_dir(&dir).map_err(|e| e.to_string())?.filter_map(Result::ok).map(|e| e.path()).collect();
    files.sort();
    let (mut passed, mut failures) = (0usize, Vec::new());
    for file in files {
        let name = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let doc = json_parse(&std::fs::read_to_string(&file).map_err(|e| e.to_string())?).map_err(|e| format!("{name}: {e}"))?;
        let Value::Object(groups) = &doc else { return Err(format!("{name}: not an object")) };
        for (group, cases) in &groups.0 {
            for case in cases.items().unwrap_or_default() {
                let expr = text(get(case, "expr"));
                let label = format!("{name}/{group}: {expr:?}");
                match run(case, &expr) {
                    Ok(()) => passed += 1,
                    Err(why) => failures.push(format!("{label}: {why}")),
                }
            }
        }
    }
    eprintln!("oracle: {passed} passed, {} failed", failures.len());
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("{} of {} vectors failed:\n{}", failures.len(), passed + failures.len(), failures.join("\n")))
    }
}

fn run(case: &Value, expr: &str) -> Result<(), String> {
    let contexts = get(case, "contexts").cloned().unwrap_or(Value::object(Vec::new()));
    let names: Vec<String> = match &contexts {
        Value::Object(o) => o.0.iter().map(|(k, _)| k.to_string()).collect(),
        _ => Vec::new(),
    };
    let expected_err = get(case, "err");
    let err_kind = text(expected_err.and_then(|e| get(e, "kind")));
    let err_text = text(expected_err.and_then(|e| get(e, "value")));
    let opts = ParseOptions { dialect: Dialect::Gha, contexts: Some(names), status_functions: false, hash_files: false };
    let parsed = Expr::parse_with(expr, &opts);
    let e = match parsed {
        Ok(e) => e,
        Err(pe) => {
            let got = pe.located(expr);
            if (err_kind == "lexing" || err_kind == "parsing") && got.contains(&err_text) {
                return Ok(());
            }
            return Err(format!("parse error {got:?}, expected {err_kind} {err_text:?}"));
        }
    };
    if err_kind == "lexing" || err_kind == "parsing" {
        return Err(format!("expected a {err_kind} error {err_text:?}, but it parsed"));
    }
    let mut cx = MapContexts::default();
    if let Value::Object(o) = &contexts {
        for (k, v) in &o.0 {
            cx = cx.with(k, v.clone());
        }
    }
    match eval(&e, &cx, &mut Budget::default()) {
        Ok(v) => {
            if err_kind == "evaluation" {
                return Err(format!("expected evaluation error {err_text:?}, got {}", json_stringify(&v, 0)));
            }
            let want = get(case, "result");
            let kind = text(want.and_then(|w| get(w, "kind")));
            let want_json = want.and_then(|w| get(w, "value")).map(|w| json_stringify(w, 0)).unwrap_or_else(|| "null".into());
            let got_json = json_stringify(&v, 0);
            if v.kind().name() != kind || got_json != want_json {
                return Err(format!("got {} {got_json}, expected {kind} {want_json}", v.kind().name()));
            }
            Ok(())
        }
        Err(ee) => {
            let got = ee.located(expr);
            if err_kind == "evaluation" && got.contains(&err_text) {
                return Ok(());
            }
            Err(format!("evaluation error {got:?}, expected {:?}", get(case, "result").map(|r| json_stringify(r, 0))))
        }
    }
}
