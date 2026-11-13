//! AC-T7.4.1: initialized backend routing on the existing Node/Store/Turns owner.

use super::*;
use crate::store::Torn;
use hex_api::{BackingKey, Capacity, DeviceId, Execution, LifecycleCause, RuntimeCost};
use hex_core::record::{NativeRecord, PoolRecord};
use hex_runtime::devices::{
    Books, ManagedLaunch, PoolRecords, Records, RouteProjection, SharedCatalog, Transaction,
    check_managed_report, managed_native_scope_id, native_route_keys,
};
use hex_runtime::firecracker::FirecrackerRuntime;
use hex_runtime::guest::GuestRuntime;
use std::sync::Weak;

pub(super) struct Backends {
    firecracker: Option<Arc<FirecrackerRuntime>>,
    shards: Option<Arc<GuestRuntime>>,
    vm_root: Option<BoxRoot>,
    /// Joint plan step 14: the charge learner of the VMs under `vm_root`, where there are any.
    pub(super) learner: Option<super::charges::Shared>,
}

/// Only frozen initialized configurations participate. This runs inside the existing metadata
/// validator: no capacity/Books/provider/native query is permitted here.
fn project_initialized_routes(
    backends: Option<&Backends>,
    pools: &[PoolRecord],
    introduced: Option<(Execution, &[DeviceId])>,
) -> Result<RouteProjection, Refusal> {
    if pools.len() > MAX_BOXES {
        return Err(Refusal::Full(MAX_BOXES));
    }
    let firecracker = backends.and_then(|backends| backends.firecracker.as_deref());
    let shards = backends.and_then(|backends| backends.shards.as_deref());
    for pool in pools {
        pool.check()?;
        let present = match pool.execution {
            Execution::Firecracker => firecracker.is_some(),
            Execution::ShardsShared | Execution::ShardsDedicated => shards.is_some(),
            Execution::Native => false,
        };
        if !present {
            return Err(Refusal::Backing {
                key: pool.key,
                cause: LifecycleCause::Uncertain,
            });
        }
    }
    if let Some((execution, _)) = introduced {
        let present = match execution {
            Execution::Firecracker => firecracker.is_some(),
            Execution::ShardsShared | Execution::ShardsDedicated => shards.is_some(),
            Execution::Native => false,
        };
        if !present {
            return Err(projection_refused(
                "new backing has no initialized VM route producer",
            ));
        }
    }
    let arch = match std::env::consts::ARCH {
        "x86_64" => hex_api::ImageArch::Amd64,
        "aarch64" => hex_api::ImageArch::Arm64,
        _ => {
            return Err(projection_refused(
                "native route projection has an unsupported architecture",
            ));
        }
    };
    let mut projection = RouteProjection::new();
    // Node's actual native capacity producer always initializes these Linux Process/OCI forms.
    let (process, oci) = native_route_keys(
        Class::Oci,
        hex_api::ImagePlatform {
            os: hex_api::ImageOs::Linux,
            arch,
        },
    )?;
    projection.retain(process, None)?;
    if let Some(oci) = oci {
        projection.retain(oci, None)?;
    }
    if let Some(backend) = firecracker {
        backend.project_routes(pools, introduced, &mut projection)?;
    }
    if let Some(backend) = shards {
        backend.project_routes(pools, introduced, &mut projection)?;
    }
    Ok(projection)
}

fn projection_refused(why: &'static str) -> Refusal {
    native_argument(std::ffi::OsStr::new(why)).map_or_else(|error| error, Refusal::Broken)
}

impl Node {
    fn managed_bounds(record: NativeRecord<'_>) -> Result<hex_api::Bounds, Refusal> {
        let refused = || record.refusal(LifecycleCause::Uncertain);
        match record {
            NativeRecord::Box(record) => {
                record.spec.check()?;
                if record.spec.route_execution() == Some(Execution::ShardsShared) {
                    return Err(refused());
                }
            }
            NativeRecord::Pool(record) => record.check()?,
        }
        if !matches!(record.class()?, Class::Shards | Class::Firecracker)
            || record.scope().is_none()
        {
            return Err(refused());
        }
        let allocation = record
            .allocation()
            .ok_or_else(|| record.refusal(LifecycleCause::Pending))?;
        allocation.guest()?;
        record.commitment()?;
        // No memory.max (joint plan step 14, ruling B): a VM's memory is held by its budget's memory.high, the last
        // resort and its fleet's budget, and its swap stays off, which its managed create writes itself.
        Ok(hex_api::Bounds {
            cpu_max_milli: Some(
                u32::try_from(allocation.resources.cpu_milli).map_err(|_| refused())?,
            ),
            ..hex_api::Bounds::default()
        })
    }

    fn vm_root(&self, record: NativeRecord<'_>) -> Result<&BoxRoot, Refusal> {
        self.backends
            .get()
            .and_then(|backends| backends.vm_root.as_ref())
            .ok_or_else(|| record.refusal(LifecycleCause::Uncertain))
    }

    /// Exact node-owned inputs shared with the configured Shards provider's initialization probe.
    /// Both native launch and ROOT's provider constructor consume this same pair.
    pub fn shards_native_paths(&self) -> Result<(PathBuf, PathBuf), Refusal> {
        let state = self.bundles.parent().ok_or_else(|| {
            Refusal::Broken("native bundles directory has no state parent".into())
        })?;
        Ok((
            native_path(state, "shards-native-cache")?,
            native_path(state, "shards-native-home")?,
        ))
    }

    fn shards_program(&self, plan: &shards_api::PreparedBoot) -> Result<Program, Refusal> {
        let mut program = Program::default();
        let count = plan
            .arguments()
            .len()
            .checked_add(1)
            .ok_or(Refusal::Full(MAX_BOXES))?;
        program
            .args
            .try_reserve_exact(count)
            .map_err(|_| Refusal::Full(count))?;
        program
            .args
            .push(native_argument(plan.binary().as_os_str())?);
        for argument in plan.arguments() {
            program.args.push(native_argument(argument)?);
        }
        let (cache, home) = self.shards_native_paths()?;
        program
            .env
            .try_reserve_exact(2)
            .map_err(|_| Refusal::Full(2))?;
        for (name, path) in [("SHARDS_CACHE", cache), ("SHARDS_HOME", home)] {
            program.env.push((
                native_argument(std::ffi::OsStr::new(name))?,
                native_argument(path.as_os_str())?,
            ));
        }
        Ok(program)
    }

