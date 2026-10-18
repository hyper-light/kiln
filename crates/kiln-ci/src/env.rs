//! Environments' protection (docs/ci.md W15-R03, R04), GitHub-exact where GitHub has it: a job
//! bound to an environment (`environment:`) starts only when the environment's rules let it.
//!
//! - **Deployment branches and tags.** With patterns, only refs matching them deploy; anything
//!   else fails the job with GitHub's message. A pull request's merge ref is a branch named
//!   `refs/pull/<n>/merge` here, as on GitHub, so it matches only patterns that say so.
//! - **Required reviewers.** The job waits until one of them approves; a rejection fails it.
//!   Who may review (listed, not the run's actor under `prevent-self-review`) is decided where
//!   people are known; [`gate`] takes the reviews that count.
//! - **Wait timer.** The job waits that many minutes after it became ready.

use serde::{Deserialize, Serialize};

use crate::trigger::pattern::Pattern;

/// The longest wait timer GitHub allows: 30 days.
pub const MAX_WAIT_MINUTES: u32 = 43_200;
/// The most required reviewers GitHub allows.
pub const MAX_REVIEWERS: usize = 6;

/// An environment's protection rules, as `.kiln/environments.yaml` and the API write them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Protection {
    /// Required reviewers: people (`alice`) or teams (`team:web`); any one of them approves.
    pub reviewers: Vec<String>,
    /// The run's actor can't approve its own deployment.
    pub prevent_self_review: bool,
    /// Minutes a job waits after it's ready.
    pub wait_timer: u32,
    /// Branches that may deploy (GitHub's patterns); with neither these nor `tags`, any ref may.
    pub branches: Option<Vec<String>>,
    /// Tags that may deploy.
    pub tags: Option<Vec<String>>,
}

impl Protection {
    /// The rules' problems, GitHub's limits included.
    pub fn check(&self) -> Result<(), String> {
        if self.reviewers.len() > MAX_REVIEWERS {
            return Err(format!("an environment has at most {MAX_REVIEWERS} required reviewers"));
        }
        if self.wait_timer > MAX_WAIT_MINUTES {
            return Err(format!("the wait timer is at most {MAX_WAIT_MINUTES} minutes (30 days)"));
        }
        for p in self.branches.iter().chain(self.tags.iter()).flatten() {
            Pattern::new(p).map_err(|e| format!("`{p}`: {e}"))?;
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.reviewers.is_empty() && self.wait_timer == 0 && self.branches.is_none() && self.tags.is_none()
    }
}

/// A reviewer's decision on a run's deployment to an environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub by: String,
    pub approved: bool,
    pub comment: String,
    pub at: i64,
}

/// What an environment's rules say about a job now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// The ref may not deploy here: the job fails with this, GitHub's message.
    Refused(String),
    /// A reviewer rejected it: the job fails.
    Rejected { by: String, comment: String },
    /// It waits: for one of `reviewers` to approve, or until `until` (Unix seconds), or both.
    Waiting { reviewers: Vec<String>, until: Option<i64> },
    /// It may start.
    Open,
}

