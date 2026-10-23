//! Targets beyond Kubernetes (docs/cd-targets.md §2): each provider's I/O half, and what makes a
//! target a cluster to the engine. The pure half (kinds, renders, reading answers, building
//! requests, health) is `kiln_cd::providers`.
//!
//! - **Connecting.** A target connects once an app deploys to it. The engine learns it as a
//!   cluster, with the provider's kinds and their scopes. It is reachable once its account
//!   (a W11 integration) gives credentials.
//! - **Reading** (§2.3). The engine's watch of a kind is a poll of the objects kiln's apps name
//!   on the target, and of those kiln applied there: every 60 seconds at rest, and every 3
//!   seconds while a sync changes the kind. Changes reach the engine as a watch's would.
//! - **Writing.** Applies and deletes go to the provider, which makes the platform's calls and
//!   answers with the object as it now reads. What kiln applied is kept ([`Applied`]). The live
//!   object carries it back as its metadata and its last-applied configuration, so the engine's
//!   comparison sees drift as §2.4 says: a field kiln sets that differs, or one kiln no longer
//!   sets that still holds kiln's value.
//! - **Budgets.** Each target spaces its requests (`GAP`), well below the platforms' documented
//!   limits, so kiln never starves the account's other users.
//! - **Credentials** are minted per call from the account's integration, as the workload of the
//!   apps on the target: their repository and environment (§6), cached by W11 until shortly
//!   before they expire.

pub mod api;
pub mod aws_beanstalk;
pub mod aws_ec2;
pub mod aws_ecs;
pub mod aws_lambda;
pub mod aws_s3;
pub mod aws_static;
pub mod azure;
pub mod azure_containerapps;
pub mod azure_static;
pub mod azure_vmss;
pub mod azure_web;
pub mod cloudflare_workers;
pub mod cloudformation;
pub mod gcp;
pub mod gcp_appengine;
pub mod gcp_compute;
pub mod gcp_functions;
pub mod gcp_run;
pub mod render;
pub mod vercel;
#[cfg(test)]
pub mod fake;
#[cfg(test)]
pub mod fake_azure;
#[cfg(test)]
pub mod fake_gcp;
#[cfg(test)]
pub mod fake_hosting;
#[cfg(test)]
pub mod fake_query;

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use kiln_cd::engine::{Input, LiveChange};
use kiln_cd::obj::{self, GroupKind, ResKey};
use kiln_cd::providers::{ProviderDef, Target};
use kiln_cd::state::{Applied, Command, ROLLOUT_REPLACED, RolloutRecord, applied_key, rollout_id};
use parking_lot::Mutex;
use serde_json::Value;

use super::Cd;
use crate::ci::integrations::{self, Integration, aws};

/// The least time between two requests to one target.
const GAP: Duration = Duration::from_millis(200);
/// How often a kind is read at rest.
const REST: Duration = Duration::from_secs(60);
/// How often a kind a sync changes is read.
const FAST: Duration = Duration::from_secs(3);
/// How long a kind stays fast after a change.
const HOT: Duration = Duration::from_secs(180);

/// A target the server reaches.
pub struct Conn {
    pub target: Target,
    pub def: &'static ProviderDef,
    /// When the next request may start.
    next: Mutex<Instant>,
    /// Kinds a sync changed lately, until when they are read fast.
    hot: Mutex<HashMap<String, Instant>>,
    /// Wakes the polls early, after a change.
    pub wake: crate::rt::Notify,
    /// What providers remember between calls (an ECS family's newest revision, say).
    pub memo: Mutex<HashMap<String, String>>,
    /// How often a change is checked while it settles.
    pub settle_every: Duration,
    /// The least time between two requests ([`GAP`]).
    pub gap: Duration,
}

impl Conn {
    pub fn new(target: Target, def: &'static ProviderDef) -> Self {
        Conn { target, def, next: Mutex::new(Instant::now()), hot: Mutex::new(HashMap::new()), wake: crate::rt::Notify::new(), memo: Mutex::new(HashMap::new()), settle_every: Duration::from_secs(2), gap: GAP }
    }

