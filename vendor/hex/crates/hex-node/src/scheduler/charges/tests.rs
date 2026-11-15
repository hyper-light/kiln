//! What the scheduler takes of its node's charge learner (joint plan step 14, AC-T7.4.1;
//! `docs/design/learned-charge.md`).

use std::sync::Arc;

use super::*;
use crate::placement::{Driver, Runtimes};
use hex_api::{LearnedPrior, LearnedUse, NodeVmPhase, NodeVmReceipt, Runtime, RuntimeCost};
use hex_sim::index::Pick;
use hex_sim::model::{NodeId, Resources};
use hex_sim::peer::Budget;

const GIB: u64 = 1 << 30;

/// A default Shards VM's profile: every CPU of a 16-core host and 8 GiB, its envelope its floor and ceiling.
fn cost() -> RuntimeCost {
    let envelope = CapacityResources {
        cpu_milli: 16_000,
        mem_bytes: 8 * GIB,
        disk_bytes: 0,
    };
    RuntimeCost {
        id: [14; 32],
        floor: envelope,
        ceiling: envelope,
        quantum: CapacityResources {
            cpu_milli: 1,
            mem_bytes: 1,
            disk_bytes: 1,
        },
        guest_overhead: CapacityResources::default(),
        overhead: CapacityResources::default(),
    }
}

fn node() -> NodeSpec {
    let mut node = NodeSpec::new(
        NodeId(0),
        Resources {
            cpu_milli: 16_000,
            mem_bytes: 64 * GIB,
            ..Resources::default()
        },
    );
    node.costs[Class::Shards.index()] = Some(cost());
    node
}

fn capacity(learned: Option<Learned>) -> hex_api::Capacity {
    hex_api::Capacity {
        own: Default::default(),
        id: "shards".into(),
        total: CapacityResources {
            cpu_milli: 16_000,
            mem_bytes: 64 * GIB,
            disk_bytes: 0,
        },
        free: CapacityResources::default(),
        devices: Vec::new(),
        cost: Some(cost()),
        routes: Vec::new(),
        device_routes: Vec::new(),
        backings: Vec::new(),
        unavailable: Vec::new(),
        learned,
        node_vms: Vec::new(),
        unlearned: None,
    }
}

/// Joint plan step 14 (AC-T7.4.1), C2 through the scheduler: do take a Shards answer carrying a prior for the class's
/// profile and ceiling, then one carrying a prior for another profile, then none. Expect the first given to the
/// node's facts as its Shards prior, which a new receipt is booked at; the second and third giving no prior; and the
/// node's capacity the machine's throughout (C8).
#[test]
fn w14_the_scheduler_takes_the_priors_its_nodes_learner_carries() {
    let booking = CapacityResources {
        cpu_milli: 100,
        mem_bytes: GIB,
        disk_bytes: 0,
    };
    let carried = |profile: [u8; 32]| Learned {
        priors: vec![LearnedPrior {
            profile,
            ceiling: cost()
                .allocate(CapacityResources::default())
                .unwrap()
                .resources,
            booking,
        }],
        used: LearnedUse::default(),
        holdings: Vec::new(),
        ended: Vec::new(),
        ceilings: Vec::new(),
        rates: Vec::new(),
        window: None,
        unread: Vec::new(),
        awaits: Vec::new(),
        ranked: Vec::new(),
        unrecorded: Vec::new(),
        unacted: Vec::new(),
        reservations: None,
    };
    let reports = vec![(Class::Shards, Some(capacity(Some(carried([14; 32])))))];
    let mut local = node();
    apply(&mut local, &learned(&reports).unwrap()).unwrap();
    assert_eq!(
        local.priors[Class::Shards.index()],
        Some(Prior {
            profile: [14; 32],
            booking,
            ceiling: cost()
                .allocate(CapacityResources::default())
                .unwrap()
                .resources,
        })
    );
    assert_eq!(local.capacity, node().capacity);
    let allocation = cost().allocate(CapacityResources::default()).unwrap();
    let booked = hex_sim::model::booked_at(Some(allocation), local.priors[Class::Shards.index()]);
    assert_eq!(booked.unwrap().booking(), booking);
    for reports in [
        vec![(Class::Shards, Some(capacity(Some(carried([15; 32])))))],
        vec![(Class::Shards, Some(capacity(None)))],
    ] {
        let mut local = node();
        apply(&mut local, &learned(&reports).unwrap()).unwrap();
        assert_eq!(local.priors, [None; Class::COUNT]);
        assert_eq!(local.capacity, node().capacity);
    }
}

/// Joint plan step 14 (AC-T7.4.1, C8): a capacity answer's learned holdings are bounded by a node's most boxes, so a
/// node never has more running VMs to report than its answer may carry.
#[test]
fn w14_learned_holdings_are_bounded_by_a_nodes_boxes() {
    assert_eq!(hex_api::MAX_LEARNED_HOLDINGS, hex_runtime::MAX_BOXES);
}

