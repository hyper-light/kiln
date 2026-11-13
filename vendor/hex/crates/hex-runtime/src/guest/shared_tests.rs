//! AC-T7.4.1: real production pool entrypoints over a controlled same-owner Books.
//! Counters are model oracles; no native process, VM or renderer is started here.

use super::super::tests::socket_fixture;
use super::*;
use crate::devices::{Books, PoolRecords, Records, Transaction};
use hex_api::{Capacity, CapacityResources};
use hex_core::record::{BoxRecord, RECORD_VERSION};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Default)]
struct Native {
    released: AtomicBool,
    stops: AtomicUsize,
    /// A stop ends what of the scope outlived its controller, as the helper's does, so the scope is Released.
    stop_releases: AtomicBool,
    /// The controller a launch published, where the model has one.
    identity: std::sync::OnceLock<shards_api::Identity>,
}

impl shards_api::ManagedProcess for Native {
    fn identity(&self) -> Result<&shards_api::Identity, shards_api::Refusal> {
        self.identity.get().ok_or(shards_api::Refusal::Launch(
            shards_api::LaunchError::Observer,
        ))
    }
    fn stop(&self) -> Result<(), shards_api::Refusal> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        if self.stop_releases.load(Ordering::SeqCst) {
            self.released.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
    fn terminal(&self) -> Result<Option<shards_api::ManagedEnd>, shards_api::Refusal> {
        Ok(Some(shards_api::ManagedEnd {
            status: shards_api::ManagedStatus::Code(7),
            scope: if self.released.load(Ordering::SeqCst) {
                shards_api::TerminalScope::Released
            } else {
                shards_api::TerminalScope::Process
            },
        }))
    }
    fn wait_for(&self, _: Duration) -> Result<Option<shards_api::ManagedEnd>, shards_api::Refusal> {
        Ok(self
            .terminal()?
            .filter(|end| end.scope == shards_api::TerminalScope::Released))
    }
}

struct Owner {
    pool: Mutex<Option<PoolRecord>>,
    boxes: Mutex<Records>,
    launches: AtomicUsize,
    /// Each launch's kernel command line.
    cmdlines: Mutex<Vec<String>>,
    preparations: AtomicUsize,
    removals: AtomicUsize,
    submission: Mutex<Submission>,
    submission_checks: AtomicUsize,
    capacity_calls: AtomicUsize,
    finalized: AtomicUsize,
    fail_finalize: AtomicBool,
    native: triomphe::Arc<Native>,
}

#[derive(Clone, Copy)]
enum Submission {
    Absent,
    Present,
    Unavailable,
}

impl Owner {
    fn new() -> Self {
        Self {
            pool: Mutex::new(None),
            boxes: Mutex::new(Records::new()),
            launches: AtomicUsize::new(0),
            cmdlines: Mutex::new(Vec::new()),
            preparations: AtomicUsize::new(0),
            removals: AtomicUsize::new(0),
            submission: Mutex::new(Submission::Absent),
            submission_checks: AtomicUsize::new(0),
            capacity_calls: AtomicUsize::new(0),
            finalized: AtomicUsize::new(0),
            fail_finalize: AtomicBool::new(false),
            native: triomphe::Arc::try_new(Native::default()).unwrap(),
        }
    }
    fn row(&self) -> PoolRecord {
        let held = self.pool.lock().unwrap();
        let row = held.as_ref().unwrap();
        PoolRecord::decode(&row.encode().unwrap()).unwrap()
    }

