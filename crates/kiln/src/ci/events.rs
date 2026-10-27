//! From what gitops sees to runs of the CI engine (docs/ci.md W03): a ref that moved (push, tag,
//! pull request) or a manual run becomes a GitHub-shaped event, with its changed files, commit
//! message and payload; each of the commit's workflows that runs (W02-R02) and matches it (W03-R03)
//! starts a run. A workflow with errors starts a run that fails at once, listing them, instead of
//! nothing happening (W02-AC09).

use std::sync::Arc;

use kiln_ci::trigger::context::{self, Commit, Person, PullRequest, Side};
use kiln_ci::trigger::{Event, Source, ZERO_SHA};
use rusqlite::OptionalExtension as _;

use super::dispatch::{Msg, Start};
use super::load;
use crate::gitops::{self, LS_REMOTE_TIMEOUT, Trigger};
use crate::hub::Hub;
use crate::store::{Repo, Status};

/// What the engine did with a trigger: whether the commit has GitHub workflows that run, so v1's
/// auto-detected pipelines stand aside (W02-R02).
pub async fn on_trigger(hub: &Hub, repo: &Repo, t: &Trigger) -> anyhow::Result<bool> {
    if !hub.ci.enabled() {
        return Ok(false);
    }
    // A host's pull request closed: its preview goes (W15-R07), whichever CI runs its workflows.
    if t.event == "pull_request"
        && let Some(p) = t.payload.as_deref().filter(|p| p.get("action").and_then(serde_json::Value::as_str) == Some("closed"))
        && let Some(n) = p.pointer("/pull_request/number").and_then(serde_json::Value::as_u64)
    {
        let merged = p.pointer("/pull_request/merged").and_then(serde_json::Value::as_bool).unwrap_or(false);
        super::previews::pull_request_closed(hub, repo, n, merged).await;
    }
    // A repository whose CI runs elsewhere (W23-R07): KilnCI stands aside.
    if !super::providers::kiln_runs(hub, repo).await {
        return Ok(true);
    }
    let commit = load::load(hub, repo, &t.sha).await?;
    // A push to the trunk (its commit here now): the tests it took into quarantine (W16-R05,
    // W11-R07).
    if t.event == "push"
        && let Some(before) = t.before.as_deref().filter(|b| *b != ZERO_SHA)
    {
        super::notify::trunk_moved(hub, repo, &t.branch, before, &t.sha).await;
    }
    // A manual run of one pipeline runs that one only.
    let wanted = |l: &load::Loaded| t.pipeline.as_deref().is_none_or(|p| l.path == p || l.workflow.as_ref().and_then(|w| w.name.as_deref()) == Some(p));
    // GitHub's workflows; the kiln dialect's too once `[ci] kiln_workflows` takes them off v1.
    let engine_runs = |l: &load::Loaded| l.dialect == kiln_ci::expr::Dialect::Gha || hub.cfg.ci.kiln_workflows;
    let running: Vec<&load::Loaded> = commit.workflows.iter().filter(|l| l.runs && engine_runs(l) && wanted(l)).collect();
    if running.is_empty() {
        return Ok(false);
    }
    let event = event(hub, repo, t).await;
    let (message, author) = gitops::commit_info(hub, repo, &t.sha).await;
    for l in running {
        start(hub, repo, &event, l, &message, &author, serde_json::Value::Null).await?;
    }
    Ok(true)
}

/// [`on_trigger`] for the one workflow `t` names (its path or name), asked for by kiln itself
/// (`[updates] in_step`'s build of the server's commit): the run's id once it's made. None when
/// the commit has no such workflow the engine runs, or it doesn't take the event.
pub async fn start_one(hub: &Hub, repo: &Repo, t: &Trigger) -> anyhow::Result<Option<String>> {
    let Some(p) = t.pipeline.as_deref() else { anyhow::bail!("no workflow named") };
    if !hub.ci.enabled() || !super::providers::kiln_runs(hub, repo).await {
        return Ok(None);
    }
    let commit = load::load(hub, repo, &t.sha).await?;
    let engine_runs = |l: &load::Loaded| l.dialect == kiln_ci::expr::Dialect::Gha || hub.cfg.ci.kiln_workflows;
    let Some(l) = commit.workflows.iter().find(|l| l.runs && engine_runs(l) && (l.path == p || l.workflow.as_ref().and_then(|w| w.name.as_deref()) == Some(p))) else { return Ok(None) };
    let event = event(hub, repo, t).await;
    let (message, author) = gitops::commit_info(hub, repo, &t.sha).await;
    let (tx, rx) = async_channel::bounded(1);
    let given = super::dispatch::Given { reply: Some(tx), ..super::dispatch::Given::default() };
    if !start_given(hub, repo, &event, l, (&message, &author), serde_json::Value::Null, given).await? {
        return Ok(None);
    }
    Ok(crate::rt::timeout(std::time::Duration::from_secs(30), rx.recv()).await.ok().and_then(Result::ok))
}

