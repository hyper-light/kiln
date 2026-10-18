//! Runs of a working tree (docs/ci.md W17-R06). `kiln workflow run` makes a commit of the working
//! tree. Against a server it sends that commit's objects as a thin pack over what the server has,
//! then starts the commit's workflows for the event it names, with what it asks for besides: one
//! job and the jobs it needs, matrix combinations, inputs, and secrets for those runs only. Without
//! `--server` it does the same against a server of its own (`super::local`).
//!
//! - `GET /api/ci/repos/:repo/heads`: the commits at the mirror's branches and tags, which the
//!   pack leaves out.
//! - `POST /api/ci/repos/:repo/packs?sha=`: the pack. The commit is kept under
//!   `refs/kiln/uploads/<sha>` for a week, so legs can fetch it.
//! - `POST /api/ci/repos/:repo/local-runs`: the runs. The server's owner gets trusted runs, with the
//!   repository's secrets; anyone else's are untrusted, as a fork's pull request is: a read-only
//!   token, and only the secrets sent with them.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use http::StatusCode;
use hyper::body::Incoming;
use serde::Deserialize;
use serde_json::{Value, json};

use super::Fail;
use super::dispatch::Given;
use super::load;
use crate::authz::Caller;
use crate::gitops::{self, LS_REMOTE_TIMEOUT};
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;
use crate::proc::Duplex;
use crate::store::Repo;

/// The largest pack taken.
const MAX_PACK: usize = 2 << 30;
/// How long an uploaded commit is kept.
const KEEP_UPLOADS: i64 = 7 * 24 * 3600;

fn internal(e: &anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn bad(m: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, m.into())
}

fn is_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

async fn find_repo(hub: &Hub, id: &str) -> Result<Repo, Fail> {
    let repos = hub.store.repos().await.map_err(|e| internal(&e))?;
    repos.into_iter().find(|r| r.id == id || r.name == id).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no repository `{id}` on this server")))
}

/// The repository's mirror, which has to be there already: the server mirrors a repository when
/// it first polls it.
fn mirror(hub: &Hub, repo: &Repo) -> Result<String, Fail> {
    let path = gitops::mirror_path(hub, &repo.name);
    if !path.join("HEAD").exists() {
        return Err((StatusCode::CONFLICT, format!("the server hasn't mirrored {} yet; try again in a minute", repo.name)));
    }
    Ok(path.to_string_lossy().into_owned())
}

/// `GET /api/ci/repos/:repo/heads`: `{ heads: [sha] }`.
pub async fn heads(hub: &Hub, repo: &str) -> Result<Response, Fail> {
    let repo = find_repo(hub, repo).await?;
    let m = mirror(hub, &repo)?;
    let out = gitops::run_git(hub, &["-C", &m, "for-each-ref", "--format=%(objectname)", "refs/heads", "refs/tags"], LS_REMOTE_TIMEOUT).await.map_err(|e| internal(&e))?;
    let heads: BTreeSet<String> = String::from_utf8_lossy(&out).lines().map(str::trim).filter(|s| is_sha(s)).map(str::to_owned).collect();
    Ok(srv::json(StatusCode::OK, &json!({ "heads": heads })))
}

