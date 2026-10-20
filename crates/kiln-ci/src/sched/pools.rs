//! Pools (docs/ci.md W13-R09, W05-R27): capacity that kiln starts and stops itself. A pool names
//! an executor kind, the hosts that may run its executors (a constraint over their facts), an
//! image, and the labels it serves; legs whose `runs-on` labels are all the pool's run in one of
//! its executors. Each executor takes one leg and is discarded, so every leg starts clean.
//!
//! The scheduler keeps each pool's executors (starting, idle, busy). When legs wait for a pool and
//! its idle and starting executors are fewer than them (or than `min_idle`), it starts more on
//! the matching hosts, within `max` and `per_host_max`, the least loaded host first; an executor
//! idle past `idle_timeout` is stopped while the pool keeps `min_idle`. A host that fails to start
//! one is left alone for a while (doubling, to [`MAX_BACKOFF_MS`]).
//!
//! A `k8s` pool's executors are the server's (W13-R07): each is a pod whose own runner connects
//! for its one leg, bound to its executor from the start (`RunnerIn::executor`), so it takes that
//! pool's legs and no other's. Kubernetes places the pods: `hosts` and `per_host_max` don't apply.

use serde::Serialize;

use super::routing::{self, Constraint};
use super::state::Resources;

/// The longest a host that failed to start a pool's executor is left alone.
pub const MAX_BACKOFF_MS: i64 = 5 * 60_000;
/// The first wait after a failed start.
pub const FIRST_BACKOFF_MS: i64 = 2_000;

/// A pool, as configured (`[[ci.pools]]`, or the Pools page).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pool {
    pub name: String,
    /// `docker`, `firecracker`, `shards` or `k8s`.
    pub executor: String,
    /// Which runners may host its executors, with its source text.
    pub hosts: Vec<(String, Constraint)>,
    pub image: Option<String>,
    /// The labels it serves.
    pub labels: Vec<String>,
    pub min_idle: usize,
    pub max: usize,
    pub per_host_max: usize,
    pub idle_timeout_ms: i64,
    /// What each executor takes of its host, in resource mode.
    pub request: Resources,
    /// Capabilities its hosts' runners need (their build starts a pool's executors).
    pub caps: u64,
    /// Its executors are the server's, each its own runner (`k8s`'s pods), not a host runner's.
    pub hosted: bool,
    /// Its pods have a Docker-capable sidecar, for Docker actions (`k8s`, W13-R07).
    pub docker: bool,
}

/// What a pool's configuration says, before it's checked.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PoolSpec {
    pub name: String,
    pub executor: String,
    pub hosts: Option<String>,
    pub image: Option<String>,
    pub labels: Vec<String>,
    pub min_idle: usize,
    pub max: Option<usize>,
    pub per_host_max: Option<usize>,
    pub idle_timeout: Option<String>,
    pub resources: Option<ResourceSpec>,
    /// Kept for matching legs in windows (W05-R12): `{ for, cron, duration, timezone }`.
    pub reserve: Option<super::reserve::ReservationSpec>,
    /// A `k8s` pool's Docker-capable sidecar in each of its pods, for Docker actions (W13-R07).
    pub docker: Option<PodDocker>,
}

/// A Docker-capable sidecar in each of a `k8s` pool's pods (W13-R07): `podman` (Podman's Docker
/// API, the default) or `docker` (rootless Docker), from `image` when given, and the Docker CLI
/// the pod's runner uses, from `cli_image`. The sidecar runs privileged, which is why a pool opts
/// in.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PodDocker {
    pub engine: Option<String>,
    pub image: Option<String>,
    pub cli_image: Option<String>,
}

impl PodDocker {
    /// `podman` or `docker`.
    pub fn engine(&self) -> &str {
        self.engine.as_deref().map(str::trim).filter(|e| !e.is_empty()).unwrap_or("podman")
    }

    /// The sidecar's image.
    pub fn image(&self) -> String {
        let default = if self.engine() == "docker" { "docker:dind-rootless" } else { "quay.io/podman/stable" };
        self.image.as_deref().map(str::trim).filter(|i| !i.is_empty()).unwrap_or(default).to_owned()
    }