    // Model the SAME native owner's read-only metadata check, not a terminal
    // scope witness. Both cancellation and its later ACK must invoke it.
    fn check_native_unsubmitted(&self, key: BackingKey) -> Result<(), Refusal> {
        self.submission_checks.fetch_add(1, Ordering::SeqCst);
        match *self.submission.lock().unwrap() {
            Submission::Absent if self.launches.load(Ordering::SeqCst) == 0 => Ok(()),
            Submission::Unavailable => Err(Refusal::os(
                "checking model native submission metadata",
                std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            )),
            Submission::Absent | Submission::Present => {
                Err(refused(key, LifecycleCause::Uncertain))
            }
        }
    }
}

impl Transaction for Owner {
    fn check_unsubmitted_pool(&self) -> Result<(), Refusal> {
        let row = self.row();
        if row.phase != BackingPhase::Prepared
            || row.allocation.is_some()
            || row.init.is_some()
            || row.exit.is_some()
        {
            return Err(refused(row.key, LifecycleCause::Uncertain));
        }
        self.check_native_unsubmitted(row.key)
    }
    fn records(&self) -> Result<Records, Refusal> {
        Ok(self.boxes.lock().unwrap().clone())
    }
    fn pool_records(&self) -> Result<PoolRecords, Refusal> {
        Ok(self
            .pool
            .lock()
            .unwrap()
            .as_ref()
            .map(|row| PoolRecord::decode(&row.encode().unwrap()).unwrap())
            .into_iter()
            .collect())
    }
    fn read_pool(&self, key: &BackingKey) -> Result<Option<PoolRecord>, Refusal> {
        Ok(Transaction::pool_records(self)?
            .into_iter()
            .find(|row| row.key == *key))
    }
    fn put_pool(&self, row: &PoolRecord) -> Result<(), Refusal> {
        row.check()?;
        let mut held = self.pool.lock().unwrap();
        held.as_ref().unwrap().accepts(row)?;
        *held = Some(PoolRecord::decode(&row.encode()?)?);
        Ok(())
    }
    fn grant_pool(&self, grant: &BackingGrant) -> Result<PoolRecord, Refusal> {
        let mut row = self.row();
        row.accept_grant(grant)?;
        self.put_pool(&row)?;
        Ok(row)
    }
    fn remove_pool(&self, key: &BackingKey) -> Result<(), Refusal> {
        let boxes = self.boxes.lock().unwrap();
        if boxes.values().any(|boxed| matches!(boxed, Ok(boxed) if boxed.spec.route.and_then(|route| route.backing) == Some(*key))) {
            return Err(refused(*key, LifecycleCause::Busy));
        }
        let mut held = self.pool.lock().unwrap();
        let row = held
            .as_ref()
            .ok_or_else(|| refused(*key, LifecycleCause::Uncertain))?;
        if row.key != *key || row.phase != BackingPhase::Released {
            return Err(refused(*key, LifecycleCause::Uncertain));
        }
        if row.allocation.is_none() && row.init.is_none() && row.exit.is_none() {
            self.check_native_unsubmitted(*key)?;
        } else if row.allocation.is_none()
            || row.exit != Some(hex_api::Exit::Code(7))
            || !self.native.released.load(Ordering::SeqCst)
            || self.finalized.load(Ordering::SeqCst) == 0
        {
            return Err(refused(*key, LifecycleCause::Uncertain));
        }
        *held = None;
        self.removals.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn read(&self, id: &BoxId) -> Result<Option<BoxRecord>, Refusal> {
        Ok(self
            .boxes
            .lock()
            .unwrap()
            .get(id)
            .map(|row| row.as_ref().unwrap().clone()))
    }
    fn put(&self, row: &BoxRecord) -> Result<(), Refusal> {
        self.boxes
            .lock()
            .unwrap()
            .insert(row.spec.id.clone(), Ok(row.clone()));
        Ok(())
    }
    fn remove(&self, id: &BoxId) -> Result<(), Refusal> {
        self.boxes.lock().unwrap().remove(id);
        Ok(())
    }
    fn launch_managed(
        &self,
        plan: &devices::ManagedLaunch<'_>,
        _: shards_api::LaunchIo,
        _: Duration,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        let devices::ManagedLaunch::Shards(prepared) = plan else {
            panic!("a shared backing launches Shards")
        };
        assert_eq!(self.row().phase, BackingPhase::Granted);
        let cmdline = prepared
            .arguments()
            .iter()
            .skip_while(|word| word.as_os_str() != "--cmdline")
            .nth(1)
            .map(|cmdline| cmdline.to_string_lossy().into_owned());
        self.cmdlines.lock().unwrap().extend(cmdline);
        self.launches.fetch_add(1, Ordering::SeqCst);
        Err(refused(self.row().key, LifecycleCause::Uncertain))
    }
    fn attach_managed(
        &self,
        identity: Option<&shards_api::Identity>,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        assert_eq!(
            identity.map(controller),
            self.row().init,
            "an attach names the controller the canonical row published, or none before it"
        );
        Ok(shards_api::managed_receipt(self.native.clone()))
    }
    fn finalize_managed(&self, identity: Option<&shards_api::Identity>) -> Result<(), Refusal> {
        assert_eq!(identity.map(controller), self.row().init);
        assert!(self.native.released.load(Ordering::SeqCst));
        if self.fail_finalize.load(Ordering::SeqCst) {
            return Err(refused(self.row().key, LifecycleCause::Uncertain));
        }
        self.finalized.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn managed_usage(&self) -> Result<Usage, Refusal> {
        panic!("no native counters are measured by these controls")
    }
}

impl Books for Owner {
    fn with_handoff_transaction(
        &self,
        _: &hex_api::BoxId,
        _: &hex_api::BackingKey,
        _: &[hex_api::DeviceId],
        _: &mut dyn FnMut(&dyn crate::devices::Transaction) -> Result<(), hex_api::Refusal>,
    ) -> Result<(), hex_api::Refusal> {
        panic!("existing test owner must not enter a subset handoff")
    }

    fn records(&self) -> Result<Records, Refusal> {
        Transaction::records(self)
    }
    fn pool_records(&self) -> Result<PoolRecords, Refusal> {
        Transaction::pool_records(self)
    }
    fn prepare_pool(
        &self,
        pool: [u8; 32],
        execution: Execution,
        cost: hex_api::RuntimeCost,
        devices: &[hex_api::DeviceId],
        family: (&[u8; 32], &[hex_api::DeviceId], Option<&PoolRecord>),
        validate: &mut crate::devices::PoolPreparationCheck<'_>,
    ) -> Result<PoolRecord, Refusal> {
        let snapshot = Books::pool_records(self)?;
        assert_eq!(snapshot.iter().find(|row| row.key.pool == pool), family.2);
        validate(&Books::records(self)?, &snapshot)?;
        let mut held = self.pool.lock().unwrap();
        if let Some(row) = held.as_ref() {
            assert_eq!(row.key.pool, pool);
            return PoolRecord::decode(&row.encode()?);
        }
        let generation = 7u8
            .checked_add(u8::try_from(self.preparations.fetch_add(1, Ordering::SeqCst)).unwrap())
            .unwrap();
        let row = PoolRecord {
            version: RECORD_VERSION,
            key: BackingKey {
                pool,
                generation: [generation; 16],
            }, // Deterministic bounded model stream, not native entropy.
            execution,
            cost,
            allocation: None,
            devices: devices.to_vec(),
            phase: BackingPhase::Prepared,
            init: None,
            exit: None,
        };
        let returned = PoolRecord::decode(&row.encode()?)?;
        *held = Some(row);
        Ok(returned)
    }
    fn physical_devices(&self) -> Result<Vec<hex_api::DeviceInfo>, Refusal> {
        Ok(Vec::new())
    }
    fn capacity(&self) -> Result<Capacity, Refusal> {
        self.capacity_calls.fetch_add(1, Ordering::SeqCst);
        let room = CapacityResources {
            cpu_milli: 8000,
            mem_bytes: 1 << 30,
            disk_bytes: 1 << 30,
        };
        Ok(Capacity {
            id: "same-model-native-owner".into(),
            total: room,
            free: room,
            cost: None,
            devices: Vec::new(),
            routes: Vec::new(),
            device_routes: Vec::new(),
            backings: Vec::new(),
            unavailable: Vec::new(),
            learned: None,
        })
    }
    fn with_transaction(
        &self,
        _: &BoxId,
        _: &[hex_api::DeviceId],
        callback: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        callback(self)
    }
    fn with_pool_transaction(
        &self,
        key: &BackingKey,
        devices: &[hex_api::DeviceId],
        callback: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        assert_eq!(self.row().key, *key);
        assert!(devices.is_empty(), "these shared CPU controls claim no GPU");
        callback(self)
    }
}

fn configured(path: &Path) -> (GuestRuntime, Arc<Owner>) {
    let owner = Arc::new(Owner::new());
    (configured_for(path, &owner), owner)
}

/// A runtime of the same configuration bound to `owner`'s books: a restarted node's.
fn configured_for(path: &Path, owner: &Arc<Owner>) -> GuestRuntime {
    let mut config = super::super::tests::config();
    config.binary = path.join("shards");
    config.kernel = path.join("kernel");
    config.initrd = path.join("initrd");
    config.shares.clear();
    let floor = selected::resources(&config).unwrap();
    let cost = hex_api::RuntimeCost {
        id: [1; 32],
        floor,
        ceiling: floor,
        quantum: CapacityResources {
            cpu_milli: 1000,
            mem_bytes: 1 << 20,
            disk_bytes: 1,
        },
        guest_overhead: CapacityResources {
            cpu_milli: 1,
            mem_bytes: 1,
            disk_bytes: 0,
        },
        overhead: CapacityResources {
            cpu_milli: 1,
            mem_bytes: 1,
            disk_bytes: 0,
        },
    };
    let runtime = GuestRuntime::new(
        Arc::new(shards_api::sim::SimGuests::new(|_| {
            shards_api::End::PowerOff
        })),
        config,
    )
    .recorded_at(path.join("@guest"))
    .with_runtime_paths(
        shards_api::RuntimePaths::new(path.join("cache"), path.join("home")).unwrap(),
    )
    .with_runtime_cost(cost)
    .unwrap();
    let books: Arc<dyn Books> = owner.clone();
    runtime.bind_books(Arc::downgrade(&books)).unwrap();
    runtime
}

fn grant(offer: &BackingOffer) -> BackingGrant {
    BackingGrant {
        key: offer.key,
        allocation: offer.allocation().unwrap(),
        devices: Vec::new(),
    }
}

/// The canonical row's name for a published controller.
fn controller(identity: &shards_api::Identity) -> hex_core::record::ProcId {
    hex_core::record::ProcId {
        pid: identity.pid,
        start_ticks: identity.started,
    }
}

/// AC-T7.4.1: restart a node whose shared backing its last start prepared, and recover it without
/// preparing it again; expect recovery refused by name: a retained shared scope is never passed over
/// as one with nothing to recover (a dedicated-only node's, gpu_pool's
/// a_fresh_dedicated_gpu_node_recovers_with_no_shared_backing_to_recover).
#[test]
fn a_retained_shared_backing_is_refused_recovery_until_prepared() {
    let dir = tempfile::tempdir().unwrap();
    let (first, owner) = configured(dir.path());
    let offer = first.prepare_backing().unwrap();
    let restarted = configured_for(dir.path(), &owner);
    assert_eq!(
        restarted.recover_backing(),
        Err(Refusal::Invalid {
            what: "guest execution route",
            why: "prepare the shared backing before recovery".into(),
        })
    );
    assert_eq!(restarted.prepare_backing().unwrap().key, offer.key);
    restarted.recover_backing().unwrap();
    assert_eq!(owner.launches.load(Ordering::SeqCst), 0);
}

#[test]
fn shared_offer_prepares_before_grant_without_a_boot_or_fake_workload_row() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, owner) = configured(dir.path());
    let offer = runtime.prepare_backing().unwrap();
    assert_eq!(offer.phase, BackingPhase::Prepared);
    let capacity = runtime.capacity(Class::Oci).unwrap().unwrap();
    assert_eq!(capacity.backings.len(), 1);
    assert_eq!(capacity.backings[0].key, offer.key);
    assert_eq!(capacity.routes[0].key.execution, Execution::ShardsShared);
    assert_eq!(capacity.routes[0].cost, None);
    assert!(capacity.devices.is_empty());
    assert_eq!(owner.launches.load(Ordering::SeqCst), 0);
    assert!(owner.boxes.lock().unwrap().is_empty());
    assert_eq!(owner.row().allocation, None);
    assert!(pool::read(&dir.path().join("@guest")).unwrap().is_none());
}

#[test]
fn shared_retained_preflight_rejects_replaced_profile_key_or_generation_before_effects() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, owner) = configured(dir.path());
    runtime.prepare_backing().unwrap();
    runtime.preflight_backing(&owner.row()).unwrap();
    let mut changed = owner.row();
    changed.key.pool = [8; 32];
    assert!(runtime.preflight_backing(&changed).is_err());
    changed = owner.row();
    changed.key.generation = [8; 16];
    assert!(runtime.preflight_backing(&changed).is_err());
    changed = owner.row();
    changed.cost.id = [8; 32];
    assert!(runtime.preflight_backing(&changed).is_err());
    assert_eq!(owner.launches.load(Ordering::SeqCst), 0);
    assert!(owner.boxes.lock().unwrap().is_empty());
    assert!(pool::read(&dir.path().join("@guest")).unwrap().is_none());
}

