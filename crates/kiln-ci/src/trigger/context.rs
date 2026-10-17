//! A run's `github` context, and GitHub-shaped payloads for the events kiln saw itself (polled
//! and native pushes and pull requests, dispatches, schedules) (docs/ci.md W03-R07, W06-R03).
//! Webhook and App events pass GitHub's own payload through instead.

use serde_json::{Value, json};

use super::event::{Event, ZERO_SHA};

/// The repository, as payloads and the context describe it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Repo {
    /// `owner/name`.
    pub full_name: String,
    /// GitHub-style numeric ids (the facade's, W14).
    pub id: u64,
    pub owner_id: u64,
    pub default_branch: String,
    pub private: bool,
    /// `https://host` of the server the repository lives on (GitHub, or kiln's facade).
    pub server_url: String,
}

impl Repo {
    pub fn owner(&self) -> &str {
        self.full_name.split_once('/').map_or(self.full_name.as_str(), |(o, _)| o)
    }

    pub fn name(&self) -> &str {
        self.full_name.split_once('/').map_or(self.full_name.as_str(), |(_, n)| n)
    }

    pub fn html_url(&self) -> String {
        format!("{}/{}", self.server_url.trim_end_matches('/'), self.full_name)
    }

    /// The `repository` object of webhook payloads.
    pub fn payload(&self) -> Value {
        let html = self.html_url();
        let host = self.server_url.trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/');
        json!({
            "id": self.id,
            "name": self.name(),
            "full_name": self.full_name,
            "private": self.private,
            "visibility": if self.private { "private" } else { "public" },
            "owner": { "login": self.owner(), "id": self.owner_id, "type": "Organization", "html_url": format!("{}/{}", self.server_url.trim_end_matches('/'), self.owner()) },
            "html_url": html,
            "url": html,
            "clone_url": format!("{html}.git"),
            "git_url": format!("git://{host}/{}.git", self.full_name),
            "ssh_url": format!("git@{host}:{}.git", self.full_name),
            "fork": false,
            "default_branch": self.default_branch,
            "master_branch": self.default_branch,
        })
    }
}

/// What the run is, for the context.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunFacts {
    pub run_id: u64,
    pub run_number: u64,
    pub run_attempt: u32,
    /// The workflow's name (its path when it has none).
    pub workflow: String,
    /// `owner/repo/.github/workflows/ci.yml@refs/heads/main`.
    pub workflow_ref: String,
    pub workflow_sha: String,
    pub api_url: String,
    pub graphql_url: String,
    pub retention_days: u32,
    pub actor_id: u64,
    /// Who started this attempt (differs from `actor` on a re-run).
    pub triggering_actor: String,
}

/// The `github` context as the server knows it (W06-R03). The runner adds what only it knows:
/// `token`, `workspace`, `action*`, `job`, `path`, `env`, `event_path`.
pub fn github(e: &Event, repo: &Repo, run: &RunFacts) -> Value {
    let pr = matches!(e.name.as_str(), "pull_request" | "pull_request_target");
    let host = repo.server_url.trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/');
    json!({
        "event_name": e.name,
        "event": e.payload,
        "ref": e.git_ref,
        "ref_name": e.ref_name(),
        "ref_type": e.ref_type(),
        "ref_protected": false,
        "sha": e.sha,
        "head_ref": if pr { e.head_ref.clone().unwrap_or_default() } else { String::new() },
        "base_ref": if pr { e.base_ref.clone().unwrap_or_default() } else { String::new() },
        "repository": repo.full_name,
        "repository_id": repo.id.to_string(),
        "repository_owner": repo.owner(),
        "repository_owner_id": repo.owner_id.to_string(),
        "repositoryUrl": format!("git://{host}/{}.git", repo.full_name),
        "actor": e.actor,
        "actor_id": run.actor_id.to_string(),
        "triggering_actor": if run.triggering_actor.is_empty() { e.actor.clone() } else { run.triggering_actor.clone() },
        "run_id": run.run_id.to_string(),
        "run_number": run.run_number.to_string(),
        "run_attempt": run.run_attempt.max(1).to_string(),
        "retention_days": run.retention_days.to_string(),
        "workflow": run.workflow,
        "workflow_ref": run.workflow_ref,
        "workflow_sha": run.workflow_sha,
        "server_url": repo.server_url,
        "api_url": run.api_url,
        "graphql_url": run.graphql_url,
        "secret_source": "Actions",
    })
}

/// A person in a commit or a push.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Person {
    pub name: String,
    pub email: String,
    pub username: Option<String>,
}

