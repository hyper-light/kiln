//! AC-T7.4.1: real Recorded/Store publication with a counted sans-I/O route producer.
//! Model tuple identities test the common encoding barrier, not actual backend configuration/native
//! eligibility. Real Guest/FC/native constructors have separate producer controls.
use super::*;
use hex_api::{
    BackingKey, CapacityResources, Execution, GpuEligibility, ImageArch, ImageOs, ImagePlatform,
    RouteKey, RuntimeCost,
};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

fn cost() -> RuntimeCost {
    let fixed = CapacityResources {
        cpu_milli: 1000,
        mem_bytes: hex_api::MIN_MEMORY,
        disk_bytes: 0,
    };
    RuntimeCost {
        id: [0x41; 32],
        floor: fixed,
        ceiling: fixed,
        quantum: CapacityResources {
            cpu_milli: 1,
            mem_bytes: 1,
            disk_bytes: 1,
        },
        guest_overhead: CapacityResources::default(),
        overhead: CapacityResources::default(),
    }
}
fn key() -> RouteKey {
    RouteKey {
        class: Class::Oci,
        image: ImagePlatform {
            os: ImageOs::Linux,
            arch: ImageArch::Amd64,
        },
        execution: Execution::ShardsShared,
        gpu: GpuEligibility::None,
    }
}
struct Projecting {
    calls: AtomicUsize,
    introductions: AtomicUsize,
}
impl Runtime for Projecting {
    fn classes(&self) -> Vec<Class> {
        panic!("projection cannot discover classes")
    }
    fn capacity(&self, _: Class) -> Result<Option<hex_api::Capacity>, Refusal> {
        panic!("projection cannot query capacity/provider")
    }
    fn create(&self, _: BoxSpec) -> Result<BoxInfo, Refusal> {
        panic!("projection cannot create")
    }
    fn start(&self, _: &BoxId) -> Result<(), Refusal> {
        panic!("projection cannot start")
    }
    fn run(&self, _: BoxSpec, _: Program, _: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        panic!("projection cannot run")
    }
    fn exec(&self, _: &BoxId, _: Program, _: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        panic!("projection cannot exec")
    }
    fn signal(&self, _: &BoxId, _: Signal) -> Result<(), Refusal> {
        panic!("projection cannot signal")
    }
    fn inspect(&self, _: &BoxId) -> Result<BoxInfo, Refusal> {
        panic!("projection cannot inspect native state")
    }
    fn usage(&self, _: &BoxId) -> Result<Usage, Refusal> {
        panic!("projection cannot sample usage")
    }
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        panic!("projection cannot enumerate native state")
    }
    fn delete(&self, _: &BoxId) -> Result<(), Refusal> {
        panic!("projection cannot delete")
    }
}
impl Recover for Projecting {
    fn trace(&self, _: &BoxId) -> Result<Option<ProcId>, Refusal> {
        panic!("projection cannot trace native state")
    }
    fn adopt(&self, _: &BoxSpec, _: Option<ProcId>) -> Result<(), Refusal> {
        panic!("projection cannot adopt")
    }
    fn end(&self, _: Option<ProcId>) -> Result<(), Refusal> {
        panic!("projection cannot end a scope")
    }
    fn preflight_retained(
        &self,
        _: &hex_runtime::devices::Records,
        _: &[PoolRecord],
    ) -> Result<(), Refusal> {
        panic!("projection cannot read native metadata")
    }
    fn project_routes(
        &self,
        pools: &[PoolRecord],
        introduced: Option<(Execution, &[DeviceId])>,
        projection: &mut hex_runtime::devices::RouteProjection,
    ) -> Result<(), Refusal> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let (process, oci) = hex_runtime::devices::native_route_keys(
            Class::Oci,
            ImagePlatform {
                os: ImageOs::Windows,
                arch: ImageArch::Amd64,
            },
        )?;
        projection.retain(process, None)?;
        projection.retain(oci.unwrap(), None)?;
        for pool in pools {
            assert_eq!(pool.execution, Execution::ShardsShared);
            projection.retain(key(), Some(pool.key))?;
        }
        if let Some((execution, ids)) = introduced {
            self.introductions.fetch_add(1, Ordering::Relaxed);
            assert_eq!(execution, Execution::ShardsShared);
            assert!(ids.is_empty());
            projection.introduce(key())?;
        }
        Ok(())
    }
}