/// A retained configuration key includes its original cost; it must ALSO
/// match the currently initialized measured cost before any owner effect.
#[test]
fn retained_backing_refuses_a_replaced_initialized_cost_without_inner_boxes() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, owner) = configured(dir.path());
    runtime.prepare_backing().unwrap();
    let original = owner.row();
    let (mut replaced, replacement_owner) = configured(dir.path());
    replaced.runtime_cost.as_mut().unwrap().id = [8; 32];
    assert!(replaced.preflight_backing(&original).is_err());
    assert_eq!(owner.launches.load(Ordering::SeqCst), 0);
    assert_eq!(replacement_owner.launches.load(Ordering::SeqCst), 0);
    assert!(owner.boxes.lock().unwrap().is_empty());
}

/// AC-T7.4.1 (original): a managed Shards boot tells its guest the boot's deadline as a CPU boot does, so its relay's
/// hello to its node takes it as its caller's. Do: initialize a shared backing, whose launch the owner reads, then
/// refuses. Expect the launch's kernel command line to name the boot's deadline once (`hex.ready-ms=`): what is left
/// of the create's budget, the first dial's bound, in milliseconds rounded up.
#[test]
fn original_a_managed_shards_boot_tells_its_guest_the_boots_deadline() {
    let dir = socket_fixture();
    let (runtime, owner) = configured(dir.path());
    let bound = runtime.first_dial_bound().unwrap();
    let offer = runtime.prepare_backing().unwrap();
    let initialized = runtime.initialize_backing(grant(&offer)).map(drop);
    let cmdlines = owner.cmdlines.lock().unwrap().clone();
    eprintln!(
        "hex_guest_ready_word boot=managed bound={bound:?} initialized={initialized:?} cmdlines={cmdlines:?}"
    );
    assert_eq!(owner.launches.load(Ordering::SeqCst), 1);
    assert_eq!(cmdlines.len(), 1);
    let given = super::super::tests::ready_ms(&cmdlines[0]);
    assert!(
        !given.is_zero() && given <= bound + Duration::from_millis(1),
        "{given:?} isn't what is left of the boot's bound {bound:?}"
    );
}

