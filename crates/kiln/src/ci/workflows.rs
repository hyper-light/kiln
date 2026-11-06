//! W02's routes (docs/ci.md §4.1): a repository's workflows at a ref, one workflow with its
//! source, model and diagnostics, checking text that isn't committed (the editor's buffer, an
//! agent's draft), saving a workflow's new text, and the JSON Schemas the editor completes from.

use std::collections::HashMap;
use std::sync::Arc;

use http::StatusCode;
use kiln_http::Incoming;
use kiln_ci::expr::Dialect;
use kiln_ci::policy::Policy;
use kiln_ci::sched::routing::{self, LegRoute};
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
    /// W29-R25: the commit it's edited at (a draft's head), whose locks it's checked against;
    /// the repository's default branch when not given.
    #[serde(default)]
    sha: Option<String>,
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
    Ok(srv::json(StatusCode::OK, &checked(hub, repo.as_ref(), &path, c.content, c.dialect, c.sha.as_deref()).await?))
}

/// The file checked as [`wf::check`] checks it, and a workflow kiln runs for `repo` against what
/// only the server knows (W02-R12): the routing rules, runners and pools its `runs-on` may name
/// (`KCI6007`), and the action policies its `uses:` answer to (W07-R04), as its run would; an
/// overlay, against the workflow it overlays.
async fn checked(hub: &Hub, repo: Option<&Repo>, path: &str, content: String, dialect: Option<Dialect>, sha: Option<&str>) -> Result<wf::Checked, Fail> {
    let p = path.to_owned();
    let overlay_text = content.clone();
    // W29-R27: a code workflow is what its program makes, for a push to the trunk at the commit.
    let mut made = None;
    let (mut checked, model) = if wf::file_kind(path) == Some(wf::FileKind::CodeWorkflow) {
        let (event, tree) = program_facts(hub, repo, sha).await;
        let (w, diagnostics, m) = crate::rt::blocking(move || super::code::checked(&p, &content, &event, tree)).await.map_err(|e| internal(&e))?;
        made = m;
        (wf::Checked { kind: "workflow", dialect: Dialect::Kiln, diagnostics, outline: w.as_ref().map(wf::outline) }, w)
    } else {
        crate::rt::blocking(move || wf::check_model(&p, &content, dialect)).await.map_err(|e| internal(&e))?
    };
    if let (Some(repo), Some(w)) = (repo, &model)
        && runs_here(hub, repo, path).await
    {
        // W29-R25/R26: what its locks hold at the commit it's edited at, read without fetching: a
        // locked use is judged by its locked commit, and the trunk's policy says what isn't locked.
        let (locked, onboarded) = super::locks::editor_facts(hub, repo, sha, path, w).await;
        let mut more = unknown_labels(hub, repo, w).await;
        more.extend(refused(&super::actions::policies(hub, repo).await, w, &locked));
        // A use the policy refuses for its pin needs no hint to pin it.
        checked.diagnostics.retain(|d| !(d.code == "KCI6002" && more.iter().any(|m| m.code == "KCI4021" && m.range == d.range)));
        checked.diagnostics.extend(more);
        let policy = super::repo_config::lock_policy(hub, repo).await.unwrap_or_default();
        kiln_ci::lock::editor::feedback(&mut checked.diagnostics, w, path, &kiln_ci::lock::editor::Facts { locked: &locked, onboarded, policy });
        // What the server says of a program's workflow, at the SDK call that made each part.
        if let Some(m) = &made {
            checked.diagnostics = checked.diagnostics.drain(..).map(|d| super::code::place(d, m)).collect();
        }
        checked.diagnostics.sort_by_key(|x| (x.range.start.line, x.range.start.col, x.code));
    }
    // W29-R29, R30: GitHub runs a GitHub workflow too, and its runner can't run a microVM action.
    if let (Some(repo), Some(w), Some(wf::FileKind::GhaWorkflow)) = (repo, &model, wf::file_kind(path)) {
        let more = microvm_uses(hub, repo, sha, w).await;
        if !more.is_empty() {
            checked.diagnostics.extend(more);
            checked.diagnostics.sort_by_key(|x| (x.range.start.line, x.range.start.col, x.code));
        }
    }
    // An overlay against the workflow it overlays, at the repository's default branch: the keys it
    // sets that the workflow hasn't (`KCI4010`), as its run would find them.
    if let (Some(repo), Some(wf::FileKind::Overlay { stem })) = (repo, wf::file_kind(path))
        && let Ok(Some(sha)) = load::resolve(hub, repo, "HEAD").await
        && let Ok(commit) = load::load(hub, repo, &sha).await
        && let Some(l) = commit.workflows.iter().find(|l| [format!(".github/workflows/{stem}.yml"), format!(".github/workflows/{stem}.yaml")].contains(&l.path))
        && let Ok(source) = load::blob(hub, repo, &sha, &l.path).await
    {
        let (text, lpath) = (overlay_text, l.path.clone());
        let more = crate::rt::blocking(move || {
            let (ov, _) = wf::parse_overlay(&text);
            // The workflow as written, not as an overlay already made it.
            let (w, _) = wf::parse_workflow(&lpath, &source, Dialect::Gha);
            match (w, ov) {
                (Some(mut w), Some(ov)) => wf::apply_overlay(&mut w, &ov),
                _ => Vec::new(),
            }
        })
        .await
        .map_err(|e| internal(&e))?;
        checked.diagnostics.extend(more);
        checked.diagnostics.sort_by_key(|x| (x.range.start.line, x.range.start.col, x.code));
    }
    Ok(checked)
}

