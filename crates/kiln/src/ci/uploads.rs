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
pub async fn pack(hub: &Hub, repo: &str, q: &HashMap<String, String>, body: Incoming, by: &str) -> Result<Response, Fail> {
    pack_from(hub, repo, q, PackFrom::Body(body), by).await
}

/// Where a pack's bytes come from: a request's body as it streams in, or bytes already read
/// (`ci.uploads.pack`'s, base64 in its input).
pub enum PackFrom {
    Body(Incoming),
    Bytes(Vec<u8>),
}

impl PackFrom {
    async fn next(&mut self) -> anyhow::Result<Option<bytes::Bytes>> {
        match self {
            PackFrom::Body(b) => srv::next_chunk(b).await,
            PackFrom::Bytes(v) => Ok((!v.is_empty()).then(|| bytes::Bytes::from(std::mem::take(v)))),
        }
    }
}

/// The same, from `from`.
pub async fn pack_from(hub: &Hub, repo: &str, q: &HashMap<String, String>, mut from: PackFrom, by: &str) -> Result<Response, Fail> {
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
        match from.next().await {
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
    keep(hub, &repo, &m, &sha).await?;
    hub.store.audit(by.to_owned(), "ci.uploads.pack".into(), format!("{} {sha} ({size} bytes)", repo.name)).await;
    Ok(srv::json(StatusCode::CREATED, &json!({ "sha": sha, "bytes": size })))
}

/// An upload's commit kept as `refs/kiln/uploads/<sha>`: in the mirror, and in a repository kiln
/// hosts too, as its legs fetch from it (W14-R02), not from the mirror.
async fn keep(hub: &Hub, repo: &Repo, m: &str, sha: &str) -> Result<(), Fail> {
    let name = format!("refs/kiln/uploads/{sha}");
    gitops::run_git(hub, &["-C", m, "update-ref", &name, sha], LS_REMOTE_TIMEOUT).await.map_err(|e| (StatusCode::CONFLICT, format!("there's no commit {sha} here: {e:#}")))?;
    forget_old_uploads(hub, m).await;
    if let Some(hosted) = super::hosting::native_path(hub, repo) {
        let h = hosted.to_string_lossy().into_owned();
        gitops::run_git(hub, &["-C", &h, "fetch", "--quiet", "--no-tags", m, &format!("{name}:{name}")], LS_REMOTE_TIMEOUT).await.map_err(|e| internal(&e))?;
        forget_old_uploads(hub, &h).await;
    }
    Ok(())
}

/// Uploaded commits older than a week go (their objects with the repository's next `gc`).
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
    let ask: Value = srv::body_json(body, 4 << 20).await.map_err(|e| bad(format!("{e:#}")))?;
    local_runs_value(hub, repo, caller, ask).await
}

/// The same, its request already read (`ci.runs.local`).
pub async fn local_runs_value(hub: &Hub, repo: &str, caller: Option<&Caller>, ask: Value) -> Result<Response, Fail> {
    if !hub.ci.enabled() {
        return Err((StatusCode::NOT_FOUND, "the CI engine is off ([ci] enabled)".into()));
    }
    let repo = find_repo(hub, repo).await?;
    let ask: Ask = serde_json::from_value(ask).map_err(|e| bad(format!("{e}")))?;
    // The owner's runs are trusted, as the owner's pushes are; anyone else's are a fork's.
    let (trusted, actor) = match caller {
        None => (true, "admin".to_owned()),
        Some(c) => (c.is_owner(), c.name.clone()),
    };
    let (runs, skipped) = start_runs(hub, &repo, &ask, trusted, &actor, true).await?;
    let what = format!("{} {} at {}: {} run(s){}", repo.name, if ask.event.is_empty() { "push" } else { ask.event.as_str() }, ask.sha, runs.len(), if trusted { "" } else { ", untrusted" });
    hub.store.audit(actor, "ci.runs.local".into(), what).await;
    Ok(srv::json(StatusCode::CREATED, &json!({ "runs": runs, "skipped": skipped, "trusted": trusted })))
}

/// The runs `ask` asks for of commit `ask.sha`, `trusted` or not, as `actor`'s: those that started
/// (`{ id, workflow }`), and those that didn't, with why. Local runs' and bisections'; a
/// bisection's run its workflow whatever its triggers' filters say (`match_triggers` false).
pub async fn start_runs(hub: &Hub, repo: &Repo, ask: &Ask, trusted: bool, actor: &str, match_triggers: bool) -> Result<(Vec<Value>, Vec<Value>), Fail> {
    if !is_sha(&ask.sha) {
        return Err(bad("`sha` is the commit's 40 hex digits"));
    }
    let repo = repo.clone();
    let actor = actor.to_owned();
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
            && match_triggers
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
    Ok((runs, skipped))
}