#[test]
fn interrupted_shared_launch_keeps_the_exact_charge_and_never_reboots_an_intent() {
    let dir = socket_fixture();
    let (runtime, owner) = configured(dir.path());
    let offer = runtime.prepare_backing().unwrap();
    let mut wrong = grant(&offer);
    wrong.key.generation = [8; 16];
    assert!(runtime.initialize_backing(wrong).is_err());
    assert_eq!(owner.launches.load(Ordering::SeqCst), 0);
    assert!(runtime.initialize_backing(grant(&offer)).is_err());
    assert_eq!(owner.launches.load(Ordering::SeqCst), 1);
    let receipt = pool::read(&dir.path().join("@guest")).unwrap().unwrap();
    assert_eq!(
        receipt.original.allocation,
        Some(offer.allocation().unwrap())
    );
    assert_eq!(receipt.original.scope(), owner.row().scope());
    assert!(matches!(receipt.phase, Phase::Intent));
    assert!(runtime.initialize_backing(grant(&offer)).is_err());
    assert_eq!(owner.launches.load(Ordering::SeqCst), 1);
    assert_eq!(owner.row().phase, BackingPhase::Granted);
    assert!(owner.boxes.lock().unwrap().is_empty());
}

#[test]
fn shared_charge_survives_controller_only_exit_and_failed_native_finalization() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, owner) = configured(dir.path());
    let offer = runtime.prepare_backing().unwrap();
    assert!(runtime.initialize_backing(grant(&offer)).is_err());
    assert!(runtime.release_backing(offer.key).is_err());
    assert_eq!(owner.row().phase, BackingPhase::Draining);
    assert_eq!(owner.row().allocation, Some(offer.allocation().unwrap()));
    owner.native.released.store(true, Ordering::SeqCst);
    owner.fail_finalize.store(true, Ordering::SeqCst);
    assert!(runtime.release_backing(offer.key).is_err());
    let stops = owner.native.stops.load(Ordering::SeqCst);
    assert_eq!(owner.row().phase, BackingPhase::Draining);
    assert!(matches!(
        pool::read(&dir.path().join("@guest"))
            .unwrap()
            .unwrap()
            .phase,
        Phase::Released { .. }
    ));
    owner.fail_finalize.store(false, Ordering::SeqCst);
    let released = runtime.release_backing(offer.key).unwrap();
    assert_eq!(released.phase, BackingPhase::Released);
    assert_eq!(owner.row().exit, Some(hex_api::Exit::Code(7)));
    assert_eq!(owner.native.stops.load(Ordering::SeqCst), stops);
    assert_eq!(owner.finalized.load(Ordering::SeqCst), 1);
}

