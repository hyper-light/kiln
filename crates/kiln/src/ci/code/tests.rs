//! W29-R27 at plan time: each of `KCI1030` to `KCI1033` from its case at the TypeScript's line, the
//! limits, the clock and entropy refused even when caught, the tree read and nothing else, W02's
//! diagnostics at the SDK call that made the part, and one program making the same workflow each
//! time.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use serde_json::{Value, json};

use super::*;

/// A commit's tree, the test's own.
struct Files(BTreeMap<&'static str, &'static str>);

impl Tree for Files {
    fn read(&self, path: &str) -> Option<String> {
        self.0.get(path).map(|s| (*s).to_owned())
    }

    fn list(&self, dir: &str) -> Vec<String> {
        let prefix = format!("{}/", dir.trim_end_matches('/'));
        let mut out: Vec<String> = self.0.keys().filter_map(|p| p.strip_prefix(prefix.as_str())).map(|rest| rest.split_once('/').map_or_else(|| format!("{prefix}{rest}"), |(d, _)| format!("{prefix}{d}/"))).collect();
        out.dedup();
        out
    }
}

fn tree() -> Arc<dyn Tree> {
    Arc::new(Files(BTreeMap::from([("ui/package.json", "{}"), ("ui/src/main.ts", ""), ("ci/gate.sh", "#!/bin/sh\n")])))
}

fn run(ts: &str) -> Result<Made, Vec<Diag>> {
    evaluate(".kiln/ci.ts", ts, &json!({ "ref": "refs/heads/main" }), tree(), limits())
}

/// The codes and lines a program's diagnostics name; `made` when it made a workflow.
fn failed(ts: &str) -> Vec<(&'static str, u32)> {
    match run(ts) {
        Ok(_) => vec![("made", 0)],
        Err(d) => d.iter().map(|d| (d.code, d.range.start.line)).collect(),
    }
}

const SMALL: &str = "jobs: { build: { \"runs-on\": \"linux\", steps: [{ run: \"true\" }] } }";

#[test]
fn w29_r27_kci1030_a_throw_names_its_line() {
    let ts = format!("import {{ workflow }} from \"@kiln/sdk\";\nconst n: number = 1;\nif (n > 0) throw new Error(\"no jobs yet\");\nexport default workflow({{ on: \"push\", {SMALL} }});\n");
    let d = run(&ts).err().unwrap_or_default();
    assert_eq!(d.iter().map(|d| (d.code, d.range.start.line)).collect::<Vec<_>>(), vec![("KCI1030", 3)], "{d:?}");
    assert!(d.iter().all(|d| d.message.contains("no jobs yet") && d.range.start.col > 1), "{d:?}");
    assert_eq!(failed("export default {\n  on: \"push\",,\n};\n"), vec![("KCI1030", 2)], "a syntax error, at its line");
    assert_eq!(failed("const a = 1;\n"), vec![("KCI1030", 1)], "no default export");
    assert_eq!(failed("export default () => undefined;\n"), vec![("KCI1030", 1)], "a function that returns no workflow");
}

#[test]
fn w29_r27_kci1031_each_limit() {
    for ts in ["while (true) {}\nexport default {};\n", "try { while (true) {} } finally { while (true) {} }\nexport default {};\n"] {
        let started = Instant::now();
        assert_eq!(failed(ts), vec![("KCI1031", 1)], "{ts}");
        assert!(started.elapsed().as_millis() <= 1200, "{ts}: {:?}", started.elapsed());
    }
    let memory = run("const a: number[][] = [];\nfor (;;) a.push(new Array(1 << 20).fill(1));\nexport default {};\n").err().unwrap_or_default();
    assert!(memory.iter().any(|d| d.code == "KCI1031" && d.message.contains("64 MiB")), "{memory:?}");
    let deep = run("const f = (n: number): number => f(n + 1) + 1;\nf(0);\nexport default {};\n").err().unwrap_or_default();
    assert!(deep.iter().any(|d| d.code == "KCI1031"), "{deep:?}");
    let output = run(&format!("export default {{ name: \"x\".repeat(5 << 20), on: \"push\", {SMALL} }};\n")).err().unwrap_or_default();
    assert!(output.iter().any(|d| d.code == "KCI1031" && d.message.contains("4 MiB")), "{output:?}");
}

