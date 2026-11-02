//! W29-R23: preflight fixes Git dependencies before policy, metadata and planning. The accepted
//! snapshot is plain JSON in the run's existing input, reused through reruns and recovery.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use kiln_ci::lock::{github, verify};
use kiln_ci::wf::{self, Workflow};
use kiln_ci::wire::ActionRef;
use kiln_seams::DependencySources;
use kiln_seams::dependencies::{Identity, Repository, Revision};
use serde::{Deserialize, Serialize};
use rusqlite::OptionalExtension as _;

use super::actions::{self, Remote};
use crate::hub::Hub;
use crate::store::Repo;

type Actions = BTreeMap<String, ActionRef>;
pub type Files = HashMap<String, Arc<Workflow>>;
const MAX_ITEMS: usize = 4096;
const MAX_BYTES: usize = 16 << 20;
const MAX_JSON: usize = 64 << 20;
const WITHIN: Duration = Duration::from_secs(60);

/// Action pins are scoped to the workflow that owns their lock. Two called workflows may
/// deliberately lock the same `uses:` at different commits; each job gets its owner's map.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scope {
    pub repository: String,
    pub path: String,
    pub commit: String,
    pub locked: bool,
    pub actions: Actions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frozen {
    pub version: u32,
    pub repository: String,
    pub path: String,
    pub commit: String,
    pub root_locked: bool,
    pub scopes: BTreeMap<String, Scope>,
    pub files: Files,
    pub warnings: Vec<String>,
}

fn scope_key(repository: &str, path: &str, commit: &str) -> String {
    format!("{}/{path}@{commit}", repository.to_ascii_lowercase())
}