#[test]
fn shared_workload_reference_refuses_release_before_any_native_stop() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, owner) = configured(dir.path());
    let offer = runtime.prepare_backing().unwrap();
    assert!(runtime.initialize_backing(grant(&offer)).is_err());
    let mut spec = super::super::tests::oci(
        hex_api::Rootfs::Dir(dir.path().join("rootfs.tar")),
        Vec::new(),
    );
    spec.route = Some(hex_api::RouteReceipt {
        key: runtime.capacity(Class::Oci).unwrap().unwrap().routes[0].key,
        backing: Some(offer.key),
    });
    Transaction::put(
        owner.as_ref(),
        &BoxRecord::new(spec, hex_core::record::Phase::Creating, Vec::new()),
    )
    .unwrap();
    assert!(matches!(
        runtime.release_backing(offer.key),
        Err(Refusal::Backing {
            cause: LifecycleCause::Busy,
            ..
        })
    ));
    assert_eq!(owner.native.stops.load(Ordering::SeqCst), 0);
    assert_eq!(owner.row().phase, BackingPhase::Granted);
}

#[test]
fn ended_shared_scope_allows_inner_cleanup_while_the_original_grant_stays_charged() {
    let dir = socket_fixture();
    let (runtime, owner) = configured(dir.path());
    let offer = runtime.prepare_backing().unwrap();
    assert!(runtime.initialize_backing(grant(&offer)).is_err());
    let mut spec = super::super::tests::oci(
        hex_api::Rootfs::Dir(dir.path().join("rootfs.tar")),
        Vec::new(),
    );
    spec.route = Some(hex_api::RouteReceipt {
        key: runtime.capacity(Class::Oci).unwrap().unwrap().routes[0].key,
        backing: Some(offer.key),
    });
    let id = spec.id.clone();
    Transaction::put(
        owner.as_ref(),
        &BoxRecord::new(spec, hex_core::record::Phase::Creating, Vec::new()),
    )
    .unwrap();
    owner.native.released.store(true, Ordering::SeqCst);
    runtime.recover_backing().unwrap();
    assert_eq!(owner.row().phase, BackingPhase::Draining);
    assert_eq!(owner.row().allocation, Some(offer.allocation().unwrap()));
    assert_eq!(runtime.inspect(&id), Err(Refusal::NoSuchBox(id.clone())));
    runtime.delete(&id).unwrap();
    Transaction::remove(owner.as_ref(), &id).unwrap();
    runtime.release_backing(offer.key).unwrap();
    assert_eq!(
        owner.native.stops.load(Ordering::SeqCst),
        0,
        "no stop of a scope already proved empty"
    );
    assert_eq!(
        owner.launches.load(Ordering::SeqCst),
        1,
        "recovery never boots another VM"
    );
}

/// AC-T7.4.1 (restart, controller gone; the original of macos_pool_restart's restarted node that dialed the
/// gone VM's guest until its deadline and exited): a shared pool Live under its published controller, which
/// ended while no node ran as a member of its scope outlived it, so the native end is the controller's alone
/// (`Process`, not `Released`). Do: recover it on a restarted runtime. Expect recovery to take nothing back and
/// boot nothing: the survivor ended by one stop of the same scope, which releases it, and the pool Draining with
/// the controller's exit recorded; then its release finalizes that scope without a second stop.
#[test]
fn a_pool_whose_controller_ended_while_no_node_ran_drains_and_its_survivor_is_ended() {
    let dir = socket_fixture();
    let (runtime, owner) = configured(dir.path());
    let offer = runtime.prepare_backing().unwrap();
    // The model's launch refuses after its intent; the row and receipt then say what a node that crashed
    // after making the pool Live leaves: its controller published, the pool Live, the receipt Running.
    assert!(runtime.initialize_backing(grant(&offer)).is_err());
    let identity = shards_api::Identity {
        pid: 41,
        started: 43,
        vsock: runtime.pool_socket(offer.key).unwrap(),
    };
    let mut row = owner.row();
    row.init = Some(controller(&identity));
    Transaction::put_pool(owner.as_ref(), &row).unwrap();
    row.phase = BackingPhase::Live;
    Transaction::put_pool(owner.as_ref(), &row).unwrap();
    let path = dir.path().join("@guest");
    let mut receipt = pool::read(&path).unwrap().unwrap();
    receipt.phase = Phase::Running(copy_identity(&identity).unwrap());
    pool::write(&path, &receipt).unwrap();
    owner.native.identity.set(identity).unwrap();
    owner.native.stop_releases.store(true, Ordering::SeqCst);

    let restarted = configured_for(dir.path(), &owner);
    assert_eq!(restarted.prepare_backing().unwrap().key, offer.key);
    assert_eq!(
        restarted.recover_backing(),
        Ok(()),
        "recovery refused a pool whose controller ended"
    );
    assert_eq!(owner.row().phase, BackingPhase::Draining);
    assert_eq!(owner.row().exit, Some(hex_api::Exit::Code(7)));
    assert_eq!(
        owner.native.stops.load(Ordering::SeqCst),
        1,
        "recovery didn't end the survivor with one stop"
    );
    assert_eq!(
        owner.launches.load(Ordering::SeqCst),
        1,
        "recovery booted another VM"
    );
    let released = restarted.release_backing(offer.key).unwrap();
    assert_eq!(released.phase, BackingPhase::Released);
    assert_eq!(
        owner.native.stops.load(Ordering::SeqCst),
        1,
        "the release stopped an ended scope again"
    );
    assert_eq!(owner.finalized.load(Ordering::SeqCst), 1);
}