/// `KCI5003` (W29-R29, R30): each step of a GitHub workflow that uses one of this repository's
/// microVM actions (`./…`, read at the commit edited), a warning at the step: where GitHub runs
/// the workflow, its runner can't run them.
async fn microvm_uses(hub: &Hub, repo: &Repo, sha: Option<&str>, w: &wf::Workflow) -> Vec<Diag> {
    let at = match sha {
        Some(s) => Some(s.to_owned()),
        None => load::resolve(hub, repo, "HEAD").await.ok().flatten(),
    };
    let Some(at) = at else { return Vec::new() };
    let mut kinds: HashMap<String, Option<String>> = HashMap::new();
    let mut out = Vec::new();
    for step in w.jobs.iter().flat_map(|j| j.steps.iter()) {
        let Some(u) = step.uses.as_deref().filter(|u| u.starts_with("./")) else { continue };
        let dir = u.trim_start_matches("./").trim_end_matches('/').to_owned();
        let using = match kinds.get(&dir) {
            Some(k) => k.clone(),
            None => {
                let mut found = None;
                for name in ["action.yml", "action.yaml"] {
                    if let Ok(text) = load::blob(hub, repo, &at, &format!("{dir}/{name}")).await {
                        found = wf::parse_action(&text).0.map(|a| a.runs.using);
                        break;
                    }
                }
                kinds.insert(dir, found.clone());
                found
            }
        };
        let kind = match using.as_deref() {
            Some("firecracker") => "a Firecracker",
            Some("shards") => "a Shards",
            _ => continue,
        };
        out.push(Diag::warning("KCI5003", format!("`{u}` is {kind} action, kiln's: GitHub's runner can't run it, so this step fails where GitHub runs the workflow"), step.range));
    }
    out
}

/// What a code workflow is made with where it's checked (W29-R27): a push of its commit to the
/// repository's trunk, and that commit's tree (`sha`, else the default branch's head); with no
/// repository, a push to `main` and no files.
async fn program_facts(hub: &Hub, repo: Option<&Repo>, sha: Option<&str>) -> (Value, Arc<dyn super::code::Tree>) {
    if let Some(repo) = repo {
        let at = match sha {
            Some(s) => Some(s.to_owned()),
            None => load::resolve(hub, repo, "HEAD").await.ok().flatten(),
        };
        if let Some(at) = at
            && let Ok(tree) = super::code::tree_at(hub, repo, &at).await
        {
            return (load::push_of(repo, &at), tree);
        }
    }
    (json!({ "ref": "refs/heads/main" }), Arc::new(super::code::NoTree))
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
    unknown_in(w, &repo.name, &rules, &runners, &pools)
}

