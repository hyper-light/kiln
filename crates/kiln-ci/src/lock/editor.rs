//! W29-R25/R26: the editor's lock feedback. KCI6002's pin hint also offers "lock it"; a use its
//! locks hold can't change under you, so it needs no hint; and under the trunk's `require_lock`
//! the file says before its run would that it isn't locked (`KCI4055`). The server reads the facts
//! without fetching: what the locks hold at the commit being edited, and the trusted policy.

use std::collections::BTreeMap;

use super::verify::{Policy, Require};
use crate::wf::Workflow;
use crate::wf::diag::{Diag, Lock, Pos, Range, Severity};
use crate::wf::ir::Step;

/// Each written `uses:` (a job's, then its steps', nested ones too) with its range and YAML path.
pub fn written(w: &Workflow) -> Vec<(String, Range, String)> {
    fn steps(list: &[Step], path: &str, out: &mut Vec<(String, Range, String)>) {
        for (i, s) in list.iter().enumerate() {
            let at = format!("{path}[{i}]");
            if let Some(u) = &s.uses {
                out.push((u.clone(), s.range, format!("{at}.uses")));
            }
            steps(&s.parallel, &format!("{at}.parallel"), out);
        }
    }
    let mut out = Vec::new();
    for j in &w.jobs {
        if let Some(u) = &j.uses {
            out.push((u.clone(), j.range, format!("jobs.{}.uses", j.id)));
        }
        steps(&j.steps, &format!("jobs.{}.steps", j.id), &mut out);
    }
    out
}

pub struct Facts<'a> {
    /// The written uses the workflow's locks hold, each with the commit they hold it at.
    pub locked: &'a BTreeMap<String, String>,
    /// The workflow has an entry in a lock, GitHub's or kiln's.
    pub onboarded: bool,
    pub policy: Policy,
}

/// What a lock is still missing does to its runs, as the policy says it.
fn consequence(require: Require) -> &'static str {
    match require {
        Require::Enforce => "`require_lock: enforce` refuses its runs until it is",
        Require::Report | Require::Off => "`require_lock: report` marks its runs until it is",
    }
}

/// `diags` as the editor shows them. A KCI6002 or a pin-requiring KCI4021 on a use kiln can lock
/// offers to lock it; required locks raise KCI6002 to a warning and add KCI4055 for an
/// unonboarded file. They stay warnings under `enforce`: Save draft, which locks, must still save.
pub fn feedback(diags: &mut Vec<Diag>, w: &Workflow, path: &str, facts: &Facts<'_>) {
    let uses: BTreeMap<Range, String> = written(w).into_iter().map(|(u, r, _)| (r, u)).collect();
    diags.retain(|d| !(d.code == "KCI6002" && uses.get(&d.range).is_some_and(|u| facts.locked.contains_key(u))));
    let required = facts.policy.require != Require::Off;
    for d in diags.iter_mut() {
        let Some(u) = uses.get(&d.range) else { continue };
        match d.code {
            "KCI6002" => {
                d.lock = Some(Lock { uses: Some(u.clone()) });
                if required {
                    d.severity = Severity::Warning;
                    d.message = format!("`{u}` follows a tag or branch and isn't locked: {}", consequence(facts.policy.require));
                }
            }
            "KCI4021" if lockable(u) => d.lock = Some(Lock { uses: Some(u.clone()) }),
            _ => {}
        }
    }
    if required && !facts.onboarded {
        let at = Pos { line: 1, col: 1, offset: 0 };
        diags.push(Diag::warning("KCI4055", format!("{path} isn't locked: {}", consequence(facts.policy.require)), Range { start: at, end: at }).at("on").with_lock(None));
    }
}

/// A use a lock can hold: another repository's action or reusable workflow by a ref.
pub fn lockable(u: &str) -> bool {
    !(u.starts_with("./") || u.starts_with("$/") || u.starts_with("docker://") || u.starts_with("kiln/")) && u.contains('@')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::Dialect;
    use crate::lock::verify::MovedTags;

    const PATH: &str = ".github/workflows/ci.yml";
    const TEXT: &str = "on: push\njobs:\n  check:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: acme/tools@v1\n      - uses: acme/other@v2\n      - uses: ./local\n";

    fn checked(text: &str) -> (Vec<Diag>, Workflow) {
        let (checked, model) = crate::wf::check_model(PATH, text, Some(Dialect::Gha));
        (checked.diagnostics, model.unwrap_or_default())
    }

    fn policy(require: Require) -> Policy { Policy { require, moved_tags: MovedTags::Flag } }

    #[test]
    fn w29_r25_a_locked_use_needs_no_hint_and_an_unlocked_one_offers_to_lock_it() {
        let (mut diags, w) = checked(TEXT);
        assert_eq!(diags.iter().filter(|d| d.code == "KCI6002").count(), 2, "{diags:?}");
        let locked = [("acme/tools@v1".to_owned(), "a".repeat(40))].into();
        feedback(&mut diags, &w, PATH, &Facts { locked: &locked, onboarded: true, policy: Policy::default() });
        let hints: Vec<_> = diags.iter().filter(|d| d.code == "KCI6002").collect();
        assert_eq!(hints.len(), 1);
        assert!(hints.iter().all(|d| d.severity == Severity::Info && d.lock == Some(Lock { uses: Some("acme/other@v2".into()) })));
        assert!(!diags.iter().any(|d| d.code == "KCI4055"), "off asks for no lock");
    }

    #[test]
    fn w29_r26_required_locks_warn_in_the_editor_and_name_what_their_runs_meet() {
        for (require, words) in [(Require::Report, "marks its runs"), (Require::Enforce, "refuses its runs")] {
            let (mut diags, w) = checked(TEXT);
            feedback(&mut diags, &w, PATH, &Facts { locked: &BTreeMap::new(), onboarded: false, policy: policy(require) });
            let hints: Vec<_> = diags.iter().filter(|d| d.code == "KCI6002").collect();
            assert_eq!(hints.len(), 2);
            assert!(hints.iter().all(|d| d.severity == Severity::Warning && d.message.contains("isn't locked") && d.message.contains(words) && d.lock.is_some()));
            let whole = diags.iter().find(|d| d.code == "KCI4055");
            assert!(whole.is_some_and(|d| d.severity == Severity::Warning && d.lock == Some(Lock::default()) && d.message.contains(words)), "{diags:?}");
            assert!(!diags.iter().any(Diag::is_error), "Save draft, which locks, still saves");
        }
        let (mut diags, w) = checked(TEXT);
        feedback(&mut diags, &w, PATH, &Facts { locked: &BTreeMap::new(), onboarded: true, policy: policy(Require::Report) });
        assert!(!diags.iter().any(|d| d.code == "KCI4055"), "an onboarded file asks only for its new uses");
    }

    #[test]
    fn w29_r25_a_pin_policys_refusal_offers_a_lock_but_not_for_a_local_action() {
        let (mut diags, w) = checked(TEXT);
        let mut refusals: Vec<Diag> = written(&w).into_iter().map(|(_, r, at)| Diag::error("KCI4021", "requires a full commit SHA", r).at(at)).collect();
        diags.append(&mut refusals);
        feedback(&mut diags, &w, PATH, &Facts { locked: &BTreeMap::new(), onboarded: true, policy: Policy::default() });
        let offered: Vec<_> = diags.iter().filter(|d| d.code == "KCI4021").map(|d| d.lock.as_ref().and_then(|l| l.uses.clone())).collect();
        assert_eq!(offered, [Some("acme/tools@v1".to_owned()), Some("acme/other@v2".to_owned()), None]);
    }
}