    fn launch_managed(
        &self,
        record: NativeRecord<'_>,
        plan: &ManagedLaunch<'_>,
        io: shards_api::LaunchIo,
        patience: Duration,
        publish: &mut dyn FnMut(&shards_api::Identity) -> Result<(), Refusal>,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        let until = Instant::now()
            .checked_add(patience)
            .filter(|_| !patience.is_zero() && patience != Duration::MAX)
            .ok_or_else(|| record.refusal(LifecycleCause::Uncertain))?;
        let bounds = Self::managed_bounds(record)?;
        self.backend(plan.class())?
            .ok_or_else(|| record.refusal(LifecycleCause::Uncertain))?;
        let shards_api::LaunchIo {
            stdout,
            stderr,
            gpu_report,
        } = io;
        let request = match plan {
            ManagedLaunch::Shards(prepared) => prepared.gpu_report_request(),
            ManagedLaunch::Firecracker { .. } => None,
        };
        let until = check_managed_report(
            Some(&self.attachments),
            record,
            plan,
            gpu_report.is_some(),
            until,
        )?;
        let report = match (request, gpu_report) {
            (Some(request), Some(writer)) => Some(process::ManagedReport {
                scope: request.scope(),
                writer,
            }),
            (None, None) => None,
            _ => return Err(record.refusal(LifecycleCause::Uncertain)),
        };
        let owned = match plan {
            ManagedLaunch::Shards(prepared) => Some(self.shards_program(prepared)?),
            ManagedLaunch::Firecracker { .. } => None,
        };
        let program = match (plan, owned.as_ref()) {
            (ManagedLaunch::Shards(_), Some(program)) => program,
            (ManagedLaunch::Firecracker { program, .. }, None) => *program,
            _ => return Err(record.refusal(LifecycleCause::Uncertain)),
        };
        program.check()?;
        program.fits(&process::exec_limit())?;
        let scope = managed_native_scope_id(record)?;
        let cgroup = self.vm_root(record)?.create_managed(&scope, &bounds)?;
        // Its swap stays off with no memory bound of its own (joint plan step 14, ruling B; HEX-RT-15), so all it
        // holds is RAM: a scope that can't hold that is removed again, refused by name.
        if let Err(why) = cgroup.set(&hex_core::cgroup::Write {
            file: "memory.swap.max",
            value: "0".to_owned(),
        }) {
            cgroup.remove()?;
            return Err(why);
        }
        let native = self
            .supervisor
            .reserve_managed(record, cgroup, plan.vsock())?;
        let stdio = Stdio {
            stdin: None,
            stdout: Some(stdout),
            stderr: Some(stderr),
            ..Stdio::default()
        };
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(record.refusal(LifecycleCause::Uncertain));
        }
        // A Firecracker VM given a device whole has its VMM's soft RLIMIT_MEMLOCK raised to its hard one, which its
        // create found holds the RAM its VMM pins for DMA.
        let memlock = match plan {
            ManagedLaunch::Firecracker { memlock, .. } => *memlock,
            ManagedLaunch::Shards(_) => None,
        };
        self.supervisor.bind_managed(
            &native,
            program,
            stdio,
            process::HeldLaunch { report, memlock },
            left,
            publish,
        )
    }

    fn attach_managed(
        &self,
        record: NativeRecord<'_>,
        identity: Option<&shards_api::Identity>,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        Self::managed_bounds(record)?;
        if let Some(native) = self.supervisor.observe_managed(record, identity)? {
            return Ok(native);
        }
        let scope = managed_native_scope_id(record)?;
        let root = self.vm_root(record)?;
        let cgroup = match BoxCgroup::observe_managed(native_path(root.dir(), scope.as_str())?)? {
            Some(cgroup) => cgroup,
            None => BoxCgroup::at(native_path(root.dir(), scope.as_str())?),
        };
        self.supervisor.recover_managed(record, cgroup, identity)
    }

    fn finalize_managed(
        &self,
        record: NativeRecord<'_>,
        identity: Option<&shards_api::Identity>,
    ) -> Result<(), Refusal> {
        Self::managed_bounds(record)?;
        let scope = managed_native_scope_id(record)?;
        self.supervisor.finalize_managed(record, identity)?;
        self.vm_root(record)?.forget(scope.as_str());
        Ok(())
    }

    /// Supply the one native provider before initializing any backend. A separate scheduler
    /// process obtains its physical report over Runtime; it never opens a second Gpus allocation.
    /// This never opens a Gpus instance or calls a capacity/attachment route recursively.
    pub fn bind_prober(&self, prober: crate::facts::SharedProber) -> Result<(), Refusal> {
        if self.backends.get().is_some() {
            return Err(Refusal::Broken(
                "physical provider must be supplied before backend initialization".into(),
            ));
        }
        self.prober
            .set(prober)
            .map_err(|_| Refusal::Broken("physical facts provider is already supplied".into()))
    }
    /// Reuse this Arc's authority; the backend retains only Weak, never another owner or cycle.
    pub fn backend_books(self: &Arc<Self>) -> Weak<dyn Books> {
        let books: Arc<dyn Books> = self.clone();
        Arc::downgrade(&books)
    }

    pub fn device_catalog(&self) -> SharedCatalog {
        self.attachments.clone()
    }

    /// Joins the core's loop `lp` to its VM backends, whose evented connections to their guests a forwarded exec takes
    /// ([`Node::forward_vm`]).
    pub(super) fn attach_backends(&self, lp: Arc<dyn crate::serve::Loop>) -> Result<(), Refusal> {
        let Some(backends) = self.backends.get() else {
            return Ok(());
        };
        let joins: Arc<dyn hex_runtime::guest::Joins> = Arc::new(crate::recorded::LoopJoins(lp));
        if let Some(backend) = &backends.firecracker {
            backend.join_loop(Arc::clone(&joins))?;
        }
        if let Some(guest) = &backends.shards {
            guest.join_loop(joins)?;
        }
        Ok(())
    }

    /// An exec into a VM box goes to its guest's node through its backend's evented connection
    /// ([`crate::recorded::forward_evented`]), as a guest's call goes on every host (AC-T7.4.1): its answer and its
    /// program's end come as the guest's messages. Its program is no process of this host's, its pid the guest's, so
    /// the node never watches or signals a process by it (GPU run r1119: the guest pid, naming no host process, was
    /// waited for as a gone one's and its end reported unknown). Every other call, and an exec into a native box or a
    /// Shards image VM, whose guest runs no node of Hex's ([`guest_node_class`]), comes back to run here: on a node with
    /// no VM backend, or into an `oci` box the supervisor watches, without reading the box's record.
    pub(super) fn forward_vm(
        &self,
        call: hex_core::wire::Call,
        stdio: Stdio,
        reply: crate::serve::Reply,
        ended: crate::serve::Ended,
    ) -> crate::serve::Forward {
        use crate::serve::Forward;
        use hex_core::wire::Call;
        let id = match &call {
            Call::Exec { id, .. } | Call::ExecStreams { id, .. } => id,
            _ => return Forward::Back(Box::new((call, stdio))),
        };
        let Some(backends) = self
            .backends
            .get()
            .filter(|backends| backends.firecracker.is_some() || backends.shards.is_some())
        else {
            return Forward::Back(Box::new((call, stdio)));
        };
        match self.supervisor.watches_container(id) {
            Ok(true) => return Forward::Back(Box::new((call, stdio))),
            Ok(false) => {}
            Err(why) => return Forward::Refused(why),
        }
        let class = match self.store.read_lifecycle(id) {
            Ok(Some(record)) => match guest_node_class(&record.spec) {
                Ok(Some(class)) => class,
                Ok(None) => return Forward::Back(Box::new((call, stdio))),
                Err(why) => return Forward::Refused(why),
            },
            Ok(None) => return Forward::Back(Box::new((call, stdio))),
            Err(why) => return Forward::Refused(why),
        };
        let client = match class {
            Class::Firecracker => match &backends.firecracker {
                Some(backend) => backend.evented_for(id),
                None => Err(uncertain(id)),
            },
            Class::Shards => match &backends.shards {
                Some(guest) => guest
                    .evented_for(id)
                    .and_then(|client| client.ok_or_else(|| uncertain(id))),
                None => Err(uncertain(id)),
            },
            Class::Process | Class::Oci => return Forward::Back(Box::new((call, stdio))),
        };
        match client {
            Ok(client) => crate::recorded::forward_evented(client, call, stdio, reply, ended),
            Err(why) => Forward::Refused(why),
        }
    }

    /// The host's boot session as this node's records carry it (T-T7.1.2), for a backend whose own marks last until
    /// the host reboots: the Firecracker adapter's single-use functions. `None` where it can't be read.
    pub fn boot_session(&self) -> Option<hex_core::record::BootSession> {
        self.store.boot().cloned()
    }

    fn initialize_vm_root(&self) -> Result<(BoxRoot, std::fs::File), Refusal> {
        let state = self.bundles.parent().ok_or(Refusal::Full(MAX_BOXES))?;
        let lease_path = native_path(state, "native-vm-owner.lock")?;
        let lease = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lease_path)
            .map_err(|e| Refusal::os("opening native VM owner lease", e))?;
        lease
            .try_lock()
            .map_err(|e| Refusal::os("locking native VM owner lease", e))?;
        let mut disks = Vec::new();
        disks
            .try_reserve_exact(self.root.disks().len())
            .map_err(|_| Refusal::Full(MAX_BOXES))?;
        disks.extend_from_slice(self.root.disks());
        let vm_parent = native_path(self.root.dir(), ".vm")?;
        let mut parent_disks = Vec::new();
        parent_disks
            .try_reserve_exact(disks.len())
            .map_err(|_| Refusal::Full(MAX_BOXES))?;
        parent_disks.extend_from_slice(&disks);
        let _parent = BoxRoot::open(&vm_parent, parent_disks)?;
        let path = native_path(&vm_parent, &self.key)?;
        let root = BoxRoot::open(&path, disks)?;
        Ok((root, lease))
    }

    /// ROOT calls this once after Arc<Node> and before recovery/serve. No class is advertised while
    /// its configured adapter/cost is uninitialized; absence keeps the native-only Node.
    pub fn install_backends(
        self: &Arc<Self>,
        firecracker: Option<Arc<FirecrackerRuntime>>,
        shards: Option<Arc<GuestRuntime>>,
    ) -> Result<(), Refusal> {
        if self.backends.get().is_some() {
            return Err(Refusal::Broken(
                "node backends are already initialized".into(),
            ));
        }
        // Each VM box holds descriptors of the node's past a box's, which only a start that ran VM boxes counted.
        if (firecracker.is_some() || shards.is_some()) && !self.pools.vms {
            return Err(Refusal::Invalid {
                what: "VM backends",
                why: "this node's start counted no VM boxes' descriptors among its own: open it running them \
                      (Node::open_running, Runs { vms: true })"
                    .to_owned(),
            });
        }
        if let Some(guest) = &shards {
            guest.bind_books(self.backend_books())?;
            guest.bind_boot_session(self.boot_session())?;
        }
        let (_, retained_pools, _) =
            self.preflight_configured(firecracker.as_deref(), shards.as_deref())?;
        let (vm_root, lease) = if firecracker.is_some() || shards.is_some() {
            let (root, lease) = self.initialize_vm_root()?;
            (Some(root), Some(lease))
        } else {
            (None, None)
        };
        if let Some(backend) = &firecracker {
            backend.initialize_native()?;
        }
        if let (Some(root), Some(lease)) = (vm_root.as_ref(), lease) {
            self.supervisor.bind_managed_root(root.dir(), lease)?;
        }
        let learner = match vm_root.as_ref() {
            Some(root) => Some(Mutex::new(super::charges::Learner::new(
                root.dir(),
                self.native_capacity()?.total,
            )?)),
            None => None,
        };
        // The same initialized owner must exist before guarded metadata preparation projects
        // its real routes. The complete retained-state barrier above precedes this publication;
        // startup still returns only after preparation and capacity validation below succeed.
        self.backends
            .set(Backends {
                firecracker,
                shards,
                vm_root,
                learner,
            })
            .map_err(|_| {
                projection_refused("node backend initialization raced another initializer")
            })?;
        let backends = self
            .backends
            .get()
            .ok_or_else(|| projection_refused("initialized node backends were not published"))?;
        // Joint plan step 14 (ROOT, 2026-11-13): what this node recorded under a profile's census cost is retired, its
        // natives up, before any preparation or recovery uses it; the preflight above passed it for this alone.
        if let Some(backend) = &backends.firecracker {
            backend.retire_census()?;
        }
        if let Some(backend) = &backends.shards {
            backend.retire_census()?;
        }
        if let Some(backend) = &backends.firecracker {
            backend.prepare_backing()?;
        }
        if retained_pools
            .iter()
            .any(|record| record.execution == Execution::ShardsShared)
        {
            backends
                .shards
                .as_ref()
                .ok_or_else(|| missing(Class::Shards))?
                .prepare_backing()?;
        }
        for (class, backend) in [
            (
                Class::Firecracker,
                backends
                    .firecracker
                    .as_deref()
                    .map(|backend| backend as &dyn Runtime),
            ),
            (
                Class::Shards,
                backends
                    .shards
                    .as_deref()
                    .map(|backend| backend as &dyn Runtime),
            ),
        ] {
            if let Some(backend) = backend {
                if !backend.classes().contains(&class) {
                    return Err(missing(class));
                }
                let capacity = backend.capacity(class)?.ok_or_else(|| missing(class))?;
                capacity.check()?;
                if capacity.routes.is_empty() && capacity.cost.is_none()
                    || !capacity.routes.is_empty()
                        && !capacity
                            .routes
                            .iter()
                            .any(|offer| offer.key.backend_class() == class)
                {
                    return Err(missing(class));
                }
                // Per-route cost/backing validation above replaces the legacy class-wide cost.
            }
        }
        if backends.learner.is_some() {
            super::charges::spawn(Arc::downgrade(self))?;
        }
        Ok(())
    }

    fn check_route_projection(
        &self,
        pools: &[PoolRecord],
        introduced: Option<(Execution, &[DeviceId])>,
    ) -> Result<(), Refusal> {
        project_initialized_routes(self.backends.get(), pools, introduced).map(|_| ())
    }

    /// The retained records and pools, each past the whole-owner barrier, and apart from them the torn
    /// records ([`crate::store::Store::take_torn`]). A torn record lies in the runtime directory, where this
    /// node keeps only a native box's: its VM backends' records are scoped, so durable
    /// ([`crate::store::Store::put_lifecycle`]), and it writes none for a shared guest's box (the Shards
    /// backend makes one unrecorded, and is offered only for dedicated VMs here). So whatever is configured,
    /// recovery proves a torn record's box ended by the box's own cgroup and removes it as unreadable
    /// (T-T7.1.2). One in the state directory that can't be read is damage: refused, naming its file.
    fn preflight_configured(
        &self,
        firecracker: Option<&FirecrackerRuntime>,
        shards: Option<&GuestRuntime>,
    ) -> Result<(Records, PoolRecords, Torn), Refusal> {
        let mut records = self.store.retained_records()?;
        let torn = self.store.take_torn(&mut records)?;
        self.store.refuse_unreadable(&records)?;
        let pools = self.store.all_pools()?;
        crate::recorded::preflight_records(&records, &pools, Some(self.attachments.as_ref()))?;
        for (id, record) in &records {
            let record = record.as_ref().map_err(|_| uncertain(id))?;
            let handoff = crate::recorded::preflight_handoff_turns(
                &self.turns,
                &self.store,
                record,
                Some(self.attachments.as_ref()),
            )?;
            let source = if handoff.is_some() {
                let key = record
                    .spec
                    .route
                    .and_then(|route| route.backing)
                    .ok_or_else(|| uncertain(id))?;
                Some(
                    self.store
                        .read_pool(&key.pool)?
                        .filter(|source| source.key == key)
                        .ok_or_else(|| uncertain(id))?,
                )
            } else {
                None
            };
            match record.spec.backend_class()? {
                Class::Firecracker => {
                    let backend = firecracker.ok_or_else(|| uncertain(id))?;
                    preflight_backend(
                        record,
                        source.as_ref(),
                        |record| backend.preflight_record(record),
                        |record, source| backend.handoff_unsubmitted(record, source),
                    )?;
                }
                Class::Shards => {
                    let backend = shards.ok_or_else(|| uncertain(id))?;
                    preflight_backend(
                        record,
                        source.as_ref(),
                        |record| backend.preflight_box(record),
                        |record, source| backend.handoff_unsubmitted(record, source),
                    )?;
                }
                Class::Process | Class::Oci => {}
            }
        }
        for record in &pools {
            let refused = || Refusal::Backing {
                key: record.key,
                cause: LifecycleCause::Uncertain,
            };
            match record.execution {
                Execution::Firecracker => {
                    firecracker.ok_or_else(refused)?.preflight_backing(record)?
                }
                Execution::ShardsShared | Execution::ShardsDedicated => {
                    shards.ok_or_else(refused)?.preflight_backing(record)?
                }
                Execution::Native => return Err(refused()),
            }
        }
        let bound = records
            .len()
            .checked_add(pools.len())
            .ok_or(Refusal::Full(MAX_BOXES))?;
        if bound > MAX_BOXES {
            return Err(Refusal::Full(MAX_BOXES));
        }
        let mut managed = Vec::new();
        managed
            .try_reserve_exact(bound)
            .map_err(|_| Refusal::Full(MAX_BOXES))?;
        for (id, record) in &records {
            let record = record.as_ref().map_err(|_| uncertain(id))?;
            // A retired box's VM is gone with its retirement (joint plan step 14): no native scope to adopt.
            if record.retired {
                continue;
            }
            if matches!(
                record.spec.backend_class()?,
                Class::Firecracker | Class::Shards
            ) && record.spec.route_execution() != Some(Execution::ShardsShared)
            {
                let native = NativeRecord::Box(record);
                managed.push((native, Some(Self::managed_bounds(native)?)));
            }
        }
        for record in &pools {
            let native = NativeRecord::Pool(record);
            let bounds = if record.allocation.is_some() {
                Some(Self::managed_bounds(native)?)
            } else {
                None
            };
            managed.push((native, bounds));
        }
        let parent = native_path(self.root.dir(), ".vm")?;
        self.supervisor
            .preflight_managed(&native_path(&parent, &self.key)?, &managed)?;
        Ok((records, pools, torn))
    }

    pub(super) fn retained_preflight(&self) -> Result<(Records, PoolRecords, Torn), Refusal> {
        let backends = self.backends.get();
        self.preflight_configured(
            backends.and_then(|backends| backends.firecracker.as_deref()),
            backends.and_then(|backends| backends.shards.as_deref()),
        )
    }

    /// Read-only whole-owner barrier, also usable by CLI before any explicit pool preparation.
    pub fn preflight_recovery(&self) -> Result<(), Refusal> {
        self.retained_preflight().map(|_| ())
    }

    pub(super) fn recover_pools(
        &self,
        pools: &[hex_core::record::PoolRecord],
    ) -> Result<(), Refusal> {
        for record in pools {
            let refused = || Refusal::Backing {
                key: record.key,
                cause: LifecycleCause::Uncertain,
            };
            match record.execution {
                Execution::Firecracker => self
                    .backends
                    .get()
                    .and_then(|backends| backends.firecracker.as_ref())
                    .ok_or_else(refused)?
                    .recover_backing()?,
                Execution::ShardsShared | Execution::ShardsDedicated => self
                    .backends
                    .get()
                    .and_then(|backends| backends.shards.as_ref())
                    .ok_or_else(refused)?
                    .recover_backing()?,
                Execution::Native => return Err(refused()),
            }
        }
        Ok(())
    }

    fn native_authority(&self) -> Result<String, Refusal> {
        let mut authority = String::new();
        let bytes = "node:"
            .len()
            .checked_add(self.key.len())
            .ok_or(Refusal::Full(MAX_BOXES))?;
        authority
            .try_reserve_exact(bytes)
            .map_err(|_| Refusal::Exhausted {
                what: "native authority identity bytes",
                bound: bytes,
            })?;
        authority.push_str("node:");
        authority.push_str(&self.key);
        Ok(authority)
    }

    pub(super) fn native_capacity(&self) -> Result<Capacity, Refusal> {
        crate::facts::capacity(&self.bundles, self.native_authority()?)
    }

    pub(super) fn native_physical_report(&self) -> Result<hex_api::PhysicalReport, Refusal> {
        let provider = self.prober.get().ok_or_else(|| {
            Refusal::Broken("the native physical provider is not initialized".into())
        })?;
        let authority = self.native_authority()?;
        let mut report = provider
            .lock()
            .map_err(|_| Refusal::Broken("physical facts provider lock poisoned".into()))?
            .physical_report(authority)?;
        if let Some(backend) = self
            .backends
            .get()
            .and_then(|backends| backends.firecracker.as_ref())
        {
            crate::facts::merge_physical_observations(
                &mut report,
                Class::Firecracker,
                backend.physical_observations(),
            )?;
        }
        if let Some(backend) = self
            .backends
            .get()
            .and_then(|backends| backends.shards.as_ref())
        {
            crate::facts::merge_physical_observations(
                &mut report,
                Class::Shards,
                backend.physical_observations(),
            )?;
        }
        report.check()?;
        Ok(report)
    }

    /// Dispatch from the retained configuration/generation before entering its method/native turn.
    /// A current offer cannot replace a recovered grant or choose a different VM implementation.
    pub(super) fn pool_backend(&self, key: &BackingKey) -> Result<&dyn Runtime, Refusal> {
        let refused = |cause| Refusal::Backing { key: *key, cause };
        let record = self
            .store
            .read_pool(&key.pool)?
            .ok_or_else(|| refused(LifecycleCause::Pending))?;
        record.check()?;
        if record.key != *key {
            return Err(refused(LifecycleCause::Uncertain));
        }
        let class = NativeRecord::Pool(&record).class()?;
        self.backend(class)
            .map_err(|_| refused(LifecycleCause::Uncertain))?
            .ok_or_else(|| refused(LifecycleCause::Uncertain))
    }

    pub(super) fn backend(&self, class: Class) -> Result<Option<&dyn Runtime>, Refusal> {
        match class {
            Class::Process | Class::Oci => Ok(None),
            Class::Firecracker => self
                .backends
                .get()
                .and_then(|backends| backends.firecracker.as_deref())
                .map(|backend| Some(backend as &dyn Runtime))
                .ok_or_else(|| missing(class)),
            Class::Shards => self
                .backends
                .get()
                .and_then(|backends| backends.shards.as_deref())
                .map(|backend| Some(backend as &dyn Runtime))
                .ok_or_else(|| missing(class)),
        }
    }

    /// Persisted class dispatch happens before native container/cgroup paths or method turns.
    pub(super) fn backend_record(&self, id: &BoxId) -> Result<Option<&dyn Runtime>, Refusal> {
        let Some(record) = self.store.read_lifecycle(id)? else {
            return Ok(None);
        };
        self.backend(record.spec.backend_class()?)
            .map_err(|_| uncertain(id))
    }

    pub(super) fn initialized_classes(&self) -> Vec<Class> {
        let mut classes = vec![Class::Process, Class::Oci];
        if let Some(backends) = self.backends.get() {
            if backends.firecracker.is_some() {
                classes.push(Class::Firecracker);
            }
            if backends.shards.is_some() {
                classes.push(Class::Shards);
            }
        }
        classes
    }

    /// Preflight every backend before recovery is allowed to mutate ANY native resource. Unknown
    /// records cannot be treated as native Process/OCI cleanup authority.
    pub(super) fn recover_backends(&self, records: &Records) -> Result<RecoverySummary, Refusal> {
        for (id, record) in records {
            let record = record.as_ref().map_err(|_| uncertain(id))?;
            if record.spec.id != *id {
                return Err(uncertain(id));
            }
            record.spec.check()?;
            if matches!(
                record.spec.backend_class()?,
                Class::Firecracker | Class::Shards
            ) && record.spec.route_execution() != Some(hex_api::Execution::ShardsShared)
                && record.spec.allocation.is_none()
            {
                return Err(uncertain(id));
            }
            self.backend(record.spec.backend_class()?)
                .map_err(|_| uncertain(id))?;
        }
        let mut summary = RecoverySummary::default();
        summary
            .adopted
            .try_reserve_exact(records.len())
            .map_err(|_| Refusal::Full(MAX_BOXES))?;
        summary
            .removed
            .try_reserve_exact(records.len())
            .map_err(|_| Refusal::Full(MAX_BOXES))?;
        summary
            .held
            .try_reserve_exact(records.len())
            .map_err(|_| Refusal::Full(MAX_BOXES))?;
        for (id, record) in records {
            let record = record.as_ref().map_err(|_| uncertain(id))?;
            // A retired box ran nothing past its retirement and is kept for its owner's delete (joint plan step 14).
            if record.retired {
                continue;
            }
            let Some(backend) = self
                .backend(record.spec.backend_class()?)
                .map_err(|_| uncertain(id))?
            else {
                continue;
            };
            match record.phase {
                Phase::Creating | Phase::Deleting => match backend.delete(id) {
                    Ok(()) => summary.removed.push((
                        id.try_clone()?,
                        "actual VM backend completed retained cleanup".into(),
                    )),
                    Err(Refusal::Awaits {
                        needs: hex_api::Upstream::AttachAfterCrash,
                    }) => summary
                        .held
                        .push((id.try_clone()?, crate::recorded::IMAGE_HELD.into())),
                    Err(why) => return Err(why),
                },
                Phase::Ready => {
                    let recovered = match record.spec.backend_class()? {
                        Class::Firecracker => self
                            .backends
                            .get()
                            .and_then(|backends| backends.firecracker.as_ref())
                            .ok_or_else(|| uncertain(id))?
                            .validate_binding(&record.spec),
                        Class::Shards => self
                            .backends
                            .get()
                            .and_then(|backends| backends.shards.as_ref())
                            .ok_or_else(|| uncertain(id))?
                            .recover_managed(&record.spec),
                        Class::Process | Class::Oci => return Err(uncertain(id)),
                    };
                    match recovered {
                        Ok(()) => {}
                        Err(Refusal::Awaits {
                            needs: hex_api::Upstream::AttachAfterCrash,
                        }) => {
                            summary
                                .held
                                .push((id.try_clone()?, crate::recorded::IMAGE_HELD.into()));
                            continue;
                        }
                        Err(why) => return Err(why),
                    }
                    let actual = backend.inspect(id)?;
                    if actual.class != record.spec.class.class()
                        || actual.allocation != record.spec.allocation
                        || actual.route != record.spec.route
                    {
                        return Err(uncertain(id));
                    }
                    summary.adopted.push(id.try_clone()?);
                }
            }
        }
        Ok(summary)
    }
}

