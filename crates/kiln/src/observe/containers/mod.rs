//! Every container a node runs (`docs/observe.md` §10.10), each node looking at its own every
//! [`super::host::EVERY`], as Datadog's container check does: its CPU (in use, user and system, its
//! limit, its throttling), its memory (in use, its working set, its RSS and cache, its limit, its OOM
//! kills), its block I/O and its processes, in the check's names and units (`container.cpu.*` in
//! nanocores, `container.memory.*` in bytes, `container.io.*` a second).
//!
//! - Linux reads the cgroups (`cgroup`): every container any engine runs (Docker, containerd,
//!   CRI-O, Podman; a Kubernetes pod's; one nested in a node's, minikube's), no engine asked.
//! - Its names: Kubernetes' (`kube`: its pod, namespace and container), and the engine's
//!   (`engine`: its name, image, Compose service), the engine asked when a container it does not
//!   know appears, at most once a look. A node without either reports its containers by id.
//! - Where the cgroups are not seen (macOS's and Windows' Docker Desktop, its VM), the engine's
//!   stats are read instead, a container at a time.
//! - Tags: `host`, `container_id`, `runtime`; and as known `container_name`, `image_name`,
//!   `image_tag`, `short_image`, `kube_namespace`, `pod_name`, `kube_container_name`, `kube_qos`,
//!   `compose_service`, `compose_project`.
//!
//! The server lists them (`inventory`, `http`), as its host list does hosts, from the store's index.

pub mod cgroup;
pub mod engine;
pub mod http;
pub mod inventory;
pub mod kube;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use kiln_obs::batch::{Kind, SeriesKey};
use kiln_obs::otlp::metrics::Decoded;

use super::Observe;

/// Containers a node reports at the most (a Kubernetes node runs 110 pods unless told otherwise;
/// a build host a few hundred containers at its busiest).
pub const CONTAINERS: usize = 1_000;
/// Containers found at the most, pods' sandboxes among them (one a pod).
const FOUND: usize = 2 * CONTAINERS;
/// How often the engine is asked again for its names when no container it does not know appears:
/// a container renamed, or its labels changed.
pub const RENAMED: Duration = Duration::from_secs(300);
/// How long a container the engine did not name is asked about again at each look: one whose
/// cgroup is there before the engine lists it, as it starts (its samples until then are written
/// without its name: a series apart from its own).
const UNNAMED_ASKED: Duration = Duration::from_secs(30);
/// The engine's events asked for again at each look, before where the last ask ended: an event the
/// engine stamps a little late (its VM's clock, a moment behind this machine's) is not missed, and
/// none is counted twice (those counted are kept that long).
const OOM_LAG_NS: i64 = 10_000_000_000;
/// OOM kills kept counted, to be told apart from the next window's (a storm's at the most).
const OOM_COUNTED: usize = 100_000;

/// The check's metrics, in Datadog's names and kinds, in the order a container's keys are kept.
const METRICS: [(&str, Kind); 18] = [
    ("container.memory.usage", Kind::Gauge),
    ("container.memory.working_set", Kind::Gauge),
    ("container.memory.rss", Kind::Gauge),
    ("container.memory.cache", Kind::Gauge),
    ("container.memory.limit", Kind::Gauge),
    ("container.pid.count", Kind::Gauge),
    ("container.cpu.limit", Kind::Gauge),
    ("container.uptime", Kind::Gauge),
    ("container.cpu.usage", Kind::Gauge),
    ("container.cpu.user", Kind::Gauge),
    ("container.cpu.system", Kind::Gauge),
    ("container.cpu.throttled", Kind::Gauge),
    ("container.io.read", Kind::Gauge),
    ("container.io.write", Kind::Gauge),
    ("container.io.read.operations", Kind::Gauge),
    ("container.io.write.operations", Kind::Gauge),
    ("container.cpu.throttled.periods", Kind::Count),
    ("container.memory.oom_events", Kind::Count),
];

