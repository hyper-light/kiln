//! AC-T7.4.1: the file-backed peer core drives the actual selected runtime. Publication follows
//! durable admission, physical creation and durable acknowledgement, in that order. A physical
//! box (even an empty process box) holds its reservation until deletion, never merely until exit.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::Instant;

use hex_api::{BoxId, BoxInfo, BoxSpec, BoxState, Refusal, Runtime};
use hex_sim::index::Pick;
use hex_sim::model::{Class, Demand, NodeId, NodeSpec, Resources};
use hex_sim::peer::{self, Boxed, Budget, Key, Msg, Stop, Told};

pub mod budget;
mod devices;
mod physical;
mod rows;
use rows::{Context, Rows};
mod view;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payload {
    pub spec: BoxSpec,
    pub start: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub msg: Msg,
    pub payload: Option<Payload>,
    pub refusal: Option<Refusal>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    Started {
        key: Key,
        node: NodeId,
    },
    Ended {
        key: Key,
    },
    Refused {
        key: Key,
        node: NodeId,
        refusal: Refusal,
    },
    /// The box may exist. Admission is retained; an operator may retry deletion or reboot.
    Unknown {
        key: Key,
        refusal: Refusal,
        cleanup: Option<Refusal>,
    },
    Status {
        key: Key,
        info: triomphe::Arc<BoxInfo>,
    },
}

#[derive(Debug, Default)]
pub struct Step {
    pub send: Vec<(NodeId, Envelope)>,
    pub notices: Vec<Notice>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub key: Key,
    pub node: Option<NodeId>,
    pub local: bool,
    pub created: bool,
    pub deleting: bool,
    pub ended: bool,
    pub physical: BoxId,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("placement stopped: {0:?}")]
    Core(Stop),
    #[error(transparent)]
    Runtime(#[from] Refusal),
}

impl From<Stop> for Error {
    fn from(stop: Stop) -> Self {
        Self::Core(stop)
    }
}

/// The operator supplies backend truth. A registry has at most the scheduler's three classes;
/// runtimes without a device-attachment seam cannot silently accept scheduler device assignments.
#[derive(Clone, Default)]
pub struct Runtimes {
    by_class: BTreeMap<Class, Arc<dyn Runtime>>,
}

/// One completed query, not an inventory cache. Unavailable VM observations never provide room.
pub(super) struct CapacitySnapshot {
    pub reports: Vec<(Class, Option<hex_api::Capacity>)>,
    pub unavailable: Vec<hex_api::BackendUnavailable>,
}

impl CapacitySnapshot {
    pub(super) fn add_unavailable(
        &mut self,
        observation: hex_api::BackendUnavailable,
    ) -> Result<(), Refusal> {
        physical::add_unavailable(&mut self.unavailable, observation)
    }
}

fn backend_class(class: Class) -> hex_api::Class {
    match class {
        Class::Oci => hex_api::Class::Oci,
        Class::Firecracker => hex_api::Class::Firecracker,
        Class::Shards => hex_api::Class::Shards,
    }
}

impl Runtimes {
    pub fn new(
        entries: impl IntoIterator<Item = (Class, Arc<dyn Runtime>)>,
    ) -> Result<Self, Error> {
        let mut by_class = BTreeMap::new();
        for (class, runtime) in entries {
            let supported = runtime.classes();
            let initialized = match class {
                Class::Oci => {
                    supported.contains(&hex_api::Class::Oci)
                        || supported.contains(&hex_api::Class::Process)
                }
                Class::Firecracker => supported.contains(&hex_api::Class::Firecracker),
                Class::Shards => supported.contains(&hex_api::Class::Shards),
            };
            if !initialized {
                return Err(invalid(
                    "runtime endpoint does not implement its configured placement backend",
                )
                .into());
            }
            if by_class.insert(class, runtime).is_some() {
                return Err(invalid("duplicate runtime class").into());
            }
        }
        Ok(Self { by_class })
    }

    fn get(&self, class: Class) -> Result<Arc<dyn Runtime>, Error> {
        self.by_class
            .get(&class)
            .cloned()
            .ok_or_else(|| invalid("no registered runtime for placement class").into())
    }

    /// Query each configured backend without holding any placement or record lock.
    pub fn capacities(&self) -> Result<Vec<(Class, Option<hex_api::Capacity>)>, Error> {
        let snapshot = self.capacity_snapshot()?;
        if let Some(unavailable) = snapshot.unavailable.into_iter().next() {
            return Err(unavailable.cause.into());
        }
        Ok(snapshot.reports)
    }

    fn native_cpu_capacity(&self) -> Result<hex_api::Capacity, Error> {
        let native = self.get(Class::Oci)?;
        if !native.classes().contains(&hex_api::Class::Process) {
            return Err(invalid_static(
                "unavailable VM observer has no initialized native CPU owner",
            )
            .into());
        }
        let capacity = native
            .capacity(hex_api::Class::Process)?
            .ok_or_else(|| invalid_static("native CPU owner supplied no resource plane"))?;
        capacity.check()?;
        if capacity.cost.is_some()
            || (!capacity.routes.is_empty()
                && !capacity.routes.iter().any(|route| {
                    route.key.class == hex_api::Class::Process
                        && route.key.execution == hex_api::Execution::Native
                        && route.cost.is_none()
                        && route.backing.is_none()
                }))
        {
            return Err(
                invalid_static("native CPU resource plane is not actual native execution").into(),
            );
        }
        Ok(capacity)
    }

    pub(super) fn capacity_snapshot(&self) -> Result<CapacitySnapshot, Error> {
        let mut reports = Vec::new();
        let mut unavailable = Vec::new();
        reports
            .try_reserve_exact(self.by_class.len())
            .map_err(|_| Refusal::Exhausted {
                what: "initialized runtime snapshots",
                bound: Class::COUNT,
            })?;
        for (&class, runtime) in &self.by_class {
            let initialized = runtime.classes();
            let form = match class {
                Class::Oci if initialized.contains(&hex_api::Class::Oci) => hex_api::Class::Oci,
                Class::Oci => hex_api::Class::Process,
                Class::Firecracker => hex_api::Class::Firecracker,
                Class::Shards => hex_api::Class::Shards,
            };
            if !initialized.contains(&form) {
                return Err(invalid_static(
                    "registered endpoint is not initialized for its placement backend",
                )
                .into());
            }
            let report = match runtime.capacity(form) {
                Ok(report) => report,
                Err(cause) => {
                    // A composite OCI endpoint cannot identify a failed VM from the payload
                    // class. Its composition owner must return the native plane separately.
                    let observation =
                        hex_api::BackendUnavailable::from_refusal(backend_class(class), cause)?;
                    physical::add_unavailable(&mut unavailable, observation)?;
                    continue;
                }
            };
            if let Some(report) = &report {
                report.check()?;
                if report
                    .routes
                    .iter()
                    .any(|route| !initialized.contains(&route.key.backend_class()))
                {
                    return Err(invalid_static(
                        "route demands a backend absent from this initialized endpoint",
                    )
                    .into());
                }
            }
            reports.push((class, report));
        }
        if !unavailable.is_empty() {
            let native = self.native_cpu_capacity()?;
            let current = reports
                .iter_mut()
                .find(|(class, _)| *class == Class::Oci)
                .and_then(|(_, report)| report.as_mut())
                .ok_or_else(|| {
                    invalid_static("unavailable observer has no successful native CPU report")
                })?;
            if current.id != native.id || current.total != native.total {
                return Err(invalid_static(
                    "native CPU query changed its resource-plane authority",
                )
                .into());
            }
            current.free.cpu_milli = current.free.cpu_milli.min(native.free.cpu_milli);
            current.free.mem_bytes = current.free.mem_bytes.min(native.free.mem_bytes);
            current.free.disk_bytes = current.free.disk_bytes.min(native.free.disk_bytes);
        }
        Ok(CapacitySnapshot {
            reports,
            unavailable,
        })
    }

