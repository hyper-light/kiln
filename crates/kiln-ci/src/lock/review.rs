//! W29-R25/R26: bind a reviewed source-code difference to the before/after lock entries.
//! The adapter proves the person's review and source facts. This pure check prevents that
//! proof from being reused for another pin, source identity, ref or pair of code trees.

use std::collections::BTreeMap;

use super::{github, kiln, regenerate, replay::Pair};
use crate::wf::diag::{Diag, Pos, Range};

pub struct Change {
    pub key: String,
    pub reference: String,
    pub before: String,
    pub after: String,
    pub identity: kiln::Identity,
}

pub struct Input<'a> {
    pub before: Pair<'a>,
    pub after: Pair<'a>,
    pub workflow: &'a str,
    pub home_host: &'a str,
    pub changes: &'a [Change],
    /// A missing consumer entry may use a prior acceptance proved in the source history.
    /// These are trusted prior pins, never the latest observation of the moved ref.
    pub prior: &'a BTreeMap<String, kiln::Git>,
}

fn error(code: &'static str, key: &str, why: &str) -> Vec<Diag> {
    let at = Pos { line: 1, col: 1, offset: 0 };
    vec![Diag::error(code, format!("{key}: {why}"), Range { start: at, end: at }).at(kiln::PATH)]
}

fn sha(s: &str) -> bool { matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit()) }

fn reference(recorded: &str, qualified: &str) -> bool {
    recorded == qualified || ["refs/tags/", "refs/heads/"].iter().any(|prefix| qualified.strip_prefix(prefix) == Some(recorded))
}

fn selected(pair: Pair<'_>, input: &Input<'_>, key: &str) -> Result<Option<kiln::Git>, Vec<Diag>> {
    match regenerate::accepted(pair.github, pair.kiln, input.workflow, key, input.home_host)? {
        Some(kiln::Dependency::Git(git)) => Ok(Some(git)),
        Some(_) => Err(error("KCI4054", key, "a reviewed Git source cannot authorize another dependency kind")),
        None => Ok(None),
    }
}

/// Return only changed pins. No workflow roots or contextual `uses` edges belong to this
/// proof; regeneration obtains those again at the exact selected code after source merging.
pub fn bind(input: Input<'_>) -> Result<BTreeMap<String, kiln::Git>, Vec<Diag>> {
    for pair in [input.before, input.after] {
        if let Some(file) = pair.github { file.write(input.home_host)?; }
        if let Some(file) = pair.kiln { file.write()?; }
        if let (Some(gh), Some(kl)) = (pair.github, pair.kiln) { regenerate::compare(gh, kl, input.home_host)?; }
    }
    let mut pins = BTreeMap::new();
    for change in input.changes {
        if kiln::canonical_key(&change.key).as_ref() != Some(&change.key) || !sha(&change.before) || !sha(&change.after) || change.before.eq_ignore_ascii_case(&change.after) {
            return Err(error("KCI4051", &change.key, "invalid source-code change"));
        }
        let before = selected(input.before, &input, &change.key)?.or_else(|| input.prior.get(&change.key).cloned()).ok_or_else(|| error("KCI4051", &change.key, "the accepted base pin needs fresh review evidence"))?;
        let mut after = selected(input.after, &input, &change.key)?.ok_or_else(|| error("KCI4050", &change.key, "the reviewed pin is missing"))?;
        for (entry, wanted) in [(&before, &change.before), (&after, &change.after)] {
            if entry.identity(input.home_host).as_ref() != Some(&change.identity) { return Err(error("KCI4052", &change.key, "the reviewed source identity differs from the lock")); }
            if !github::digest(&entry.commit).is_some_and(|c| c.eq_ignore_ascii_case(wanted)) || !reference(&entry.git_ref, &change.reference) {
                return Err(error("KCI4051", &change.key, "the code difference and lock select different commits or refs"));
            }
        }
        after.uses.clear();
        if pins.insert(change.key.clone(), after).is_some() { return Err(error("KCI4054", &change.key, "duplicate reviewed pin")); }
    }
    Ok(pins)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(file: &github::File) -> Pair<'_> { Pair { github: Some(file), ..Default::default() } }

    #[test]
    fn w29_r25_review_binds_both_code_trees_ref_and_stable_identity() -> Result<(), Vec<Diag>> {
        let path = ".github/workflows/ci.yml";
        let key = "acme/tools@v1.2.3";
        let a = "a".repeat(40);
        let m = "b".repeat(40);
        let file = |sha: &str| github::File { workflows: [(path.into(), vec![key.into()])].into(), dependencies: [(key.into(), github::Action { git_ref: "v1.2.3".into(), commit: format!("sha1-{sha}"), owner_id: 41, repo_id: 42, uses: Vec::new(), ..Default::default() })].into(), ..Default::default() };
        let (before, after) = (file(&a), file(&m));
        let mut changes = [Change { key: key.into(), reference: "refs/tags/v1.2.3".into(), before: a.clone(), after: m.clone(), identity: kiln::Identity::Github { hostname: "github.com".into(), owner_id: 41, repo_id: 42 } }];
        let prior = BTreeMap::new();
        let check = |changes: &[Change]| bind(Input { before: pair(&before), after: pair(&after), workflow: path, home_host: "github.com", changes, prior: &prior });
        let approved = check(&changes)?;
        assert_eq!(approved.get(key).and_then(|g| github::digest(&g.commit)), Some(m.as_str()));
        if let Some(change) = changes.first_mut() { change.before = "c".repeat(40); }
        assert!(check(&changes).is_err_and(|ds| ds.iter().any(|d| d.code == "KCI4051")));
        if let Some(change) = changes.first_mut() { change.before = a; change.after = "c".repeat(40); }
        assert!(check(&changes).is_err_and(|ds| ds.iter().any(|d| d.code == "KCI4051")));
        if let Some(change) = changes.first_mut() { change.after = m; change.reference = "refs/heads/different".into(); }
        assert!(check(&changes).is_err_and(|ds| ds.iter().any(|d| d.code == "KCI4051")));
        if let Some(change) = changes.first_mut() { change.reference = "refs/tags/v1.2.3".into(); change.identity = kiln::Identity::Github { hostname: "github.com".into(), owner_id: 41, repo_id: 99 }; }
        assert!(check(&changes).is_err_and(|ds| ds.iter().any(|d| d.code == "KCI4052")));
        Ok(())
    }
}