#[derive(Deserialize)]
struct TryAsk {
    /// The action's file (`.github/actions/<name>/action.yml`, `.kiln/actions/…`).
    path: String,
    /// Its text, as its editor has it (saved or not).
    content: String,
    /// The branch, tag or commit it's tried on; the default branch when empty.
    #[serde(default, rename = "ref")]
    git_ref: String,
    #[serde(default)]
    inputs: serde_json::Map<String, Value>,
    /// What runs it: a label, or a runner's name.
    runs_on: String,
}

/// Where the try workflow is, in the commit made for it.
const TRY_PATH: &str = ".kiln/kiln-try.yaml";

/// A name a workflow's expressions can take as written (`inputs.<name>`, `steps.action.outputs.<name>`).
fn plain_name(n: &str) -> bool {
    n.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The workflow that tries an action at `dir` on `runs_on`: each of its inputs a dispatch input
/// passed on to it (so what's given is data, never YAML or an expression), its outputs the job's.
fn try_workflow(dir: &str, action: &kiln_ci::wf::Action, runs_on: &str) -> String {
    let q = |s: &str| serde_json::to_string(s).unwrap_or_default();
    let mut w = format!("name: {}\non:\n  workflow_dispatch:\n", q(&format!("Try ./{dir}")));
    if !action.inputs.is_empty() {
        w.push_str("    inputs:\n");
        for i in &action.inputs {
            w.push_str(&format!("      {}:\n        type: string\n        required: false\n", q(&i.name)));
        }
    }
    w.push_str(&format!("jobs:\n  try:\n    runs-on: {}\n", q(runs_on)));
    if !action.outputs.is_empty() {
        w.push_str("    outputs:\n");
        for o in &action.outputs {
            w.push_str(&format!("      {}: ${{{{ steps.action.outputs.{} }}}}\n", q(&o.name), o.name));
        }
    }
    w.push_str(&format!("    steps:\n      - id: action\n        uses: {}\n", q(&format!("./{dir}"))));
    if !action.inputs.is_empty() {
        w.push_str("        with:\n");
        for i in &action.inputs {
            w.push_str(&format!("          {}: ${{{{ inputs.{} }}}}\n", q(&i.name), i.name));
        }
    }
    w
}

/// `POST /api/ci/repos/:repo/actions/try { path, content, ref?, inputs, runs_on }` (W21-R07's "Try
/// it"): an action, as its editor has it, run once with `inputs` on what `runs_on` names. It's
/// committed on `ref` with a workflow whose one job uses it and gives its outputs as the job's,
/// kept as an upload is (a week, under `refs/kiln/uploads/`), and that workflow is started. The
/// owner's run is trusted; anyone else's is a fork's, as `local-runs`' are. → `{ run }`.
pub async fn try_action(hub: &Hub, caller: Option<&Caller>, repo: &str, body: Value) -> Result<Value, Fail> {
    if !hub.ci.enabled() {
        return Err((StatusCode::NOT_FOUND, "the CI engine is off ([ci] enabled)".into()));
    }
    let repo = find_repo(hub, repo).await?;
    let ask: TryAsk = serde_json::from_value(body).map_err(|e| bad(format!("{e}")))?;
    let path = ask.path.trim_start_matches("./").to_owned();
    if kiln_ci::wf::file_kind(&path) != Some(kiln_ci::wf::FileKind::Action) || path.split('/').any(|s| s.is_empty() || s == "." || s == "..") {
        return Err(bad(format!("`{}` isn't an action's file (.github/actions/<name>/action.yml, .kiln/actions/…)", ask.path)));
    }
    let runs_on = ask.runs_on.trim();
    if runs_on.is_empty() || !runs_on.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:/+".contains(c)) {
        return Err(bad(format!("`{runs_on}` isn't a label or a runner's name")));
    }
    let content = ask.content.clone();
    let (action, diags) = crate::rt::blocking(move || kiln_ci::wf::parse_action(&content)).await.map_err(|e| internal(&e))?;
    let errors: Vec<String> = diags.iter().filter(|d| d.is_error()).map(|d| d.render(&path)).collect();
    let Some(action) = action.filter(|_| errors.is_empty()) else {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, format!("{path} has errors, so it isn't tried:\n{}", errors.join("\n"))));
    };
    if let Some(n) = action.inputs.iter().map(|i| &i.name).chain(action.outputs.iter().map(|o| &o.name)).find(|n| !plain_name(n)) {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, format!("`{n}` can't be passed on by name: try it from a workflow of your own")));
    }
    if let Some(k) = ask.inputs.keys().find(|k| !action.inputs.iter().any(|i| &i.name == *k)) {
        return Err(bad(format!("{path} has no input `{k}`")));
    }
    let dir = path.rsplit_once('/').map_or("", |(d, _)| d).to_owned();
    let files = [(path.clone(), ask.content.clone()), (TRY_PATH.to_owned(), try_workflow(&dir, &action, runs_on))];
    let once = run_once(hub, caller, &repo, &ask.git_ref, &files, TRY_PATH, &ask.inputs, &format!("Try {path}")).await?;
    hub.store
        .audit(once.actor.clone(), "ci.actions.try".into(), format!("{} {path} at {} on {runs_on}: run {}{}", repo.name, &once.sha[..12], once.run, if once.trusted { "" } else { ", untrusted" }))
        .await;
    Ok(json!({ "run": once.run, "sha": once.sha, "trusted": once.trusted }))
}

