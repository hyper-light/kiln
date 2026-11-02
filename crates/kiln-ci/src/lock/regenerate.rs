//! The two lockfiles are one pure result (W29-R24/R25). Supplied metadata describes exact
//! commits; ordinary regeneration retains accepted pins and never borrows a moved ref's
//! dependency edges. Source I/O and reviewed acceptance of proposed changes live outside.

use std::collections::{BTreeMap, BTreeSet};

use super::{github, kiln};
use kiln::{Dependency, Git, Identity};
use crate::wf::diag::{Diag, Pos, Range};

mod source;
pub use source::{SourceCheck, accepted, source};
mod images;
pub use images::{ImageContext, images};

/// The adapter expands local actions/builtins and supplies their external dependencies.
/// None removes a workflow. Unmentioned workflows and their accepted pins are retained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workflow { pub path: String, pub uses: Option<Vec<String>> }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution { pub key: String, pub dependency: Dependency }

/// A callee's accepted pins, read at its selected workflow commit. Caller pins take priority.
#[derive(Debug, Clone)]
pub struct Seed { pub source: String, pub pins: Vec<Resolution> }

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Preserve,
    /// Produce a draft of new pins. This is not trusted acceptance of a moved full version.
    Propose,
}

pub struct Input<'a> {
    pub github: Option<&'a github::File>,
    pub kiln: Option<&'a kiln::File>,
    pub workflows: &'a [Workflow],
    pub resolutions: &'a [Resolution],
    pub seeds: &'a [Seed],
    pub home_host: &'a str,
    pub mode: Mode,
}

/// None removes a now-empty file (only when no workflow entries or pins remain). Empty
/// onboarded workflows still produce a file. Either both byte strings succeed or neither does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Files { pub github: Option<String>, pub kiln: Option<String> }

fn diag(code: &'static str, path: &str, message: impl Into<String>) -> Diag {
    let at = Pos { line: 1, col: 1, offset: 0 };
    Diag::error(code, message, Range { start: at, end: at }).at(path)
}

fn conflict(key: &str, a: &Entry, b: &Entry) -> Diag {
    let mut entries = [format!("{} = {:?}", a.source, a.pin), format!("{} = {:?}", b.source, b.pin)];
    entries.sort();
    diag("KCI4054", kiln::PATH, format!("conflicting lock entries for {key}: {}", entries.join("; ")))
}

#[derive(Debug, Clone)]
struct Entry { pin: Dependency, source: String }

fn git_ref_equal(a: &str, b: &str) -> bool {
    a == b || ["refs/heads/", "refs/tags/"].iter().any(|p| a.strip_prefix(p) == Some(b) || b.strip_prefix(p) == Some(a))
        || a == "HEAD" && b.starts_with("refs/heads/") || b == "HEAD" && a.starts_with("refs/heads/")
}

fn same_pin(a: &Dependency, b: &Dependency, home: &str) -> bool {
    match (a, b) {
        (Dependency::Git(a), Dependency::Git(b)) => a.identity(home) == b.identity(home) && a.identity(home).is_some()
            && a.commit.eq_ignore_ascii_case(&b.commit) && git_ref_equal(&a.git_ref, &b.git_ref),
        (Dependency::Image(a), Dependency::Image(b)) => a.digest.eq_ignore_ascii_case(&b.digest),
        (Dependency::Runtime(a), Dependency::Runtime(b)) => match (&a.sha256, &b.sha256) {
            (Some(hash), None) => !b.archives.is_empty() && b.archives.values().all(|archive| archive.sha256.eq_ignore_ascii_case(hash)),
            (None, Some(hash)) => !a.archives.is_empty() && a.archives.values().all(|archive| archive.sha256.eq_ignore_ascii_case(hash)),
            _ => a.sha256 == b.sha256 && a.archives.iter().all(|(p, a)| b.archives.get(p).is_none_or(|b| a == b)),
        },
        _ => false,
    }
}