impl Frozen {
    fn bounded(value: &impl Serialize) -> Result<(), String> {
        struct Count(usize);
        impl std::io::Write for Count {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self.0.saturating_add(bytes.len());
                if self.0 > MAX_JSON { return Err(std::io::Error::other("dependency snapshot exceeds 64 MiB")); }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        serde_json::to_writer(Count(0), value).map_err(|_| "the dependency snapshot exceeds its JSON bound or cannot be encoded".into())
    }
    pub fn root_actions(&self) -> Result<&Actions, String> {
        self.scopes.get(&scope_key(&self.repository, &self.path, &self.commit)).map(|s| &s.actions).ok_or_else(|| "the saved dependency snapshot has no root workflow".into())
    }

    pub fn actions_for(&self, defined: &kiln_ci::plan::DefinedIn) -> Option<&Actions> {
        self.scopes.get(&scope_key(&defined.workflow_repository, &defined.workflow_file_path, &defined.workflow_sha)).map(|s| &s.actions)
    }

    pub fn workflow_for(&self, defined: &kiln_ci::plan::DefinedIn) -> Option<Arc<Workflow>> {
        self.files.values().find(|wf| {
            let (repo, sha) = wf.origin.as_ref().map(|o| (o.repository.as_str(), o.sha.as_str())).unwrap_or((&self.repository, &self.commit));
            repo.eq_ignore_ascii_case(&defined.workflow_repository) && sha == defined.workflow_sha && wf.path == defined.workflow_file_path
        }).cloned()
    }

    /// Absence is the old-run compatibility path. A present malformed snapshot is an error,
    /// never permission to fetch mutable refs again.
    pub fn saved(value: &serde_json::Value, repository: &str, path: &str, commit: &str) -> Result<Option<Self>, String> {
        let Some(raw) = value.get("dependency_snapshot").filter(|v| !v.is_null()) else { return Ok(None) };
        if value.get("dependency_preflight_failed").and_then(serde_json::Value::as_bool) == Some(true) { return Err("the saved run marks an accepted snapshot as failed".into()); }
        Self::bounded(raw)?;
        let snapshot: Self = serde_json::from_value(raw.clone()).map_err(|_| "the run's saved dependency snapshot doesn't read".to_owned())?;
        if snapshot.version != 1 || !snapshot.repository.eq_ignore_ascii_case(repository) || snapshot.path != path || snapshot.commit != commit {
            return Err("the run's saved dependency snapshot belongs to a different workflow, commit or version".into());
        }
        if snapshot.scopes.len() > MAX_ITEMS || snapshot.files.len() > MAX_ITEMS || snapshot.scopes.values().fold(0usize, |n, s| n.saturating_add(s.actions.len())) > MAX_ITEMS {
            return Err("the run's saved dependency snapshot exceeds its item bound".into());
        }
        snapshot.root_actions()?;
        if snapshot.scopes.get(&scope_key(repository, path, commit)).is_none_or(|s| s.locked != snapshot.root_locked) { return Err("the saved root lock state is inconsistent".into()); }
        for (key, scope) in &snapshot.scopes {
            if *key != scope_key(&scope.repository, &scope.path, &scope.commit) || !kiln_seams::dependencies::commit_valid(&scope.commit) || scope.actions.values().any(|a| a.error.is_none() && (!kiln_seams::dependencies::commit_valid(&a.sha) || !a.url.starts_with("/_kiln/actions/") || !a.url.contains(&format!("/{}.tar.gz", a.sha)))) {
                return Err("the run's saved dependency snapshot contains an invalid source pin".into());
            }
        }
        for wf in snapshot.files.values() {
            let (repo, sha) = wf.origin.as_ref().map(|o| (o.repository.as_str(), o.sha.as_str())).unwrap_or((repository, commit));
            if !snapshot.scopes.contains_key(&scope_key(repo, &wf.path, sha)) { return Err("the saved dependency snapshot omits a called workflow's source scope".into()); }
        }
        Ok(Some(snapshot))
    }
}

/// A bounded immutable blob. Absence is established by a successful tree listing; a missing
/// object or failed lazy fetch must not turn a present lock into an unlocked workflow.
async fn blob(hub: &Hub, dir: &Path, remote: Option<&Remote>, commit: &str, path: &str) -> Result<Option<Vec<u8>>, String> {
    if !kiln_seams::dependencies::commit_valid(commit) || !kiln_seams::dependencies::path_valid(path) { return Err("KCI1022 invalid dependency file location".into()); }
    if !dir.join("HEAD").exists() { return Ok(None); }
    async fn run(hub: &Hub, dir: &Path, remote: Option<&Remote>, args: &[&str]) -> Result<Vec<u8>, String> {
        let mut command = match remote { Some(r) => actions::source_command(hub, r).await, None => crate::gitops::git(hub) };
        command.env("GIT_NO_REPLACE_OBJECTS", "1").arg("-C").arg(dir).args(args);
        let result = crate::proc::output(command, WITHIN).await.map_err(|_| "KCI4051 dependency mirror is unavailable".to_owned())?;
        if !result.status.success() { return Err("KCI4051 dependency file could not be read at its captured commit".into()); }
        Ok(result.stdout)
    }
    let listing = run(hub, dir, remote, &["ls-tree", "-z", commit, "--", path]).await?;
    if listing.is_empty() { return Ok(None); }
    let meta = listing.split(|b| *b == b'\t').next().and_then(|b| std::str::from_utf8(b).ok()).ok_or_else(|| "KCI1022 dependency tree entry doesn't read".to_owned())?;
    let mut parts = meta.split_whitespace();
    let (_, kind, object) = (parts.next(), parts.next(), parts.next());
    let object = object.filter(|s| kiln_seams::dependencies::commit_valid(s)).ok_or_else(|| "KCI1022 dependency tree names no object".to_owned())?;
    if kind != Some("blob") { return Err(format!("KCI1022 {path} is not a file")); }
    let size = run(hub, dir, remote, &["cat-file", "-s", object]).await?;
    let size: usize = String::from_utf8_lossy(&size).trim().parse().map_err(|_| "KCI1022 dependency file has an invalid size".to_owned())?;
    if size > kiln_seams::dependencies::FILE_MAX { return Err(format!("KCI1022 {path} exceeds 1 MiB")); }
    let bytes = run(hub, dir, remote, &["cat-file", "blob", object]).await?;
    if bytes.len() != size { return Err("KCI4051 dependency file differs from its immutable size".into()); }
    Ok(Some(bytes))
}

#[derive(Clone)]
struct Owner {
    repository: String,
    commit: String,
    remote: Option<Remote>,
}

impl Owner {
    async fn file(&self, hub: &Hub, path: &str) -> Result<Option<Vec<u8>>, String> {
        let dir = self.remote.as_ref().map(|r| r.mirror(hub)).unwrap_or_else(|| crate::gitops::mirror_path(hub, &self.repository));
        blob(hub, &dir, self.remote.as_ref(), &self.commit, path).await
    }

