//! W02's routes (docs/ci.md §4.1): a repository's workflows at a ref, one workflow with its
//! source, model and diagnostics, checking text that isn't committed (the editor's buffer, an
//! agent's draft), saving a workflow's new text, and the JSON Schemas the editor completes from.

use std::collections::HashMap;
use std::sync::Arc;

use http::StatusCode;
use hyper::body::Incoming;
use kiln_ci::expr::Dialect;
use kiln_ci::policy::Policy;
use kiln_ci::sched::routing::{self, LegRoute};
use kiln_ci::wf::diag::Range;
use kiln_ci::wf::ir::RunsOn;
use kiln_ci::wf::{self, Diag, SchemaKind, Severity};
use serde::Deserialize;
use serde_json::{Value, json};

use super::Fail;
use super::load::{self, Loaded};
use crate::authz::{Caller, Scope};
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;
use crate::store::Repo;

/// A workflow is at most 1 MiB (W02-R03); the rest of the body is JSON's.
const LIMIT: usize = 2 << 20;

fn bad(m: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, m.into())
}

fn missing(m: impl Into<String>) -> Fail {
    (StatusCode::NOT_FOUND, m.into())
}

fn internal(e: &anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

#[derive(Deserialize)]
struct Check {
    /// Where the file is or will be: decides what it is (W02-R01). Defaults to a kiln workflow.
    #[serde(default)]
    path: String,
    content: String,
    #[serde(default)]
    dialect: Option<Dialect>,
    /// The repository it is for (its id or name): the checks that need one too.
    #[serde(default)]
    repo: Option<String>,
}

/// `POST /api/ci/validate { path, content, dialect?, repo? }` → `{ kind, dialect, diagnostics,
/// outline? }`.
pub async fn validate(hub: &Hub, caller: Option<&Caller>, body: Incoming) -> Result<Response, Fail> {
    validate_value(hub, caller, srv::body_json(body, LIMIT).await.map_err(|e| bad(format!("{e:#}")))?).await
}

/// [`validate`], its body read (`ci.workflows.validate`, W20).
pub async fn validate_value(hub: &Hub, caller: Option<&Caller>, body: Value) -> Result<Response, Fail> {
    let c: Check = serde_json::from_value(body).map_err(|e| bad(format!("{e}")))?;
    let path = if c.path.is_empty() { ".kiln/workflow.yaml".to_owned() } else { c.path };
    let repo = match c.repo.as_deref().filter(|r| !r.is_empty()) {
        Some(key) => {
            let repo = find_repo(hub, key).await?;
            // The route is open to whoever reads runs somewhere; this repository must be one of theirs.
            if caller.is_some_and(|c| !c.can("runs.read", &Scope::Repo { repo: repo.name.clone() })) {
                return Err(missing(format!("no repository `{key}`")));
            }
            Some(repo)
        }
        None => None,
    };
    Ok(srv::json(StatusCode::OK, &checked(hub, repo.as_ref(), &path, c.content, c.dialect).await?))
}

/// The file checked as [`wf::check`] checks it, and a workflow kiln runs for `repo` against what
/// only the server knows (W02-R12): the routing rules, runners and pools its `runs-on` may name
/// (`KCI6007`), and the action policies its `uses:` answer to (W07-R04), as its run would.
async fn checked(hub: &Hub, repo: Option<&Repo>, path: &str, content: String, dialect: Option<Dialect>) -> Result<wf::Checked, Fail> {
    let p = path.to_owned();
    let (mut checked, model) = crate::rt::blocking(move || wf::check_model(&p, &content, dialect)).await.map_err(|e| internal(&e))?;
    if let (Some(repo), Some(w)) = (repo, &model)
        && runs_here(hub, repo, path).await
    {
        let mut more = unknown_labels(hub, repo, w).await;
        more.extend(refused(&super::actions::policies(hub, repo).await, w));
        // A use the policy refuses for its pin needs no hint to pin it.
        checked.diagnostics.retain(|d| !(d.code == "KCI6002" && more.iter().any(|m| m.code == "KCI4021" && m.range == d.range)));
        checked.diagnostics.extend(more);
        checked.diagnostics.sort_by_key(|x| (x.range.start.line, x.range.start.col, x.code));
    }
    Ok(checked)
}

/// Whether kiln runs the workflow at `path` of `repo`: KilnCI is a CI provider of it (W23-R01),
/// and a GitHub workflow is one kiln runs at its default branch (W02-R02).
async fn runs_here(hub: &Hub, repo: &Repo, path: &str) -> bool {
    if !super::providers::kiln_runs(hub, repo).await {
        return false;
    }
    if wf::file_kind(path) != Some(wf::FileKind::GhaWorkflow) {
        return true;
    }
    match load::resolve(hub, repo, "HEAD").await {
        Ok(Some(sha)) => load::load(hub, repo, &sha).await.is_ok_and(|c| c.github_run),
        _ => true,
    }
}

/// `KCI6007` (W02-R08): each `runs-on` label that no routing rule maps, no runner carries (its
/// own labels, its name alone, and those GitHub gives every self-hosted runner) and no pool gives
/// its runners. Labels written as expressions are the run's to know.
async fn unknown_labels(hub: &Hub, repo: &Repo, w: &wf::Workflow) -> Vec<Diag> {
    let rules = super::routing::sources(hub).await.effective();
    let runners = hub.store.agents().await.unwrap_or_default();
    let pools = super::pools::specs(hub).await;
    let carried = |label: &str, alone: bool| {
        runners.iter().any(|a| (alone && a.name.eq_ignore_ascii_case(label)) || carries(a).iter().any(|x| x.eq_ignore_ascii_case(label)))
            || pools.iter().any(|p| p.labels.iter().any(|x| x.eq_ignore_ascii_case(label)))
    };
    // A rule may match on the event: a label is mapped if it is under any the workflow takes.
    let events: Vec<String> = if w.on.is_empty() { vec![String::new()] } else { w.on.iter().map(|t| t.event.clone()).collect() };
    let mut out = Vec::new();
    for j in &w.jobs {
        let labels = match &j.runs_on {
            Some(RunsOn::Labels(l) | RunsOn::Group { labels: l, .. }) => l,
            None => continue,
        };
        let named: Vec<&wf::ir::Tpl> = labels.iter().filter(|t| !t.has_expr() && !t.src.trim().is_empty()).collect();
        let asked: Vec<String> = named.iter().map(|t| t.src.trim().to_owned()).collect();
        let mut left: Option<Vec<String>> = None;
        for event in &events {
            let leg = LegRoute {
                job: j.id.clone(),
                job_name: j.name.as_ref().map_or_else(|| j.id.clone(), |n| n.src.clone()),
                workflow: w.path.clone(),
                repo: repo.name.clone(),
                event: event.clone(),
                labels: asked.clone(),
                ..LegRoute::default()
            };
            // A rule that can't be read routes nothing: the leg's own explanation says why.
            let Ok(req) = routing::resolve(&leg, &rules) else { continue };
            left = Some(left.map_or(req.labels.clone(), |l| l.into_iter().filter(|x| req.labels.contains(x)).collect()));
        }
        let Some(left) = left else { continue };
        for t in named {
            let label = t.src.trim();
            if left.iter().any(|l| l == label) && !carried(label, asked.len() == 1) {
                out.push(
                    Diag::warning("KCI6007", format!("`{label}` is no runner's label, no routing rule maps it and no pool has it: a leg that asks for it waits for one"), t.range)
                        .at(format!("jobs.{}.runs-on", j.id)),
                );
            }
        }
    }
    out
}

/// A runner's labels, and those GitHub gives every self-hosted runner, from its facts.
fn carries(a: &crate::store::AgentRow) -> Vec<String> {
    let facts = kiln_ci::facts::RunnerFacts { os: a.os.clone(), arch: a.arch.clone(), ..kiln_ci::facts::RunnerFacts::default() };
    let mut out = a.labels.clone();
    out.extend(routing::implicit_labels(&facts).into_iter().filter(|x| !a.labels.iter().any(|l| l.eq_ignore_ascii_case(x))));
    out
}

/// `GET /api/ci/repos/:repo/labels`: what a `runs-on` of the repository's may name (W21-R06's
/// select of labels and rules), each label with where it comes from (`runner`, `pool`, `rule`),
/// as [`unknown_labels`] knows them.
pub async fn labels(hub: &Hub, repo: &str) -> Result<Response, Fail> {
    let repo = find_repo(hub, repo).await?;
    let mut found: Vec<(String, Vec<&'static str>)> = Vec::new();
    let mut add = |label: &str, from: &'static str| match found.iter_mut().find(|(l, _)| l.eq_ignore_ascii_case(label)) {
        Some((_, f)) if !f.contains(&from) => f.push(from),
        Some(_) => {}
        None => found.push((label.to_owned(), vec![from])),
    };
    for a in hub.store.agents().await.unwrap_or_default() {
        carries(&a).iter().for_each(|l| add(l, "runner"));
    }
    for p in super::pools::specs(hub).await {
        p.labels.iter().for_each(|l| add(l, "pool"));
    }
    // The rules that may apply to this repository's legs.
    for r in super::routing::sources(hub).await.effective() {
        let here = r.matches.repos.is_empty() || r.matches.repos.iter().any(|g| kiln_ci::trigger::pattern::Pattern::new(g).is_ok_and(|p| p.matches(&repo.name)));
        if here {
            r.matches.labels.iter().for_each(|l| add(l, "rule"));
        }
    }
    found.sort_by_key(|(l, _)| l.to_ascii_lowercase());
    let labels: Vec<Value> = found.into_iter().map(|(label, from)| json!({ "label": label, "from": from })).collect();
    Ok(srv::json(StatusCode::OK, &json!({ "labels": labels })))
}

/// What the action policies (W07-R04) refuse of a workflow's `uses:`, where each is: what its run
/// would refuse at the start.
fn refused(policies: &[Policy], w: &wf::Workflow) -> Vec<Diag> {
    fn steps(list: &[wf::Step], path: &str, check: &mut impl FnMut(&str, Range, String)) {
        for (i, s) in list.iter().enumerate() {
            let sp = format!("{path}[{i}]");
            if let Some(u) = &s.uses {
                check(u, s.range, format!("{sp}.uses"));
            }
            steps(&s.parallel, &format!("{sp}.parallel"), check);
        }
    }
    let policy: Vec<&Policy> = policies.iter().collect();
    let mut out = Vec::new();
    let mut check = |u: &str, range: Range, at: String| {
        if let Err(r) = kiln_ci::policy::check(&policy, u) {
            out.push(Diag::error(r.code, r.message, range).at(at));
        }
    };
    for j in &w.jobs {
        if let Some(u) = &j.uses {
            check(u, j.range, format!("jobs.{}.uses", j.id));
        }
        steps(&j.steps, &format!("jobs.{}.steps", j.id), &mut check);
    }
    out
}

#[derive(Deserialize)]
struct EditAsk {
    content: String,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

/// `POST /api/ci/repos/:repo/workflows/:path/edit { content, to?, branch?, title?, draft?,
/// reviewers?, topic? }` (W02-R12, W21-R06, `ci.workflows.edit`): a workflow's new text, checked
/// as [`validate`] checks it with its repository (refused with its errors), then saved as `to`
/// says, by default the first of these the repository takes:
///
/// - `diff`: a diff on the trunk of a repository kiln hosts, landing through review (W14);
/// - `change`: a new branch off the default branch and a pull or merge request of it, on GitHub
///   (through the App), GitLab or Bitbucket;
/// - `branch`: a commit to `branch`, on its head (a new branch on the default branch's), refused
///   for what a push of it would be;
/// - `patch`: nothing written: the patch that makes it, against the default branch.
///
/// Each answer has the diagnostics, and the plan a push of it to the default branch makes (W20-R03).
pub async fn edit(hub: &Arc<Hub>, caller: &Caller, repo_key: &str, path: &str, body: &Value) -> Result<Value, Fail> {
    let ask: EditAsk = serde_json::from_value(body.clone()).map_err(|e| bad(format!("{e}")))?;
    let file = path.trim_start_matches("./");
    if !matches!(wf::file_kind(file), Some(wf::FileKind::GhaWorkflow | wf::FileKind::KilnWorkflow)) || file.split('/').any(|s| s.is_empty() || s == "." || s == "..") {
        return Err(bad(format!("`{path}` is not where workflows are (.github/workflows/*.yml, .kiln/*.yaml)")));
    }
    let repo = find_repo(hub, repo_key).await?;
    let checked = checked(hub, Some(&repo), file, ask.content.clone(), None).await?;
    let errors: Vec<String> = checked.diagnostics.iter().filter(|d| d.is_error()).map(|d| d.render(file)).collect();
    if !errors.is_empty() {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, format!("{file} has errors, so it isn't saved:\n{}", errors.join("\n"))));
    }
    let host = crate::githost::for_repo(hub, &repo).await;
    let kiln = matches!(host, Some(crate::githost::GitHost::Kiln));
    let to = match ask.to.as_deref() {
        Some(t @ ("diff" | "change" | "branch" | "patch")) => t,
        Some(other) => return Err(bad(format!("`to` is diff, change, branch or patch, not `{other}`"))),
        None if kiln => "diff",
        None if host.is_some() => "change",
        None => "patch",
    };
    let title = ask.title.as_deref().map(str::trim).filter(|t| !t.is_empty()).map_or_else(|| format!("Edit {file}"), str::to_owned);
    let branch = ask.branch.as_deref().map(str::trim).filter(|b| !b.is_empty());
    let mut out = match (to, &host) {
        ("patch", _) => json!({ "patch": patch(hub, &repo, file, &ask.content).await? }),
        ("diff", Some(crate::githost::GitHost::Kiln)) => {
            let content = ask.content.clone();
            super::diffs::propose_file(hub, &repo.name, caller, file, &title, body, move |_| Ok(content)).await?
        }
        ("branch", Some(crate::githost::GitHost::Kiln)) => {
            let branch = branch.ok_or_else(|| bad("`branch` names the branch to commit to"))?;
            let (sha, said) = super::hosting::commit_branch(hub, &repo, caller, branch, file, ask.content.clone().into_bytes(), &title).await?;
            json!({ "branch": branch, "sha": sha, "said": said })
        }
        ("change", Some(crate::githost::GitHost::Kiln)) => return Err((StatusCode::CONFLICT, format!("{} is kiln's own: its changes are diffs (or commits to a branch)", repo.name))),
        ("diff", _) => return Err((StatusCode::CONFLICT, format!("diffs are for repositories kiln hosts: {} takes a change on its host, a commit to a branch, or a patch", repo.name))),
        (_, None) => return Err((StatusCode::CONFLICT, format!("kiln can't write to {}: no App or credentials of its host are set up here, so it takes a patch", repo.name))),
        (_, Some(h)) => on_host(hub, caller, &repo, h, file, &ask.content, to == "change", branch, &title).await?,
    };
    // W20-R03: its effect too, as a push to the default branch plans it.
    let preview = super::ops::answer(super::preview::of(hub, Some(caller), json!({ "repo": repo.name, "path": file, "content": ask.content, "event": "push" })).await).await.unwrap_or(Value::Null);
    if let Some(o) = out.as_object_mut() {
        o.insert("to".into(), json!(to));
        o.insert("diagnostics".into(), serde_json::to_value(&checked.diagnostics).unwrap_or(Value::Null));
        o.insert("preview".into(), preview);
    }
    Ok(out)
}

