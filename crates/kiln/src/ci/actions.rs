//! Actions on the server (docs/ci.md W07-R02 to W07-R04, W07-R12): every remote `uses:` a run may
//! reach is resolved to a commit when the run starts (through composite actions and the reusable
//! workflows it calls, recursively), mirrored, and served to the run's legs as a tarball of that
//! commit, so every leg and every rerun of the attempt runs exactly the same code, and a source
//! that goes away later doesn't stop them. The server's and the repository's action policies are
//! checked on each first.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use http::StatusCode;
use kiln_ci::wf::Workflow;
use kiln_ci::wf::ir::Step;
use kiln_ci::wire::ActionRef;

use kiln_ci::policy::Policy;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use super::{Fail, token};
use crate::gitops;
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;
use crate::store::Repo;

const GIT_TIMEOUT: Duration = Duration::from_secs(300);

/// A remote action: `owner/repo[/path]@ref`, or in the kiln dialect `kiln://owner/repo[/path]@ref`,
/// one of kiln's own repositories (`native`, W07-R01).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub owner: String,
    pub repo: String,
    pub path: String,
    pub git_ref: String,
    pub native: bool,
}

impl Remote {
    pub fn parse(uses: &str) -> Option<Remote> {
        let (uses, native) = match uses.strip_prefix("kiln://") {
            Some(rest) => (rest, true),
            None => (uses, false),
        };
        if uses.starts_with("./") || uses.starts_with("$/") || uses.starts_with("docker://") || (!native && uses.starts_with("kiln/")) {
            return None;
        }
        let (name, git_ref) = uses.split_once('@')?;
        let mut parts = name.splitn(3, '/');
        let (owner, repo) = (parts.next()?, parts.next()?);
        if owner.is_empty() || repo.is_empty() || git_ref.is_empty() {
            return None;
        }
        let safe = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) && !s.starts_with('.');
        if !safe(owner) || !safe(repo) || git_ref.starts_with('-') || git_ref.contains("..") {
            return None;
        }
        Some(Remote { owner: owner.into(), repo: repo.into(), path: parts.next().unwrap_or_default().trim_matches('/').into(), git_ref: git_ref.into(), native })
    }

    fn full(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    /// Its mirror: one of kiln's own repositories' apart from GitHub's of the same name.
    pub(crate) fn mirror(&self, hub: &Hub) -> PathBuf {
        if self.native { hub.cfg.data("actions").join("kiln").join(format!("{}__{}.git", self.owner, self.repo)) } else { mirror(hub, &self.owner, &self.repo) }
    }

    /// Where a leg downloads it at `sha`.
    fn url(&self, sha: &str) -> String {
        format!("/_kiln/actions/{}{}/{}/{sha}.tar.gz", if self.native { "kiln/" } else { "" }, self.owner, self.repo)
    }

    /// Whether a workflow of `caller` may use it: one of kiln's own repositories is its owner's
    /// own repositories' to use, as GitHub's private actions are their owner's (W07-R01).
    pub(crate) fn usable_by(&self, caller: &str) -> Result<(), String> {
        let theirs = caller.split('/').next().unwrap_or_default();
        if self.native && !self.owner.eq_ignore_ascii_case(theirs) {
            return Err(format!("`kiln://{}` is another owner's repository: {caller}'s workflows use `kiln://` actions of `{theirs}/`'s repositories", self.full()));
        }
        Ok(())
    }
}

pub(crate) fn mirror(hub: &Hub, owner: &str, repo: &str) -> PathBuf {
    hub.cfg.data("actions").join(format!("{owner}__{repo}.git"))
}

/// Where a tarball was kept before the store kept them: it moves in when it's first asked for.
fn tarball_path(hub: &Hub, owner: &str, repo: &str, sha: &str) -> PathBuf {
    hub.cfg.data("actions").join("tar").join(format!("{owner}__{repo}")).join(format!("{sha}.tar.gz"))
}

/// A `kiln://` action's tarball, apart from GitHub's of the same name (as `tarball_path`).
fn native_tarball_path(hub: &Hub, owner: &str, repo: &str, sha: &str) -> PathBuf {
    hub.cfg.data("actions").join("tar").join("kiln").join(format!("{owner}__{repo}")).join(format!("{sha}.tar.gz"))
}

