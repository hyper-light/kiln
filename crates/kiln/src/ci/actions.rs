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
        if !plain_name(owner) || !plain_name(repo) || git_ref.starts_with('-') || git_ref.contains("..") {
            return None;
        }
        Some(Remote { owner: owner.into(), repo: repo.into(), path: parts.next().unwrap_or_default().trim_matches('/').into(), git_ref: git_ref.into(), native })
    }

    pub(crate) fn full(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    /// Its repository's `path` at `sha`, as a `uses:` names it.
    pub(crate) fn at(&self, path: &str, sha: &str) -> String {
        format!("{}{}/{}@{sha}", if self.native { "kiln://" } else { "" }, self.full(), path.trim_start_matches('/'))
    }

    /// Its mirror: one of kiln's own repositories' apart from GitHub's of the same name.
    pub(crate) fn mirror(&self, hub: &Hub) -> PathBuf {
        if self.native { hub.cfg.data("actions").join("kiln").join(format!("{}__{}.git", self.owner, self.repo)) } else { mirror(hub, &self.owner, &self.repo) }
    }

    /// Where a leg downloads it at `sha`.
    pub(crate) fn url(&self, sha: &str) -> String {
        format!("/_kiln/actions/{}{}/{}/{sha}.tar.gz", if self.native { "kiln/" } else { "" }, self.owner, self.repo)
    }

    /// Its name without its ref: `kiln://owner/repo/path`, `owner/repo/path`.
    pub(crate) fn named(&self) -> String {
        let path = if self.path.is_empty() { String::new() } else { format!("/{}", self.path) };
        format!("{}{}{path}", if self.native { "kiln://" } else { "" }, self.full())
    }

    /// Whether its name leads to one of kiln's own repositories: `kiln://`, or a plain name a
    /// native `[[ci.actions.sources]]` sends there (W29-R18).
    pub(crate) fn hosted(&self, hub: &Hub) -> bool {
        self.native || hub.cfg.ci.actions.source_of(&self.owner).is_some_and(|s| s.native)
    }

    /// Whether `sha`, the commit it pinned, is one only a draft's branch reaches of another of
    /// kiln's repositories (docs/ci.md W29-R10, kiln's addition): named by its commit (`@<sha>`,
    /// whole or abbreviated) and reached by no branch or tag but a draft's, it's the draft's, and
    /// refused as a draft's ref is (KCI4023). For an action and a reusable workflow alike, and
    /// wherever the name leads to kiln's own: `kiln://`, or a native source's plain name.
    pub(crate) async fn draft_only(&self, hub: &Hub, caller: &str, sha: &str) -> Result<(), String> {
        let by_commit = !self.git_ref.is_empty() && self.git_ref.bytes().all(|b| b.is_ascii_hexdigit()) && sha.to_ascii_lowercase().starts_with(&self.git_ref.to_ascii_lowercase());
        if by_commit && self.hosted(hub) && !self.full().eq_ignore_ascii_case(caller) && !published(hub, self, sha).await.unwrap_or(true) {
            return Err(format!("KCI4023 `{}@{}` names a commit only a draft reaches (a branch under {}), which is its author's until it's published", self.named(), self.git_ref, kiln_ci::drafts::DRAFT_PREFIX));
        }
        Ok(())
    }

    /// Whether workflows of repository `caller` may use it (docs/ci.md W29-R17, R19): one of
    /// kiln's own repositories as its Access says, for an action and a reusable workflow alike;
    /// GitHub's own are GitHub's to allow. A refusal is KCI4022's.
    pub(crate) async fn usable_by(&self, hub: &Hub, caller: &str) -> Result<(), String> {
        if !self.hosted(hub) {
            return Ok(());
        }
        let name = self.full();
        // One kiln doesn't host: pinning it says so.
        let Some(publisher) = hub.store.repos().await.map_err(|e| format!("{e:#}"))?.into_iter().find(|x| x.name.eq_ignore_ascii_case(&name)) else { return Ok(()) };
        let access = super::sharing::access_of(hub, &publisher).await;
        kiln_ci::sharing::allows(access, &publisher.name, caller).map_err(|why| format!("KCI4022 `{}`: {why}", self.named()))
    }
}

