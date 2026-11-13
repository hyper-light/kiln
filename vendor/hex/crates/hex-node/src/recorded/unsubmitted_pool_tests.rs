//! AC-T7.4.1: Prepared cancellation asks the SAME native owner about submission metadata.
//! These actual Store/transaction controls use counted callbacks, never a native absence witness.
use super::*;
use hex_api::{
    BackingGrant, BackingKey, BackingPhase, Bounds, CapacityResources, ClassSpec, Execution,
    LifecycleCause, RuntimeCost,
};
use std::cell::Cell;
use std::time::Duration;

fn cost() -> RuntimeCost {
    let fixed = CapacityResources {
        cpu_milli: 1000,
        mem_bytes: hex_api::MIN_MEMORY,
        disk_bytes: 0,
    };
    RuntimeCost {
        id: [0x31; 32],
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

fn no_launch(
    _: NativeRecord<'_>,
    _: &hex_runtime::devices::ManagedLaunch<'_>,
    _: shards_api::LaunchIo,
    _: Duration,
    _: &mut dyn FnMut(&shards_api::Identity) -> Result<(), Refusal>,
) -> Result<ManagedReceipt, Refusal> {
    panic!("read-only submission preflight cannot launch")
}
fn no_attach(
    _: NativeRecord<'_>,
    _: Option<&shards_api::Identity>,
) -> Result<ManagedReceipt, Refusal> {
    panic!("read-only submission preflight cannot attach")
}
fn no_finalize(_: NativeRecord<'_>, _: Option<&shards_api::Identity>) -> Result<(), Refusal> {
    panic!("read-only submission preflight cannot finalize")
}
fn no_usage(_: NativeRecord<'_>) -> Result<Usage, Refusal> {
    panic!("read-only submission preflight cannot sample capacity or usage")
}
fn native<'a>(check: &'a NativeUnsubmittedPool<'a>) -> Native<'a> {
    Native {
        catalog: None,
        launch: &no_launch,
        attach: &no_attach,
        finalize: &no_finalize,
        usage: &no_usage,
        unsubmitted_pool: check,
    }
}
fn pool_bytes(directory: &Path, key: BackingKey) -> Vec<u8> {
    let name: String = key.pool.iter().map(|byte| format!("{byte:02x}")).collect();
    std::fs::read(directory.join("disk/.pools").join(format!("{name}.json"))).unwrap()
}
fn cpu_sibling(store: &Store) -> BoxRecord {
    let row = BoxRecord::new(
        BoxSpec {
            id: BoxId::new("unrelated-native-cpu").unwrap(),
            class: ClassSpec::Process,
            bounds: Bounds::default(),
            owner: None,
            allocation: None,
            devices: Vec::new(),
            route: None,
        },
        Phase::Ready,
        Vec::new(),
    );
    store.put(&row).unwrap();
    row
}

/// Exact Prepared authority reaches only its scoped native metadata callback. An ordinary CPU
/// sibling remains untouched and is not passed to a singleton/global native preflight.
#[test]
fn exact_prepared_callback_is_read_only_and_scoped_with_a_retained_cpu_sibling() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        &directory.path().join("disk"),
        &directory.path().join("run"),
    )
    .unwrap();
    let row = store
        .prepare_pool([0x32; 32], Execution::ShardsShared, cost(), &[])
        .unwrap();
    let sibling = cpu_sibling(&store);
    let before = pool_bytes(directory.path(), row.key);
    let syncs = store.syncs();
    let calls = Cell::new(0usize);
    let check = |record: NativeRecord<'_>| {
        let NativeRecord::Pool(actual) = record else {
            panic!("a pool cannot be checked as a Box")
        };
        assert_eq!(actual, &row);
        assert_eq!(actual.scope(), row.scope());
        assert_eq!(actual.phase, BackingPhase::Prepared);
        assert!(actual.allocation.is_none() && actual.init.is_none() && actual.exit.is_none());
        calls.set(calls.get() + 1);
        Ok(())
    };
    pool_transaction(&store, &row.key, &[], &native(&check), &mut |tx| {
        tx.check_unsubmitted_pool()
    })
    .unwrap();
    assert_eq!(calls.get(), 1);
    assert_eq!(pool_bytes(directory.path(), row.key), before);
    assert_eq!(store.read_pool(&row.key.pool).unwrap().unwrap(), row);
    assert_eq!(
        store.read_lifecycle(&sibling.spec.id).unwrap(),
        Some(crate::store::in_this_boot(sibling))
    );
    assert_eq!(store.syncs(), syncs);
}