/// A run [`run_once`] started.
struct Once {
    run: String,
    sha: String,
    trusted: bool,
    actor: String,
}

/// `files` as an editor has them, run once: committed on `git_ref`'s head (the default branch when
/// empty), kept as an upload is, and the workflow at `workflow` among them started for
/// `workflow_dispatch` with `inputs`. Trusted for the server's owner; a fork's for anyone else.
#[allow(clippy::too_many_arguments)]
async fn run_once(
    hub: &Hub,
    caller: Option<&Caller>,
    repo: &Repo,
    git_ref: &str,
    files: &[(String, String)],
    workflow: &str,
    inputs: &serde_json::Map<String, Value>,
    message: &str,
) -> Result<Once, Fail> {
    // The mirror, made now if nothing has needed it yet.
    let m = match mirror(hub, repo) {
        Ok(m) => m,
        Err(_) => gitops::sync_mirror(hub, repo).await.map_err(|e| internal(&e))?.to_string_lossy().into_owned(),
    };
    let r = if git_ref.is_empty() { super::events::facts(hub, repo).await.default_branch } else { git_ref.to_owned() };
    let tip = load::resolve(hub, repo, &r).await.map_err(|e| internal(&e))?.ok_or_else(|| (StatusCode::NOT_FOUND, format!("{} has no branch, tag or commit `{r}`", repo.name)))?;
    let mut tree = tip.clone();
    for (file, text) in files {
        let mode = gitops::run_git(hub, &["-C", &m, "ls-tree", &tree, "--", file], LS_REMOTE_TIMEOUT)
            .await
            .ok()
            .and_then(|o| String::from_utf8_lossy(&o).split_whitespace().next().map(str::to_owned))
            .unwrap_or_else(|| "100644".into());
        tree = super::diffs::tree_with(hub, &m, &tree, super::diffs::Edit::Put { file, mode: &mode, content: text.clone().into_bytes() }).await.map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    }
    let (actor, trusted) = match caller {
        None => ("admin".to_owned(), true),
        Some(c) => (c.name.clone(), c.is_owner()),
    };
    let (who, _) = super::diffs::author_of(&actor, caller.map(|c| c.kind));
    let who = who.strip_prefix("user:").unwrap_or(&who).to_owned();
    let sha = super::diffs::make_commit(hub, &m, &tree, &tip, (&who, None), &who, &format!("{message}\n")).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    keep(hub, repo, &m, &sha).await?;
    let commit = load::load(hub, repo, &sha).await.map_err(|e| internal(&e))?;
    let l = commit.workflows.into_iter().find(|l| l.path == workflow).ok_or_else(|| (StatusCode::INTERNAL_SERVER_ERROR, format!("{workflow} wasn't read")))?;
    if l.has_errors() {
        let said: Vec<String> = l.diagnostics.iter().filter(|d| d.severity == kiln_ci::wf::Severity::Error).map(|d| format!("{} {}", d.code, d.message)).collect();
        return Err((StatusCode::UNPROCESSABLE_ENTITY, format!("it can't be run as it is: {}", said.join("; "))));
    }
    let declared = l.workflow.as_ref().and_then(|w| w.on.iter().find(|t| t.event == "workflow_dispatch")).map(|t| t.inputs.clone());
    let full_ref = if r.starts_with("refs/") { r.clone() } else { format!("refs/heads/{r}") };
    let (mut event, typed) = super::events::preview_event(hub, repo, "workflow_dispatch", &full_ref, &sha, None, inputs, declared.as_deref(), &l.path).await.map_err(|r| (r.status, r.message))?;
    event.trusted = trusted;
    event.actor = actor.clone();
    event.source = kiln_ci::trigger::Source::Api;
    let (tx, rx) = async_channel::bounded(1);
    let given = Given { reply: Some(tx), ..Given::default() };
    super::events::start_given(hub, repo, &event, &l, (message, &who), typed, given).await.map_err(|e| internal(&e))?;
    match crate::rt::timeout(std::time::Duration::from_secs(60), rx.recv()).await {
        Ok(Ok(run)) => Ok(Once { run, sha, trusted, actor }),
        _ => Err((StatusCode::INTERNAL_SERVER_ERROR, "the run didn't start".into())),
    }
}