    /// The image the Docker CLI is copied from.
    pub fn cli_image(&self) -> String {
        self.cli_image.as_deref().map(str::trim).filter(|i| !i.is_empty()).unwrap_or("docker:cli").to_owned()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResourceSpec {
    pub cpu: Option<String>,
    pub memory: Option<String>,
    pub disk: Option<String>,
    pub gpu: Option<u32>,
}

/// The executor kinds a pool may have.
pub const KINDS: &[&str] = &["docker", "firecracker", "shards", "k8s"];

impl PoolSpec {
    /// Its reservation, if it has one: the pool's executors, for the legs it names.
    pub fn reservation(&self) -> Option<Result<super::reserve::Reservation, String>> {
        let r = self.reserve.as_ref()?;
        let name = if r.name.trim().is_empty() { format!("pool {}", self.name) } else { r.name.clone() };
        Some(super::reserve::ReservationSpec { name, pool: Some(self.name.clone()), runners: None, ..r.clone() }.reservation())
    }

    /// The pool, or what's wrong with it.
    pub fn pool(&self) -> Result<Pool, String> {
        let name = self.name.trim();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
            return Err(format!("pool `{name}`: a name is letters, digits, `-`, `_` and `.`"));
        }
        if !KINDS.contains(&self.executor.as_str()) {
            return Err(format!("pool `{name}`: `executor` is one of {}, not `{}`", KINDS.join(", "), self.executor));
        }
        if self.labels.is_empty() {
            return Err(format!("pool `{name}`: `labels`, the `runs-on` labels it serves"));
        }
        if self.executor != "k8s" && self.image.as_deref().is_none_or(|i| i.trim().is_empty()) {
            return Err(format!("pool `{name}`: `image`, what its executors run"));
        }
        if let Some(d) = &self.docker {
            if self.executor != "k8s" {
                return Err(format!("pool `{name}`: `docker` is a sidecar in a `k8s` pool's pods, and this pool's executors are `{}`", self.executor));
            }
            if !matches!(d.engine(), "podman" | "docker") {
                return Err(format!("pool `{name}`: `docker.engine` is `podman` or `docker`, not `{}`", d.engine()));
            }
        }
        let max = self.max.unwrap_or(self.min_idle.max(1));
        if max == 0 || self.min_idle > max {
            return Err(format!("pool `{name}`: `min_idle` ({}) is at most `max` ({max}), which is at least 1", self.min_idle));
        }
        let hosts = match self.hosts.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
            Some(src) => vec![(src.to_owned(), Constraint::parse(src).map_err(|e| format!("pool `{name}`: `hosts`: {e}"))?)],
            None => Vec::new(),
        };
        let idle = match self.idle_timeout.as_deref() {
            Some(t) => routing::number(t.trim()).filter(|s| *s >= 0.0).ok_or_else(|| format!("pool `{name}`: `idle_timeout` is a duration (`90s`, `10m`), not `{t}`"))?,
            None => 600.0,
        };
        let size = |what: &str, v: &Option<String>| -> Result<f64, String> {
            match v.as_deref() {
                Some(s) => routing::number(s.trim()).filter(|n| *n >= 0.0).ok_or_else(|| format!("pool `{name}`: `resources.{what}` isn't a quantity: `{s}`")),
                None => Ok(0.0),
            }
        };
        let r = self.resources.clone().unwrap_or_default();
        let request = Resources { cpu: size("cpu", &r.cpu)?, mem: size("memory", &r.memory)? as u64, disk: size("disk", &r.disk)? as u64, gpu: r.gpu.unwrap_or(0) };
        Ok(Pool {
            name: name.to_owned(),
            executor: self.executor.clone(),
            hosts,
            image: self.image.clone().map(|i| i.trim().to_owned()).filter(|i| !i.is_empty()),
            labels: self.labels.iter().map(|l| l.trim().to_owned()).filter(|l| !l.is_empty()).collect(),
            min_idle: self.min_idle,
            max,
            per_host_max: self.per_host_max.unwrap_or(max).max(1),
            idle_timeout_ms: (idle * 1000.0) as i64,
            request,
            caps: 0,
            hosted: self.executor == "k8s",
            docker: self.docker.is_some(),
        })
    }
}

impl Pool {
    /// Whether it serves a leg that asks for `labels`: every one is the pool's (`self-hosted`,
    /// which every kiln runner is, aside), and there is one.
    pub fn serves(&self, labels: &[String]) -> bool {
        let wanted: Vec<&String> = labels.iter().filter(|l| !l.eq_ignore_ascii_case("self-hosted")).collect();
        !wanted.is_empty() && wanted.iter().all(|l| self.labels.iter().any(|p| p.eq_ignore_ascii_case(l)))
    }
}

/// Where an executor is in its life.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum ExecState {
    /// Asked of its host, not ready yet.
    Starting,
    /// Ready for a leg.
    Idle,
    /// Running one; discarded when it ends.
    Busy { leg: String },
}

/// One of a pool's executors.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Executor {
    pub id: String,
    pub pool: String,
    pub runner: String,
    #[serde(flatten)]
    pub state: ExecState,
    /// When it entered its state (unix milliseconds).
    pub since: i64,
    /// What it holds of its host, in resource mode.
    #[serde(skip)]
    pub reserved: Resources,
    /// The leg it was made for (a `k8s` pod with that leg's `container:` and `services:`, W13-R07);
    /// else any of its pool's legs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leg: Option<String>,
}

/// A pool as the API shows it: its settings and its executors now.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PoolView {
    pub name: String,
    pub executor: String,
    pub image: Option<String>,
    pub labels: Vec<String>,
    pub hosts: Option<String>,
    pub min_idle: usize,
    pub max: usize,
    pub per_host_max: usize,
    pub idle_timeout_ms: i64,
    pub starting: usize,
    pub idle: usize,
    pub busy: usize,
    /// Legs waiting for it.
    pub waiting: usize,
    pub executors: Vec<Executor>,
    /// The last failure to start one, and on which host.
    pub last_error: Option<(String, String)>,
}

/// How many more executors a pool should start: enough that its idle and starting ones cover the
/// legs waiting for it and `min_idle`, within `max`.
pub fn wanted(pool: &Pool, starting: usize, idle: usize, busy: usize, waiting: usize) -> usize {
    let spare = starting + idle;
    let total = spare + busy;
    let need = waiting.max(pool.min_idle).saturating_sub(spare);
    need.min(pool.max.saturating_sub(total))
}