/// The store's object for a tarball's key, when it holds one.
async fn stored_tarball(hub: &Hub, key: &str) -> anyhow::Result<Option<super::services::cas::Object>> {
    let k = key.to_owned();
    let digest: Option<String> = hub.store.call(move |c| Ok(c.query_row("SELECT object FROM ci_action_tarballs WHERE key = ?1", [&k], |r| r.get(0)).optional()?)).await?;
    match digest {
        Some(d) => super::services::cas::object(hub, &d).await,
        None => Ok(None),
    }
}

/// `file` into the store as the tarball of `key`.
async fn keep_tarball(hub: &Hub, key: &str, file: &std::path::Path) -> anyhow::Result<super::services::cas::Object> {
    let obj = super::services::cas::put_file(hub, file).await?;
    let (k, d) = (key.to_owned(), obj.digest.clone());
    hub.store
        .call(move |c| {
            c.execute("INSERT INTO ci_action_tarballs (key, object, created_at) VALUES (?1, ?2, ?3) ON CONFLICT (key) DO UPDATE SET object = excluded.object", rusqlite::params![k, d, crate::util::now_unix()])?;
            Ok(())
        })
        .await?;
    Ok(obj)
}

fn steps_uses(steps: &[Step], out: &mut Vec<String>) {
    for s in steps {
        if let Some(u) = &s.uses {
            out.push(u.clone());
        }
        steps_uses(&s.parallel, out);
    }
}

/// The policies a run of `repo` answers to: the server's, then the repository's (its
/// `.kiln/config.yaml` on the default branch, and one set through the API).
pub async fn policies(hub: &Hub, repo: &Repo) -> Vec<Policy> {
    let mut out = vec![hub.cfg.ci.actions.policy()];
    if let Some(p) = super::repo_config::load(hub, repo).await.actions {
        out.push(p);
    }
    let id = repo.id.clone();
    let own: Option<String> = hub.store.call(move |c| Ok(c.query_row("SELECT policy FROM ci_action_policies WHERE repo_id = ?1", [&id], |r| r.get(0)).optional()?)).await.unwrap_or_default();
    if let Some(p) = own.and_then(|t| serde_json::from_str::<Policy>(&t).ok()) {
        out.push(p);
    }
    out
}

/// What the policies refuse among `uses`, without fetching anything: for the plan preview.
pub fn refusals(policies: &[Policy], uses: impl IntoIterator<Item = String>) -> Vec<String> {
    let refs: Vec<&Policy> = policies.iter().collect();
    uses.into_iter().filter_map(|u| kiln_ci::policy::check(&refs, &u).err()).map(|r| format!("{} {}", r.code, r.message)).collect()
}

/// The `uses:` of a workflow: its jobs' calls of reusable workflows, and their steps' actions.
pub fn uses_of(wf: &Workflow) -> Vec<String> {
    let mut out: Vec<String> = wf.jobs.iter().filter_map(|j| j.uses.clone()).collect();
    for j in &wf.jobs {
        steps_uses(&j.steps, &mut out);
    }
    out
}

