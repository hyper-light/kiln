//! Going back (docs/cd.md, "Rollback"): which deployment a rollback goes to, and whether kiln
//! goes back on its own after a failure.
//!
//! - **Where to.** A rollback names a deployment of the app's history by its id, or by what it
//!   ran: a commit (whole, or its first 7 or more hex digits), a revision as the app recorded it
//!   (a chart's version), or a tag the server has already turned into its commit. Only a
//!   deployment that succeeded can be gone back to, and only one kept with the commits a render
//!   needs. [`candidates`] lists them as a person chooses among them: newest first, what runs now
//!   left out, and one entry for each render.
//! - **On its own.** After a failure (a rollout that aborted, kiln's own restart that didn't take,
//!   a release that failed after its commit) kiln goes back by itself unless told not to. A person
//!   says so for one app, or for every app of a repository; the app's word beats its
//!   repository's, and either beats the deploy file's `rollout: { auto-rollback }`. Said nowhere,
//!   it goes back ([`decide`]).
//!
//! Pure: the history and the settings are handed in.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::state::Deployment;

/// The store setting (`Data::settings`) that holds one app's auto-rollback, by the app's id.
pub const APP_SETTING: &str = "auto-rollback/app/";
/// The store setting that holds a repository's, for every app it deploys.
pub const REPO_SETTING: &str = "auto-rollback/repo/";

/// The setting key of an app's auto-rollback.
pub fn app_setting(id: &str) -> String {
    format!("{APP_SETTING}{id}")
}

/// The setting key of a repository's auto-rollback: its URL as people write it, without a
/// trailing slash or `.git`, so `https://github.com/a/b.git` and `https://github.com/a/b/` are one.
pub fn repo_setting(url: &str) -> String {
    format!("{REPO_SETTING}{}", repo_name(url))
}

/// A repository's URL without a trailing slash or `.git`.
pub fn repo_name(url: &str) -> String {
    let u = url.trim().trim_end_matches('/');
    u.strip_suffix(".git").unwrap_or(u).to_owned()
}

/// Auto-rollback set by a person, for an app or a repository.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setting {
    /// Whether kiln goes back on its own.
    pub on: bool,
    /// Who set it.
    #[serde(default)]
    pub by: Option<String>,
    /// When (ms since the epoch).
    #[serde(default)]
    pub at_ms: u64,
}

/// Where an auto-rollback decision comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scope {
    /// Set for the app.
    App,
    /// Set for its repository.
    Repo,
    /// Its deploy file's `rollout: { auto-rollback }`.
    DeployFile,
    /// Said nowhere: kiln goes back.
    Default,
}

impl Scope {
    /// The scope as the API names it.
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::App => "app",
            Scope::Repo => "repo",
            Scope::DeployFile => "deploy-file",
            Scope::Default => "default",
        }
    }
}

/// Whether kiln goes back on its own, and who said so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    pub on: bool,
    pub from: Scope,
}

/// Auto-rollback for an app: its own setting, else its repository's, else its deploy file's
/// (`None` where it has no rollout), else on.
pub fn decide(app: Option<bool>, repo: Option<bool>, file: Option<bool>) -> Decision {
    match (app, repo, file) {
        (Some(on), _, _) => Decision { on, from: Scope::App },
        (None, Some(on), _) => Decision { on, from: Scope::Repo },
        (None, None, Some(on)) => Decision { on, from: Scope::DeployFile },
        (None, None, None) => Decision { on: true, from: Scope::Default },
    }
}

/// What a rollback asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ask<'a> {
    /// The newest deployment that succeeded before what runs now.
    Previous,
    /// A deployment, by its id in the app's history.
    Deployment(u64),
    /// What a deployment ran: a commit (7 or more hex digits are enough), or a revision exactly
    /// as the app recorded it (a chart's version).
    Revision(&'a str),
}

/// Why a rollback can't go where it was asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Nothing in the history is what was asked for.
    Missing(String),
    /// Something is, but it can't be gone back to.
    Unusable(String),
}