/// Both real backends choose exactly one proof under the canonical guard. Ordinary selected
/// receipt rules cannot run first and reject a valid no-submission Deleting reservation.
fn preflight_backend(
    record: &BoxRecord,
    source: Option<&hex_core::record::PoolRecord>,
    ordinary: impl FnOnce(&BoxRecord) -> Result<(), Refusal>,
    unsubmitted: impl FnOnce(&BoxRecord, &hex_core::record::PoolRecord) -> Result<(), Refusal>,
) -> Result<(), Refusal> {
    match source {
        Some(source)
            if matches!(
                source.phase,
                hex_api::BackingPhase::Live | hex_api::BackingPhase::Draining
            ) =>
        {
            unsubmitted(record, source)
        }
        None => ordinary(record),
        Some(source) if source.phase == hex_api::BackingPhase::Released => ordinary(record),
        Some(source) => Err(Refusal::Backing {
            key: source.key,
            cause: LifecycleCause::Uncertain,
        }),
    }
}

fn native_argument(argument: &std::ffi::OsStr) -> Result<String, Refusal> {
    let argument = argument.to_str().ok_or_else(|| Refusal::Invalid {
        what: "managed native program",
        why: "configured native argument is not UTF-8".into(),
    })?;
    let mut value = String::new();
    value
        .try_reserve_exact(argument.len())
        .map_err(|_| Refusal::Exhausted {
            what: "managed native argument bytes",
            bound: argument.len(),
        })?;
    value.push_str(argument);
    Ok(value)
}