/// AC-T7.4.1: a checked renderer/native join can supply a factual device when
/// the host resource plane has no native-driver slot. No catalog placeholder
/// may replace it, and a conflicting existing identity must remain a refusal.
/// Original production is unchanged until this source control is reproduced.
#[test]
fn checked_gpu_facts_create_a_missing_capacity_slot_and_refuse_conflicting_metadata() {
    use hex_api::{
        DeviceHolder, DeviceId, DeviceInfo, DeviceKind, GpuEligibility, ImageOs, ImagePlatform,
        RouteKey, RouteOffer,
    };
    let dir = tempfile::tempdir().unwrap();
    let (runtime, owner) = configured(dir.path());
    let allocation = runtime.configured_allocation().unwrap();
    let id = DeviceId {
        kind: DeviceKind::Gpu,
        key: 256,
    };
    let key = BackingKey {
        pool: [8; 32],
        generation: [9; 16],
    };
    let backing = PoolRecord {
        version: RECORD_VERSION,
        key,
        execution: Execution::ShardsDedicated,
        cost: allocation.cost,
        allocation: Some(allocation),
        devices: vec![id],
        phase: BackingPhase::Live,
        init: Some(hex_core::record::ProcId {
            pid: 42,
            start_ticks: 7,
        }),
        exit: None,
    }
    .offer()
    .unwrap();
    let image = ImagePlatform {
        os: ImageOs::Linux,
        arch: route::host_platform().unwrap().arch,
    };
    let native = RouteOffer {
        key: RouteKey {
            class: Class::Process,
            image,
            execution: Execution::Native,
            gpu: GpuEligibility::None,
        },
        cost: None,
        backing: None,
    };
    let selected = RouteOffer {
        key: RouteKey {
            class: Class::Shards,
            image,
            execution: Execution::ShardsDedicated,
            gpu: GpuEligibility::Required,
        },
        cost: Some(allocation.cost),
        backing: Some(key),
    };
    // Explicit input from the checked-facts boundary, not a catalog or native
    // renderer invocation. This unit tests publication of those exact values.
    let observed = DeviceInfo {
        id,
        model: "checked renderer model input".into(),
        size: hex_api::MIN_MEMORY,
        holder: Some(DeviceHolder::Backing(key)),
    };
    observed.check().unwrap();
    for previous in [
        None,
        Some(observed.try_clone().unwrap()),
        Some(DeviceInfo {
            model: "conflicting native model input".into(),
            ..observed.try_clone().unwrap()
        }),
    ] {
        let conflict = previous
            .as_ref()
            .is_some_and(|info| info.model != observed.model);
        let mut capacity = Books::capacity(owner.as_ref()).unwrap();
        capacity.routes = vec![native];
        capacity.devices = previous.into_iter().collect();
        capacity.device_routes = vec![0; capacity.devices.len()];
        capacity.check().unwrap();
        // add_offers stages the canonical backing immediately before its route;
        // that intermediate pair is checked only after push_offer completes it.
        capacity.backings = vec![backing.try_clone().unwrap()];
        let result = push_offer(&mut capacity, selected, &[observed.try_clone().unwrap()]);
        if conflict {
            assert!(
                result.is_err(),
                "the same physical identity accepted conflicting measured metadata"
            );
        } else {
            result.unwrap();
            assert_eq!(capacity.routes, vec![native, selected]);
            assert_eq!(
                capacity.devices,
                vec![observed.try_clone().unwrap()],
                "checked factual GPU disappeared because the host resource plane had no slot"
            );
            assert_eq!(capacity.device_routes, vec![2]);
            assert_eq!(capacity.backings, vec![backing.try_clone().unwrap()]);
            capacity.check().unwrap();
        }
    }
    assert_eq!(owner.launches.load(Ordering::SeqCst), 0);
    assert!(owner.pool.lock().unwrap().is_none());
    assert!(owner.boxes.lock().unwrap().is_empty());
}

/// AC-T7.4.1: an empty native prefix has no physical slots to normalize. The
/// actual OCI wrapper must still add its initialized guest route, without a
/// Books capacity/provider call or an uncharged shared guest boot.
#[test]
fn empty_native_oci_prefix_adds_guest_route_without_native_facts_query() {
    use hex_api::{GpuEligibility, ImageOs, ImagePlatform, RouteKey, RouteOffer};
    let dir = tempfile::tempdir().unwrap();
    let (runtime, owner) = configured(dir.path());
    let shared = runtime.prepare_backing().unwrap();
    let image = ImagePlatform {
        os: ImageOs::Windows,
        arch: route::host_platform().unwrap().arch,
    };
    let native = Capacity {
        id: "native-cpu-only-prefix".into(),
        total: CapacityResources {
            cpu_milli: 8000,
            mem_bytes: 1 << 30,
            disk_bytes: 1 << 30,
        },
        free: CapacityResources {
            cpu_milli: 4000,
            mem_bytes: 1 << 29,
            disk_bytes: 1 << 29,
        },
        devices: Vec::new(),
        device_routes: Vec::new(),
        backings: Vec::new(),
        cost: None,
        routes: vec![
            RouteOffer {
                key: RouteKey {
                    class: Class::Process,
                    image,
                    execution: Execution::Native,
                    gpu: GpuEligibility::None,
                },
                cost: None,
                backing: None,
            },
            RouteOffer {
                key: RouteKey {
                    class: Class::Oci,
                    image,
                    execution: Execution::Native,
                    gpu: GpuEligibility::Any,
                },
                cost: None,
                backing: None,
            },
        ],
        unavailable: Vec::new(),
        learned: None,
    };
    native.check().unwrap();
    assert_eq!(owner.capacity_calls.load(Ordering::SeqCst), 0);
    let node = super::super::WithGuest::configured(
        super::super::tests::native_reporting(native.try_clone().unwrap()),
        runtime,
        |_host, _class| Ok(super::super::Side::Host),
    );
    let actual = node.capacity(Class::Oci).unwrap().unwrap();
    assert_eq!(
        owner.capacity_calls.load(Ordering::SeqCst),
        0,
        "empty native slots queried physical facts"
    );
    assert_eq!(actual.id, native.id);
    assert_eq!(actual.total, native.total);
    assert_eq!(actual.free, native.free);
    assert_eq!(actual.routes[..native.routes.len()], native.routes);
    assert!(actual.devices.is_empty());
    assert!(actual.device_routes.is_empty());
    assert!(
        actual
            .routes
            .iter()
            .any(|route| route.key.execution == Execution::ShardsShared
                && route.backing == Some(shared.key))
    );
    assert_eq!(actual.backings, [shared]);
    actual.check().unwrap();
    assert_eq!(owner.launches.load(Ordering::SeqCst), 0);
}