/// The new text committed on a repository's own host (GitHub through the App, GitLab, Bitbucket):
/// to `branch` on its head, or (`change`) to a new branch off the default branch's head, then
/// proposed into it as a pull or merge request.
#[allow(clippy::too_many_arguments)]
async fn on_host(hub: &Arc<Hub>, caller: &Caller, repo: &Repo, host: &crate::githost::GitHost, file: &str, content: &str, change: bool, branch: Option<&str>, title: &str) -> Result<Value, Fail> {
    if !caller.can("diffs.submit", &Scope::Repo { repo: repo.name.clone() }) {
        return Err((StatusCode::FORBIDDEN, format!("saving to {}'s host needs `diffs.submit`", repo.name)));
    }
    let default = super::events::facts(hub, repo).await.default_branch;
    let tip = load::resolve(hub, repo, &default).await.map_err(|e| internal(&e))?.ok_or_else(|| (StatusCode::UNPROCESSABLE_ENTITY, format!("{default} has no commits yet")))?;
    let stem = file.rsplit('/').next().unwrap_or(file).split('.').next().unwrap_or("workflow").to_owned();
    let branch = match (change, branch) {
        (_, Some(b)) => b.to_owned(),
        (true, None) => format!("kiln/edit-{stem}-{}", crate::util::new_id().chars().take(8).collect::<String>()),
        (false, None) => return Err(bad("`branch` names the branch to commit to")),
    };
    if !load::ref_ok(&branch) || branch.starts_with("refs/") || branch == "HEAD" {
        return Err(bad(format!("`{branch}` isn't a branch name")));
    }
    // A change's branch is new; a commit to a branch is made on its head, when it has one.
    let head = load::resolve(hub, repo, &format!("refs/heads/{branch}")).await.ok().flatten();
    if change && head.is_some() {
        return Err((StatusCode::CONFLICT, format!("{branch} is there already: a change's branch is a new one")));
    }
    let base = head.clone().unwrap_or_else(|| tip.clone());
    let (me, _) = super::diffs::author_of(&caller.name, Some(caller.kind));
    let me = me.strip_prefix("user:").unwrap_or(&me).to_owned();
    let author = crate::githost::Author { email: format!("{}@kiln", me.replace([' ', ':'], "-")), name: me };
    let files = [crate::githost::Change { path: file.to_owned(), content: Some(content.as_bytes().to_vec()) }];
    let made = host.commit_on(hub, repo, &branch, Some(&base), head.as_deref(), &files, &format!("{title}\n"), &author).await.map_err(|e| match e {
        crate::githost::CommitError::Moved => (StatusCode::CONFLICT, format!("{branch} moved while this was saved: reload it and save again")),
        crate::githost::CommitError::Other(e) => (StatusCode::BAD_GATEWAY, format!("{}'s host refused the commit: {e:#}", repo.name)),
    })?;
    let sha = match made {
        crate::githost::Committed::New(sha) => sha,
        crate::githost::Committed::Unchanged(_) => return Err((StatusCode::CONFLICT, format!("{file} is already so on {}: nothing to save", if change { &default } else { &branch }))),
    };
    hub.store.audit(caller.name.clone(), "ci.workflow.edit".into(), format!("{} {file} → {branch} {}", repo.name, sha.get(..12).unwrap_or(&sha))).await;
    if !change {
        return Ok(json!({ "branch": branch, "sha": sha }));
    }
    let body = format!("{title}, saved in kiln's workflow editor by {}.", caller.name);
    let opened =
        host.open_change(hub, repo, &branch, &default, title, &body).await.map_err(|e| (StatusCode::BAD_GATEWAY, format!("the branch {branch} is saved, but its change wasn't opened: {e:#}")))?;
    Ok(json!({ "branch": branch, "sha": sha, "change": { "number": opened.number, "url": opened.url } }))
}

