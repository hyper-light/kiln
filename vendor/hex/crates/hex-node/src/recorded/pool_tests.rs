//! AC-T7.4.1: actual Store + concrete Box/Pool transaction ownership. Native callbacks are counted
//! controls only; these tests neither create a VM nor establish native scope emptiness.
use super::*;
use hex_api::{BackingGrant, BackingKey, BackingPhase, CapacityResources, Execution, RuntimeCost};
use std::cell::Cell;

fn cost() -> RuntimeCost {
    let guest = CapacityResources {
        cpu_milli: 1000,
        mem_bytes: 4096,
        disk_bytes: 4096,
    };
    RuntimeCost {
        id: [1; 32],
        floor: guest,
        ceiling: guest,
        quantum: guest,
        overhead: CapacityResources {
            cpu_milli: 1,
            mem_bytes: 1,
            disk_bytes: 1,
        },
        guest_overhead: CapacityResources::default(),
    }
}

#[test]
fn prepared_pool_cannot_reach_native_usage_before_its_exact_grant() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("disk"), &dir.path().join("run")).unwrap();
    let mut row = store
        .prepare_pool([7; 32], Execution::ShardsShared, cost(), &[])
        .unwrap();
    let key = row.key;
    let calls = Cell::new(0usize);
    let launch =
        |_: NativeRecord<'_>,
         _: &hex_runtime::devices::ManagedLaunch<'_>,
         _: shards_api::LaunchIo,
         _: std::time::Duration,
         _: &mut dyn FnMut(&shards_api::Identity) -> Result<(), Refusal>| {
            panic!("the usage control cannot launch a VM")
        };
    let attach = |_: NativeRecord<'_>,
                  _: Option<&shards_api::Identity>|
     -> Result<ManagedReceipt, Refusal> {
        panic!("the usage control cannot attach a VM")
    };
    let finalize = |_: NativeRecord<'_>, _: Option<&shards_api::Identity>| -> Result<(), Refusal> {
        panic!("the usage control cannot finalize a VM")
    };
    let usage = |native: NativeRecord<'_>| {
        let NativeRecord::Pool(actual) = native else {
            panic!("a backing is not a Box")
        };
        assert_eq!(actual.key, key);
        assert_eq!(actual.phase, BackingPhase::Live);
        assert_eq!(
            actual.allocation,
            Some(cost().allocate(CapacityResources::default()).unwrap())
        );
        calls.set(calls.get() + 1);
        Ok(Usage {
            memory_bytes: 17,
            ..Usage::default()
        })
    };
    let native = Native {
        catalog: None,
        launch: &launch,
        attach: &attach,
        finalize: &finalize,
        usage: &usage,
        unsubmitted_pool: &|_| panic!("this control cannot check Prepared native submission"),
    };
    pool_transaction(&store, &key, &[], &native, &mut |tx| {
        assert!(matches!(tx.managed_usage(), Err(Refusal::Backing { key: got, .. }) if got == key));
        assert_eq!(calls.get(), 0);
        let grant = BackingGrant {
            key,
            allocation: cost().allocate(CapacityResources::default()).unwrap(),
            devices: Vec::new(),
        };
        row = tx.grant_pool(&grant)?;
        row.init = Some(ProcId {
            pid: 11,
            start_ticks: 13,
        });
        tx.put_pool(&row)?;
        row.phase = BackingPhase::Live;
        tx.put_pool(&row)?;
        assert_eq!(tx.managed_usage()?.memory_bytes, 17);
        Ok(())
    })
    .unwrap();
    assert_eq!(calls.get(), 1);
}

#[test]
fn stale_pool_generation_is_refused_before_any_native_callback() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("disk"), &dir.path().join("run")).unwrap();
    let row = store
        .prepare_pool([9; 32], Execution::ShardsShared, cost(), &[])
        .unwrap();
    let mut generation = row.key.generation;
    generation[0] ^= 1;
    let stale = BackingKey {
        pool: row.key.pool,
        generation,
    };
    let launch =
        |_: NativeRecord<'_>,
         _: &hex_runtime::devices::ManagedLaunch<'_>,
         _: shards_api::LaunchIo,
         _: std::time::Duration,
         _: &mut dyn FnMut(&shards_api::Identity) -> Result<(), Refusal>| {
            panic!("a stale owner cannot launch")
        };
    let attach = |_: NativeRecord<'_>,
                  _: Option<&shards_api::Identity>|
     -> Result<ManagedReceipt, Refusal> { panic!("a stale owner cannot attach") };
    let finalize = |_: NativeRecord<'_>, _: Option<&shards_api::Identity>| -> Result<(), Refusal> {
        panic!("a stale owner cannot finalize")
    };
    let usage =
        |_: NativeRecord<'_>| -> Result<Usage, Refusal> { panic!("a stale owner cannot observe") };
    let native = Native {
        catalog: None,
        launch: &launch,
        attach: &attach,
        finalize: &finalize,
        usage: &usage,
        unsubmitted_pool: &|_| panic!("this control cannot check Prepared native submission"),
    };
    pool_transaction(&store, &stale, &[], &native, &mut |tx| {
        assert!(matches!(tx.read_pool(&stale), Err(Refusal::Backing { key, .. }) if key == stale));
        assert!(matches!(tx.managed_usage(), Err(Refusal::Backing { key, .. }) if key == stale));
        assert!(
            matches!(tx.remove_pool(&stale), Err(Refusal::Backing { key, .. }) if key == stale)
        );
        Ok(())
    })
    .unwrap();
    assert_eq!(
        store.read_pool(&row.key.pool).unwrap().unwrap().key,
        row.key
    );
}
