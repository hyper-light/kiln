//! W29-R25: writing a proposal does not authorize it to run. Retain an accepted tree while
//! verifying its source; a moved full tag still fails ordinary execution verification.

use super::*;
use crate::lock::{verify, verify_kiln};

pub struct SourceCheck<'a> {
    pub workflow: &'a str,
    pub key: &'a str,
    pub accepted: Option<&'a Git>,
    pub current: &'a Git,
    pub facts: &'a verify_kiln::GitFacts,
    pub home_host: &'a str,
    pub mode: Mode,
}

/// The same accepted pool used by regeneration, before the adapter reads metadata. A caller's
/// native additions precede callee seeds; unrelated GitHub/native workflows may keep distinct
/// action pins. Returning a conflicting pool is an error rather than a second source choice.
pub fn accepted(github: Option<&github::File>, native: Option<&kiln::File>, workflow: &str, uses: &str, home: &str) -> Result<Option<Dependency>, Vec<Diag>> {
    let mut pool = BTreeMap::new();
    let is_github = workflow.starts_with(".github/workflows/");
    if is_github && let Some(file) = github {
        let reachable: BTreeSet<_> = file.workflows.keys().flat_map(|w| file.closure(w).unwrap_or_default()).collect();
        for key in reachable {
            if let Some(action) = file.dependencies.get(&key) { pool.insert(key.clone(), Entry { pin: from_github(action), source: format!("{}:{key}", github::PATH) }); }
        }
    }
    if let Some(file) = native {
        let explicit: BTreeSet<_> = file.closure(workflow).unwrap_or_default().into_iter().collect();
        let reachable: BTreeSet<_> = file.workflows.keys().flat_map(|w| file.closure(w).unwrap_or_default()).collect();
        for key in reachable {
            let Some(pin) = file.dependencies.get(&key) else { continue };
            if is_github && github::action_pin(&key).is_some() && matches!(pin, Dependency::Git(g) if matches!(g.identity(home), Some(Identity::Github { .. }))) && !explicit.contains(&key) { continue; }
            add(&mut pool, key.clone(), Entry { pin: pin.clone(), source: format!("{}:{key}", kiln::PATH) }, home).map_err(|d| vec![*d])?;
        }
    }
    let mut found = lookup(&pool, uses).map(|e| e.pin.clone());
    if let Some(Dependency::Git(g)) = &mut found && g.hostname.is_none() && matches!(g.identity(home), Some(Identity::Github { .. })) { g.hostname = Some(home.into()); }
    Ok(found)
}

/// Verify identity and membership in the captured recorded ref before metadata is read.
/// Preserving A in a draft is allowed when a full tag points at descendant M; it neither
/// accepts M nor makes A executable under that moved full tag. The run checker still refuses.
pub fn source(check: SourceCheck<'_>) -> Result<Git, Vec<Diag>> {
    let qualify = |git: &Git| {
        let mut git = git.clone();
        if !git.git_ref.starts_with("refs/") && git_ref_equal(&git.git_ref, &check.facts.reference) { git.git_ref = check.facts.reference.clone(); }
        git
    };
    let accepted = check.accepted.map(qualify);
    let current = qualify(check.current);
    let entry = accepted.as_ref().unwrap_or(&current);
    for candidate in std::iter::once(entry).chain((check.mode == Mode::Propose).then_some(&current)) {
        if let Err(refused) = verify_kiln::check_git(verify_kiln::Check {
            workflow: check.workflow, key: check.key, entry: candidate, facts: check.facts,
            home_host: check.home_host, policy: verify::Policy::default(), accepted_moves: &BTreeMap::new(),
        }) {
            let errors: Vec<_> = refused.diagnostics.into_iter().filter(|d| d.is_error() && d.code != "KCI4053").collect();
            if !errors.is_empty() { return Err(errors); }
        }
    }
    let mut selected = if check.mode == Mode::Propose { current } else { entry.clone() };
    // Metadata at the selected immutable tree supplies its edges, never metadata at M for A.
    selected.uses.clear();
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w29_r25_a_draft_preserves_a_moved_full_tag_without_authorizing_its_execution() -> Result<(), Vec<Diag>> {
        let old = "a".repeat(40);
        let now = "b".repeat(40);
        let accepted = Git { git_ref: "v1.2.3".into(), commit: format!("sha1-{old}"), repo: "github:41/42".into(), ..Default::default() };
        let current = Git { git_ref: "refs/tags/v1.2.3".into(), commit: format!("sha1-{now}"), ..accepted.clone() };
        let identity = Identity::Github { hostname: "github.com".into(), owner_id: 41, repo_id: 42 };
        let mut facts = verify_kiln::GitFacts { identity, reference: current.git_ref.clone(), current: now.clone(), reachable: [old.clone()].into(), previous: Some(old.clone()) };
        let make = |facts: &verify_kiln::GitFacts, mode| source(SourceCheck { workflow: ".github/workflows/ci.yml", key: "acme/tools@v1.2.3", accepted: Some(&accepted), current: &current, facts, home_host: "github.com", mode });
        let preserved = make(&facts, Mode::Preserve)?;
        assert_eq!(preserved.commit, accepted.commit);
        assert_eq!(make(&facts, Mode::Propose)?.commit, current.commit);
        let refused = verify_kiln::check_git(verify_kiln::Check { workflow: ".github/workflows/ci.yml", key: "acme/tools@v1.2.3", entry: &preserved, facts: &facts, home_host: "github.com", policy: verify::Policy::default(), accepted_moves: &BTreeMap::new() });
        assert!(refused.is_err_and(|e| e.diagnostics.iter().any(|d| d.code == "KCI4053")));
        facts.reachable.clear();
        assert!(make(&facts, Mode::Preserve).is_err_and(|ds| ds.iter().any(|d| d.code == "KCI4051")));
        facts.reachable.insert(old);
        facts.identity = Identity::Github { hostname: "github.com".into(), owner_id: 99, repo_id: 100 };
        assert!(make(&facts, Mode::Propose).is_err_and(|ds| ds.iter().any(|d| d.code == "KCI4052")));
        Ok(())
    }
}
