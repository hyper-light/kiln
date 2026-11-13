use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Instant;

use hex_api::{
    BoxId, BoxInfo, BoxSpec, Class, ClassSpec, DeviceId, DeviceKind, LifecycleCause, Proc, Program,
    Refusal, Runtime, Signal, Stdio, Usage,
};
use hex_client::guest::{Client, LinkOwner, own_link, stage};
use hex_core::record::{BootSession, BoxRecord, Phase, PoolRecord};

use super::{
    Books, Config, Transaction, exhausted, invalid, observations, single_use, transport, vfio,
};
use crate::devices::{Binding, ManagedLaunch, Route, SharedCatalog, managed_scope_id};

#[path = "pool.rs"]
mod pool;
use crate::guest::rewrite;

/// One initialized Linux KVM backend. The same Node/Store/Turns/Supervisor owns every native
/// receipt and physical claim. This adapter keeps no child table or independent reaper.
pub struct FirecrackerRuntime {
    config: Config,
    catalog: SharedCatalog,
    books: Weak<dyn Books>,
    initialized: std::sync::OnceLock<()>,
    host: vfio::Host,
    /// Where a function no reset clears is marked given on this boot: the node's runtime directory (`Node::oci_root`),
    /// the one every node of its user on the host shares (`/run/hex-node` for root, `$XDG_RUNTIME_DIR/hex-node` for
    /// another user), or, where it is the node's own (a rootless node with no `$XDG_RUNTIME_DIR` keeps it in its own
    /// state), why; such a node gives no such function to a VM, and no node gives one whose VFIO group another user
    /// owns ([`FirecrackerRuntime::unavailable_once`]).
    marks: single_use::MarksAt,
    /// This node's effective user.
    uid: u32,
    /// The host's boot session, the node's (`Node::boot_session`): a function no reset clears goes to one VM per boot.
    boot: Option<BootSession>,
    /// The node's loop (`Node`'s `Host::attach`), which each VM box's evented connection to its guest joins.
    joins: std::sync::Mutex<Option<Arc<dyn crate::guest::Joins>>>,
    /// Each VM box's evented connection to its guest's node, joined to the node's loop: an exec the node forwards there
    /// is answered, and its program's end told, as the guest's messages (AC-T7.4.1). At most one a box, at most
    /// [`crate::MAX_BOXES`], let go with its box.
    evented: std::sync::Mutex<std::collections::BTreeMap<BoxId, crate::guest::EventedClient>>,
}

struct Paths {
    dir: PathBuf,
    config: PathBuf,
    vsock: PathBuf,
    api: PathBuf,
    ready: PathBuf,
}

impl FirecrackerRuntime {
    /// Uses the native service's existing delegated cgroup root and record/attachment turns.
    /// Configuration never binds a host driver, installs a helper or starts a guest here.
    pub fn open(
        config: Config,
        catalog: SharedCatalog,
        books: Weak<dyn Books>,
        marks: single_use::MarksAt,
        boot: Option<BootSession>,
    ) -> Result<Self, Refusal> {
        config.verify_inputs()?;
        Ok(Self {
            config,
            catalog,
            books,
            initialized: std::sync::OnceLock::new(),
            host: vfio::Host::system()?,
            marks,
            uid: rustix::process::geteuid().as_raw(),
            boot,
            joins: std::sync::Mutex::new(None),
            evented: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        })
    }

    /// Called after the native owner's whole retained-state/configuration barrier. This exact
    /// configured adapter publishes no class before these actual native prerequisites succeed.
    pub fn initialize_native(&self) -> Result<(), Refusal> {
        if self.initialized.get().is_some() {
            return Ok(());
        }
        self.config.verify_inputs()?;
        let config = &self.config;
        let capacity = self.authority()?.capacity()?;
        capacity.check()?;
        if !capacity.total.holds(
            config
                .cost
                .allocate(hex_api::CapacityResources::default())?
                .resources,
        ) {
            return Err(invalid(
                "configured minimum VM charge exceeds the physical pool",
            ));
        }
        let _kvm = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .map_err(|error| Refusal::os("opening Firecracker KVM backend", error))?;
        for path in [&config.binary, &config.kernel, &config.initrd] {
            if !fs::metadata(path)
                .map_err(|error| Refusal::os("inspecting Firecracker boot input", error))?
                .is_file()
            {
                return Err(invalid("boot input is not a regular file"));
            }
        }
        fs::create_dir_all(&config.state_dir)
            .and_then(|()| {
                fs::set_permissions(&config.state_dir, fs::Permissions::from_mode(0o700))
            })
            .map_err(|error| Refusal::os("opening private Firecracker state", error))?;
        self.initialized.set(()).or_else(|()| {
            if self.initialized.get().is_some() {
                Ok(())
            } else {
                Err(invalid("Firecracker initialization was not published"))
            }
        })
    }

    fn require_initialized(&self) -> Result<(), Refusal> {
        if self.initialized.get().is_none() {
            return Err(invalid("Firecracker native adapter is not initialized"));
        }
        Ok(())
    }

    fn paths(&self, record: &BoxRecord) -> Result<Paths, Refusal> {
        let scope = managed_scope_id(record)?;
        self.scope_paths(&scope)
    }

    fn scope_paths(&self, scope: &BoxId) -> Result<Paths, Refusal> {
        let dir = path(&self.config.state_dir, scope.as_str())?;
        let vsock = path(&dir, "vsock")?;
        let mut suffix = String::new();
        use std::fmt::Write;
        // Port is the fixed u32 protocol field: decimal needs at most ten digits and an underscore.
        suffix
            .try_reserve_exact(11)
            .map_err(|_| exhausted("readiness suffix bytes", 11))?;
        write!(&mut suffix, "_{}", hex_core::stream::GUEST_READY_PORT)
            .map_err(|_| invalid("readiness suffix cannot be encoded"))?;
        let mut ready = PathBuf::new();
        let room = vsock
            .as_os_str()
            .len()
            .checked_add(suffix.len())
            .ok_or_else(|| invalid("readiness path length overflow"))?;
        ready
            .try_reserve_exact(room)
            .map_err(|_| exhausted("readiness path bytes", room))?;
        ready.push(&vsock);
        ready.as_mut_os_string().push(suffix);
        Ok(Paths {
            config: path(&dir, "config.json")?,
            api: path(&dir, "api")?,
            ready,
            vsock,
            dir,
        })
    }

    fn vm_program(&self, paths: &Paths) -> Result<Program, Refusal> {
        let mut program = Program::default();
        program
            .args
            .try_reserve_exact(6)
            .map_err(|_| exhausted("VMM arguments", 6))?;
        for argument in [
            self.config.binary.as_os_str(),
            std::ffi::OsStr::new("--enable-pci"),
            std::ffi::OsStr::new("--api-sock"),
            paths.api.as_os_str(),
            std::ffi::OsStr::new("--config-file"),
            paths.config.as_os_str(),
        ] {
            program.args.push(text(
                argument
                    .to_str()
                    .ok_or_else(|| invalid("VMM argument is not UTF-8"))?,
            )?);
        }
        Ok(program)
    }

    /// The VMM's stdout and stderr: one pipe, whose reader `transport::ready` alone drains while
    /// create waits for readiness, so a refusal can carry the end of what the VMM wrote.
    fn vm_io() -> Result<(shards_api::LaunchIo, std::io::PipeReader), Refusal> {
        let (output, writer) =
            std::io::pipe().map_err(|e| Refusal::os("opening VMM output pipe", e))?;
        let error = writer
            .try_clone()
            .map_err(|e| Refusal::os("duplicating VMM output pipe", e))?;
        Ok((
            shards_api::LaunchIo {
                stdout: writer,
                stderr: error,
                gpu_report: None,
            },
            output,
        ))
    }

    fn authority(&self) -> Result<Arc<dyn Books>, Refusal> {
        self.books
            .upgrade()
            .ok_or_else(|| Refusal::Broken("native node authority is gone".into()))
    }

    fn record(&self, id: &BoxId) -> Result<BoxRecord, Refusal> {
        crate::devices::with_transaction(&self.books, id, &[], |transaction| {
            transaction
                .read(id)?
                .filter(|r| matches!(r.spec.class, ClassSpec::Firecracker(_)))
                .ok_or_else(|| missing(id))
        })
    }

    /// Keep this exact VM's method/device receipts through protocol setup and reply. Releasing the
    /// method turn after only reading a record would allow delete/recreate at the same socket path.
    fn with_record<T>(
        &self,
        id: &BoxId,
        operation: impl FnOnce(&BoxRecord, &dyn Transaction) -> Result<T, Refusal>,
    ) -> Result<T, Refusal> {
        let before = self.record(id)?;
        let keys = self.catalog.conflict_ids(&before.spec.devices)?;
        crate::devices::with_transaction(&self.books, id, &keys, |transaction| {
            let record = transaction.read(id)?.ok_or_else(|| missing(id))?;
            if record.spec != before.spec
                || record.init != before.init
                || record.native_scope != before.native_scope
                || !matches!(record.spec.class, ClassSpec::Firecracker(_))
            {
                return Err(lifecycle(id, LifecycleCause::Uncertain)?);
            }
            // A box whose backing was retired has no VM to ask (joint plan step 14).
            if record.retired {
                return Err(record.retired_refusal()?);
            }
            operation(&record, transaction)
        })
    }

    fn link(&self, record: &BoxRecord, until: Option<Instant>) -> Result<LinkOwner, Refusal> {
        own_link(transport::Endpoint {
            path: self.paths(record)?.vsock,
            timeout: self.config.operation_timeout,
            create_deadline: until,
        })
    }

    fn native_identity(&self, record: &BoxRecord) -> Result<Option<shards_api::Identity>, Refusal> {
        record
            .init
            .map(|trace| {
                Ok(shards_api::Identity {
                    pid: trace.pid,
                    started: trace.start_ticks,
                    vsock: self.paths(record)?.vsock,
                })
            })
            .transpose()
    }

    fn native(
        &self,
        record: &BoxRecord,
        transaction: &dyn Transaction,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        let identity = self.native_identity(record)?;
        transaction.attach_managed(identity.as_ref())
    }

    fn check_process(
        &self,
        record: &BoxRecord,
        transaction: &dyn Transaction,
    ) -> Result<(), Refusal> {
        let native = self.native(record, transaction)?;
        if native
            .terminal()
            .map_err(|_| uncertain(&record.spec.id))?
            .is_some()
        {
            return Err(Refusal::NotRunning(
                record.spec.id.try_clone()?,
                "guest protocol",
            ));
        }
        Ok(())
    }

    fn client(&self, record: &BoxRecord, transaction: &dyn Transaction) -> Result<Client, Refusal> {
        self.client_before(record, transaction, None)
    }

    fn client_before(
        &self,
        record: &BoxRecord,
        transaction: &dyn Transaction,
        until: Option<Instant>,
    ) -> Result<Client, Refusal> {
        if record.phase != Phase::Ready {
            return Err(lifecycle(&record.spec.id, LifecycleCause::Pending)?);
        }
        self.check_process(record, transaction)?;
        let link = self.link(record, until)?;
        match until {
            Some(until) => Client::connect_before(link, until),
            None => Client::connect(link),
        }
    }

    fn translate(
        &self,
        record: &BoxRecord,
        client: &Client,
        until: Option<Instant>,
    ) -> Result<Vec<DeviceId>, Refusal> {
        let spec = &record.spec;
        let paths = self.paths(record)?;
        if spec.devices.is_empty() {
            return Ok(Vec::new());
        }
        let mut capacity = match until {
            Some(until) => client.capacity_before(Class::Oci, until),
            None => client.capacity(Class::Oci),
        }?
        .ok_or_else(|| invalid("guest has no initialized native device adapter"))?;
        capacity.check()?;
        capacity.devices.sort_unstable_by_key(|device| device.id);
        let mut catalog = Vec::new();
        catalog
            .try_reserve_exact(self.catalog.entries().len())
            .map_err(|_| exhausted("selected VFIO catalog index", self.catalog.entries().len()))?;
        catalog.extend(self.catalog.entries().iter());
        catalog.sort_unstable_by_key(|entry| entry.info.id);
        let mut assignments = Vec::new();
        assignments
            .try_reserve_exact(spec.devices.len())
            .map_err(|_| exhausted("guest PCI assignment witnesses", spec.devices.len()))?;
        let mut translated = Vec::new();
        translated
            .try_reserve_exact(spec.devices.len())
            .map_err(|_| exhausted("guest device assignments", spec.devices.len()))?;
        for selected in &spec.devices {
            let at = catalog
                .binary_search_by_key(selected, |entry| entry.info.id)
                .map_err(|_| invalid("recorded physical binding is absent"))?;
            let entry = catalog
                .get(at)
                .ok_or_else(|| invalid("recorded physical binding is absent"))?;
            let Some(Binding::Vfio { sbdf, .. }) = entry.binding(Route::Vfio) else {
                return Err(invalid("recorded binding is no longer VFIO"));
            };
            let id = vfio::function_id(sbdf)?;
            let witness =
                transport::attachment(&paths.api, &id, self.config.operation_timeout, until)?;
            if witness.id != id || witness.sbdf != *sbdf {
                return Err(invalid(
                    "VMM attachment witness names different host hardware",
                ));
            }
            let guest = hex_gpu::PciAddress::parse(&witness.guest_sbdf)
                .ok_or_else(|| invalid("VMM reported malformed guest PCI identity"))?;
            // A group request is fulfilled by its whole native VMM assignment. The container's
            // GPU claims still name each exact guest function, independently checked by the node.
            if selected.kind == DeviceKind::Gpu {
                let guest_id = DeviceId {
                    kind: DeviceKind::Gpu,
                    key: hex_gpu::Identity::Pci(guest).key(),
                };
                let at = capacity
                    .devices
                    .binary_search_by_key(&guest_id, |offer| offer.id)
                    .map_err(|_| invalid("assigned GPU has no matching guest native attachment"))?;
                let found = capacity.devices.get(at).ok_or_else(|| {
                    invalid("assigned GPU has no matching guest native attachment")
                })?;
                if found.size == 0 {
                    return Err(invalid(
                        "assigned GPU has no current native memory observation",
                    ));
                }
                if found
                    .holder
                    .as_ref()
                    .is_some_and(|holder| holder.box_id() != Some(&spec.id))
                {
                    return Err(lifecycle(&spec.id, LifecycleCause::Uncertain)?);
                }
                translated.push(guest_id);
                assignments.push(observations::Assignment {
                    host: *selected,
                    guest: guest_id,
                });
            }
        }
        // This is the initialized guest Node's SAME native provider, not a second host Gpus open
        // or a catalog substitute. The VMM witness is the only host/guest identity translation.
        let report = match until {
            Some(until) => client.physical_report_before(until),
            None => client.physical_report(),
        }?;
        let observed = observations::join(&assignments, &report)?;
        for actual in observed {
            if catalog
                .binary_search_by_key(&actual.info.id, |entry| entry.info.id)
                .is_err()
            {
                return Err(invalid(
                    "observed GPU is outside the original physical selection",
                ));
            }
            // Model/VRAM/counters are this actual native provider's facts, never admin constants.
        }
        if let Some(until) = until {
            remaining(until)?;
        }
        Ok(translated)
    }

