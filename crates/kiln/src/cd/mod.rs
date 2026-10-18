//! Deployments, inside the server: kiln-cd's engine with the I/O it asks for.
//!
//! The engine is a state machine with no clock, disk or network (`kiln_cd::engine`). It runs
//! on a thread of its own, since its work is CPU work and hyper-rt shards are never preempted;
//! what it asks for is done here, on the runtime: renders on the blocking pool, watches and API
//! requests as tasks, records into the durable store (hyper-raft, `kiln_cd::store`). Sources are
//! polled from bare mirrors, and an app's render key is the content address of what its render
//! reads, so a commit that doesn't touch an app doesn't render it again.
//!
//! The service registers itself in [`get`] rather than on the hub, so the server's busiest files
//! carry one line each for it.

pub mod api;
pub mod cli;
pub mod guard;
pub mod images;
pub mod kube;
pub mod live;
pub mod ops;
pub mod plans;
pub mod targets;

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use kiln_cd::apps::{self, AppDef, ArtifactRef, Origin};
use kiln_cd::engine::{Action, Config as EngineConfig, Engine, Input, LiveChange, Record, Summary};
use kiln_cd::git::{Auth as GitAuth, Mirror};
use kiln_cd::model::Tracking;
use kiln_cd::obj::{self, GroupKind, ResKey};
use kiln_cd::render::{self, Dep, Kind, Tools};
use kiln_cd::state::{ClusterAuth, ClusterDef, Command, Deployment};
use kiln_cd::store::Store;
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};

use crate::ci::integrations::{self, oci};
use crate::hub::Hub;
use kube::Cluster;

static CD: RwLock<Option<Arc<Cd>>> = RwLock::new(None);

/// The running service, once it has started.
pub fn get() -> Option<Arc<Cd>> {
    CD.read().clone()
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// What the engine thread is sent.
pub enum ToEngine {
    /// An input.
    Input(Box<Input>),
    /// A question, answered on the engine's thread.
    Query(Box<dyn FnOnce(&Engine) + Send>),
}

/// An app as the server knows it: its definition and where its sources stand.
#[derive(Clone, Debug)]
pub struct AppEntry {
    /// The definition.
    pub def: AppDef,
    /// Each source's commit, as last resolved.
    pub commits: Vec<Option<String>>,
    /// What each source's render reads.
    pub deps: Vec<Vec<Dep>>,
    /// The render key last given to the engine.
    pub key: Option<u64>,
    /// Why its sources couldn't be read, if they couldn't.
    pub error: Option<String>,
    /// Apps it renders (app of apps), by id.
    pub children: BTreeSet<String>,
    /// Its latest resolutions, so a render (or a history entry) reads the commits its key
    /// stands for even after git moved.
    pub resolved: VecDeque<Resolved>,
}

/// One resolution of an app's sources: what a render key stands for.
#[derive(Clone, Debug, Default)]
pub struct Resolved {
    /// The first source's commit: the app's revision.
    pub revision: String,
    /// The render key: the content address of everything a render reads, the cluster's version
    /// and API versions included.
    pub key: u64,
    /// The same without the cluster's part: what git holds for the app.
    pub content: u64,
    /// Each source's commit.
    pub commits: Vec<Option<String>>,
    /// What each source's render reads.
    pub deps: Vec<Vec<Dep>>,
    /// Its artifacts, pinned for this revision (docs/cd-targets.md §1.3).
    pub artifacts: Vec<ArtifactRef>,
    /// Pull secrets its artifacts ask for.
    pub pull_secrets: Vec<PullSecret>,
}

impl Resolved {
    /// Makes `artifacts` part of what this resolution stands for: they move its key and content
    /// as a source's files do, so a new digest renders (and syncs) again, and an unchanged one
    /// doesn't.
    pub fn add_artifacts(&mut self, artifacts: Vec<ArtifactRef>, pull_secrets: Vec<PullSecret>) {
        let mut h = 0u64;
        for a in &artifacts {
            h = h.wrapping_add(blake(&[b"artifact", a.name.as_bytes(), a.reference.as_bytes()]));
        }
        for p in &pull_secrets {
            h = h.wrapping_add(blake(&[b"pull-secret", p.name.as_bytes(), crate::util::sha256_hex(p.config.as_bytes()).as_bytes()]));
        }
        self.key ^= h.rotate_left(13);
        self.content ^= h.rotate_left(13);
        self.artifacts = artifacts;
        self.pull_secrets = pull_secrets;
    }
}

/// A Secret of registry credentials an artifact asks for (`pull-secret`): its name and the
/// Docker config it holds, which is never printed.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct PullSecret {
    pub name: String,
    pub config: String,
}

impl std::fmt::Debug for PullSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PullSecret").field("name", &self.name).field("config", &"<redacted>").finish()
    }
}

/// The deployments service.
pub struct Cd {
    hub: Arc<Hub>,
    /// What is kept durably.
    pub store: Store,
    engine: std::sync::mpsc::Sender<ToEngine>,
    /// Every app, by id.
    pub apps: RwLock<BTreeMap<String, AppEntry>>,
    /// Connected clusters, by name.
    pub clusters: RwLock<BTreeMap<String, Arc<Cluster>>>,
    /// The latest status of every app.
    pub statuses: RwLock<BTreeMap<String, Summary>>,
    dirty: Mutex<BTreeSet<String>>,
    watched: Mutex<BTreeSet<(String, GroupKind)>>,
    connecting: Mutex<BTreeSet<String>>,
    /// Held while an app's inputs are checked against its pin and sent, so the source poll and a
    /// rollback never interleave.
    inputs_gate: Mutex<()>,
    dir: PathBuf,
    tools: Tools,
    poll: Duration,
    in_cluster_context: String,
    kubeconfig: String,
    kick: crate::rt::Notify,
    /// What viewers read from clusters (trees, logs), kept a few seconds.
    pub live: live::Live,
    /// Renders plans made, kept by key.
    plans: plans::Renders,
    /// Images resolved lately, by registry and reference, so apps sharing one ask once a minute.
    artifacts: Mutex<HashMap<String, (crate::ci::integrations::oci::Resolved, Instant)>>,
    /// Why a repository's deploy file doesn't read, by repository.
    pub deploy_errors: Mutex<BTreeMap<String, String>>,
    /// Each repository's environments (`.kiln/environments.yaml` at trunk), by its URL.
    pub environments: RwLock<BTreeMap<String, BTreeMap<String, apps::EnvironmentSpec>>>,
    /// The latest sync each app started, for a self-deploy's record.
    pub started: Mutex<HashMap<String, guard::Started>>,
    /// Connected targets beyond Kubernetes, by name (docs/cd-targets.md §2).
    pub targets: RwLock<BTreeMap<String, Arc<targets::Conn>>>,
    /// Why a target can't be reached yet, by name.
    pub target_errors: Mutex<BTreeMap<String, String>>,
    /// Repositories' `owner/name`, by clone URL: whose workload a target's calls are.
    pub repo_names: RwLock<HashMap<String, String>>,
}

impl Cd {
    /// Gives the engine an input.
    pub fn input(&self, input: Input) {
        let _ = self.engine.send(ToEngine::Input(Box::new(input)));
    }