/// The patch that makes `file` say `content`, against the default branch's head (in kiln's copy
/// of the repository: nothing is written to it but the objects the comparison needs).
async fn patch(hub: &Hub, repo: &Repo, file: &str, content: &str) -> Result<String, Fail> {
    let default = super::events::facts(hub, repo).await.default_branch;
    let tip = load::resolve(hub, repo, &default).await.map_err(|e| internal(&e))?.ok_or_else(|| (StatusCode::UNPROCESSABLE_ENTITY, format!("{default} has no commits yet")))?;
    let dir = crate::gitops::ensure_commit(hub, repo, &tip).await.map_err(|e| internal(&e))?;
    let d = dir.to_string_lossy().into_owned();
    let git = |args: Vec<&str>| {
        let mut a = vec!["-C", d.as_str()];
        a.extend(args);
        let a: Vec<String> = a.into_iter().map(str::to_owned).collect();
        async move {
            let a: Vec<&str> = a.iter().map(String::as_str).collect();
            crate::gitops::run_git(hub, &a, crate::gitops::LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).into_owned())
        }
    };
    let mode = git(vec!["ls-tree", &tip, "--", file]).await.ok().and_then(|l| l.split_whitespace().next().map(str::to_owned)).unwrap_or_else(|| "100644".into());
    let tree = super::diffs::tree_with(hub, &d, &tip, super::diffs::Edit::Put { file, mode: &mode, content: content.as_bytes().to_vec() }).await.map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let patch = git(vec!["diff", "--no-color", "--no-ext-diff", "--no-renames", &tip, &tree, "--", file]).await.map_err(|e| internal(&e))?;
    if patch.is_empty() {
        return Err((StatusCode::CONFLICT, format!("{file} is already so on {default}: there is no patch")));
    }
    Ok(patch)
}