/// An image's name, tag and short name (Datadog's `image_name`, `image_tag`, `short_image`): its
/// digest left out, its tag after its last `:` past its last `/`.
pub fn image_parts(image: &str) -> (&str, Option<&str>, &str) {
    let image = image.split('@').next().unwrap_or(image);
    let slash = image.rfind('/').map_or(0, |i| i + 1);
    let (name, tag) = match image.rfind(':').filter(|&c| c > slash) {
        Some(c) => (image.get(..c).unwrap_or(image), image.get(c + 1..).filter(|t| !t.is_empty())),
        None => (image, None),
    };
    let short = name.rsplit('/').next().unwrap_or(name);
    (name, tag, short)
}

/// Whether an image (its short name) is a pod's sandbox's: Kubernetes' `pause`, as registries
/// name it (`registry.k8s.io/pause`, Rancher's `mirrored-pause`, OpenShift's `origin-pod`).
fn sandbox_image(short: &str) -> bool {
    short == "pause" || short.starts_with("pause-") || short == "mirrored-pause" || short == "origin-pod"
}

/// A container a look found: its id, engine, pod (uid, QoS), and stats.
struct Seen {
    id: String,
    runtime: &'static str,
    qos: Option<&'static str>,
    stats: cgroup::Stats,
}

/// What the check keeps of a container between looks: its counters, against which the next look's
/// rates are taken; its series' keys, made at its first look and again when its tags change (a
/// name learned), so that a look makes none; and the look that last saw it.
struct Kept {
    at: Instant,
    counters: cgroup::Counters,
    tags: Vec<(String, String)>,
    keys: [Option<SeriesKey>; METRICS.len()],
    look: u64,
}

/// The check: what it reads, and what it read last (its counters, the engine's names).
pub struct Check {
    host: String,
    root: PathBuf,
    links: PathBuf,
    proc: PathBuf,
    version: Option<cgroup::Version>,
    engine: Option<engine::Endpoint>,
    kept: HashMap<String, Kept>,
    /// Pods' containers known to be their sandboxes or not, each asked once: the look that last
    /// saw it.
    sandboxes: HashMap<String, (bool, u64)>,
    named: HashMap<String, engine::Named>,
    /// The containers the engine did not name when last asked, and when it first did not: after
    /// [`UNNAMED_ASKED`] another engine's (a Kubernetes node's containerd beside Docker), not a
    /// reason to ask it again.
    unnamed: HashMap<String, Instant>,
    asked: Option<Instant>,
    /// Kubernetes' names, read again when its links change.
    pods: kube::Names,
    looks: u64,
    reader: cgroup::Reader,
    /// Where there are no cgroups to read (Docker Desktop's VM): each container's limits as its
    /// settings say them, and when they were asked (again every [`RENAMED`]); its OOM kills the
    /// engine told of since the first look; where the events were last asked to, and those counted
    /// in the last [`OOM_LAG_NS`].
    limits: HashMap<String, (engine::Limits, Instant)>,
    ooms: HashMap<String, u64>,
    oom_until: Option<i64>,
    oom_counted: std::collections::HashSet<(i64, String)>,
}

impl Check {
    /// A check of this node's containers (`/sys/fs/cgroup`, `/var/log/containers`, `/proc`, the
    /// engine's socket).
    pub fn new(host: String) -> Self {
        let root = PathBuf::from("/sys/fs/cgroup");
        let version = if cfg!(target_os = "linux") { cgroup::version(&root) } else { None };
        Self::at(host, root, PathBuf::from("/var/log/containers"), version, engine::Endpoint::find())
    }

    /// A check of what is at these places (a test's).
    pub fn at(host: String, root: PathBuf, links: PathBuf, version: Option<cgroup::Version>, engine: Option<engine::Endpoint>) -> Self {
        Self {
            host,
            root,
            links,
            proc: PathBuf::from("/proc"),
            version,
            engine,
            kept: HashMap::new(),
            sandboxes: HashMap::new(),
            named: HashMap::new(),
            unnamed: HashMap::new(),
            asked: None,
            pods: kube::Names::default(),
            looks: 0,
            reader: cgroup::Reader::default(),
            limits: HashMap::new(),
            ooms: HashMap::new(),
            oom_until: None,
            oom_counted: std::collections::HashSet::new(),
        }
    }