/// Whether `s` is an owner's or a repository's name as GitHub allows one: letters, digits, `-`,
/// `_` and `.`, a leading dot too (an owner's `.github`, where its shared actions and workflows
/// are), but never dots alone (`.`, `..`), which would name a directory where it's a path's.
pub(crate) fn plain_name(s: &str) -> bool {
    s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) && s.chars().any(|c| c != '.')
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
        if let Some(why) = kiln_ci::drafts::uses_of_another(&u, &repo.name) {
            return Err(format!("KCI4023 {why}"));
        }
        if let Some(r) = Remote::parse(&u) {
            r.usable_by(hub, &repo.name).await?;
        }
        let key = u.strip_prefix("./").unwrap_or(&u);
        if let Some(w) = called.get(&u).or_else(|| called.get(key)) {
            // The commit a remote one was pinned at, as an action's is (W29-R10).
            if let (Some(r), Some(o)) = (Remote::parse(&u), w.origin.as_ref()) {
                r.draft_only(hub, &repo.name, &o.sha).await?;
            }
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
        if let Some(why) = kiln_ci::drafts::uses_of_another(&uses, &repo.name) {
            return Err(format!("KCI4023 {why}"));
        }
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
        // `./path`: an action of the workspace, read from it as the step runs (W07-R01). A
        // composite one's own actions are pinned now, as a remote composite's are: read from this
        // repository at the running commit, where a checkout of it puts the action.
        if let Some(p) = uses.strip_prefix("./") {
            let path = p.trim_matches('/').to_owned();
            if !path.split('/').any(|s| s == ".." || s == ".git")
                && let Some(meta) = own_metadata(hub, repo, sha, &path).await
                && let (Some(action), _) = kiln_ci::wf::parse_action(&meta)
                && action.runs.using == "composite"
            {
                let mut inner = Vec::new();
                steps_uses(&action.runs.steps, &mut inner);
                todo.extend(inner.into_iter().map(|x| (x, depth + 1, Some(below.clone()))));
            }
            continue;
        }
        let Some(r) = Remote::parse(&uses) else { continue };
        if depth > 9 {
            return Err(format!("`{uses}`: composite actions nest at most 10 deep"));
        }
        r.usable_by(hub, &repo.name).await?;
        let key = (format!("{}{}", if r.native { "kiln://" } else { "" }, r.full()), r.git_ref.clone());
        let pinned = match shas.get(&key) {
            Some(s) => s.clone(),
            None => {
                let s = pin(hub, &r).await.map_err(|e| format!("{e:#}"));
                shas.insert(key, s.clone());
                s
            }
        };
        // A commit of another of kiln's repositories that only a draft's branch reaches is the
        // draft's (W29-R10, kiln's addition): refused, as a draft's ref is.
        if let Ok(sha) = &pinned {
            r.draft_only(hub, &repo.name, sha).await?;
        }
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

/// Whether a branch or tag of `r`'s repository other than a draft's (W29-R10) reaches `sha`, as
/// its mirror has them.
async fn published(hub: &Hub, r: &Remote, sha: &str) -> anyhow::Result<bool> {
    let d = r.mirror(hub).to_string_lossy().into_owned();
    let out = gitops::run_git(hub, &["-C", &d, "for-each-ref", "--contains", sha, "--format=%(refname)", "refs/heads", "refs/tags"], GIT_TIMEOUT).await?;
    Ok(String::from_utf8_lossy(&out).lines().any(|r| !r.is_empty() && !kiln_ci::drafts::is_draft_ref(r)))
}

/// The commit `ref` names now, fetched into the mirror (W07-R02, W07-R03): a tag (peeled), a
/// branch, or a full SHA. When the source can't be reached, a ref the mirror knows still works.
pub(crate) async fn pin(hub: &Hub, r: &Remote) -> anyhow::Result<String> {
    pin_ref(hub, r).await.map(|(sha, _)| sha)
}

/// A mirror's tags.
async fn mirror_tags(hub: &Hub, dir: &str) -> std::collections::HashSet<String> {
    gitops::run_git(hub, &["-C", dir, "for-each-ref", "--format=%(refname:short)", "refs/tags"], GIT_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).lines().map(str::to_owned).collect()).unwrap_or_default()
}

/// [`pin`], and the ref as GitHub writes it, which it was found as: `refs/tags/v1`,
/// `refs/heads/main`, or the commit's SHA.
pub(crate) async fn pin_ref(hub: &Hub, r: &Remote) -> anyhow::Result<(String, String)> {
    pin_ref_with(hub, r, true, true).await.map(|(sha, full, _)| (sha, full))
}