/// [`unknown_labels`] against these rules, runners and pools.
pub(crate) fn unknown_in(w: &wf::Workflow, repo: &str, rules: &[routing::Rule], runners: &[crate::store::AgentRow], pools: &[kiln_ci::sched::pools::PoolSpec]) -> Vec<Diag> {
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
                repo: repo.to_owned(),
                event: event.clone(),
                labels: asked.clone(),
                ..LegRoute::default()
            };
            // A rule that can't be read routes nothing: the leg's own explanation says why.
            let Ok(req) = routing::resolve(&leg, rules) else { continue };
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
fn refused(policies: &[Policy], w: &wf::Workflow, locked: &std::collections::BTreeMap<String, String>) -> Vec<Diag> {
    let policy: Vec<&Policy> = policies.iter().collect();
    let mut out = Vec::new();
    for (u, range, at) in kiln_ci::lock::editor::written(w) {
        // A locked use runs its locked commit, and that's what the policy judges (W29-R23).
        let judged = match (locked.get(&u), u.split_once('@')) {
            (Some(sha), Some((name, _))) => kiln_ci::policy::check_pinned(&policy, &format!("{name}@{sha}")),
            _ => kiln_ci::policy::check(&policy, &u),
        };
        if let Err(r) = judged {
            out.push(Diag::error(r.code, r.message, range).at(at));
        }
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
    /// Saved as a Pipelines draft (docs/ci.md W29-R11): a draft diff, or a host's draft change on
    /// a branch of its own.
    #[serde(default)]
    draft: Option<bool>,
    /// Saved with it in the draft: a GitHub workflow's overlay, where Flow writes kiln's own
    /// triggers (W29-R06).
    #[serde(default)]
    files: Vec<FileAsk>,
}

#[derive(Deserialize)]
struct FileAsk {
    path: String,
    text: String,
}

/// `POST /api/ci/repos/:repo/workflows/:path/edit { content, to?, branch?, title?, draft?,
/// reviewers?, topic? }` (W02-R12, W21-R06, `ci.workflows.edit`): a workflow's (or an action's, W21-R07) new text, checked
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
/// With `draft` (Pipelines' Save draft, docs/ci.md W29-R11), it's a draft: a draft diff, or on a
/// host a draft change from a branch of its own (`kiln/draft/<stem>-<id8>`) whose commits say
/// `[skip ci]`. A later save of the caller's draft of the file adds to it: a new version of the
/// diff, or a new commit on the branch. Its answer says the draft as kiln records it (`draft`).
/// A GitHub workflow's draft may carry its overlay (`files: [{ path, text }]`, W29-R06), and a code
/// workflow's the GitHub workflow of its name that it makes (W29-R27), each checked as it is.
///
/// Each answer has the diagnostics, and the plan a push of it to the default branch makes (W20-R03).
pub async fn edit(hub: &Arc<Hub>, caller: &Caller, repo_key: &str, path: &str, body: &Value) -> Result<Value, Fail> {
    let ask: EditAsk = serde_json::from_value(body.clone()).map_err(|e| bad(format!("{e}")))?;
    let file = path.trim_start_matches("./");
    if !matches!(wf::file_kind(file), Some(wf::FileKind::GhaWorkflow | wf::FileKind::KilnWorkflow | wf::FileKind::CodeWorkflow | wf::FileKind::Action)) || file.split('/').any(|s| s.is_empty() || s == "." || s == "..") {
        return Err(bad(format!("`{path}` is not where workflows or actions are (.github/workflows/*.yml, .kiln/*.yaml, .github/actions/**/action.yml)")));
    }
    let repo = find_repo(hub, repo_key).await?;
    let checked = checked(hub, Some(&repo), file, ask.content.clone(), None, None).await?;
    let errors: Vec<String> = checked.diagnostics.iter().filter(|d| d.is_error()).map(|d| d.render(file)).collect();
    if !errors.is_empty() {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, format!("{file} has errors, so it isn't saved:\n{}", errors.join("\n"))));
    }
    // Its overlay, saved beside it in the draft (W29-R06): only its own, each checked as it is. A
    // code workflow's draft may carry the GitHub workflow of its name that it makes (W29-R27).
    let stem = |p: &str| p.rsplit('/').next().and_then(|n| n.rsplit_once('.')).map(|(s, _)| s.to_owned());
    let mut more = Vec::new();
    for f in &ask.files {
        let own = match (wf::file_kind(file), wf::file_kind(&f.path)) {
            (Some(wf::FileKind::GhaWorkflow), Some(wf::FileKind::Overlay { stem: s })) => stem(file).as_deref() == Some(s.as_str()),
            (Some(wf::FileKind::CodeWorkflow), Some(wf::FileKind::GhaWorkflow)) => stem(file) == stem(&f.path),
            _ => false,
        };
        if !own || ask.draft != Some(true) {
            return Err(bad(format!(
                "`files` carries a GitHub workflow's own overlay in its draft (.kiln/overlays/<its name>.yaml), or a code workflow's GitHub workflow (.github/workflows/<its name>.yml), with `draft`; not `{}`",
                f.path
            )));
        }
        let c = self::checked(hub, Some(&repo), &f.path, f.text.clone(), None, None).await?;
        let errors: Vec<String> = c.diagnostics.iter().filter(|d| d.is_error()).map(|d| d.render(&f.path)).collect();
        if !errors.is_empty() {
            return Err((StatusCode::UNPROCESSABLE_ENTITY, format!("{} has errors, so it isn't saved:\n{}", f.path, errors.join("\n"))));
        }
        more.push((f.path.clone(), f.text.clone()));
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
        _ if ask.draft == Some(true) => save_draft(hub, caller, &repo, host.as_ref(), to, (file, &ask.content), &more, &title, body).await?,
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
    // W20-R03: a workflow's effect too, as a push to the default branch plans it.
    let preview = if wf::file_kind(file) == Some(wf::FileKind::Action) {
        Value::Null
    } else {
        super::ops::answer(super::preview::of(hub, Some(caller), json!({ "repo": repo.name, "path": file, "content": ask.content, "event": "push" })).await).await.unwrap_or(Value::Null)
    };
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
    let sha = commit_file(hub, caller, repo, host, (&branch, &base, head.as_deref()), &[(file, content)], &format!("{title}\n"), if change { &default } else { &branch }).await?;
    hub.store.audit(caller.name.clone(), "ci.workflow.edit".into(), format!("{} {file} → {branch} {}", repo.name, sha.get(..12).unwrap_or(&sha))).await;
    if !change {
        return Ok(json!({ "branch": branch, "sha": sha }));
    }
    let body = format!("{title}, saved in kiln's workflow editor by {}.", caller.name);
    let opened =
        host.open_change(hub, repo, &branch, &default, title, &body, false).await.map_err(|e| (StatusCode::BAD_GATEWAY, format!("the branch {branch} is saved, but its change wasn't opened: {e:#}")))?;
    Ok(json!({ "branch": branch, "sha": sha, "change": { "number": opened.number, "url": opened.url } }))
}

/// `caller` as a commit's author on a host: their name as diffs give it, at `kiln`.
pub(crate) fn commit_author(caller: &Caller) -> crate::githost::Author {
    let (me, _) = super::diffs::author_of(&caller.name, Some(caller.kind));
    let me = me.strip_prefix("user:").unwrap_or(&me).to_owned();
    crate::githost::Author { email: format!("{}@kiln", me.replace([' ', ':'], "-")), name: me }
}

/// One file committed as `caller` to `branch` of a repository's host: made on `base`, onto `head`
/// (the branch's, when it's there). `already` names where a commit changing nothing is refused.
#[allow(clippy::too_many_arguments)]
async fn commit_file(hub: &Arc<Hub>, caller: &Caller, repo: &Repo, host: &crate::githost::GitHost, (branch, base, head): (&str, &str, Option<&str>), changed: &[(&str, &str)], message: &str, already: &str) -> Result<String, Fail> {
    let files: Vec<crate::githost::Change> = changed.iter().map(|(file, content)| crate::githost::Change { path: (*file).to_owned(), content: Some(content.as_bytes().to_vec()) }).collect();
    commit_changes(hub, caller, repo, host, (branch, base, head), &files, message, already).await
}

#[allow(clippy::too_many_arguments)]
async fn commit_changes(hub: &Arc<Hub>, caller: &Caller, repo: &Repo, host: &crate::githost::GitHost, (branch, base, head): (&str, &str, Option<&str>), files: &[crate::githost::Change], message: &str, already: &str) -> Result<String, Fail> {
    let author = commit_author(caller);
    let file = files.first().map_or("the draft", |f| f.path.as_str());
    let made = host.commit_on(hub, repo, branch, Some(base), head, files, message, &author).await.map_err(|e| match e {
        crate::githost::CommitError::Moved => (StatusCode::CONFLICT, format!("{branch} moved while this was saved: reload it and save again")),
        crate::githost::CommitError::Other(e) => (StatusCode::BAD_GATEWAY, format!("{}'s host refused the commit: {e:#}", repo.name)),
    })?;
    match made {
        crate::githost::Committed::New(sha) => Ok(sha),
        crate::githost::Committed::Unchanged(_) => Err((StatusCode::CONFLICT, format!("{file} is already so on {already}: nothing to save"))),
    }
}

