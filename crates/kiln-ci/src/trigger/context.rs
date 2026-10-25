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
    /// When it was made (Unix seconds; 0 when unknown).
    pub created_at: i64,
}

/// GitHub's node id shape for kiln's objects: a kind (`R` a repository, `U` a user, `O` an
/// organization, `PR` a pull request, `LA` a label, `W` a workflow, `WFR` a run, `CS` a check
/// suite) and its number.
pub fn node_id(kind: &str, id: u64) -> String {
    format!("{kind}_kiln{id}")
}

/// A stable positive number for what kiln has none for (a login, a label, a pull request): its
/// SHA-256's first six bytes.
pub fn id_of(name: &str) -> u64 {
    use sha2::Digest as _;
    sha2::Sha256::digest(name.as_bytes()).iter().take(6).fold(0, |n, b| (n << 8) | u64::from(*b))
}

/// Seconds since the epoch, as RFC 3339.
pub fn rfc3339(secs: i64) -> String {
    jiff::Timestamp::from_second(secs).map_or_else(|_| "1970-01-01T00:00:00Z".to_owned(), |t| t.to_string())
}

/// Where a server's REST API is: GitHub's own, or the facade's under `/api/v3` (W14-R11).
fn api_base(server_url: &str) -> String {
    let base = server_url.trim_end_matches('/');
    if base == "https://github.com" { "https://api.github.com".to_owned() } else { format!("{base}/api/v3") }
}