/// Every remote action a run of `wf` at `sha` may use, resolved: by `uses`, the commit and where
/// to download it. That covers the actions of the reusable workflows it calls (through their
/// calls, as deep as GitHub allows) and of composite actions. Each call and action is checked
/// against the action policies first.
pub async fn resolve(hub: &Hub, repo: &Repo, sha: &str, wf: &Workflow) -> Result<BTreeMap<String, ActionRef>, String> {
    let policies = policies(hub, repo).await;
    let policy: Vec<&Policy> = policies.iter().collect();
    let allowed = |u: &str| kiln_ci::policy::check(&policy, u).map_err(|r| format!("{} {}", r.code, r.message));
    // The workflows the run can reach: `wf`, then what it calls.
    let called = super::reusable::files(hub, repo, sha, wf).await;
    let mut reached: Vec<&Workflow> = vec![wf];
    let mut calls: Vec<(String, usize)> = wf.jobs.iter().filter_map(|j| j.uses.clone()).map(|u| (u, 0)).collect();
    let mut seen = std::collections::BTreeSet::new();
    while let Some((u, depth)) = calls.pop() {
        if depth >= 10 || !seen.insert(u.clone()) {
            continue;
        }
        allowed(&u)?;
        let key = u.strip_prefix("./").unwrap_or(&u);
        if let Some(w) = called.get(&u).or_else(|| called.get(key)) {
            calls.extend(w.jobs.iter().filter_map(|j| j.uses.clone()).map(|x| (x, depth + 1)));
            reached.push(w);
        }
    }
    // Each action, its depth, and the job's own step that reached it (a composite's actions are
    // its step's).
    let mut todo: Vec<(String, usize, Option<String>)> = Vec::new();
    for w in &reached {
        for j in &w.jobs {
            let mut u = Vec::new();
            steps_uses(&j.steps, &mut u);
            todo.extend(u.into_iter().map(|x| (x, 0, None)));
        }
    }
    let mut out: BTreeMap<String, ActionRef> = BTreeMap::new();
    let mut shas: BTreeMap<(String, String), Result<String, String>> = BTreeMap::new();
    // Actions that couldn't be resolved, under a step's action that uses them: that step's action
    // can't run either.
    let mut beneath: Vec<(String, String)> = Vec::new();
    // Composite actions with a Docker action or image beneath them: they need Docker too.
    let mut docker_beneath: Vec<String> = Vec::new();
    while let Some((uses, depth, root)) = todo.pop() {
        let below = root.clone().unwrap_or_else(|| uses.clone());
        if let Some(a) = out.get(&uses) {
            if let (Some(why), Some(root)) = (&a.error, &root) {
                beneath.push((root.clone(), format!("`{uses}`: {why}")));
            }
            if let (true, Some(root)) = (a.docker, &root) {
                docker_beneath.push(root.clone());
            }
            continue;
        }
        // kiln's own actions (W07-R10) run on the runner itself: nothing to fetch or pin, and no
        // repository by that name is ever asked for.
        if crate::runner::builtins::is_kiln(&uses) {
            if crate::runner::builtins::builtin(&uses).is_none() {
                let names: Vec<String> = crate::runner::builtins::BUILTINS.iter().map(|b| format!("kiln/{}@v1", b.name)).collect();
                return Err(format!("`{uses}` isn't one of kiln's built-in actions: {}", names.join(", ")));
            }
            continue;
        }
        allowed(&uses)?;
        // `$/path`: this repository at the running commit (W07-R01), read from the server's
        // mirror, so no checkout is needed.
        if let Some(p) = uses.strip_prefix("$/") {
            let path = p.trim_matches('/').to_owned();
            if path.split('/').any(|s| s == ".." || s == ".git") {
                return Err(format!("`{uses}`: the path must stay inside the repository"));
            }
            let url = format!("/_kiln/actions/{}/{sha}.tar.gz?path={}", repo.name, encode_path(&path));
            let mut a = ActionRef { repo: repo.name.clone(), sha: sha.to_owned(), path: path.clone(), url, ..ActionRef::default() };
            if let Some(meta) = own_metadata(hub, repo, sha, &path).await
                && let (Some(action), _) = kiln_ci::wf::parse_action(&meta)
            {
                // A kiln action's declarations (W07-R11, W09-R07).
                a.declares(&action);
                if let (true, Some(root)) = (a.docker, &root) {
                    docker_beneath.push(root.clone());
                }
                if action.runs.using == "composite" {
                    let mut inner = Vec::new();
                    steps_uses(&action.runs.steps, &mut inner);
                    todo.extend(inner.into_iter().map(|x| (x, depth + 1, Some(below.clone()))));
                }
            }
            out.insert(uses.clone(), a);
            continue;
        }
        if uses.starts_with("docker://") {
            if let Some(root) = &root {
                docker_beneath.push(root.clone());
            }
            continue;
        }
        let Some(r) = Remote::parse(&uses) else { continue };
        if depth > 9 {
            return Err(format!("`{uses}`: composite actions nest at most 10 deep"));
        }
        let key = (format!("{}{}", if r.native { "kiln://" } else { "" }, r.full()), r.git_ref.clone());
        let pinned = match shas.get(&key) {
            Some(s) => s.clone(),
            None => {
                let s = match r.usable_by(&repo.name) {
                    Ok(()) => pin(hub, &r).await.map_err(|e| format!("{e:#}")),
                    Err(why) => Err(why),
                };
                shas.insert(key, s.clone());
                s
            }
        };
        // One that can't be resolved fails the jobs that use it, not the run (W07-R03).
        let sha = match pinned {
            Ok(sha) => sha,
            Err(why) => {
                if let Some(root) = &root {
                    beneath.push((root.clone(), format!("`{uses}`: {why}")));
                }
                out.insert(uses.clone(), ActionRef { repo: r.full(), path: r.path.clone(), error: Some(why), ..ActionRef::default() });
                continue;
            }
        };
        let mut a = ActionRef { repo: r.full(), sha: sha.clone(), path: r.path.clone(), url: r.url(&sha), ..ActionRef::default() };
        if let Some(meta) = metadata(hub, &r, &sha).await
            && let (Some(action), _) = kiln_ci::wf::parse_action(&meta)
        {
            a.declares(&action);
            if let (true, Some(root)) = (a.docker, &root) {
                docker_beneath.push(root.clone());
            }
            // A composite action's own actions are the run's too.
            if action.runs.using == "composite" {
                let mut inner = Vec::new();
                steps_uses(&action.runs.steps, &mut inner);
                todo.extend(inner.into_iter().map(|x| (x, depth + 1, Some(below.clone()))));
            }
        }
        out.insert(uses.clone(), a);
    }
    for root in docker_beneath {
        if let Some(a) = out.get_mut(&root) {
            a.docker = true;
        }
    }
    for (root, why) in beneath {
        if let Some(a) = out.get_mut(&root)
            && a.error.is_none()
        {
            a.error = Some(format!("an action it uses could not be resolved, {why}"));
        }
    }
    Ok(out)
}