/// Pipelines' Save draft (docs/ci.md W29-R11), the text checked already: the caller's draft of
/// `file` opened, or added to when they have one open. On a repository kiln hosts it's a draft
/// diff; a later save carries its `Kiln-Change`, so it's a new version of it. On a host it's a
/// branch of its own (`kiln/draft/<stem>-<id8>`) with a draft change, each commit's message
/// saying `[skip ci]`; a later save is a new commit on the branch. Its answer says the draft as
/// kiln records it.
#[allow(clippy::too_many_arguments)]
async fn save_draft(hub: &Arc<Hub>, caller: &Caller, repo: &Repo, host: Option<&crate::githost::GitHost>, to: &str, (file, content): (&str, &str), more: &[(String, String)], title: &str, body: &Value) -> Result<Value, Fail> {
    let files: Vec<_> = std::iter::once((file, content)).chain(more.iter().map(|(p, text)| (p.as_str(), text.as_str()))).map(|(path, text)| crate::githost::Change { path: path.into(), content: Some(text.as_bytes().to_vec()) }).collect();
    save_draft_files(hub, caller, repo, host, to, file, &files, title, body, None, false).await
}

/// W29-R25: retain every companion from a native draft's last version. A host keeps those in
/// its branch already. Resolution and generation finish before either writer is called.
async fn carried_files(hub: &Hub, repo: &Repo, base: &str, head: &str) -> Result<Vec<crate::githost::Change>, Fail> {
    let dir = crate::gitops::ensure_commit(hub, repo, head).await.map_err(|e| internal(&e))?;
    let paths = crate::gitops::run_git(hub, &["-C", &dir.to_string_lossy(), "diff", "--name-only", "--no-renames", "-z", base, head, "--"], crate::gitops::LS_REMOTE_TIMEOUT).await.map_err(|e| internal(&e))?;
    let mut files = Vec::new();
    let mut bytes = 0usize;
    for path in paths.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let path = std::str::from_utf8(path).map_err(|_| bad("a draft has a non-UTF-8 path"))?;
        let content = super::locks::blob(hub, &dir, None, head, path).await.map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
        bytes = bytes.saturating_add(content.as_ref().map_or(0, Vec::len));
        if files.len() >= 4096 || bytes > 16 << 20 { return Err(bad("draft companions exceed 16 MiB or 4096 files")); }
        files.push(crate::githost::Change { path: path.into(), content });
    }
    Ok(files)
}