#[derive(Clone, Copy)]
enum BookOwner<'a> {
    Box(&'a BoxId),
    Handoff {
        id: &'a BoxId,
        source: &'a BackingKey,
    },
    Pool(&'a BackingKey),
}

impl Node {
    fn book_transaction(
        &self,
        owner: BookOwner<'_>,
        devices: &[DeviceId],
        callback: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        let keys = self.attachments.conflict_ids(devices)?;
        let (_pool, _method) = match owner {
            BookOwner::Box(id) => (None, self.turns.try_take_devices(id, &keys)?),
            BookOwner::Pool(key) => (None, self.turns.try_take_pool(key, &keys)?),
            BookOwner::Handoff { id, source } => {
                let (pool, boxed) = self.turns.try_take_handoff(id, source, &keys)?;
                (Some(pool), boxed)
            }
        };
        let _devices = self.hold_attachments(&keys)?;
        let launch =
            |record: NativeRecord<'_>,
             plan: &ManagedLaunch<'_>,
             io: shards_api::LaunchIo,
             patience: Duration,
             publish: &mut dyn FnMut(&shards_api::Identity) -> Result<(), Refusal>| {
                self.launch_managed(record, plan, io, patience, publish)
            };
        let attach = |record: NativeRecord<'_>, identity: Option<&shards_api::Identity>| {
            self.attach_managed(record, identity)
        };
        let finalize = |record: NativeRecord<'_>, identity: Option<&shards_api::Identity>| {
            self.finalize_managed(record, identity)
        };
        let usage = |record: NativeRecord<'_>| self.supervisor.managed_usage(record);
        let unsubmitted_pool = |record: NativeRecord<'_>| {
            self.supervisor
                .check_unsubmitted_pool(record, self.vm_root(record)?.dir())
        };
        let native = crate::recorded::Native {
            catalog: Some(self.attachments.as_ref()),
            launch: &launch,
            attach: &attach,
            finalize: &finalize,
            usage: &usage,
            unsubmitted_pool: &unsubmitted_pool,
        };
        match owner {
            BookOwner::Box(id) => {
                crate::recorded::transaction(&self.store, id, &keys, &native, callback)
            }
            BookOwner::Pool(key) => {
                crate::recorded::pool_transaction(&self.store, key, &keys, &native, callback)
            }
            BookOwner::Handoff { id, source } => crate::recorded::handoff_transaction(
                &self.store,
                id,
                source,
                &keys,
                &native,
                callback,
            ),
        }
    }
}