/// W29-R23: lock reads refresh mirrors without announcing releases or queuing updates. A
/// failed refresh can use the mirror but cannot claim the ref is current upstream evidence.
pub(crate) async fn pin_ref_readonly(hub: &Hub, r: &Remote, refresh: bool) -> anyhow::Result<(String, String, bool)> {
    pin_ref_with(hub, r, false, refresh).await
}

/// A fixed number of locks, shared by pinning and lock-file reads. Fetch cannot expose a
/// commit before its trees have arrived; hash collisions only serialize unrelated sources.
pub(crate) fn source_lock(r: &Remote) -> anyhow::Result<&'static async_lock::RwLock<()>> {
    static LOCKS: std::sync::OnceLock<[async_lock::RwLock<()>; 64]> = std::sync::OnceLock::new();
    let locks = LOCKS.get_or_init(|| std::array::from_fn(|_| async_lock::RwLock::new(())));
    let slot = r.full().bytes().fold(usize::from(r.native), |n, b| n.wrapping_mul(31).wrapping_add(usize::from(b))) % 64;
    locks.get(slot).ok_or_else(|| anyhow::anyhow!("action mirror lock is unavailable"))
}

/// W29-R23: the App's read credential, scoped to this repository's HTTP URL, stays inside
/// the adapter. Public actions outside its installation need no App credential. Git's
/// configured credentials for other hosts continue to come from gitops::git.
pub(crate) async fn source_command(hub: &Hub, r: &Remote) -> std::process::Command {
    let mut command = gitops::git(hub);
    command.env("GIT_NO_REPLACE_OBJECTS", "1");
    if !r.hosted(hub) && let Ok(url) = source(hub, &r.owner, &r.repo)
        && url.starts_with("https://") && let Some((host, name)) = crate::githost::split_url(&url)
        && (kiln_seams::dependencies::Repository { name: name.clone(), native: false }).validate().is_ok()
        && let Some(app) = crate::githost::github::app(hub).await.filter(|a| crate::githost::host_of(&a.web) == host)
        && let Ok(token) = app.token(&name, &[("contents".into(), "read".into()), ("metadata".into(), "read".into())].into()).await {
        use base64::Engine as _;
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
        command.args(["-c", &format!("http.{url}.extraheader=Authorization: Basic {basic}")]);
    }
    command
}

async fn pin_git(hub: &Hub, r: &Remote, observe: bool, args: &[&str], within: Duration) -> anyhow::Result<Vec<u8>> {
    if observe { return gitops::run_git(hub, args, within).await; }
    let mut command = source_command(hub, r).await;
    command.args(args);
    let out = crate::proc::output(command, within).await.map_err(|_| anyhow::anyhow!("dependency git source is unavailable"))?;
    anyhow::ensure!(out.status.success(), "dependency git source refused the read");
    Ok(out.stdout)
}