/// GitHub's `user` object for `login` on the server at `server_url`, with GitHub's API links: a
/// `Bot` when the login is one (`…[bot]`), an `Organization` when `organization`.
pub fn user(server_url: &str, login: &str, organization: bool) -> Value {
    let base = server_url.trim_end_matches('/');
    let api = format!("{}/users/{login}", api_base(server_url));
    let (kind, id) = (if organization { "Organization" } else if login.ends_with("[bot]") { "Bot" } else { "User" }, id_of(login));
    json!({
        "login": login,
        "id": id,
        "node_id": node_id(if organization { "O" } else { "U" }, id),
        "avatar_url": format!("{base}/{login}.png"),
        "gravatar_id": "",
        "url": api,
        "html_url": format!("{base}/{login}"),
        "followers_url": format!("{api}/followers"),
        "following_url": format!("{api}/following{{/other_user}}"),
        "gists_url": format!("{api}/gists{{/gist_id}}"),
        "starred_url": format!("{api}/starred{{/owner}}{{/repo}}"),
        "subscriptions_url": format!("{api}/subscriptions"),
        "organizations_url": format!("{api}/orgs"),
        "repos_url": format!("{api}/repos"),
        "events_url": format!("{api}/events{{/privacy}}"),
        "received_events_url": format!("{api}/received_events"),
        "type": kind,
        "site_admin": false,
    })
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

    /// Its number: the facade's, or one from its name when it has none.
    fn number(&self) -> u64 {
        if self.id == 0 { id_of(&self.full_name) } else { self.id }
    }

    /// GitHub's `repository-lite` (a workflow run's repositories): what it is, and its API's
    /// links.
    pub fn lite(&self) -> Value {
        let html = self.html_url();
        let api = format!("{}/repos/{}", api_base(&self.server_url), self.full_name);
        let id = self.number();
        let mut owner = user(&self.server_url, self.owner(), true);
        if self.owner_id != 0
            && let Some(o) = owner.as_object_mut()
        {
            o.insert("id".into(), json!(self.owner_id));
            o.insert("node_id".into(), json!(node_id("O", self.owner_id)));
        }
        let mut v = json!({
            "id": id,
            "node_id": node_id("R", id),
            "name": self.name(),
            "full_name": self.full_name,
            "private": self.private,
            "owner": owner,
            "html_url": html,
            "description": Value::Null,
            "fork": false,
            "url": api,
        });
        if let Some(m) = v.as_object_mut() {
            for (k, path) in [
                ("forks_url", "/forks"),
                ("keys_url", "/keys{/key_id}"),
                ("collaborators_url", "/collaborators{/collaborator}"),
                ("teams_url", "/teams"),
                ("hooks_url", "/hooks"),
                ("issue_events_url", "/issues/events{/number}"),
                ("events_url", "/events"),
                ("assignees_url", "/assignees{/user}"),
                ("branches_url", "/branches{/branch}"),
                ("tags_url", "/tags"),
                ("blobs_url", "/git/blobs{/sha}"),
                ("git_tags_url", "/git/tags{/sha}"),
                ("git_refs_url", "/git/refs{/sha}"),
                ("trees_url", "/git/trees{/sha}"),
                ("statuses_url", "/statuses/{sha}"),
                ("languages_url", "/languages"),
                ("stargazers_url", "/stargazers"),
                ("contributors_url", "/contributors"),
                ("subscribers_url", "/subscribers"),
                ("subscription_url", "/subscription"),
                ("commits_url", "/commits{/sha}"),
                ("git_commits_url", "/git/commits{/sha}"),
                ("comments_url", "/comments{/number}"),
                ("issue_comment_url", "/issues/comments{/number}"),
                ("contents_url", "/contents/{+path}"),
                ("compare_url", "/compare/{base}...{head}"),
                ("merges_url", "/merges"),
                ("archive_url", "/{archive_format}{/ref}"),
                ("downloads_url", "/downloads"),
                ("issues_url", "/issues{/number}"),
                ("pulls_url", "/pulls{/number}"),
                ("milestones_url", "/milestones{/number}"),
                ("notifications_url", "/notifications{?since,all,participating}"),
                ("labels_url", "/labels{/name}"),
                ("releases_url", "/releases{/id}"),
                ("deployments_url", "/deployments"),
            ] {
                m.insert(k.into(), json!(format!("{api}{path}")));
            }
        }
        v
    }

    /// The `repository` object of webhook payloads, as GitHub's.
    pub fn payload(&self) -> Value {
        let html = self.html_url();
        let host = self.server_url.trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/');
        let mut v = self.lite();
        if let Some(m) = v.as_object_mut() {
            for (k, x) in [
                ("created_at", json!(self.created_at)),
                ("updated_at", json!(rfc3339(self.created_at))),
                ("pushed_at", json!(self.created_at)),
                ("git_url", json!(format!("git://{host}/{}.git", self.full_name))),
                ("ssh_url", json!(format!("git@{host}:{}.git", self.full_name))),
                ("clone_url", json!(format!("{html}.git"))),
                ("svn_url", json!(html)),
                ("homepage", Value::Null),
                ("size", json!(0)),
                ("stargazers_count", json!(0)),
                ("watchers_count", json!(0)),
                ("language", Value::Null),
                ("has_issues", json!(false)),
                ("has_projects", json!(false)),
                ("has_downloads", json!(true)),
                ("has_wiki", json!(false)),
                ("has_pages", json!(false)),
                ("has_discussions", json!(false)),
                ("forks_count", json!(0)),
                ("mirror_url", Value::Null),
                ("archived", json!(false)),
                ("disabled", json!(false)),
                ("open_issues_count", json!(0)),
                ("license", Value::Null),
                ("allow_forking", json!(true)),
                ("is_template", json!(false)),
                ("web_commit_signoff_required", json!(false)),
                ("topics", json!([])),
                ("visibility", json!(if self.private { "private" } else { "public" })),
                ("forks", json!(0)),
                ("open_issues", json!(0)),
                ("watchers", json!(0)),
                ("default_branch", json!(self.default_branch)),
                ("master_branch", json!(self.default_branch)),
                ("stargazers", json!(0)),
                ("custom_properties", json!({})),
            ] {
                m.insert(k.into(), x);
            }
        }
        v
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
        "sender": user(&repo.server_url, &e.actor, false),
    })
}

