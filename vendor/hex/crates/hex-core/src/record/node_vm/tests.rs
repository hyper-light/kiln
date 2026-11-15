//! A node VM's record (joint plan step 39): its row's bytes round trip, and a record not whole is refused.

use super::*;

/// A node VM's admitted size, its cost's own allocation: two CPUs and the least memory.
fn allocation() -> Allocation {
    let fixed = hex_api::CapacityResources {
        cpu_milli: 2_000,
        mem_bytes: hex_api::MIN_MEMORY,
        disk_bytes: 0,
    };
    hex_api::RuntimeCost {
        id: [3; 32],
        floor: fixed,
        ceiling: fixed,
        quantum: hex_api::CapacityResources {
            cpu_milli: 1,
            mem_bytes: 1,
            disk_bytes: 1,
        },
        guest_overhead: hex_api::CapacityResources::default(),
        overhead: hex_api::CapacityResources::default(),
    }
    .allocate(hex_api::CapacityResources::default())
    .unwrap()
}

fn record(phase: NodeVmPhase, exit: Option<Exit>) -> NodeVmRecord {
    NodeVmRecord {
        version: RECORD_VERSION,
        run: NodeRun::from_bytes([9; 16]),
        image:
            "hex-node:rev@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_owned(),
        allocation: allocation(),
        phase,
        diagnostics: Some(PathBuf::from("/state/node-vms/run.log")),
        exit,
        last_resort: None,
        started: None,
    }
}

/// Why a last resort ended holder `id`: 3 GiB held of a 2 GiB budget, in band 0, its fleet past its budget.
fn end_of(id: hex_api::LearnedHolder) -> LearnedEnd {
    LearnedEnd {
        id,
        held: 3 << 30,
        budget: 2 << 30,
        band: 0,
        fleet_held: 9 << 30,
        fleet_growth: 1 << 30,
        fleet_budget: 8 << 30,
        unmoved: Some(hex_api::Upstream::LiveMove),
    }
}

/// Joint plan step 39, place 1 (AC-T7.4.1; ROOT, 2026-11-13: a node VM's last-resort end lives in its row): do keep a
/// node VM's row with why the last resort ended its own run, then one naming another run's VM, and one naming a box.
/// Expect the first to round trip as its row's bytes, and the other two refused: a row keeps its own VM's end alone.
#[test]
fn w39_a_node_vm_rows_last_resort_end_names_its_own_run() {
    let mut kept = record(NodeVmPhase::Ended, Some(Exit::Signal(9)));
    kept.last_resort = Some(end_of(hex_api::LearnedHolder::Node(kept.run)));
    assert_eq!(NodeVmRecord::decode(&kept.encode().unwrap()).unwrap(), kept);
    for other in [
        hex_api::LearnedHolder::Node(NodeRun::from_bytes([8; 16])),
        hex_api::LearnedHolder::Box(hex_api::BoxId::new("a-box").unwrap()),
    ] {
        let mut row = record(NodeVmPhase::Ready, None);
        row.last_resort = Some(end_of(other));
        assert!(
            matches!(row.check(), Err(Refusal::Invalid { .. })),
            "{:?}",
            row.last_resort
        );
    }
}

/// A node VM's record as its row's bytes and back. Do: encode a booting and an ended one, and decode them. Expect: the
/// same records, each row one line.
#[test]
fn a_node_vms_record_round_trips_as_one_line() {
    for kept in [
        record(NodeVmPhase::Booting, None),
        record(NodeVmPhase::Ended, Some(Exit::Code(0))),
    ] {
        let bytes = kept.encode().unwrap();
        assert_eq!(bytes.iter().filter(|b| **b == b'\n').count(), 1);
        assert_eq!(NodeVmRecord::decode(&bytes).unwrap(), kept);
    }
}

/// A record not whole is refused, never guessed at. Do: check one of another version, one naming no image, an ended
/// run with no exit, a booting one with an exit, and bytes cut short. Expect: each refused.
#[test]
fn a_node_vms_record_not_whole_is_refused() {
    let mut other = record(NodeVmPhase::Booting, None);
    other.version += 1;
    let mut unnamed = record(NodeVmPhase::Booting, None);
    unnamed.image.clear();
    for bad in [
        other,
        unnamed,
        record(NodeVmPhase::Ended, None),
        record(NodeVmPhase::Booting, Some(Exit::Code(0))),
    ] {
        assert!(bad.check().is_err(), "{bad:?}");
    }
    let bytes = record(NodeVmPhase::Ready, None).encode().unwrap();
    assert!(NodeVmRecord::decode(&bytes[..bytes.len() - 1]).is_err());
}

/// The review's C-17 (AC-T7.4.1; docs/design/node-under-shards-init.md §4): a node VM's row names the image it boots
/// pinned by its digest, so no row keeps one a tag names, which may move before its node boots it again. Do: check rows
/// naming their image by a tag and by a digest too short. Expect each refused; then one pinned by its sha256 digest
/// taken.
#[test]
fn c17_a_node_vm_row_whose_image_its_digest_doesnt_pin_is_refused() {
    for unpinned in ["hex-node:latest", "hex-node@sha256:0123"] {
        let mut row = record(NodeVmPhase::Booting, None);
        row.image = unpinned.to_owned();
        assert!(
            matches!(row.check(), Err(Refusal::Invalid { .. })),
            "{unpinned}"
        );
    }
    let mut row = record(NodeVmPhase::Booting, None);
    row.image = format!("hex-node@sha256:{}", "0123456789abcdef".repeat(4));
    assert_eq!(row.check(), Ok(()));
}

/// Joint plan step 39: a node VM's receipt is held to the VM it sizes, not to a workload of none. Do: check a record
/// whose receipt was admitted for a VM of two CPUs and 256 MiB within a profile from none to that, and one booking past
/// what it charges. Expect: the first whole, the second refused.
#[test]
fn a_node_vms_receipt_is_held_to_the_vm_it_sizes() {
    let vm = hex_api::CapacityResources {
        cpu_milli: 2_000,
        mem_bytes: 256 << 20,
        disk_bytes: 0,
    };
    let sized = hex_api::RuntimeCost {
        id: [4; 32],
        floor: hex_api::CapacityResources::default(),
        ceiling: vm,
        quantum: hex_api::CapacityResources {
            cpu_milli: 1_000,
            mem_bytes: 1 << 20,
            disk_bytes: 1,
        },
        guest_overhead: hex_api::CapacityResources::default(),
        overhead: hex_api::CapacityResources::default(),
    }
    .allocate(vm)
    .unwrap();
    let mut kept = record(NodeVmPhase::Ready, None);
    kept.allocation = sized;
    kept.check().unwrap();
    kept.allocation.booked = Some(hex_api::CapacityResources {
        cpu_milli: 3_000,
        ..vm
    });
    assert!(kept.check().is_err());
}
