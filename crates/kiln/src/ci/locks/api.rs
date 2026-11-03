//! W29-R25: updating writes one reviewed draft; verification only reads source facts. A read
//! cannot establish a first acceptance, change moved-tag history or emit a notification.

use futures::future::BoxFuture;
use http::StatusCode;
use kiln_seams::dependencies::{Archive, Manifest, Refusal, Runtime};
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::*;
use crate::authz::{Caller, Scope};
use crate::ci::Fail;

async fn repository(hub: &Hub, key: &str, caller: &Caller, permission: &str) -> Result<Repo, Fail> {
    let repo = hub.store.repos().await.map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "repositories could not be read".into()))?.into_iter().find(|r| r.name == key || r.id == key).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no repository {key}")))?;
    if !caller.can(permission, &Scope::Repo { repo: repo.name.clone() }) { return Err((StatusCode::FORBIDDEN, format!("this needs `{permission}` on {}", repo.name))); }
    Ok(repo)
}

pub async fn update(hub: &Arc<Hub>, caller: &Caller, key: &str, body: &Value) -> Result<Value, Fail> {
    let repo = repository(hub, key, caller, "workflows.edit").await?;
    if !caller.can("diffs.submit", &Scope::Repo { repo: repo.name.clone() }) { return Err((StatusCode::FORBIDDEN, "updating locks opens a draft and needs `diffs.submit`".into())); }
    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct File { path: String, text: String }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Ask {
        #[serde(rename = "ref")]
        reference: Option<String>,
        /// The editor's "lock it": its file as typed, and its overlay, saved with their locks.
        path: Option<String>,
        content: Option<String>,
        #[serde(default)]
        files: Vec<File>,
    }
    let ask: Ask = serde_json::from_value(body.clone()).map_err(|e| (StatusCode::BAD_REQUEST, format!("lock update: {e}")))?;
    if let Some(content) = ask.content {
        let path = ask.path.ok_or_else(|| (StatusCode::BAD_REQUEST, "`content` is a workflow's as the editor has it: `path` names it".to_owned()))?;
        if ask.reference.is_some() { return Err((StatusCode::BAD_REQUEST, "the editor's file is locked in the draft it's saved to, at no other `ref`".into())); }
        // W29-R25: the same draft path as Save draft, whose locks take what it uses.
        let out = super::super::workflows::edit(hub, caller, &repo.name, &path, &serde_json::json!({ "content": content, "files": ask.files, "draft": true })).await?;
        hub.store.audit(caller.name.clone(), "ci.locks.update".into(), format!("{} {path}: locked in a draft from the editor", repo.name)).await;
        return Ok(out);
    }
    if ask.path.is_some() || !ask.files.is_empty() { return Err((StatusCode::BAD_REQUEST, "`path` and `files` come with the editor's `content`".into())); }
    let reference = ask.reference.unwrap_or(super::super::events::facts(hub, &repo).await.default_branch);
    if !super::super::load::ref_ok(&reference) { return Err((StatusCode::BAD_REQUEST, "`ref` needs a branch, tag or commit".into())); }
    let out = super::super::workflows::lock_draft(hub, caller, &repo, &reference).await?;
    hub.store.audit(caller.name.clone(), "ci.locks.update".into(), format!("{} at {reference}: locks proposed in a draft", repo.name)).await;
    Ok(out)
}

/// A verification captures a mutable ref once even when several workflows share it. File
/// reads remain at their selected commit; the cache holds no URL, credential or host writer.
struct Facts {
    inner: Arc<dyn DependencySources>,
    revisions: Mutex<BTreeMap<(Repository, String), Result<Revision, Refusal>>>,
    files: Mutex<BTreeMap<(Repository, String, String), Vec<u8>>>,
}

impl Facts {
    fn new(inner: Arc<dyn DependencySources>) -> Self { Self { inner, revisions: Mutex::default(), files: Mutex::default() } }