/// A `merge_group` `checks_requested` payload: the land queue's candidate (W14) `head` at
/// `head_ref`, on `base_branch` at `base_sha`.
pub fn merge_group(repo: &Repo, base_branch: &str, base_sha: &str, head: &Commit, head_ref: &str, sender: &Person) -> Value {
    let who = |p: &Person| json!({ "name": p.name, "email": p.email });
    json!({
        "action": "checks_requested",
        "merge_group": {
            "head_sha": head.id,
            "head_ref": head_ref,
            "base_sha": base_sha,
            "base_ref": format!("refs/heads/{base_branch}"),
            "head_commit": { "id": head.id, "tree_id": head.tree_id, "message": head.message, "timestamp": head.timestamp, "author": who(&head.author), "committer": who(&head.committer) },
        },
        "repository": repo.payload(),
        "sender": user(&repo.server_url, &sender.username.clone().unwrap_or_else(|| sender.name.clone()), false),
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
    /// RFC 3339 (now, when unknown).
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    /// What it changes over its base (0 each when unknown).
    pub counts: Counts,
}

/// A pull request's commits and changes, as GitHub counts them: the commits on its head and not
/// its base, and the lines and files changed since their merge base.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub commits: u64,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
}

/// A `pull_request` (or `pull_request_target`) payload, as GitHub's: what kiln knows of the
/// pull request, with GitHub's links and the comment counts and states kiln doesn't keep at their
/// empty values. `synchronize` says the head's move (`before`, `after`).
pub fn pull_request(e: &Event, repo: &Repo, pr: &PullRequest) -> Value {
    let partial = pr.title.is_none() || pr.labels.is_none() || pr.author.is_none();
    let side = |s: &Side| {
        json!({
            "ref": s.branch, "sha": s.sha,
            "label": format!("{}:{}", s.repo.owner(), s.branch),
            "user": user(&s.repo.server_url, s.repo.owner(), true),
            "repo": s.repo.payload(),
        })
    };
    let api = format!("{}/repos/{}", api_base(&repo.server_url), repo.full_name);
    let html = format!("{}/pull/{}", repo.html_url(), pr.number);
    let (pr_api, issue_api) = (format!("{api}/pulls/{}", pr.number), format!("{api}/issues/{}", pr.number));
    let id = id_of(&format!("{}#{}", repo.full_name, pr.number));
    let state = if pr.state.is_empty() { "open" } else { pr.state.as_str() };
    let now = jiff::Timestamp::now().to_string();
    let link = |href: &str| json!({ "href": href });
    let mut p = json!({
        "url": pr_api,
        "id": id,
        "node_id": node_id("PR", id),
        "html_url": html,
        "diff_url": format!("{html}.diff"),
        "patch_url": format!("{html}.patch"),
        "issue_url": issue_api,
        "number": pr.number,
        "state": state,
        "locked": false,
        "created_at": pr.created_at.clone().unwrap_or_else(|| now.clone()),
        "updated_at": pr.updated_at.clone().unwrap_or_else(|| now.clone()),
        "closed_at": if state == "closed" { json!(pr.updated_at.clone().unwrap_or_else(|| now.clone())) } else { Value::Null },
        "merged_at": Value::Null,
        "merge_commit_sha": Value::Null,
        "assignee": Value::Null,
        "assignees": [],
        "requested_reviewers": [],
        "requested_teams": [],
        "milestone": Value::Null,
        "commits_url": format!("{pr_api}/commits"),
        "review_comments_url": format!("{pr_api}/comments"),
        "review_comment_url": format!("{api}/pulls/comments{{/number}}"),
        "comments_url": format!("{issue_api}/comments"),
        "statuses_url": format!("{api}/statuses/{}", pr.head.sha),
    });
    // (In parts: one literal this long is past `json!`'s recursion.)
    let rest = json!({
        "head": side(&pr.head),
        "base": side(&pr.base),
        "_links": {
            "self": link(&pr_api), "html": link(&html), "issue": link(&issue_api), "comments": link(&format!("{issue_api}/comments")),
            "review_comments": link(&format!("{pr_api}/comments")), "review_comment": link(&format!("{api}/pulls/comments{{/number}}")),
            "commits": link(&format!("{pr_api}/commits")), "statuses": link(&format!("{api}/statuses/{}", pr.head.sha)),
        },
        "author_association": "MEMBER",
        "auto_merge": Value::Null,
        "active_lock_reason": Value::Null,
        "draft": pr.draft.unwrap_or(false),
        "merged": false,
        "mergeable": Value::Null,
        "rebaseable": Value::Null,
        "mergeable_state": "unknown",
        "merged_by": Value::Null,
        "comments": 0,
        "review_comments": 0,
        "maintainer_can_modify": false,
        "commits": pr.counts.commits,
        "additions": pr.counts.additions,
        "deletions": pr.counts.deletions,
        "changed_files": pr.counts.changed_files,
    });
    if let (Some(m), Value::Object(rest)) = (p.as_object_mut(), rest) {
        m.extend(rest);
    }
    if let Some(m) = p.as_object_mut() {
        if let Some(t) = &pr.title {
            m.insert("title".into(), json!(t));
        }
        if let Some(b) = &pr.body {
            m.insert("body".into(), json!(b));
        }
        if let Some(a) = &pr.author {
            m.insert("user".into(), user(&repo.server_url, a, false));
        }
        if let Some(l) = &pr.labels {
            let label = |n: &String| {
                let id = id_of(&format!("{}:{n}", repo.full_name));
                json!({ "id": id, "node_id": node_id("LA", id), "url": format!("{api}/labels/{n}"), "name": n, "color": "ededed", "default": false, "description": Value::Null })
            };
            m.insert("labels".into(), json!(l.iter().map(label).collect::<Vec<_>>()));
        }
    }
    let mut out = json!({
        "action": e.action,
        "number": pr.number,
        "pull_request": p,
        "repository": repo.payload(),
        "sender": user(&repo.server_url, &e.actor, false),
    });
    if let Some(m) = out.as_object_mut() {
        if e.action == "synchronize" {
            m.insert("before".into(), json!(e.before.clone().unwrap_or_else(|| ZERO_SHA.to_owned())));
            m.insert("after".into(), json!(pr.head.sha));
        }
        if partial {
            m.insert("kiln".into(), json!({ "partial": true }));
        }
    }
    out
}

/// A comment on a pull request, as GitHub's payloads carry one.
#[derive(Debug, Clone, Default)]
pub struct Comment {
    pub id: u64,
    pub body: String,
    /// Its author's login.
    pub author: String,
    /// RFC 3339.
    pub created_at: String,
    /// A comment on lines: its file, last line and commit.
    pub path: Option<String>,
    pub line: Option<u64>,
    pub commit: Option<String>,
}

/// A review of a pull request, as `pull_request_review` carries one.
#[derive(Debug, Clone, Default)]
pub struct Review {
    pub id: u64,
    /// GitHub's: `approved`, `changes_requested` or `commented`.
    pub state: String,
    pub body: String,
    pub author: String,
    pub submitted_at: String,
    pub commit: String,
}

fn comment_json(c: &Comment, pr_url: &str) -> Value {
    let mut v = json!({
        "id": c.id, "node_id": format!("kiln-comment-{}", c.id), "body": c.body,
        "user": { "login": c.author, "type": "User" }, "created_at": c.created_at, "updated_at": c.created_at,
        "html_url": format!("{pr_url}#discussion_r{}", c.id), "author_association": "MEMBER",
    });
    if let (Some(m), Some(path)) = (v.as_object_mut(), &c.path) {
        m.insert("path".into(), json!(path));
        m.insert("line".into(), json!(c.line));
        m.insert("side".into(), json!("RIGHT"));
        m.insert("commit_id".into(), json!(c.commit));
    }
    v
}

/// The `pull_request` payload `pr` with its top-level `number` taken out and `key` put in, the
/// sender `actor`: the shape of GitHub's events about a pull request's conversation.
fn about(pr: &Value, key: &str, item: Value, action: &str, actor: &str) -> Value {
    let mut v = pr.clone();
    if let Some(m) = v.as_object_mut() {
        m.remove("number");
        m.insert("action".into(), json!(action));
        m.insert(key.into(), item);
        m.insert("sender".into(), json!({ "login": actor, "type": "User" }));
    }
    v
}

/// `issue_comment` (`created`) from a pull request's `pull_request` payload: the pull request as an
/// issue (linking back to it), and the comment.
pub fn issue_comment(pr: &Value, c: &Comment, actor: &str) -> Value {
    let p = pr.get("pull_request").cloned().unwrap_or_default();
    let url = p.get("html_url").and_then(Value::as_str).unwrap_or_default().to_owned();
    let issue = json!({
        "number": p.get("number"), "title": p.get("title"), "body": p.get("body"), "state": p.get("state"),
        "user": p.get("user"), "labels": p.get("labels").cloned().unwrap_or_else(|| json!([])), "html_url": url,
        "pull_request": { "html_url": url },
    });
    let mut v = json!({
        "action": "created", "issue": issue, "comment": comment_json(c, &url),
        "repository": pr.get("repository"), "sender": { "login": actor, "type": "User" },
    });
    if let (Some(m), Some(k)) = (v.as_object_mut(), pr.get("kiln")) {
        m.insert("kiln".into(), k.clone());
    }
    v
}

/// `pull_request_review_comment` (`created`): the comment on the pull request's lines.
pub fn pull_request_review_comment(pr: &Value, c: &Comment, actor: &str) -> Value {
    let url = pr.pointer("/pull_request/html_url").and_then(Value::as_str).unwrap_or_default().to_owned();
    about(pr, "comment", comment_json(c, &url), "created", actor)
}

/// `pull_request_review` (`submitted`): the review.
pub fn pull_request_review(pr: &Value, r: &Review, actor: &str) -> Value {
    let url = pr.pointer("/pull_request/html_url").and_then(Value::as_str).unwrap_or_default().to_owned();
    let review = json!({
        "id": r.id, "node_id": format!("kiln-review-{}", r.id), "user": { "login": r.author, "type": "User" }, "body": r.body,
        "state": r.state, "commit_id": r.commit, "submitted_at": r.submitted_at, "html_url": format!("{url}#pullrequestreview-{}", r.id),
        "author_association": "MEMBER",
    });
    about(pr, "review", review, "submitted", actor)
}

/// The names a comment mentions (`@ada`, `@review-bot`), in order, once each.
pub fn mentions(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let chars: Vec<char> = body.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        // `ada@example.com` mentions no one.
        if *c != '@' || i.checked_sub(1).and_then(|j| chars.get(j)).is_some_and(char::is_ascii_alphanumeric) {
            continue;
        }
        let name: String = chars.iter().skip(i + 1).take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '[' | ']')).collect();
        let name = name.trim_end_matches('.').to_owned();
        if !name.is_empty() && !out.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            out.push(name);
        }
    }
    out
}