    /// The processes' names read from `proc` rather than `/proc` (a test's).
    pub fn with_proc(mut self, proc: PathBuf) -> Self {
        self.proc = proc;
        self
    }

    /// What it reads (for a node's status): its cgroups' version and its engine.
    pub fn sources(&self) -> (Option<cgroup::Version>, Option<&engine::Endpoint>) {
        (self.version, self.engine.as_ref())
    }

    /// The containers this look sees, pods' sandboxes left out.
    fn seen(&mut self) -> Vec<Seen> {
        match self.version {
            Some(v) => {
                let look = self.looks;
                let mut out = Vec::new();
                for f in cgroup::discover(&self.root, v, FOUND) {
                    if out.len() >= CONTAINERS {
                        break;
                    }
                    if f.pod_uid.is_some() {
                        let verdict = match self.sandboxes.get_mut(&f.id) {
                            Some(known) => {
                                known.1 = look;
                                Some(known.0)
                            }
                            None => {
                                let is = cgroup::sandbox(&self.root, v, &f, &self.proc, &mut self.reader);
                                if let Some(b) = is {
                                    self.sandboxes.insert(f.id.clone(), (b, look));
                                }
                                is
                            }
                        };
                        if verdict == Some(true) {
                            continue;
                        }
                    }
                    if let Some(stats) = cgroup::stats(&self.root, v, &f, &mut self.reader) {
                        out.push(Seen { id: f.id, runtime: f.runtime, qos: f.qos, stats });
                    }
                }
                self.sandboxes.retain(|_, (_, l)| *l == look);
                out
            }
            None => {
                let Some(e) = self.engine.clone() else { return Vec::new() };
                self.count_ooms(&e);
                let now = Instant::now();
                let named = &self.named;
                self.limits.retain(|id, _| named.contains_key(id));
                self.ooms.retain(|id, _| named.contains_key(id));
                let mut out = Vec::new();
                for id in self.named.keys().take(CONTAINERS) {
                    let Ok(mut stats) = engine::stats(&e, id) else { continue };
                    // Its limits: the stats carry none of its CPU, and the VM's memory where it has
                    // none of its own.
                    if self.limits.get(id).is_none_or(|(_, at)| now.duration_since(*at) >= RENAMED)
                        && let Ok(l) = engine::limits(&e, id)
                    {
                        self.limits.insert(id.clone(), (l, now));
                    }
                    if let Some((l, _)) = self.limits.get(id) {
                        (stats.cpu_limit, stats.memory_limit) = (l.cpu, l.memory);
                    }
                    stats.counters.oom_kills = self.ooms.get(id).copied().unwrap_or(0);
                    out.push(Seen { id: id.clone(), runtime: "docker", qos: None, stats });
                }
                out
            }
        }
    }