#[allow(clippy::too_many_arguments)]
async fn save_draft_files(hub: &Arc<Hub>, caller: &Caller, repo: &Repo, host: Option<&crate::githost::GitHost>, to: &str, file: &str, changed: &[crate::githost::Change], title: &str, body: &Value, reference: Option<&str>, fresh: bool) -> Result<Value, Fail> {
    use super::pipeline_drafts::{self as drafts, Draft};
    if !caller.can("diffs.submit", &Scope::Repo { repo: repo.name.clone() }) { return Err((StatusCode::FORBIDDEN, format!("saving a draft in {} needs `diffs.submit`", repo.name))); }
    match (to, host) {
        ("diff", Some(crate::githost::GitHost::Kiln)) => {}
        ("change", Some(h)) if !matches!(h, crate::githost::GitHost::Kiln) => {}
        ("branch" | "patch", _) => return Err(bad(format!("a draft is saved as a diff, or as a draft change on its host, not as `{to}` (W29-R11)"))),
        (_, None) => return Err((StatusCode::CONFLICT, format!("kiln can't write to {}: no App or credentials of its host are set up here, so a draft can't be saved", repo.name))),
        _ => return Err(bad(format!("`to: {to}` isn't how {} takes a draft (W29-R11)", repo.name))),
    }
    let (author, _) = super::diffs::author_of(&caller.name, Some(caller.kind));
    let open = drafts::open_for(hub, &repo.name, file, &author).await.map_err(|e| internal(&e))?;
    let default = if matches!(host, Some(crate::githost::GitHost::Kiln)) { body.get("trunk").and_then(Value::as_str).map(str::to_owned) } else { None }.unwrap_or(super::events::facts(hub, repo).await.default_branch);
    let tip = load::resolve(hub, repo, &format!("refs/heads/{default}")).await.map_err(|e| internal(&e))?.ok_or_else(|| (StatusCode::UNPROCESSABLE_ENTITY, format!("{default} has no commits yet")))?;
    let selected = match reference { Some(reference) => load::resolve(hub, repo, reference).await.map_err(|e| bad(format!("{e:#}")))?.ok_or_else(|| missing(format!("no ref {reference}")))?, None => tip.clone() };
    let base = match &open { Some(d) => drafts::head(hub, repo, d).await.map_err(|e| internal(&e))?, None => selected.clone() };
    if open.is_some() && reference.is_some() && selected != tip {
        let dir = crate::gitops::ensure_commit(hub, repo, &base).await.map_err(|e| internal(&e))?;
        if crate::gitops::run_git(hub, &["-C", &dir.to_string_lossy(), "merge-base", "--is-ancestor", &selected, &base], crate::gitops::LS_REMOTE_TIMEOUT).await.is_err() {
            return Err((StatusCode::CONFLICT, "the open workflow draft does not contain that ref; choose its base ref or finish that draft first".into()));
        }
    }
    let mut files = std::collections::BTreeMap::new();
    if let Some(d) = &open && d.kind == "diff" {
        let diff = super::diffs::find(hub, d.name.as_deref().unwrap_or_default()).await.map_err(|e| internal(&e))?.ok_or_else(|| missing("the draft's diff is gone"))?;
        let version = super::diffs::versions(hub, &diff).await.map_err(|e| internal(&e))?.into_iter().last().ok_or_else(|| missing("the draft's version is gone"))?;
        for f in carried_files(hub, repo, &version.base, &base).await? { files.insert(f.path, f.content); }
    } else if open.is_none() && to == "diff" && selected != tip {
        for f in carried_files(hub, repo, &tip, &selected).await? { files.insert(f.path, f.content); }
    }
    for f in changed {
        if !kiln_seams::dependencies::path_valid(&f.path) { return Err(bad(format!("{}: draft file must stay in its repository", f.path))); }
        files.insert(f.path.clone(), f.content.clone());
    }
    let all: Vec<_> = files.iter().map(|(path, content)| crate::githost::Change { path: path.clone(), content: content.clone() }).collect();
    let (locks, differences) = if fresh {
        let proposal = super::locks::proposed_files(hub, repo, &base, &all, file).await.map_err(|why| (StatusCode::UNPROCESSABLE_ENTITY, why))?;
        (proposal.files, Some(proposal.differences))
    } else { (super::locks::draft_files(hub, repo, &base, &all, reference.is_some()).await.map_err(|why| (StatusCode::UNPROCESSABLE_ENTITY, why))?, None) };
    for (path, text) in [(kiln_ci::lock::github::PATH, locks.github), (kiln_ci::lock::kiln::PATH, locks.kiln)] { files.insert(path.into(), text.map(String::into_bytes)); }
    let compare = if to == "diff" { &tip } else { &base };
    let dir = crate::gitops::ensure_commit(hub, repo, compare).await.map_err(|e| internal(&e))?;
    let mut all = Vec::new();
    for (path, content) in files {
        let before = super::locks::blob(hub, &dir, None, compare, &path).await.map_err(|why| (StatusCode::UNPROCESSABLE_ENTITY, why))?;
        if before != content { all.push(crate::githost::Change { path, content }); }
    }
    if all.is_empty() { return Err((StatusCode::CONFLICT, format!("{file} and its locks are already so: nothing to save"))); }
    let now = crate::util::now_unix();
    let mut d = open.clone().unwrap_or_else(|| Draft { id: crate::util::new_id(), repo: repo.name.clone(), path: file.to_owned(), author: author.clone(), state: "draft".into(), created_at: now, ..Draft::default() });
    d.title = title.to_owned();
    d.updated_at = now;
    let out = match (to, host) {
        ("diff", Some(crate::githost::GitHost::Kiln)) => {
            // A later save names the diff's change: it's that diff's next version.
            let title = match d.change.as_deref() {
                Some(change) => format!("{title}\n\nKiln-Change: {change}"),
                None => title.to_owned(),
            };
            let mut body = body.clone();
            if let Some(o) = body.as_object_mut() {
                o.insert("draft".into(), json!(true));
            }
            let out = super::diffs::propose_changes(hub, &repo.name, caller, &all, &title, &body).await?;
            let made = out.pointer("/diffs/0").cloned().unwrap_or(Value::Null);
            d.kind = "diff".into();
            d.name = made.get("name").and_then(Value::as_str).map(str::to_owned);
            d.number = made.get("number").and_then(Value::as_u64);
            d.url = made.get("url").and_then(Value::as_str).map(str::to_owned);
            d.change = made.get("change").and_then(Value::as_str).map(str::to_owned).or(d.change);
            out
        }
        ("change", Some(h)) if !matches!(h, crate::githost::GitHost::Kiln) => {
            if !caller.can("diffs.submit", &Scope::Repo { repo: repo.name.clone() }) {
                return Err((StatusCode::FORBIDDEN, format!("saving a draft to {}'s host needs `diffs.submit`", repo.name)));
            }
            let message = kiln_ci::drafts::draft_message(title);
            let (branch, sha) = match (d.branch.clone(), d.head.clone()) {
                // A later save: a new commit on the draft's branch, on what kiln saved last.
                (Some(branch), Some(head)) => {
                    let sha = commit_changes(hub, caller, repo, h, (&branch, &head, Some(&head)), &all, &message, &branch).await?;
                    (branch, sha)
                }
                _ => {
                    // The id's random end: its start is a time, the same for drafts made the same minute.
                    let branch = kiln_ci::drafts::draft_branch(file, d.id.get(d.id.len().saturating_sub(8)..).unwrap_or(&d.id));
                    let sha = commit_changes(hub, caller, repo, h, (&branch, &base, None), &all, &message, &default).await?;
                    let said = format!("{title}, a draft saved in kiln's Pipelines by {}: nothing runs it until it's ready (docs/ci.md W29-R10).", caller.name);
                    let opened = h.open_change(hub, repo, &branch, &default, title, &said, true).await.map_err(|e| (StatusCode::BAD_GATEWAY, format!("the draft's branch {branch} is saved, but its change wasn't opened: {e:#}")))?;
                    d.number = Some(opened.number);
                    d.url = Some(opened.url);
                    d.name = Some(match h {
                        crate::githost::GitHost::Gitlab(_) => format!("!{}", opened.number),
                        _ => format!("#{}", opened.number),
                    });
                    (branch, sha)
                }
            };
            d.kind = "change".into();
            d.branch = Some(branch.clone());
            d.head = Some(sha.clone());
            json!({ "branch": branch, "sha": sha, "change": { "number": d.number, "url": d.url } })
        }
        ("branch" | "patch", _) => return Err(bad(format!("a draft is saved as a diff, or as a draft change on its host, not as `{to}` (W29-R11)"))),
        (_, None) => return Err((StatusCode::CONFLICT, format!("kiln can't write to {}: no App or credentials of its host are set up here, so a draft can't be saved", repo.name))),
        _ => return Err(bad(format!("`to: {to}` isn't how {} takes a draft (W29-R11)", repo.name))),
    };
    drafts::put(hub, &d).await.map_err(|e| internal(&e))?;
    hub.store.audit(caller.name.clone(), "ci.workflow.draft".into(), format!("{} {file} → {} {}", repo.name, d.name.as_deref().unwrap_or("draft"), if open.is_some() { "saved again" } else { "opened" })).await;
    let mut out = out;
    if let Some(differences) = differences {
        let head = drafts::head(hub, repo, &d).await.map_err(|e| internal(&e))?;
        let evidence = super::locks::keep_proposal(hub, repo, &d, &base, &head, &differences).await?;
        if let Some(out) = out.as_object_mut() { out.insert("lock_review".into(), evidence); }
    }
    if let Some(o) = out.as_object_mut() {
        o.insert("draft".into(), serde_json::to_value(&d).unwrap_or(Value::Null));
    }
    Ok(out)
}