    /// Keep the originating endpoint: a composite native owner routes its canonical PoolRecord,
    /// while a generic Shards/native endpoint cannot initialize a Firecracker route.
    pub fn prepared_backings(
        reports: &[(Class, Option<hex_api::Capacity>)],
    ) -> Result<Vec<(Class, hex_api::BackingKey)>, Error> {
        let mut pending = Vec::new();
        pending
            .try_reserve_exact(hex_api::MAX_RUNTIME_ROUTES)
            .map_err(|_| Refusal::Exhausted {
                what: "prepared native backing count",
                bound: hex_api::MAX_RUNTIME_ROUTES,
            })?;
        for (class, report) in reports {
            let Some(report) = report else { continue };
            report.check()?;
            for backing in &report.backings {
                if backing.phase != hex_api::BackingPhase::Prepared
                    || !report
                        .routes
                        .iter()
                        .any(|route| route.backing == Some(backing.key))
                    || pending.iter().any(|(_, key)| *key == backing.key)
                {
                    continue;
                }
                if pending.len() == hex_api::MAX_RUNTIME_ROUTES {
                    return Err(Refusal::Exhausted {
                        what: "prepared native backing count",
                        bound: hex_api::MAX_RUNTIME_ROUTES,
                    }
                    .into());
                }
                pending.push((*class, backing.key));
            }
        }
        Ok(pending)
    }

    pub fn released_backings(
        reports: &[(Class, Option<hex_api::Capacity>)],
    ) -> Result<Vec<(Class, hex_api::BackingOffer)>, Error> {
        let mut released: Vec<(Class, hex_api::BackingOffer)> = Vec::new();
        released
            .try_reserve_exact(hex_api::MAX_RUNTIME_ROUTES)
            .map_err(|_| Refusal::Exhausted {
                what: "released native backing count",
                bound: hex_api::MAX_RUNTIME_ROUTES,
            })?;
        for (class, report) in reports {
            let Some(report) = report else { continue };
            report.check()?;
            for backing in &report.backings {
                if backing.phase != hex_api::BackingPhase::Released
                    || released.iter().any(|(_, prior)| prior.key == backing.key)
                    || report.devices.iter().any(|device| {
                        matches!(device.holder.as_ref(), Some(hex_api::DeviceHolder::Box(_)))
                            && backing.devices.binary_search(&device.id).is_ok()
                    })
                {
                    continue;
                }
                if released.len() == hex_api::MAX_RUNTIME_ROUTES {
                    return Err(Refusal::Exhausted {
                        what: "released native backing count",
                        bound: hex_api::MAX_RUNTIME_ROUTES,
                    }
                    .into());
                }
                released.push((*class, backing.try_clone()?));
            }
        }
        Ok(released)
    }

    /// Canonical drain intent is accounting/cleanup authority, independent of fresh GPU readings.
    /// Keep the actual endpoint and complete physical set for the same native owner.
    pub fn draining_backings(
        reports: &[(Class, Option<hex_api::Capacity>)],
    ) -> Result<Vec<(Class, hex_api::BackingOffer)>, Error> {
        let mut draining: Vec<(Class, hex_api::BackingOffer)> = Vec::new();
        draining
            .try_reserve_exact(hex_api::MAX_RUNTIME_ROUTES)
            .map_err(|_| Refusal::Exhausted {
                what: "draining native backing count",
                bound: hex_api::MAX_RUNTIME_ROUTES,
            })?;
        for (class, report) in reports {
            let Some(report) = report else { continue };
            report.check()?;
            for backing in &report.backings {
                if backing.phase != hex_api::BackingPhase::Draining {
                    continue;
                }
                if let Some((_, prior)) =
                    draining.iter().find(|(_, prior)| prior.key == backing.key)
                {
                    if prior != backing {
                        return Err(
                            invalid_static("canonical backing drain receipts disagree").into()
                        );
                    }
                    continue;
                }
                if draining.len() == hex_api::MAX_RUNTIME_ROUTES {
                    return Err(Refusal::Exhausted {
                        what: "draining native backing count",
                        bound: hex_api::MAX_RUNTIME_ROUTES,
                    }
                    .into());
                }
                draining.push((*class, backing.try_clone()?));
            }
        }
        Ok(draining)
    }

    /// The native Store/turn owner supplies facts over the existing Runtime seam. This registry
    /// has no mutable query lock, and no provider is opened in the scheduling process.
    pub fn physical_report(&self) -> Result<Option<hex_api::PhysicalReport>, Error> {
        let mut reports = Vec::new();
        let mut unavailable = Vec::new();
        let mut native_succeeded = false;
        reports
            .try_reserve_exact(self.by_class.len())
            .map_err(|_| Refusal::Exhausted {
                what: "native runtime report count",
                bound: Class::COUNT,
            })?;
        for (&class, runtime) in &self.by_class {
            let report = match runtime.physical_report() {
                Ok(report) => report,
                Err(cause) => {
                    let observation =
                        hex_api::BackendUnavailable::from_refusal(backend_class(class), cause)?;
                    physical::add_unavailable(&mut unavailable, observation)?;
                    continue;
                }
            };
            report.check()?;
            native_succeeded |=
                class == Class::Oci && runtime.classes().contains(&hex_api::Class::Process);
            // A resource pool may be a guest's narrower envelope; its id is not the physical
            // Store/turn authority. Coalesce validates actual report authorities independently.
            reports.push(report);
        }
        let mut report = physical::coalesce(reports)?;
        if !unavailable.is_empty()
            || report
                .as_ref()
                .is_some_and(|report| !report.unavailable.is_empty())
        {
            if !native_succeeded {
                return Err(invalid_static(
                    "unavailable observer has no successful native physical report",
                )
                .into());
            }
            self.native_cpu_capacity()?;
            let actual = report
                .as_mut()
                .ok_or_else(|| invalid_static("native physical owner supplied no report"))?;
            for observation in unavailable {
                physical::add_unavailable(&mut actual.unavailable, observation)?;
            }
            actual.check()?;
        }
        Ok(report)
    }

    /// Read the one native scope per live backing outside all placement/model locks. A shared
    /// guest's inner workloads have no additional native RSS; unrelated backings receive no credit.
    pub fn measured_backing_use(
        &self,
        reports: &[(Class, Option<hex_api::Capacity>)],
    ) -> Result<Resources, Error> {
        self.measure_backing_use(reports, None)
    }

    pub(super) fn measured_available_backing_use(
        &self,
        snapshot: &mut CapacitySnapshot,
    ) -> Result<Resources, Error> {
        self.measure_backing_use(&snapshot.reports, Some(&mut snapshot.unavailable))
    }

    fn measure_backing_use(
        &self,
        reports: &[(Class, Option<hex_api::Capacity>)],
        mut unavailable: Option<&mut Vec<hex_api::BackendUnavailable>>,
    ) -> Result<Resources, Error> {
        let mut seen = Vec::new();
        seen.try_reserve_exact(hex_api::MAX_RUNTIME_ROUTES)
            .map_err(|_| Refusal::Exhausted {
                what: "measured backing identities",
                bound: hex_api::MAX_RUNTIME_ROUTES,
            })?;
        let mut owned = Resources::default();
        for (class, report) in reports {
            let Some(report) = report else { continue };
            report.check()?;
            for backing in &report.backings {
                if !matches!(
                    backing.phase,
                    hex_api::BackingPhase::Live | hex_api::BackingPhase::Draining
                ) || seen.contains(&backing.key)
                {
                    continue;
                }
                if seen.len() >= hex_api::MAX_RUNTIME_ROUTES {
                    return Err(Refusal::Exhausted {
                        what: "measured backing identities",
                        bound: hex_api::MAX_RUNTIME_ROUTES,
                    }
                    .into());
                }
                let usage = match self.get(*class)?.backing_usage(&backing.key) {
                    Ok(usage) => usage,
                    Err(cause) => {
                        let Some(observations) = unavailable.as_deref_mut() else {
                            return Err(cause.into());
                        };
                        let backend = report
                            .routes
                            .iter()
                            .find(|route| route.backing == Some(backing.key))
                            .map(|route| route.key.backend_class())
                            .ok_or_else(|| {
                                invalid_static("measured backing has no initialized route")
                            })?;
                        physical::add_unavailable(
                            observations,
                            hex_api::BackendUnavailable::from_refusal(backend, cause)?,
                        )?;
                        // An unread scope receives no measured-usage credit. Its canonical full
                        // allocation/claims remain in the retained route catalog and Index.
                        continue;
                    }
                };
                owned.mem_bytes = owned
                    .mem_bytes
                    .checked_add(usage.memory_bytes)
                    .ok_or_else(|| invalid_static("native backing memory sum overflows"))?;
                seen.push(backing.key);
            }
        }
        if unavailable
            .as_ref()
            .is_some_and(|values| !values.is_empty())
        {
            self.native_cpu_capacity()?;
        }
        Ok(owned)
    }
}

