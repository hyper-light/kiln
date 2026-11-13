//! AC-T7.4.1: real private-file Store metadata controls for one family preparation boundary.
//! Terminal Pool facts below are injected model state; no VM/HCS/GPU or native scope proof.
use super::*;
use hex_api::{BoxId, BoxSpec, CapacityResources, ClassSpec, DeviceKind};
use hex_core::record::{BoxRecord, Phase, ProcId};
use std::cell::Cell;
use std::time::Duration;

fn cost() -> RuntimeCost {
    super::tests::cost()
}
fn cpu(id: &str, phase: Phase) -> BoxRecord {
    BoxRecord::new(
        BoxSpec {
            id: BoxId::new(id).unwrap(),
            bounds: hex_api::Bounds::default(),
            class: ClassSpec::Process,
            owner: None,
            devices: Vec::new(),
            allocation: None,
            route: None,
        },
        phase,
        Vec::new(),
    )
}
fn model(
    records: &hex_runtime::devices::Records,
    pools: &[PoolRecord],
    expected: Option<&PoolRecord>,
) -> Result<(), Refusal> {
    for (id, row) in records {
        let row = row
            .as_ref()
            .map_err(|_| invalid("model row is unreadable"))?;
        if row.spec.id != *id || !row.spec.devices.is_empty() || row.spec.route.is_some() {
            return Err(invalid("model CPU sibling changed its claims"));
        }
        row.spec.check()?;
    }
    if pools.len() != usize::from(expected.is_some()) || pools.first() != expected {
        return Err(invalid("model family current or pending ACK changed"));
    }
    if let Some(row) = expected {
        row.check()?;
        if row.cost != cost() {
            return Err(invalid("model initialized cost changed"));
        }
    }
    Ok(())
}
fn grant(store: &Store, record: &PoolRecord) -> PoolRecord {
    store
        .grant_pool(&BackingGrant {
            key: record.key,
            allocation: record.cost.allocate(CapacityResources::default()).unwrap(),
            devices: record.devices.to_vec(),
        })
        .unwrap()
}

/// The real expected-row check precedes the configured callback and any new scope/publication.
#[test]
fn family_expected_row_and_validator_gate_scope_publication() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("disk"), &dir.path().join("run")).unwrap();
    store.put(&cpu("retained-cpu", Phase::Ready)).unwrap();
    let calls = Cell::new(0);
    let before = store.syncs();
    assert!(
        store
            .prepare_pool_validated(
                [1; 32],
                Execution::ShardsShared,
                cost(),
                &[],
                (&[9; 32], &[], None),
                &mut |records, pools| {
                    calls.set(calls.get() + 1);
                    model(records, pools, None)?;
                    Err(invalid("injected configured-proposal rejection"))
                }
            )
            .is_err()
    );
    assert_eq!(calls.get(), 1);
    assert!(store.read_pool(&[1; 32]).unwrap().is_none());
    assert_eq!(store.syncs(), before);
    let first = store
        .prepare_pool_validated(
            [1; 32],
            Execution::ShardsShared,
            cost(),
            &[],
            (&[9; 32], &[], None),
            &mut |records, pools| model(records, pools, None),
        )
        .unwrap();
    assert_eq!(first.phase, BackingPhase::Prepared);
    assert_eq!(first.allocation, None);
    let actual = grant(&store, &first);
    calls.set(0);
    let before = actual.encode().unwrap();
    for expected in [Some(&first), None] {
        assert!(
            store
                .prepare_pool_validated(
                    [1; 32],
                    Execution::ShardsShared,
                    cost(),
                    &[],
                    (&[9; 32], &[], expected),
                    &mut |_, _| {
                        calls.set(calls.get() + 1);
                        Err(invalid("stale state must refuse before validator"))
                    }
                )
                .is_err()
        );
    }
    assert_eq!(calls.get(), 0);
    assert_eq!(
        store
            .read_pool(&[1; 32])
            .unwrap()
            .unwrap()
            .encode()
            .unwrap(),
        before
    );
    let reused = store
        .prepare_pool_validated(
            [1; 32],
            Execution::ShardsShared,
            cost(),
            &[],
            (&[9; 32], &[], Some(&actual)),
            &mut |records, pools| model(records, pools, Some(&actual)),
        )
        .unwrap();
    assert_eq!(reused, actual);
}