impl Person {
    fn payload(&self) -> Value {
        let mut v = json!({ "name": self.name, "email": self.email });
        if let (Some(u), Some(m)) = (&self.username, v.as_object_mut()) {
            m.insert("username".into(), json!(u));
        }
        v
    }
}

/// A commit, for `push` payloads.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Commit {
    pub id: String,
    pub tree_id: String,
    pub message: String,
    /// RFC 3339.
    pub timestamp: String,
    pub author: Person,
    pub committer: Person,
}

/// Commits in a `push` payload: GitHub lists up to 20.
pub const PUSH_COMMITS: usize = 20;

/// A `push` payload (W03-R07). `commits` oldest first; as GitHub's for Actions, the commit objects
/// carry no `added`, `removed` or `modified`.
pub fn push(e: &Event, repo: &Repo, commits: &[Commit], pusher: &Person, forced: bool) -> Value {
    let before = e.before.clone().unwrap_or_else(|| ZERO_SHA.to_owned());
    let after = if e.deleted { ZERO_SHA.to_owned() } else { e.sha.clone() };
    let html = repo.html_url();
    let listed: Vec<Value> = commits
        .iter()
        .rev()
        .take(PUSH_COMMITS)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|c| {
            json!({
                "id": c.id, "tree_id": c.tree_id, "distinct": true, "message": c.message, "timestamp": c.timestamp,
                "url": format!("{html}/commit/{}", c.id), "author": c.author.payload(), "committer": c.committer.payload(),
            })
        })
        .collect();
    let head = listed.last().cloned().unwrap_or(Value::Null);
    json!({
        "ref": e.git_ref,
        "before": before,
        "after": after,
        "created": e.created || before == ZERO_SHA,
        "deleted": e.deleted,
        "forced": forced,
        "base_ref": Value::Null,
        "compare": format!("{html}/compare/{}...{}", short(&before), short(&after)),
        "commits": listed,
        "head_commit": head,
        "repository": repo.payload(),
        "pusher": pusher.payload(),
        "sender": { "login": e.actor, "type": "User" },
    })
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// One side of a pull request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Side {
    pub branch: String,
    pub sha: String,
    /// The repository the side lives in (a fork's for the head of an outside pull request).
    pub repo: Repo,
}

/// A pull request as kiln knows it. Polling without a token knows only the head (`refs/pull/N`):
/// the rest is `None`, and the payload says `kiln.partial`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PullRequest {
    pub number: u64,
    pub title: Option<String>,
    pub body: Option<String>,
    pub draft: Option<bool>,
    pub state: String,
    pub author: Option<String>,
    pub labels: Option<Vec<String>>,
    pub head: Side,
    pub base: Side,
}

/// A `pull_request` (or `pull_request_target`) payload.
pub fn pull_request(e: &Event, repo: &Repo, pr: &PullRequest) -> Value {
    let partial = pr.title.is_none() || pr.labels.is_none() || pr.author.is_none();
    let side = |s: &Side| {
        json!({
            "ref": s.branch, "sha": s.sha,
            "label": format!("{}:{}", s.repo.owner(), s.branch),
            "repo": s.repo.payload(),
        })
    };
    let mut p = json!({
        "number": pr.number,
        "state": if pr.state.is_empty() { "open" } else { pr.state.as_str() },
        "html_url": format!("{}/pull/{}", repo.html_url(), pr.number),
        "head": side(&pr.head),
        "base": side(&pr.base),
    });
    if let Some(m) = p.as_object_mut() {
        if let Some(t) = &pr.title {
            m.insert("title".into(), json!(t));
        }
        if let Some(b) = &pr.body {
            m.insert("body".into(), json!(b));
        }
        if let Some(d) = pr.draft {
            m.insert("draft".into(), json!(d));
        }
        if let Some(a) = &pr.author {
            m.insert("user".into(), json!({ "login": a, "type": "User" }));
        }
        if let Some(l) = &pr.labels {
            m.insert("labels".into(), json!(l.iter().map(|n| json!({ "name": n })).collect::<Vec<_>>()));
        }
    }
    let mut out = json!({
        "action": e.action,
        "number": pr.number,
        "pull_request": p,
        "repository": repo.payload(),
        "sender": { "login": e.actor, "type": "User" },
    });
    if partial && let Some(m) = out.as_object_mut() {
        m.insert("kiln".into(), json!({ "partial": true }));
    }
    out
}

