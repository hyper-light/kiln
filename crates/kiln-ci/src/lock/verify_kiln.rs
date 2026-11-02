//! Git pins in kiln's supplemental lock (W29-R24). Caller/callee ownership and source reads
//! are supplied by the adapter; a selected pin proves identity and recorded-ref membership.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{github, kiln, verify};
use crate::wf::diag::{Diag, Pos, Range};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitFacts {
    pub identity: kiln::Identity,
    pub reference: String,
    pub current: String,
    pub reachable: BTreeSet<String>,
    pub previous: Option<String>,
}

pub struct Check<'a> {
    pub workflow: &'a str,
    pub key: &'a str,
    pub entry: &'a kiln::Git,
    pub facts: &'a GitFacts,
    pub home_host: &'a str,
    pub policy: verify::Policy,
    pub accepted_moves: &'a BTreeMap<String, String>,
}

#[derive(Debug)]
pub struct Verified {
    pub selection: verify::Selection,
    pub warnings: Vec<Diag>,
    pub moved: Vec<verify::MovedRef>,
}

fn diag(code: &'static str, workflow: &str, message: impl Into<String>, warning: bool) -> Diag {
    let at = Pos { line: 1, col: 1, offset: 0 };
    let range = Range { start: at, end: at };
    (if warning { Diag::warning(code, message, range) } else { Diag::error(code, message, range) }).at(workflow)
}

/// Full-SHA references may reuse a symbolic entry for the same source path and commit.
/// The path of a reusable workflow stays part of its identity, unlike an action's subdirectory.
pub fn pin_for<'a>(file: &'a kiln::File, workflow: &str, uses: &str) -> Option<(&'a str, &'a kiln::Git)> {
    let wanted = kiln::canonical_key(uses)?;
    let closure = file.closure(workflow)?;
    let action = |key: &str| {
        if kiln::workflow_key(key) { return None; }
        let (prefix, key) = key.strip_prefix("kiln://").map_or(("", key), |key| ("kiln://", key));
        github::action_pin(key).map(|key| format!("{prefix}{key}"))
    };
    if closure.contains(&wanted) && let Some((key, kiln::Dependency::Git(g))) = file.dependencies.get_key_value(&wanted) { return Some((key.as_str(), g)); }
    for key in &closure {
        if action(&wanted).as_ref() == Some(key) && let Some(kiln::Dependency::Git(g)) = file.dependencies.get(key) {
            return file.dependencies.get_key_value(key).map(|(key, _)| (key.as_str(), g));
        }
    }
    let (name, wanted) = wanted.split_once('@')?;
    if !matches!(wanted.len(), 40 | 64) || !wanted.bytes().all(|b| b.is_ascii_hexdigit()) { return None; }
    for key in closure {
        if key.split_once('@').is_none_or(|(source, _)| source != name) { continue; }
        if let Some((key, kiln::Dependency::Git(g))) = file.dependencies.get_key_value(&key)
            && github::digest(&g.commit).is_some_and(|c| c.eq_ignore_ascii_case(wanted))
        { return Some((key.as_str(), g)); }
    }
    None
}

pub fn check_git(check: Check<'_>) -> Result<Verified, verify::Refused> {
    let Check { workflow, key, entry, facts, home_host, policy, accepted_moves } = check;
    let policy = policy.stricter(verify::Policy::default());
    let (mut diagnostics, mut moved) = (Vec::new(), Vec::new());
    if let Err(why) = kiln::Dependency::Git(entry.clone()).validate(key, home_host) {
        return Err(verify::Refused { diagnostics: vec![diag("KCI1022", workflow, format!("{key}: {why}"), false)], moved });
    }
    let wanted = github::digest(&entry.commit).unwrap_or_default().to_ascii_lowercase();
    let now = facts.current.to_ascii_lowercase();
    let exact = facts.reference.strip_prefix("refs/tags/").is_some_and(|tag| {
        key.split_once('@').is_some_and(|(_, r)| r.strip_prefix("refs/tags/").unwrap_or(r) == tag)
            && matches!(crate::releases::kind_of(tag), crate::releases::TagKind::Full)
    });
    if exact && let Some(previous) = &facts.previous && !previous.eq_ignore_ascii_case(&now) {
        moved.push(verify::MovedRef { pin: key.into(), before: previous.clone(), now: now.clone() });
        if policy.moved_tags != verify::MovedTags::Refuse && wanted == now && !accepted_moves.get(key).is_some_and(|a| a.eq_ignore_ascii_case(&now)) {
            diagnostics.push(diag("KCI4053", workflow, format!("{key}: full-version tag moved from accepted {previous} to {now}; changing the lock needs a person's review"), false));
        }
        if policy.moved_tags == verify::MovedTags::Refuse && !accepted_moves.get(key).is_some_and(|a| a.eq_ignore_ascii_case(&now)) {
            diagnostics.push(diag("KCI4056", workflow, format!("{key}: full-version tag moved from {previous} to {now}; it needs an explicitly reviewed lock change"), false));
        }
    }
    if entry.identity(home_host).as_ref() != Some(&facts.identity) {
        diagnostics.push(diag("KCI4052", workflow, format!("{key}: repository identity differs from locked {:?}; source is {:?}", entry.identity(home_host), facts.identity), false));
    } else if facts.reference != entry.git_ref || !matches!(now.len(), 40 | 64) || !now.bytes().all(|b| b.is_ascii_hexdigit())
        || !(now == wanted || facts.reachable.iter().any(|c| c.eq_ignore_ascii_case(&wanted)))
    {
        diagnostics.push(diag("KCI4051", workflow, format!("{key}: locked {wanted} is not proven reachable from recorded ref {:?} in its own repository", entry.git_ref), false));
    } else if now != wanted {
        if exact { diagnostics.push(diag("KCI4053", workflow, format!("{key}: full-version tag now names {now}, where the lock names {wanted}"), false)); }
        else if key.split_once('@').is_none_or(|(_, r)| !(matches!(r.len(), 40 | 64) && r.bytes().all(|b| b.is_ascii_hexdigit()))) {
            diagnostics.push(diag("KCI4057", workflow, format!("{key}: ref moved to {now}; the locked {wanted} runs"), true));
        }
    }
    if diagnostics.iter().any(Diag::is_error) { return Err(verify::Refused { diagnostics, moved }); }
    Ok(Verified { selection: verify::Selection { pin: key.into(), commit: wanted, locked: true }, warnings: diagnostics, moved })
}

#[cfg(test)]
mod tests;
