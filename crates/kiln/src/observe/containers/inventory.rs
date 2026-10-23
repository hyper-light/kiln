//! The server's list of every container (`docs/observe.md` §10.10): made of the store's index (each
//! container's series by its `container_id`, or OpenTelemetry's `container.id`: their tags, their
//! newest sample; `Store::tag_seen`) and of each figure's series' newest sample
//! (`Store::newest_by`), as the host list is; made again at most every [`FRESH_MS`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

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

#[allow(clippy::type_complexity)]
const FIGURE_METRICS: [(&str, fn(&mut Figures, f64)); 10] = [
    ("container.cpu.usage", |f, v| f.cpu = Some(v)),
    ("container.cpu.limit", |f, v| f.cpu_limit = Some(v)),
    ("container.cpu.throttled", |f, v| f.throttled = Some(v)),
    ("container.memory.usage", |f, v| f.memory = Some(v)),
    ("container.memory.working_set", |f, v| f.working_set = Some(v)),
    ("container.memory.limit", |f, v| f.memory_limit = Some(v)),
    ("container.pid.count", |f, v| f.pids = Some(v)),
    ("container.io.read", |f, v| f.io_read = Some(v)),
    ("container.io.write", |f, v| f.io_write = Some(v)),
    ("container.uptime", |f, v| f.uptime = Some(v)),
];

/// A container, as its series say it: its tags' keys [`TAGS`]' own, their values shared with the
/// containers that have the same (a host's, an image's, a namespace's).
#[derive(Debug, Clone, PartialEq)]
pub struct Container {
    pub id: String,
    pub series: usize,
    pub last_ms: i64,
    pub tags: Vec<(&'static str, Arc<str>)>,
    pub figures: Figures,
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
}

/// The list as made at `at_ms`.
#[derive(Debug, Default)]
pub struct Inventory {
    pub at_ms: i64,
    pub containers: Vec<Container>,
    /// How many of them run.
    pub running: usize,
}

/// The list made of the store at `now_ms`: each container its id's (Datadog's tag's, else
/// OpenTelemetry's attribute's) series, tags and newest sample, read as the index has them (a
/// tag's value made once for every container that has it), and its figures, the store held a
/// metric at a time.
pub fn make(store: &kiln_obs::machine::Shared, now_ms: i64) -> Inventory {
    let mut containers: Vec<Container> = Vec::new();
    {
        let s = store.read().unwrap_or_else(PoisonError::into_inner);
        let mut at: HashMap<&str, usize> = HashMap::new();
        let mut shared: HashMap<&str, Arc<str>> = HashMap::new();
        for key in ["container_id", "container.id"] {
            s.tag_seen_each(key, now_ms - LISTED_MS, &TAGS, &mut |c| {
                let i = *at.entry(c.value).or_insert_with(|| {
                    containers.push(Container { id: c.value.to_owned(), series: 0, last_ms: i64::MIN, tags: Vec::with_capacity(c.tags.len()), figures: Figures::default() });
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
    // Each figure's samples summed by container (two checks of one taken on average), found by
    // their place: nothing made a sample.
    let mut sums = vec![[(0.0f64, 0u32); FIGURE_METRICS.len()]; containers.len()];
    {
        let at: HashMap<&str, usize> = containers.iter().enumerate().map(|(i, c)| (c.id.as_str(), i)).collect();
        let mut scratch = kiln_obs::store::Scratch::default();
        for (m, (metric, _)) in FIGURE_METRICS.iter().enumerate() {
            let s = store.read().unwrap_or_else(PoisonError::into_inner);
            s.newest_by(metric, "container_id", now_ms - FIGURES_MS, &mut scratch, &mut |id, v| {
                if v.is_finite()
                    && let Some(slot) = at.get(id).and_then(|&i| sums.get_mut(i)).and_then(|row| row.get_mut(m))
                {
                    slot.0 += v;
                    slot.1 += 1;
                }
            });
        }
    }
    for (c, row) in containers.iter_mut().zip(&sums) {
        for ((_, put), (sum, n)) in FIGURE_METRICS.iter().zip(row) {
            if *n > 0 {
                put(&mut c.figures, sum / f64::from(*n));
            }
        }
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
        let store: kiln_obs::machine::Shared = std::sync::Arc::new(std::sync::RwLock::new(s));
        let inv = make(&store, now);
        assert_eq!(inv.containers.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["aaa1", "bbb2"], "by name: checkout, worker");
        let a = &inv.containers[0];
        assert_eq!((a.name(), a.host(), a.image(), a.tag(&["image_tag"]), a.running(now)), (Some("checkout"), Some("node-1"), Some("shop/checkout"), Some("2.41.0"), true));
        assert_eq!((a.figures.cpu, a.figures.memory, a.figures.memory_limit, a.figures.pids), (Some(6e8), Some(1.5e8), Some(2.68e8), None), "the newest of each");
        let b = &inv.containers[1];
        assert_eq!((b.name(), b.host(), b.running(now)), (Some("worker"), Some("node-2"), false), "ten minutes quiet: not running");
    }
}