    /// A connection for tests on a fake: no gap between requests, settling looked at often.
    #[cfg(test)]
    pub fn quick(target: Target, def: &'static ProviderDef) -> Self {
        Conn { settle_every: Duration::from_millis(5), gap: Duration::ZERO, ..Conn::new(target, def) }
    }

    /// Waits for the target's budget.
    pub async fn budget(&self) {
        let wait = {
            let mut next = self.next.lock();
            let now = Instant::now();
            let at = (*next).max(now);
            *next = at + self.gap;
            at - now
        };
        if !wait.is_zero() {
            crate::rt::sleep(wait).await;
        }
    }

    /// A sync changed `kind`: read it fast for a while, starting now.
    pub fn heat(&self, kind: &str) {
        self.hot.lock().insert(kind.to_owned(), Instant::now() + HOT);
        self.wake.notify();
    }

    fn interval(&self, kind: &str) -> Duration {
        let mut hot = self.hot.lock();
        match hot.get(kind) {
            Some(until) if *until > Instant::now() => FAST,
            Some(_) => {
                hot.remove(kind);
                REST
            }
            None => REST,
        }
    }

    /// The target's region, for providers placed by one.
    pub fn region(&self) -> Result<&str> {
        self.target.place_value("region").ok_or_else(|| anyhow::anyhow!("{} has no region", self.target.name()))
    }
}

/// What a provider's calls go with.
pub enum Creds {
    Aws(aws::Aws),
    /// A bearer token: a Google Cloud or Azure access token.
    Bearer(String),
    /// A token platform's stored token, with its integration's configuration (an account id).
    Token { token: String, config: Value },
}

/// Where what kiln applied is kept, and the rollouts of what it applied (§5): the CD store
/// ([`Cd`]), or memory in tests.
pub trait Records: Send + Sync {
    /// What kiln last applied to `key` on `target`.
    fn get(&self, target: &str, key: &str) -> Option<Applied>;
    /// Keeps it, or forgets it.
    fn put<'a>(&'a self, target: &'a str, key: String, applied: Option<Applied>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>;
    /// The rollout of `key` on `target`, as kept.
    fn rollout(&self, target: &str, key: &str) -> Option<RolloutRecord>;
    /// Keeps it, or forgets it, if the one kept is still at `expect` (its version; none: whatever
    /// is kept): whether it was kept.
    fn keep_rollout<'a>(&'a self, target: &'a str, key: &'a str, record: Option<RolloutRecord>, expect: Option<u64>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + 'a>>;
}

impl Records for Cd {
    fn get(&self, target: &str, key: &str) -> Option<Applied> {
        self.store.read(|d| d.applied.get(target).and_then(|m| m.get(key)).cloned())
    }

    fn put<'a>(&'a self, target: &'a str, key: String, applied: Option<Applied>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            match self.store.submit(Command::Applied { target: target.to_owned(), key, applied: applied.map(Box::new) }).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => bail!("keeping what kiln applied: {e}"),
                Err(_) => bail!("the store stopped"),
            }
        })
    }

    fn rollout(&self, target: &str, key: &str) -> Option<RolloutRecord> {
        self.store.read(|d| d.rollouts.get(&rollout_id(target, key)).cloned())
    }

    fn keep_rollout<'a>(&'a self, target: &'a str, key: &'a str, record: Option<RolloutRecord>, expect: Option<u64>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + 'a>> {
        Box::pin(async move {
            match self.store.submit(Command::Rollout { id: rollout_id(target, key), record: record.map(Box::new), expect }).await {
                Ok(Ok(())) => Ok(true),
                Ok(Err(e)) if e == ROLLOUT_REPLACED => Ok(false),
                Ok(Err(e)) => bail!("keeping the rollout: {e}"),
                Err(_) => bail!("the store stopped"),
            }
        })
    }
}

/// What a provider's calls know: the target, credentials, what kiln applied there, and where
/// artifacts' content is.
pub struct Cx<'a> {
    pub conn: &'a Conn,
    pub creds: Creds,
    pub records: &'a dyn Records,
    pub files: &'a dyn crate::cd::artifacts::Files,
}