async fn pin_ref_with(hub: &Hub, r: &Remote, observe: bool, refresh: bool) -> anyhow::Result<(String, String, bool)> {
    let _guard = source_lock(r)?.write().await;
    let dir = r.mirror(hub);
    let d = dir.to_string_lossy().to_string();
    let url = if !refresh { None } else {
        match if r.native { native_source(hub, &r.owner, &r.repo) } else { source(hub, &r.owner, &r.repo) } {
            Ok(url) => Some(url),
            Err(_) if !observe && dir.join("HEAD").exists() => None,
            Err(e) => return Err(e),
        }
    };
    // The mirror's tags before it fetches: a new full version after is a release for consumers
    // (W29-R22). A first mirror's are all new, and no one's update.
    let before = if observe && dir.join("HEAD").exists() { Some(mirror_tags(hub, &d).await) } else { None };
    if dir.join("HEAD").exists() {
        // The source may have moved (`[[ci.actions.sources]]` changed): fetch from where it is now.
        if let Some(url) = &url { let _ = pin_git(hub, r, observe, &["-C", &d, "remote", "set-url", "origin", url], GIT_TIMEOUT).await; }
    } else {
        let url = url.as_deref().ok_or_else(|| anyhow::anyhow!("action source unreachable and not mirrored"))?;
        if let Some(parent) = dir.parent() {
            let p = parent.to_path_buf();
            crate::rt::blocking(move || std::fs::create_dir_all(p)).await??;
        }
        // Treeless: every commit, and a commit's trees and files only when one is read (its
        // `action.yml`, its tarball). An action repository that keeps its built code in every
        // commit (codeql-action) mirrors in seconds, not minutes.
        if let Err(e) = pin_git(hub, r, observe, &["clone", "--bare", "--quiet", "--filter=tree:0", url, &d], GIT_TIMEOUT).await {
            // What a failed clone leaves isn't a mirror.
            let _ = crate::rt::blocking(move || std::fs::remove_dir_all(dir)).await;
            anyhow::bail!("action source unreachable and not mirrored: {e:#}");
        }
    }
    let full_sha = matches!(r.git_ref.len(), 40 | 64) && r.git_ref.bytes().all(|b| b.is_ascii_hexdigit());
    let fetched = if url.is_none() { Err(anyhow::anyhow!("action source wasn't refreshed")) } else if full_sha {
        pin_git(hub, r, observe, &["-C", &d, "fetch", "--quiet", "origin", &r.git_ref], GIT_TIMEOUT).await.map(|_| ())
    } else {
        pin_git(hub, r, observe, &["-C", &d, "fetch", "--quiet", "--force", "--prune", "origin", "+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"], GIT_TIMEOUT).await.map(|_| ())
    };
    let fresh = fetched.is_ok();
    if let Err(e) = fetched {
        if observe { tracing::warn!(action = %r.full(), error = %format!("{e:#}"), "action source unreachable; using the mirror"); }
    } else if let Some(before) = before {
        for tag in mirror_tags(hub, &d).await.into_iter().filter(|t| !before.contains(t)) {
            if let Err(e) = super::updates::released(hub, &r.full(), &tag).await {
                tracing::warn!(action = %r.full(), %tag, error = %format!("{e:#}"), "a new release's updates weren't made due");
            }
        }
    }
    // W29-R23/R24: a qualified ref is exact. A tag literally named refs/heads/main must
    // never substitute its history for the branch a lock recorded.
    let mut choices = if full_sha { vec![r.git_ref.to_ascii_lowercase()] }
        else if r.git_ref.starts_with("refs/") { vec![r.git_ref.clone()] }
        else { vec![format!("refs/tags/{}", r.git_ref), format!("refs/heads/{}", r.git_ref), r.git_ref.clone()] };
    if !observe && r.git_ref == "HEAD" {
        // Refresh the default-branch alias too; fetch alone doesn't update a bare mirror's HEAD.
        if fresh && let Ok(out) = pin_git(hub, r, observe, &["-C", &d, "ls-remote", "--symref", "origin", "HEAD"], GIT_TIMEOUT).await
            && let Some(target) = String::from_utf8_lossy(&out).lines().find_map(|l| l.strip_prefix("ref: ").and_then(|s| s.strip_suffix("\tHEAD"))).filter(|s| s.starts_with("refs/heads/") && kiln_seams::dependencies::reference_valid(s)) {
            let _ = pin_git(hub, r, observe, &["-C", &d, "symbolic-ref", "HEAD", target], GIT_TIMEOUT).await;
        }
        if let Ok(out) = pin_git(hub, r, observe, &["-C", &d, "symbolic-ref", "--quiet", "HEAD"], GIT_TIMEOUT).await {
            let target = String::from_utf8_lossy(&out).trim().to_owned();
            if target.starts_with("refs/heads/") { choices = vec![target]; }
        }
    }
    for full in choices {
        if let Ok(o) = pin_git(hub, r, observe, &["-C", &d, "rev-parse", "--verify", "--quiet", &format!("{full}^{{commit}}")], GIT_TIMEOUT).await {
            let sha = String::from_utf8_lossy(&o).trim().to_owned();
            if full_sha && !sha.eq_ignore_ascii_case(&r.git_ref) { continue; }
            if matches!(sha.len(), 40 | 64) {
                // A ref given as a commit stays the commit, as GitHub writes it.
                let full = if full == r.git_ref && !full.starts_with("refs/") { sha.clone() } else { full };
                return Ok((sha, full, fresh));
            }
        }
    }
    anyhow::bail!("no tag, branch or commit `{}` in {} (or the action source is unreachable and it was never mirrored)", r.git_ref, r.full())
}