/// `GET /api/ci/schema/:kind`: `gha`, `kiln`, `overlay` or `action` (W02-R09).
pub fn schema(kind: &str) -> Result<Response, Fail> {
    let k = SchemaKind::parse(kind).ok_or_else(|| missing(format!("no schema `{kind}`: gha, kiln, overlay or action")))?;
    let body = serde_json::to_vec(&wf::schema(k)).map_err(|e| internal(&e.into()))?;
    Ok(srv::response(StatusCode::OK, "application/schema+json", srv::full(body)))
}

async fn find_repo(hub: &Hub, id: &str) -> Result<Repo, Fail> {
    let repos = hub.store.repos().await.map_err(|e| internal(&e))?;
    repos.into_iter().find(|r| r.id == id || r.name == id).ok_or_else(|| missing(format!("no repository `{id}`")))
}

/// The repository, the ref asked for (`HEAD` by default) and the commit it names.
async fn at_ref(hub: &Hub, repo: &str, q: &HashMap<String, String>) -> Result<(Repo, String, String), Fail> {
    let repo = find_repo(hub, repo).await?;
    let r = q.get("ref").filter(|r| !r.is_empty()).cloned().unwrap_or_else(|| "HEAD".into());
    if !load::ref_ok(&r) {
        return Err(bad(format!("`{r}` is not a branch, tag or commit")));
    }
    let sha = load::resolve(hub, &repo, &r).await.map_err(|e| internal(&e))?.ok_or_else(|| missing(format!("{} has no branch, tag or commit `{r}`", repo.name)))?;
    Ok((repo, r, sha))
}

