//! W29-R23/R24: preflight fixes Git dependencies before policy, metadata and planning. The accepted
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
    /// W29-R24: only the execution inputs each job needs, including its composite actions.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub execution: BTreeMap<String, kiln_ci::lock::execution::Locks>,
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

    pub fn execution_for(&self, defined: &kiln_ci::plan::DefinedIn, job: &str) -> Option<&kiln_ci::lock::execution::Locks> {
        self.scopes.get(&scope_key(&defined.workflow_repository, &defined.workflow_file_path, &defined.workflow_sha))?
            .execution.get(job.rsplit('/').next().unwrap_or(job).trim())
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
            for (job, locks) in &scope.execution {
                if job.is_empty() || job.contains('/') || job.chars().any(char::is_control) { return Err("the saved execution lock names an invalid job".into()); }
                locks.validate()?;
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
pub(super) async fn blob(hub: &Hub, dir: &Path, remote: Option<&Remote>, commit: &str, path: &str) -> Result<Option<Vec<u8>>, String> {
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
    revision: Option<Revision>,
}

impl Owner {
    async fn file(&self, hub: &Hub, source: &dyn DependencySources, path: &str) -> Result<Option<Vec<u8>>, String> {
        if let Some(revision) = &self.revision {
            return match source.file(revision.clone(), self.commit.clone(), path.into()).await {
                Ok(bytes) => Ok(Some(bytes)), Err(kiln_seams::dependencies::Refusal::Missing("file")) => Ok(None),
                Err(e) => Err(format!("KCI4051 {}:{path}: {e}", self.repository)),
            };
        }
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
    native: Vec<native::Layer>,
    prefix: String,
}

struct Capture<'a> {
    hub: &'a Hub,
    caller: &'a Repo,
    source: &'a dyn DependencySources,
    root_commit: &'a str,
    root_path: &'a str,
    root_name: String,
    policies: Vec<kiln_ci::policy::Policy>,
    lock_policy: verify::Policy,
    record: bool,
    revisions: BTreeMap<String, Revision>,
    legacy: BTreeMap<String, Result<(String, String), String>>,
    origins: BTreeMap<String, Remote>,
    locks: BTreeMap<String, Option<Arc<github::File>>>,
    native_locks: BTreeMap<String, Option<Arc<kiln_ci::lock::kiln::File>>>,
    texts: BTreeMap<String, Option<Arc<str>>>,
    bytes: usize,
    items: usize,
    warnings: BTreeSet<String>,
    manifests: BTreeSet<String>,
    archives: BTreeMap<kiln_seams::dependencies::Runtime, kiln_seams::dependencies::Archive>,
    histories: BTreeMap<String, history::Seen>,
}

impl Capture<'_> {
    fn missing(&self, workflow: &str, uses: &str) -> String { verify::missing_pin(workflow, uses, self.lock_policy).iter().map(|d| d.render(workflow)).collect::<Vec<_>>().join("\n") }
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

    fn allowed_pinned(&self, uses: &str) -> Result<(), String> {
        let policies: Vec<_> = self.policies.iter().collect();
        kiln_ci::policy::check_pinned(&policies, uses).map_err(|r| format!("{} {}", r.code, r.message))
    }

    fn captured(&self, remote: &Remote, reference: &str) -> Option<Revision> {
        self.revisions.get(&Self::ref_key(remote, reference)).cloned().or_else(|| {
            // A captured qualified tag also proves its bare spelling. HEAD still needs its
            // own request, and a qualified branch must not assume no same-named tag exists.
            let canonical = if reference.starts_with("refs/") { reference.to_owned() }
                else if reference != "HEAD" && !kiln_seams::dependencies::commit_valid(reference) { format!("refs/tags/{reference}") } else { return None };
            self.revisions.values().find(|r| r.repository.name.eq_ignore_ascii_case(&remote.full()) && r.repository.native == remote.native && r.reference == canonical).cloned()
        })
    }

    async fn lock(&mut self, owner: &Owner, path: &str) -> Result<Option<Arc<github::File>>, String> {
        if !path.starts_with(".github/workflows/") { return Ok(None); }
        let key = format!("{}:{}@{}", if owner.remote.is_some() { "remote" } else { "own" }, owner.repository, owner.commit);
        if let Some(cached) = self.locks.get(&key) { return Ok(cached.clone()); }
        let Some(bytes) = owner.file(self.hub, self.source, github::PATH).await? else { self.locks.insert(key, None); return Ok(None) };
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
        let possible_full = remote.git_ref.strip_prefix("refs/tags/").or_else(|| (!remote.git_ref.starts_with("refs/")).then_some(remote.git_ref.as_str()))
            .is_some_and(|tag| matches!(kiln_ci::releases::kind_of(tag), kiln_ci::releases::TagKind::Full));
        if possible_full {
            let revision = match self.captured(remote, &remote.git_ref) {
                Some(revision) => Ok(revision),
                None => self.source.resolve(Repository { name: remote.full(), native: remote.native }, remote.git_ref.clone()).await,
            };
            match revision {
                Ok(revision) => {
                    let seen = self.history(remote, &revision).await?;
                    self.remember(&seen, &format!("{}@{}", remote.named(), remote.git_ref), self.root_path, false, true).await?;
                    let got = (revision.commit.clone(), revision.reference.clone());
                    self.origins.insert(key.clone(), remote.clone());
                    self.revisions.insert(key.clone(), revision);
                    self.legacy.insert(key, Ok(got.clone()));
                    return Ok(got);
                }
                Err(error) if self.lock_policy.moved_tags == verify::MovedTags::Refuse => return Err(format!("KCI4051 {}: full-version history could not be verified: {error}", remote.named())),
                Err(_) => { self.warnings.insert(format!("{}: full-version history could not be verified; moved_tags is flag", remote.named())); }
            }
        }
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
    async fn select(&mut self, uses: &str, remote: &Remote, workflow: &str, lock: Option<&github::File>, native: &[native::Layer], home: &str) -> Result<(String, Option<Revision>, bool), String> {
        let onboarded = lock.is_some_and(|f| f.workflows.contains_key(workflow));
        if let Some(selection) = self.native_selection(uses, remote, native).await? {
            let same = github::action_pin(uses).and_then(|pin| lock.and_then(|f| verify::pin_for(f, workflow, &pin)))
                .is_some_and(|(_, entry)| github::digest(&entry.commit).is_some_and(|c| c.eq_ignore_ascii_case(&selection.0)));
            // A caller may override a callee's seed. Where both files select the same tree,
            // retain the GitHub file's own provenance, HEAD and moved-full-tag checks too.
            if !same { return Ok(selection); }
        }
        if onboarded && remote.hosted(self.hub) && let Some(pin) = github::action_pin(uses)
            && lock.is_some_and(|f| verify::pin_for(f, workflow, &pin).is_some()) {
            return Err(format!("KCI4052 {uses}: the source is now hosted by kiln, not the GitHub repository whose ids were locked"));
        }
        // Hosted sources belong to .kiln/lock (W29-R24), never GitHub's numeric-id file.
        if remote.hosted(self.hub) && onboarded { return Err(self.missing(workflow, uses)); }
        if !onboarded {
            if !native.is_empty() { return Err(self.missing(workflow, uses)); }
            self.allowed(uses)?;
            let key = Self::ref_key(remote, &remote.git_ref);
            let (sha, _) = self.pin(remote).await?;
            return Ok((sha, self.revisions.get(&key).cloned(), false));
        }
        let file = lock.ok_or_else(|| "KCI4050 the workflow's lock is missing".to_owned())?;
        let canonical = github::action_pin(uses).ok_or_else(|| format!("KCI4050 {uses} is not an action pin"))?;
        let (key, entry) = verify::pin_for(file, workflow, &canonical).ok_or_else(|| self.missing(workflow, uses))?;
        let wanted = github::digest(&entry.commit).ok_or_else(|| format!("KCI1022 {uses}: invalid locked commit"))?.to_ascii_lowercase();
        let ref_key = Self::ref_key(remote, &entry.git_ref);
        let revision = match self.captured(remote, &entry.git_ref) {
            Some(r) => r,
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
        proposals::check_pin(self.hub, self.caller, &revision.identity, &wanted).await?;
        let reachable = self.source.contains(revision.clone(), wanted.clone()).await.map_err(|e| format!("KCI4051 {uses}: {e}"))?;
        let (hostname, owner_id, repo_id) = match &revision.identity {
            Identity::Github { hostname, owner_id, repo_id } => (hostname.clone(), *owner_id, *repo_id),
            Identity::Native { .. } => (String::new(), 0, 0),
        };
        let seen = self.history(remote, &revision).await?;
        let facts = [(key.to_owned(), verify::GitFacts {
            hostname, owner_id, repo_id, reference: revision.reference.clone(), current: revision.commit.clone(),
            reachable: if reachable { [wanted].into() } else { BTreeSet::new() }, previous: seen.previous.clone(), requested_ref: revision.requested.clone(),
        })].into();
        let accepted_moves = seen.accepted_moves(key);
        let checked = verify::check_github(verify::GithubCheck {
            workflow, uses: &[uses.to_owned()], file: Some(file), facts: &facts, home_host: home, policy: self.lock_policy, accepted_moves: &accepted_moves,
        });
        self.remember(&seen, uses, workflow, true, checked.is_ok()).await?;
        let checked = checked.map_err(|e| e.diagnostics.iter().map(|d| d.render(workflow)).collect::<Vec<_>>().join("\n"))?;
        self.warnings.extend(checked.warnings.iter().map(|d| d.render(workflow)));
        let selected = checked.pins.get(&canonical).ok_or_else(|| format!("KCI4050 {uses}: no verified selection"))?;
        // The action policy judges the code that will run, including require_sha_pins.
        self.allowed_pinned(&format!("{}@{}", uses.split_once('@').map_or(uses, |(name, _)| name), selected.commit))?;
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

    async fn action_map(&mut self, workflow: &Workflow, lock: Option<&github::File>, native: &[native::Layer], home: &str, prefix: &str) -> Result<(Actions, BTreeMap<String, kiln_ci::lock::execution::Locks>), String> {
        let locked = !native.is_empty() || lock.is_some_and(|f| f.workflows.contains_key(&workflow.path));
        let mut todo = Vec::new();
        for job in &workflow.jobs { step_uses(&job.steps, &mut todo, 0, None); }
        let mut out = Actions::new();
        let mut assets = BTreeMap::new();
        let mut children = BTreeMap::new();
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
                if native.is_empty() { self.allowed(&uses)?; }
                docker.insert(top);
                if !native.is_empty() {
                    let mut locks = kiln_ci::lock::execution::Locks::default();
                    self.image(&uses, native, &mut locks).await?;
                    assets.insert(uses, locks);
                }
                continue;
            }
            if depth > 9 { return Err(format!("{uses}: composite actions nest at most 10 deep")); }
            let local = uses.strip_prefix("./").or_else(|| uses.strip_prefix("$/"));
            let (mut resolved, text, strict, location, base) = if let Some(path) = local {
                if !path.is_empty() && !kiln_seams::dependencies::path_valid(path.trim_end_matches('/')) { return Err(format!("KCI1022 {uses}: action path must stay inside the repository")); }
                let owner = Owner { repository: self.caller.name.clone(), commit: self.root_commit.to_owned(), remote: None, revision: None };
                let base = path.trim_matches('/');
                let mut text = None;
                for name in ["action.yml", "action.yaml"] {
                    let file = if base.is_empty() { name.to_owned() } else { format!("{base}/{name}") };
                    if let Some(bytes) = owner.file(self.hub, self.source, &file).await? {
                        self.count(bytes.len())?;
                        text = Some(Arc::<str>::from(String::from_utf8(bytes).map_err(|_| format!("KCI1022 {uses}: action metadata is not UTF-8"))?));
                        break;
                    }
                }
                let resolved = uses.starts_with("$/").then(|| ActionRef {
                    repo: self.caller.name.clone(), sha: owner.commit.clone(), path: base.to_owned(),
                    url: format!("/_kiln/actions/{}/{}.tar.gz?path={}", self.caller.name, owner.commit, actions::encode_path(base)), ..ActionRef::default()
                });
                (resolved, text, !native.is_empty(), owner, base.to_owned())
            } else {
                let Some(remote) = Remote::parse(&uses) else { continue };
                let selected = self.select(&uses, &remote, &workflow.path, lock, native, home).await;
                let (sha, revision, strict) = match selected {
                    Ok(s) => s,
                    Err(why) if locked || why.starts_with("KCI402") || (self.lock_policy.moved_tags == verify::MovedTags::Refuse && ["KCI4051", "KCI4052", "KCI4056"].iter().any(|code| why.contains(code))) => return Err(why),
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
                let owner = Owner { repository: effective.full(), commit: sha.clone(), remote: Some(effective.clone()), revision };
                (Some(ActionRef { repo: effective.full(), sha: sha.clone(), path: effective.path.clone(), url: effective.url(&sha), ..ActionRef::default() }), text, strict, owner, effective.path.clone())
            };
            let action = text.as_deref().map(wf::parse_action);
            if strict && action.as_ref().is_none_or(|(a, d)| a.is_none() || d.iter().any(|d| d.is_error())) {
                return Err(format!("KCI4051 {uses}: locked action metadata is missing or invalid"));
            }
            if let Some((Some(action), _)) = action {
                assets.insert(uses.clone(), self.action_execution(&uses, &action, &location, &base, native).await?);
                let mut nested = Vec::new();
                step_uses(&action.runs.steps, &mut nested, 0, None);
                children.insert(uses.clone(), nested.into_iter().map(|(u, _, _)| u).collect());
                if let Some(a) = &mut resolved { a.declares(&action); }
                if action.runs.using == "docker" { docker.insert(top.clone()); }
                if action.runs.using == "composite" { step_uses(&action.runs.steps, &mut todo, depth.saturating_add(1), Some(top)); }
            }
            if let Some(a) = resolved { out.insert(uses, a); }
        }
        for root in docker { if let Some(a) = out.get_mut(&root) { a.docker = true; } }
        for (root, why) in beneath { if let Some(a) = out.get_mut(&root) && a.error.is_none() { a.error = Some(format!("an action it uses could not be resolved, {why}")); } }
        let execution = self.job_execution(workflow, native, &assets, &children, prefix).await?;
        Ok((out, execution))
    }

    async fn called(&mut self, uses: &str, owner: &Owner, native: &[native::Layer], files: &mut Files) -> Result<(Arc<Workflow>, Owner), String> {
        if let Some(why) = kiln_ci::drafts::uses_of_another(uses, &self.caller.name) { return Err(format!("KCI4023 {why}")); }
        if let Some(path) = uses.strip_prefix("./").or_else(|| uses.strip_prefix("$/")) {
            self.allowed(uses)?;
            if let Some(wf) = files.get(uses) { return Ok((Arc::clone(wf), owner.clone())); }
            // Local workflows include their overlays, exactly as the normal loader reads them.
            let loaded = super::load::load(self.hub, self.caller, self.root_commit).await.map_err(|_| format!("KCI4051 {uses}: caller workflow tree is unavailable"))?;
            let loaded = loaded.workflows.into_iter().find(|w| w.path == path).ok_or_else(|| format!("{uses}: no local reusable workflow"))?;
            if loaded.has_errors() { return Err(format!("{uses}: local reusable workflow is invalid")); }
            let wf = Arc::new(loaded.workflow.ok_or_else(|| format!("{uses}: no local workflow model"))?);
            self.count(0)?;
            files.insert(uses.to_owned(), Arc::clone(&wf));
            files.insert(path.to_owned(), Arc::clone(&wf));
            return Ok((wf, Owner { repository: self.caller.name.clone(), commit: self.root_commit.into(), remote: None, revision: None }));
        }
        let remote = Remote::parse(uses).ok_or_else(|| format!("{uses}: not a reusable workflow reference"))?;
        remote.usable_by(self.hub, &self.caller.name).await?;
        // own_calls rewrites a publisher's relative call to that same captured tree. It
        // inherits the parent's proof; a tag is neither fetched nor selected a second time.
        let relative = remote.full().eq_ignore_ascii_case(&owner.repository) && remote.git_ref == owner.commit;
        let (commit, reference, revision) = if relative {
            self.allowed_pinned(&format!("{}@{}", remote.named(), owner.commit))?;
            (owner.commit.clone(), owner.revision.as_ref().map_or_else(|| owner.commit.clone(), |r| r.reference.clone()), owner.revision.clone())
        } else if let Some((commit, revision, _)) = self.native_selection(uses, &remote, native).await? {
            let reference = revision.as_ref().map_or_else(|| remote.git_ref.clone(), |r| r.reference.clone());
            (commit, reference, revision)
        } else {
            if !native.is_empty() { return Err(self.missing(self.root_path, uses)); }
            self.allowed(uses)?;
            let (commit, reference) = self.pin(&remote).await?;
            (commit, reference, None)
        };
        if let Some(wf) = files.get(uses) {
            let origin = wf.origin.as_ref().ok_or_else(|| format!("{uses}: captured workflow has no origin"))?;
            if origin.sha != commit { return Err(format!("KCI4054 {uses}: caller lock entries select both {} and {commit} for the same call", origin.sha)); }
            return Ok((Arc::clone(wf), Owner { repository: origin.repository.clone(), commit: origin.sha.clone(), remote: Some(self.origin(&remote)), revision }));
        }
        let remote = self.origin(&remote);
        let owner = Owner { repository: remote.full(), commit: commit.clone(), remote: Some(remote.clone()), revision };
        let bytes = owner.file(self.hub, self.source, &remote.path).await?.ok_or_else(|| format!("{uses}: reusable workflow file is missing at {commit}"))?;
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

/// W29-R26: previews may refresh source facts, but cannot accept or announce an observation.
pub async fn inspect(hub: &Arc<Hub>, caller: &Repo, commit: &str, path: &str, workflow: Arc<Workflow>) -> Result<Frozen, String> {
    crate::gitops::ensure_commit(hub, caller, commit).await.map_err(|_| "KCI4051 the workflow's exact tree is unavailable".to_owned())?;
    let source = super::dependency_sources::Sources::new(Arc::clone(hub), caller.name.clone());
    capture_with(hub, &source, caller, commit, path, workflow, false).await
}

/// W29-R25: generate the complete pair before the draft writer changes any branch or diff.
pub async fn draft_files(hub: &Arc<Hub>, caller: &Repo, commit: &str, changed: &[crate::githost::Change], all: bool) -> Result<kiln_ci::lock::regenerate::Files, String> {
    crate::gitops::ensure_commit(hub, caller, commit).await.map_err(|_| "KCI4051 the draft's exact base is unavailable".to_owned())?;
    let source = super::dependency_sources::Sources::new(Arc::clone(hub), caller.name.clone());
    let all = all || changed.iter().any(|f| wf::file_kind(&f.path) == Some(wf::FileKind::Action));
    let paths: Vec<_> = changed.iter().filter(|f| matches!(wf::file_kind(&f.path), Some(wf::FileKind::GhaWorkflow | wf::FileKind::KilnWorkflow))).map(|f| f.path.clone()).collect::<BTreeSet<_>>().into_iter().collect();
    let edits = changed.iter().map(|f| (f.path.clone(), f.content.clone())).collect();
    generate::collect(hub, &source, caller, commit, edits, if all { None } else { Some(&paths) }, kiln_ci::lock::regenerate::Mode::Preserve).await
}

async fn prepare_with(hub: &Hub, source: &dyn DependencySources, caller: &Repo, commit: &str, path: &str, workflow: Arc<Workflow>) -> Result<Frozen, String> {
    capture_with(hub, source, caller, commit, path, workflow, true).await
}

async fn capture_with(hub: &Hub, source: &dyn DependencySources, caller: &Repo, commit: &str, path: &str, workflow: Arc<Workflow>, record: bool) -> Result<Frozen, String> {
    capture_with_policy(hub, source, caller, commit, path, workflow, CaptureOptions { record, ..Default::default() }).await
}

#[derive(Default)]
struct CaptureOptions { record: bool, required: verify::Policy }

async fn capture_with_policy(hub: &Hub, source: &dyn DependencySources, caller: &Repo, commit: &str, path: &str, workflow: Arc<Workflow>, options: CaptureOptions) -> Result<Frozen, String> {
    if workflow.path != path { return Err("KCI1022 the workflow model's path differs from the run's path".into()); }
    let mut capture = Capture {
        hub, caller, source, root_commit: commit, root_path: path, root_name: workflow.name.clone().unwrap_or_default(), record: options.record, policies: actions::policies(hub, caller).await,
        lock_policy: super::repo_config::lock_policy(hub, caller).await?.stricter(options.required),
        revisions: BTreeMap::new(), legacy: BTreeMap::new(), origins: BTreeMap::new(), locks: BTreeMap::new(), native_locks: BTreeMap::new(), texts: BTreeMap::new(),
        bytes: 0, items: 0, warnings: BTreeSet::new(), manifests: BTreeSet::new(), archives: BTreeMap::new(), histories: BTreeMap::new(),
    };
    let mut frozen = Frozen { version: 1, repository: caller.name.clone(), path: path.into(), commit: commit.into(), root_locked: false, scopes: BTreeMap::new(), files: Files::new(), warnings: Vec::new() };
    let mut todo = vec![Work { workflow, owner: Owner { repository: caller.name.clone(), commit: commit.into(), remote: None, revision: None }, depth: 0, native: Vec::new(), prefix: String::new() }];
    while let Some(work) = todo.pop() {
        let key = scope_key(&work.owner.repository, &work.workflow.path, &work.owner.commit);
        if work.depth > 10 { return Err("reusable workflows nest at most 10 deep".into()); }
        capture.count(0)?;
        let lock = capture.lock(&work.owner, &work.workflow.path).await?;
        let own_native = capture.native_lock(&work.owner, &work.workflow.path).await?;
        if let (Some(gh), Some(layer)) = (&lock, &own_native) {
            kiln_ci::lock::regenerate::compare(gh, &layer.file, &layer.home).map_err(|ds| ds.iter().map(|d| d.render(&work.workflow.path)).collect::<Vec<_>>().join("\n"))?;
        }
        let mut native = work.native;
        if let Some(layer) = own_native { native.push(layer); }
        let locked = !native.is_empty() || lock.as_ref().is_some_and(|f| f.workflows.contains_key(&work.workflow.path));
        if !locked && capture.lock_policy.require != verify::Require::Off {
            let why = format!("KCI4055 {}: Require lockfile: this workflow has no lockfile entry", work.workflow.path);
            if capture.lock_policy.require == verify::Require::Enforce { return Err(why); }
            capture.warnings.insert(why);
        }
        if work.depth == 0 { frozen.root_locked = locked; }
        let home = work.owner.home_host(hub, caller);
        let (actions, execution) = capture.action_map(&work.workflow, lock.as_deref(), &native, &home, &work.prefix).await?;
        if let Some(before) = frozen.scopes.get(&key) {
            let pins = |actions: &Actions| actions.iter().map(|(k, a)| (k.clone(), a.sha.clone())).collect::<BTreeMap<_, _>>();
            if before.execution != execution || serde_json::to_value(&before.actions).map_err(|_| "KCI1022 action snapshot cannot be encoded")? != serde_json::to_value(&actions).map_err(|_| "KCI1022 action snapshot cannot be encoded")? {
                return Err(format!("KCI4054 {}: caller lock contexts select both {:?} and {:?}", work.workflow.path, pins(&before.actions), pins(&actions)));
            }
        }
        frozen.scopes.insert(key, Scope { repository: work.owner.repository.clone(), path: work.workflow.path.clone(), commit: work.owner.commit.clone(), locked, actions, execution });
        for (job, uses) in work.workflow.jobs.iter().filter_map(|j| j.uses.as_ref().map(|uses| (j.id.as_str(), uses))) {
            let (workflow, owner) = capture.called(uses, &work.owner, &native, &mut frozen.files).await?;
            todo.push(Work { workflow, owner, depth: work.depth.saturating_add(1), native: native.clone(), prefix: format!("{}{job}/", work.prefix) });
        }
    }
    frozen.warnings = capture.warnings.into_iter().collect();
    Frozen::bounded(&frozen)?;
    Ok(frozen)
}

/// Old runs have no snapshot; their exact original tree still decides whether a fresh-actions
/// request is a lock change. New unlocked runs have already captured that fact.
pub async fn refresh_allowed(hub: &Arc<Hub>, caller: &Repo, commit: &str, path: &str, saved: Option<&Frozen>) -> Result<(), String> {
    if is_locked(hub, caller, commit, path, saved).await? { Err("This workflow is locked. Fresh actions require a reviewed lock change (W29-R25).".into()) } else { Ok(()) }
}

async fn is_locked(hub: &Arc<Hub>, caller: &Repo, commit: &str, path: &str, saved: Option<&Frozen>) -> Result<bool, String> {
    let locked = match saved {
        Some(f) => f.root_locked,
        None => {
            crate::gitops::ensure_commit(hub, caller, commit).await.map_err(|_| "the original workflow tree is unavailable; fresh actions cannot bypass its locks".to_owned())?;
            let owner = Owner { repository: caller.name.clone(), commit: commit.into(), remote: None, revision: None };
            let source = super::dependency_sources::Sources::new(Arc::clone(hub), caller.name.clone());
            if let Some(bytes) = owner.file(hub, &source, kiln_ci::lock::kiln::PATH).await? {
                let text = String::from_utf8(bytes).map_err(|_| "KCI1022 the original kiln lock is not UTF-8".to_owned())?;
                if kiln_ci::lock::kiln::parse(&text).map_err(|_| "KCI1022 the original kiln lock doesn't read".to_owned())?.workflows.contains_key(path) {
                    return Ok(true);
                }
            }
            if !path.starts_with(".github/workflows/") { return Ok(false); }
            match owner.file(hub, &source, github::PATH).await? {
                None => false,
                Some(bytes) => {
                    let text = String::from_utf8(bytes).map_err(|_| "KCI1022 the original lock is not UTF-8".to_owned())?;
                    github::parse(&text).map_err(|_| "KCI1022 the original lock doesn't read".to_owned())?.workflows.contains_key(path)
                }
            }
        }
    };
    Ok(locked)
}

pub async fn retry_needs_preflight(hub: &Hub, run: &str) -> Result<bool, String> {
    let id = run.to_owned();
    hub.store.call(move |c| Ok(c.query_row(
        "SELECT COALESCE(json_extract(CASE WHEN json_valid(input) THEN input ELSE '{}' END, '$.dependency_preflight_failed'), 0) FROM ci_plans WHERE run_id = ?1", [&id], |r| r.get(0),
    ).optional()?.unwrap_or(false))).await.map_err(|_| "the saved preflight state could not be read".into())
}

/// W29-R26: cached catalog facts, without resolving or accepting a mutable ref.
pub async fn moved_tags(hub: &Hub, publisher: &str, native: bool) -> anyhow::Result<Vec<serde_json::Value>> { history::moved(hub, publisher, native).await }

/// Pending observations survive a server restart; only declared notification rules send them.
pub async fn notify_moves(hub: &Hub) -> anyhow::Result<()> {
    let pending: Vec<String> = hub.store.call(|c| {
        let mut st = c.prepare("SELECT DISTINCT consumer_repo_id FROM ci_lock_moves WHERE notified_at IS NULL")?;
        let rows = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?; Ok(rows)
    }).await?;
    for repo in hub.store.repos().await?.into_iter().filter(|r| pending.contains(&r.id)) {
        history::notify_pending(hub, &repo).await.map_err(anyhow::Error::msg)?;
    }
    Ok(())
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

mod native;
mod execution;
mod history;
mod generate;
mod proposals;
pub use proposals::{fresh_draft, proposal_for};
pub(crate) use proposals::{keep_proposal, proposed_files};
mod replay;
pub(crate) use replay::{lock_path, regenerate_tree};
mod api;
pub use api::{update, verify_repo};