/// Where `owner/repo` is fetched from (W07-R02): the first `[[ci.actions.sources]]` that names
/// its owner (another host, or one of kiln's native repositories), else GitHub.
pub(crate) fn source(hub: &Hub, owner: &str, repo: &str) -> anyhow::Result<String> {
    let Some(s) = hub.cfg.ci.actions.source_of(owner) else { return Ok(format!("https://github.com/{owner}/{repo}.git")) };
    match (&s.url, s.native) {
        (Some(url), false) if url.contains("{repo}") => Ok(url.replace("{owner}", owner).replace("{repo}", repo)),
        (None, true) => super::hosting::path_of(hub, &format!("{owner}/{repo}"))
            .filter(|p| p.join("HEAD").exists())
            .map(|p| p.to_string_lossy().into_owned())
            .ok_or_else(|| anyhow::anyhow!("`{owner}/{repo}` is no repository kiln hosts (`[[ci.actions.sources]]` takes `{owner}`'s actions from kiln's own)")),
        _ => anyhow::bail!("`[[ci.actions.sources]]` for `{owner}` needs `url` (with `{{repo}}` in it) or `native = true`, and not both"),
    }
}

/// A `kiln://` action's repository: kiln's own.
fn native_source(hub: &Hub, owner: &str, repo: &str) -> anyhow::Result<String> {
    super::hosting::path_of(hub, &format!("{owner}/{repo}"))
        .filter(|p| p.join("HEAD").exists())
        .map(|p| p.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow::anyhow!("`kiln://{owner}/{repo}` is no repository kiln hosts"))
}

/// The commit `ref` names now, fetched into the mirror (W07-R02, W07-R03): a tag (peeled), a
/// branch, or a full SHA. When the source can't be reached, a ref the mirror knows still works.
pub(crate) async fn pin(hub: &Hub, r: &Remote) -> anyhow::Result<String> {
    let dir = r.mirror(hub);
    let d = dir.to_string_lossy().to_string();
    let url = if r.native { native_source(hub, &r.owner, &r.repo)? } else { source(hub, &r.owner, &r.repo)? };
    if dir.join("HEAD").exists() {
        // The source may have moved (`[[ci.actions.sources]]` changed): fetch from where it is now.
        let _ = gitops::run_git(hub, &["-C", &d, "remote", "set-url", "origin", &url], GIT_TIMEOUT).await;
    } else {
        if let Some(parent) = dir.parent() {
            let p = parent.to_path_buf();
            crate::rt::blocking(move || std::fs::create_dir_all(p)).await??;
        }
        // Treeless: every commit, and a commit's trees and files only when one is read (its
        // `action.yml`, its tarball). An action repository that keeps its built code in every
        // commit (codeql-action) mirrors in seconds, not minutes.
        if let Err(e) = gitops::run_git(hub, &["clone", "--bare", "--quiet", "--filter=tree:0", &url, &d], GIT_TIMEOUT).await {
            // What a failed clone leaves isn't a mirror.
            let _ = crate::rt::blocking(move || std::fs::remove_dir_all(dir)).await;
            anyhow::bail!("action source unreachable and not mirrored: {e:#}");
        }
    }
    let full_sha = r.git_ref.len() == 40 && r.git_ref.bytes().all(|b| b.is_ascii_hexdigit());
    let fetched = if full_sha {
        gitops::run_git(hub, &["-C", &d, "fetch", "--quiet", "origin", &r.git_ref], GIT_TIMEOUT).await.or(Ok::<_, anyhow::Error>(Vec::new())).map(|_| ())
    } else {
        gitops::run_git(hub, &["-C", &d, "fetch", "--quiet", "--force", "--prune", "origin", "+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"], GIT_TIMEOUT).await.map(|_| ())
    };
    if let Err(e) = fetched {
        tracing::warn!(action = %r.full(), error = %format!("{e:#}"), "action source unreachable; using the mirror");
    }
    for candidate in [format!("refs/tags/{}^{{commit}}", r.git_ref), format!("refs/heads/{}^{{commit}}", r.git_ref), format!("{}^{{commit}}", r.git_ref)] {
        if let Ok(o) = gitops::run_git(hub, &["-C", &d, "rev-parse", "--verify", "--quiet", &candidate], GIT_TIMEOUT).await {
            let sha = String::from_utf8_lossy(&o).trim().to_owned();
            if sha.len() == 40 {
                return Ok(sha);
            }
        }
    }
    anyhow::bail!("no tag, branch or commit `{}` in {} (or the action source is unreachable and it was never mirrored)", r.git_ref, r.full())
}