    fn create_inner(
        &self,
        spec: BoxSpec,
        expected_scope: Option<hex_core::record::NativeScope>,
        transaction: &dyn Transaction,
        until: Instant,
    ) -> Result<BoxInfo, Refusal> {
        remaining(until)?;
        self.config.verify_inputs()?;
        let records = transaction.records()?;
        if expected_scope.is_none() && records.len() >= crate::MAX_BOXES {
            return Err(Refusal::Full(crate::MAX_BOXES));
        }
        let pools = transaction.pool_records()?;
        let _selection = self.catalog.prepare(&spec, &records, &pools)?;
        // A VM given devices whole has its VMM pin all its RAM for DMA: refused here, before anything is written or
        // launched, where its memlock can't hold that; else its VMM's soft limit raised to its hard one if need be.
        let memlock = if spec.devices.is_empty() {
            None
        } else {
            let ram = self
                .config
                .machine_memory(&spec)?
                .checked_mul(1 << 20)
                .ok_or_else(|| invalid("guest memory overflow"))?;
            self.host.dma_memlock(ram)?
        };
        let id = spec.id.try_clone()?;
        // The optional handoff already persisted this SAME scope before stopping its observer.
        if let Some(scope) = expected_scope {
            let current = transaction.read(&id)?.ok_or_else(|| uncertain(&id))?;
            if current.spec != spec
                || current.native_scope != Some(scope)
                || current.phase != Phase::Creating
                || current.init.is_some()
            {
                return Err(uncertain(&id));
            }
        } else {
            transaction.put(&BoxRecord::new(spec, Phase::Creating, Vec::new()))?;
        }
        let mut record = transaction.read(&id)?.ok_or_else(|| uncertain(&id))?;
        let plan = vfio::plan(&self.host, &self.catalog, &record.spec.devices, &[])?;
        // A function no reset clears goes to one VM per host boot: refused where any node's VM had it on this one, or
        // where this node can't tell (unavailable_once).
        let functions = vfio::functions(&plan)?;
        let once = vfio::once_per_boot(&self.host, &functions)?;
        if let Some((function, spent)) = self.unavailable_once(&once)? {
            return Err(single_use::refused(function, &spent, self.boot.as_ref()));
        }
        let marks = single_use::Marks::of(&self.marks.dir, self.boot.as_ref())?;
        let paths = self.paths(&record)?;
        match fs::symlink_metadata(&paths.dir) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Refusal::os("checking unclaimed VM path", error)),
            Ok(_) => {
                return Err(invalid(
                    "this native incarnation already has unclaimed VM state",
                ));
            }
        }
        // The VM's directory is its native scope's (paths), which remove and recovery derive again;
        // the record's directories stay as created: the Store keeps them immutable.
        let vm = self.config.machine(&record.spec, &paths.vsock, &plan)?;
        // Prepare the original inner workload and endpoint before creating a native process.
        let mut guest_spec = transaction
            .read(&id)?
            .ok_or_else(|| invalid("Creating record disappeared"))?
            .spec;
        let ClassSpec::Firecracker(workload) = guest_spec.class else {
            return Err(invalid("wrong payload backend"));
        };
        guest_spec.class = ClassSpec::Oci(workload);
        guest_spec.allocation = None;
        guest_spec.route = None;
        let link = own_link(transport::Endpoint {
            path: copy_path(&paths.vsock)?,
            timeout: self.config.operation_timeout,
            create_deadline: Some(until),
        })?;
        fs::create_dir(&paths.dir)
            .and_then(|()| fs::set_permissions(&paths.dir, fs::Permissions::from_mode(0o700)))
            .map_err(|e| Refusal::os("creating private VM directory", e))?;
        write_config(&paths.config, &vm)?;
        let listener = UnixListener::bind(&paths.ready)
            .map_err(|e| Refusal::os("binding private guest readiness endpoint", e))?;
        let program = self.vm_program(&paths)?;
        let (io, mut output) = Self::vm_io()?;
        // Given on this boot before the VMM that opens them starts, so no crash past here forgets it.
        marks.spend(&once)?;
        let native = transaction.launch_managed(
            &ManagedLaunch::Firecracker {
                program: &program,
                vsock: &paths.vsock,
                memlock,
            },
            io,
            remaining(until)?,
        )?;
        // Publication happened while the child was held. Read the SAME immutable scope/spec back.
        let current = transaction
            .read(&id)?
            .ok_or_else(|| invalid("published VM record disappeared"))?;
        if current.spec != record.spec
            || current.native_scope != record.native_scope
            || current.init.is_none()
        {
            return Err(lifecycle(&id, LifecycleCause::Uncertain)?);
        }
        record = current;
        transport::ready(&listener, native.as_ref(), &mut output, until)?;
        // Undrained, the pipe would stall the VMM once full; dropped, its writes fail and are counted.
        drop(output);
        let client = Client::connect_before(link.clone(), until)?;
        guest_spec.devices = self.translate(&record, &client, Some(until))?;
        let memory = self
            .config
            .machine_memory(&record.spec)?
            .checked_mul(1 << 20)
            .ok_or_else(|| invalid("guest memory overflow"))?;
        let rewritten = rewrite::rewrite(guest_spec, &[], stage::STAGED, &rewrite::Known::new())?;
        let mut limits = self.config.stage_limits;
        limits.bytes = limits.bytes.min(memory);
        stage::stage(link.as_ref(), &id, &rewritten.items, &limits)?;
        remaining(until)?;
        let mut info = client.create_before(rewritten.spec, until)?;
        record.phase = Phase::Ready;
        transaction.put(&record)?;
        info.class = record.spec.class.class();
        info.bounds = record.spec.bounds;
        info.allocation = record.spec.allocation;
        info.route = record.spec.route;
        Ok(info)
    }

    /// Stops box `record`'s own VM and releases its scope, in `transaction` (its turn): its VMM stopped, its whole
    /// scope seen released, its exit recorded, its controller finalized and its folder removed. Its phase is the
    /// caller's: a delete's Deleting, a retirement's Ready (joint plan step 14).
    pub(super) fn stop_vm(
        &self,
        record: &mut BoxRecord,
        transaction: &dyn Transaction,
    ) -> Result<(), Refusal> {
        let id = record.spec.id.try_clone()?;
        let paths = self.paths(record)?;
        let identity = self.native_identity(record)?;
        let native = transaction.attach_managed(identity.as_ref())?;
        native.stop().map_err(|_| uncertain(&id))?;
        let end = native
            .wait_for(self.config.stop_timeout)
            .map_err(|_| uncertain(&id))?
            .filter(|end| end.scope == shards_api::TerminalScope::Released)
            .ok_or(lifecycle(&id, LifecycleCause::Uncertain)?)?;
        record.exit = Some(exit(end.status));
        transaction.put(record)?;
        // Persisted exit is not itself absence proof. Finalize checks the same native receipt and
        // actual empty kernel scope before removing it; the record/physical claims still live here.
        transaction.finalize_managed(identity.as_ref())?;
        match fs::remove_dir_all(&paths.dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Refusal::os("removing owned VM state", e)),
        }
        File::open(&self.config.state_dir)
            .and_then(|file| file.sync_all())
            .map_err(|e| Refusal::os("syncing VM state removal", e))
    }

    fn remove(&self, id: &BoxId, transaction: &dyn Transaction) -> Result<(), Refusal> {
        let mut record = transaction
            .read(id)?
            .ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
        if !matches!(record.spec.class, ClassSpec::Firecracker(_)) {
            return Err(invalid("record belongs to another backend"));
        }
        // Its state's place is known before its delete is recorded.
        self.paths(&record)?;
        record.phase = Phase::Deleting;
        transaction.put(&record)?;
        self.stop_vm(&mut record, transaction)?;
        transaction.remove(id)
    }

    /// The first of `once`, functions no reset clears (by SBDF), that can't go to a VM now, and why. Only a node whose
    /// marks every node of its user on the host reads gives one out: one whose runtime directory is its own doesn't,
    /// and none takes a function whose VFIO group another user owns, whose nodes mark it in their own runtime
    /// directory. Then the marks say whether a VM had it on this boot ([`single_use::Marks`]).
    fn unavailable_once<'f>(
        &self,
        once: &[&'f str],
    ) -> Result<Option<(&'f str, single_use::Spent)>, Refusal> {
        let Some(&first) = once.first() else {
            return Ok(None);
        };
        if let Some(why) = &self.marks.unshared {
            return Ok(Some((
                first,
                single_use::Spent::Unshared {
                    dir: copy_path(&self.marks.dir)?,
                    why: text(why)?,
                },
            )));
        }
        for &function in once {
            let owner = vfio::group_owner(&self.host, function)?;
            if owner != self.uid {
                return Ok(Some((
                    function,
                    single_use::Spent::Foreign {
                        owner,
                        uid: self.uid,
                    },
                )));
            }
        }
        single_use::Marks::of(&self.marks.dir, self.boot.as_ref())?.check(once)
    }

    /// Joins `joins`, the node's loop, which each VM box's evented connection to its guest joins from now
    /// ([`FirecrackerRuntime::evented_for`]).
    pub fn join_loop(&self, joins: Arc<dyn crate::guest::Joins>) -> Result<(), Refusal> {
        *self
            .joins
            .lock()
            .map_err(|_| Refusal::Broken("the VM backend's loop lock was poisoned".to_owned()))? =
            Some(joins);
        Ok(())
    }

    /// VM box `id`'s evented connection to its guest's node, joined to the node's loop, for an exec the node forwards
    /// there: its answer and its program's end come as the guest's messages, never as a pid the node watches (the
    /// guest's pid names no process of the host's, or an unrelated one). One whose connection ended while its VM runs
    /// is opened again, the box's record and native receipt checked as any call's are ([`Self::client`]).
    pub fn evented_for(&self, id: &BoxId) -> Result<crate::guest::EventedClient, Refusal> {
        let poisoned = || Refusal::Broken("the VM backend's evented lock was poisoned".to_owned());
        let held = self
            .evented
            .lock()
            .map_err(|_| poisoned())?
            .get(id)
            .filter(|client| !client.closed())
            .cloned();
        if let Some(client) = held {
            return Ok(client);
        }
        let joins = self
            .joins
            .lock()
            .map_err(|_| poisoned())?
            .clone()
            .ok_or_else(|| invalid("the VM box's guest has no node event loop to join"))?;
        let link = self.with_record(id, |record, transaction| {
            if record.phase != Phase::Ready {
                return Err(lifecycle(id, LifecycleCause::Pending)?);
            }
            self.check_process(record, transaction)?;
            self.link(record, None)
        })?;
        let (token, room) = joins.reserve()?;
        let (client, pump) = Client::connect_evented(link, room)?;
        let client = triomphe::Arc::try_new(client)
            .map_err(|_| exhausted("guest evented client owner", crate::MAX_BOXES))?;
        joins.join(token, pump)?;
        let mut evented = self.evented.lock().map_err(|_| poisoned())?;
        evented.retain(|_, held| !held.closed());
        if evented.len() >= crate::MAX_BOXES && !evented.contains_key(id) {
            return Err(Refusal::Full(crate::MAX_BOXES));
        }
        evented.insert(id.try_clone()?, triomphe::Arc::clone(&client));
        Ok(client)
    }

    /// Recovery uses the canonical native scope and VMM witnesses, never another backend or pick.
    pub fn validate_binding(&self, expected: &BoxSpec) -> Result<(), Refusal> {
        let keys = self.catalog.conflict_ids(&expected.devices)?;
        crate::devices::with_transaction(&self.books, &expected.id, &keys, |transaction| {
            let record = transaction
                .read(&expected.id)?
                .ok_or(Refusal::NoSuchBox(expected.id.try_clone()?))?;
            if record.spec != *expected
                || record.phase != Phase::Ready
                || !matches!(record.spec.class, ClassSpec::Firecracker(_))
            {
                return Err(lifecycle(&expected.id, LifecycleCause::Uncertain)?);
            }
            expected.check()?;
            let native = self.native(&record, transaction)?;
            if native
                .terminal()
                .map_err(|_| uncertain(&expected.id))?
                .is_some()
            {
                return Ok(());
            }
            let client = self.client(&record, transaction)?;
            self.translate(&record, &client, None)?;
            let actual = client.inspect(&expected.id)?;
            if actual.class != Class::Oci
                || actual.bounds != expected.bounds
                || actual.allocation.is_some()
            {
                return Err(lifecycle(&expected.id, LifecycleCause::Uncertain)?);
            }
            Ok(())
        })
    }

    /// Pure retained-configuration check for the common all-record preflight. No paths, probes,
    /// protocol clients, native receipts or current capacity are opened here.
    pub fn preflight_record(&self, record: &BoxRecord) -> Result<(), Refusal> {
        record.spec.check()?;
        self.config.check()?;
        // A VM recorded under the census cost, or retired, is its node start's to retire or keep (joint plan step 14).
        if record.retired || self.census_vm(record) {
            return Ok(());
        }
        if record.spec.backend_class()? != Class::Firecracker
            || !matches!(record.spec.class, ClassSpec::Firecracker(_))
            || record.native_scope.is_none()
        {
            return Err(uncertain(&record.spec.id));
        }
        self.config
            .guest(&record.spec)
            .map_err(|_| uncertain(&record.spec.id))?;
        self.config
            .machine_memory(&record.spec)
            .map_err(|_| uncertain(&record.spec.id))?;
        let paths = self.paths(record)?;
        if record.dirs.iter().any(|owned| owned != &paths.dir) {
            return Err(uncertain(&record.spec.id));
        }
        Ok(())
    }

    /// Read-only no-submission proof under the SAME canonical source/Box/full-P guard. This is
    /// not native absence proof: after source Released, ordinary scope cleanup remains mandatory.
    pub fn handoff_unsubmitted(
        &self,
        record: &BoxRecord,
        source: &PoolRecord,
    ) -> Result<(), Refusal> {
        self.preflight_record(record)?;
        self.preflight_backing(source)?;
        let id = &record.spec.id;
        let route = record.spec.route.ok_or_else(|| uncertain(id))?;
        if !matches!(record.phase, Phase::Creating | Phase::Deleting)
            || record.init.is_some()
            || record.run.is_some()
            || record.exit.is_some()
            || !record.dirs.is_empty()
            || route.key.execution != hex_api::Execution::Firecracker
            || route.backing != Some(source.key)
            || !matches!(
                source.phase,
                hex_api::BackingPhase::Live | hex_api::BackingPhase::Draining
            )
        {
            return Err(uncertain(id));
        }
        route.check_backing_parts(
            record.spec.allocation.map(|allocation| allocation.cost),
            source.key,
            source.cost,
            &source.devices,
            &record.spec.devices,
        )?;
        record.native_commitment()?;
        source.native_commitment()?;
        // The canonical dirs write precedes private directory creation and native launch.
        // A collision or an unread path cannot prove that this incarnation was never submitted.
        match fs::symlink_metadata(self.paths(record)?.dir) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Refusal::os(
                "checking unsubmitted Firecracker incarnation",
                error,
            )),
            Ok(_) => Err(uncertain(id)),
        }
    }

    fn create_handoff(
        &self,
        spec: BoxSpec,
        key: hex_api::BackingKey,
        until: Instant,
    ) -> Result<BoxInfo, Refusal> {
        let source = self
            .authority()?
            .pool_records()?
            .into_iter()
            .find(|pool| pool.key == key)
            .ok_or(Refusal::Backing {
                key,
                cause: LifecycleCause::Uncertain,
            })?;
        self.preflight_backing(&source)?;
        // This is an exact isolation/identity check, not another picker. Refuse an inseparable
        // unselected GPU before draining the observer; companion metadata cannot enlarge S.
        // The source's observer holds the full set's groups until it is drained below.
        let plan = vfio::plan(
            &self.host,
            &self.catalog,
            &spec.devices,
            source.init.as_slice(),
        )?;
        // A function no reset clears that the observer had on this boot can't go to a box before the host reboots:
        // the pool says so before its observer is drained.
        let functions = vfio::functions(&plan)?;
        let once = vfio::once_per_boot(&self.host, &functions)?;
        if let Some((function, spent)) = self.unavailable_once(&once)? {
            return Err(single_use::pool_refused(
                "a GPU pool's handoff to a box (VFIO)",
                &key,
                function,
                &spent,
                self.boot.as_ref(),
            ));
        }
        drop(plan);
        let keys = self.catalog.conflict_ids(&source.devices)?;
        let id = spec.id.try_clone()?;
        let (pending, commitment, init) = crate::devices::with_handoff_transaction(
            &self.books,
            &id,
            &key,
            &keys,
            |transaction| {
                let source = transaction.read_pool(&key)?.ok_or_else(|| uncertain(&id))?;
                self.preflight_backing(&source)?;
                if source.phase != hex_api::BackingPhase::Live {
                    return Err(Refusal::Backing {
                        key,
                        cause: LifecycleCause::Pending,
                    });
                }
                if transaction.read(&id)?.is_some() {
                    return Err(Refusal::Exists(id.try_clone()?));
                }
                let records = transaction.records()?;
                if records.len() >= crate::MAX_BOXES {
                    return Err(Refusal::Full(crate::MAX_BOXES));
                }
                let pools = transaction.pool_records()?;
                self.catalog.prepare(&spec, &records, &pools)?;
                let commitment = source.native_commitment()?;
                transaction.put(&BoxRecord::new(spec, Phase::Creating, Vec::new()))?;
                let pending = transaction.read(&id)?.ok_or_else(|| uncertain(&id))?;
                self.handoff_unsubmitted(&pending, &source)?;
                Ok((pending, commitment, source.init))
            },
        )?;
        // No Box or device turn is held while the SAME owner drains the original full-P scope.
        // Controller exit alone never produces this positive whole-scope Released receipt.
        let released = self.release_observer_before(&key, Some(until))?;
        remaining(until)?;
        crate::devices::with_handoff_transaction(&self.books, &id, &key, &keys, |transaction| {
            let source = transaction.read_pool(&key)?.ok_or_else(|| uncertain(&id))?;
            self.preflight_backing(&source)?;
            if source.phase != hex_api::BackingPhase::Released
                || source.exit.is_none()
                || source.native_commitment()? != commitment
                || source.init != init
                || released.key != key
                || released.phase != hex_api::BackingPhase::Released
                || released.cost != source.cost
                || released.devices != source.devices
            {
                return Err(uncertain(&id));
            }
            if transaction.read(&id)?.as_ref() != Some(&pending) {
                return Err(uncertain(&id));
            }
            let scope = pending.native_scope.ok_or_else(|| uncertain(&id))?;
            self.create_or_cleanup(&id, pending.spec, Some(scope), transaction, until)
        })
    }

    fn create_or_cleanup(
        &self,
        id: &BoxId,
        spec: BoxSpec,
        scope: Option<hex_core::record::NativeScope>,
        transaction: &dyn Transaction,
        until: Instant,
    ) -> Result<BoxInfo, Refusal> {
        let refusal = match self.create_inner(spec, scope, transaction, until) {
            Ok(info) => return Ok(info),
            Err(refusal) => refusal,
        };
        // The create's refusal is kept whatever its cleanup meets: a cleanup that fails leaves the box uncertain, and
        // its refusal carries both causes, the create's first (AC-T7.4.1).
        let cleanup = transaction.read(id).and_then(|record| match record {
            Some(_) => self.remove(id, transaction),
            None => Ok(()),
        });
        match cleanup {
            Ok(()) => Err(refusal),
            Err(cleanup) => Err(transport::uncertain_after(
                id.try_clone()?,
                refusal,
                cleanup,
            )),
        }
    }

    fn route_key(&self, gpu: hex_api::GpuEligibility) -> Result<hex_api::RouteKey, Refusal> {
        let arch = match std::env::consts::ARCH {
            "x86_64" => hex_api::ImageArch::Amd64,
            "aarch64" => hex_api::ImageArch::Arm64,
            _ => {
                return Err(invalid(
                    "native architecture is outside the supported Firecracker targets",
                ));
            }
        };
        let key = hex_api::RouteKey {
            class: Class::Firecracker,
            image: hex_api::ImagePlatform {
                os: hex_api::ImageOs::Linux,
                arch,
            },
            execution: hex_api::Execution::Firecracker,
            gpu,
        };
        key.check()?;
        Ok(key)
    }

    /// Actual initialized fixed prefix and canonical row forms, without capacity/provider/Books
    /// reentry. The same key constructor drives current_capacity below.
    pub fn project_routes(
        &self,
        pools: &[PoolRecord],
        introduced: Option<(hex_api::Execution, &[DeviceId])>,
        projection: &mut crate::devices::RouteProjection,
    ) -> Result<(), Refusal> {
        self.require_initialized()?;
        projection.retain(self.route_key(hex_api::GpuEligibility::None)?, None)?;
        for record in pools
            .iter()
            .filter(|record| record.execution == hex_api::Execution::Firecracker)
        {
            self.preflight_backing(record)?;
            projection.retain(
                self.route_key(hex_api::GpuEligibility::Required)?,
                Some(record.key),
            )?;
        }
        if let Some((hex_api::Execution::Firecracker, devices)) = introduced {
            hex_api::BackingOffer::check_selection_parts(&self.observer_claims()?, devices)?;
            // The caller's absent-target proof supplies lifetime distinctness; no generation is
            // guessed for this projection. The canonical row is created only after it passes.
            projection.introduce(self.route_key(hex_api::GpuEligibility::Required)?)?;
        }
        Ok(())
    }

    fn current_capacity(
        &self,
        class: Class,
        until: Option<Instant>,
    ) -> Result<Option<hex_api::Capacity>, Refusal> {
        self.require_initialized()?;
        if class != Class::Firecracker {
            return Err(invalid("capacity requested for another backend"));
        }
        self.prepare_backing()?;
        let authority = self.authority()?;
        let records = authority.records()?;
        let pools = authority.pool_records()?;
        let mut capacity = authority.capacity()?;
        let fresh = self.fresh_observer(&records, &pools)?;
        let guest_observed = match self.physical_observations_before(until) {
            Ok(observed) => observed,
            Err(why) => {
                // Current typed diagnostics are reported independently by the SAME native
                // PhysicalReport owner. A qualified exact-key observation failure cannot erase
                // the first canonical resource/claim topology or invent fresh facts.
                let unavailable =
                    hex_api::BackendUnavailable::from_refusal(Class::Firecracker, why)?;
                let Refusal::Backing { key, .. } = unavailable.cause else {
                    return Err(invalid("observer failure lost its canonical source key"));
                };
                if !pools.iter().any(|pool| {
                    pool.key == key && pool.execution == hex_api::Execution::Firecracker
                }) {
                    return Err(invalid(
                        "observer failure names another configured authority",
                    ));
                }
                Vec::new()
            }
        };
        let mut observed: Vec<hex_api::DeviceInfo> = Vec::new();
        observed
            .try_reserve_exact(guest_observed.len())
            .map_err(|_| exhausted("current observer facts", guest_observed.len()))?;
        observed.extend(guest_observed.into_iter().map(|actual| actual.info));
        observed.sort_unstable_by_key(|actual| actual.id);
        // A group one of this node's own VMMs holds is in use by its VM, never opened beside it (vfio::validate).
        let vmms = recorded_vmms(&records, &pools)?;
        capacity.cost = None;
        capacity.routes.clear();
        capacity.device_routes.clear();
        capacity.backings.clear();
        capacity.devices.clear();
        capacity
            .devices
            .try_reserve_exact(self.catalog.entries().len())
            .map_err(|_| exhausted("VFIO capacity", self.catalog.entries().len()))?;
        // A function no reset clears that can't go to a VM now (unavailable_once) is listed, never offered:
        // unavailable, with why.
        let mut unavailable = Vec::new();
        unavailable
            .try_reserve_exact(self.catalog.entries().len())
            .map_err(|_| exhausted("unavailable VFIO capacity", self.catalog.entries().len()))?;
        for entry in self
            .catalog
            .entries()
            .iter()
            .filter(|entry| entry.binding(Route::Vfio).is_some())
        {
            let device = if entry.info.id.kind == DeviceKind::Gpu {
                // An administrator's catalog is not current VRAM/load evidence. Prebound VFIO
                // may hide host NVML: only the same initialized facts provider may supply this
                // actual identity/metadata observation (including a witnessed live guest).
                let Some(actual) = observed
                    .binary_search_by_key(&entry.info.id, |actual| actual.id)
                    .ok()
                    .and_then(|at| observed.get(at))
                else {
                    continue;
                };
                vfio::validate(&self.host, entry, &vmms)?;
                let mut actual = actual.try_clone()?;
                actual.holder = self.catalog.holder(entry.info.id, &records, &pools)?;
                actual
            } else {
                vfio::validate(&self.host, entry, &vmms)?;
                entry.report(self.catalog.holder(entry.info.id, &records, &pools)?)?
            };
            // A box holding it keeps it as its own; anything else, a pool's observer included, may not offer it on.
            if !matches!(device.holder, Some(hex_api::DeviceHolder::Box(_))) {
                let declared = vfio::declared(entry)?;
                let once = vfio::once_per_boot(&self.host, &declared)?;
                if let Some((function, spent)) = self.unavailable_once(&once)? {
                    unavailable.push(hex_api::DeviceUnavailable {
                        id: device.id,
                        why: single_use::reason(function, &spent, self.boot.as_ref()),
                    });
                }
            }
            capacity.devices.push(device);
        }
        let count = pools
            .iter()
            .filter(|pool| pool.execution == hex_api::Execution::Firecracker)
            .count();
        let routes = count
            .checked_add(1)
            .filter(|count| *count <= hex_api::MAX_RUNTIME_ROUTES)
            .ok_or_else(|| {
                exhausted(
                    "Firecracker initialized routes",
                    hex_api::MAX_RUNTIME_ROUTES,
                )
            })?;
        capacity
            .routes
            .try_reserve_exact(routes)
            .map_err(|_| exhausted("Firecracker initialized routes", routes))?;
        capacity
            .backings
            .try_reserve_exact(count)
            .map_err(|_| exhausted("Firecracker backing receipts", count))?;
        capacity
            .device_routes
            .try_reserve_exact(capacity.devices.len())
            .map_err(|_| exhausted("Firecracker device eligibility", capacity.devices.len()))?;
        capacity.routes.push(hex_api::RouteOffer {
            key: self.route_key(hex_api::GpuEligibility::None)?,
            cost: Some(self.config.cost),
            backing: None,
        });
        for pool in pools
            .iter()
            .filter(|pool| pool.execution == hex_api::Execution::Firecracker)
        {
            self.preflight_backing(pool)?;
            capacity.routes.push(hex_api::RouteOffer {
                key: self.route_key(hex_api::GpuEligibility::Required)?,
                cost: Some(self.config.cost),
                backing: Some(pool.key),
            });
            capacity.backings.push(pool.offer()?);
        }
        let fresh = fresh
            .map(|key| {
                let source = pools
                    .iter()
                    .find(|pool| pool.key == key)
                    .ok_or_else(|| invalid("fresh observer source vanished within snapshot"))?;
                let at = capacity
                    .routes
                    .iter()
                    .position(|route| route.backing == Some(key))
                    .ok_or_else(|| invalid("fresh observer route vanished within snapshot"))?;
                let bit = u32::try_from(at)
                    .ok()
                    .and_then(|at| 1u64.checked_shl(at))
                    .ok_or_else(|| {
                        exhausted("Firecracker route mask", hex_api::MAX_RUNTIME_ROUTES)
                    })?;
                Ok::<_, Refusal>((source, bit))
            })
            .transpose()?;
        for device in &capacity.devices {
            let mut word = if device.id.kind == DeviceKind::Gpu {
                0u64
            } else {
                1u64
            };
            if let Some((source, bit)) = fresh
                && (device.id.kind != DeviceKind::Gpu
                    || source.devices.binary_search(&device.id).is_ok())
                && !unavailable.iter().any(|spent| spent.id == device.id)
            {
                word |= bit;
            }
            capacity.device_routes.push(word);
        }
        capacity.unavailable = unavailable;
        capacity.check()?;
        Ok(Some(capacity))
    }

    pub fn recover(&self) -> Result<(), Refusal> {
        for (id, record) in self.authority()?.records()? {
            let record = record.map_err(|_| uncertain(&id))?;
            if !matches!(record.spec.class, ClassSpec::Firecracker(_)) {
                continue;
            }
            match record.phase {
                Phase::Ready => self.validate_binding(&record.spec)?,
                Phase::Creating | Phase::Deleting => self.delete(&id)?,
            }
        }
        Ok(())
    }
}

