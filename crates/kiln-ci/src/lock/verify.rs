//! Pin verification and opt-in policy (W29-R23, R26). Every source fact is supplied by the
//! adapter. This module never resolves a ref, invents ancestry, accepts a move or starts work.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::github as format;
use crate::wf::diag::{Diag, Pos, Range};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Require {
    #[default]
    Off,
    Report,
    Enforce,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MovedTags {
    #[default]
    Flag,
    Refuse,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    pub require: Require,
    pub moved_tags: MovedTags,
}

impl Policy {
    /// Trusted server and trunk policies only; the commit being checked cannot loosen either.
    pub fn stricter(self, other: Self) -> Self {
        let require = self.require.max(other.require);
        let moved_tags = if require == Require::Enforce { MovedTags::Refuse } else { self.moved_tags.max(other.moved_tags) };
        Self { require, moved_tags }
    }
}

/// Facts about the exact recorded ref in its own source repository. Reachable commits are
/// proofs anchored at reference, not merely objects a hosting API happened to return.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GitFacts {
    pub hostname: String,
    pub owner_id: i64,
    pub repo_id: i64,
    /// A fully qualified branch or tag; a bare SHA cannot establish repository provenance.
    pub reference: String,
    pub current: String,
    pub reachable: BTreeSet<String>,
    /// Previously accepted resolution, from trusted persistent history.
    pub previous: Option<String>,
    /// W29-R23: the source's actual request, retained when HEAD resolves to a named default
    /// branch. A bare commit is still no proof of membership in a branch or tag.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub requested_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    /// The serialized pin used, which can differ from a workflow's full-SHA spelling.
    pub pin: String,
    /// Raw lowercase commit SHA, ready for the existing action download path.
    pub commit: String,
    pub locked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovedRef {
    pub pin: String,
    pub before: String,
    pub now: String,
}

#[derive(Debug)]
pub struct Verified {
    pub onboarded: bool,
    pub pins: BTreeMap<String, Selection>,
    pub warnings: Vec<Diag>,
    pub moved: Vec<MovedRef>,
}

#[derive(Debug)]
pub struct Refused {
    pub diagnostics: Vec<Diag>,
    pub moved: Vec<MovedRef>,
}

/// Uses reached in the workflow and the locked composite metadata. The adapter expands only
/// verified commits, checking again as it discovers dependencies before scheduling any job.
pub struct GithubCheck<'a> {
    pub workflow: &'a str,
    pub uses: &'a [String],
    pub file: Option<&'a format::File>,
    /// By canonical serialized pin, at the entry's recorded ref.
    pub facts: &'a BTreeMap<String, GitFacts>,
    pub home_host: &'a str,
    pub policy: Policy,
    /// An explicit reviewed acceptance from trusted state, never inferred from a changed file.
    pub accepted_moves: &'a BTreeMap<String, String>,
}

fn diag(code: &'static str, workflow: &str, message: impl Into<String>, warning: bool) -> Diag {
    let at = Pos { line: 1, col: 1, offset: 0 };
    let range = Range { start: at, end: at };
    let d = if warning { Diag::warning(code, message, range) } else { Diag::error(code, message, range) };
    d.at(workflow)
}

fn sha(value: &str) -> Option<String> {
    let value = format::digest(value).unwrap_or(value);
    (matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())).then(|| value.to_ascii_lowercase())
}

fn recorded_ref_matches(written: &str, facts: &GitFacts) -> bool {
    if written.starts_with("refs/") { return written == facts.reference; }
    if matches!(written.len(), 40 | 64) && written.bytes().all(|b| b.is_ascii_hexdigit()) { return false; }
    ["refs/heads/", "refs/tags/"].iter().any(|prefix| facts.reference.strip_prefix(*prefix).is_some_and(|name| !name.is_empty() && (written == facts.reference || written == name)))
        || (written == "HEAD" && facts.requested_ref == "HEAD" && facts.reference.strip_prefix("refs/heads/").is_some_and(|b| !b.is_empty()))
}

fn exact_tag(pin: &str, facts: &GitFacts) -> bool {
    let Some(tag) = facts.reference.strip_prefix("refs/tags/") else { return false };
    let Some((_, git_ref)) = pin.split_once('@') else { return false };
    let git_ref = git_ref.strip_prefix("refs/tags/").unwrap_or(git_ref);
    git_ref == tag && matches!(crate::releases::kind_of(tag), crate::releases::TagKind::Full)
}

/// The official checker accepts a workflow's full SHA when this workflow already locks that
/// same repository and SHA through a symbolic ref. A pin in another workflow grants nothing.
pub fn pin_for<'a>(file: &'a format::File, workflow: &str, requested: &str) -> Option<(&'a str, &'a format::Action)> {
    let closure = file.closure(workflow)?;
    if closure.iter().any(|p| p == requested) {
        return file.dependencies.get_key_value(requested).map(|(key, a)| (key.as_str(), a));
    }
    let (repo, git_ref) = requested.split_once('@')?;
    let wanted = sha(git_ref)?;
    for key in closure {
        let (owner, _) = key.split_once('@')?;
        if owner != repo { continue; }
        if let Some((key, a)) = file.dependencies.get_key_value(&key)
            && format::digest(&a.commit).is_some_and(|s| s.eq_ignore_ascii_case(&wanted))
        {
            return Some((key.as_str(), a));
        }
    }
    None
}