fn checked_namespace(namespace: &str) -> Result<&str, Refusal> {
    // 128-bit UUID-width nonce or a complete SHA-256 digest, in canonical hexadecimal.
    if !matches!(namespace.len(), 32 | 64)
        || !namespace
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid(
            "placement namespace must be 32 or 64 lowercase hexadecimal digits",
        ));
    }
    Ok(namespace)
}

pub fn physical_id(namespace: &str, key: Key) -> Result<BoxId, Refusal> {
    let namespace = checked_namespace(namespace)?;
    BoxId::new(&format!(
        "hex-place-{namespace}-{}-{}-{}",
        key.owner, key.epoch, key.seq
    ))
}

fn invalid(why: impl Into<String>) -> Refusal {
    Refusal::Invalid {
        what: "placement",
        why: why.into(),
    }
}
fn invalid_static(why: &'static str) -> Refusal {
    let mut owned = String::new();
    if owned.try_reserve_exact(why.len()).is_err() {
        return Refusal::Exhausted {
            what: "placement refusal bytes",
            bound: why.len(),
        };
    }
    owned.push_str(why);
    Refusal::Invalid {
        what: "placement",
        why: owned,
    }
}
fn bad(why: impl Into<String>) -> Stop {
    Stop::Damaged(why.into())
}
fn name(kind: &str, key: Key) -> String {
    format!("{kind}-{}-{}-{}", key.owner, key.epoch, key.seq)
}

struct State {
    node: peer::Node,
    rows: Rows,
    stopped: Option<Stop>,
    admission: Option<Refusal>,
    /// The machine's capacity view, once opened (joint plan step 13).
    view: Option<view::View>,
    /// This node's facts as last refreshed: the machine's physical room, before every holder's reservations
    /// and the node's own rebooked holdings move the room its ledger admits against (`admission_spec`).
    facts: Option<NodeSpec>,
    /// The room last given the node's ledger from its facts (`readmit`), so an unchanged one isn't given again.
    admitted: Option<Resources>,
    /// What the node holds passes the room left it (a holding risen past it): it admits nothing of its own
    /// until it fits, typed.
    over: Option<Refusal>,
}

enum Effect {
    Create(Key, Demand, Box<Payload>),
    Delete(Key, Demand),
}

type Collected = (Step, Vec<Effect>);

/// One node in an explicitly shared cluster namespace.
pub struct Identity<'a> {
    pub namespace: &'a str,
    pub node: NodeId,
}

pub struct Driver {
    state: Mutex<State>,
    // One operation owns physical effects, with no queued waiters: concurrent mutation is refused.
    // The core/record lock is always released before calling any Runtime method.
    operation: Mutex<()>,
    runtimes: Runtimes,
    peers: Vec<NodeId>,
    namespace: String,
}

impl Driver {
    /// Opens standalone state, generating its durable UUID-width namespace only when genuinely
    /// empty. Peers must use boot_in with one explicitly shared cluster namespace.
    pub fn boot(
        id: NodeId,
        nodes: &[NodeSpec],
        models: &[String],
        budget: Budget,
        pick: Pick,
        dir: &Path,
        runtimes: Runtimes,
    ) -> Result<Self, Error> {
        Self::boot_scoped((id, None), nodes, models, budget, pick, dir, runtimes)
    }

    /// All peers in one cluster supply the same canonical namespace. Independent clusters may
    /// share a physical Runtime without sharing physical BoxIds. Existing state must match.
    pub fn boot_in(
        identity: Identity<'_>,
        nodes: &[NodeSpec],
        models: &[String],
        budget: Budget,
        pick: Pick,
        dir: &Path,
        runtimes: Runtimes,
    ) -> Result<Self, Error> {
        checked_namespace(identity.namespace)?;
        Self::boot_scoped(
            (identity.node, Some(identity.namespace)),
            nodes,
            models,
            budget,
            pick,
            dir,
            runtimes,
        )
    }