impl Cx<'_> {
    /// AWS, for AWS providers.
    pub fn aws(&self) -> Result<&aws::Aws> {
        match &self.creds {
            Creds::Aws(a) => Ok(a),
            _ => bail!("{} isn't an AWS target", self.conn.target.name()),
        }
    }

    /// Google Cloud's APIs, for `gcp-*` providers.
    pub fn gcp(&self) -> Result<integrations::gcp::Gcp<'_>> {
        match &self.creds {
            Creds::Bearer(token) if self.conn.def.integration == "gcp" => Ok(integrations::gcp::Gcp { token }),
            _ => bail!("{} isn't a Google Cloud target", self.conn.target.name()),
        }
    }

    /// Azure's APIs, for `azure-*` providers.
    pub fn arm(&self) -> Result<integrations::azure::Arm<'_>> {
        match &self.creds {
            Creds::Bearer(token) if self.conn.def.integration == "azure" => Ok(integrations::azure::Arm { token }),
            _ => bail!("{} isn't an Azure target", self.conn.target.name()),
        }
    }

    /// The target's place value `key` (`project`, `region`, `zone`).
    pub fn place(&self, key: &str) -> Result<&str> {
        self.conn.target.place_value(key).ok_or_else(|| anyhow::anyhow!("{} has no {key}", self.conn.target.name()))
    }

    /// A token platform's token and its integration's configuration.
    pub fn token(&self) -> Result<(&str, &Value)> {
        match &self.creds {
            Creds::Token { token, config } => Ok((token, config)),
            _ => bail!("{} takes no stored token", self.conn.target.name()),
        }
    }

    /// What kiln last applied to an object here.
    pub fn applied(&self, group: &str, kind: &str, namespace: &str, name: &str) -> Option<Applied> {
        self.records.get(&self.conn.target.name(), &applied_key(group, kind, namespace, name))
    }

    /// The rollout of an object here (§5), as kept.
    pub fn rollout(&self, group: &str, kind: &str, namespace: &str, name: &str) -> Option<RolloutRecord> {
        self.records.rollout(&self.conn.target.name(), &applied_key(group, kind, namespace, name))
    }

    /// Keeps what kiln applied to an object here (or forgets it).
    pub async fn record(&self, group: &str, kind: &str, namespace: &str, name: &str, applied: Option<Applied>) -> Result<()> {
        let target = self.conn.target.name();
        self.records.put(&target, applied_key(group, kind, namespace, name), applied).await
    }

    /// The live object, with what kiln applied read back (§2.4): kiln's labels and annotations,
    /// and the applied object as its last-applied configuration.
    pub fn with_applied(&self, mut live: Value) -> Value {
        let (group, kind, ns, name) = (obj::group(&live).to_owned(), obj::kind(&live).to_owned(), obj::namespace(&live).to_owned(), obj::name(&live).to_owned());
        let Some(a) = self.applied(&group, &kind, &ns, &name) else { return live };
        for part in ["labels", "annotations"] {
            if let Some(Value::Object(want)) = a.object.pointer(&format!("/metadata/{part}"))
                && let Some(have) = obj::ensure_map(&mut live, &["metadata", part])
            {
                for (k, v) in want {
                    have.insert(k.clone(), v.clone());
                }
            }
        }
        let mut last = a.object.clone();
        if let Some(m) = last.get_mut("metadata").and_then(Value::as_object_mut)
            && let Some(ann) = m.get_mut("annotations").and_then(Value::as_object_mut)
        {
            ann.remove(kiln_cd::diff::LAST_APPLIED);
        }
        if let (Ok(text), Some(ann)) = (serde_json::to_string(&last), obj::ensure_map(&mut live, &["metadata", "annotations"])) {
            ann.insert(kiln_cd::diff::LAST_APPLIED.into(), Value::String(text));
        }
        live
    }
}

