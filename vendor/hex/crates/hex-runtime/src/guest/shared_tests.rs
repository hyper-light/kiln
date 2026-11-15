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
    /// The scope ends only once its wait follows it with no bound, as a scope's own observer ends such a wait at its
    /// end (decision 9's following wait): any bounded wait for it sees no end.
    released_when_followed: AtomicBool,
    /// Each patience a wait for the scope was asked with.
    asked: Mutex<Vec<Duration>>,
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
    fn wait_for(
        &self,
        patience: Duration,
    ) -> Result<Option<shards_api::ManagedEnd>, shards_api::Refusal> {
        self.asked.lock().unwrap().push(patience);
        if patience == Duration::MAX && self.released_when_followed.load(Ordering::SeqCst) {
            self.released.store(true, Ordering::SeqCst);
        }
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
    /// Each launch's deadline, as what is left of it: none for one that follows its guest.
    patience: Mutex<Vec<Option<Duration>>>,
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
            patience: Mutex::new(Vec::new()),
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
        if boxes.values().any(|boxed| matches!(boxed, Ok(boxed) if !boxed.retired && boxed.spec.route.and_then(|route| route.backing) == Some(*key))) {
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
        patience: Option<Duration>,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        self.patience.lock().unwrap().push(patience);
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
            own: Default::default(),
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
            node_vms: Vec::new(),
            unlearned: None,
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
    config.boot = crate::guest::Boot::Kernel {
        kernel: path.join("kernel"),
        initrd: path.join("initrd"),
    };
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

/// A runtime bound to `owner`'s books whose profile is the census-free one (joint plan step 14,
/// `RuntimeCost::census_free`): a node restarted after decision 9's census-free cost.
fn configured_census_free(path: &Path, owner: &Arc<Owner>) -> GuestRuntime {
    let mut runtime = configured_for(path, owner);
    runtime.runtime_cost = runtime.runtime_cost.map(hex_api::RuntimeCost::census_free);
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

/// The review's C-9 (1) (decision 9): a managed Shards boot tells its guest no boot deadline, as the node's own guest is
/// told none, so its relay keeps its own learned bounds. Do: initialize a shared backing, whose launch the owner reads,
/// then refuses. Expect the launch's kernel command line to name no boot deadline (`hex.ready-ms`).
#[test]
fn c9_a_managed_shards_boot_tells_its_guest_no_boot_deadline() {
    let dir = socket_fixture();
    let (runtime, owner) = configured(dir.path());
    let offer = runtime.prepare_backing().unwrap();
    let initialized = runtime.initialize_backing(grant(&offer)).map(drop);
    let cmdlines = owner.cmdlines.lock().unwrap().clone();
    eprintln!(
        "hex_guest_ready_word boot=managed initialized={initialized:?} cmdlines={cmdlines:?}"
    );
    assert_eq!(owner.launches.load(Ordering::SeqCst), 1);
    assert_eq!(cmdlines.len(), 1);
    assert!(
        !cmdlines[0].contains(super::super::tests::READY_MS),
        "{cmdlines:?}"
    );
}

/// The review's C-9 (1) (decision 9; ROOT's (c1)): a managed create that isn't a GPU create follows its guest, its
/// budget no deadline of the record's, each wait in it within its own W* and past it following its party. Do: initialize
/// a shared backing, whose launch the owner reads, then refuses. Expect its native launch given no deadline.
#[test]
fn c9_a_shared_backings_launch_is_given_no_deadline() {
    let dir = socket_fixture();
    let (runtime, owner) = configured(dir.path());
    let offer = runtime.prepare_backing().unwrap();
    let initialized = runtime.initialize_backing(grant(&offer)).map(drop);
    let patience = owner.patience.lock().unwrap().clone();
    eprintln!("c9_shared_launch initialized={initialized:?} patience={patience:?}");
    assert_eq!(patience, [None], "its launch was given a deadline");
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

/// Joint plan step 14 (AC-T7.4.1; ROOT, 2026-11-13, by decision 9's rule: a census row is evidence only): a shared
/// backing its node recorded under a census cost is retired at the node's start, never leaked. Do prepare and grant a
/// shared backing under a cost whose census measured both overheads, with one box on it; then start a node whose
/// profile is the census-free one, prepare its backing, and call the box. Expect its inspect to say why (goal 12's
/// `BoxInfo::ended`, the cause's words) with its end unknown, its every other call but delete refused as retired, typed,
/// its record kept, ready, until its owner's delete removes it; the old backing's scope stopped and released, no VM
/// process left; and neither its pool record nor its receipt kept.
#[test]
fn w14_a_census_backing_is_retired_at_start_and_its_box_says_why() {
    let dir = socket_fixture();
    let (old, owner) = configured(dir.path());
    let offer = old.prepare_backing().unwrap();
    assert!(old.initialize_backing(grant(&offer)).is_err());
    let mut spec = super::super::tests::oci(
        hex_api::Rootfs::Dir(dir.path().join("rootfs.tar")),
        Vec::new(),
    );
    spec.route = Some(hex_api::RouteReceipt {
        key: old.capacity(Class::Oci).unwrap().unwrap().routes[0].key,
        backing: Some(offer.key),
    });
    let id = spec.id.clone();
    Transaction::put(
        owner.as_ref(),
        &BoxRecord::new(spec, hex_core::record::Phase::Ready, Vec::new()),
    )
    .unwrap();
    owner.native.stop_releases.store(true, Ordering::SeqCst);
    let restarted = configured_census_free(dir.path(), &owner);
    // The node's preflight retires first, before any exact cost check; then its backing is prepared afresh.
    restarted.retire_census().unwrap();
    let fresh = restarted.prepare_backing().unwrap();
    assert_ne!(fresh.key, offer.key);
    let info = restarted.inspect(&id).unwrap();
    assert_eq!(
        info.ended.as_deref(),
        Some("its backing's identity changed with decision 9's census-free cost")
    );
    assert_eq!(info.exit, Some(hex_api::Exit::Unknown));
    assert_eq!(
        restarted.usage(&id),
        Err(Refusal::Backing {
            key: offer.key,
            cause: LifecycleCause::Retired,
            why: None,
        })
    );
    let kept = Transaction::read(owner.as_ref(), &id).unwrap().unwrap();
    assert!(kept.retired);
    assert_eq!(kept.ended, info.ended);
    assert_eq!(kept.phase, hex_core::record::Phase::Ready);
    assert_eq!(kept.exit, Some(hex_api::Exit::Unknown));
    assert!(
        owner.native.released.load(Ordering::SeqCst),
        "its scope released: no VM process left"
    );
    assert_eq!(owner.removals.load(Ordering::SeqCst), 1);
    assert!(pool::read(&dir.path().join("@guest")).unwrap().is_none());
    restarted.delete(&id).unwrap();
}

/// A-6 (ROOT's audit, 2026-11-13: an oci box its node VM's last-resort end ended vanished from its node's list): an oci
/// box its node retired with its node VM is listed from its record, as its inspect answers it, until its owner's
/// delete. Do keep, in a guest runtime's books, an oci box retired for NodeVmEnded; list the runtime's boxes, inspect
/// the box, then delete it and list again. Expect it listed, ended, its end unknown and its `ended` the cause's words,
/// as its inspect says; then gone.
#[test]
fn a6_an_oci_box_retired_with_its_node_vm_is_listed_from_its_record() {
    let dir = socket_fixture();
    let (runtime, owner) = configured(dir.path());
    let spec = super::super::tests::oci(
        hex_api::Rootfs::Dir(dir.path().join("rootfs.tar")),
        Vec::new(),
    );
    let id = spec.id.clone();
    let mut record = BoxRecord::new(spec, hex_core::record::Phase::Ready, Vec::new());
    record.retire_for(LifecycleCause::NodeVmEnded);
    Transaction::put(owner.as_ref(), &record).unwrap();
    let inspected = runtime.inspect(&id).unwrap();
    assert_eq!(
        inspected.ended,
        Some(LifecycleCause::NodeVmEnded.to_string())
    );
    assert_eq!(inspected.exit, Some(hex_api::Exit::Unknown));
    let listed: Vec<hex_api::BoxInfo> = runtime
        .list()
        .unwrap()
        .into_iter()
        .filter(|info| info.id == id)
        .collect();
    assert_eq!(listed, [inspected]);
    runtime.delete(&id).unwrap();
    assert!(runtime.list().unwrap().iter().all(|info| info.id != id));
}

/// Joint plan step 14 (AC-T7.4.1; ROOT, 2026-11-13: no legacy receipt runs on with a census number as its bound): a
/// node's start retires every VM box and backing it recorded under its profile's census cost, before any exact check.
/// Do record, under a cost whose census measured both overheads, a Shards VM box and a shared backing holding an OCI
/// box; then start a census-free node: preflight each record, retire, and prepare its backing. Expect each preflight to
/// pass; both boxes retired, ended in the cause's words, every call but inspect and delete refused; the backing's scope
/// released, its pool record and receipt gone; and a receipt the node admits now census-free.
#[test]
fn w14_a_node_start_retires_every_census_record_before_its_exact_checks() {
    let dir = socket_fixture();
    let (old, owner) = configured(dir.path());
    let offer = old.prepare_backing().unwrap();
    assert!(old.initialize_backing(grant(&offer)).is_err());
    let mut oci = super::super::tests::oci(
        hex_api::Rootfs::Dir(dir.path().join("rootfs.tar")),
        Vec::new(),
    );
    oci.route = Some(hex_api::RouteReceipt {
        key: old.capacity(Class::Oci).unwrap().unwrap().routes[0].key,
        backing: Some(offer.key),
    });
    let census = old.runtime_cost.unwrap();
    let mut vm = super::super::tests::oci(
        hex_api::Rootfs::Dir(dir.path().join("rootfs.tar")),
        Vec::new(),
    );
    vm.id = BoxId::new("vm-census").unwrap();
    vm.class = match vm.class {
        hex_api::ClassSpec::Oci(spec) => hex_api::ClassSpec::Shards(spec),
        other => other,
    };
    vm.allocation = Some(census.allocate(CapacityResources::default()).unwrap());
    for spec in [oci.clone(), vm.clone()] {
        Transaction::put(
            owner.as_ref(),
            &BoxRecord::new(spec, hex_core::record::Phase::Ready, Vec::new()),
        )
        .unwrap();
    }
    owner.native.stop_releases.store(true, Ordering::SeqCst);
    let restarted = configured_census_free(dir.path(), &owner);
    for id in [&oci.id, &vm.id] {
        let record = Transaction::read(owner.as_ref(), id).unwrap().unwrap();
        restarted.preflight_box(&record).unwrap();
    }
    restarted.preflight_backing(&owner.row()).unwrap();
    restarted.retire_census().unwrap();
    for id in [&oci.id, &vm.id] {
        let kept = Transaction::read(owner.as_ref(), id).unwrap().unwrap();
        assert!(kept.retired, "{id} kept retired");
        let info = restarted.inspect(id).unwrap();
        assert_eq!(
            info.ended.as_deref(),
            Some("its backing's identity changed with decision 9's census-free cost")
        );
        assert_eq!(info.exit, Some(hex_api::Exit::Unknown));
        assert!(matches!(
            restarted.usage(id),
            Err(Refusal::Backing {
                cause: LifecycleCause::Retired,
                ..
            } | Refusal::Lifecycle {
                cause: LifecycleCause::Retired,
                ..
            })
        ));
    }
    assert!(owner.native.released.load(Ordering::SeqCst));
    assert_eq!(owner.removals.load(Ordering::SeqCst), 1);
    assert!(pool::read(&dir.path().join("@guest")).unwrap().is_none());
    assert_ne!(restarted.prepare_backing().unwrap().key, offer.key);
    let fresh = restarted
        .runtime_cost
        .unwrap()
        .allocate(CapacityResources::default())
        .unwrap();
    assert_eq!(fresh.resources, fresh.guest().unwrap());
    for id in [&oci.id, &vm.id] {
        restarted.delete(id).unwrap();
        assert!(Transaction::read(owner.as_ref(), id).unwrap().is_none());
    }
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
        own: Default::default(),
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
        node_vms: Vec::new(),
        unlearned: None,
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

/// Joint plan step 14 (ROOT, 2026-11-13, (C); decision 9): a managed scope's stop is a wait of its own, measuring,
/// never a boot's record. Before its first stop, and past the slowest stop seen (the run-out told), it follows the
/// scope by its own party to its end (Studio r201: a release's stop waited the guest's first boot's 5.26 s record and
/// was refused Uncertain at it). Do grant a shared backing whose launch's scope ends only once a wait follows it with
/// no bound; then release it. Expect it Released at once, its end recorded and its native scope
/// finalized; its stop's wait followed with no bound; and the boot's bound never asked.
#[test]
fn w14_c_a_pools_stop_follows_its_scope_and_never_waits_a_boots_record() {
    let dir = socket_fixture();
    let (runtime, owner) = configured(dir.path());
    let offer = runtime.prepare_backing().unwrap();
    assert!(runtime.initialize_backing(grant(&offer)).is_err());
    owner
        .native
        .released_when_followed
        .store(true, Ordering::SeqCst);
    let released = runtime.release_backing(offer.key);
    let asked = owner.native.asked.lock().unwrap().clone();
    let released = released.unwrap_or_else(|why| panic!("{why:?}; its waits asked {asked:?}"));
    assert_eq!(released.phase, BackingPhase::Released);
    assert_eq!(owner.row().exit, Some(hex_api::Exit::Code(7)));
    assert_eq!(owner.finalized.load(Ordering::SeqCst), 1);
    assert!(
        asked.contains(&Duration::MAX),
        "its stop never followed its scope: {asked:?}"
    );
}

/// Joint plan step 14, (B) (ROOT, 2026-11-13: an Uncertain with no reason is a CLAUDE.md §3 gap): a backing's refusal
/// says why, in its refusing site's own words. Do prepare the shared backing, then release another generation of its
/// pool, one its guest holds no record of. Expect it refused Uncertain, its key the one asked, and its reason that its
/// guest holds no record of it.
#[test]
fn w14_b_a_release_of_a_backing_its_guest_holds_no_record_of_says_why() {
    let dir = socket_fixture();
    let (runtime, _owner) = configured(dir.path());
    let offer = runtime.prepare_backing().unwrap();
    let other = BackingKey {
        pool: offer.key.pool,
        generation: [9; 16],
    };
    let refused = runtime.release_backing(other).unwrap_err();
    assert!(
        matches!(
            &refused,
            Refusal::Backing {
                key,
                cause: LifecycleCause::Uncertain,
                why: Some(why),
            } if *key == other && why == "its guest holds no record of it"
        ),
        "{refused:?}"
    );
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
        matches!(runtime.release_backing(original.key), Err(Refusal::Backing { key, cause: LifecycleCause::Busy, why: None }) if key == original.key)
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

/// Joint plan step 14 (C7; ROOT, 2026-11-13): a shared backing's guest's own overhead is what its agent says it uses past
/// what its boxes hold, the most seen, only rising; its CPU and disk unread, so none. Do measure a guest of 8 GiB with
/// 5 GiB free whose boxes hold 2 GiB, from cold; then one with 6 GiB free and 1.5 GiB in its boxes; then one whose
/// boxes hold more than it says it uses. Expect 1 GiB, then still 1 GiB (0.5 GiB less is no fall), and 1 GiB again.
#[test]
fn w14_a_shared_guests_own_overhead_is_its_use_past_its_boxes_the_most_seen() {
    let gib = 1u64 << 30;
    let guest = |free: u64| {
        let mut capacity = hex_api::Capacity {
            own: Default::default(),
            id: "guest".into(),
            total: CapacityResources {
                cpu_milli: 2_000,
                mem_bytes: 8 * gib,
                disk_bytes: 0,
            },
            free: CapacityResources {
                cpu_milli: 2_000,
                mem_bytes: free,
                disk_bytes: 0,
            },
            devices: Vec::new(),
            cost: None,
            routes: Vec::new(),
            device_routes: Vec::new(),
            backings: Vec::new(),
            unavailable: Vec::new(),
            learned: None,
            node_vms: Vec::new(),
            unlearned: None,
        };
        capacity.free.mem_bytes = free;
        capacity
    };
    let boxes = |memory: u64| super::Boxes { memory };
    let first = super::guest_seen(super::Seen::default(), &guest(5 * gib), boxes(2 * gib));
    assert_eq!(first.overhead.mem_bytes, Some(gib));
    let second = super::guest_seen(first, &guest(6 * gib), boxes(3 * gib / 2));
    assert_eq!(second.overhead.mem_bytes, Some(gib));
    let third = super::guest_seen(second, &guest(5 * gib), boxes(4 * gib));
    assert_eq!(third.overhead.mem_bytes, Some(gib));
}

/// Joint plan step 14 (C7; ROOT, 2026-11-13, pin 2; the review's A-8): a shared guest's own CPU is its machine's CPU
/// time past its boxes': at its first answer over its uptime, its boot's work among it, which the first interval its
/// counters resolve replaces; then the most of such intervals. For a guest of 2 CPUs an interval resolves once it spans
/// 1000 x (5 + 3 x 2) ticks of 10 ms, 110 s. Its disk is its used bytes, its boxes' disk no usage reads. Do measure a
/// guest of 100 GiB of disk with 40 GiB free, up 10 s with 4 s of CPU and its boxes 1 s; then 1 s on with 0.5 s more and
/// its boxes 0.3 s more; then 110 s after the first with 55 s more and its boxes 33 s more; then 110 s on with 11 s more
/// and its boxes none. Expect 300 milli-CPU from boot (3 s over 10 s), still so after 1 s, which no interval resolves;
/// then 200 (110 s's, in the boot's place); then 200 still (100 is less); and 60 GiB of disk throughout.
#[test]
fn w14_a_shared_guests_own_cpu_is_its_resolved_intervals_past_its_boxes_and_its_disk_its_use() {
    let gib = 1u64 << 30;
    let second = 1_000_000_000u64;
    let guest = |cpu_ns: u64, uptime_ns: u64, boxes_cpu_ns: u64| hex_api::Capacity {
        id: "guest".into(),
        total: CapacityResources {
            cpu_milli: 2_000,
            mem_bytes: 8 * gib,
            disk_bytes: 100 * gib,
        },
        free: CapacityResources {
            cpu_milli: 2_000,
            mem_bytes: 8 * gib,
            disk_bytes: 40 * gib,
        },
        devices: Vec::new(),
        cost: None,
        routes: Vec::new(),
        device_routes: Vec::new(),
        backings: Vec::new(),
        unavailable: Vec::new(),
        learned: None,
        own: hex_api::MachineTime {
            cpu_ns: Some(cpu_ns),
            uptime_ns: Some(uptime_ns),
            boxes_cpu_ns: Some(boxes_cpu_ns),
        },
        node_vms: Vec::new(),
        unlearned: None,
    };
    let boxes = super::Boxes::default();
    let first = super::guest_seen(
        super::Seen::default(),
        &guest(4 * second, 10 * second, second),
        boxes,
    );
    assert_eq!(
        (first.overhead.cpu_milli, first.cpu_from_boot),
        (Some(300), true)
    );
    assert_eq!(first.overhead.disk_bytes, Some(60 * gib));
    let soon = super::guest_seen(
        first,
        &guest(
            4 * second + second / 2,
            11 * second,
            second + 3 * second / 10,
        ),
        boxes,
    );
    assert_eq!(
        (soon.overhead.cpu_milli, soon.cpu_from_boot),
        (Some(300), true)
    );
    let next = super::guest_seen(soon, &guest(59 * second, 120 * second, 34 * second), boxes);
    assert_eq!(
        (next.overhead.cpu_milli, next.cpu_from_boot),
        (Some(200), false)
    );
    let last = super::guest_seen(next, &guest(70 * second, 230 * second, 34 * second), boxes);
    assert_eq!(last.overhead.cpu_milli, Some(200));
    assert_eq!(last.overhead.disk_bytes, Some(60 * gib));
}

/// The review's A-8 (AC-T7.4.1; ROOT, 2026-11-14: no silent hold): an interval spans its guest's boxes' coming and
/// going, since their CPU time is the guest's own count of everything under its box root, which keeps an ended box's
/// work (cgroup v2 flushes a removed cgroup's counts into its parent's). Do measure a 2-CPU guest up 10 s with 4 s of
/// CPU, its boxes 1 s; then 110 s on with 60 s more, its boxes 38 s more, a box having started and another ended
/// meanwhile (the boxes on its backing read 1 GiB, then 3 GiB). Expect the interval's 200 milli-CPU in place of the boot
/// reading's 300, the boxes' change no break in it.
#[test]
fn a8_an_interval_spans_its_guests_boxes_coming_and_going() {
    let gib = 1u64 << 30;
    let second = 1_000_000_000u64;
    let guest = |cpu_ns: u64, uptime_ns: u64, boxes_cpu_ns: u64| hex_api::Capacity {
        id: "guest".into(),
        total: CapacityResources {
            cpu_milli: 2_000,
            mem_bytes: 8 * gib,
            disk_bytes: 100 * gib,
        },
        free: CapacityResources {
            cpu_milli: 2_000,
            mem_bytes: 4 * gib,
            disk_bytes: 40 * gib,
        },
        devices: Vec::new(),
        cost: None,
        routes: Vec::new(),
        device_routes: Vec::new(),
        backings: Vec::new(),
        unavailable: Vec::new(),
        learned: None,
        own: hex_api::MachineTime {
            cpu_ns: Some(cpu_ns),
            uptime_ns: Some(uptime_ns),
            boxes_cpu_ns: Some(boxes_cpu_ns),
        },
        node_vms: Vec::new(),
        unlearned: None,
    };
    let first = super::guest_seen(
        super::Seen::default(),
        &guest(4 * second, 10 * second, second),
        super::Boxes { memory: gib },
    );
    assert_eq!(
        (first.overhead.cpu_milli, first.cpu_from_boot),
        (Some(300), true)
    );
    let next = super::guest_seen(
        first,
        &guest(64 * second, 120 * second, 39 * second),
        super::Boxes { memory: 3 * gib },
    );
    assert_eq!(
        (next.overhead.cpu_milli, next.cpu_from_boot),
        (Some(200), false)
    );
}

/// The review's A-8 (AC-T7.4.1): a shared guest's counters move in 10 ms ticks (`/proc/stat` in USER_HZ, `/proc/uptime`
/// in hundredths), so an interval of a few ticks says nothing of its share, and the most of such intervals only climbs
/// on their noise. Do measure a 2-CPU guest up 10 s with 4 s of CPU and its boxes 1 s; then an answer 20 ms on in which
/// its CPU time moved one tick, its boxes' none. Expect 300 milli-CPU read since its boot, and still that, from its
/// boot, after the 20 ms interval (which read 500 milli-CPU, and kept it).
#[test]
fn a8_an_interval_its_counters_cant_resolve_reads_nothing() {
    let gib = 1u64 << 30;
    let second = 1_000_000_000u64;
    let tick = 10_000_000u64;
    let guest = |cpu_ns: u64, uptime_ns: u64| hex_api::Capacity {
        id: "guest".into(),
        total: CapacityResources {
            cpu_milli: 2_000,
            mem_bytes: 8 * gib,
            disk_bytes: 100 * gib,
        },
        free: CapacityResources {
            cpu_milli: 2_000,
            mem_bytes: 8 * gib,
            disk_bytes: 40 * gib,
        },
        devices: Vec::new(),
        cost: None,
        routes: Vec::new(),
        device_routes: Vec::new(),
        backings: Vec::new(),
        unavailable: Vec::new(),
        learned: None,
        own: hex_api::MachineTime {
            cpu_ns: Some(cpu_ns),
            uptime_ns: Some(uptime_ns),
            boxes_cpu_ns: Some(second),
        },
        node_vms: Vec::new(),
        unlearned: None,
    };
    let boxes = super::Boxes::default();
    let first = super::guest_seen(
        super::Seen::default(),
        &guest(4 * second, 10 * second),
        boxes,
    );
    assert_eq!(
        (first.overhead.cpu_milli, first.cpu_from_boot),
        (Some(300), true)
    );
    let next = super::guest_seen(
        first,
        &guest(4 * second + tick, 10 * second + 2 * tick),
        boxes,
    );
    assert_eq!(
        (next.overhead.cpu_milli, next.cpu_from_boot),
        (Some(300), true),
        "an interval of two ticks read as a share"
    );
}

/// Joint plan step 14 (C7; ROOT, 2026-11-13, pin 2): two answers that measured a shared guest at once keep pin 2 as one
/// does: the guest's CPU read since its boot gives way to the first interval, whichever answer stored first, and the
/// intervals keep their most. Do merge an answer's first interval, 200 milli-CPU, with a kept boot reading of 300, each
/// way round; an interval of 150 with a kept interval of 250; and a boot reading of 300 with a guest still cold. Expect
/// 200 each way, no longer read from boot; 250; and 300, still from boot, which the next interval replaces.
#[test]
fn w14_answers_that_measured_a_shared_guest_at_once_keep_its_boot_reading_until_an_interval() {
    let cpu = |milli: Option<u64>, from_boot: bool| super::Seen {
        overhead: hex_api::LearnedOverhead {
            cpu_milli: milli,
            mem_bytes: Some(1 << 30),
            disk_bytes: Some(0),
        },
        start: None,
        cpu_from_boot: from_boot,
    };
    let boot = cpu(Some(300), true);
    let interval = cpu(Some(200), false);
    for (measured, other) in [(interval, boot), (boot, interval)] {
        let kept = super::merged(measured, other);
        assert_eq!(
            (kept.overhead.cpu_milli, kept.cpu_from_boot),
            (Some(200), false),
            "{measured:?} after {other:?}"
        );
    }
    let kept = super::merged(cpu(Some(150), false), cpu(Some(250), false));
    assert_eq!(
        (kept.overhead.cpu_milli, kept.cpu_from_boot),
        (Some(250), false)
    );
    let kept = super::merged(boot, cpu(None, false));
    assert_eq!(
        (kept.overhead.cpu_milli, kept.cpu_from_boot),
        (Some(300), true)
    );
    let next = super::merged(cpu(Some(100), false), kept);
    assert_eq!(
        (next.overhead.cpu_milli, next.cpu_from_boot),
        (Some(100), false)
    );
}

/// Joint plan step 14 (C7; AC-T7.4.1; ROOT, 2026-11-13, pin 1): a shared backing goes Live only measured: its runtime
/// takes its guest's first measurement from its agent's capacity answer before its record says Live, and leaves it
/// Granted, saying why, without one. Do grant a backing whose launch published its controller and whose guest runs (a
/// node that restarted with it up takes it back), its agent answering no capacity; then again, its agent answering 8
/// GiB with 7 free and 100 GiB of disk with 40 free, up 10 s with 4 s of CPU, no box on it. Expect the first refused,
/// the backing Granted; the second Live, its offer's guest measured: 1 GiB, 60 GiB of disk, and 400 milli-CPU read
/// since its boot.
#[test]
fn w14_a_shared_backing_goes_live_only_measured() {
    let dir = socket_fixture();
    let (runtime, owner) = configured(dir.path());
    let offer = runtime.prepare_backing().unwrap();
    // The model's launch refuses after its intent; the row and receipt then say what a launch that published its
    // controller leaves, the backing still Granted.
    assert!(runtime.initialize_backing(grant(&offer)).is_err());
    let identity = shards_api::Identity {
        pid: 41,
        started: 43,
        vsock: runtime.pool_socket(offer.key).unwrap(),
    };
    let mut row = owner.row();
    row.init = Some(controller(&identity));
    Transaction::put_pool(owner.as_ref(), &row).unwrap();
    let path = dir.path().join("@guest");
    let mut receipt = pool::read(&path).unwrap().unwrap();
    receipt.phase = Phase::Running(copy_identity(&identity).unwrap());
    pool::write(&path, &receipt).unwrap();
    let binding = super::Binding {
        key: offer.key,
        commitment: owner.row().native_commitment().unwrap(),
    };
    *runtime.booted.lock().unwrap() = Some(super::super::tests::running_guest(None, Some(binding)));
    let refused = runtime.initialize_backing(grant(&offer));
    assert!(
        matches!(
            &refused,
            Err(Refusal::Backing {
                cause: LifecycleCause::Uncertain,
                why: Some(why),
                ..
            }) if why.contains("measured")
        ),
        "{refused:?}"
    );
    assert_eq!(owner.row().phase, BackingPhase::Granted);
    let gib = 1u64 << 30;
    let answer = Capacity {
        id: "guest".into(),
        total: CapacityResources {
            cpu_milli: 2_000,
            mem_bytes: 8 * gib,
            disk_bytes: 100 * gib,
        },
        free: CapacityResources {
            cpu_milli: 2_000,
            mem_bytes: 7 * gib,
            disk_bytes: 40 * gib,
        },
        devices: Vec::new(),
        cost: None,
        routes: Vec::new(),
        device_routes: Vec::new(),
        backings: Vec::new(),
        unavailable: Vec::new(),
        learned: None,
        own: hex_api::MachineTime {
            cpu_ns: Some(4_000_000_000),
            uptime_ns: Some(10_000_000_000),
            boxes_cpu_ns: Some(0),
        },
        node_vms: Vec::new(),
        unlearned: None,
    };
    *runtime.booted.lock().unwrap() = Some(super::super::tests::running_guest(
        Some(answer),
        Some(binding),
    ));
    let live = runtime.initialize_backing(grant(&offer)).unwrap();
    assert_eq!(live.phase, BackingPhase::Live);
    assert_eq!(
        live.guest,
        hex_api::LearnedOverhead {
            cpu_milli: Some(400),
            mem_bytes: Some(gib),
            disk_bytes: Some(60 * gib),
        }
    );
}