/// Presence and read errors are distinct typed refusals. Neither failure publishes Released,
/// discards the generation, calls native cleanup, or grants resource room.
#[test]
fn native_presence_and_io_failure_propagate_without_publishing_or_removing() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        &directory.path().join("disk"),
        &directory.path().join("run"),
    )
    .unwrap();
    let row = store
        .prepare_pool([0x33; 32], Execution::ShardsShared, cost(), &[])
        .unwrap();
    let before = pool_bytes(directory.path(), row.key);
    let syncs = store.syncs();
    let calls = Cell::new(0usize);
    for presence in [true, false] {
        let check = |record: NativeRecord<'_>| {
            assert!(matches!(record, NativeRecord::Pool(actual) if actual == &row));
            calls.set(calls.get() + 1);
            if presence {
                Err(record.refusal(LifecycleCause::Busy))
            } else {
                Err(Refusal::os(
                    "reading this owner's native submission metadata",
                    std::io::Error::from(std::io::ErrorKind::PermissionDenied),
                ))
            }
        };
        let result = pool_transaction(&store, &row.key, &[], &native(&check), &mut |tx| {
            tx.check_unsubmitted_pool()
        });
        if presence {
            assert!(
                matches!(result, Err(Refusal::Backing { key, cause: LifecycleCause::Busy }) if key == row.key)
            );
        } else {
            assert!(
                matches!(result, Err(Refusal::Os { op, why }) if op == "reading this owner's native submission metadata" && why.contains("denied"))
            );
        }
        assert_eq!(pool_bytes(directory.path(), row.key), before);
        assert_eq!(store.read_pool(&row.key.pool).unwrap().unwrap(), row);
        assert_eq!(store.syncs(), syncs);
    }
    assert_eq!(calls.get(), 2);
}

/// The concrete transaction rereads canonical phase each time. Once granted, even an exact key
/// cannot reuse Prepared no-submission permission, including before init and after terminal facts.
#[test]
fn every_granted_or_later_phase_refuses_before_the_native_callback() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        &directory.path().join("disk"),
        &directory.path().join("run"),
    )
    .unwrap();
    let prepared = store
        .prepare_pool([0x34; 32], Execution::ShardsShared, cost(), &[])
        .unwrap();
    let mut row = store
        .grant_pool(&BackingGrant {
            key: prepared.key,
            allocation: cost().allocate(CapacityResources::default()).unwrap(),
            devices: Vec::new(),
        })
        .unwrap();
    let calls = Cell::new(0usize);
    let check = |_: NativeRecord<'_>| {
        calls.set(calls.get() + 1);
        panic!("a granted or terminal owner cannot use Prepared permission")
    };
    let assert_refused = || {
        let before = pool_bytes(directory.path(), row.key);
        let syncs = store.syncs();
        assert!(
            matches!(pool_transaction(&store, &row.key, &[], &native(&check), &mut |tx| tx.check_unsubmitted_pool()), Err(Refusal::Backing { key, cause: LifecycleCause::Uncertain }) if key == row.key)
        );
        assert_eq!(pool_bytes(directory.path(), row.key), before);
        assert_eq!(store.syncs(), syncs);
    };
    assert_refused(); // Granted, before init.
    row.init = Some(ProcId {
        pid: 17,
        start_ticks: 19,
    });
    store.put_pool(&row).unwrap(); // Identity is introduced while still Granted.
    for phase in [
        BackingPhase::Live,
        BackingPhase::Draining,
        BackingPhase::Released,
    ] {
        row.phase = phase;
        if phase == BackingPhase::Draining {
            row.exit = Some(hex_api::Exit::Code(23));
        }
        store.put_pool(&row).unwrap();
        let before = pool_bytes(directory.path(), row.key);
        let syncs = store.syncs();
        assert!(
            matches!(pool_transaction(&store, &row.key, &[], &native(&check), &mut |tx| tx.check_unsubmitted_pool()), Err(Refusal::Backing { key, cause: LifecycleCause::Uncertain }) if key == row.key)
        );
        assert_eq!(pool_bytes(directory.path(), row.key), before);
        assert_eq!(store.syncs(), syncs);
    }
    assert_eq!(calls.get(), 0);
}

/// A stale generation, a missing canonical row, and an ordinary Box transaction cannot consult
/// a native receipt under another owner's authority. Prepared->Released without a grant is also
/// terminal; it does not restore no-submission permission for a reused pool configuration.
#[test]
fn stale_missing_box_and_ungranted_released_owners_refuse_without_callback() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        &directory.path().join("disk"),
        &directory.path().join("run"),
    )
    .unwrap();
    let mut row = store
        .prepare_pool([0x35; 32], Execution::ShardsShared, cost(), &[])
        .unwrap();
    let sibling = cpu_sibling(&store);
    let calls = Cell::new(0usize);
    let check = |_: NativeRecord<'_>| {
        calls.set(calls.get() + 1);
        panic!("wrong owner or phase cannot inspect native submission metadata")
    };
    let mut stale = row.key;
    stale.generation[0] ^= 1;
    let mut missing = row.key;
    missing.pool[0] ^= 1;
    let before = pool_bytes(directory.path(), row.key);
    let syncs = store.syncs();
    for key in [stale, missing] {
        assert!(
            matches!(pool_transaction(&store, &key, &[], &native(&check), &mut |tx| tx.check_unsubmitted_pool()), Err(Refusal::Backing { key: got, cause: LifecycleCause::Uncertain }) if got == key)
        );
    }
    assert!(
        matches!(transaction(&store, &sibling.spec.id, &[], &native(&check), &mut |tx| tx.check_unsubmitted_pool()), Err(Refusal::Lifecycle { id, cause: LifecycleCause::Uncertain }) if id == sibling.spec.id)
    );
    assert_eq!(pool_bytes(directory.path(), row.key), before);
    assert_eq!(store.syncs(), syncs);
    row.phase = BackingPhase::Released;
    store.put_pool(&row).unwrap();
    let released = pool_bytes(directory.path(), row.key);
    let syncs = store.syncs();
    assert!(
        pool_transaction(&store, &row.key, &[], &native(&check), &mut |tx| tx
            .check_unsubmitted_pool())
        .is_err()
    );
    assert_eq!(pool_bytes(directory.path(), row.key), released);
    assert_eq!(store.syncs(), syncs);
    assert_eq!(calls.get(), 0);
}