/// The engine's key of a provider object.
pub fn key_of(o: &Value) -> ResKey {
    ResKey::new(obj::group(o), obj::kind(o), obj::namespace(o), obj::name(o))
}

impl Cd {
    /// The connected target named `cluster`.
    pub fn target(&self, cluster: &str) -> Option<Arc<Conn>> {
        self.targets.read().get(cluster).cloned()
    }

    /// Connects the targets the apps deploy to that aren't yet.
    pub(super) async fn connect_targets(self: &Arc<Self>) {
        let wanted: BTreeSet<String> = self.apps.read().values().map(|e| e.def.destination.cluster.clone()).filter(|c| kiln_cd::providers::is_target(c)).collect();
        for name in wanted {
            if self.targets.read().contains_key(&name) || !self.connecting.lock().insert(name.clone()) {
                continue;
            }
            let this = self.clone();
            crate::rt::spawn(async move { this.connect_target(name).await });
        }
    }

    async fn connect_target(self: Arc<Self>, name: String) {
        let mut wait = Duration::from_secs(5);
        loop {
            match self.try_connect_target(&name).await {
                Ok(conn) => {
                    tracing::info!(target = %name, provider = conn.def.name, "deployments: target connected");
                    self.input(Input::UpsertCluster { name: name.as_str().into(), qps: Some(1.0 / GAP.as_secs_f64()), inflight: Some(4) });
                    for k in conn.def.kinds {
                        self.input(Input::Scope { cluster: name.as_str().into(), kind: GroupKind::new(k.group, k.kind), namespaced: k.namespaced });
                    }
                    self.targets.write().insert(name.clone(), conn);
                    self.input(Input::ClusterReachable { name: name.as_str().into(), reachable: true });
                    self.connecting.lock().remove(&name);
                    self.kick();
                    return;
                }
                Err(e) => {
                    tracing::warn!(target = %name, error = %format!("{e:#}"), "deployments: can't reach a target yet");
                    self.target_errors.lock().insert(name.clone(), format!("{e:#}"));
                    self.input(Input::ClusterReachable { name: name.as_str().into(), reachable: false });
                }
            }
            // Gone with its last app: stop trying.
            if !self.apps.read().values().any(|e| e.def.destination.cluster == name) {
                self.connecting.lock().remove(&name);
                return;
            }
            if !self.hub.shutdown.sleep(wait).await {
                return;
            }
            wait = (wait * 2).min(Duration::from_secs(300));
        }
    }

    async fn try_connect_target(&self, name: &str) -> Result<Arc<Conn>> {
        let target = Target::parse(name).ok_or_else(|| anyhow::anyhow!("`{name}` isn't a target"))?;
        let def = target.def().ok_or_else(|| anyhow::anyhow!("no provider {}", target.provider))?;
        if !def.built {
            bail!("{} isn't built yet: kiln lists it, and deploys to it once it is", def.title);
        }
        let conn = Arc::new(Conn::new(target, def));
        // A real exchange: the account's credentials, as the apps here would use them.
        self.creds(&conn, None, "connect").await?;
        self.target_errors.lock().remove(name);
        Ok(conn)
    }

    /// The account a target acts through.
    async fn account(&self, conn: &Conn) -> Result<Integration> {
        let i = integrations::get(&self.hub, &conn.target.account).await?.ok_or_else(|| anyhow::anyhow!("no integration `{}` (Settings → Integrations)", conn.target.account))?;
        if i.kind != conn.def.integration {
            bail!("integration `{}` is {}, and {} needs {}", i.name, i.kind, conn.def.title, conn.def.integration);
        }
        Ok(i)
    }