    /// The OOM kills the engine told of since the last look, each container's counted (the stats
    /// carry none): from where the last ask ended, less [`OOM_LAG_NS`], to a second ago, those
    /// counted already left out. None before the first look (as the cgroups' count from it).
    fn count_ooms(&mut self, e: &engine::Endpoint) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
        let until = now.saturating_sub(1_000_000_000);
        let Some(last) = self.oom_until else {
            self.oom_until = Some(until);
            return;
        };
        match engine::ooms(e, last.saturating_sub(OOM_LAG_NS), until) {
            Ok(events) => {
                for (at, id) in events {
                    if self.oom_counted.len() < OOM_COUNTED && self.oom_counted.insert((at, id.clone())) {
                        *self.ooms.entry(id).or_insert(0) += 1;
                    }
                }
                self.oom_counted.retain(|(at, _)| *at >= until.saturating_sub(OOM_LAG_NS));
                self.oom_until = Some(until);
            }
            Err(err) => tracing::debug!(error = %err, "the container engine's events were not read"),
        }
    }

    /// The engine's names asked for: at the first look; at a look where a container appears that it
    /// has not named, nor left unnamed for [`UNNAMED_ASKED`] (once a look: a container is named at
    /// the look it first appears in, or the next); every [`RENAMED`]; and every look where there are
    /// no cgroups to read (its list is what there is).
    fn rename(&mut self, ids: &[&str], now: Instant) {
        let Some(e) = &self.engine else { return };
        let due = match self.asked.map(|a| now.duration_since(a)) {
            None => true,
            Some(since) => {
                self.version.is_none() || since >= RENAMED || ids.iter().any(|id| !self.named.contains_key(*id) && self.unnamed.get(*id).is_none_or(|at| now.duration_since(*at) < UNNAMED_ASKED))
            }
        };
        if !due {
            return;
        }
        self.asked = Some(now);
        match engine::list(e) {
            Ok(named) => {
                let before = std::mem::take(&mut self.unnamed);
                self.unnamed = ids.iter().filter(|id| !named.contains_key(**id)).map(|id| ((*id).to_owned(), before.get(*id).copied().unwrap_or(now))).collect();
                self.named = named;
            }
            Err(err) => tracing::debug!(error = %err, "the container engine was not asked its names"),
        }
    }

    /// One look: every container's figures, its rates against the last look's, written into `out`
    /// (a decoding the owner hands back: a look allocates nothing for its samples).
    #[allow(clippy::cast_precision_loss)]
    pub fn look(&mut self, ts: i64, out: &mut Decoded) {
        let now = Instant::now();
        self.looks += 1;
        let look = self.looks;
        if self.version.is_none() {
            // Without cgroups, the engine's list is what there is to read.
            self.rename(&[], now);
        }
        let seen = self.seen();
        if self.version.is_some() {
            let ids: Vec<&str> = seen.iter().map(|s| s.id.as_str()).collect();
            self.rename(&ids, now);
        }
        let pods = self.pods.of(&self.links, CONTAINERS * 4, now);
        let mut tags: Vec<(&str, &str)> = Vec::with_capacity(16);
        for s in &seen {
            let named = self.named.get(&s.id);
            let image = named.filter(|n| !n.image.is_empty()).map(|n| image_parts(&n.image));
            // A pod's sandbox where the engine names its image (Docker's Kubernetes): not a container.
            if image.is_some_and(|(_, _, short)| sandbox_image(short)) {
                continue;
            }
            tags.clear();
            tags.extend([("host", self.host.as_str()), ("container_id", s.id.as_str()), ("runtime", s.runtime)]);
            if let Some(n) = named
                && !n.name.is_empty()
            {
                tags.push(("container_name", &n.name));
            }
            if let Some((name, tag, short)) = image {
                tags.push(("image_name", name));
                if let Some(t) = tag {
                    tags.push(("image_tag", t));
                }
                tags.push(("short_image", short));
            }
            if let Some(n) = named {
                for (label, tag) in [("com.docker.compose.service", "compose_service"), ("com.docker.compose.project", "compose_project")] {
                    if let Some(v) = n.labels.get(label) {
                        tags.push((tag, v));
                    }
                }
            }
            if let Some(p) = pods.get(&s.id) {
                tags.extend([("kube_namespace", p.namespace.as_str()), ("pod_name", p.pod.as_str()), ("kube_container_name", p.container.as_str())]);
                if !tags.iter().any(|(k, _)| *k == "container_name") {
                    tags.push(("container_name", &p.container));
                }
            }
            if let Some(q) = s.qos {
                tags.push(("kube_qos", q));
            }
            let first = !self.kept.contains_key(&s.id);
            if first {
                self.kept.insert(s.id.clone(), Kept { at: now, counters: s.stats.counters, tags: Vec::new(), keys: Default::default(), look });
            }
            let Some(k) = self.kept.get_mut(&s.id) else { continue };
            // Its tags changed (a name learned): its keys made again.
            if k.tags.len() != tags.len() || k.tags.iter().zip(&tags).any(|((a, b), (c, d))| a != c || b != d) {
                k.tags = tags.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect();
                k.keys = Default::default();
            }
            let (st, c) = (&s.stats, s.stats.counters);
            // Rates against the last look's counters: none at its first.
            let rated = (!first).then(|| (now.duration_since(k.at).as_secs_f64(), k.counters)).filter(|(secs, _)| *secs > 0.0);
            let rate = |f: fn(&cgroup::Counters) -> u64| rated.map(|(secs, p)| f(&c).saturating_sub(f(&p)) as f64 / secs);
            let count = |f: fn(&cgroup::Counters) -> u64| rated.map(|(_, p)| f(&c).saturating_sub(f(&p)) as f64);
            let g = |v: Option<u64>| v.map(|v| v as f64);
            let values: [Option<f64>; METRICS.len()] = [
                g(st.memory_usage),
                g(st.memory_working_set),
                g(st.memory_rss),
                g(st.memory_cache),
                g(st.memory_limit),
                g(st.pids),
                st.cpu_limit.map(|cores| cores * 1e9),
                named.filter(|n| n.created > 0).map(|n| (ts / 1000 - n.created).max(0) as f64),
                rate(|c| c.cpu_ns),
                rate(|c| c.user_ns),
                rate(|c| c.system_ns),
                rate(|c| c.throttled_ns),
                rate(|c| c.read_bytes),
                rate(|c| c.write_bytes),
                rate(|c| c.read_ops),
                rate(|c| c.write_ops),
                count(|c| c.throttled_periods),
                count(|c| c.oom_kills),
            ];
            for ((v, slot), (metric, kind)) in values.into_iter().zip(k.keys.iter_mut()).zip(METRICS) {
                let Some(v) = v.filter(|v| v.is_finite()) else { continue };
                if slot.is_none() {
                    *slot = SeriesKey::new(metric, tags.iter().copied(), kind).ok();
                }
                if let Some(key) = slot
                    && let Ok(at) = u32::try_from(out.keys.len())
                {
                    out.keys.push(key.clone());
                    out.samples.push((at, ts, v));
                    out.points += 1;
                }
            }
            (k.at, k.counters, k.look) = (now, c, look);
        }
        // What is gone is let go: its counters and keys; its names when the engine next lists.
        self.kept.retain(|_, k| k.look == look);
    }
}