fn normalized(key: &str) -> Result<Option<String>, Box<Diag>> {
    if key.contains("${{") { return Err(Box::new(diag("KCI4050", kiln::PATH, format!("{key}: resolve the dependency expression before locking")))); }
    if key.starts_with("./") || key.starts_with("$/") || key.starts_with("kiln/") { return Ok(None); }
    kiln::canonical_key(key).map(Some).ok_or_else(|| Box::new(diag("KCI4050", kiln::PATH, format!("{key}: a lock needs a literal dependency reference"))))
}

fn normalize_pin(mut pin: Dependency) -> Result<Dependency, Box<Diag>> {
    if let Dependency::Git(g) = &mut pin {
        g.commit = g.commit.to_ascii_lowercase();
        if let Some(Identity::Github { owner_id, repo_id, .. }) = g.identity("github.com") { g.repo = format!("github:{owner_id}/{repo_id}"); }
        let mut uses = BTreeSet::new();
        for key in &g.uses { if let Some(key) = normalized(key)? { uses.insert(key); } }
        g.uses = uses.into_iter().collect();
    }
    if let Dependency::Image(i) = &mut pin { i.digest = i.digest.to_ascii_lowercase(); }
    if let Dependency::Runtime(r) = &mut pin {
        r.sha256 = r.sha256.as_ref().map(|s| s.to_ascii_lowercase());
        for a in r.archives.values_mut() { a.sha256 = a.sha256.to_ascii_lowercase(); }
    }
    Ok(pin)
}

fn merge(a: &mut Entry, b: Entry, home: &str) -> Result<(), Box<Diag>> {
    if !same_pin(&a.pin, &b.pin, home) { return Err(Box::new(conflict("one dependency", a, &b))); }
    match (&mut a.pin, b.pin) {
        (Dependency::Git(a), Dependency::Git(b)) => {
            if !a.git_ref.starts_with("refs/") && b.git_ref.starts_with("refs/") { a.git_ref = b.git_ref; }
            if a.hostname.is_none() { a.hostname = b.hostname; }
            a.uses = a.uses.iter().chain(&b.uses).cloned().collect::<BTreeSet<_>>().into_iter().collect();
        }
        (Dependency::Runtime(a), Dependency::Runtime(b)) => {
            // W29-R24: a checksum can gain a versioned location only for those same bytes.
            // The wire format deliberately uses either shorthand or archives, never both.
            if !b.archives.is_empty() { a.sha256 = None; }
            a.archives.extend(b.archives);
        }
        _ => {}
    }
    Ok(())
}

fn add(pool: &mut BTreeMap<String, Entry>, key: String, entry: Entry, home: &str) -> Result<(), Box<Diag>> {
    if let Some(old) = pool.get_mut(&key) {
        if !same_pin(&old.pin, &entry.pin, home) { return Err(Box::new(conflict(&key, old, &entry))); }
        merge(old, entry, home)?;
    } else { pool.insert(key, entry); }
    Ok(())
}

fn from_github(action: &github::Action) -> Dependency {
    Dependency::Git(Git { git_ref: action.git_ref.clone(), commit: action.commit.clone(), repo: format!("github:{}/{}", action.owner_id, action.repo_id), hostname: action.hostname.clone(), uses: action.uses.clone() })
}

fn lookup<'a>(pool: &'a BTreeMap<String, Entry>, key: &str) -> Option<&'a Entry> {
    pool.get(key).or_else(|| github::action_pin(key).and_then(|key| pool.get(&key))).or_else(|| {
        let (name, requested) = key.split_once('@')?;
        if !matches!(requested.len(), 40 | 64) || !requested.bytes().all(|b| b.is_ascii_hexdigit()) { return None; }
        pool.iter().find_map(|(key, entry)| {
            let Dependency::Git(g) = &entry.pin else { return None };
            let (source, _) = key.split_once('@')?;
            (source == name && github::digest(&g.commit).is_some_and(|c| c.eq_ignore_ascii_case(requested))).then_some(entry)
        })
    })
}

fn workflow_path(path: &str) -> bool {
    !path.is_empty() && path.len() <= 4096 && !path.starts_with('/') && !path.contains(['\\', ':']) && !path.chars().any(|c| c.is_control() || c.is_whitespace())
        && path.split('/').all(|p| !p.is_empty() && p != "." && p != "..")
}

