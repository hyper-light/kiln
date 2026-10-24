//! The server's list of every container (`docs/observe.md` §10.10): made of the store's index (each
//! container's series by its `container_id`, or OpenTelemetry's `container.id`: their tags, their
//! newest sample; `Store::tag_seen`) and of each figure's series' newest sample
//! (`Store::newest_by`), as the host list is; made again at most every [`FRESH_MS`].

use std::collections::HashMap;
use std::hash::BuildHasher as _;

use foldhash::fast::RandomState as Fast;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use kiln_obs::store::{Reading, Reads};

use super::super::api::Fail;

/// Listed an hour past its last sample; running while it has one within two minutes (eight of the
/// check's 15-second looks); its figures its newest of the last ten minutes.
pub const LISTED_MS: i64 = 3_600_000;
pub const RUNNING_MS: i64 = 2 * 60_000;
pub const FIGURES_MS: i64 = 10 * 60_000;
/// A list is made again at most this often.
pub const FRESH_MS: i64 = 15_000;

/// [`RUNNING_MS`], or `KILN_OBSERVE_CONTAINER_RUNNING_MS` (tests').
pub fn running_ms() -> i64 {
    static MS: OnceLock<i64> = OnceLock::new();
    *MS.get_or_init(|| std::env::var("KILN_OBSERVE_CONTAINER_RUNNING_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(RUNNING_MS))
}

/// What a container's series carry that names it, its image, pod, engine and host: Datadog's
/// container check's tags, and OpenTelemetry's resource attributes.
pub const TAGS: [&str; 20] = [
    "host",
    "host.name",
    "runtime",
    "container.runtime",
    "container_name",
    "container.name",
    "image_name",
    "image_tag",
    "short_image",
    "container.image.name",
    "container.image.tag",
    "kube_namespace",
    "k8s.namespace.name",
    "pod_name",
    "k8s.pod.name",
    "kube_container_name",
    "k8s.container.name",
    "kube_qos",
    "compose_service",
    "compose_project",
];

/// A container's figures, each its newest sample of the last [`FIGURES_MS`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Figures {
    /// Nanocores.
    pub cpu: Option<f64>,
    pub cpu_limit: Option<f64>,
    /// Throttled ns a second.
    pub throttled: Option<f64>,
    /// Bytes.
    pub memory: Option<f64>,
    pub working_set: Option<f64>,
    pub memory_limit: Option<f64>,
    pub pids: Option<f64>,
    /// Bytes a second.
    pub io_read: Option<f64>,
    pub io_write: Option<f64>,
    /// Seconds.
    pub uptime: Option<f64>,
}

/// A container's figures, in [`Figures`]' order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Figure {
    Cpu,
    CpuLimit,
    Throttled,
    Memory,
    WorkingSet,
    MemoryLimit,
    Pids,
    IoRead,
    IoWrite,
    Uptime,
}

pub const FIGURES: usize = 10;

/// The figures in their order.
pub const FIGURE_ORDER: [Figure; FIGURES] =
    [Figure::Cpu, Figure::CpuLimit, Figure::Throttled, Figure::Memory, Figure::WorkingSet, Figure::MemoryLimit, Figure::Pids, Figure::IoRead, Figure::IoWrite, Figure::Uptime];

impl Figures {
    fn put(&mut self, f: Figure, v: f64) {
        let slot = match f {
            Figure::Cpu => &mut self.cpu,
            Figure::CpuLimit => &mut self.cpu_limit,
            Figure::Throttled => &mut self.throttled,
            Figure::Memory => &mut self.memory,
            Figure::WorkingSet => &mut self.working_set,
            Figure::MemoryLimit => &mut self.memory_limit,
            Figure::Pids => &mut self.pids,
            Figure::IoRead => &mut self.io_read,
            Figure::IoWrite => &mut self.io_write,
            Figure::Uptime => &mut self.uptime,
        };
        *slot = Some(v);
    }
}

/// Where a figure is read from: a metric's series, each naming its container by `key` and its
/// reporter by `host` (a container's series of one reporter summed, its devices' and interfaces',
/// and its reporters' taken on average), what is read of each (its newest sample, or its rate a
/// second) times `scale`, into the figure's unit; where `only` names a tag and its value, the
/// series with it alone. On a container's page it is drawn by `{agg}:{metric}{{key}:<id>,only}`
/// and `then` (into the chart's unit).
#[derive(Debug, Clone, Copy)]
pub struct Source {
    pub figure: Figure,
    pub metric: &'static str,
    pub key: &'static str,
    pub reading: Reading,
    pub scale: f64,
    pub only: Option<(&'static str, &'static str)>,
    pub agg: &'static str,
    pub then: &'static str,
}