    /// Asks the engine something.
    pub async fn query<T: Send + 'static>(&self, f: impl FnOnce(&Engine) -> T + Send + 'static) -> Option<T> {
        let (tx, rx) = futures::channel::oneshot::channel();
        let job: Box<dyn FnOnce(&Engine) + Send> = Box::new(move |e| {
            let _ = tx.send(f(e));
        });
        self.engine.send(ToEngine::Query(job)).ok()?;
        rx.await.ok()
    }

    /// Polls sources now.
    pub fn kick(&self) {
        self.kick.notify();
    }

    pub(crate) fn mirror_dir(&self, url: &str) -> PathBuf {
        self.dir.join("git").join(format!("{:016x}.git", obj::hash_bytes(url.as_bytes())))
    }

    pub(crate) fn git_auth(&self, url: &str) -> GitAuth {
        // The longest matching repository entry's credentials (Argo CD's credential templates).
        self.store.read(|d| {
            d.repos
                .values()
                .filter(|r| url.starts_with(r.url.trim_end_matches('/')))
                .max_by_key(|r| r.url.len())
                .map(|r| GitAuth {
                    header: match (&r.username, &r.password) {
                        (Some(u), Some(p)) => {
                            use base64::Engine as _;
                            Some(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"))))
                        }
                        (None, Some(p)) => Some(format!("Bearer {p}")),
                        _ => None,
                    },
                    ssh_key: None,
                    insecure: r.insecure,
                })
                .unwrap_or_default()
        })
    }

    /// Registers (or replaces) an app and tells the engine.
    pub fn register(&self, def: AppDef) {
        let id = def.id();
        let mut cfg = def.config();
        cfg.read_only = is_shadow(&def);
        {
            let mut apps = self.apps.write();
            let entry = apps.entry(id.clone()).or_insert_with(|| AppEntry {
                def: def.clone(),
                commits: Vec::new(),
                deps: Vec::new(),
                key: None,
                error: None,
                children: BTreeSet::new(),
                resolved: VecDeque::new(),
            });
            if entry.def != def {
                entry.def = def;
                entry.key = None;
                entry.commits.clear();
                entry.resolved.clear();
            }
        }
        self.input(Input::UpsertApp(cfg));
        // The last sync from before a restart: automation goes on from it, not from scratch.
        let last = self.store.read(|d| d.history.get(&id).and_then(|h| h.back()).and_then(|h| Some((h.content?, h.succeeded))));
        if let Some((content, succeeded)) = last {
            self.input(Input::Remember { app: id.as_str().into(), content, succeeded });
        }
        self.kick();
    }

    /// Forgets an app (and its children); with `cascade`, deletes its objects.
    pub fn unregister(&self, id: &str, cascade: bool) {
        let children = self.apps.write().remove(id).map(|e| e.children).unwrap_or_default();
        self.statuses.write().remove(id);
        self.input(Input::RemoveApp { id: id.into(), cascade: cascade.then_some(kiln_cd::model::Propagation::Foreground) });
        for child in children {
            self.unregister(&child, cascade);
        }
        self.hub.emit(&json!({"type": "cd_app_deleted", "app": id}));
    }

    // ---------------------------------------------------------------- sources

    async fn poll_loop(self: Arc<Self>) {
        loop {
            self.refresh_sources().await;
            let wait = crate::rt::timeout(self.poll, self.kick.notified());
            let _ = wait.await;
            if self.hub.shutdown.is_cancelled() {
                return;
            }
        }
    }

    /// Apps from `.kiln/deploy.yaml` in kiln's repositories (cd.md, Defining applications): each
    /// round reads the file, and `.kiln/environments.yaml`, at the commit trunk is at. An app is
    /// registered when its definition changed, and removed (with its objects, as deleting an app
    /// does) when the file no longer defines it. A file that doesn't read, or a repository that
    /// can't be fetched, changes nothing: its apps stay as they were.
    async fn refresh_deploy_files(self: &Arc<Self>) {
        let repos = match self.hub.store.repos().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "deployments: can't list repositories");
                return;
            }
        };
        self.repo_names.write().extend(repos.iter().map(|r| (r.clone_url.clone(), r.name.clone())));
        for repo in repos.into_iter().filter(|r| r.enabled) {
            let trunk = repo.branches.first().cloned().unwrap_or_else(|| "main".into());
            let (dir, auth, url, t) = (self.mirror_dir(&repo.clone_url), self.git_auth(&repo.clone_url), repo.clone_url.clone(), trunk.clone());
            let read = crate::rt::blocking(move || -> Result<(String, Option<String>, Option<String>)> {
                let m = Mirror::open(&dir, &url, auth)?;
                m.fetch()?;
                let commit = m.resolve(&t)?;
                let text = |p: &str| -> Result<Option<String>> { Ok(m.read(&commit, p)?.map(|b| String::from_utf8_lossy(&b).into_owned())) };
                let deploy = text(".kiln/deploy.yaml")?;
                let envs = if deploy.is_some() { text(".kiln/environments.yaml")? } else { None };
                Ok((commit, deploy, envs))
            })
            .await
            .and_then(|r| r);
            let (commit, deploy, envs) = match read {
                Ok(x) => x,
                Err(e) => {
                    tracing::debug!(repo = %repo.name, error = %format!("{e:#}"), "deployments: can't read the repository's deploy file");
                    continue;
                }
            };
            let defs = match deploy {
                // No file: none of its apps are left.
                None => Vec::new(),
                Some(text) => {
                    let environments: BTreeMap<String, apps::EnvironmentSpec> = match envs.as_deref().map(serde_yaml::from_str).transpose() {
                        Ok(e) => e.unwrap_or_default(),
                        Err(e) => {
                            self.deploy_file_error(&repo.name, Some(format!(".kiln/environments.yaml at {}: {e}", short(&commit))));
                            continue;
                        }
                    };
                    let mut scope = apps::Scope::default();
                    scope.set("git.sha", json!(commit));
                    scope.set("git.short_sha", json!(short(&commit)));
                    scope.set("git.ref", json!(format!("refs/heads/{trunk}")));
                    scope.set("repo.name", json!(repo.name));
                    let parsed = apps::from_deploy_file(&text, &repo.clone_url, &trunk, &environments, &scope);
                    if parsed.is_ok() {
                        self.environments.write().insert(repo.clone_url.clone(), environments);
                    }
                    match parsed {
                        Ok(d) => d,
                        Err(e) => {
                            self.deploy_file_error(&repo.name, Some(format!(".kiln/deploy.yaml at {}: {}", short(&commit), e.0)));
                            continue;
                        }
                    }
                }
            };
            self.deploy_file_error(&repo.name, None);
            let ids: BTreeSet<String> = defs.iter().map(AppDef::id).collect();
            for def in defs {
                let changed = self.apps.read().get(&def.id()).is_none_or(|e| e.def != def);
                if changed {
                    tracing::info!(app = %def.id(), repo = %repo.name, commit = %short(&commit), "deployments: an app from a deploy file");
                    self.register(def);
                }
            }
            let gone: Vec<String> = self
                .apps
                .read()
                .iter()
                .filter(|(id, e)| matches!(&e.def.origin, Origin::Deploy { repo: r } if *r == repo.clone_url) && !ids.contains(*id))
                .map(|(id, _)| id.clone())
                .collect();
            for id in gone {
                tracing::info!(app = %id, repo = %repo.name, "deployments: the deploy file no longer defines an app; removing it");
                let cascade = self.apps.read().get(&id).is_some_and(|e| e.def.cascade);
                self.unregister(&id, cascade);
            }
        }
    }

    /// Records (or clears) why a repository's deploy file doesn't read, once per change.
    fn deploy_file_error(&self, repo: &str, error: Option<String>) {
        let mut errors = self.deploy_errors.lock();
        if errors.get(repo) == error.as_ref() {
            return;
        }
        match error {
            Some(e) => {
                tracing::warn!(repo = %repo, error = %e, "deployments: a deploy file doesn't read; its apps stay as they were");
                errors.insert(repo.to_owned(), e);
            }
            None => {
                errors.remove(repo);
            }
        }
    }

    async fn refresh_sources(self: &Arc<Self>) {
        self.refresh_deploy_files().await;
        self.connect_targets().await;
        let apps: Vec<AppEntry> = self.apps.read().values().cloned().collect();
        // Each repository is fetched once a round.
        let mut urls: BTreeSet<String> = BTreeSet::new();
        for a in &apps {
            for s in a.def.sources.iter().filter(|s| s.chart.is_none()) {
                urls.insert(s.repo.clone());
            }
        }
        let mut fetched: HashMap<String, Result<(), String>> = HashMap::new();
        for url in urls {
            let (dir, auth) = (self.mirror_dir(&url), self.git_auth(&url));
            let u = url.clone();
            let r = crate::rt::blocking(move || -> Result<()> {
                let m = Mirror::open(&dir, &u, auth)?;
                m.fetch()
            })
            .await
            .and_then(|r| r)
            .map_err(|e| format!("{e:#}"));
            if let Err(e) = &r {
                tracing::warn!(repo = %url, error = %e, "deployments: fetching a source failed");
            }
            fetched.insert(url, r);
        }
        for a in apps {
            let id = a.def.id();
            let failed = a.def.sources.iter().find_map(|s| fetched.get(&s.repo).and_then(|r| r.as_ref().err()).cloned());
            if let Some(e) = failed {
                self.set_source_error(&id, Some(e));
                continue;
            }
            let this = self.clone();
            let def = a.def.clone();
            let def_for_artifacts = a.def.clone();
            // Renders read the cluster's version and API versions (Helm's .Capabilities), so an
            // app's inputs wait for its cluster: a render without them would differ from the one
            // after, and could be applied first.
            let Some(cluster) = self.clusters.read().get(&def.destination.cluster).cloned() else { continue };
            let (kube_version, api_versions) = (cluster.version.read().clone(), cluster.discovery.read().api_versions.clone());
            let pin = self.store.read(|d| d.pins.get(&id).cloned());
            let mut seen = pin.clone();
            let result = crate::rt::blocking(move || -> Result<(Resolved, Option<Resolved>)> {
                let head = this.inputs_of(&def, kube_version.as_deref(), &api_versions, None)?;
                // A rolled-back app holds its deployment's commits until git changes what it
                // renders; then it follows git again.
                let held = match &pin {
                    Some(p) if p.head == head.content => Some(this.inputs_of(&def, kube_version.as_deref(), &api_versions, Some(&p.commits))?),
                    _ => None,
                };
                Ok((head, held))
            })
            .await
            .and_then(|r| r);
            // Artifacts resolve for each revision: the head's, and a rolled-back app's own.
            let result = match result {
                Ok((mut head, mut held)) => async {
                    let (a, p) = self.resolve_artifacts(&def_for_artifacts, &head.revision).await?;
                    head.add_artifacts(a, p);
                    if let Some(h) = held.as_mut() {
                        let (a, p) = self.held_artifacts(&def_for_artifacts, &id, h).await?;
                        h.add_artifacts(a, p);
                    }
                    Ok::<_, anyhow::Error>((head, held))
                }
                .await,
                Err(e) => Err(e),
            };
            match result {
                Ok((head, held)) => {
                    if seen.is_some() && held.is_none() {
                        tracing::info!(app = %id, "deployments: git changed a rolled-back app; it follows git again");
                        match self.store.submit(Command::Pin { app: id.clone(), pin: None }).await {
                            Ok(Ok(())) => seen = None,
                            Ok(Err(e)) => tracing::warn!(app = %id, error = %e, "deployments: letting go of a rollback failed"),
                            Err(_) => tracing::warn!(app = %id, "deployments: the store stopped"),
                        }
                    }
                    // A rollback may have pinned the app (or let go of it) meanwhile: then its
                    // inputs stand, and the next poll starts from them.
                    let _gate = self.inputs_gate.lock();
                    if self.store.read(|d| d.pins.get(&id).cloned()) == seen {
                        self.use_inputs(&id, held.unwrap_or(head));
                    }
                }
                Err(e) => self.set_source_error(&id, Some(format!("{e:#}"))),
            }
        }
    }

    fn set_source_error(&self, id: &str, e: Option<String>) {
        if let Some(entry) = self.apps.write().get_mut(id) {
            if entry.error != e {
                tracing::warn!(app = %id, error = ?e, "deployments: an app's sources can't be read");
            }
            entry.error = e;
        }
    }

    /// Makes `r` an app's inputs, telling the engine when they moved.
    fn use_inputs(&self, id: &str, r: Resolved) {
        if self.keep_inputs(id, &r) {
            self.input(Input::Inputs { app: id.into(), revision: r.revision.into(), key: r.key, content: r.content });
        }
    }

    /// Records `r` as an app's inputs (renders read their commits from here); whether they moved.
    fn keep_inputs(&self, id: &str, r: &Resolved) -> bool {
        let mut apps = self.apps.write();
        let Some(e) = apps.get_mut(id) else { return false };
        let moved = e.key != Some(r.key) || e.commits != r.commits;
        e.key = Some(r.key);
        e.commits = r.commits.clone();
        e.deps = r.deps.clone();
        e.error = None;
        if !e.resolved.iter().any(|x| x.key == r.key) {
            e.resolved.push_back(r.clone());
            while e.resolved.len() > 8 {
                e.resolved.pop_front();
            }
        }
        moved
    }

    /// Resolves an app's sources: its revision, render key, each source's commit and what each
    /// source reads. `at` holds sources at given commits (a rollback); the others resolve their
    /// revisions.
    fn inputs_of(&self, def: &AppDef, kube_version: Option<&str>, api_versions: &[String], at: Option<&[Option<String>]>) -> Result<Resolved> {
        let mut key = blake(&[serde_json::to_vec(&def.sources)?.as_slice(), def.destination.namespace.as_bytes(), def.name.as_bytes()]);
        // Only Helm and plugins read the cluster's version and API versions. Other renders come out
        // alike anywhere, so a CRD installed on the cluster moves none of their keys.
        let cluster = blake(&[kube_version.unwrap_or("").as_bytes(), api_versions.join(",").as_bytes()]).rotate_left(7);
        let mut reads_cluster = false;
        let held = |i: usize| at.and_then(|c| c.get(i).cloned().flatten());
        let mut commits = Vec::new();
        let mut all_deps = Vec::new();
        let mut revision = String::new();
        // Commits of ref sources first: other sources' value files read them.
        let mut ref_commits: HashMap<String, (Mirror, String)> = HashMap::new();
        for (i, s) in def.sources.iter().enumerate() {
            if let (Some(name), None) = (&s.as_ref, &s.chart) {
                let m = Mirror::open(&self.mirror_dir(&s.repo), &s.repo, self.git_auth(&s.repo))?;
                let commit = match held(i) {
                    Some(c) => c,
                    None => m.resolve(&s.revision)?,
                };
                ref_commits.insert(name.clone(), (m, commit));
            }
        }
        for (i, s) in def.sources.iter().enumerate() {
            if s.chart.is_some() {
                reads_cluster = true;
                commits.push(None);
                // A chart from a Helm repository: its version is its content (ref value files below).
                let deps = render::deps(s, Kind::Helm, &|_| None);
                key ^= self.ref_dep_hashes(&deps, &ref_commits)?.rotate_left((i % 61) as u32);
                all_deps.push(deps);
                continue;
            }
            let m = Mirror::open(&self.mirror_dir(&s.repo), &s.repo, self.git_auth(&s.repo))?;
            let commit = match held(i) {
                Some(c) => c,
                None => m.resolve(&s.revision)?,
            };
            if revision.is_empty() && s.as_ref.is_none() {
                revision = commit.clone();
            }
            if s.as_ref.is_some() {
                commits.push(Some(commit));
                all_deps.push(Vec::new());
                continue;
            }
            let read = |p: &str| m.read(&commit, p).ok().flatten().map(|b| String::from_utf8_lossy(&b).into_owned());
            let kind = render::kind_of(s, &|p| m.hashes(&commit, &[p]).ok().and_then(|h| h.into_iter().next().flatten()).is_some());
            reads_cluster |= matches!(kind, Kind::Helm | Kind::Plugin);
            // A plugin, or parameters naming the revision, can write the commit itself into the
            // objects: then it is part of what the render reads.
            if kind == Kind::Plugin || serde_json::to_string(&s.engine).is_ok_and(|e| e.contains("ARGOCD_APP_REVISION") || e.contains("KILN_SHA")) {
                key ^= blake(&[b"commit", commit.as_bytes()]).rotate_left(((i + 17) % 61) as u32);
            }
            let deps = render::deps(s, kind, &read);
            let own: Vec<String> = deps.iter().filter(|d| d.source.is_none()).map(|d| d.path.clone()).collect();
            let own = m.with_links(&commit, &own)?;
            let refs: Vec<Dep> = deps.into_iter().filter(|d| d.source.is_some()).collect();
            let paths: Vec<&str> = own.iter().map(String::as_str).collect();
            let hashes = m.hashes(&commit, &paths)?;
            let mut h = 0u64;
            for (p, hash) in paths.iter().zip(&hashes) {
                h = h.wrapping_add(blake(&[p.as_bytes(), hash.as_deref().unwrap_or("-").as_bytes()]));
            }
            key ^= h.rotate_left((i % 61) as u32);
            key ^= self.ref_dep_hashes(&refs, &ref_commits)?.rotate_left(((i + 31) % 61) as u32);
            let mut deps: Vec<Dep> = own.into_iter().map(|path| Dep { source: None, path }).collect();
            deps.extend(refs);
            commits.push(Some(commit));
            all_deps.push(deps);
        }
        if revision.is_empty() {
            revision = def.sources.first().map(|s| s.revision.clone()).unwrap_or_default();
        }
        let content = key;
        if reads_cluster {
            key ^= cluster;
        }
        Ok(Resolved { revision, key, content, commits, deps: all_deps, ..Resolved::default() })
    }

    fn ref_dep_hashes(&self, deps: &[Dep], refs: &HashMap<String, (Mirror, String)>) -> Result<u64> {
        let mut h = 0u64;
        for d in deps {
            let Some(name) = &d.source else { continue };
            let (m, commit) = refs.get(name).with_context(|| format!("no source is named {name}"))?;
            let hash = m.hashes(commit, &[d.path.as_str()])?.into_iter().next().flatten();
            h = h.wrapping_add(blake(&[name.as_bytes(), d.path.as_bytes(), hash.as_deref().unwrap_or("-").as_bytes()]));
        }
        Ok(h)
    }

    // ---------------------------------------------------------------- artifacts

    /// The registry an artifact is pulled from: its `registry` integration, else the one set up
    /// for its registry, else none (anonymously).
    async fn artifact_registry(&self, spec: &apps::ArtifactSpec, reference: &oci::Reference) -> Result<oci::Registry> {
        match &spec.registry {
            Some(name) => integrations::registry(&self.hub, name).await,
            None => Ok(integrations::registry_for(&self.hub, &reference.registry).await?.unwrap_or_else(|| oci::Registry::for_name(&reference.registry, None, false))),
        }
    }

    /// An app's artifacts for `revision`: each image's digest now, and the pull secrets they ask
    /// for. An image not in its registry yet (CI hasn't pushed it) is an error that says so: the
    /// app's inputs wait, and the next poll looks again.
    pub(crate) async fn resolve_artifacts(&self, def: &AppDef, revision: &str) -> Result<(Vec<ArtifactRef>, Vec<PullSecret>)> {
        let mut refs = Vec::new();
        for (name, given) in &def.artifacts {
            // A published image: in a registry, an image like any other; local, ready when its
            // pipeline's run at this commit succeeded, and run by its tag.
            let spec = match (&given.published, self.published_spec(given)) {
                (Some(_), Some(s)) => s,
                (Some(p), None) => {
                    let pipeline = given.pipeline.as_deref().unwrap_or("image");
                    self.published_locally(def, pipeline, revision).await.with_context(|| format!("artifact {name}"))?;
                    refs.push(ArtifactRef { name: name.clone(), reference: format!("{p}:{revision}"), repository: p.clone(), tag: Some(revision.to_owned()), digest: String::new() });
                    continue;
                }
                (None, _) => given.clone(),
            };
            let spec = &spec;
            let text = spec.reference(revision);
            let reference = oci::parse_reference(&text).with_context(|| format!("artifact {name}"))?;
            let registry = self.artifact_registry(spec, &reference).await.with_context(|| format!("artifact {name}"))?;
            let who = registry.credentials.as_ref().map(|(u, _)| u.as_str()).unwrap_or("");
            let key = format!("{}|{who}|{reference}", registry.name);
            let cached = self.artifacts.lock().get(&key).filter(|(_, at)| at.elapsed() < Duration::from_secs(60)).map(|(r, _)| r.clone());
            let resolved = match cached {
                Some(r) => r,
                None => {
                    let r = oci::resolve(&registry, &reference)
                        .await
                        .with_context(|| format!("artifact {name}"))?
                        .with_context(|| format!("waiting for {reference}: it isn't in {} yet", registry.name))?;
                    self.artifacts.lock().insert(key, (r.clone(), Instant::now()));
                    r
                }
            };
            for want in &spec.platforms {
                let has = resolved.platforms.is_empty() || resolved.platforms.iter().any(|p| p == want || p.starts_with(&format!("{want}/")));
                anyhow::ensure!(has, "artifact {name}: {reference} isn't built for {want} (it has {})", resolved.platforms.join(", "));
            }
            refs.push(ArtifactRef { name: name.clone(), reference: reference.pinned(&resolved.digest), repository: reference.name(), tag: reference.tag.clone(), digest: resolved.digest.clone() });
        }
        Ok((refs, self.pull_secrets_of(def, revision).await?))
    }

    /// The pull secrets an app's artifacts ask for, with their registries' credentials now.
    async fn pull_secrets_of(&self, def: &AppDef, revision: &str) -> Result<Vec<PullSecret>> {
        let mut out = Vec::new();
        for (name, given) in &def.artifacts {
            // A local published image is on the nodes: nothing to pull with.
            let spec = match (&given.published, self.published_spec(given)) {
                (Some(_), Some(s)) => s,
                (Some(_), None) => continue,
                (None, _) => given.clone(),
            };
            let spec = &spec;
            let Some(ps) = &spec.pull_secret else { continue };
            let reference = oci::parse_reference(&spec.reference(revision)).with_context(|| format!("artifact {name}"))?;
            let registry = self.artifact_registry(spec, &reference).await.with_context(|| format!("artifact {name}"))?;
            let config = oci::docker_config(&registry)
                .with_context(|| format!("artifact {name} asks for pull secret {ps}, but no registry integration holds credentials for {}", reference.registry))?;
            out.push(PullSecret { name: ps.clone(), config });
        }
        Ok(out)
    }

    /// A rolled-back app's artifacts: exactly what its deployment recorded; else (a deployment
    /// kept before artifacts were) its revision's now.
    async fn held_artifacts(&self, def: &AppDef, id: &str, held: &Resolved) -> Result<(Vec<ArtifactRef>, Vec<PullSecret>)> {
        let recorded = self
            .store
            .read(|d| d.pins.get(id).and_then(|p| d.history.get(id).and_then(|h| h.iter().find(|x| x.id == p.to)).map(|x| x.artifacts.clone())))
            .unwrap_or_default();
        self.recorded_artifacts(def, recorded, &held.revision).await
    }

    /// `recorded` when it covers every artifact the app has, else the revision's resolved now.
    pub(crate) async fn recorded_artifacts(&self, def: &AppDef, recorded: Vec<ArtifactRef>, revision: &str) -> Result<(Vec<ArtifactRef>, Vec<PullSecret>)> {
        if recorded.is_empty() || !def.artifacts.keys().all(|n| recorded.iter().any(|a| &a.name == n)) {
            return self.resolve_artifacts(def, revision).await;
        }
        Ok((recorded, self.pull_secrets_of(def, revision).await?))
    }

    /// Artifacts for a plan, without asking any registry (a planned commit's image may not be
    /// built yet): the app's own when the revision is one it resolved, else each image by its tag.
    pub(crate) fn planned_artifacts(&self, def: &AppDef, revision: &str) -> (Vec<ArtifactRef>, Vec<PullSecret>) {
        let apps = self.apps.read();
        let entry = apps.get(&def.id());
        if let Some(r) = entry.and_then(|e| e.resolved.iter().rev().find(|r| r.revision == revision && r.artifacts.len() == def.artifacts.len())) {
            return (r.artifacts.clone(), r.pull_secrets.clone());
        }
        let secrets = entry.and_then(|e| e.resolved.back().map(|r| r.pull_secrets.clone())).unwrap_or_default();
        let refs = def
            .artifacts
            .iter()
            .map(|(name, given)| {
                let spec = match (&given.published, self.published_spec(given)) {
                    (Some(_), Some(s)) => s,
                    (Some(p), None) => return ArtifactRef { name: name.clone(), reference: format!("{p}:{revision}"), repository: p.clone(), tag: Some(revision.to_owned()), digest: String::new() },
                    (None, _) => given.clone(),
                };
                let text = spec.reference(revision);
                let (repository, tag) = oci::parse_reference(&text).map(|r| (r.name(), r.tag.clone())).unwrap_or_else(|_| (text.clone(), None));
                ArtifactRef { name: name.clone(), reference: text, repository, tag, digest: String::new() }
            })
            .collect();
        (refs, secrets)
    }

    // ---------------------------------------------------------------- renders

    async fn render(self: Arc<Self>, key: u64, app: String) {
        let entry = self.apps.read().get(&app).cloned();
        let Some(entry) = entry else {
            self.input(Input::Rendered { key, result: Err(format!("no app {app}")) });
            return;
        };
        let cluster = self.clusters.read().get(&entry.def.destination.cluster).cloned();
        let (kube_version, api_versions) = match &cluster {
            Some(c) => (c.version.read().clone(), c.discovery.read().api_versions.clone()),
            None => (None, Vec::new()),
        };
        // The commits this key stands for, even if git moved since it was resolved.
        let inputs = entry.resolved.iter().find(|r| r.key == key).cloned().unwrap_or_else(|| Resolved {
            key,
            commits: entry.commits.clone(),
            deps: entry.deps.clone(),
            ..Resolved::default()
        });
        let this = self.clone();
        let def = entry.def.clone();
        let result = crate::rt::blocking(move || this.render_app(&def, &inputs, key, kube_version.as_deref(), &api_versions)).await.and_then(|r| r);
        match result {
            Ok((objects, children)) => {
                self.adopt_children(&app, children);
                self.input(Input::Rendered { key, result: Ok(objects) });
            }
            Err(e) => {
                tracing::warn!(app = %app, error = %format!("{e:#}"), "deployments: render failed");
                self.input(Input::Rendered { key, result: Err(format!("{e:#}")) });
            }
        }
    }

    /// Renders every source of an app into one list; Argo CD Applications among the objects are
    /// its children (app of apps), returned apart.
    fn render_app(&self, def: &AppDef, inputs: &Resolved, key: u64, kube_version: Option<&str>, api_versions: &[String]) -> Result<(Vec<Value>, Vec<Value>)> {
        // A directory of its own: a plan may render a key while the app renders it too.
        static VIEWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let views = self.dir.join("views").join(format!("{key:016x}-{n}"));
        let _ = std::fs::remove_dir_all(&views);
        // Ref sources' files, by name.
        let mut refs: BTreeMap<String, PathBuf> = BTreeMap::new();
        for (i, s) in def.sources.iter().enumerate() {
            let Some(name) = &s.as_ref else { continue };
            let commit = inputs.commits.get(i).cloned().flatten().context("a ref source's commit is unknown")?;
            let wanted: Vec<String> = inputs.deps.iter().flatten().filter(|d| d.source.as_deref() == Some(name)).map(|d| d.path.clone()).collect();
            let m = Mirror::open(&self.mirror_dir(&s.repo), &s.repo, self.git_auth(&s.repo))?;
            let root = views.join(format!("ref-{name}"));
            let wanted: Vec<&str> = wanted.iter().map(String::as_str).collect();
            m.extract(&commit, &wanted, &root)?;
            refs.insert(name.clone(), root);
        }
        // The revision's artifacts, filled in (`${{ artifacts.<name> }}`).
        let sources = apps::with_artifacts(&def.sources, &inputs.artifacts).map_err(|e| anyhow::anyhow!("{}", e.0))?;
        let mut objects = Vec::new();
        for (i, s) in sources.iter().enumerate() {
            if s.as_ref.is_some() && s.chart.is_none() {
                continue;
            }
            let root = views.join(format!("src-{i}"));
            let commit = inputs.commits.get(i).cloned().flatten().unwrap_or_else(|| s.revision.clone());
            if s.chart.is_none() {
                let m = Mirror::open(&self.mirror_dir(&s.repo), &s.repo, self.git_auth(&s.repo))?;
                let own: Vec<&str> = inputs.deps.get(i).into_iter().flatten().filter(|d| d.source.is_none()).map(|d| d.path.as_str()).collect();
                m.extract(&commit, &own, &root)?;
            }
            std::fs::create_dir_all(&root)?;
            let req = render::Request {
                source: s,
                root: &root,
                refs: &refs,
                app: &def.name,
                namespace: &def.destination.namespace,
                revision: &commit,
                kube_version,
                api_versions,
                scratch: &views.join("scratch"),
            };
            objects.extend(render::render(&req, &self.tools)?);
        }
        // Plain manifests read them too: `image: ${{ artifacts.web }}` in a Deployment.
        if !inputs.artifacts.is_empty() {
            for o in &mut objects {
                apps::substitute_value(o, &inputs.artifacts).map_err(|e| anyhow::anyhow!("{} {}: {}", obj::kind(o), obj::name(o), e.0))?;
            }
        }
        for p in &inputs.pull_secrets {
            objects.push(apps::pull_secret(&p.name, &def.destination.namespace, &p.config));
        }
        let _ = std::fs::remove_dir_all(&views);
        let (children, objects): (Vec<Value>, Vec<Value>) =
            objects.into_iter().partition(|o| obj::group(o) == "argoproj.io" && obj::kind(o) == "Application");
        Ok((objects, children))
    }

    /// Registers the Applications an app rendered as its children, and forgets the ones it no
    /// longer renders.
    fn adopt_children(&self, parent: &str, children: Vec<Value>) {
        let clusters = self.store.read(|d| d.clusters.clone());
        let by_url = |url: &str| clusters.values().find(|c| c.server.trim_end_matches('/') == url.trim_end_matches('/')).map(|c| c.name.clone());
        let mut ids = BTreeSet::new();
        for c in children {
            match apps::from_argo(&c, Some(parent), &by_url) {
                Ok(def) => {
                    ids.insert(def.id());
                    self.register(def);
                }
                Err(e) => tracing::warn!(parent = %parent, error = %e.0, "deployments: a rendered Application can't be read"),
            }
        }
        let (gone, prune) = {
            let mut apps = self.apps.write();
            let Some(p) = apps.get_mut(parent) else { return };
            let gone: Vec<String> = p.children.difference(&ids).cloned().collect();
            p.children = ids;
            (gone, p.def.sync.prune)
        };
        for g in gone {
            let cascade = prune && self.apps.read().get(&g).is_some_and(|e| e.def.cascade);
            self.unregister(&g, cascade);
        }
    }

    // ---------------------------------------------------------------- clusters

    /// The endpoint of a registered cluster.
    fn endpoint(&self, c: &ClusterDef) -> Result<kiln_cd::kube::Endpoint> {
        use kiln_cd::kube::{Auth, Endpoint, Exec};
        let base = || Endpoint {
            server: c.server.trim_end_matches('/').to_owned(),
            ca: c.ca.as_ref().map(|s| s.clone().into_bytes()),
            insecure: c.insecure,
            ..Endpoint::default()
        };
        Ok(match &c.auth {
            ClusterAuth::InCluster => kiln_cd::kube::in_cluster().context("kiln doesn't run in a Kubernetes pod")?,
            ClusterAuth::Token { token } => Endpoint { auth: Auth::Token(token.clone()), ..base() },
            ClusterAuth::Certificate { cert, key } => Endpoint { auth: Auth::Certificate { cert: cert.clone().into_bytes(), key: key.clone().into_bytes() }, ..base() },
            ClusterAuth::Exec { command, args, env } => Endpoint {
                auth: Auth::Exec(Exec {
                    command: command.clone(),
                    args: args.clone(),
                    env: env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                    api_version: "client.authentication.k8s.io/v1".into(),
                    provide_cluster_info: false,
                }),
                ..base()
            },
            ClusterAuth::Kubeconfig { path, context } => kubeconfig_endpoint(path, context)?,
        })
    }

    /// Every cluster to connect: the registered ones, and the one kiln runs in (or stands in for
    /// it, outside Kubernetes) as `in-cluster`.
    fn cluster_defs(&self) -> Vec<ClusterDef> {
        let mut defs: Vec<ClusterDef> = self.store.read(|d| d.clusters.values().cloned().collect());
        if !defs.iter().any(|c| c.name == apps::IN_CLUSTER) {
            if kiln_cd::kube::in_cluster().is_some() {
                defs.push(ClusterDef { name: apps::IN_CLUSTER.into(), server: apps::IN_CLUSTER_URL.into(), ..ClusterDef::default() });
            } else if !self.in_cluster_context.is_empty() {
                defs.push(ClusterDef {
                    name: apps::IN_CLUSTER.into(),
                    server: apps::IN_CLUSTER_URL.into(),
                    auth: ClusterAuth::Kubeconfig { path: self.kubeconfig.clone(), context: self.in_cluster_context.clone() },
                    ..ClusterDef::default()
                });
            }
        }
        defs
    }

    async fn connect_clusters(self: &Arc<Self>) {
        for def in self.cluster_defs() {
            if self.clusters.read().contains_key(&def.name) || !self.connecting.lock().insert(def.name.clone()) {
                continue;
            }
            let this = self.clone();
            crate::rt::spawn(async move { this.connect(def).await });
        }
    }

    async fn connect(self: Arc<Self>, def: ClusterDef) {
        let mut wait = Duration::from_secs(2);
        loop {
            let attempt = async {
                let ep = self.endpoint(&def)?;
                let cluster = Arc::new(Cluster::new(&def.name, ep)?);
                let d = cluster.discover().await?;
                Ok::<_, anyhow::Error>((cluster, d))
            };
            match attempt.await {
                Ok((cluster, d)) => {
                    tracing::info!(cluster = %def.name, kinds = d.kinds.len(), version = ?cluster.version.read(), "deployments: cluster connected");
                    self.input(Input::UpsertCluster { name: def.name.as_str().into(), qps: def.qps, inflight: def.inflight });
                    for (gk, r) in &d.kinds {
                        self.input(Input::Scope { cluster: def.name.as_str().into(), kind: gk.clone(), namespaced: r.namespaced });
                    }
                    self.clusters.write().insert(def.name.clone(), cluster.clone());
                    self.input(Input::ClusterReachable { name: def.name.as_str().into(), reachable: true });
                    // kiln's own cluster: a self-deploy in flight before this start finishes here.
                    if def.name == apps::IN_CLUSTER {
                        let this = self.clone();
                        crate::rt::spawn(async move { this.finish_self_deploy(&cluster).await });
                    }
                    self.connecting.lock().remove(&def.name);
                    self.kick();
                    return;
                }
                Err(e) => {
                    tracing::warn!(cluster = %def.name, error = %format!("{e:#}"), "deployments: can't reach a cluster yet");
                    self.input(Input::ClusterReachable { name: def.name.as_str().into(), reachable: false });
                }
            }
            if !self.hub.shutdown.sleep(wait).await {
                return;
            }
            wait = (wait * 2).min(Duration::from_secs(60));
        }
    }

    // ---------------------------------------------------------------- watches

    /// Objects of kiln's apps carry its label, so the API server filters for kiln; apps
    /// adopted from Argo CD don't, so a cluster with any of them is watched unfiltered.
    fn selector(&self, cluster: &str) -> Option<&'static str> {
        let any_argo = self.apps.read().values().any(|e| e.def.destination.cluster == cluster && e.def.tracking != Tracking::Kiln);
        (!any_argo).then(kiln_cd::kube::kiln_selector)
    }

    fn watch(self: &Arc<Self>, cluster: String, kind: GroupKind) {
        if !self.watched.lock().insert((cluster.clone(), kind.clone())) {
            return;
        }
        let this = self.clone();
        crate::rt::spawn(async move { this.watch_loop(cluster, kind).await });
    }

    async fn watch_loop(self: Arc<Self>, cluster: String, kind: GroupKind) {
        let name: obj::Str = cluster.as_str().into();
        let mut wait = Duration::from_secs(1);
        loop {
            if self.hub.shutdown.is_cancelled() {
                return;
            }
            let Some(c) = self.clusters.read().get(&cluster).cloned() else {
                if !self.hub.shutdown.sleep(Duration::from_secs(2)).await {
                    return;
                }
                continue;
            };
            if !self.serves(&c, &kind, Duration::ZERO).await {
                // The cluster serves no such kind (yet), so none of it exists: say so, or what
                // waits for the kind to be read waits forever. Look again later: its CRD may be
                // what a sync is about to apply.
                self.input(Input::Live { cluster: name.clone(), change: LiveChange::Listed { kind: kind.clone(), objects: Vec::new() } });
                if !self.hub.shutdown.sleep(wait).await {
                    return;
                }
                wait = (wait * 2).min(Duration::from_secs(10));
                continue;
            }
            let selector = self.selector(&cluster);
            let listed = c.list(&kind, selector).await;
            let (objects, mut rv) = match listed {
                Ok(x) => x,
                Err(e) => {
                    tracing::debug!(cluster = %cluster, kind = %kind, error = %format!("{e:#}"), "deployments: list failed");
                    if !self.hub.shutdown.sleep(wait).await {
                        return;
                    }
                    wait = (wait * 2).min(Duration::from_secs(60));
                    continue;
                }
            };
            wait = Duration::from_secs(1);
            self.input(Input::Live { cluster: name.clone(), change: LiveChange::Listed { kind: kind.clone(), objects } });
            // Watch from the list's version; a dropped stream resumes from the last version seen,
            // and only a 410 lists again.
            'watch: loop {
                let Ok(path) = c.watch_path(&kind, &rv, selector) else { return };
                let mut resp = match c.stream(&path).await {
                    Ok(r) if r.status().is_success() => r,
                    _ => break 'watch,
                };
                let mut buf: Vec<u8> = Vec::new();
                loop {
                    let chunk = crate::rt::timeout(Duration::from_secs(330), kube::next_chunk(resp.body_mut())).await;
                    let Ok(Ok(Some(bytes))) = chunk else { continue 'watch };
                    buf.extend_from_slice(&bytes);
                    for line in kiln_cd::kube::take_lines(&mut buf) {
                        match kiln_cd::kube::watch_event(&line) {
                            Some(kiln_cd::kube::WatchEvent::Upsert(v)) => {
                                if let Some(r) = obj::str_at(&v, &["metadata", "resourceVersion"]) {
                                    rv = r.to_owned();
                                }
                                self.input(Input::Live { cluster: name.clone(), change: LiveChange::Upsert(v) });
                            }
                            Some(kiln_cd::kube::WatchEvent::Delete(v)) => {
                                if let Some(r) = obj::str_at(&v, &["metadata", "resourceVersion"]) {
                                    rv = r.to_owned();
                                }
                                let key = ResKey::new(obj::group(&v), obj::kind(&v), obj::namespace(&v), obj::name(&v));
                                self.input(Input::Live { cluster: name.clone(), change: LiveChange::Delete(key) });
                            }
                            Some(kiln_cd::kube::WatchEvent::Bookmark(r)) => rv = r,
                            Some(kiln_cd::kube::WatchEvent::Expired) => break 'watch,
                            Some(kiln_cd::kube::WatchEvent::Error(e)) => {
                                tracing::debug!(cluster = %cluster, kind = %kind, error = %e, "deployments: watch error");
                                break 'watch;
                            }
                            None => {}
                        }
                    }
                    if self.hub.shutdown.is_cancelled() {
                        return;
                    }
                }
            }
        }
    }

    /// Whether `c` serves `kind`, discovering again when it doesn't seem to: a CRD applied since
    /// the last discovery serves new kinds. With `wait`, keeps looking that long (a CRD just
    /// applied takes a moment to be served). Discovery runs at most once a second per cluster,
    /// and what it finds goes to the engine.
    async fn serves(&self, c: &Arc<Cluster>, kind: &GroupKind, wait: Duration) -> bool {
        let until = Instant::now() + wait;
        loop {
            if c.knows(kind) {
                return true;
            }
            if c.claim_discovery(Duration::from_secs(1)) {
                match c.discover().await {
                    Ok(d) => {
                        for (gk, r) in &d.kinds {
                            self.input(Input::Scope { cluster: c.name.as_str().into(), kind: gk.clone(), namespaced: r.namespaced });
                        }
                        if c.knows(kind) {
                            return true;
                        }
                    }
                    Err(e) => tracing::debug!(cluster = %c.name, error = %format!("{e:#}"), "deployments: discovery failed"),
                }
            }
            if Instant::now() >= until || !self.hub.shutdown.sleep(Duration::from_millis(500)).await {
                return false;
            }
        }
    }

    // ---------------------------------------------------------------- actions

    async fn dispatch(self: Arc<Self>, rx: async_channel::Receiver<Action>) {
        while let Ok(a) = rx.recv().await {
            match a {
                Action::Render { key, app, .. } => {
                    let this = self.clone();
                    crate::rt::spawn(this.render(key, app.to_string()));
                }
                Action::Watch { cluster, kind } if kiln_cd::providers::is_target(&cluster) => self.target_watch(cluster.to_string(), kind),
                Action::Watch { cluster, kind } => self.watch(cluster.to_string(), kind),
                Action::Apply { id, cluster, object, dry_run, .. } if kiln_cd::providers::is_target(&cluster) => {
                    let this = self.clone();
                    crate::rt::spawn(async move {
                        let result = this.target_apply(&cluster, &object, dry_run).await;
                        this.input(Input::Answer { id, result });
                    });
                }
                Action::Delete { id, cluster, key, .. } if kiln_cd::providers::is_target(&cluster) => {
                    let this = self.clone();
                    crate::rt::spawn(async move {
                        let result = this.target_delete(&cluster, &key).await.map(|()| None);
                        this.input(Input::Answer { id, result });
                    });
                }
                Action::Apply { id, cluster, object, dry_run, manager, .. } => {
                    let this = self.clone();
                    crate::rt::spawn(async move {
                        let c = this.clusters.read().get(cluster.as_ref()).cloned();
                        let result = match c {
                            Some(c) => {
                                // A kind discovery doesn't know may be a CRD's this very sync just
                                // applied: look again, and give a real apply a moment for the CRD
                                // to be served. A dry run doesn't wait; the engine knows a miss
                                // the sync answers itself.
                                let gk = GroupKind::new(obj::group(&object), obj::kind(&object));
                                let wait = if dry_run { Duration::ZERO } else { Duration::from_secs(15) };
                                this.serves(&c, &gk, wait).await;
                                // kiln's own Deployment: a guard watches the restart first.
                                let guarded = if dry_run || cluster.as_ref() != apps::IN_CLUSTER { Ok(()) } else { this.before_self_apply(&object, &c).await };
                                match guarded {
                                    Ok(()) => c.apply(&object, &manager, dry_run).await.map(Some).map_err(|e| format!("{e:#}")),
                                    Err(e) => Err(format!("not deploying kiln itself without its guard: {e:#}")),
                                }
                            }
                            None => Err(format!("cluster {cluster} is not connected")),
                        };
                        this.input(Input::Answer { id, result });
                    });
                }
                Action::Delete { id, cluster, key, propagation } => {
                    let this = self.clone();
                    crate::rt::spawn(async move {
                        let c = this.clusters.read().get(cluster.as_ref()).cloned();
                        let result = match c {
                            Some(c) => c.delete(&key, propagation.as_str()).await.map(|_| None).map_err(|e| format!("{e:#}")),
                            None => Err(format!("cluster {cluster} is not connected")),
                        };
                        this.input(Input::Answer { id, result });
                    });
                }
                Action::Status(s) => {
                    let app = s.app.to_string();
                    self.statuses.write().insert(app.clone(), s);
                    self.dirty.lock().insert(app);
                }
                Action::Record(r) => {
                    if let kiln_cd::engine::Record::Started { app, op, revision, trigger, by, at_ms } = &r {
                        let s = guard::Started { op: *op, revision: revision.as_ref().map(|r| r.to_string()), trigger: trigger.as_str().to_owned(), by: by.clone(), at_ms: *at_ms };
                        self.started.lock().insert(app.to_string(), s);
                    }
                    self.record(r)
                }
            }
        }
    }

    fn record(&self, r: Record) {
        let Record::Finished { app, op, revision, key, content, trigger, by, succeeded, message, applied, pruned, at_ms } = r else { return };
        if !succeeded {
            tracing::info!(app = %app, op, message = %message, "deployments: a sync failed");
        }
        let (limit, commits, artifacts) = {
            let apps = self.apps.read();
            let e = apps.get(app.as_ref());
            let r = key.and_then(|k| e.and_then(|e| e.resolved.iter().find(|r| r.key == k)));
            let commits = r.map(|r| r.commits.clone()).unwrap_or_default();
            let artifacts = r.map(|r| r.artifacts.clone()).unwrap_or_default();
            (e.map(|e| e.def.history_limit).unwrap_or(10), commits, artifacts)
        };
        let d = Deployment {
            app: app.to_string(),
            op,
            revision: revision.map(|r| r.to_string()),
            key,
            trigger: trigger.as_str().to_owned(),
            by,
            succeeded,
            message,
            applied,
            pruned,
            at_ms,
            id: 0,
            commits,
            content,
            artifacts,
        };
        let rx = self.store.submit(Command::Deployed { deployment: Box::new(d), limit });
        crate::rt::spawn(async move {
            if let Ok(Err(e)) = rx.await {
                tracing::warn!(error = %e, "deployments: recording a deployment failed");
            }
        });
    }

    /// Publishes changed statuses at most every 300 ms, in one event.
    async fn flush_loop(self: Arc<Self>) {
        while self.hub.shutdown.sleep(Duration::from_millis(300)).await {
            let dirty: Vec<String> = std::mem::take(&mut *self.dirty.lock()).into_iter().collect();
            if dirty.is_empty() {
                continue;
            }
            let statuses = self.statuses.read();
            let apps: Vec<Value> = dirty.iter().filter_map(|a| statuses.get(a)).map(summary_json).collect();
            drop(statuses);
            for chunk in apps.chunks(500) {
                self.hub.emit(&json!({"type": "cd_apps", "apps": chunk}));
            }
        }
    }
}