/// W29-R25: the standalone lock operation uses the same writer as Save draft and New. Its
/// anchor is a workflow where there is one, so a later editor save remains the same draft.
pub(crate) async fn lock_draft(hub: &Arc<Hub>, caller: &Caller, repo: &Repo, reference: &str) -> Result<Value, Fail> {
    let scope = Scope::Repo { repo: repo.name.clone() };
    if !caller.can("workflows.edit", &scope) || !caller.can("diffs.submit", &scope) { return Err((StatusCode::FORBIDDEN, "updating locks needs `workflows.edit` and `diffs.submit`".into())); }
    let commit = load::resolve(hub, repo, reference).await.map_err(|e| bad(format!("{e:#}")))?.ok_or_else(|| missing(format!("no ref {reference}")))?;
    let loaded = load::load(hub, repo, &commit).await.map_err(|e| internal(&e))?;
    let anchor = loaded.workflows.first().map_or(kiln_ci::lock::github::PATH, |w| w.path.as_str());
    let host = crate::githost::for_repo(hub, repo).await;
    let to = match &host { Some(crate::githost::GitHost::Kiln) => "diff", Some(_) => "change", None => "patch" };
    save_draft_files(hub, caller, repo, host.as_ref(), to, anchor, &[], "Lock workflow dependencies", &json!({}), Some(reference), false).await
}

/// W29-R25: a fresh-action request uses the same draft, author and companion-file writer.
/// The flag is internal, never taken from the editor's JSON or a caller's display name.
pub(crate) async fn fresh_lock_draft(hub: &Arc<Hub>, caller: &Caller, repo: &Repo, commit: &str, path: &str) -> Result<Value, Fail> {
    let scope = Scope::Repo { repo: repo.name.clone() };
    if !caller.can("workflows.edit", &scope) || !caller.can("diffs.submit", &scope) { return Err((StatusCode::FORBIDDEN, "fresh lock drafts need `workflows.edit` and `diffs.submit`".into())); }
    let host = crate::githost::for_repo(hub, repo).await;
    let to = match &host { Some(crate::githost::GitHost::Kiln) => "diff", Some(_) => "change", None => "patch" };
    save_draft_files(hub, caller, repo, host.as_ref(), to, path, &[], "Review fresh workflow dependencies", &json!({}), Some(commit), true).await
}