const fn of(figure: Figure, metric: &'static str, key: &'static str, reading: Reading, scale: f64, agg: &'static str, then: &'static str) -> Source {
    Source { figure, metric, key, reading, scale, only: None, agg, then }
}

const fn only(s: Source, tag: (&'static str, &'static str)) -> Source {
    Source { only: Some(tag), ..s }
}

const DD: &str = "container_id";
const OTEL: &str = "container.id";
const NS: f64 = 1e9;

/// Each figure's sources, kiln's own first (Datadog's container check's names and units: a CPU's
/// nanocores, bytes, rates a second), then OpenTelemetry collectors' (their containers named by
/// `container.id`): Docker's (`docker_stats`: cumulative CPU and throttled time in nanoseconds;
/// its memory "usage", Docker's own figure, the usage less the inactive file cache, which is a
/// working set; its memory limit the host's memory where none is set, the container's limit in
/// effect; bytes read and written by each device, `operation` read or write; a CPU limit, the
/// throttled time, processes and uptime where they are asked for), the kubelet's (`kubeletstats`:
/// CPU in cores, or its cumulative time in seconds; usage and working set; a container named by its
/// id where the receiver is asked for it, `extra_metadata_labels: [container.id]`), the cluster's
/// (`k8s_cluster`: limits in cores and bytes, its containers named by their ids) and the semantic
/// conventions' (`container.cpu.time` and `container.cpu.usage` by `cpu.mode`, summed;
/// `container.disk.io` by `disk.io.direction`). A metric of kiln's and OpenTelemetry's both
/// (`container.cpu.usage`: nanocores in one, cores in the other) is told apart by the tag naming
/// its container. A container's page draws a figure by its first source that reported it.
pub const SOURCES: [Source; 27] = [
    of(Figure::Cpu, "container.cpu.usage", DD, Reading::Newest, 1.0, "avg", " / 1000000000"),
    of(Figure::CpuLimit, "container.cpu.limit", DD, Reading::Newest, 1.0, "avg", " / 1000000000"),
    of(Figure::Throttled, "container.cpu.throttled", DD, Reading::Newest, 1.0, "avg", " / 1000000"),
    of(Figure::Memory, "container.memory.usage", DD, Reading::Newest, 1.0, "avg", ""),
    of(Figure::WorkingSet, "container.memory.working_set", DD, Reading::Newest, 1.0, "avg", ""),
    of(Figure::MemoryLimit, "container.memory.limit", DD, Reading::Newest, 1.0, "avg", ""),
    of(Figure::Pids, "container.pid.count", DD, Reading::Newest, 1.0, "avg", ""),
    of(Figure::IoRead, "container.io.read", DD, Reading::Newest, 1.0, "avg", ""),
    of(Figure::IoWrite, "container.io.write", DD, Reading::Newest, 1.0, "avg", ""),
    of(Figure::Uptime, "container.uptime", DD, Reading::Newest, 1.0, "avg", ""),
    // docker_stats.
    of(Figure::Cpu, "container.cpu.usage.total", OTEL, Reading::Rate, 1.0, "sum", " / 1000000000"),
    of(Figure::CpuLimit, "container.cpu.limit", OTEL, Reading::Newest, NS, "avg", ""),
    of(Figure::Throttled, "container.cpu.throttling_data.throttled_time", OTEL, Reading::Rate, 1.0, "sum", " / 1000000"),
    of(Figure::WorkingSet, "container.memory.usage.total", OTEL, Reading::Newest, 1.0, "avg", ""),
    of(Figure::MemoryLimit, "container.memory.usage.limit", OTEL, Reading::Newest, 1.0, "avg", ""),
    of(Figure::Pids, "container.pids.count", OTEL, Reading::Newest, 1.0, "avg", ""),
    only(of(Figure::IoRead, "container.blockio.io_service_bytes_recursive", OTEL, Reading::Rate, 1.0, "sum", ""), ("operation", "read")),
    only(of(Figure::IoWrite, "container.blockio.io_service_bytes_recursive", OTEL, Reading::Rate, 1.0, "sum", ""), ("operation", "write")),
    of(Figure::Uptime, "container.uptime", OTEL, Reading::Newest, 1.0, "avg", ""),
    // kubeletstats, k8s_cluster, the semantic conventions.
    of(Figure::Cpu, "container.cpu.usage", OTEL, Reading::Newest, NS, "sum", ""),
    of(Figure::Cpu, "container.cpu.time", OTEL, Reading::Rate, NS, "sum", ""),
    of(Figure::CpuLimit, "k8s.container.cpu_limit", OTEL, Reading::Newest, NS, "avg", ""),
    of(Figure::Memory, "container.memory.usage", OTEL, Reading::Newest, 1.0, "avg", ""),
    of(Figure::WorkingSet, "container.memory.working_set", OTEL, Reading::Newest, 1.0, "avg", ""),
    of(Figure::MemoryLimit, "k8s.container.memory_limit", OTEL, Reading::Newest, 1.0, "avg", ""),
    only(of(Figure::IoRead, "container.disk.io", OTEL, Reading::Rate, 1.0, "sum", ""), ("disk.io.direction", "read")),
    only(of(Figure::IoWrite, "container.disk.io", OTEL, Reading::Rate, 1.0, "sum", ""), ("disk.io.direction", "write")),
];

