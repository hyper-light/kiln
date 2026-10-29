//! An agent's change request (docs/agentic.md C1, C6): its work, from its folder, committed and
//! pushed for review, its author the agent, on behalf of its person.
//!
//! - **A repository kiln hosts:** the machine pushes `refs/for/<trunk>/branch/agents/<name>` to
//!   kiln's git with an agent token (`kiln_agt_…`, minted for this push only, for `agent:<id>`
//!   acting for its person, limited to that repository): kiln makes the change request, and its
//!   author is the agent, which is how comments, reviews, CI results and the land queue find their
//!   way back to it ([`super::follow`]).
//! - **One kiln follows elsewhere** (GitHub, GitLab, Bitbucket): the machine pushes the branch to
//!   the folder's remote, with its own credentials, as a person there would; kiln opens the pull
//!   request through the provider when it has an integration with it, and otherwise says where to.
//!
//! Each push is a new version: the commit is the folder's last commit with the agent's files on
//! it, made beside the folder (`super::git`), so the person's branch, index and working tree are
//! never touched. Only the files the agent changed go (git's own ignore rules apply); a person's
//! own edits in the same folder stay theirs.

use std::sync::Arc;

use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::git::{Facts, Ident, Remote};
use crate::harness::session::Info;
use crate::hub::Hub;
use crate::ops::Fail;
use crate::store::Repo;

/// Where an agent's change request is kept (`kv`).
pub fn key(agent: &str) -> String {
    format!("agents/change/{agent}")
}

/// An agent's change request, as kiln keeps it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentChange"))]
pub struct Record {
    pub agent: String,
    /// The repository as kiln knows it (`owner/name`), when it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub repo: Option<String>,
    /// `kiln`, `github`, `gitlab`, `bitbucket`, or `git` (a remote kiln has no integration with).
    pub provider: String,
    /// `CR12` (kiln), `#34` (a pull or merge request), or none: the branch is pushed, the change
    /// not opened yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub change: Option<String>,
    /// Its page, or where to open it.
    pub url: String,
    pub branch: String,
    pub trunk: String,
    /// The commit last pushed.
    pub sha: String,
    /// How many versions have been pushed.
    pub version: u32,
    pub files: Vec<String>,
    pub title: String,
    /// `open`, `landed` or `closed`.
    pub state: String,
    /// The review state of a change kiln hosts (`needs-review`, `approved`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub review: Option<String>,
    /// How its newest version's checks went (`passed`, `failed`, …), once kiln knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub checks: Option<String>,
    /// Whether failing checks go to the agent to fix (C2).
    pub fix_checks: bool,
    pub opened_at: i64,
    pub updated_at: i64,
    /// The plan run whose stack it is a step of (docs/agentic.md P4), when it is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub stack: Option<InStack>,
}

/// A change that is one step of a plan run's stack.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct InStack {
    pub run: String,
    /// Its step, from 1.
    pub step: usize,
}

/// What a person or the agent asks for.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Want {
    pub title: Option<String>,
    pub body: Option<String>,
    pub draft: bool,
    pub trunk: Option<String>,
    /// Failing checks go to the agent (default: yes).
    pub fix_checks: Option<bool>,
}

/// Agent `agent`'s change request, if it has one.
pub async fn of(hub: &Hub, agent: &str) -> Option<Record> {
    let text = hub.store.kv_get(key(agent)).await.ok().flatten()?;
    serde_json::from_str(&text).ok()
}

/// Every agent's change request, newest first.
pub async fn all(hub: &Hub) -> Vec<Record> {
    let mut v: Vec<Record> = hub.store.kv_prefix("agents/change/".to_owned()).await.unwrap_or_default().into_iter().filter_map(|(_, t)| serde_json::from_str(&t).ok()).collect();
    v.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
    v
}

pub async fn save(hub: &Hub, r: &Record) {
    if let Ok(text) = serde_json::to_string(r) {
        let _ = hub.store.kv_set(key(&r.agent), text).await;
    }
    hub.emit(&crate::wire::events::ServerEvent::AgentChange { agent: r.agent.clone(), change: Box::new(r.clone()) });
}

fn fail(code: StatusCode, msg: impl Into<String>) -> Fail {
    (code, msg.into())
}

