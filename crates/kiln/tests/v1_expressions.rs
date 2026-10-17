//! docs/ci.md W01-AC09: every `if:` and `${{ }}` in the v1 corpus (F-02: every hyper-light
//! repository's `.kiln/*.yaml`) evaluates in the kiln dialect to exactly what v1's
//! `pipeline::Context` gives, for each event and branch v1 runs them on.

use std::collections::BTreeMap;
use std::path::Path;

use kiln::pipeline::{self, Context, Pipeline};
use kiln_ci::expr::{Budget, Condition, Dialect, MapContexts, ParseOptions, Template, Value, eval_condition, render};

fn scalar(v: &serde_yaml::Value) -> (String, Value) {
    match v {
        serde_yaml::Value::String(s) => (s.clone(), Value::str(s)),
        serde_yaml::Value::Bool(b) => (b.to_string(), Value::Bool(*b)),
        serde_yaml::Value::Number(n) => (n.to_string(), n.as_f64().map_or_else(|| Value::str(&n.to_string()), Value::Number)),
        serde_yaml::Value::Null => (String::new(), Value::Null),
        other => {
            let s = serde_yaml::to_string(other).unwrap_or_default().trim().to_owned();
            (s.clone(), Value::str(&s))
        }
    }
}

/// Every combination of a v1 matrix, as v1 expands it: (axis, v1's string, the typed value).
fn combos(m: &BTreeMap<String, Vec<serde_yaml::Value>>) -> Vec<Vec<(String, String, Value)>> {
    let mut out: Vec<Vec<(String, String, Value)>> = vec![Vec::new()];
    for (k, values) in m {
        if values.is_empty() {
            continue;
        }
        let mut next = Vec::new();
        for c in &out {
            for v in values {
                let (s, typed) = scalar(v);
                let mut c2 = c.clone();
                c2.push((k.clone(), s, typed));
                next.push(c2);
            }
        }
        out = next;
    }
    out
}

/// (job id, every templated string in the job, its condition)
fn templates(p: &Pipeline) -> Vec<(String, Vec<String>, Option<String>)> {
    p.jobs
        .iter()
        .map(|(id, j)| {
            let mut t: Vec<String> = Vec::new();
            t.extend(j.name.clone());
            t.extend(j.runs_on.iter().cloned());
            t.extend(j.image.clone());
            t.extend(p.env.values().cloned());
            t.extend(j.env.values().cloned());
            for s in &j.steps {
                t.push(s.run.clone());
                t.extend(s.name.clone());
                t.extend(s.env.values().cloned());
                t.extend(s.working_directory.clone());
            }
            (id.clone(), t, j.condition.clone())
        })
        .collect()
}

fn obj(pairs: &[(&str, &str)]) -> Value {
    Value::object(pairs.iter().map(|(k, v)| ((*k).to_owned(), Value::str(v))))
}

#[test]
fn w01_ac09_v1_expressions_mean_the_same_in_the_kiln_dialect() -> Result<(), String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../kiln-ci/tests/fixtures/v1");
    let mut files = Vec::new();
    for repo in std::fs::read_dir(&root).map_err(|e| e.to_string())?.filter_map(Result::ok) {
        if repo.path().is_dir() {
            for f in std::fs::read_dir(repo.path()).map_err(|e| e.to_string())?.filter_map(Result::ok) {
                files.push(f.path());
            }
        }
    }
    files.sort();
    let (mut parsed, mut checked, mut conditions, mut failures) = (0usize, 0usize, 0usize, Vec::new());
    let opts = ParseOptions::permissive(Dialect::Kiln);
    for file in &files {
        let src = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
        let Ok(p) = pipeline::parse(&src) else { continue };
        parsed += 1;
        for (job, strings, cond) in templates(&p) {
            let matrix = p.jobs.get(&job).map(|j| combos(&j.matrix)).unwrap_or_default();
            for event in ["push", "pull_request", "schedule", "manual"] {
                for branch in ["main", "feature/x"] {
                    for leg in &matrix {
                        let mut v1 = Context::default();
                        for (k, val) in [("git.sha", "0123456789abcdef"), ("git.short_sha", "01234567"), ("git.branch", branch), ("branch", branch), ("event", event), ("repo", "hyper-light/x"), ("sha", "0123456789abcdef")] {
                            v1.set(k, val);
                        }
                        let mut m = Vec::new();
                        for (k, s, typed) in leg {
                            v1.set(format!("matrix.{k}"), s.clone());
                            m.push((k.clone(), typed.clone()));
                        }
                        let cx = MapContexts::default()
                            .with("git", obj(&[("sha", "0123456789abcdef"), ("short_sha", "01234567"), ("branch", branch)]))
                            .with("github", obj(&[("event_name", event), ("ref_name", branch), ("repository", "hyper-light/x"), ("sha", "0123456789abcdef")]))
                            .with("matrix", Value::object(m));
                        for s in &strings {
                            checked += 1;
                            let new = Template::parse(s, Dialect::Kiln).and_then(|t| render(&t, &cx, &mut Budget::default()));
                            let old = v1.interpolate(s);
                            if new.as_deref() != Ok(old.as_str()) {
                                failures.push(format!("{}: {job}: {s:?} on {event}/{branch}: v1 {old:?}, kiln {new:?}", file.display()));
                            }
                        }
                        if let Some(c) = &cond {
                            conditions += 1;
                            let old = v1.eval(&v1.interpolate(c));
                            let new = Condition::parse(c, &opts).and_then(|c| eval_condition(&c, &cx, &mut Budget::default()));
                            if new != Ok(old) {
                                failures.push(format!("{}: {job}: if {c:?} on {event}/{branch}: v1 {old}, kiln {new:?}", file.display()));
                            }
                        }
                    }
                }
            }
        }
    }
    eprintln!("v1 corpus: {parsed} of {} files parsed by v1; {checked} templated strings and {conditions} conditions compared", files.len());
    assert!(checked > 1_000, "the corpus is there ({checked})");
    if failures.is_empty() { Ok(()) } else { Err(format!("{} differences:\n{}", failures.len(), failures.into_iter().take(40).collect::<Vec<_>>().join("\n"))) }
}