/// `path`, percent-encoded for a query (its `/` kept).
fn encode_path(path: &str) -> String {
    path.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// A `$/` action's `action.yml` (or `.yaml`), from the repository's own mirror.
async fn own_metadata(hub: &Hub, repo: &Repo, sha: &str, path: &str) -> Option<String> {
    let d = gitops::mirror_path(hub, &repo.name).to_string_lossy().to_string();
    let base = if path.is_empty() { String::new() } else { format!("{path}/") };
    for name in ["action.yml", "action.yaml"] {
        if let Ok(o) = gitops::run_git(hub, &["-C", &d, "show", &format!("{sha}:{base}{name}")], GIT_TIMEOUT).await {
            return Some(String::from_utf8_lossy(&o).into_owned());
        }
    }
    None
}

/// `action.yml` (or `.yaml`) of an action at a commit.
async fn metadata(hub: &Hub, r: &Remote, sha: &str) -> Option<String> {
    let d = r.mirror(hub).to_string_lossy().to_string();
    let base = if r.path.is_empty() { String::new() } else { format!("{}/", r.path) };
    for name in ["action.yml", "action.yaml"] {
        if let Ok(o) = gitops::run_git(hub, &["-C", &d, "show", &format!("{sha}:{base}{name}")], GIT_TIMEOUT).await {
            return Some(String::from_utf8_lossy(&o).into_owned());
        }
    }
    None
}

/// `GET /_kiln/actions/<owner>/<repo>/<sha>.tar.gz`, for a leg's token (W07-R03). With
/// `?path=`, a `$/` action: that directory of the leg's own repository at the commit, from the
/// repository's mirror (W07-R01). Tarballs are made once, by `git archive`, and kept in the store
/// (W08), where versions of an action share their chunks.
pub async fn tarball(hub: &Arc<Hub>, rest: &[&str], req: &Request) -> Result<Response, Fail> {
    // `kiln/<owner>/<repo>/<sha>.tar.gz`: a `kiln://` action, one of kiln's own repositories.
    let (native, rest) = match rest {
        ["kiln", more @ ..] if more.len() == 3 => (true, more),
        _ => (false, rest),
    };
    let [owner, repo, file] = rest else { return Err((StatusCode::NOT_FOUND, "no such action".into())) };
    let sha = file.trim_end_matches(".tar.gz");
    let ok_name = |s: &str| !s.is_empty() && !s.starts_with('.') && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    if !ok_name(owner) || !ok_name(repo) || sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err((StatusCode::NOT_FOUND, "no such action".into()));
    }
    let Some(leg) = token::from_headers(req.headers()).and_then(|t| token::verify(hub, &t)) else {
        return Err((StatusCode::UNAUTHORIZED, "a leg's token is needed".into()));
    };
    let own = srv::query(req).get("path").cloned().filter(|_| !native);
    if native {
        // Its owner's repositories' legs only.
        let mine = hub.store.repos().await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?.into_iter().find(|r| r.id == leg.repo);
        let r = Remote { owner: (*owner).to_owned(), repo: (*repo).to_owned(), path: String::new(), git_ref: String::new(), native: true };
        if mine.is_none_or(|m| r.usable_by(&m.name).is_err()) {
            return Err((StatusCode::FORBIDDEN, "a `kiln://` action is its owner's repositories' to use".into()));
        }
    }
    let (path, key, mirror_dir, args) = match &own {
        Some(p) => {
            if p.split('/').any(|s| s == ".." || s == ".git") {
                return Err((StatusCode::NOT_FOUND, "no such action".into()));
            }
            // Only the leg's own repository.
            let mine = hub.store.repos().await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?.into_iter().find(|r| r.id == leg.repo);
            let name = format!("{owner}/{repo}");
            let Some(mine) = mine.filter(|r| r.name == name) else { return Err((StatusCode::FORBIDDEN, "a `$/` action is the leg's own repository's".into())) };
            let digest = crate::util::sha256_hex(p.as_bytes());
            let file = tarball_path(hub, owner, repo, &format!("{sha}-{}", digest.get(..16).unwrap_or(&digest)));
            let p = p.trim_matches('/').to_owned();
            let args = if p.is_empty() { vec![sha.to_owned()] } else { vec![sha.to_owned(), "--".to_owned(), p] };
            (file, format!("own/{owner}__{repo}/{sha}-{}", digest.get(..16).unwrap_or(&digest)), gitops::mirror_path(hub, &mine.name), args)
        }
        None if native => (
            native_tarball_path(hub, owner, repo, sha),
            format!("kiln/{owner}__{repo}/{sha}"),
            Remote { owner: (*owner).to_owned(), repo: (*repo).to_owned(), path: String::new(), git_ref: String::new(), native: true }.mirror(hub),
            vec![sha.to_owned()],
        ),
        None => (tarball_path(hub, owner, repo, sha), format!("gh/{owner}__{repo}/{sha}"), mirror(hub, owner, repo), vec![sha.to_owned()]),
    };
    let internal = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    let obj = match stored_tarball(hub, &key).await.map_err(internal)? {
        Some(o) => o,
        // One kept before the store held them moves in; else git makes it.
        None if path.is_file() => {
            let o = keep_tarball(hub, &key, &path).await.map_err(internal)?;
            let _ = std::fs::remove_file(&path);
            o
        }
        None => {
            let d = mirror_dir.to_string_lossy().to_string();
            let mut git = vec!["-C", &d, "archive", "--format=tar.gz"];
            git.extend(args.iter().map(String::as_str));
            let out = gitops::run_git(hub, &git, GIT_TIMEOUT).await.map_err(|e| (StatusCode::NOT_FOUND, format!("action not mirrored: {e:#}")))?;
            let staging = super::services::cas::staging(hub);
            let tmp = staging.join(format!("action-{}.tar.gz", crate::util::new_id()));
            let t = tmp.clone();
            crate::rt::blocking(move || -> std::io::Result<()> {
                std::fs::create_dir_all(&staging)?;
                std::fs::write(&t, out)
            })
            .await
            .map_err(internal)?
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            let kept = keep_tarball(hub, &key, &tmp).await;
            let _ = std::fs::remove_file(&tmp);
            kept.map_err(internal)?
        }
    };
    let rx = super::services::cas::read(hub, &obj, 0, obj.size).await;
    Ok(srv::response(StatusCode::OK, "application/gzip", srv::channel_body(rx)))
}