/// Asks the machine agent `info` works on about its folder: the answer's JSON, or why not.
pub async fn ask_machine(hub: &Hub, info: &Info, ask: &super::Ask) -> Result<Value, Fail> {
    let json = serde_json::to_string(ask).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let machine = crate::harness::wait::machine(&info.target).to_owned();
    let answer = if machine == "local" {
        super::serve(std::path::Path::new(&info.cwd), std::path::Path::new(info.work_dir()), info.workspace.as_ref(), &json).await
    } else {
        let id = info.id.clone();
        let got = hub
            .ask_runner(&machine, crate::proto::CAP_WORKSPACE, super::WAIT, move |req| crate::proto::ServerMsg::Workspace { req, session: id, json })
            .await
            .map_err(|e| fail(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
        match got {
            None => return Err(fail(StatusCode::CONFLICT, format!("{machine}'s kiln is too old to do this: upgrade it (System → Updates)"))),
            Some(text) => text.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null),
        }
    };
    if let Some(e) = answer.get("error").and_then(Value::as_str) {
        return Err(fail(StatusCode::CONFLICT, e));
    }
    Ok(answer)
}

/// A repository URL as compared: no scheme, user, port or `.git`, the host in lower case
/// (`git@github.com:acme/app.git` and `https://github.com/acme/app` are both
/// `github.com/acme/app`).
pub fn norm_url(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let (host, path) = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/').unwrap_or((rest, "")),
        // scp-like: user@host:path
        None => match url.split_once(':') {
            Some((h, p)) if !h.contains('/') => (h, p),
            _ => return url.to_owned(),
        },
    };
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let host = host.split_once(':').map_or(host, |(h, _)| h).to_ascii_lowercase();
    format!("{host}/{}", path.trim_start_matches('/'))
}

/// Of `repos`, the one a remote of the folder is (kiln's own git for one it hosts, else its
/// clone or mirror URL), and that remote's name.
pub fn matched<'a>(repos: &'a [Repo], remotes: &[Remote], public_url: &str) -> Option<(&'a Repo, String)> {
    // `origin` first: the remote a folder is usually cloned from.
    let mut order: Vec<&Remote> = remotes.iter().collect();
    order.sort_by_key(|r| r.name != "origin");
    for remote in order {
        let have = norm_url(&remote.url);
        for repo in repos {
            let mut urls = vec![norm_url(&repo.clone_url)];
            urls.extend(repo.mirror.as_deref().map(norm_url));
            if repo.mode == "native" {
                let base = public_url.trim_end_matches('/');
                urls.push(norm_url(&format!("{base}/git/{}", repo.name)));
                urls.push(norm_url(&format!("{base}/{}", repo.name)));
            }
            if urls.contains(&have) {
                return Some((repo, remote.name.clone()));
            }
        }
    }
    None
}

/// Agent branches: `agents/<what it's about>-<its id's start>`.
pub fn branch_for(info: &Info) -> String {
    let about = [&info.title, &info.goal, &info.name].into_iter().map(|s| s.trim()).find(|s| !s.is_empty()).unwrap_or("work");
    let (slug, short) = (slug(about), id_end(&info.id));
    if slug.is_empty() { format!("agents/{short}") } else { format!("agents/{slug}-{short}") }
}

/// `text` as a branch name takes it: lower case letters and digits, `-` between words, at most
/// about 40 of them.
pub fn slug(text: &str) -> String {
    let mut slug = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= 40 {
            break;
        }
    }
    slug.trim_end_matches('-').to_owned()
}

/// The end of a kiln id: its ids start with the time they were made, so those made within a
/// minute of each other share their beginnings, never their ends.
pub fn id_end(id: &str) -> String {
    let alnum: Vec<char> = id.chars().filter(char::is_ascii_alphanumeric).collect();
    alnum.get(alnum.len().saturating_sub(8)..).unwrap_or_default().iter().collect()
}

/// Where to open a pull request for `branch` into `trunk` on a known host, by its remote's URL.
pub fn compare_url(remote_url: &str, trunk: &str, branch: &str) -> Option<String> {
    let n = norm_url(remote_url);
    let (host, path) = n.split_once('/')?;
    match host {
        "github.com" => Some(format!("https://github.com/{path}/compare/{trunk}...{branch}?expand=1")),
        "gitlab.com" => Some(format!("https://gitlab.com/{path}/-/merge_requests/new?merge_request%5Bsource_branch%5D={branch}&merge_request%5Btarget_branch%5D={trunk}")),
        "bitbucket.org" => Some(format!("https://bitbucket.org/{path}/pull-requests/new?source={branch}&dest={trunk}")),
        _ => None,
    }
}