/// Both files may name one action, including an action's subdirectory spelling. The two
/// entries must select the same immutable tree and stable repository identity.
pub fn compare(github: &github::File, kiln: &kiln::File, home: &str) -> Result<(), Vec<Diag>> {
    let mut diagnostics = Vec::new();
    for workflow in github.workflows.keys().filter(|p| kiln.workflows.contains_key(*p)) {
        let closure = github.closure(workflow).unwrap_or_default();
        for key in kiln.closure(workflow).unwrap_or_default() {
            if kiln::workflow_key(&key) { continue; }
            let Some(requested) = github::action_pin(&key) else { continue };
            let Some(native) = kiln.dependencies.get(&key) else { continue };
            let mut matching = BTreeMap::new();
            if let Some((pin, action)) = super::verify::pin_for(github, workflow, &requested) { matching.insert(pin, action); }
            for pin in &closure {
                if let (Some((repo, reference)), Some((wanted_repo, wanted_ref))) = (pin.split_once('@'), requested.split_once('@'))
                    && repo == wanted_repo && git_ref_equal(reference, wanted_ref) && let Some(action) = github.dependencies.get(pin)
                { matching.insert(pin.as_str(), action); }
            }
            for (pin, action) in matching {
                let a = Entry { pin: from_github(action), source: format!("{}:{pin}", github::PATH) };
                let b = Entry { pin: native.clone(), source: format!("{}:{key}", kiln::PATH) };
                if !same_pin(&a.pin, &b.pin, home) { diagnostics.push(conflict(&key, &a, &b).at(workflow)); }
            }
        }
    }
    diagnostics.sort_by(|a, b| (&a.path, &a.message).cmp(&(&b.path, &b.message)));
    diagnostics.dedup();
    if diagnostics.is_empty() { Ok(()) } else { Err(diagnostics) }
}

struct Graph<'a> {
    accepted: BTreeMap<String, Entry>,
    seeds: BTreeMap<String, Entry>,
    facts: BTreeMap<String, Entry>,
    chosen: BTreeMap<String, Dependency>,
    home: &'a str,
    mode: Mode,
}

impl Graph<'_> {
    fn choose(&self, key: &str, preserve: bool) -> Result<Dependency, Box<Diag>> {
        let accepted = lookup(&self.accepted, key);
        let seed = lookup(&self.seeds, key);
        let fact = lookup(&self.facts, key);
        let old = accepted.or(seed);
        let selected = if self.mode == Mode::Propose && !preserve { fact.or(old) } else { old.or(fact) };
        let Some(selected) = selected else { return Err(Box::new(diag("KCI4050", kiln::PATH, format!("{key}: no supplied source facts or accepted lock entry")))) };
        let mut selected = selected.clone();
        // A different owner/repository cannot inherit a prior pin, even if it has that object.
        if let (Some(Entry { pin: Dependency::Git(old), .. }), Some(Entry { pin: Dependency::Git(now), .. })) = (old, fact)
            && (old.identity(self.home).is_none() || old.identity(self.home) != now.identity(self.home))
        {
            let message = format!("{key}: repository identity differs between {} ({:?}) and supplied source ({:?})", selected.source, old.identity(self.home), now.identity(self.home));
            return Err(Box::new(diag("KCI4052", kiln::PATH, message)));
        }
        if let Some(fact) = fact {
            if same_pin(&selected.pin, &fact.pin, self.home) { merge(&mut selected, fact.clone(), self.home)?; }
            // GitHub's file may have a bare recorded ref. Qualify it from honest source facts,
            // never by guessing that a version-shaped ref is a tag. New commit metadata is not
            // used as dependency edges for the old accepted commit.
            if let (Dependency::Git(old), Dependency::Git(now)) = (&mut selected.pin, &fact.pin)
                && !old.git_ref.starts_with("refs/") && now.git_ref.starts_with("refs/") && git_ref_equal(&old.git_ref, &now.git_ref)
            { old.git_ref = now.git_ref.clone(); }
            if let (Dependency::Runtime(old), Dependency::Runtime(now)) = (&mut selected.pin, &fact.pin)
                && old.sha256.is_none() && now.sha256.is_none()
            {
                for (platform, archive) in &now.archives { old.archives.entry(platform.clone()).or_insert_with(|| archive.clone()); }
            }
        }
        Ok(selected.pin)
    }

    fn expand(&mut self, roots: impl IntoIterator<Item = String>, preserve: bool) -> Result<(), Vec<Diag>> {
        let mut todo: BTreeSet<_> = roots.into_iter().collect();
        while let Some(key) = todo.pop_first() {
            if self.chosen.contains_key(&key) { continue; }
            if self.chosen.len() >= kiln::MAX_ITEMS { return Err(vec![diag("KCI1022", kiln::PATH, "dependency graph exceeds 4096 nodes")]); }
            let pin = self.choose(&key, preserve).map_err(|d| vec![*d])?;
            todo.extend(pin.uses().iter().cloned());
            self.chosen.insert(key, pin);
        }
        Ok(())
    }

    fn github_action(&self, key: &str) -> Option<String> {
        match self.chosen.get(key)? {
            Dependency::Git(g) if !kiln::workflow_key(key) && matches!(g.identity(self.home), Some(Identity::Github { .. })) => github::action_pin(key),
            _ => None,
        }
    }

    fn needs_supplement(&self, root: &str) -> bool {
        let (mut todo, mut seen) = (vec![root], BTreeSet::new());
        while let Some(key) = todo.pop() {
            if !seen.insert(key) { continue; }
            if self.github_action(key).is_none() { return true; }
            if let Some(pin) = self.chosen.get(key) { todo.extend(pin.uses().iter().map(String::as_str)); }
        }
        false
    }

    fn cycle(&self) -> Option<String> {
        let (mut active, mut done) = (BTreeSet::new(), BTreeSet::new());
        for root in self.chosen.keys() {
            let mut todo = vec![(root.as_str(), false)];
            while let Some((key, leaving)) = todo.pop() {
                if leaving { active.remove(key); done.insert(key); continue; }
                if done.contains(key) { continue; }
                if !active.insert(key) { return Some(key.into()); }
                todo.push((key, true));
                if let Some(pin) = self.chosen.get(key) { todo.extend(pin.uses().iter().rev().map(|u| (u.as_str(), false))); }
            }
        }
        None
    }
}