impl Refused {
    /// Why, in words.
    pub fn message(&self) -> &str {
        match self {
            Refused::Missing(m) | Refused::Unusable(m) => m,
        }
    }
}

/// Whether two deployments rendered the same thing: their render keys where both were kept, else
/// their commits, else their revisions.
pub fn same(a: &Deployment, b: &Deployment) -> bool {
    match (a.key, b.key) {
        (Some(x), Some(y)) => x == y,
        _ if !a.commits.is_empty() && !b.commits.is_empty() => a.commits == b.commits,
        _ => a.revision == b.revision,
    }
}

/// Whether a deployment can be rendered again: it was kept with each source's commit, or the app
/// has one source and its revision is that commit (entries kept before kiln recorded commits).
pub fn renderable(d: &Deployment, sources: usize) -> bool {
    !d.commits.is_empty() || (sources == 1 && d.revision.is_some())
}

/// What runs now: the newest deployment that succeeded.
pub fn current(history: &VecDeque<Deployment>) -> Option<&Deployment> {
    history.iter().rev().find(|d| d.succeeded)
}

/// The deployments a rollback can go to, newest first: those that succeeded and can be rendered
/// again, what runs now left out, and only the newest of those that rendered the same thing.
pub fn candidates(history: &VecDeque<Deployment>, sources: usize) -> Vec<&Deployment> {
    let now = current(history);
    let mut out: Vec<&Deployment> = Vec::new();
    for d in history.iter().rev().filter(|d| d.succeeded && renderable(d, sources)) {
        if now.is_some_and(|c| same(c, d)) || out.iter().any(|o| same(o, d)) {
            continue;
        }
        out.push(d);
    }
    out
}