/// Apps adopted from Argo CD are shadowed (compared, never written) until they are taken over.
pub fn is_shadow(def: &AppDef) -> bool {
    matches!(def.origin, Origin::Argo { .. }) && def.labels.get("kiln.dev/take-over").map(String::as_str) != Some("true")
}

/// An app's status as the API and events show it.
pub fn summary_json(s: &Summary) -> Value {
    json!({
        "app": s.app.as_ref(),
        "sync": s.sync.as_str(),
        "health": s.health.as_str(),
        "revision": s.revision.as_deref(),
        "resources": s.resources,
        "out_of_sync": s.out_of_sync,
        "message": s.message,
        "operation": s.operation.as_ref().map(|o| json!({
            "id": o.id,
            "phase": format!("{:?}", o.phase),
            "trigger": o.trigger.as_str(),
            "attempt": o.attempt,
            "message": o.message,
            "by": o.by,
            "started_ms": o.started_ms,
            "finished_ms": o.finished_ms,
        })),
    })
}

/// A commit's first seven characters.
fn short(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

fn blake(parts: &[&[u8]]) -> u64 {
    let mut joined = Vec::new();
    for p in parts {
        joined.extend_from_slice(&(p.len() as u64).to_le_bytes());
        joined.extend_from_slice(p);
    }
    obj::hash_bytes(&joined)
}

/// The endpoint of a kubeconfig context (the default kubeconfig when `path` is empty).
pub fn kubeconfig_endpoint(path: &str, context: &str) -> Result<kiln_cd::kube::Endpoint> {
    let path = if path.is_empty() {
        std::env::var("KUBECONFIG")
            .ok()
            .and_then(|p| p.split(':').next().map(PathBuf::from))
            .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".kube/config")))
            .context("no kubeconfig: set KUBECONFIG or give a path")?
    } else {
        PathBuf::from(path)
    };
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    kiln_cd::kube::kubeconfig(&text, context, path.parent().unwrap_or(std::path::Path::new(".")))
}