    fn observed(&self, remote: &Remote) -> Option<Revision> {
        self.revisions.lock().iter().find_map(|((repo, reference), result)| {
            let revision = result.as_ref().ok()?;
            (repo.name.eq_ignore_ascii_case(&remote.full()) && repo.native == remote.native && (reference == &remote.git_ref || revision.reference == remote.git_ref || ["refs/tags/", "refs/heads/"].iter().any(|p| revision.reference.strip_prefix(p) == Some(remote.git_ref.as_str())))).then(|| revision.clone())
        })
    }

    fn locked(&self, uses: &str, root: &BTreeMap<String, String>) -> Option<String> {
        let key = kiln_ci::lock::kiln::canonical_key(uses)?;
        if let Some(commit) = root.get(&key).or_else(|| github::action_pin(uses).and_then(|k| root.get(&k))) { return Some(commit.clone()); }
        for ((_, _, path), bytes) in self.files.lock().iter() {
            let text = std::str::from_utf8(bytes).ok()?;
            if path == github::PATH && let Ok(file) = github::parse(text) {
                for workflow in file.workflows.keys() {
                    if let Some((_, action)) = verify::pin_for(&file, workflow, &key) { return github::digest(&action.commit).map(str::to_owned); }
                }
            } else if path == kiln_ci::lock::kiln::PATH && let Ok(file) = kiln_ci::lock::kiln::parse(text) {
                for workflow in file.workflows.keys() {
                    if let Some((_, action)) = kiln_ci::lock::verify_kiln::pin_for(&file, workflow, &key) { return github::digest(&action.commit).map(str::to_owned); }
                }
            }
        }
        None
    }
}

impl DependencySources for Facts {
    fn compare(&self, before: Revision, after: Revision) -> BoxFuture<'_, Result<kiln_seams::dependencies::Difference, Refusal>> { self.inner.compare(before, after) }
    fn resolve(&self, repository: Repository, reference: String) -> BoxFuture<'_, Result<Revision, Refusal>> {
        Box::pin(async move {
            let key = (repository.clone(), reference.clone());
            if let Some(value) = self.revisions.lock().get(&key) { return value.clone(); }
            if let Some(revision) = self.revisions.lock().iter().find_map(|((repo, _), result)| {
                let revision = result.as_ref().ok()?;
                (repo == &repository && (revision.reference == reference || revision.reference.strip_prefix("refs/tags/") == Some(reference.as_str()))).then(|| revision.clone())
            }) { return Ok(revision); }
            if self.revisions.lock().len() >= MAX_ITEMS { return Err(Refusal::Full { what: "verification refs", limit: MAX_ITEMS }); }
            let value = self.inner.resolve(repository, reference).await;
            self.revisions.lock().insert(key, value.clone());
            value
        })
    }
    fn contains(&self, revision: Revision, commit: String) -> BoxFuture<'_, Result<bool, Refusal>> { self.inner.contains(revision, commit) }
    fn tags_at(&self, revision: Revision, commit: String) -> BoxFuture<'_, Result<Vec<String>, Refusal>> { self.inner.tags_at(revision, commit) }
    fn file(&self, revision: Revision, commit: String, path: String) -> BoxFuture<'_, Result<Vec<u8>, Refusal>> {
        Box::pin(async move {
            let bytes = self.inner.file(revision.clone(), commit.clone(), path.clone()).await?;
            if matches!(path.as_str(), github::PATH | kiln_ci::lock::kiln::PATH) {
                let mut files = self.files.lock();
                let key = (revision.repository, commit, path);
                if !files.contains_key(&key) {
                    if files.len() >= MAX_ITEMS || files.values().fold(bytes.len(), |n, b| n.saturating_add(b.len())) > MAX_BYTES { return Err(Refusal::TooLarge { what: "verification lockfiles", limit: MAX_BYTES }); }
                    files.insert(key, bytes.clone());
                }
            }
            Ok(bytes)
        })
    }
    fn manifest(&self, reference: String) -> BoxFuture<'_, Result<Manifest, Refusal>> { self.inner.manifest(reference) }
    fn runtime(&self, runtime: Runtime) -> BoxFuture<'_, Result<Archive, Refusal>> { self.inner.runtime(runtime) }
}