/// W29-R23/R26: return executable selections only when every relevant lock check passed.
pub fn check_github(check: GithubCheck<'_>) -> Result<Verified, Refused> {
    let policy = check.policy.stricter(Policy::default());
    let onboarded = check.file.is_some_and(|f| f.workflows.contains_key(check.workflow));
    let (mut pins, mut diagnostics, mut moved) = (BTreeMap::new(), Vec::new(), Vec::new());
    if check.file.is_some_and(|f| f.version != format::VERSION) {
        return Err(Refused { diagnostics: vec![diag("KCI1022", check.workflow, "the normalized lock format is unsupported", false)], moved });
    }
    if !onboarded && policy.require != Require::Off {
        diagnostics.push(diag("KCI4055", check.workflow, "Require lockfile: this workflow has no lockfile entry", policy.require == Require::Report));
    }
    let requested: BTreeSet<_> = check.uses.iter().filter_map(|u| format::action_pin(u)).collect();
    for requested in requested {
        let locked = check.file.filter(|_| onboarded).and_then(|f| pin_for(f, check.workflow, &requested));
        if onboarded && locked.is_none() {
            diagnostics.push(diag("KCI4050", check.workflow, format!("{requested}: an action this locked workflow reaches is missing from its lock"), false));
            continue;
        }
        let key = locked.map_or(requested.as_str(), |(key, _)| key);
        let Some(facts) = check.facts.get(key) else {
            if onboarded {
                diagnostics.push(diag("KCI4051", check.workflow, format!("{key}: source identity and recorded-ref history could not be verified"), false));
            }
            continue;
        };
        let now = sha(&facts.current);
        if !onboarded && let Some(now) = &now {
            let reference = requested.split_once('@').map(|(_, r)| r).unwrap_or_default();
            let bound = sha(reference).map_or_else(|| recorded_ref_matches(reference, facts), |s| s == *now);
            if !bound {
                diagnostics.push(diag("KCI4051", check.workflow, format!("{key}: source facts do not name the requested ref"), false));
                continue;
            }
        }
        if exact_tag(key, facts)
            && let (Some(before), Some(now)) = (facts.previous.as_deref(), now.as_ref())
            && !before.eq_ignore_ascii_case(now)
        {
            moved.push(MovedRef { pin: key.to_owned(), before: before.to_owned(), now: now.clone() });
            let accepted = check.accepted_moves.get(key).is_some_and(|a| a.eq_ignore_ascii_case(now));
            if policy.moved_tags == MovedTags::Refuse && !accepted {
                diagnostics.push(diag("KCI4056", check.workflow, format!("{key}: full-version tag moved from {before} to {now}; it needs an explicitly reviewed lock change"), false));
            }
        }
        let Some((_, entry)) = locked else {
            if let Some(now) = now {
                pins.insert(requested.clone(), Selection { pin: key.to_owned(), commit: now, locked: false });
            }
            continue;
        };
        let expected_host = entry.hostname.as_deref().unwrap_or(check.home_host);
        if !format::hostname(expected_host) || facts.hostname != expected_host || entry.owner_id <= 0 || entry.repo_id <= 0 || facts.owner_id != entry.owner_id || facts.repo_id != entry.repo_id {
            diagnostics.push(diag("KCI4052", check.workflow, format!("{key}: repository identity differs from locked {expected_host}:{}/{}; source is {}:{}/{}", entry.owner_id, entry.repo_id, facts.hostname, facts.owner_id, facts.repo_id), false));
            continue;
        }
        let Some(wanted) = format::digest(&entry.commit).map(str::to_ascii_lowercase) else {
            diagnostics.push(diag("KCI1022", check.workflow, format!("{key}: invalid locked commit digest"), false));
            continue;
        };
        if !recorded_ref_matches(&entry.git_ref, facts)
            || now.is_none()
            || !(now.as_ref() == Some(&wanted) || facts.reachable.iter().any(|s| s.eq_ignore_ascii_case(&wanted)))
        {
            diagnostics.push(diag("KCI4051", check.workflow, format!("{key}: locked {wanted} is not proven reachable from recorded ref {:?} in its own repository", entry.git_ref), false));
            continue;
        }
        if now.as_ref().is_some_and(|now| now != &wanted) {
            if exact_tag(key, facts) {
                diagnostics.push(diag("KCI4053", check.workflow, format!("{key}: full-version tag now names {}, where the lock names {wanted}", facts.current), false));
                continue;
            }
            let immutable_key = key.split_once('@').is_some_and(|(_, r)| sha(r).is_some());
            if !immutable_key {
                diagnostics.push(diag("KCI4057", check.workflow, format!("{key}: ref moved to {}; the locked {wanted} runs", facts.current), true));
            }
        }
        let selection = Selection { pin: key.to_owned(), commit: wanted, locked: true };
        pins.insert(requested, selection);
    }
    if diagnostics.iter().any(Diag::is_error) {
        Err(Refused { diagnostics, moved })
    } else {
        Ok(Verified { onboarded, pins, warnings: diagnostics, moved })
    }
}

#[cfg(test)]
mod tests;