/// Prepared no-submission is not a durable native absence observation. The later explicit ACK
/// checks the SAME native owner again, retains bytes on presence/error, and fences a stale ACK
/// after removal/rearm. This controlled callback is never a native scope-release witness.
#[test]
fn cancelled_unallocated_ack_rechecks_native_metadata_and_fences_reused_generation() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        &directory.path().join("disk"),
        &directory.path().join("run"),
    )
    .unwrap();
    let mut row = store
        .prepare_pool([0x36; 32], Execution::ShardsShared, cost(), &[])
        .unwrap();
    let sibling = cpu_sibling(&store);
    let key = row.key;
    let calls = Cell::new(0usize);
    let phase = Cell::new(BackingPhase::Prepared);
    let outcome = Cell::new(0u8);
    let check = |record: NativeRecord<'_>| {
        let NativeRecord::Pool(actual) = record else {
            panic!("cancellation/ACK cannot check another owner")
        };
        assert_eq!(actual.key, key);
        assert_eq!(actual.phase, phase.get());
        assert!(actual.allocation.is_none() && actual.init.is_none() && actual.exit.is_none());
        calls.set(calls.get() + 1);
        match outcome.get() {
            0 => Ok(()),
            1 => Err(record.refusal(LifecycleCause::Busy)),
            2 => Err(Refusal::os(
                "reading cancelled owner's native metadata for ACK",
                std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            )),
            other => panic!("unknown injected outcome {other}"),
        }
    };
    pool_transaction(&store, &key, &[], &native(&check), &mut |tx| {
        tx.check_unsubmitted_pool()?;
        row.phase = BackingPhase::Released;
        tx.put_pool(&row)
    })
    .unwrap();
    phase.set(BackingPhase::Released);
    let released = pool_bytes(directory.path(), key);
    let syncs = store.syncs();
    assert!(
        pool_transaction(&store, &key, &[], &native(&check), &mut |tx| {
            tx.check_unsubmitted_pool()
        })
        .is_err()
    ); // The public method is still Prepared-only.
    assert_eq!(calls.get(), 1);
    for injected in [1, 2] {
        outcome.set(injected);
        let result = pool_transaction(&store, &key, &[], &native(&check), &mut |tx| {
            tx.remove_pool(&key)
        });
        match injected {
            1 => assert!(
                matches!(result, Err(Refusal::Backing { key: actual, cause: LifecycleCause::Busy }) if actual == key)
            ),
            2 => assert!(
                matches!(result, Err(Refusal::Os { op, why }) if op == "reading cancelled owner's native metadata for ACK" && why.contains("denied"))
            ),
            other => panic!("unknown injected outcome {other}"),
        }
        assert_eq!(pool_bytes(directory.path(), key), released);
        assert_eq!(store.read_pool(&key.pool).unwrap().unwrap(), row);
        assert_eq!(store.syncs(), syncs);
    }
    outcome.set(0);
    pool_transaction(&store, &key, &[], &native(&check), &mut |tx| {
        tx.remove_pool(&key)
    })
    .unwrap();
    assert_eq!(calls.get(), 4);
    assert!(store.read_pool(&key.pool).unwrap().is_none());
    let replacement = store
        .prepare_pool(key.pool, Execution::ShardsShared, cost(), &[])
        .unwrap();
    assert_ne!(replacement.key, key);
    let replacement_bytes = pool_bytes(directory.path(), replacement.key);
    let syncs = store.syncs();
    assert!(
        pool_transaction(&store, &key, &[], &native(&check), &mut |tx| {
            tx.remove_pool(&key)
        })
        .is_err()
    );
    assert_eq!(calls.get(), 4);
    assert_eq!(
        pool_bytes(directory.path(), replacement.key),
        replacement_bytes
    );
    assert_eq!(store.read_pool(&key.pool).unwrap().unwrap(), replacement);
    assert_eq!(
        store.read_lifecycle(&sibling.spec.id).unwrap(),
        Some(crate::store::in_this_boot(sibling))
    );
    assert_eq!(store.syncs(), syncs);
}