    fn home_host(&self, hub: &Hub, caller: &Repo) -> String {
        let url = self.remote.as_ref().and_then(|r| actions::source(hub, &r.owner, &r.repo).ok()).unwrap_or_else(|| caller.clone_url.clone());
        crate::githost::split_url(&url).map(|(h, _)| h).filter(|h| h == "github.com" || h.ends_with(".ghe.com")).unwrap_or_else(|| "github.com".into())
    }
}

struct Work {
    workflow: Arc<Workflow>,
    owner: Owner,
    depth: usize,
}

struct Capture<'a> {
    hub: &'a Hub,
    caller: &'a Repo,
    source: &'a dyn DependencySources,
    root_commit: &'a str,
    policies: Vec<kiln_ci::policy::Policy>,
    revisions: BTreeMap<String, Revision>,
    legacy: BTreeMap<String, Result<(String, String), String>>,
    origins: BTreeMap<String, Remote>,
    locks: BTreeMap<String, Option<Arc<github::File>>>,
    texts: BTreeMap<String, Option<Arc<str>>>,
    bytes: usize,
    items: usize,
    warnings: BTreeSet<String>,
}

impl Capture<'_> {
    fn ref_key(remote: &Remote, reference: &str) -> String {
        format!("{}{}@{reference}", if remote.native { "kiln://" } else { "" }, remote.full().to_ascii_lowercase())
    }

    fn origin(&self, remote: &Remote) -> Remote {
        let mut got = self.origins.get(&Self::ref_key(remote, &remote.git_ref)).cloned().unwrap_or_else(|| remote.clone());
        got.path = remote.path.clone();
        got.git_ref = remote.git_ref.clone();
        got
    }
    fn count(&mut self, bytes: usize) -> Result<(), String> {
        self.bytes = self.bytes.saturating_add(bytes);
        self.items = self.items.saturating_add(1);
        if self.bytes > MAX_BYTES || self.items > MAX_ITEMS { return Err("KCI1022 dependency snapshot exceeds 16 MiB or 4096 items".into()); }
        Ok(())
    }

    fn allowed(&self, uses: &str) -> Result<(), String> {
        let policies: Vec<_> = self.policies.iter().collect();
        kiln_ci::policy::check(&policies, uses).map_err(|r| format!("{} {}", r.code, r.message))
    }

    async fn lock(&mut self, owner: &Owner, path: &str) -> Result<Option<Arc<github::File>>, String> {
        if !path.starts_with(".github/workflows/") { return Ok(None); }
        let key = format!("{}:{}@{}", if owner.remote.is_some() { "remote" } else { "own" }, owner.repository, owner.commit);
        if let Some(cached) = self.locks.get(&key) { return Ok(cached.clone()); }
        let Some(bytes) = owner.file(self.hub, github::PATH).await? else { self.locks.insert(key, None); return Ok(None) };
        self.count(bytes.len())?;
        let text = String::from_utf8(bytes).map_err(|_| format!("KCI1022 {}:{} is not UTF-8", owner.repository, github::PATH))?;
        let file = github::parse(&text).map_err(|ds| ds.iter().map(|d| format!("{}: {}", owner.repository, d.render(github::PATH))).collect::<Vec<_>>().join("\n"))?;
        let file = Some(Arc::new(file));
        self.locks.insert(key, file.clone());
        Ok(file)
    }

    /// Existing unlocked resolution keeps its soft source failures and needs no new host
    /// metadata permission. Its ref is captured once, without release/update side effects.
    async fn pin(&mut self, remote: &Remote) -> Result<(String, String), String> {
        let key = Self::ref_key(remote, &remote.git_ref);
        if let Some(got) = self.legacy.get(&key) { return got.clone(); }
        remote.usable_by(self.hub, &self.caller.name).await?;
        let got = actions::pin_ref_readonly(self.hub, remote, true).await.map(|(sha, reference, _)| (sha, reference)).map_err(|_| {
            let why = if remote.mirror(self.hub).join("HEAD").exists() { "source is unavailable or the ref is missing" } else { "action source unreachable and not mirrored" };
            format!("{}@{}: {why}", remote.named(), remote.git_ref)
        });
        if let Ok((sha, _)) = &got { remote.draft_only(self.hub, &self.caller.name, sha).await?; }
        self.origins.insert(key.clone(), remote.clone());
        self.legacy.insert(key, got.clone());
        got
    }

    /// W29-R23: select and prove the pin before opening its action metadata. The proof stays
    /// anchored to the captured revision, even if a subsequent request sees a moved tag.
    async fn select(&mut self, uses: &str, remote: &Remote, workflow: &str, lock: Option<&github::File>, home: &str) -> Result<(String, Option<Revision>, bool), String> {
        let onboarded = lock.is_some_and(|f| f.workflows.contains_key(workflow));
        if onboarded && remote.hosted(self.hub) && let Some(pin) = github::action_pin(uses)
            && lock.is_some_and(|f| verify::pin_for(f, workflow, &pin).is_some()) {
            return Err(format!("KCI4052 {uses}: the source is now hosted by kiln, not the GitHub repository whose ids were locked"));
        }
        // Hosted sources belong to .kiln/lock (W29-R24), never GitHub's numeric-id file.
        if !onboarded || remote.hosted(self.hub) {
            self.allowed(uses)?;
            let key = Self::ref_key(remote, &remote.git_ref);
            let (sha, _) = self.pin(remote).await?;
            return Ok((sha, self.revisions.get(&key).cloned(), false));
        }
        let file = lock.ok_or_else(|| "KCI4050 the workflow's lock is missing".to_owned())?;
        let canonical = github::action_pin(uses).ok_or_else(|| format!("KCI4050 {uses} is not an action pin"))?;
        let (key, entry) = verify::pin_for(file, workflow, &canonical).ok_or_else(|| format!("KCI4050 {workflow}: {uses} is missing from this workflow's lock closure"))?;
        let wanted = github::digest(&entry.commit).ok_or_else(|| format!("KCI1022 {uses}: invalid locked commit"))?.to_ascii_lowercase();
        let ref_key = Self::ref_key(remote, &entry.git_ref);
        let revision = match self.revisions.get(&ref_key) {
            Some(r) => r.clone(),
            None => {
                let r = self.source.resolve(Repository { name: remote.full(), native: remote.native }, entry.git_ref.clone()).await.map_err(|e| format!("KCI4051 {uses}: {e}"))?;
                if self.legacy.get(&ref_key).and_then(|r| r.as_ref().ok()).is_some_and(|(old, _)| *old != r.commit) {
                    return Err(format!("KCI4051 {uses}: the source ref changed during preflight; start the run again"));
                }
                self.legacy.insert(ref_key.clone(), Ok((r.commit.clone(), r.reference.clone())));
                self.origins.insert(ref_key.clone(), remote.clone());
                self.revisions.insert(ref_key, r.clone());
                r
            }
        };
        let reachable = self.source.contains(revision.clone(), wanted.clone()).await.map_err(|e| format!("KCI4051 {uses}: {e}"))?;
        let (hostname, owner_id, repo_id) = match &revision.identity {
            Identity::Github { hostname, owner_id, repo_id } => (hostname.clone(), *owner_id, *repo_id),
            Identity::Native { .. } => (String::new(), 0, 0),
        };
        let facts = [(key.to_owned(), verify::GitFacts {
            hostname, owner_id, repo_id, reference: revision.reference.clone(), current: revision.commit.clone(),
            reachable: if reachable { [wanted].into() } else { BTreeSet::new() }, previous: None, requested_ref: revision.requested.clone(),
        })].into();
        let checked = verify::check_github(verify::GithubCheck {
            workflow, uses: &[uses.to_owned()], file: Some(file), facts: &facts, home_host: home, policy: verify::Policy::default(), accepted_moves: &BTreeMap::new(),
        }).map_err(|e| e.diagnostics.iter().map(|d| d.render(workflow)).collect::<Vec<_>>().join("\n"))?;
        self.warnings.extend(checked.warnings.iter().map(|d| d.render(workflow)));
        let selected = checked.pins.get(&canonical).ok_or_else(|| format!("KCI4050 {uses}: no verified selection"))?;
        // The action policy judges the code that will run, including require_sha_pins.
        self.allowed(&format!("{}@{}", uses.rsplit_once('@').map_or(uses, |(name, _)| name), selected.commit))?;
        Ok((selected.commit.clone(), Some(revision), true))
    }

    async fn metadata(&mut self, remote: &Remote, commit: &str, revision: Option<&Revision>) -> Result<Option<Arc<str>>, String> {
        let key = format!("{}@{commit}", remote.named());
        if let Some(text) = self.texts.get(&key) { return Ok(text.clone()); }
        let base = if remote.path.is_empty() { String::new() } else { format!("{}/", remote.path) };
        for name in ["action.yml", "action.yaml"] {
            let path = format!("{base}{name}");
            let bytes = match revision {
                Some(r) => match self.source.file(r.clone(), commit.to_owned(), path).await {
                    Ok(b) => Some(b), Err(kiln_seams::dependencies::Refusal::Missing("file")) => None,
                    Err(e) => return Err(format!("KCI4051 {}: {e}", remote.named())),
                },
                None => blob(self.hub, &remote.mirror(self.hub), Some(remote), commit, &path).await?,
            };
            if let Some(bytes) = bytes {
                self.count(bytes.len())?;
                let text: Arc<str> = String::from_utf8(bytes).map_err(|_| format!("KCI1022 {}: action metadata is not UTF-8", remote.named()))?.into();
                self.texts.insert(key, Some(Arc::clone(&text)));
                return Ok(Some(text));
            }
        }
        self.texts.insert(key, None);
        Ok(None)
    }

    async fn action_map(&mut self, workflow: &Workflow, lock: Option<&github::File>, home: &str) -> Result<Actions, String> {
        let locked = lock.is_some_and(|f| f.workflows.contains_key(&workflow.path));
        let mut todo = Vec::new();
        for job in &workflow.jobs { step_uses(&job.steps, &mut todo, 0, None); }
        let mut out = Actions::new();
        let mut seen = BTreeSet::new();
        let mut beneath = Vec::new();
        let mut docker = BTreeSet::new();
        while let Some((uses, depth, root)) = todo.pop() {
            if !seen.insert(uses.clone()) {
                if let Some(parent) = &root && let Some(a) = out.get(&uses) {
                    if let Some(why) = &a.error { beneath.push((parent.clone(), format!("{uses}: {why}"))); }
                    if a.docker { docker.insert(parent.clone()); }
                }
                continue;
            }
            self.count(0)?;
            if let Some(why) = kiln_ci::drafts::uses_of_another(&uses, &self.caller.name) { return Err(format!("KCI4023 {why}")); }
            if crate::runner::builtins::is_kiln(&uses) {
                if crate::runner::builtins::builtin(&uses).is_none() { return Err(format!("{uses} is not a built-in kiln action")); }
                continue;
            }
            let top = root.clone().unwrap_or_else(|| uses.clone());
            if uses.starts_with("docker://") {
                self.allowed(&uses)?;
                docker.insert(top);
                continue;
            }
            if depth > 9 { return Err(format!("{uses}: composite actions nest at most 10 deep")); }
            let local = uses.strip_prefix("./").or_else(|| uses.strip_prefix("$/"));
            let (mut resolved, text, strict) = if let Some(path) = local {
                if !path.is_empty() && !kiln_seams::dependencies::path_valid(path.trim_end_matches('/')) { return Err(format!("KCI1022 {uses}: action path must stay inside the repository")); }
                let owner = Owner { repository: self.caller.name.clone(), commit: self.root_commit.to_owned(), remote: None };
                let base = path.trim_matches('/');
                let mut text = None;
                for name in ["action.yml", "action.yaml"] {
                    let file = if base.is_empty() { name.to_owned() } else { format!("{base}/{name}") };
                    if let Some(bytes) = owner.file(self.hub, &file).await? {
                        self.count(bytes.len())?;
                        text = Some(Arc::<str>::from(String::from_utf8(bytes).map_err(|_| format!("KCI1022 {uses}: action metadata is not UTF-8"))?));
                        break;
                    }
                }
                let resolved = uses.starts_with("$/").then(|| ActionRef {
                    repo: self.caller.name.clone(), sha: owner.commit.clone(), path: base.to_owned(),
                    url: format!("/_kiln/actions/{}/{}.tar.gz?path={}", self.caller.name, owner.commit, actions::encode_path(base)), ..ActionRef::default()
                });
                (resolved, text, false)
            } else {
                let Some(remote) = Remote::parse(&uses) else { continue };
                let selected = self.select(&uses, &remote, &workflow.path, lock, home).await;
                let (sha, revision, strict) = match selected {
                    Ok(s) => s,
                    Err(why) if locked || why.starts_with("KCI402") => return Err(why),
                    Err(why) => {
                        if let Some(root) = &root { beneath.push((root.clone(), format!("{uses}: {why}"))); }
                        out.insert(uses.clone(), ActionRef { repo: remote.full(), path: remote.path, error: Some(why), ..ActionRef::default() });
                        continue;
                    }
                };
                let mut effective = self.origin(&remote);
                if let Some(revision) = &revision && let Some((owner, repo)) = revision.repository.name.split_once('/') {
                    effective.owner = owner.into(); effective.repo = repo.into();
                }
                let text = match self.metadata(&effective, &sha, revision.as_ref()).await {
                    Ok(text) => text,
                    Err(e) if strict => return Err(e),
                    Err(_) => None,
                };
                (Some(ActionRef { repo: effective.full(), sha: sha.clone(), path: effective.path.clone(), url: effective.url(&sha), ..ActionRef::default() }), text, strict)
            };
            let action = text.as_deref().map(wf::parse_action);
            if strict && action.as_ref().is_none_or(|(a, d)| a.is_none() || d.iter().any(|d| d.is_error())) {
                return Err(format!("KCI4051 {uses}: locked action metadata is missing or invalid"));
            }
            if let Some((Some(action), _)) = action {
                if let Some(a) = &mut resolved { a.declares(&action); }
                if action.runs.using == "docker" { docker.insert(top.clone()); }
                if action.runs.using == "composite" { step_uses(&action.runs.steps, &mut todo, depth.saturating_add(1), Some(top)); }
            }
            if let Some(a) = resolved { out.insert(uses, a); }
        }
        for root in docker { if let Some(a) = out.get_mut(&root) { a.docker = true; } }
        for (root, why) in beneath { if let Some(a) = out.get_mut(&root) && a.error.is_none() { a.error = Some(format!("an action it uses could not be resolved, {why}")); } }
        Ok(out)
    }

    async fn called(&mut self, uses: &str, owner: &Owner, files: &mut Files) -> Result<(Arc<Workflow>, Owner), String> {
        if let Some(why) = kiln_ci::drafts::uses_of_another(uses, &self.caller.name) { return Err(format!("KCI4023 {why}")); }
        self.allowed(uses)?;
        if let Some(path) = uses.strip_prefix("./").or_else(|| uses.strip_prefix("$/")) {
            if let Some(wf) = files.get(uses) { return Ok((Arc::clone(wf), owner.clone())); }
            // Local workflows include their overlays, exactly as the normal loader reads them.
            let loaded = super::load::load(self.hub, self.caller, self.root_commit).await.map_err(|_| format!("KCI4051 {uses}: caller workflow tree is unavailable"))?;
            let loaded = loaded.workflows.into_iter().find(|w| w.path == path).ok_or_else(|| format!("{uses}: no local reusable workflow"))?;
            if loaded.has_errors() { return Err(format!("{uses}: local reusable workflow is invalid")); }
            let wf = Arc::new(loaded.workflow.ok_or_else(|| format!("{uses}: no local workflow model"))?);
            self.count(0)?;
            files.insert(uses.to_owned(), Arc::clone(&wf));
            files.insert(path.to_owned(), Arc::clone(&wf));
            return Ok((wf, Owner { repository: self.caller.name.clone(), commit: self.root_commit.into(), remote: None }));
        }
        let remote = Remote::parse(uses).ok_or_else(|| format!("{uses}: not a reusable workflow reference"))?;
        if let Some(wf) = files.get(uses) {
            let origin = wf.origin.as_ref().ok_or_else(|| format!("{uses}: captured workflow has no origin"))?;
            return Ok((Arc::clone(wf), Owner { repository: origin.repository.clone(), commit: origin.sha.clone(), remote: Some(self.origin(&remote)) }));
        }
        let (commit, reference) = self.pin(&remote).await?;
        let remote = self.origin(&remote);
        let owner = Owner { repository: remote.full(), commit: commit.clone(), remote: Some(remote.clone()) };
        let bytes = owner.file(self.hub, &remote.path).await?.ok_or_else(|| format!("{uses}: reusable workflow file is missing at {commit}"))?;
        self.count(bytes.len())?;
        let text = String::from_utf8(bytes).map_err(|_| format!("KCI1022 {uses}: workflow is not UTF-8"))?;
        let dialect = if matches!(wf::file_kind(&remote.path), Some(wf::FileKind::KilnWorkflow)) { kiln_ci::expr::Dialect::Kiln } else { kiln_ci::expr::Dialect::Gha };
        let (model, diagnostics) = wf::parse_workflow(&remote.path, &text, dialect);
        if let Some(d) = diagnostics.iter().find(|d| d.is_error()) { return Err(format!("{uses}: {} {}", d.code, d.message)); }
        let mut model = model.ok_or_else(|| format!("{uses}: not a reusable workflow"))?;
        super::reusable::own_calls(&mut model, &remote, &commit);
        model.origin = Some(wf::ir::Origin { repository: remote.full(), git_ref: reference, sha: commit });
        let model = Arc::new(model);
        files.insert(uses.to_owned(), Arc::clone(&model));
        Ok((model, owner))
    }
}