/// Starts workflow `l` for `event` when it matches (W03-R03), with its remote actions pinned
/// now (W07-R02). A workflow with errors gets a run that fails at once, listing them (W02-AC09).
/// `inputs` are the run's typed inputs (`workflow_dispatch`), or null. Whether a run began.
pub async fn start(hub: &Hub, repo: &Repo, event: &Event, l: &load::Loaded, message: &str, author: &str, inputs: serde_json::Value) -> anyhow::Result<bool> {
    start_given(hub, repo, event, l, (message, author), inputs, super::dispatch::Given::default()).await
}

/// [`start`], with what a run of a working tree brings besides (W17-R06).
pub async fn start_given(hub: &Hub, repo: &Repo, event: &Event, l: &load::Loaded, (message, author): (&str, &str), inputs: serde_json::Value, given: super::dispatch::Given) -> anyhow::Result<bool> {
    let (Some(w), false) = (&l.workflow, l.has_errors()) else {
        let id = startup_failure(hub, repo, event, l, message, author).await?;
        if let Some(r) = &given.reply {
            let _ = r.try_send(id);
        }
        return Ok(true);
    };
    if let Err(why) = kiln_ci::trigger::matches(w, event) {
        tracing::debug!(workflow = %l.path, %why, "workflow not run");
        return Ok(false);
    }
    match super::actions::resolve(hub, repo, &event.sha, w).await {
        Ok(actions) => {
            // Every resolution is audited (W07-R04): who, which ref, which commit.
            if !actions.is_empty() {
                let list: Vec<String> = actions.iter().map(|(u, a)| format!("{u} → {}", a.error.as_ref().map_or_else(|| a.sha.clone(), |e| format!("unresolved ({e})")))).collect();
                hub.store.audit(event.actor.clone(), "ci.actions.resolve".into(), format!("{} {} at {}: {}", repo.name, l.path, event.sha, list.join(", "))).await;
            }
            hub.ci.send(Msg::Start(Box::new(Start {
                repo: repo.clone(),
                event: event.clone(),
                path: l.path.clone(),
                workflow: Arc::new(w.clone()),
                message: message.to_owned(),
                author: author.to_owned(),
                actions,
                inputs,
                given,
            })));
        }
        Err(e) => {
            let id = fail_run(hub, repo, event, l, message, author, e).await?;
            if let Some(r) = &given.reply {
                let _ = r.try_send(id);
            }
        }
    }
    Ok(true)
}

/// What events need to know about a repository: how payloads describe it, its default branch
/// and its mirror.
pub struct Facts {
    pub repo: context::Repo,
    pub default_branch: String,
    pub mirror: String,
}

pub async fn facts(hub: &Hub, repo: &Repo) -> Facts {
    let mirror = gitops::mirror_path(hub, &repo.name).to_string_lossy().to_string();
    let default_branch = gitops::run_git(hub, &["-C", &mirror, "symbolic-ref", "--short", "HEAD"], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).unwrap_or_else(|_| "main".into());
    let ctx_repo = context::Repo {
        full_name: repo.name.clone(),
        id: 0,
        owner_id: 0,
        default_branch: default_branch.clone(),
        private: false,
        server_url: if repo.clone_url.contains("github.com") { "https://github.com".into() } else { hub.cfg.server.public_url.trim_end_matches('/').to_owned() },
        created_at: repo.created_at,
    };
    Facts { repo: ctx_repo, default_branch, mirror }
}

/// The event a v1 trigger stands for. When a webhook delivered it, GitHub's payload is the
/// event's (W03-R07); the changed files are still kiln's own diff.
async fn event(hub: &Hub, repo: &Repo, t: &Trigger) -> Event {
    let made = synthesize(hub, repo, t).await;
    let Some(p) = &t.payload else { return made };
    let name = match t.event {
        "tag" => "push",
        "manual" => "workflow_dispatch",
        other => other,
    };
    let default_branch = facts(hub, repo).await.default_branch;
    match kiln_ci::trigger::github::event(name, p, &default_branch, &t.sha) {
        Some(mut e) => {
            e.changed = made.changed;
            e.started_by = made.started_by;
            if e.sha.is_empty() {
                e.sha = t.sha.clone();
            }
            if e.message.is_none() {
                e.message = made.message;
            }
            e
        }
        None => made,
    }
}

/// Who pushed `sha` to `git_ref` of a kiln-hosted repository, as its push handler kept it (W10-R02's
/// `secrets.use`): the branch's commit itself, or the tag's latest push (the poller sees an
/// annotated tag's commit, the push its tag). `None` for what nobody kiln holds to less pushed (a
/// host's repository, the owner). Unreadable, it may use nothing.
async fn pushed_by(hub: &Hub, repo: &str, git_ref: &str, sha: &str) -> Option<serde_json::Value> {
    let (r, g, s) = (repo.to_owned(), git_ref.to_owned(), sha.to_owned());
    let found = hub
        .store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT started_by FROM ci_pushes WHERE repo_id = ?1 AND ref = ?2 AND (sha = ?3 OR ref LIKE 'refs/tags/%') ORDER BY ts DESC LIMIT 1",
                rusqlite::params![r, g, s],
                |x| x.get::<_, String>(0),
            )
            .optional()?)
        })
        .await;
    match found {
        Ok(by) => by.and_then(|t| serde_json::from_str(&t).ok()),
        Err(e) => {
            tracing::warn!(repo, git_ref, error = %format!("{e:#}"), "who pushed couldn't be read: the run's legs get no secrets");
            Some(serde_json::json!({ "name": "unknown", "user": null, "rights": [], "limit": null }))
        }
    }
}