/// The native identities of this node's VMMs as its records name them: each Firecracker VM's and pool observer's
/// that launched.
fn recorded_vmms(
    records: &crate::devices::Records,
    pools: &[PoolRecord],
) -> Result<Vec<hex_core::record::ProcId>, Refusal> {
    let count = records
        .len()
        .checked_add(pools.len())
        .ok_or_else(|| exhausted("recorded VMMs", crate::MAX_BOXES))?;
    let mut vmms = Vec::new();
    vmms.try_reserve_exact(count)
        .map_err(|_| exhausted("recorded VMMs", count))?;
    for record in records.values().filter_map(|record| record.as_ref().ok()) {
        if matches!(record.spec.class, ClassSpec::Firecracker(_))
            && let Some(init) = record.init
        {
            vmms.push(init);
        }
    }
    for pool in pools
        .iter()
        .filter(|pool| pool.execution == hex_api::Execution::Firecracker)
    {
        if let Some(init) = pool.init {
            vmms.push(init);
        }
    }
    Ok(vmms)
}

fn remaining(until: Instant) -> Result<std::time::Duration, Refusal> {
    let left = until.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err(Refusal::os(
            "creating Firecracker VM",
            "whole create deadline elapsed",
        ))
    } else {
        Ok(left)
    }
}

fn missing(id: &BoxId) -> Refusal {
    match id.try_clone() {
        Ok(id) => Refusal::NoSuchBox(id),
        Err(error) => error,
    }
}

fn uncertain(id: &BoxId) -> Refusal {
    match id.try_clone() {
        Ok(id) => Refusal::Lifecycle {
            id,
            cause: LifecycleCause::Uncertain,
        },
        Err(error) => error,
    }
}

fn lifecycle(id: &BoxId, cause: LifecycleCause) -> Result<Refusal, Refusal> {
    Ok(Refusal::Lifecycle {
        id: id.try_clone()?,
        cause,
    })
}

