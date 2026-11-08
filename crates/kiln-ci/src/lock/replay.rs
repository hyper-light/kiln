//! W29-R25: accepted pins for regeneration after workflow trees have been merged. Text conflict
//! markers are never parsed as a lock, and a submitted branch cannot replace the trunk's pin.

use std::collections::BTreeSet;

use super::{github, kiln, regenerate};
use crate::wf::diag::{Diag, Pos, Range};

#[derive(Clone, Copy, Default)]
pub struct Pair<'a> { pub github: Option<&'a github::File>, pub kiln: Option<&'a kiln::File> }

pub struct Input<'a> {
    pub base: Pair<'a>,
    pub onto: Pair<'a>,
    pub change: Pair<'a>,
    /// Only pins changed by a person's exact approved lock-change revision belong here, never
    /// its unchanged entries. The adapter proves that approval; a newly observed tag is none.
    pub reviewed: Pair<'a>,
    pub home_host: &'a str,
}

/// These are accepted seeds, not executable selections. Regenerate from the actual merged
/// workflow tree to prune unused roots and obtain source identity/reachability proofs.
pub struct Accepted { pub github: Option<github::File>, pub kiln: Option<kiln::File> }

fn conflict(code: &'static str, key: &str, first: &str, second: &str) -> Vec<Diag> {
    let pos = Pos { line: 1, col: 1, offset: 0 };
    vec![Diag::error(code, format!("{key}: replay lock entries disagree: {first}; {second}"), Range { start: pos, end: pos }).at(kiln::PATH)]
}

fn identity(pin: &kiln::Dependency, home: &str) -> Option<kiln::Identity> {
    match pin { kiln::Dependency::Git(git) => git.identity(home), _ => None }
}