/// `POST /api/ci/repos/:repo/packs?sha=`: a pack holding commit `sha` (thin: its bases may be
/// commits the server has), kept as `refs/kiln/uploads/<sha>`. 409 when the server lacks a base.
pub async fn pack(hub: &Hub, repo: &str, q: &HashMap<String, String>, mut body: Incoming, by: &str) -> Result<Response, Fail> {
    let repo = find_repo(hub, repo).await?;
    let sha = q.get("sha").filter(|s| is_sha(s)).cloned().ok_or_else(|| bad("say which commit the pack holds: ?sha= and its 40 hex digits"))?;
    let m = mirror(hub, &repo)?;
    let mut cmd = gitops::git(hub);
    cmd.args(["-C", &m, "index-pack", "--stdin", "--fix-thin"]);
    let child = crate::proc::spawn_duplex(cmd).map_err(|e| internal(&e))?;
    let (out, err) = (child.stdout.clone(), child.stderr.clone());
    let drain = |rx: async_channel::Receiver<Vec<u8>>| crate::rt::spawn(async move {
        let mut all = Vec::new();
        while let Ok(c) = rx.recv().await {
            if all.len() < 64 << 10 {
                all.extend(c);
            }
        }
        all
    });
    let (out, err) = (drain(out), drain(err));
    let mut size = 0usize;
    let mut failed = None;
    loop {
        match srv::next_chunk(&mut body).await {
            Ok(Some(c)) => {
                size += c.len();
                if size > MAX_PACK {
                    failed = Some((StatusCode::PAYLOAD_TOO_LARGE, format!("a pack is at most {} GiB", MAX_PACK >> 30)));
                    break;
                }
                if child.stdin.send(c.to_vec()).await.is_err() {
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => {
                failed = Some(bad(format!("{e:#}")));
                break;
            }
        }
    }
    if let Some(f) = failed {
        child.terminate(std::time::Duration::from_secs(1));
        return Err(f);
    }
    let Duplex { stdin, exit, .. } = child;
    drop(stdin);
    let code = exit.await.ok().flatten();
    let _ = out.await;
    let err = err.await.unwrap_or_default();
    if code != Some(0) {
        let why = String::from_utf8_lossy(&err).trim().to_owned();
        // A thin pack over a commit the server doesn't have: the client sends it whole instead.
        return Err((StatusCode::CONFLICT, format!("the pack doesn't apply here: {}", crate::util::truncate(&why, 600))));
    }
    let name = format!("refs/kiln/uploads/{sha}");
    gitops::run_git(hub, &["-C", &m, "update-ref", &name, &sha], LS_REMOTE_TIMEOUT).await.map_err(|e| (StatusCode::CONFLICT, format!("the pack doesn't hold commit {sha}: {e:#}")))?;
    hub.store.audit(by.to_owned(), "ci.uploads.pack".into(), format!("{} {sha} ({size} bytes)", repo.name)).await;
    forget_old_uploads(hub, &m).await;
    Ok(srv::json(StatusCode::CREATED, &json!({ "sha": sha, "bytes": size })))
}

/// Uploaded commits older than a week go (their objects with the mirror's next `gc`).
async fn forget_old_uploads(hub: &Hub, mirror: &str) {
    let Ok(out) = gitops::run_git(hub, &["-C", mirror, "for-each-ref", "--format=%(refname) %(committerdate:unix)", "refs/kiln/uploads"], LS_REMOTE_TIMEOUT).await else { return };
    let cutoff = crate::util::now_unix() - KEEP_UPLOADS;
    for line in String::from_utf8_lossy(&out).lines() {
        let Some((name, at)) = line.rsplit_once(' ') else { continue };
        if at.parse::<i64>().is_ok_and(|t| t < cutoff) {
            let _ = gitops::run_git(hub, &["-C", mirror, "update-ref", "-d", name], LS_REMOTE_TIMEOUT).await;
        }
    }
}

/// What `POST /api/ci/repos/:repo/local-runs` asks for.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Ask {
    /// The commit: uploaded, or one the server has.
    pub sha: String,
    /// The branch (or tag, or `refs/…` name) it stands for; the default branch when empty.
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// The event: `push` unless said.
    pub event: String,
    /// GitHub's payload of that event, for `github.event`; else kiln makes one.
    pub payload: Option<Value>,
    /// Workflows by path, file name or name; every one the event starts when empty.
    pub workflows: Vec<String>,
    /// Only this job, and the jobs it needs.
    pub job: Option<String>,
    /// Only these matrix combinations.
    pub matrix: BTreeMap<String, String>,
    /// `workflow_dispatch` inputs.
    pub inputs: serde_json::Map<String, Value>,
    /// Secrets for these runs only.
    pub secrets: BTreeMap<String, String>,
    /// Variables over the server's.
    pub vars: BTreeMap<String, String>,
}

/// `job` and the jobs it needs, transitively; `None` when the workflow has no such job.
fn only_job(w: &kiln_ci::wf::Workflow, job: &str) -> Option<kiln_ci::wf::Workflow> {
    if !w.jobs.iter().any(|j| j.id == job) {
        return None;
    }
    let mut keep: BTreeSet<String> = BTreeSet::new();
    let mut todo = vec![job.to_owned()];
    while let Some(j) = todo.pop() {
        if !keep.insert(j.clone()) {
            continue;
        }
        if let Some(def) = w.jobs.iter().find(|x| x.id == j) {
            todo.extend(def.needs.iter().map(|n| n.job().to_owned()));
        }
    }
    let mut w = w.clone();
    w.jobs.retain(|j| keep.contains(&j.id));
    Some(w)
}

/// Whether `l` is the workflow `asked` names: its path, its file name, or its `name`.
fn named(l: &load::Loaded, asked: &str) -> bool {
    let asked = asked.trim_start_matches("./");
    l.path == asked || l.path.rsplit('/').next() == Some(asked) || l.workflow.as_ref().and_then(|w| w.name.as_deref()) == Some(asked)
}

/// `POST /api/ci/repos/:repo/local-runs`: `{ runs: [{ id, workflow }], skipped: [{ workflow, why }] }`.
pub async fn local_runs(hub: &Hub, repo: &str, caller: Option<&Caller>, body: Incoming) -> Result<Response, Fail> {
    if !hub.ci.enabled() {
        return Err((StatusCode::NOT_FOUND, "the CI engine is off ([ci] enabled)".into()));
    }
    let repo = find_repo(hub, repo).await?;
    let ask: Ask = srv::body_json(body, 4 << 20).await.map_err(|e| bad(format!("{e:#}")))?;
    if !is_sha(&ask.sha) {
        return Err(bad("`sha` is the commit's 40 hex digits"));
    }
    let m = mirror(hub, &repo)?;
    if gitops::run_git(hub, &["-C", &m, "cat-file", "-e", &format!("{}^{{commit}}", ask.sha)], LS_REMOTE_TIMEOUT).await.is_err() {
        return Err((StatusCode::CONFLICT, format!("the server doesn't have commit {}: send it first (POST …/packs?sha=)", ask.sha)));
    }
    let commit = load::load(hub, &repo, &ask.sha).await.map_err(|e| internal(&e))?;
    let event_name = if ask.event.is_empty() { "push".to_owned() } else { ask.event.clone() };
    let full_ref = match ask.git_ref.as_str() {
        // A detached work tree stands for the default branch.
        "" => format!("refs/heads/{}", super::events::facts(hub, &repo).await.default_branch),
        r if r.starts_with("refs/") => r.to_owned(),
        r => format!("refs/heads/{r}"),
    };
    // The owner's runs are trusted, as the owner's pushes are; anyone else's are a fork's.
    let (trusted, actor) = match caller {
        None => (true, "admin".to_owned()),
        Some(c) => (c.is_owner(), c.name.clone()),
    };
    let chosen: Vec<&load::Loaded> = if ask.workflows.is_empty() { commit.workflows.iter().filter(|l| l.runs).collect() } else { commit.workflows.iter().filter(|l| ask.workflows.iter().any(|a| named(l, a))).collect() };
    if let Some(a) = ask.workflows.iter().find(|a| !commit.workflows.iter().any(|l| named(l, a))) {
        return Err((StatusCode::NOT_FOUND, format!("no workflow `{a}` at {}", &ask.sha[..12])));
    }
    let (secrets, vars) = (Arc::new(ask.secrets.clone()), Arc::new(ask.vars.clone()));
    let only_matrix: Vec<(String, String)> = ask.matrix.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let (message, author) = gitops::commit_info(hub, &repo, &ask.sha).await;
    let mut runs = Vec::new();
    let mut skipped = Vec::new();
    for l in chosen {
        let declared = l.workflow.as_ref().and_then(|w| w.on.iter().find(|t| t.event == "workflow_dispatch")).map(|t| t.inputs.clone());
        let (mut event, typed) = match super::events::preview_event(hub, &repo, &event_name, &full_ref, &ask.sha, ask.payload.as_ref(), &ask.inputs, declared.as_deref(), &l.path).await {
            Ok(x) => x,
            Err(r) => {
                skipped.push(json!({ "workflow": l.path, "why": r.message }));
                continue;
            }
        };
        event.trusted = trusted;
        event.actor = actor.clone();
        event.source = kiln_ci::trigger::Source::Api;
        let mut loaded = l.clone();
        if let (Some(job), Some(w)) = (&ask.job, &l.workflow) {
            match only_job(w, job) {
                Some(w) => loaded.workflow = Some(w),
                None => {
                    skipped.push(json!({ "workflow": l.path, "why": format!("it has no job `{job}`") }));
                    continue;
                }
            }
        }
        if let Some(w) = &loaded.workflow
            && !loaded.has_errors()
            && let Err(why) = kiln_ci::trigger::matches(w, &event)
        {
            skipped.push(json!({ "workflow": l.path, "why": format!("it doesn't run on this {event_name}: {why}") }));
            continue;
        }
        let (tx, rx) = async_channel::bounded(1);
        let given = Given { only_matrix: only_matrix.clone(), secrets: Arc::clone(&secrets), vars: Arc::clone(&vars), reply: Some(tx) };
        super::events::start_given(hub, &repo, &event, &loaded, (&message, &author), typed, given).await.map_err(|e| internal(&e))?;
        match crate::rt::timeout(std::time::Duration::from_secs(60), rx.recv()).await {
            Ok(Ok(id)) => runs.push(json!({ "id": id, "workflow": l.path })),
            _ => skipped.push(json!({ "workflow": l.path, "why": "the run didn't start" })),
        }
    }
    let what = format!("{} {} at {}: {} run(s){}", repo.name, event_name, ask.sha, runs.len(), if trusted { "" } else { ", untrusted" });
    hub.store.audit(actor, "ci.runs.local".into(), what).await;
    Ok(srv::json(StatusCode::CREATED, &json!({ "runs": runs, "skipped": skipped, "trusted": trusted })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w17_r06_one_job_and_its_needs() {
        let src = "on: push\njobs:\n  a:\n    runs-on: x\n    steps: [{run: a}]\n  b:\n    needs: a\n    runs-on: x\n    steps: [{run: b}]\n  c:\n    needs: [b]\n    runs-on: x\n    steps: [{run: c}]\n  d:\n    runs-on: x\n    steps: [{run: d}]\n";
        let (w, _) = kiln_ci::wf::parse_workflow("ci.yml", src, kiln_ci::expr::Dialect::Gha);
        let one = w.as_ref().and_then(|w| only_job(w, "c"));
        let ids: Vec<String> = one.map(|w| w.jobs.iter().map(|j| j.id.clone()).collect()).unwrap_or_default();
        assert_eq!(ids, ["a", "b", "c"]);
        assert!(w.as_ref().and_then(|w| only_job(w, "zzz")).is_none());
    }
}