pub(crate) fn provider_of(host: Option<&crate::githost::GitHost>) -> &'static str {
    match host {
        Some(crate::githost::GitHost::Kiln) => "kiln",
        Some(crate::githost::GitHost::Github(_)) => "github",
        Some(crate::githost::GitHost::Gitlab(_)) => "gitlab",
        Some(crate::githost::GitHost::Bitbucket(_)) => "bitbucket",
        None => "git",
    }
}

/// The title a change gets: asked for, else what the agent is about.
fn title_of(info: &Info, want: &Want) -> String {
    let asked = want.title.as_deref().map(str::trim).filter(|t| !t.is_empty());
    let line = asked.or_else(|| [&info.title, &info.goal, &info.name].into_iter().map(|s| s.trim()).find(|s| !s.is_empty())).unwrap_or("An agent's work");
    line.lines().next().unwrap_or(line).chars().take(120).collect()
}

/// Opens agent `info`'s change request from its work, or pushes a new version of the one it has.
/// `actor` is who asked, as the audit trail names them.
pub async fn open(hub: &Arc<Hub>, actor: &str, info: &Info, want: Want) -> Result<Record, Fail> {
    // A plan step's agent: its work is its step's in the run's stack.
    if let Some(run) = info.plan_run.as_deref().filter(|r| !r.is_empty())
        && let Some(r) = crate::plans::stack::amend(hub, actor, info, &want, run).await?
    {
        return Ok(r);
    }
    let paths = paths_of(hub, &info.id);
    // In a workspace of its own, everything that changed there is its: its commands' changes too.
    let own = info.workspace.as_ref().is_some_and(|w| w.made.is_some());
    if paths.is_empty() && !own {
        return Err(fail(StatusCode::CONFLICT, format!("{} hasn't changed any files yet", info.name)));
    }
    let facts: Facts = serde_json::from_value(ask_machine(hub, info, &super::Ask::Git).await?.get("facts").cloned().unwrap_or_default()).map_err(|e| fail(StatusCode::BAD_GATEWAY, format!("the machine's answer: {e}")))?;
    let repos = hub.store.repos().await.map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let found = matched(&repos, &facts.remotes, &hub.cfg.server.public_url).map(|(r, name)| (r.clone(), name));
    let host = match &found {
        Some((repo, _)) => crate::githost::for_repo(hub, repo).await,
        None => None,
    };
    let native = matches!(host, Some(crate::githost::GitHost::Kiln));
    let previous = of(hub, &info.id).await;
    let branch = previous.as_ref().map_or_else(|| branch_for(info), |p| p.branch.clone());
    let trunk = want
        .trunk
        .clone()
        .filter(|t| !t.trim().is_empty())
        .or_else(|| previous.as_ref().map(|p| p.trunk.clone()))
        .or_else(|| found.as_ref().and_then(|(r, _)| r.default_branch.clone()))
        .or_else(|| facts.default_branch.clone())
        // In a worktree, the branch its checkout has (its own branch is the agent's).
        .or_else(|| facts.main_branch.clone())
        .or_else(|| facts.branch.clone())
        .unwrap_or_else(|| "main".to_owned());
    let title = title_of(info, &want);
    let mut message = title.clone();
    if let Some(body) = want.body.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
        message.push_str("\n\n");
        message.push_str(body);
    }
    message.push_str(&format!("\n\nKiln-Agent: {} ({}, {})\n", info.name, info.kind, info.id));
    let opener = hub.store.kv_get(crate::mcp::opener_key(&info.id)).await.ok().flatten().unwrap_or_else(|| "owner".to_owned());
    // Where it goes, and as whom.
    let (to, to_ref, bearer, token_id) = if native {
        let Some((repo, _)) = &found else { return Err(fail(StatusCode::INTERNAL_SERVER_ERROR, "no repository")) };
        let grants = [format!("diffs.read@repo:{}", repo.name), format!("diffs.submit@repo:{}", repo.name)];
        let grants: Vec<&str> = grants.iter().map(String::as_str).collect();
        let (id, token) = crate::authz::tokens::mint_agent(hub, &info.id, &opener, &grants, Some(1), &format!("{}'s change request", info.name))
            .await
            .map_err(|e| fail(StatusCode::FORBIDDEN, format!("an agent token for {}: {e:#}", info.name)))?;
        let url = format!("{}/git/{}.git", hub.cfg.server.public_url.trim_end_matches('/'), repo.name);
        let draft = if want.draft { "%draft" } else { "" };
        (url, format!("refs/for/{trunk}/branch/{branch}{draft}"), Some(token), Some(id))
    } else {
        let remote = match &found {
            Some((_, name)) => name.clone(),
            None if facts.remotes.iter().any(|r| r.name == "origin") => "origin".to_owned(),
            None => facts.remotes.first().map(|r| r.name.clone()).ok_or_else(|| fail(StatusCode::CONFLICT, format!("{} has no remote to push the agent's work to", facts.top)))?,
        };
        (remote, format!("refs/heads/{branch}"), None, None)
    };
    let ask = super::Ask::Change(super::Change {
        paths: (!own).then_some(paths),
        message,
        fallback: Ident { name: format!("{} (kiln agent)", info.name), email: "agent@kiln.invalid".to_owned() },
        to: to.clone(),
        to_ref,
        bearer,
    });
    let pushed = ask_machine(hub, info, &ask).await;
    // The token was for this push only.
    if let Some(id) = &token_id {
        let id = id.clone();
        let now = crate::util::now_unix();
        let _ = hub.store.call(move |c| Ok(c.execute("UPDATE tokens SET revoked_at = ?2 WHERE id = ?1", rusqlite::params![id, now])?)).await;
    }
    let pushed = pushed?;
    let made: super::git::Made = serde_json::from_value(pushed.get("made").cloned().unwrap_or_default()).map_err(|e| fail(StatusCode::BAD_GATEWAY, format!("the machine's answer: {e}")))?;
    let said = pushed.get("said").and_then(Value::as_str).unwrap_or_default().to_owned();
    // The change itself.
    let remote_url = found.as_ref().map_or_else(|| facts.remotes.iter().find(|r| r.name == to).map(|r| r.url.clone()).unwrap_or_default(), |(r, _)| r.clone_url.clone());
    let mut version = previous.as_ref().map_or(1, |p| p.version.saturating_add(1));
    let (change, url, review) = match (&host, &found) {
        (Some(crate::githost::GitHost::Kiln), Some((repo, _))) => match crate::ci::cr::open_of(hub, &repo.id, &trunk, &branch).await {
            Ok(Some((id, number))) => {
                let name = format!("CR{number}");
                let review = crate::ci::diffs::find(hub, &name).await.ok().flatten().map(|d| d.state);
                // Its versions are kiln's: pushes the agent made itself count too.
                let n: Option<u32> = hub.store.call(move |c| Ok(c.query_row("SELECT MAX(n) FROM diff_versions WHERE diff_id = ?1", [&id], |r| r.get(0))?)).await.ok().flatten();
                version = n.unwrap_or(version);
                (Some(name), format!("{}/diffs/CR{number}", hub.cfg.server.public_url.trim_end_matches('/')), review)
            }
            _ => return Err(fail(StatusCode::BAD_GATEWAY, format!("the push went through but no change request came of it: {}", said.trim()))),
        },
        (Some(h), Some((repo, _))) => match previous.as_ref().and_then(|p| p.change.clone()) {
            // A new version of a pull request is its branch's new commit: nothing to open.
            Some(c) => (Some(c), previous.as_ref().map(|p| p.url.clone()).unwrap_or_default(), None),
            None => match h.open_change(hub, repo, &branch, &trunk, &title, want.body.as_deref().unwrap_or_default()).await {
                Ok(c) => (Some(format!("#{}", c.number)), c.url, None),
                Err(e) => {
                    tracing::info!(agent = %info.id, error = %format!("{e:#}"), "a pull request didn't open: the branch is pushed");
                    (None, compare_url(&remote_url, &trunk, &branch).unwrap_or_default(), None)
                }
            },
        },
        _ => (None, compare_url(&remote_url, &trunk, &branch).unwrap_or_default(), None),
    };
    let now = crate::util::now_unix();
    let record = Record {
        agent: info.id.clone(),
        repo: found.as_ref().map(|(r, _)| r.name.clone()),
        provider: provider_of(host.as_ref()).to_owned(),
        change,
        url,
        branch: branch.clone(),
        trunk: trunk.clone(),
        sha: made.sha.clone(),
        version,
        files: made.files,
        title,
        state: "open".to_owned(),
        review,
        checks: None,
        fix_checks: want.fix_checks.or(previous.as_ref().map(|p| p.fix_checks)).unwrap_or(true),
        opened_at: previous.as_ref().map_or(now, |p| p.opened_at),
        updated_at: now,
        stack: None,
    };
    save(hub, &record).await;
    let what = record.change.clone().unwrap_or_else(|| format!("branch {branch}"));
    hub.store.audit(actor.to_owned(), "agent.change".into(), format!("{} ({}): {what} v{} into {trunk}", info.name, lineage_short(&info.id), record.version)).await;
    Ok(record)
}