impl Runtime for FirecrackerRuntime {
    fn classes(&self) -> Vec<Class> {
        if self.initialized.get().is_some() {
            vec![Class::Firecracker]
        } else {
            Vec::new()
        }
    }
    fn initialize_backing(
        &self,
        grant: hex_api::BackingGrant,
    ) -> Result<hex_api::BackingOffer, Refusal> {
        self.require_initialized()?;
        self.initialize_observer(grant)
    }
    fn release_backing(&self, key: hex_api::BackingKey) -> Result<hex_api::BackingOffer, Refusal> {
        self.require_initialized()?;
        // A cleanup response must not also rearm/remove its generation. The driver's separate
        // explicit call after BOTH Index views consumed Released is the metadata ACK phase.
        if self
            .observer_records()?
            .iter()
            .any(|record| record.key == key && record.phase == hex_api::BackingPhase::Released)
        {
            self.acknowledge_observer(&key)
        } else {
            self.release_observer(&key)
        }
    }
    fn backing_usage(&self, key: &hex_api::BackingKey) -> Result<Usage, Refusal> {
        self.require_initialized()?;
        self.observer_usage(key)
    }

    fn capacity(&self, class: Class) -> Result<Option<hex_api::Capacity>, Refusal> {
        self.current_capacity(class, None)
    }

    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        self.require_initialized()?;
        let until = Instant::now()
            .checked_add(self.config.boot_timeout)
            .ok_or_else(|| invalid("whole create deadline is unrepresentable"))?;
        spec.check()?;
        if !matches!(spec.class, ClassSpec::Firecracker(_)) {
            return Err(invalid(
                "endpoint only executes explicit Firecracker payloads",
            ));
        }
        if spec.route.is_some() {
            self.current_capacity(Class::Firecracker, Some(until))?
                .ok_or_else(|| invalid("Firecracker endpoint has no initialized capacity"))?
                .validate_route(&spec)?;
        }
        remaining(until)?;
        if let Some(key) = spec.route.and_then(|route| route.backing) {
            return self.create_handoff(spec, key, until);
        }
        let keys = self.catalog.conflict_ids(&spec.devices)?;
        let id = spec.id.try_clone()?;
        crate::devices::with_transaction(&self.books, &id, &keys, |transaction| {
            if transaction.read(&id)?.is_some() {
                return Err(Refusal::Exists(id.try_clone()?));
            }
            self.create_or_cleanup(&id, spec, None, transaction, until)
        })
    }

    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.with_record(id, |record, transaction| {
            self.client(record, transaction)?.start(id)
        })
    }
    fn exec(&self, id: &BoxId, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.with_record(id, |record, transaction| {
            self.client(record, transaction)?.exec(id, program, stdio)
        })
    }
    fn run(
        &self,
        spec: BoxSpec,
        _program: Program,
        _stdio: Stdio,
    ) -> Result<Box<dyn Proc>, Refusal> {
        spec.check_run()?;
        Err(invalid(
            "Firecracker init is created and started explicitly",
        ))
    }
    fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
        self.with_record(id, |record, transaction| {
            self.client(record, transaction)?.signal(id, signal)
        })
    }
    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        // A box whose backing was retired says why from its record (joint plan step 14).
        let record = self.record(id)?;
        if record.retired {
            return record.retired_info();
        }
        self.with_record(id, |record, transaction| {
            if record.phase == Phase::Ready
                && let Some(end) = self
                    .native(record, transaction)?
                    .terminal()
                    .map_err(|_| uncertain(id))?
            {
                return Ok(BoxInfo {
                    published: Vec::new(),
                    grants: Vec::new(),
                    id: id.try_clone()?,
                    class: record.spec.class.class(),
                    state: hex_api::BoxState::Empty,
                    bounds: record.spec.bounds.clone(),
                    allocation: record.spec.allocation,
                    route: record.spec.route,
                    adopted: false,
                    membership: None,
                    exit: Some(exit(end.status)),
                    abandoned: record.abandoned,
                    last_resort: None,
                    ended: None,
                    credentials: Vec::new(),
                });
            }
            let mut info = self.client(record, transaction)?.inspect(id)?;
            info.class = record.spec.class.class();
            info.bounds = record.spec.bounds.clone();
            info.allocation = record.spec.allocation;
            info.route = record.spec.route;
            Ok(info)
        })
    }
    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.with_record(id, |record, transaction| {
            let _native = self.native(record, transaction)?;
            transaction.managed_usage()
        })
    }
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        let authority = self.authority()?;
        let records = authority.records()?;
        let mut boxes = Vec::new();
        boxes
            .try_reserve_exact(records.len())
            .map_err(|_| exhausted("VM list", records.len()))?;
        for (id, record) in records {
            let record = record.map_err(|_| {
                Refusal::Broken("unreadable native record prevents listing VMs".into())
            })?;
            if matches!(record.spec.class, ClassSpec::Firecracker(_)) {
                boxes.push(self.inspect(&id)?);
            }
        }
        Ok(boxes)
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        // Its evented connection goes with it: a program forwarded there ends with its VM.
        self.evented
            .lock()
            .map_err(|_| Refusal::Broken("the VM backend's evented lock was poisoned".to_owned()))?
            .remove(id);
        let record = self.record(id)?;
        // A box whose backing was retired is gone with it: its delete removes its record alone (joint plan step 14).
        if record.retired {
            return crate::devices::with_transaction(&self.books, id, &[], |transaction| {
                transaction.remove(id)
            });
        }
        if let Some(key) = record.spec.route.and_then(|route| route.backing) {
            let source = self
                .authority()?
                .pool_records()?
                .into_iter()
                .find(|pool| pool.key == key)
                .ok_or_else(|| uncertain(id))?;
            if matches!(
                source.phase,
                hex_api::BackingPhase::Live | hex_api::BackingPhase::Draining
            ) {
                let keys = self.catalog.conflict_ids(&source.devices)?;
                return crate::devices::with_handoff_transaction(
                    &self.books,
                    id,
                    &key,
                    &keys,
                    |transaction| {
                        let source = transaction.read_pool(&key)?.ok_or_else(|| uncertain(id))?;
                        let mut current = transaction.read(id)?.ok_or_else(|| missing(id))?;
                        self.handoff_unsubmitted(&current, &source)?;
                        current.phase = Phase::Deleting;
                        transaction.put(&current)?;
                        // No Box native scope was submitted. Original full-P scope/claims remain
                        // owned by its PoolRecord; cancellation removes only this consuming reference.
                        transaction.remove(id)
                    },
                );
            }
        }
        let keys = self.catalog.conflict_ids(&record.spec.devices)?;
        crate::devices::with_transaction(&self.books, id, &keys, |transaction| {
            self.remove(id, transaction)
        })
    }
}

fn exit(status: shards_api::ManagedStatus) -> hex_api::Exit {
    match status {
        shards_api::ManagedStatus::Code(code) => hex_api::Exit::Code(code),
        shards_api::ManagedStatus::Signal(signal) => hex_api::Exit::Signal(signal),
        shards_api::ManagedStatus::Unknown => hex_api::Exit::Unknown,
    }
}

pub(super) fn text(value: &str) -> Result<String, Refusal> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| exhausted("VMM argument bytes", value.len()))?;
    owned.push_str(value);
    Ok(owned)
}

fn copy_path(value: &Path) -> Result<PathBuf, Refusal> {
    let mut owned = PathBuf::new();
    owned
        .try_reserve_exact(value.as_os_str().len())
        .map_err(|_| exhausted("VM path bytes", value.as_os_str().len()))?;
    owned.push(value);
    Ok(owned)
}

pub(super) fn path(parent: &Path, child: &str) -> Result<PathBuf, Refusal> {
    let room = parent
        .as_os_str()
        .len()
        .checked_add(child.len())
        .and_then(|n| n.checked_add(1))
        .ok_or_else(|| invalid("VM path length overflow"))?;
    let mut owned = PathBuf::new();
    owned
        .try_reserve_exact(room)
        .map_err(|_| exhausted("VM path bytes", room))?;
    owned.push(parent);
    owned.push(child);
    Ok(owned)
}

fn write_config(path: &Path, value: &impl serde::Serialize) -> Result<(), Refusal> {
    struct Buffer(Vec<u8>);
    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self
                .0
                .len()
                .checked_add(bytes.len())
                .is_none_or(|n| n > hex_core::bundle::MAX_CONFIG_BYTES)
            {
                return Err(std::io::Error::other("VM config exceeds native byte bound"));
            }
            self.0
                .try_reserve_exact(bytes.len())
                .map_err(std::io::Error::other)?;
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut buffer = Buffer(Vec::new());
    serde_json::to_writer(&mut buffer, value).map_err(|e| Refusal::os("encoding VM config", e))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| Refusal::os("creating VM config", e))?;
    file.write_all(&buffer.0)
        .and_then(|()| file.sync_all())
        .map_err(|e| Refusal::os("writing VM config", e))?;
    let parent = path
        .parent()
        .ok_or_else(|| invalid("VM config has no directory"))?;
    File::open(parent)
        .and_then(|file| file.sync_all())
        .map_err(|e| Refusal::os("syncing VM config directory", e))
}

#[cfg(test)]
mod io_tests {
    use super::*;
    use std::os::fd::AsRawFd;

    /// The descriptors this process holds on `pipe`'s write end now: its `/proc/self/fd` entries naming the same pipe
    /// (`pipe:[<inode>]`) open for writing (their fdinfo's `flags`, O_ACCMODE; proc(5)). By the pipe, not by number,
    /// so another file under a closed number is no write end; by this process's own table, so a child another test
    /// forks, holding a copy until its exec, is never counted.
    fn write_ends(pipe: &impl std::os::fd::AsFd) -> Vec<String> {
        let target = format!("pipe:[{}]", rustix::fs::fstat(pipe).unwrap().st_ino);
        let mut ends = Vec::new();
        for entry in fs::read_dir("/proc/self/fd").unwrap() {
            let entry = entry.unwrap();
            // A descriptor another test closed since the listing names nothing now.
            let Ok(link) = fs::read_link(entry.path()) else {
                continue;
            };
            let fd = entry.file_name().into_string().unwrap();
            let Ok(info) = fs::read_to_string(format!("/proc/self/fdinfo/{fd}")) else {
                continue;
            };
            let flags = info
                .lines()
                .find_map(|line| line.strip_prefix("flags:"))
                .map(|flags| u32::from_str_radix(flags.trim(), 8).unwrap())
                .unwrap();
            if link.as_os_str() == target.as_str() && flags & 0o3 == 0o1 {
                ends.push(fd);
            }
        }
        ends.sort();
        ends
    }

    /// AC-T7.4.1: configured VMM output must accept both streams, and both exact owned
    /// descriptors close on drop: no descriptor of this process holds their pipe's write end
    /// after, and its reader reads what they wrote. The end is told by the pipe, which no
    /// descriptor number can fake (red at 37dc9d9f: another file took a closed number), in this
    /// process's own table: a read to the pipe's end also waited for a child another test had
    /// forked, holding the write end until its exec (WouldBlock under this binary's parallel
    /// tests). No Config, guest, process, cgroup, KVM or GPU is created.
    #[test]
    fn configured_vmm_output_sinks_accept_bytes_and_close_on_drop() {
        use std::io::Read;
        let (
            shards_api::LaunchIo {
                mut stdout,
                mut stderr,
                gpu_report,
            },
            mut reader,
        ) = FirecrackerRuntime::vm_io().unwrap();
        assert!(gpu_report.is_none());
        let descriptors = [stdout.as_raw_fd(), stderr.as_raw_fd()];
        assert_ne!(descriptors[0], descriptors[1]);
        let mut sinks = descriptors.map(|fd| fd.to_string());
        sinks.sort();
        assert_eq!(
            write_ends(&reader),
            sinks,
            "the sinks are the pipe's two write ends"
        );
        stdout.write_all(b"configured-vmm-stdout\n").unwrap();
        stderr.write_all(b"configured-vmm-stderr\n").unwrap();
        drop(stdout);
        drop(stderr);
        // Another test's descriptor taking a closed number, as one may at any moment among this
        // binary's parallel tests: dupfd returns the lowest free number from the one given.
        let other = fs::File::open("/dev/null").unwrap();
        let _reuse = rustix::io::fcntl_dupfd_cloexec(&other, descriptors[0]).unwrap();
        assert_eq!(write_ends(&reader), Vec::<String>::new(), "closed on drop");
        let wrote = b"configured-vmm-stdout\nconfigured-vmm-stderr\n";
        let mut read = [0u8; 44];
        reader.read_exact(&mut read).unwrap();
        assert_eq!(&read, wrote);
    }

    /// A stand-in VMM: a real child process, observed as the native receipt observes one. Its start
    /// time is read while it waits on its gate, its stdin, as the node's launcher reads a VMM's while
    /// it is held before its exec (hex-node `ManagedVm::observe_identity`): the read never races its end.
    struct StandIn {
        identity: shards_api::Identity,
        child: std::sync::Mutex<std::process::Child>,
    }

    /// A stand-in's run from its gate to its end: a program this test runs as its own child.
    static STAND_IN_ENDS: hex_core::patience::Patience = hex_core::patience::Patience::seeded(
        "test.hex_runtime.firecracker.stand_in_ends",
        hex_testkit::waits::PROGRAM,
    );

    /// What a VMM whose DMA map RLIMIT_MEMLOCK refused writes as it ends.
    const MEMLOCK: &str = "vfio: RLIMIT_MEMLOCK (8388608) exceeded";

    impl StandIn {
        /// Starts `script` on `stdout` and `stderr`, held until a line comes on the gate it returns.
        fn held(
            script: &str,
            stdout: std::io::PipeWriter,
            stderr: std::io::PipeWriter,
        ) -> (Self, std::process::ChildStdin) {
            let mut child = std::process::Command::new("/bin/sh")
                .args(["-c", &format!("read -r gate; {script}")])
                .stdin(std::process::Stdio::piped())
                .stdout(stdout)
                .stderr(stderr)
                .spawn()
                .unwrap();
            let gate = child.stdin.take().unwrap();
            let pid = child.id();
            let started =
                crate::linux::oci::state::start_time(i32::try_from(pid).unwrap()).unwrap();
            let vmm = Self {
                identity: shards_api::Identity {
                    pid,
                    started,
                    vsock: PathBuf::from("/nonexistent/stand-in.vsock"),
                },
                child: std::sync::Mutex::new(child),
            };
            (vmm, gate)
        }

        /// Starts `script` held, and lets it run once its start time is read.
        fn start(script: &str, stdout: std::io::PipeWriter, stderr: std::io::PipeWriter) -> Self {
            let (vmm, mut gate) = Self::held(script, stdout, stderr);
            gate.write_all(b"\n").unwrap();
            vmm
        }

        fn pid(&self) -> rustix::process::Pid {
            rustix::process::Pid::from_raw(i32::try_from(self.identity.pid).unwrap()).unwrap()
        }