impl Source {
    /// The query that draws this source's figure of container `id` on its page: a rate's a second
    /// whether its series are cumulative counters or each interval's counts.
    pub fn query(&self, id: &str) -> String {
        self.query_in(&format!("{}:{id}", self.key))
    }

    /// The query that draws this source's figure of the series `scope` selects.
    pub fn query_in(&self, scope: &str) -> String {
        let only = self.only.map(|(k, v)| format!(",{k}:{v}")).unwrap_or_default();
        let rate = if self.reading == Reading::Rate { ".as_rate()" } else { "" };
        format!("{}:{}{{{scope}{only}}}{rate}{}", self.agg, self.metric, self.then)
    }
}

/// A container, as its series say it: its tags' keys [`TAGS`]' own, their values shared with the
/// containers that have the same (a host's, an image's, a namespace's).
#[derive(Debug, Clone, PartialEq)]
pub struct Container {
    pub id: String,
    pub series: usize,
    pub last_ms: i64,
    pub tags: Vec<(&'static str, Arc<str>)>,
    pub figures: Figures,
    /// Each figure's first source that reported it ([`SOURCES`]' place), and whether its series
    /// named it by its pod and name alone (the kubelet's, not asked for its id): what its page
    /// draws.
    pub drawn: [Option<(u8, bool)>; FIGURES],
}

impl Container {
    /// Its first tag of `keys`.
    pub fn tag(&self, keys: &[&str]) -> Option<&str> {
        keys.iter().find_map(|k| self.tags.iter().find(|(t, _)| t == k).map(|(_, v)| &**v))
    }

    pub fn name(&self) -> Option<&str> {
        self.tag(&["container_name", "container.name", "kube_container_name", "k8s.container.name"])
    }

    pub fn host(&self) -> Option<&str> {
        self.tag(&["host", "host.name"])
    }

    pub fn image(&self) -> Option<&str> {
        self.tag(&["image_name", "container.image.name", "short_image"])
    }

    pub fn running(&self, now_ms: i64) -> bool {
        now_ms - self.last_ms <= running_ms()
    }

    /// Its Kubernetes namespace, pod and name, as the cluster's receiver or kiln's check names it.
    pub fn kube_names(&self) -> Option<[&str; 3]> {
        Some([self.tag(&["k8s.namespace.name", "kube_namespace"])?, self.tag(&["k8s.pod.name", "pod_name"])?, self.tag(&["k8s.container.name", "kube_container_name"])?])
    }