fn counts(l: &Loaded) -> (usize, usize) {
    let errors = l.diagnostics.iter().filter(|d| d.severity == Severity::Error).count();
    (errors + usize::from(l.workflow.is_none() && errors == 0), l.diagnostics.len() - errors)
}

/// `GET /api/ci/repos/:repo/workflows?ref=`: every workflow at the ref, whether each runs and
/// why GitHub's do or don't (W02-R02).
pub async fn list(hub: &Hub, repo: &str, q: &HashMap<String, String>) -> Result<Response, Fail> {
    let (repo, r, sha) = at_ref(hub, repo, q).await?;
    let commit = load::load(hub, &repo, &sha).await.map_err(|e| internal(&e))?;
    let workflows: Vec<serde_json::Value> = commit
        .workflows
        .iter()
        .map(|l| {
            let (errors, others) = counts(l);
            json!({
                "path": l.path,
                "dialect": l.dialect,
                "name": l.workflow.as_ref().and_then(|w| w.name.clone()).unwrap_or_else(|| l.path.clone()),
                "runs": l.runs,
                "errors": errors,
                "warnings": others,
                "outline": l.workflow.as_ref().map(wf::outline),
            })
        })
        .collect();
    Ok(srv::json(StatusCode::OK, &json!({ "ref": r, "sha": sha, "github": { "run": commit.github_run, "why": commit.github_note }, "workflows": workflows })))
}