/// `path`, percent-encoded for a query (its `/` kept).
pub(crate) fn encode_path(path: &str) -> String {
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
    if !plain_name(owner) || !plain_name(repo) || sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err((StatusCode::NOT_FOUND, "no such action".into()));
    }
    let Some(leg) = token::from_headers(req.headers()).and_then(|t| token::verify(hub, &t)) else {
        return Err((StatusCode::UNAUTHORIZED, "a leg's token is needed".into()));
    };
    let own = srv::query(req).get("path").cloned().filter(|_| !native);
    let r = Remote { owner: (*owner).to_owned(), repo: (*repo).to_owned(), path: String::new(), git_ref: String::new(), native };
    if own.is_none() && r.hosted(hub) {
        // One of kiln's own repositories: the legs of the repositories its Access lets use it
        // only (W29-R17).
        let mine = hub.store.repos().await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?.into_iter().find(|x| x.id == leg.repo).map(|x| x.name).unwrap_or_default();
        r.usable_by(hub, &mine).await.map_err(|why| (StatusCode::FORBIDDEN, why))?;
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
    let rx = super::services::cas::read(hub, &obj, 0, obj.size);
    Ok(srv::response(StatusCode::OK, "application/gzip", srv::channel_body(rx)))
}

/// `/api/ci/actions…` (W07-R04, W07-R12), behind the console's auth:
/// - `GET /api/ci/actions?q=&kind=&owner=&repo=`: the built-ins, the mirrored actions, with
///   versions and `action.yml` inputs and outputs (the editor's completion), and what kiln's own
///   repositories share (W29-R20), those `repo`'s workflows may use with it;
/// - `GET /api/ci/actions/:owner/:repo?path=`: one's versions, access and consumers (W29-R20);
/// - `GET /api/ci/actions/policy[?repo=]`: the server's policy and a repository's;
/// - `PUT /api/ci/actions/policy?repo=` `{allow, deny, require_sha_pins}`, `DELETE` to drop it;
/// - `POST /api/ci/actions/sync`: mirror `[ci.actions] prefetch` now.
pub async fn route(hub: &Hub, method: &http::Method, rest: &[&str], q: &std::collections::HashMap<String, String>, body: Value) -> Result<Response, Fail> {
    let fail = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    match (method, rest) {
        (&http::Method::GET, []) => {
            let filter = kiln_ci::catalog::Filter { q: q.get("q").cloned().unwrap_or_default(), kind: q.get("kind").cloned().unwrap_or_default(), owner: q.get("owner").cloned().unwrap_or_default() };
            let (kind, owner) = (filter.kind.trim().to_ascii_lowercase(), filter.owner.trim().to_ascii_lowercase());
            // The built-ins first (kiln's own, of no kind), then what the mirrors hold (W07-R12),
            // then what kiln's own repositories share (W29-R20).
            let want = filter.q.to_ascii_lowercase();
            let mut list: Vec<Value> = if kind.is_empty() && (owner.is_empty() || owner == "kiln") {
                crate::runner::builtins::catalog().as_array().cloned().unwrap_or_default().into_iter().filter(|b| b.get("uses").and_then(Value::as_str).is_some_and(|u| u.contains(&want))).collect()
            } else {
                Vec::new()
            };
            let mirrored = catalog(hub, &filter.q).await.into_iter().filter(|e| {
                let of = e.get("uses").and_then(Value::as_str).and_then(|u| u.split('/').next()).unwrap_or_default().to_ascii_lowercase();
                let using = e.pointer("/action/using").and_then(Value::as_str).map(kiln_ci::catalog::kind_of);
                // A native owner's are kiln's own, listed as such below.
                !hub.cfg.ci.actions.source_of(&of).is_some_and(|s| s.native) && (owner.is_empty() || of == owner) && (kind.is_empty() || using.as_deref() == Some(kind.as_str()))
            });
            list.extend(mirrored);
            let caller = q.get("repo").map(String::as_str).filter(|r| !r.is_empty());
            list.extend(Box::pin(super::catalog::hosted(hub, &filter, caller)).await.iter().filter_map(|e| serde_json::to_value(e).ok()));
            Ok(srv::json(StatusCode::OK, &json!({ "actions": list })))
        }
        // W07-R10: the built-in actions' metadata.
        (&http::Method::GET, ["kiln"]) => Ok(srv::json(StatusCode::OK, &json!({ "actions": crate::runner::builtins::catalog() }))),
        (&http::Method::GET, [owner, repo]) if plain_name(owner) && plain_name(repo) => {
            let one = Box::pin(super::catalog::one(hub, owner, repo, q.get("path").map(String::as_str))).await?;
            Ok(srv::json(StatusCode::OK, &one))
        }
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

/// What's said of an action that signs only for github.com and GHE.com (docs/ci.md W18-R02):
/// `@actions/attest` accepts no other server (`GITHUB_SERVER_URL`) and checks the job's ID token
/// with `token.actions.<host>`'s keys, so on kiln it fails whatever Sigstore is set up. The
/// catalog notes it, and a leg running one is told.
pub fn github_only(repo: &str) -> Option<&'static str> {
    const NOTE: &str = "signs only on github.com and GHE.com: @actions/attest accepts no other server, and checks the job's ID token with token.actions.<host>'s keys. On kiln, kiln/attest@v1 attests the same subjects (subject-path, subject-name with subject-digest, subject-checksums), signed by kiln.";
    ["actions/attest", "actions/attest-build-provenance", "actions/attest-sbom"].iter().any(|r| r.eq_ignore_ascii_case(repo)).then_some(NOTE)
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
        let uses = format!("{owner}/{repo}");
        out.push(json!({ "note": github_only(&uses), "uses": uses, "versions": versions, "action": action }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W18-R02: the actions that sign only on GitHub are noted so; others aren't.
    #[test]
    fn w18_r02_actions_that_sign_only_on_github() {
        for repo in ["actions/attest", "actions/attest-build-provenance", "Actions/Attest-SBOM"] {
            assert!(github_only(repo).is_some_and(|n| n.contains("kiln/attest@v1")), "{repo}");
        }
        for repo in ["actions/checkout", "actions/attest-x", "sigstore/cosign-installer"] {
            assert_eq!(github_only(repo), None, "{repo}");
        }
    }

    #[test]
    fn forms() {
        assert_eq!(Remote::parse("actions/checkout@v4"), Some(Remote { owner: "actions".into(), repo: "checkout".into(), path: String::new(), git_ref: "v4".into(), native: false }));
        assert_eq!(Remote::parse("github/codeql-action/init@v3").map(|r| r.path), Some("init".into()));
        for local in
            ["./.github/actions/x", "$/actions/y", "docker://alpine:3", "kiln/cache@v1", "nope", "a/b@", "../x/y@v1", "a/b@-x", "kiln://a/b@", "kiln://../x@v1", "a/../x@v1", "a/./x@v1", "a/.../x@v1"]
        {
            assert!(Remote::parse(local).is_none(), "{local}");
        }
        // An owner's `.github`, where its repositories' shared actions and workflows are.
        let shared = Remote::parse("crazy-max/.github/.github/actions/gotest-annotations@f91220cba053bd54299ba645a599ee8ed436f222");
        assert_eq!(shared.as_ref().map(|r| (r.owner.as_str(), r.repo.as_str(), r.path.as_str())), Some(("crazy-max", ".github", ".github/actions/gotest-annotations")));
        assert_eq!(shared.map(|r| r.at(".github/workflows/x.yml", "abc")), Some("crazy-max/.github/.github/workflows/x.yml@abc".to_owned()));
        assert!(["dotfiles", ".github", "a.b-c_d"].iter().all(|n| plain_name(n)) && ["", ".", "..", "a/b", "a b"].iter().all(|n| !plain_name(n)));
    }

    /// W07-R01: `kiln://owner/repo[/path]@ref`, one of kiln's own repositories, mirrored and
    /// served apart from GitHub's of the same name; who may use it is its Access (W29-R17).
    /// `tests/sharing.rs` holds `usable_by` to that.
    #[test]
    fn w07_r01_kiln_forms() {
        let r = Remote::parse("kiln://acme/tools/lint@v2");
        assert_eq!(r, Some(Remote { owner: "acme".into(), repo: "tools".into(), path: "lint".into(), git_ref: "v2".into(), native: true }));
        let r = r.unwrap_or_else(|| Remote { owner: String::new(), repo: String::new(), path: String::new(), git_ref: String::new(), native: true });
        assert_eq!(r.full(), "acme/tools");
        assert_eq!(r.url(&"a".repeat(40)), format!("/_kiln/actions/kiln/acme/tools/{}.tar.gz", "a".repeat(40)));
        // GitHub's `acme/tools` lives apart.
        let gh = Remote { native: false, ..r.clone() };
        assert_eq!(gh.url(&"a".repeat(40)), format!("/_kiln/actions/acme/tools/{}.tar.gz", "a".repeat(40)));
    }
}