fn lineage_short(id: &str) -> &str {
    crate::harness::lineage::short(id)
}

/// The files agent `id` changed, as its changeset names them.
pub fn paths_of(hub: &Hub, id: &str) -> Vec<String> {
    let (_, _, changes) = hub.harness.conclusion(id).unwrap_or_default();
    paths_in(&changes, hub.harness.info(id).and_then(|i| i.tree).as_deref())
}

/// The files of `changes` (a changeset) in working tree `tree`, as git names them there (a
/// worktree's own, not the checkout's it was made from); with none, every file, as named.
fn paths_in(changes: &Value, tree: Option<&str>) -> Vec<String> {
    let mine = |f: &&Value| tree.is_none_or(|t| f.get("tree").and_then(Value::as_str) == Some(t));
    changes.as_array().into_iter().flatten().filter(mine).filter_map(|f| f.get("path").and_then(Value::as_str)).map(str::to_owned).collect()
}

/// A record as `agents.change` and `agents.result` answer with it.
pub fn shown(r: &Record) -> Value {
    serde_json::to_value(r).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str, url: &str, mode: &str) -> Repo {
        Repo { id: name.into(), name: name.into(), clone_url: url.into(), branches: Vec::new(), poll_seconds: 60, enabled: true, created_at: 0, last_polled_at: None, last_error: None, mode: mode.into(), default_branch: None, mirror: None }
    }

    #[test]
    fn its_change_request_takes_the_files_of_the_working_tree_its_work_is_in() {
        let changes = serde_json::json!([
            {"path": "README.md", "tree": "/w/repo/.claude/worktrees/f"},
            {"path": "notes.txt", "tree": "/w/repo/.claude/worktrees/f"},
            {"path": "src/old.rs", "tree": "/w/repo"},
            {"path": "/tmp/scratch.txt"}
        ]);
        assert_eq!(paths_in(&changes, Some("/w/repo/.claude/worktrees/f")), ["README.md", "notes.txt"]);
        assert_eq!(paths_in(&changes, Some("/w/repo")), ["src/old.rs"]);
        assert_eq!(paths_in(&changes, None), ["README.md", "notes.txt", "src/old.rs", "/tmp/scratch.txt"], "no working tree known: every file, as named");
    }

    #[test]
    fn a_folder_s_remote_finds_the_repository_kiln_knows_it_as() {
        assert_eq!(norm_url("git@github.com:Acme/App.git"), "github.com/Acme/App");
        assert_eq!(norm_url("https://user:pw@GitHub.com:443/Acme/App/"), "github.com/Acme/App");
        assert_eq!(norm_url("ssh://git@github.com/Acme/App.git"), "github.com/Acme/App");
        let repos = vec![repo("acme/app", "https://github.com/acme/app.git", "upstream"), repo("acme/kept", "file:///data/hosted/acme/kept.git", "native")];
        let remotes = |url: &str| vec![Remote { name: "up".into(), url: "git@elsewhere:x/y.git".into() }, Remote { name: "origin".into(), url: url.into() }];
        let hit = matched(&repos, &remotes("git@github.com:acme/app.git"), "https://kiln.example.com").map(|(r, n)| (r.name.clone(), n));
        assert_eq!(hit, Some(("acme/app".to_owned(), "origin".to_owned())));
        // A repository kiln hosts, cloned from kiln's own git.
        let hit = matched(&repos, &remotes("https://kiln.example.com/git/acme/kept.git"), "https://kiln.example.com/").map(|(r, n)| (r.name.clone(), n));
        assert_eq!(hit, Some(("acme/kept".to_owned(), "origin".to_owned())));
        assert!(matched(&repos, &remotes("git@github.com:acme/other.git"), "https://kiln.example.com").is_none());
    }

    #[test]
    fn an_agent_s_branch_says_what_it_is_about_and_whose_it_is() {
        let info = Info { id: "0192abcd5f3a2b1c9d8e7f6a5b4c3d2e".into(), title: "Fix the flaky reconnect test (again!)".into(), ..Info::default() };
        assert_eq!(branch_for(&info), "agents/fix-the-flaky-reconnect-test-again-5b4c3d2e");
        // Two agents started in the same minute share their ids' beginnings, not their branches.
        let twin = Info { id: "0192abcd5f3a2b1c9d8e7f6a11112222".into(), title: info.title.clone(), ..Info::default() };
        assert_ne!(branch_for(&info), branch_for(&twin));
        let info = Info { id: "abcdef1234".into(), ..Info::default() };
        assert_eq!(branch_for(&info), "agents/work-cdef1234");
        let long = Info { id: "12345678".into(), title: "x".repeat(200), ..Info::default() };
        assert!(branch_for(&long).len() <= "agents/".len() + 41 + 8, "{}", branch_for(&long));
        assert!(crate::ci::cr::branch_ok(&branch_for(&info), &["main".to_owned()]).is_ok());
    }

    #[test]
    fn a_pull_request_opens_where_its_host_says() {
        assert_eq!(compare_url("git@github.com:acme/app.git", "main", "agents/x-1").as_deref(), Some("https://github.com/acme/app/compare/main...agents/x-1?expand=1"));
        assert!(compare_url("https://gitlab.com/g/p.git", "main", "agents/x").is_some_and(|u| u.contains("merge_requests/new")));
        assert!(compare_url("https://bitbucket.org/w/r.git", "main", "agents/x").is_some_and(|u| u.contains("pull-requests/new")));
        assert_eq!(compare_url("git@git.internal:a/b.git", "main", "x"), None);
    }
}