/// A commit's first hex digits, as people give one: 7 to 40 of them, any case.
fn commit_prefix(s: &str) -> Option<String> {
    (7..=64).contains(&s.len()).then(|| s.to_ascii_lowercase()).filter(|p| p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Whether a deployment ran `rev`: its revision exactly, or a commit (its revision's or a
/// source's) that `prefix` begins.
fn ran(d: &Deployment, rev: &str, prefix: Option<&str>) -> bool {
    if d.revision.as_deref() == Some(rev) {
        return true;
    }
    let Some(p) = prefix else { return false };
    let starts = |c: &str| c.to_ascii_lowercase().starts_with(p);
    d.revision.as_deref().is_some_and(starts) || d.commits.iter().flatten().any(|c| starts(c))
}

/// The deployment a rollback of `app` goes to.
pub fn pick<'a>(app: &str, history: &'a VecDeque<Deployment>, ask: Ask<'_>, sources: usize) -> Result<&'a Deployment, Refused> {
    let usable = |d: &'a Deployment| -> Result<&'a Deployment, Refused> {
        if !d.succeeded {
            return Err(Refused::Unusable(format!("deployment {} of {app} failed; roll back to one that succeeded", d.id)));
        }
        if !renderable(d, sources) {
            return Err(Refused::Unusable(format!("deployment {} of {app} was kept without its sources' commits, so it can't be rendered again", d.id)));
        }
        Ok(d)
    };
    match ask {
        Ask::Previous => candidates(history, sources).first().copied().ok_or_else(|| Refused::Missing(format!("{app} has no earlier deployment to roll back to"))),
        Ask::Deployment(n) => usable(history.iter().find(|d| d.id == n).ok_or_else(|| Refused::Missing(format!("{app} has no deployment {n}")))?),
        Ask::Revision(rev) => {
            let rev = rev.trim();
            if rev.is_empty() {
                return Err(Refused::Missing("name a commit, a tag or a deployment to roll back to".into()));
            }
            let prefix = commit_prefix(rev);
            let matched: Vec<&Deployment> = history.iter().rev().filter(|d| ran(d, rev, prefix.as_deref())).collect();
            let Some(newest) = matched.first() else {
                return Err(Refused::Missing(format!("no deployment of {app} ran {rev}: kiln rolls back only to what it deployed (its history lists them)")));
            };
            // A short commit two deployments' different commits begin: say which.
            if prefix.is_some() && matched.iter().any(|d| d.revision != newest.revision && d.commits != newest.commits) {
                let mut seen: Vec<&str> = Vec::new();
                for r in matched.iter().filter_map(|d| d.revision.as_deref()) {
                    if !seen.contains(&r) {
                        seen.push(r);
                    }
                }
                return Err(Refused::Missing(format!("{rev} begins more than one commit {app} deployed ({}): give more of it", seen.join(", "))));
            }
            // The newest that ran it and can be gone back to, else why the newest can't.
            match matched.iter().find(|d| d.succeeded && renderable(d, sources)) {
                Some(d) => Ok(d),
                None => usable(newest),
            }
        }
    }
}

/// The tags among `tags` (each a name and the commit it names) that name `commit`.
pub fn tags_of<'a>(tags: &'a [(String, String)], commit: &str) -> Vec<&'a str> {
    tags.iter().filter(|(_, c)| c.eq_ignore_ascii_case(commit)).map(|(n, _)| n.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dep(id: u64, rev: &str, key: u64, ok: bool) -> Deployment {
        Deployment { app: "default/web".into(), id, revision: Some(rev.into()), key: Some(key), succeeded: ok, commits: vec![Some(rev.into())], ..Deployment::default() }
    }

    fn history(list: Vec<Deployment>) -> VecDeque<Deployment> {
        list.into_iter().collect()
    }

    const A: &str = "aaaaaaa1111111111111111111111111111111111";
    const B: &str = "bbbbbbb2222222222222222222222222222222222";
    const C: &str = "ccccccc3333333333333333333333333333333333";

    #[test]
    fn auto_rollback_follows_the_most_specific_word() {
        assert_eq!(decide(None, None, None), Decision { on: true, from: Scope::Default }, "said nowhere, kiln goes back");
        assert_eq!(decide(None, None, Some(false)), Decision { on: false, from: Scope::DeployFile });
        assert_eq!(decide(None, Some(false), Some(true)), Decision { on: false, from: Scope::Repo }, "the repository's beats the deploy file's");
        assert_eq!(decide(Some(true), Some(false), Some(false)), Decision { on: true, from: Scope::App }, "the app's beats both");
        assert_eq!(decide(Some(false), None, None), Decision { on: false, from: Scope::App });
    }

    #[test]
    fn a_repository_is_one_setting_however_its_url_is_written() {
        assert_eq!(repo_setting("https://github.com/a/b.git"), repo_setting("https://github.com/a/b/"));
        assert_eq!(repo_setting(" https://github.com/a/b "), "auto-rollback/repo/https://github.com/a/b");
        assert_eq!(app_setting("default/web"), "auto-rollback/app/default/web");
    }

    #[test]
    fn candidates_leave_out_what_runs_failures_and_repeats() {
        // Oldest first: A, A again (same render), B failed, B, C (what runs).
        let h = history(vec![dep(1, A, 10, true), dep(2, A, 10, true), dep(3, B, 20, false), dep(4, B, 20, true), dep(5, C, 30, true)]);
        let ids: Vec<u64> = candidates(&h, 1).iter().map(|d| d.id).collect();
        assert_eq!(ids, [4, 2], "newest first, one per render, what runs and the failure left out");
        // After a rollback to A, A runs: B and C are the way forward again.
        let mut back = h.clone();
        back.push_back(dep(6, A, 10, true));
        let ids: Vec<u64> = candidates(&back, 1).iter().map(|d| d.id).collect();
        assert_eq!(ids, [5, 4]);
    }

    #[test]
    fn the_same_commit_rendered_anew_is_its_own_candidate() {
        // Same commit, another render (its image rebuilt): a different thing to go back to.
        let h = history(vec![dep(1, A, 10, true), dep(2, A, 11, true), dep(3, B, 20, true)]);
        let ids: Vec<u64> = candidates(&h, 1).iter().map(|d| d.id).collect();
        assert_eq!(ids, [2, 1]);
    }

    #[test]
    fn a_rollback_goes_to_a_deployment_by_id_commit_or_revision() {
        let h = history(vec![dep(1, A, 10, true), dep(2, B, 20, false), dep(3, B, 21, true), dep(4, C, 30, true)]);
        assert!(matches!(pick("web", &h, Ask::Previous, 1), Ok(d) if d.id == 3));
        assert!(matches!(pick("web", &h, Ask::Deployment(1), 1), Ok(d) if d.id == 1));
        assert!(matches!(pick("web", &h, Ask::Revision(A), 1), Ok(d) if d.id == 1), "the whole commit");
        assert!(matches!(pick("web", &h, Ask::Revision("AAAAAAA"), 1), Ok(d) if d.id == 1), "its first 7 digits, any case");
        assert!(matches!(pick("web", &h, Ask::Revision("bbbbbbb"), 1), Ok(d) if d.id == 3), "the newest that succeeded, past a failure of the same commit");
        let charts = history(vec![dep(1, "92.1.0", 10, true), dep(2, "92.2.0", 20, true)]);
        assert!(matches!(pick("prom", &charts, Ask::Revision("92.1.0"), 1), Ok(d) if d.id == 1), "a chart's version as recorded");
    }

    #[test]
    fn a_rollback_refuses_what_it_cant_go_back_to() {
        let h = history(vec![dep(1, A, 10, true), dep(2, B, 20, false), dep(3, C, 30, true)]);
        assert!(matches!(pick("web", &h, Ask::Deployment(2), 1), Err(Refused::Unusable(m)) if m.contains("failed")));
        assert!(matches!(pick("web", &h, Ask::Revision(B), 1), Err(Refused::Unusable(m)) if m.contains("deployment 2") && m.contains("failed")), "only a failure ran it");
        assert!(matches!(pick("web", &h, Ask::Deployment(9), 1), Err(Refused::Missing(_))));
        assert!(matches!(pick("web", &h, Ask::Revision("ddddddd"), 1), Err(Refused::Missing(m)) if m.contains("only to what it deployed")));
        assert!(matches!(pick("web", &h, Ask::Revision("aaaa"), 1), Err(Refused::Missing(_))), "fewer than 7 digits is no commit");
        assert!(matches!(pick("web", &h, Ask::Revision("  "), 1), Err(Refused::Missing(_))));
        // Two commits that begin alike: more of it, please.
        let twins = history(vec![dep(1, "abcdef01111", 10, true), dep(2, "abcdef02222", 20, true), dep(3, C, 30, true)]);
        assert!(matches!(pick("web", &twins, Ask::Revision("abcdef0"), 1), Err(Refused::Missing(m)) if m.contains("more than one commit")));
        assert!(matches!(pick("web", &twins, Ask::Revision("abcdef02"), 1), Ok(d) if d.id == 2));
        // Kept before kiln recorded commits, with two sources: nothing to render it from.
        let mut old = dep(1, A, 10, true);
        old.commits.clear();
        let h = history(vec![old, dep(2, C, 30, true)]);
        assert!(matches!(pick("web", &h, Ask::Deployment(1), 2), Err(Refused::Unusable(m)) if m.contains("can't be rendered again")));
        assert!(pick("web", &h, Ask::Deployment(1), 1).is_ok(), "one source: its revision is its commit");
        let first = history(vec![dep(1, A, 10, true)]);
        assert!(matches!(pick("web", &first, Ask::Previous, 1), Err(Refused::Missing(m)) if m.contains("no earlier deployment")));
    }

    #[test]
    fn a_commit_s_tags() {
        let tags = vec![("v1.0.0".to_owned(), A.to_owned()), ("v1.0".to_owned(), A.to_owned()), ("v1.1.0".to_owned(), B.to_owned())];
        assert_eq!(tags_of(&tags, A), ["v1.0.0", "v1.0"]);
        assert_eq!(tags_of(&tags, &B.to_ascii_uppercase()), ["v1.1.0"]);
        assert!(tags_of(&tags, C).is_empty());
    }
}