/// ORIGINAL AC-T7.4.1: observing the original positive scope end and an explicit
/// Index ACK must allow the SAME shared family to prepare a fresh incarnation.
/// This uses real runtime entrypoints; native outcomes are controlled model facts.
#[test]
fn shared_backing_rearms_only_after_original_scope_release_and_index_ack() {
    let dir = socket_fixture();
    let (runtime, owner) = configured(dir.path());
    let original = runtime.prepare_backing().unwrap();
    let allocation = original.allocation().unwrap();
    assert_eq!(original.phase, BackingPhase::Prepared);
    assert_eq!(owner.preparations.load(Ordering::SeqCst), 1);
    assert!(runtime.initialize_backing(grant(&original)).is_err());
    assert_eq!(owner.launches.load(Ordering::SeqCst), 1);
    assert_eq!(owner.row().key, original.key);
    assert_eq!(owner.row().allocation, Some(allocation));
    assert!(runtime.release_backing(original.key).is_err());
    assert_eq!(owner.row().phase, BackingPhase::Draining);
    assert_eq!(owner.row().allocation, Some(allocation));
    assert_eq!(owner.removals.load(Ordering::SeqCst), 0);
    assert_eq!(owner.finalized.load(Ordering::SeqCst), 0);
    // Positive SAME whole-scope outcome, not controller-only exit or socket EOF.
    owner.native.released.store(true, Ordering::SeqCst);
    let released = runtime.release_backing(original.key).unwrap();
    assert_eq!(released.key, original.key);
    assert_eq!(released.phase, BackingPhase::Released);
    assert_eq!(owner.row().exit, Some(hex_api::Exit::Code(7)));
    assert_eq!(owner.finalized.load(Ordering::SeqCst), 1);
    assert_eq!(owner.removals.load(Ordering::SeqCst), 0);
    assert!(pool::read(&dir.path().join("@guest")).unwrap().is_none());
    // Existing Driver observes this exact Released row before its ACK. No
    // scheduler/Index mutation or replacement is fabricated in this fixture.
    let observed = runtime.capacity(Class::Oci).unwrap().unwrap();
    observed.check().unwrap();
    assert_eq!(observed.backings, [released.try_clone().unwrap()]);
    assert_eq!(owner.preparations.load(Ordering::SeqCst), 1);
    let ack = runtime.release_backing(original.key).unwrap();
    assert_eq!(ack.key, released.key);
    assert_eq!(ack.phase, BackingPhase::Released);
    let stops = owner.native.stops.load(Ordering::SeqCst);
    let fresh = runtime.prepare_backing().unwrap();
    assert_ne!(
        fresh.key, original.key,
        "shared family remained pinned to its positively ended/acknowledged incarnation"
    );
    assert_eq!(fresh.key.pool, original.key.pool);
    assert_eq!(fresh.cost, original.cost);
    assert_eq!(fresh.phase, BackingPhase::Prepared);
    assert_eq!(fresh.allocation().unwrap(), allocation);
    assert_eq!(owner.row().key, fresh.key);
    assert_eq!(owner.row().allocation, None);
    assert_eq!(owner.row().init, None);
    assert_eq!(owner.row().exit, None);
    assert_eq!(owner.removals.load(Ordering::SeqCst), 1);
    assert_eq!(owner.preparations.load(Ordering::SeqCst), 2);
    assert_eq!(
        owner.launches.load(Ordering::SeqCst),
        1,
        "rearm booted an uncharged replacement"
    );
    assert_eq!(owner.native.stops.load(Ordering::SeqCst), stops);
    assert!(owner.boxes.lock().unwrap().is_empty());
}