impl Books for Node {
    fn records(&self) -> Result<Records, Refusal> {
        self.store.all()
    }
    fn pool_records(&self) -> Result<PoolRecords, Refusal> {
        self.store.all_pools()
    }
    fn prepare_pool(
        &self,
        pool: [u8; 32],
        execution: Execution,
        cost: RuntimeCost,
        devices: &[DeviceId],
        family: (&[u8; 32], &[DeviceId], Option<&PoolRecord>),
        validate: &mut hex_runtime::devices::PoolPreparationCheck<'_>,
    ) -> Result<PoolRecord, Refusal> {
        let keys = self.attachments.conflict_ids(family.1)?;
        let _method = self.turns.try_take_pool_configuration(family.0, &keys)?;
        let _devices = self.hold_attachments(&keys)?;
        let introduced = if family.2.is_none() {
            Some((execution, devices))
        } else {
            None
        };
        let mut checked = |records: &Records, pools: &[PoolRecord]| {
            validate(records, pools)?;
            self.check_route_projection(pools, introduced)
        };
        crate::recorded::prepare_pool(
            &self.store,
            pool,
            execution,
            cost,
            devices,
            (family.0, &keys, family.2),
            &mut checked,
        )
    }
    fn capacity(&self) -> Result<Capacity, Refusal> {
        self.native_capacity()
    }
    fn physical_devices(&self) -> Result<Vec<hex_api::DeviceInfo>, Refusal> {
        let provider = self.prober.get().ok_or_else(|| {
            Refusal::Broken("the Node physical facts provider was not initialized".into())
        })?;
        crate::facts::physical_devices(provider)
    }
    fn physical_luid(&self, id: DeviceId) -> Result<[u8; 8], Refusal> {
        let provider = self.prober.get().ok_or_else(|| missing(Class::Shards))?;
        crate::facts::physical_luid(provider, id)
    }
    fn with_transaction(
        &self,
        id: &BoxId,
        devices: &[DeviceId],
        callback: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        self.book_transaction(BookOwner::Box(id), devices, callback)
    }
    fn with_pool_transaction(
        &self,
        key: &BackingKey,
        devices: &[DeviceId],
        callback: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        self.book_transaction(BookOwner::Pool(key), devices, callback)
    }
    fn with_handoff_transaction(
        &self,
        id: &BoxId,
        source: &BackingKey,
        full_conflicts: &[DeviceId],
        callback: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        self.book_transaction(BookOwner::Handoff { id, source }, full_conflicts, callback)
    }
}