pub fn workflow_dispatch(e: &Event, repo: &Repo, workflow_path: &str, inputs: &Value) -> Value {
    json!({ "inputs": inputs, "ref": e.git_ref, "repository": repo.payload(), "sender": user(&repo.server_url, &e.actor, false), "workflow": workflow_path })
}

/// A `repository_dispatch` payload.
pub fn repository_dispatch(e: &Event, repo: &Repo, client_payload: &Value) -> Value {
    json!({ "action": e.action, "branch": repo.default_branch, "client_payload": client_payload, "repository": repo.payload(), "sender": user(&repo.server_url, &e.actor, false) })
}

/// A `schedule` payload: the cron that fired.
pub fn schedule(cron: &str, repo: &Repo) -> Value {
    json!({ "schedule": cron, "repository": repo.payload() })
}

/// A run, as `workflow_run` payloads describe it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunSummary {
    /// `github.run_id`.
    pub id: u64,
    /// The workflow's name (its path when it has none).
    pub name: String,
    pub path: String,
    pub display_title: String,
    pub run_number: u64,
    pub run_attempt: u32,
    pub event: String,
    /// `requested`, `in_progress` or `completed`.
    pub status: String,
    pub conclusion: Option<String>,
    pub head_branch: String,
    pub head_sha: String,
    pub head_message: String,
    pub actor: String,
    /// RFC 3339.
    pub created_at: String,
    pub updated_at: String,
}