fn step_uses(steps: &[wf::ir::Step], out: &mut Vec<(String, usize, Option<String>)>, depth: usize, root: Option<String>) {
    for step in steps {
        if let Some(uses) = &step.uses { out.push((uses.clone(), depth, root.clone())); }
        step_uses(&step.parallel, out, depth, root.clone());
    }
}

pub async fn prepare(hub: &Arc<Hub>, caller: &Repo, commit: &str, path: &str, workflow: Arc<Workflow>) -> Result<Frozen, String> {
    crate::gitops::ensure_commit(hub, caller, commit).await.map_err(|_| "KCI4051 the run's exact workflow tree is unavailable".to_owned())?;
    let source = super::dependency_sources::Sources::new(Arc::clone(hub), caller.name.clone());
    prepare_with(hub, &source, caller, commit, path, workflow).await
}

async fn prepare_with(hub: &Hub, source: &dyn DependencySources, caller: &Repo, commit: &str, path: &str, workflow: Arc<Workflow>) -> Result<Frozen, String> {
    if workflow.path != path { return Err("KCI1022 the workflow model's path differs from the run's path".into()); }
    let mut capture = Capture {
        hub, caller, source, root_commit: commit, policies: actions::policies(hub, caller).await,
        revisions: BTreeMap::new(), legacy: BTreeMap::new(), origins: BTreeMap::new(), locks: BTreeMap::new(), texts: BTreeMap::new(),
        bytes: 0, items: 0, warnings: BTreeSet::new(),
    };
    let mut frozen = Frozen { version: 1, repository: caller.name.clone(), path: path.into(), commit: commit.into(), root_locked: false, scopes: BTreeMap::new(), files: Files::new(), warnings: Vec::new() };
    let mut todo = vec![Work { workflow, owner: Owner { repository: caller.name.clone(), commit: commit.into(), remote: None }, depth: 0 }];
    while let Some(work) = todo.pop() {
        let key = scope_key(&work.owner.repository, &work.workflow.path, &work.owner.commit);
        if frozen.scopes.contains_key(&key) { continue; }
        if work.depth > 10 { return Err("reusable workflows nest at most 10 deep".into()); }
        capture.count(0)?;
        let lock = capture.lock(&work.owner, &work.workflow.path).await?;
        let locked = lock.as_ref().is_some_and(|f| f.workflows.contains_key(&work.workflow.path));
        if work.depth == 0 { frozen.root_locked = locked; }
        let home = work.owner.home_host(hub, caller);
        let actions = capture.action_map(&work.workflow, lock.as_deref(), &home).await?;
        frozen.scopes.insert(key, Scope { repository: work.owner.repository.clone(), path: work.workflow.path.clone(), commit: work.owner.commit.clone(), locked, actions });
        for uses in work.workflow.jobs.iter().filter_map(|j| j.uses.as_ref()) {
            let (workflow, owner) = capture.called(uses, &work.owner, &mut frozen.files).await?;
            todo.push(Work { workflow, owner, depth: work.depth.saturating_add(1) });
        }
    }
    frozen.warnings = capture.warnings.into_iter().collect();
    Frozen::bounded(&frozen)?;
    Ok(frozen)
}