/// Looks every `every` at this node's containers, from now, on a thread of its own.
pub fn start(o: Observe, host: String, every: Duration) {
    if every.is_zero() {
        return;
    }
    let spawned = std::thread::Builder::new().name("kiln-observe-containers".into()).spawn(move || {
        let mut c = Check::new(host);
        let (version, engine) = c.sources();
        tracing::debug!(?version, ?engine, "observe's container check started");
        loop {
            let started = Instant::now();
            let mut d = o.decoding();
            c.look(crate::util::now_ms(), &mut d);
            o.write_samples(d);
            std::thread::sleep(every.saturating_sub(started.elapsed()));
        }
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "observe's container check did not start");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const A: &str = "56bca3739fe8bd187d49969e4a6a2af672a186b7830f64c9d7fd41c88ec8fdff";

    fn file(root: &std::path::Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    /// A look's samples, each with its series.
    fn look(check: &mut Check, ts: i64) -> Vec<(SeriesKey, Vec<(i64, f64)>)> {
        let mut d = Decoded::default();
        check.look(ts, &mut d);
        d.samples.iter().map(|&(i, ts, v)| (d.keys[i as usize].clone(), vec![(ts, v)])).collect()
    }

    /// An image's parts as Datadog's tags have them: a registry's port, a digest, no tag.
    #[test]
    fn an_image_is_named_tagged_and_shortened() {
        assert_eq!(image_parts("registry.example.com:5000/shop/checkout:2.41.0"), ("registry.example.com:5000/shop/checkout", Some("2.41.0"), "checkout"));
        assert_eq!(image_parts("gcr.io/k8s-minikube/kicbase:v0.0.50@sha256:abcd"), ("gcr.io/k8s-minikube/kicbase", Some("v0.0.50"), "kicbase"));
        assert_eq!(image_parts("registry.example.com:5000/busybox"), ("registry.example.com:5000/busybox", None, "busybox"));
        assert_eq!(image_parts("nginx"), ("nginx", None, "nginx"));
        assert!(sandbox_image(image_parts("registry.k8s.io/pause:3.10").2) && sandbox_image("mirrored-pause") && !sandbox_image("pauser"));
    }

    /// Two looks at a container (cgroup v2), named by Kubernetes' links: its gauges each look, its
    /// rates the second (CPU in nanocores, I/O a second, its OOM kills counted), its tags its pod's;
    /// a container gone, its counters let go.
    #[test]
    fn a_container_s_figures_are_read_and_rated() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("cgroup");
        let links = dir.path().join("links");
        let scope = format!("kubepods.slice/kubepods-besteffort.slice/kubepods-besteffort-pod1f2e3d4c_5b6a_7980_a1b2_c3d4e5f60718.slice/cri-containerd-{A}.scope");
        file(&root, "cgroup.controllers", "cpu memory io pids");
        let write = |cpu_us: u64, oom: u64, rbytes: u64| {
            file(&root, &format!("{scope}/cpu.stat"), &format!("usage_usec {cpu_us}\nuser_usec {cpu_us}\nsystem_usec 0\nnr_throttled 0\nthrottled_usec 0\n"));
            file(&root, &format!("{scope}/memory.events"), &format!("oom_kill {oom}\n"));
            file(&root, &format!("{scope}/io.stat"), &format!("8:0 rbytes={rbytes} wbytes=0 rios=0 wios=0\n"));
        };
        write(1_000_000, 0, 0);
        file(&root, &format!("{scope}/memory.current"), "1048576\n");
        file(&links, &format!("web-0_shop_nginx-{A}.log"), "");
        let mut check = Check::at("node-1".into(), root.clone(), links, cgroup::version(&root), None);
        let first = look(&mut check, 1_000);
        let names = |pts: &[(SeriesKey, Vec<(i64, f64)>)]| -> Vec<String> {
            let mut n: Vec<String> = pts.iter().filter_map(|(k, _)| kiln_obs::batch::parse_key(&k.key).ok().map(|p| p.metric.to_owned())).collect();
            n.sort();
            n
        };
        assert_eq!(names(&first), ["container.memory.usage", "container.memory.working_set"], "gauges the first look, no rate");
        let p = kiln_obs::batch::parse_key(&first[0].0.key).unwrap();
        let tags: HashMap<&str, &str> = p.tags.iter().map(|(k, v)| (*k, *v)).collect();
        assert_eq!(
            (tags["host"], tags["container_id"], tags["runtime"], tags["kube_namespace"], tags["pod_name"], tags["container_name"], tags["kube_qos"]),
            ("node-1", A, "containerd", "shop", "web-0", "nginx", "besteffort")
        );
        // A second later (as the clock says): a core's worth of CPU and an OOM kill.
        std::thread::sleep(Duration::from_millis(20));
        write(1_020_000, 1, 2048);
        let second = look(&mut check, 2_000);
        let value = |m: &str| second.iter().find(|(k, _)| kiln_obs::batch::parse_key(&k.key).is_ok_and(|p| p.metric == m)).map(|(_, v)| v[0].1).unwrap();
        assert!(value("container.cpu.usage") > 0.0 && value("container.io.read") > 0.0);
        assert_eq!(value("container.memory.oom_events"), 1.0);
        // Gone: nothing said of it, its counters let go.
        std::fs::remove_dir_all(root.join(&scope)).unwrap();
        assert!(look(&mut check, 3_000).is_empty() && check.kept.is_empty());
    }

    /// A stand-in for the engine on a Unix socket: each request's path answered by `answer`
    /// (HTTP/1.0, whole, then closed).
    #[cfg(unix)]
    fn engine_at(dir: &std::path::Path, answer: impl Fn(&str) -> String + Send + 'static) -> engine::Endpoint {
        let path = dir.join("engine.sock");
        let l = std::os::unix::net::UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { break };
                let mut buf = [0u8; 4096];
                let n = std::io::Read::read(&mut s, &mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let body = answer(req.split_whitespace().nth(1).unwrap_or(""));
                let _ = std::io::Write::write_all(&mut s, format!("HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{body}").as_bytes());
            }
        });
        engine::Endpoint::Unix(path)
    }

    /// Where there are no cgroups to read (Docker Desktop's VM): a container's figures are the
    /// engine's stats, its limits its settings' (the stats carry none of its CPU, and the VM's memory
    /// where it has none of its own: none then), its OOM kills the engine's events, counted from the
    /// first look, each once though the next window asks for it again.
    #[cfg(unix)]
    #[test]
    fn without_cgroups_a_container_s_limits_and_oom_kills_are_the_engine_s() {
        let dir = tempfile::tempdir().unwrap();
        let (limited, free) = ("a".repeat(64), "b".repeat(64));
        let events = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let e = engine_at(dir.path(), {
            let (limited, free, events) = (limited.clone(), free.clone(), events.clone());
            move |path| {
                if path == "/containers/json" {
                    return format!(r#"[{{"Id":"{limited}","Names":["/busy"],"Image":"alpine:latest","Created":1}},{{"Id":"{free}","Names":["/free"],"Image":"alpine:latest","Created":1}}]"#);
                }
                if path.starts_with("/events") {
                    return events.lock().unwrap().clone();
                }
                let id = path.trim_start_matches("/containers/").split('/').next().unwrap_or("").to_owned();
                if path.ends_with("/json") {
                    let host = if id == limited { r#"{"NanoCpus":500000000,"Memory":67108864}"# } else { r#"{"NanoCpus":0,"Memory":0}"# };
                    return format!(r#"{{"Id":"{id}","HostConfig":{host}}}"#);
                }
                r#"{"cpu_stats":{"cpu_usage":{"total_usage":1000}},"memory_stats":{"usage":4096,"stats":{"inactive_file":0},"limit":33554432000},"pids_stats":{"current":1}}"#.to_owned()
            }
        });
        let mut check = Check::at("desk".into(), dir.path().join("no-cgroups"), dir.path().join("links"), None, Some(e));
        let value = |pts: &[(SeriesKey, Vec<(i64, f64)>)], id: &str, m: &str| {
            pts.iter().find(|(k, _)| kiln_obs::batch::parse_key(&k.key).is_ok_and(|p| p.metric == m && p.tags.iter().any(|(t, v)| *t == "container_id" && *v == id))).map(|(_, v)| v[0].1)
        };
        let first = look(&mut check, 1_000);
        assert_eq!((value(&first, &limited, "container.cpu.limit"), value(&first, &limited, "container.memory.limit")), (Some(5e8), Some(67_108_864.0)), "its limits, its settings'");
        assert_eq!((value(&first, &free, "container.cpu.limit"), value(&first, &free, "container.memory.limit")), (None, None), "none: not the VM's");
        assert_eq!(value(&first, &free, "container.memory.usage"), Some(4096.0));
        // Two OOM kills of the limited one, told of in the window the next look asks for and again
        // in the one after (its lag).
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as i64;
        *events.lock().unwrap() =
            [now - 3_000_000_000, now - 2_000_000_000].iter().map(|t| format!(r#"{{"Type":"container","Action":"oom","Actor":{{"ID":"{limited}"}},"timeNano":{t}}}"#) + "\n").collect();
        std::thread::sleep(Duration::from_millis(20));
        let second = look(&mut check, 2_000);
        assert_eq!((value(&second, &limited, "container.memory.oom_events"), value(&second, &free, "container.memory.oom_events")), (Some(2.0), Some(0.0)));
        std::thread::sleep(Duration::from_millis(20));
        let third = look(&mut check, 3_000);
        assert_eq!(value(&third, &limited, "container.memory.oom_events"), Some(0.0), "each counted once");
    }

    /// A pod's sandbox is not a container (its `pause` process; its image, where the engine names
    /// it), one whose process is not seen from here is; a container's keys are made at its first
    /// look and kept, made again when its tags change (a name learned).
    #[test]
    fn sandboxes_are_left_out_and_keys_kept() {
        let dir = tempfile::tempdir().unwrap();
        let (root, links, proc) = (dir.path().join("cgroup"), dir.path().join("links"), dir.path().join("proc"));
        file(&root, "cgroup.controllers", "cpu memory io pids");
        let pod = "kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod1f2e3d4c_5b6a_7980_a1b2_c3d4e5f60718.slice";
        let (sandbox, app, unseen) = ("a".repeat(64), "b".repeat(64), "c".repeat(64));
        for (id, pid) in [(&sandbox, "4242\n"), (&app, "4243\n"), (&unseen, "")] {
            file(&root, &format!("{pod}/cri-containerd-{id}.scope/cpu.stat"), "usage_usec 1000\n");
            file(&root, &format!("{pod}/cri-containerd-{id}.scope/memory.current"), "4096\n");
            file(&root, &format!("{pod}/cri-containerd-{id}.scope/cgroup.procs"), pid);
        }
        file(&proc, "4242/comm", "pause\n");
        file(&proc, "4243/comm", "nginx\n");
        let mut check = Check::at("node-1".into(), root.clone(), links, cgroup::version(&root), None).with_proc(proc);
        let ids = |pts: &[(SeriesKey, Vec<(i64, f64)>)]| -> Vec<String> {
            let mut v: Vec<String> =
                pts.iter().filter_map(|(k, _)| kiln_obs::batch::parse_key(&k.key).ok().and_then(|p| p.tags.iter().find(|(t, _)| *t == "container_id").map(|(_, v)| (*v).to_owned()))).collect();
            v.sort();
            v.dedup();
            v
        };
        let usage = |pts: &[(SeriesKey, Vec<(i64, f64)>)]| {
            let of = |k: &SeriesKey| kiln_obs::batch::parse_key(&k.key).is_ok_and(|p| p.metric == "container.memory.usage" && p.tags.iter().any(|(t, v)| *t == "container_id" && *v == app));
            pts.iter().find(|(k, _)| of(k)).map(|(k, _)| k.key.clone()).unwrap()
        };
        let first = look(&mut check, 1_000);
        assert_eq!(ids(&first), [app.clone(), unseen.clone()], "the sandbox left out; one whose process is not seen kept");
        let second = look(&mut check, 2_000);
        assert!(std::sync::Arc::ptr_eq(&usage(&first), &usage(&second)), "its keys kept, none made");
        check.named.insert(app.clone(), engine::Named { name: "web".into(), image: "nginx:1.29".into(), ..engine::Named::default() });
        let third = usage(&look(&mut check, 3_000));
        assert!(!std::sync::Arc::ptr_eq(&usage(&second), &third), "named: made again");
        assert!(kiln_obs::batch::parse_key(&third).unwrap().tags.iter().any(|(t, v)| *t == "container_name" && *v == "web"));
        check.named.insert(unseen.clone(), engine::Named { name: "k8s_POD_web-0".into(), image: "registry.k8s.io/pause:3.10".into(), ..engine::Named::default() });
        assert_eq!(ids(&look(&mut check, 4_000)), std::slice::from_ref(&app), "a sandbox by its image");
        assert!(check.sandboxes.contains_key(&sandbox) && !check.sandboxes.contains_key(&unseen) && !check.kept.contains_key(&unseen), "a verdict kept where one was had; what is gone let go");
    }
}
