//! The infrastructure list (`docs/observe.md` §10.9), Datadog's: every host that reports, from
//! whatever reports it (kiln's runners, each with its own figures, `super::host`, and what the hub
//! knows of it; Datadog's Agent, its metrics and what it says of its host, [`agent`]; an
//! OpenTelemetry resource's `host.name`; anything that tags a series `host:<name>`). Each host with
//! when it last reported and whether it is up, its CPU, I/O wait and load (Datadog's host list's
//! figures) and its memory's and fullest disk's use, the apps that report it (its metrics'
//! namespaces), its platform, its tags by source, and whether it is muted: a downtime of every
//! monitor's groups of the host, as Datadog's mute makes one.
//!
//! An inventory is made of the series index (each host's series and newest sample, nothing read
//! from disk), ten queries of the last ten minutes (its figures, every host at once), the Agent's
//! hosts, the hub's runners and the downtimes; and is reused for [`FRESH_MS`].

pub mod agent;
pub mod http;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use crate::hub::Hub;

/// Hosts listed: those that reported within three hours (Datadog's host list), unless asked from
/// further back.
pub const LISTED_MS: i64 = 3 * 3_600_000;
/// Up: reported within two hours; active, within one (Datadog's totals).
pub const UP_MS: i64 = 2 * 3_600_000;
pub const ACTIVE_MS: i64 = 3_600_000;
/// Reporting (kiln's page: a dot of its own): within two minutes, eight of the Agent's and kiln's
/// own 15-second intervals (`super::host::EVERY`), a late batch beside.
pub const REPORTING_MS: i64 = 2 * 60_000;
/// A host's figures are its newest of the last ten minutes.
pub const FIGURES_MS: i64 = 10 * 60_000;
/// Hosts are kept seven days (Datadog's retention of its host list), what the Agent said as long.
pub const KEPT_MS: i64 = 7 * 86_400_000;
/// An inventory is reused this long (the list asks every 30 s); past it, it is still answered while
/// one is made again (a fleet of 20,000 hosts' takes some 400 ms, §12.31).
pub const FRESH_MS: i64 = 15_000;
/// The most series one figure's query reads: a fleet's hosts, a few series each.
const FIGURE_SERIES: usize = 200_000;

/// What a host's series carry that says its platform and where it runs: OpenTelemetry's resource
/// attributes (`host.name` says one is OpenTelemetry's), a cluster's.
pub const PLATFORM_TAGS: [&str; 12] = [
    "host.name",
    "os.type",
    "os.description",
    "host.arch",
    "host.type",
    "cloud.provider",
    "cloud.platform",
    "cloud.region",
    "cloud.availability_zone",
    "k8s.cluster.name",
    "k8s.node.name",
    "kube_cluster_name",
];

/// A host's figures, its newest within [`FIGURES_MS`]: CPU in use (%), I/O wait (%), the load
/// over 15 minutes (and a core's), memory in use (%), its fullest disk's use (%), and its cores and
/// memory (MiB) as its metrics say them.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Figures {
    pub cpu: Option<f64>,
    pub iowait: Option<f64>,
    pub load: Option<f64>,
    pub load_norm: Option<f64>,
    pub memory: Option<f64>,
    pub disk: Option<f64>,
    pub cores: Option<f64>,
    pub memory_mib: Option<f64>,
}

/// A kiln runner on the host: what the hub knows of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Runner {
    pub name: String,
    pub connected: bool,
    pub last_seen_ms: i64,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub labels: Vec<String>,
    pub facts: Option<Box<kiln_ci::facts::RunnerFacts>>,
}

/// A host's mute: the downtime that holds every monitor's groups of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mute {
    pub downtime: String,
    pub end_ms: Option<i64>,
    pub message: String,
}