/// W29-R24/R25: pure, atomic regeneration. No timestamp, network, clock, or host writes.
pub fn regenerate(input: Input<'_>) -> Result<Files, Vec<Diag>> {
    if !github::hostname(input.home_host) { return Err(vec![diag("KCI1022", github::PATH, "unsupported GitHub home hostname")]); }
    let size = |pin: &Dependency| 1usize.saturating_add(pin.uses().len()).saturating_add(match pin { Dependency::Runtime(r) => r.archives.len(), _ => 0 });
    let count = input.workflows.iter().fold(0usize, |n, w| n.saturating_add(1).saturating_add(w.uses.as_ref().map_or(0, Vec::len)));
    let count = input.resolutions.iter().chain(input.seeds.iter().flat_map(|s| &s.pins)).fold(count, |n, r| n.saturating_add(size(&r.dependency)));
    if count > kiln::MAX_ITEMS {
        return Err(vec![diag("KCI1022", kiln::PATH, "regeneration input exceeds 4096 entries")]);
    }
    let mut gh = input.github.cloned().unwrap_or_default();
    let mut kl = input.kiln.cloned().unwrap_or_default();
    gh.write(input.home_host)?;
    kl.write()?;
    compare(&gh, &kl, input.home_host)?;
    let (mut accepted_gh, mut accepted_kl, mut facts) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
    let gh_reachable: BTreeSet<_> = gh.workflows.keys().flat_map(|w| gh.closure(w).unwrap_or_default()).collect();
    let kl_reachable: BTreeSet<_> = kl.workflows.keys().flat_map(|w| kl.closure(w).unwrap_or_default()).collect();
    for (key, action) in gh.dependencies.iter().filter(|(k, _)| gh_reachable.contains(*k)) {
        accepted_gh.insert(key.clone(), Entry { pin: from_github(action), source: format!("{}:{key}", github::PATH) });
    }
    for (key, pin) in kl.dependencies.iter().filter(|(k, _)| kl_reachable.contains(*k)) {
        accepted_kl.insert(key.clone(), Entry { pin: normalize_pin(pin.clone()).map_err(|d| vec![*d])?, source: format!("{}:{key}", kiln::PATH) });
    }
    for resolution in input.resolutions {
        let Some(key) = normalized(&resolution.key).map_err(|d| vec![*d])? else { continue };
        let pin = normalize_pin(resolution.dependency.clone()).map_err(|d| vec![*d])?;
        pin.validate(&key, input.home_host).map_err(|m| vec![diag("KCI4051", &key, m)])?;
        add(&mut facts, key, Entry { pin, source: format!("supplied facts: {}", resolution.key) }, input.home_host).map_err(|d| vec![*d])?;
    }
    let mut edits = BTreeMap::new();
    for edit in input.workflows {
        if !workflow_path(&edit.path) { return Err(vec![diag("KCI1022", &edit.path, "workflow needs a safe repository-relative path")]); }
        let roots = edit.uses.as_ref().map(|uses| {
            let mut roots = BTreeSet::new();
            for key in uses { if let Some(key) = normalized(key)? { roots.insert(key); } }
            Ok::<_, Box<Diag>>(roots.into_iter().collect::<Vec<_>>())
        }).transpose().map_err(|d| vec![(*d).at(&edit.path)])?;
        if edits.get(&edit.path).is_some_and(|old| old != &roots) { return Err(vec![diag("KCI4054", &edit.path, "two different workflow edits were supplied")]); }
        edits.insert(edit.path.clone(), roots);
    }
    for path in edits.keys() { gh.workflows.remove(path); kl.workflows.remove(path); }
    let paths: BTreeSet<_> = gh.workflows.keys().chain(kl.workflows.keys()).chain(edits.iter().filter(|(_, roots)| roots.is_some()).map(|(p, _)| p)).cloned().collect();
    let (mut native, mut actions) = (BTreeMap::<String, Entry>::new(), BTreeMap::<String, Entry>::new());
    let mut seeds: Vec<_> = input.seeds.iter().collect();
    seeds.sort_by(|a, b| a.source.cmp(&b.source));
    for path in paths {
        let edited = edits.get(&path).and_then(Option::as_ref);
        let roots = edited.cloned().unwrap_or_else(|| gh.workflows.get(&path).into_iter().chain(kl.workflows.get(&path)).flatten().cloned().collect());
        let is_github = if edited.is_some() { path.starts_with(".github/workflows/") } else { gh.workflows.contains_key(&path) };
        let mut accepted = if is_github { accepted_gh.clone() } else { accepted_kl.clone() };
        if is_github {
            let explicit: BTreeSet<_> = input.kiln.and_then(|f| f.closure(&path)).unwrap_or_default().into_iter().collect();
            for (key, entry) in &accepted_kl {
                // GitHub actions in another native workflow may intentionally lock a different
                // commit. Borrow only this workflow's explicit supplemental action entries.
                if github::action_pin(key).is_some() && matches!(&entry.pin, Dependency::Git(g) if matches!(g.identity(input.home_host), Some(Identity::Github { .. }))) && !explicit.contains(key) { continue; }
                add(&mut accepted, key.clone(), entry.clone(), input.home_host).map_err(|d| vec![*d])?;
            }
        }
        let mut graph = Graph { accepted, seeds: BTreeMap::new(), facts: facts.clone(), chosen: BTreeMap::new(), home: input.home_host, mode: input.mode };
        for seed in &seeds {
            for resolution in &seed.pins {
                let Some(key) = normalized(&resolution.key).map_err(|d| vec![*d])? else { continue };
                if lookup(&graph.accepted, &key).is_some() { continue; }
                let pin = normalize_pin(resolution.dependency.clone()).map_err(|d| vec![*d])?;
                pin.validate(&key, input.home_host).map_err(|m| vec![diag("KCI4051", &seed.source, m)])?;
                add(&mut graph.seeds, key, Entry { pin, source: seed.source.clone() }, input.home_host).map_err(|d| vec![*d])?;
            }
        }
        graph.expand(roots.iter().cloned(), edited.is_none())?;
        if let Some(key) = graph.cycle() { return Err(vec![diag("KCI1022", &path, format!("dependency graph has a cycle at {key}"))]); }
        if edited.is_some() {
            if is_github {
                gh.workflows.insert(path.clone(), roots.iter().filter_map(|key| graph.github_action(key)).collect::<BTreeSet<_>>().into_iter().collect());
                let supplemental: Vec<_> = roots.into_iter().filter(|key| graph.needs_supplement(key)).collect();
                if !supplemental.is_empty() || input.kiln.is_some_and(|f| f.workflows.contains_key(&path)) { kl.workflows.insert(path.clone(), supplemental); }
            } else { kl.workflows.insert(path.clone(), roots); }
        }
        // A supplemental action retains its complete accepted graph. Moved metadata cannot
        // replace its old image/runtime edges when the workflow is regenerated later.
        let mut todo: BTreeSet<_> = kl.workflows.get(&path).into_iter().flatten().cloned().collect();
        let mut seen = BTreeSet::new();
        while let Some(key) = todo.pop_first() {
            if !seen.insert(key.clone()) { continue; }
            let pin = graph.chosen.get(&key).ok_or_else(|| vec![diag("KCI4050", &path, format!("{key}: missing native dependency"))])?;
            if let Dependency::Git(g) = pin && !g.git_ref.starts_with("refs/") && !matches!(g.git_ref.len(), 40 | 64) {
                return Err(vec![diag("KCI4051", &path, format!("{key}: supply its qualified source ref before writing a supplemental entry"))]);
            }
            todo.extend(pin.uses().iter().cloned());
            add(&mut native, key, Entry { pin: pin.clone(), source: format!("{}:{path}", kiln::PATH) }, input.home_host).map_err(|d| vec![*d])?;
        }
        let mut todo: BTreeSet<_> = gh.workflows.get(&path).into_iter().flatten().cloned().collect();
        let mut seen = BTreeSet::new();
        while let Some(key) = todo.pop_first() {
            if !seen.insert(key.clone()) { continue; }
            // GitHub stores an NWO pin for all the sub-actions in that same immutable tree.
            let entries: Vec<_> = graph.chosen.iter().filter(|(k, _)| graph.github_action(k).as_ref() == Some(&key)).collect();
            if entries.is_empty() { return Err(vec![diag("KCI4050", &path, format!("{key}: missing GitHub action"))]); }
            for (_, entry) in entries {
                let Dependency::Git(g) = entry else { continue };
                let Some(Identity::Github { hostname, owner_id, repo_id }) = g.identity(input.home_host) else { continue };
                let reference = key.split_once('@').map(|(_, r)| r).unwrap_or_default();
                let git_ref = if matches!(reference.len(), 40 | 64) && reference.bytes().all(|b| b.is_ascii_hexdigit()) { g.git_ref.clone() } else { reference.into() };
                let uses: Vec<_> = g.uses.iter().filter_map(|k| graph.github_action(k)).filter(|k| k != &key).collect::<BTreeSet<_>>().into_iter().collect();
                todo.extend(uses.iter().cloned());
                let action = github::Action { hostname: Some(hostname), git_ref, commit: g.commit.clone(), owner_id, repo_id, uses };
                add(&mut actions, key.clone(), Entry { pin: from_github(&action), source: format!("{}:{path}", github::PATH) }, input.home_host).map_err(|d| vec![*d])?;
            }
        }
    }
    kl.dependencies = native.into_iter().map(|(key, entry)| (key, entry.pin)).collect();
    gh.dependencies = actions.into_iter().filter_map(|(key, entry)| {
        let Dependency::Git(g) = entry.pin else { return None };
        let Some(Identity::Github { hostname, owner_id, repo_id }) = g.identity(input.home_host) else { return None };
        Some((key, github::Action { hostname: Some(hostname), git_ref: g.git_ref, commit: g.commit, owner_id, repo_id, uses: g.uses }))
    }).collect();
    compare(&gh, &kl, input.home_host)?;
    let github = if gh.workflows.is_empty() && gh.dependencies.is_empty() { None } else { Some(gh.write(input.home_host)?) };
    let kiln = if kl.workflows.is_empty() && kl.dependencies.is_empty() { None } else { Some(kl.write()?) };
    Ok(Files { github, kiln })
}

#[cfg(test)]
mod tests;