/// `GET /api/ci/repos/:repo/workflows/:path?ref=`: one workflow's source, model and diagnostics.
pub async fn get(hub: &Hub, repo: &str, path: &str, q: &HashMap<String, String>) -> Result<Response, Fail> {
    if !matches!(wf::file_kind(path), Some(wf::FileKind::GhaWorkflow | wf::FileKind::KilnWorkflow)) {
        return Err(missing(format!("`{path}` is not where workflows are (.github/workflows/*.yml, .kiln/*.yaml)")));
    }
    let (repo, r, sha) = at_ref(hub, repo, q).await?;
    let commit = load::load(hub, &repo, &sha).await.map_err(|e| internal(&e))?;
    let l = commit.workflows.into_iter().find(|l| l.path == path).ok_or_else(|| missing(format!("no workflow `{path}` at {r} ({sha})")))?;
    let source = load::blob(hub, &repo, &sha, &l.path).await.map_err(|e| internal(&e))?;
    let (host, saves) = saves(hub, &repo).await;
    Ok(srv::json(
        StatusCode::OK,
        &json!({
            "path": l.path,
            "ref": r,
            "sha": sha,
            "dialect": l.dialect,
            "runs": l.runs,
            "github": { "run": commit.github_run, "why": commit.github_note },
            "source": source,
            "workflow": l.workflow,
            "diagnostics": l.diagnostics,
            "outline": l.workflow.as_ref().map(wf::outline),
            "host": host,
            "saves": saves,
        }),
    ))
}