    /// The query that draws figure `f` on its page, of its first source that reported it: of its
    /// series by its id, or by its pod and name where they named it so.
    pub fn query(&self, f: Figure) -> Option<String> {
        let (at, by_names) = (*self.drawn.get(f as usize)?)?;
        let src = SOURCES.get(usize::from(at))?;
        if by_names {
            let [ns, pod, name] = self.kube_names()?;
            return Some(src.query_in(&format!("k8s.namespace.name:{ns},k8s.pod.name:{pod},k8s.container.name:{name}")));
        }
        Some(src.query(&self.id))
    }
}

/// The list as made at `at_ms`.
#[derive(Debug, Default)]
pub struct Inventory {
    pub at_ms: i64,
    pub containers: Vec<Container>,
    /// How many of them run.
    pub running: usize,
}

/// A reporter's sum of a container's series in a pass: its newest time's (its host's hash, that
/// time, the sum, how many reporters once a pass is summed, and whether its series named the
/// container by its pod and name alone). `n` 0: an empty slot.
#[derive(Debug, Clone, Copy, Default)]
struct Reported {
    host: u64,
    t: i64,
    sum: f64,
    n: u32,
    by_names: bool,
}

impl Reported {
    /// A series at time `t` of value `v`: summed at this time, the sum begun again at a later one,
    /// an earlier one left out.
    fn take(&mut self, t: i64, v: f64) {
        if t > self.t {
            (self.t, self.sum) = (t, v);
        } else if t == self.t {
            self.sum += v;
        }
    }
}

/// The list made of the store at `now_ms`: each container its id's (Datadog's tag's, else
/// OpenTelemetry's attribute's) series, tags and newest sample, read as the index has them (a
/// tag's value made once for every container that has it), and its figures, the store held a
/// metric at a time.
pub fn make(store: &kiln_obs::machine::Shared, now_ms: i64) -> Inventory {
    let mut containers: Vec<Container> = Vec::new();
    {
        let s = store.read().unwrap_or_else(PoisonError::into_inner);
        let mut at: HashMap<&str, usize, Fast> = HashMap::default();
        let mut shared: HashMap<&str, Arc<str>, Fast> = HashMap::default();
        for key in ["container_id", "container.id"] {
            s.tag_seen_each(key, now_ms - LISTED_MS, &TAGS, &mut |c| {
                let i = *at.entry(c.value).or_insert_with(|| {
                    containers.push(Container { id: c.value.to_owned(), series: 0, last_ms: i64::MIN, tags: Vec::with_capacity(c.tags.len()), figures: Figures::default(), drawn: [None; FIGURES] });
                    containers.len() - 1
                });
                let Some(e) = containers.get_mut(i) else { return };
                e.series += c.series;
                e.last_ms = e.last_ms.max(c.last_ms);
                for &(k, v) in c.tags {
                    if let Some(&k) = TAGS.iter().find(|t| **t == k)
                        && !e.tags.iter().any(|(x, _)| *x == k)
                    {
                        let v = shared.entry(v).or_insert_with(|| Arc::from(v)).clone();
                        e.tags.push((k, v));
                    }
                }
            });
        }
    }
    // Each figure of each of its sources, the store held a metric at a time: a container's series
    // of one reporter at its newest time summed (its devices', its modes': one scrape's; a series
    // its reporter no longer writes, its tags since changed, behind them and left out), its
    // reporters' taken on average (two checks of one container), and its sources' (kiln's check
    // and a collector both); found by their place, nothing made a sample.
    let mut sums = vec![[(0.0f64, 0u32); FIGURES]; containers.len()];
    let mut drawn = vec![[None::<(u8, bool)>; FIGURES]; containers.len()];
    {
        // Hashed seeded and fast (foldhash): a 64-character id a series, std's SipHash its time.
        let hosts = Fast::default();
        let at: HashMap<&str, usize, Fast> = containers.iter().enumerate().map(|(i, c)| (c.id.as_str(), i)).collect();
        // Each container by its namespace, pod and name (the cluster's receiver's or kiln's check's
        // names of it): the kubelet's series name a container so unless asked for its id. Found
        // by their hash, then compared.
        let by_names: HashMap<u64, usize, Fast> = containers.iter().enumerate().filter_map(|(i, c)| Some((hosts.hash_one(c.kube_names()?), i))).collect();
        let named = |n: [&str; 3]| by_names.get(&hosts.hash_one(n)).copied().filter(|&i| containers.get(i).and_then(Container::kube_names) == Some(n));
        let mut scratch = kiln_obs::store::Scratch::default();
        // A pass's reporters' sums: each container's first reporter in a slot of its own (a
        // container most often has one), a second and on in `spill`; the containers a pass reached
        // listed, their slots made empty again after it.
        let mut slots = vec![Reported::default(); containers.len()];
        let mut reached: Vec<usize> = Vec::new();
        let mut spill: HashMap<(usize, u64), Reported, Fast> = HashMap::default();
        for (place, src) in SOURCES.iter().enumerate() {
            {
                let s = store.read().unwrap_or_else(PoisonError::into_inner);
                // Each series by its container's place and its reporter: at the reporter's newest
                // time summed, an older one left out.
                let mut put = |i: usize, host: &str, v: f64, t: i64, by_names: bool| {
                    if !v.is_finite() {
                        return;
                    }
                    let (host, v) = (hosts.hash_one(host), v * src.scale);
                    let Some(slot) = slots.get_mut(i) else { return };
                    if slot.n == 0 {
                        *slot = Reported { host, t, sum: v, n: 1, by_names };
                        reached.push(i);
                    } else if slot.host == host {
                        slot.take(t, v);
                    } else {
                        spill.entry((i, host)).or_insert(Reported { host, t, sum: 0.0, n: 1, by_names }).take(t, v);
                    }
                };
                let since_ms = now_ms - FIGURES_MS;
                if src.key == OTEL {
                    // OpenTelemetry's by their id, or by their pod and name (the kubelet's); a
                    // series of neither (kiln's own, its metric's name one of OpenTelemetry's too)
                    // passed over unread.
                    let q = Reads {
                        metric: src.metric,
                        by: [OTEL, "host", "k8s.namespace.name", "k8s.pod.name", "k8s.container.name"],
                        only: src.only,
                        any_of: &[OTEL, "k8s.container.name"],
                        reading: src.reading,
                        since_ms,
                    };
                    s.read_by(&q, &mut scratch, &mut |[id, host, ns, pod, name], v, t| match at.get(id) {
                        Some(&i) => put(i, host, v, t, false),
                        None if id.is_empty() && !pod.is_empty() && !name.is_empty() => {
                            if let Some(i) = named([ns, pod, name]) {
                                put(i, host, v, t, true);
                            }
                        }
                        None => {}
                    });
                } else {
                    // kiln's check's by their id alone: nothing more of a series looked up.
                    let q = Reads { metric: src.metric, by: [src.key, "host"], only: src.only, any_of: &[DD], reading: src.reading, since_ms };
                    s.read_by(&q, &mut scratch, &mut |[id, host], v, t| {
                        if let Some(&i) = at.get(id) {
                            put(i, host, v, t, false);
                        }
                    });
                }
            }
            // A container's reporters on average: its first's slot, its others' from `spill` added in
            // (a slot's `n` now its reporters).
            for (&(i, _), r) in &spill {
                if let Some(slot) = slots.get_mut(i) {
                    (slot.sum, slot.n, slot.by_names) = (slot.sum + r.sum, slot.n + 1, slot.by_names && r.by_names);
                }
            }
            spill.clear();
            let f = src.figure as usize;
            for &i in &reached {
                let Some(r) = slots.get_mut(i) else { continue };
                if let (Some(slot), Some(first)) = (sums.get_mut(i).and_then(|row| row.get_mut(f)), drawn.get_mut(i).and_then(|row| row.get_mut(f))) {
                    slot.0 += r.sum / f64::from(r.n);
                    slot.1 += 1;
                    first.get_or_insert((u8::try_from(place).unwrap_or(u8::MAX), r.by_names));
                }
                *r = Reported::default();
            }
            reached.clear();
        }
    }
    for ((c, row), first) in containers.iter_mut().zip(&sums).zip(drawn) {
        for (&f, &(sum, n)) in FIGURE_ORDER.iter().zip(row) {
            if n > 0 {
                c.figures.put(f, sum / f64::from(n));
            }
        }
        c.drawn = first;
    }
    containers.sort_by(|a, b| a.name().unwrap_or(&a.id).cmp(b.name().unwrap_or(&b.id)).then_with(|| a.id.cmp(&b.id)));
    let running = containers.iter().filter(|c| c.running(now_ms)).count();
    Inventory { at_ms: now_ms, containers, running }
}

/// A list older than [`FRESH_MS`] is given while another is made, up to this old; one older (the
/// first asked after a quiet while) is made before it is given.
pub const STALE_MS: i64 = 60_000;

/// The list kept between askings.
#[derive(Default)]
pub struct Containers {
    kept: Mutex<Option<Arc<Inventory>>>,
    making: AtomicBool,
}

static CONTAINERS: OnceLock<Arc<Containers>> = OnceLock::new();

pub fn get() -> &'static Arc<Containers> {
    CONTAINERS.get_or_init(Arc::default)
}