#[test]
fn w29_r27_kci1032_imports_clocks_and_entropy() {
    assert_eq!(failed("import fs from \"node:fs\";\nexport default {};\n"), vec![("KCI1032", 1)]);
    assert_eq!(failed("const m = await import(\"x\");\nexport default {};\n"), vec![("KCI1032", 1)]);
    for (line, what) in [(3, "Date.now()"), (3, "new Date()"), (3, "Date()"), (3, "Math.random()"), (3, "performance.now()"), (3, "new (new Date(0).constructor)()")] {
        let ts = format!("import {{ workflow }} from \"@kiln/sdk\";\nconst a: number = 1;\nconst t = {what};\nexport default workflow({{ on: \"push\", {SMALL} }});\n");
        assert_eq!(failed(&ts), vec![("KCI1032", line)], "{what}");
    }
    // Caught, it still fails the plan: the same commit always plans the same.
    assert_eq!(failed("let t = 0;\ntry { t = Date.now(); } catch {}\nexport default {};\n"), vec![("KCI1032", 2)]);
    assert_eq!(failed("const t = (0, eval)(\"Date.now()\");\nexport default {};\n").first().map(|f| f.0), Some("KCI1032"));
    // A date of its own is no clock.
    assert_eq!(failed(&format!("const d = new Date(0).toISOString();\nexport default {{ name: d, on: \"push\", {SMALL} }};\n")), vec![("made", 0)]);
}

#[test]
fn w29_r27_kci1033_what_isnt_erasable() {
    assert_eq!(failed("enum E { A }\nexport default {};\n"), vec![("KCI1033", 1)]);
}

#[test]
fn w29_r27_w02_points_at_the_sdk_call_that_made_the_part() {
    let ts = "import { workflow, job, step } from \"@kiln/sdk\";\n\nexport default workflow({\n  on: \"push\",\n  jobs: {\n    build: job({ \"runs-on\": \"linux\", stepz: [step({ run: \"true\" })] }),\n  },\n});\n";
    let (w, d) = workflow(".kiln/ci.ts", ts, &json!({}), tree());
    assert!(w.is_some());
    let errors: Vec<(u32, u32)> = d.iter().filter(|d| d.is_error()).map(|d| (d.range.start.line, d.range.start.col)).collect();
    assert!(!errors.is_empty() && errors.iter().all(|(line, col)| *line == 6 && *col >= 12), "{d:?}");
    // A part no SDK call made: at the call around it, else at `export default`.
    let (_, d) = workflow(".kiln/ci.ts", "const n: number = 1;\nexport default { on: \"push\", jobz: {} };\n", &json!({}), tree());
    assert!(d.iter().filter(|d| d.is_error()).all(|d| d.range.start.line == 2 && d.range.start.col == 1), "{d:?}");
}

#[test]
fn w29_r27_one_program_makes_the_same_workflow_every_time() -> anyhow::Result<()> {
    let ts = "import { workflow, job, step, event, tree } from \"@kiln/sdk\";\nconst jobs: Record<string, unknown> = {};\nfor (const os of [\"linux\", \"macos\"]) jobs[`build-${os}`] = job({ \"runs-on\": os, steps: [step({ run: `make ${os}` })] });\nexport default workflow({ name: String(event.ref), on: \"push\", jobs, env: { UI: String(tree.read(\"ui/package.json\") !== null) } });\n";
    let first = run(ts).map_err(|d| anyhow::anyhow!("{d:?}"))?;
    for _ in 0..100 {
        let again = run(ts).map_err(|d| anyhow::anyhow!("{d:?}"))?;
        assert_eq!((&again.json, &again.at), (&first.json, &first.at));
    }
    let v: serde_json::Value = serde_json::from_str(&first.json)?;
    assert_eq!(v.pointer("/name"), Some(&json!("refs/heads/main")));
    assert_eq!(v.pointer("/env/UI"), Some(&json!("true")));
    assert_eq!(v.pointer("/jobs/build-macos/steps/0/run"), Some(&json!("make macos")));
    let paths: Vec<&str> = first.at.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(paths, vec!["", "jobs.build-linux", "jobs.build-linux.steps[0]", "jobs.build-macos", "jobs.build-macos.steps[0]"]);
    Ok(())
}