pub(super) fn uncertain(id: &BoxId) -> Refusal {
    match id.try_clone() {
        Ok(id) => Refusal::Lifecycle {
            id,
            cause: LifecycleCause::Uncertain,
        },
        Err(refusal) => refusal,
    }
}
fn missing(class: Class) -> Refusal {
    Refusal::Platform {
        what: "initialized VM backend",
        platform: "Linux node",
        why: format!(
            "{} backend or verified cost is not initialized",
            class.name()
        ),
    }
}

fn native_path(parent: &Path, child: &str) -> Result<PathBuf, Refusal> {
    let room = parent
        .as_os_str()
        .len()
        .checked_add(child.len())
        .and_then(|n| n.checked_add(1))
        .ok_or(Refusal::Full(MAX_BOXES))?;
    let mut path = PathBuf::new();
    path.try_reserve_exact(room)
        .map_err(|_| Refusal::Exhausted {
            what: "native VM path bytes",
            bound: room,
        })?;
    path.push(parent);
    path.push(child);
    Ok(path)
}

/// The VM backend whose guest's node an exec into a box of `spec` goes to ([`Node::forward_vm`]), or none where it
/// runs here: a native box's, and a Shards image VM's, whose guest runs no node of Hex's, so its runtime answers the
/// exec, refused naming the members' exec it awaits (joint plan step 12, docs/design/shards-image-class.md §2).
fn guest_node_class(spec: &BoxSpec) -> Result<Option<Class>, Refusal> {
    if matches!(spec.class, ClassSpec::ShardsImage(_)) {
        return Ok(None);
    }
    Ok(match spec.backend_class()? {
        class @ (Class::Firecracker | Class::Shards) => Some(class),
        Class::Process | Class::Oci => None,
    })
}

