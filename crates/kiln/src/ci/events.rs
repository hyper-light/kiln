//! From what gitops sees to runs of the CI engine (docs/ci.md W03): a ref that moved (push, tag,
//! pull request) or a manual run becomes a GitHub-shaped event, with its changed files, commit
//! message and payload; each of the commit's workflows that runs (W02-R02) and matches it (W03-R03)
//! starts a run. A workflow with errors starts a run that fails at once, listing them, instead of
//! nothing happening (W02-AC09).

use std::sync::Arc;

use kiln_ci::trigger::context::{self, Commit, Person, PullRequest, Side};
use kiln_ci::trigger::{Event, Source, ZERO_SHA};

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
    let commit = load::load(hub, repo, &t.sha).await?;
    // A manual run of one pipeline runs that one only.
    let wanted = |l: &load::Loaded| t.pipeline.as_deref().is_none_or(|p| l.path == p || l.workflow.as_ref().and_then(|w| w.name.as_deref()) == Some(p));
    let running: Vec<&load::Loaded> = commit.workflows.iter().filter(|l| l.runs && l.dialect == kiln_ci::expr::Dialect::Gha && wanted(l)).collect();
    if running.is_empty() {
        return Ok(false);
    }
    let event = event(hub, repo, t).await;
    let (message, author) = gitops::commit_info(hub, repo, &t.sha).await;
    for l in running {
        match (&l.workflow, l.has_errors()) {
            (Some(w), false) => match kiln_ci::trigger::matches(w, &event) {
                Ok(()) => match super::actions::resolve(hub, w).await {
                    // Every action the run may reach, pinned now (W07-R02).
                    Ok(actions) => hub.ci.send(Msg::Start(Box::new(Start {
                        repo: repo.clone(),
                        event: event.clone(),
                        path: l.path.clone(),
                        workflow: Arc::new(w.clone()),
                        message: message.clone(),
                        author: author.clone(),
                        actions,
                    }))),
                    Err(e) => fail_run(hub, repo, &event, l, &message, &author, e).await?,
                },
                Err(why) => tracing::debug!(workflow = %l.path, %why, "workflow not run"),
            },
            _ => startup_failure(hub, repo, &event, l, &message, &author).await?,
        }
    }
    Ok(true)
}

/// The event a v1 trigger stands for.
async fn event(hub: &Hub, repo: &Repo, t: &Trigger) -> Event {
    let mirror = gitops::mirror_path(hub, &repo.name).to_string_lossy().to_string();
    let default_branch = gitops::run_git(hub, &["-C", &mirror, "symbolic-ref", "--short", "HEAD"], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).unwrap_or_else(|_| "main".into());
    let ctx_repo = context::Repo {
        full_name: repo.name.clone(),
        id: 0,
        owner_id: 0,
        default_branch: default_branch.clone(),
        private: false,
        server_url: if repo.clone_url.contains("github.com") { "https://github.com".into() } else { hub.cfg.server.public_url.trim_end_matches('/').to_owned() },
    };
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
        }
        "tag" => {
            e.name = "push".into();
            e.git_ref = format!("refs/tags/{}", t.branch);
            e.created = t.before.is_none();
            e.payload = context::push(&e, &ctx_repo, &[], &Person { name: author.clone(), email: String::new(), username: None }, false);
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
            let pr = PullRequest { number, state: "open".into(), head: Side { branch: format!("pr/{number}"), sha: t.sha.clone(), repo: ctx_repo.clone() }, base: Side { branch: default_branch.clone(), sha: base_sha, repo: ctx_repo.clone() }, ..PullRequest::default() };
            e.payload = context::pull_request(&e, &ctx_repo, &pr);
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
async fn startup_failure(hub: &Hub, repo: &Repo, e: &Event, l: &load::Loaded, message: &str, author: &str) -> anyhow::Result<()> {
    let lines: Vec<String> = l.diagnostics.iter().filter(|d| d.severity == kiln_ci::wf::Severity::Error).map(|d| format!("{}:{}:{}: {} {}", l.path, d.range.start.line, d.range.start.col, d.code, d.message)).collect();
    let error = if lines.is_empty() { format!("{} could not be read", l.path) } else { lines.join("\n") };
    fail_run(hub, repo, e, l, message, author, error).await
}

/// A run that fails before any job, saying why.
async fn fail_run(hub: &Hub, repo: &Repo, e: &Event, l: &load::Loaded, message: &str, author: &str, error: String) -> anyhow::Result<()> {
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
    Ok(())
}
