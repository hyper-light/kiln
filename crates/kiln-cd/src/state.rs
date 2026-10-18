//! What deployments keep durably, and the state machine that keeps it.
//!
//! Only what can't be recomputed is kept: apps made through the API (apps from git are read
//! again from git), clusters, repositories, projects, settings, and each app's deployment
//! history. Renders, live objects and comparisons are soft state the engine rebuilds. Every
//! change is a [`Command`], replicated by hyper-raft through hyper-durable ([`crate::store`]) and
//! applied here in log order on every member.
//!
//! The applied state is shared with readers under a lock ([`Data`]), never copied per change;
//! [`Machine::persist`] writes it as an image so the log can be compacted behind it.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hyper_durable::{EntryRef, Fatal, Point, StateMachine};
use hyper_raft::proto::{ConfChangeV2, ConfState};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::apps::AppDef;

/// How kiln reaches a cluster's API server.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum ClusterMode {
    /// Over the network, with the cluster's credentials.
    #[default]
    Direct,
    /// Through a kiln runner inside the cluster, over its QUIC connection.
    Runner {
        /// The runner's name.
        runner: String,
    },
    /// A runner in the cluster watches and applies, and sends kiln what changed.
    Delegated {
        /// The runner's name.
        runner: String,
    },
}

/// How kiln authenticates to a cluster.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum ClusterAuth {
    /// The service account of the pod kiln runs in.
    #[default]
    InCluster,
    /// A bearer token.
    Token {
        /// The token.
        token: String,
    },
    /// A client certificate.
    Certificate {
        /// PEM certificate.
        cert: String,
        /// PEM private key.
        key: String,
    },
    /// A credential plugin (`aws eks get-token`, `gke-gcloud-auth-plugin`, `kubelogin`).
    Exec {
        /// The program.
        command: String,
        /// Its arguments.
        args: Vec<String>,
        /// Its environment.
        env: BTreeMap<String, String>,
    },
    /// A context of a kubeconfig file on the server.
    Kubeconfig {
        /// The file (the default kubeconfig when empty).
        path: String,
        /// The context (the current one when empty).
        context: String,
    },
}

/// A cluster kiln deploys to.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct ClusterDef {
    /// kiln's name for it.
    pub name: String,
    /// Its API server's URL (Argo CD's `server`); empty for the cluster kiln runs in.
    pub server: String,
    /// How kiln reaches it.
    pub mode: ClusterMode,
    /// How kiln authenticates.
    pub auth: ClusterAuth,
    /// Its CA, PEM, when not the system's.
    pub ca: Option<String>,
    /// Skip TLS verification.
    pub insecure: bool,
    /// Labels, for fleets and environments.
    pub labels: BTreeMap<String, String>,
    /// The only namespaces kiln may touch, when set.
    pub namespaces: Vec<String>,
    /// Requests a second kiln may send it.
    pub qps: Option<f64>,
    /// Requests kiln may have in flight with it.
    pub inflight: Option<u32>,
}

/// What a repository is.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum RepoKind {
    /// A git repository.
    #[default]
    Git,
    /// A Helm chart repository.
    Helm,
    /// An OCI registry.
    Oci,
}

/// A repository with credentials, or with settings of its own.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct RepoDef {
    /// Its URL, or a URL prefix (Argo CD's credential templates).
    pub url: String,
    /// What it is.
    pub kind: RepoKind,
    /// User name for HTTPS.
    pub username: Option<String>,
    /// Password or token for HTTPS.
    pub password: Option<String>,
    /// SSH private key.
    pub ssh_key: Option<String>,
    /// Skip TLS verification.
    pub insecure: bool,
    /// The project allowed to use it, if only one.
    pub project: Option<String>,
}

/// A group and kind pattern (`*` for any).
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct KindPattern {
    /// Group.
    pub group: String,
    /// Kind.
    pub kind: String,
}

/// A destination pattern.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct DestinationPattern {
    /// Cluster name glob.
    pub cluster: String,
    /// Namespace glob.
    pub namespace: String,
}