#[cfg(test)]
mod forward_route_tests {
    use super::*;
    use hex_api::{Bounds, Network};

    fn spec(class: ClassSpec, allocation: Option<hex_api::Allocation>) -> BoxSpec {
        BoxSpec {
            id: BoxId::new("forwarded").unwrap(),
            bounds: Bounds::default(),
            class,
            owner: None,
            devices: Vec::new(),
            allocation,
            route: None,
            band: 0,
        }
    }

    /// Joint plan step 12, AC-T7.4.1: do route an exec by its box's spec. Expect: a Shards guest box's to its
    /// guest's node; a Shards image VM's back to run here, whose runtime refuses it naming the members' exec it
    /// awaits, since no node of Hex's runs in its guest (forwarded, it was refused Uncertain); a native box's back.
    #[test]
    fn an_exec_into_a_shards_image_vm_runs_here_and_into_a_shards_guest_box_at_its_node() {
        let image = spec(
            ClassSpec::ShardsImage(Box::new(hex_api::ShardsImage::new(hex_api::sim::SIM_IMAGE))),
            Some(hex_api::sim::sim_vm_allocation().unwrap()),
        );
        assert_eq!(guest_node_class(&image).unwrap(), None);
        let guest = spec(
            ClassSpec::Shards(Box::new(OciSpec {
                rootfs: Rootfs::Dir(PathBuf::from("/")),
                init: Program::of(&["/bin/true"]),
                mounts: Vec::new(),
                hostname: None,
                network: Network::Private,
                hosts: Vec::new(),
            })),
            None,
        );
        assert_eq!(guest_node_class(&guest).unwrap(), Some(Class::Shards));
        assert_eq!(
            guest_node_class(&spec(ClassSpec::Process, None)).unwrap(),
            None
        );
    }
}