/// A `workflow_run` payload (W03-R02): `action` is `requested`, `in_progress` or `completed`.
pub fn workflow_run(action: &str, r: &RunSummary, repo: &Repo) -> Value {
    let repos = format!("{}/repos/{}", api_base(&repo.server_url), repo.full_name);
    let api = format!("{repos}/actions/runs/{}", r.id);
    let actor = user(&repo.server_url, &r.actor, false);
    let (workflow_id, suite) = (id_of(&format!("{}:{}", repo.full_name, r.path)), id_of(&format!("{}#{}", repo.full_name, r.id)));
    let workflow_url = format!("{repos}/actions/workflows/{workflow_id}");
    let who = json!({ "name": r.actor, "email": Value::Null });
    let mut run = json!({
        "id": r.id,
        "name": r.name,
        "node_id": node_id("WFR", r.id),
        "head_branch": r.head_branch,
        "head_sha": r.head_sha,
        "path": r.path,
        "display_title": r.display_title,
        "run_number": r.run_number,
        "run_attempt": r.run_attempt,
        "event": r.event,
        "status": r.status,
        "conclusion": r.conclusion,
        "workflow_id": workflow_id,
        "workflow_url": workflow_url,
        "check_suite_id": suite,
        "check_suite_node_id": node_id("CS", suite),
        "check_suite_url": format!("{repos}/check-suites/{suite}"),
        "url": api,
        "html_url": format!("{}/actions/runs/{}", repo.html_url(), r.id),
        "jobs_url": format!("{api}/jobs"),
        "logs_url": format!("{api}/logs"),
        "artifacts_url": format!("{api}/artifacts"),
        "cancel_url": format!("{api}/cancel"),
        "rerun_url": format!("{api}/rerun"),
    });
    // (In parts: one literal this long is past `json!`'s recursion.)
    let rest = json!({
        "previous_attempt_url": if r.run_attempt > 1 { json!(format!("{api}/attempts/{}", r.run_attempt - 1)) } else { Value::Null },
        "created_at": r.created_at,
        "updated_at": r.updated_at,
        "run_started_at": r.created_at,
        "actor": actor,
        "triggering_actor": actor,
        "head_commit": { "id": r.head_sha, "tree_id": "", "message": r.head_message, "timestamp": r.created_at, "author": who, "committer": who },
        "repository": repo.lite(),
        "head_repository": repo.lite(),
        "pull_requests": [],
        "referenced_workflows": [],
    });
    if let (Some(m), Value::Object(rest)) = (run.as_object_mut(), rest) {
        m.extend(rest);
    }
    json!({
        "action": action,
        "workflow_run": run,
        "workflow": {
            "id": workflow_id,
            "node_id": node_id("W", workflow_id),
            "name": r.name,
            "path": r.path,
            "state": "active",
            "created_at": rfc3339(repo.created_at),
            "updated_at": rfc3339(repo.created_at),
            "url": workflow_url,
            "html_url": format!("{}/blob/{}/{}", repo.html_url(), repo.default_branch, r.path),
            "badge_url": format!("{}/workflows/{}/badge.svg", repo.html_url(), r.name),
        },
        "repository": repo.payload(),
        "sender": actor,
    })
}