/// A `workflow_dispatch` payload.
pub fn workflow_dispatch(e: &Event, repo: &Repo, workflow_path: &str, inputs: &Value) -> Value {
    json!({ "inputs": inputs, "ref": e.git_ref, "repository": repo.payload(), "sender": { "login": e.actor, "type": "User" }, "workflow": workflow_path })
}

/// A `repository_dispatch` payload.
pub fn repository_dispatch(e: &Event, repo: &Repo, client_payload: &Value) -> Value {
    json!({ "action": e.action, "branch": repo.default_branch, "client_payload": client_payload, "repository": repo.payload(), "sender": { "login": e.actor, "type": "User" } })
}

/// A `schedule` payload: the cron that fired.
pub fn schedule(cron: &str, repo: &Repo) -> Value {
    json!({ "schedule": cron, "repository": repo.payload() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(v: &Value, p: &str) -> Value {
        v.pointer(p).cloned().unwrap_or(Value::Null)
    }

    fn repo() -> Repo {
        Repo { full_name: "acme/app".into(), id: 7, owner_id: 3, default_branch: "main".into(), private: false, server_url: "https://kiln.example".into() }
    }

    #[test]
    fn the_context() {
        let e = Event { name: "push".into(), git_ref: "refs/heads/main".into(), sha: "abc".into(), actor: "mona".into(), payload: json!({"x": 1}), ..Event::default() };
        let run = RunFacts { run_id: 99, run_number: 4, run_attempt: 0, workflow: "CI".into(), retention_days: 90, ..RunFacts::default() };
        let g = github(&e, &repo(), &run);
        assert_eq!(at(&g, "/ref_name"), "main");
        assert_eq!(at(&g, "/ref_type"), "branch");
        assert_eq!(at(&g, "/run_id"), "99");
        assert_eq!(at(&g, "/run_attempt"), "1");
        assert_eq!(at(&g, "/head_ref"), "");
        assert_eq!(at(&g, "/repository_owner"), "acme");
        assert_eq!(at(&g, "/triggering_actor"), "mona");
        assert_eq!(at(&g, "/event/x"), 1);
        let t = Event { git_ref: "refs/tags/v1.2".into(), ..e };
        assert_eq!((at(&github(&t, &repo(), &run), "/ref_type").clone(), at(&github(&t, &repo(), &run), "/ref_name").clone()), (json!("tag"), json!("v1.2")));
    }

    #[test]
    fn a_push() {
        let e = Event { name: "push".into(), git_ref: "refs/heads/main".into(), sha: "b".repeat(40), before: Some("a".repeat(40)), actor: "mona".into(), ..Event::default() };
        let commits: Vec<Commit> = (0..25).map(|i| Commit { id: format!("{i:040}"), message: format!("c{i}"), ..Commit::default() }).collect();
        let p = push(&e, &repo(), &commits, &Person { name: "Mona".into(), email: "m@x".into(), username: None }, false);
        assert_eq!(at(&p, "/commits").as_array().map(Vec::len), Some(20));
        assert_eq!(at(&p, "/commits/0/message"), "c5", "the last 20, oldest first");
        assert_eq!(at(&p, "/head_commit/message"), "c24");
        assert!(at(&p, "/commits/0").get("added").is_none());
        assert_eq!(at(&p, "/created"), false);
        assert_eq!(at(&p, "/repository/full_name"), "acme/app");
        let new = Event { before: None, ..e.clone() };
        assert_eq!(at(&push(&new, &repo(), &[], &Person::default(), false), "/created"), true);
        assert_eq!(at(&push(&new, &repo(), &[], &Person::default(), false), "/head_commit"), Value::Null);
    }

    #[test]
    fn a_partial_pull_request() {
        let e = Event { name: "pull_request".into(), action: "opened".into(), actor: "mona".into(), ..Event::default() };
        let pr = PullRequest { number: 3, head: Side { branch: "fix".into(), sha: "h".into(), repo: repo() }, base: Side { branch: "main".into(), sha: "b".into(), repo: repo() }, ..PullRequest::default() };
        let p = pull_request(&e, &repo(), &pr);
        assert_eq!(at(&p, "/kiln/partial"), true);
        assert_eq!(at(&p, "/pull_request/head/ref"), "fix");
        assert!(at(&p, "/pull_request").get("labels").is_none(), "unknown fields are absent");
        let full = PullRequest { title: Some("t".into()), author: Some("mona".into()), labels: Some(vec!["bug".into()]), ..pr };
        let p = pull_request(&e, &repo(), &full);
        assert!(p.get("kiln").is_none());
        assert_eq!(at(&p, "/pull_request/labels/0/name"), "bug");
    }
}