/// Pipelines' New (docs/ci.md W29-R08): `kiln_ci::scaffold`'s files for a kind (`kind`), a
/// dialect (`language`: `gha`, `kiln` or `ts`, W29-R27), a name and where it starts (`from`: `blank`,
/// `recommended`, `{ "starter": "rust" }` or `{ "copy": path }`), or `files` as the caller edited
/// them, each one of the scaffold's paths or under its folder. With `dry` it says the files;
/// otherwise each is checked as Save draft checks a file, and they're saved together as the
/// caller's draft (W29-R10, R11), which needs `diffs.submit`.
pub async fn new_draft(hub: &Arc<Hub>, caller: &Caller, repo_key: &str, body: &Value) -> Result<Value, Fail> {
    use kiln_ci::scaffold::{self, Kind, Start};
    let text = |k: &str| body.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let kind = text("kind").and_then(Kind::parse).ok_or_else(|| bad("`kind` is workflow, reusable, composite, javascript, docker, firecracker or shards"))?;
    let name = text("name").ok_or_else(|| bad("`name` names it"))?;
    let dialect = match text("language") {
        None | Some("gha" | "yaml") => scaffold::Dialect::Gha,
        Some("kiln") => scaffold::Dialect::Kiln,
        Some("ts" | "typescript") => scaffold::Dialect::TypeScript,
        Some(other) => return Err(bad(format!("`language` is gha, kiln or ts, not `{other}`"))),
    };
    let repo = find_repo(hub, repo_key).await?;
    let default = super::events::facts(hub, &repo).await.default_branch;
    let tip = load::resolve(hub, &repo, &default).await.map_err(|e| internal(&e))?.ok_or_else(|| (StatusCode::UNPROCESSABLE_ENTITY, format!("{default} has no commits yet")))?;
    let dir = crate::gitops::ensure_commit(hub, &repo, &tip).await.map_err(|e| internal(&e))?;
    let d = dir.to_string_lossy().into_owned();
    let listing = crate::gitops::run_git(hub, &["-C", &d, "ls-tree", "--name-only", &tip], crate::gitops::LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).into_owned()).unwrap_or_default();
    let paths: Vec<&str> = listing.lines().collect();
    let from = body.get("from");
    let copy = match from.and_then(|f| f.get("copy")).and_then(Value::as_str) {
        Some(path) => Some(load::blob(hub, &repo, &tip, path).await.map_err(|_| missing(format!("{} has no `{path}` on {default} to copy", repo.name)))?),
        None => None,
    };
    let start = match (from.and_then(Value::as_str), from.and_then(|f| f.get("starter")).and_then(Value::as_str), copy.as_deref()) {
        (_, _, Some(text)) => Start::Copy(text),
        (_, Some(lang), _) => Start::Starter(lang),
        (Some("recommended"), ..) => Start::Recommended,
        (None | Some("blank"), ..) => Start::Blank,
        (Some(other), ..) => return Err(bad(format!("`from` is blank, recommended, a starter or a copy, not `{other}`"))),
    };
    let mut files = scaffold::scaffold(kind, dialect, name, start, &paths, &default).map_err(bad)?;
    let main = files.first().map(|f| f.path.clone()).unwrap_or_default();
    let folder = if kind.is_action() { main.rsplit_once('/').map_or(String::new(), |(f, _)| format!("{f}/")) } else { main.clone() };
    // The caller's own edits of them: each one of its paths, or a file of the action's folder.
    if let Some(edited) = body.get("files").and_then(Value::as_array) {
        let mut out = Vec::new();
        for f in edited {
            let (Some(path), Some(text)) = (f.get("path").and_then(Value::as_str), f.get("text").and_then(Value::as_str)) else { return Err(bad("each of `files` is { path, text }")) };
            if path != main && !(kind.is_action() && path.starts_with(&folder) && !path.contains("..")) {
                return Err(bad(format!("`{path}` isn't {main}'s: New writes {}", if kind.is_action() { format!("files under {folder}") } else { main.clone() })));
            }
            out.push(scaffold::File { path: path.to_owned(), text: text.to_owned() });
        }
        if !out.iter().any(|f| f.path == main) {
            return Err(bad(format!("`files` leaves out {main}")));
        }
        out.sort_by_key(|f| f.path != main);
        files = out;
    }
    if body.get("dry").and_then(Value::as_bool) == Some(true) {
        // W29-R08, pl-6: New says which connected machines run each microVM kind, and the next
        // version that could publish a new action. A host assigns its draft's number when it
        // opens it; only kiln's own next diff number can be read without making a draft.
        let machines = {
            let agents = hub.agents.load();
            let names = |executor: &str| {
                let mut names: Vec<String> = agents.values().filter(|a| a.connected && a.facts.as_deref().is_some_and(|f| kiln_ci::sched::routing::has_executor(f, executor))).map(|a| a.name.clone()).collect();
                names.sort();
                names
            };
            json!({ "firecracker": names("firecracker"), "shards": names("shards") })
        };
        let tags = crate::gitops::run_git(hub, &["-C", &d, "for-each-ref", "--format=%(refname:strip=2)", "refs/tags"], crate::gitops::LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).into_owned()).unwrap_or_default();
        let added = kiln_ci::wf::diff::Change { kind: "action", name: name.to_owned(), what: "added", detail: None };
        let next_version = kiln_ci::releases::next_versions(kiln_ci::pipelines::newest_release(tags.lines()), &[added]).into_iter().next();
        let draft_name = if repo.mode == "native" {
            hub.store.call(|c| c.query_row("SELECT COALESCE(MAX(number), 0) + 1 FROM diffs", [], |r| r.get::<_, i64>(0)).map(|n| Some(format!("D{n}"))).map_err(Into::into)).await.map_err(|e| internal(&e))?
        } else {
            None
        };
        return Ok(json!({ "path": main, "files": files, "machines": machines, "next_version": next_version, "draft_name": draft_name }));
    }
    if load::blob(hub, &repo, &tip, &main).await.is_ok() {
        return Err((StatusCode::CONFLICT, format!("{} has {main} on {default} already: open it in Pipelines to change it", repo.name)));
    }
    for f in &files {
        if wf::file_kind(&f.path).is_none() {
            continue;
        }
        let c = checked(hub, Some(&repo), &f.path, f.text.clone(), None, None).await?;
        let errors: Vec<String> = c.diagnostics.iter().filter(|d| d.is_error()).map(|d| d.render(&f.path)).collect();
        if !errors.is_empty() {
            return Err((StatusCode::UNPROCESSABLE_ENTITY, format!("{} has errors, so it isn't saved:\n{}", f.path, errors.join("\n"))));
        }
    }
    let host = crate::githost::for_repo(hub, &repo).await;
    let to = match &host {
        Some(crate::githost::GitHost::Kiln) => "diff",
        Some(_) => "change",
        None => return Err((StatusCode::CONFLICT, format!("kiln can't write to {}: no App or credentials of its host are set up here, so a draft can't be saved", repo.name))),
    };
    let title = text("title").map_or_else(|| format!("New {main}"), str::to_owned);
    let (first, rest) = files.split_first().ok_or_else(|| bad("New made no files"))?;
    let more: Vec<(String, String)> = rest.iter().map(|f| (f.path.clone(), f.text.clone())).collect();
    let mut out = save_draft(hub, caller, &repo, host.as_ref(), to, (&first.path, &first.text), &more, &title, body).await?;
    if let Some(o) = out.as_object_mut() {
        o.insert("path".into(), json!(main));
        o.insert("files".into(), json!(files.iter().map(|f| &f.path).collect::<Vec<_>>()));
    }
    Ok(out)
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