    fn boot_scoped(
        identity: (NodeId, Option<&str>),
        nodes: &[NodeSpec],
        models: &[String],
        budget: Budget,
        pick: Pick,
        dir: &Path,
        runtimes: Runtimes,
    ) -> Result<Self, Error> {
        let (id, namespace) = identity;
        let mut rows = Rows::open(dir, budget, namespace)?;
        let namespace = rows.namespace().to_owned();
        let mut actual = BTreeMap::new();
        let mut start = Vec::new();
        let mut retained = Resources::default();
        let mut retained_by_class = BTreeMap::<Class, Resources>::new();
        let live_node = nodes
            .iter()
            .find(|node| node.id == id)
            .ok_or_else(|| bad("recovery node is not configured"))?;
        for (key, payload) in rows.running()? {
            // Core demand is durable beside this payload; decode it through core boot below.
            // Select its runtime using the shared model codec, without a second decision format.
            let (demand, allocation, route, ended) = durable_demand(&rows, key, models)?;
            let charged = hex_sim::model::reserved_resources(demand.need, allocation, route)
                .map_err(|_| bad("retained allocation differs from original workload"))?;
            let runtime = runtimes.get(demand.class)?;
            validate(&demand, &payload)?;
            validate_backend(&payload, &runtime)?;
            let physical = physical_id(&namespace, key)?;
            let boxed = match runtime.inspect(&physical) {
                Ok(info) => {
                    if info.class != payload.spec.class.class() {
                        return Err(bad(
                            "surviving physical box belongs to another runtime backend",
                        )
                        .into());
                    }
                    if info.allocation != allocation || info.route != route {
                        return Err(Refusal::Lifecycle {
                            id: physical.try_clone()?,
                            cause: hex_api::LifecycleCause::Uncertain,
                        }
                        .into());
                    }
                    if !ended && device_request(&demand) {
                        let selected =
                            peer::recorded_device_specs(rows.row(key)?, &live_node.devices)?
                                .ok_or_else(|| {
                                    bad("a surviving device-bearing box has no durable bindings")
                                })?;
                        devices::verify(
                            &runtime,
                            &physical,
                            payload.spec.class.class(),
                            &devices::physical(selected)?,
                        )?;
                    }
                    if !ended {
                        retained = retained
                            .checked_add(&charged)
                            .ok_or_else(|| bad("recovered physical reservations overflow"))?;
                        let class = retained_by_class.entry(demand.class).or_default();
                        *class = class
                            .checked_add(&charged)
                            .ok_or_else(|| bad("recovered runtime reservations overflow"))?;
                    }
                    if !ended && payload.start && info.state == BoxState::Created {
                        start.push((runtime, physical));
                    }
                    Boxed::Running
                }
                Err(Refusal::NoSuchBox(_)) => Boxed::Absent,
                Err(why) => return Err(why.into()),
            };
            actual.insert(key, boxed);
        }
        let live = live_node.capacity;
        let pool_use = |classes: hex_sim::model::Classes| {
            retained_by_class
                .iter()
                .filter(|(class, _)| classes.has(**class))
                .try_fold(Resources::default(), |used, (_, more)| {
                    used.checked_add(more)
                        .ok_or_else(|| bad("recovered runtime pool reservations overflow"))
                })
        };
        let mut raised = !live.holds(&retained);
        for pool in &live_node.runtime_pools {
            raised |= !pool.capacity.holds(&pool_use(pool.classes)?);
        }
        // Live FREE room is an admission offer, not a reason to forget an existing physical box.
        // Only the boot ledger gets a floor equal to checked, actually surviving reservations.
        // Service's original facts stay untouched and new local admission is blocked below.
        let recovery_nodes = if raised {
            let mut recovered = nodes.to_vec();
            let local = recovered
                .iter_mut()
                .find(|node| node.id == id)
                .ok_or_else(|| bad("recovery node missing"))?;
            local.capacity = Resources {
                cpu_milli: live.cpu_milli.max(retained.cpu_milli),
                mem_bytes: live.mem_bytes.max(retained.mem_bytes),
                disk_bytes: live.disk_bytes.max(retained.disk_bytes),
                gpus: live.gpus.max(retained.gpus),
            };
            for pool in &mut local.runtime_pools {
                let held = pool_use(pool.classes)?;
                pool.capacity = Resources {
                    cpu_milli: pool.capacity.cpu_milli.max(held.cpu_milli),
                    mem_bytes: pool.capacity.mem_bytes.max(held.mem_bytes),
                    disk_bytes: pool.capacity.disk_bytes.max(held.disk_bytes),
                    gpus: pool.capacity.gpus.max(held.gpus),
                };
            }
            Some(recovered)
        } else {
            None
        };
        let recovery_budget = rows.recovery_budget();
        let mut node = peer::Node::boot(
            id,
            recovery_nodes.as_deref().unwrap_or(nodes),
            models,
            recovery_budget,
            pick,
            &mut rows,
            |key| actual.get(&key).copied().unwrap_or(Boxed::Absent),
        )?;
        node.set_budget(budget);
        // Only validated, recovered records may resume an init; no acknowledgement has escaped.
        for (runtime, id) in start {
            runtime.start(&id)?;
        }
        Ok(Self {
            state: Mutex::new(State {
                node,
                rows,
                stopped: None,
                view: None,
                facts: None,
                admitted: None,
                over: None,
                admission: raised.then(||Refusal::Invalid {
                    what:"local admission",
                    why:format!("current host or runtime pool room cannot cover recovered physical reservations {retained:?}; a successful live-room refresh is required before new local admission"),
                }),
            }),
            operation: Mutex::new(()),
            runtimes,
            peers: nodes.iter().map(|n| n.id).collect(),
            namespace,
        })
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn physical_id(&self, key: Key) -> Result<BoxId, Refusal> {
        physical_id(&self.namespace, key)
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>, Error> {
        let state = self
            .state
            .lock()
            .map_err(|_| Refusal::Broken("placement mutex poisoned".into()))?;
        if let Some(stop) = &state.stopped {
            return Err(Error::Core(stop.clone()));
        }
        Ok(state)
    }

    fn operation(&self) -> Result<MutexGuard<'_, ()>, Error> {
        self.operation.try_lock().map_err(|e| {
            match e {
                TryLockError::WouldBlock => Refusal::Exhausted {
                    what: "placement operation",
                    bound: 1,
                },
                TryLockError::Poisoned(_) => {
                    Refusal::Broken("placement operation mutex poisoned".into())
                }
            }
            .into()
        })
    }

    pub fn submit(&self, demand: Demand, payload: Payload) -> Result<(Key, Step), Error> {
        let _operation = self.operation()?;
        validate(&demand, &payload)?;
        let mut demand = demand;
        demand.ask.runtime_class = Some(payload.spec.class.class());
        demand.ask.native_only |= matches!(payload.spec.class, hex_api::ClassSpec::Bundle(_));
        let mut state = self.lock()?;
        state.rows.context = Context::NextOwned(payload);
        let State { node, rows, .. } = &mut *state;
        let result = node.submit(demand, rows);
        state.rows.context = Context::None;
        let (key, out) = finish(&mut state, result, true)?;
        let (step, create) = collect(&mut state, out)?;
        drop(state);
        Ok((key, self.drive(step, create)?))
    }

    pub fn hear(&self, from: NodeId, envelope: Envelope) -> Result<Step, Error> {
        let _operation = self.operation()?;
        let mut state = self.lock()?;
        if let Msg::Start { key, demand, .. } = envelope.msg {
            if key.owner != from.0 || !self.peers.contains(&from) {
                return Err(invalid("start owner is not its known sender").into());
            }
            if state.node.demand(key).is_some_and(|d| d != demand) {
                return Err(invalid("start changed its durable demand").into());
            }
            let payload = envelope
                .payload
                .clone()
                .ok_or_else(|| invalid("start has no payload"))?;
            if state.node.owned_keys().any(|(k, _)| k == key)
                && state.rows.payload("own", key)? != payload
            {
                return Err(invalid("start changed its owner's durable payload").into());
            }
            let prior = state.node.ran_keys().any(|(k, _, _)| k == key);
            if prior && state.rows.payload("ran", key)? != payload {
                return Err(invalid("repeated start changed its payload").into());
            }
            // Runtime::classes is a backend call too: validate after dropping the core lock.
            drop(state);
            let checked = validate(&demand, &payload).and_then(|()| {
                if let Msg::Start {
                    route: Some(route), ..
                } = envelope.msg
                    && (route.key.class != payload.spec.class.class()
                        || matches!(payload.spec.class, hex_api::ClassSpec::Bundle(_))
                            && route.key.execution != hex_api::Execution::Native)
                {
                    return Err(invalid_static(
                        "committed route differs from the actual original payload",
                    )
                    .into());
                }
                let runtime = self.runtimes.get(demand.class)?;
                validate_backend(&payload, &runtime)
            });
            state = self.lock()?;
            // The room this admission commits in, given the ledger and checked under the lock its commit holds (CV3):
            // a holder's word or a rebook may have moved it while the lock was let go, and a box whose holding was
            // less than its receipt may have ended since, its credit gone with it (joint plan step 13).
            if !prior {
                readmit(&mut state)?;
                if let Some(why) = state.admission.as_ref().or(state.over.as_ref()) {
                    return Ok(Step {
                        send: vec![(
                            from,
                            Envelope {
                                msg: Msg::Refused { key },
                                payload: None,
                                refusal: Some(why.clone()),
                            },
                        )],
                        notices: vec![Notice::Refused {
                            key,
                            node: state.node.id(),
                            refusal: why.clone(),
                        }],
                    });
                }
            }
            match checked {
                Ok(()) => {}
                Err(Error::Runtime(why)) if !prior => {
                    return Ok(Step {
                        send: vec![(
                            from,
                            Envelope {
                                msg: Msg::Refused { key },
                                payload: None,
                                refusal: Some(why),
                            },
                        )],
                        notices: Vec::new(),
                    });
                }
                Err(why) => return Err(why),
            }
            state.rows.context = Context::Start(key, payload);
        } else if envelope.payload.is_some() {
            return Err(invalid("payload outside start").into());
        }
        let State { node, rows, .. } = &mut *state;
        let result = node.hear(from, envelope.msg, rows);
        state.rows.context = Context::None;
        let out = finish(&mut state, result, false)?;
        let (mut step, create) = collect(&mut state, out)?;
        if let (Msg::Refused { key }, Some(refusal)) = (envelope.msg, envelope.refusal) {
            step.notices.push(Notice::Refused {
                key,
                node: from,
                refusal,
            });
        }
        drop(state);
        self.drive(step, create)
    }

    pub fn link_up(&self, peer: NodeId) -> Result<Step, Error> {
        self.change(|node, rows| node.link_up(peer, rows))
    }

    /// Recovery uses the same link-up protocol, including the laptop's self-link. Messages stay
    /// external so a local workload never recurses through the runtime driver.
    pub fn take_recovered(&self) -> Result<Step, Error> {
        self.change(|node, rows| {
            let mut out = Vec::new();
            for peer in &self.peers {
                out.extend(node.link_up(*peer, rows)?);
            }
            Ok(out)
        })
    }

    /// A node's facts refreshed; for this node's, the capacity view's room with them (joint plan step 13), under the
    /// lock every admission commits under.
    pub fn refresh_node(&self, spec: NodeSpec) -> Result<(), Error> {
        let _operation = self.operation()?;
        let mut state = self.lock()?;
        let local = spec.id == state.node.id();
        let spec = if local {
            let kept = spec.try_clone().map_err(|_| Refusal::Exhausted {
                what: "local node facts",
                bound: 1,
            })?;
            if let Some(view) = state.view.as_mut() {
                view.set_room(view::amounts(&spec.capacity));
            }
            state.facts = Some(kept);
            let admits = admission_spec(&state);
            finish(&mut state, admits, false)?.unwrap_or(spec)
        } else {
            spec
        };
        let capacity = spec.capacity;
        let result = state.node.refresh_node(spec);
        if local {
            state.admission = result.as_ref().err().map(admission_refusal);
            if result.is_ok() {
                state.admitted = Some(capacity);
                state.over = None;
            }
        }
        result.map_err(Error::Core)
    }

    /// Opens the machine's capacity view in `dir` (joint plan step 13, `docs/design/capacity-view.md`),
    /// serving `holders` holders, each holding as many as one sync lists (CV8). The node admits nothing of
    /// its own until its next facts refresh gives the view its room: the reservations the view recovered
    /// come off the node's room first.
    pub fn open_view(&self, dir: &Path, holders: usize) -> Result<(), Error> {
        let _operation = self.operation()?;
        let mut state = self.lock()?;
        if state.view.is_some() {
            return Err(Refusal::Invalid {
                what: "capacity view",
                why: "it is open already".into(),
            }
            .into());
        }
        let opened = view::View::open(dir, holders);
        let mut opened = finish(&mut state, opened, false)?;
        match state.facts.as_ref() {
            Some(facts) => {
                opened.set_room(view::amounts(&facts.capacity));
                state.view = Some(opened);
                readmit(&mut state)?;
            }
            None => {
                state.view = Some(opened);
                state.admission = Some(Refusal::Invalid {
                    what: "local admission",
                    why: "the capacity view opened; a live-room refresh is required before new local admission"
                        .into(),
                });
            }
        }
        Ok(())
    }

    /// Each box this node runs, by its physical id (joint plan step 14, C8): a capacity answer names a VM's holding
    /// by its box, which [`Driver::rebook`] books against its key. Like rebook, it reads only state under the core
    /// lock, so it takes no operation's and never waits on a placement.
    pub fn running_ids(&self) -> Result<BTreeMap<BoxId, Key>, Error> {
        let state = self.lock()?;
        let mut ids = BTreeMap::new();
        for (key, _, ended) in state.node.ran_keys() {
            if !ended {
                ids.insert(physical_id(&self.namespace, key)?, key);
            }
        }
        Ok(ids)
    }

    // The view's calls and rebook change only state under the core lock, never a runtime's: they don't take the
    // operation's, which refuses a call while another holds it, so a holder's word never waits on a placement.

    /// What each microVM this node runs holds now, as the learned charge has it (joint plan step 14, C8):
    /// its learned booking, and the memory and disk it holds past it. The machine's capacity view counts
    /// the node's own held so (step 13, CV2), and the node's ledger, which books each box at its receipt,
    /// is given the room those holdings leave, its room the machine's physical room still. Replaces what
    /// was said before; returns how many boxes it holds so.
    pub fn rebook(&self, holds: &[(Key, hex_api::CapacityResources)]) -> Result<usize, Error> {
        let mut state = self.lock()?;
        let kept = state.node.rebook(holds.iter().map(|(key, holding)| {
            (
                *key,
                Resources {
                    cpu_milli: holding.cpu_milli,
                    mem_bytes: holding.mem_bytes,
                    disk_bytes: holding.disk_bytes,
                    gpus: 0,
                },
            )
        }));
        readmit(&mut state)?;
        Ok(kept)
    }

    /// A holder's word on `link`, decided beside what this node holds itself. A change to the holders'
    /// reservations gives the node its new room before the lock is let go, so no admission of either side
    /// sees the room the other's took.
    pub fn view_hear(
        &self,
        link: u64,
        msg: hex_api::capacity::ToKeeper,
    ) -> Result<hex_api::capacity::ToHolder, Error> {
        let mut state = self.lock()?;
        let local = state
            .node
            .held()
            .map(hex_api::capacity::Amounts::from_array);
        let local = finish(&mut state, local, false)?;
        let heard = view_mut(&mut state)?.hear(link, msg, local);
        let (answer, changed) = finish(&mut state, heard, false)?;
        if changed {
            readmit(&mut state)?;
        }
        Ok(answer)
    }

    /// Link `link` dropped: its holder keeps what it holds (CV5), so the room it leaves the node is the same.
    pub fn view_link_down(&self, link: u64) -> Result<(), Error> {
        let mut state = self.lock()?;
        view_mut(&mut state)?.link_down(link);
        Ok(())
    }

    /// The hints owed to the holders, by link, with the view as it is now (CV7).
    pub fn view_hints(&self) -> Result<Vec<(u64, hex_api::capacity::ToHolder)>, Error> {
        let mut state = self.lock()?;
        let local = state
            .node
            .held()
            .map(hex_api::capacity::Amounts::from_array);
        let local = finish(&mut state, local, false)?;
        let hints = view_mut(&mut state)?.hints(local);
        finish(&mut state, hints, false)
    }

    /// The person's release of holder `name`'s reservations, refused while it is connected (CV6). What
    /// it released is returned for the caller to log.
    pub fn view_forget(
        &self,
        name: &str,
    ) -> Result<Result<hex_api::capacity::Forgotten, hex_api::capacity::NotForgotten>, Error> {
        let mut state = self.lock()?;
        let forgotten = view_mut(&mut state)?.forget(name);
        let (forgotten, changed) = finish(&mut state, forgotten, false)?;
        if changed {
            readmit(&mut state)?;
        }
        Ok(forgotten)
    }

    /// The machine's view as the keeper would answer it now.
    pub fn view(&self) -> Result<hex_api::capacity::View, Error> {
        let mut state = self.lock()?;
        let local = state
            .node
            .held()
            .map(hex_api::capacity::Amounts::from_array);
        let local = finish(&mut state, local, false)?;
        Ok(view_mut(&mut state)?.view(local))
    }

    /// The existing operation receipt spans the pure grant, unlocked native call and exact
    /// result. No runtime RPC or Store effect occurs under the core/model mutex. False means
    /// current room cannot initialize this backing; unrelated eligible CPU routes remain usable.
    pub fn initialize_backing(
        &self,
        endpoint: Class,
        key: hex_api::BackingKey,
    ) -> Result<bool, Error> {
        let _operation = self.operation()?;
        let runtime = self.runtimes.get(endpoint)?;
        let grant = match self.lock()?.node.grant_backing(key) {
            Ok(grant) => grant,
            Err(hex_sim::SimError::Resources(_)) => return Ok(false),
            Err(_) => {
                return Err(Refusal::Backing {
                    key,
                    cause: hex_api::LifecycleCause::Pending,
                }
                .into());
            }
        };
        let expected = grant.try_clone()?;
        let observed = match runtime.initialize_backing(grant) {
            Ok(observed) => observed,
            Err(refusal) => {
                self.lock()?.admission = Some(Refusal::Backing {
                    key,
                    cause: hex_api::LifecycleCause::Uncertain,
                });
                return Err(refusal.into());
            }
        };
        let accepted = observed.key == expected.key
            && observed.devices == expected.devices
            && observed.allocation().ok() == Some(expected.allocation)
            && observed.phase == hex_api::BackingPhase::Live;
        let mut state = self.lock()?;
        if !accepted || state.node.observe_backing(&observed).is_err() {
            state.admission = Some(Refusal::Backing {
                key,
                cause: hex_api::LifecycleCause::Uncertain,
            });
            return Err(Refusal::Backing {
                key,
                cause: hex_api::LifecycleCause::Uncertain,
            }
            .into());
        }
        Ok(true)
    }

    /// Drive native cleanup under the SAME operation, with no core lock through the callback.
    /// Draining retains its complete grant until an exact positive whole-scope result advances
    /// BOTH existing Index views. A later explicit call acknowledges that terminal observation.
    pub fn drain_backing(
        &self,
        endpoint: Class,
        expected: &hex_api::BackingOffer,
    ) -> Result<hex_api::BackingOffer, Error> {
        expected.allocation()?;
        let _operation = self.operation()?;
        let key = expected.key;
        let refused = || Refusal::Backing {
            key,
            cause: hex_api::LifecycleCause::Uncertain,
        };
        if expected.phase != hex_api::BackingPhase::Draining {
            return Err(invalid_static("native cleanup requires canonical Draining intent").into());
        }
        self.lock()?
            .node
            .observe_backing(expected)
            .map_err(|_| refused())?;
        let observed = self.runtimes.get(endpoint)?.release_backing(key)?;
        if observed.key != key
            || observed.cost != expected.cost
            || observed.devices != expected.devices
            || observed.phase != hex_api::BackingPhase::Released
        {
            self.lock()?.admission = Some(refused());
            return Err(invalid_static("native cleanup changed its exact backing receipt").into());
        }
        let mut state = self.lock()?;
        if state.node.observe_backing(&observed).is_err() {
            state.admission = Some(refused());
            return Err(refused().into());
        }
        Ok(observed)
    }

    /// Acknowledges an already consumed native terminal receipt, before that owner may remove
    /// the old generation. The callback remains outside the core mutex, under the same operation.
    pub fn release_backing(
        &self,
        endpoint: Class,
        expected: &hex_api::BackingOffer,
    ) -> Result<(), Error> {
        expected.allocation()?;
        let _operation = self.operation()?;
        let key = expected.key;
        let refused = || Refusal::Backing {
            key,
            cause: hex_api::LifecycleCause::Uncertain,
        };
        if expected.phase != hex_api::BackingPhase::Released
            || !self
                .lock()?
                .node
                .backing_released(key)
                .map_err(|_| refused())?
        {
            return Err(refused().into());
        }
        let observed = self.runtimes.get(endpoint)?.release_backing(key)?;
        if observed.key != key
            || observed.cost != expected.cost
            || observed.devices != expected.devices
            || observed.phase != expected.phase
        {
            self.lock()?.admission = Some(refused());
            return Err(refused().into());
        }
        Ok(())
    }

    /// Admission changes never erase retained rows. A larger persisted read ceiling
    /// precedes any new core row; a smaller current ceiling only refuses new work.
    pub fn set_budget(&self, budget: Budget) -> Result<usize, Error> {
        let _operation = self.operation()?;
        let mut state = self.lock()?;
        let bound = state.rows.control_bound(budget)?;
        state.rows.grow_budget(budget)?;
        state.node.set_budget(budget);
        Ok(bound)
    }

    pub fn control_bound(&self) -> Result<usize, Error> {
        let state = self.lock()?;
        state
            .rows
            .control_bound(state.node.budget())
            .map_err(Error::Core)
    }

    /// A probe that cannot establish current room blocks new reservations, while already held
    /// keys and deletion/control remain available. The refusal remains typed across peer replies.
    pub fn block_admission(&self, why: Refusal) -> Result<(), Error> {
        let _operation = self.operation()?;
        self.lock()?.admission = Some(why);
        Ok(())
    }

    pub fn set_pick(&self, pick: Pick) -> Result<(), Error> {
        let _operation = self.operation()?;
        self.lock()?.node.set_pick(pick);
        Ok(())
    }

    pub fn set_gpu_load(
        &self,
        node: NodeId,
        device: usize,
        load: hex_sim::model::GpuLoad,
    ) -> Result<(), Error> {
        let _operation = self.operation()?;
        let mut state = self.lock()?;
        let result = state.node.set_gpu_load(node, device, load);
        if node == state.node.id()
            && let Err(why) = &result
        {
            state.admission = Some(admission_refusal(why));
        }
        result.map_err(Error::Core)
    }

    /// Bounded node/class aggregates, never individual launch samples. The authenticated transport
    /// publishes only the sender's cells and keeps the snapshot unchanged across packet retries.
    pub fn start_observations(&self) -> Result<Vec<hex_sim::start_cost::StartObservation>, Error> {
        Ok(self.lock()?.node.start_observations())
    }

    pub fn merge_peer_start_observations(
        &self,
        peer: NodeId,
        observations: &[hex_sim::start_cost::StartObservation],
    ) -> Result<(), Error> {
        let _operation = self.operation()?;
        let mut state = self.lock()?;
        let State { node, rows, .. } = &mut *state;
        let result = node.merge_peer_start_observations(peer, observations, rows);
        finish(&mut state, result, false)
    }

    pub fn retry(&self) -> Result<Step, Error> {
        self.change(|node, rows| {
            let mut out = Vec::new();
            node.retry(rows, &mut out)?;
            Ok(out)
        })
    }

    fn change(
        &self,
        change: impl FnOnce(&mut peer::Node, &mut Rows) -> Result<Vec<(NodeId, Msg)>, Stop>,
    ) -> Result<Step, Error> {
        let _operation = self.operation()?;
        let mut state = self.lock()?;
        let State { node, rows, .. } = &mut *state;
        let result = change(node, rows);
        let out = finish(&mut state, result, false)?;
        let (step, create) = collect(&mut state, out)?;
        drop(state);
        self.drive(step, create)
    }

    fn drive(&self, mut step: Step, effects: Vec<Effect>) -> Result<Step, Error> {
        for effect in effects {
            let (key, demand, payload) = match effect {
                Effect::Create(key, demand, payload) => (key, demand, payload),
                Effect::Delete(key, demand) => {
                    // The core verified full-key ownership and persisted Deleting at the owner.
                    // A missing ran row (e.g. dropped Start) is not itself an absence proof.
                    match self
                        .runtimes
                        .get(demand.class)?
                        .delete(&self.physical_id(key)?)
                    {
                        Ok(()) | Err(Refusal::NoSuchBox(_)) => {
                            let next = self.ended(key)?;
                            step.send.extend(next.send);
                            step.notices.extend(next.notices);
                        }
                        Err(why) => step.notices.push(Notice::Unknown {
                            key,
                            refusal: why,
                            cleanup: None,
                        }),
                    }
                    continue;
                }
            };
            let runtime = self.runtimes.get(demand.class)?;
            let id = self.physical_id(key)?;
            let mut spec = payload.spec.clone();
            spec.id = id.clone();
            let allocation = self.lock()?.node.assigned_allocation(key)?;
            spec.allocation = allocation;
            let route = self.lock()?.node.assigned_route(key)?;
            spec.route = route;
            let selected = self
                .lock()?
                .node
                .assigned_device_specs(key)?
                .ok_or_else(|| bad("physical create has no active receiver reservation"))?;
            spec.devices = devices::physical(selected)?;
            let mut assigned = Vec::new();
            assigned
                .try_reserve_exact(spec.devices.len())
                .map_err(|_| Refusal::Exhausted {
                    what: "physical device assignments",
                    bound: spec.devices.len(),
                })?;
            assigned.extend_from_slice(&spec.devices);
            let mut launched = None;
            let made = match runtime.inspect(&id) {
                Ok(info) if info.allocation != allocation || info.route != route => {
                    step.notices.push(Notice::Unknown {
                        key,
                        refusal: Refusal::Lifecycle {
                            id: id.try_clone()?,
                            cause: hex_api::LifecycleCause::Uncertain,
                        },
                        cleanup: None,
                    });
                    continue;
                }
                Ok(info) => Ok(info),
                Err(Refusal::NoSuchBox(_)) => {
                    // Combined launch service includes any staging performed inside create.
                    // Explicit modeled transfer/work costs omit this aggregate in the scorer.
                    // Recovery/preexisting boxes do not create a second or partial sample.
                    let checked = (|| -> Result<(), Refusal> {
                        if route.is_some() {
                            let offered =
                                runtime.capacity(spec.class.class())?.ok_or_else(|| {
                                    invalid_static("routed create has no initialized native offer")
                                })?;
                            offered.validate_route(&spec)?;
                        }
                        Ok(())
                    })();
                    checked.and_then(|()| {
                        launched = Some(Instant::now());
                        runtime.create(spec)
                    })
                }
                Err(why) => {
                    step.notices.push(Notice::Unknown {
                        key,
                        refusal: why,
                        cleanup: None,
                    });
                    continue;
                }
            };
            let made = made.and_then(|info| {
                if info.allocation != allocation || info.route != route {
                    return Err(Refusal::Lifecycle {
                        id: id.try_clone()?,
                        cause: hex_api::LifecycleCause::Uncertain,
                    });
                }
                if info.class != payload.spec.class.class() {
                    return Err(Refusal::Broken(
                        "physical box belongs to another runtime backend".into(),
                    ));
                }
                devices::verify(&runtime, &id, payload.spec.class.class(), &assigned).map_err(
                    |error| match error {
                        Error::Runtime(why) => why,
                        Error::Core(_) => Refusal::Broken(
                            "native device bindings disagree with their durable reservation".into(),
                        ),
                    },
                )?;
                if payload.start && info.state == BoxState::Created {
                    runtime.start(&id)?;
                }
                Ok(())
            });
            match made {
                Ok(()) => {
                    let elapsed = launched
                        .map(|at| u64::try_from(at.elapsed().as_nanos()))
                        .transpose();
                    match elapsed {
                        Ok(elapsed) => self.completed(key, None, elapsed, &mut step)?,
                        Err(why) => step.notices.push(Notice::Unknown {
                            key,
                            refusal: invalid(format!("combined launch duration: {why}")),
                            cleanup: None,
                        }),
                    }
                }
                Err(
                    why @ (Refusal::Exists(_)
                    | Refusal::Lifecycle { .. }
                    | Refusal::Uncertain { .. }),
                ) => step.notices.push(Notice::Unknown {
                    key,
                    refusal: why,
                    cleanup: None,
                }),
                Err(why) => match runtime.delete(&id) {
                    Ok(()) | Err(Refusal::NoSuchBox(_)) => {
                        self.completed(key, Some(why), None, &mut step)?
                    }
                    Err(cleanup) => step.notices.push(Notice::Unknown {
                        key,
                        refusal: why,
                        cleanup: Some(cleanup),
                    }),
                },
            }
        }
        Ok(step)
    }

    fn completed(
        &self,
        key: Key,
        refusal: Option<Refusal>,
        elapsed: Option<u64>,
        step: &mut Step,
    ) -> Result<(), Error> {
        let mut state = self.lock()?;
        let State { node, rows, .. } = &mut *state;
        let result = match (refusal.is_some(), elapsed) {
            (true, _) => node.create_refused(key, rows),
            (false, Some(ns)) => node.created_with_start(key, ns, rows),
            (false, None) => node.created(key, rows),
        };
        let out = finish(&mut state, result, false)?;
        let (mut next, create) = collect(&mut state, out)?;
        if !create.is_empty() {
            state.stopped = Some(bad("create recursively requested creation"));
            return Err(bad("unexpected nested creation").into());
        }
        if let Some(why) = refusal {
            for (_, message) in &mut next.send {
                if matches!(message.msg,Msg::Refused{key:k} if k==key) {
                    message.refusal = Some(why.clone());
                }
            }
            next.notices.push(Notice::Refused {
                key,
                node: state.node.id(),
                refusal: why,
            });
        }
        step.send.extend(next.send);
        step.notices.extend(next.notices);
        Ok(())
    }

    pub fn owned(&self) -> Result<Vec<(Key, Option<NodeId>)>, Error> {
        Ok(self.lock()?.node.owned_keys().collect())
    }

    pub fn status(&self, key: Key) -> Result<Option<Status>, Error> {
        let state = self.lock()?;
        let own = state.node.owned_keys().find(|(k, _)| *k == key);
        let ran = state.node.ran_keys().find(|(k, _, _)| *k == key);
        if own.is_none() && ran.is_none() {
            return Ok(None);
        }
        Ok(Some(Status {
            key,
            node: ran
                .map(|_| state.node.id())
                .or_else(|| own.and_then(|(_, n)| n)),
            local: ran.is_some(),
            deleting: state.node.is_deleting(key),
            created: ran.is_some_and(|(_, c, _)| c) || state.node.is_created(key),
            ended: ran.is_some_and(|(_, _, e)| e) || state.node.is_ended(key),
            physical: self.physical_id(key)?,
        }))
    }

    pub fn delete(&self, key: Key) -> Result<Step, Error> {
        let _operation = self.operation()?;
        let mut state = self.lock()?;
        if state.node.owned_keys().any(|(k, _)| k == key) {
            let State { node, rows, .. } = &mut *state;
            let result = node.delete(key, rows);
            let out = finish(&mut state, result, false)?;
            let (step, effects) = collect(&mut state, out)?;
            drop(state);
            return self.drive(step, effects);
        }
        if !state.node.ran_keys().any(|(k, _, _)| k == key) {
            return Err(Refusal::NoSuchBox(self.physical_id(key)?).into());
        }
        let demand = state
            .node
            .demand(key)
            .ok_or_else(|| bad("ran demand missing"))?;
        drop(state);
        match self
            .runtimes
            .get(demand.class)?
            .delete(&self.physical_id(key)?)
        {
            Ok(()) | Err(Refusal::NoSuchBox(_)) => self.ended(key),
            Err(why) => Err(why.into()),
        }
    }

    fn ended(&self, key: Key) -> Result<Step, Error> {
        let mut state = self.lock()?;
        let State { node, rows, .. } = &mut *state;
        let result = node.end(key, rows);
        let out = finish(&mut state, result, false)?;
        let (step, create) = collect(&mut state, out)?;
        if !create.is_empty() {
            return Err(bad("end requested creation").into());
        }
        Ok(step)
    }

    pub fn poll_status(&self) -> Result<Step, Error> {
        let _operation = self.operation()?;
        let state = self.lock()?;
        let keys: Vec<_> = state
            .node
            .ran_keys()
            .filter(|(_, _, ended)| !ended)
            .map(|(key, _, _)| {
                Ok((
                    key,
                    state
                        .node
                        .demand(key)
                        .ok_or_else(|| bad("ran demand missing"))?,
                ))
            })
            .collect::<Result<_, Stop>>()?;
        drop(state);
        let mut step = Step::default();
        for (key, demand) in keys {
            match self
                .runtimes
                .get(demand.class)?
                .inspect(&self.physical_id(key)?)
            {
                Ok(info) => {
                    let info = triomphe::Arc::try_new(info).map_err(|_| Refusal::Exhausted {
                        what: "placement status ownership",
                        bound: 1,
                    })?;
                    step.notices
                        .try_reserve(1)
                        .map_err(|_| Refusal::Exhausted {
                            what: "placement status notice entries",
                            bound: 1,
                        })?;
                    step.notices.push(Notice::Status { key, info });
                }
                Err(Refusal::NoSuchBox(_)) => {
                    let next = self.ended(key)?;
                    step.send.extend(next.send);
                    step.notices.extend(next.notices);
                }
                Err(why) => step.notices.push(Notice::Unknown {
                    key,
                    refusal: why,
                    cleanup: None,
                }),
            }
        }
        Ok(step)
    }

    /// Actual memory held by this driver's physical boxes, by runtime class. CPU/disk are not
    /// invented from caps. The caller can credit a shared pool once for all its classes.
    pub fn measured_owned_use_by_class(&self) -> Result<BTreeMap<Class, Resources>, Error> {
        let _operation = self.operation()?;
        let state = self.lock()?;
        let mut keys = Vec::new();
        let count = state.node.ran_keys().count();
        keys.try_reserve_exact(count)
            .map_err(|_| Refusal::Exhausted {
                what: "measured native box identities",
                bound: count,
            })?;
        for (key, _, ended) in state.node.ran_keys() {
            if ended
                || state
                    .node
                    .assigned_route(key)?
                    .is_some_and(|route| route.key.execution == hex_api::Execution::ShardsShared)
            {
                continue;
            }
            keys.push((
                key,
                state
                    .node
                    .demand(key)
                    .ok_or_else(|| bad("ran demand missing"))?,
            ));
        }
        drop(state);
        let mut resources = BTreeMap::<Class, Resources>::new();
        for (key, demand) in keys {
            match self
                .runtimes
                .get(demand.class)?
                .usage(&self.physical_id(key)?)
            {
                Ok(usage) => {
                    let owned = resources.entry(demand.class).or_default();
                    owned.mem_bytes = owned
                        .mem_bytes
                        .checked_add(usage.memory_bytes)
                        .ok_or_else(|| invalid("physical memory usage overflow"))?
                }
                Err(Refusal::NoSuchBox(_)) => {}
                Err(why) => return Err(why.into()),
            }
        }
        Ok(resources)
    }
}

fn validate(demand: &Demand, payload: &Payload) -> Result<(), Error> {
    payload.spec.check()?;
    if payload.spec.route.is_some() {
        return Err(invalid_static("the scheduler derives the initialized execution route").into());
    }
    if demand
        .ask
        .runtime_class
        .is_some_and(|class| class != payload.spec.class.class())
    {
        return Err(invalid_static("original runtime form differs from the actual payload").into());
    }
    if payload.spec.allocation.is_some() {
        return Err(
            invalid("the scheduler derives the allocation from the original demand").into(),
        );
    }
    let cpu = payload
        .spec
        .bounds
        .cpu_max_milli
        .ok_or_else(|| invalid("CPU cap required for a reserved box"))?;
    let mem = payload
        .spec
        .bounds
        .memory_max
        .ok_or_else(|| invalid("memory cap required for a reserved box"))?;
    if u64::from(cpu) > demand.need.cpu_milli || mem > demand.need.mem_bytes {
        return Err(invalid("box limits exceed placement reservation").into());
    }
    if !payload.spec.devices.is_empty() {
        return Err(invalid("the receiver selects physical devices from the demand").into());
    }
    if device_request(demand)
        && !matches!(
            &payload.spec.class,
            hex_api::ClassSpec::Oci(_)
                | hex_api::ClassSpec::Firecracker(_)
                | hex_api::ClassSpec::Shards(_)
        )
    {
        return Err(invalid("device placement requires a structured backend payload").into());
    }
    let class = payload.spec.class.class();
    if payload.start && class == hex_api::Class::Process {
        return Err(invalid("process boxes have no init to start").into());
    }
    if !matches!(
        (demand.class, class),
        (Class::Oci, hex_api::Class::Process | hex_api::Class::Oci)
            | (Class::Firecracker, hex_api::Class::Firecracker)
            | (Class::Shards, hex_api::Class::Shards)
    ) {
        return Err(invalid("placement backend and payload class disagree").into());
    }
    let value = rows::payload_value(payload)?;
    let text = serde_json::to_string(&value).map_err(|e| bad(e.to_string()))?;
    if text.len() > hex_core::wire::MAX_MESSAGE {
        return Err(invalid("placement payload exceeds wire bound").into());
    }
    Ok(())
}

fn device_request(demand: &Demand) -> bool {
    demand.need.gpus > 0
        || demand.ask.devices()
        || demand.ask.gpu_model.is_some()
        || demand.ask.gpu_memory > 0
        || demand.ask.block_size > 0
}

fn validate_backend(payload: &Payload, runtime: &Arc<dyn Runtime>) -> Result<(), Error> {
    if !runtime.classes().contains(&payload.spec.class.class()) {
        return Err(invalid("registered runtime does not support box spec class").into());
    }
    Ok(())
}

fn finish<T>(state: &mut State, result: Result<T, Stop>, submit: bool) -> Result<T, Error> {
    result.map_err(|stop| {
        if !(submit && matches!(stop, Stop::Full { .. })) {
            state.stopped = Some(stop.clone());
        }
        Error::Core(stop)
    })
}

/// The node's capacity view as its lane serves it (`hex_quic::capacity::Server`; joint plan step 13, CV9): this
/// Driver's keeper, each call under its core lock.
pub struct DriverView(pub Arc<Driver>);

impl hex_api::capacity::CapacityView for DriverView {
    fn hear(
        &mut self,
        link: u64,
        msg: hex_api::capacity::ToKeeper,
    ) -> Result<hex_api::capacity::ToHolder, Refusal> {
        self.0.view_hear(link, msg).map_err(placement_refusal)
    }