    /// Credentials for calls on `conn`, as the workload of `app` (or of the target's apps), for
    /// `trigger` (`watch`, `sync`, `delete`).
    pub(super) async fn creds(&self, conn: &Conn, app: Option<&str>, trigger: &str) -> Result<Creds> {
        let i = self.account(conn).await?;
        let (repository, environment, app) = self.identity(&conn.target.name(), app);
        let w = kiln_ci::oidc::Workload { repository: &repository, environment: environment.as_deref(), actor: "kiln", source: "cd", app: app.as_deref(), trigger: Some(trigger), ..Default::default() };
        match i.kind.as_str() {
            "aws" => {
                let creds = aws::credentials(&self.hub, &i, &w).await.map_err(|e| match aws::advice(&e) {
                    Some(a) => anyhow::anyhow!("{e:#} — {a}"),
                    None => e,
                })?;
                Ok(Creds::Aws(aws::Aws { creds, region: conn.region()?.to_owned() }))
            }
            "gcp" => Ok(Creds::Bearer(integrations::gcp::token(&self.hub, &i, &w).await?.access_token)),
            "azure" => Ok(Creds::Bearer(integrations::azure::token(&self.hub, &i, &w, None).await?.access_token)),
            _ => Ok(Creds::Token { token: integrations::token(&self.hub, &i).await?.ok_or_else(|| anyhow::anyhow!("integration `{}` holds no token", i.name))?, config: i.config.clone() }),
        }
    }

    /// Whose workload calls on `cluster` are (§6): the repository and environment of `app`, or
    /// of the first app deploying there.
    fn identity(&self, cluster: &str, app: Option<&str>) -> (String, Option<String>, Option<String>) {
        let apps = self.apps.read();
        let entry = app.and_then(|a| apps.get(a)).or_else(|| apps.values().find(|e| e.def.destination.cluster == cluster));
        let Some(e) = entry else { return ("kiln/kiln".into(), None, None) };
        let repository = match &e.def.origin {
            kiln_cd::apps::Origin::Deploy { repo } => self.repo_names.read().get(repo).cloned().unwrap_or_else(|| repo_name(repo)),
            _ => "kiln/kiln".into(),
        };
        (repository, e.def.environment.clone(), Some(e.def.name.clone()))
    }

    // ---------------------------------------------------------------- reading

    /// The engine watches a target's kind: poll it.
    pub(super) fn target_watch(self: &Arc<Self>, cluster: String, kind: GroupKind) {
        if !self.watched.lock().insert((cluster.clone(), kind.clone())) {
            return;
        }
        let this = self.clone();
        crate::rt::spawn(async move { this.poll_kind(cluster, kind).await });
    }

    async fn poll_kind(self: Arc<Self>, cluster: String, kind: GroupKind) {
        let name: obj::Str = cluster.as_str().into();
        let mut seen: Option<HashMap<ResKey, u64>> = None;
        let mut wait = Duration::from_secs(5);
        loop {
            if self.hub.shutdown.is_cancelled() {
                return;
            }
            let Some(conn) = self.target(&cluster) else {
                if !self.hub.shutdown.sleep(Duration::from_secs(2)).await {
                    return;
                }
                continue;
            };
            let names = self.names(&cluster, &kind).await;
            let read = async {
                let cx = Cx { conn: &conn, creds: self.creds(&conn, None, "watch").await?, records: self.as_ref(), files: self.hub.as_ref() };
                list(&cx, &kind.kind, &names).await
            };
            match read.await {
                Ok(objects) => {
                    wait = Duration::from_secs(5);
                    let now: HashMap<ResKey, u64> = objects.iter().map(|o| (key_of(o), obj::hash_value(o))).collect();
                    match &seen {
                        // The first read lists the kind; later ones send what changed.
                        None => self.input(Input::Live { cluster: name.clone(), change: LiveChange::Listed { kind: kind.clone(), objects } }),
                        Some(before) => {
                            for o in objects {
                                if before.get(&key_of(&o)) != Some(&obj::hash_value(&o)) {
                                    self.input(Input::Live { cluster: name.clone(), change: LiveChange::Upsert(o) });
                                }
                            }
                            for k in before.keys().filter(|k| !now.contains_key(*k)) {
                                self.input(Input::Live { cluster: name.clone(), change: LiveChange::Delete(k.clone()) });
                            }
                        }
                    }
                    seen = Some(now);
                    let pause = crate::rt::timeout(conn.interval(&kind.kind), conn.wake.notified());
                    let _ = pause.await;
                }
                Err(e) => {
                    tracing::debug!(target = %cluster, kind = %kind, error = %format!("{e:#}"), "deployments: reading a target failed");
                    if !self.hub.shutdown.sleep(wait).await {
                        return;
                    }
                    wait = (wait * 2).min(Duration::from_secs(120));
                }
            }
        }
    }