fn pin_map(github: Option<&github::File>, native: Option<&kiln_ci::lock::kiln::File>, workflow: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Some(file) = github {
        for key in file.closure(workflow).unwrap_or_default() {
            if let Some(sha) = file.dependencies.get(&key).and_then(|a| github::digest(&a.commit)) { out.insert(key, sha.into()); }
        }
    }
    if let Some(file) = native {
        for key in file.closure(workflow).unwrap_or_default() {
            match file.dependencies.get(&key) {
                Some(kiln_ci::lock::kiln::Dependency::Git(g)) => { if let Some(sha) = github::digest(&g.commit) { out.insert(key, sha.into()); } }
                Some(kiln_ci::lock::kiln::Dependency::Image(i)) => { out.insert(key, i.digest.clone()); }
                _ => {}
            }
        }
    }
    out
}

fn failed_use(why: &str, workflow: &str) -> String {
    why.split_whitespace().filter_map(|s| kiln_ci::lock::kiln::canonical_key(s.trim_matches(['`', '\'', '"', ',', ';', ':']))).next().unwrap_or_else(|| workflow.to_owned())
}

pub async fn verify_repo(hub: &Arc<Hub>, caller: &Caller, key: &str, q: &HashMap<String, String>) -> Result<Value, Fail> {
    let repo = repository(hub, key, caller, "runs.read").await?;
    let reference = q.get("sha").filter(|s| !s.is_empty()).cloned().unwrap_or(super::super::events::facts(hub, &repo).await.default_branch);
    if !super::super::load::ref_ok(&reference) { return Err((StatusCode::BAD_REQUEST, "`sha` needs a branch, tag or commit".into())); }
    let commit = super::super::load::resolve(hub, &repo, &reference).await.map_err(|_| (StatusCode::BAD_REQUEST, "`sha` needs a branch, tag or commit".into()))?.ok_or_else(|| (StatusCode::NOT_FOUND, "the workflow commit is unavailable".into()))?;
    let source = Arc::new(super::super::dependency_sources::Sources::new(Arc::clone(hub), repo.name.clone()));
    verify_with(hub, source, &repo, &commit).await
}

