//! What the providers that act through kiln's runners share (docs/cd-targets.md §2.2): the
//! workflow kiln writes for a release, in kiln's dialect (a manual run, the release's commit
//! checked out first), the batches runners take a release in, and how an object reads from the run
//! kiln started for it.
//!
//! `terraform`, `pulumi` and `job` run one job on a runner; `host` and `compose` run one on each
//! runner the target's selector picks, a batch at a time, each batch after the one before passed.

use serde_json::{Map, Value, json};

use super::status;
use crate::health::Status;
use crate::model::Health;

/// One job of a workflow kiln writes.
pub struct Job {
    pub id: String,
    pub name: String,
    /// `runs-on`: labels, or one runner's name.
    pub runs_on: Value,
    pub needs: Vec<String>,
    /// Whether the run's commit is checked out first (the dialect's implicit checkout).
    pub checkout: bool,
    /// The environment it's bound to (a `microvm` one's deployments are made on its runner).
    pub environment: Option<String>,
    pub step: Step,
    pub timeout_minutes: u32,
}

/// A job's one step.
pub enum Step {
    /// A script, in `bash`, or `pwsh` on Windows.
    Run { shell: &'static str, script: String },
    /// An action, with its inputs.
    Uses { uses: String, with: Vec<(String, String)> },
}

impl Job {
    /// A job of one `bash` script, after the run's commit is checked out, for up to an hour.
    pub fn script(id: String, name: String, runs_on: Value, script: String) -> Job {
        Job { id, name, runs_on, needs: Vec::new(), checkout: true, environment: None, step: Step::Run { shell: "bash", script }, timeout_minutes: 60 }
    }
}

/// A YAML scalar, quoted (JSON's strings are YAML's double-quoted ones).
pub fn yq(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// A word for `sh`, single-quoted.
pub fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// A string for PowerShell, single-quoted.
pub fn pq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A job's id from a name: letters, digits and `-`, after `prefix`.
pub fn job_id(prefix: &str, name: &str) -> String {
    let mut id: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    id.truncate(40);
    format!("{prefix}-{}", id.trim_matches('-'))
}

/// `hosts` in batches of `size`, in order.
pub fn batches<T: Clone>(hosts: &[T], size: usize) -> Vec<Vec<T>> {
    hosts.chunks(size.max(1)).map(<[T]>::to_vec).collect()
}

/// A value a workflow's text would read as an expression (`${{`) is refused: what kiln writes
/// runs as it says.
pub fn plain(what: &str, s: &str) -> Result<(), String> {
    if s.contains("${{") { Err(format!("{what} holds `${{{{`, which the workflow would read as an expression")) } else { Ok(()) }
}

/// The workflow kiln writes, as YAML: started by hand (by kiln), its jobs in order.
pub fn workflow(name: &str, jobs: &[Job]) -> String {
    let mut y = format!("name: {}\non:\n  manual: true\njobs:\n", yq(name));
    for j in jobs {
        y.push_str(&format!("  {}:\n    name: {}\n    runs-on: {}\n    timeout-minutes: {}\n", j.id, yq(&j.name), j.runs_on, j.timeout_minutes));
        if !j.needs.is_empty() {
            y.push_str(&format!("    needs: {}\n", serde_json::to_string(&j.needs).unwrap_or_else(|_| "[]".into())));
        }
        if !j.checkout {
            y.push_str("    checkout: false\n");
        }
        if let Some(e) = &j.environment {
            y.push_str(&format!("    environment: {}\n", yq(e)));
        }
        match &j.step {
            Step::Run { shell, script } => {
                y.push_str(&format!("    steps:\n      - name: {}\n        shell: {shell}\n        run: |\n", yq(&j.name)));
                block(&mut y, script, 10);
            }
            Step::Uses { uses, with } => {
                y.push_str(&format!("    steps:\n      - name: {}\n        uses: {uses}\n", yq(&j.name)));
                if !with.is_empty() {
                    y.push_str("        with:\n");
                    for (k, v) in with {
                        if v.contains('\n') {
                            y.push_str(&format!("          {k}: |\n"));
                            block(&mut y, v, 12);
                        } else {
                            y.push_str(&format!("          {k}: {}\n", yq(v)));
                        }
                    }
                }
            }
        }
    }
    y
}

/// `text`'s lines into `y` as a block scalar's, indented by `indent`.
fn block(y: &mut String, text: &str, indent: usize) {
    for line in text.lines() {
        if line.trim().is_empty() {
            y.push('\n');
        } else {
            y.push_str(&" ".repeat(indent));
            y.push_str(line);
            y.push('\n');
        }
    }
}

/// A setting's map of names to values, as text: `vars`, `env`.
pub fn text_map(spec: &Value, key: &str) -> Result<Map<String, Value>, String> {
    match spec.get(key) {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(m)) => {
            let mut out = Map::new();
            for (k, v) in m {
                let s = v.as_str().map_or_else(|| v.to_string(), str::to_owned);
                plain(&format!("{key}.{k}"), &s)?;
                out.insert(k.clone(), json!(s));
            }
            Ok(out)
        }
        Some(_) => Err(format!("{key}: a map of names to values")),
    }
}

/// `runs-on` as a setting says it (labels, or one label), or `default`.
pub fn runs_on(spec: &Value, default: &str) -> Result<Value, String> {
    match spec.get("runs-on") {
        None | Some(Value::Null) => Ok(json!(default)),
        Some(v @ Value::String(_)) => Ok(v.clone()),
        Some(Value::Array(a)) if a.iter().all(Value::is_string) => Ok(Value::Array(a.clone())),
        Some(_) => Err("runs-on: a label, or a list of them".into()),
    }
}

/// An object as it reads from the run kiln last started for it: what kiln applied, and how that
/// run stands (`run`: its id, number, status, error and address).
pub fn live(applied: &Value, run: Option<&Value>) -> Value {
    let mut o = applied.clone();
    if let Some(m) = o.as_object_mut() {
        m.remove("status");
        m.insert("status".into(), json!({ "run": run }));
    }
    o
}

/// Health from the run kiln last started: passed, under way, or why it didn't.
pub fn health(o: &Value) -> Option<Status> {
    let run = o.pointer("/status/run").filter(|r| !r.is_null())?;
    let s = run.get("status").and_then(Value::as_str).unwrap_or_default();
    let what = format!("run {}", run.get("number").map_or_else(|| run.get("id").map(Value::to_string).unwrap_or_default(), |n| format!("#{}", n.as_i64().map_or_else(|| n.to_string(), |n| n.to_string()))));
    match s {
        "succeeded" => status(Health::Healthy, ""),
        "pending" | "queued" | "running" => status(Health::Progressing, format!("{what} is {s}")),
        _ => status(Health::Degraded, format!("{what} {}{}", s.replace('_', " "), run.get("error").and_then(Value::as_str).filter(|e| !e.is_empty()).map(|e| format!(": {e}")).unwrap_or_default())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflows_are_kilns_dialect() {
        let j = Job { needs: vec!["plan".into()], ..Job::script("apply".into(), "terraform apply".into(), json!(["linux", "terraform"]), "set -eu\ncd 'infra'\n\nterraform apply".into()) };
        let y = workflow("kiln: terraform prod", &[j]);
        assert!(y.starts_with("name: \"kiln: terraform prod\"\non:\n  manual: true\njobs:\n  apply:\n"), "{y}");
        assert!(y.contains("    runs-on: [\"linux\",\"terraform\"]\n    timeout-minutes: 60\n    needs: [\"plan\"]\n"), "{y}");
        assert!(y.contains("        run: |\n          set -eu\n          cd 'infra'\n\n          terraform apply\n"), "{y}");
        let vm = Job { id: "vm-a".into(), name: "vm on a".into(), runs_on: json!("a"), needs: Vec::new(), checkout: false, environment: Some("kiln-vm".into()), step: Step::Uses { uses: "kiln/deploy@v1".into(), with: vec![("image".into(), "ghcr.io/acme/vm:1".into()), ("ports".into(), "8080:80\n8443:443".into())] }, timeout_minutes: 30 };
        let y = workflow("kiln: vm", &[vm]);
        assert!(y.contains("    checkout: false\n    environment: \"kiln-vm\"\n    steps:\n      - name: \"vm on a\"\n        uses: kiln/deploy@v1\n        with:\n          image: \"ghcr.io/acme/vm:1\"\n          ports: |\n            8080:80\n            8443:443\n"), "{y}");
        assert_eq!(sq("it's"), "'it'\"'\"'s'");
        assert_eq!(pq("it's"), "'it''s'");
        assert_eq!(job_id("host", "Web 1.local"), "host-web-1-local");
        assert_eq!(batches(&[1, 2, 3], 2), vec![vec![1, 2], vec![3]]);
        assert!(plain("vars.x", "${{ secrets.x }}").is_err());
        let run = |s: &str| json!({ "status": { "run": { "id": "r1", "number": 7, "status": s, "error": "exit code 1" } } });
        assert_eq!(health(&run("succeeded")).map(|h| h.health), Some(Health::Healthy));
        assert_eq!(health(&run("running")).map(|h| h.health), Some(Health::Progressing));
        assert!(health(&run("failed")).is_some_and(|h| h.health == Health::Degraded && h.message == "run #7 failed: exit code 1"));
    }
}
