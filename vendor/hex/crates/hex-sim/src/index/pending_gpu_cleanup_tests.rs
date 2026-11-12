//! Positive cleanup companion to the original pending-GPU refresh red.
use super::*;
use crate::model::Ask;

#[test]
fn pending_gpu_room_survives_sibling_cleanup_then_returns_on_its_own_refund() {
    let node = NodeSpec::new(
        NodeId(0),
        Resources {
            cpu_milli: 8_000,
            mem_bytes: 8 << 30,
            disk_bytes: 0,
            gpus: 2,
        },
    );
    let demand = Demand {
        class: Class::Oci,
        kind: Kind::Plain,
        need: Resources {
            cpu_milli: 100,
            mem_bytes: 64 << 20,
            disk_bytes: 0,
            gpus: 1,
        },
        ask: Ask::default(),
    };
    let mut index = FitIndex::new(std::slice::from_ref(&node)).unwrap();
    index
        .take_pending_routed(NodeId(0), &demand, None, None)
        .unwrap();
    index.take(NodeId(0), WorkloadId(1), &demand).unwrap();
    assert_eq!(index.key(NodeId(0)).unwrap().gpus, 0);
    index.give(NodeId(0), WorkloadId(1), &demand).unwrap();
    assert_eq!(index.key(NodeId(0)).unwrap().gpus, 1);
    index.refresh_node(&node).unwrap();
    assert_eq!(index.key(NodeId(0)).unwrap().gpus, 1);
    // An owner's pending row has no receiver-selected WorkloadId/physical binding yet.
    index
        .give_routed(NodeId(0), WorkloadId(2), &demand, None, None)
        .unwrap();
    assert_eq!(index.key(NodeId(0)).unwrap().gpus, 2);
    index.take(NodeId(0), WorkloadId(3), &demand).unwrap();
    index.take(NodeId(0), WorkloadId(4), &demand).unwrap();
}

/// ROOT's fa7 source-review concern: native unavailability is not a local pending row. Its
/// repeated snapshot must be stable; a true pending claim still reserves the only eligible card.
#[test]
fn unavailable_gpu_refresh_distinguishes_foreign_unavailability_from_local_pending() {
    for pending in [false, true] {
        let mut node = NodeSpec::new(
            NodeId(0),
            Resources {
                cpu_milli: 8_000,
                mem_bytes: 8 << 30,
                disk_bytes: 0,
                gpus: 2,
            },
        );
        node.devices[0].key = Some(1);
        node.devices[0].available = false;
        node.devices[1].key = Some(2);
        let demand = Demand {
            class: Class::Oci,
            kind: Kind::Plain,
            need: Resources {
                cpu_milli: 100,
                mem_bytes: 64 << 20,
                disk_bytes: 0,
                gpus: 1,
            },
            ask: Ask::default(),
        };
        let mut index = FitIndex::new(std::slice::from_ref(&node)).unwrap();
        let original = index.key(NodeId(0)).unwrap();
        assert_eq!(index.verdict(NodeId(0), &demand).unwrap(), Verdict::Admits);
        if pending {
            index
                .take_pending_routed(NodeId(0), &demand, None, None)
                .unwrap();
        }
        let reserved = index.key(NodeId(0)).unwrap();
        for _ in 0..2 {
            index.refresh_node(&node).unwrap();
            assert_eq!(index.key(NodeId(0)).unwrap(), reserved);
        }
        let actual = index.verdict(NodeId(0), &demand).unwrap();
        eprintln!(
            "unavailable/pending GPU source control: local_pending={pending}, original={original:?}, repeated={reserved:?}, fresh={actual:?}; native_effect=false"
        );
        assert_eq!(
            actual,
            if pending {
                Verdict::NoRoom
            } else {
                Verdict::Admits
            }
        );
        if pending {
            index
                .give_routed(NodeId(0), WorkloadId(99), &demand, None, None)
                .unwrap();
        }
        index.take(NodeId(0), WorkloadId(1), &demand).unwrap();
        let selected = index.held_devices(NodeId(0), WorkloadId(1)).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected.first().unwrap().key, Some(2));
        index.give(NodeId(0), WorkloadId(1), &demand).unwrap();
        assert_eq!(index.key(NodeId(0)).unwrap(), original);
    }
}