/// Old runs have no snapshot; their exact original tree still decides whether a fresh-actions
/// request is a lock change. New unlocked runs have already captured that fact.
pub async fn refresh_allowed(hub: &Arc<Hub>, caller: &Repo, commit: &str, path: &str, saved: Option<&Frozen>) -> Result<(), String> {
    let locked = match saved {
        Some(f) => f.root_locked,
        None if path.starts_with(".github/workflows/") => {
            crate::gitops::ensure_commit(hub, caller, commit).await.map_err(|_| "the original workflow tree is unavailable; fresh actions cannot bypass its locks".to_owned())?;
            let owner = Owner { repository: caller.name.clone(), commit: commit.into(), remote: None };
            match owner.file(hub, github::PATH).await? {
                None => false,
                Some(bytes) => {
                    let text = String::from_utf8(bytes).map_err(|_| "KCI1022 the original lock is not UTF-8".to_owned())?;
                    github::parse(&text).map_err(|_| "KCI1022 the original lock doesn't read".to_owned())?.workflows.contains_key(path)
                }
            }
        }
        None => false,
    };
    if locked { Err("This workflow is locked. Fresh actions require a reviewed lock change (W29-R25).".into()) } else { Ok(()) }
}

pub async fn retry_needs_preflight(hub: &Hub, run: &str) -> Result<bool, String> {
    let id = run.to_owned();
    hub.store.call(move |c| Ok(c.query_row(
        "SELECT COALESCE(json_extract(CASE WHEN json_valid(input) THEN input ELSE '{}' END, '$.dependency_preflight_failed'), 0) FROM ci_plans WHERE run_id = ?1", [&id], |r| r.get(0),
    ).optional()?.unwrap_or(false))).await.map_err(|_| "the saved preflight state could not be read".into())
}