/// A changed proposal drains only the old exact scope. No new R crosses release/ACK removal.
#[test]
fn family_changed_proposal_drains_old_scope_before_replacement() {
    for live in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("disk"), &dir.path().join("run")).unwrap();
        let a = DeviceId {
            kind: DeviceKind::Gpu,
            key: 7,
        };
        let b = DeviceId {
            kind: DeviceKind::Gpu,
            key: 8,
        };
        let first = store
            .prepare_pool([1; 32], Execution::ShardsDedicated, cost(), &[a])
            .unwrap();
        let mut old = grant(&store, &first);
        if live {
            old.init = Some(ProcId {
                pid: 11,
                start_ticks: 13,
            });
            store.put_pool(&old).unwrap();
            old.phase = BackingPhase::Live;
            store.put_pool(&old).unwrap();
        }
        let mut expected = PoolRecord::decode(&old.encode().unwrap()).unwrap();
        expected.phase = BackingPhase::Draining;
        let mut returned = store
            .prepare_pool_validated(
                [2; 32],
                Execution::ShardsDedicated,
                cost(),
                &[a, b],
                (&[9; 32], &[a, b], Some(&old)),
                &mut |records, pools| model(records, pools, Some(&old)),
            )
            .unwrap();
        assert_eq!(returned, expected);
        assert_eq!(store.read_pool(&old.key.pool).unwrap(), Some(expected));
        assert!(store.read_pool(&[2; 32]).unwrap().is_none());
        assert!(
            store
                .prepare_pool_validated(
                    [2; 32],
                    Execution::ShardsDedicated,
                    cost(),
                    &[a, b],
                    (&[9; 32], &[a, b], None),
                    &mut |records, pools| model(records, pools, None)
                )
                .is_err()
        );
        // Inject only the canonical model terminal fact; this is not native termination evidence.
        returned.exit = Some(hex_api::Exit::Code(0));
        returned.phase = BackingPhase::Released;
        store.put_pool(&returned).unwrap();
        let same = store
            .prepare_pool_validated(
                [2; 32],
                Execution::ShardsDedicated,
                cost(),
                &[a, b],
                (&[9; 32], &[a, b], Some(&returned)),
                &mut |records, pools| model(records, pools, Some(&returned)),
            )
            .unwrap();
        assert_eq!(same, returned);
        assert!(store.read_pool(&[2; 32]).unwrap().is_none());
        // Stand-in for the already-validated existing Index acknowledgment, then actual removal.
        store.remove_pool(&returned.key).unwrap();
        let next = store
            .prepare_pool_validated(
                [2; 32],
                Execution::ShardsDedicated,
                cost(),
                &[a, b],
                (&[9; 32], &[a, b], None),
                &mut |records, pools| model(records, pools, None),
            )
            .unwrap();
        assert_eq!(next.key.pool, [2; 32]);
        assert_eq!(next.devices, [a, b]);
        assert_eq!(next.phase, BackingPhase::Prepared);
        assert_eq!(next.allocation, None);
    }
}

/// Prepared is not an absence witness. The owner returns it unchanged for adapter proof/cancel.
#[test]
fn family_prepared_change_keeps_original_scope_without_submission_proof() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("disk"), &dir.path().join("run")).unwrap();
    let first = store
        .prepare_pool([1; 32], Execution::ShardsShared, cost(), &[])
        .unwrap();
    let before = store.syncs();
    let current = store
        .prepare_pool_validated(
            [2; 32],
            Execution::ShardsShared,
            cost(),
            &[],
            (&[9; 32], &[], Some(&first)),
            &mut |records, pools| model(records, pools, Some(&first)),
        )
        .unwrap();
    assert_eq!(current, first);
    assert_eq!(store.syncs(), before);
    assert!(store.read_pool(&[2; 32]).unwrap().is_none());
}