/// The gate for a job of `git_ref` bound to environment `env`, `ready_at` being when it could
/// first have started.
pub fn gate(env: &str, p: &Protection, git_ref: &str, ready_at: i64, now: i64, reviews: &[Review]) -> Gate {
    if p.branches.is_some() || p.tags.is_some() {
        let (what, name, patterns) = match git_ref.strip_prefix("refs/tags/") {
            Some(tag) => ("Tag", tag, &p.tags),
            None => ("Branch", git_ref.strip_prefix("refs/heads/").unwrap_or(git_ref), &p.branches),
        };
        let allowed = patterns.as_ref().is_some_and(|ps| ps.iter().filter_map(|x| Pattern::new(x).ok()).any(|x| x.matches(name)));
        if !allowed {
            return Gate::Refused(format!("{what} \"{name}\" is not allowed to deploy to {env} due to environment protection rules."));
        }
    }
    if let Some(r) = reviews.iter().filter(|r| !r.approved).max_by_key(|r| r.at) {
        return Gate::Rejected { by: r.by.clone(), comment: r.comment.clone() };
    }
    let approved = p.reviewers.is_empty() || reviews.iter().any(|r| r.approved);
    let until = (p.wait_timer > 0).then(|| ready_at + i64::from(p.wait_timer) * 60).filter(|t| *t > now);
    if approved && until.is_none() {
        return Gate::Open;
    }
    Gate::Waiting { reviewers: if approved { Vec::new() } else { p.reviewers.clone() }, until }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review(by: &str, approved: bool, at: i64) -> Review {
        Review { by: by.into(), approved, comment: if approved { String::new() } else { "not now".into() }, at }
    }

    /// W15-AC01: each rule blocks and releases as specified.
    #[test]
    fn w15_ac01_branches_and_tags() {
        let p = Protection { branches: Some(vec!["main".into(), "release/*".into()]), ..Protection::default() };
        assert_eq!(gate("production", &p, "refs/heads/main", 0, 0, &[]), Gate::Open);
        assert_eq!(gate("production", &p, "refs/heads/release/1.2", 0, 0, &[]), Gate::Open);
        assert_eq!(gate("production", &p, "refs/heads/feature", 0, 0, &[]), Gate::Refused("Branch \"feature\" is not allowed to deploy to production due to environment protection rules.".into()));
        assert_eq!(
            gate("production", &p, "refs/pull/7/merge", 0, 0, &[]),
            Gate::Refused("Branch \"refs/pull/7/merge\" is not allowed to deploy to production due to environment protection rules.".into()),
            "a pull request's merge ref, as GitHub names it"
        );
        assert_eq!(gate("production", &p, "refs/tags/v1.0", 0, 0, &[]), Gate::Refused("Tag \"v1.0\" is not allowed to deploy to production due to environment protection rules.".into()), "branch patterns alone let no tag deploy");
        let tags = Protection { tags: Some(vec!["v*".into()]), ..Protection::default() };
        assert_eq!(gate("production", &tags, "refs/tags/v1.0", 0, 0, &[]), Gate::Open);
        assert!(matches!(gate("production", &tags, "refs/heads/main", 0, 0, &[]), Gate::Refused(_)));
        assert_eq!(gate("production", &Protection::default(), "refs/heads/anything", 0, 0, &[]), Gate::Open, "no patterns: any ref");
    }

    #[test]
    fn w15_ac01_reviewers_and_the_wait_timer() {
        let p = Protection { reviewers: vec!["alice".into(), "team:ops".into()], wait_timer: 10, ..Protection::default() };
        // Ready at 1000: 10 minutes to wait, and a reviewer.
        assert_eq!(gate("prod", &p, "refs/heads/main", 1000, 1000, &[]), Gate::Waiting { reviewers: vec!["alice".into(), "team:ops".into()], until: Some(1600) });
        assert_eq!(gate("prod", &p, "refs/heads/main", 1000, 1200, &[review("alice", true, 1100)]), Gate::Waiting { reviewers: vec![], until: Some(1600) }, "approved, the timer still runs");
        assert_eq!(gate("prod", &p, "refs/heads/main", 1000, 1600, &[review("alice", true, 1100)]), Gate::Open);
        assert_eq!(gate("prod", &p, "refs/heads/main", 1000, 1700, &[]), Gate::Waiting { reviewers: vec!["alice".into(), "team:ops".into()], until: None }, "the timer ran out, the reviewer not yet");
        assert_eq!(gate("prod", &p, "refs/heads/main", 1000, 1700, &[review("alice", true, 1100), review("bob", false, 1200)]), Gate::Rejected { by: "bob".into(), comment: "not now".into() }, "a rejection wins");
    }

    #[test]
    fn w15_r03_github_limits() {
        assert!(Protection { reviewers: (0..7).map(|i| format!("u{i}")).collect(), ..Protection::default() }.check().is_err());
        assert!(Protection { wait_timer: MAX_WAIT_MINUTES + 1, ..Protection::default() }.check().is_err());
        assert!(Protection { wait_timer: MAX_WAIT_MINUTES, reviewers: vec!["a".into()], branches: Some(vec!["main".into()]), ..Protection::default() }.check().is_ok());
        let parsed: Protection = serde_yaml::from_str("reviewers: [alice]\nprevent-self-review: true\nwait-timer: 5\nbranches: [main]\n").unwrap_or_default();
        assert_eq!(parsed, Protection { reviewers: vec!["alice".into()], prevent_self_review: true, wait_timer: 5, branches: Some(vec!["main".into()]), tags: None });
    }
}