/// Approved pins win, then the destination's accepted pins, then the merge base's. A new
/// reference in the change contributes a new pin; movement of an existing ref retains its
/// acceptance. Immutable metadata is collected again at that selected commit by regeneration.
pub fn accepted(input: Input<'_>) -> Result<Accepted, Vec<Diag>> {
    let sources = [input.reviewed, input.onto, input.base, input.change];
    for pair in &sources {
        if let Some(file) = pair.github { file.write(input.home_host)?; }
        if let Some(file) = pair.kiln { file.write()?; }
        if let (Some(gh), Some(kl)) = (pair.github, pair.kiln) { regenerate::compare(gh, kl, input.home_host)?; }
    }
    let mut github = sources.iter().any(|s| s.github.is_some()).then(github::File::default);
    let mut kiln = sources.iter().any(|s| s.kiln.is_some()).then(kiln::File::default);
    for pair in sources {
        if let (Some(into), Some(file)) = (&mut github, pair.github) {
            for (path, roots) in &file.workflows {
                let merged: BTreeSet<_> = into.workflows.entry(path.clone()).or_default().iter().chain(roots).cloned().collect();
                into.workflows.insert(path.clone(), merged.into_iter().collect());
            }
            for (key, pin) in &file.dependencies {
                if let Some(old) = into.dependencies.get_mut(key) {
                    if old.owner_id != pin.owner_id || old.repo_id != pin.repo_id {
                        return Err(conflict("KCI4052", key, &format!("github:{}/{}", old.owner_id, old.repo_id), &format!("github:{}/{}", pin.owner_id, pin.repo_id)));
                    }
                    if old.commit.eq_ignore_ascii_case(&pin.commit) { old.uses = old.uses.iter().chain(&pin.uses).cloned().collect::<BTreeSet<_>>().into_iter().collect(); }
                } else { into.dependencies.insert(key.clone(), pin.clone()); }
            }
        }
        if let (Some(into), Some(file)) = (&mut kiln, pair.kiln) {
            for (path, roots) in &file.workflows {
                let merged: BTreeSet<_> = into.workflows.entry(path.clone()).or_default().iter().chain(roots).cloned().collect();
                into.workflows.insert(path.clone(), merged.into_iter().collect());
            }
            for (key, pin) in &file.dependencies {
                if let Some(old) = into.dependencies.get_mut(key) {
                    if identity(old, input.home_host) != identity(pin, input.home_host) {
                        return Err(conflict("KCI4052", key, &format!("{:?}", identity(old, input.home_host)), &format!("{:?}", identity(pin, input.home_host))));
                    }
                    if std::mem::discriminant(old) != std::mem::discriminant(pin) { return Err(conflict("KCI4054", key, "accepted dependency kind", "submitted dependency kind")); }
                    match (old, pin) {
                        (kiln::Dependency::Git(old), kiln::Dependency::Git(pin)) if old.commit.eq_ignore_ascii_case(&pin.commit) => { old.uses = old.uses.iter().chain(&pin.uses).cloned().collect::<BTreeSet<_>>().into_iter().collect(); }
                        (kiln::Dependency::Runtime(old), kiln::Dependency::Runtime(pin)) if old.sha256.is_none() && pin.sha256.is_none() => {
                            for (platform, archive) in &pin.archives { old.archives.entry(platform.clone()).or_insert_with(|| archive.clone()); }
                        }
                        _ => {}
                    }
                } else { into.dependencies.insert(key.clone(), pin.clone()); }
            }
        }
    }
    if let Some(file) = &github { file.write(input.home_host)?; }
    if let Some(file) = &kiln { file.write()?; }
    if let (Some(gh), Some(kl)) = (&github, &kiln) { regenerate::compare(gh, kl, input.home_host)?; }
    Ok(Accepted { github, kiln })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, key: &str, commit: char) -> github::File {
        github::File { workflows: [(path.into(), vec![key.into()])].into(), dependencies: [(key.into(), github::Action {
            git_ref: key.rsplit_once('@').map(|(_, r)| r).unwrap_or_default().into(), commit: format!("sha1-{}", commit.to_string().repeat(40)), owner_id: 1, repo_id: 2, ..Default::default()
        })].into(), ..Default::default() }
    }

    fn pair(file: &github::File) -> Pair<'_> { Pair { github: Some(file), ..Default::default() } }
    fn native_pair(file: &kiln::File) -> Pair<'_> { Pair { kiln: Some(file), ..Default::default() } }

    #[test]
    fn w29_r25_independent_workflow_changes_regenerate_without_lock_text_merging() -> Result<(), Vec<Diag>> {
        let first = ".github/workflows/first.yml";
        let second = ".github/workflows/second.yml";
        let onto = file(first, "acme/first@v2", 'a');
        let change = file(second, "acme/second@v3", 'b');
        let seed = accepted(Input { base: Pair::default(), onto: pair(&onto), change: pair(&change), reviewed: Pair::default(), home_host: "github.com" })?;
        let workflows = [regenerate::Workflow { path: first.into(), uses: Some(vec!["acme/first@v2".into()]), local: Vec::new() }, regenerate::Workflow { path: second.into(), uses: Some(vec!["acme/second@v3".into()]), local: Vec::new() }];
        let result = regenerate::regenerate(regenerate::Input { github: seed.github.as_ref(), kiln: seed.kiln.as_ref(), workflows: &workflows, resolutions: &[], seeds: &[], home_host: "github.com", mode: regenerate::Mode::Preserve })?;
        let parsed = github::parse(result.github.as_deref().unwrap_or_default())?;
        assert_eq!(parsed.workflows.len(), 2);
        assert_eq!(parsed.dependencies.len(), 2);
        assert_eq!(parsed.dependencies.get("acme/first@v2"), onto.dependencies.get("acme/first@v2"));
        assert_eq!(parsed.dependencies.get("acme/second@v3"), change.dependencies.get("acme/second@v3"));
        Ok(())
    }

    #[test]
    fn w29_r25_a_branch_does_not_replace_the_destinations_pin_without_review() -> Result<(), Vec<Diag>> {
        let path = ".github/workflows/ci.yml";
        let onto = file(path, "acme/tools@v1", 'a');
        let change = file(path, "acme/tools@v1", 'b');
        for reviewed in [false, true] {
            let seed = accepted(Input { base: pair(&onto), onto: pair(&onto), change: pair(&change), reviewed: if reviewed { pair(&change) } else { Pair::default() }, home_host: "github.com" })?;
            assert_eq!(seed.github.as_ref().and_then(|f| f.dependencies.get("acme/tools@v1")), if reviewed { change.dependencies.get("acme/tools@v1") } else { onto.dependencies.get("acme/tools@v1") });
        }
        let mut reclaimed = change;
        if let Some(pin) = reclaimed.dependencies.get_mut("acme/tools@v1") { pin.repo_id = 99; }
        assert!(accepted(Input { base: pair(&onto), onto: pair(&onto), change: pair(&reclaimed), reviewed: pair(&reclaimed), home_host: "github.com" }).is_err_and(|ds| ds.iter().any(|d| d.code == "KCI4052")), "review cannot transfer an old pin to another repository identity");
        Ok(())
    }

    #[test]
    fn w29_r25_native_replay_keeps_git_and_image_pins_and_refuses_reclaimed_identity() -> Result<(), Vec<Diag>> {
        let key = "kiln://acme/tools@v1";
        let image = "image://acme/guest:1";
        let onto = kiln::File {
            workflows: [(".kiln/ci.yaml".into(), vec![key.into(), image.into()])].into(),
            dependencies: [(key.into(), kiln::Dependency::Git(kiln::Git { git_ref: "refs/tags/v1".into(), commit: format!("sha1-{}", "a".repeat(40)), repo: "kiln:original-repo".into(), ..Default::default() })), (image.into(), kiln::Dependency::Image(kiln::Image { digest: format!("sha256:{}", "a".repeat(64)) }))].into(), ..Default::default()
        };
        let mut change = onto.clone();
        if let Some(kiln::Dependency::Git(g)) = change.dependencies.get_mut(key) { g.commit = format!("sha1-{}", "b".repeat(40)); }
        if let Some(kiln::Dependency::Image(i)) = change.dependencies.get_mut(image) { i.digest = format!("sha256:{}", "b".repeat(64)); }
        let result = accepted(Input { base: native_pair(&onto), onto: native_pair(&onto), change: native_pair(&change), reviewed: Pair::default(), home_host: "github.com" })?;
        assert_eq!(result.kiln.as_ref().map(kiln::File::write).transpose()?, Some(onto.write()?));
        if let Some(kiln::Dependency::Git(g)) = change.dependencies.get_mut(key) { g.repo = "kiln:replacement-repo".into(); }
        assert!(accepted(Input { base: native_pair(&onto), onto: native_pair(&onto), change: native_pair(&change), reviewed: native_pair(&change), home_host: "github.com" }).is_err_and(|ds| ds.iter().any(|d| d.code == "KCI4052")));
        Ok(())
    }

    #[test]
    fn w29_r25_same_commit_edges_survive_replay_but_moved_commit_edges_cannot_replace_them() -> Result<(), Vec<Diag>> {
        let path = ".github/workflows/ci.yml";
        let key = "acme/tools@v1";
        let onto = file(path, key, 'a');
        let mut change = onto.clone();
        let child = "acme/child@v1";
        change.dependencies.extend(file(path, child, 'c').dependencies);
        if let Some(pin) = change.dependencies.get_mut(key) { pin.uses.push(child.into()); }
        for same in [true, false] {
            if !same && let Some(pin) = change.dependencies.get_mut(key) { pin.commit = format!("sha1-{}", "b".repeat(40)); }
            let seed = accepted(Input { base: Pair::default(), onto: pair(&onto), change: pair(&change), reviewed: Pair::default(), home_host: "github.com" })?;
            let children = seed.github.as_ref().and_then(|f| f.dependencies.get(key)).map(|p| p.uses.clone()).unwrap_or_default();
            assert_eq!(children, if same { vec![child.to_owned()] } else { Vec::new() });
        }
        Ok(())
    }

    #[test]
    fn w29_r25_new_runtime_platforms_do_not_replace_already_accepted_archives() -> Result<(), Vec<Diag>> {
        let archive = |version: &str, file: &str, hash: char| kiln::Archive { version: version.into(), url: format!("https://nodejs.org/dist/{version}/{file}"), file: file.into(), sha256: hash.to_string().repeat(64) };
        let old = archive("v24.1.0", "node-v24.1.0-linux-x64.tar.gz", 'a');
        let newer = archive("v24.2.0", "node-v24.2.0-linux-x64.tar.gz", 'b');
        let arm = archive("v24.2.0", "node-v24.2.0-darwin-arm64.tar.gz", 'c');
        let onto = kiln::File { workflows: [(".kiln/ci.yaml".into(), vec!["node24".into()])].into(), dependencies: [("node24".into(), kiln::Dependency::Runtime(kiln::Runtime { sha256: None, archives: [("linux/amd64".into(), old.clone())].into() }))].into(), ..Default::default() };
        let mut change = onto.clone();
        if let Some(kiln::Dependency::Runtime(r)) = change.dependencies.get_mut("node24") { r.archives = [("linux/amd64".into(), newer), ("macos/arm64".into(), arm.clone())].into(); }
        let result = accepted(Input { base: Pair::default(), onto: native_pair(&onto), change: native_pair(&change), reviewed: Pair::default(), home_host: "github.com" })?;
        assert!(matches!(result.kiln.as_ref().and_then(|f| f.dependencies.get("node24")), Some(kiln::Dependency::Runtime(r)) if r.archives.len() == 2 && r.archives.get("linux/amd64") == Some(&old) && r.archives.get("macos/arm64") == Some(&arm)));
        Ok(())
    }
}