    /// The objects of `kind` kiln reads on `cluster`: those its apps there name, and those it
    /// applied there.
    async fn names(&self, cluster: &str, kind: &GroupKind) -> Vec<(String, String)> {
        let ids: Vec<String> = self.apps.read().iter().filter(|(_, e)| e.def.destination.cluster == cluster).map(|(id, _)| id.clone()).collect();
        let k = kind.clone();
        let mut out: BTreeSet<(String, String)> = self
            .query(move |e| ids.iter().flat_map(|id| e.resources(id)).filter(|r| r.key.group == k.group && r.key.kind == k.kind).map(|r| (r.key.namespace.to_string(), r.key.name.to_string())).collect::<Vec<_>>())
            .await
            .unwrap_or_default()
            .into_iter()
            .collect();
        let prefix = format!("{}/{}/", kind.group, kind.kind);
        self.store.read(|d| {
            if let Some(m) = d.applied.get(cluster) {
                for k in m.keys() {
                    if let Some((ns, name)) = k.strip_prefix(&prefix).and_then(|r| r.split_once('/')) {
                        out.insert((ns.to_owned(), name.to_owned()));
                    }
                }
            }
        });
        out.into_iter().collect()
    }

    // ---------------------------------------------------------------- writing

    /// Applies an object to a target: the object as it now reads. One that carries traffic
    /// rolls out when its app has a rollout (§5).
    pub(super) async fn target_apply(self: &Arc<Self>, cluster: &str, object: &Value, dry_run: bool) -> Result<Option<Value>, String> {
        let conn = self.target(cluster).ok_or_else(|| format!("target {cluster} is not connected"))?;
        // No platform here checks a change without making it: a dry run answers with the object.
        if dry_run {
            return Ok(Some(object.clone()));
        }
        let app = app_of(object);
        let result = async {
            let cx = Cx { conn: &conn, creds: self.creds(&conn, app.as_deref(), "sync").await?, records: self.as_ref(), files: self.hub.as_ref() };
            if super::rollouts::rolls_out(conn.def.name, obj::kind(object)) {
                return self.rollout_apply(&cx, object).await.with_context(|| format!("{} {}", obj::kind(object), obj::name(object)));
            }
            apply(&cx, object).await
        };
        let r = result.await.map(Some).map_err(|e| format!("{e:#}"));
        conn.heat(obj::kind(object));
        r
    }

    /// Deletes an object from a target.
    pub(super) async fn target_delete(&self, cluster: &str, key: &ResKey) -> Result<(), String> {
        let conn = self.target(cluster).ok_or_else(|| format!("target {cluster} is not connected"))?;
        let result = async {
            let cx = Cx { conn: &conn, creds: self.creds(&conn, None, "delete").await?, records: self, files: self.hub.as_ref() };
            delete(&cx, key).await?;
            if super::rollouts::rolls_out(conn.def.name, &key.kind) {
                super::rollouts::forget(&cx, key).await?;
            }
            cx.record(&key.group, &key.kind, &key.namespace, &key.name, None).await
        };
        let r = result.await.map_err(|e| format!("{e:#}"));
        conn.heat(&key.kind);
        r
    }
}

/// The app an object was applied for, by its tracking annotation (`<app>:<group>/<kind>:…`).
pub(super) fn app_of(o: &Value) -> Option<String> {
    obj::annotation(o, kiln_cd::engine::TRACKING).and_then(|t| t.split_once(':')).map(|(app, _)| app.to_owned())
}