impl Containers {
    /// The list: the one made within [`FRESH_MS`]; one made within [`STALE_MS`] while another is
    /// made (one at a time); else one made now, on the blocking pool.
    pub async fn inventory(self: &Arc<Self>) -> Result<Arc<Inventory>, Fail> {
        let now = super::super::clock_ms();
        let kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner).clone();
        match kept {
            Some(inv) if (0..FRESH_MS).contains(&(now - inv.at_ms)) => Ok(inv),
            Some(inv) if (0..STALE_MS).contains(&(now - inv.at_ms)) => {
                if !self.making.swap(true, Ordering::AcqRel) {
                    let me = self.clone();
                    drop(crate::rt::spawn(async move {
                        if let Err(e) = me.make(super::super::clock_ms()).await {
                            tracing::debug!(error = ?e, "the containers' list was not made again");
                        }
                        me.making.store(false, Ordering::Release);
                    }));
                }
                Ok(inv)
            }
            _ => self.make(now).await,
        }
    }

    async fn make(&self, now: i64) -> Result<Arc<Inventory>, Fail> {
        let inv = Arc::new(super::super::api::apart(move |store| Ok(make(store, now))).await?);
        let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        // A list made later kept already (two askers at once) stays.
        if kept.as_ref().is_none_or(|k| k.at_ms <= inv.at_ms) {
            *kept = Some(inv.clone());
        }
        Ok(inv)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use kiln_obs::batch::{Builder, Kind, SeriesKey};
    use kiln_obs::hash::Keyed;
    use kiln_obs::store::Store;

    use super::*;

    /// Containers from their series: kiln's check's (its id, names and figures), OpenTelemetry's
    /// (`container.id`, its own names); one that stopped reporting not running; a container's
    /// figures each its newest sample.
    #[test]
    fn containers_are_listed_from_their_series() {
        let now = 1_792_670_400_000;
        let mut s = Store::new(Keyed::with_keys(1, 2));
        let mut b = Builder::new();
        let ours: [(&str, &str); 6] = [("host", "node-1"), ("container_id", "aaa1"), ("runtime", "docker"), ("container_name", "checkout"), ("image_name", "shop/checkout"), ("image_tag", "2.41.0")];
        for (metric, v, at) in
            [("container.cpu.usage", 5e8, now - 30_000), ("container.cpu.usage", 6e8, now - 15_000), ("container.memory.usage", 1.5e8, now - 15_000), ("container.memory.limit", 2.68e8, now - 15_000)]
        {
            let k = SeriesKey::new(metric, ours.iter().copied(), Kind::Gauge).unwrap();
            b.define(&k);
            b.run(k.fp, [(at, v)]);
        }
        let otel = SeriesKey::new("container.memory.usage", [("host.name", "node-2"), ("container.id", "bbb2"), ("container.name", "worker")], Kind::Gauge).unwrap();
        b.define(&otel);
        b.run(otel.fp, [(now - 600_000, 1.0)]);
        s.apply(&b.finish()).unwrap();
        let store: kiln_obs::machine::Shared = std::sync::Arc::new(kiln_obs::lock::RwLock::new(s));
        let inv = make(&store, now);
        assert_eq!(inv.containers.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["aaa1", "bbb2"], "by name: checkout, worker");
        let a = &inv.containers[0];
        assert_eq!((a.name(), a.host(), a.image(), a.tag(&["image_tag"]), a.running(now)), (Some("checkout"), Some("node-1"), Some("shop/checkout"), Some("2.41.0"), true));
        assert_eq!((a.figures.cpu, a.figures.memory, a.figures.memory_limit, a.figures.pids), (Some(6e8), Some(1.5e8), Some(2.68e8), None), "the newest of each");
        let b = &inv.containers[1];
        assert_eq!((b.name(), b.host(), b.running(now)), (Some("worker"), Some("node-2"), false), "ten minutes quiet: not running");
    }

    /// A container's series of one reporter: those at its newest time summed (one look's), one
    /// it no longer writes (its tags since changed: a name learned) left out, however recent; two
    /// reporters' on average.
    #[test]
    fn a_reporter_s_series_left_behind_are_not_counted() {
        let now = 1_792_670_400_000;
        let mut s = Store::new(Keyed::with_keys(1, 2));
        let mut b = Builder::new();
        let mut put = |tags: &[(&str, &str)], at: i64, v: f64| {
            let k = SeriesKey::new("container.cpu.usage", tags.iter().copied(), Kind::Gauge).unwrap();
            b.define(&k);
            b.run(k.fp, [(at, v)]);
        };
        put(&[("host", "node-1"), ("container_id", "aaa1")], now - 45_000, 9e8);
        put(&[("host", "node-1"), ("container_id", "aaa1"), ("container_name", "web")], now - 30_000, 2e8);
        put(&[("host", "node-1"), ("container_id", "aaa1"), ("container_name", "web")], now - 15_000, 3e8);
        put(&[("host", "node-2"), ("container_id", "aaa1"), ("container_name", "web")], now - 15_000, 5e8);
        s.apply(&b.finish()).unwrap();
        let store: kiln_obs::machine::Shared = Arc::new(kiln_obs::lock::RwLock::new(s));
        let inv = make(&store, now);
        assert_eq!(inv.containers.iter().map(|c| (c.id.as_str(), c.figures.cpu)).collect::<Vec<_>>(), [("aaa1", Some(4e8))], "node-1's newest, 3e8, and node-2's 5e8 on average");
    }

    /// Containers OpenTelemetry's collectors report, in their own names and units, each named by
    /// `container.id`: Docker's (`docker_stats`: cumulative CPU and throttled nanoseconds read as
    /// rates; its usage less the cache, a working set; each device's bytes read summed, its other
    /// operations left out; its limits), the kubelet's and the cluster's (CPU in cores, its modes
    /// summed; usage, working set; limits in cores and bytes). One reported by kiln's check and
    /// Docker's collector both: their figures on average, drawn by kiln's. Each figure drawn by its
    /// first source, a rate's query a rate whatever its series' kind.
    #[test]
    fn containers_are_read_in_open_telemetry_s_names_and_units() {
        let now = 1_792_670_400_000;
        let mut s = Store::new(Keyed::with_keys(1, 2));
        let mut b = Builder::new();
        let put = |b: &mut Builder, metric: &str, tags: &[(&str, &str)], kind: Kind, pts: &[(i64, f64)]| {
            let k = SeriesKey::new(metric, tags.iter().copied(), kind).unwrap();
            b.define(&k);
            b.run(k.fp, pts.iter().copied());
        };
        let (t0, t1) = (now - 30_000, now - 15_000);
        let docker = [("host", "node-1"), ("container.id", "d0c1"), ("container.name", "web"), ("container.image.name", "nginx:1.27"), ("container.runtime", "docker")];
        put(&mut b, "container.cpu.usage.total", &docker, Kind::Counter, &[(t0, 5e9), (t1, 6.5e9)]);
        put(&mut b, "container.cpu.limit", &docker, Kind::Gauge, &[(t1, 0.5)]);
        put(&mut b, "container.cpu.throttling_data.throttled_time", &docker, Kind::Counter, &[(t0, 0.0), (t1, 3e8)]);
        put(&mut b, "container.memory.usage.total", &docker, Kind::Gauge, &[(t1, 4e7)]);
        put(&mut b, "container.memory.usage.limit", &docker, Kind::Gauge, &[(t1, 2.68e8)]);
        let device = |major: &'static str, op: &'static str| [docker[0], docker[1], ("device_major", major), ("device_minor", "0"), ("operation", op)];
        put(&mut b, "container.blockio.io_service_bytes_recursive", &device("8", "read"), Kind::Counter, &[(t0, 1e4), (t1, 1.15e4)]);
        put(&mut b, "container.blockio.io_service_bytes_recursive", &device("259", "read"), Kind::Counter, &[(t0, 5e3), (t1, 6.5e3)]);
        put(&mut b, "container.blockio.io_service_bytes_recursive", &device("8", "write"), Kind::Counter, &[(t0, 9e3), (t1, 9e3)]);
        put(&mut b, "container.blockio.io_service_bytes_recursive", &device("8", "total"), Kind::Counter, &[(t0, 0.0), (t1, 9e9)]);
        let kube = [("host", "node-2"), ("container.id", "k8s1"), ("k8s.container.name", "api"), ("k8s.pod.name", "api-7d9f"), ("k8s.namespace.name", "shop")];
        let mode = |m: &'static str| [kube[0], kube[1], kube[2], kube[3], kube[4], ("cpu.mode", m)];
        put(&mut b, "container.cpu.usage", &mode("user"), Kind::Gauge, &[(t1, 0.25)]);
        put(&mut b, "container.cpu.usage", &mode("system"), Kind::Gauge, &[(t1, 0.125)]);
        put(&mut b, "container.memory.usage", &kube, Kind::Gauge, &[(t1, 3e8)]);
        put(&mut b, "container.memory.working_set", &kube, Kind::Gauge, &[(t1, 2e8)]);
        put(&mut b, "k8s.container.cpu_limit", &kube, Kind::Gauge, &[(t1, 1.0)]);
        put(&mut b, "k8s.container.memory_limit", &kube, Kind::Gauge, &[(t1, 5.12e8)]);
        put(&mut b, "container.cpu.usage", &[("host", "node-3"), ("container_id", "b0th"), ("container_name", "both")], Kind::Gauge, &[(t1, 3e8)]);
        put(&mut b, "container.cpu.usage.total", &[("host", "node-3"), ("container.id", "b0th")], Kind::Counter, &[(t0, 0.0), (t1, 7.5e9)]);
        s.apply(&b.finish()).unwrap();
        let store: kiln_obs::machine::Shared = Arc::new(kiln_obs::lock::RwLock::new(s));
        let inv = make(&store, now);
        let by = |id: &str| inv.containers.iter().find(|c| c.id == id).unwrap();
        let d = by("d0c1");
        assert_eq!((d.name(), d.image(), d.tag(&["runtime", "container.runtime"])), (Some("web"), Some("nginx:1.27"), Some("docker")));
        let f = d.figures;
        assert_eq!(
            (f.cpu, f.cpu_limit, f.throttled, f.memory, f.working_set, f.memory_limit, f.io_read, f.io_write),
            (Some(1e8), Some(5e8), Some(2e7), None, Some(4e7), Some(2.68e8), Some(200.0), Some(0.0)),
            "1.5e9 ns over 15 s, 0.1 cores; two devices' 1,500 bytes each over 15 s"
        );
        assert_eq!(d.query(Figure::Cpu).as_deref(), Some("sum:container.cpu.usage.total{container.id:d0c1}.as_rate() / 1000000000"));
        assert_eq!(d.query(Figure::IoRead).as_deref(), Some("sum:container.blockio.io_service_bytes_recursive{container.id:d0c1,operation:read}.as_rate()"));
        assert_eq!((d.query(Figure::Memory), d.query(Figure::Pids)), (None, None), "none reported them");
        let k = by("k8s1");
        let f = k.figures;
        assert_eq!((k.name(), f.cpu, f.cpu_limit, f.memory, f.working_set, f.memory_limit), (Some("api"), Some(3.75e8), Some(1e9), Some(3e8), Some(2e8), Some(5.12e8)), "its modes' cores summed");
        assert_eq!(k.query(Figure::Cpu).as_deref(), Some("sum:container.cpu.usage{container.id:k8s1}"));
        assert_eq!(k.query(Figure::CpuLimit).as_deref(), Some("avg:k8s.container.cpu_limit{container.id:k8s1}"));
        let both = by("b0th");
        assert_eq!((both.name(), both.figures.cpu), (Some("both"), Some(4e8)), "kiln's 3e8 and Docker's 5e8 on average");
        assert_eq!(both.query(Figure::Cpu).as_deref(), Some("avg:container.cpu.usage{container_id:b0th} / 1000000000"), "drawn by kiln's");
    }

    /// The kubelet's series of a container it names by its pod alone (not asked for its id):
    /// joined to the container the cluster's receiver names by its id, namespace, pod and name, and
    /// drawn by those names; the same names in another namespace, or a pod no container is known
    /// by, not joined.
    #[test]
    fn the_kubelet_s_series_join_their_container_by_its_pod_and_name() {
        let now = 1_792_670_400_000;
        let mut s = Store::new(Keyed::with_keys(1, 2));
        let mut b = Builder::new();
        let mut put = |metric: &str, tags: &[(&str, &str)], v: f64| {
            let k = SeriesKey::new(metric, tags.iter().copied(), Kind::Gauge).unwrap();
            b.define(&k);
            b.run(k.fp, [(now - 15_000, v)]);
        };
        let names = |ns: &'static str, pod: &'static str| [("host", "node-1"), ("k8s.namespace.name", ns), ("k8s.pod.name", pod), ("k8s.container.name", "api")];
        let cluster = [("container.id", "c0ff"), ("k8s.namespace.name", "shop"), ("k8s.pod.name", "api-1"), ("k8s.container.name", "api"), ("container.image.name", "shop/api")];
        put("k8s.container.cpu_limit", &cluster, 1.0);
        put("container.cpu.usage", &names("shop", "api-1"), 0.5);
        put("container.memory.working_set", &names("shop", "api-1"), 1e8);
        put("container.cpu.usage", &names("other", "api-1"), 0.75);
        put("container.cpu.usage", &names("shop", "api-2"), 0.875);
        s.apply(&b.finish()).unwrap();
        let store: kiln_obs::machine::Shared = Arc::new(kiln_obs::lock::RwLock::new(s));
        let inv = make(&store, now);
        assert_eq!(inv.containers.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["c0ff"], "the kubelet's alone name no container of their own");
        let c = &inv.containers[0];
        assert_eq!((c.figures.cpu, c.figures.cpu_limit, c.figures.working_set), (Some(5e8), Some(1e9), Some(1e8)), "its pod's in shop alone");
        assert_eq!(c.query(Figure::Cpu).as_deref(), Some("sum:container.cpu.usage{k8s.namespace.name:shop,k8s.pod.name:api-1,k8s.container.name:api}"));
        assert_eq!(c.query(Figure::CpuLimit).as_deref(), Some("avg:k8s.container.cpu_limit{container.id:c0ff}"));
    }
}