/// `GET /api/ci/sdk/kiln.d.ts`: `@kiln/sdk`'s declarations, for the editor and people's own
/// (W29-R27).
pub fn sdk_types() -> Result<Response, Fail> {
    Ok(srv::response(StatusCode::OK, "application/typescript; charset=utf-8", srv::full(wf::sdk::dts())))
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
    Ok(srv::json(StatusCode::OK, &json!({ "ref": r, "sha": sha, "github": { "run": commit.github_run, "why": commit.github_note }, "workflows": workflows, "actions": commit.actions })))
}

/// `GET /api/ci/repos/:repo/workflows/:path?ref=`: one workflow's source, model and diagnostics;
/// or an action's (`kind: action`, W21-R07), its source and diagnostics.
pub async fn get(hub: &Hub, repo: &str, path: &str, q: &HashMap<String, String>) -> Result<Response, Fail> {
    if wf::file_kind(path) == Some(wf::FileKind::Action) {
        return get_action(hub, repo, path, q).await;
    }
    if !matches!(wf::file_kind(path), Some(wf::FileKind::GhaWorkflow | wf::FileKind::KilnWorkflow | wf::FileKind::CodeWorkflow)) {
        return Err(missing(format!("`{path}` is not where workflows are (.github/workflows/*.yml, .kiln/*.yaml, .kiln/*.ts)")));
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

async fn get_action(hub: &Hub, repo: &str, path: &str, q: &HashMap<String, String>) -> Result<Response, Fail> {
    let (repo, r, sha) = at_ref(hub, repo, q).await?;
    let commit = load::load(hub, &repo, &sha).await.map_err(|e| internal(&e))?;
    if !commit.actions.iter().any(|a| a == path) {
        return Err(missing(format!("no action `{path}` at {r} ({sha})")));
    }
    let source = load::blob(hub, &repo, &sha, path).await.map_err(|e| internal(&e))?;
    let (p, text) = (path.to_owned(), source.clone());
    let checked = crate::rt::blocking(move || wf::check(&p, &text, None)).await.map_err(|e| internal(&e))?;
    let (host, saves) = saves(hub, &repo).await;
    Ok(srv::json(
        StatusCode::OK,
        &json!({ "path": path, "kind": "action", "ref": r, "sha": sha, "source": source, "diagnostics": checked.diagnostics, "outline": Value::Null, "host": host, "saves": saves }),
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
    let ask: serde_json::Value = srv::body_json(body, 1 << 20).await.map_err(|e| bad(format!("{e:#}")))?;
    repository_dispatch_value(hub, repo, ask, started_by).await
}

/// The same, its request already read (`ci.runs.repository_dispatch`).
pub async fn repository_dispatch_value(hub: &Hub, repo: &str, ask: serde_json::Value, started_by: Option<serde_json::Value>) -> Result<Response, Fail> {
    let repo = find_repo(hub, repo).await?;
    let ask: RepoDispatchAsk = serde_json::from_value(ask).map_err(|e| bad(format!("{e}")))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// W29-R27: `GET /api/ci/sdk/kiln.d.ts` answers with the generator's declarations.
    #[test]
    fn w29_r27_the_sdk_types_are_served() -> anyhow::Result<()> {
        use http_body_util::BodyExt as _;
        let resp = sdk_types().map_err(|(s, m)| anyhow::anyhow!("{s}: {m}"))?;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get(http::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()), Some("application/typescript; charset=utf-8"));
        let body = crate::rt::block_on(async move { Ok(resp.into_body().collect().await?.to_bytes()) })?;
        assert_eq!(String::from_utf8_lossy(&body), wf::sdk::dts());
        Ok(())
    }

    fn runner(name: &str, labels: &[&str], os: &str, arch: &str) -> crate::store::AgentRow {
        crate::store::AgentRow {
            name: name.into(),
            labels: labels.iter().map(|l| (*l).to_owned()).collect(),
            slots: 1,
            hostname: name.into(),
            os: os.into(),
            arch: arch.into(),
            version: String::new(),
            ephemeral: false,
            connected: true,
            last_seen: 0,
            enrolled_at: 0,
        }
    }

    /// The labels `KCI6007` finds no runner, rule or pool for, in `runs-on: [..labels]`.
    fn unknown(labels: &str, runners: &[crate::store::AgentRow]) -> Vec<String> {
        let text = format!("name: w\non:\n  push: {{ branches: [main] }}\njobs:\n  j:\n    runs-on: [{labels}]\n    steps:\n      - run: true\n");
        let (w, _) = wf::parse_workflow(".kiln/w.yaml", &text, Dialect::Kiln);
        w.map(|w| unknown_in(&w, "acme/app", &super::super::routing::defaults(false), runners, &[]).into_iter().map(|d| d.message).collect()).unwrap_or_else(|| vec!["no model".into()])
    }

    /// W05-R21 in `KCI6007`: `linux` is carried by a Linux runner whose only Linux label is
    /// GitHub's implicit `Linux`, `Linux` by one labelled `linux`; a Mac carries neither.
    #[test]
    fn w05_r21_kci6007_matches_labels_in_any_case() {
        let implicit = runner("adacachy", &["baremetal", "bench", "local"], "linux", "x86_64");
        let labelled = runner("fedora", &["linux"], "linux", "x86_64");
        let mac = runner("mac", &["baremetal"], "macos", "aarch64");
        assert_eq!(unknown("baremetal, linux", std::slice::from_ref(&implicit)), Vec::<String>::new());
        assert_eq!(unknown("Linux", std::slice::from_ref(&labelled)), Vec::<String>::new());
        assert_eq!(unknown("BAREMETAL, LINUX", std::slice::from_ref(&implicit)), Vec::<String>::new());
        let on_mac = unknown("baremetal, linux", std::slice::from_ref(&mac));
        assert!(on_mac.len() == 1 && on_mac.iter().all(|m| m.contains("`linux`")), "{on_mac:?}");
    }
}