/// A project: what its apps may deploy, from where, to where.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct ProjectDef {
    /// Name.
    pub name: String,
    /// What it is for.
    pub description: String,
    /// Repository URL globs its apps may use (`*` for any).
    pub source_repos: Vec<String>,
    /// Where its apps may deploy.
    pub destinations: Vec<DestinationPattern>,
    /// Cluster-scoped kinds its apps may create (none if empty, Argo CD's default).
    pub cluster_resources: Vec<KindPattern>,
    /// Namespaced kinds its apps may not create.
    pub namespace_resources_denied: Vec<KindPattern>,
}

/// A finished sync: one entry of an app's history.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Deployment {
    /// The app.
    pub app: String,
    /// The sync.
    pub op: u64,
    /// The revision deployed.
    pub revision: Option<String>,
    /// The render key deployed (enough to roll back to it).
    pub key: Option<u64>,
    /// Why it ran (`manual`, `auto`, `self-heal`, `rollback`, `promotion`).
    pub trigger: String,
    /// Who asked, if a person or agent did.
    pub by: Option<String>,
    /// Whether it succeeded.
    pub succeeded: bool,
    /// Its last message.
    pub message: String,
    /// Objects applied.
    pub applied: u32,
    /// Objects pruned.
    pub pruned: u32,
    /// When it finished (ms since the epoch).
    pub at_ms: u64,
    /// Its number in the app's history (1, 2, ...), given when it is recorded.
    #[serde(default)]
    pub id: u64,
    /// The artifacts it deployed, pinned (docs/cd-targets.md §1.3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<crate::apps::ArtifactRef>,
    /// Each source's commit: what a rollback renders again.
    #[serde(default)]
    pub commits: Vec<Option<String>>,
    /// What git held for the app: its inputs' content address, the cluster's part left out.
    #[serde(default)]
    pub content: Option<u64>,
}

/// An app held at an earlier deployment (a rollback) until git changes what it renders.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Pin {
    /// The deployment it went back to (its history id).
    pub to: u64,
    /// That deployment's revision.
    pub revision: Option<String>,
    /// Each source's commit it holds.
    pub commits: Vec<Option<String>>,
    /// What git held for the app when it was pinned: the content address of its inputs, the
    /// cluster's part left out. When that moves, the app follows git again.
    pub head: u64,
    /// Who rolled it back.
    pub by: Option<String>,
    /// When (ms since the epoch).
    pub at_ms: u64,
}

/// A change to what is kept.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum Command {
    /// Create or replace an app kept in kiln.
    PutApp(Box<AppDef>),
    /// Delete an app kept in kiln (by id).
    DeleteApp(String),
    /// Create or replace a cluster.
    PutCluster(Box<ClusterDef>),
    /// Delete a cluster.
    DeleteCluster(String),
    /// Create or replace a repository.
    PutRepo(Box<RepoDef>),
    /// Delete a repository.
    DeleteRepo(String),
    /// Create or replace a project.
    PutProject(Box<ProjectDef>),
    /// Delete a project.
    DeleteProject(String),
    /// Record a finished sync, keeping at most `limit` per app.
    Deployed {
        /// The record.
        deployment: Box<Deployment>,
        /// History kept for the app.
        limit: u32,
    },
    /// Set a setting.
    PutSetting(String, Value),
    /// Hold an app at an earlier deployment (a rollback), or let it follow git again.
    Pin {
        /// The app.
        app: String,
        /// The hold; none to follow git.
        pin: Option<Box<Pin>>,
    },
    /// Keep what kiln last applied to a provider's object, or forget it once the object is gone
    /// (docs/cd-targets.md §2.4).
    Applied {
        /// The target (its cluster name).
        target: String,
        /// The object: [`applied_key`].
        key: String,
        /// The record; none to forget it.
        applied: Option<Box<Applied>>,
    },
}

/// What kiln last applied to a provider's object (docs/cd-targets.md §2.4). Platforms have no
/// server-side apply, so kiln keeps the object it applied, as `kubectl apply` keeps its
/// last-applied configuration: the fields kiln owns, and its metadata, read back onto the live
/// object. `made` is what the platform made of it (an ARN, a code hash), so a provider can tell
/// whether the platform still holds what kiln made.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Applied {
    /// The desired object kiln applied.
    pub object: Value,
    /// What the platform made of it.
    #[serde(default)]
    pub made: Value,
    /// When.
    pub at_ms: u64,
}