        /// Lets it run, and waits until it has ended, unreaped: `waitid` with `WNOWAIT` sees its end
        /// and leaves it a zombie.
        fn end(&self, mut gate: std::process::ChildStdin) {
            gate.write_all(b"\n").unwrap();
            drop(gate);
            let pid = self.pid();
            hex_testkit::waits::until(&STAND_IN_ENDS, "the stand-in VMM's end", || {
                rustix::process::waitid(
                    rustix::process::WaitId::Pid(pid),
                    rustix::process::WaitIdOptions::EXITED
                        | rustix::process::WaitIdOptions::NOWAIT
                        | rustix::process::WaitIdOptions::NOHANG,
                )
                .unwrap()
                .is_some()
            })
            .unwrap();
        }

        /// Has its receipt collect its end, as the node's supervisor collects an ended VMM's at once
        /// (its pidfd turns readable in the supervisor's epoll set: hex-node supervise.rs `ready`,
        /// `ManagedVm::collect`); then no child of this process has its pid.
        fn reap(&self) {
            assert!(
                shards_api::ManagedProcess::terminal(self)
                    .unwrap()
                    .is_some()
            );
            let left = rustix::process::waitid(
                rustix::process::WaitId::Pid(self.pid()),
                rustix::process::WaitIdOptions::EXITED
                    | rustix::process::WaitIdOptions::NOWAIT
                    | rustix::process::WaitIdOptions::NOHANG,
            );
            assert!(
                matches!(left, Err(rustix::io::Errno::CHILD)),
                "reaped: {left:?}"
            );
        }
    }

    /// Waits for `vmm`'s guest readiness as create does, on an endpoint no guest reaches, within the
    /// create's deadline as `create` takes it, its config's boot deadline.
    fn readiness(vmm: &StandIn, output: &mut std::io::PipeReader) -> Result<(), Refusal> {
        let dir = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(dir.path().join("ready")).unwrap();
        let until = Instant::now() + pool::tests::config().boot_timeout;
        transport::ready(&listener, vmm, output, until)
    }

    impl shards_api::ManagedProcess for StandIn {
        fn identity(&self) -> Result<&shards_api::Identity, shards_api::Refusal> {
            Ok(&self.identity)
        }
        fn wait_for(
            &self,
            _: std::time::Duration,
        ) -> Result<Option<shards_api::ManagedEnd>, shards_api::Refusal> {
            self.terminal()
        }
        fn stop(&self) -> Result<(), shards_api::Refusal> {
            self.child.lock().unwrap().kill().unwrap();
            Ok(())
        }
        fn terminal(&self) -> Result<Option<shards_api::ManagedEnd>, shards_api::Refusal> {
            let status = self.child.lock().unwrap().try_wait().unwrap();
            Ok(status.map(|status| shards_api::ManagedEnd {
                status: shards_api::ManagedStatus::Code(status.code().unwrap_or(-1)),
                scope: shards_api::TerminalScope::Released,
            }))
        }
    }

    /// AC-T7.4.1: original. A VMM that says why it stops and ends before its guest is ready (as
    /// a DMA map refused by RLIMIT_MEMLOCK ends one), ended before readiness first looks at it, its
    /// end not yet collected. Do: start a stand-in on the configured output sinks, its start time
    /// read while it is held; let it write and end, and see it a zombie (`waitid` with `WNOWAIT`);
    /// then wait for readiness as create does. Expect the refusal to carry what it wrote. Red at
    /// 925882c1 (and racy before, 2 of 7 runs at af9131d4): readiness read no start time for the
    /// zombie and refused it Invalid, "native readiness identity changed".
    #[test]
    fn original_a_vmm_ending_before_readiness_is_refused_with_what_it_wrote() {
        let (shards_api::LaunchIo { stdout, stderr, .. }, mut output) =
            FirecrackerRuntime::vm_io().unwrap();
        let (vmm, gate) = StandIn::held(&format!("echo '{MEMLOCK}' >&2; exit 1"), stdout, stderr);
        vmm.end(gate);
        let refusal = readiness(&vmm, &mut output).unwrap_err();
        assert!(
            matches!(&refusal, Refusal::Broken(why) if why.contains(MEMLOCK)),
            "{refusal:?}"
        );
    }

    /// AC-T7.4.1: original. As above, but its receipt has collected its end before readiness first
    /// looks, as the node's supervisor collects an ended VMM's at once. Do: start the stand-in held,
    /// let it write and end, have its receipt reap it and see no child has its pid; then wait for
    /// readiness as create does. Expect the refusal to carry what it wrote. Red at 925882c1:
    /// readiness opened no pidfd at the reaped pid and refused Os, "observing native readiness
    /// controller" (ESRCH).
    #[test]
    fn original_a_vmm_reaped_before_readiness_is_refused_with_what_it_wrote() {
        let (shards_api::LaunchIo { stdout, stderr, .. }, mut output) =
            FirecrackerRuntime::vm_io().unwrap();
        let (vmm, gate) = StandIn::held(&format!("echo '{MEMLOCK}' >&2; exit 1"), stdout, stderr);
        vmm.end(gate);
        vmm.reap();
        let refusal = readiness(&vmm, &mut output).unwrap_err();
        assert!(
            matches!(&refusal, Refusal::Broken(why) if why.contains(MEMLOCK)),
            "{refusal:?}"
        );
    }

    /// AC-T7.4.1: non-vacuity of the identity check the originals above pass. Do: hold a stand-in
    /// on its gate, so it runs throughout, and wait for readiness through a receipt naming its pid
    /// with another start. Expect Invalid, "native readiness identity changed": its receipt says
    /// it runs, and no process with that identity does.
    #[test]
    fn a_running_vmm_whose_identity_names_another_start_is_refused() {
        let (shards_api::LaunchIo { stdout, stderr, .. }, mut output) =
            FirecrackerRuntime::vm_io().unwrap();
        let (mut vmm, gate) = StandIn::held("exit 0", stdout, stderr);
        vmm.identity.started += 1;
        let refused = readiness(&vmm, &mut output);
        vmm.end(gate);
        vmm.reap();
        assert_eq!(refused, Err(invalid("native readiness identity changed")));
    }

    /// AC-T7.4.1: do have a stand-in VMM write twice what one wire answer holds, as invalid
    /// UTF-8, then its last words, and end before readiness; expect a refusal that fits one
    /// refused answer under the wire bound and keeps the end of the output, last words included.
    #[test]
    fn a_vmm_refusal_keeps_the_end_of_its_output_within_one_wire_answer() {
        let (shards_api::LaunchIo { stdout, stderr, .. }, mut output) =
            FirecrackerRuntime::vm_io().unwrap();
        let bytes = hex_core::wire::MAX_MESSAGE * 2;
        let vmm = StandIn::start(
            &format!(
                "head -c {bytes} /dev/zero | tr '\\000' '\\377'; echo 'the last words' >&2; exit 1"
            ),
            stdout,
            stderr,
        );
        let dir = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(dir.path().join("ready")).unwrap();
        let until = Instant::now() + std::time::Duration::from_secs(60);
        let refusal = transport::ready(&listener, &vmm, &mut output, until).unwrap_err();
        let Refusal::Broken(why) = &refusal else {
            panic!("{refusal:?}");
        };
        assert!(why.starts_with("Firecracker ended before guest readiness; its last output:\n"));
        assert!(
            why.ends_with("\u{fffd}the last words\n"),
            "{:?}",
            why.get(why.len() - 64..)
        );
        let answer =
            hex_core::wire::snapshot_refusal(&refusal, hex_core::wire::MAX_MESSAGE).unwrap();
        assert!(answer.len() <= hex_core::wire::MAX_MESSAGE);
        assert!(
            answer.len() > hex_core::wire::MAX_MESSAGE - 4,
            "{}",
            answer.len()
        );
    }

    /// One CPU box's record, as a Store keeps it, and its launcher, as the node's: the launch starts a
    /// stand-in VMM on the create's own output, held until its identity is read and published in the
    /// record (hex-node supervise.rs `bind_managed` publishes it before the gate's release); then lets
    /// it run, and answers once it has ended and its receipt has collected its end, as the node's
    /// supervisor collects an ended VMM's at once.
    #[derive(Default)]
    struct Launcher {
        record: std::cell::RefCell<Option<BoxRecord>>,
        vmm: std::cell::RefCell<Option<triomphe::Arc<StandIn>>>,
    }

    impl Transaction for Launcher {
        fn records(&self) -> Result<crate::devices::Records, Refusal> {
            Ok(crate::devices::Records::new())
        }
        fn pool_records(&self) -> Result<crate::devices::PoolRecords, Refusal> {
            Ok(crate::devices::PoolRecords::new())
        }
        fn read_pool(&self, _: &hex_api::BackingKey) -> Result<Option<PoolRecord>, Refusal> {
            panic!("a CPU box reads no pool")
        }
        fn put_pool(&self, _: &PoolRecord) -> Result<(), Refusal> {
            panic!("a CPU box writes no pool")
        }
        fn grant_pool(&self, _: &hex_api::BackingGrant) -> Result<PoolRecord, Refusal> {
            panic!("a CPU box is granted no pool")
        }
        fn remove_pool(&self, _: &hex_api::BackingKey) -> Result<(), Refusal> {
            panic!("a CPU box removes no pool")
        }
        fn check_unsubmitted_pool(&self) -> Result<(), Refusal> {
            panic!("a CPU box checks no pool")
        }
        fn read(&self, _: &BoxId) -> Result<Option<BoxRecord>, Refusal> {
            Ok(self.record.borrow().clone())
        }
        fn put(&self, record: &BoxRecord) -> Result<(), Refusal> {
            let mut kept = record.clone();
            kept.native_scope
                .get_or_insert(hex_core::record::NativeScope::from_bytes([7; 16]));
            *self.record.borrow_mut() = Some(kept);
            Ok(())
        }
        fn remove(&self, _: &BoxId) -> Result<(), Refusal> {
            *self.record.borrow_mut() = None;
            Ok(())
        }
        fn launch_managed(
            &self,
            _: &ManagedLaunch<'_>,
            io: shards_api::LaunchIo,
            _: std::time::Duration,
        ) -> Result<shards_api::ManagedReceipt, Refusal> {
            let (vmm, gate) = StandIn::held(
                &format!("echo '{MEMLOCK}' >&2; exit 1"),
                io.stdout,
                io.stderr,
            );
            let mut record = self.record.borrow().clone().unwrap();
            record.init = Some(hex_core::record::ProcId {
                pid: vmm.identity.pid,
                start_ticks: vmm.identity.started,
            });
            *self.record.borrow_mut() = Some(record);
            vmm.end(gate);
            vmm.reap();
            let vmm = triomphe::Arc::new(vmm);
            *self.vmm.borrow_mut() = Some(triomphe::Arc::clone(&vmm));
            Ok(shards_api::managed_receipt(vmm))
        }
        fn attach_managed(
            &self,
            identity: Option<&shards_api::Identity>,
        ) -> Result<shards_api::ManagedReceipt, Refusal> {
            let vmm = triomphe::Arc::clone(self.vmm.borrow().as_ref().unwrap());
            assert_eq!(
                identity.map(|identity| (identity.pid, identity.started)),
                Some((vmm.identity.pid, vmm.identity.started)),
                "the cleanup attaches the receipt its record names"
            );
            Ok(shards_api::managed_receipt(vmm))
        }
        fn finalize_managed(&self, _: Option<&shards_api::Identity>) -> Result<(), Refusal> {
            Ok(())
        }
        fn managed_usage(&self) -> Result<hex_api::Usage, Refusal> {
            panic!("a VM whose guest never came up reads no usage")
        }
    }

    /// AC-T7.4.1: original. A create whose VMM says why it stops and ends before its guest is ready,
    /// its end collected before readiness first looks, as on a node whose supervisor reaps it at once.
    /// Do: create a CPU box through the launcher above, its stand-in VMM's identity published while it
    /// is held, then the VMM ended and reaped as the launch answers. Expect the create refused with
    /// what the VMM wrote, and nothing of the box kept. Red at 925882c1: refused Os, "observing
    /// native readiness controller" (ESRCH).
    #[test]
    fn original_a_create_whose_vmm_ends_before_readiness_is_refused_with_what_it_wrote() {
        use sha2::Digest;
        let dir = tempfile::tempdir().unwrap();
        let pinned = |name: &str, bytes: &[u8]| {
            let path = dir.path().join(name);
            fs::write(&path, bytes).unwrap();
            (path, <[u8; 32]>::from(sha2::Sha256::digest(bytes)))
        };
        let mut config = pool::tests::config();
        (config.binary, config.binary_sha256) = pinned("firecracker", b"a stand-in VMM, never run");
        (config.cost_evidence, config.cost.id) = pinned("evidence", b"a stand-in cost's evidence");
        config.state_dir = dir.path().join("state");
        fs::create_dir(&config.state_dir).unwrap();
        let catalog = crate::devices::Catalog::parse(b"hex-devices-1\n").unwrap();
        let owner: Arc<dyn Books> = Arc::new(super::handoff_tests::NoOwnerCalls);
        let allocation = config
            .cost
            .allocate(hex_api::CapacityResources {
                cpu_milli: 1000,
                mem_bytes: hex_api::MIN_MEMORY,
                disk_bytes: 0,
            })
            .unwrap();
        let runtime = FirecrackerRuntime {
            config,
            catalog: triomphe::Arc::try_new(catalog).unwrap(),
            books: Arc::downgrade(&owner),
            initialized: std::sync::OnceLock::new(),
            host: vfio::Host::system().unwrap(),
            marks: single_use::MarksAt {
                dir: PathBuf::from("/nonexistent/runtime"),
                unshared: None,
            },
            uid: rustix::process::geteuid().as_raw(),
            boot: None,
            joins: std::sync::Mutex::new(None),
            evented: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        };
        let id = BoxId::new("ends-before-readiness").unwrap();
        let spec = BoxSpec {
            id: id.clone(),
            bounds: hex_api::Bounds {
                cpu_max_milli: Some(1000),
                memory_max: Some(hex_api::MIN_MEMORY),
                ..hex_api::Bounds::default()
            },
            class: ClassSpec::Firecracker(Box::new(hex_api::OciSpec {
                rootfs: hex_api::Rootfs::Dir("/fixture/root".into()),
                init: Program::of(&["true"]),
                mounts: Vec::new(),
                hostname: None,
                network: hex_api::Network::Private,
                hosts: Vec::new(),
            })),
            owner: None,
            devices: Vec::new(),
            allocation: Some(allocation),
            route: Some(hex_api::RouteReceipt {
                key: hex_api::RouteKey {
                    class: Class::Firecracker,
                    image: hex_api::ImagePlatform {
                        os: hex_api::ImageOs::Linux,
                        arch: hex_api::ImageArch::Amd64,
                    },
                    execution: hex_api::Execution::Firecracker,
                    gpu: hex_api::GpuEligibility::None,
                },
                backing: None,
            }),
            band: 0,
        };
        let launcher = Launcher::default();
        // The create's deadline, as `create` takes it from its config.
        let until = Instant::now() + runtime.config.boot_timeout;
        let refused = runtime.create_or_cleanup(&id, spec, None, &launcher, until);
        assert!(
            matches!(&refused, Err(Refusal::Broken(why)) if why.contains(MEMLOCK)),
            "{refused:?}"
        );
        assert!(launcher.record.borrow().is_none(), "no record is kept");
        assert_eq!(
            fs::read_dir(&runtime.config.state_dir).unwrap().count(),
            0,
            "no VM state is kept"
        );
    }
}

