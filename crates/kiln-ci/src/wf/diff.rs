//! What an update changes for its consumer (docs/ci.md W29-R22): two versions of an action
//! (`action.yml`) or of a reusable workflow (`on.workflow_call`), compared as a workflow that
//! uses them meets them: their inputs, secrets, permissions and egress, and the steps or jobs a
//! run of them has.

use serde::Serialize;

use super::ir::{Action, Permissions, Step, Workflow};

/// One version of what's updated, parsed: an action with its file's text (egress is read from
/// it), or a reusable workflow.
#[derive(Debug, Clone, Copy)]
pub enum Version<'a> {
    Action(&'a Action, &'a str),
    Workflow(&'a Workflow),
}

/// One change an update makes for its consumer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Change {
    /// `input`, `secret`, `permission`, `egress`, `step` or `job`.
    pub kind: &'static str,
    pub name: String,
    /// `added`, `removed`, `now required`, `no longer required` or `changed`.
    pub what: &'static str,
    /// What it is now: `required`, a permission's level (`read → write`).
    pub detail: Option<String>,
}

impl Version<'_> {
    /// Inputs, each with whether it's required.
    fn inputs(&self) -> Vec<(String, bool)> {
        match self {
            Version::Action(a, _) => a.inputs.iter().map(|i| (i.name.clone(), i.required)).collect(),
            Version::Workflow(w) => w.trigger("workflow_call").map(|t| t.inputs.iter().map(|i| (i.name.clone(), i.required)).collect()).unwrap_or_default(),
        }
    }

    /// Secrets it declares, each with whether it's required.
    fn secrets(&self) -> Vec<(String, bool)> {
        match self {
            Version::Action(a, _) => a.secrets.iter().map(|s| (s.name.clone(), s.required)).collect(),
            Version::Workflow(w) => w.trigger("workflow_call").map(|t| t.secrets.iter().map(|s| (s.name.clone(), s.required)).collect()).unwrap_or_default(),
        }
    }

    /// Permissions it asks for, scope by scope (`*` for all).
    fn permissions(&self) -> Vec<(String, String)> {
        let of = |p: &Option<Permissions>| match p {
            Some(Permissions::ReadAll) => vec![("*".to_owned(), "read".to_owned())],
            Some(Permissions::WriteAll) => vec![("*".to_owned(), "write".to_owned())],
            Some(Permissions::Scopes(s)) => s.clone(),
            None => Vec::new(),
        };
        match self {
            Version::Action(a, _) => of(&a.permissions),
            Version::Workflow(w) => {
                let mut all = of(&w.permissions);
                for j in &w.jobs {
                    for (scope, level) in of(&j.permissions) {
                        if !all.iter().any(|(s, _)| *s == scope) {
                            all.push((scope, level));
                        }
                    }
                }
                all
            }
        }
    }

    /// The hosts a microVM action may reach (`runs.egress`, W29-R29).
    fn egress(&self) -> Vec<String> {
        let Version::Action(_, text) = self else { return Vec::new() };
        let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(text) else { return Vec::new() };
        doc.get("runs").and_then(|r| r.get("egress")).and_then(serde_yaml::Value::as_sequence).map(|s| s.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default()
    }

    /// What a run of it has: a composite action's steps, a reusable workflow's jobs.
    fn parts(&self) -> (&'static str, Vec<String>) {
        match self {
            Version::Action(a, _) => ("step", a.runs.steps.iter().enumerate().map(|(i, s)| step_name(i, s)).collect()),
            Version::Workflow(w) => ("job", w.jobs.iter().map(|j| j.id.clone()).collect()),
        }
    }
}

/// A step as the plan names it: its id, its name, what it uses, or its command's first line.
fn step_name(i: usize, s: &Step) -> String {
    s.id.clone()
        .or_else(|| s.name.as_ref().map(|n| n.src.clone()))
        .or_else(|| s.uses.clone())
        .or_else(|| s.run.as_ref().map(|r| r.src.lines().next().unwrap_or_default().to_owned()))
        .unwrap_or_else(|| format!("step {}", i + 1))
}

fn change(kind: &'static str, name: &str, what: &'static str, detail: Option<String>) -> Change {
    Change { kind, name: name.to_owned(), what, detail }
}

/// Names with whether each is required: added, removed, and required now or no longer.
fn named(kind: &'static str, old: &[(String, bool)], new: &[(String, bool)], out: &mut Vec<Change>) {
    for (n, required) in new {
        match old.iter().find(|(o, _)| o == n) {
            None => out.push(change(kind, n, "added", required.then(|| "required".to_owned()))),
            Some((_, false)) if *required => out.push(change(kind, n, "now required", None)),
            Some((_, true)) if !required => out.push(change(kind, n, "no longer required", None)),
            Some(_) => {}
        }
    }
    out.extend(old.iter().filter(|(o, _)| !new.iter().any(|(n, _)| n == o)).map(|(o, _)| change(kind, o, "removed", None)));
}