/// `agents.change` end to end on a hub of its own, under a timeout: an agent kiln runs on the
/// server, in a folder cloned from a remote, its work pushed there as its branch (no harness ever
/// runs: its program doesn't exist, and its changeset is set as its machine would report it).
#[cfg(all(test, unix))]
mod end_to_end {
    use std::path::Path;
    use std::process::Command;
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::{Value, json};

    use crate::harness::session::Spec;
    use crate::ops::{Cx, call};

    fn run(f: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static) -> anyhow::Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::rt::block_on(f));
        });
        match rx.recv_timeout(Duration::from_secs(90)) {
            Ok(r) => r,
            Err(_) => anyhow::bail!("the operation hung"),
        }
    }

    fn sh(dir: &Path, script: &str) -> anyhow::Result<String> {
        let out = Command::new("sh").arg("-c").arg(script).current_dir(dir).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").output()?;
        anyhow::ensure!(out.status.success(), "{script}: {}", String::from_utf8_lossy(&out.stderr));
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    fn get<'a>(v: &'a Value, pointer: &str) -> &'a Value {
        v.pointer(pointer).unwrap_or(&Value::Null)
    }

    #[test]
    fn an_agent_s_work_goes_up_as_its_branch_and_its_folder_stays_as_it_was() -> anyhow::Result<()> {
        run(async {
            let hub = crate::hub::tests::scratch_hub_with(|c| c.terminal.enabled = true).await?;
            let h = Arc::clone(&hub);
            crate::rt::spawn(async move {
                let _ = crate::harness::registry::Registry::run_local(h).await;
            });
            let bare = tempfile::tempdir()?;
            sh(bare.path(), "git init -q --bare -b main")?;
            let work = tempfile::tempdir()?;
            let remote = bare.path().to_string_lossy().into_owned();
            sh(work.path(), &format!("git init -q -b main && git config user.name Ada && git config user.email ada@example.com && git config commit.gpgsign false && git config tag.gpgsign false && echo a > a && git add a && git commit -qm init && git remote add origin {remote} && git push -q origin main"))?;
            let head = sh(work.path(), "git rev-parse HEAD")?;
            let cwd = work.path().to_string_lossy().into_owned();
            let spec = Spec { kind: "claude".into(), cwd: Some(cwd.clone()), name: Some("Fix a".into()), program: Some(work.path().join("no-claude").to_string_lossy().into_owned()), ..Spec::default() };
            let id = hub.harness.open(&hub, "local", spec, 1).await?.first().map(|i| i.id.clone()).ok_or_else(|| anyhow::anyhow!("no agent"))?;
            // Its harness can't start: once it has said so, nothing more comes from its machine.
            for _ in 0..250 {
                if hub.harness.info(&id).is_some_and(|i| i.status == crate::harness::model::Status::Exited) {
                    break;
                }
                crate::rt::sleep(Duration::from_millis(20)).await;
            }
            let cx = Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            let fail = |(c, m): crate::ops::Fail| anyhow::anyhow!("{c}: {m}");
            // What the agent changed, as its runner would report it. The runner's own reports go on
            // as its session settles (its harness gone), and one arriving after this replaces it:
            // set again, and asked again, until the change request takes it.
            let change = |input: Value| {
                let (cx, hub, id, cwd) = (cx.clone(), Arc::clone(&hub), id.clone(), cwd.clone());
                async move {
                    for _ in 0..50 {
                        anyhow::ensure!(hub.harness.set_changes(&id, json!([{"path": format!("{cwd}/a"), "added": 1, "removed": 1}])), "no agent to set the changes of");
                        match call(cx.clone(), "agents.change", input.clone()).await {
                            Err((_, m)) if m.contains("hasn't changed any files") => crate::rt::sleep(Duration::from_millis(100)).await,
                            r => return r.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}")),
                        }
                    }
                    anyhow::bail!("the agent's changes kept being replaced")
                }
            };
            // Nothing changed yet: nothing to open.
            let none = call(cx.clone(), "agents.change", json!({"agent": id})).await.err().map(|(_, m)| m).unwrap_or_default();
            assert!(none.contains("hasn't changed any files"), "{none}");
            // It changed a; the person has a file of their own beside it.
            sh(work.path(), "echo a2 > a && echo mine > b")?;
            let status = sh(work.path(), "git status --porcelain")?;
            let out = change(json!({"agent": id, "title": "Fix a", "fix_checks": false})).await?;
            let branch = get(&out, "/change_request/branch").as_str().unwrap_or_default().to_owned();
            assert!(branch.starts_with("agents/fix-a-"), "{out}");
            assert_eq!(get(&out, "/change_request/provider"), &json!("git"), "a remote kiln has no integration with");
            assert_eq!(get(&out, "/change_request/version"), &json!(1));
            assert_eq!(get(&out, "/change_request/files"), &json!(["a"]), "the person's b stays theirs");
            assert_eq!(get(&out, "/change_request/fix_checks"), &json!(false));
            assert_eq!(sh(bare.path(), &format!("git show {branch}:a"))?, "a2", "the remote has its work");
            assert!(sh(bare.path(), &format!("git ls-tree --name-only {branch}"))?.lines().all(|l| l != "b"));
            assert!(sh(bare.path(), &format!("git log -1 --format=%B {branch}"))?.contains(&format!("Kiln-Agent: Fix a (claude, {id})")));
            assert_eq!(sh(work.path(), "git rev-parse HEAD")?, head, "the folder's branch didn't move");
            assert_eq!(sh(work.path(), "git status --porcelain")?, status, "nor its index and working tree");
            // What it concluded says where its work went.
            let res = call(cx.clone(), "agents.result", json!({"agent": id})).await.map_err(fail)?;
            assert_eq!(get(&res, "/branch"), &json!(branch));
            let list = call(cx.clone(), "agents.changes", json!({})).await.map_err(fail)?;
            assert_eq!(get(&list, "/changes/0/agent"), &json!(id));
            // More work: a new version on the same branch.
            sh(work.path(), "echo a3 > a")?;
            let again = change(json!({"agent": id})).await?;
            assert_eq!(get(&again, "/change_request/version"), &json!(2));
            assert_eq!(get(&again, "/change_request/branch"), &json!(branch));
            assert_eq!(get(&again, "/change_request/fix_checks"), &json!(false), "as it was opened");
            assert_eq!(sh(bare.path(), &format!("git show {branch}:a"))?, "a3");
            Ok(())
        })
    }
}