#[cfg(test)]
mod handoff_tests {
    use super::*;
    use crate::devices::{Catalog, PoolRecords, Records};
    use hex_api::{
        BackingKey, BackingPhase, Capacity, Execution, GpuEligibility, ImageArch, ImageOs,
        ImagePlatform, RouteKey, RouteReceipt, RuntimeCost,
    };

    pub(super) struct NoOwnerCalls;
    impl Books for NoOwnerCalls {
        fn records(&self) -> Result<Records, Refusal> {
            panic!("read-only proof must not call Books")
        }
        fn pool_records(&self) -> Result<PoolRecords, Refusal> {
            panic!("read-only proof must not call Books")
        }
        fn prepare_pool(
            &self,
            _: [u8; 32],
            _: Execution,
            _: RuntimeCost,
            _: &[DeviceId],
            _: (&[u8; 32], &[DeviceId], Option<&PoolRecord>),
            _: &mut crate::devices::PoolPreparationCheck<'_>,
        ) -> Result<PoolRecord, Refusal> {
            panic!("read-only proof must not call Books")
        }
        fn capacity(&self) -> Result<Capacity, Refusal> {
            panic!("read-only proof must not query capacity")
        }
        fn physical_devices(&self) -> Result<Vec<hex_api::DeviceInfo>, Refusal> {
            panic!("read-only proof must not probe")
        }
        fn with_transaction(
            &self,
            _: &BoxId,
            _: &[DeviceId],
            _: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
        ) -> Result<(), Refusal> {
            panic!("read-only proof must not acquire another transaction")
        }
        fn with_pool_transaction(
            &self,
            _: &BackingKey,
            _: &[DeviceId],
            _: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
        ) -> Result<(), Refusal> {
            panic!("read-only proof must not acquire another transaction")
        }
        fn with_handoff_transaction(
            &self,
            _: &BoxId,
            _: &BackingKey,
            _: &[DeviceId],
            _: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
        ) -> Result<(), Refusal> {
            panic!("read-only proof must not acquire another transaction")
        }
    }

    /// AC-T7.4.1: cancelling an unsubmitted S never infers native absence after P Released.
    /// Only private regular-file metadata and synthetic canonical records; no native/Books calls.
    #[test]
    fn unsubmitted_subset_proof_refuses_intent_collision_and_released_source() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = pool::tests::config();
        config.state_dir = dir.path().to_owned();
        let catalog = Catalog::parse(b"hex-devices-1\ngpu\t256\ta\t1\tvfio\t0000:01:00.0\t7\ngpu\t512\tb\t1\tvfio\t0000:02:00.0\t8\n").unwrap();
        let owner: Arc<dyn Books> = Arc::new(NoOwnerCalls);
        let runtime = FirecrackerRuntime {
            config,
            catalog: triomphe::Arc::try_new(catalog).unwrap(),
            books: Arc::downgrade(&owner),
            initialized: std::sync::OnceLock::new(),
            host: vfio::Host::system().unwrap(),
            marks: single_use::MarksAt {
                dir: PathBuf::from("/nonexistent/runtime"),
                unshared: None,
            },
            uid: 0,
            boot: None,
            joins: std::sync::Mutex::new(None),
            evented: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        };
        let devices = [256, 512]
            .map(|key| DeviceId {
                kind: DeviceKind::Gpu,
                key,
            })
            .to_vec();
        let cost = RuntimeCost {
            ceiling: runtime.config.cost.floor,
            ..runtime.config.cost
        };
        let mut source = PoolRecord {
            version: hex_core::record::RECORD_VERSION,
            key: BackingKey {
                pool: pool::observer_configuration(&runtime.config, &devices).unwrap(),
                generation: [3; 16],
            },
            execution: Execution::Firecracker,
            cost,
            allocation: Some(
                cost.allocate(hex_api::CapacityResources::default())
                    .unwrap(),
            ),
            devices,
            phase: BackingPhase::Live,
            init: Some(hex_core::record::ProcId {
                pid: 1,
                start_ticks: 1,
            }),
            exit: None,
        };
        let spec = BoxSpec {
            id: BoxId::new("unsubmitted-subset").unwrap(),
            bounds: hex_api::Bounds {
                cpu_max_milli: Some(1000),
                memory_max: Some(hex_api::MIN_MEMORY),
                ..hex_api::Bounds::default()
            },
            class: ClassSpec::Firecracker(Box::new(hex_api::OciSpec {
                rootfs: hex_api::Rootfs::Dir("/fixture/root".into()),
                init: Program::of(&["true"]),
                mounts: Vec::new(),
                hostname: None,
                network: hex_api::Network::Private,
                hosts: Vec::new(),
            })),
            owner: None,
            devices: vec![source.devices[0]],
            allocation: Some(
                runtime
                    .config
                    .cost
                    .allocate(hex_api::CapacityResources {
                        cpu_milli: 1000,
                        mem_bytes: hex_api::MIN_MEMORY,
                        disk_bytes: 0,
                    })
                    .unwrap(),
            ),
            route: Some(RouteReceipt {
                key: RouteKey {
                    class: Class::Firecracker,
                    image: ImagePlatform {
                        os: ImageOs::Linux,
                        arch: ImageArch::Amd64,
                    },
                    execution: Execution::Firecracker,
                    gpu: GpuEligibility::Required,
                },
                backing: Some(source.key),
            }),
            band: 0,
        };
        let mut record = BoxRecord::new(spec, Phase::Creating, Vec::new());
        record.native_scope = Some(hex_core::record::NativeScope::from_bytes([6; 16]));
        runtime.handoff_unsubmitted(&record, &source).unwrap();
        record.phase = Phase::Deleting;
        source.phase = BackingPhase::Draining;
        runtime.handoff_unsubmitted(&record, &source).unwrap();
        let paths = runtime.paths(&record).unwrap();
        fs::create_dir(&paths.dir).unwrap();
        assert!(runtime.handoff_unsubmitted(&record, &source).is_err());
        fs::remove_dir(&paths.dir).unwrap();
        record.init = source.init;
        assert!(runtime.handoff_unsubmitted(&record, &source).is_err());
        record.init = None;
        record.dirs.push(paths.dir);
        assert!(runtime.handoff_unsubmitted(&record, &source).is_err());
        record.dirs.clear();
        source.phase = BackingPhase::Released;
        source.exit = Some(hex_api::Exit::Code(0));
        assert!(runtime.handoff_unsubmitted(&record, &source).is_err());
        assert!(fs::read_dir(dir.path()).unwrap().next().is_none());
    }
    /// AC-T7.4.1: actual FC key constructors project CPU/history/subset forms with no Books/I/O.
    #[test]
    fn pure_firecracker_projection_uses_configured_subset_rows_and_no_native_queries() {
        let catalog = Catalog::parse(b"hex-devices-1\ngpu\t256\ta\t1\tvfio\t0000:01:00.0\t7\ngpu\t512\tb\t1\tvfio\t0000:02:00.0\t8\n").unwrap();
        let owner: Arc<dyn Books> = Arc::new(NoOwnerCalls);
        let runtime = FirecrackerRuntime {
            config: pool::tests::config(),
            catalog: triomphe::Arc::try_new(catalog).unwrap(),
            books: Arc::downgrade(&owner),
            initialized: std::sync::OnceLock::new(),
            host: vfio::Host::system().unwrap(),
            marks: single_use::MarksAt {
                dir: PathBuf::from("/nonexistent/runtime"),
                unshared: None,
            },
            uid: 0,
            boot: None,
            joins: std::sync::Mutex::new(None),
            evented: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        };
        let full = [256, 512].map(|key| DeviceId {
            kind: DeviceKind::Gpu,
            key,
        });
        let cost = RuntimeCost {
            ceiling: runtime.config.cost.floor,
            ..runtime.config.cost
        };
        let make = |devices: Vec<DeviceId>, phase| PoolRecord {
            version: hex_core::record::RECORD_VERSION,
            key: BackingKey {
                pool: pool::observer_configuration(&runtime.config, &devices).unwrap(),
                generation: [if devices.len() == full.len() { 3 } else { 4 }; 16],
            },
            execution: Execution::Firecracker,
            cost,
            allocation: (phase != BackingPhase::Prepared).then(|| {
                cost.allocate(hex_api::CapacityResources::default())
                    .unwrap()
            }),
            devices,
            phase,
            init: if phase == BackingPhase::Released {
                Some(hex_core::record::ProcId {
                    pid: 1,
                    start_ticks: 1,
                })
            } else {
                None
            },
            exit: if phase == BackingPhase::Released {
                Some(hex_api::Exit::Code(0))
            } else {
                None
            },
        };
        let rows = [
            make(full.to_vec(), BackingPhase::Released),
            make(vec![full[1]], BackingPhase::Prepared),
        ];
        let mut projection = crate::devices::RouteProjection::new();
        assert!(
            runtime
                .project_routes(&rows, None, &mut projection)
                .is_err(),
            "uninitialized adapter cannot project VM prefixes"
        );
        assert_eq!(projection.count().unwrap(), 0);
        runtime.initialized.set(()).unwrap(); // Pure fixture condition only, never native readiness proof.
        let (process, oci) = crate::devices::native_route_keys(
            Class::Oci,
            runtime.route_key(GpuEligibility::None).unwrap().image,
        )
        .unwrap();
        projection.retain(process, None).unwrap();
        projection.retain(oci.unwrap(), None).unwrap();
        runtime
            .project_routes(
                &rows,
                Some((Execution::Firecracker, &full[..1])),
                &mut projection,
            )
            .unwrap();
        assert_eq!(
            projection.count().unwrap(),
            6,
            "native2 +FCcpu1 +retainedGPUpairs2 +newLifetime1"
        );
        assert!(
            runtime
                .project_routes(
                    &rows,
                    Some((
                        Execution::Firecracker,
                        &[DeviceId {
                            kind: DeviceKind::Gpu,
                            key: 768
                        }]
                    )),
                    &mut crate::devices::RouteProjection::new()
                )
                .is_err()
        );
        let mut wrong = make(vec![full[1]], BackingPhase::Prepared);
        wrong.cost.id = [9; 32];
        assert!(
            runtime
                .project_routes(&[wrong], None, &mut crate::devices::RouteProjection::new())
                .is_err()
        );
    }
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;
    use crate::devices::{Catalog, PoolRecords, Records};
    use hex_api::{
        CapacityResources, Execution, GpuEligibility, ImageArch, ImageOs, ImagePlatform, RouteKey,
        RouteReceipt,
    };
    use sha2::{Digest, Sha256};
    use std::cell::RefCell;

    /// One box's record as a Store keeps it, its fresh write given a native scope, whose native scope a cleanup can't
    /// attach: as one that can't reach its VMM meets it.
    struct Unattachable(RefCell<Option<BoxRecord>>);

    impl Transaction for Unattachable {
        fn records(&self) -> Result<Records, Refusal> {
            Ok(Records::new())
        }
        fn pool_records(&self) -> Result<PoolRecords, Refusal> {
            Ok(PoolRecords::new())
        }
        fn read_pool(&self, _: &hex_api::BackingKey) -> Result<Option<PoolRecord>, Refusal> {
            panic!("a CPU box reads no pool")
        }
        fn put_pool(&self, _: &PoolRecord) -> Result<(), Refusal> {
            panic!("a CPU box writes no pool")
        }
        fn grant_pool(&self, _: &hex_api::BackingGrant) -> Result<PoolRecord, Refusal> {
            panic!("a CPU box is granted no pool")
        }
        fn remove_pool(&self, _: &hex_api::BackingKey) -> Result<(), Refusal> {
            panic!("a CPU box removes no pool")
        }
        fn check_unsubmitted_pool(&self) -> Result<(), Refusal> {
            panic!("a CPU box checks no pool")
        }
        fn read(&self, _: &BoxId) -> Result<Option<BoxRecord>, Refusal> {
            Ok(self.0.borrow().clone())
        }
        fn put(&self, record: &BoxRecord) -> Result<(), Refusal> {
            let mut kept = record.clone();
            kept.native_scope
                .get_or_insert(hex_core::record::NativeScope::from_bytes([7; 16]));
            *self.0.borrow_mut() = Some(kept);
            Ok(())
        }
        fn remove(&self, _: &BoxId) -> Result<(), Refusal> {
            *self.0.borrow_mut() = None;
            Ok(())
        }
        fn launch_managed(
            &self,
            _: &ManagedLaunch<'_>,
            _: shards_api::LaunchIo,
            _: std::time::Duration,
        ) -> Result<shards_api::ManagedReceipt, Refusal> {
            panic!("this create is refused before its VMM")
        }
        fn attach_managed(
            &self,
            _: Option<&shards_api::Identity>,
        ) -> Result<shards_api::ManagedReceipt, Refusal> {
            Err(Refusal::Os {
                op: "attaching the box's native scope",
                why: "the test refuses it".to_owned(),
            })
        }
        fn finalize_managed(&self, _: Option<&shards_api::Identity>) -> Result<(), Refusal> {
            panic!("an unattached scope is never finalized")
        }
        fn managed_usage(&self) -> Result<hex_api::Usage, Refusal> {
            panic!("a refused create reads no usage")
        }
    }

    /// AC-T7.4.1 (original): a create refused whose cleanup fails too keeps both causes, typed, its own first. Do:
    /// create a CPU box whose VM folder can't be made (its state folder is absent), on records whose native scope its
    /// cleanup can't attach. Expect `Uncertain` carrying the create's refusal, then the cleanup's: the cleanup's
    /// failure had replaced both with a bare `Lifecycle` `Uncertain`.
    #[test]
    fn original_a_create_whose_cleanup_fails_keeps_both_causes_its_own_first() {
        let dir = tempfile::tempdir().unwrap();
        let pinned = |name: &str, bytes: &[u8]| {
            let path = dir.path().join(name);
            fs::write(&path, bytes).unwrap();
            (path, <[u8; 32]>::from(Sha256::digest(bytes)))
        };
        let mut config = pool::tests::config();
        (config.binary, config.binary_sha256) = pinned("firecracker", b"a stand-in VMM, never run");
        (config.cost_evidence, config.cost.id) = pinned("evidence", b"a stand-in cost's evidence");
        config.state_dir = dir.path().join("absent");
        let catalog =
            Catalog::parse(b"hex-devices-1\ngpu\t256\ta\t1\tvfio\t0000:01:00.0\t7\n").unwrap();
        let owner: Arc<dyn Books> = Arc::new(super::handoff_tests::NoOwnerCalls);
        let allocation = config
            .cost
            .allocate(CapacityResources {
                cpu_milli: 1000,
                mem_bytes: hex_api::MIN_MEMORY,
                disk_bytes: 0,
            })
            .unwrap();
        let runtime = FirecrackerRuntime {
            config,
            catalog: triomphe::Arc::try_new(catalog).unwrap(),
            books: Arc::downgrade(&owner),
            initialized: std::sync::OnceLock::new(),
            host: vfio::Host::system().unwrap(),
            marks: single_use::MarksAt {
                dir: PathBuf::from("/nonexistent/runtime"),
                unshared: None,
            },
            uid: 0,
            boot: None,
            joins: std::sync::Mutex::new(None),
            evented: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        };
        let id = BoxId::new("cleanup-refused").unwrap();
        let spec = BoxSpec {
            id: id.clone(),
            bounds: hex_api::Bounds {
                cpu_max_milli: Some(1000),
                memory_max: Some(hex_api::MIN_MEMORY),
                ..hex_api::Bounds::default()
            },
            class: ClassSpec::Firecracker(Box::new(hex_api::OciSpec {
                rootfs: hex_api::Rootfs::Dir("/fixture/root".into()),
                init: Program::of(&["true"]),
                mounts: Vec::new(),
                hostname: None,
                network: hex_api::Network::Private,
                hosts: Vec::new(),
            })),
            owner: None,
            devices: Vec::new(),
            allocation: Some(allocation),
            route: Some(RouteReceipt {
                key: RouteKey {
                    class: Class::Firecracker,
                    image: ImagePlatform {
                        os: ImageOs::Linux,
                        arch: ImageArch::Amd64,
                    },
                    execution: Execution::Firecracker,
                    gpu: GpuEligibility::None,
                },
                backing: None,
            }),
            band: 0,
        };
        let records = Unattachable(RefCell::new(None));
        let until = Instant::now() + std::time::Duration::from_secs(30);
        let refused = runtime
            .create_or_cleanup(&id, spec, None, &records, until)
            .unwrap_err();
        let Refusal::Uncertain {
            id: named,
            refusal,
            cleanup,
        } = &refused
        else {
            panic!("both causes kept: {refused:?}");
        };
        assert_eq!(named, &id);
        assert!(
            matches!(
                &**refusal,
                Refusal::Os {
                    op: "creating private VM directory",
                    ..
                }
            ),
            "{refusal:?}"
        );
        assert_eq!(
            **cleanup,
            Refusal::Os {
                op: "attaching the box's native scope",
                why: "the test refuses it".to_owned(),
            }
        );
    }
}