/// `/api/ci/actions…` (W07-R04, W07-R12), behind the console's auth:
/// - `GET /api/ci/actions?q=`: the mirrored actions, with versions and `action.yml` inputs and
///   outputs (the editor's completion);
/// - `GET /api/ci/actions/policy[?repo=]`: the server's policy and a repository's;
/// - `PUT /api/ci/actions/policy?repo=` `{allow, deny, require_sha_pins}`, `DELETE` to drop it;
/// - `POST /api/ci/actions/sync`: mirror `[ci.actions] prefetch` now.
pub async fn route(hub: &Hub, method: &http::Method, rest: &[&str], q: &std::collections::HashMap<String, String>, body: Value) -> Result<Response, Fail> {
    let fail = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    match (method, rest) {
        (&http::Method::GET, []) => {
            // The built-ins first, then what the mirrors hold (W07-R12).
            let want = q.get("q").map(|s| s.to_ascii_lowercase()).unwrap_or_default();
            let mut list: Vec<Value> = crate::runner::builtins::catalog().as_array().cloned().unwrap_or_default().into_iter().filter(|b| b.get("uses").and_then(Value::as_str).is_some_and(|u| u.contains(&want))).collect();
            list.extend(catalog(hub, q.get("q").map(String::as_str).unwrap_or_default()).await);
            Ok(srv::json(StatusCode::OK, &json!({ "actions": list })))
        }
        // W07-R10: the built-in actions' metadata.
        (&http::Method::GET, ["kiln"]) => Ok(srv::json(StatusCode::OK, &json!({ "actions": crate::runner::builtins::catalog() }))),
        (&http::Method::POST, ["sync"]) => {
            let mut done = Vec::new();
            for uses in &hub.cfg.ci.actions.prefetch {
                let r = match Remote::parse(uses) {
                    Some(r) => pin(hub, &r).await.map(|sha| json!({ "uses": uses, "sha": sha })).unwrap_or_else(|e| json!({ "uses": uses, "error": format!("{e:#}") })),
                    None => json!({ "uses": uses, "error": "not an `owner/repo[/path]@ref`" }),
                };
                done.push(r);
            }
            hub.store.audit("admin".into(), "ci.actions.sync".into(), format!("{} actions mirrored", done.len())).await;
            Ok(srv::json(StatusCode::OK, &json!({ "synced": done })))
        }
        (_, ["policy"]) => {
            let server = hub.cfg.ci.actions.policy();
            let Some(name) = q.get("repo") else {
                return match *method {
                    http::Method::GET => Ok(srv::json(StatusCode::OK, &json!({ "server": server }))),
                    _ => Err((StatusCode::BAD_REQUEST, "the server's policy is `[ci.actions]` in its config; name a `repo` to set one's".into())),
                };
            };
            let repo = hub.store.repos().await.map_err(fail)?.into_iter().find(|r| r.name.eq_ignore_ascii_case(name)).ok_or_else(|| (StatusCode::NOT_FOUND, format!("No repository {name}")))?;
            let id = repo.id.clone();
            match *method {
                http::Method::GET => {
                    let file = super::repo_config::load(hub, &repo).await.actions;
                    let rid = repo.id.clone();
                    let set: Option<String> = hub.store.call(move |c| Ok(c.query_row("SELECT policy FROM ci_action_policies WHERE repo_id = ?1", [&rid], |r| r.get(0)).optional()?)).await.map_err(fail)?;
                    let own = set.and_then(|t| serde_json::from_str::<Policy>(&t).ok());
                    Ok(srv::json(StatusCode::OK, &json!({ "server": server, "repository": own, "config_yaml": file })))
                }
                http::Method::PUT => {
                    let p: Policy = serde_json::from_value(body).map_err(|e| (StatusCode::BAD_REQUEST, format!("not a policy ({{allow, deny, require_sha_pins}}): {e}")))?;
                    let text = serde_json::to_string(&p).map_err(|e| fail(e.into()))?;
                    hub.store.call(move |c| Ok(c.execute("INSERT INTO ci_action_policies (repo_id, policy, updated_at, updated_by) VALUES (?1, ?2, ?3, 'admin') ON CONFLICT(repo_id) DO UPDATE SET policy = excluded.policy, updated_at = excluded.updated_at, updated_by = excluded.updated_by", params![id, text, crate::util::now_unix()]).map(|_| ())?)).await.map_err(fail)?;
                    hub.store.audit("admin".into(), "ci.actions.policy".into(), format!("{}: {}", repo.name, serde_json::to_string(&p).unwrap_or_default())).await;
                    Ok(srv::json(StatusCode::OK, &json!({ "repository": p })))
                }
                http::Method::DELETE => {
                    hub.store.call(move |c| Ok(c.execute("DELETE FROM ci_action_policies WHERE repo_id = ?1", [&id]).map(|_| ())?)).await.map_err(fail)?;
                    hub.store.audit("admin".into(), "ci.actions.policy".into(), format!("{}: removed", repo.name)).await;
                    Ok(srv::json(StatusCode::OK, &json!({ "repository": Value::Null })))
                }
                _ => Err((StatusCode::METHOD_NOT_ALLOWED, "GET, PUT or DELETE".into())),
            }
        }
        _ => Err((StatusCode::NOT_FOUND, "no such route".into())),
    }
}