#[cfg(test)]
mod managed_bounds_tests {
    use super::*;

    /// Joint plan step 14 (AC-T7.4.1; ROOT, 2026-11-13, ruling B; goal 15's finding): a VM's native scope gets no
    /// memory.max at its create. With a census-free cost its allocation is its guest's RAM alone, so a cap there would
    /// leave its VMM's own memory no room before its first window; its budget's memory.high, the last resort and its
    /// fleet's budget hold it. Do bound a Shards image VM's record for its create. Expect its CPU capped at its
    /// allocation and no memory bound.
    #[test]
    fn w14_a_vm_scope_gets_no_memory_max_at_create() {
        let allocation = hex_api::sim::sim_vm_allocation().unwrap();
        let spec = BoxSpec {
            id: BoxId::new("image-vm").unwrap(),
            bounds: hex_api::Bounds::default(),
            class: ClassSpec::ShardsImage(Box::new(hex_api::ShardsImage::new(
                hex_api::sim::SIM_IMAGE,
            ))),
            owner: None,
            devices: Vec::new(),
            allocation: Some(allocation),
            route: None,
            band: 0,
        };
        let mut record = BoxRecord::new(spec, Phase::Creating, Vec::new());
        record.native_scope = Some(hex_core::record::NativeScope::from_bytes([7; 16]));
        let bounds = Node::managed_bounds(NativeRecord::Box(&record)).unwrap();
        assert_eq!(bounds.memory_max, None);
        assert_eq!(
            bounds.cpu_max_milli,
            Some(u32::try_from(allocation.resources.cpu_milli).unwrap())
        );
    }
}

#[cfg(test)]
mod preflight_dispatch_tests {
    use super::*;
    use hex_api::{BackingPhase, CapacityResources};

    /// AC-T7.4.1: a native-only initialized Node has its actual two native prefixes; an absent
    /// adapter cannot create VM route room merely because a proposed lifetime names that class.
    #[test]
    fn native_only_projection_does_not_invent_vm_routes_for_new_lifetimes() {
        assert_eq!(
            project_initialized_routes(None, &[], None)
                .unwrap()
                .count()
                .unwrap(),
            2
        );
        let no_vm = Backends {
            firecracker: None,
            shards: None,
            vm_root: None,
            learner: None,
        };
        assert_eq!(
            project_initialized_routes(Some(&no_vm), &[], None)
                .unwrap()
                .count()
                .unwrap(),
            2
        );
        let gpu = [DeviceId {
            kind: hex_api::DeviceKind::Gpu,
            key: 256,
        }];
        for (execution, devices) in [
            (Execution::Firecracker, gpu.as_slice()),
            (Execution::ShardsDedicated, gpu.as_slice()),
            (Execution::ShardsShared, [].as_slice()),
            (Execution::Native, [].as_slice()),
        ] {
            assert!(
                project_initialized_routes(Some(&no_vm), &[], Some((execution, devices))).is_err()
            );
        }
    }

    /// AC-T7.4.1: both concrete adapters use this actual mutually exclusive callback dispatch.
    /// The adapter proof bodies are covered separately; this is not native absence evidence.
    #[test]
    fn guarded_creating_deleting_proof_is_exclusive_and_released_uses_ordinary_strict() {
        let cost = RuntimeCost {
            id: [1; 32],
            floor: CapacityResources::default(),
            ceiling: CapacityResources::default(),
            quantum: CapacityResources {
                cpu_milli: 1,
                mem_bytes: 1,
                disk_bytes: 1,
            },
            guest_overhead: CapacityResources::default(),
            overhead: CapacityResources::default(),
        };
        let mut source = hex_core::record::PoolRecord {
            version: hex_core::record::RECORD_VERSION,
            key: BackingKey {
                pool: [2; 32],
                generation: [3; 16],
            },
            execution: Execution::ShardsDedicated,
            cost,
            allocation: None,
            devices: Vec::new(),
            phase: BackingPhase::Live,
            init: None,
            exit: None,
        };
        let spec = BoxSpec {
            id: BoxId::new("dispatch-only").unwrap(),
            class: hex_api::ClassSpec::Process,
            bounds: hex_api::Bounds::default(),
            owner: None,
            devices: Vec::new(),
            allocation: None,
            route: None,
            band: 0,
        };
        let mut record = BoxRecord::new(spec, Phase::Creating, Vec::new());
        for phase in [Phase::Creating, Phase::Deleting] {
            record.phase = phase;
            for pool_phase in [
                BackingPhase::Live,
                BackingPhase::Draining,
                BackingPhase::Released,
            ] {
                source.phase = pool_phase;
                let mut ordinary = 0;
                let mut proof = 0;
                let checked = preflight_backend(
                    &record,
                    Some(&source),
                    |_| {
                        ordinary += 1;
                        Err(uncertain(&record.spec.id))
                    },
                    |record, source| {
                        proof += 1;
                        assert!(record.init.is_none());
                        assert!(matches!(
                            source.phase,
                            BackingPhase::Live | BackingPhase::Draining
                        ));
                        Ok(())
                    },
                );
                if pool_phase == BackingPhase::Released {
                    assert!(checked.is_err());
                    assert_eq!((ordinary, proof), (1, 0));
                } else {
                    assert!(checked.is_ok());
                    assert_eq!((ordinary, proof), (0, 1));
                }
            }
        }
        let mut ordinary = 0;
        let mut proof = 0;
        assert!(
            preflight_backend(
                &record,
                None,
                |_| {
                    ordinary += 1;
                    Err(uncertain(&record.spec.id))
                },
                |_, _| {
                    proof += 1;
                    Ok(())
                }
            )
            .is_err()
        );
        assert_eq!((ordinary, proof), (1, 0));
        source.phase = BackingPhase::Prepared;
        assert!(
            preflight_backend(
                &record,
                Some(&source),
                |_| panic!("invalid source may not enter ordinary callback"),
                |_, _| panic!("ungranted source may not enter proof")
            )
            .is_err()
        );
    }
}