#[cfg(test)]
mod memlock_tests {
    use super::*;
    use crate::devices::{Catalog, PoolRecords, Records};
    use hex_api::{
        CapacityResources, Execution, GpuEligibility, ImageArch, ImagePlatform, RouteKey,
        RouteReceipt,
    };
    use sha2::{Digest, Sha256};
    use std::cell::RefCell;

    /// Set in the copy of this binary that runs [`original_a_vfio_vm_past_its_memlock_is_refused_before_launch`]'s
    /// case under a lowered RLIMIT_MEMLOCK, which no process raises back without CAP_SYS_RESOURCE.
    const CASE_ENV: &str = "HEX_FC_MEMLOCK_CASE";
    const PASS: &str = "memlock: the VFIO VM was refused before its VMM";
    const TEST: &str = "firecracker::native::memlock_tests::original_a_vfio_vm_past_its_memlock_is_refused_before_launch";

    /// A Store's records for one box, its fresh write given a native scope; its VMM's launch is the one call that
    /// mustn't come.
    struct Unlaunched(RefCell<Option<BoxRecord>>);

    impl Transaction for Unlaunched {
        fn records(&self) -> Result<Records, Refusal> {
            Ok(Records::new())
        }
        fn pool_records(&self) -> Result<PoolRecords, Refusal> {
            Ok(PoolRecords::new())
        }
        fn read_pool(&self, _: &hex_api::BackingKey) -> Result<Option<PoolRecord>, Refusal> {
            Ok(None)
        }
        fn put_pool(&self, _: &PoolRecord) -> Result<(), Refusal> {
            panic!("a dedicated GPU box writes no pool")
        }
        fn grant_pool(&self, _: &hex_api::BackingGrant) -> Result<PoolRecord, Refusal> {
            panic!("a dedicated GPU box is granted no pool")
        }
        fn remove_pool(&self, _: &hex_api::BackingKey) -> Result<(), Refusal> {
            panic!("a dedicated GPU box removes no pool")
        }
        fn check_unsubmitted_pool(&self) -> Result<(), Refusal> {
            Ok(())
        }
        fn read(&self, _: &BoxId) -> Result<Option<BoxRecord>, Refusal> {
            Ok(self.0.borrow().clone())
        }
        fn put(&self, record: &BoxRecord) -> Result<(), Refusal> {
            let mut kept = record.clone();
            kept.native_scope
                .get_or_insert(hex_core::record::NativeScope::from_bytes([7; 16]));
            *self.0.borrow_mut() = Some(kept);
            Ok(())
        }
        fn remove(&self, _: &BoxId) -> Result<(), Refusal> {
            *self.0.borrow_mut() = None;
            Ok(())
        }
        fn launch_managed(
            &self,
            _: &ManagedLaunch<'_>,
            _: shards_api::LaunchIo,
            _: std::time::Duration,
        ) -> Result<shards_api::ManagedReceipt, Refusal> {
            Err(Refusal::Broken(
                "the VMM was launched past its memlock".to_owned(),
            ))
        }
        fn attach_managed(
            &self,
            _: Option<&shards_api::Identity>,
        ) -> Result<shards_api::ManagedReceipt, Refusal> {
            panic!("a create refused before its VMM attaches nothing")
        }
        fn finalize_managed(&self, _: Option<&shards_api::Identity>) -> Result<(), Refusal> {
            panic!("a create refused before its VMM finalizes nothing")
        }
        fn managed_usage(&self) -> Result<hex_api::Usage, Refusal> {
            panic!("a refused create reads no usage")
        }
    }

    /// The lead's ruling of 2026-11-11 (AC-T7.4.1; original; the GPU run r690's step a on 00aad8da): a VFIO VM's VMM
    /// maps all its guest's RAM for DMA, which vfio's type1 IOMMU charges to the VMM's RLIMIT_MEMLOCK unless the VMM
    /// holds CAP_IPC_LOCK; Firecracker then ended with `IommuDmaMap(Error(12))`, ENOMEM, after its launch. Do, in a copy
    /// of this binary whose RLIMIT_MEMLOCK, soft and hard, is one MiB, below its VM's RAM, and which holds no
    /// CAP_IPC_LOCK to give its VMM (a root copy drops it from its bounding and ambient sets): create a VM given a
    /// GPU whole (VFIO) through the adapter, its records' VMM launch refused if it comes. Expect the create refused before
    /// anything is written or launched (no record, no VFIO plan read from the host, no VMM), by type, naming the VM's RAM
    /// in bytes, the soft and the hard limit, and CAP_IPC_LOCK.
    #[test]
    fn original_a_vfio_vm_past_its_memlock_is_refused_before_launch() {
        if std::env::var_os(CASE_ENV).is_some() {
            refused_before_launch_under_a_low_memlock();
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
            .env(CASE_ENV, "1")
            .output()
            .unwrap();
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        eprint!("{log}");
        assert!(output.status.success(), "the copy failed");
        assert!(log.lines().any(|line| line == PASS), "the copy ran no case");
    }

    fn refused_before_launch_under_a_low_memlock() {
        use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
        use rustix::thread::CapabilitySet;
        if rustix::process::geteuid().is_root() {
            rustix::thread::remove_capability_from_bounding_set(CapabilitySet::IPC_LOCK).unwrap();
            rustix::thread::configure_capability_in_ambient_set(CapabilitySet::IPC_LOCK, false)
                .unwrap();
        }
        let low = 1 << 20;
        setrlimit(
            Resource::Memlock,
            Rlimit {
                current: Some(low),
                maximum: Some(low),
            },
        )
        .unwrap();
        assert_eq!(getrlimit(Resource::Memlock).maximum, Some(low));
        let dir = tempfile::tempdir().unwrap();
        let pinned = |name: &str, bytes: &[u8]| {
            let path = dir.path().join(name);
            fs::write(&path, bytes).unwrap();
            (path, <[u8; 32]>::from(Sha256::digest(bytes)))
        };
        let mut config = pool::tests::config();
        (config.binary, config.binary_sha256) = pinned("firecracker", b"a stand-in VMM, never run");
        (config.cost_evidence, config.cost.id) = pinned("evidence", b"a stand-in cost's evidence");
        config.state_dir = dir.path().join("state");
        fs::create_dir(&config.state_dir).unwrap();
        let catalog =
            Catalog::parse(b"hex-devices-1\ngpu\t256\ta\t1\tvfio\t0000:01:00.0\t7\n").unwrap();
        let owner: Arc<dyn Books> = Arc::new(super::handoff_tests::NoOwnerCalls);
        let ram = 64 << 20;
        let allocation = config
            .cost
            .allocate(CapacityResources {
                cpu_milli: 1000,
                mem_bytes: ram,
                disk_bytes: 0,
            })
            .unwrap();
        let runtime = FirecrackerRuntime {
            config,
            catalog: triomphe::Arc::try_new(catalog).unwrap(),
            books: Arc::downgrade(&owner),
            initialized: std::sync::OnceLock::new(),
            host: vfio::Host::system().unwrap(),
            marks: single_use::MarksAt {
                dir: PathBuf::from("/nonexistent/runtime"),
                unshared: None,
            },
            uid: 0,
            boot: None,
            joins: std::sync::Mutex::new(None),
            evented: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        };
        let id = BoxId::new("gpu-memlock").unwrap();
        let spec = BoxSpec {
            id: id.clone(),
            bounds: hex_api::Bounds {
                cpu_max_milli: Some(1000),
                memory_max: Some(ram),
                ..hex_api::Bounds::default()
            },
            class: ClassSpec::Firecracker(Box::new(hex_api::OciSpec {
                rootfs: hex_api::Rootfs::Dir("/fixture/root".into()),
                init: Program::of(&["true"]),
                mounts: Vec::new(),
                hostname: None,
                network: hex_api::Network::Private,
                hosts: Vec::new(),
            })),
            owner: None,
            devices: vec![DeviceId {
                kind: DeviceKind::Gpu,
                key: 256,
            }],
            allocation: Some(allocation),
            route: Some(RouteReceipt {
                key: RouteKey {
                    class: Class::Firecracker,
                    image: ImagePlatform {
                        os: hex_api::ImageOs::Linux,
                        arch: ImageArch::Amd64,
                    },
                    execution: Execution::Firecracker,
                    gpu: GpuEligibility::Required,
                },
                backing: None,
            }),
            band: 0,
        };
        let vm_ram = runtime.config.machine_memory(&spec).unwrap() * (1 << 20);
        let records = Unlaunched(RefCell::new(None));
        let until = Instant::now() + std::time::Duration::from_secs(30);
        let refused = runtime
            .create_inner(spec, None, &records, until)
            .unwrap_err();
        let Refusal::Platform { why, .. } = &refused else {
            panic!("refused before its VMM, by its memlock: {refused:?}");
        };
        assert!(
            records.0.borrow().is_none(),
            "refused before anything is written"
        );
        for named in [
            vm_ram.to_string(),
            format!("soft {low}"),
            format!("hard {low}"),
            "CAP_IPC_LOCK".to_owned(),
        ] {
            assert!(why.contains(&named), "{named} named: {why}");
        }
        eprintln!("{PASS}");
    }
}

#[cfg(test)]
mod single_use_tests {
    use super::*;
    use crate::devices::{Catalog, PoolRecords, Records};
    use hex_api::{
        CapacityResources, Execution, GpuEligibility, ImageArch, ImageOs, ImagePlatform, RouteKey,
        RouteReceipt,
    };
    use sha2::{Digest, Sha256};
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;

    /// A VMM that has ended, its whole scope released: a stand-in VMM whose guest never came up.
    struct Ended(shards_api::Identity);

    impl shards_api::ManagedProcess for Ended {
        fn identity(&self) -> Result<&shards_api::Identity, shards_api::Refusal> {
            Ok(&self.0)
        }
        fn wait_for(
            &self,
            _: std::time::Duration,
        ) -> Result<Option<shards_api::ManagedEnd>, shards_api::Refusal> {
            self.terminal()
        }
        fn stop(&self) -> Result<(), shards_api::Refusal> {
            Ok(())
        }
        fn terminal(&self) -> Result<Option<shards_api::ManagedEnd>, shards_api::Refusal> {
            Ok(Some(shards_api::ManagedEnd {
                status: shards_api::ManagedStatus::Code(1),
                scope: shards_api::TerminalScope::Released,
            }))
        }
    }

    /// One node's records of the VMs these tests make, and the VMMs it launches: each launch hands its VM the
    /// functions it was given, and the VMM ends at once, as one whose guest never came up.
    #[derive(Default)]
    struct Handed {
        records: RefCell<BTreeMap<BoxId, BoxRecord>>,
        scopes: Cell<u8>,
        launched: Cell<usize>,
    }

    impl Transaction for Handed {
        fn records(&self) -> Result<Records, Refusal> {
            Ok(self
                .records
                .borrow()
                .iter()
                .map(|(id, record)| (id.clone(), Ok(record.clone())))
                .collect())
        }
        fn pool_records(&self) -> Result<PoolRecords, Refusal> {
            Ok(PoolRecords::new())
        }
        fn read_pool(&self, _: &hex_api::BackingKey) -> Result<Option<PoolRecord>, Refusal> {
            Ok(None)
        }
        fn put_pool(&self, _: &PoolRecord) -> Result<(), Refusal> {
            panic!("a dedicated GPU VM writes no pool")
        }
        fn grant_pool(&self, _: &hex_api::BackingGrant) -> Result<PoolRecord, Refusal> {
            panic!("a dedicated GPU VM is granted no pool")
        }
        fn remove_pool(&self, _: &hex_api::BackingKey) -> Result<(), Refusal> {
            panic!("a dedicated GPU VM removes no pool")
        }
        fn check_unsubmitted_pool(&self) -> Result<(), Refusal> {
            panic!("a dedicated GPU VM checks no pool")
        }
        fn read(&self, id: &BoxId) -> Result<Option<BoxRecord>, Refusal> {
            Ok(self.records.borrow().get(id).cloned())
        }
        fn put(&self, record: &BoxRecord) -> Result<(), Refusal> {
            let mut kept = record.clone();
            if kept.native_scope.is_none() {
                self.scopes.set(self.scopes.get() + 1);
                kept.native_scope = Some(hex_core::record::NativeScope::from_bytes(
                    [self.scopes.get(); 16],
                ));
            }
            self.records
                .borrow_mut()
                .insert(record.spec.id.clone(), kept);
            Ok(())
        }
        fn remove(&self, id: &BoxId) -> Result<(), Refusal> {
            self.records.borrow_mut().remove(id);
            Ok(())
        }
        fn launch_managed(
            &self,
            _: &ManagedLaunch<'_>,
            _: shards_api::LaunchIo,
            _: std::time::Duration,
        ) -> Result<shards_api::ManagedReceipt, Refusal> {
            self.launched.set(self.launched.get() + 1);
            Err(Refusal::Broken(
                "the stand-in VMM ended before its guest was ready".to_owned(),
            ))
        }
        fn attach_managed(
            &self,
            _: Option<&shards_api::Identity>,
        ) -> Result<shards_api::ManagedReceipt, Refusal> {
            Ok(shards_api::managed_receipt(triomphe::Arc::new(Ended(
                shards_api::Identity {
                    pid: 1,
                    started: 1,
                    vsock: PathBuf::from("/nonexistent/ended.vsock"),
                },
            ))))
        }
        fn finalize_managed(&self, _: Option<&shards_api::Identity>) -> Result<(), Refusal> {
            Ok(())
        }
        fn managed_usage(&self) -> Result<hex_api::Usage, Refusal> {
            panic!("an ended VM reads no usage")
        }
    }