/// The mirrored actions whose name has `q`: each with its newest versions (tags, newest first,
/// with their commits) and the `action.yml` of the newest (W07-R12).
async fn catalog(hub: &Hub, q: &str) -> Vec<Value> {
    let dir = hub.cfg.data("actions");
    let q = q.to_ascii_lowercase();
    let names: Vec<(String, String)> = crate::rt::blocking(move || {
        let mut v: Vec<(String, String)> = std::fs::read_dir(dir)
            .map(|rd| rd.filter_map(Result::ok).filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".git")).and_then(|n| n.split_once("__")).map(|(o, r)| (o.to_owned(), r.to_owned()))).collect())
            .unwrap_or_default();
        v.sort();
        v
    })
    .await
    .unwrap_or_default();
    let mut out = Vec::new();
    for (owner, repo) in names.into_iter().filter(|(o, r)| q.is_empty() || format!("{o}/{r}").to_ascii_lowercase().contains(&q)).take(200) {
        let d = mirror(hub, &owner, &repo).to_string_lossy().to_string();
        let tags = gitops::run_git(hub, &["-C", &d, "for-each-ref", "--sort=-v:refname", "--count=20", "--format=%(refname:short) %(objectname) %(*objectname)", "refs/tags"], GIT_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).into_owned()).unwrap_or_default();
        let versions: Vec<Value> = tags
            .lines()
            .filter_map(|l| {
                let mut f = l.split_whitespace();
                let (tag, obj, peeled) = (f.next()?, f.next()?, f.next());
                Some(json!({ "ref": tag, "sha": peeled.unwrap_or(obj) }))
            })
            .collect();
        let newest = versions.first().and_then(|v| v.get("sha")).and_then(Value::as_str).map(str::to_owned);
        let meta = match newest {
            Some(sha) => metadata(hub, &Remote { owner: owner.clone(), repo: repo.clone(), path: String::new(), git_ref: sha.clone(), native: false }, &sha).await,
            None => None,
        };
        let action = meta.as_deref().and_then(|m| kiln_ci::wf::parse_action(m).0).map(|a| {
            json!({
                "name": a.name,
                "description": a.description,
                "using": a.runs.using,
                "inputs": a.inputs.iter().map(|i| json!({ "name": i.name, "description": i.description, "required": i.required, "default": i.default.as_ref().map(|d| d.src.clone()), "deprecated": i.deprecation_message })).collect::<Vec<_>>(),
                "outputs": a.outputs.iter().map(|o| json!({ "name": o.name, "description": o.description })).collect::<Vec<_>>(),
            })
        });
        out.push(json!({ "uses": format!("{owner}/{repo}"), "versions": versions, "action": action }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forms() {
        assert_eq!(Remote::parse("actions/checkout@v4"), Some(Remote { owner: "actions".into(), repo: "checkout".into(), path: String::new(), git_ref: "v4".into(), native: false }));
        assert_eq!(Remote::parse("github/codeql-action/init@v3").map(|r| r.path), Some("init".into()));
        for local in ["./.github/actions/x", "$/actions/y", "docker://alpine:3", "kiln/cache@v1", "nope", "a/b@", "../x/y@v1", "a/b@-x", "kiln://a/b@", "kiln://../x@v1"] {
            assert!(Remote::parse(local).is_none(), "{local}");
        }
    }

    /// W07-R01: `kiln://owner/repo[/path]@ref`, one of kiln's own repositories, its owner's
    /// repositories' to use, mirrored and served apart from GitHub's of the same name.
    #[test]
    fn w07_r01_kiln_forms() {
        let r = Remote::parse("kiln://acme/tools/lint@v2");
        assert_eq!(r, Some(Remote { owner: "acme".into(), repo: "tools".into(), path: "lint".into(), git_ref: "v2".into(), native: true }));
        let r = r.unwrap_or_else(|| Remote { owner: String::new(), repo: String::new(), path: String::new(), git_ref: String::new(), native: true });
        assert!(r.usable_by("acme/app").is_ok() && r.usable_by("ACME/web").is_ok());
        assert!(r.usable_by("other/app").is_err_and(|e| e.contains("another owner's")));
        assert_eq!(r.url(&"a".repeat(40)), format!("/_kiln/actions/kiln/acme/tools/{}.tar.gz", "a".repeat(40)));
        // GitHub's `acme/tools` is anyone's to use, and lives apart.
        let gh = Remote { native: false, ..r.clone() };
        assert!(gh.usable_by("other/app").is_ok());
        assert_eq!(gh.url(&"a".repeat(40)), format!("/_kiln/actions/acme/tools/{}.tar.gz", "a".repeat(40)));
    }
}