/// Pause the pure callback at an explicit interleaving. Every ordinary mutation shares this SAME
/// Store fence, including writers that hold no outer backend method or family turn.
#[test]
fn family_snapshot_fences_ordinary_record_and_pool_writers() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("disk"), &dir.path().join("run")).unwrap();
    let original = cpu("existing-deleting", Phase::Deleting);
    store.put(&original).unwrap();
    let mut released = store
        .prepare_pool([1; 32], Execution::ShardsShared, cost(), &[])
        .unwrap();
    released.phase = BackingPhase::Released; // injected no-submission model fact, no native claim
    store.put_pool(&released).unwrap();
    let pending = store
        .prepare_pool([2; 32], Execution::ShardsShared, cost(), &[])
        .unwrap();
    let expected = PoolRecord::decode(&released.encode().unwrap()).unwrap();
    let worker_store = store.clone();
    let (paused, entered) = std::sync::mpsc::sync_channel(1);
    let (resume, released_callback) = std::sync::mpsc::sync_channel(1);
    // The existing macOS test hang guard is only a finite unit-test coordination budget.
    let guard = Duration::from_millis(39_507);
    let worker = std::thread::Builder::new()
        .spawn(move || {
            worker_store.prepare_pool_validated(
                [1; 32],
                Execution::ShardsShared,
                cost(),
                &[],
                (&[9; 32], &[], Some(&expected)),
                &mut |records, pools| {
                    if records.len() != 1 || pools.len() != 2 || pools.first() != Some(&expected) {
                        return Err(invalid("model snapshots changed before the pause"));
                    }
                    for (id, row) in records {
                        let row = row
                            .as_ref()
                            .map_err(|_| invalid("model unreadable sibling"))?;
                        if row.spec.id != *id || row.phase != Phase::Deleting {
                            return Err(invalid("model sibling changed"));
                        }
                    }
                    paused.send(()).unwrap();
                    released_callback.recv_timeout(guard).unwrap();
                    Ok(())
                },
            )
        })
        .unwrap();
    entered.recv_timeout(guard).unwrap();
    let proposed = cpu("new-during-snapshot", Phase::Creating);
    let results = [
        store.put(&proposed),
        store.begin(&proposed),
        store.put_lifecycle(&proposed),
        store
            .update(&original.spec.id, |row| {
                row.abandoned += 1;
                true
            })
            .map(|_| ()),
        store.remove(&original.spec.id),
        store.remove_lifecycle(&original.spec.id),
        store.put_pool(&released),
        store.remove_pool(&released.key),
        store
            .grant_pool(&BackingGrant {
                key: pending.key,
                allocation: pending.cost.allocate(CapacityResources::default()).unwrap(),
                devices: Vec::new(),
            })
            .map(|_| ()),
    ];
    resume.send(()).unwrap();
    let returned = worker.join().unwrap().unwrap();
    assert!(results.iter().all(Result::is_err));
    assert_eq!(returned, released);
    assert_eq!(
        store.read_lifecycle(&original.spec.id).unwrap(),
        Some(crate::store::in_this_boot(original))
    );
    assert!(store.read_lifecycle(&proposed.spec.id).unwrap().is_none());
    assert_eq!(store.read_pool(&pending.key.pool).unwrap(), Some(pending));
}

/// A malformed sibling blocks both the callback and publication and retains its exact bytes.
#[test]
fn family_malformed_sibling_refuses_before_callback_or_publication() {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("disk");
    let store = Store::open(&disk, &dir.path().join("run")).unwrap();
    let damaged = disk.join("retained-malformed.json");
    let bytes = b"{unreadable retained owner\n";
    std::fs::write(&damaged, bytes).unwrap();
    let calls = Cell::new(0);
    assert!(
        store
            .prepare_pool_validated(
                [1; 32],
                Execution::ShardsShared,
                cost(),
                &[],
                (&[9; 32], &[], None),
                &mut |_, _| {
                    calls.set(calls.get() + 1);
                    Err(invalid("malformed row must not reach configured validator"))
                }
            )
            .is_err()
    );
    assert_eq!(calls.get(), 0);
    assert_eq!(std::fs::read(&damaged).unwrap(), bytes);
    assert!(store.read_pool(&[1; 32]).unwrap().is_none());
}