    fn boot(name: &str) -> Option<BootSession> {
        Some(BootSession::new(name).unwrap())
    }

    const THIS_BOOT: &str = "6d1c0e2a-7b4f-4a3e-9c51-1f0e2d3c4b5a";

    /// The host the tests' nodes share, at `root`: one function, `0000:01:00.0`, alone in IOMMU group 7, a display
    /// controller bound to vfio-pci whose kernel lists no reset method.
    fn stand_in(root: &Path) {
        let host = vfio::stand_in::host(root, vfio::stand_in::opened);
        vfio::stand_in::function(&host, "0000:01:00.0", 7, "vfio-pci", "0x030000", None);
    }

    /// A node's adapter on the host at `host` with catalog `catalog`: its state under `dir`, its runtime directory
    /// `runtime`, on boot `boot`.
    fn node(
        dir: &Path,
        host: &Path,
        runtime: &Path,
        catalog: &[u8],
        owner: &Arc<dyn Books>,
    ) -> FirecrackerRuntime {
        fs::create_dir_all(dir).unwrap();
        fs::create_dir_all(runtime).unwrap();
        let pinned = |name: &str, bytes: &[u8]| {
            let path = dir.join(name);
            fs::write(&path, bytes).unwrap();
            (path, <[u8; 32]>::from(Sha256::digest(bytes)))
        };
        let mut config = pool::tests::config();
        (config.binary, config.binary_sha256) = pinned("firecracker", b"a stand-in VMM, never run");
        (config.cost_evidence, config.cost.id) = pinned("evidence", b"a stand-in cost's evidence");
        config.state_dir = dir.join("state");
        fs::create_dir(&config.state_dir).unwrap();
        // Its VMMs may pin the RAM of the VMs these tests create (`spec`'s, one RAM whatever the name) and no more,
        // whatever this machine's own limit (AC-T7.4.1).
        let ram = config.machine_memory(&spec(&config, "vm")).unwrap() * (1 << 20);
        FirecrackerRuntime {
            config,
            catalog: triomphe::Arc::try_new(Catalog::parse(catalog).unwrap()).unwrap(),
            books: Arc::downgrade(owner),
            initialized: std::sync::OnceLock::new(),
            host: vfio::Host {
                sys: host.join("sys"),
                dev: host.join("dev"),
                open: vfio::stand_in::opened,
                memlock: vfio::stand_in::memlock(ram),
            },
            marks: single_use::MarksAt {
                dir: runtime.to_path_buf(),
                unshared: None,
            },
            uid: rustix::process::geteuid().as_raw(),
            boot: boot(THIS_BOOT),
            joins: std::sync::Mutex::new(None),
            evented: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    const GPU: &[u8] = b"hex-devices-1\ngpu\t256\ta\t1\tvfio\t0000:01:00.0\t7\n";

    /// A VM given the stand-in GPU on a node configured with `config`.
    fn spec(config: &Config, name: &str) -> BoxSpec {
        BoxSpec {
            id: BoxId::new(name).unwrap(),
            bounds: hex_api::Bounds {
                cpu_max_milli: Some(1000),
                memory_max: Some(hex_api::MIN_MEMORY),
                ..hex_api::Bounds::default()
            },
            class: ClassSpec::Firecracker(Box::new(hex_api::OciSpec {
                rootfs: hex_api::Rootfs::Dir("/fixture/root".into()),
                init: Program::of(&["true"]),
                mounts: Vec::new(),
                hostname: None,
                network: hex_api::Network::Private,
                hosts: Vec::new(),
            })),
            owner: None,
            devices: vec![DeviceId {
                kind: DeviceKind::Gpu,
                key: 256,
            }],
            allocation: Some(
                config
                    .cost
                    .allocate(CapacityResources {
                        cpu_milli: 1000,
                        mem_bytes: hex_api::MIN_MEMORY,
                        disk_bytes: 0,
                    })
                    .unwrap(),
            ),
            route: Some(RouteReceipt {
                key: RouteKey {
                    class: Class::Firecracker,
                    image: ImagePlatform {
                        os: ImageOs::Linux,
                        arch: ImageArch::Amd64,
                    },
                    execution: Execution::Firecracker,
                    gpu: GpuEligibility::Required,
                },
                backing: None,
            }),
            band: 0,
        }
    }

    fn create(node: &FirecrackerRuntime, books: &Handed, name: &str) -> Result<BoxInfo, Refusal> {
        let until = Instant::now() + std::time::Duration::from_secs(30);
        node.create_or_cleanup(
            &BoxId::new(name).unwrap(),
            spec(&node.config, name),
            None,
            books,
            until,
        )
    }

    fn refused_for_its_boot(refused: &Result<BoxInfo, Refusal>) {
        let Err(Refusal::Platform { why, .. }) = refused else {
            panic!("refused by type: {refused:?}");
        };
        for named in ["0000:01:00.0", "no reset method", "the host must reboot"] {
            assert!(why.contains(named), "{named}: {why}");
        }
    }

    /// The lead's ruling of 2026-11-11 (AC-T7.4.1; original): GPU run r940 on 00aad8da found 0000:79:00.0 broken in
    /// the VM after r862's ("PSP create ring failed"): its kernel lists no reset that clears it between VMs, so the
    /// next VM given it boots into the last one's state. Do: on a stand-in host, create a VM given a function whose
    /// kernel lists no reset method, its VMM ending at once; then a second VM given it on the same boot; then a third
    /// on a later boot. Expect the first handed it (its VMM launched), the second refused by type before its VMM,
    /// saying it has no reset method and the host must reboot, and the third handed it again.
    #[test]
    fn original_a_function_no_reset_clears_goes_to_one_vm_per_boot() {
        let dir = tempfile::tempdir().unwrap();
        stand_in(&dir.path().join("host"));
        let owner: Arc<dyn Books> = Arc::new(super::handoff_tests::NoOwnerCalls);
        let mut runtime = node(
            &dir.path().join("node"),
            &dir.path().join("host"),
            &dir.path().join("runtime"),
            GPU,
            &owner,
        );
        let books = Handed::default();
        let first = create(&runtime, &books, "first-vm");
        assert!(matches!(first, Err(Refusal::Broken(_))), "{first:?}");
        assert_eq!(books.launched.get(), 1, "the first VM is handed it");
        assert!(books.records.borrow().is_empty(), "the first VM is gone");
        let second = create(&runtime, &books, "second-vm");
        assert_eq!(
            books.launched.get(),
            1,
            "a second VM on the same boot is refused before its VMM: {second:?}"
        );
        refused_for_its_boot(&second);
        assert!(books.records.borrow().is_empty(), "nothing of it is kept");
        runtime.boot = boot("0f9e8d7c-6b5a-4c3d-8e1f-2a3b4c5d6e7f");
        let third = create(&runtime, &books, "third-vm");
        assert!(matches!(third, Err(Refusal::Broken(_))), "{third:?}");
        assert_eq!(books.launched.get(), 2, "a later boot's VM is handed it");
    }

    /// The lead's ruling of 2026-11-11 (AC-T7.4.1; original): a function's mark lay in its node's own state, so a node
    /// with another state directory on the same host and boot (a fresh one, or the lab's next run) handed the same
    /// spent function out again. Do: on one stand-in host and boot, create a VM given a function no reset clears on
    /// one node, its VMM ending at once, then on a second node with another state directory but the host's runtime
    /// directory. Expect the first handed it and the second refused by type before its VMM.
    #[test]
    fn original_another_node_on_the_host_is_refused_a_function_one_node_spent_this_boot() {
        let dir = tempfile::tempdir().unwrap();
        stand_in(&dir.path().join("host"));
        let owner: Arc<dyn Books> = Arc::new(super::handoff_tests::NoOwnerCalls);
        let [first, second] = ["first-node", "second-node"].map(|name| {
            node(
                &dir.path().join(name),
                &dir.path().join("host"),
                &dir.path().join("runtime"),
                GPU,
                &owner,
            )
        });
        let (first_books, second_books) = (Handed::default(), Handed::default());
        let given = create(&first, &first_books, "first-vm");
        assert!(matches!(given, Err(Refusal::Broken(_))), "{given:?}");
        assert_eq!(first_books.launched.get(), 1, "the first node hands it out");
        let refused = create(&second, &second_books, "second-vm");
        assert_eq!(
            second_books.launched.get(),
            0,
            "another node on the same boot is refused it before its VMM: {refused:?}"
        );
        refused_for_its_boot(&refused);
    }

    /// The lead's review of 3cea2fcf (AC-T7.4.1; original): a rootless node with no absolute `$XDG_RUNTIME_DIR` keeps its
    /// runtime directory in its own state, which no other node on the host reads, so another node on the same boot handed
    /// out a function one node had spent (r940's failure). Do: on one stand-in host and boot, create a VM given a function
    /// no reset clears on two nodes whose runtime directories are each their own. Expect each refused by type before its
    /// VMM, saying the directory is the node's own and why.
    #[test]
    fn original_a_node_whose_runtime_directory_is_its_own_gives_out_no_single_use_function() {
        let dir = tempfile::tempdir().unwrap();
        stand_in(&dir.path().join("host"));
        let owner: Arc<dyn Books> = Arc::new(super::handoff_tests::NoOwnerCalls);
        let why = "a rootless node with no absolute $XDG_RUNTIME_DIR keeps it in its own state";
        for name in ["first-node", "second-node"] {
            let mut node = node(
                &dir.path().join(name),
                &dir.path().join("host"),
                &dir.path().join(name).join("oci"),
                GPU,
                &owner,
            );
            node.marks.unshared = Some(why.to_owned());
            let books = Handed::default();
            let refused = create(&node, &books, "vm");
            assert_eq!(
                books.launched.get(),
                0,
                "{name}: refused before its VMM: {refused:?}"
            );
            let Err(Refusal::Platform { why: said, .. }) = &refused else {
                panic!("{name}: refused by type: {refused:?}");
            };
            for named in ["0000:01:00.0", "its own", why] {
                assert!(said.contains(named), "{named}: {said}");
            }
        }
    }

    /// The lead's review of 3cea2fcf (AC-T7.4.1; original): a root node opens any VFIO group, and marked a function in
    /// `/run/hex-node`, where a rootless node of the group's owner, marking in its own runtime directory, never looked.
    /// Do: create a VM given a function no reset clears on a node whose user isn't its group's owner. Expect it refused
    /// by type before its VMM, naming the group's owner.
    #[test]
    fn original_a_node_is_refused_a_single_use_function_another_users_group_holds() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        stand_in(&dir.path().join("host"));
        let owner: Arc<dyn Books> = Arc::new(super::handoff_tests::NoOwnerCalls);
        let mut node = node(
            &dir.path().join("node"),
            &dir.path().join("host"),
            &dir.path().join("runtime"),
            GPU,
            &owner,
        );
        let group = fs::metadata(dir.path().join("host/dev/vfio/7"))
            .unwrap()
            .uid();
        node.uid = if group == 0 { 1 } else { 0 };
        let books = Handed::default();
        let refused = create(&node, &books, "vm");
        assert_eq!(
            books.launched.get(),
            0,
            "refused before its VMM: {refused:?}"
        );
        let Err(Refusal::Platform { why, .. }) = &refused else {
            panic!("refused by type: {refused:?}");
        };
        for named in ["0000:01:00.0".to_owned(), format!("user {group}")] {
            assert!(why.contains(&named), "{named}: {why}");
        }
    }

    /// One node's records, empty, over a host of ample room: what a backend's capacity reads.
    struct Room;

    impl Books for Room {
        fn records(&self) -> Result<Records, Refusal> {
            Ok(Records::new())
        }
        fn pool_records(&self) -> Result<PoolRecords, Refusal> {
            Ok(PoolRecords::new())
        }
        fn prepare_pool(
            &self,
            _: [u8; 32],
            _: Execution,
            _: hex_api::RuntimeCost,
            _: &[DeviceId],
            _: (&[u8; 32], &[DeviceId], Option<&PoolRecord>),
            _: &mut crate::devices::PoolPreparationCheck<'_>,
        ) -> Result<PoolRecord, Refusal> {
            panic!("a host of no GPU prepares no pool")
        }
        fn capacity(&self) -> Result<hex_api::Capacity, Refusal> {
            let room = CapacityResources {
                cpu_milli: 64_000,
                mem_bytes: 1 << 40,
                disk_bytes: 1 << 40,
            };
            Ok(hex_api::Capacity {
                id: "node:stand-in".into(),
                total: room,
                free: room,
                devices: Vec::new(),
                cost: None,
                routes: Vec::new(),
                device_routes: Vec::new(),
                backings: Vec::new(),
                unavailable: Vec::new(),
                learned: None,
            })
        }
        fn physical_devices(&self) -> Result<Vec<hex_api::DeviceInfo>, Refusal> {
            panic!("a backend's capacity probes nothing")
        }
        fn with_transaction(
            &self,
            _: &BoxId,
            _: &[DeviceId],
            _: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
        ) -> Result<(), Refusal> {
            panic!("a backend's capacity enters no transaction")
        }
        fn with_pool_transaction(
            &self,
            _: &hex_api::BackingKey,
            _: &[DeviceId],
            _: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
        ) -> Result<(), Refusal> {
            panic!("a backend's capacity enters no transaction")
        }
        fn with_handoff_transaction(
            &self,
            _: &BoxId,
            _: &hex_api::BackingKey,
            _: &[DeviceId],
            _: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
        ) -> Result<(), Refusal> {
            panic!("a backend's capacity enters no transaction")
        }
    }

    /// The lead's ruling of 2026-11-11 (AC-T7.4.1; original): a VFIO function no reset clears, spent on this boot, read
    /// as free in the backend's capacity, so placement could choose it and only its create refused it. Do: mark a
    /// stand-in VFIO group's one function given on this boot, as a VM given it leaves it, and read the backend's
    /// capacity. Expect the group listed, and listed unavailable with why: no reset method, the host must reboot.
    #[test]
    fn original_a_spent_function_is_listed_unavailable_with_why() {
        let dir = tempfile::tempdir().unwrap();
        stand_in(&dir.path().join("host"));
        let owner: Arc<dyn Books> = Arc::new(Room);
        let node = node(
            &dir.path().join("node"),
            &dir.path().join("host"),
            &dir.path().join("runtime"),
            b"hex-devices-1\nvfio\t7\tgroup-7\t0\tvfio\t0000:01:00.0\t7\n",
            &owner,
        );
        node.initialized.set(()).unwrap();
        single_use::Marks::of(&dir.path().join("runtime"), node.boot.as_ref())
            .unwrap()
            .spend(&["0000:01:00.0"])
            .unwrap();
        let group = DeviceId {
            kind: DeviceKind::Vfio,
            key: 7,
        };
        let capacity = node.capacity(Class::Firecracker).unwrap().unwrap();
        assert!(
            capacity.devices.iter().any(|device| device.id == group),
            "listed: {:?}",
            capacity.devices
        );
        let [unavailable] = &capacity.unavailable[..] else {
            panic!("listed unavailable once: {:?}", capacity.unavailable);
        };
        assert_eq!(unavailable.id, group);
        for named in ["0000:01:00.0", "no reset method", "the host must reboot"] {
            assert!(
                unavailable.why.contains(named),
                "{named}: {}",
                unavailable.why
            );
        }
    }
}
