//! What the scheduler takes of its node's charge learner (joint plan step 14, AC-T7.4.1;
//! `docs/design/learned-charge.md`).

use super::*;
use hex_api::{LearnedPrior, LearnedUse, RuntimeCost};
use hex_sim::model::{NodeId, Resources};

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
        id: BoxId::new(id).unwrap(),
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
            (key(1), holding("vm-a", 2).holding),
            (key(2), holding("vm-b", 1).holding),
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
    };
    let mut local = node();
    local.costs[Class::Shards.index()] = Some(sizes);
    apply(&mut local, &[(Class::Shards, carried)]).unwrap();
    let prior = local.priors[Class::Shards.index()];
    let at = |receipt| hex_sim::model::booked_at(Some(receipt), prior).unwrap();
    assert_eq!(at(least).booking(), booking);
    assert_eq!(at(largest).booking(), largest.resources, "booked whole");
}