/// A host as the list shows it.
#[derive(Debug, Clone)]
pub struct Host {
    pub name: String,
    pub aliases: Vec<String>,
    pub apps: Vec<String>,
    pub sources: Vec<String>,
    /// Its newest sample (ms): Datadog's `last_reported_time`.
    pub last_reported_ms: Option<i64>,
    /// The newest of anything of it: a sample, the Agent's word, its runner.
    pub seen_ms: i64,
    pub series: usize,
    pub figures: Figures,
    /// [`PLATFORM_TAGS`] its series carry.
    pub tags: Vec<(String, String)>,
    pub agent: Option<Arc<agent::AgentHost>>,
    pub runner: Option<Runner>,
    pub mute: Option<Mute>,
}

/// A host's platform, from the best of what says it: its runner's facts, the Agent's inventory or
/// metadata, its resource's attributes, its metrics.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Platform {
    pub os: Option<String>,
    pub kernel: Option<String>,
    pub arch: Option<String>,
    pub cpu_model: Option<String>,
    pub cores: Option<u64>,
    pub threads: Option<u64>,
    pub memory_bytes: Option<u64>,
    pub cloud: Option<String>,
    pub instance_type: Option<String>,
    pub region: Option<String>,
    pub agent_version: Option<String>,
    pub kiln_version: Option<String>,
}

impl Host {
    /// Up, as Datadog's totals have it: a sample within [`UP_MS`].
    pub fn up(&self, now_ms: i64) -> bool {
        self.last_reported_ms.is_some_and(|t| now_ms.saturating_sub(t) < UP_MS)
    }

    /// Active: a sample within [`ACTIVE_MS`].
    pub fn active(&self, now_ms: i64) -> bool {
        self.last_reported_ms.is_some_and(|t| now_ms.saturating_sub(t) < ACTIVE_MS)
    }