/// Where `repo` is kept (`kiln`, `github`, `gitlab`, `bitbucket`; none: nowhere kiln writes), and
/// what [`edit`] may save a workflow of it as (its `to`), the first its default.
async fn saves(hub: &Hub, repo: &Repo) -> (Option<&'static str>, &'static [&'static str]) {
    match crate::githost::for_repo(hub, repo).await {
        Some(crate::githost::GitHost::Kiln) => (Some("kiln"), &["diff", "branch", "patch"]),
        Some(crate::githost::GitHost::Github(_)) => (Some("github"), &["change", "branch", "patch"]),
        Some(crate::githost::GitHost::Gitlab(_)) => (Some("gitlab"), &["change", "branch", "patch"]),
        Some(crate::githost::GitHost::Bitbucket(_)) => (Some("bitbucket"), &["change", "branch", "patch"]),
        None => (None, &["patch"]),
    }
}

#[derive(Deserialize)]
struct DispatchAsk {
    #[serde(rename = "ref")]
    git_ref: String,
    #[serde(default)]
    inputs: serde_json::Map<String, serde_json::Value>,
}

/// `POST /api/ci/repos/:repo/workflows/<path>/dispatches { ref, inputs }` (W03-R11): GitHub's
/// `workflow_dispatch`. 204 when the run starts.
pub async fn dispatch(hub: &Hub, repo: &str, path: &str, body: Incoming, started_by: Option<serde_json::Value>) -> Result<Response, Fail> {
    let v: serde_json::Value = srv::body_json(body, 1 << 20).await.map_err(|e| bad(format!("{e:#}")))?;
    dispatch_value(hub, repo, path, v, started_by).await
}

/// [`dispatch`], its body read already.
pub async fn dispatch_value(hub: &Hub, repo: &str, path: &str, body: serde_json::Value, started_by: Option<serde_json::Value>) -> Result<Response, Fail> {
    let repo = find_repo(hub, repo).await?;
    let ask: DispatchAsk = serde_json::from_value(body).map_err(|e| bad(format!("{e}")))?;
    // A repository whose CI is another provider's (W23-R01): that provider starts it.
    let inputs = serde_json::Value::Object(ask.inputs.clone());
    if let Some(started) = super::providers::dispatch(hub, &repo, path, &ask.git_ref, &inputs).await {
        started?;
        return Ok(srv::status(StatusCode::NO_CONTENT));
    }
    super::events::workflow_dispatch(hub, &repo, &ask.git_ref, path, &ask.inputs, "kiln", started_by).await.map_err(|r| (r.status, r.message))?;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

#[derive(Deserialize)]
struct RepoDispatchAsk {
    event_type: String,
    #[serde(default)]
    client_payload: serde_json::Value,
}

/// `POST /api/ci/repos/:repo/dispatches { event_type, client_payload }` (W03-R11): GitHub's
/// `repository_dispatch`. 204, as GitHub answers, whether or not a workflow listens.
pub async fn repository_dispatch(hub: &Hub, repo: &str, body: Incoming, started_by: Option<serde_json::Value>) -> Result<Response, Fail> {
    let repo = find_repo(hub, repo).await?;
    let ask: RepoDispatchAsk = srv::body_json(body, 1 << 20).await.map_err(|e| bad(format!("{e:#}")))?;
    super::events::repository_dispatch(hub, &repo, &ask.event_type, &ask.client_payload, "kiln", started_by).await.map_err(|r| (r.status, r.message))?;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

#[derive(Deserialize)]
struct NamedDispatchAsk {
    workflow: String,
    #[serde(rename = "ref")]
    git_ref: String,
    #[serde(default)]
    inputs: serde_json::Map<String, serde_json::Value>,
}

/// `POST /api/ci/repos/:repo/dispatch { workflow, ref, inputs }` (§4.1's form of the same).
/// `POST /api/ci/repos/:repo/agent` `{ workflow, ref, inputs }`: kiln's `agent` event (W03-R09),
/// `agent` (the caller) asking for the run.
pub async fn agent_run(hub: &Hub, repo: &str, agent: &str, body: Incoming, started_by: Option<serde_json::Value>) -> Result<Response, Fail> {
    #[derive(serde::Deserialize)]
    struct AgentAsk {
        workflow: String,
        #[serde(default, rename = "ref")]
        git_ref: String,
        #[serde(default)]
        inputs: serde_json::Map<String, serde_json::Value>,
    }
    let repo = find_repo(hub, repo).await?;
    let ask: AgentAsk = srv::body_json(body, 1 << 20).await.map_err(|e| bad(format!("{e:#}")))?;
    let git_ref = if ask.git_ref.is_empty() { super::events::facts(hub, &repo).await.default_branch } else { ask.git_ref.clone() };
    super::events::agent(hub, &repo, &git_ref, &ask.workflow, &ask.inputs, agent, started_by).await.map_err(|r| (r.status, r.message))?;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

pub async fn dispatch_named(hub: &Hub, repo: &str, body: Incoming, started_by: Option<serde_json::Value>) -> Result<Response, Fail> {
    let repo = find_repo(hub, repo).await?;
    let ask: NamedDispatchAsk = srv::body_json(body, 1 << 20).await.map_err(|e| bad(format!("{e:#}")))?;
    super::events::workflow_dispatch(hub, &repo, &ask.git_ref, &ask.workflow, &ask.inputs, "kiln", started_by).await.map_err(|r| (r.status, r.message))?;
    Ok(srv::status(StatusCode::NO_CONTENT))
}