/// A provider object's key in [`Data::applied`]: `group/kind/namespace/name`.
pub fn applied_key(group: &str, kind: &str, namespace: &str, name: &str) -> String {
    format!("{group}/{kind}/{namespace}/{name}")
}

/// Something that changed, for whoever follows the store.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Change {
    /// An app was put (by id).
    App(String),
    /// An app was deleted.
    AppDeleted(String),
    /// A cluster was put.
    Cluster(String),
    /// A cluster was deleted.
    ClusterDeleted(String),
    /// A repository was put or deleted.
    Repo(String),
    /// A project was put or deleted.
    Project(String),
    /// A deployment was recorded for an app.
    Deployed(String),
    /// A setting changed.
    Setting(String),
    /// An app was held at an earlier deployment, or let go.
    Pinned(String),
    /// What kiln applied to a target's object changed (by target).
    Applied(String),
}

/// What is kept.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Data {
    /// Apps kept in kiln, by id.
    pub apps: BTreeMap<String, AppDef>,
    /// Clusters by name.
    pub clusters: BTreeMap<String, ClusterDef>,
    /// Repositories by URL.
    pub repos: BTreeMap<String, RepoDef>,
    /// Projects by name.
    pub projects: BTreeMap<String, ProjectDef>,
    /// Each app's deployments, oldest first.
    pub history: BTreeMap<String, VecDeque<Deployment>>,
    /// Settings by key.
    pub settings: BTreeMap<String, Value>,
    /// Apps held at an earlier deployment, by id.
    #[serde(default)]
    pub pins: BTreeMap<String, Pin>,
    /// What kiln last applied to each provider object, by target, then [`applied_key`].
    #[serde(default)]
    pub applied: BTreeMap<String, BTreeMap<String, Applied>>,
}

impl Data {
    /// Applies a command, saying what changed.
    pub fn apply(&mut self, cmd: Command, changes: &mut Vec<Change>) -> Result<(), String> {
        match cmd {
            Command::PutApp(app) => {
                if app.name.is_empty() {
                    return Err("an app needs a name".into());
                }
                let id = app.id();
                self.apps.insert(id.clone(), *app);
                changes.push(Change::App(id));
            }
            Command::DeleteApp(id) => {
                if self.apps.remove(&id).is_none() {
                    return Err(format!("no app {id}"));
                }
                self.history.remove(&id);
                self.pins.remove(&id);
                changes.push(Change::AppDeleted(id));
            }
            Command::PutCluster(c) => {
                if c.name.is_empty() {
                    return Err("a cluster needs a name".into());
                }
                let name = c.name.clone();
                self.clusters.insert(name.clone(), *c);
                changes.push(Change::Cluster(name));
            }
            Command::DeleteCluster(name) => {
                if self.apps.values().any(|a| a.destination.cluster == name) {
                    return Err(format!("apps still deploy to cluster {name}"));
                }
                if self.clusters.remove(&name).is_none() {
                    return Err(format!("no cluster {name}"));
                }
                changes.push(Change::ClusterDeleted(name));
            }
            Command::PutRepo(r) => {
                let url = r.url.clone();
                self.repos.insert(url.clone(), *r);
                changes.push(Change::Repo(url));
            }
            Command::DeleteRepo(url) => {
                self.repos.remove(&url).ok_or_else(|| format!("no repository {url}"))?;
                changes.push(Change::Repo(url));
            }
            Command::PutProject(p) => {
                let name = p.name.clone();
                self.projects.insert(name.clone(), *p);
                changes.push(Change::Project(name));
            }
            Command::DeleteProject(name) => {
                if self.apps.values().any(|a| a.project == name) {
                    return Err(format!("apps still belong to project {name}"));
                }
                self.projects.remove(&name).ok_or_else(|| format!("no project {name}"))?;
                changes.push(Change::Project(name));
            }
            Command::Deployed { deployment, limit } => {
                let app = deployment.app.clone();
                let h = self.history.entry(app.clone()).or_default();
                // Entries are numbered in order; ones kept before numbering get theirs first.
                if h.iter().any(|d| d.id == 0) {
                    for (n, d) in h.iter_mut().enumerate() {
                        d.id = n as u64 + 1;
                    }
                }
                let mut deployment = *deployment;
                deployment.id = h.back().map_or(0, |d| d.id) + 1;
                h.push_back(deployment);
                // Failures go first, so a run of failed syncs never pushes out the deployments a
                // rollback needs; the newest entry always stays (automation goes on from it).
                while h.len() > limit.max(1) as usize {
                    let newest = h.len().saturating_sub(1);
                    match h.iter().position(|d| !d.succeeded).filter(|&i| i < newest) {
                        Some(i) => {
                            h.remove(i);
                        }
                        None => {
                            h.pop_front();
                        }
                    }
                }
                changes.push(Change::Deployed(app));
            }
            Command::PutSetting(key, value) => {
                self.settings.insert(key.clone(), value);
                changes.push(Change::Setting(key));
            }
            Command::Pin { app, pin } => {
                match pin {
                    Some(p) => {
                        self.pins.insert(app.clone(), *p);
                    }
                    None => {
                        self.pins.remove(&app);
                    }
                }
                changes.push(Change::Pinned(app));
            }
            Command::Applied { target, key, applied } => {
                match applied {
                    Some(a) => {
                        self.applied.entry(target.clone()).or_default().insert(key, *a);
                    }
                    None => {
                        if let Some(m) = self.applied.get_mut(&target) {
                            m.remove(&key);
                            if m.is_empty() {
                                self.applied.remove(&target);
                            }
                        }
                    }
                }
                changes.push(Change::Applied(target));
            }
        }
        Ok(())
    }
}