/// The event kiln synthesizes for what it saw itself (polling, the UI).
async fn synthesize(hub: &Hub, repo: &Repo, t: &Trigger) -> Event {
    let Facts { repo: ctx_repo, default_branch, mirror } = facts(hub, repo).await;
    let (message, author) = gitops::commit_info(hub, repo, &t.sha).await;
    let mut e = Event { sha: t.sha.clone(), before: t.before.clone(), actor: author.clone(), trusted: true, source: Source::Poll, message: Some(message), ..Event::default() };
    match t.event {
        "push" => {
            e.name = "push".into();
            e.git_ref = format!("refs/heads/{}", t.branch);
            e.created = t.before.is_none();
            e.changed = match &t.before {
                Some(b) => gitops::changed_files(hub, repo, b, &t.sha).await,
                // A new branch: what it adds over the default branch (W03-R05).
                None => gitops::changed_files(hub, repo, &default_branch, &t.sha).await,
            };
            let commits = commits(hub, &mirror, t.before.as_deref(), &t.sha).await;
            e.payload = context::push(&e, &ctx_repo, &commits, &Person { name: author.clone(), email: String::new(), username: None }, false);
            e.started_by = pushed_by(hub, &repo.id, &e.git_ref, &t.sha).await;
        }
        "tag" => {
            e.name = "push".into();
            e.git_ref = format!("refs/tags/{}", t.branch);
            e.created = t.before.is_none();
            e.payload = context::push(&e, &ctx_repo, &[], &Person { name: author.clone(), email: String::new(), username: None }, false);
            e.started_by = pushed_by(hub, &repo.id, &e.git_ref, &t.sha).await;
        }
        "pull_request" => {
            // Polling sees `refs/pull/N/head` only: no title, labels or author (the payload says
            // `kiln.partial`); the base is taken to be the default branch.
            let number: u64 = t.branch.trim_start_matches("pr/").parse().unwrap_or(0);
            e.name = "pull_request".into();
            e.action = if t.before.is_none() { "opened".into() } else { "synchronize".into() };
            e.git_ref = format!("refs/pull/{number}/merge");
            e.base_ref = Some(default_branch.clone());
            e.head_ref = Some(format!("pr/{number}"));
            e.changed = gitops::changed_files(hub, repo, &default_branch, &t.sha).await;
            let base_sha = gitops::run_git(hub, &["-C", &mirror, "rev-parse", &default_branch], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).unwrap_or_default();
            let counts = gitops::pr_counts(hub, repo, &default_branch, &t.sha).await;
            let pr = PullRequest {
                number,
                state: "open".into(),
                head: Side { branch: format!("pr/{number}"), sha: t.sha.clone(), repo: ctx_repo.clone() },
                base: Side { branch: default_branch.clone(), sha: base_sha, repo: ctx_repo.clone() },
                counts,
                ..PullRequest::default()
            };
            e.payload = context::pull_request(&e, &ctx_repo, &pr);
            // Polling can't see where the head lives: one on none of the repository's branches
            // came from a fork, and is untrusted (W03-R08, W18-R05). Its author counts as a
            // contributor once the default branch has a commit of theirs, as GitHub counts.
            e.trusted = gitops::run_git(hub, &["-C", &mirror, "for-each-ref", "--count=1", "--contains", &t.sha, "--format=%(refname)", "refs/heads/"], LS_REMOTE_TIMEOUT).await.is_ok_and(|o| !o.trim_ascii().is_empty());
            if !e.trusted {
                let email = gitops::run_git(hub, &["-C", &mirror, "log", "-1", "--format=%ae", &t.sha], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).unwrap_or_default();
                let merged = !email.is_empty()
                    && gitops::run_git(hub, &["-C", &mirror, "log", "-1", "--format=%H", "--fixed-strings", &format!("--author=<{email}>"), &format!("refs/heads/{default_branch}")], LS_REMOTE_TIMEOUT).await.is_ok_and(|o| !o.trim_ascii().is_empty());
                if let Some(p) = e.payload.get_mut("pull_request").and_then(serde_json::Value::as_object_mut) {
                    p.insert("author_association".into(), serde_json::json!(if merged { "CONTRIBUTOR" } else { "FIRST_TIME_CONTRIBUTOR" }));
                }
            }
        }
        "manual" => {
            e.name = "workflow_dispatch".into();
            e.git_ref = format!("refs/heads/{}", t.branch);
            e.payload = context::workflow_dispatch(&e, &ctx_repo, "", &serde_json::json!({}));
            e.source = Source::Api;
        }
        other => {
            e.name = other.to_owned();
            e.git_ref = format!("refs/heads/{}", t.branch);
        }
    }
    e
}