pub async fn prepare_retry(hub: &Arc<Hub>, run: &str, fresh: bool) -> Result<Frozen, String> {
    let id = run.to_owned();
    let saved: Option<(String, String, String, String, String)> = hub.store.call(move |c| Ok(c.query_row(
        "SELECT r.repo_id, r.sha, r.status, p.workflow, p.input FROM runs r JOIN ci_plans p ON p.run_id = r.id WHERE r.id = ?1", [&id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    ).optional()?)).await.map_err(|_| "the original run could not be read".to_owned())?;
    let (repo_id, commit, status, model, input) = saved.ok_or_else(|| "no such run of the CI engine".to_owned())?;
    if !crate::store::Status::parse(&status).is_terminal() { return Err("only a finished run can be run again".into()); }
    let caller = hub.store.repos().await.map_err(|_| "the run's repository could not be read".to_owned())?.into_iter().find(|r| r.id == repo_id).ok_or_else(|| "the run's repository is gone".to_owned())?;
    let workflow: Arc<Workflow> = serde_json::from_str(&model).map_err(|_| "the original workflow model doesn't read".to_owned())?;
    let input = serde_json::from_str(&input).map_err(|_| "the original run input doesn't read".to_owned())?;
    let snapshot = Frozen::saved(&input, &caller.name, &workflow.path, &commit)?;
    if !fresh {
        if let Some(snapshot) = snapshot { return Ok(snapshot); }
        if input.get("dependency_preflight_failed").and_then(serde_json::Value::as_bool) != Some(true) { return Err("the run has no failed preflight to repeat".into()); }
    } else { refresh_allowed(hub, &caller, &commit, &workflow.path, snapshot.as_ref()).await?; }
    let prepared = prepare(hub, &caller, &commit, &workflow.path, Arc::clone(&workflow)).await?;
    if fresh && prepared.root_locked { return Err("Fresh actions require a reviewed lock change (W29-R25).".into()); }
    Ok(prepared)
}

#[cfg(all(test, unix))]
mod tests;