#[cfg(test)]
mod conversation_tests {
    use super::*;

    fn pr() -> Value {
        json!({ "action": "opened", "number": 7, "pull_request": { "number": 7, "title": "A change", "body": "Why", "state": "open", "user": { "login": "ada" }, "html_url": "https://kiln.example/acme/app/pull/7", "labels": [] }, "repository": { "full_name": "acme/app" }, "sender": { "login": "ada" } })
    }

    #[test]
    fn w03_conversation_events_are_githubs() {
        let at = |v: &Value, p: &str| v.pointer(p).cloned().unwrap_or(Value::Null);
        let c = Comment { id: 3, body: "/deploy @bo".into(), author: "bo".into(), created_at: "2026-10-19T00:00:00Z".into(), ..Comment::default() };
        let v = issue_comment(&pr(), &c, "bo");
        assert_eq!(at(&v, "/action"), json!("created"));
        assert_eq!(at(&v, "/issue/number"), json!(7));
        assert_eq!(at(&v, "/issue/pull_request/html_url"), json!("https://kiln.example/acme/app/pull/7"));
        assert_eq!(at(&v, "/comment/body"), json!("/deploy @bo"));
        assert_eq!(at(&v, "/sender/login"), json!("bo"));
        assert!(v.pointer("/comment/path").is_none(), "a comment on the whole pull request has no path");
        let line = Comment { path: Some("src/a.rs".into()), line: Some(12), commit: Some("f".repeat(40)), ..c.clone() };
        let v = pull_request_review_comment(&pr(), &line, "bo");
        assert_eq!((at(&v, "/action"), at(&v, "/comment/path"), at(&v, "/comment/line")), (json!("created"), json!("src/a.rs"), json!(12)));
        assert_eq!(at(&v, "/pull_request/number"), json!(7));
        assert!(v.get("number").is_none(), "GitHub's review events have no top-level number");
        let r = Review { id: 9, state: "approved".into(), body: "lgtm".into(), author: "bo".into(), submitted_at: "2026-10-19T00:00:00Z".into(), commit: "e".repeat(40) };
        let v = pull_request_review(&pr(), &r, "bo");
        assert_eq!((at(&v, "/action"), at(&v, "/review/state"), at(&v, "/review/user/login")), (json!("submitted"), json!("approved"), json!("bo")));
    }

