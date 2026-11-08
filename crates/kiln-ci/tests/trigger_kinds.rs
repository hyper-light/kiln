//! docs/ci.md W29-R06: each of Add a trigger's 18 kinds, as Ship's Pipelines writes its first lines
//! (`ui/tests/fixtures/trigger-kinds.json`, which `ui/tests/workflow-doc-triggers.test.ts` holds to
//! `ui/src/lib/triggers.ts`), reads with no errors and as its own events: in a kiln workflow, in a
//! GitHub one, and kiln's own kinds in a GitHub workflow's overlay (W02-R07).

use std::path::Path;

use kiln_ci::expr::Dialect;
use kiln_ci::wf::{apply_overlay, check_model, parse_overlay, parse_workflow};
use serde_json::Value;

#[test]
fn w29_r06_each_kinds_first_lines_read_clean() -> Result<(), String> {
    let at = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/tests/fixtures/trigger-kinds.json");
    let text = std::fs::read_to_string(&at).map_err(|e| format!("{}: {e}", at.display()))?;
    let kinds: Vec<Value> = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let field = |k: &Value, f: &str| k.get(f).and_then(Value::as_str).unwrap_or_default().to_owned();
    let mut failures = Vec::new();
    for k in &kinds {
        let (kind, path, src) = (field(k, "kind"), field(k, "path"), field(k, "text"));
        let events: Vec<String> = k.get("events").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
        let (diagnostics, on) = if path.starts_with(".kiln/overlays/") {
            // An overlay, read and applied to a GitHub workflow as a run applies it.
            let (ov, mut d) = parse_overlay(&src);
            let (wf, _) = parse_workflow(".github/workflows/x.yml", "on: push\njobs:\n  a:\n    runs-on: x\n    steps: [{run: echo}]\n", Dialect::Gha);
            let mut on = Vec::new();
            if let (Some(mut wf), Some(ov)) = (wf, ov) {
                d.extend(apply_overlay(&mut wf, &ov));
                on = wf.on.iter().map(|t| t.event.clone()).collect();
            }
            (d, on)
        } else {
            let (checked, wf) = check_model(&path, &src, None);
            (checked.diagnostics, wf.map(|w| w.on.iter().map(|t| t.event.clone()).collect()).unwrap_or_default())
        };
        let errors: Vec<String> = diagnostics.iter().filter(|d| d.is_error()).map(|d| d.render(&path)).collect();
        if !errors.is_empty() {
            failures.push(format!("{kind} in {path}:\n{}\n{src}", errors.join("\n")));
        }
        for e in &events {
            if !on.contains(e) {
                failures.push(format!("{kind} in {path} doesn't read as `{e}`: {on:?}\n{src}"));
            }
        }
    }
    assert_eq!(kinds.len(), 36, "18 kinds, each in kiln's dialect and in GitHub's");
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    Ok(())
}