fn engine_thread(rx: std::sync::mpsc::Receiver<ToEngine>, out: async_channel::Sender<Action>, cfg: EngineConfig) {
    use std::sync::mpsc::RecvTimeoutError;
    let mut engine = Engine::new(cfg);
    let mut actions = Vec::new();
    loop {
        let wait = engine.deadline().map(|d| d.saturating_sub(now_ms())).unwrap_or(1_000).clamp(1, 1_000);
        match rx.recv_timeout(Duration::from_millis(wait)) {
            Ok(ToEngine::Input(i)) => engine.handle(now_ms(), *i),
            Ok(ToEngine::Query(f)) => f(&engine),
            Err(RecvTimeoutError::Timeout) => engine.handle(now_ms(), Input::Tick),
            Err(RecvTimeoutError::Disconnected) => return,
        }
        // Take whatever else is waiting before answering, so bursts are handled together.
        for _ in 0..4096 {
            match rx.try_recv() {
                Ok(ToEngine::Input(i)) => engine.handle(now_ms(), *i),
                Ok(ToEngine::Query(f)) => f(&engine),
                Err(_) => break,
            }
        }
        engine.drain(&mut actions);
        for a in actions.drain(..) {
            if out.send_blocking(a).is_err() {
                return;
            }
        }
    }
}