    /// `reporting` (a sample within [`REPORTING_MS`]), `late` (up, not lately) or `gone`.
    pub fn status(&self, now_ms: i64) -> &'static str {
        match self.last_reported_ms {
            Some(t) if now_ms.saturating_sub(t) < REPORTING_MS => "reporting",
            Some(t) if now_ms.saturating_sub(t) < UP_MS => "late",
            _ => "gone",
        }
    }

    fn tag(&self, key: &str) -> Option<&str> {
        self.tags.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// Its tags by where they come from: the Agent's sources, kiln's (its runner's labels), and
    /// OpenTelemetry's (its resource's attributes).
    pub fn tags_by_source(&self) -> BTreeMap<String, Vec<String>> {
        let mut out: BTreeMap<String, Vec<String>> = self.agent.as_ref().map(|a| a.tags_by_source.clone()).unwrap_or_default();
        if let Some(r) = &self.runner
            && !r.labels.is_empty()
        {
            out.insert("kiln".into(), r.labels.clone());
        }
        let resource: Vec<String> = self.tags.iter().filter(|(k, _)| k != "host.name").map(|(k, v)| format!("{k}:{v}")).collect();
        if !resource.is_empty() {
            out.insert("OpenTelemetry".into(), resource);
        }
        out
    }

    /// Its platform, each part from the best of what says it.
    pub fn platform(&self) -> Platform {
        let facts = self.runner.as_ref().and_then(|r| r.facts.as_deref());
        let agent = self.agent.as_deref();
        let field = |k: &str| agent.and_then(|a| a.field(k));
        let meta = |k: &str| agent.and_then(|a| a.meta.get(k)).and_then(serde_json::Value::as_str).map(str::to_owned).filter(|s| !s.is_empty());
        let number = |s: Option<String>| s.and_then(|s| s.parse::<f64>().ok()).filter(|n| n.is_finite() && *n >= 0.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let whole = |n: f64| n.round() as u64;
        let os = facts
            .map(|f| [f.os_distro.as_deref(), f.os_version.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(" "))
            .filter(|s| !s.is_empty())
            .or_else(|| facts.map(|f| f.os.clone()))
            .or_else(|| field("os_version"))
            .or_else(|| self.tag("os.description").map(str::to_owned))
            .or_else(|| {
                // The Agent's own word of its distribution: `nixV` (`["debian", "13", ""]`).
                let v = agent.and_then(|a| a.meta.get("nixV")).and_then(serde_json::Value::as_array)?;
                let words: Vec<&str> = v.iter().take(2).filter_map(serde_json::Value::as_str).filter(|s| !s.is_empty()).collect();
                (!words.is_empty()).then(|| words.join(" "))
            })
            .or_else(|| meta("platform"))
            .or_else(|| self.tag("os.type").map(str::to_owned))
            .or_else(|| self.runner.as_ref().map(|r| r.os.clone()).filter(|s| !s.is_empty()));
        Platform {
            os,
            kernel: facts.and_then(|f| f.kernel.clone()).or_else(|| field("kernel_release")),
            arch: facts
                .map(|f| f.arch.clone())
                .filter(|s| !s.is_empty())
                .or_else(|| field("cpu_architecture"))
                .or_else(|| meta("machine"))
                .or_else(|| self.tag("host.arch").map(str::to_owned))
                .or_else(|| self.runner.as_ref().map(|r| r.arch.clone()).filter(|s| !s.is_empty())),
            cpu_model: facts.and_then(|f| f.cpu.model.clone()).or_else(|| field("cpu_model")).or_else(|| meta("processor")),
            cores: facts
                .map(|f| u64::from(f.cpu.cores))
                .filter(|c| *c > 0)
                .or_else(|| number(field("cpu_cores")).map(whole))
                .or_else(|| agent.and_then(|a| a.meta.get("cpuCores")).and_then(serde_json::Value::as_u64).filter(|c| *c > 0))
                .or_else(|| self.figures.cores.map(whole)),
            threads: facts.map(|f| u64::from(f.cpu.threads)).filter(|c| *c > 0).or_else(|| number(field("cpu_logical_processors")).map(whole)),
            memory_bytes: facts
                .map(|f| f.mem.total)
                .filter(|m| *m > 0)
                .or_else(|| number(field("memory_total_kb")).map(|kb| whole(kb * 1024.0)))
                .or_else(|| self.figures.memory_mib.map(|mib| whole(mib * 1_048_576.0))),
            cloud: field("cloud_provider").or_else(|| self.tag("cloud.provider").map(str::to_owned)),
            instance_type: field("instance-type").or_else(|| self.tag("host.type").map(str::to_owned)),
            region: self.tag("cloud.region").map(str::to_owned),
            agent_version: agent.and_then(|a| a.version().map(str::to_owned)),
            kiln_version: self.runner.as_ref().map(|r| r.version.clone()).filter(|s| !s.is_empty()),
        }
    }
}

/// Every host at one time.
#[derive(Debug, Clone, Default)]
pub struct Inventory {
    pub at_ms: i64,
    pub hosts: Vec<Host>,
}

impl Inventory {
    pub fn host(&self, name: &str) -> Option<&Host> {
        self.hosts.binary_search_by(|h| h.name.as_str().cmp(name)).ok().and_then(|i| self.hosts.get(i))
    }
}

/// An app as Datadog's host list names it: a metric's namespace, the Agent's own `agent`.
fn app_of(namespace: &str) -> &str {
    match namespace {
        "datadog" => "agent",
        n => n,
    }
}

/// The figures of every host: each query's series' newest value, by host.
#[derive(Debug, Default)]
struct Read {
    by: HashMap<String, Figures>,
}

/// The figures' metrics (the System Check's), whether a host's several series of one are taken at
/// their most (its fullest disk) or on average, and where each value goes.
#[allow(clippy::type_complexity)]
const FIGURE_METRICS: [(&str, bool, fn(&mut Figures, f64)); 10] = [
    ("system.cpu.idle", false, |f, v| f.cpu = Some((100.0 - v).clamp(0.0, 100.0))),
    ("system.cpu.iowait", false, |f, v| f.iowait = Some(v)),
    ("system.load.15", false, |f, v| f.load = Some(v)),
    ("system.load.norm.15", false, |f, v| f.load_norm = Some(v)),
    ("system.mem.pct_usable", false, |f, v| f.memory = Some((100.0 * (1.0 - v)).clamp(0.0, 100.0))),
    ("system.disk.in_use", true, |f, v| f.disk = Some((100.0 * v).clamp(0.0, 100.0))),
    ("system.cpu.num_cores", false, |f, v| f.cores = Some(v)),
    ("system.mem.total", false, |f, v| f.memory_mib = Some(v)),
    // A runner too old to report its own figures (`super::hub::host_sample`): its CPU in use, and
    // its memory's use of its total. Read where the above say nothing.
    ("system.cpu.pct_usage", false, |f, v| {
        if f.cpu.is_none() {
            f.cpu = Some(v.clamp(0.0, 100.0));
        }
    }),
    ("system.mem.used", false, |f, v| {
        if f.memory.is_none()
            && let Some(total) = f.memory_mib.filter(|t| *t > 0.0)
        {
            f.memory = Some((100.0 * v / total).clamp(0.0, 100.0));
        }
    }),
];

/// Each host's figures: of each metric, each series' newest sample of the last [`FIGURES_MS`]
/// (read from the head with nothing decoded where it holds it, `Store::newest_by`), a host's taken
/// together; the store held a metric at a time.
fn read_figures(store: &kiln_obs::machine::Shared, now_ms: i64) -> Read {
    let mut hosts: HashMap<String, usize> = HashMap::new();
    let mut figures: Vec<Figures> = Vec::new();
    // Of one metric, each host's value so far and its series, and the hosts it reached.
    let mut taken: Vec<(f64, u32)> = Vec::new();
    let mut touched: Vec<usize> = Vec::new();
    let mut scratch = kiln_obs::store::Scratch::default();
    for (metric, most, put) in FIGURE_METRICS {
        {
            let s = store.read().unwrap_or_else(PoisonError::into_inner);
            s.newest_by(metric, "host", now_ms - FIGURES_MS, &mut scratch, &mut |host, v| {
                if host.is_empty() || !v.is_finite() {
                    return;
                }
                let i = match hosts.get(host) {
                    Some(&i) => i,
                    None => {
                        hosts.insert(host.to_owned(), figures.len());
                        figures.push(Figures::default());
                        figures.len() - 1
                    }
                };
                if taken.len() <= i {
                    taken.resize(i + 1, (0.0, 0));
                }
                if let Some(t) = taken.get_mut(i) {
                    if t.1 == 0 {
                        touched.push(i);
                        *t = (v, 1);
                    } else {
                        t.0 = if most { t.0.max(v) } else { t.0 + v };
                        t.1 += 1;
                    }
                }
            });
        }
        for &i in &touched {
            if let (Some(t), Some(f)) = (taken.get_mut(i), figures.get_mut(i)) {
                put(f, if most { t.0 } else { t.0 / f64::from(t.1) });
                *t = (0.0, 0);
            }
        }
        touched.clear();
    }
    Read { by: hosts.into_iter().filter_map(|(h, i)| figures.get(i).map(|f| (h, *f))).collect() }
}

/// The infrastructure list's keeping: what the Agent says of its hosts, and the inventory made
/// last.
pub struct Infra {
    path: PathBuf,
    agents: Mutex<agent::Agents>,
    dirty: AtomicBool,
    hub: Option<Arc<Hub>>,
    made: Mutex<Option<Arc<Inventory>>>,
    /// One is being made again (one at a time).
    making: AtomicBool,
    /// Bumped as the made one is forgotten: one begun before is not kept.
    generation: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

static INFRA: OnceLock<Arc<Infra>> = OnceLock::new();

pub fn get() -> Option<&'static Arc<Infra>> {
    INFRA.get()
}

pub fn set(i: Arc<Infra>) {
    let _ = INFRA.set(i);
}

/// What an Agent posted (`/intake/`, `/api/v1/metadata`): kept when it says something of its host.
pub fn heard_agent(v: &serde_json::Value) {
    if let (Some(i), Some((host, said))) = (get(), agent::read(v)) {
        i.keep(&host, said, super::clock_ms());
    }
}

impl Infra {
    pub fn open(dir: &Path, hub: Option<Arc<Hub>>) -> Arc<Self> {
        let path = dir.join("hosts.json");
        let agents = std::fs::read(&path).ok().and_then(|b| agent::Agents::from_json(&b)).unwrap_or_default();
        Arc::new(Self { path, agents: Mutex::new(agents), dirty: AtomicBool::new(false), hub, made: Mutex::new(None), making: AtomicBool::new(false), generation: AtomicU64::new(0) })
    }

    /// What the Agent said of `host`, kept (saved within [`save_every`]).
    pub fn keep(&self, host: &str, said: agent::Said, now_ms: i64) {
        if lock(&self.agents).keep(host, said, now_ms) {
            self.dirty.store(true, Ordering::Release);
        }
    }

    /// The Agent's hosts saved, when changed; those it has said nothing of for [`KEPT_MS`] let go.
    pub fn save(&self, now_ms: i64) {
        let bytes = {
            let mut a = lock(&self.agents);
            if a.expire(now_ms - KEPT_MS) > 0 {
                self.dirty.store(true, Ordering::Release);
            }
            if !self.dirty.swap(false, Ordering::AcqRel) {
                return;
            }
            a.to_json()
        };
        let saved = match bytes {
            Ok(b) => crate::sync::group::save_atomic(&self.path, &b).map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        if let Err(e) = saved {
            self.dirty.store(true, Ordering::Release);
            tracing::warn!(error = %e, "the Agent's hosts were not saved");
        }
    }

    /// Saves what changed every [`save_every`] until shutdown, and once as it shuts down.
    pub async fn run(self: Arc<Self>, shutdown: crate::rt::Shutdown) {
        while shutdown.sleep(save_every()).await {
            self.save(super::clock_ms());
        }
        self.save(super::clock_ms());
    }

    /// Every host now: the one made within [`FRESH_MS`]; one older, while another is made (one at
    /// a time); none made yet (or forgotten, a mute changed it), one made now.
    pub async fn inventory(self: &Arc<Self>) -> Result<Arc<Inventory>, super::api::Fail> {
        let now = super::clock_ms();
        let made = lock(&self.made).clone();
        match made {
            Some(m) if (0..FRESH_MS).contains(&(now - m.at_ms)) => Ok(m),
            Some(m) => {
                if !self.making.swap(true, Ordering::AcqRel) {
                    let me = self.clone();
                    drop(crate::rt::spawn(async move {
                        if let Err(e) = me.make(super::clock_ms()).await {
                            tracing::debug!(error = ?e, "the hosts' inventory was not made again");
                        }
                        me.making.store(false, Ordering::Release);
                    }));
                }
                Ok(m)
            }
            None => self.make(now).await,
        }
    }

    /// An inventory made now, and kept (unless what was made was forgotten meanwhile): on the
    /// blocking pool, the shards' tasks never waiting on it.
    async fn make(self: &Arc<Self>, now: i64) -> Result<Arc<Inventory>, super::api::Fail> {
        let generation = self.generation.load(Ordering::Acquire);
        let me = self.clone();
        let made = super::api::apart(move |store| {
            let seen = store.read().unwrap_or_else(PoisonError::into_inner).tag_seen("host", now - KEPT_MS, &PLATFORM_TAGS);
            // The hold let go before the queries take theirs, a part at a time.
            let read = read_figures(store, now);
            Ok(Arc::new(me.merge(now, seen, read)))
        })
        .await?;
        let mut kept = lock(&self.made);
        if self.generation.load(Ordering::Acquire) == generation {
            *kept = Some(made.clone());
        }
        Ok(made)
    }

    /// The runners the hub knows, by name.
    fn runners(&self) -> HashMap<String, Runner> {
        let Some(hub) = &self.hub else { return HashMap::new() };
        hub.agents
            .lock()
            .values()
            .map(|a| {
                (
                    a.name.clone(),
                    Runner {
                        name: a.name.clone(),
                        connected: a.connected,
                        // The hub keeps it in seconds.
                        last_seen_ms: a.last_seen.saturating_mul(1000),
                        version: a.version.clone(),
                        os: a.os.clone(),
                        arch: a.arch.clone(),
                        labels: a.labels.clone(),
                        facts: a.facts.clone(),
                    },
                )
            })
            .collect()
    }

    /// The downtimes that mute a host: active, of every monitor (`monitor_tags: ["*"]`), scoped
    /// to the host alone.
    fn mutes(now_ms: i64) -> HashMap<String, Mute> {
        let Ok(m) = super::api::monitors_kept() else { return HashMap::new() };
        m.downtimes
            .list()
            .into_iter()
            .filter(|d| d.monitor_identifier.monitor_tags == ["*"] && d.active(now_ms).is_some())
            .filter_map(|d| {
                let host = d.scope.trim().strip_prefix("host:")?.trim();
                (!host.is_empty() && !host.contains([' ', '(', ')', '*'])).then(|| {
                    let end_ms = match &d.schedule {
                        super::downtimes::Schedule::Once { end, .. } => end.as_ref().and_then(|e| e.ms().ok()),
                        super::downtimes::Schedule::Recurring { .. } => d.active(now_ms).map(|w| w.1).filter(|e| *e != i64::MAX),
                    };
                    (host.to_owned(), Mute { downtime: d.id.clone(), end_ms, message: d.message.clone() })
                })
            })
            .collect()
    }

    /// The hosts the index, the Agent and the hub say, each with its figures and mute.
    fn merge(&self, now_ms: i64, seen: Vec<kiln_obs::store::TagSeen>, read: Read) -> Inventory {
        let agents: HashMap<String, Arc<agent::AgentHost>> = lock(&self.agents).hosts.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut parts = Parts { agents, runners: self.runners(), mutes: Self::mutes(now_ms), read };
        let mut hosts: Vec<Host> = Vec::with_capacity(seen.len());
        for s in seen {
            hosts.push(parts.host(s.value, s.series, Some(s.last_ms), &s.namespaces, s.tags));
        }
        // What the Agent spoke of, or a runner the hub knows, that has sent no sample yet.
        let rest: Vec<String> = parts.agents.keys().chain(parts.runners.keys()).cloned().collect();
        for name in rest {
            if !hosts.iter().any(|h| h.name == name) {
                hosts.push(parts.host(name, 0, None, &[], Vec::new()));
            }
        }
        hosts.retain(|h| now_ms.saturating_sub(h.seen_ms) < KEPT_MS);
        hosts.sort_by(|a, b| a.name.cmp(&b.name));
        Inventory { at_ms: now_ms, hosts }
    }

    /// The made inventory forgotten (a mute changed it), and one being made not kept.
    pub fn forget(&self) {
        let mut kept = lock(&self.made);
        self.generation.fetch_add(1, Ordering::AcqRel);
        *kept = None;
    }
}

/// What a container's series carry that names it, its image and pod, and its host: Datadog's
/// container tags, OpenTelemetry's resource attributes.
pub const CONTAINER_TAGS: [&str; 12] =
    ["host", "host.name", "container_name", "image_name", "short_image", "image_tag", "kube_namespace", "pod_name", "container.name", "container.image.name", "k8s.pod.name", "k8s.namespace.name"];

/// A host's containers: each container (`container_id`, OpenTelemetry's `container.id`) whose
/// series say it is on the host, its name, image and pod, when it last reported, and its CPU
/// (nanocores) and memory (bytes) of the last ten minutes (Datadog's container metrics).
pub async fn containers(host: &str, now_ms: i64) -> Option<Vec<serde_json::Value>> {
    let host = host.to_owned();
    super::api::apart(move |store| {
        let mut seen = {
            let s = store.read().unwrap_or_else(PoisonError::into_inner);
            let mut all = s.tag_seen("container_id", now_ms - KEPT_MS, &CONTAINER_TAGS);
            all.extend(s.tag_seen("container.id", now_ms - KEPT_MS, &CONTAINER_TAGS));
            all
        };
        seen.retain(|c| c.tags.iter().any(|(k, v)| (k == "host" || k == "host.name") && *v == host));
        // Their figures, where the host's name reads whole in a query.
        let mut figures: HashMap<String, (Option<f64>, Option<f64>)> = HashMap::new();
        if !seen.is_empty() && !host.contains(|c: char| c.is_whitespace() || "{}(),:\"".contains(c)) {
            use kiln_obs::query::eval;
            for (i, metric) in ["container.cpu.usage", "container.memory.usage"].iter().enumerate() {
                for key in ["container_id", "container.id"] {
                    let q = format!("avg:{metric}{{host:{host}}} by {{{key}}}");
                    let mut r = eval::Request::new(&q, now_ms - FIGURES_MS, now_ms);
                    r.step = Some(60_000);
                    r.max_series = FIGURE_SERIES;
                    let Ok((a, _)) = eval::eval_shared(store, &r) else { continue };
                    for s in &a.series {
                        let (Some(id), Some(v)) = (s.tags.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()), s.values.iter().rev().copied().find(|v| v.is_finite())) else { continue };
                        let e = figures.entry(id).or_default();
                        if i == 0 {
                            e.0 = Some(v);
                        } else {
                            e.1 = Some(v);
                        }
                    }
                }
            }
        }
        let tag = |c: &kiln_obs::store::TagSeen, keys: &[&str]| keys.iter().find_map(|k| c.tags.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone()));
        Ok(seen
            .iter()
            .map(|c| {
                let (cpu, memory) = figures.get(&c.value).copied().unwrap_or_default();
                serde_json::json!({
                    "id": c.value,
                    "name": tag(c, &["container_name", "container.name"]),
                    "image": tag(c, &["image_name", "short_image", "container.image.name"]),
                    "image_tag": tag(c, &["image_tag"]),
                    "pod": tag(c, &["pod_name", "k8s.pod.name"]),
                    "namespace": tag(c, &["kube_namespace", "k8s.namespace.name"]),
                    "last_reported": c.last_ms,
                    "series": c.series,
                    "cpu_nanocores": cpu,
                    "memory_bytes": memory,
                })
            })
            .collect())
    })
    .await
    .ok()
}

/// What a host is made of, each taken as its host is.
struct Parts {
    agents: HashMap<String, Arc<agent::AgentHost>>,
    runners: HashMap<String, Runner>,
    mutes: HashMap<String, Mute>,
    read: Read,
}

impl Parts {
    fn host(&mut self, name: String, series: usize, last: Option<i64>, namespaces: &[String], tags: Vec<(String, String)>) -> Host {
        let agent = self.agents.remove(&name);
        let runner = self.runners.remove(&name);
        let mut apps: Vec<String> = namespaces.iter().map(|n| app_of(n).to_owned()).collect();
        let mut sources: Vec<String> = Vec::new();
        if runner.is_some() {
            sources.push("kiln".into());
        }
        if agent.is_some() || apps.iter().any(|a| a == "agent") {
            sources.push("agent".into());
            apps.push("agent".into());
        }
        if tags.iter().any(|(k, _)| k == "host.name") {
            sources.push("otel".into());
        }
        let cloud = agent.as_ref().and_then(|a| a.field("cloud_provider")).or_else(|| tags.iter().find(|(k, _)| k == "cloud.provider").map(|(_, v)| v.clone()));
        if let Some(c) = cloud.map(|c| c.to_ascii_lowercase())
            && !sources.contains(&c)
        {
            sources.push(c);
        }
        apps.sort();
        apps.dedup();
        let seen_ms = [last, agent.as_ref().map(|a| a.updated_ms), runner.as_ref().map(|r| r.last_seen_ms)].into_iter().flatten().max().unwrap_or(i64::MIN);
        Host {
            aliases: agent.as_ref().map(|a| a.aliases.clone()).unwrap_or_default(),
            apps,
            sources,
            last_reported_ms: last,
            seen_ms,
            series,
            figures: self.read.by.remove(&name).unwrap_or_default(),
            tags,
            agent,
            runner,
            mute: self.mutes.remove(&name),
            name,
        }
    }
}

/// How often what the Agent said is saved, when it changed: a minute (the Agent says it again every
/// 30); `KILN_OBSERVE_HOSTS_SAVE_MS` for tests.
fn save_every() -> std::time::Duration {
    std::env::var("KILN_OBSERVE_HOSTS_SAVE_MS").ok().and_then(|v| v.parse().ok()).map_or(std::time::Duration::from_secs(60), std::time::Duration::from_millis)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn host(name: &str) -> Host {
        Host {
            name: name.into(),
            aliases: Vec::new(),
            apps: Vec::new(),
            sources: Vec::new(),
            last_reported_ms: None,
            seen_ms: 0,
            series: 0,
            figures: Figures::default(),
            tags: Vec::new(),
            agent: None,
            runner: None,
            mute: None,
        }
    }

    /// Up within two hours of its last sample, active within one, reporting within two minutes;
    /// a host of no sample is none of them.
    #[test]
    fn a_host_s_status_is_its_last_sample_s() {
        let now = 10 * 3_600_000;
        let at = |ago: i64| Host { last_reported_ms: Some(now - ago), ..host("h") };
        assert_eq!((at(60_000).status(now), at(60_000).up(now), at(60_000).active(now)), ("reporting", true, true));
        assert_eq!((at(30 * 60_000).status(now), at(90 * 60_000).active(now), at(90 * 60_000).up(now)), ("late", false, true));
        assert_eq!((at(3 * 3_600_000).status(now), at(3 * 3_600_000).up(now)), ("gone", false));
        assert_eq!((host("h").status(now), host("h").up(now)), ("gone", false));
    }

    /// A host's platform from the best of what says it: its runner's facts first, then the Agent's
    /// inventory, then its resource's attributes, then its metrics.
    #[test]
    fn a_host_s_platform_is_the_best_of_what_says_it() {
        let mut agent = agent::AgentHost::default();
        agent.meta.insert("agent_version".into(), serde_json::json!("7.84.4"));
        agent.meta.insert("machine".into(), serde_json::json!("amd64"));
        agent.inventory.insert("os_version".into(), serde_json::json!("ubuntu 24.04"));
        agent.inventory.insert("cpu_cores".into(), serde_json::json!(16));
        agent.inventory.insert("memory_total_kb".into(), serde_json::json!(2048));
        agent.inventory.insert("cloud_provider".into(), serde_json::json!("AWS"));
        let h = Host { agent: Some(Arc::new(agent)), tags: vec![("host.arch".into(), "arm64".into()), ("cloud.region".into(), "eu-west-1".into())], ..host("h") };
        let p = h.platform();
        assert_eq!((p.os.as_deref(), p.arch.as_deref(), p.cores, p.memory_bytes), (Some("ubuntu 24.04"), Some("amd64"), Some(16), Some(2048 * 1024)));
        assert_eq!((p.cloud.as_deref(), p.region.as_deref(), p.agent_version.as_deref()), (Some("AWS"), Some("eu-west-1"), Some("7.84.4")));
        let facts = kiln_ci::facts::RunnerFacts {
            os: "linux".into(),
            os_distro: Some("fedora".into()),
            os_version: Some("43".into()),
            arch: "x86_64".into(),
            cpu: kiln_ci::facts::Cpu { model: Some("AMD Ryzen".into()), cores: 16, threads: 32, flags: Vec::new() },
            mem: kiln_ci::facts::Mem { total: 64 << 30, free: 0 },
            ..kiln_ci::facts::RunnerFacts::default()
        };
        let runner =
            Runner { name: "h".into(), connected: true, last_seen_ms: 0, version: "0.2.0".into(), os: "linux".into(), arch: "x86_64".into(), labels: vec!["gpu".into()], facts: Some(Box::new(facts)) };
        let h = Host { runner: Some(runner), ..h };
        let p = h.platform();
        assert_eq!((p.os.as_deref(), p.arch.as_deref(), p.cores, p.threads, p.memory_bytes), (Some("fedora 43"), Some("x86_64"), Some(16), Some(32), Some(64 << 30)));
        assert_eq!(p.kiln_version.as_deref(), Some("0.2.0"));
        assert_eq!(h.tags_by_source().get("kiln"), Some(&vec!["gpu".to_owned()]));
        // Of metrics alone: its cores and memory as they say them.
        let h = Host { figures: Figures { cores: Some(8.0), memory_mib: Some(1024.0), ..Figures::default() }, ..host("m") };
        assert_eq!((h.platform().cores, h.platform().memory_bytes), (Some(8), Some(1 << 30)));
    }
}