#[test]
fn w29_r27_a_program_reads_its_event_and_tree_and_nothing_else() -> anyhow::Result<()> {
    let ts = "import { tree } from \"@kiln/sdk\";\nexport default (event: { ref: string }) => ({ name: event.ref, on: \"push\", env: { PASSWD: String(tree.read(\"../../etc/passwd\")), UI: tree.list(\"ui\").join(\",\"), KILN: typeof (globalThis as any).kiln, TIMERS: typeof (globalThis as any).setTimeout, CRYPTO: typeof (globalThis as any).crypto, ORIGIN: typeof (globalThis as any).performance.timeOrigin }, jobs: {} });\n";
    let made = run(ts).map_err(|d| anyhow::anyhow!("{d:?}"))?;
    let v: serde_json::Value = serde_json::from_str(&made.json)?;
    assert_eq!(v.pointer("/name"), Some(&json!("refs/heads/main")));
    assert_eq!(v.pointer("/env"), Some(&json!({ "PASSWD": "null", "UI": "ui/package.json,ui/src/", "KILN": "undefined", "TIMERS": "undefined", "CRYPTO": "undefined", "ORIGIN": "undefined" })));
    Ok(())
}

/// The first place two values differ, as a path (`jobs[0].steps[3].run`).
fn differs(a: &Value, b: &Value, at: &str) -> Option<String> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => x.keys().chain(y.keys()).find_map(|k| {
            let here = if at.is_empty() { k.clone() } else { format!("{at}.{k}") };
            match (x.get(k), y.get(k)) {
                (Some(p), Some(q)) => differs(p, q, &here),
                _ => Some(here),
            }
        }),
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => x.iter().zip(y).enumerate().find_map(|(i, (p, q))| differs(p, q, &format!("{at}[{i}]"))),
        _ => (a != b).then(|| at.to_owned()),
    }
}

/// A workflow's model as the comparison reads it: its file's own path left out.
fn model(w: Option<&Workflow>) -> anyhow::Result<Value> {
    let mut v = serde_json::to_value(w)?;
    if let Some(o) = v.as_object_mut() {
        o.remove("path");
    }
    Ok(v)
}

/// Kiln's own workflow `name` (tests/fixtures/code/kiln): what its YAML makes, and its program.
fn kilns(name: &str) -> anyhow::Result<(Value, String)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/code/kiln");
    let yaml = std::fs::read_to_string(dir.join(format!("{name}.yaml")))?;
    let (w, d) = wf::parse_workflow(&format!(".kiln/{name}.yaml"), &yaml, Dialect::Kiln);
    anyhow::ensure!(!d.iter().any(Diag::is_error), "{name}.yaml: {d:?}");
    Ok((model(w.as_ref())?, std::fs::read_to_string(dir.join(format!("{name}.ts")))?))
}

/// W29-AC25: kiln's own workflows, each also written as a TypeScript program (`ci.ts` by hand,
/// the rest by `translate.mjs`), make the workflows their YAML does: the same model, ranges left
/// out, so every event plans alike.
#[test]
fn w29_ac25_kilns_workflows_in_typescript_make_what_their_yaml_does() -> anyhow::Result<()> {
    for name in ["bench", "ci", "custody-lab", "e2e", "image", "release", "sim", "stress", "wire-lab"] {
        let (want, ts) = kilns(name)?;
        let (got, d) = workflow(&format!(".kiln/{name}.ts"), &ts, &json!({ "ref": "refs/heads/main" }), Arc::new(NoTree));
        assert!(!d.iter().any(Diag::is_error), "{name}.ts: {d:?}");
        let got = model(got.as_ref())?;
        assert!(differs(&got, &want, "").is_none(), "{name}.ts makes another workflow from {:?} on", differs(&got, &want, ""));
    }
    Ok(())
}

/// W29-AC25: a step of ci.ts changed, the comparison names the job and the step.
#[test]
fn w29_ac25_a_changed_step_is_named() -> anyhow::Result<()> {
    let (want, ts) = kilns("ci")?;
    let changed = ts.replace("cargo test --locked --no-fail-fast", "cargo test --locked");
    let (got, _) = workflow(".kiln/ci.ts", &changed, &json!({}), Arc::new(NoTree));
    let at = differs(&model(got.as_ref())?, &want, "").unwrap_or_default();
    let job = at.strip_prefix("jobs[").and_then(|r| r.split_once(']')).and_then(|(i, _)| i.parse::<usize>().ok()).and_then(|i| want.pointer(&format!("/jobs/{i}/id")).cloned());
    assert_eq!((job, at.contains(".steps[3].")), (Some(json!("rust")), true), "{at}");
    Ok(())
}