/// Starts the service and runs until the server stops.
pub async fn run(hub: Arc<Hub>) -> Result<()> {
    let cfg = hub.cfg.cd.clone();
    let dir = hub.cfg.server.data_dir.join("cd");
    let store_dir = dir.join("store");
    let store = crate::rt::blocking(move || Store::open(&store_dir, 1, &[1])).await??;
    let changes = store.changes();
    let (etx, erx) = std::sync::mpsc::channel();
    let (atx, arx) = async_channel::unbounded();
    let ecfg = EngineConfig { wave_delay_ms: cfg.wave_delay_ms, cluster_qps: cfg.qps, cluster_burst: cfg.qps * 2.0, ..EngineConfig::default() };
    std::thread::Builder::new().name("kiln-cd-engine".into()).spawn(move || engine_thread(erx, atx, ecfg))?;
    let tools = Tools {
        helm: cfg.helm.clone().into(),
        kubectl: cfg.kubectl.clone().into(),
        kustomize: (!cfg.kustomize.is_empty()).then(|| cfg.kustomize.clone().into()),
        kustomize_options: cfg.kustomize_options.clone(),
        timeout: Duration::from_secs(cfg.render_timeout_seconds.max(5)),
        helm_home: Some(dir.join("helm")),
        ..Tools::default()
    };
    let cd = Arc::new(Cd {
        hub: hub.clone(),
        store,
        engine: etx,
        apps: RwLock::new(BTreeMap::new()),
        clusters: RwLock::new(BTreeMap::new()),
        statuses: RwLock::new(BTreeMap::new()),
        dirty: Mutex::new(BTreeSet::new()),
        watched: Mutex::new(BTreeSet::new()),
        connecting: Mutex::new(BTreeSet::new()),
        inputs_gate: Mutex::new(()),
        dir,
        tools,
        poll: Duration::from_secs(cfg.poll_seconds.max(5)),
        in_cluster_context: cfg.in_cluster_context.clone(),
        kubeconfig: cfg.kubeconfig.clone(),
        kick: crate::rt::Notify::new(),
        live: live::Live::default(),
        plans: plans::Renders::default(),
        artifacts: Mutex::new(HashMap::new()),
        deploy_errors: Mutex::new(BTreeMap::new()),
        environments: RwLock::new(BTreeMap::new()),
        started: Mutex::new(HashMap::new()),
        targets: RwLock::new(BTreeMap::new()),
        target_errors: Mutex::new(BTreeMap::new()),
        repo_names: RwLock::new(HashMap::new()),
    });
    *CD.write() = Some(cd.clone());
    // Apps kept in kiln, then the clusters they deploy to.
    let kept: Vec<AppDef> = cd.store.read(|d| d.apps.values().cloned().collect());
    for def in kept {
        cd.register(def);
    }
    cd.connect_clusters().await;
    crate::rt::spawn(cd.clone().dispatch(arx));
    crate::rt::spawn(cd.clone().poll_loop());
    crate::rt::spawn(cd.clone().flush_loop());
    // The store's changes (from this server or, later, other members) reach the registry.
    if let Some(changes) = changes {
        let (ctx, crx) = async_channel::unbounded();
        std::thread::Builder::new().name("kiln-cd-changes".into()).spawn(move || {
            while let Ok(c) = changes.recv() {
                if ctx.send_blocking(c).is_err() {
                    return;
                }
            }
        })?;
        let c2 = cd.clone();
        crate::rt::spawn(async move {
            while let Ok(change) = crx.recv().await {
                match change {
                    kiln_cd::state::Change::App(id) => {
                        if let Some(def) = c2.store.read(|d| d.apps.get(&id).cloned()) {
                            c2.register(def);
                        }
                    }
                    kiln_cd::state::Change::AppDeleted(id) => c2.unregister(&id, false),
                    kiln_cd::state::Change::Cluster(_) => c2.connect_clusters().await,
                    kiln_cd::state::Change::ClusterDeleted(name) => {
                        c2.clusters.write().remove(&name);
                    }
                    kiln_cd::state::Change::Pinned(_) => c2.kick(),
                    _ => {}
                }
            }
        });
    }
    // Agents kiln starts on this machine get its MCP server and skills.
    let port = hub.cfg.server.http_addr.rsplit_once(':').map(|(_, p)| p.to_owned()).unwrap_or_else(|| "7070".into());
    if let Err(e) = crate::mcp::init_kit(&hub, &format!("http://127.0.0.1:{port}")) {
        tracing::warn!(error = %format!("{e:#}"), "agents won't be given kiln's MCP server and skills");
    }
    tracing::info!(apps = cd.apps.read().len(), "deployments ready");
    let _ = hub.shutdown.cancelled().await;
    *CD.write() = None;
    Ok(())
}