/// The commits a push brought, oldest first (at most GitHub's 20).
async fn commits(hub: &Hub, mirror: &str, before: Option<&str>, after: &str) -> Vec<Commit> {
    let range = match before {
        Some(b) if b != ZERO_SHA => format!("{b}..{after}"),
        _ => after.to_owned(),
    };
    let fmt = "--format=%H%x1f%T%x1f%aI%x1f%an%x1f%ae%x1f%cn%x1f%ce%x1f%B%x1e";
    let Ok(out) = gitops::run_git(hub, &["-C", mirror, "log", "-n", "20", "--reverse", fmt, &range], LS_REMOTE_TIMEOUT).await else { return Vec::new() };
    String::from_utf8_lossy(&out)
        .split('\x1e')
        .filter_map(|rec| {
            let f: Vec<&str> = rec.trim_start_matches('\n').splitn(8, '\x1f').collect();
            let get = |i: usize| f.get(i).copied().unwrap_or_default().to_owned();
            (!get(0).is_empty()).then(|| Commit {
                id: get(0),
                tree_id: get(1),
                timestamp: get(2),
                author: Person { name: get(3), email: get(4), username: None },
                committer: Person { name: get(5), email: get(6), username: None },
                message: get(7).trim_end().to_owned(),
            })
        })
        .collect()
}

/// A workflow that can't run: one failed run, its diagnostics as the error (W02-AC09).
async fn startup_failure(hub: &Hub, repo: &Repo, e: &Event, l: &load::Loaded, message: &str, author: &str) -> anyhow::Result<String> {
    let lines: Vec<String> = l.diagnostics.iter().filter(|d| d.severity == kiln_ci::wf::Severity::Error).map(|d| format!("{}:{}:{}: {} {}", l.path, d.range.start.line, d.range.start.col, d.code, d.message)).collect();
    let error = if lines.is_empty() { format!("{} could not be read", l.path) } else { lines.join("\n") };
    fail_run(hub, repo, e, l, message, author, error).await
}

/// A run that fails before any job, saying why. Its id.
async fn fail_run(hub: &Hub, repo: &Repo, e: &Event, l: &load::Loaded, message: &str, author: &str, error: String) -> anyhow::Result<String> {
    let id = crate::util::new_id();
    let n = super::runs::NewRun {
        id: id.clone(),
        workflow: l.path.clone(),
        name: l.workflow.as_ref().and_then(|w| w.name.clone()).unwrap_or_else(|| l.path.clone()),
        event: e.name.clone(),
        branch: e.ref_name().to_owned(),
        sha: e.sha.clone(),
        message: message.to_owned(),
        author: author.to_owned(),
        actor: e.actor.clone(),
        trigger: serde_json::json!({ "event": e.name, "ref": e.git_ref }),
        trusted: e.trusted,
        priority: 50,
        payload: e.payload.clone(),
        model: String::new(),
        input: String::new(),
    };
    super::runs::create(hub, repo, n).await?;
    super::runs::finish_run(hub, &id, Status::Failed, "startup_failure", Some(error)).await?;
    hub.emit_run(&id).await;
    Ok(id)
}

/// Why a dispatch was refused: GitHub's status and message.
#[derive(Debug)]
pub struct Refused {
    pub status: http::StatusCode,
    pub message: String,
}

fn refused(status: http::StatusCode, message: impl Into<String>) -> Refused {
    Refused { status, message: message.into() }
}