/// Neither component alone is over-bound. Their union reaches exactly MAX at the first absent
/// target, reuses that SAME lifetime without another introduced form, then refuses MAX+1 before
/// publishing any metadata. No capacity/provider/native callback can satisfy this test.
#[test]
fn common_family_publication_checks_global_union_before_new_lifetime_write() {
    let directory = tempfile::tempdir().unwrap();
    let recorded = Recorded::open(
        Projecting {
            calls: AtomicUsize::new(0),
            introductions: AtomicUsize::new(0),
        },
        directory.path(),
    )
    .unwrap();
    // Two fixed native forms plus these distinct canonical test-model tuple lifetimes leave
    // exactly one slot. Each row is valid Store data; initialized real-config checks are separate.
    for n in 0..hex_api::MAX_RUNTIME_ROUTES - 3 {
        let mut pool = [0x42; 32];
        pool[0] = u8::try_from(n).unwrap();
        recorded
            .books
            .store
            .prepare_pool(pool, Execution::ShardsShared, cost(), &[])
            .unwrap();
    }
    let pool = [0xfd; 32];
    let calls = Cell::new(0usize);
    let mut validate = |records: &hex_runtime::devices::Records, pools: &[PoolRecord]| {
        calls.set(calls.get() + 1);
        assert!(records.is_empty());
        assert!(pools.len() <= hex_api::MAX_RUNTIME_ROUTES);
        for pool in pools {
            pool.check()?;
        }
        Ok(())
    };
    let row = hex_runtime::devices::Books::prepare_pool(
        &recorded,
        pool,
        Execution::ShardsShared,
        cost(),
        &[],
        (&pool, &[], None),
        &mut validate,
    )
    .unwrap();
    assert_eq!(
        recorded.books.store.all_pools().unwrap().len(),
        hex_api::MAX_RUNTIME_ROUTES - 2
    );
    let before = recorded
        .books
        .store
        .read_pool(&pool)
        .unwrap()
        .unwrap()
        .encode()
        .unwrap();
    let syncs = recorded.books.store.syncs();
    let same = hex_runtime::devices::Books::prepare_pool(
        &recorded,
        pool,
        Execution::ShardsShared,
        cost(),
        &[],
        (&pool, &[], Some(&row)),
        &mut validate,
    )
    .unwrap();
    assert_eq!(same, row);
    assert_eq!(recorded.books.store.syncs(), syncs);
    let absent = [0xfe; 32];
    let refused = hex_runtime::devices::Books::prepare_pool(
        &recorded,
        absent,
        Execution::ShardsShared,
        cost(),
        &[],
        (&absent, &[], None),
        &mut validate,
    );
    assert!(matches!(
        refused,
        Err(Refusal::Exhausted {
            what: "projected initialized runtime routes",
            bound: hex_api::MAX_RUNTIME_ROUTES
        })
    ));
    assert!(recorded.books.store.read_pool(&absent).unwrap().is_none());
    assert_eq!(
        recorded
            .books
            .store
            .read_pool(&pool)
            .unwrap()
            .unwrap()
            .encode()
            .unwrap(),
        before
    );
    assert_eq!(recorded.books.store.syncs(), syncs);
    assert_eq!(calls.get(), 3);
    assert_eq!(recorded.runtime.calls.load(Ordering::Relaxed), 3);
    assert_eq!(recorded.runtime.introductions.load(Ordering::Relaxed), 2);
}

/// A native-only host cannot hide a retained or introduced VM tuple. WithGuest dispatches its
/// actual VM component separately; an unconfigured bare host must refuse instead of dropping it.
#[test]
fn native_only_component_refuses_vm_owners_and_new_vm_lifetimes() {
    check_native_projection(&[], None).unwrap();
    let pool = PoolRecord {
        version: hex_core::record::RECORD_VERSION,
        key: BackingKey {
            pool: [0x43; 32],
            generation: [0x44; 16],
        },
        execution: Execution::ShardsShared,
        cost: cost(),
        allocation: None,
        devices: Vec::new(),
        phase: hex_api::BackingPhase::Prepared,
        init: None,
        exit: None,
    };
    pool.check().unwrap();
    assert!(
        matches!(check_native_projection(std::slice::from_ref(&pool), None), Err(Refusal::Backing { key, cause: hex_api::LifecycleCause::Uncertain, why: Some(_) }) if key == pool.key)
    );
    for execution in [
        Execution::ShardsShared,
        Execution::ShardsDedicated,
        Execution::Firecracker,
        Execution::Native,
    ] {
        assert!(check_native_projection(&[], Some((execution, &[]))).is_err());
    }
}