/// The answer to a proposal: its request id, and whether it applied.
pub type Answer = (u64, Result<(), String>);

#[derive(Serialize, Deserialize)]
struct Configuration {
    voters: Vec<u64>,
    learners: Vec<u64>,
    voters_outgoing: Vec<u64>,
    learners_next: Vec<u64>,
    auto_leave: bool,
}

impl From<&ConfState> for Configuration {
    fn from(c: &ConfState) -> Self {
        Self {
            voters: c.voters.clone(),
            learners: c.learners.clone(),
            voters_outgoing: c.voters_outgoing.clone(),
            learners_next: c.learners_next.clone(),
            auto_leave: c.auto_leave,
        }
    }
}

impl From<Configuration> for ConfState {
    fn from(c: Configuration) -> Self {
        ConfState {
            voters: c.voters,
            learners: c.learners,
            voters_outgoing: c.voters_outgoing,
            learners_next: c.learners_next,
            auto_leave: c.auto_leave,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Image {
    index: u64,
    term: u64,
    configuration: Configuration,
    data: Data,
}

/// The state machine: [`Data`] in log order, persisted as an image file.
pub struct Machine {
    data: Arc<RwLock<Data>>,
    applied: Point,
    durable: Point,
    configuration: ConfState,
    file: PathBuf,
    changes: Vec<Change>,
}

impl Machine {
    /// The machine whose image is at `file`, or a new one of `configuration` if there is none.
    pub fn open(file: &Path, configuration: ConfState) -> anyhow::Result<Machine> {
        let mut m = Machine {
            data: Arc::new(RwLock::new(Data::default())),
            applied: Point { index: 0, term: 0 },
            durable: Point { index: 0, term: 0 },
            configuration,
            file: file.to_owned(),
            changes: Vec::new(),
        };
        if let Ok(bytes) = std::fs::read(file) {
            let image: Image = serde_json::from_slice(&bytes)?;
            m.applied = Point { index: image.index, term: image.term };
            m.durable = m.applied;
            m.configuration = image.configuration.into();
            *m.data.write() = image.data;
        }
        Ok(m)
    }

    /// The shared data.
    pub fn data(&self) -> Arc<RwLock<Data>> {
        self.data.clone()
    }

    /// Takes what changed since the last call.
    pub fn take_changes(&mut self) -> Vec<Change> {
        std::mem::take(&mut self.changes)
    }

    /// The last entry applied.
    pub fn applied(&self) -> Point {
        self.applied
    }

    /// Writes what is applied now (on a clean stop), so the next start replays nothing.
    pub fn persist_now(&mut self) {
        if let Err(e) = StateMachine::persist(self) {
            tracing::warn!("deployments: writing the state image: {}", e.0);
        }
    }

    fn write_image(&self, at: Point, data: &Data) -> std::io::Result<()> {
        let image = ImageRef { index: at.index, term: at.term, configuration: (&self.configuration).into(), data };
        let bytes = serde_json::to_vec(&image).map_err(std::io::Error::other)?;
        let tmp = self.file.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        restrict(&tmp);
        std::fs::rename(&tmp, &self.file)?;
        if let Some(dir) = self.file.parent().and_then(|d| std::fs::File::open(d).ok()) {
            dir.sync_all().ok();
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct ImageRef<'a> {
    index: u64,
    term: u64,
    configuration: Configuration,
    data: &'a Data,
}

/// The image holds credentials: only its owner reads it.
fn restrict(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).ok();
    }
    #[cfg(not(unix))]
    let _ = path;
}

impl StateMachine for Machine {
    type Answer = Answer;

    fn apply(&mut self, entry: &EntryRef<'_>, answers: &mut Vec<Answer>) -> Result<(), Fatal> {
        if entry.index != self.applied.index + 1 {
            return Err(Fatal("an entry applied out of order"));
        }
        self.applied = Point { index: entry.index, term: entry.term };
        // A new leader's empty entry, and anything without a request, answers no one.
        let id = entry.context.get(..8).and_then(|b| b.try_into().ok()).map(u64::from_le_bytes).unwrap_or(0);
        if entry.data.is_empty() {
            return Ok(());
        }
        let result = match serde_json::from_slice::<Command>(entry.data) {
            Ok(cmd) => self.data.write().apply(cmd, &mut self.changes),
            Err(e) => Err(format!("unreadable command: {e}")),
        };
        if id != 0 {
            answers.push((id, result));
        }
        Ok(())
    }

    fn apply_change(&mut self, at: Point, _change: &ConfChangeV2, configuration: &ConfState) -> Result<(), Fatal> {
        if at.index != self.applied.index + 1 {
            return Err(Fatal("a change applied out of order"));
        }
        self.configuration = configuration.clone();
        self.applied = at;
        Ok(())
    }

    fn durable(&self) -> Point {
        self.durable
    }

    fn configuration(&self) -> &ConfState {
        &self.configuration
    }

    fn acts_at_start(&self, _entry: &EntryRef<'_>) -> bool {
        false
    }

    fn image(&mut self, into: &mut Vec<u8>) -> Result<(Point, ConfState), Fatal> {
        let data = self.data.read();
        let image = ImageRef { index: self.applied.index, term: self.applied.term, configuration: (&self.configuration).into(), data: &data };
        serde_json::to_writer(&mut *into, &image).map_err(|_| Fatal("the state could not be imaged"))?;
        Ok((self.applied, self.configuration.clone()))
    }

    fn image_bytes(&self) -> Option<u64> {
        let data = self.data.read();
        serde_json::to_vec(&*data).ok().map(|b| b.len() as u64 + 128)
    }

    fn install(&mut self, image: &[u8], at: Point, configuration: &ConfState) -> Result<(), Fatal> {
        let img: Image = serde_json::from_slice(image).map_err(|_| Fatal("an image that does not read"))?;
        *self.data.write() = img.data;
        self.configuration = configuration.clone();
        self.applied = at;
        {
            let data = self.data.read();
            self.write_image(at, &data).map_err(|_| Fatal("the installed image could not be written"))?;
        }
        self.durable = at;
        self.changes.push(Change::Setting("*".into()));
        Ok(())
    }

    fn persist(&mut self) -> Result<(), Fatal> {
        if self.durable == self.applied {
            return Ok(());
        }
        let at = self.applied;
        {
            let data = self.data.read();
            self.write_image(at, &data).map_err(|_| Fatal("the state could not be written"))?;
        }
        self.durable = at;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::{Destination, Origin, Source};

    pub(crate) fn app(name: &str) -> AppDef {
        AppDef {
            name: name.into(),
            project: "default".into(),
            sources: vec![Source { repo: "r".into(), path: "p".into(), revision: "main".into(), ..Source::default() }],
            destination: Destination { cluster: "in-cluster".into(), namespace: "web".into() },
            sync: Default::default(),
            ignore: Vec::new(),
            tracking: Default::default(),
            origin: Origin::Api,
            environment: None,
            labels: Default::default(),
            history_limit: 10,
            cascade: true,
            managed_namespace: None,
            artifacts: BTreeMap::new(),
            rollout: None,
            checks: BTreeMap::new(),
        }
    }

    #[test]
    fn deployments_are_numbered_and_pins_come_and_go() {
        let mut d = Data::default();
        let mut ch = Vec::new();
        assert!(d.apply(Command::PutCluster(Box::new(ClusterDef { name: "in-cluster".into(), ..ClusterDef::default() })), &mut ch).is_ok());
        assert!(d.apply(Command::PutApp(Box::new(app("web"))), &mut ch).is_ok());
        // One kept before numbering: numbered when the next comes.
        d.history.entry("default/web".into()).or_default().push_back(Deployment { app: "default/web".into(), succeeded: true, ..Deployment::default() });
        for op in 1..3 {
            let dep = Deployment { app: "default/web".into(), op, succeeded: true, ..Deployment::default() };
            assert!(d.apply(Command::Deployed { deployment: Box::new(dep), limit: 10 }, &mut ch).is_ok());
        }
        let ids: Vec<u64> = d.history.get("default/web").map(|h| h.iter().map(|x| x.id).collect()).unwrap_or_default();
        assert_eq!(ids, [1, 2, 3]);
        let pin = Pin { to: 1, ..Pin::default() };
        assert!(d.apply(Command::Pin { app: "default/web".into(), pin: Some(Box::new(pin)) }, &mut ch).is_ok());
        assert_eq!(d.pins.get("default/web").map(|p| p.to), Some(1));
        assert!(d.apply(Command::Pin { app: "default/web".into(), pin: None }, &mut ch).is_ok());
        assert!(d.pins.is_empty());
        assert!(d.apply(Command::Pin { app: "default/web".into(), pin: Some(Box::new(Pin::default())) }, &mut ch).is_ok());
        assert!(d.apply(Command::DeleteApp("default/web".into()), &mut ch).is_ok());
        assert!(d.pins.is_empty());
        assert_eq!(ch.last(), Some(&Change::AppDeleted("default/web".into())));
    }

    #[test]
    fn failed_syncs_never_push_out_successful_ones() {
        let mut d = Data::default();
        let mut ch = Vec::new();
        for (op, succeeded) in [(0, true), (1, true), (2, true), (3, false), (4, false), (5, false)] {
            let dep = Deployment { app: "default/web".into(), op, succeeded, ..Deployment::default() };
            assert!(d.apply(Command::Deployed { deployment: Box::new(dep), limit: 3 }, &mut ch).is_ok());
        }
        let ops: Vec<u64> = d.history.get("default/web").map(|h| h.iter().map(|x| x.op).collect()).unwrap_or_default();
        assert_eq!(ops, [1, 2, 5]);
    }

    #[test]
    fn commands_apply_and_refuse_what_would_dangle() {
        let mut d = Data::default();
        let mut ch = Vec::new();
        assert!(d.apply(Command::PutCluster(Box::new(ClusterDef { name: "in-cluster".into(), ..ClusterDef::default() })), &mut ch).is_ok());
        assert!(d.apply(Command::PutApp(Box::new(app("web"))), &mut ch).is_ok());
        assert!(d.apply(Command::DeleteCluster("in-cluster".into()), &mut ch).is_err(), "an app still deploys there");
        for i in 0..15 {
            let dep = Deployment { app: "default/web".into(), op: i, succeeded: true, ..Deployment::default() };
            assert!(d.apply(Command::Deployed { deployment: Box::new(dep), limit: 10 }, &mut ch).is_ok());
        }
        assert_eq!(d.history.get("default/web").map(|h| (h.len(), h.front().map(|x| x.op))), Some((10, Some(5))));
        assert!(d.apply(Command::DeleteApp("default/web".into()), &mut ch).is_ok());
        assert!(d.history.is_empty());
        assert_eq!(ch.first(), Some(&Change::Cluster("in-cluster".into())));
    }
}