/// The full ref and commit `r` names: a branch, a tag, or a `refs/…` name.
pub(crate) async fn named_ref(hub: &Hub, repo: &Repo, r: &str) -> Result<(String, String), Refused> {
    let refs: std::collections::HashMap<String, String> = hub.store.refs(repo.id.clone()).await.map_err(|e| refused(http::StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?.into_iter().collect();
    for full in [r.to_owned(), format!("refs/heads/{r}"), format!("refs/tags/{r}")] {
        if let Some(sha) = refs.get(&full) {
            return Ok((full, sha.clone()));
        }
    }
    Err(refused(http::StatusCode::UNPROCESSABLE_ENTITY, format!("No ref found for: {r}")))
}

/// The ref and commit a plan preview (W04-R29) is for: `git_ref` (a branch, a tag, a `refs/…`
/// name; the default branch when empty), at `sha` when given.
pub async fn preview_ref(hub: &Hub, repo: &Repo, git_ref: &str, sha: Option<&str>) -> Result<(String, String), Refused> {
    let r = if git_ref.is_empty() { facts(hub, repo).await.default_branch } else { git_ref.to_owned() };
    match (named_ref(hub, repo, &r).await, sha) {
        (Ok((full, _)), Some(sha)) => Ok((full, sha.to_owned())),
        (Ok(found), None) => Ok(found),
        (Err(_), Some(sha)) => Ok((if r.starts_with("refs/") { r } else { format!("refs/heads/{r}") }, sha.to_owned())),
        (Err(e), None) => Err(e),
    }
}

/// The event a plan preview stands for: `name` at `full_ref` and `sha` as kiln would see it. A
/// push brings what its commit changed; a pull request targets the default branch. A `payload`
/// (a webhook's, say) is taken as GitHub's. `workflow_dispatch` inputs are checked against what
/// the workflow declares; the typed inputs come back with the event.
#[allow(clippy::too_many_arguments)]
pub async fn preview_event(hub: &Hub, repo: &Repo, name: &str, full_ref: &str, sha: &str, payload: Option<&serde_json::Value>, given: &serde_json::Map<String, serde_json::Value>, declared: Option<&[kiln_ci::wf::ir::Input]>, path: &str) -> Result<(Event, serde_json::Value), Refused> {
    let f = facts(hub, repo).await;
    if let Some(p) = payload {
        let mut e = kiln_ci::trigger::github::event(name, p, &f.default_branch, sha).ok_or_else(|| refused(http::StatusCode::UNPROCESSABLE_ENTITY, format!("The payload isn't a {name} event's")))?;
        if e.sha.is_empty() {
            e.sha = sha.to_owned();
        }
        return Ok((e, serde_json::Value::Null));
    }
    let short = full_ref.strip_prefix("refs/heads/").or_else(|| full_ref.strip_prefix("refs/tags/")).unwrap_or(full_ref).to_owned();
    match name {
        "push" => {
            let parent = gitops::run_git(hub, &["-C", &f.mirror, "rev-parse", "--verify", "--quiet", &format!("{sha}^")], LS_REMOTE_TIMEOUT).await.ok().map(|o| String::from_utf8_lossy(&o).trim().to_owned()).filter(|s| !s.is_empty());
            let event = if full_ref.starts_with("refs/tags/") { "tag" } else { "push" };
            Ok((synthesize(hub, repo, &Trigger { event, branch: short, sha: sha.to_owned(), before: parent, pipeline: None, payload: None }).await, serde_json::Value::Null))
        }
        "pull_request" | "pull_request_target" => {
            let mut e = synthesize(hub, repo, &Trigger { event: "pull_request", branch: "pr/0".into(), sha: sha.to_owned(), before: None, pipeline: None, payload: None }).await;
            e.name = name.to_owned();
            Ok((e, serde_json::Value::Null))
        }
        "workflow_dispatch" => {
            let declared = declared.ok_or_else(|| refused(http::StatusCode::UNPROCESSABLE_ENTITY, "Workflow does not have 'workflow_dispatch' trigger"))?;
            let (typed, strings) = kiln_ci::trigger::dispatch::inputs(declared, given).map_err(|m| refused(http::StatusCode::UNPROCESSABLE_ENTITY, m))?;
            let mut e = Event { name: name.to_owned(), repo: repo.name.clone(), git_ref: full_ref.to_owned(), sha: sha.to_owned(), actor: "kiln".into(), trusted: true, source: Source::Api, ..Event::default() };
            e.payload = context::workflow_dispatch(&e, &f.repo, path, &strings);
            Ok((e, typed))
        }
        other => {
            let mut e = Event { name: other.to_owned(), repo: repo.name.clone(), git_ref: full_ref.to_owned(), sha: sha.to_owned(), actor: "kiln".into(), trusted: true, source: Source::Api, ..Event::default() };
            if other == "schedule" {
                e.payload = context::schedule("", &f.repo);
            }
            Ok((e, serde_json::Value::Null))
        }
    }
}

/// `workflow_dispatch` (W03-R11): runs workflow `which` (its path, or its file name as GitHub's
/// API takes it) at `git_ref` with `inputs`, checked against what it declares. `started_by`: who
/// asked ([`crate::authz::Caller::started_by`]), whose `secrets.use` its legs are held to.
pub async fn workflow_dispatch(hub: &Hub, repo: &Repo, git_ref: &str, which: &str, given: &serde_json::Map<String, serde_json::Value>, actor: &str, started_by: Option<serde_json::Value>) -> Result<(), Refused> {
    dispatch(hub, repo, git_ref, which, given, actor, started_by, false).await.map(|_| ())
}

/// [`workflow_dispatch`] for kiln's own reasons (a release's check, docs/deploy-api.md §9.1): of
/// what's `offered`, the workflow is given the inputs it declares, and the run's id comes back.
pub async fn dispatch_run(hub: &Hub, repo: &Repo, git_ref: &str, which: &str, offered: &serde_json::Map<String, serde_json::Value>, actor: &str) -> Result<String, Refused> {
    dispatch(hub, repo, git_ref, which, offered, actor, None, true).await?.ok_or_else(|| refused(http::StatusCode::INTERNAL_SERVER_ERROR, format!("{which} didn't start")))
}

/// [`dispatch_run`] at commit `sha` (a release's, docs/cd-targets.md §2.2): the workflow as that
/// commit has it, run with the default branch as its ref; with `job`, that job of it and the jobs
/// it needs.
pub async fn dispatch_at(hub: &Hub, repo: &Repo, sha: &str, which: &str, job: Option<&str>, offered: &serde_json::Map<String, serde_json::Value>, actor: &str) -> Result<String, Refused> {
    let full_ref = format!("refs/heads/{}", facts(hub, repo).await.default_branch);
    dispatch_with(hub, repo, (full_ref, sha.to_owned()), (which, job), offered, actor, None, true).await?.ok_or_else(|| refused(http::StatusCode::INTERNAL_SERVER_ERROR, format!("{which} didn't start")))
}

/// A workflow kiln wrote itself (a CD provider's jobs on its runners, docs/cd-targets.md §2.2),
/// started in `repo` at commit `sha` as CD's own, at `path`: the run's id. One that doesn't read
/// is refused before anything runs.
pub async fn start_written(hub: &Hub, repo: &Repo, sha: &str, path: &str, yaml: &str, actor: &str) -> Result<String, Refused> {
    if !hub.ci.enabled() {
        return Err(refused(http::StatusCode::NOT_FOUND, "The CI engine is off ([ci] enabled)."));
    }
    let (p, text) = (path.to_owned(), yaml.to_owned());
    let (workflow, diags) = crate::rt::blocking(move || kiln_ci::wf::parse_workflow(&p, &text, kiln_ci::expr::Dialect::Kiln)).await.map_err(|e| refused(http::StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let l = load::Loaded { path: path.to_owned(), dialect: kiln_ci::expr::Dialect::Kiln, runs: true, workflow, diagnostics: load::stored(&diags) };
    if l.has_errors() {
        let first = l.diagnostics.iter().find(|d| d.severity == kiln_ci::wf::Severity::Error).map(|d| format!("{}: {}", d.path, d.message)).unwrap_or_else(|| "it isn't a workflow".into());
        return Err(refused(http::StatusCode::UNPROCESSABLE_ENTITY, format!("the workflow kiln wrote for {path} doesn't read: {first}")));
    }
    let f = facts(hub, repo).await;
    let full_ref = format!("refs/heads/{}", f.default_branch);
    let (message, _) = gitops::commit_info(hub, repo, sha).await;
    let mut e = Event { name: "workflow_dispatch".into(), repo: repo.name.clone(), git_ref: full_ref, sha: sha.to_owned(), actor: actor.to_owned(), trusted: true, source: Source::Cd, message: Some(message.clone()), ..Event::default() };
    e.payload = context::workflow_dispatch(&e, &f.repo, path, &serde_json::Value::Object(serde_json::Map::new()));
    let (tx, rx) = async_channel::bounded(1);
    let given = super::dispatch::Given { reply: Some(tx), ..super::dispatch::Given::default() };
    start_given(hub, repo, &e, &l, (&message, actor), serde_json::Value::Null, given).await.map_err(|err| refused(http::StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    crate::rt::timeout(std::time::Duration::from_secs(30), rx.recv()).await.ok().and_then(Result::ok).ok_or_else(|| refused(http::StatusCode::INTERNAL_SERVER_ERROR, format!("{path} didn't start")))
}

/// With `offered`, the inputs are what's offered, of which the workflow takes those it declares,
/// and the run's id comes back once it's made.
#[allow(clippy::too_many_arguments)]
async fn dispatch(
    hub: &Hub,
    repo: &Repo,
    git_ref: &str,
    which: &str,
    given: &serde_json::Map<String, serde_json::Value>,
    actor: &str,
    started_by: Option<serde_json::Value>,
    offered: bool,
) -> Result<Option<String>, Refused> {
    if !hub.ci.enabled() {
        return Err(refused(http::StatusCode::NOT_FOUND, "The CI engine is off ([ci] enabled)."));
    }
    let at = named_ref(hub, repo, git_ref).await?;
    dispatch_with(hub, repo, at, (which, None), given, actor, started_by, offered).await
}

/// [`dispatch`] at a ref and commit resolved already: workflow `which`, or one job of it and the
/// jobs that job needs.
#[allow(clippy::too_many_arguments)]
async fn dispatch_with(
    hub: &Hub,
    repo: &Repo,
    (full_ref, sha): (String, String),
    (which, job): (&str, Option<&str>),
    given: &serde_json::Map<String, serde_json::Value>,
    actor: &str,
    started_by: Option<serde_json::Value>,
    offered: bool,
) -> Result<Option<String>, Refused> {
    if !hub.ci.enabled() {
        return Err(refused(http::StatusCode::NOT_FOUND, "The CI engine is off ([ci] enabled)."));
    }
    let commit = load::load(hub, repo, &sha).await.map_err(|e| refused(http::StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let Some(l) = commit.workflows.iter().find(|l| l.path == which || l.path.rsplit('/').next() == Some(which)) else {
        return Err(refused(http::StatusCode::NOT_FOUND, format!("Workflow {which} not found at {full_ref}")));
    };
    let only;
    let l = match job {
        None => l,
        Some(job) => {
            let w = l.workflow.as_ref().and_then(|w| super::uploads::only_job(w, job)).ok_or_else(|| refused(http::StatusCode::NOT_FOUND, format!("{} has no job `{job}`", l.path)))?;
            only = load::Loaded { workflow: Some(w), ..l.clone() };
            &only
        }
    };
    let declared = l.workflow.as_ref().and_then(|w| w.on.iter().find(|t| t.event == "workflow_dispatch")).map(|t| t.inputs.clone());
    let Some(declared) = declared else {
        return Err(refused(http::StatusCode::UNPROCESSABLE_ENTITY, "Workflow does not have 'workflow_dispatch' trigger"));
    };
    let taken: serde_json::Map<String, serde_json::Value>;
    let given = if offered {
        taken = given.iter().filter(|(k, _)| declared.iter().any(|i| i.name.eq_ignore_ascii_case(k))).map(|(k, v)| (k.clone(), v.clone())).collect();
        &taken
    } else {
        given
    };
    let (typed, strings) = kiln_ci::trigger::dispatch::inputs(&declared, given).map_err(|m| refused(http::StatusCode::UNPROCESSABLE_ENTITY, m))?;
    let f = facts(hub, repo).await;
    let (message, _) = gitops::commit_info(hub, repo, &sha).await;
    let mut e = Event { name: "workflow_dispatch".into(), repo: repo.name.clone(), git_ref: full_ref, sha, actor: actor.to_owned(), trusted: true, source: Source::Api, message: Some(message.clone()), started_by, ..Event::default() };
    e.payload = context::workflow_dispatch(&e, &f.repo, &l.path, &strings);
    let (tx, rx) = async_channel::bounded(1);
    let reply = super::dispatch::Given { reply: offered.then_some(tx), ..super::dispatch::Given::default() };
    let started = start_given(hub, repo, &e, l, (&message, actor), typed, reply).await.map_err(|err| refused(http::StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    if !offered || !started {
        return Ok(None);
    }
    Ok(crate::rt::timeout(std::time::Duration::from_secs(30), rx.recv()).await.ok().and_then(Result::ok))
}

/// kiln's `agent` event (W03-R09, W20): agent `agent` asks for a run of workflow `which` at
/// `git_ref`, one that takes it (`on: { agent: { agents, inputs } }`, its `agents` naming it or
/// `*`), with `inputs` typed and checked as `workflow_dispatch`'s are. The agent is the run's actor.
pub async fn agent(hub: &Hub, repo: &Repo, git_ref: &str, which: &str, given: &serde_json::Map<String, serde_json::Value>, agent: &str, started_by: Option<serde_json::Value>) -> Result<(), Refused> {
    if !hub.ci.enabled() {
        return Err(refused(http::StatusCode::NOT_FOUND, "The CI engine is off ([ci] enabled)."));
    }
    let (full_ref, sha) = named_ref(hub, repo, git_ref).await?;
    let commit = load::load(hub, repo, &sha).await.map_err(|e| refused(http::StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let Some(l) = commit.workflows.iter().find(|l| l.path == which || l.path.rsplit('/').next() == Some(which)) else {
        return Err(refused(http::StatusCode::NOT_FOUND, format!("Workflow {which} not found at {git_ref}")));
    };
    let Some(declared) = l.workflow.as_ref().and_then(|w| w.on.iter().find(|t| t.event == "agent")).map(|t| t.inputs.clone()) else {
        return Err(refused(http::StatusCode::UNPROCESSABLE_ENTITY, format!("{} doesn't take kiln's `agent` event (`on: agent`)", l.path)));
    };
    let (typed, strings) = kiln_ci::trigger::dispatch::inputs(&declared, given).map_err(|m| refused(http::StatusCode::UNPROCESSABLE_ENTITY, m))?;
    let f = facts(hub, repo).await;
    let (message, _) = gitops::commit_info(hub, repo, &sha).await;
    let mut e = Event { name: "agent".into(), repo: repo.name.clone(), git_ref: full_ref, sha, actor: agent.to_owned(), trusted: true, source: Source::Agent, message: Some(message.clone()), started_by, ..Event::default() };
    e.payload = context::workflow_dispatch(&e, &f.repo, &l.path, &strings);
    if let Some(p) = e.payload.as_object_mut() {
        p.insert("sender".into(), serde_json::json!({ "login": agent, "type": "Bot" }));
        p.insert("kiln".into(), serde_json::json!({ "agent": agent }));
    }
    match start(hub, repo, &e, l, &message, agent, typed).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(refused(http::StatusCode::FORBIDDEN, format!("{} doesn't take a run asked for by {agent} (its `agents`)", l.path))),
        Err(err) => Err(refused(http::StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}"))),
    }
}

/// `repository_dispatch` (W03-R11): runs the default branch's workflows that listen for
/// `event_type`, with `client_payload` in the event; held to `started_by` as `workflow_dispatch` is.
pub async fn repository_dispatch(hub: &Hub, repo: &Repo, event_type: &str, client_payload: &serde_json::Value, actor: &str, started_by: Option<serde_json::Value>) -> Result<usize, Refused> {
    if !hub.ci.enabled() {
        return Err(refused(http::StatusCode::NOT_FOUND, "The CI engine is off ([ci] enabled)."));
    }
    if event_type.is_empty() || event_type.chars().count() > 100 {
        return Err(refused(http::StatusCode::UNPROCESSABLE_ENTITY, "event_type must be 1 to 100 characters"));
    }
    if client_payload.as_object().is_some_and(|o| o.len() > 10) {
        return Err(refused(http::StatusCode::UNPROCESSABLE_ENTITY, "client_payload can have at most 10 top-level properties"));
    }
    let f = facts(hub, repo).await;
    let (full_ref, sha) = named_ref(hub, repo, &f.default_branch).await?;
    let commit = load::load(hub, repo, &sha).await.map_err(|e| refused(http::StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let (message, _) = gitops::commit_info(hub, repo, &sha).await;
    let mut e = Event { name: "repository_dispatch".into(), action: event_type.to_owned(), repo: repo.name.clone(), git_ref: full_ref, sha, actor: actor.to_owned(), trusted: true, source: Source::Api, message: Some(message.clone()), started_by, ..Event::default() };
    e.payload = context::repository_dispatch(&e, &f.repo, client_payload);
    let mut started = 0;
    for l in commit.workflows.iter().filter(|l| l.runs && !l.has_errors()) {
        if start(hub, repo, &e, l, &message, actor, serde_json::Value::Null).await.map_err(|err| refused(http::StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))? {
            started += 1;
        }
    }
    Ok(started)
}

/// `registry_package` (W12-R07): a package (an image tag) was published or updated in kiln's
/// registry under the repository's name; the default branch's workflows that listen for it run,
/// with the package in `github.event.registry_package`.
pub async fn registry_package(hub: &Hub, repo: &Repo, action: &str, package: &serde_json::Value, actor: &str) -> Result<usize, Refused> {
    if !hub.ci.enabled() {
        return Err(refused(http::StatusCode::NOT_FOUND, "The CI engine is off ([ci] enabled)."));
    }
    let f = facts(hub, repo).await;
    let (full_ref, sha) = named_ref(hub, repo, &f.default_branch).await?;
    let commit = load::load(hub, repo, &sha).await.map_err(|e| refused(http::StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let listening: Vec<&load::Loaded> = commit.workflows.iter().filter(|l| l.runs && !l.has_errors() && l.workflow.as_ref().is_some_and(|w| w.on.iter().any(|t| t.event == "registry_package"))).collect();
    if listening.is_empty() {
        return Ok(0);
    }
    let (message, _) = gitops::commit_info(hub, repo, &sha).await;
    let mut e = Event { name: "registry_package".into(), action: action.to_owned(), repo: repo.name.clone(), git_ref: full_ref, sha, actor: actor.to_owned(), trusted: true, source: Source::Api, message: Some(message.clone()), ..Event::default() };
    e.payload = serde_json::json!({ "action": action, "registry_package": package, "repository": f.repo.payload(), "sender": { "login": actor, "type": "User" } });
    let mut started = 0;
    for l in listening {
        if start(hub, repo, &e, l, &message, actor, serde_json::Value::Null).await.map_err(|err| refused(http::StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))? {
            started += 1;
        }
    }
    Ok(started)
}

/// A run's chain of `workflow_run` triggers is at most this deep (W03-R13, GitHub-exact).
pub const MAX_WORKFLOW_RUN_DEPTH: u64 = 3;
/// How deep a kiln workflow may say it takes part in a chain (`kiln: { chain-depth }`).
pub const MAX_CHAIN_DEPTH: u64 = 10;

/// `workflow_run` (W03-R02, W03-R13): a run was requested or completed. The default branch's
/// workflows that listen for it run there, with the run in `github.event.workflow_run`; kiln's
/// `pipeline` too, once it completed. Chains stop at three levels, or as deep as a listening
/// workflow's `kiln: { chain-depth }` says (ten at most).
pub async fn workflow_run(hub: &Hub, repo: &Repo, action: &str, run: &context::RunSummary, depth: u64) {
    if depth >= MAX_CHAIN_DEPTH {
        tracing::debug!(repo = %repo.name, run = run.id, depth, "workflow_run chain at its limit");
        return;
    }
    let f = facts(hub, repo).await;
    let Ok((full_ref, sha)) = named_ref(hub, repo, &f.default_branch).await else { return };
    let commit = match load::load(hub, repo, &sha).await {
        Ok(c) => c,
        Err(e) => return tracing::warn!(repo = %repo.name, error = %format!("{e:#}"), "workflow_run: the default branch's workflows could not be read"),
    };
    let takes = |w: &kiln_ci::wf::ir::Workflow, event: &str| w.on.iter().any(|t| t.event == event) && depth < w.chain_depth.map_or(MAX_WORKFLOW_RUN_DEPTH, |d| u64::from(d).min(MAX_CHAIN_DEPTH));
    let completed = action == "completed";
    let listening: Vec<&load::Loaded> = commit.workflows.iter().filter(|l| l.runs && !l.has_errors() && l.workflow.as_ref().is_some_and(|w| takes(w, "workflow_run") || (completed && takes(w, "pipeline")))).collect();
    if listening.is_empty() {
        return;
    }
    let (message, _) = gitops::commit_info(hub, repo, &sha).await;
    let mut payload = context::workflow_run(action, run, &f.repo);
    if let Some(o) = payload.as_object_mut() {
        o.insert("kiln".into(), serde_json::json!({ "depth": depth + 1 }));
    }
    let e = Event {
        name: "workflow_run".into(),
        action: action.to_owned(),
        repo: repo.name.clone(),
        git_ref: full_ref,
        sha,
        actor: run.actor.clone(),
        trusted: true,
        source: Source::WorkflowRun,
        payload,
        message: Some(message.clone()),
        workflow: Some(run.name.clone()),
        workflow_path: Some(run.path.clone()),
        conclusion: run.conclusion.clone(),
        workflow_branch: Some(run.head_branch.clone()),
        ..Event::default()
    };
    // kiln's: another workflow's run finished (by its kiln name or path).
    let pipeline = Event { name: "pipeline".into(), ..e.clone() };
    for l in listening {
        let Some(w) = l.workflow.as_ref() else { continue };
        for ev in [takes(w, "workflow_run").then_some(&e), (completed && takes(w, "pipeline")).then_some(&pipeline)].into_iter().flatten() {
            if let Err(err) = start(hub, repo, ev, l, &message, &run.actor, serde_json::Value::Null).await {
                tracing::warn!(repo = %repo.name, workflow = %l.path, event = %ev.name, error = %format!("{err:#}"), "workflow_run");
            }
        }
    }
}