#[derive(Deserialize)]
struct BuildAsk {
    name: String,
    /// `dockerfile` (by default) or `agentfile`.
    #[serde(default)]
    kind: Option<String>,
    content: String,
    #[serde(default)]
    platforms: Vec<String>,
    runs_on: String,
    #[serde(default)]
    push: bool,
    #[serde(default, rename = "ref")]
    git_ref: String,
}

/// Where the build's workflow is, in the commit made for it.
const BUILD_PATH: &str = ".kiln/kiln-build-image.yaml";

/// `POST /api/ci/images/build?repo= { name, kind?, content, platforms?, runs_on, push?, ref? }`
/// (W21-R08's image builder): an image's definition, as the editor has it, built once by
/// `kiln/build-image@v1` on what `runs_on` names, from `.kiln/images/<name>/` in a commit made for
/// it (as "Try it"'s is). With `push`, pushed to kiln's registry and registered, which only the
/// server's owner's (trusted) runs may: anyone else's is a fork's, its token read-only. → `{ run,
/// sha, trusted }`.
pub async fn build(hub: &Hub, caller: Option<&Caller>, q: &HashMap<String, String>, body: Value) -> Result<Value, Fail> {
    if !hub.ci.enabled() {
        return Err((StatusCode::NOT_FOUND, "the CI engine is off ([ci] enabled)".into()));
    }
    let repo = find_repo(hub, q.get("repo").map_or("", String::as_str)).await?;
    let ask: BuildAsk = serde_json::from_value(body).map_err(|e| bad(format!("{e}")))?;
    let name = ask.name.trim().to_owned();
    if name.contains('/') || !crate::ci::registry::valid_name(&name) {
        return Err(bad(format!("`{name}` isn't an image name (lowercase letters, digits, `.`, `_`, `-`)")));
    }
    let agentfile = ask.kind.as_deref().is_some_and(|k| k.eq_ignore_ascii_case("agentfile"));
    if !agentfile {
        let c = ask.content.clone();
        let found = crate::rt::blocking(move || kiln_ci::dockerfile::check(&c)).await.map_err(|e| internal(&e))?;
        let errors: Vec<String> = found.iter().filter(|d| d.is_error()).map(|d| d.render("Dockerfile")).collect();
        if !errors.is_empty() {
            return Err((StatusCode::UNPROCESSABLE_ENTITY, format!("the Dockerfile has errors, so it isn't built:\n{}", errors.join("\n"))));
        }
    }
    let runs_on = ask.runs_on.trim();
    if runs_on.is_empty() || !runs_on.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:/+".contains(c)) {
        return Err(bad(format!("`{runs_on}` isn't a label or a runner's name")));
    }
    if let Some(p) = ask.platforms.iter().find(|p| !p.split('/').all(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric()))) {
        return Err(bad(format!("`{p}` isn't a platform (`linux/amd64`, `linux/arm64`, …)")));
    }
    if ask.push && !caller.is_none_or(Caller::is_owner) {
        return Err((StatusCode::FORBIDDEN, "pushing an image from the editor is the server owner's: save it to the repository, and its workflow pushes it".into()));
    }
    let q = |s: &str| serde_json::to_string(s).unwrap_or_default();
    let mut w = format!(
        "name: {}\non: workflow_dispatch\njobs:\n  build:\n    runs-on: {}\n    permissions:\n      contents: read\n      packages: {}\n    steps:\n      - uses: kiln/build-image@v1\n        with:\n          name: {}\n          push: {}\n",
        q(&format!("Build the {name} image")),
        q(runs_on),
        if ask.push { "write" } else { "read" },
        q(&name),
        q(if ask.push { "true" } else { "false" }),
    );
    if !ask.platforms.is_empty() {
        w.push_str(&format!("          platforms: {}\n", q(&ask.platforms.join(","))));
    }
    let file = format!(".kiln/images/{name}/{}", if agentfile { "Agentfile" } else { "Dockerfile" });
    let files = [(file, ask.content.clone()), (BUILD_PATH.to_owned(), w)];
    let what = if ask.push { "Build and push" } else { "Build" };
    let once = run_once(hub, caller, &repo, &ask.git_ref, &files, BUILD_PATH, &serde_json::Map::new(), &format!("{what} the {name} image")).await?;
    hub.store
        .audit(once.actor.clone(), "ci.images.build".into(), format!("{} {name} at {} on {runs_on}{}: run {}", repo.name, &once.sha[..12], if ask.push { ", pushed" } else { "" }, once.run))
        .await;
    Ok(json!({ "run": once.run, "sha": once.sha, "trusted": once.trusted }))
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