pub(super) async fn verify_with(hub: &Hub, source: Arc<dyn DependencySources>, repo: &Repo, commit: &str) -> Result<Value, Fail> {
    let source = Facts::new(source);
    let owner = Owner { repository: repo.name.clone(), commit: commit.into(), remote: None, revision: None };
    let parse = async {
        let gh = owner.file(hub, &source, github::PATH).await?.map(|b| String::from_utf8(b).map_err(|_| "KCI1022 GitHub lock is not UTF-8".to_owned())).transpose()?.map(|s| github::parse(&s).map_err(|ds| ds.iter().map(|d| d.render(github::PATH)).collect::<Vec<_>>().join("\n"))).transpose()?;
        let kl = owner.file(hub, &source, kiln_ci::lock::kiln::PATH).await?.map(|b| String::from_utf8(b).map_err(|_| "KCI1022 kiln lock is not UTF-8".to_owned())).transpose()?.map(|s| kiln_ci::lock::kiln::parse(&s).map_err(|ds| ds.iter().map(|d| d.render(kiln_ci::lock::kiln::PATH)).collect::<Vec<_>>().join("\n"))).transpose()?;
        Ok::<_, String>((gh, kl))
    }.await;
    let (gh, kl) = match parse {
        Ok(files) => files,
        Err(why) => return Ok(json!({ "fresh": false, "stale": [{ "workflow": "", "uses": "lockfile", "locked": null, "now": null, "error": why }] })),
    };
    let loaded = super::super::load::load(hub, repo, commit).await.map_err(|_| (StatusCode::UNPROCESSABLE_ENTITY, "the workflow tree is unavailable".into()))?;
    if loaded.workflows.len() > MAX_ITEMS { return Err((StatusCode::UNPROCESSABLE_ENTITY, "too many workflows to verify".into())); }
    let (mut stale, mut warnings) = (Vec::new(), Vec::new());
    let paths: BTreeSet<_> = loaded.workflows.iter().map(|w| w.path.clone()).collect();
    let recorded: BTreeSet<_> = gh.iter().flat_map(|f| f.workflows.keys()).chain(kl.iter().flat_map(|f| f.workflows.keys())).collect();
    for removed in recorded.into_iter().filter(|p| !paths.contains(*p)) {
        stale.push(json!({ "workflow": removed, "uses": removed, "locked": null, "now": null, "error": "KCI4055 lockfile entry names a removed workflow" }));
    }
    for loaded in loaded.workflows {
        let path = loaded.path;
        let Some(model) = loaded.workflow.filter(|_| !loaded.diagnostics.iter().any(|d| d.severity == wf::Severity::Error)) else {
            stale.push(json!({ "workflow": path, "uses": path, "locked": null, "now": null, "error": "KCI1022 workflow is invalid" })); continue;
        };
        let pins = pin_map(gh.as_ref(), kl.as_ref(), &path);
        // Explicit verification checks all inputs of an onboarded workflow, even when running
        // unlocked jobs remains opt-in. It cannot change the trusted repository policy.
        let require = if gh.as_ref().is_some_and(|f| f.workflows.contains_key(&path)) || kl.as_ref().is_some_and(|f| f.workflows.contains_key(&path)) { verify::Require::Enforce } else { verify::Require::Off };
        match capture_with_policy(hub, &source, repo, commit, &path, Arc::new(model), CaptureOptions { required: verify::Policy { require, ..Default::default() }, ..Default::default() }).await {
            Ok(snapshot) => {
                warnings.extend(snapshot.warnings.iter().cloned());
                if let Some(file) = &gh {
                    let uses: Vec<_> = snapshot.scopes.values().filter(|s| s.repository.eq_ignore_ascii_case(&repo.name)).flat_map(|s| s.actions.keys().cloned()).collect();
                    for diagnostic in verify::unused_github(file, &path, &uses, verify::Require::Enforce) {
                        let uses = failed_use(&diagnostic.message, &path);
                        stale.push(json!({ "workflow": path, "uses": uses, "locked": source.locked(&uses, &pins), "now": null, "error": diagnostic.render(&path) }));
                    }
                }
                if !snapshot.root_locked {
                    let actions = snapshot.root_actions().map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
                    for (uses, action) in actions { stale.push(json!({ "workflow": path, "uses": uses, "locked": null, "now": action.sha, "error": "KCI4055 workflow has no lockfile entry" })); }
                    if actions.is_empty() { stale.push(json!({ "workflow": path, "uses": path, "locked": null, "now": null, "error": "KCI4055 workflow has no lockfile entry" })); }
                }
            }
            Err(why) => {
                let uses = failed_use(&why, &path);
                let now = match Remote::parse(&uses) {
                    Some(remote) => match source.observed(&remote) {
                        Some(revision) => revision.fresh.then_some(revision.commit),
                        None => source.resolve(Repository { name: remote.full(), native: remote.native }, remote.git_ref).await.ok().filter(|r| r.fresh).map(|r| r.commit),
                    },
                    None => None,
                };
                stale.push(json!({ "workflow": path, "uses": uses, "locked": source.locked(&uses, &pins), "now": now, "error": why }));
            }
        }
    }
    for ((repo, reference), revision) in source.revisions.lock().iter() {
        if let Ok(revision) = revision && !revision.fresh { stale.push(json!({ "workflow": "", "uses": format!("{}@{reference}", repo.name), "locked": null, "now": null, "error": "KCI4051 upstream ref could only be read from a previously fetched mirror" })); }
    }
    Ok(json!({ "fresh": stale.is_empty(), "stale": stale, "warnings": warnings }))
}