    #[test]
    fn w03_mentions() {
        assert_eq!(mentions("@ada and @review-bot[bot], thanks @ada."), vec!["ada".to_owned(), "review-bot[bot]".to_owned()]);
        assert!(mentions("mail ada@example.com").is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(v: &Value, p: &str) -> Value {
        v.pointer(p).cloned().unwrap_or(Value::Null)
    }

    fn repo() -> Repo {
        Repo { full_name: "acme/app".into(), id: 7, owner_id: 3, default_branch: "main".into(), private: false, server_url: "https://kiln.example".into(), created_at: 1_792_000_000 }
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

    /// W03-R07: the counts a workflow sizes its fetch by (`--depth=$(( commits + 1 ))`).
    #[test]
    fn w03_r07_a_pull_request_counts_its_commits_and_changes() {
        let e = Event { name: "pull_request".into(), action: "opened".into(), actor: "mona".into(), ..Event::default() };
        let pr = PullRequest { number: 3, counts: Counts { commits: 2, additions: 10, deletions: 4, changed_files: 3 }, ..PullRequest::default() };
        let p = pull_request(&e, &repo(), &pr);
        assert_eq!([at(&p, "/pull_request/commits"), at(&p, "/pull_request/additions"), at(&p, "/pull_request/deletions"), at(&p, "/pull_request/changed_files")], [2, 10, 4, 3]);
        let unknown = pull_request(&e, &repo(), &PullRequest { number: 3, ..PullRequest::default() });
        assert_eq!(at(&unknown, "/pull_request/commits"), 0);
    }
}