    fn link_down(&mut self, link: u64) -> Result<(), Refusal> {
        self.0.view_link_down(link).map_err(placement_refusal)
    }

    fn hints(&mut self) -> Result<Vec<(u64, hex_api::capacity::ToHolder)>, Refusal> {
        self.0.view_hints().map_err(placement_refusal)
    }
}

fn placement_refusal(error: Error) -> Refusal {
    match error {
        Error::Runtime(refusal) => refusal,
        Error::Core(stop) => admission_refusal(&stop),
    }
}

fn view_mut(state: &mut State) -> Result<&mut view::View, Error> {
    state.view.as_mut().ok_or_else(|| {
        Refusal::Invalid {
            what: "capacity view",
            why: "it isn't open".into(),
        }
        .into()
    })
}

/// The node's own admission given the room the holders' reservations leave it, at once (CV3). The keeper
/// granted only what fits beside what the node holds, so its admission index never refuses the shrink.
/// Gives the node's ledger the room its facts, the holders' reservations and its own holdings leave it
/// now. Where what the node holds passes that room, its ledger can't be given less than it books: the node
/// admits nothing of its own until it fits (`over`), typed, and goes on otherwise.
fn readmit(state: &mut State) -> Result<(), Error> {
    let spec = admission_spec(state);
    let Some(spec) = finish(state, spec, false)? else {
        return Ok(());
    };
    if state.admitted == Some(spec.capacity) && state.over.is_none() {
        return Ok(());
    }
    let capacity = spec.capacity;
    match state.node.refresh_node(spec) {
        Ok(()) => {
            state.admitted = Some(capacity);
            state.over = None;
        }
        Err(why) => {
            state.over = Some(Refusal::Invalid {
                what: "local admission",
                why: format!(
                    "what this node holds passes the room left it ({capacity:?}): {}",
                    admission_refusal(&why)
                ),
            });
        }
    }
    Ok(())
}

/// The spec this node's ledger admits against (joint plan steps 13 and 14): its facts, the machine's
/// physical room less every holder's reservations (CV3), plus what its boxes' learned holdings leave of the
/// receipts its ledger books, less what they take past them, so its free room is the physical room less what
/// everything holds. None before its first facts.
fn admission_spec(state: &State) -> Result<Option<NodeSpec>, Stop> {
    let Some(facts) = state.facts.as_ref() else {
        return Ok(None);
    };
    let foreign = state
        .view
        .as_ref()
        .map_or(hex_api::capacity::Amounts::ZERO, |view| view.foreign());
    let booked = hex_api::capacity::Amounts::from_array(state.node.booked()?);
    let held = hex_api::capacity::Amounts::from_array(state.node.held()?);
    let (credit, debit) = (booked.less(held), held.less(booked));
    let room = |physical: u64, credit: u64, foreign: u64, debit: u64| {
        physical
            .saturating_add(credit)
            .saturating_sub(foreign)
            .saturating_sub(debit)
    };
    let mut spec = facts
        .try_clone()
        .map_err(|e| Stop::Damaged(format!("the node's facts: {e}")))?;
    spec.capacity = Resources {
        cpu_milli: room(
            facts.capacity.cpu_milli,
            credit.cpu_milli,
            foreign.cpu_milli,
            debit.cpu_milli,
        ),
        mem_bytes: room(
            facts.capacity.mem_bytes,
            credit.mem_bytes,
            foreign.mem_bytes,
            debit.mem_bytes,
        ),
        disk_bytes: room(
            facts.capacity.disk_bytes,
            credit.disk_bytes,
            foreign.disk_bytes,
            debit.disk_bytes,
        ),
        // A holder holds no GPU (CV3) and a holding none past its receipt: the devices still agree.
        gpus: facts.capacity.gpus,
    };
    Ok(Some(spec))
}

fn admission_refusal(why: &Stop) -> Refusal {
    match why {
        Stop::Full { what, bound } => Refusal::Exhausted {
            what,
            bound: *bound,
        },
        _ => Refusal::Broken(format!("current placement facts: {why:?}")),
    }
}

fn collect(state: &mut State, out: Vec<(NodeId, Msg)>) -> Result<Collected, Error> {
    let mut step = Step::default();
    let mut create = Vec::new();
    for (node, msg) in out {
        let payload = match msg {
            Msg::Start { key, .. } => Some(state.rows.payload("own", key)?),
            _ => None,
        };
        step.send.push((
            node,
            Envelope {
                msg,
                payload,
                refusal: None,
            },
        ));
    }
    for told in std::mem::take(&mut state.node.told) {
        match told {
            Told::Started(key, node) => step.notices.push(Notice::Started { key, node }),
            Told::Ended(key) => step.notices.push(Notice::Ended { key }),
            Told::Create(key, demand) => create.push(Effect::Create(
                key,
                demand,
                Box::new(state.rows.payload("ran", key)?),
            )),
            Told::Delete(key, demand) => create.push(Effect::Delete(key, demand)),
        }
    }
    Ok((step, create))
}

fn durable_demand(
    rows: &Rows,
    key: Key,
    models: &[String],
) -> Result<
    (
        Demand,
        Option<hex_api::Allocation>,
        Option<hex_api::RouteReceipt>,
        bool,
    ),
    Stop,
> {
    let row = rows.row(key)?;
    // The existing core durable grammar prefixes a ran demand with owner and lifecycle state.
    let (_, rest) = row.split_once(' ').ok_or_else(|| bad("ran owner"))?;
    let (state, _) = rest.split_once(' ').ok_or_else(|| bad("ran state"))?;
    let (demand, allocation, route) = peer::recorded_assignment(row, models)?;
    Ok((demand, allocation, route, state == "ended"))
}

#[cfg(test)]
mod tests;
