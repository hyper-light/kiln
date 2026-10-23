//! What happened, normalized (docs/ci.md W03-R01), whichever source saw it.

use serde::{Deserialize, Serialize};

/// Where an event came from (W03-R02).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// `git ls-remote` polling.
    #[default]
    Poll,
    /// A GitHub webhook.
    Webhook,
    /// A GitHub App event.
    App,
    /// A kiln-hosted repository (W14).
    Native,
    /// The API, CLI or UI.
    Api,
    Schedule,
    /// Another run finishing.
    WorkflowRun,
    Agent,
    /// CD (W15).
    Cd,
    /// An inbound webhook (`/api/ci/hooks/:id`).
    Hook,
}

/// One event.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// A GitHub event name (`push`, `pull_request`, …) or a kiln event (`diff`, `land`, …).
    pub name: String,
    /// The activity type (`opened`, `completed`, …); `repository_dispatch`'s `event_type`;
    /// `schedule`'s cron expression; empty when the event has none.
    pub action: String,
    /// `owner/name`.
    pub repo: String,
    /// `GITHUB_REF` for the run: `refs/heads/main`, `refs/tags/v1`, `refs/pull/3/merge`.
    pub git_ref: String,
    /// `GITHUB_SHA` for the run.
    pub sha: String,
    /// The ref's previous commit (push); `None` or the zero SHA for a new ref.
    pub before: Option<String>,
    /// Pull requests and merge groups: the base branch; pull requests: the head branch.
    pub base_ref: Option<String>,
    pub head_ref: Option<String>,
    pub actor: String,
    /// Untrusted events (a fork's pull request, W03-R08) get read-only tokens and no secrets.
    pub trusted: bool,
    pub source: Source,
    /// The GitHub-shaped payload (`github.event`, `$GITHUB_EVENT_PATH`).
    pub payload: serde_json::Value,
    /// The changed files (W03-R05), when known.
    pub changed: Option<Vec<String>>,
    /// The head commit's message, for the skip markers (W03-R06).
    pub message: Option<String>,
    /// `workflow_run` and `pipeline`: the finished run's workflow name (and path).
    pub workflow: Option<String>,
    pub workflow_path: Option<String>,
    /// `workflow_run` and `pipeline`: the finished run's conclusion and branch.
    pub conclusion: Option<String>,
    pub workflow_branch: Option<String>,
    /// The ref was created or deleted by this push.
    pub created: bool,
    pub deleted: bool,
    /// Who in kiln caused it, when someone did (a push to a kiln-hosted repository, a diff, a
    /// dispatch through the API, an agent): the principal whose `secrets.use` its legs' secrets
    /// are held to (docs/ci.md W10-R02). `None` for what the repository itself started
    /// (schedules, another run, a host's webhook), whose secrets its configuration decides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_by: Option<serde_json::Value>,
}

/// The zero SHA, `before` of a new ref and `after` of a deleted one.
pub const ZERO_SHA: &str = "0000000000000000000000000000000000000000";

impl Event {
    /// The short name of `git_ref` (`main`, `v1`, `3/merge`), as `github.ref_name`.
    pub fn ref_name(&self) -> &str {
        let r = self.git_ref.as_str();
        r.strip_prefix("refs/heads/").or_else(|| r.strip_prefix("refs/tags/")).or_else(|| r.strip_prefix("refs/pull/")).or_else(|| r.strip_prefix("refs/")).unwrap_or(r)
    }

    pub fn is_tag(&self) -> bool {
        self.git_ref.starts_with("refs/tags/")
    }

    /// `github.ref_type`: `branch` or `tag`.
    pub fn ref_type(&self) -> &'static str {
        if self.is_tag() { "tag" } else { "branch" }
    }
}