/// AC-T7.4.1: explicit Index ACK cannot erase a remaining shared workload or
/// remove a later incarnation when the original key is replayed.
#[test]
fn shared_index_ack_preserves_references_and_refuses_a_stale_generation() {
    use hex_api::{GpuEligibility, ImageOs, ImagePlatform, RouteKey, RouteReceipt};
    let dir = socket_fixture();
    let (runtime, owner) = configured(dir.path());
    let original = runtime.prepare_backing().unwrap();
    assert!(runtime.initialize_backing(grant(&original)).is_err());
    assert_eq!(owner.launches.load(Ordering::SeqCst), 1);
    owner.native.released.store(true, Ordering::SeqCst);
    let released = runtime.release_backing(original.key).unwrap();
    assert_eq!(released.phase, BackingPhase::Released);
    assert_eq!(owner.finalized.load(Ordering::SeqCst), 1);
    let stops = owner.native.stops.load(Ordering::SeqCst);
    // A retained canonical workload is an explicit model input. The actual
    // existing native scope has ended; metadata retention still fences ACK.
    let mut spec =
        super::super::tests::oci(hex_api::Rootfs::Dir(dir.path().to_path_buf()), Vec::new());
    spec.id = BoxId::new("retained-shared-workload").unwrap();
    spec.bounds.cpu_max_milli = Some(hex_api::MIN_CPU_MILLI);
    spec.bounds.memory_max = Some(hex_api::MIN_MEMORY);
    spec.route = Some(RouteReceipt {
        key: RouteKey {
            class: Class::Oci,
            image: ImagePlatform {
                os: ImageOs::Linux,
                arch: route::host_platform().unwrap().arch,
            },
            execution: Execution::ShardsShared,
            gpu: GpuEligibility::None,
        },
        backing: Some(original.key),
    });
    spec.check().unwrap();
    let id = spec.id.clone();
    owner.boxes.lock().unwrap().insert(
        id.clone(),
        Ok(BoxRecord::new(
            spec,
            hex_core::record::Phase::Deleting,
            Vec::new(),
        )),
    );
    assert!(
        matches!(runtime.release_backing(original.key), Err(Refusal::Backing { key, cause: LifecycleCause::Busy }) if key == original.key)
    );
    assert_eq!(owner.removals.load(Ordering::SeqCst), 0);
    assert_eq!(owner.row().key, original.key);
    assert_eq!(runtime.prepare_backing().unwrap().key, original.key);
    owner.boxes.lock().unwrap().remove(&id);
    runtime.release_backing(original.key).unwrap();
    let fresh = runtime.prepare_backing().unwrap();
    assert_ne!(fresh.key, original.key);
    let row = owner.row().encode().unwrap();
    assert!(runtime.release_backing(original.key).is_err());
    assert_eq!(owner.row().encode().unwrap(), row);
    assert_eq!(owner.removals.load(Ordering::SeqCst), 1);
    assert_eq!(owner.preparations.load(Ordering::SeqCst), 2);
    assert_eq!(owner.launches.load(Ordering::SeqCst), 1);
    assert_eq!(owner.finalized.load(Ordering::SeqCst), 1);
    assert_eq!(owner.native.stops.load(Ordering::SeqCst), stops);
}

/// AC-T7.4.1: Prepared cancellation is a no-submission proof, not native scope
/// release. Its explicit ACK rechecks the same owner before canonical removal.
#[test]
fn shared_prepared_cancel_and_ack_recheck_native_absence_without_a_scope_end() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, owner) = configured(dir.path());
    let original = runtime.prepare_backing().unwrap();
    let before = owner.row().encode().unwrap();
    assert_eq!(original.phase, BackingPhase::Prepared);
    assert_eq!(owner.row().allocation, None);
    assert_eq!(owner.row().init, None);
    assert_eq!(owner.row().exit, None);
    assert!(pool::read(&dir.path().join("@guest")).unwrap().is_none());
    for metadata in [Submission::Present, Submission::Unavailable] {
        *owner.submission.lock().unwrap() = metadata;
        assert!(runtime.release_backing(original.key).is_err());
        assert_eq!(owner.row().encode().unwrap(), before);
        assert_eq!(runtime.shared_key().unwrap(), Some(original.key));
        assert_eq!(owner.removals.load(Ordering::SeqCst), 0);
    }
    *owner.submission.lock().unwrap() = Submission::Absent;
    let cancelled = runtime.release_backing(original.key).unwrap();
    assert_eq!(cancelled.key, original.key);
    assert_eq!(cancelled.phase, BackingPhase::Released);
    let row = owner.row();
    assert_eq!(row.allocation, None);
    assert_eq!(row.init, None);
    assert_eq!(row.exit, None, "metadata absence fabricated a native end");
    runtime.preflight_backing(&row).unwrap();
    assert!(pool::read(&dir.path().join("@guest")).unwrap().is_none());
    let checks = owner.submission_checks.load(Ordering::SeqCst);
    assert_eq!(checks, 3);
    assert!(Transaction::check_unsubmitted_pool(owner.as_ref()).is_err());
    assert_eq!(owner.submission_checks.load(Ordering::SeqCst), checks);
    let observed = runtime.capacity(Class::Oci).unwrap().unwrap();
    observed.check().unwrap();
    assert_eq!(observed.backings, [cancelled]);
    let before_ack = row.encode().unwrap();
    for metadata in [Submission::Present, Submission::Unavailable] {
        *owner.submission.lock().unwrap() = metadata;
        assert!(runtime.release_backing(original.key).is_err());
        assert_eq!(owner.row().encode().unwrap(), before_ack);
        assert_eq!(runtime.shared_key().unwrap(), Some(original.key));
        assert_eq!(owner.removals.load(Ordering::SeqCst), 0);
    }
    *owner.submission.lock().unwrap() = Submission::Absent;
    let ack = runtime.release_backing(original.key).unwrap();
    assert_eq!(ack.key, original.key);
    assert_eq!(ack.phase, BackingPhase::Released);
    assert_eq!(owner.submission_checks.load(Ordering::SeqCst), 6);
    assert!(owner.pool.lock().unwrap().is_none());
    assert_eq!(runtime.shared_key().unwrap(), None);
    let fresh = runtime.prepare_backing().unwrap();
    assert_ne!(fresh.key, original.key);
    assert_eq!(fresh.key.pool, original.key.pool);
    assert_eq!(fresh.phase, BackingPhase::Prepared);
    assert_eq!(owner.row().allocation, None);
    assert_eq!(owner.removals.load(Ordering::SeqCst), 1);
    assert_eq!(owner.preparations.load(Ordering::SeqCst), 2);
    assert_eq!(owner.launches.load(Ordering::SeqCst), 0);
    assert_eq!(owner.finalized.load(Ordering::SeqCst), 0);
    assert_eq!(owner.native.stops.load(Ordering::SeqCst), 0);
    assert!(!owner.native.released.load(Ordering::SeqCst));
    assert!(owner.boxes.lock().unwrap().is_empty());
}