/// Joint plan step 14 (AC-T7.4.1), C8 through the scheduler: do take two classes' answers holding three boxes, two of
/// which the node runs under its scheduler; expect those two booked at their holdings against their keys, in the
/// answers' order, and the third passed over.
#[test]
fn w14_the_scheduler_books_each_vm_it_runs_at_its_holding() {
    let key = |seq| hex_sim::peer::Key {
        owner: 0,
        epoch: 1,
        seq,
    };
    let holding = |id: &str, gib: u64| hex_api::LearnedHolding {
        id: hex_api::LearnedHolder::Box(BoxId::new(id).unwrap()),
        holding: CapacityResources {
            cpu_milli: 100,
            mem_bytes: gib * GIB,
            disk_bytes: 0,
        },
    };
    let carrying = |holdings| Learned {
        priors: Vec::new(),
        used: LearnedUse::default(),
        holdings,
        ended: Vec::new(),
        ceilings: Vec::new(),
        rates: Vec::new(),
        window: None,
        unread: Vec::new(),
        awaits: Vec::new(),
        ranked: Vec::new(),
        unrecorded: Vec::new(),
        unacted: Vec::new(),
        reservations: None,
    };
    let learned = vec![
        (
            Class::Shards,
            carrying(vec![holding("vm-a", 2), holding("vm-stray", 9)]),
        ),
        (Class::Firecracker, carrying(vec![holding("vm-b", 1)])),
    ];
    let ids = BTreeMap::from([
        (BoxId::new("vm-a").unwrap(), key(1)),
        (BoxId::new("vm-b").unwrap(), key(2)),
    ]);
    let booked = holds(&learned, &ids).unwrap();
    assert_eq!(
        booked,
        vec![
            (
                hex_sim::peer::Holder::Box(key(1)),
                holding("vm-a", 2).holding
            ),
            (
                hex_sim::peer::Holder::Box(key(2)),
                holding("vm-b", 1).holding
            ),
        ]
    );
}

/// Joint plan step 39 (ROOT's ruling of 2026-11-13; goal 14's learner, goal 13's ledger): a node VM's learned holding
/// reaches the ledger as its run's, a holder beside the boxes. Do: take an answer holding a box the node runs and a node
/// VM. Expect: the box booked against its key, the node VM against its run.
#[test]
fn a_node_vms_learned_holding_reaches_the_ledger_as_its_runs() {
    let key = hex_sim::peer::Key {
        owner: 0,
        epoch: 1,
        seq: 1,
    };
    let run = hex_api::NodeRun::from_bytes([4; 16]);
    let resources = CapacityResources {
        cpu_milli: 300,
        mem_bytes: 2 * GIB,
        disk_bytes: 0,
    };
    let learned = vec![(
        Class::Shards,
        Learned {
            priors: Vec::new(),
            used: LearnedUse::default(),
            holdings: vec![
                hex_api::LearnedHolding {
                    id: hex_api::LearnedHolder::Box(BoxId::new("vm-a").unwrap()),
                    holding: resources,
                },
                hex_api::LearnedHolding {
                    id: hex_api::LearnedHolder::Node(run),
                    holding: resources,
                },
            ],
            ended: Vec::new(),
            ceilings: Vec::new(),
            rates: Vec::new(),
            window: None,
            unread: Vec::new(),
            awaits: Vec::new(),
            ranked: Vec::new(),
            unrecorded: Vec::new(),
            unacted: Vec::new(),
            reservations: None,
        },
    )];
    let ids = BTreeMap::from([(BoxId::new("vm-a").unwrap(), key)]);
    assert_eq!(
        holds(&learned, &ids).unwrap(),
        vec![
            (hex_sim::peer::Holder::Box(key), resources),
            (hex_sim::peer::Holder::Node(run), resources),
        ]
    );
}