/// Names only: added and removed.
fn listed(kind: &'static str, old: &[String], new: &[String], out: &mut Vec<Change>) {
    out.extend(new.iter().filter(|n| !old.contains(n)).map(|n| change(kind, n, "added", None)));
    out.extend(old.iter().filter(|o| !new.contains(o)).map(|o| change(kind, o, "removed", None)));
}

/// What an update from `old` to `new` changes for a workflow that uses it (W29-R22): inputs
/// added, removed or newly required, secrets, permissions, egress, and the steps or jobs a run of
/// it has, added or gone. Nothing for an update that changes none of them.
pub fn action_diff(old: Version<'_>, new: Version<'_>) -> Vec<Change> {
    let mut out = Vec::new();
    named("input", &old.inputs(), &new.inputs(), &mut out);
    named("secret", &old.secrets(), &new.secrets(), &mut out);
    let (was, now) = (old.permissions(), new.permissions());
    for (scope, level) in &now {
        match was.iter().find(|(s, _)| s == scope) {
            None => out.push(change("permission", scope, "added", Some(level.clone()))),
            Some((_, before)) if before != level => out.push(change("permission", scope, "changed", Some(format!("{before} → {level}")))),
            Some(_) => {}
        }
    }
    out.extend(was.iter().filter(|(s, _)| !now.iter().any(|(n, _)| n == s)).map(|(s, _)| change("permission", s, "removed", None)));
    listed("egress", &old.egress(), &new.egress(), &mut out);
    let ((kind, before), (_, after)) = (old.parts(), new.parts());
    listed(kind, &before, &after, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::Dialect;

    #[test]
    fn w29_r22_what_an_update_changes() {
        let v1 = "name: x\ninputs:\n  who:\n    required: true\n  how:\n    default: loudly\nsecrets:\n  token:\n    required: true\npermissions:\n  contents: read\nruns:\n  using: composite\n  egress: [a.example]\n  steps:\n    - id: hello\n      run: echo hi\n      shell: bash\n    - id: bye\n      run: echo bye\n      shell: bash\n";
        let v2 = "name: x\ninputs:\n  who:\n    required: true\n  how:\n    required: true\n  where:\n    required: true\npermissions:\n  contents: write\n  issues: read\nruns:\n  using: composite\n  egress: [a.example, b.example]\n  steps:\n    - id: hello\n      run: echo hi\n      shell: bash\n    - id: wave\n      run: echo wave\n      shell: bash\n";
        let (a1, a2) = (crate::wf::parse_action(v1).0.unwrap_or_default(), crate::wf::parse_action(v2).0.unwrap_or_default());
        let said: Vec<(&str, String, &str, Option<String>)> = action_diff(Version::Action(&a1, v1), Version::Action(&a2, v2)).into_iter().map(|c| (c.kind, c.name, c.what, c.detail)).collect();
        let want: Vec<(&str, String, &str, Option<String>)> = vec![
            ("input", "how".into(), "now required", None),
            ("input", "where".into(), "added", Some("required".into())),
            ("secret", "token".into(), "removed", None),
            ("permission", "contents".into(), "changed", Some("read → write".into())),
            ("permission", "issues".into(), "added", Some("read".into())),
            ("egress", "b.example".into(), "added", None),
            ("step", "wave".into(), "added", None),
            ("step", "bye".into(), "removed", None),
        ];
        assert_eq!(said, want);
        assert!(action_diff(Version::Action(&a1, v1), Version::Action(&a1, v1)).is_empty(), "the same version changes nothing");

        let w1 = "on:\n  workflow_call:\n    inputs:\n      target:\n        type: string\n    secrets:\n      token:\n        required: true\njobs:\n  build:\n    runs-on: local\n    steps:\n      - run: echo build\n  lint:\n    runs-on: local\n    steps:\n      - run: echo lint\n";
        let w2 = "on:\n  workflow_call:\n    inputs:\n      target:\n        type: string\n        required: true\n      cache:\n        type: boolean\n    secrets:\n      token:\n        required: false\npermissions:\n  packages: write\njobs:\n  build:\n    runs-on: local\n    steps:\n      - run: echo build\n  test:\n    runs-on: local\n    steps:\n      - run: echo test\n";
        let parse = |src: &str| crate::wf::parse_workflow(".github/workflows/build.yml", src, Dialect::Gha).0.unwrap_or_default();
        let (r1, r2) = (parse(w1), parse(w2));
        let said: Vec<(&str, String, &str)> = action_diff(Version::Workflow(&r1), Version::Workflow(&r2)).into_iter().map(|c| (c.kind, c.name, c.what)).collect();
        assert_eq!(
            said,
            [
                ("input", "target".to_owned(), "now required"),
                ("input", "cache".into(), "added"),
                ("secret", "token".into(), "no longer required"),
                ("permission", "packages".into(), "added"),
                ("job", "test".into(), "added"),
                ("job", "lint".into(), "removed"),
            ]
        );
    }
}