/// `body` with each secret reference in it ([`kiln_cd::apps::SECRET_REF`]) replaced by the value
/// it stands for, as text, for the app `desired` is of (docs/deploy-api.md §3.5): what a provider
/// sends the platform, and never keeps.
pub async fn resolve_secrets(cx: &Cx<'_>, desired: &Value, mut body: Value) -> Result<Value> {
    fn refs(v: &Value, out: &mut BTreeSet<String>) {
        match v {
            Value::String(s) if s.starts_with(kiln_cd::apps::SECRET_REF) => {
                out.insert(s.clone());
            }
            Value::Array(a) => a.iter().for_each(|x| refs(x, out)),
            Value::Object(m) => m.values().for_each(|x| refs(x, out)),
            _ => {}
        }
    }
    fn fill(v: &mut Value, values: &HashMap<String, String>) {
        match v {
            Value::String(s) => {
                if let Some(x) = values.get(s.as_str()) {
                    x.clone_into(s);
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|x| fill(x, values)),
            Value::Object(m) => m.values_mut().for_each(|x| fill(x, values)),
            _ => {}
        }
    }
    let mut found = BTreeSet::new();
    refs(&body, &mut found);
    if found.is_empty() {
        return Ok(body);
    }
    let app = app_of(desired).unwrap_or_default();
    let mut values = HashMap::new();
    for r in found {
        let name = kiln_cd::apps::parse_secret_ref(&r).map(|(_, n, _)| n.to_owned()).unwrap_or_default();
        let text = String::from_utf8(cx.files.secret(&app, &r).await?).map_err(|_| anyhow::anyhow!("secret {name} isn't text, and {} takes secrets as text", cx.conn.def.title))?;
        values.insert(r, text);
    }
    fill(&mut body, &values);
    Ok(body)
}

/// `owner/name` from a clone URL.
pub fn repo_name(url: &str) -> String {
    let path = url.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = path.rsplit(['/', ':']);
    match (parts.next(), parts.next()) {
        (Some(name), Some(owner)) => format!("{owner}/{name}"),
        _ => path.to_owned(),
    }
}

// ------------------------------------------------------------------ the providers

/// Reads `names` (namespace, name) of `kind` on the target.
async fn list(cx: &Cx<'_>, kind: &str, names: &[(String, String)]) -> Result<Vec<Value>> {
    let objects = match cx.conn.def.name {
        "aws-beanstalk" => aws_beanstalk::list(cx, kind, names).await?,
        "aws-ec2" => aws_ec2::list(cx, kind, names).await?,
        "aws-lambda" => aws_lambda::list(cx, kind, names).await?,
        "aws-static" => aws_static::list(cx, kind, names).await?,
        "azure-appservice" | "azure-functions" => azure_web::list(cx, kind, names).await?,
        "azure-containerapps" => azure_containerapps::list(cx, kind, names).await?,
        "azure-static" => azure_static::list(cx, kind, names).await?,
        "azure-vmss" => azure_vmss::list(cx, kind, names).await?,
        "aws-ecs" => aws_ecs::list(cx, kind, names).await?,
        "cloudflare-workers" => cloudflare_workers::list(cx, kind, names).await?,
        "cloudformation" => cloudformation::list(cx, kind, names).await?,
        "gcp-appengine" => gcp_appengine::list(cx, kind, names).await?,
        "gcp-compute" => gcp_compute::list(cx, kind, names).await?,
        "gcp-functions" => gcp_functions::list(cx, kind, names).await?,
        "gcp-run" => gcp_run::list(cx, kind, names).await?,
        "render" => render::list(cx, kind, names).await?,
        "vercel" => vercel::list(cx, kind, names).await?,
        other => bail!("no reading {other} yet"),
    };
    Ok(objects.into_iter().map(|o| cx.with_applied(o)).collect())
}

/// An object as the watch reads it, or none if it's gone.
pub(super) async fn read(cx: &Cx<'_>, o: &Value) -> Result<Option<Value>> {
    Ok(list(cx, obj::kind(o), &[(obj::namespace(o).to_owned(), obj::name(o).to_owned())]).await?.into_iter().next())
}

/// Applies `desired`, keeping what was applied: the object as it now reads.
pub(super) async fn apply(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let live = match cx.conn.def.name {
        "aws-beanstalk" => aws_beanstalk::apply(cx, desired).await,
        "aws-ec2" => aws_ec2::apply(cx, desired).await,
        "aws-lambda" => aws_lambda::apply(cx, desired).await,
        "aws-static" => aws_static::apply(cx, desired).await,
        "azure-appservice" | "azure-functions" => azure_web::apply(cx, desired).await,
        "azure-containerapps" => azure_containerapps::apply(cx, desired).await,
        "azure-static" => azure_static::apply(cx, desired).await,
        "azure-vmss" => azure_vmss::apply(cx, desired).await,
        "aws-ecs" => aws_ecs::apply(cx, desired).await,
        "cloudflare-workers" => cloudflare_workers::apply(cx, desired).await,
        "cloudformation" => cloudformation::apply(cx, desired).await,
        "gcp-appengine" => gcp_appengine::apply(cx, desired).await,
        "gcp-compute" => gcp_compute::apply(cx, desired).await,
        "gcp-functions" => gcp_functions::apply(cx, desired).await,
        "gcp-run" => gcp_run::apply(cx, desired).await,
        "render" => render::apply(cx, desired).await,
        "vercel" => vercel::apply(cx, desired).await,
        other => bail!("no applying to {other} yet"),
    }
    .with_context(|| format!("{} {}", obj::kind(desired), obj::name(desired)))?;
    Ok(cx.with_applied(live))
}

async fn delete(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    match cx.conn.def.name {
        "aws-beanstalk" => aws_beanstalk::delete(cx, key).await,
        "aws-ec2" => aws_ec2::delete(cx, key).await,
        "aws-lambda" => aws_lambda::delete(cx, key).await,
        "aws-static" => aws_static::delete(cx, key).await,
        "azure-appservice" | "azure-functions" => azure_web::delete(cx, key).await,
        "azure-containerapps" => azure_containerapps::delete(cx, key).await,
        "azure-static" => azure_static::delete(cx, key).await,
        "azure-vmss" => azure_vmss::delete(cx, key).await,
        "aws-ecs" => aws_ecs::delete(cx, key).await,
        "cloudflare-workers" => cloudflare_workers::delete(cx, key).await,
        "cloudformation" => cloudformation::delete(cx, key).await,
        "gcp-appengine" => gcp_appengine::delete(cx, key).await,
        "gcp-compute" => gcp_compute::delete(cx, key).await,
        "gcp-functions" => gcp_functions::delete(cx, key).await,
        "gcp-run" => gcp_run::delete(cx, key).await,
        "render" => render::delete(cx, key).await,
        "vercel" => vercel::delete(cx, key).await,
        other => bail!("no deleting from {other} yet"),
    }
    .with_context(|| format!("{} {}", key.kind, key.name))
}

/// The record of an apply: the desired object, what the platform made, now.
pub fn applied(desired: &Value, made: Value) -> Applied {
    Applied { object: desired.clone(), made, at_ms: crate::util::now_ms().max(0) as u64 }
}

/// Polls `f` until it says done (`Some`), for at most `limit`.
pub async fn until<T, F, Fut>(conn: &Conn, limit: Duration, what: &str, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Option<T>>>,
{
    let end = Instant::now() + limit;
    loop {
        conn.budget().await;
        if let Some(t) = f().await? {
            return Ok(t);
        }
        if Instant::now() >= end {
            bail!("{what} took longer than {}s", limit.as_secs());
        }
        crate::rt::sleep(conn.settle_every).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repositories_name_from_urls() {
        assert_eq!(repo_name("https://github.com/acme/api.git"), "acme/api");
        assert_eq!(repo_name("git@github.com:acme/api.git"), "acme/api");
        assert_eq!(repo_name("https://git.example.com/acme/api/"), "acme/api");
    }

    #[test]
    fn objects_name_their_app() {
        let o = serde_json::json!({ "metadata": { "annotations": { "kiln.dev/tracking": "default/api-production:ecs.aws.kiln.dev/Service:main/api" } } });
        assert_eq!(app_of(&o).as_deref(), Some("default/api-production"));
    }
}