/// Joint plan step 14 (AC-T7.4.1), C2 through the scheduler: a class is its profile and its ceiling, so a prior books a
/// receipt of its own ceiling alone. Do take an answer carrying a prior for a profile's least VM (1 CPU, 1 GiB), then
/// book at the node's prior a receipt of that size and one of the profile's largest (8 CPUs, 8 GiB); expect the first
/// booked at the prior, and the second whole, no VM of its size having warmed for it.
#[test]
fn w14_a_prior_books_a_receipt_of_its_own_ceiling_alone() {
    let unit = CapacityResources {
        cpu_milli: 1,
        mem_bytes: 1,
        disk_bytes: 1,
    };
    let sizes = RuntimeCost {
        id: [16; 32],
        floor: CapacityResources {
            cpu_milli: 1_000,
            mem_bytes: GIB,
            disk_bytes: 0,
        },
        ceiling: CapacityResources {
            cpu_milli: 8_000,
            mem_bytes: 8 * GIB,
            disk_bytes: 0,
        },
        quantum: unit,
        guest_overhead: CapacityResources::default(),
        overhead: CapacityResources::default(),
    };
    let least = sizes.allocate(CapacityResources::default()).unwrap();
    let largest = sizes.allocate(sizes.ceiling).unwrap();
    let booking = CapacityResources {
        cpu_milli: 100,
        mem_bytes: GIB / 2,
        disk_bytes: 0,
    };
    let carried = Learned {
        priors: vec![LearnedPrior {
            profile: sizes.id,
            ceiling: least.resources,
            booking,
        }],
        used: LearnedUse::default(),
        holdings: Vec::new(),
        ended: Vec::new(),
        ceilings: Vec::new(),
        rates: Vec::new(),
        window: None,
        unread: Vec::new(),
        awaits: Vec::new(),
        ranked: Vec::new(),
        unrecorded: Vec::new(),
        unacted: Vec::new(),
        reservations: None,
    };
    let mut local = node();
    local.costs[Class::Shards.index()] = Some(sizes);
    apply(&mut local, &[(Class::Shards, carried)]).unwrap();
    let prior = local.priors[Class::Shards.index()];
    let at = |receipt| hex_sim::model::booked_at(Some(receipt), prior).unwrap();
    assert_eq!(at(least).booking(), booking);
    assert_eq!(at(largest).booking(), largest.resources, "booked whole");
}

/// Joint plan step 39 (AC-T7.4.1; ROOT, 2026-11-13: a node VM's receipt reaches the scheduler on its node's capacity
/// answer, its record the one source): do refresh a node's ledger from its answers, the node VM they name Booting, then
/// Ready twice, then Ended; then Ready again, then named by no answer. Expect its booking held while it is Booting or
/// Ready, once however many refreshes name it, and released once it is Ended and once no answer names it.
#[test]
fn w39_a_node_vms_receipt_is_booked_from_its_answer_while_it_runs_and_released_at_its_end() {
    let dir = tempfile::tempdir().unwrap();
    let room = node().capacity;
    let runtime: Arc<dyn Runtime> = Arc::new(hex_api::sim::SimRuntime::default());
    let driver = Driver::boot(
        NodeId(0),
        &[NodeSpec::new(NodeId(0), room)],
        &[],
        Budget { owned: 4, ran: 4 },
        Pick::default(),
        dir.path(),
        Runtimes::new([(Class::Oci, runtime)]).unwrap(),
    )
    .unwrap();
    driver.refresh_node(NodeSpec::new(NodeId(0), room)).unwrap();
    driver.open_view(&dir.path().join("capacity"), 1).unwrap();
    let run = hex_api::NodeRun::from_bytes([39; 16]);
    let booking = CapacityResources {
        cpu_milli: 4_000,
        mem_bytes: 8 * GIB,
        disk_bytes: 0,
    };
    let refresh = |phase: Option<NodeVmPhase>| {
        let mut answer = capacity(None);
        answer.node_vms = phase
            .map(|phase| NodeVmReceipt {
                run,
                booking,
                phase,
                guest: CapacityResources::default(),
            })
            .into_iter()
            .collect();
        driver
            .book_nodes(&node_vms(&[(Class::Oci, Some(answer))]).unwrap())
            .unwrap();
        let held = driver.view().unwrap().held;
        (held.cpu_milli, held.mem_bytes)
    };
    let booked = (4_000, 8 * GIB);
    assert_eq!(refresh(Some(NodeVmPhase::Booting)), booked);
    assert_eq!(refresh(Some(NodeVmPhase::Ready)), booked);
    assert_eq!(
        refresh(Some(NodeVmPhase::Ready)),
        booked,
        "booked once, however many refreshes name it"
    );
    assert_eq!(refresh(Some(NodeVmPhase::Ended)), (0, 0));
    assert_eq!(refresh(Some(NodeVmPhase::Ready)), booked);
    assert_eq!(refresh(None), (0, 0), "released once no answer names it");
}

/// Joint plan step 39: one node's records name each of its node VMs once. Do: take two classes' answers naming the same
/// node VM. Expect them refused, never booked twice.
#[test]
fn w39_answers_naming_a_node_vm_twice_are_refused() {
    let mut answer = capacity(None);
    answer.node_vms = vec![NodeVmReceipt {
        run: hex_api::NodeRun::from_bytes([39; 16]),
        booking: CapacityResources::default(),
        phase: NodeVmPhase::Ready,
        guest: CapacityResources::default(),
    }];
    let twice = [
        (Class::Oci, Some(answer.try_clone().unwrap())),
        (Class::Shards, Some(answer)),
    ];
    assert!(matches!(node_vms(&twice), Err(Refusal::Invalid { .. })));
}
